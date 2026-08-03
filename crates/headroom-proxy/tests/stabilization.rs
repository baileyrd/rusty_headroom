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
