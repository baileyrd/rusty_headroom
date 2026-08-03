//! The invariant gates — gap row E2.
//!
//! These are not unit tests of a module. They are the guarantees the project claims,
//! asserted end to end through a real proxy talking to a real (loopback) provider, so
//! that a refactor which upholds every module contract but breaks the *system* property
//! still fails here.
//!
//! The invariants gated here:
//!
//! | I1 | Byte-faithful passthrough — SHA-256 equality on unmutated bytes |
//! | I2 | The cache hot zone is never modified |
//! | I3 | Append-only — compressing twice reaches no further back than once |
//! | I4 | Determinism — same input, byte-equal output |
//! | I6 | Position-preserving — surviving content keeps its order and its block |
//! | I7 | Tool definitions are never compressed |
//! | I8 | Signed and encrypted blocks are passthrough-only |
//! | I9 | Telemetry observes and never alters |
//!
//! I5 (token-aware) and I10 (auth mode gates policy) are gated by `properties.rs`
//! instead, because both are statements about *many* inputs rather than one: "never
//! larger, for any body" and "never modified, under any restricted policy" are
//! properties a single fixture cannot establish.
//!
//! # Why they run against the simulator rather than against `compress_request`
//!
//! `compress_request` is already unit-tested for all four. What those tests cannot show
//! is that the property survives the relay: header rebuilding, hyper's framing, chunked
//! transfer encoding, and the `Cow` passthrough all sit between the pure function and
//! the provider. A regression in any of them breaks the guarantee while every unit test
//! stays green.

use headroom_proxy::server::{router_with, AppState};
use headroom_simulators::Simulator;
use sha2::{Digest, Sha256};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

fn sha(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Sends `body` through a proxy pointed at `simulator` and returns the status.
async fn through_proxy(simulator: &Simulator, body: &str, api_key: &str) -> StatusCode {
    let app = router_with(AppState::new(simulator.base_url()));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("x-api-key", api_key)
                .header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    // Drained so the observing stream completes; otherwise the request is only
    // half-finished when the assertion runs.
    let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024).await;
    status
}

/// A request whose bytes a careless round-trip would mangle: a trailing-zero float, an
/// integer past 2^53, CJK, emoji, an escaped string, and unusual key order.
fn hostile_request() -> String {
    concat!(
        r#"{"zzz":1.0,"max_tokens":9007199254740993,"model":"claude-opus-4","#,
        r#""system":"Ünïcödé 日本語 😀","#,
        r#""tools":[{"name":"read","input_schema":{"type":"object","b":1,"a":2}}],"#,
        r#""messages":[{"role":"user","content":"escaped \" quote and \\ backslash"}],"#,
        r#""aaa":true}"#
    )
    .to_owned()
}

/// A request with a bulky live tool result and five frozen turns.
fn compressible_request() -> String {
    let records: Vec<String> = (0..120)
        .map(|i| {
            format!(
                r#"{{\"path\":\"src/module_{i}.rs\",\"kind\":\"file\",\"status\":\"ok\",\"size\":{}}}"#,
                1000 + i
            )
        })
        .collect();

    format!(
        r#"{{"model":"claude-opus-4","max_tokens":4096,"system":"You are a careful assistant.","tools":[{{"name":"read_file","input_schema":{{"type":"object"}}}}],"messages":[{{"role":"user","content":"turn one"}},{{"role":"assistant","content":"answer one"}},{{"role":"user","content":"turn two"}},{{"role":"assistant","content":"answer two"}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t_old","content":"small older result"}}]}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t_new","content":"[{}]"}}]}}]}}"#,
        records.join(",")
    )
}

// ---- I1 ----

#[tokio::test]
async fn i1_a_request_with_nothing_to_compress_reaches_the_provider_byte_identical() {
    // The strongest form of I1: SHA-256 equality, across the shapes that a `Value`
    // round-trip silently changes. A proxy that fails this makes every request more
    // expensive than having no proxy at all, and nothing in the response says why.
    let simulator = Simulator::anthropic().await.unwrap();
    let source = hostile_request();

    assert_eq!(
        through_proxy(&simulator, &source, "sk-ant-api03-x").await,
        StatusCode::OK
    );

    let received = simulator.recorder().last().expect("nothing arrived");
    assert_eq!(
        sha(&received.body),
        sha(source.as_bytes()),
        "the proxy rewrote a request it had no reason to touch"
    );
}

#[tokio::test]
async fn i1_holds_for_every_auth_mode() {
    // Subscription and OAuth forbid the lossy compressors entirely, so their traffic
    // must arrive byte-identical however compressible it looks.
    let source = compressible_request();

    for credential in ["Bearer sk-ant-oat01-x", "Bearer opaque-session-token"] {
        let simulator = Simulator::anthropic().await.unwrap();
        let app = router_with(AppState::new(simulator.base_url()));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("authorization", credential)
                    .body(Body::from(source.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024).await;

        let received = simulator.recorder().last().expect("nothing arrived");
        assert_eq!(
            sha(&received.body),
            sha(source.as_bytes()),
            "{credential} traffic was modified"
        );
    }
}

// ---- I2 ----

#[tokio::test]
async fn i2_the_hot_zone_survives_a_compressed_request_unchanged() {
    // Compression must actually have happened, or this passes vacuously — asserted
    // first, then the hot zone is hashed member by member.
    let simulator = Simulator::anthropic().await.unwrap();
    let source = compressible_request();

    through_proxy(&simulator, &source, "sk-ant-api03-x").await;

    let received = simulator.recorder().last().expect("nothing arrived");
    assert!(
        received.body.len() < source.len(),
        "nothing was compressed, so this assertion proves nothing"
    );

    let before: serde_json::Value = serde_json::from_str(&source).unwrap();
    let after: serde_json::Value = serde_json::from_slice(&received.body).unwrap();

    for member in ["system", "tools", "model", "max_tokens"] {
        assert_eq!(
            sha(serde_json::to_string(&before[member]).unwrap().as_bytes()),
            sha(serde_json::to_string(&after[member]).unwrap().as_bytes()),
            "{member} was modified"
        );
    }

    for index in 0..5 {
        assert_eq!(
            sha(serde_json::to_string(&before["messages"][index])
                .unwrap()
                .as_bytes()),
            sha(serde_json::to_string(&after["messages"][index])
                .unwrap()
                .as_bytes()),
            "frozen turn {index} was modified"
        );
    }
}

#[tokio::test]
async fn i2_the_frozen_prefix_arrives_as_a_literal_byte_substring() {
    // Stronger than comparing parsed values: the raw prefix of the request, up to the
    // live message, must appear verbatim in what the provider received. Equal-parsing
    // JSON can still be different bytes, and different bytes miss the cache.
    let simulator = Simulator::anthropic().await.unwrap();
    let source = compressible_request();

    through_proxy(&simulator, &source, "sk-ant-api03-x").await;

    let received = simulator.recorder().last().expect("nothing arrived");
    let prefix_end = source
        .find(r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"t_new""#)
        .unwrap();

    assert!(
        received.text().starts_with(&source[..prefix_end]),
        "the frozen prefix was re-serialized rather than copied"
    );
}

// ---- I3 ----

#[tokio::test]
async fn i3_compressing_an_already_compressed_request_reaches_no_further_back() {
    // Append-only. A second pass over the proxy's own output must be a fixed point;
    // anything else means each turn of an agent loop erodes a little more history.
    let first = Simulator::anthropic().await.unwrap();
    through_proxy(&first, &compressible_request(), "sk-ant-api03-x").await;
    let once = first.recorder().last().unwrap().body;

    let second = Simulator::anthropic().await.unwrap();
    through_proxy(
        &second,
        &String::from_utf8(once.clone()).unwrap(),
        "sk-ant-api03-x",
    )
    .await;
    let twice = second.recorder().last().unwrap().body;

    assert_eq!(sha(&twice), sha(&once), "a second pass compressed further");
}

// ---- I4 ----

#[tokio::test]
async fn i4_the_same_request_produces_byte_equal_output_every_time() {
    // No clocks, no RNG, no dependence on accumulated state. A failure here means a
    // production incident cannot be reproduced from the failing request alone.
    let source = compressible_request();
    let mut hashes = Vec::new();

    for _ in 0..8 {
        let simulator = Simulator::anthropic().await.unwrap();
        through_proxy(&simulator, &source, "sk-ant-api03-x").await;
        hashes.push(sha(&simulator.recorder().last().unwrap().body));
    }

    assert!(
        hashes.windows(2).all(|pair| pair[0] == pair[1]),
        "compression was not deterministic: {hashes:?}"
    );
}

#[tokio::test]
async fn i4_holds_across_separate_proxy_instances() {
    // A fresh process must agree with a warm one. If it does not, the CCR store or some
    // other accumulated state is leaking into the output, and the recorded hash of a
    // request stops being a property of that request.
    let source = compressible_request();

    let warm = Simulator::anthropic().await.unwrap();
    let app = router_with(AppState::new(warm.base_url()));
    // Two requests through the *same* state, so the second sees a populated CCR store.
    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("x-api-key", "sk-ant-api03-x")
                    .body(Body::from(source.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024).await;
    }
    let warm_hash = sha(&warm.recorder().last().unwrap().body);

    let cold = Simulator::anthropic().await.unwrap();
    through_proxy(&cold, &source, "sk-ant-api03-x").await;
    let cold_hash = sha(&cold.recorder().last().unwrap().body);

    assert_eq!(
        warm_hash, cold_hash,
        "a warm store produced different bytes from a cold one"
    );
}

// ---- the guarantee the invariants exist to protect ----

#[tokio::test]
async fn compression_measurably_helps_while_every_invariant_holds() {
    // The four invariants are all satisfiable by doing nothing at all. This asserts the
    // proxy is not passing them that way.
    let simulator = Simulator::anthropic().await.unwrap();
    let source = compressible_request();

    through_proxy(&simulator, &source, "sk-ant-api03-x").await;

    let received = simulator.recorder().last().unwrap();
    let ratio = 1.0 - (received.body.len() as f64 / source.len() as f64);
    assert!(
        ratio > 0.5,
        "only {:.1}% smaller; the invariants are being met by doing nothing",
        ratio * 100.0
    );
}

/// A request carrying a signed thinking block and an encrypted reasoning block beside
/// compressible bulk.
///
/// The bulk is what makes the test meaningful: without it the compressor would decline
/// for lack of anything to do, and the assertion would pass while proving nothing.
///
/// Built through `serde_json` rather than by formatting a string. The first version
/// interpolated a JSON array into a JSON *string* without escaping it, so the body never
/// parsed, the proxy forwarded it untouched, and the invariant assertion passed for
/// entirely the wrong reason. The "nothing was compressed" guard below is what caught it.
fn sacrosanct_request() -> String {
    let bulk: Vec<serde_json::Value> = (0..300)
        .map(
            |i| serde_json::json!({"path": format!("src/f{i}.rs"), "size": i * 10, "kind": "file"}),
        )
        .collect();

    serde_json::json!({
        "model": "claude-opus-4",
        "messages": [
            {"role": "user", "content": "first"},
            // The thinking block carries the *same* bulky JSON as the tool result below.
            // That is the whole point: if the sacrosanct guard were removed, a compressor
            // would happily rewrite this block, because the content is exactly the shape
            // it compresses best. A short thinking block would be left alone by the size
            // threshold alone, and the test would pass without the guard existing.
            {"role": "assistant", "content": [
                {"type": "thinking",
                 "thinking": serde_json::to_string(&bulk).unwrap(),
                 "signature": "SIG-DO-NOT-TOUCH-abc123"},
                {"type": "redacted_thinking", "data": "REDACTED-DO-NOT-TOUCH-xyz789"}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1",
                 "content": serde_json::to_string(&bulk).unwrap()}
            ]}
        ]
    })
    .to_string()
}

#[tokio::test]
async fn i8_signed_and_encrypted_blocks_arrive_untouched() {
    // The invariant with the loudest failure mode. A signature covers the exact bytes of
    // the thinking block; alter one and the provider rejects the whole request as
    // tampered-with. The customer sees a hard error, not a smaller saving.
    let simulator = Simulator::anthropic().await.unwrap();
    let source = sacrosanct_request();

    through_proxy(&simulator, &source, "sk-ant-api03-x").await;
    let received = simulator.recorder().last().expect("nothing arrived");

    assert!(
        received.body.len() < source.len(),
        "nothing was compressed, so this assertion proves nothing"
    );
    assert!(
        received.text().contains("SIG-DO-NOT-TOUCH-abc123"),
        "the signature was altered or dropped"
    );
    assert!(
        received.text().contains("REDACTED-DO-NOT-TOUCH-xyz789"),
        "redacted thinking data was altered or dropped"
    );

    // Stronger than substring presence: the whole block must survive verbatim, since a
    // signature covers its bytes and not merely its content.
    let before: serde_json::Value = serde_json::from_str(&source).unwrap();
    let after: serde_json::Value = serde_json::from_slice(&received.body).unwrap();
    assert_eq!(
        sha(serde_json::to_string(&before["messages"][1])
            .unwrap()
            .as_bytes()),
        sha(serde_json::to_string(&after["messages"][1])
            .unwrap()
            .as_bytes()),
        "the signed message was re-serialized"
    );
}

#[tokio::test]
async fn i7_tool_definitions_are_never_compressed() {
    // Tools sit in the cache hot zone and are normalized at most, never compressed —
    // and normalization is opt-in (D20), so the default path must leave them alone.
    // A compressed tool schema is a tool the model can no longer call correctly.
    let simulator = Simulator::anthropic().await.unwrap();
    let source = compressible_request();

    through_proxy(&simulator, &source, "sk-ant-api03-x").await;
    let received = simulator.recorder().last().expect("nothing arrived");

    let before: serde_json::Value = serde_json::from_str(&source).unwrap();
    let after: serde_json::Value = serde_json::from_slice(&received.body).unwrap();

    assert_eq!(before["tools"], after["tools"], "tools were modified");
    assert_eq!(
        before["system"], after["system"],
        "the system block was modified"
    );

    // Scoped to the hot zone rather than the whole body. A marker in the *tool result*
    // is compression working; the first version of this assertion scanned everything and
    // failed on exactly that, which would have read as an invariant breach.
    for member in ["tools", "system"] {
        assert!(
            !serde_json::to_string(&after[member])
                .unwrap()
                .contains("<<ccr:"),
            "a CCR marker reached {member}"
        );
    }
}

#[tokio::test]
async fn i6_surviving_content_keeps_its_position() {
    // Position-preserving. A compressor may replace a block's content, but the messages
    // must stay in order, keep their roles, and keep their block counts — a model
    // reading a reordered conversation is reading a different conversation.
    let simulator = Simulator::anthropic().await.unwrap();
    let source = compressible_request();

    through_proxy(&simulator, &source, "sk-ant-api03-x").await;
    let received = simulator.recorder().last().expect("nothing arrived");

    let before: serde_json::Value = serde_json::from_str(&source).unwrap();
    let after: serde_json::Value = serde_json::from_slice(&received.body).unwrap();

    let (before_msgs, after_msgs) = (
        before["messages"].as_array().unwrap(),
        after["messages"].as_array().unwrap(),
    );
    assert_eq!(
        before_msgs.len(),
        after_msgs.len(),
        "a message was added or dropped"
    );

    for (index, (b, a)) in before_msgs.iter().zip(after_msgs).enumerate() {
        assert_eq!(b["role"], a["role"], "message {index} changed role");

        if let (Some(b_blocks), Some(a_blocks)) = (b["content"].as_array(), a["content"].as_array())
        {
            assert_eq!(
                b_blocks.len(),
                a_blocks.len(),
                "message {index} changed block count"
            );
            for (block, (bb, ab)) in b_blocks.iter().zip(a_blocks).enumerate() {
                assert_eq!(
                    bb["type"], ab["type"],
                    "message {index} block {block} changed type"
                );
            }
        }
    }
}

#[tokio::test]
async fn i9_telemetry_records_without_altering_the_request() {
    // Observation must be observation. The same request is sent twice through separate
    // proxies; the bytes that reach the provider must be identical, and the metrics must
    // be non-empty — otherwise this passes by recording nothing.
    let simulator = Simulator::anthropic().await.unwrap();
    let source = compressible_request();

    let state = AppState::new(simulator.base_url());
    let metrics_before = state.metrics().render();

    let app = router_with(state.clone());
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("x-api-key", "sk-ant-api03-x")
                .header("content-type", "application/json")
                .body(Body::from(source.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024).await;
    let first = simulator.recorder().last().expect("nothing arrived").body;

    through_proxy(&simulator, &source, "sk-ant-api03-x").await;
    let second = simulator.recorder().last().expect("nothing arrived").body;

    assert_eq!(sha(&first), sha(&second), "observation changed the bytes");
    assert_ne!(
        state.metrics().render(),
        metrics_before,
        "nothing was recorded, so this test proves nothing"
    );
}
