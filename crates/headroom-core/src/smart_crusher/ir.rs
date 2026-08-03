//! The structural IR SmartCrusher analyses over.
//!
//! # Document versus shape
//!
//! Two representations, deliberately separate:
//!
//! - The **document** is `serde_json::Value`, which under this workspace's feature
//!   flags preserves key insertion order and the literal text of every number.
//! - The **shape** ([`Shape`]) is a structural summary derived from it — what is an
//!   array of records, how homogeneous those records are, which fields every record
//!   carries.
//!
//! Analysis, planning, and formatting all read the shape; only formatting touches
//! the document. Keeping them apart means a planning bug cannot corrupt data, and
//! the shape can be cheap to compare across candidate plans.
//!
//! # Determinism
//!
//! No `HashMap` appears anywhere on a path that influences output. `HashMap`
//! iteration order varies per process, so a single one here would make compressed
//! bytes differ between runs and bust the provider's prompt cache — invariant I4.
//! Object fields are held in a `Vec<(String, Shape)>`, which is both deterministic
//! *and* preserves document order, where a `BTreeMap` would be deterministic but
//! would silently sort the keys.

use serde_json::Value;

use crate::error::{Error, Result};

use super::CrushConfig;

/// A structural summary of a JSON value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool,
    /// Any number.
    Number,
    /// A string, with its length in bytes.
    Str {
        /// Length of the string's contents in bytes.
        len: usize,
    },
    /// An array.
    Array {
        /// How many elements.
        len: usize,
        /// The shape shared by every element, when they agree.
        ///
        /// `Some` here is what makes an array a *record set* — the thing SmartCrusher
        /// can describe far more cheaply than it can list. `None` means the elements
        /// disagree and the array must be treated as heterogeneous.
        element: Option<Box<Shape>>,
    },
    /// An object, with its fields in document order.
    Object {
        /// Field names paired with their shapes, in the order they appeared.
        fields: Vec<(String, Shape)>,
    },
    /// Nesting deeper than [`CrushConfig::max_depth`].
    ///
    /// Analysis stops rather than recursing without bound; the value is still
    /// emitted verbatim by the formatter, it is simply not described.
    TooDeep,
}

impl Shape {
    /// Whether this is an array of two or more consistently-shaped records.
    ///
    /// The single most valuable thing SmartCrusher can find: 500 rows that all look
    /// alike can be described in a few lines instead of listed in full.
    pub fn is_record_set(&self) -> bool {
        matches!(self, Self::Array { len, element } if *len >= 2 && element.is_some())
    }

    /// How deeply this shape nests. A scalar is depth 1.
    pub fn depth(&self) -> usize {
        match self {
            Self::Null | Self::Bool | Self::Number | Self::Str { .. } | Self::TooDeep => 1,
            Self::Array { element, .. } => 1 + element.as_ref().map_or(0, |shape| shape.depth()),
            Self::Object { fields } => {
                1 + fields
                    .iter()
                    .map(|(_, shape)| shape.depth())
                    .max()
                    .unwrap_or(0)
            }
        }
    }

    /// Total bytes held in strings beneath this shape.
    ///
    /// Used to spot documents whose weight is prose rather than structure. Those have
    /// nothing for a structural compressor to exploit and belong to CCR offload
    /// instead.
    pub fn string_bytes(&self) -> usize {
        match self {
            Self::Str { len } => *len,
            Self::Null | Self::Bool | Self::Number | Self::TooDeep => 0,
            Self::Array { len, element } => match element {
                Some(shape) => len * shape.string_bytes(),
                None => 0,
            },
            Self::Object { fields } => fields.iter().map(|(_, s)| s.string_bytes()).sum(),
        }
    }

    /// Number of leaf scalars beneath this shape.
    ///
    /// A rough proxy for how much a subtree costs in tokens, used to decide which
    /// parts of a document are worth attention. Cheaper than tokenizing, and the
    /// ranking is what matters rather than the absolute figure.
    pub fn leaf_count(&self) -> usize {
        match self {
            Self::Null | Self::Bool | Self::Number | Self::Str { .. } | Self::TooDeep => 1,
            Self::Array { len, element } => match element {
                // Homogeneous: one description times the element count, without
                // walking every element.
                Some(shape) => len * shape.leaf_count(),
                None => *len,
            },
            Self::Object { fields } => fields.iter().map(|(_, shape)| shape.leaf_count()).sum(),
        }
    }
}

/// A parsed JSON document plus its structural summary.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    value: Value,
    shape: Shape,
}

impl Document {
    /// Parses `source` and derives its shape.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] if `source` is not valid JSON. That is a routing
    /// failure rather than a fatal one — the content detector believed this was JSON
    /// and was wrong — so the caller forwards the original content unchanged.
    pub fn parse(source: &str, config: &CrushConfig) -> Result<Self> {
        let value: Value = serde_json::from_str(source).map_err(|err| Error::Malformed {
            content_type: "json",
            detail: err.to_string(),
        })?;
        let shape = derive_shape(&value, config, 0);
        Ok(Self { value, shape })
    }

    /// The parsed document.
    pub fn value(&self) -> &Value {
        &self.value
    }

    /// The structural summary.
    pub fn shape(&self) -> &Shape {
        &self.shape
    }

    /// Renders the document back to compact JSON.
    ///
    /// # Byte fidelity
    ///
    /// Key order and numeric literals survive this round trip — `1.0` does not
    /// collapse to `1`, and integers beyond `2^53` keep their digits — because the
    /// workspace enables `preserve_order` and `arbitrary_precision`.
    ///
    /// Insignificant whitespace does **not** survive: pretty-printed input comes back
    /// compact. That is intentional and safe, because this method is only reached
    /// for documents SmartCrusher is actually rewriting. A document it declines is
    /// restored from the caller's untouched original by the invariant I5 fallback,
    /// which never re-serializes anything.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(&self.value)?)
    }
}

/// Builds the structural summary for `value`.
fn derive_shape(value: &Value, config: &CrushConfig, depth: usize) -> Shape {
    if depth >= config.max_depth {
        return Shape::TooDeep;
    }

    match value {
        Value::Null => Shape::Null,
        Value::Bool(_) => Shape::Bool,
        Value::Number(_) => Shape::Number,
        Value::String(s) => Shape::Str { len: s.len() },
        Value::Array(items) => {
            let shapes: Vec<Shape> = items
                .iter()
                .map(|item| derive_shape(item, config, depth + 1))
                .collect();

            // The elements agree only if every one of them matches the first. An
            // array where 99 of 100 records share a shape is still heterogeneous
            // here — claiming otherwise would let the odd record out be summarized
            // away as though it were ordinary, and the odd record out is usually the
            // one that matters.
            let element = match shapes.split_first() {
                Some((first, rest)) if rest.iter().all(|s| s == first) => {
                    Some(Box::new(first.clone()))
                }
                _ => None,
            };

            Shape::Array {
                len: items.len(),
                element,
            }
        }
        Value::Object(map) => Shape::Object {
            fields: map
                .iter()
                .map(|(key, val)| (key.clone(), derive_shape(val, config, depth + 1)))
                .collect(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> Document {
        Document::parse(source, &CrushConfig::default()).expect("valid json")
    }

    #[test]
    fn malformed_json_is_a_recoverable_routing_failure() {
        let err = Document::parse("{not json", &CrushConfig::default()).unwrap_err();
        assert!(matches!(err, Error::Malformed { .. }));
        // Recoverable: the detector was wrong, the content is forwarded unchanged.
        assert!(err.is_recoverable());
    }

    // ---- byte fidelity ----

    #[test]
    fn key_order_survives_the_round_trip() {
        // Sorted keys would change the bytes sent upstream on every request and bust
        // the prompt cache. This is what `preserve_order` buys.
        let source = r#"{"zebra":1,"apple":2,"mango":3}"#;
        assert_eq!(parse(source).to_json().unwrap(), source);
    }

    #[test]
    fn float_literals_do_not_collapse() {
        // `1.0` becoming `1` is a byte change in the outgoing request.
        let source = r#"{"a":1.0,"b":2.50,"c":0.0}"#;
        assert_eq!(parse(source).to_json().unwrap(), source);
    }

    #[test]
    fn large_integers_keep_their_precision() {
        // Beyond 2^53 an f64 round trip silently corrupts the value. This is what
        // `arbitrary_precision` buys.
        let source = r#"{"id":12345678901234567890,"big":9007199254740993}"#;
        assert_eq!(parse(source).to_json().unwrap(), source);
    }

    #[test]
    fn compact_documents_round_trip_byte_exactly() {
        for source in [
            r#"{}"#,
            r#"[]"#,
            r#"{"a":null,"b":true,"c":false}"#,
            r#"[1,2,3]"#,
            r#"{"nested":{"deep":{"deeper":[1,2]}}}"#,
            r#"{"unicode":"日本語","emoji":"😀"}"#,
            r#"{"escaped":"line\nbreak\ttab\"quote"}"#,
        ] {
            assert_eq!(parse(source).to_json().unwrap(), source, "{source}");
        }
    }

    // ---- shape derivation ----

    #[test]
    fn scalars_map_to_scalar_shapes() {
        assert_eq!(*parse("null").shape(), Shape::Null);
        assert_eq!(*parse("true").shape(), Shape::Bool);
        assert_eq!(*parse("42").shape(), Shape::Number);
        assert_eq!(*parse(r#""hello""#).shape(), Shape::Str { len: 5 });
    }

    #[test]
    fn object_fields_are_held_in_document_order() {
        // Not sorted. A BTreeMap here would be deterministic but would reorder the
        // fields, changing the output bytes.
        let doc = parse(r#"{"zebra":1,"apple":2}"#);
        let Shape::Object { fields } = doc.shape() else {
            panic!("expected object shape");
        };
        assert_eq!(
            fields.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            vec!["zebra", "apple"]
        );
    }

    #[test]
    fn a_homogeneous_record_array_is_recognized() {
        // The case SmartCrusher exists for.
        let doc = parse(r#"[{"id":1,"ok":true},{"id":2,"ok":false},{"id":3,"ok":true}]"#);
        assert!(doc.shape().is_record_set());
    }

    #[test]
    fn one_odd_record_makes_the_array_heterogeneous() {
        // Deliberate strictness. Calling this homogeneous would let the record that
        // differs be summarized away as ordinary — and the record that differs is
        // usually the one worth reading.
        let doc = parse(r#"[{"id":1},{"id":2},{"id":3,"error":"boom"}]"#);
        assert!(!doc.shape().is_record_set());
        assert!(matches!(doc.shape(), Shape::Array { element: None, .. }));
    }

    #[test]
    fn arrays_too_short_to_summarize_are_not_record_sets() {
        assert!(!parse(r#"[]"#).shape().is_record_set());
        assert!(!parse(r#"[{"id":1}]"#).shape().is_record_set());
        // Two is the floor at which sameness is a claim worth making.
        assert!(parse(r#"[{"id":1},{"id":2}]"#).shape().is_record_set());
    }

    #[test]
    fn strings_of_differing_length_are_different_shapes() {
        // Length is part of the shape because it drives the elision decision. Two
        // records whose string fields differ wildly in size are not interchangeable
        // for summarization purposes.
        let doc = parse(r#"["ab","abcd"]"#);
        assert!(!doc.shape().is_record_set());
    }

    // ---- depth bounding ----

    #[test]
    fn nesting_beyond_max_depth_is_marked_rather_than_recursed() {
        let config = CrushConfig {
            max_depth: 3,
            ..CrushConfig::default()
        };
        let doc = Document::parse(r#"{"a":{"b":{"c":{"d":1}}}}"#, &config).unwrap();

        // Walk to the bottom and confirm the analysis stopped.
        let mut shape = doc.shape();
        let mut depth = 0;
        while let Shape::Object { fields } = shape {
            shape = &fields[0].1;
            depth += 1;
        }
        assert_eq!(*shape, Shape::TooDeep);
        assert_eq!(depth, 3);
    }

    #[test]
    fn deeply_nested_input_does_not_overflow_the_stack() {
        // Tool output is not trusted input. Without the depth bound this recurses
        // until the stack dies.
        let depth = 10_000;
        let source = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        let doc = Document::parse(&source, &CrushConfig::default());

        // serde_json may reject it first with its own recursion limit; either way
        // the outcome must be an error or a bounded shape, never a crash.
        match doc {
            Ok(d) => assert!(matches!(d.shape(), Shape::Array { .. })),
            Err(err) => assert!(err.is_recoverable()),
        }
    }

    // ---- leaf counting ----

    #[test]
    fn leaf_count_reflects_subtree_size() {
        assert_eq!(parse("1").shape().leaf_count(), 1);
        assert_eq!(parse(r#"{"a":1,"b":2}"#).shape().leaf_count(), 2);
        assert_eq!(parse(r#"[1,2,3]"#).shape().leaf_count(), 3);
        // Homogeneous arrays multiply rather than walking every element.
        assert_eq!(
            parse(r#"[{"a":1,"b":2},{"a":3,"b":4}]"#)
                .shape()
                .leaf_count(),
            4
        );
    }

    #[test]
    fn shape_derivation_is_deterministic() {
        // Invariant I4. A HashMap anywhere in here would break this intermittently,
        // which is the worst way for it to break.
        let source = r#"{"z":1,"a":[{"k":"v"},{"k":"w"}],"m":{"n":null}}"#;
        let first = parse(source).shape().clone();
        for _ in 0..50 {
            assert_eq!(*parse(source).shape(), first);
        }
    }
}
