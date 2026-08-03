//! The invariant gates — gap row E2.
//!
//! These are not unit tests of a module. They are the guarantees the project claims,
//! asserted end to end through a real proxy talking to a real (loopback) provider, so
//! that a refactor which upholds every module contract but breaks the *system* property
//! still fails here.
//!
//! Four invariants are gated:
//!
//! | I1 | Byte-faithful passthrough — SHA-256 equality on unmutated bytes |
//! | I2 | The cache hot zone is never modified |
//! | I3 | Append-only — compressing twice reaches no further back than once |
//! | I4 | Determinism — same input, byte-equal output |
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
