//! Telemetry that observes and never advises — invariant I9.
//!
//! # The API this module deliberately does not have
//!
//! There is no `Telemetry::hint_for(&request)`. No method here returns anything a
//! compressor could consult while a request is in flight, and that absence is the
//! design rather than an omission waiting to be filled.
//!
//! The reason is invariant I4. Compression must be deterministic: the same bytes, the
//! same frozen count and the same auth mode must always produce byte-equal output. A
//! request-time hint API breaks that immediately — the same request compresses
//! differently depending on what happened to be observed before it, and the output
//! stops being a function of the input. Debugging becomes impossible in the specific
//! way where a failure cannot be reproduced from the failing request alone.
//!
//! So telemetry flows one way at runtime, and the loop closes only at startup:
//! observations are aggregated, published to a file, and read back when the process
//! next boots ([`Recommendations`]). A recommendation is a *configuration input*, fixed
//! for the process lifetime, not a live signal.
//!
//! # Aggregation key
//!
//! Observations are keyed by `(auth_mode, model_family, structure_hash)`. Raw content
//! never enters the key: a structure hash describes the *shape* of a payload, so two
//! customers sending structurally identical but textually different data aggregate
//! together, and no key can be reversed into what anyone sent.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth_mode::AuthMode;
use crate::detection::ContentType;

/// A structural fingerprint of a payload.
///
/// # Why structure rather than content
///
/// Two tool results listing different files have identical structure and should
/// aggregate together — the thing worth learning is "arrays of flat objects with these
/// keys compress well", not anything about the particular files.
///
/// It also means the key carries no customer data. A content hash would be a
/// pseudonymous identifier for an exact payload; a structure hash is not reversible
/// into one, because every value has already been discarded before hashing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StructureHash(u64);

impl StructureHash {
    /// Computes the structural fingerprint of a JSON payload.
    ///
    /// Non-JSON input hashes its content type alone, which is coarse but honest — the
    /// alternative is inventing structure that is not there.
    pub fn of(content: &str, content_type: ContentType) -> Self {
        let Ok(value) = serde_json::from_str::<Value>(content) else {
            return Self(fnv1a(content_type.as_str().as_bytes()));
        };

        let mut shape = String::new();
        describe(&value, &mut shape, 0);
        Self(fnv1a(shape.as_bytes()))
    }

    /// The fingerprint as a stable hex string.
    pub fn as_hex(self) -> String {
        format!("{:016x}", self.0)
    }
}

/// Maximum nesting depth the shape description walks.
///
/// Deeply nested payloads are rare and their tails carry little signal, but the real
/// reason for the bound is that a self-referential structure would otherwise recurse
/// forever. `serde_json` cannot build a cycle, so this is belt and braces — which is
/// what a bound on a recursive walk over untrusted input should be.
const MAX_SHAPE_DEPTH: usize = 8;

/// Writes a structural description of `value`, discarding every scalar value.
fn describe(value: &Value, out: &mut String, depth: usize) {
    if depth >= MAX_SHAPE_DEPTH {
        out.push('…');
        return;
    }

    match value {
        Value::Null => out.push('n'),
        Value::Bool(_) => out.push('b'),
        Value::Number(_) => out.push('#'),
        Value::String(_) => out.push('s'),
        Value::Array(items) => {
            out.push('[');
            // Only the first element. A homogeneous array of 10,000 records has the
            // same shape as one of 10, and including every element would make the
            // fingerprint depend on length — which is exactly the thing that varies
            // between two payloads that should aggregate together.
            if let Some(first) = items.first() {
                describe(first, out, depth + 1);
            }
            out.push(']');
        }
        Value::Object(members) => {
            out.push('{');
            // Keys sorted, so two payloads that differ only in member order aggregate
            // together. Keys are structure; values are content.
            let mut keys: Vec<&String> = members.keys().collect();
            keys.sort();
            for key in keys {
                let _ = write!(out, "{key}:");
                if let Some(member) = members.get(key) {
                    describe(member, out, depth + 1);
                }
                out.push(',');
            }
            out.push('}');
        }
    }
}

/// FNV-1a, 64-bit.
///
/// Chosen because it is deterministic across processes and platforms, which the
/// standard library's `DefaultHasher` explicitly is not — its output may change
/// between releases, so a recommendations file written by one build would key
/// differently under the next.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// What a set of observations is grouped by.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AggregationKey {
    /// How the request authenticated.
    pub auth_mode: &'static str,
    /// The model family, e.g. `claude-opus`.
    pub model_family: String,
    /// The payload's structural fingerprint.
    pub structure_hash: StructureHash,
}

impl AggregationKey {
    /// Builds a key.
    pub fn new(auth_mode: AuthMode, model: &str, structure_hash: StructureHash) -> Self {
        Self {
            auth_mode: auth_mode.as_str(),
            model_family: model_family(model),
            structure_hash,
        }
    }

    /// A stable string form, for use as a map key in a serialized file.
    pub fn as_str(&self) -> String {
        format!(
            "{}|{}|{}",
            self.auth_mode,
            self.model_family,
            self.structure_hash.as_hex()
        )
    }
}

/// Reduces a model identifier to its family.
///
/// `claude-opus-4-20250514` becomes `claude-opus`. Aggregating per exact model would
/// split the data across every point release and leave each bucket too small to say
/// anything — and a point release rarely changes how a payload compresses.
pub fn model_family(model: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for segment in model.split(['-', '.', ':']) {
        // Stop at the first version-looking segment. Everything after it is a release
        // identifier rather than a family name.
        if segment.is_empty() || segment.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            break;
        }
        parts.push(segment);
        if parts.len() == 2 {
            break;
        }
    }

    if parts.is_empty() {
        return "unknown".into();
    }
    parts.join("-")
}

/// One observation of a compression outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Observation {
    /// Times this shape was seen.
    pub samples: u64,
    /// Tokens before compression, summed.
    pub tokens_before: u64,
    /// Tokens after compression, summed.
    pub tokens_after: u64,
    /// Times a compressor declined or failed to help.
    pub declines: u64,
}

impl Observation {
    /// The mean compression ratio, in `0.0..=1.0`, or `None` with no data.
    ///
    /// `None` rather than `1.0` before anything has been seen. A ratio of "no
    /// compression" and "no data" are different claims, and a caller choosing whether
    /// to enable a compressor should not have to guess which one it is reading.
    pub fn ratio(&self) -> Option<f64> {
        (self.tokens_before > 0)
            .then(|| 1.0 - (self.tokens_after as f64 / self.tokens_before as f64))
    }
}

/// The observation-only telemetry sink.
///
/// # No request-time reads
///
/// Every method here takes `&mut self` and returns nothing a compressor could act on.
/// That is the shape of the trait, not an accident of the current implementations: an
/// implementor cannot add a request-time hint without changing this definition, which
/// is where the invariant is meant to be defended.
pub trait Telemetry {
    /// Records a compression outcome.
    fn record(&mut self, key: &AggregationKey, tokens_before: u64, tokens_after: u64);

    /// Records that a compressor declined or did not help.
    fn record_decline(&mut self, key: &AggregationKey);
}

/// An in-memory aggregator.
#[derive(Debug, Clone, Default)]
pub struct Aggregator {
    observations: BTreeMap<String, Observation>,
}

impl Aggregator {
    /// Creates an empty aggregator.
    pub fn new() -> Self {
        Self::default()
    }

    /// The observations gathered so far.
    pub fn observations(&self) -> &BTreeMap<String, Observation> {
        &self.observations
    }

    /// Folds another aggregator's observations into this one.
    ///
    /// Sums rather than replaces, because two deployments observing the same shape have
    /// each seen real traffic and the merged sample count is the honest total. Replacing
    /// would silently discard whichever side was imported second.
    ///
    /// Exists for aggregate interchange between deployments: a fleet of proxies each
    /// learning in isolation cannot pool what it learned without this, which is most of
    /// why TOIN is worth having across more than one machine.
    pub fn merge(&mut self, other: &Aggregator) {
        for (key, incoming) in &other.observations {
            let entry = self.observations.entry(key.clone()).or_default();
            entry.samples = entry.samples.saturating_add(incoming.samples);
            entry.tokens_before = entry.tokens_before.saturating_add(incoming.tokens_before);
            entry.tokens_after = entry.tokens_after.saturating_add(incoming.tokens_after);
            entry.declines = entry.declines.saturating_add(incoming.declines);
        }
    }

    /// Rebuilds an aggregator from exported observations.
    ///
    /// Used by the import path, which reads data from outside this process. Unparseable
    /// input yields an empty aggregator rather than an error: an import that fails must
    /// not be able to take down the endpoint, and merging nothing is the safe outcome.
    pub fn from_observations(observations: BTreeMap<String, Observation>) -> Self {
        Self { observations }
    }

    /// Builds recommendations from what has been observed.
    ///
    /// `min_samples` is the floor below which a key is omitted entirely. A shape seen
    /// twice says nothing, and publishing it as a recommendation gives a number the
    /// authority of a measurement it has not earned.
    pub fn recommend(&self, min_samples: u64) -> Recommendations {
        let entries = self
            .observations
            .iter()
            .filter(|(_, observation)| observation.samples >= min_samples)
            .map(|(key, observation)| {
                // A shape that *never* compressed has no ratio, and publishing nothing
                // for it would be the wrong way round: that is precisely the shape worth
                // recording as not worth attempting. Absent data reads as "unmeasured",
                // and an unmeasured shape is always retried.
                let ratio = observation.ratio().unwrap_or(0.0);

                (
                    key.clone(),
                    Recommendation {
                        mean_ratio: ratio,
                        samples: observation.samples,
                        // A shape that mostly declines is worth not attempting: the work
                        // costs latency on every request and returns nothing.
                        worth_compressing: ratio > 0.10
                            && observation.declines * 2 < observation.samples,
                    },
                )
            })
            .collect();

        Recommendations { entries }
    }
}

impl Telemetry for Aggregator {
    fn record(&mut self, key: &AggregationKey, tokens_before: u64, tokens_after: u64) {
        let entry = self.observations.entry(key.as_str()).or_default();
        entry.samples += 1;
        entry.tokens_before += tokens_before;
        entry.tokens_after += tokens_after;
    }

    fn record_decline(&mut self, key: &AggregationKey) {
        let entry = self.observations.entry(key.as_str()).or_default();
        entry.samples += 1;
        entry.declines += 1;
    }
}

/// What was learned about one shape.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Recommendation {
    /// Mean fraction of tokens removed.
    pub mean_ratio: f64,
    /// How many observations back this.
    pub samples: u64,
    /// Whether compression is worth attempting for this shape.
    pub worth_compressing: bool,
}

/// Recommendations, as published and as read back at startup.
///
/// # Read once, at startup
///
/// This is a configuration input, fixed for the process lifetime. Consulting it per
/// request would make compression depend on accumulated history, which breaks
/// invariant I4 — the same request would compress differently depending on what came
/// before it, and a failure could not be reproduced from the failing request alone.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Recommendations {
    /// Keyed by [`AggregationKey::as_str`].
    #[serde(default)]
    pub entries: BTreeMap<String, Recommendation>,
}

impl Recommendations {
    /// Serializes for publication.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails, which for this shape means an
    /// unrepresentable float.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Parses a published file.
    ///
    /// # Malformed input yields empty, not an error
    ///
    /// A corrupt recommendations file must not stop the proxy starting. The file is an
    /// optimization; refusing to boot without a valid one would make a cache of
    /// statistics into a hard startup dependency.
    pub fn from_json_lossy(source: &str) -> Self {
        match serde_json::from_str(source) {
            Ok(parsed) => parsed,
            Err(err) => {
                tracing::warn!(%err, "recommendations file unreadable; starting with none");
                Self::default()
            }
        }
    }

    /// Whether compression is worth attempting for `key`.
    ///
    /// Unknown keys return `true`. A shape nobody has measured should be attempted and
    /// measured, not skipped — the alternative never gathers the data that would let it
    /// be skipped for a reason.
    pub fn worth_compressing(&self, key: &AggregationKey) -> bool {
        self.entries
            .get(&key.as_str())
            .map(|entry| entry.worth_compressing)
            .unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(model: &str, content: &str) -> AggregationKey {
        AggregationKey::new(
            AuthMode::PayAsYouGo,
            model,
            StructureHash::of(content, ContentType::Json),
        )
    }

    // ---- structure hashing ----

    #[test]
    fn structurally_identical_payloads_with_different_content_hash_the_same() {
        // The whole point of hashing structure. Two tool results listing different
        // files should aggregate together — the lesson is about the shape, not the
        // files.
        let a = r#"[{"path":"src/a.rs","size":100},{"path":"src/b.rs","size":200}]"#;
        let b = r#"[{"path":"lib/z.py","size":9999},{"path":"lib/y.py","size":1}]"#;

        assert_eq!(
            StructureHash::of(a, ContentType::Json),
            StructureHash::of(b, ContentType::Json)
        );
    }

    #[test]
    fn array_length_does_not_change_the_fingerprint() {
        // Otherwise every payload gets its own bucket and nothing ever aggregates.
        let short = r#"[{"a":1}]"#;
        let long = r#"[{"a":1},{"a":2},{"a":3},{"a":4},{"a":5}]"#;

        assert_eq!(
            StructureHash::of(short, ContentType::Json),
            StructureHash::of(long, ContentType::Json)
        );
    }

    #[test]
    fn member_order_does_not_change_the_fingerprint() {
        assert_eq!(
            StructureHash::of(r#"{"a":1,"b":"x"}"#, ContentType::Json),
            StructureHash::of(r#"{"b":"x","a":1}"#, ContentType::Json)
        );
    }

    #[test]
    fn genuinely_different_shapes_hash_differently() {
        // The over-correction to guard against: a fingerprint that collapses everything
        // aggregates perfectly and learns nothing.
        let shapes = [
            r#"[{"path":"a","size":1}]"#,
            r#"[{"path":"a"}]"#,
            r#"{"path":"a","size":1}"#,
            r#"[{"path":1,"size":1}]"#,
            r#"[]"#,
        ];

        let hashes: Vec<_> = shapes
            .iter()
            .map(|s| StructureHash::of(s, ContentType::Json))
            .collect();

        for (i, a) in hashes.iter().enumerate() {
            for (j, b) in hashes.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "{:?} and {:?} collided", shapes[i], shapes[j]);
                }
            }
        }
    }

    #[test]
    fn the_fingerprint_contains_no_customer_content() {
        // A content hash would be a pseudonymous identifier for an exact payload. This
        // discards every value before hashing, so it cannot be one.
        let secret = r#"{"api_key":"sk-ant-SECRET","email":"a@example.com"}"#;
        let benign = r#"{"api_key":"x","email":"y"}"#;

        assert_eq!(
            StructureHash::of(secret, ContentType::Json),
            StructureHash::of(benign, ContentType::Json),
            "values leaked into the fingerprint"
        );
    }

    #[test]
    fn hashing_is_stable_across_calls() {
        // FNV-1a rather than `DefaultHasher`, whose output may change between compiler
        // releases — a recommendations file written by one build would key differently
        // under the next.
        let payload = r#"[{"a":1,"b":[{"c":"x"}]}]"#;
        let first = StructureHash::of(payload, ContentType::Json);
        for _ in 0..25 {
            assert_eq!(StructureHash::of(payload, ContentType::Json), first);
        }
        assert_eq!(first.as_hex().len(), 16);
    }

    #[test]
    fn the_fingerprint_is_pinned_to_a_literal() {
        // Self-consistency and distinctness are not enough. A hasher seeded once per
        // process satisfies both — `of(x) == of(x)` every time within a run — and still
        // produces a different value in the next process. That was verified by swapping
        // FNV-1a for a `OnceLock<RandomState>`: the entire workspace test suite passed.
        //
        // The consequence is silent and total. `headroom learn` writes recommendations in
        // one process and the proxy reads them in another; the key contains this hash. If
        // it varied across processes every lookup would miss, and the measure-then-skip
        // loop would quietly stop working while looking healthy — compression re-attempted
        // forever on shapes already measured as useless.
        //
        // Pinning to a literal is what makes that a build failure instead. `ContentHash`
        // is pinned the same way and for the same reason.
        assert_eq!(
            StructureHash::of(r#"{"a":1,"b":[2,3]}"#, ContentType::Json).as_hex(),
            "b937411607289da4"
        );
        assert_eq!(
            StructureHash::of("not json at all", ContentType::Log).as_hex(),
            "125073191daf5431"
        );
    }

    #[test]
    fn deep_nesting_terminates() {
        let deep = format!("{}1{}", "[".repeat(200), "]".repeat(200));
        let _ = StructureHash::of(&deep, ContentType::Json);
    }

    #[test]
    fn non_json_hashes_by_content_type() {
        let a = StructureHash::of("2026-01-01 ERROR boom", ContentType::Log);
        let b = StructureHash::of("2026-06-06 ERROR other", ContentType::Log);
        assert_eq!(a, b);
        assert_ne!(a, StructureHash::of("not json", ContentType::Code));
    }

    // ---- model family ----

    #[test]
    fn a_model_identifier_reduces_to_its_family() {
        // Aggregating per exact model splits the data across every point release and
        // leaves each bucket too small to say anything.
        for (model, family) in [
            ("claude-opus-4-20250514", "claude-opus"),
            ("claude-opus-4-5", "claude-opus"),
            ("gpt-4o", "gpt"),
            ("gpt-4o-mini", "gpt"),
            ("claude-sonnet-5", "claude-sonnet"),
        ] {
            assert_eq!(model_family(model), family, "{model}");
        }
    }

    #[test]
    fn two_point_releases_of_one_model_aggregate_together() {
        assert_eq!(
            key("claude-opus-4-20250514", "{}").model_family,
            key("claude-opus-4-20260101", "{}").model_family
        );
    }

    #[test]
    fn an_unrecognizable_model_is_not_a_panic() {
        for model in ["", "4", "-", "1.2.3"] {
            assert!(!model_family(model).is_empty(), "{model:?}");
        }
    }

    // ---- aggregation ----

    #[test]
    fn observations_accumulate_under_one_key() {
        let mut aggregator = Aggregator::new();
        let key = key("claude-opus-4", r#"[{"a":1}]"#);

        aggregator.record(&key, 1000, 100);
        aggregator.record(&key, 1000, 300);

        let observation = aggregator.observations()[&key.as_str()];
        assert_eq!(observation.samples, 2);
        assert_eq!(observation.ratio(), Some(0.8));
    }

    #[test]
    fn a_ratio_is_absent_rather_than_perfect_before_any_data() {
        // `None`, not `1.0`. "No compression" and "no data" are different claims, and
        // a caller deciding whether to enable a compressor should not have to guess.
        assert_eq!(Observation::default().ratio(), None);
    }

    #[test]
    fn different_auth_modes_do_not_share_a_bucket() {
        // Policy gates which compressors run, so subscription traffic compresses
        // differently by design. Pooling them would average two unrelated populations.
        let hash = StructureHash::of(r#"[{"a":1}]"#, ContentType::Json);
        let payg = AggregationKey::new(AuthMode::PayAsYouGo, "claude-opus-4", hash);
        let sub = AggregationKey::new(AuthMode::Subscription, "claude-opus-4", hash);

        assert_ne!(payg.as_str(), sub.as_str());
    }

    #[test]
    fn the_aggregation_key_wire_format_is_pinned() {
        // This string *is* the key in `recommendations.json`. `headroom learn` writes the
        // file in one process and the proxy reads it in another, quite possibly from a
        // different build — so the separator, the field order, and the model-family
        // reduction are all wire format, not internal detail.
        //
        // Testing only that two keys differ (above) would pass through any format change:
        // swap `|` for `:`, reorder the fields, and every previously written
        // recommendation stops matching. Nothing errors; compression is simply
        // re-attempted forever on shapes already measured as useless.
        //
        // Same reasoning as `the_fingerprint_is_pinned_to_a_literal`, one layer out.
        let key = AggregationKey::new(
            AuthMode::PayAsYouGo,
            "claude-opus-4-20250514",
            StructureHash::of(r#"{"a":1,"b":[2,3]}"#, ContentType::Json),
        );

        assert_eq!(key.as_str(), "payg|claude-opus|b937411607289da4");
    }

    // ---- recommendations ----

    #[test]
    fn a_shape_seen_too_few_times_is_not_published() {
        // Publishing it would give a number the authority of a measurement it has not
        // earned.
        let mut aggregator = Aggregator::new();
        let key = key("claude-opus-4", r#"[{"a":1}]"#);
        aggregator.record(&key, 1000, 100);
        aggregator.record(&key, 1000, 100);

        assert!(aggregator.recommend(10).entries.is_empty());
        assert_eq!(aggregator.recommend(2).entries.len(), 1);
    }

    #[test]
    fn a_shape_that_never_compressed_is_published_as_not_worth_attempting() {
        // The case `learn` surfaced. Publishing nothing for an all-decline shape is the
        // wrong way round: absent data reads as "unmeasured", and an unmeasured shape is
        // always retried — so the one shape most worth skipping would be retried forever.
        let mut aggregator = Aggregator::new();
        let key = key("claude-opus-4", r#"[{"a":1}]"#);
        for _ in 0..6 {
            aggregator.record_decline(&key);
        }

        let recommendations = aggregator.recommend(5);
        assert_eq!(recommendations.entries.len(), 1, "nothing was published");
        assert!(!recommendations.worth_compressing(&key));
    }

    #[test]
    fn a_shape_that_mostly_declines_is_marked_not_worth_compressing() {
        // The work costs latency on every request and returns nothing.
        let mut aggregator = Aggregator::new();
        let key = key("claude-opus-4", r#"[{"a":1}]"#);
        aggregator.record(&key, 1000, 100);
        for _ in 0..9 {
            aggregator.record_decline(&key);
        }

        let recommendations = aggregator.recommend(1);
        assert!(!recommendations.worth_compressing(&key));
    }

    #[test]
    fn an_unmeasured_shape_is_worth_attempting() {
        // Skipping it would never gather the data that would let it be skipped for a
        // reason.
        let recommendations = Recommendations::default();
        assert!(recommendations.worth_compressing(&key("claude-opus-4", "{}")));
    }

    #[test]
    fn recommendations_survive_a_round_trip() {
        let mut aggregator = Aggregator::new();
        let key = key("claude-opus-4", r#"[{"a":1}]"#);
        for _ in 0..5 {
            aggregator.record(&key, 1000, 100);
        }

        let published = aggregator.recommend(1);
        let parsed = Recommendations::from_json_lossy(&published.to_json().unwrap());

        assert_eq!(parsed, published);
        assert!(parsed.worth_compressing(&key));
    }

    #[test]
    fn a_corrupt_recommendations_file_does_not_stop_startup() {
        // The file is an optimization. Refusing to boot without a valid one makes a
        // cache of statistics into a hard startup dependency.
        for source in ["", "{not json", "null", r#"{"entries":"wrong type"}"#] {
            let parsed = Recommendations::from_json_lossy(source);
            assert!(parsed.entries.is_empty(), "{source:?}");
            // And an empty set still answers the only question anyone asks of it.
            assert!(parsed.worth_compressing(&key("claude-opus-4", "{}")));
        }
    }

    #[test]
    fn the_trait_exposes_no_request_time_read() {
        // Invariant I9, asserted structurally. This compiles only while every
        // `Telemetry` method returns `()`, so adding a hint API means changing the
        // trait definition — which is where the invariant is meant to be defended.
        fn assert_write_only<T: Telemetry>(sink: &mut T, key: &AggregationKey) {
            let _: () = sink.record(key, 1, 1);
            let _: () = sink.record_decline(key);
        }

        assert_write_only(&mut Aggregator::new(), &key("claude-opus-4", "{}"));
    }
}
