//! Byte-faithful request bodies — invariant I1.
//!
//! > For every request, the bytes sent upstream are byte-equal to the bytes received
//! > from the client, modulo only the byte ranges a transform explicitly modified.
//!
//! This is the most load-bearing module in the proxy, and the one whose failure mode
//! is hardest to notice. If merely *passing a request through* re-serializes the
//! JSON — reordering keys, collapsing `1.0` to `1`, escaping UTF-8 as `\uXXXX`,
//! inserting prettifier whitespace — then the bytes differ from what the provider
//! cached. The prefix misses. Every request costs more than it would have with no
//! proxy at all, and nothing in the response indicates why.
//!
//! # Why the feature flags are not enough on their own
//!
//! The workspace enables `preserve_order` and `arbitrary_precision`, which fix key
//! order and numeric literals. They are necessary and insufficient: a `Value` round
//! trip still normalizes insignificant whitespace and may differ on string escapes.
//!
//! The only actually byte-faithful approach is to never round-trip untouched
//! content. `RawValue` retains the original byte slice, so a message the proxy did
//! not modify is forwarded as an exact copy of what arrived. That is what this module
//! is built around.

use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;

use serde::de::{Deserializer, MapAccess, Visitor};
use serde::Deserialize;
use serde_json::value::RawValue;

/// A JSON object's members, in document order, as borrowed slices.
///
/// `serde_json` has no built-in way to deserialize an object into an ordered
/// sequence of raw pairs: `Vec<(K, V)>` expects a JSON *array*, and `serde_json::Map`
/// is fixed to `Value` and would therefore parse — and later re-serialize — every
/// member, which is exactly the round trip invariant I1 forbids.
///
/// So the members are collected by hand. Every value stays a `RawValue`, meaning an
/// untouched member is still the original bytes when it is written back out.
struct OrderedMembers<'a>(Vec<(&'a str, &'a RawValue)>);

impl<'de: 'a, 'a> Deserialize<'de> for OrderedMembers<'a> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct MembersVisitor<'a>(PhantomData<&'a ()>);

        impl<'de: 'a, 'a> Visitor<'de> for MembersVisitor<'a> {
            type Value = OrderedMembers<'a>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut members = Vec::new();
                // Borrowed keys only. A key needing unescaping cannot be borrowed, and
                // the parse fails — which routes the body to verbatim passthrough
                // rather than to a lossy reconstruction. The safe direction.
                while let Some((key, value)) = map.next_entry::<&str, &RawValue>()? {
                    members.push((key, value));
                }
                Ok(OrderedMembers(members))
            }
        }

        deserializer.deserialize_map(MembersVisitor(PhantomData))
    }
}

/// A parsed request body that can be rebuilt without disturbing untouched parts.
///
/// Holds the original bytes plus a view of the `messages` array as raw slices. A
/// message that is never replaced is emitted from its original slice.
#[derive(Debug)]
pub struct FaithfulBody<'a> {
    source: &'a [u8],
    /// `None` when the body is not JSON, or not an object with a `messages` array.
    /// Such a body is forwarded verbatim: the proxy is not a validator, and a request
    /// it does not understand is one it must not touch.
    parsed: Option<Parsed<'a>>,
}

#[derive(Debug)]
struct Parsed<'a> {
    /// Top-level members in document order, as raw slices.
    members: Vec<(&'a str, &'a RawValue)>,
    /// Index into `members` of the `messages` entry.
    messages_at: usize,
    /// The messages, as raw slices.
    messages: Vec<&'a RawValue>,
}

impl<'a> FaithfulBody<'a> {
    /// Parses `source`, retaining the original bytes.
    ///
    /// Never fails. A body that cannot be understood is retained whole and forwarded
    /// unchanged — see [`FaithfulBody::is_understood`].
    pub fn parse(source: &'a [u8]) -> Self {
        let parsed = serde_json::from_slice::<OrderedMembers>(source)
            .ok()
            .and_then(|OrderedMembers(members)| {
                let messages_at = members.iter().position(|(key, _)| *key == "messages")?;
                let messages: Vec<&RawValue> =
                    serde_json::from_str(members[messages_at].1.get()).ok()?;
                Some(Parsed {
                    members,
                    messages_at,
                    messages,
                })
            });

        Self { source, parsed }
    }

    /// Whether the body was recognized as a message-carrying request.
    pub fn is_understood(&self) -> bool {
        self.parsed.is_some()
    }

    /// The original bytes.
    pub fn source(&self) -> &'a [u8] {
        self.source
    }

    /// How many messages the body carries.
    pub fn message_count(&self) -> usize {
        self.parsed.as_ref().map_or(0, |p| p.messages.len())
    }

    /// The raw JSON text of message `index`, exactly as it arrived.
    pub fn message(&self, index: usize) -> Option<&'a str> {
        self.parsed
            .as_ref()?
            .messages
            .get(index)
            .map(|raw| raw.get())
    }

    /// Rebuilds the body, substituting only the messages present in `replacements`.
    ///
    /// Every other byte — other top-level members, untouched messages, key order,
    /// numeric literals — comes from the original slice.
    ///
    /// With an empty `replacements` the output is byte-identical to the input for any
    /// understood body. That is the property the invariant I1 test asserts, and it is
    /// the reason this returns [`Cow`]: the passthrough case does not allocate a new
    /// body at all.
    pub fn rebuild(&self, replacements: &[(usize, String)]) -> Cow<'a, [u8]> {
        let Some(parsed) = self.parsed.as_ref() else {
            return Cow::Borrowed(self.source);
        };
        if replacements.is_empty() {
            // The passthrough path. Not "rebuild it and hope it matches" — the
            // original bytes are handed back untouched, so I1 holds by construction
            // rather than by the serializer happening to agree.
            return Cow::Borrowed(self.source);
        }

        let mut messages = String::from("[");
        for (index, raw) in parsed.messages.iter().enumerate() {
            if index > 0 {
                messages.push(',');
            }
            match replacements.iter().find(|(at, _)| *at == index) {
                Some((_, replacement)) => messages.push_str(replacement),
                // Exact byte copy — this is the whole point of `RawValue`.
                None => messages.push_str(raw.get()),
            }
        }
        messages.push(']');

        let mut out = String::from("{");
        for (position, (key, value)) in parsed.members.iter().enumerate() {
            if position > 0 {
                out.push(',');
            }
            // Key order follows the original. Re-emitting the key through
            // `serde_json` keeps escaping correct for exotic key names.
            out.push_str(&serde_json::to_string(key).unwrap_or_else(|_| format!("\"{key}\"")));
            out.push(':');
            if position == parsed.messages_at {
                out.push_str(&messages);
            } else {
                out.push_str(value.get());
            }
        }
        out.push('}');

        Cow::Owned(out.into_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn sha(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    /// A representative Anthropic request.
    const REQUEST: &str = r#"{"model":"claude-opus-4","max_tokens":1024,"system":"You are helpful.","tools":[{"name":"read","input_schema":{"type":"object"}}],"messages":[{"role":"user","content":"first"},{"role":"assistant","content":"second"},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"bulky"}]}]}"#;

    // ---- invariant I1 ----

    #[test]
    fn passthrough_is_byte_identical_by_sha256() {
        // The gate. Note this asserts on a *hash of the bytes*, not on parsed
        // equality — comparing parsed values would pass on exactly the corruption
        // this module exists to prevent.
        let body = FaithfulBody::parse(REQUEST.as_bytes());
        let rebuilt = body.rebuild(&[]);

        assert_eq!(sha(&rebuilt), sha(REQUEST.as_bytes()));
        assert_eq!(&*rebuilt, REQUEST.as_bytes());
    }

    #[test]
    fn passthrough_survives_the_shapes_a_value_round_trip_would_mangle() {
        let cases = [
            // Pretty-printed: a Value round trip would emit this compact.
            "{\n  \"messages\": [\n    {\n      \"role\": \"user\"\n    }\n  ]\n}",
            // Key order a sorting map would rearrange.
            r#"{"zebra":1,"messages":[{"role":"user"}],"apple":2}"#,
            // Floats that must not collapse to integers.
            r#"{"temperature":1.0,"top_p":0.50,"messages":[{"a":2.0}]}"#,
            // Integers past 2^53.
            r#"{"id":12345678901234567890,"messages":[{"n":9007199254740993}]}"#,
            // Non-ASCII that must not become \uXXXX.
            r#"{"messages":[{"content":"日本語 😀 café"}]}"#,
            // Escapes the serializer might spell differently.
            r#"{"messages":[{"content":"line\nbreak\ttab\"quote\\slash"}]}"#,
            // Extra whitespace inside the messages array.
            r#"{"messages":[ {"role":"user"} , {"role":"assistant"} ]}"#,
        ];

        for case in cases {
            let body = FaithfulBody::parse(case.as_bytes());
            assert!(body.is_understood(), "should parse: {case}");
            assert_eq!(
                sha(&body.rebuild(&[])),
                sha(case.as_bytes()),
                "byte-faithfulness lost on: {case}"
            );
        }
    }

    #[test]
    fn passthrough_does_not_allocate() {
        // The borrowed variant is the proof that nothing was rebuilt.
        let body = FaithfulBody::parse(REQUEST.as_bytes());
        assert!(matches!(body.rebuild(&[]), Cow::Borrowed(_)));
    }

    // ---- replacement ----

    #[test]
    fn replacing_one_message_leaves_every_other_byte_alone() {
        let body = FaithfulBody::parse(REQUEST.as_bytes());
        let replacement = r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"COMPRESSED"}]}"#;
        let rebuilt = body.rebuild(&[(2, replacement.to_owned())]);
        let rebuilt = String::from_utf8(rebuilt.into_owned()).unwrap();

        // The replacement landed...
        assert!(rebuilt.contains("COMPRESSED"));
        // ...and the untouched neighbours are byte-for-byte what arrived.
        assert!(rebuilt.contains(r#"{"role":"user","content":"first"}"#));
        assert!(rebuilt.contains(r#"{"role":"assistant","content":"second"}"#));
        // ...as are the hot-zone members.
        assert!(rebuilt.contains(r#""system":"You are helpful.""#));
        assert!(rebuilt.contains(r#""tools":[{"name":"read","input_schema":{"type":"object"}}]"#));
    }

    #[test]
    fn top_level_key_order_is_preserved_through_a_replacement() {
        let source = r#"{"zebra":1,"messages":[{"a":1},{"b":2}],"apple":2}"#;
        let body = FaithfulBody::parse(source.as_bytes());
        let rebuilt = body.rebuild(&[(0, r#"{"a":9}"#.to_owned())]);
        let rebuilt = String::from_utf8(rebuilt.into_owned()).unwrap();

        let zebra = rebuilt.find("zebra").unwrap();
        let apple = rebuilt.find("apple").unwrap();
        assert!(zebra < apple, "key order changed: {rebuilt}");
    }

    #[test]
    fn a_replacement_rebuild_is_still_valid_json() {
        let body = FaithfulBody::parse(REQUEST.as_bytes());
        let rebuilt = body.rebuild(&[(0, r#"{"role":"user","content":"changed"}"#.to_owned())]);
        let parsed: serde_json::Value = serde_json::from_slice(&rebuilt).expect("valid json");
        assert_eq!(parsed["messages"][0]["content"], "changed");
        assert_eq!(parsed["messages"][1]["content"], "second");
    }

    #[test]
    fn replacing_every_message_still_preserves_the_envelope() {
        let body = FaithfulBody::parse(REQUEST.as_bytes());
        let all: Vec<(usize, String)> = (0..body.message_count())
            .map(|i| (i, r#"{"role":"user","content":"x"}"#.to_owned()))
            .collect();
        let rebuilt = String::from_utf8(body.rebuild(&all).into_owned()).unwrap();

        assert!(rebuilt.contains(r#""system":"You are helpful.""#));
        assert!(rebuilt.contains(r#""model":"claude-opus-4""#));
    }

    // ---- bodies the proxy must not touch ----

    #[test]
    fn malformed_json_is_forwarded_unchanged() {
        // The proxy is not a validator. A body it cannot parse is one it must not
        // touch — rejecting it would break a client the provider would have accepted.
        for case in ["{not json", "", "[]", "null", "{\"no\":\"messages\"}"] {
            let body = FaithfulBody::parse(case.as_bytes());
            assert!(
                !body.is_understood(),
                "should not claim to understand {case:?}"
            );
            assert_eq!(&*body.rebuild(&[]), case.as_bytes());
        }
    }

    #[test]
    fn a_body_whose_messages_is_not_an_array_is_left_alone() {
        let source = r#"{"messages":"not an array"}"#;
        let body = FaithfulBody::parse(source.as_bytes());
        assert!(!body.is_understood());
        assert_eq!(&*body.rebuild(&[]), source.as_bytes());
    }

    #[test]
    fn messages_are_readable_as_their_original_text() {
        let body = FaithfulBody::parse(REQUEST.as_bytes());
        assert_eq!(body.message_count(), 3);
        assert_eq!(
            body.message(0),
            Some(r#"{"role":"user","content":"first"}"#)
        );
        assert_eq!(body.message(3), None);
    }

    #[test]
    fn rebuilding_is_deterministic() {
        let body = FaithfulBody::parse(REQUEST.as_bytes());
        let once = body.rebuild(&[(1, r#"{"role":"assistant","content":"z"}"#.to_owned())]);
        for _ in 0..25 {
            let again = body.rebuild(&[(1, r#"{"role":"assistant","content":"z"}"#.to_owned())]);
            assert_eq!(once, again);
        }
    }
}
