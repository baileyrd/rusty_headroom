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

use super::{CrushConfig, Document, FieldKind, Outlier, RecordSetStats};

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
