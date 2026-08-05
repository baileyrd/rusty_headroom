//! Planning — deciding what to keep, completely, before anything is mutated.
//!
//! Everything upstream of this module observes. This one decides.
//!
//! # Why the plan is data
//!
//! A planner that emitted output as it walked would interleave "what should happen"
//! with "make it happen", and a bug in the first would silently corrupt the second.
//! A [`CrushPlan`] is inert: it can be inspected, compared, asserted on, and thrown
//! away. If it turns out not to be worth executing, nothing has been touched.
//!
//! It also means the decision to decline is cheap. [`plan`] returns `None` for
//! documents where compression would not pay, before any formatting or tokenizing
//! happens.

use serde_json::Value;

use super::{classify_field, CrushConfig, Document, FieldKind, FieldRole, Outlier, RecordSetStats};
use crate::relevance::RelevanceScorer;

/// What will be said about a field instead of repeating it per record.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldPlan {
    /// Stated once: every record has this value.
    Constant {
        /// The field name.
        name: String,
        /// The shared value.
        value: Value,
    },
    /// Enumerated: the field takes one of these values.
    Enumerated {
        /// The field name.
        name: String,
        /// The distinct values, in first-appearance order.
        values: Vec<Value>,
    },
}

impl FieldPlan {
    /// The field this plan describes.
    pub fn name(&self) -> &str {
        match self {
            Self::Constant { name, .. } | Self::Enumerated { name, .. } => name,
        }
    }
}

/// A complete decision about how to compress a record set.
///
/// Pure data. Building one mutates nothing.
#[derive(Debug, Clone, PartialEq)]
pub struct CrushPlan {
    /// Records kept verbatim, ascending and deduplicated.
    ///
    /// Ascending because output order follows input order; deduplicated because a
    /// record can be both a head sample and an outlier, and it appears once.
    pub anchors: Vec<usize>,
    /// Fields described once rather than repeated per record.
    pub fields: Vec<FieldPlan>,
    /// Total records in the source array.
    pub total_records: usize,
}

impl CrushPlan {
    /// How many records will not be shown.
    pub fn elided(&self) -> usize {
        self.total_records.saturating_sub(self.anchors.len())
    }

    /// Whether a given record survives verbatim.
    pub fn keeps(&self, record: usize) -> bool {
        self.anchors.binary_search(&record).is_ok()
    }
}

/// Decides how to compress a record set, or that it should be left alone.
///
/// Returns `None` when compression is not worth attempting.
///
/// # Why declining early matters
///
/// Invariant I5 would catch a bad plan after the fact — format it, tokenize both
/// versions, discover the "compressed" form is no smaller, forward the original. That
/// is correct but wasteful. Where the planner can already tell there is nothing to
/// gain, it says so and the expensive path never runs.
///
/// # Example
///
/// ```
/// use headroom_core::smart_crusher::{
///     analyze_record_set, plan, rank_outliers, CrushConfig, Document,
/// };
///
/// let config = CrushConfig::default();
/// let records: Vec<String> = (0..50)
///     .map(|i| format!(r#"{{"id":{i},"kind":"file","ok":true}}"#))
///     .collect();
/// let doc = Document::parse(&format!("[{}]", records.join(",")), &config).unwrap();
///
/// let stats = analyze_record_set(&doc, &config).unwrap();
/// let outliers = rank_outliers(&doc, &stats, &config);
/// let p = plan(&doc, &stats, &outliers, &config).unwrap();
///
/// assert!(p.elided() > 0);
/// ```
pub fn plan(
    document: &Document,
    stats: &RecordSetStats,
    outliers: &[Outlier],
    config: &CrushConfig,
) -> Option<CrushPlan> {
    plan_with_query(document, stats, outliers, config, None)
}

/// Decides how to compress a record set, pinning whatever answers `query`.
///
/// [`plan`] is this with `query = None`, and the two are byte-identical in that case
/// — the relevance pass is skipped entirely rather than run against an empty string.
///
/// # Why the query changes what compression means
///
/// Every other decision this planner makes is **structural**: how repetitive the
/// records are, which fields are constant, which records are statistically anomalous.
/// Structure is a property of the data alone, so without a query a tool result
/// compresses identically whether the user asked "how many files are there" or "show
/// me `src/parser.rs`".
///
/// The consequence is not a worse ratio — it is a worse *answer*. A user asking about
/// order `a3f9` among four hundred orders gets a compressed result in which that one
/// record was no more likely to survive than any other, and the ratio looks exactly as
/// healthy as it would have if the record had been kept. Nothing measures this, which
/// is why it went unnoticed through Round 1.
///
/// Relevant records join the anchor set as a **floor**, the same relationship outliers
/// already have: the sample yields to them, and they are never dropped to make room.
///
/// # What the query is
///
/// Supplied by the caller, not derived here. On the request path it is the newest user
/// message text joined with the arguments of the tool call this output answers — see
/// [`Block::with_query`](crate::block::Block::with_query). A caller with no
/// conversation to draw on passes `None` and gets the previous behavior.
pub fn plan_with_query(
    document: &Document,
    stats: &RecordSetStats,
    outliers: &[Outlier],
    config: &CrushConfig,
    query: Option<&str>,
) -> Option<CrushPlan> {
    let Value::Array(items) = document.value() else {
        return None;
    };

    let total = stats.records;
    if total < config.min_records_to_summarize {
        // Too few records for a summary to beat simply listing them.
        return None;
    }

    // Outliers first, and unconditionally. If they exceed the sample budget the
    // sample yields — an outlier is never dropped to make room. Dropping the
    // anomalous record is the single failure that makes compressed output actively
    // worse than no compression at all.
    let mut anchors: Vec<usize> = outliers.iter().map(|o| o.record).collect();

    // Records the data itself ranks highest. A score field is the payload saying which
    // of its own records matter — a search result set's `relevance`, a ranked list's
    // `confidence` — and eliding the top of it while keeping an arbitrary head sample
    // inverts what the tool was asked to produce.
    anchors.extend(top_ranked_records(items, config));

    // Records answering the query, on the same footing as outliers. Both are floors
    // the sample budget cannot cut into, for the same reason: the record the user
    // asked about and the record that breaks the pattern are exactly the two a reader
    // would notice were missing.
    anchors.extend(relevant_records(items, query, config));

    // Head sample, so the model can infer the shape of what was elided. Taken from
    // the front because a truncated list's first entries are what a reader uses to
    // generalize about the rest.
    for index in 0..config.sample_records.min(total) {
        anchors.push(index);
    }

    anchors.sort_unstable();
    anchors.dedup();

    // Nothing would be elided, so there is nothing to gain and a marker to pay for.
    if anchors.len() >= total {
        return None;
    }

    let fields = field_plans(items, stats);

    // The honest-decline case. With no fields to describe once and almost every
    // record kept, the output would be the original plus a header — bigger, not
    // smaller. Requiring that a clear majority is elided keeps this off documents
    // where the win is marginal at best.
    if fields.is_empty() && anchors.len() * 2 >= total {
        return None;
    }

    Some(CrushPlan {
        anchors,
        fields,
        total_records: total,
    })
}

/// Indices of the highest-ranked records, when the payload carries a score field.
///
/// Empty when no field qualifies, which is the common case — so a record set without a
/// ranking signal plans exactly as it did before this existed.
///
/// # Why the identifier check is load-bearing
///
/// A numeric `id` running `1..200` is numeric, non-constant and evenly distributed, which
/// is everything a score looks like. Ranking by it would pin records 195–199 and present
/// that as a summary of what mattered. [`classify_field`] rules those out first, and that
/// is the reason it exists.
///
/// Only the *first* qualifying field is used. Two score fields ranking in different
/// directions is a payload nobody can summarize correctly, and picking one arbitrarily is
/// more honest than blending them into a number that means nothing.
fn top_ranked_records(items: &[Value], config: &CrushConfig) -> Vec<usize> {
    let budget = config.max_ranked_records.min(items.len());
    if budget == 0 {
        return Vec::new();
    }

    let records: Vec<&serde_json::Map<String, Value>> =
        items.iter().filter_map(Value::as_object).collect();
    if records.len() != items.len() {
        return Vec::new();
    }

    // Document order, so which field wins does not depend on map iteration order (I4).
    let mut names: Vec<&str> = Vec::new();
    for record in &records {
        for name in record.keys() {
            if !names.contains(&name.as_str()) {
                names.push(name);
            }
        }
    }

    let Some(field) = names
        .into_iter()
        .find(|name| classify_field(&records, name) == FieldRole::Score)
    else {
        return Vec::new();
    };

    let mut ranked: Vec<(usize, f64)> = records
        .iter()
        .enumerate()
        .filter_map(|(index, record)| record.get(field)?.as_f64().map(|score| (index, score)))
        .collect();

    // Descending by score; ascending by index within a tie, so the order is a property of
    // this comparator rather than of the sort (I4).
    ranked.sort_by(|(left_index, left), (right_index, right)| {
        right
            .partial_cmp(left)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left_index.cmp(right_index))
    });

    ranked
        .into_iter()
        .take(budget)
        .map(|(index, _)| index)
        .collect()
}

/// Indices of the records that answer `query`.
///
/// Empty when there is no query, which is what makes [`plan`] and
/// [`plan_with_query`] byte-identical in the absent case — the scorer is never
/// constructed and the records are never serialized for scoring.
///
/// # Why there is a cap
///
/// A query sharing a common term with every record — "file", "error", "status" —
/// would otherwise pin the whole set and turn compression off without saying so. The
/// cap keeps the worst case bounded: relevance can promote records past the sample,
/// never past the point where a summary stops being a summary.
///
/// Selection is by score, and ties break toward the **earlier** record so the result
/// does not depend on sort stability (I4).
fn relevant_records(items: &[Value], query: Option<&str>, config: &CrushConfig) -> Vec<usize> {
    let Some(query) = query else {
        return Vec::new();
    };

    let budget = config.max_relevant_records.min(items.len());
    if budget == 0 {
        return Vec::new();
    }

    // Scored against each record's serialized form. That is what the model will read,
    // so it is what "does this answer the question" has to be measured over — scoring
    // values alone would miss a match on a field *name*, which is how a query like
    // "error message" finds the record that has an `error` field at all.
    let rendered: Vec<String> = items.iter().map(Value::to_string).collect();
    let scores = config.scorer().score_all(query, &rendered);

    let mut ranked: Vec<(usize, f64)> = scores
        .iter()
        .enumerate()
        .filter(|(_, score)| score.clears(config.relevance_threshold))
        .map(|(index, score)| (index, score.value()))
        .collect();

    // Descending by score; ascending by index within a tie. `sort_by` is stable, but
    // spelling the tiebreak out means the ordering is a property of this comparator
    // rather than of the sort implementation.
    ranked.sort_by(|(left_index, left), (right_index, right)| {
        right
            .partial_cmp(left)
            // Both values cleared a threshold, so neither is NaN — but I4 does not
            // survive on "so it cannot happen", and an unordered pair would otherwise
            // leave the order to the sort.
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left_index.cmp(right_index))
    });

    ranked
        .into_iter()
        .take(budget)
        .map(|(index, _)| index)
        .collect()
}

/// Builds the per-field descriptions.
fn field_plans(items: &[Value], stats: &RecordSetStats) -> Vec<FieldPlan> {
    stats
        .fields
        .iter()
        .filter_map(|stat| match &stat.kind {
            FieldKind::Constant { value } => Some(FieldPlan::Constant {
                name: stat.name.clone(),
                value: value.clone(),
            }),

            // Only fields present in every record are enumerated. A field some
            // records lack cannot be described by its value set alone without also
            // saying which records have it, and that qualification costs more than
            // the enumeration saves.
            FieldKind::LowCardinality { .. } if stat.present_in == stats.records => {
                Some(FieldPlan::Enumerated {
                    name: stat.name.clone(),
                    values: distinct_values(items, &stat.name),
                })
            }

            _ => None,
        })
        .collect()
}

/// Distinct values of `field`, in first-appearance order.
///
/// Document order rather than sorted: the output follows the source, and a sorted
/// enumeration would be a second, gratuitous reordering for a reader to reconcile.
fn distinct_values(items: &[Value], field: &str) -> Vec<Value> {
    let mut seen: Vec<Value> = Vec::new();
    for item in items {
        let Some(value) = item.as_object().and_then(|o| o.get(field)) else {
            continue;
        };
        if !seen.contains(value) {
            seen.push(value.clone());
        }
    }
    seen
}

#[cfg(test)]
mod ranking_tests {
    use super::*;
    use crate::smart_crusher::{analyze_record_set, rank_outliers};

    /// A search result set: 60 hits, descending relevance, best first is *not* the
    /// document order.
    fn search_hits() -> String {
        let mut records: Vec<String> = (0..60)
            .map(|i| format!(r#"{{"doc":"doc-{i:03}","relevance":0.100,"state":"ok"}}"#))
            .collect();
        // The genuinely best hit sits well past the head sample.
        records[47] = r#"{"doc":"doc-047","relevance":0.950,"state":"ok"}"#.to_owned();
        records[48] = r#"{"doc":"doc-048","relevance":0.910,"state":"ok"}"#.to_owned();
        format!("[{}]", records.join(","))
    }

    fn plan_for(source: &str) -> CrushPlan {
        let config = CrushConfig::default();
        let doc = Document::parse(source, &config).expect("parses");
        let stats = analyze_record_set(&doc, &config).expect("is a record set");
        let outliers = rank_outliers(&doc, &stats, &config);
        plan_with_query(&doc, &stats, &outliers, &config, None).expect("plans")
    }

    #[test]
    fn the_highest_scoring_records_survive() {
        // A search result set is the payload saying which of its own records matter.
        // Eliding the top of it while keeping an arbitrary head sample inverts what the
        // tool was asked to produce.
        let plan = plan_for(&search_hits());

        assert!(plan.keeps(47), "the best-scoring record was elided");
        assert!(plan.keeps(48), "the second-best record was elided");
    }

    #[test]
    fn a_numeric_id_does_not_rank_anything() {
        // The reason `classify_field` exists. `seq` running 0..60 is numeric,
        // non-constant and evenly spread — everything a score looks like. Ranking by it
        // would pin the highest-numbered records and call that a summary of what
        // mattered.
        let records: Vec<String> = (0..60)
            .map(|i| format!(r#"{{"seq":{i},"state":"ok"}}"#))
            .collect();
        let plan = plan_for(&format!("[{}]", records.join(",")));

        for tail in 55..60 {
            assert!(
                !plan.keeps(tail),
                "record {tail} was pinned by its sequence number"
            );
        }
    }

    #[test]
    fn a_timestamp_does_not_rank_anything() {
        // Same shape, different disguise: ranking by `created_at` keeps the newest
        // records rather than the ones that matter.
        let records: Vec<String> = (0..60)
            .map(|i| {
                format!(
                    r#"{{"created_at":{},"state":"ok"}}"#,
                    1_700_000_000u64 + i * 3600
                )
            })
            .collect();
        let plan = plan_for(&format!("[{}]", records.join(",")));

        for tail in 55..60 {
            assert!(
                !plan.keeps(tail),
                "record {tail} was pinned by its timestamp"
            );
        }
    }

    #[test]
    fn a_record_set_without_a_score_plans_exactly_as_before() {
        let records: Vec<String> = (0..60)
            .map(|i| format!(r#"{{"path":"src/module_{i}.rs","kind":"file"}}"#))
            .collect();
        let source = format!("[{}]", records.join(","));

        let config = CrushConfig::default();
        let doc = Document::parse(&source, &config).unwrap();
        let stats = analyze_record_set(&doc, &config).unwrap();
        let outliers = rank_outliers(&doc, &stats, &config);

        let mut without = config;
        without.max_ranked_records = 0;

        assert_eq!(
            plan_with_query(&doc, &stats, &outliers, &config, None),
            plan_with_query(&doc, &stats, &outliers, &without, None)
        );
    }

    #[test]
    fn ranking_still_compresses() {
        // The cap. A score field with a narrow spread must not pin most of the set and
        // quietly stop compression.
        let plan = plan_for(&search_hits());
        assert!(plan.elided() > 0, "ranking pinned the whole set");
    }

    #[test]
    fn ranking_is_deterministic() {
        // Every relevance below the top two is identical here, so the tie-break is doing
        // the work — exactly the case that would vary if it relied on sort stability.
        let source = search_hits();
        let first = plan_for(&source);
        for _ in 0..5 {
            assert_eq!(first, plan_for(&source));
        }
    }
}

#[cfg(test)]
mod relevance_tests {
    use super::*;
    use crate::smart_crusher::{analyze_record_set, rank_outliers};

    /// 60 orders, one of which is the one being asked about.
    fn orders() -> String {
        let records: Vec<String> = (0..60)
            .map(|i| format!(r#"{{"order":"ord-{i:04}","state":"pending","items":2}}"#))
            .collect();
        format!("[{}]", records.join(","))
    }

    fn plan_for(source: &str, query: Option<&str>) -> CrushPlan {
        let config = CrushConfig::default();
        let doc = Document::parse(source, &config).expect("parses");
        let stats = analyze_record_set(&doc, &config).expect("is a record set");
        let outliers = rank_outliers(&doc, &stats, &config);
        plan_with_query(&doc, &stats, &outliers, &config, query).expect("plans")
    }

    #[test]
    fn the_record_that_was_asked_about_survives() {
        // The defect this exists for. Record 42 is structurally identical to its 59
        // peers, so nothing about the data itself would keep it — only the fact that
        // it is what the user asked for.
        let source = orders();

        let without = plan_for(&source, None);
        assert!(
            !without.keeps(42),
            "record 42 already survived without a query; the test proves nothing"
        );

        let with = plan_for(&source, Some("ord-0042"));
        assert!(with.keeps(42), "the record the user asked about was elided");
    }

    #[test]
    fn an_absent_query_plans_byte_for_byte_as_before() {
        // The compatibility guarantee every non-proxy caller depends on: the CLI, the
        // MCP server and the Python binding have no conversation to draw a query from.
        let source = orders();
        assert_eq!(plan_for(&source, None), plan_for(&source, Some("")).clone());
    }

    #[test]
    fn a_query_matching_nothing_changes_nothing() {
        let source = orders();
        assert_eq!(
            plan_for(&source, None),
            plan_for(&source, Some("zzzz-no-such-term"))
        );
    }

    #[test]
    fn relevance_still_compresses_rather_than_pinning_everything() {
        // A query sharing a common term with every record must not quietly turn
        // compression off. Without the cap, `state` pins all 60 and `plan` returns
        // `None` for "nothing would be elided" — compression silently disabled while
        // the metrics report a healthy passthrough.
        let source = orders();
        let plan = plan_for(&source, Some("pending state"));

        assert!(
            plan.elided() > 0,
            "a common term pinned the whole set and compression stopped"
        );
        assert!(
            plan.anchors.len()
                <= CrushConfig::default().sample_records
                    + CrushConfig::default().max_relevant_records
                    + 8,
            "pinned {} records, which is not a summary",
            plan.anchors.len()
        );
    }

    #[test]
    fn outliers_are_never_displaced_by_relevance() {
        // Both are floors. Relevance joining the anchor set must not cost an outlier
        // its place — dropping the anomalous record is the one failure that makes
        // compressed output worse than none.
        let config = CrushConfig::default();
        let source = orders();
        let doc = Document::parse(&source, &config).expect("parses");
        let stats = analyze_record_set(&doc, &config).expect("is a record set");
        let outliers = rank_outliers(&doc, &stats, &config);

        let plan = plan_with_query(&doc, &stats, &outliers, &config, Some("ord-0042")).unwrap();

        for outlier in &outliers {
            assert!(
                plan.keeps(outlier.record),
                "outlier at {} was displaced by relevance",
                outlier.record
            );
        }
    }

    #[test]
    fn planning_with_a_query_is_deterministic() {
        // I4, on the path that newly introduces float comparison and a sort.
        let source = orders();
        let first = plan_for(&source, Some("ord-0042 pending"));
        for _ in 0..5 {
            assert_eq!(first, plan_for(&source, Some("ord-0042 pending")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smart_crusher::{analyze_record_set, rank_outliers};

    fn config() -> CrushConfig {
        CrushConfig::default()
    }

    fn make_plan(source: &str) -> Option<CrushPlan> {
        let doc = Document::parse(source, &config()).expect("valid json");
        let stats = analyze_record_set(&doc, &config())?;
        let outliers = rank_outliers(&doc, &stats, &config());
        plan(&doc, &stats, &outliers, &config())
    }

    /// `count` uniform records with a constant field and a rotating status.
    fn uniform(count: usize) -> String {
        let records: Vec<String> = (0..count)
            .map(|i| format!(r#"{{"id":{i},"kind":"file","status":"ok"}}"#))
            .collect();
        format!("[{}]", records.join(","))
    }

    #[test]
    fn a_large_uniform_array_produces_a_plan_that_elides_most_of_it() {
        let p = make_plan(&uniform(100)).expect("should plan");
        assert_eq!(p.total_records, 100);
        assert!(p.elided() > 90, "elided {}", p.elided());
        assert!(p.anchors.len() <= 10);
    }

    #[test]
    fn constant_fields_are_described_once() {
        let p = make_plan(&uniform(50)).expect("should plan");
        let constants: Vec<&str> = p
            .fields
            .iter()
            .filter(|f| matches!(f, FieldPlan::Constant { .. }))
            .map(FieldPlan::name)
            .collect();
        assert!(constants.contains(&"kind"), "got {constants:?}");
    }

    #[test]
    fn low_cardinality_fields_are_enumerated_in_document_order() {
        let records: Vec<String> = (0..30)
            .map(|i| {
                let status = ["ok", "retry", "failed"][i % 3];
                format!(r#"{{"id":{i},"status":"{status}"}}"#)
            })
            .collect();
        let p = make_plan(&format!("[{}]", records.join(","))).expect("should plan");

        let enumerated = p
            .fields
            .iter()
            .find_map(|f| match f {
                FieldPlan::Enumerated { name, values } if name == "status" => Some(values),
                _ => None,
            })
            .expect("status should be enumerated");

        let rendered: Vec<&str> = enumerated.iter().filter_map(Value::as_str).collect();
        assert_eq!(rendered, vec!["ok", "retry", "failed"]);
    }

    #[test]
    fn an_optional_low_cardinality_field_is_not_enumerated() {
        // Describing it by value set alone would need a "on some records"
        // qualification that costs more than the enumeration saves.
        let mut records: Vec<String> = (0..29)
            .map(|i| format!(r#"{{"id":{i},"kind":"file"}}"#))
            .collect();
        records.push(r#"{"id":29,"kind":"file","tag":"x"}"#.into());
        let p = make_plan(&format!("[{}]", records.join(","))).expect("should plan");

        assert!(
            !p.fields.iter().any(|f| f.name() == "tag"),
            "optional field should not be enumerated"
        );
    }

    // ---- anchors ----

    #[test]
    fn outliers_are_always_anchored() {
        let mut records: Vec<String> = (0..60)
            .map(|i| format!(r#"{{"id":{i},"kind":"file","status":"ok"}}"#))
            .collect();
        records.push(r#"{"id":60,"kind":"file","status":"fail","error":"boom"}"#.into());
        let source = format!("[{}]", records.join(","));

        let doc = Document::parse(&source, &config()).unwrap();
        let stats = analyze_record_set(&doc, &config()).unwrap();
        let outliers = rank_outliers(&doc, &stats, &config());
        let p = plan(&doc, &stats, &outliers, &config()).unwrap();

        assert!(!outliers.is_empty());
        for outlier in &outliers {
            assert!(
                p.keeps(outlier.record),
                "outlier {} was not anchored",
                outlier.record
            );
        }
    }

    #[test]
    fn outliers_are_never_dropped_to_fit_the_sample_budget() {
        // More outliers than sample_records. The sample yields; the outliers do not.
        let mut records: Vec<String> = (0..40)
            .map(|i| format!(r#"{{"id":{i},"kind":"file","status":"ok"}}"#))
            .collect();
        for i in 40..50 {
            records.push(format!(
                r#"{{"id":{i},"kind":"file","status":"ok","error":"failure {i}"}}"#
            ));
        }
        let source = format!("[{}]", records.join(","));

        let doc = Document::parse(&source, &config()).unwrap();
        let stats = analyze_record_set(&doc, &config()).unwrap();
        let outliers = rank_outliers(&doc, &stats, &config());
        let p = plan(&doc, &stats, &outliers, &config()).unwrap();

        assert!(outliers.len() > config().sample_records);
        for outlier in &outliers {
            assert!(
                p.keeps(outlier.record),
                "outlier {} dropped",
                outlier.record
            );
        }
    }

    #[test]
    fn a_record_both_sampled_and_anomalous_appears_once() {
        // Record 0 is in the head sample and is also the outlier.
        let mut records = vec![r#"{"id":0,"kind":"file","error":"boom"}"#.to_string()];
        records.extend((1..40).map(|i| format!(r#"{{"id":{i},"kind":"file"}}"#)));
        let source = format!("[{}]", records.join(","));

        let doc = Document::parse(&source, &config()).unwrap();
        let stats = analyze_record_set(&doc, &config()).unwrap();
        let outliers = rank_outliers(&doc, &stats, &config());
        let p = plan(&doc, &stats, &outliers, &config()).unwrap();

        assert_eq!(
            p.anchors.iter().filter(|&&a| a == 0).count(),
            1,
            "record 0 duplicated in {:?}",
            p.anchors
        );
    }

    #[test]
    fn anchors_are_sorted_and_deduplicated() {
        let p = make_plan(&uniform(80)).expect("should plan");
        let mut sorted = p.anchors.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(p.anchors, sorted);
    }

    // ---- declining ----

    #[test]
    fn too_few_records_declines() {
        assert!(make_plan(r#"[{"a":1,"kind":"x"},{"a":2,"kind":"x"}]"#).is_none());
    }

    #[test]
    fn an_array_that_is_entirely_outliers_declines() {
        // Every record anomalous means every record is anchored, so there is nothing
        // to elide and a marker to pay for. "Compressing" this to itself plus a
        // header would be strictly worse.
        let records: Vec<String> = (0..8)
            .map(|i| {
                format!(
                    r#"{{"id":{i},"error":"distinct failure {i}","payload":"{}"}}"#,
                    "x".repeat(i * 40)
                )
            })
            .collect();
        let source = format!("[{}]", records.join(","));

        let doc = Document::parse(&source, &config()).unwrap();
        let stats = analyze_record_set(&doc, &config()).unwrap();
        let outliers = rank_outliers(&doc, &stats, &config());

        if outliers.len() >= stats.records {
            assert!(plan(&doc, &stats, &outliers, &config()).is_none());
        }
    }

    #[test]
    fn nothing_to_say_and_nothing_to_elide_declines() {
        // No constant or enumerable fields, and most records anchored.
        let records: Vec<String> = (0..6)
            .map(|i| format!(r#"{{"a":{i},"b":{},"c":{}}}"#, i * 7, i * 13))
            .collect();
        let p = make_plan(&format!("[{}]", records.join(",")));

        if let Some(p) = p {
            // If it did plan, it must at least be eliding a clear majority.
            assert!(p.elided() * 2 > p.total_records);
        }
    }

    #[test]
    fn non_record_input_declines() {
        assert!(make_plan(r#"{"a":1}"#).is_none());
        assert!(make_plan("[]").is_none());
        assert!(make_plan("[1,2,3]").is_none());
    }

    // ---- invariants ----

    #[test]
    fn planning_mutates_nothing() {
        let source = uniform(60);
        let doc = Document::parse(&source, &config()).unwrap();
        let before = doc.to_json().unwrap();

        let stats = analyze_record_set(&doc, &config()).unwrap();
        let outliers = rank_outliers(&doc, &stats, &config());
        let _ = plan(&doc, &stats, &outliers, &config());

        assert_eq!(doc.to_json().unwrap(), before);
    }

    #[test]
    fn planning_is_deterministic() {
        let source = uniform(70);
        let first = make_plan(&source);
        for _ in 0..50 {
            assert_eq!(make_plan(&source), first);
        }
    }

    #[test]
    fn keeps_agrees_with_the_anchor_list() {
        let p = make_plan(&uniform(60)).expect("should plan");
        for record in 0..p.total_records {
            assert_eq!(
                p.keeps(record),
                p.anchors.contains(&record),
                "record {record}"
            );
        }
    }
}
