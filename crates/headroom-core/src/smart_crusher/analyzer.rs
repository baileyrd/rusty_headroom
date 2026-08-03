//! Analysis — deciding what is worth doing about a document's structure.
//!
//! The IR describes *what a document is*. This module decides *what is worth doing
//! about it*, and is the step between structure and a compression plan.
//!
//! Two independent outputs:
//!
//! - [`classify`] names the overall pattern, from the shape alone.
//! - [`analyze_record_set`] computes per-field statistics, which needs the actual
//!   values and not just their shapes.
//!
//! # Determinism
//!
//! Cardinality counting keys on the serialized form of each value and accumulates in
//! a `BTreeMap`. Sorting is safe *here* — unlike in the IR, where a sorted map would
//! have reordered the document's own keys — because these counts drive decisions, not
//! output ordering. Field statistics themselves are returned in document order.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{CrushConfig, Document, Shape};

/// The overall pattern a document exhibits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    /// An array of consistently-shaped records. The headline case: describable far
    /// more cheaply than it is listable.
    RecordSet {
        /// How many records.
        records: usize,
    },
    /// A document whose weight is long strings rather than structure.
    ///
    /// Checked before [`Pattern::WideObject`] because a wide object full of prose is
    /// a CCR offload candidate, not a structural one — there is nothing for a
    /// structural compressor to exploit.
    ScalarHeavy {
        /// Total bytes held in strings.
        bytes: usize,
    },
    /// An object with many fields, typically envelope boilerplate around the part
    /// that matters.
    WideObject {
        /// How many top-level fields.
        fields: usize,
    },
    /// Much structure, few leaves. Usually not worth compressing at all — the
    /// brackets *are* the content.
    DeepNest {
        /// How deeply the document nests.
        depth: usize,
    },
    /// Nothing SmartCrusher knows how to exploit.
    ///
    /// An honest outcome rather than a failure. Most small documents land here, and
    /// the correct response is to decline and forward the original.
    Unremarkable,
}

/// Names the pattern `shape` exhibits.
///
/// Order matters: the checks run most-valuable-first, so a document that is both a
/// record set and deeply nested is reported as the record set.
///
/// # Example
///
/// ```
/// use headroom_core::smart_crusher::{classify, CrushConfig, Document, Pattern};
///
/// let config = CrushConfig::default();
/// let doc = Document::parse(r#"[{"id":1},{"id":2},{"id":3}]"#, &config).unwrap();
/// assert_eq!(classify(doc.shape(), &config), Pattern::RecordSet { records: 3 });
/// ```
pub fn classify(shape: &Shape, config: &CrushConfig) -> Pattern {
    if let Shape::Array { len, element } = shape {
        // A record set is specifically an array of *objects*. An array of 500
        // consistent integers is homogeneous but has no fields to summarize, so it
        // is not this pattern.
        if let Some(element) = element {
            if *len >= 2 && matches!(**element, Shape::Object { .. }) {
                return Pattern::RecordSet { records: *len };
            }
        }
    }

    let string_bytes = shape.string_bytes();
    if string_bytes >= config.scalar_heavy_bytes {
        return Pattern::ScalarHeavy {
            bytes: string_bytes,
        };
    }

    if let Shape::Object { fields } = shape {
        if fields.len() >= config.wide_object_fields {
            return Pattern::WideObject {
                fields: fields.len(),
            };
        }
    }

    // Deep-and-thin: lots of nesting carrying very little. Requiring more depth than
    // leaves is what separates a genuinely spindly document from an ordinary
    // three-level object, which is neither remarkable nor worth special handling.
    let depth = shape.depth();
    if depth >= 4 && depth > shape.leaf_count() {
        return Pattern::DeepNest { depth };
    }

    Pattern::Unremarkable
}

/// What a field looks like across a set of records.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    /// Present in every record with the same value.
    ///
    /// The highest-value finding: 500 repetitions collapse to one statement. Also
    /// the easiest to get wrong, which is why presence is checked as strictly as
    /// equality — see [`analyze_record_set`].
    Constant {
        /// The shared value.
        value: Value,
    },
    /// Few enough distinct values to be worth enumerating.
    LowCardinality {
        /// How many distinct values.
        distinct: usize,
    },
    /// A distinct value in every record — an identifier.
    ///
    /// Never elided. Identifiers are how the model refers back to a specific record,
    /// so summarizing them away costs it the ability to ask about anything it sees.
    Unique,
    /// Many distinct values, but not one per record.
    Varied {
        /// How many distinct values.
        distinct: usize,
    },
}

/// Statistics for one field across a record set.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldStat {
    /// The field name.
    pub name: String,
    /// What the field looks like across the records.
    pub kind: FieldKind,
    /// How many records actually carry this field.
    ///
    /// Less than the record count means the field is optional, which is why it
    /// cannot be [`FieldKind::Constant`] however uniform its present values are.
    pub present_in: usize,
}

/// Per-field statistics across an array of objects.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordSetStats {
    /// How many records were analyzed.
    pub records: usize,
    /// Field statistics, in first-appearance order.
    pub fields: Vec<FieldStat>,
}

impl RecordSetStats {
    /// Fields present in every record with an identical value.
    pub fn constant_fields(&self) -> impl Iterator<Item = &FieldStat> {
        self.fields
            .iter()
            .filter(|f| matches!(f.kind, FieldKind::Constant { .. }))
    }
}

/// Computes per-field statistics over an array of objects.
///
/// Returns `None` unless `document` is an array holding at least two objects.
///
/// # Why this accepts arrays the IR calls heterogeneous
///
/// [`Shape::is_record_set`] is deliberately strict: one record with a different
/// field set makes the whole array heterogeneous. That strictness is right for
/// deciding whether records are interchangeable, but wrong here. An array where 99
/// records have `error` and one does not is exactly the case field statistics exist
/// to describe — and refusing to analyze it would discard the most interesting
/// signal in the document.
///
/// So analysis is permissive and reports `present_in` per field; classification
/// stays strict.
///
/// # Example
///
/// ```
/// use headroom_core::smart_crusher::{analyze_record_set, CrushConfig, Document};
///
/// let config = CrushConfig::default();
/// let doc = Document::parse(
///     r#"[{"kind":"file","id":1},{"kind":"file","id":2}]"#,
///     &config,
/// ).unwrap();
///
/// let stats = analyze_record_set(&doc, &config).unwrap();
/// assert_eq!(stats.constant_fields().count(), 1); // "kind"
/// ```
pub fn analyze_record_set(document: &Document, config: &CrushConfig) -> Option<RecordSetStats> {
    let Value::Array(items) = document.value() else {
        return None;
    };

    // Every element must be an object. A mixed array of objects and scalars has no
    // coherent field set, and pretending otherwise would produce statistics that
    // describe only part of the data while reading as though they described all of
    // it.
    let records: Vec<&serde_json::Map<String, Value>> =
        items.iter().filter_map(Value::as_object).collect();
    if records.len() < 2 || records.len() != items.len() {
        return None;
    }

    // First-appearance order, so output follows the document rather than an
    // alphabetical accident.
    let mut field_order: Vec<&str> = Vec::new();
    for record in &records {
        for key in record.keys() {
            if !field_order.iter().any(|k| k == &key.as_str()) {
                field_order.push(key);
            }
        }
    }

    let fields = field_order
        .into_iter()
        .map(|name| field_stat(name, &records, config))
        .collect();

    Some(RecordSetStats {
        records: records.len(),
        fields,
    })
}

/// Computes the statistic for a single field.
fn field_stat(
    name: &str,
    records: &[&serde_json::Map<String, Value>],
    config: &CrushConfig,
) -> FieldStat {
    // Keyed by serialized form because `Value` is neither `Hash` nor `Ord` under
    // `arbitrary_precision`. Serialization is deterministic given `preserve_order`,
    // so equal values always produce equal keys.
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut first_value: Option<&Value> = None;
    let mut present_in = 0usize;

    for record in records {
        let Some(value) = record.get(name) else {
            continue;
        };
        present_in += 1;
        if first_value.is_none() {
            first_value = Some(value);
        }
        let key = serde_json::to_string(value).unwrap_or_else(|_| format!("{value:?}"));
        *counts.entry(key).or_insert(0) += 1;
    }

    let distinct = counts.len();

    let kind = if distinct == 1 && present_in == records.len() {
        // Both conditions are required. A field that is absent from one record is
        // *optional*, not constant, however uniform its present values are —
        // reporting it constant would tell the model every record carries it, which
        // is false.
        FieldKind::Constant {
            value: first_value.cloned().unwrap_or(Value::Null),
        }
    } else if distinct == records.len() && present_in == records.len() {
        FieldKind::Unique
    } else if distinct <= config.max_low_cardinality {
        FieldKind::LowCardinality { distinct }
    } else {
        FieldKind::Varied { distinct }
    };

    FieldStat {
        name: name.to_owned(),
        kind,
        present_in,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> CrushConfig {
        CrushConfig::default()
    }

    fn doc(source: &str) -> Document {
        Document::parse(source, &config()).expect("valid json")
    }

    fn stats(source: &str) -> Option<RecordSetStats> {
        analyze_record_set(&doc(source), &config())
    }

    fn kind_of(stats: &RecordSetStats, name: &str) -> FieldKind {
        stats
            .fields
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("no field {name}"))
            .kind
            .clone()
    }

    // ---- classification ----

    #[test]
    fn an_array_of_objects_is_a_record_set() {
        let d = doc(r#"[{"id":1,"ok":true},{"id":2,"ok":false}]"#);
        assert_eq!(
            classify(d.shape(), &config()),
            Pattern::RecordSet { records: 2 }
        );
    }

    #[test]
    fn an_array_of_scalars_is_not_a_record_set() {
        // Homogeneous, but there are no fields to summarize.
        let d = doc(r#"[1,2,3,4,5]"#);
        assert_ne!(
            classify(d.shape(), &config()),
            Pattern::RecordSet { records: 5 }
        );
    }

    #[test]
    fn empty_and_single_element_arrays_classify_sanely() {
        assert_eq!(
            classify(doc("[]").shape(), &config()),
            Pattern::Unremarkable
        );
        assert_eq!(
            classify(doc(r#"[{"id":1}]"#).shape(), &config()),
            Pattern::Unremarkable
        );
    }

    #[test]
    fn a_heterogeneous_array_is_not_a_record_set() {
        let d = doc(r#"[{"id":1},{"name":"x"},{"other":true}]"#);
        assert!(!matches!(
            classify(d.shape(), &config()),
            Pattern::RecordSet { .. }
        ));
    }

    #[test]
    fn a_document_dominated_by_prose_is_scalar_heavy() {
        let long = "x".repeat(2000);
        let d = doc(&format!(r#"{{"body":"{long}"}}"#));
        assert!(matches!(
            classify(d.shape(), &config()),
            Pattern::ScalarHeavy { .. }
        ));
    }

    #[test]
    fn scalar_heavy_is_checked_before_wide_object() {
        // A wide object full of prose has no structure to exploit; it belongs to CCR
        // offload. If the order were reversed it would be misrouted to a structural
        // compressor that cannot help it.
        let long = "y".repeat(200);
        let fields: Vec<String> = (0..20).map(|i| format!(r#""f{i}":"{long}""#)).collect();
        let d = doc(&format!("{{{}}}", fields.join(",")));
        assert!(matches!(
            classify(d.shape(), &config()),
            Pattern::ScalarHeavy { .. }
        ));
    }

    #[test]
    fn an_object_with_many_small_fields_is_wide() {
        let fields: Vec<String> = (0..15).map(|i| format!(r#""f{i}":{i}"#)).collect();
        let d = doc(&format!("{{{}}}", fields.join(",")));
        assert_eq!(
            classify(d.shape(), &config()),
            Pattern::WideObject { fields: 15 }
        );
    }

    #[test]
    fn a_spindly_document_is_deep_nest() {
        let d = doc(r#"{"a":{"b":{"c":{"d":1}}}}"#);
        assert!(matches!(
            classify(d.shape(), &config()),
            Pattern::DeepNest { .. }
        ));
    }

    #[test]
    fn an_ordinary_shallow_object_is_unremarkable() {
        // Most small documents land here, and declining is the right response.
        let d = doc(r#"{"a":1,"b":2}"#);
        assert_eq!(classify(d.shape(), &config()), Pattern::Unremarkable);
    }

    #[test]
    fn a_record_set_wins_over_other_patterns() {
        // An array of records that also nests deeply is still a record set.
        let d = doc(r#"[{"a":{"b":{"c":1}}},{"a":{"b":{"c":2}}}]"#);
        assert_eq!(
            classify(d.shape(), &config()),
            Pattern::RecordSet { records: 2 }
        );
    }

    // ---- field statistics ----

    #[test]
    fn a_genuine_constant_field_is_reported_constant() {
        let s = stats(r#"[{"kind":"file","id":1},{"kind":"file","id":2},{"kind":"file","id":3}]"#)
            .unwrap();

        assert_eq!(s.records, 3);
        assert_eq!(
            kind_of(&s, "kind"),
            FieldKind::Constant {
                value: Value::String("file".into())
            }
        );
        assert_eq!(s.constant_fields().count(), 1);
    }

    #[test]
    fn one_differing_record_defeats_constancy() {
        // The near-miss. Reporting "kind" constant here would assert something false
        // to the model, which is worse than not compressing.
        let s = stats(r#"[{"kind":"file"},{"kind":"file"},{"kind":"dir"}]"#).unwrap();

        assert!(!matches!(kind_of(&s, "kind"), FieldKind::Constant { .. }));
        assert_eq!(s.constant_fields().count(), 0);
    }

    #[test]
    fn a_field_missing_from_one_record_is_not_constant() {
        // Uniform where present, but absent from one record. Calling it constant
        // would tell the model every record carries it — false.
        let s = stats(r#"[{"kind":"file","id":1},{"kind":"file","id":2},{"id":3}]"#).unwrap();

        let kind = s.fields.iter().find(|f| f.name == "kind").unwrap();
        assert_eq!(kind.present_in, 2, "present in only two of three");
        assert!(!matches!(kind.kind, FieldKind::Constant { .. }));
        assert_eq!(s.constant_fields().count(), 0);
    }

    #[test]
    fn a_field_distinct_in_every_record_is_unique() {
        let s = stats(r#"[{"id":1},{"id":2},{"id":3}]"#).unwrap();
        assert_eq!(kind_of(&s, "id"), FieldKind::Unique);
    }

    #[test]
    fn a_field_with_a_few_repeated_values_is_low_cardinality() {
        let s = stats(
            r#"[{"status":"ok"},{"status":"retry"},{"status":"ok"},{"status":"failed"},{"status":"ok"}]"#,
        )
        .unwrap();
        assert_eq!(
            kind_of(&s, "status"),
            FieldKind::LowCardinality { distinct: 3 }
        );
    }

    #[test]
    fn many_distinct_values_short_of_one_per_record_are_varied() {
        let records: Vec<String> = (0..40).map(|i| format!(r#"{{"v":{}}}"#, i % 20)).collect();
        let s = stats(&format!("[{}]", records.join(","))).unwrap();
        assert_eq!(kind_of(&s, "v"), FieldKind::Varied { distinct: 20 });
    }

    #[test]
    fn values_that_differ_only_by_type_are_distinct() {
        // `1` and `"1"` must not collapse. Serializing as the cardinality key keeps
        // them apart; comparing rendered scalars would not.
        let s = stats(r#"[{"v":1},{"v":"1"}]"#).unwrap();
        assert_eq!(kind_of(&s, "v"), FieldKind::Unique);
    }

    #[test]
    fn nested_object_values_compare_structurally() {
        let s = stats(r#"[{"meta":{"a":1}},{"meta":{"a":1}}]"#).unwrap();
        assert!(matches!(kind_of(&s, "meta"), FieldKind::Constant { .. }));
    }

    #[test]
    fn fields_are_returned_in_first_appearance_order() {
        // Document order, not alphabetical — output follows the source.
        let s = stats(r#"[{"zebra":1,"apple":2},{"zebra":3,"apple":4,"mango":5}]"#).unwrap();
        assert_eq!(
            s.fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            vec!["zebra", "apple", "mango"]
        );
    }

    // ---- what analysis refuses ----

    #[test]
    fn non_arrays_and_short_arrays_yield_no_statistics() {
        assert!(stats(r#"{"a":1}"#).is_none());
        assert!(stats("[]").is_none());
        assert!(stats(r#"[{"id":1}]"#).is_none());
        assert!(stats("[1,2,3]").is_none());
    }

    #[test]
    fn an_array_mixing_objects_and_scalars_yields_no_statistics() {
        // Statistics over only the object elements would read as though they
        // described the whole array.
        assert!(stats(r#"[{"id":1},2,{"id":3}]"#).is_none());
    }

    #[test]
    fn heterogeneous_arrays_are_still_analyzed() {
        // Deliberately more permissive than `Shape::is_record_set`. An array where
        // one record carries `error` and the rest do not is exactly what field
        // statistics exist to surface.
        let s = stats(r#"[{"id":1},{"id":2},{"id":3,"error":"boom"}]"#).unwrap();

        let error = s.fields.iter().find(|f| f.name == "error").unwrap();
        assert_eq!(error.present_in, 1);
        assert_eq!(s.records, 3);
    }

    #[test]
    fn analysis_is_deterministic() {
        // Invariant I4. Cardinality counting keys on serialized values in a
        // BTreeMap; a HashMap here would break this intermittently.
        let source = r#"[{"z":1,"a":"x"},{"z":2,"a":"x"},{"z":3,"a":"y"}]"#;
        let first = stats(source).unwrap();
        for _ in 0..50 {
            assert_eq!(stats(source).unwrap(), first);
        }
    }

    #[test]
    fn classification_is_deterministic() {
        let d = doc(r#"[{"id":1,"s":"a"},{"id":2,"s":"b"}]"#);
        let first = classify(d.shape(), &config());
        for _ in 0..50 {
            assert_eq!(classify(d.shape(), &config()), first);
        }
    }
}
