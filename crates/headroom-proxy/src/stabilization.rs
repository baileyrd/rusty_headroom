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

use std::borrow::Cow;

use headroom_core::auth_mode::CompressionPolicy;
use serde_json::{Map, Value};

use crate::body::FaithfulBody;
use crate::compression::Dialect;

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

/// Where breakpoints go, as fixed indices from the start of the conversation.
///
/// # Why fixed anchors rather than an even spread
///
/// The obvious rule — spread `MAX_BREAKPOINTS` evenly across the frozen portion — is
/// worse than placing none at all. A conversation grows by two messages a turn, so an
/// even spread recomputes to a *different* set every couple of turns, and the index that
/// moves first is the earliest one. Moving the earliest breakpoint rewrites bytes at the
/// head of the prefix, which invalidates the entire cache rather than its tail. The
/// feature would then bust the cache periodically on exactly the long conversations it
/// exists to help.
///
/// These anchors are monotone: as the conversation grows, breakpoints are only ever
/// added, never moved. A marker placed at index 3 on turn two is still at index 3 on
/// turn twenty, so every prefix that was cached stays cached.
///
/// They double because the value of a breakpoint is the prefix behind it, and prefixes
/// worth caching grow geometrically rather than linearly.
const ANCHORS: [usize; MAX_BREAKPOINTS] = [1, 3, 7, 15];

/// Places `cache_control` breakpoints on stable prefix boundaries.
///
/// No-op unless the policy permits it. Returns how many were placed.
///
/// Breakpoints go on the *earliest* messages, not the latest. The prefix before a
/// breakpoint is what gets cached, so marking late in the conversation caches almost
/// nothing; marking early caches the bulk that never changes.
pub fn place_cache_control(body: &mut Value, policy: CompressionPolicy) -> usize {
    place_at(body, policy, &breakpoints_for(body))
}

/// The message indices that should carry a breakpoint.
///
/// Empty when the policy forbids it, when the customer has already set a marker, or when
/// there is not enough history to be worth caching.
pub fn breakpoints_for(body: &Value) -> Vec<usize> {
    // A customer-set marker means they have thought about this. Adding more could push
    // past the provider's limit and silently invalidate the ones they chose.
    if has_cache_control(body) {
        return Vec::new();
    }

    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return Vec::new();
    };
    // Nothing to cache if there is no history to speak of.
    if messages.len() < 4 {
        return Vec::new();
    }

    // The newest turn is left alone: it is exactly the part that changes next request,
    // so a breakpoint there caches a prefix that is already stale.
    let usable = messages.len().saturating_sub(1);
    ANCHORS
        .iter()
        .copied()
        .filter(|index| *index < usable)
        .collect()
}

/// Inserts the marker at each index in `indices`.
fn place_at(body: &mut Value, policy: CompressionPolicy, indices: &[usize]) -> usize {
    if !policy.auto_cache_control {
        return 0;
    }

    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return 0;
    };

    let mut placed = 0;
    for index in indices {
        if let Some(message) = messages.get_mut(*index).and_then(Value::as_object_mut) {
            message.insert(
                "cache_control".into(),
                serde_json::json!({ "type": "ephemeral" }),
            );
            placed += 1;
        }
    }

    placed
}

/// Adds a `cache_control` marker to one raw message, returning its new JSON.
///
/// Returns `None` if the message is not a JSON object or already carries a marker.
///
/// # Why this takes raw JSON rather than a parsed body
///
/// Only the messages that gain a marker are re-serialized; every other byte of the
/// request — including every other message — is copied verbatim by
/// [`FaithfulBody::rebuild`]. Round-tripping the whole body through `Value` instead
/// would rewrite the untouched frozen prefix, costing the cache miss this function
/// exists to avoid.
///
/// [`FaithfulBody::rebuild`]: crate::body::FaithfulBody::rebuild
pub fn mark_message(raw: &str) -> Option<String> {
    let mut message: Value = serde_json::from_str(raw).ok()?;
    let object = message.as_object_mut()?;

    if object.contains_key("cache_control") {
        return None;
    }
    object.insert(
        "cache_control".into(),
        serde_json::json!({ "type": "ephemeral" }),
    );

    serde_json::to_string(&message).ok()
}

/// Injects `prompt_cache_key` on OpenAI-shaped requests, when permitted.
///
/// Never overwrites one the customer set: their key is presumably chosen to group
/// requests the way they want, and replacing it would scatter their cache.
///
/// # Not the request path
///
/// The proxy inserts this key in [`openai::shape_openai`], which uses
/// [`body::insert_top_level_member`] and so rewrites nothing but the new member. This
/// version works on a parsed [`Value`] and is for callers that already hold one; routing
/// a request through it would re-serialize the whole body and cost the cache miss the
/// key was inserted to avoid.
///
/// [`openai::shape_openai`]: crate::openai
/// [`body::insert_top_level_member`]: crate::body::insert_top_level_member
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

/// Applies every cache-stabilizing rewrite to a request body.
///
/// Returns the original bytes whenever nothing applies, so a body that is already
/// canonical costs no rebuild — invariant I1.
///
/// # Order, and why it is this one
///
/// Tool normalization runs on every auth mode because sorting discards nothing and
/// reveals nothing: the same tools with the same schemas, in a canonical order. The two
/// *injections* run only where the policy permits them, because adding a marker to
/// someone's request is a change they did not ask for — on OAuth it could fall outside
/// the granted scope, and on subscription traffic it makes the request identifiably
/// proxied (invariant I10).
///
/// Each step rewrites only what it touches. Nothing here round-trips the whole body
/// through `Value`, which would rewrite the untouched frozen prefix and cost the cache
/// miss all of this exists to avoid.
pub fn stabilize<'a>(dialect: Dialect, body: &'a [u8], policy: CompressionPolicy) -> Cow<'a, [u8]> {
    // Off unless the operator opted in. Everything below rewrites the cache hot zone,
    // which invariant I2 says is never modified — see `Config::stabilization_enabled`
    // for why that is a decision to make deliberately rather than a default.
    if !crate::config::Config::stabilization_enabled() {
        return Cow::Borrowed(body);
    }

    let mut current = Cow::Borrowed(body);

    if let Some(normalized) = normalized_tools_member(&current) {
        current = Cow::Owned(normalized);
    }

    // Breakpoints are Anthropic-only: it caches what the customer marks, so a marker is
    // the only way to cache anything at all. Both OpenAI surfaces cache prefixes
    // automatically and need no marker — their stabilization is `prompt_cache_key`,
    // which `openai::shape_openai` already inserts byte-faithfully on the request path.
    if dialect == Dialect::Anthropic && policy.auto_cache_control {
        if let Some(marked) = marked_body(&current) {
            current = Cow::Owned(marked);
        }
    }

    current
}

/// The request's `tools` member, normalized — or `None` if it is already canonical.
fn normalized_tools_member(body: &[u8]) -> Option<Vec<u8>> {
    let mut parsed: Value = serde_json::from_slice(body).ok()?;
    if !normalize_tools(&mut parsed) {
        return None;
    }

    let tools = serde_json::to_string(parsed.get("tools")?).ok()?;
    crate::body::replace_top_level_member(body, "tools", &tools)
}

/// The request with breakpoints placed — or `None` if none apply.
fn marked_body(body: &[u8]) -> Option<Vec<u8>> {
    let parsed: Value = serde_json::from_slice(body).ok()?;
    let indices = breakpoints_for(&parsed);
    if indices.is_empty() {
        return None;
    }

    let faithful = FaithfulBody::parse(body);
    if !faithful.is_understood() {
        return None;
    }

    // Only the marked messages are re-serialized. Every other message is copied
    // verbatim, which is what keeps the rest of the frozen prefix cacheable.
    let replacements: Vec<(usize, String)> = indices
        .iter()
        .filter_map(|index| Some((*index, mark_message(faithful.message(*index)?)?)))
        .collect();
    if replacements.is_empty() {
        return None;
    }

    match faithful.rebuild(&replacements) {
        Cow::Owned(bytes) => Some(bytes),
        // Borrowed means nothing was substituted, so there is nothing to report.
        Cow::Borrowed(_) => None,
    }
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
    // ---- the opt-in path (gap rows X15, I7) ----

    /// A conversation of `turns` messages as raw JSON, tools deliberately out of order.
    fn raw_conversation(turns: usize) -> String {
        let messages: Vec<String> = (0..turns)
            .map(|i| {
                let role = if i % 2 == 0 { "user" } else { "assistant" };
                format!(r#"{{"role":"{role}","content":"m{i}"}}"#)
            })
            .collect();
        format!(
            r#"{{"model":"claude-opus-4","tools":[{{"name":"zebra"}},{{"name":"apple"}}],"messages":[{}]}}"#,
            messages.join(",")
        )
    }

    #[test]
    fn stabilization_is_off_unless_the_operator_opts_in() {
        // Invariant I2. Everything here rewrites the hot zone, so the default must leave
        // the request byte-identical — that is what keeps the I2 integration tests
        // meaningful rather than merely passing.
        let source = raw_conversation(10);
        let out = stabilize(Dialect::Anthropic, source.as_bytes(), payg());

        assert_eq!(out.as_ref(), source.as_bytes());
        assert!(matches!(out, Cow::Borrowed(_)), "the body was rebuilt");
    }

    #[test]
    fn breakpoints_never_move_as_the_conversation_grows() {
        // The property that makes the trade positive. An even spread recomputes to a
        // different set every couple of turns, and the index that moves first is the
        // earliest — rewriting the head of the prefix and invalidating the whole cache,
        // periodically, on exactly the long conversations this is meant to help.
        let mut previous: Vec<usize> = Vec::new();

        for turns in 4..40 {
            let body: Value = serde_json::from_str(&raw_conversation(turns)).unwrap();
            let current = breakpoints_for(&body);

            assert!(
                previous.iter().all(|index| current.contains(index)),
                "a breakpoint moved at {turns} messages: {previous:?} -> {current:?}"
            );
            assert!(current.len() <= MAX_BREAKPOINTS);
            previous = current;
        }
    }

    #[test]
    fn a_customer_marker_suppresses_every_automatic_one() {
        // They have thought about this. Adding more could push past the provider's limit
        // and silently invalidate the ones they chose.
        let body: Value = serde_json::from_str(
            r#"{"messages":[{"role":"user","content":"a","cache_control":{"type":"ephemeral"}},
                {"role":"assistant","content":"b"},{"role":"user","content":"c"},
                {"role":"assistant","content":"d"},{"role":"user","content":"e"}]}"#,
        )
        .unwrap();

        assert!(breakpoints_for(&body).is_empty());
    }

    #[test]
    fn marking_a_message_leaves_the_rest_of_the_body_byte_identical() {
        // Only the marked messages are re-serialized. Round-tripping the whole body
        // through `Value` would rewrite the untouched frozen prefix, costing the very
        // cache miss the marker is placed to avoid.
        let source = raw_conversation(10);
        let faithful = FaithfulBody::parse(source.as_bytes());
        let marked = mark_message(faithful.message(1).unwrap()).unwrap();
        let rebuilt = faithful.rebuild(&[(1, marked)]);
        let out = String::from_utf8(rebuilt.into_owned()).unwrap();

        // Every other message survives verbatim.
        for index in [0, 2, 3, 9] {
            assert!(
                out.contains(faithful.message(index).unwrap()),
                "message {index} was re-serialized"
            );
        }
        assert!(out.contains(r#""cache_control":{"type":"ephemeral"}"#));
    }

    #[test]
    fn marking_a_message_that_is_already_marked_reports_nothing() {
        assert!(mark_message(r#"{"role":"user","cache_control":{"type":"ephemeral"}}"#).is_none());
        assert!(mark_message("not json").is_none());
        assert!(mark_message("[1,2]").is_none());
    }

    #[test]
    fn already_sorted_tools_are_not_rewritten() {
        // Invariant I1. Rebuilding a body to write back what it already said would cost
        // a cache miss to change nothing.
        let source = r#"{"model":"m","tools":[{"name":"apple"},{"name":"zebra"}]}"#;
        let mut parsed: Value = serde_json::from_str(source).unwrap();

        assert!(!normalize_tools(&mut parsed));
        assert!(crate::body::replace_top_level_member(
            source.as_bytes(),
            "tools",
            r#"[{"name":"apple"},{"name":"zebra"}]"#
        )
        .is_none());
    }
}
