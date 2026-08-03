//! Outlier detection — deciding which records must survive verbatim.
//!
//! The analyzer answers "what do these records look like on the whole". This answers
//! the complementary and more consequential question: **which specific records must
//! not be summarized away?**
//!
//! Summarizing 500 near-identical records is only safe if the ones that are *not*
//! near-identical are kept. In agent tool output the interesting record is almost
//! always the anomalous one — the failed test among 200 passes, the file with a
//! permission error, the request that took 30 seconds when the rest took 30
//! milliseconds. Compressing that away yields output that is smaller, cheaper, and
//! useless.
//!
//! # Scores are integers on purpose
//!
//! Ranking must be deterministic down to tie-breaking: two records with equal scores
//! must always order the same way, or the compressed bytes vary between runs and bust
//! the provider's prompt cache (invariant I4).
//!
//! Scores are therefore accumulated as fixed-point integers rather than `f64`, and
//! ties break on record index. Sorting floats needs `partial_cmp` and a decision
//! about `NaN`; an integer score has a total order by construction and there is no
//! `NaN` to decide about.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{CrushConfig, Document, FieldKind, RecordSetStats};

/// Fixed-point scale. A score of `SCALE` means "one full signal fired".
const SCALE: u64 = 1000;

/// Field names that mark a record as having gone wrong.
///
/// Matched case-insensitively against the whole field name. Substring matching would
/// fire on `error_rate` and `terror`, which is the wrong kind of eager.
const ERROR_FIELD_NAMES: [&str; 10] = [
    "error",
    "err",
    "errors",
    "exception",
    "failure",
    "failed",
    "stderr",
    "traceback",
    "panic",
    "fault",
];

/// Why a record was flagged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutlierReason {
    /// Holds a rare value of a field whose values are otherwise few and repeated.
    RareValue {
        /// The field.
        field: String,
        /// How many records share this record's value.
        shared_by: usize,
    },
    /// Carries a field most records lack.
    RareField {
        /// The field.
        field: String,
        /// How many records carry it.
        present_in: usize,
    },
    /// Carries an error-shaped field its peers lack.
    ///
    /// Weighted above a plain rare field. A record that failed is the record the
    /// model most needs to see, and it is also the one a naive summarizer is most
    /// likely to drop, because failures are by definition the minority.
    ErrorField {
        /// The field.
        field: String,
    },
    /// A numeric value far from the rest of its column.
    NumericOutlier {
        /// The field.
        field: String,
    },
    /// Materially larger than its peers.
    SizeOutlier {
        /// Serialized size in bytes.
        bytes: usize,
    },
}

/// A record and how much it stands out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outlier {
    /// Index into the original array.
    pub record: usize,
    /// Accumulated score. Higher stands out more.
    pub score: u64,
    /// What contributed, in a stable order.
    pub reasons: Vec<OutlierReason>,
}

/// Ranks records by how much they stand out, most anomalous first.
///
/// Records that stand out in no way at all are omitted rather than ranked last, so an
/// array of genuinely uniform records yields an empty result instead of an arbitrary
/// pick.
///
/// # Example
///
/// ```
/// use headroom_core::smart_crusher::{
///     analyze_record_set, rank_outliers, CrushConfig, Document,
/// };
///
/// let config = CrushConfig::default();
/// let doc = Document::parse(
///     r#"[{"t":"a","ok":true},{"t":"b","ok":true},{"t":"c","ok":false}]"#,
///     &config,
/// ).unwrap();
/// let stats = analyze_record_set(&doc, &config).unwrap();
///
/// let ranked = rank_outliers(&doc, &stats, &config);
/// assert_eq!(ranked[0].record, 2); // the one that is not ok
/// ```
pub fn rank_outliers(
    document: &Document,
    stats: &RecordSetStats,
    config: &CrushConfig,
) -> Vec<Outlier> {
    let Value::Array(items) = document.value() else {
        return Vec::new();
    };

    let records: Vec<&serde_json::Map<String, Value>> =
        items.iter().filter_map(Value::as_object).collect();
    if records.len() != items.len() || records.len() < 2 {
        return Vec::new();
    }

    let total = records.len();
    let mut scores: Vec<(u64, Vec<OutlierReason>)> = (0..total).map(|_| (0, Vec::new())).collect();

    // Field order comes from `stats`, which is document order, so reasons accumulate
    // in a stable sequence across runs.
    for stat in &stats.fields {
        match &stat.kind {
            // A constant field distinguishes nothing — every record agrees.
            FieldKind::Constant { .. } => {}

            // A unique field distinguishes every record equally, which is to say not
            // at all. Scoring it would rank all records identically and add noise.
            FieldKind::Unique => {}

            FieldKind::LowCardinality { .. } | FieldKind::Varied { .. } => {
                score_rare_values(&records, &stat.name, total, &mut scores);
                score_numeric_outliers(&records, &stat.name, &mut scores);
            }
        }

        // Presence rarity is independent of value distribution: a field only two of
        // 200 records carry marks those two out whatever their values are.
        if stat.present_in < total {
            score_rare_presence(&records, stat, total, config, &mut scores);
        }
    }

    score_size_outliers(&records, &mut scores);

    let mut ranked: Vec<Outlier> = scores
        .into_iter()
        .enumerate()
        .filter(|(_, (score, _))| *score > 0)
        .map(|(record, (score, reasons))| Outlier {
            record,
            score,
            reasons,
        })
        .collect();

    // Descending by score, ties broken on record index. The tie-break is what makes
    // this a total order rather than merely a sort — see the module docs on I4.
    ranked.sort_by(|a, b| b.score.cmp(&a.score).then(a.record.cmp(&b.record)));
    ranked
}

/// Scores records holding a rare value of `field`.
fn score_rare_values(
    records: &[&serde_json::Map<String, Value>],
    field: &str,
    total: usize,
    scores: &mut [(u64, Vec<OutlierReason>)],
) {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for record in records {
        if let Some(value) = record.get(field) {
            *counts.entry(serialize(value)).or_insert(0) += 1;
        }
    }

    for (index, record) in records.iter().enumerate() {
        let Some(value) = record.get(field) else {
            continue;
        };
        let shared_by = counts.get(&serialize(value)).copied().unwrap_or(0);

        // A value the majority shares is not remarkable. Requiring a strict minority
        // keeps this from firing on every record of a two-valued field split evenly.
        if shared_by == 0 || shared_by * 2 >= total {
            continue;
        }

        // Rarer scores higher: a value held by 1 of 200 outranks one held by 50.
        let weight = SCALE * (total - shared_by) as u64 / total as u64;
        scores[index].0 += weight;
        scores[index].1.push(OutlierReason::RareValue {
            field: field.to_owned(),
            shared_by,
        });
    }
}

/// Scores records carrying a field most records lack.
fn score_rare_presence(
    records: &[&serde_json::Map<String, Value>],
    stat: &super::FieldStat,
    total: usize,
    config: &CrushConfig,
    scores: &mut [(u64, Vec<OutlierReason>)],
) {
    let is_error_field = config.preserve_error_fields && is_error_name(&stat.name);
    let weight = SCALE * (total - stat.present_in) as u64 / total as u64;

    for (index, record) in records.iter().enumerate() {
        if !record.contains_key(&stat.name) {
            continue;
        }

        if is_error_field {
            // Doubled, and deliberately so. A failed record is what the model most
            // needs to see, and it is also what a naive summarizer is most likely to
            // drop, since failures are by definition the minority.
            scores[index].0 += weight * 2;
            scores[index].1.push(OutlierReason::ErrorField {
                field: stat.name.clone(),
            });
        } else {
            scores[index].0 += weight;
            scores[index].1.push(OutlierReason::RareField {
                field: stat.name.clone(),
                present_in: stat.present_in,
            });
        }
    }
}

/// Scores numeric values far from their column's centre.
///
/// Uses the median and the median absolute deviation rather than mean and standard
/// deviation. Tool output distributions are routinely skewed — latencies, file sizes,
/// row counts — and a single extreme value drags the mean toward itself, masking the
/// very outlier it should expose. The median does not move.
fn score_numeric_outliers(
    records: &[&serde_json::Map<String, Value>],
    field: &str,
    scores: &mut [(u64, Vec<OutlierReason>)],
) {
    let values: Vec<(usize, f64)> = records
        .iter()
        .enumerate()
        .filter_map(|(i, r)| r.get(field).and_then(Value::as_f64).map(|v| (i, v)))
        .filter(|(_, v)| v.is_finite())
        .collect();

    // Below four values there is no distribution to speak of, and "outlier" would
    // just mean "not the median".
    if values.len() < 4 {
        return;
    }

    let median = median_of(&values.iter().map(|(_, v)| *v).collect::<Vec<_>>());
    let deviations: Vec<f64> = values.iter().map(|(_, v)| (v - median).abs()).collect();
    let mad = median_of(&deviations);

    // A zero MAD means the majority share one exact value. Any deviation at all is
    // then remarkable, but scaling by zero is not, so handle it separately.
    for (index, value) in &values {
        let deviates = if mad > 0.0 {
            (value - median).abs() / mad > 3.5
        } else {
            (value - median).abs() > 0.0
        };

        if deviates {
            scores[*index].0 += SCALE;
            scores[*index].1.push(OutlierReason::NumericOutlier {
                field: field.to_owned(),
            });
        }
    }
}

/// Scores records materially larger than their peers.
fn score_size_outliers(
    records: &[&serde_json::Map<String, Value>],
    scores: &mut [(u64, Vec<OutlierReason>)],
) {
    let sizes: Vec<usize> = records
        .iter()
        .map(|r| serialize(&Value::Object((*r).clone())).len())
        .collect();

    let as_f64: Vec<f64> = sizes.iter().map(|s| *s as f64).collect();
    let median = median_of(&as_f64);
    if median <= 0.0 {
        return;
    }

    for (index, size) in sizes.iter().enumerate() {
        // Three times the median. A record carrying a stack trace among one-line
        // successes clears this comfortably; ordinary variation does not.
        if (*size as f64) > median * 3.0 {
            scores[index].0 += SCALE;
            scores[index]
                .1
                .push(OutlierReason::SizeOutlier { bytes: *size });
        }
    }
}

/// Median of `values`. Returns `0.0` for an empty slice.
fn median_of(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    // `total_cmp` rather than `partial_cmp`: a total order with no `NaN` decision to
    // make, so the sort is deterministic whatever the input.
    sorted.sort_by(|a, b| a.total_cmp(b));

    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    }
}

/// Whether `name` marks a record as having gone wrong.
fn is_error_name(name: &str) -> bool {
    ERROR_FIELD_NAMES
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

/// Deterministic string form of a value, for use as a map key.
fn serialize(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("{value:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smart_crusher::analyze_record_set;

    fn config() -> CrushConfig {
        CrushConfig::default()
    }

    fn rank(source: &str) -> Vec<Outlier> {
        let doc = Document::parse(source, &config()).expect("valid json");
        let Some(stats) = analyze_record_set(&doc, &config()) else {
            return Vec::new();
        };
        rank_outliers(&doc, &stats, &config())
    }

    #[test]
    fn the_one_failure_among_many_passes_ranks_first() {
        // The case this module exists for. 199 passes and one failure: compressing
        // the failure away produces output that is smaller, cheaper, and useless.
        let mut records: Vec<String> = (0..199)
            .map(|i| format!(r#"{{"test":"t{i}","status":"pass"}}"#))
            .collect();
        records.push(r#"{"test":"t199","status":"fail","error":"assertion failed"}"#.into());

        let ranked = rank(&format!("[{}]", records.join(",")));

        assert!(!ranked.is_empty());
        assert_eq!(ranked[0].record, 199, "the failure must rank first");
        assert!(ranked[0]
            .reasons
            .iter()
            .any(|r| matches!(r, OutlierReason::ErrorField { .. })));
    }

    #[test]
    fn uniform_records_produce_no_outliers() {
        // No arbitrary pick. If nothing stands out, nothing is reported.
        let records: Vec<String> = (0..20)
            .map(|_| r#"{"status":"ok","kind":"file"}"#.to_string())
            .collect();
        assert!(rank(&format!("[{}]", records.join(","))).is_empty());
    }

    #[test]
    fn all_distinct_records_produce_no_outliers() {
        // Every record unique means no record is unusual. A unique field
        // distinguishes all records equally, which is to say not at all.
        let records: Vec<String> = (0..20).map(|i| format!(r#"{{"id":{i}}}"#)).collect();
        assert!(rank(&format!("[{}]", records.join(","))).is_empty());
    }

    #[test]
    fn a_rare_value_of_a_low_cardinality_field_is_flagged() {
        let mut records: Vec<String> = (0..19)
            .map(|i| format!(r#"{{"n":{i},"status":"ok"}}"#))
            .collect();
        records.push(r#"{"n":99,"status":"degraded"}"#.into());

        let ranked = rank(&format!("[{}]", records.join(",")));
        assert_eq!(ranked[0].record, 19);
        assert!(ranked[0]
            .reasons
            .iter()
            .any(|r| matches!(r, OutlierReason::RareValue { .. })));
    }

    #[test]
    fn a_majority_value_is_not_treated_as_rare() {
        // An evenly split two-valued field must not flag every record.
        let records: Vec<String> = (0..20)
            .map(|i| format!(r#"{{"n":{i},"flag":{}}}"#, i % 2 == 0))
            .collect();
        let ranked = rank(&format!("[{}]", records.join(",")));

        assert!(
            !ranked.iter().any(|o| o
                .reasons
                .iter()
                .any(|r| matches!(r, OutlierReason::RareValue { .. }))),
            "an even split is not rarity"
        );
    }

    #[test]
    fn an_error_field_outranks_an_ordinary_rare_field() {
        // Both records carry a field the others lack; the error-shaped one must win.
        let mut records: Vec<String> = (0..18).map(|i| format!(r#"{{"id":{i},"v":1}}"#)).collect();
        records.push(r#"{"id":18,"v":1,"note":"just a note"}"#.into());
        records.push(r#"{"id":19,"v":1,"error":"it broke"}"#.into());

        let ranked = rank(&format!("[{}]", records.join(",")));
        assert_eq!(ranked[0].record, 19, "the error record must rank first");
    }

    #[test]
    fn error_field_names_match_whole_words_only() {
        // `error_rate` is a metric, not a failure. Substring matching would fire on
        // it and rank ordinary telemetry as anomalous.
        assert!(is_error_name("error"));
        assert!(is_error_name("ERROR"));
        assert!(is_error_name("stderr"));
        assert!(!is_error_name("error_rate"));
        assert!(!is_error_name("terror"));
        assert!(!is_error_name("mirror"));
    }

    #[test]
    fn a_numeric_outlier_is_found_without_assuming_normality() {
        // A skewed latency distribution with one extreme value. Mean-and-sigma would
        // be dragged toward the outlier and could miss it; median-and-MAD is not.
        let mut records: Vec<String> = (0..20)
            .map(|i| format!(r#"{{"id":{i},"ms":{}}}"#, 30 + i % 3))
            .collect();
        records.push(r#"{"id":20,"ms":30000}"#.into());

        let ranked = rank(&format!("[{}]", records.join(",")));
        assert_eq!(ranked[0].record, 20);
        assert!(ranked[0]
            .reasons
            .iter()
            .any(|r| matches!(r, OutlierReason::NumericOutlier { .. })));
    }

    #[test]
    fn a_much_larger_record_is_flagged_on_size() {
        let mut records: Vec<String> = (0..20)
            .map(|i| format!(r#"{{"id":{i},"msg":"ok"}}"#))
            .collect();
        let big = "x".repeat(500);
        records.push(format!(r#"{{"id":20,"msg":"{big}"}}"#));

        let ranked = rank(&format!("[{}]", records.join(",")));
        assert_eq!(ranked[0].record, 20);
        assert!(ranked[0]
            .reasons
            .iter()
            .any(|r| matches!(r, OutlierReason::SizeOutlier { .. })));
    }

    #[test]
    fn too_few_numbers_to_have_a_distribution_are_left_alone() {
        // With three values, "outlier" would just mean "not the median".
        let ranked = rank(r#"[{"a":1,"n":1},{"a":2,"n":2},{"a":3,"n":900}]"#);
        assert!(
            !ranked.iter().any(|o| o
                .reasons
                .iter()
                .any(|r| matches!(r, OutlierReason::NumericOutlier { .. }))),
            "three values is not a distribution"
        );
    }

    #[test]
    fn two_record_arrays_behave_sanely() {
        // No panic, no nonsense. Two records cannot really have an outlier.
        let ranked = rank(r#"[{"a":1},{"a":2}]"#);
        assert!(ranked.len() <= 2);
    }

    #[test]
    fn non_record_input_yields_nothing() {
        assert!(rank(r#"{"a":1}"#).is_empty());
        assert!(rank("[]").is_empty());
        assert!(rank("[1,2,3]").is_empty());
    }

    #[test]
    fn ties_break_on_record_index() {
        // Two records identical in their anomaly must always order the same way.
        // Without the tie-break the order would depend on sort internals, and the
        // compressed bytes would vary between runs.
        let mut records: Vec<String> = (0..18).map(|i| format!(r#"{{"id":{i},"v":1}}"#)).collect();
        records.push(r#"{"id":18,"v":1,"extra":"same"}"#.into());
        records.push(r#"{"id":19,"v":1,"extra":"same"}"#.into());

        let ranked = rank(&format!("[{}]", records.join(",")));
        let tied: Vec<usize> = ranked
            .iter()
            .filter(|o| o.score == ranked[0].score)
            .map(|o| o.record)
            .collect();

        let mut sorted = tied.clone();
        sorted.sort_unstable();
        assert_eq!(tied, sorted, "tied records must be in index order");
    }

    #[test]
    fn ranking_is_deterministic() {
        // Invariant I4, the property the integer scoring exists to guarantee.
        let mut records: Vec<String> = (0..30)
            .map(|i| format!(r#"{{"id":{i},"status":"ok","ms":{}}}"#, 10 + i % 4))
            .collect();
        records.push(r#"{"id":30,"status":"fail","ms":9000,"error":"boom"}"#.into());
        let source = format!("[{}]", records.join(","));

        let first = rank(&source);
        for _ in 0..50 {
            assert_eq!(rank(&source), first);
        }
    }
}
