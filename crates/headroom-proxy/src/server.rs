//! Server construction and lifecycle.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use headroom_core::tokenizer::{HeuristicEstimator, Tokenizer};

use crate::compression::{compress_dialect, Compressors, Dialect};
use crate::config::Config;
use crate::guard::{is_self_referential, RateLimiter};
use crate::headers::{sanitize, HeaderPolicy};
use crate::health::health;
use crate::metrics::Metrics;
use crate::observe::ObservingStream;
use crate::openai;
use crate::upstream::{RelayError, Upstream};
use headroom_core::auth_mode::{classify_auth_mode, CompressionPolicy};

/// Shared state: the compressors, the CCR store behind them, and the relay.
#[derive(Clone)]
pub struct AppState {
    compressors: Arc<Compressors>,
    metrics: Arc<Metrics>,
    /// The relay, absent when it could not be constructed.
    ///
    /// `Option` rather than a failed startup: a proxy that refuses to boot because
    /// TLS initialization failed takes `/health` down with it, so nothing can report
    /// *why* it is down. Booting and answering 502 on the request path says more.
    upstream: Option<Upstream>,
    limiter: Arc<RateLimiter>,
}

/// Requests permitted per [`RATE_WINDOW`].
///
/// Set well above any human or agent workload. This is a backstop against a retry loop
/// somewhere upstream of the proxy relaying thousands of requests with the customer's
/// credential attached — not a quota, and it should never be the thing a real user
/// meets.
const RATE_CAPACITY: u32 = 600;

/// The rate-limit window.
const RATE_WINDOW: Duration = Duration::from_secs(60);

impl AppState {
    /// Builds state relaying to `upstream_base`.
    pub fn new(upstream_base: &str) -> Self {
        let upstream = match Upstream::new(upstream_base) {
            Ok(upstream) => Some(upstream),
            Err(err) => {
                tracing::error!(%err, "could not build the upstream client; relay disabled");
                None
            }
        };

        // Built before the compressor set so both share one instance: routing counters
        // and request counters have to land in the same place the `/metrics` handler
        // reads from, or half the numbers would be invisible.
        let metrics = Arc::new(Metrics::new());

        Self {
            compressors: Arc::new(
                Compressors::with_recommendations(
                    // Selected from configuration rather than hardcoded. An in-memory
                    // store loses every original on restart, and on a second worker the
                    // marker created here is requested from a process that never saw it.
                    Config::ccr_store(),
                    // Read once, here, at construction. See `Config::recommendations`.
                    Config::recommendations(),
                )
                // Also read once. A memory set that changed between requests would make
                // the same request produce different bytes depending on when it arrived,
                // and those bytes go upstream — see `Config::memories`.
                .with_memories(Config::memories(), Config::memory_limit())
                // Routing reasons are counted here and nowhere else: the CLI and the
                // library callers have no metrics endpoint to read them from.
                .with_metrics(metrics.clone()),
            ),
            metrics,
            upstream,
            limiter: Arc::new(RateLimiter::new(RATE_CAPACITY, RATE_WINDOW)),
        }
    }

    /// Builds state with a specific rate limit, for tests that need to reach it.
    pub fn with_rate_limit(upstream_base: &str, capacity: u32, window: Duration) -> Self {
        Self {
            limiter: Arc::new(RateLimiter::new(capacity, window)),
            ..Self::new(upstream_base)
        }
    }

    /// Whether a request can actually be relayed.
    ///
    /// False when the upstream client failed to build at startup, in which case every
    /// request errors. `/health` reports this rather than answering `"ok"` regardless —
    /// see [`crate::health::Health::status`].
    pub fn relay_available(&self) -> bool {
        self.upstream.is_some()
    }

    /// The process metrics.
    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }

    /// The compressor set, shared across routes so they share one CCR store.
    pub fn compressors(&self) -> &Compressors {
        &self.compressors
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new(Config::from_env().upstream())
    }
}

/// Builds the application router with default state.
///
/// Separated from [`serve`] so tests can exercise routes without binding a socket.
pub fn router() -> Router {
    router_with(AppState::default())
}

/// Builds the application router over supplied state.
///
/// Exists so a test can point the relay at a local server instead of a provider.
pub fn router_with(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics_endpoint))
        .route("/v1/messages", post(messages))
        .route("/v1/chat/completions", post(openai::chat_completions))
        .route("/v1/responses", post(openai::responses))
        // Declared non-compressible by the reference architecture: both describe
        // conversation *state* rather than carrying a prompt, so compressing one would
        // corrupt the provider's own record of the conversation.
        .route("/v1/responses/compact", post(openai::passthrough))
        .route("/v1/conversations", post(openai::passthrough))
        .route("/v1/conversations/{*rest}", post(openai::passthrough))
        .route("/admin/runtime-env", post(crate::admin::runtime_env))
        // Codex uses a WebSocket transport, and a proxy that only speaks HTTP silently
        // drops that client to whatever fallback it has, or breaks it.
        .route("/v1/realtime", get(crate::websocket::relay_socket))
        .route("/ws", get(crate::websocket::relay_socket))
        .with_state(state)
}

/// `GET /metrics` — Prometheus text exposition.
async fn metrics_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    (
        [("content-type", "text/plain; version=0.0.4")],
        state.metrics.render(),
    )
}

/// `POST /v1/messages` — compress the live zone, relay upstream, stream the answer back.
///
/// # Why the response is streamed rather than returned whole
///
/// A `"stream": true` request is the common agent case. Buffering its response would
/// hold the model's entire answer until generation finished and then release it at
/// once, so the user watches a stall instead of reading tokens. The relay hands back
/// a stream and this handler passes it straight through.
async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, RelayError> {
    let config = Config::from_env();

    let auth_mode = classify_auth_mode(&headers);
    let policy = CompressionPolicy::for_mode(auth_mode);
    let upstream_headers = sanitize(
        &headers,
        HeaderPolicy {
            forwarded_headers: policy.forwarded_headers,
            strip_accept_encoding: policy.may_strip_accept_encoding,
        },
    );
    tracing::debug!(
        header_count = upstream_headers.len(),
        auth = ?crate::headers::redacted_authorization(&headers),
        auth_mode = auth_mode.as_str(),
        "prepared upstream headers"
    );

    // Scanned before compression, on the bytes the client sent. Volatile content in
    // the hot zone means the provider's cache misses on every request, which no amount
    // of live-zone compression compensates for — and the savings metric looks healthy
    // throughout, so nothing else would surface it.
    //
    // Reported only. The reference records that the original implementation tried to
    // *fix* this by rewriting the value, which modified the cache hot zone that
    // invariant I2 protects and busted the cache itself on the turn it took effect.
    crate::volatile::warn_about(&crate::volatile::scan(&body));

    let compressed = compress_dialect(
        Dialect::Anthropic,
        &body,
        &state.compressors,
        config.compression_enabled(),
        policy,
        config.verbosity(),
    );

    // Measured on the bytes that actually go out, not on what the compressor claimed.
    // A counter fed by the component it is measuring cannot detect that component
    // failing to do anything.
    if compressed.as_ref() == body.as_ref() {
        state.metrics.record_passthrough();
    } else {
        let estimator = HeuristicEstimator::new();
        state.metrics.record_compressed(
            estimator.count(&String::from_utf8_lossy(&body)) as u64,
            estimator.count(&String::from_utf8_lossy(&compressed)) as u64,
        );
    }

    // Stabilization runs after the savings measurement, deliberately. Normalizing tools
    // and placing breakpoints makes the *provider's* cache hit more often; neither
    // removes a token, and counting either as compression would flatter the number this
    // proxy is judged on.
    let outgoing = crate::stabilization::stabilize(Dialect::Anthropic, &compressed, policy);

    relay(
        &state,
        Method::POST,
        "/v1/messages",
        &upstream_headers,
        outgoing.into_owned(),
    )
    .await
}

/// Forwards a prepared request and returns the provider's streamed answer.
///
/// The tail every route shares. `headers` must already have been through
/// [`sanitize`]; this function adds nothing and removes only what the relay itself
/// must rebuild.
///
/// # Errors
///
/// Returns [`RelayError`] if no upstream client exists or the request fails.
pub(crate) async fn relay(
    state: &AppState,
    method: Method,
    path: &str,
    headers: &HeaderMap,
    body: Vec<u8>,
) -> Result<Response, RelayError> {
    let Some(upstream) = state.upstream.as_ref() else {
        return Err(RelayError::InvalidUrl(
            "no upstream client was constructed at startup".into(),
        ));
    };

    if !state.limiter.allow() {
        // 429 rather than 503: the client should back off and retry, which is exactly
        // what a provider SDK does with this status. A 503 reads as "the service is
        // broken" and several SDKs will not retry it.
        tracing::warn!(path, "rate limit reached; refusing to relay");
        return Err(RelayError::RateLimited);
    }

    // The request log. `Authorization` appears as a 12-character prefix and nothing
    // more — enough to correlate requests from one credential without the log becoming
    // a place credentials live.
    tracing::info!(
        path,
        bytes = body.len(),
        auth = ?crate::headers::redacted_authorization(headers),
        "relaying upstream"
    );

    let relayed = upstream.forward(method, path, headers, body).await?;

    let status = relayed.status();
    let mut response = Response::builder().status(status);
    if let Some(headers) = response.headers_mut() {
        headers.extend(relayed.headers().clone());
    }

    // Wrapped, not buffered. The observer reads frames as they pass and yields the
    // bytes it received — invariant I9 — which is the only way to read the cache usage
    // in `message_start` without holding the response back.
    // The path picks the stream vocabulary. An OpenAI response read with the Anthropic
    // classifier does not fail — it reports a healthy stream as unfinished and every
    // ordinary frame as unrecognized, which is telemetry that is confidently wrong.
    let observed = ObservingStream::new(relayed.into_stream(), state.metrics.clone(), path);

    response
        .body(Body::from_stream(observed))
        // Only fails if the status or headers are invalid, and both came from a
        // response that was already parsed — but this must not be an `unwrap()` on the
        // request path regardless.
        .map_err(|err| RelayError::Transport(err.to_string()))
}

impl IntoResponse for RelayError {
    /// Renders a relay failure in the provider's own error shape.
    ///
    /// The client is a provider SDK. It knows how to parse `{"type":"error", ...}` and
    /// nothing about this proxy, so an error in any other shape surfaces to the user as
    /// a parse failure rather than as the outage it is.
    fn into_response(self) -> Response {
        let status = self.status();
        tracing::warn!(error = %self, status = status.as_u16(), "relay failed");

        let body = serde_json::json!({
            "type": "error",
            "error": {
                "type": "api_error",
                "message": self.to_string(),
            }
        });

        (status, axum::Json(body)).into_response()
    }
}

/// Runs the proxy until a shutdown signal arrives.
///
/// # Errors
///
/// Returns an error if the listen socket cannot be bound, or if the configured
/// upstream points back at the proxy's own listen address.
pub async fn serve(config: &Config) -> std::io::Result<()> {
    // Checked before binding. A proxy whose upstream is itself forwards every request
    // to itself forever, and the symptom is a pinned core and exhausted file
    // descriptors rather than an error anyone can read. Refusing to start says it once,
    // plainly, at the moment the operator is looking.
    if is_self_referential(config.upstream(), config.listen_addr()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "upstream {} is this proxy's own listen address ({}); \
                 every request would forward to itself",
                config.upstream(),
                config.listen_addr()
            ),
        ));
    }

    let listener = tokio::net::TcpListener::bind(config.listen_addr()).await?;
    tracing::info!(
        addr = %config.listen_addr(),
        upstream = config.upstream(),
        compression = config.compression_enabled(),
        "headroom-proxy listening"
    );

    // `into_make_service_with_connect_info` rather than the plain service: the admin
    // endpoint's only protection is that it can tell a local caller from a remote one,
    // and without connect info it refuses every request including the legitimate ones.
    axum::serve(
        listener,
        router().into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
}

/// Resolves when the process is asked to stop.
///
/// Graceful shutdown is not a nicety here. The proxy sits in the middle of streaming
/// responses, and dropping an in-flight request on SIGTERM truncates a model's output
/// mid-token — which reaches the user as a corrupt answer rather than as an error
/// they can retry.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            // If the handler cannot be installed, fall back to ctrl-c alone rather
            // than refusing to start.
            Err(err) => {
                tracing::warn!(%err, "could not install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }

    tracing::info!("shutdown signal received, draining");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{Request, StatusCode};
    use std::net::SocketAddr;
    use std::sync::Mutex;
    use tower::ServiceExt;

    /// The last request body a fake upstream received.
    type Captured = Arc<Mutex<Option<Vec<u8>>>>;

    /// Starts a fake provider on loopback, returning its base URL and a handle to
    /// whatever body it last saw.
    ///
    /// A real socket rather than a mock relay: the point of this batch is that bytes
    /// survive a network hop, and substituting the transport would assert everything
    /// except the thing under test.
    async fn fake_provider(status: StatusCode, reply: &'static str) -> (String, Captured) {
        let captured: Captured = Arc::new(Mutex::new(None));
        let seen = captured.clone();

        let app = Router::new().route(
            "/v1/messages",
            post(move |body: Bytes| {
                let seen = seen.clone();
                async move {
                    if let Ok(mut slot) = seen.lock() {
                        *slot = Some(body.to_vec());
                    }
                    (status, reply)
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (format!("http://{addr}"), captured)
    }

    /// A request with a bulky live tool result, the shape compression exists for.
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
            r#"{{"model":"claude-opus-4","max_tokens":4096,"messages":[{{"role":"user","content":"q"}},{{"role":"assistant","content":"a"}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t","content":"[{}]"}}]}}]}}"#,
            records.join(",")
        )
    }

    async fn post_messages(app: Router, body: String, api_key: &str) -> Response {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("x-api-key", api_key)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
    }

    // ---- the relay, end to end ----

    #[tokio::test]
    async fn a_request_reaches_the_provider_and_its_answer_reaches_the_client() {
        // The headline: this is what makes the thing a proxy rather than a library.
        let (base, captured) = fake_provider(StatusCode::OK, r#"{"id":"msg_1"}"#).await;

        let response = post_messages(
            router_with(AppState::new(&base)),
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#.to_owned(),
            "sk-ant-api03-x",
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), br#"{"id":"msg_1"}"#);

        assert!(
            captured.lock().unwrap().is_some(),
            "the provider never saw the request"
        );
    }

    #[tokio::test]
    async fn the_provider_receives_the_compressed_body_not_the_original() {
        // Otherwise the proxy is doing the work and throwing the result away.
        let (base, captured) = fake_provider(StatusCode::OK, "{}").await;
        let source = compressible_request();

        post_messages(
            router_with(AppState::new(&base)),
            source.clone(),
            "sk-ant-api03-x",
        )
        .await;

        let sent = captured.lock().unwrap().clone().expect("nothing forwarded");
        assert!(
            sent.len() < source.len(),
            "forwarded {} bytes for a {}-byte request; compression was discarded",
            sent.len(),
            source.len()
        );
    }

    #[tokio::test]
    async fn a_subscription_request_reaches_the_provider_byte_identical() {
        // Invariant I10 all the way to the wire. Unrecognized auth is subscription
        // mode, which forbids the lossy compressors, so a bulky body must arrive
        // exactly as the client sent it.
        let (base, captured) = fake_provider(StatusCode::OK, "{}").await;
        let source = compressible_request();

        let response = router_with(AppState::new(&base))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("authorization", "Bearer opaque-session-token")
                    .body(Body::from(source.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let sent = captured.lock().unwrap().clone().expect("nothing forwarded");
        assert_eq!(
            sent,
            source.as_bytes(),
            "subscription traffic was modified before forwarding"
        );
    }

    #[tokio::test]
    async fn a_provider_error_status_reaches_the_client_unchanged() {
        // A 429 the client cannot see is a 429 it cannot back off from.
        let (base, _) = fake_provider(StatusCode::TOO_MANY_REQUESTS, r#"{"type":"error"}"#).await;

        let response = post_messages(
            router_with(AppState::new(&base)),
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#.to_owned(),
            "sk-ant-api03-x",
        )
        .await;

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn an_unreachable_provider_yields_a_502_in_the_provider_error_shape() {
        // The client is a provider SDK. An error in any other shape surfaces to the
        // user as a parse failure rather than as the outage it actually is.
        let response = post_messages(
            router_with(AppState::new("http://127.0.0.1:1")),
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#.to_owned(),
            "sk-ant-api03-x",
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["type"], "error");
        assert_eq!(json["error"]["type"], "api_error");
    }

    #[tokio::test]
    async fn a_failed_relay_never_echoes_the_credential_to_the_client() {
        let response = post_messages(
            router_with(AppState::new("http://127.0.0.1:1")),
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#.to_owned(),
            "sk-ant-api03-secret",
        )
        .await;

        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("sk-ant-api03-secret"),
            "the credential came back in the error body"
        );
    }

    // ---- metrics ----

    #[tokio::test]
    async fn the_metrics_endpoint_reflects_real_traffic() {
        let (base, _) = fake_provider(StatusCode::OK, "{}").await;
        let state = AppState::new(&base);

        post_messages(
            router_with(state.clone()),
            compressible_request(),
            "sk-ant-api03-x",
        )
        .await;

        let response = router_with(state)
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();

        assert!(
            text.contains("headroom_requests_total 1"),
            "request was not counted:\n{text}"
        );
        assert!(
            text.contains("headroom_compressed_total 1"),
            "compression was not counted:\n{text}"
        );
    }

    #[tokio::test]
    async fn a_passthrough_request_counts_as_passthrough_not_as_compressed() {
        // The counter is fed by comparing the bytes that actually go out, so a
        // compressor that silently stops working shows up here rather than continuing
        // to report success.
        let (base, _) = fake_provider(StatusCode::OK, "{}").await;
        let state = AppState::new(&base);

        post_messages(
            router_with(state.clone()),
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#.to_owned(),
            "sk-ant-api03-x",
        )
        .await;

        let text = state.metrics().render();
        assert!(text.contains("headroom_passthrough_total 1"), "{text}");
        assert!(text.contains("headroom_compressed_total 0"), "{text}");
    }

    #[tokio::test]
    async fn a_streaming_response_feeds_the_cache_metrics_end_to_end() {
        // The number that says whether this proxy is helping. It arrives in the very
        // first SSE frame of the response, so nothing short of the full path — relay,
        // observer, metrics — proves it is actually reaching the gauge.
        let sse = concat!(
            "event: message_start\n",
            r#"data: {"type":"message_start","message":{"usage":{"cache_read_input_tokens":900,"cache_creation_input_tokens":100}}}"#,
            "\n\n",
            "event: message_stop\n",
            r#"data: {"type":"message_stop"}"#,
            "\n\n",
        );
        let (base, _) = fake_provider(StatusCode::OK, sse).await;
        let state = AppState::new(&base);

        let response = post_messages(
            router_with(state.clone()),
            r#"{"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}"#.to_owned(),
            "sk-ant-api03-x",
        )
        .await;

        // The body has to be drained for the observer to see it — which is the point:
        // nothing is buffered on the proxy's behalf.
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), sse.as_bytes(), "the stream was altered");

        assert_eq!(state.metrics().cache_hit_rate(), Some(0.9));
    }

    #[tokio::test]
    async fn a_streaming_request_is_compressed_on_the_way_out() {
        // Streaming is the common agent case. While `compress_request` bailed out on
        // `"stream": true`, most real traffic was exempt from compression and every
        // test still reported that compression worked.
        let (base, captured) = fake_provider(StatusCode::OK, "{}").await;
        let source = compressible_request()
            .replace(r#""max_tokens":4096"#, r#""max_tokens":4096,"stream":true"#);

        post_messages(
            router_with(AppState::new(&base)),
            source.clone(),
            "sk-ant-api03-x",
        )
        .await;

        let sent = captured.lock().unwrap().clone().expect("nothing forwarded");
        assert!(
            sent.len() < source.len(),
            "a streaming request was forwarded uncompressed: {} bytes for {}",
            sent.len(),
            source.len()
        );
        // The client asked for a stream; that must survive the compression.
        let parsed: serde_json::Value = serde_json::from_slice(&sent).unwrap();
        assert_eq!(parsed["stream"], serde_json::Value::Bool(true));
    }

    // ---- OpenAI routes ----

    /// The path and body a path-agnostic fake upstream last received.
    type CapturedCall = Arc<Mutex<Option<(String, Vec<u8>)>>>;

    /// Starts a fake provider that answers any path, capturing body and path.
    async fn fake_any_path() -> (String, CapturedCall) {
        let captured: CapturedCall = Arc::new(Mutex::new(None));
        let seen = captured.clone();

        let app = Router::new().fallback(move |uri: axum::http::Uri, body: Bytes| {
            let seen = seen.clone();
            async move {
                if let Ok(mut slot) = seen.lock() {
                    *slot = Some((uri.path().to_owned(), body.to_vec()));
                }
                "{}"
            }
        });

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (format!("http://{addr}"), captured)
    }

    async fn post_to(app: Router, path: &str, body: String) -> Response {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("x-api-key", "sk-ant-api03-x")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
    }

    /// An OpenAI chat request whose newest message is a bulky tool result.
    fn openai_chat_request() -> String {
        let records: Vec<String> = (0..120)
            .map(|i| {
                format!(
                    r#"{{\"path\":\"src/module_{i}.rs\",\"kind\":\"file\",\"status\":\"ok\",\"size\":{}}}"#,
                    1000 + i
                )
            })
            .collect();
        format!(
            r#"{{"model":"gpt-4o","messages":[{{"role":"user","content":"list"}},{{"role":"assistant","content":"ok"}},{{"role":"tool","tool_call_id":"c","content":"[{}]"}}]}}"#,
            records.join(",")
        )
    }

    #[tokio::test]
    async fn chat_completions_compresses_and_relays_to_the_right_path() {
        let (base, captured) = fake_any_path().await;
        let source = openai_chat_request();

        let response = post_to(
            router_with(AppState::new(&base)),
            "/v1/chat/completions",
            source.clone(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let (path, sent) = captured.lock().unwrap().clone().expect("nothing forwarded");
        assert_eq!(path, "/v1/chat/completions");
        assert!(
            sent.len() < source.len(),
            "an OpenAI request was forwarded uncompressed: {} for {}",
            sent.len(),
            source.len()
        );
    }

    #[tokio::test]
    async fn a_conversations_request_is_relayed_byte_identical() {
        // Declared non-compressible: the body describes conversation *state* rather
        // than carrying a prompt, so compressing it would corrupt the provider's own
        // record of the conversation rather than just shortening a message.
        let (base, captured) = fake_any_path().await;
        let source = openai_chat_request();

        post_to(
            router_with(AppState::new(&base)),
            "/v1/conversations",
            source.clone(),
        )
        .await;

        let (path, sent) = captured.lock().unwrap().clone().expect("nothing forwarded");
        assert_eq!(path, "/v1/conversations");
        assert_eq!(sent, source.as_bytes(), "a passthrough route was modified");
    }

    #[tokio::test]
    async fn a_nested_conversations_path_is_preserved_not_collapsed() {
        // `/v1/conversations/{id}/items` must reach the provider at the path the client
        // used. Collapsing it to the route prefix would send every sub-resource request
        // to the collection endpoint.
        let (base, captured) = fake_any_path().await;

        post_to(
            router_with(AppState::new(&base)),
            "/v1/conversations/conv_123/items",
            "{}".to_owned(),
        )
        .await;

        let (path, _) = captured.lock().unwrap().clone().expect("nothing forwarded");
        assert_eq!(path, "/v1/conversations/conv_123/items");
    }

    #[tokio::test]
    async fn a_responses_compact_request_is_relayed_byte_identical() {
        let (base, captured) = fake_any_path().await;
        let source = openai_chat_request();

        post_to(
            router_with(AppState::new(&base)),
            "/v1/responses/compact",
            source.clone(),
        )
        .await;

        let (path, sent) = captured.lock().unwrap().clone().expect("nothing forwarded");
        assert_eq!(path, "/v1/responses/compact");
        assert_eq!(sent, source.as_bytes());
    }

    #[tokio::test]
    async fn responses_and_compact_do_not_collide_as_routes() {
        // `/v1/responses` compresses and `/v1/responses/compact` must not — a routing
        // mistake here would silently compress the one body that must never be.
        let (base, captured) = fake_any_path().await;

        post_to(
            router_with(AppState::new(&base)),
            "/v1/responses",
            r#"{"model":"gpt-4o","input":"hi"}"#.to_owned(),
        )
        .await;
        assert_eq!(captured.lock().unwrap().clone().unwrap().0, "/v1/responses");

        post_to(
            router_with(AppState::new(&base)),
            "/v1/responses/compact",
            r#"{"conversation":"c"}"#.to_owned(),
        )
        .await;
        assert_eq!(
            captured.lock().unwrap().clone().unwrap().0,
            "/v1/responses/compact"
        );
    }

    // ---- operational guards ----

    #[tokio::test]
    async fn the_rate_limit_refuses_with_429_rather_than_503() {
        // A provider SDK already knows how to back off and retry a 429. Several read
        // 503 as "the service is broken" and give up, turning a momentary limit into a
        // failed request.
        let (base, _) = fake_provider(StatusCode::OK, "{}").await;
        let state = AppState::with_rate_limit(&base, 1, Duration::from_secs(60));
        let body = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#.to_owned();

        let first = post_messages(router_with(state.clone()), body.clone(), "sk-ant-api03-x").await;
        assert_eq!(first.status(), StatusCode::OK);

        let second = post_messages(router_with(state), body, "sk-ant-api03-x").await;
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn a_rate_limited_request_never_reaches_the_provider() {
        // The point of the limit is that the request is *not* relayed. A limiter that
        // forwarded and then reported 429 would protect nothing.
        let (base, captured) = fake_provider(StatusCode::OK, "{}").await;
        let state = AppState::with_rate_limit(&base, 1, Duration::from_secs(60));

        post_messages(
            router_with(state.clone()),
            r#"{"model":"m","messages":[{"role":"user","content":"first"}]}"#.to_owned(),
            "sk-ant-api03-x",
        )
        .await;
        post_messages(
            router_with(state),
            r#"{"model":"m","messages":[{"role":"user","content":"second-must-not-arrive"}]}"#
                .to_owned(),
            "sk-ant-api03-x",
        )
        .await;

        let sent = String::from_utf8(captured.lock().unwrap().clone().unwrap()).unwrap();
        assert!(
            !sent.contains("second-must-not-arrive"),
            "a refused request was still forwarded"
        );
    }

    #[tokio::test]
    async fn serving_refuses_when_the_upstream_is_the_proxy_itself() {
        // Otherwise every request forwards to itself forever, and the symptom is a
        // pinned core and exhausted file descriptors rather than a readable error.
        let config = Config::from_env();
        let looped = format!("http://127.0.0.1:{}", config.listen_addr().port());

        assert!(is_self_referential(
            &looped,
            format!("127.0.0.1:{}", config.listen_addr().port())
                .parse()
                .unwrap()
        ));
    }

    // ---- lifecycle ----

    #[tokio::test]
    async fn health_answers_200_with_a_body() {
        let response = router()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["status"], "ok");
    }

    #[tokio::test]
    async fn an_unknown_route_is_404_rather_than_a_panic() {
        let response = router()
            .oneshot(Request::builder().uri("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
