//! Statistical detection of identifier and score fields.
//!
//! Two field shapes need telling apart, and their names cannot be trusted to do it.
//!
//! # The distinction
//!
//! - An **identifier** is near-unique by construction: `id`, `request_id`, `uuid`, a
//!   sequence number. Its values carry no ranking signal at all — record 199 is not more
//!   interesting than record 3 for having a larger id.
//! - A **score** is numeric and meaningfully distributed: `score`, `relevance`, `rank`,
//!   `similarity`. Its values are precisely a statement about which records matter, which
//!   makes it the one field a summarizer should be reading.
//!
//! # Why this is statistical and not a name match
//!
//! A name list fails on every schema that does not speak English, and on every field
//! named for its domain rather than its role — `bm25`, `puntuación`, `距離`, `w`. The
//! reference detects both statistically for the same reason, and this is a clean-room
//! implementation of that idea rather than a port of its thresholds.
//!
//! Names are used only as a **tiebreaker**, never as the primary signal, because a field
//! name is a convention and not a contract.
//!
//! # Why the identifier half matters
//!
//! It exists to stop the score half from firing on the wrong field. A numeric `id`
//! running `1..200` is numeric, non-constant and beautifully distributed — everything the
//! score detector looks for. Ranking by it would pin records 195–199 and call that a
//! summary of what mattered. Identifier detection is what makes score detection safe.

use serde_json::{Map, Value};

/// At or above this fraction of distinct values, a field is an identifier.
///
/// Not 1.0, deliberately. Exact uniqueness is already [`FieldKind::Unique`]; what this
/// catches is the *near*-unique case that falls through it — a request id duplicated
/// once by a retry, a sequence with a gap, a field one record happens to omit. Those are
/// identifiers in every sense that matters and nothing else in the analyzer treats them
/// as such.
///
/// [`FieldKind::Unique`]: super::FieldKind::Unique
const IDENTIFIER_RATIO: f64 = 0.9;

/// Fewest records before either detector will judge anything.
///
/// Below this, "90% distinct" is three values out of three and says nothing. A detector
/// that fires on tiny inputs is a detector that fires on noise.
const MIN_RECORDS: usize = 8;

/// Name fragments that make an already-plausible score field more likely.
///
/// Consulted only to break a tie between two equally plausible candidates. A field that
/// fails the statistical test is never promoted by its name.
const SCORE_HINTS: [&str; 8] = [
    "score",
    "rank",
    "relevance",
    "similarity",
    "confidence",
    "weight",
    "rating",
    "priority",
];

/// Name fragments that mark a field as an identifier regardless of distribution.
///
/// The tiebreaker in the other direction, and the more important one: a field named `id`
/// that happens to fall under the ratio is still an id, and treating it as a score would
/// rank records by an accident of numbering.
const IDENTIFIER_HINTS: [&str; 6] = ["_id", "id", "uuid", "guid", "hash", "seq"];

/// What role a field plays in deciding which records matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldRole {
    /// Near-unique. Carries no ranking signal.
    Identifier,
    /// Numeric and meaningfully distributed. Carries a ranking signal.
    Score,
    /// Neither. The ordinary case.
    Ordinary,
}

/// Classifies `field` from its values across `records`.
///
/// Pure and deterministic: the same records always yield the same role (I4). Values are
/// read in document order and no floating-point comparison decides anything a small
/// perturbation could flip — the ratio test is on counts, and the distribution test is a
/// strict inequality against a bound.
pub fn classify_field(records: &[&Map<String, Value>], field: &str) -> FieldRole {
    if records.len() < MIN_RECORDS {
        return FieldRole::Ordinary;
    }

    let present: Vec<&Value> = records
        .iter()
        .filter_map(|record| record.get(field))
        .collect();
    if present.len() < MIN_RECORDS {
        return FieldRole::Ordinary;
    }

    // The one place a name decides anything on its own, and it decides in the safe
    // direction: a field named like an identifier is never ranked by, whatever its values
    // look like. Without this veto a `checksum` or a fractional `shard_id` would reach
    // the score test and could pass it, and ranking records by an identifier is the
    // failure this module exists to prevent.
    if named_like_an_identifier(field) {
        return FieldRole::Identifier;
    }

    // Score is tested first, and the order is load-bearing rather than incidental.
    //
    // A relevance score is *also* near-unique — sixty hits with sixty distinct
    // similarities is the normal shape, not the exceptional one. Asking "is this an
    // identifier" first classifies every well-behaved score field as an id and the score
    // half never runs. Asking "is this a score" first costs nothing, because the score
    // test is strict about what it accepts: bounded spread, genuinely numeric, and at
    // least two independent signals.
    if is_score(&present, field) {
        return FieldRole::Score;
    }

    if is_identifier(&present, field) {
        return FieldRole::Identifier;
    }

    FieldRole::Ordinary
}

/// Whether the values are near-unique, or the name says identifier outright.
fn is_identifier(values: &[&Value], field: &str) -> bool {
    let mut seen: Vec<String> = values
        .iter()
        .map(|value| serde_json::to_string(value).unwrap_or_else(|_| format!("{value:?}")))
        .collect();
    seen.sort();
    seen.dedup();

    let ratio = seen.len() as f64 / values.len() as f64;
    if ratio >= IDENTIFIER_RATIO {
        return true;
    }

    named_like_an_identifier(field)
}

/// Whether the field's *name* marks it as an identifier.
///
/// A field named `id` with repeated values is still an identifier — a foreign key, say —
/// and ranking records by one would be ranking them by which row they point at.
fn named_like_an_identifier(field: &str) -> bool {
    let lower = field.to_lowercase();
    IDENTIFIER_HINTS
        .iter()
        .any(|hint| lower == *hint || lower.ends_with(hint))
}

/// Whether the values are numeric and spread widely enough to rank by.
///
/// Requires more than one distinct value — a numeric field where every record agrees
/// ranks nothing — and that the values are genuinely numbers rather than numeric-looking
/// strings, since a string `"10"` sorts before `"9"` and a ranking built on that is
/// wrong in a way nobody would notice.
fn is_score(values: &[&Value], field: &str) -> bool {
    let numbers: Vec<f64> = values.iter().filter_map(|value| value.as_f64()).collect();

    // Every present value has to be numeric. A field that is a number nine times and a
    // string once is not a score; it is a schema someone should look at.
    if numbers.len() != values.len() {
        return false;
    }

    let mut distinct = numbers
        .iter()
        .map(|value| value.to_bits())
        .collect::<Vec<_>>();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.len() < 2 {
        return false;
    }

    // A score is bounded in practice — a relevance, a probability, a rating. An unbounded
    // integer spread across thousands of values is far more likely a timestamp, a byte
    // count or an id that slipped the ratio test, and ranking by any of those keeps the
    // biggest rather than the best.
    let low = numbers.iter().copied().fold(f64::INFINITY, f64::min);
    let high = numbers.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if !(low.is_finite() && high.is_finite()) {
        return false;
    }

    let spread = high - low;
    let plausible_range = spread > 0.0 && spread <= 1000.0;

    // A fractional value is strong evidence: identifiers and counters are integers.
    let fractional = numbers.iter().any(|value| value.fract() != 0.0);

    let lower = field.to_lowercase();
    let named_like_a_score = SCORE_HINTS.iter().any(|hint| lower.contains(hint));

    // Any two of the three. The name alone is never enough — that is the whole point of
    // detecting this statistically — but it is allowed to confirm what the numbers
    // already suggest.
    let signals = [plausible_range, fractional, named_like_a_score]
        .iter()
        .filter(|present| **present)
        .count();

    signals >= 2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(raw: &str) -> Vec<Map<String, Value>> {
        serde_json::from_str::<Vec<Value>>(raw)
            .expect("fixture parses")
            .into_iter()
            .filter_map(|value| value.as_object().cloned())
            .collect()
    }

    fn refs(owned: &[Map<String, Value>]) -> Vec<&Map<String, Value>> {
        owned.iter().collect()
    }

    fn build(field: &str, values: impl Iterator<Item = String>) -> Vec<Map<String, Value>> {
        let body: Vec<String> = values
            .map(|value| format!(r#"{{"{field}":{value},"state":"ok"}}"#))
            .collect();
        records(&format!("[{}]", body.join(",")))
    }

    #[test]
    fn a_near_unique_field_is_an_identifier() {
        // The case exact-uniqueness misses: one duplicate from a retry, and the field
        // stops being `FieldKind::Unique` while remaining an identifier in every sense.
        let owned = build(
            "request_id",
            (0..40).map(|i| format!(r#""r-{}""#, if i == 5 { 4 } else { i })),
        );
        assert_eq!(
            classify_field(&refs(&owned), "request_id"),
            FieldRole::Identifier
        );
    }

    #[test]
    fn a_numeric_sequence_is_an_identifier_not_a_score() {
        // The reason the identifier half exists. `1..40` is numeric, non-constant and
        // perfectly distributed — everything the score detector looks for. Ranking by it
        // would pin the highest-numbered records and call that a summary.
        let owned = build("seq", (0..40).map(|i| i.to_string()));
        assert_eq!(classify_field(&refs(&owned), "seq"), FieldRole::Identifier);
    }

    #[test]
    fn a_relevance_field_is_a_score() {
        let owned = build(
            "relevance",
            (0..40).map(|i| format!("{:.3}", 1.0 - (i as f64) / 50.0)),
        );
        assert_eq!(classify_field(&refs(&owned), "relevance"), FieldRole::Score);
    }

    #[test]
    fn a_score_field_is_detected_without_a_helpful_name() {
        // The whole point of doing this statistically. `w` says nothing; the fractional
        // values in a bounded range say everything.
        let owned = build("w", (0..40).map(|i| format!("{:.2}", (i as f64) / 40.0)));
        assert_eq!(classify_field(&refs(&owned), "w"), FieldRole::Score);
    }

    #[test]
    fn a_timestamp_is_not_a_score() {
        // Numeric, non-constant, widely spread — and ranking by it keeps the newest
        // records rather than the best ones.
        let owned = build(
            "created_at",
            (0..40).map(|i| (1_700_000_000u64 + i * 3600).to_string()),
        );
        assert_ne!(
            classify_field(&refs(&owned), "created_at"),
            FieldRole::Score
        );
    }

    #[test]
    fn a_numeric_looking_string_is_not_a_score() {
        // `"10"` sorts before `"9"`, and a ranking built on that is wrong in a way
        // nobody would notice from the output.
        let owned = build("value", (0..40).map(|i| format!(r#""{}""#, i % 7)));
        assert_ne!(classify_field(&refs(&owned), "value"), FieldRole::Score);
    }

    #[test]
    fn a_constant_numeric_field_ranks_nothing() {
        let owned = build("weight", (0..40).map(|_| "1.0".to_owned()));
        assert_eq!(classify_field(&refs(&owned), "weight"), FieldRole::Ordinary);
    }

    #[test]
    fn a_fractional_id_field_is_still_an_identifier() {
        // The veto. Without it, a checksum or a fractional shard id reaches the score
        // test — bounded, fractional, two signals — and records get ranked by an
        // identifier, which is the failure this module exists to prevent.
        let owned = build(
            "shard_id",
            (0..40).map(|i| format!("{:.2}", (i as f64) / 40.0)),
        );
        assert_eq!(
            classify_field(&refs(&owned), "shard_id"),
            FieldRole::Identifier
        );
    }

    #[test]
    fn a_field_named_like_an_id_is_never_a_score() {
        // A foreign key: repeated values, bounded range, so the ratio test alone would
        // let it through. Ranking records by which row they point at is meaningless.
        let owned = build("owner_id", (0..40).map(|i| (i % 5).to_string()));
        assert_eq!(
            classify_field(&refs(&owned), "owner_id"),
            FieldRole::Identifier
        );
    }

    #[test]
    fn a_small_record_set_is_judged_ordinary() {
        // "90% distinct" over three values is three values. A detector that fires on
        // tiny inputs fires on noise.
        let owned = build("score", (0..4).map(|i| format!("0.{i}")));
        assert_eq!(classify_field(&refs(&owned), "score"), FieldRole::Ordinary);
    }

    #[test]
    fn an_absent_field_is_ordinary_rather_than_a_panic() {
        let owned = build("score", (0..40).map(|i| format!("0.{}", i % 9)));
        assert_eq!(
            classify_field(&refs(&owned), "nonexistent"),
            FieldRole::Ordinary
        );
    }

    #[test]
    fn classification_is_deterministic() {
        let owned = build(
            "relevance",
            (0..40).map(|i| format!("{:.3}", (i as f64) / 41.0)),
        );
        let first = classify_field(&refs(&owned), "relevance");
        for _ in 0..5 {
            assert_eq!(first, classify_field(&refs(&owned), "relevance"));
        }
    }
}
