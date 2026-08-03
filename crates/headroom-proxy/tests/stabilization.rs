//! Cache stabilization, with the opt-in actually turned on.
//!
//! # Why this is its own test binary
//!
//! `stabilize` is gated on `HEADROOM_STABILIZE`, and the gate is the first thing it does.
//! Every other test in this workspace runs with the flag unset, so the whole of
//! stabilization was reached exactly once from `stabilize` itself — by
//! `stabilization_is_off_unless_the_operator_opts_in`, which asserts it does *nothing*.
//!
//! Everything past that guard clause — tool normalization, breakpoint placement, the
//! dialect condition — was tested only by calling the private helpers directly. That is
//! the shape this project has been bitten by five times: a test proves a function works,
//! not that anything calls it. If `stabilize` stopped calling `normalized_tools_member`,
//! or the Anthropic condition inverted, every test would still pass.
//!
//! Turning the flag on in-process is not an option. `Config` reads it globally, so
//! flipping it would leak into the handler tests running in parallel and break their I2
//! byte-identity assertions — the exact flakiness this sweep introduced once already and
//! had to back out. Cargo gives each `tests/*.rs` its own binary and therefore its own
//! process, so setting the variable here affects nothing else.

use headroom_proxy::compression::Dialect;
use headroom_proxy::stabilization::stabilize;

use headroom_core::auth_mode::{AuthMode, CompressionPolicy};
use serde_json::Value;

/// Enables stabilization for this process.
///
/// Called by every test here rather than once, because test order within a binary is not
/// defined and `std::env` is the only state they share.
fn enable() {
    std::env::set_var("HEADROOM_STABILIZE", "1");
}

fn payg() -> CompressionPolicy {
    CompressionPolicy::for_mode(AuthMode::PayAsYouGo)
}

/// A conversation long enough for breakpoints to have somewhere to go, with tools in an
/// order normalization will change.
fn conversation(turns: usize) -> String {
    let messages: Vec<String> = (0..turns)
        .map(|i| format!(r#"{{"role":"user","content":"turn {i}"}}"#))
        .collect();
    format!(
        r#"{{"model":"claude-opus-4","tools":[{{"name":"zebra"}},{{"name":"apple"}}],"messages":[{}]}}"#,
        messages.join(",")
    )
}

#[test]
fn enabling_it_actually_changes_the_request() {
    // The claim the off-by-default test cannot make. Without this, `stabilize` could
    // return the body untouched in every case and nothing would notice.
    enable();
    let source = conversation(10);

    let out = stabilize(Dialect::Anthropic, source.as_bytes(), payg());

    assert_ne!(
        out.as_ref(),
        source.as_bytes(),
        "stabilization is enabled and changed nothing"
    );
}

#[test]
fn tools_are_normalized_into_a_stable_order() {
    // Half of what stabilization is for: two agents sending the same tools in different
    // orders otherwise miss each other's cache entirely.
    enable();
    let source = conversation(10);

    let out = stabilize(Dialect::Anthropic, source.as_bytes(), payg());
    let parsed: Value = serde_json::from_slice(out.as_ref()).expect("not JSON");

    let names: Vec<&str> = parsed["tools"]
        .as_array()
        .expect("tools missing")
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or_default())
        .collect();

    assert_eq!(names, vec!["apple", "zebra"], "tools were not reordered");
}

#[test]
fn breakpoints_are_placed_on_an_anthropic_request() {
    // The other half. Anthropic caches what the customer marks, so with no marker there
    // is nothing cached at all — which is why this feature exists despite touching the
    // zone I2 protects.
    enable();
    let source = conversation(20);

    let out = stabilize(Dialect::Anthropic, source.as_bytes(), payg());
    let parsed: Value = serde_json::from_slice(out.as_ref()).expect("not JSON");

    let marked = parsed["messages"]
        .as_array()
        .expect("messages missing")
        .iter()
        .filter(|message| message.get("cache_control").is_some())
        .count();

    assert!(marked > 0, "no breakpoint was placed:\n{parsed:#}");
}

#[test]
fn openai_gets_normalized_tools_and_no_breakpoints() {
    // The dialect condition, which was reachable only by reading it. Both OpenAI surfaces
    // cache prefixes automatically and need no marker; placing one would add a member the
    // provider does not read, changing the bytes for nothing.
    enable();
    let source = conversation(20);

    for dialect in [Dialect::OpenAi, Dialect::OpenAiResponses] {
        let out = stabilize(dialect, source.as_bytes(), payg());
        let parsed: Value = serde_json::from_slice(out.as_ref()).expect("not JSON");

        let names: Vec<&str> = parsed["tools"]
            .as_array()
            .expect("tools missing")
            .iter()
            .map(|tool| tool["name"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(names, vec!["apple", "zebra"], "{dialect:?}");

        let marked = parsed["messages"]
            .as_array()
            .expect("messages missing")
            .iter()
            .filter(|message| message.get("cache_control").is_some())
            .count();
        assert_eq!(marked, 0, "{dialect:?} got a breakpoint it cannot use");
    }
}

#[test]
fn a_restricted_policy_gets_no_breakpoints_even_when_enabled() {
    // I10 outranks the opt-in. A subscription token forbids `auto_cache_control`, and a
    // marker is a modification the provider can see — so enabling stabilization must not
    // be a way around the policy.
    enable();
    let source = conversation(20);
    let restricted = CompressionPolicy::for_mode(AuthMode::Subscription);

    let out = stabilize(Dialect::Anthropic, source.as_bytes(), restricted);
    let parsed: Value = serde_json::from_slice(out.as_ref()).expect("not JSON");

    let marked = parsed["messages"]
        .as_array()
        .expect("messages missing")
        .iter()
        .filter(|message| message.get("cache_control").is_some())
        .count();

    assert_eq!(marked, 0, "a restricted policy was given a breakpoint");
}

/// A pretty-printed request with tools in an order normalization will change.
///
/// Written by hand rather than through `to_string_pretty`, so it carries spacing no
/// serializer here would choose — the same reason `invariants.rs` builds its I1 fixture
/// that way.
fn pretty_conversation() -> String {
    concat!(
        "{\n",
        "    \"model\" : \"claude-opus-4\",\n",
        "    \"max_tokens\": 4096,\n",
        "    \"system\":   \"You are a careful assistant.\",\n",
        "    \"tools\": [ { \"name\": \"zebra\" }, { \"name\": \"apple\" } ],\n",
        "    \"messages\": [\n",
        "        { \"role\": \"user\",  \"content\": \"a short question\" }\n",
        "    ]\n",
        "}"
    )
    .to_owned()
}

#[test]
fn stabilizing_twice_changes_nothing_the_second_time() {
    // The property that makes the whole trade acceptable, and nothing tested it.
    //
    // Stabilization deliberately rewrites the cache hot zone (D20), which costs one
    // invalidation on the turn it is switched on. That is a price worth paying only if it
    // is paid *once*. A stabilizer that is not a fixed point would rewrite the prefix on
    // every turn and invalidate the cache every turn — the exact opposite of the feature's
    // purpose, delivered silently, on the long conversations it exists to help.
    enable();

    let once = stabilize(Dialect::Anthropic, conversation(20).as_bytes(), payg()).into_owned();
    let twice = stabilize(Dialect::Anthropic, &once, payg()).into_owned();
    let thrice = stabilize(Dialect::Anthropic, &twice, payg()).into_owned();

    assert_eq!(once, twice, "a second pass changed the request again");
    assert_eq!(twice, thrice, "a third pass changed the request again");

    // Not vacuous: the first pass has to have done something, or three no-ops agree.
    assert_ne!(
        once,
        conversation(20).as_bytes(),
        "nothing was stabilized, so the fixed point is trivial"
    );
}

#[test]
fn the_same_request_stabilizes_to_the_same_bytes() {
    // I4 for this path. Two identical requests that stabilize differently would miss each
    // other's cache entry forever, which is the failure this feature exists to prevent.
    enable();
    let source = conversation(20);

    let first = stabilize(Dialect::Anthropic, source.as_bytes(), payg()).into_owned();
    for _ in 0..4 {
        assert_eq!(
            stabilize(Dialect::Anthropic, source.as_bytes(), payg()).as_ref(),
            first.as_slice(),
            "stabilization was not deterministic"
        );
    }
}

#[test]
fn a_pretty_printed_body_is_reflowed_once_and_then_left_alone() {
    // Measured, and worth pinning rather than discovering later.
    //
    // `replace_top_level_member` rebuilds the object framing compactly, so enabling
    // stabilization reflows a pretty-printing client's top-level whitespace —
    // `"model" : "claude-opus-4"` becomes `"model":"claude-opus-4"`. That is a byte change
    // beyond the tool reordering that was asked for, and it invalidates the cached prefix.
    //
    // It costs exactly one invalidation because the result is a fixed point, which is the
    // same one-time price the reordering itself charges. Values are still copied verbatim:
    // the pretty-printed `messages` array survives untouched, because every member is a
    // `RawValue`.
    enable();
    let source = pretty_conversation();

    let once = stabilize(Dialect::Anthropic, source.as_bytes(), payg()).into_owned();
    assert_ne!(once, source.as_bytes(), "the fixture did not stabilize");

    let twice = stabilize(Dialect::Anthropic, &once, payg()).into_owned();
    assert_eq!(once, twice, "a pretty-printed body kept being reflowed");

    // The message value keeps its original spacing — only the framing was rebuilt.
    let text = String::from_utf8(once).expect("not utf-8");
    assert!(
        text.contains("\"role\": \"user\",  \"content\""),
        "a value was re-serialized rather than copied:\n{text}"
    );
}

/// Adds turns to an already-stabilized body, the way an agent loop does.
fn grown_to(body: &[u8], turns: usize) -> Vec<u8> {
    let mut parsed: Value = serde_json::from_slice(body).expect("not JSON");
    let messages = parsed["messages"].as_array_mut().expect("messages missing");
    for i in messages.len()..turns {
        messages.push(serde_json::json!({"role": "user", "content": format!("turn {i}")}));
    }
    serde_json::to_vec(&parsed).expect("could not re-serialize")
}

/// How many messages carry a breakpoint.
fn breakpoints(body: &[u8]) -> usize {
    let parsed: Value = serde_json::from_slice(body).expect("not JSON");
    parsed["messages"]
        .as_array()
        .expect("messages missing")
        .iter()
        .filter(|message| message.get("cache_control").is_some())
        .count()
}

#[test]
fn a_growing_conversation_gains_the_anchors_it_becomes_long_enough_for() {
    // Stabilization used to be a one-shot. `breakpoints_for` bailed on *any* existing
    // marker — a check whose stated reason is that a customer-set marker means they have
    // thought about it — and could not tell the customer's markers from the ones it placed
    // last turn.
    //
    // An agent loop stabilizes every turn, so the first stabilized turn placed whatever
    // anchors were usable then and every later turn saw markers and bailed. Measured: a
    // conversation stabilized fresh at 20 messages got 4 breakpoints; one stabilized at 5
    // and grown to 20 got 2, permanently. Half the breakpoints, on exactly the long
    // conversations the feature exists to help.
    enable();

    let mut body = stabilize(Dialect::Anthropic, conversation(5).as_bytes(), payg()).into_owned();
    assert_eq!(breakpoints(&body), 2, "at 5 messages");

    for (turns, expected) in [(9usize, 3usize), (20, 4)] {
        body = grown_to(&body, turns);
        body = stabilize(Dialect::Anthropic, &body, payg()).into_owned();

        // The same count a conversation of this length gets from a standing start, which
        // is the property that was missing.
        let source = conversation(turns);
        let fresh = stabilize(Dialect::Anthropic, source.as_bytes(), payg());
        assert_eq!(
            breakpoints(&body),
            expected,
            "grown to {turns} messages, got {} breakpoints",
            breakpoints(&body)
        );
        assert_eq!(
            breakpoints(&body),
            breakpoints(fresh.as_ref()),
            "a grown conversation and a fresh one of the same length disagree at {turns}"
        );
    }
}

#[test]
fn a_marker_the_customer_placed_is_still_left_alone() {
    // The half that must not regress. A customer-set breakpoint means they have thought
    // about it, and adding more could push past the provider's limit and silently
    // invalidate the ones they chose. Only markers sitting exactly on anchors are treated
    // as ours to extend.
    enable();

    let mut parsed: Value = serde_json::from_str(&conversation(20)).expect("fixture is not JSON");
    // Message 6 is not an anchor — [1, 3, 7, 15] are.
    parsed["messages"][6]
        .as_object_mut()
        .expect("message 6 missing")
        .insert(
            "cache_control".into(),
            serde_json::json!({"type": "ephemeral"}),
        );
    let source = serde_json::to_vec(&parsed).expect("could not re-serialize");

    let out = stabilize(Dialect::Anthropic, &source, payg());

    assert_eq!(
        breakpoints(out.as_ref()),
        1,
        "breakpoints were added beside a customer's own"
    );
}
