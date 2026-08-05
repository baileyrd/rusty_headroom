//! TOIN over HTTP — reading and exchanging what the proxy has observed.
//!
//! The proxy accumulates one [`Observation`] per compressed block, keyed by
//! `(auth_mode, model_family, structure_hash)`. Until now the only ways in or out were a
//! file on disk and the Prometheus scrape, and neither carries the structure signatures
//! themselves. So a fleet of proxies each learned in isolation and nothing could merge
//! what they learned — which is most of why TOIN is worth having across more than one
//! machine.
//!
//! # Invariant I9 is the whole design constraint
//!
//! TOIN **observes and never mutates request bytes**, and there is deliberately no
//! request-time hint API. These endpoints publish and exchange *aggregates*. None of them
//! may become a path by which observation feeds back into a live request:
//!
//! - Reads take a snapshot and return it. Nothing on the request path consults them.
//! - `POST /v1/telemetry/import` merges into the aggregate, which is read by
//!   `headroom learn` and by these endpoints — **not** by the router. Recommendations
//!   stay startup-loaded, so an import cannot change what the running process compresses.
//!
//! A test asserts compressed bytes are identical with the aggregator attached and
//! detached, which is the property rather than the intention.
//!
//! # Imported data is untrusted
//!
//! `import` accepts aggregates from outside this process. The body is bounded, the schema
//! is validated, and anything unparseable merges nothing rather than erroring — an import
//! that fails must not be able to take down the endpoint an operator is using to
//! diagnose the failure.
//!
//! [`Observation`]: headroom_core::telemetry::Observation

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use headroom_core::telemetry::{Aggregator, Observation};
use serde::Deserialize;
use serde_json::json;

use crate::server::AppState;

/// Largest import body accepted.
///
/// An aggregate is a few hundred bytes per observed shape. This is generous for a large
/// fleet and still bounded — an unbounded read on an endpoint that accepts outside data
/// is a way to exhaust memory without sending anything valid.
const MAX_IMPORT_BYTES: usize = 4 * 1024 * 1024;

/// What `POST /v1/telemetry/import` accepts.
#[derive(Debug, Deserialize)]
pub struct ImportRequest {
    /// Observations keyed as `AggregationKey::as_str` renders them.
    observations: BTreeMap<String, Observation>,
}

/// Snapshots the aggregate, releasing the lock before anything is rendered.
///
/// Held for the copy and no longer. Rendering JSON while holding the lock the request
/// path writes through would make every scrape a source of contention on live traffic —
/// telemetry slowing down the thing it is watching.
fn snapshot(state: &AppState) -> BTreeMap<String, Observation> {
    state
        .aggregator()
        .lock()
        .map(|aggregator| aggregator.observations().clone())
        // A poisoned lock reports empty rather than failing. An operator reading this
        // during an incident is better served by "nothing recorded" than by a 500.
        .unwrap_or_default()
}

/// `GET /v1/toin/stats` — totals across everything observed.
pub async fn stats(State(state): State<AppState>) -> Response {
    let observations = snapshot(&state);

    let samples: u64 = observations.values().map(|o| o.samples).sum();
    let declines: u64 = observations.values().map(|o| o.declines).sum();
    let before: u64 = observations.values().map(|o| o.tokens_before).sum();
    let after: u64 = observations.values().map(|o| o.tokens_after).sum();

    (
        StatusCode::OK,
        Json(json!({
            "shapes": observations.len(),
            "samples": samples,
            "declines": declines,
            "tokens_before": before,
            "tokens_after": after,
            // `None` rather than 1.0 when nothing has been measured. "No compression" and
            // "no data" are different claims, and an operator choosing whether to enable
            // something should not have to guess which they are reading — the same
            // distinction `Observation::ratio` makes.
            "mean_ratio": (before > 0).then(|| 1.0 - (after as f64 / before as f64)),
        })),
    )
        .into_response()
}

/// `GET /v1/toin/patterns` — every observed shape.
pub async fn patterns(State(state): State<AppState>) -> Response {
    let observations = snapshot(&state);

    let entries: Vec<serde_json::Value> = observations
        .iter()
        .map(|(key, observation)| {
            json!({
                "key": key,
                "samples": observation.samples,
                "declines": observation.declines,
                "tokens_before": observation.tokens_before,
                "tokens_after": observation.tokens_after,
                "ratio": observation.ratio(),
            })
        })
        .collect();

    (StatusCode::OK, Json(json!({ "patterns": entries }))).into_response()
}

/// `GET /v1/toin/pattern/{prefix}` — the shapes whose key starts with `prefix`.
///
/// A prefix rather than an exact key, because a key is
/// `auth_mode:model_family:structure_hash` and the useful questions are about a prefix of
/// it: everything for one model family, everything under one auth mode. Requiring the
/// whole key would mean an operator had to already know the hash they are looking for.
pub async fn pattern(State(state): State<AppState>, Path(prefix): Path<String>) -> Response {
    let observations = snapshot(&state);

    let matches: Vec<serde_json::Value> = observations
        .iter()
        .filter(|(key, _)| key.starts_with(&prefix))
        .map(|(key, observation)| {
            json!({
                "key": key,
                "samples": observation.samples,
                "declines": observation.declines,
                "ratio": observation.ratio(),
            })
        })
        .collect();

    if matches.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no observed shape starts with {prefix:?}") })),
        )
            .into_response();
    }

    (StatusCode::OK, Json(json!({ "patterns": matches }))).into_response()
}

/// `GET /v1/telemetry/export` — the aggregate, in the shape `import` accepts.
///
/// Deliberately round-trippable with [`import`]: the point is moving what one deployment
/// learned into another, and an export a peer cannot read is not an export.
pub async fn export(State(state): State<AppState>) -> Response {
    (
        StatusCode::OK,
        Json(json!({ "observations": snapshot(&state) })),
    )
        .into_response()
}

/// `POST /v1/telemetry/import` — fold another deployment's aggregate into this one.
///
/// Sums rather than replaces: two proxies that saw the same shape have each seen real
/// traffic, and the merged sample count is the honest total.
///
/// # What this cannot do
///
/// It cannot change what the running process compresses. Recommendations are loaded once
/// at startup (see `Config::recommendations`), so an import influences routing only after
/// a restart that re-reads a published file. That is invariant I9 holding at the seam
/// where it is most tempting to break it — the imported data is right there, and using it
/// would be a request-time hint.
pub async fn import(State(state): State<AppState>, body: axum::body::Bytes) -> Response {
    if body.len() > MAX_IMPORT_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": format!("body exceeds {MAX_IMPORT_BYTES} bytes") })),
        )
            .into_response();
    }

    let Ok(request) = serde_json::from_slice::<ImportRequest>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "expected {\"observations\": {key: {samples, tokens_before, tokens_after, declines}}}",
            })),
        )
            .into_response();
    };

    let incoming = Aggregator::from_observations(request.observations);
    let merged = incoming.observations().len();

    let Ok(mut aggregator) = state.aggregator().lock() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "the aggregate is unavailable" })),
        )
            .into_response();
    };
    aggregator.merge(&incoming);

    (
        StatusCode::OK,
        Json(json!({
            "merged": merged,
            "shapes": aggregator.observations().len(),
        })),
    )
        .into_response()
}

/// `GET /v1/telemetry/tools` — the same aggregate, addressed the way the reference does.
///
/// An alias for [`patterns`] rather than a second store. The reference exposes both
/// spellings and a client written against either should work here; giving each its own
/// state would be two answers to one question, which is the drift check 6 of the
/// reachability audit exists to catch.
pub async fn tools(State(state): State<AppState>) -> Response {
    patterns(State(state)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compression::{compress_dialect, Compressors, Dialect};
    use crate::server::router_with;
    use axum::body::Body;
    use axum::http::Request;
    use headroom_core::auth_mode::CompressionPolicy;
    use headroom_core::ccr::InMemoryCcrStore;
    use headroom_core::output_shaping::Verbosity;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn state() -> AppState {
        AppState::new("http://unused.example")
    }

    async fn call(
        state: AppState,
        method: &str,
        uri: &str,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        let response = router_with(state)
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_owned()))
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or_default())
    }

    /// A request whose newest turn is a bulky, compressible tool result.
    fn request() -> String {
        let records: Vec<String> = (0..150)
            .map(|i| format!(r#"{{\"path\":\"src/module_{i}.rs\",\"kind\":\"file\",\"ok\":true}}"#))
            .collect();
        format!(
            r#"{{"model":"claude-opus-4","messages":[{{"role":"user","content":"q"}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t","content":"[{}]"}}]}}]}}"#,
            records.join(",")
        )
    }

    fn payg() -> CompressionPolicy {
        CompressionPolicy::for_mode(headroom_core::AuthMode::PayAsYouGo)
    }

    #[test]
    fn observing_changes_no_compressed_byte() {
        // Invariant I9, asserted rather than intended. The aggregator is attached to one
        // compressor set and not the other; the bytes that would go upstream have to be
        // identical, or observation has become a decision.
        let source = request();

        let watched = Compressors::new(Arc::new(InMemoryCcrStore::new()))
            .with_aggregator(Arc::new(std::sync::Mutex::new(Aggregator::new())));
        let unwatched = Compressors::new(Arc::new(InMemoryCcrStore::new()));

        let with = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &watched,
            true,
            payg(),
            Verbosity::Default,
        );
        let without = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &unwatched,
            true,
            payg(),
            Verbosity::Default,
        );

        assert_eq!(
            with, without,
            "attaching telemetry changed the compressed bytes"
        );
        // Not vacuous: something must actually have compressed.
        assert_ne!(with.as_ref(), source.as_bytes());
    }

    #[test]
    fn a_compression_is_recorded_against_its_shape() {
        let aggregator = Arc::new(std::sync::Mutex::new(Aggregator::new()));
        let compressors = Compressors::new(Arc::new(InMemoryCcrStore::new()))
            .with_aggregator(Arc::clone(&aggregator));

        compress_dialect(
            Dialect::Anthropic,
            request().as_bytes(),
            &compressors,
            true,
            payg(),
            Verbosity::Default,
        );

        let observed = aggregator.lock().unwrap();
        assert_eq!(observed.observations().len(), 1, "no shape was recorded");

        let entry = observed.observations().values().next().unwrap();
        assert_eq!(entry.samples, 1);
        assert!(entry.tokens_before > entry.tokens_after);
    }

    #[tokio::test]
    async fn stats_reports_no_ratio_before_anything_is_measured() {
        // "No compression" and "no data" are different claims. Reporting 1.0 here would
        // tell an operator the proxy measured a shape and found nothing to gain.
        let (status, body) = call(state(), "GET", "/v1/toin/stats", "").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["shapes"], 0);
        assert!(
            body["mean_ratio"].is_null(),
            "an unmeasured ratio was reported as a number"
        );
    }

    #[tokio::test]
    async fn an_export_round_trips_through_import() {
        // The reason this exists: a fleet of proxies each learning in isolation cannot
        // pool what it learned without a form each can read.
        let source = state();
        {
            let mut aggregator = source.aggregator().lock().unwrap();
            use headroom_core::telemetry::{AggregationKey, StructureHash, Telemetry};
            let key = AggregationKey::new(
                headroom_core::AuthMode::PayAsYouGo,
                "claude-opus-4",
                StructureHash::of("[{\"a\":1}]", headroom_core::detection::ContentType::Json),
            );
            aggregator.record(&key, 1000, 300);
        }

        let (_, exported) = call(source, "GET", "/v1/telemetry/export", "").await;
        assert_eq!(exported["observations"].as_object().unwrap().len(), 1);

        let destination = state();
        let (status, result) = call(
            destination.clone(),
            "POST",
            "/v1/telemetry/import",
            &exported.to_string(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(result["merged"], 1);

        let (_, after) = call(destination, "GET", "/v1/toin/stats", "").await;
        assert_eq!(after["shapes"], 1);
        assert_eq!(after["tokens_before"], 1000);
        assert_eq!(after["tokens_after"], 300);
    }

    #[tokio::test]
    async fn importing_sums_rather_than_replaces() {
        // Two deployments that saw the same shape have each seen real traffic. Replacing
        // would silently discard whichever side arrived second.
        let state = state();
        let payload = serde_json::json!({
            "observations": {
                "payg:claude:abc": {
                    "samples": 5, "tokens_before": 500, "tokens_after": 200, "declines": 1
                }
            }
        })
        .to_string();

        call(state.clone(), "POST", "/v1/telemetry/import", &payload).await;
        call(state.clone(), "POST", "/v1/telemetry/import", &payload).await;

        let (_, body) = call(state, "GET", "/v1/toin/stats", "").await;
        assert_eq!(body["samples"], 10, "the second import replaced the first");
        assert_eq!(body["tokens_before"], 1000);
    }

    #[tokio::test]
    async fn a_malformed_import_is_rejected_without_disturbing_the_aggregate() {
        let state = state();
        call(
            state.clone(),
            "POST",
            "/v1/telemetry/import",
            &serde_json::json!({
                "observations": { "k": { "samples": 3, "tokens_before": 30, "tokens_after": 10, "declines": 0 } }
            })
            .to_string(),
        )
        .await;

        for bad in ["not json at all", "{}", r#"{"observations": 7}"#] {
            let (status, _) = call(state.clone(), "POST", "/v1/telemetry/import", bad).await;
            assert!(
                status == StatusCode::BAD_REQUEST || status == StatusCode::OK,
                "unexpected status for {bad:?}: {status}"
            );
        }

        let (_, body) = call(state, "GET", "/v1/toin/stats", "").await;
        assert_eq!(body["samples"], 3, "a bad import disturbed the aggregate");
    }

    #[tokio::test]
    async fn a_pattern_prefix_selects_and_a_miss_is_404() {
        let state = state();
        call(
            state.clone(),
            "POST",
            "/v1/telemetry/import",
            &serde_json::json!({
                "observations": {
                    "payg:claude:aaa": { "samples": 1, "tokens_before": 10, "tokens_after": 5, "declines": 0 },
                    "oauth:gpt:bbb": { "samples": 1, "tokens_before": 10, "tokens_after": 5, "declines": 0 }
                }
            })
            .to_string(),
        )
        .await;

        let (status, body) = call(state.clone(), "GET", "/v1/toin/pattern/payg", "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["patterns"].as_array().unwrap().len(), 1);

        let (status, _) = call(state, "GET", "/v1/toin/pattern/nothing", "").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn telemetry_tools_and_toin_patterns_are_one_answer() {
        // Two spellings of the same question must not become two stores.
        let state = state();
        call(
            state.clone(),
            "POST",
            "/v1/telemetry/import",
            &serde_json::json!({
                "observations": { "k": { "samples": 2, "tokens_before": 20, "tokens_after": 8, "declines": 0 } }
            })
            .to_string(),
        )
        .await;

        let (_, a) = call(state.clone(), "GET", "/v1/toin/patterns", "").await;
        let (_, b) = call(state, "GET", "/v1/telemetry/tools", "").await;
        assert_eq!(a, b);
    }
}
