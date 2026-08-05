//! CCR over HTTP — resolving a marker without speaking MCP.
//!
//! Compression replaces content with a `<<ccr:HASH>>` marker. Until now the only way to
//! redeem one was the `headroom_retrieve` MCP tool, which means the proxy would hand a
//! marker to a client that had no way to read it: a plain-HTTP caller, any SDK user, any
//! agent that does not speak MCP. R5 keeps `ccr_retrieve` permanently registered so the
//! tools array never busts, but that only helps clients that see the tools array at all.
//!
//! # Everything routes through the same decision
//!
//! `POST /v1/compress` goes through the same [`Orchestrator`] the proxy and the MCP
//! server use, and retrieval through the same [`handle_retrieve`]. Check 6 of the
//! reachability audit already fails the build on a second copy of the routing table —
//! eight were found once — and an HTTP surface that made its own decisions would be the
//! ninth.
//!
//! [`Orchestrator`]: headroom_core::pipeline::Orchestrator

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use headroom_core::auth_mode::CompressionPolicy;
use headroom_core::block::{Block, BlockKind};
use headroom_core::ccr::{handle_retrieve, Retrieval};
use headroom_core::tokenizer::{HeuristicEstimator, Tokenizer};
use headroom_core::validate::validated_apply;
use headroom_core::AuthMode;
use serde::Deserialize;
use serde_json::json;

use crate::server::AppState;

/// Largest body these endpoints will read.
///
/// Generous for a compression request and still bounded. An unbounded read on an
/// endpoint anyone can reach is a way to exhaust memory without sending anything valid —
/// the same reasoning as `admin::MAX_BODY_BYTES`, with a larger number because content
/// is the point here.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// What `POST /v1/compress` accepts.
#[derive(Debug, Deserialize)]
pub struct CompressRequest {
    /// The content to compress.
    content: String,
    /// Where the content came from. Defaults to tool output.
    ///
    /// The prose summarizer runs only on tool output, because `text` is what somebody
    /// typed and summarizing a person's own words is a different act (D24). Every
    /// non-proxy surface takes this from its caller and defaults the same way — the CLI's
    /// `--kind`, the MCP tool's `kind` property, and `headroom.compress(kind=...)`.
    #[serde(default)]
    kind: Option<String>,
}

/// What `POST /v1/retrieve` accepts.
#[derive(Debug, Deserialize)]
pub struct RetrieveRequest {
    /// Markers or bare hashes to resolve.
    hashes: Vec<String>,
}

/// `POST /v1/compress` — compress content directly.
///
/// The HTTP twin of the `headroom_compress` MCP tool, and deliberately identical in
/// behavior: same orchestrator, same policy, same token validation, so a caller cannot
/// get one answer here and another there for the same bytes.
pub async fn compress(
    State(state): State<AppState>,
    Json(request): Json<CompressRequest>,
) -> Response {
    if request.content.len() > MAX_BODY_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": format!("content exceeds {MAX_BODY_BYTES} bytes") })),
        )
            .into_response();
    }

    let kind = match request.kind.as_deref() {
        None | Some("tool_output") => BlockKind::ToolResult,
        Some("text") => BlockKind::Text,
        Some(other) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("unknown kind {other:?}; expected \"tool_output\" or \"text\""),
                })),
            )
                .into_response();
        }
    };

    // Pay-as-you-go, matching the MCP tool: a direct call is the caller compressing their
    // own content deliberately, not relayed traffic whose credential decides what is
    // permitted. The proxy applies the real policy to real requests.
    let policy = CompressionPolicy::for_mode(AuthMode::PayAsYouGo);
    let estimator = HeuristicEstimator::new();
    let before = estimator.count(&request.content);

    let mut block = Block::new(kind, request.content.clone());
    let compressed = match state
        .compressors()
        .routed_transform(&block, policy)
        .map(|transform| validated_apply(transform, &mut block, &estimator))
    {
        Some(Ok(outcome)) if outcome.is_compressed() => true,
        // Declining is an ordinary outcome, not a failure: the caller asked for something
        // smaller and gets something correct. An error here would make the endpoint look
        // broken for the common case of unremarkable input.
        _ => false,
    };

    let content = if compressed {
        block.content()
    } else {
        request.content.as_str()
    };

    (
        StatusCode::OK,
        Json(json!({
            "content": content,
            "compressed": compressed,
            "tokens_before": before,
            "tokens_after": estimator.count(content),
        })),
    )
        .into_response()
}

/// `GET /v1/retrieve/{hash}` — resolve one marker.
///
/// # The path takes the bare hash, not the marker
///
/// A marker is `<<ccr:HASH>>`, and `<`/`>` are not legal in a URI path — a client pasting
/// one in whole gets a malformed request from its own HTTP library before anything here
/// runs. So the path segment is the hex hash: strip `<<ccr:` and `>>`, or use
/// `POST /v1/retrieve`, whose JSON body accepts either form.
///
/// [`handle_retrieve`] tolerates both regardless, so a percent-encoded marker also works.
/// That is a courtesy, not the documented shape.
///
/// Returns the original bytes as `text/plain`, because that is what they are: the caller
/// asked for content, not for a description of it. Wrapping them in JSON would make every
/// client unwrap a string that is frequently not JSON to begin with.
pub async fn retrieve_one(State(state): State<AppState>, Path(hash): Path<String>) -> Response {
    match handle_retrieve(state.ccr_store().as_ref(), &hash) {
        Retrieval::Found(bytes) => (
            StatusCode::OK,
            [("content-type", "text/plain; charset=utf-8")],
            bytes,
        )
            .into_response(),
        // Expired and malformed are both 404, but the body says which. A model or an
        // operator needs to tell "this existed and is gone" from "check what you sent" —
        // the distinction `Retrieval` was built to preserve.
        other => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": other.message() })),
        )
            .into_response(),
    }
}

/// `POST /v1/retrieve` — resolve several markers in one call.
///
/// A per-hash result rather than an all-or-nothing answer: one expired entry among twenty
/// should not cost the caller the other nineteen, and re-requesting them individually to
/// find out which failed is worse than telling them.
pub async fn retrieve_batch(
    State(state): State<AppState>,
    Json(request): Json<RetrieveRequest>,
) -> Response {
    let store = state.ccr_store();

    let results: Vec<serde_json::Value> = request
        .hashes
        .iter()
        .map(|hash| match handle_retrieve(store.as_ref(), hash) {
            Retrieval::Found(bytes) => json!({
                "hash": hash,
                "found": true,
                "content": String::from_utf8_lossy(&bytes),
            }),
            other => json!({
                "hash": hash,
                "found": false,
                "error": other.message(),
            }),
        })
        .collect();

    (StatusCode::OK, Json(json!({ "results": results }))).into_response()
}

/// `GET /v1/retrieve/stats` — what the store currently holds.
///
/// Reports only what the store can actually answer. `CcrStore` exposes a live entry count
/// and nothing else, so a hit rate here would be invented — and `/metrics` already
/// carries the counters that are genuinely measured.
pub async fn retrieve_stats(State(state): State<AppState>) -> Response {
    (
        StatusCode::OK,
        Json(json!({
            "entries": state.ccr_store().len(),
            "store": state.ccr_store_kind().as_str(),
            "survives_restart": state.ccr_store_kind().survives_restart(),
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::router_with;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn state() -> AppState {
        AppState::new("http://unused.example")
    }

    async fn call(state: AppState, method: &str, uri: &str, body: &str) -> (StatusCode, Vec<u8>) {
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
        let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
            .await
            .unwrap();
        (status, bytes.to_vec())
    }

    /// A bulky JSON tool result, the shape compression exists for.
    fn bulky() -> String {
        let records: Vec<String> = (0..150)
            .map(|i| format!(r#"{{"path":"src/module_{i}.rs","kind":"file","status":"ok"}}"#))
            .collect();
        format!("[{}]", records.join(","))
    }

    #[tokio::test]
    async fn a_marker_produced_by_compress_resolves_over_http() {
        // The end-to-end claim: a client that never speaks MCP can compress content and
        // redeem the marker it gets back. Before this, the proxy would hand out a marker
        // such a client had no way to read.
        let state = state();
        let source = bulky();

        let (status, body) = call(
            state.clone(),
            "POST",
            "/v1/compress",
            &serde_json::json!({ "content": source }).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let compressed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(compressed["compressed"], true, "nothing was compressed");

        let text = compressed["content"].as_str().unwrap();
        // The bare hash, not the marker: `<` and `>` are not legal in a URI path, so a
        // client pasting `<<ccr:...>>` whole is rejected by its own HTTP library before
        // reaching this proxy. The batch endpoint takes either form.
        let start = text.find("<<ccr:").expect("a marker was returned") + "<<ccr:".len();
        let end = text[start..].find(">>").unwrap() + start;
        let hash = &text[start..end];

        let (status, bytes) = call(state, "GET", &format!("/v1/retrieve/{hash}"), "").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            bytes,
            source.as_bytes(),
            "retrieval returned something other than the exact original"
        );
    }

    #[tokio::test]
    async fn http_compress_and_the_mcp_tool_agree() {
        // Both route through the same `Orchestrator`. If they could disagree, a caller
        // would get one answer over HTTP and another over MCP for identical bytes — the
        // drift check 6 of the reachability audit exists to prevent.
        let source = bulky();

        let (_, body) = call(
            state(),
            "POST",
            "/v1/compress",
            &serde_json::json!({ "content": source }).to_string(),
        )
        .await;
        let over_http: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // The same decision, taken directly through the shared orchestrator.
        let policy = headroom_core::auth_mode::CompressionPolicy::for_mode(AuthMode::PayAsYouGo);
        let compressors = crate::compression::Compressors::new(std::sync::Arc::new(
            headroom_core::ccr::InMemoryCcrStore::new(),
        ));
        let block = Block::new(BlockKind::ToolResult, source.clone());
        let routed = compressors.routed_transform(&block, policy).is_some();

        assert_eq!(
            over_http["compressed"].as_bool().unwrap(),
            routed,
            "the HTTP endpoint and the shared router disagreed about the same bytes"
        );
    }

    #[tokio::test]
    async fn an_unknown_hash_is_404_with_a_reason() {
        // Expired and malformed are both 404, but the body has to say which: a caller
        // needs to tell "this existed and is gone" from "check what you sent".
        let (status, body) = call(state(), "GET", "/v1/retrieve/deadbeef", "").await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["error"].as_str().is_some_and(|m| !m.is_empty()));
    }

    #[tokio::test]
    async fn a_batch_reports_per_hash_rather_than_failing_whole() {
        // One expired entry among twenty must not cost the caller the other nineteen.
        let state = state();
        let source = bulky();

        let (_, body) = call(
            state.clone(),
            "POST",
            "/v1/compress",
            &serde_json::json!({ "content": source }).to_string(),
        )
        .await;
        let compressed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let text = compressed["content"].as_str().unwrap();
        let start = text.find("<<ccr:").unwrap();
        let end = text[start..].find(">>").unwrap() + start + 2;
        let marker = text[start..end].to_owned();

        let (status, body) = call(
            state,
            "POST",
            "/v1/retrieve",
            &serde_json::json!({ "hashes": [marker, "not-a-hash"] }).to_string(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let results = json["results"].as_array().unwrap();

        assert_eq!(results[0]["found"], true, "the good hash did not resolve");
        assert_eq!(results[1]["found"], false);
        assert!(results[1]["error"].as_str().is_some());
    }

    #[tokio::test]
    async fn compress_honors_the_content_kind() {
        // D24 over HTTP: `text` is what somebody typed, and the prose summarizer must
        // not run on it. Every other surface takes this from its caller the same way.
        let prose = (0..200)
            .map(|i| format!("The deployment step number {i} completed as expected."))
            .collect::<Vec<_>>()
            .join("\n");

        let (_, as_tool) = call(
            state(),
            "POST",
            "/v1/compress",
            &serde_json::json!({ "content": prose, "kind": "tool_output" }).to_string(),
        )
        .await;
        let (_, as_text) = call(
            state(),
            "POST",
            "/v1/compress",
            &serde_json::json!({ "content": prose, "kind": "text" }).to_string(),
        )
        .await;

        let tool: serde_json::Value = serde_json::from_slice(&as_tool).unwrap();
        let text: serde_json::Value = serde_json::from_slice(&as_text).unwrap();

        assert_eq!(tool["compressed"], true, "tool output was not summarized");
        assert_eq!(
            text["compressed"], false,
            "a person's own words were summarized"
        );
    }

    #[tokio::test]
    async fn an_unknown_kind_is_rejected_rather_than_guessed() {
        let (status, _) = call(
            state(),
            "POST",
            "/v1/compress",
            &serde_json::json!({ "content": "x", "kind": "sideways" }).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn stats_report_only_what_the_store_can_answer() {
        let state = state();
        let (status, body) = call(state.clone(), "GET", "/v1/retrieve/stats", "").await;

        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["entries"], 0);

        call(
            state.clone(),
            "POST",
            "/v1/compress",
            &serde_json::json!({ "content": bulky() }).to_string(),
        )
        .await;

        let (_, body) = call(state, "GET", "/v1/retrieve/stats", "").await;
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["entries"], 1, "a stored original was not counted");
    }

    #[tokio::test]
    async fn stats_is_not_shadowed_by_the_hash_route() {
        // `/v1/retrieve/stats` and `/v1/retrieve/{hash}` overlap. If the parameter route
        // won, `stats` would be read as a hash and answer 404 forever.
        let (status, _) = call(state(), "GET", "/v1/retrieve/stats", "").await;
        assert_eq!(status, StatusCode::OK);
    }
}
