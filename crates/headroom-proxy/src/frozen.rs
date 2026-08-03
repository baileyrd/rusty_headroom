//! Deriving the frozen-message floor from customer `cache_control` markers.
//!
//! The live-zone dispatcher takes a `frozen_message_count` and nothing has been
//! supplying it — every caller passes `0`. This is where it comes from.
//!
//! When a customer places a `cache_control` breakpoint on a message, they are telling
//! the provider "cache everything up to here". Modifying anything at or before that
//! point invalidates the cache they explicitly asked for. Honoring the marker is not
//! an optimization; ignoring it destroys something the customer set up deliberately.
//!
//! # Conservative on failure
//!
//! Every uncertain path here returns a floor that freezes *more*, never less. A body
//! that cannot be understood yields [`FROZEN_EVERYTHING`], not zero.
//!
//! The asymmetry is the usual one. Freezing too much costs some compression. Freezing
//! too little modifies a message the provider has cached, which costs money and
//! context and does so silently. Guessing wrong in the safe direction is cheap;
//! guessing wrong in the other direction is the failure this whole project exists to
//! prevent.

use serde_json::Value;

/// A floor that freezes every message, whatever the array length.
///
/// Returned whenever the body cannot be understood well enough to say otherwise.
pub const FROZEN_EVERYTHING: usize = usize::MAX;

/// Computes the frozen-message floor for an Anthropic-shaped request body.
///
/// Messages at an index below the returned value are never eligible for compression.
///
/// # Example
///
/// ```
/// use headroom_proxy::frozen::frozen_message_count;
///
/// // A breakpoint on message 0 freezes it, leaving message 1 live.
/// let body = br#"{"messages":[
///     {"role":"user","content":"a","cache_control":{"type":"ephemeral"}},
///     {"role":"user","content":"b"}
/// ]}"#;
/// assert_eq!(frozen_message_count(body), 1);
///
/// // No markers, nothing frozen by this rule.
/// assert_eq!(frozen_message_count(br#"{"messages":[{"role":"user"}]}"#), 0);
/// ```
pub fn frozen_message_count(body: &[u8]) -> usize {
    let Ok(parsed) = serde_json::from_slice::<Value>(body) else {
        // Unparseable. The safe reading is "assume the customer cached all of it".
        return FROZEN_EVERYTHING;
    };

    let Some(messages) = parsed.get("messages") else {
        return FROZEN_EVERYTHING;
    };
    let Some(messages) = messages.as_array() else {
        return FROZEN_EVERYTHING;
    };

    // An empty array has nothing to freeze and nothing to compress; zero is exact
    // rather than a guess.
    let mut floor = 0usize;

    for (index, message) in messages.iter().enumerate() {
        if has_cache_control(message) {
            // Everything up to *and including* this message is frozen, so the floor
            // is the next index. Later breakpoints overwrite earlier ones, which is
            // how "the last one wins" falls out without a special case.
            floor = index + 1;
        }
    }

    floor
}

/// Whether a message carries a `cache_control` marker, directly or on a content block.
///
/// Both placements mean the same thing for this purpose. A marker on a block inside
/// message 4 still says "cache through message 4".
///
/// Shared with [`crate::stabilization`], which asks the same question when deciding
/// whether a breakpoint is already in place. It had its own whole-body version; two
/// answers to "does this carry a breakpoint" is the shape of drift this repository keeps
/// finding, so there is one.
pub(crate) fn has_cache_control(message: &Value) -> bool {
    if message.get("cache_control").is_some() {
        return true;
    }

    match message.get("content") {
        Some(Value::Array(blocks)) => blocks.iter().any(|b| b.get("cache_control").is_some()),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_markers_freezes_nothing() {
        let body = br#"{"messages":[{"role":"user","content":"a"},{"role":"user","content":"b"}]}"#;
        assert_eq!(frozen_message_count(body), 0);
    }

    #[test]
    fn a_marker_on_a_message_freezes_through_it() {
        let body = br#"{"messages":[
            {"role":"user","content":"a","cache_control":{"type":"ephemeral"}},
            {"role":"assistant","content":"b"},
            {"role":"user","content":"c"}
        ]}"#;
        assert_eq!(frozen_message_count(body), 1);
    }

    #[test]
    fn a_marker_on_a_content_block_counts_the_same() {
        // Both placements mean "cache through this message".
        let body = br#"{"messages":[
            {"role":"user","content":[{"type":"text","text":"a","cache_control":{"type":"ephemeral"}}]},
            {"role":"user","content":"b"}
        ]}"#;
        assert_eq!(frozen_message_count(body), 1);
    }

    #[test]
    fn the_last_breakpoint_wins() {
        // Everything before the final marker is frozen too, so the floor is the last
        // one rather than the first.
        let body = br#"{"messages":[
            {"role":"user","content":"a","cache_control":{"type":"ephemeral"}},
            {"role":"assistant","content":"b"},
            {"role":"user","content":"c","cache_control":{"type":"ephemeral"}},
            {"role":"assistant","content":"d"}
        ]}"#;
        assert_eq!(frozen_message_count(body), 3);
    }

    #[test]
    fn a_marker_on_the_last_message_freezes_everything() {
        let body = br#"{"messages":[
            {"role":"user","content":"a"},
            {"role":"user","content":"b","cache_control":{"type":"ephemeral"}}
        ]}"#;
        assert_eq!(frozen_message_count(body), 2);
    }

    // ---- the conservative direction ----

    #[test]
    fn unparseable_input_freezes_everything() {
        // The direction that matters. Returning 0 here would let an unparseable
        // request become a cache-busting one.
        for body in [
            &b"{not json"[..],
            &b""[..],
            &b"null"[..],
            &b"[1,2,3]"[..],
            &b"{\"no\":\"messages\"}"[..],
            &b"{\"messages\":\"not an array\"}"[..],
        ] {
            assert_eq!(
                frozen_message_count(body),
                FROZEN_EVERYTHING,
                "should freeze everything: {:?}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn the_everything_floor_actually_excludes_every_index() {
        // The value has to be one no message index can reach, or "freeze everything"
        // silently leaves the tail live.
        let floor = FROZEN_EVERYTHING;
        for index in [0usize, 1, 1000, usize::MAX - 1] {
            assert!(index < floor, "index {index} escaped the floor");
        }
    }

    #[test]
    fn an_empty_message_array_is_exactly_zero() {
        // Nothing to freeze and nothing to compress — zero is exact, not a guess.
        assert_eq!(frozen_message_count(br#"{"messages":[]}"#), 0);
    }

    #[test]
    fn a_non_object_message_does_not_panic() {
        let body = br#"{"messages":[1,"two",null,{"role":"user"}]}"#;
        assert_eq!(frozen_message_count(body), 0);
    }

    #[test]
    fn the_floor_is_deterministic() {
        let body = br#"{"messages":[{"role":"user","cache_control":{"type":"ephemeral"}},{"role":"user"}]}"#;
        let first = frozen_message_count(body);
        for _ in 0..25 {
            assert_eq!(frozen_message_count(body), first);
        }
    }
}
