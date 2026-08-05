//! Rendering a plan into text, and the transform that ties it all together.
//!
//! This is where JSON finally gets smaller. Everything upstream describes and
//! decides; this renders a [`CrushPlan`] and wires SmartCrusher up as a real
//! [`LossyTransform`].
//!
//! # Who the output is for
//!
//! A language model. Not a human, and emphatically not a parser — nothing
//! round-trips this back into JSON. That frees the format from syntactic
//! obligations and leaves one job: convey, in far fewer tokens than the original,
//! what the data was.
//!
//! Which means saying four things:
//!
//! - how many records there were, and how many are shown
//! - what they all had in common, once
//! - the anchor records, verbatim
//! - that content was elided, and the key to retrieve it
//!
//! The last is what makes the lossiness acceptable. The original is in the CCR
//! store, and the marker is how the model asks for it back.

use std::borrow::Cow;
use std::sync::Arc;

use serde_json::Value;

use super::{
    analyze_record_set, plan_with_query, rank_outliers, CrushConfig, CrushPlan, Document, FieldPlan,
};
use crate::block::{Block, BlockKind};
use crate::ccr::{store_and_mark, CcrStore};
use crate::detection::{detect, AdaptiveSizer, ContentType};
use crate::error::{Declined, Error, Result};
use crate::text_crusher::TextSummarizer;
use crate::transform::{LossyTransform, Transform};

/// How long an original stays retrievable.
///
/// Generous, because a model may not ask until several turns later, and a marker
/// whose content has expired is worse than not having compressed — the model is told
/// something is retrievable and then finds it is not.
const CCR_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// Renders `plan` against `document`, returning the compressed text.
///
/// Anchor records are serialized from their original values, so key order and
/// numeric literals survive exactly. An anchor that came back reformatted would not
/// be the record the output promised.
///
/// # Example
///
/// ```
/// use headroom_core::smart_crusher::{
///     analyze_record_set, format_plan, plan, rank_outliers, CrushConfig, Document,
/// };
///
/// let config = CrushConfig::default();
/// let records: Vec<String> = (0..40)
///     .map(|i| format!(r#"{{"id":{i},"kind":"file","ok":true}}"#))
///     .collect();
/// let doc = Document::parse(&format!("[{}]", records.join(",")), &config).unwrap();
///
/// let stats = analyze_record_set(&doc, &config).unwrap();
/// let outliers = rank_outliers(&doc, &stats, &config);
/// let p = plan(&doc, &stats, &outliers, &config).unwrap();
///
/// let text = format_plan(&doc, &p, None).unwrap();
/// assert!(text.contains("40 records"));
/// assert!(text.len() < doc.to_json().unwrap().len());
/// ```
pub fn format_plan(document: &Document, plan: &CrushPlan, marker: Option<&str>) -> Result<String> {
    let Value::Array(items) = document.value() else {
        return Err(Error::declined(Declined::WrongContentType));
    };

    let mut out = String::new();

    out.push_str(&format!(
        "[{} records, {} shown, {} elided]\n",
        plan.total_records,
        plan.anchors.len(),
        plan.elided()
    ));

    for field in &plan.fields {
        match field {
            FieldPlan::Constant { name, value } => {
                out.push_str(&format!("all: {name}={}\n", serde_json::to_string(value)?));
            }
            FieldPlan::Enumerated { name, values } => {
                let rendered: Vec<String> = values
                    .iter()
                    .map(serde_json::to_string)
                    .collect::<std::result::Result<_, _>>()?;
                out.push_str(&format!("{name} one of: {}\n", rendered.join(" | ")));
            }
        }
    }

    // Anchors in ascending order, with their original indices, so the model can tell
    // that record 0 and record 137 are not adjacent in the source.
    for &index in &plan.anchors {
        let Some(item) = items.get(index) else {
            continue;
        };
        out.push_str(&format!("{index}: {}\n", serde_json::to_string(item)?));
    }

    if let Some(marker) = marker {
        out.push_str(&format!("full content: {marker}\n"));
    }

    Ok(out)
}

/// Structural compression for JSON.
///
/// Holds a [`CcrStore`] so the original of every compression it performs stays
/// retrievable. That is what makes it safe to be lossy: nothing is destroyed, only
/// moved out of the prompt.
pub struct SmartCrusher {
    config: CrushConfig,
    store: Arc<dyn CcrStore>,
    sizer: AdaptiveSizer,
}

impl SmartCrusher {
    /// Creates a crusher backed by `store`.
    pub fn new(store: Arc<dyn CcrStore>) -> Self {
        Self {
            config: CrushConfig::default(),
            store,
            sizer: AdaptiveSizer::default(),
        }
    }

    /// Overrides the configuration.
    pub fn with_config(mut self, config: CrushConfig) -> Self {
        self.config = config;
        self
    }

    /// Compresses `source`, or explains why it declined.
    ///
    /// Separated from [`Transform::apply`] so the whole decision path is testable
    /// without constructing a [`Block`].
    pub fn crush(&self, source: &str) -> Result<String> {
        self.crush_for(source, None)
    }

    /// Compresses `source`, keeping whatever answers `query`.
    ///
    /// [`SmartCrusher::crush`] is this with no query, and the two produce identical
    /// bytes in that case. See [`plan_with_query`] for why the query matters.
    pub fn crush_for(&self, source: &str, query: Option<&str>) -> Result<String> {
        self.crush_with(source, query, false)
    }

    /// Compresses `source`, optionally shrinking prose strings nested inside it.
    ///
    /// # The gap `prose_leaves` closes
    ///
    /// Content routing (D1) is a decision about a *block*: the block is JSON, or code, or
    /// prose, and one compressor gets it. So a block routed here has its prose leaves
    /// handled by structural rules and never by the prose compressor — a 6KB narrative
    /// inside a JSON tool result is, to the analyzer, one long string value.
    ///
    /// That is the same asymmetry as #82 and #84, one level further in: `headroom
    /// compress` shrinks that string substantially when handed it directly, while the
    /// proxy forwards it whole inside its envelope.
    ///
    /// Off by default and enabled only from [`Transform::apply`] for tool output, because
    /// summarizing prose somebody wrote is a different act from summarizing what a
    /// command printed (D24).
    pub fn crush_with(
        &self,
        source: &str,
        query: Option<&str>,
        prose_leaves: bool,
    ) -> Result<String> {
        // Cheapest checks first. Detection is a pass over the bytes; parsing is more.
        let detection = detect(source.as_bytes());
        if detection.content_type != ContentType::Json {
            return Err(Error::declined(Declined::WrongContentType));
        }
        if !self.sizer.should_attempt(ContentType::Json, source.len()) {
            return Err(Error::declined(Declined::BelowThreshold));
        }

        // Prose leaves are shrunk first, so the structural pass that follows plans over
        // the reduced document and any anchor it keeps carries the shorter text.
        let (reduced, leaves) = if prose_leaves {
            self.shrink_prose_leaves(source)?
        } else {
            (Cow::Borrowed(source), 0)
        };

        let structural = self.crush_structurally(reduced.as_ref(), source, query);

        match structural {
            Ok(compressed) => Ok(compressed),
            // The document is not a record set, or summarizing it would not pay. If prose
            // leaves shrank, that is still a real reduction and forwarding the original
            // would throw it away — the decline is about the *structure*, not about the
            // work already done.
            // Only a *decline* is absorbed. A real failure — malformed JSON, a store
            // that will not write — must still surface, or a broken CCR round-trip would
            // read as a successful compression whose marker redeems nothing.
            Err(Error::Declined(_)) if leaves > 0 => Ok(reduced.into_owned()),
            Err(err) => Err(err),
        }
    }

    /// The record-set pass, over `source` — with `original` stored for retrieval.
    ///
    /// `original` is the caller's bytes, not the prose-reduced ones: a marker has to
    /// redeem what actually arrived, or retrieving it returns content the model never
    /// saw in full.
    fn crush_structurally(
        &self,
        source: &str,
        original: &str,
        query: Option<&str>,
    ) -> Result<String> {
        let document = Document::parse(source, &self.config)?;

        let stats = analyze_record_set(&document, &self.config)
            .ok_or_else(|| Error::declined(Declined::WrongContentType))?;
        let outliers = rank_outliers(&document, &stats, &self.config);
        let plan = plan_with_query(&document, &stats, &outliers, &self.config, query)
            .ok_or_else(|| Error::declined(Declined::NotSmaller))?;

        // Store before marking, via the helper that pairs them — a marker must never
        // advertise a hash nothing was stored under.
        let marker = store_and_mark(self.store.as_ref(), original.as_bytes(), CCR_TTL)?;

        format_plan(&document, &plan, Some(&marker))
    }

    /// Replaces long prose string values with their summarized form.
    ///
    /// Returns the rewritten JSON and how many leaves changed. Borrowed and unchanged
    /// when nothing qualified, so the common case allocates nothing.
    ///
    /// Reuses [`TextSummarizer`] rather than reimplementing a summary here: it already
    /// owns the anchor floor, the tag-delimiter floor, the relevance pass and the CCR
    /// round-trip, and a second copy of that decision is what check 6 of the reachability
    /// audit fails the build over. Its own size threshold applies, so a leaf too short to
    /// be worth summarizing declines and is left exactly as it was.
    fn shrink_prose_leaves<'a>(&self, source: &'a str) -> Result<(Cow<'a, str>, usize)> {
        let mut value: Value = serde_json::from_str(source).map_err(|err| Error::Malformed {
            content_type: "json",
            detail: err.to_string(),
        })?;

        let summarizer = TextSummarizer::new(Arc::clone(&self.store));
        let mut changed = 0usize;
        shrink_in_place(&mut value, &summarizer, &mut changed, self.config.max_depth);

        if changed == 0 {
            return Ok((Cow::Borrowed(source), 0));
        }

        Ok((Cow::Owned(serde_json::to_string(&value)?), changed))
    }
}

/// Walks `value`, summarizing prose leaves in place.
///
/// Depth-bounded for the same reason [`CrushConfig::max_depth`] exists: this recurses
/// over tool output, which is not trusted input, and unbounded recursion over it is a
/// stack overflow waiting to be triggered.
fn shrink_in_place(
    value: &mut Value,
    summarizer: &TextSummarizer,
    changed: &mut usize,
    depth: usize,
) {
    if depth == 0 {
        return;
    }

    match value {
        Value::String(text) => {
            // Detection first, on the leaf itself. A long identifier, a base64 blob or an
            // embedded JSON document are all long strings and none of them is prose.
            if detect(text.as_bytes()).content_type != ContentType::Prose {
                return;
            }

            let mut block = Block::new(BlockKind::ToolResult, text.clone());
            if summarizer.apply(&mut block).is_ok() {
                *text = block.content().to_owned();
                *changed += 1;
            }
        }
        Value::Array(items) => {
            for item in items {
                shrink_in_place(item, summarizer, changed, depth - 1);
            }
        }
        Value::Object(fields) => {
            for (_, field) in fields.iter_mut() {
                shrink_in_place(field, summarizer, changed, depth - 1);
            }
        }
        _ => {}
    }
}

impl Transform for SmartCrusher {
    fn name(&self) -> &'static str {
        "smart_crusher"
    }

    fn apply(&self, block: &mut Block) -> Result<()> {
        // The block carries the question its content answers, when the caller had one.
        // This is the single line that makes the relevance pass reachable from a real
        // request rather than only from a test — the defect that produced #71, #73,
        // #75, #82 and #84, each time by landing capability with no caller.
        // Prose leaves are only shrunk for tool output. A JSON payload a person wrote
        // may carry their own prose, and summarizing that is a different act (D24) —
        // the same gate `TextSummarizer` applies at the top level, applied one level in.
        let compressed = self.crush_with(
            block.content(),
            block.query(),
            block.kind().is_tool_output(),
        )?;
        block.replace_content(compressed);
        Ok(())
    }
}

impl LossyTransform for SmartCrusher {}

#[cfg(test)]
mod prose_leaf_tests {
    use super::*;
    use crate::ccr::InMemoryCcrStore;

    fn crusher() -> (SmartCrusher, Arc<InMemoryCcrStore>) {
        let store = Arc::new(InMemoryCcrStore::new());
        (SmartCrusher::new(store.clone()), store)
    }

    /// Prose long enough for the summarizer's own threshold to accept it.
    fn narrative() -> String {
        (0..200)
            .map(|i| format!("The deployment step number {i} completed as expected."))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A tool result whose envelope is small and whose payload is a long narrative.
    fn envelope() -> String {
        serde_json::json!({
            "status": "ok",
            "report": narrative(),
        })
        .to_string()
    }

    #[test]
    fn a_prose_leaf_inside_json_is_shrunk() {
        // The gap. Routing hands this block to SmartCrusher because it is JSON, so the
        // prose compressor never sees the payload — `headroom compress` shrinks that
        // same string substantially when handed it directly. Same asymmetry as #82 and
        // #84, one level further in.
        let (crusher, _store) = crusher();
        let source = envelope();

        assert!(
            crusher.crush(&source).is_err(),
            "the structural pass already handled this; the test proves nothing"
        );

        let out = crusher
            .crush_with(&source, None, true)
            .expect("prose leaves should shrink");

        assert!(
            out.len() < source.len() / 2,
            "output is {} bytes against {} in",
            out.len(),
            source.len()
        );
        assert!(
            out.contains("\"status\":\"ok\""),
            "the envelope did not survive"
        );
    }

    #[test]
    fn the_shrunk_leaf_is_retrievable() {
        // Lossy with a way back, like every other offload. The summarizer stores the
        // original leaf and leaves its marker in place.
        let (crusher, store) = crusher();
        let out = crusher
            .crush_with(&envelope(), None, true)
            .expect("prose leaves should shrink");

        // Sliced between the delimiters rather than to end-of-line: the marker now sits
        // inside a JSON string, so the newline after it is the two characters `\` and
        // `n` and a line-oriented split hands `parse_marker` an escape sequence.
        let start = out.find("<<ccr:").expect("a marker survived into the leaf");
        let end = out[start..].find(">>").expect("the marker is closed") + start + 2;
        let hash = crate::ccr::parse_marker(&out[start..end]).expect("the marker parses");
        let stored = store.get(hash).expect("store readable").expect("present");

        assert_eq!(String::from_utf8_lossy(&stored), narrative());
    }

    #[test]
    fn a_short_leaf_is_left_exactly_as_it_was() {
        // The summarizer's own size threshold applies unchanged, so a leaf too short to
        // be worth summarizing is not touched — and the whole document then declines
        // exactly as it did before this existed.
        let (crusher, _store) = crusher();
        let source = serde_json::json!({
            "status": "ok",
            "note": "a short human-readable note that is nowhere near the threshold",
            "padding": "x".repeat(2000),
        })
        .to_string();

        let with = crusher.crush_with(&source, None, true);
        let without = crusher.crush_with(&source, None, false);

        assert_eq!(with.is_err(), without.is_err());
    }

    #[test]
    fn non_prose_leaves_are_never_touched() {
        // A long identifier, a base64 blob and an embedded JSON document are all long
        // strings, and summarizing any of them would corrupt content the model needs
        // verbatim.
        let (crusher, _store) = crusher();
        let blob = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVph".repeat(200);
        let source = serde_json::json!({ "status": "ok", "payload": blob }).to_string();

        let out = crusher.crush_with(&source, None, true);

        // Either it declined outright or it left the blob alone; what it must not do is
        // return something with the blob summarized.
        if let Ok(compressed) = out {
            assert!(
                compressed.contains(&blob) || !compressed.contains("lines,"),
                "a base64 payload was summarized as prose"
            );
        }
    }

    #[test]
    fn the_structural_pass_still_wins_where_it_applies() {
        // A record set must compress structurally, as before — the prose pass runs first
        // but must not prevent the summary that is the higher-value result.
        let (crusher, _store) = crusher();
        let records: Vec<String> = (0..200)
            .map(|i| format!(r#"{{"path":"src/module_{i}.rs","kind":"file","status":"ok"}}"#))
            .collect();
        let source = format!("[{}]", records.join(","));

        let out = crusher
            .crush_with(&source, None, true)
            .expect("should compress");

        assert!(
            out.contains("200 records"),
            "the structural pass did not run"
        );
    }

    #[test]
    fn shrinking_prose_leaves_is_deterministic() {
        let (crusher, _store) = crusher();
        let source = envelope();
        let first = crusher.crush_with(&source, None, true).expect("shrinks");
        for _ in 0..5 {
            assert_eq!(
                first,
                crusher.crush_with(&source, None, true).expect("shrinks")
            );
        }
    }

    #[test]
    fn a_block_that_is_not_tool_output_keeps_its_prose() {
        // D24, one level in. A JSON payload a person wrote may carry their own prose,
        // and summarizing that is a different act from summarizing what a command
        // printed.
        let (crusher, _store) = crusher();

        let mut authored = Block::new(BlockKind::Text, envelope());
        let authored_result = crusher.apply(&mut authored);

        let mut tool = Block::new(BlockKind::ToolResult, envelope());
        crusher.apply(&mut tool).expect("tool output shrinks");

        assert!(
            authored_result.is_err(),
            "a person's own JSON had its prose summarized"
        );
        assert!(tool.content().len() < envelope().len() / 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::BlockKind;
    use crate::ccr::{parse_marker, InMemoryCcrStore};
    use crate::tokenizer::{HeuristicEstimator, Tokenizer};
    use crate::validate::validated_apply;

    fn crusher() -> (SmartCrusher, Arc<InMemoryCcrStore>) {
        let store = Arc::new(InMemoryCcrStore::new());
        (SmartCrusher::new(store.clone()), store)
    }

    /// A realistic file-listing tool result.
    fn tool_output(count: usize) -> String {
        let records: Vec<String> = (0..count)
            .map(|i| {
                format!(
                    r#"{{"path":"src/module_{i}.rs","kind":"file","status":"ok","size":{},"repo":"acme/widgets"}}"#,
                    1000 + i * 7
                )
            })
            .collect();
        format!("[{}]", records.join(","))
    }

    #[test]
    fn a_realistic_tool_result_gets_measurably_smaller() {
        // The end-to-end claim this whole crate exists to make.
        let (crusher, _store) = crusher();
        let source = tool_output(200);
        let compressed = crusher.crush(&source).expect("should compress");

        let estimator = HeuristicEstimator::new();
        let before = estimator.count(&source);
        let after = estimator.count(&compressed);

        assert!(
            after < before / 2,
            "expected a large reduction, got {before} -> {after}"
        );
    }

    #[test]
    fn the_original_is_retrievable_through_the_emitted_marker() {
        // What makes the lossiness acceptable.
        let (crusher, store) = crusher();
        let source = tool_output(100);
        let compressed = crusher.crush(&source).unwrap();

        let marker_line = compressed
            .lines()
            .find(|l| l.starts_with("full content: "))
            .expect("marker line");
        let marker = marker_line.trim_start_matches("full content: ");
        let hash = parse_marker(marker).expect("well-formed marker");

        let retrieved = store.get(hash).unwrap().expect("original stored");
        assert_eq!(
            String::from_utf8(retrieved).unwrap(),
            source,
            "retrieved content must be the exact original"
        );
    }

    #[test]
    fn anchor_records_are_byte_identical_to_their_source_form() {
        // An anchor that came back reformatted is not the record we promised.
        let (crusher, _store) = crusher();
        // Record 0 leads with `zebra` before `id`, carries a float that must not
        // collapse to an integer, and an integer past 2^53 that must not lose digits
        // through f64. `note` is padding so the payload clears the 1 KB JSON floor.
        let mut records =
            vec![r#"{"zebra":1.0,"id":12345678901234567890,"note":"padding-0"}"#.to_string()];
        records
            .extend((1..60).map(|i| format!(r#"{{"zebra":1.0,"id":{i},"note":"padding-{i}"}}"#)));
        let source = format!("[{}]", records.join(","));
        assert!(
            source.len() > 1024,
            "fixture must clear the JSON size floor"
        );

        let compressed = crusher.crush(&source).expect("should compress");

        // Record 0 is in the head sample, and comes back exactly as it went in.
        assert!(
            compressed.contains(r#"{"zebra":1.0,"id":12345678901234567890,"note":"padding-0"}"#),
            "anchor was reformatted:\n{compressed}"
        );
    }

    #[test]
    fn constant_fields_are_stated_once() {
        let (crusher, _store) = crusher();
        let compressed = crusher.crush(&tool_output(100)).unwrap();

        assert!(compressed.contains(r#"all: kind="file""#), "{compressed}");
        assert!(
            compressed.contains(r#"all: repo="acme/widgets""#),
            "{compressed}"
        );
        // And not repeated per anchor beyond the records themselves.
        assert_eq!(compressed.matches(r#"all: kind="file""#).count(), 1);
    }

    #[test]
    fn the_record_and_elision_counts_are_reported() {
        let (crusher, _store) = crusher();
        let compressed = crusher.crush(&tool_output(150)).unwrap();
        assert!(compressed.starts_with("[150 records,"), "{compressed}");
        assert!(compressed.contains("elided]"), "{compressed}");
    }

    #[test]
    fn anchors_carry_their_original_indices() {
        // So the model can tell record 0 and record 137 are not adjacent.
        let (crusher, _store) = crusher();
        let compressed = crusher.crush(&tool_output(100)).unwrap();
        assert!(compressed.contains("\n0: {"), "{compressed}");
    }

    // ---- declining ----

    #[test]
    fn non_json_is_declined_untouched() {
        let (crusher, _store) = crusher();
        let err = crusher.crush(&"plain prose ".repeat(500)).unwrap_err();
        assert!(matches!(err, Error::Declined(_)));
        assert!(err.is_recoverable());
    }

    #[test]
    fn below_threshold_json_is_declined() {
        let (crusher, _store) = crusher();
        let err = crusher.crush(r#"[{"a":1},{"a":2}]"#).unwrap_err();
        assert!(matches!(
            err,
            Error::Declined(Declined::BelowThreshold) | Error::Declined(Declined::WrongContentType)
        ));
    }

    #[test]
    fn json_with_no_workable_plan_is_declined() {
        // A large object rather than a record array — nothing for the planner.
        let (crusher, _store) = crusher();
        let big = "x".repeat(3000);
        let err = crusher
            .crush(&format!(r#"{{"body":"{big}"}}"#))
            .unwrap_err();
        assert!(err.is_recoverable());
    }

    #[test]
    fn declining_stores_nothing() {
        // A decline must not leave a CCR entry behind for content that was never
        // compressed.
        let (crusher, store) = crusher();
        let _ = crusher.crush(r#"[{"a":1},{"a":2}]"#);
        assert!(store.is_empty());
    }

    // ---- integration with the invariants ----

    #[test]
    fn it_runs_through_the_guarded_validated_path() {
        // No special casing for the flagship compressor: apply_guarded (I8) and
        // validated_apply (I5) apply exactly as to any other transform.
        let (crusher, _store) = crusher();
        let mut block = Block::tool_result(tool_output(200), "toolu_1");

        let outcome = validated_apply(&crusher, &mut block, &HeuristicEstimator::new()).unwrap();

        assert!(outcome.is_compressed());
        assert!(outcome.tokens_saved() > 0);
        assert_eq!(block.tool_use_id(), Some("toolu_1"));
        assert_eq!(block.kind(), BlockKind::ToolResult);
    }

    #[test]
    fn a_sacrosanct_block_is_refused_before_the_crusher_sees_it() {
        let (crusher, store) = crusher();
        let original = tool_output(200);
        let mut block = Block::new(BlockKind::Thinking, original.clone());

        let outcome = validated_apply(&crusher, &mut block, &HeuristicEstimator::new()).unwrap();

        assert!(!outcome.is_compressed());
        assert_eq!(block.content(), original);
        assert!(store.is_empty(), "nothing should have been stored");
    }

    #[test]
    fn the_i5_fallback_restores_the_original_when_output_is_not_smaller() {
        // Force a config that keeps nearly everything, so the "compressed" form is
        // no improvement and I5 discards it.
        let store = Arc::new(InMemoryCcrStore::new());
        let crusher = SmartCrusher::new(store).with_config(CrushConfig {
            sample_records: 10_000,
            ..CrushConfig::default()
        });

        let source = tool_output(30);
        let mut block = Block::new(BlockKind::Text, source.clone());
        validated_apply(&crusher, &mut block, &HeuristicEstimator::new()).unwrap();

        assert_eq!(block.content(), source);
    }

    #[test]
    fn requires_ccr_is_true() {
        let (crusher, _store) = crusher();
        assert!(crusher.requires_ccr());
    }

    #[test]
    fn compression_is_deterministic() {
        // Invariant I4, end to end. Same input, byte-equal output, every run —
        // including the CCR marker, which is content-addressed.
        let source = tool_output(120);
        let first = {
            let (crusher, _store) = crusher();
            crusher.crush(&source).unwrap()
        };
        for _ in 0..25 {
            let (crusher, _store) = crusher();
            assert_eq!(crusher.crush(&source).unwrap(), first);
        }
    }

    #[test]
    fn an_outlier_survives_compression_verbatim() {
        // The whole point of outlier detection, checked at the output.
        let mut records: Vec<String> = (0..120)
            .map(|i| format!(r#"{{"id":{i},"kind":"file","status":"ok"}}"#))
            .collect();
        records.push(
            r#"{"id":120,"kind":"file","status":"ok","error":"permission denied on /etc/shadow"}"#
                .into(),
        );
        let source = format!("[{}]", records.join(","));

        let (crusher, _store) = crusher();
        let compressed = crusher.crush(&source).expect("should compress");

        assert!(
            compressed.contains("permission denied on /etc/shadow"),
            "the anomalous record was elided:\n{compressed}"
        );
    }
}
