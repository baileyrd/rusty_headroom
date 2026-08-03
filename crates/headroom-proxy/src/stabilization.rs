//! Cache stabilization — making a stable request *look* stable to the provider.
//!
//! A provider caches on an exact byte prefix. Two requests that a human would call
//! identical miss the cache if a client serialized its tool array in a different order,
//! or emitted JSON Schema keys differently between runs. The client is not doing
//! anything wrong; the cache is just stricter than the semantics.
//!
//! Normalizing those into a canonical form turns an accidental miss into a hit.
//!
//! # Why this is normalization and not compression
//!
//! Invariant I7: tool definitions are normalized, never compressed. Nothing here
//! removes information. Sorting is deterministic and reversible in the only sense that
//! matters — the model sees the same tools with the same schemas, in a different
//! order.
//!
//! # Why it is gated by auth mode
//!
//! Sorting is safe everywhere. *Injecting* `cache_control` breakpoints or a
//! `prompt_cache_key` is not: on OAuth traffic an injected marker could fall outside
//! the granted scope, and on subscription traffic it is a proxy-revealing change. Both
//! are gated on [`CompressionPolicy`].

use headroom_core::auth_mode::CompressionPolicy;
use serde_json::{Map, Value};

/// Most `cache_control` breakpoints Anthropic accepts.
const MAX_BREAKPOINTS: usize = 4;

/// Sorts a JSON object's keys recursively, in place.
///
/// Arrays keep their order: an array is ordered data, and reordering one changes
/// meaning rather than presentation. Only object keys move, because a JSON object is
/// unordered by definition and the provider's cache is the only thing that disagrees.
pub fn sort_keys(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = std::mem::take(map).into_iter().collect();
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));

            let mut sorted = Map::new();
            for (key, mut child) in entries {
                sort_keys(&mut child);
                sorted.insert(key, child);
            }
            *map = sorted;
        }
        Value::Array(items) => {
            for item in items {
                sort_keys(item);
            }
        }
        _ => {}
    }
}

/// Normalizes a request's `tools` array: alphabetical by name, schema keys sorted.
///
/// Returns whether anything changed.
///
/// # Example
///
/// ```
/// use headroom_proxy::stabilization::normalize_tools;
/// use serde_json::json;
///
/// let mut body = json!({
///     "tools": [{"name": "zebra"}, {"name": "apple"}]
/// });
/// assert!(normalize_tools(&mut body));
/// assert_eq!(body["tools"][0]["name"], "apple");
/// ```
pub fn normalize_tools(body: &mut Value) -> bool {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return false;
    };

    let before = tools.clone();

    // Sort by name, with a stable fallback for a tool that somehow has none — an
    // unstable comparator here would defeat the entire point.
    tools.sort_by(|a, b| {
        let name_a = a.get("name").and_then(Value::as_str).unwrap_or("");
        let name_b = b.get("name").and_then(Value::as_str).unwrap_or("");
        name_a.cmp(name_b)
    });
    for tool in tools.iter_mut() {
        sort_keys(tool);
    }

    *tools != before
}

/// Places `cache_control` breakpoints on the largest stable prefix boundaries.
///
/// No-op unless the policy permits it.
///
/// Breakpoints go on the *earliest* messages, not the latest. The prefix before a
/// breakpoint is what gets cached, so marking late in the conversation caches almost
/// nothing; marking early caches the bulk that never changes.
pub fn place_cache_control(body: &mut Value, policy: CompressionPolicy) -> usize {
    if !policy.auto_cache_control {
        return 0;
    }

    // A customer-set marker means they have thought about this. Adding more could push
    // past the provider's limit and silently invalidate the ones they chose.
    if has_cache_control(body) {
        return 0;
    }

    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return 0;
    };
    // Nothing to cache if there is no history to speak of.
    if messages.len() < 4 {
        return 0;
    }

    // Spread across the frozen portion, leaving the newest turn alone since it is
    // exactly the part that changes next request.
    let usable = messages.len().saturating_sub(1);
    let stride = usable.div_ceil(MAX_BREAKPOINTS).max(1);

    let mut placed = 0;
    let mut index = stride.saturating_sub(1);
    while index < usable && placed < MAX_BREAKPOINTS {
        if let Some(message) = messages.get_mut(index).and_then(Value::as_object_mut) {
            message.insert(
                "cache_control".into(),
                serde_json::json!({ "type": "ephemeral" }),
            );
            placed += 1;
        }
        index += stride;
    }

    placed
}

/// Injects `prompt_cache_key` on OpenAI-shaped requests, when permitted.
///
/// Never overwrites one the customer set: their key is presumably chosen to group
/// requests the way they want, and replacing it would scatter their cache.
pub fn inject_prompt_cache_key(body: &mut Value, policy: CompressionPolicy, key: &str) -> bool {
    if !policy.auto_prompt_cache_key {
        return false;
    }
    let Some(object) = body.as_object_mut() else {
        return false;
    };
    if object.contains_key("prompt_cache_key") {
        return false;
    }
    object.insert("prompt_cache_key".into(), Value::String(key.to_owned()));
    true
}

/// Whether the body already carries a `cache_control` marker anywhere.
fn has_cache_control(body: &Value) -> bool {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return false;
    };
    messages.iter().any(|message| {
        message.get("cache_control").is_some()
            || message
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| blocks.iter().any(|b| b.get("cache_control").is_some()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use headroom_core::AuthMode;
    use serde_json::json;

    fn payg() -> CompressionPolicy {
        CompressionPolicy::for_mode(AuthMode::PayAsYouGo)
    }

    fn restricted() -> CompressionPolicy {
        CompressionPolicy::for_mode(AuthMode::Subscription)
    }

    fn conversation(turns: usize) -> Value {
        json!({
            "messages": (0..turns)
                .map(|i| json!({"role": if i % 2 == 0 {"user"} else {"assistant"}, "content": format!("turn {i}")}))
                .collect::<Vec<_>>()
        })
    }

    // ---- sorting ----

    #[test]
    fn object_keys_sort_recursively() {
        let mut value = json!({"zebra": {"yak": 1, "ant": 2}, "apple": 3});
        sort_keys(&mut value);

        let rendered = serde_json::to_string(&value).unwrap();
        assert_eq!(rendered, r#"{"apple":3,"zebra":{"ant":2,"yak":1}}"#);
    }

    #[test]
    fn arrays_keep_their_order() {
        // An array is ordered data. Reordering one changes meaning, not presentation.
        let mut value = json!({"items": [3, 1, 2]});
        sort_keys(&mut value);
        assert_eq!(value["items"], json!([3, 1, 2]));
    }

    #[test]
    fn tools_sort_by_name_with_schemas_normalized() {
        let mut body = json!({
            "tools": [
                {"name": "zebra", "input_schema": {"type": "object", "properties": {"b": 1, "a": 2}}},
                {"name": "apple", "input_schema": {"type": "object"}}
            ]
        });

        assert!(normalize_tools(&mut body));
        assert_eq!(body["tools"][0]["name"], "apple");

        let schema = serde_json::to_string(&body["tools"][1]["input_schema"]).unwrap();
        assert!(schema.find(r#""a""#) < schema.find(r#""b""#), "{schema}");
    }

    #[test]
    fn normalizing_already_sorted_tools_reports_no_change() {
        // Otherwise the proxy would rewrite the body on every request for no reason,
        // which is itself a way to lose byte-faithfulness.
        let mut body = json!({"tools": [{"name": "apple"}, {"name": "zebra"}]});
        assert!(!normalize_tools(&mut body));
    }

    #[test]
    fn sorting_is_idempotent() {
        let mut once = json!({"tools": [{"name": "z"}, {"name": "a"}, {"name": "m"}]});
        normalize_tools(&mut once);
        let mut twice = once.clone();
        normalize_tools(&mut twice);
        assert_eq!(once, twice);
    }

    #[test]
    fn a_body_without_tools_is_left_alone() {
        let mut body = json!({"messages": []});
        assert!(!normalize_tools(&mut body));
    }

    // ---- cache_control ----

    #[test]
    fn breakpoints_are_placed_under_a_permissive_policy() {
        let mut body = conversation(12);
        let placed = place_cache_control(&mut body, payg());

        assert!(placed > 0 && placed <= MAX_BREAKPOINTS, "placed {placed}");
    }

    #[test]
    fn breakpoints_land_early_not_late() {
        // The prefix *before* a breakpoint is what gets cached. Marking late caches
        // almost nothing.
        let mut body = conversation(12);
        place_cache_control(&mut body, payg());

        let first_marked = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .position(|m| m.get("cache_control").is_some())
            .expect("something marked");
        assert!(first_marked < 6, "first breakpoint at {first_marked}");
    }

    #[test]
    fn the_newest_turn_is_never_marked() {
        // It is exactly the part that changes next request.
        let mut body = conversation(12);
        place_cache_control(&mut body, payg());

        let messages = body["messages"].as_array().unwrap();
        assert!(messages.last().unwrap().get("cache_control").is_none());
    }

    #[test]
    fn a_restricted_policy_places_nothing() {
        let mut body = conversation(12);
        let before = body.clone();
        assert_eq!(place_cache_control(&mut body, restricted()), 0);
        assert_eq!(body, before, "a restricted request was modified");
    }

    #[test]
    fn a_customer_set_marker_suppresses_placement() {
        // They have thought about this. Adding more could push past the provider's
        // limit and silently invalidate the ones they chose.
        let mut body = conversation(12);
        body["messages"][2]
            .as_object_mut()
            .unwrap()
            .insert("cache_control".into(), json!({"type": "ephemeral"}));

        assert_eq!(place_cache_control(&mut body, payg()), 0);
    }

    #[test]
    fn a_marker_on_a_content_block_also_suppresses_placement() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}}]},
                {"role": "assistant", "content": "b"},
                {"role": "user", "content": "c"},
                {"role": "assistant", "content": "d"},
                {"role": "user", "content": "e"}
            ]
        });
        assert_eq!(place_cache_control(&mut body, payg()), 0);
    }

    #[test]
    fn a_short_conversation_gets_no_breakpoints() {
        let mut body = conversation(3);
        assert_eq!(place_cache_control(&mut body, payg()), 0);
    }

    #[test]
    fn placement_never_exceeds_the_provider_limit() {
        for turns in [4usize, 5, 9, 20, 100, 500] {
            let mut body = conversation(turns);
            assert!(
                place_cache_control(&mut body, payg()) <= MAX_BREAKPOINTS,
                "{turns} turns exceeded the limit"
            );
        }
    }

    #[test]
    fn placement_is_deterministic() {
        let first = {
            let mut body = conversation(17);
            place_cache_control(&mut body, payg());
            body
        };
        for _ in 0..20 {
            let mut body = conversation(17);
            place_cache_control(&mut body, payg());
            assert_eq!(body, first);
        }
    }

    // ---- prompt_cache_key ----

    #[test]
    fn a_cache_key_is_injected_when_permitted() {
        let mut body = json!({"model": "gpt-4"});
        assert!(inject_prompt_cache_key(&mut body, payg(), "session-1"));
        assert_eq!(body["prompt_cache_key"], "session-1");
    }

    #[test]
    fn a_customer_key_is_never_overwritten() {
        // Theirs is presumably chosen to group requests the way they want; replacing
        // it would scatter their cache.
        let mut body = json!({"prompt_cache_key": "theirs"});
        assert!(!inject_prompt_cache_key(&mut body, payg(), "ours"));
        assert_eq!(body["prompt_cache_key"], "theirs");
    }

    #[test]
    fn a_restricted_policy_injects_nothing() {
        let mut body = json!({"model": "gpt-4"});
        assert!(!inject_prompt_cache_key(
            &mut body,
            restricted(),
            "session-1"
        ));
        assert!(body.get("prompt_cache_key").is_none());
    }
}
