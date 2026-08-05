//! Server construction and lifecycle.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use headroom_core::ccr::CcrStore;
use headroom_core::tokenizer::{HeuristicEstimator, Tokenizer};

use crate::compression::{compress_dialect, Compressors, Dialect};
use crate::config::Config;
use crate::guard::{is_self_referential, RateLimiter};
use crate::headers::{apply_forwarded, sanitize, HeaderPolicy};
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
    /// The last upstream reachability probe and when it was taken.
    ///
    /// See [`AppState::upstream_reachable`].
    upstream_health: Arc<std::sync::Mutex<Option<(std::time::Instant, bool)>>>,
    /// The store that was built, not the one that was configured — see
    /// [`Config::ccr_store_with_kind`].
    ccr_store_kind: crate::config::CcrStoreKind,
    /// The same store handed to [`Compressors`], kept here too so [`serve`] can spawn
    /// the periodic purge task against it — see [`spawn_ccr_purge_task`].
    ccr_store: Arc<dyn CcrStore>,
}

/// Requests permitted per [`RATE_WINDOW`].
///
/// Set well above any human or agent workload. This is a backstop against a retry loop
/// somewhere upstream of the proxy relaying thousands of requests with the customer's
/// credential attached — not a quota, and it should never be the thing a real user
/// meets.
/// How long an upstream reachability answer is reused before probing again.
///
/// See [`AppState::upstream_reachable`].
const UPSTREAM_PROBE_CACHE: Duration = Duration::from_secs(5);

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

        // Destructured here so the kind reaching `/health` is the one this store was
        // built as. Re-reading the configuration downstream would report the store the
        // operator asked for, which is precisely the case worth telling them about.
        let (ccr_store, ccr_store_kind) = Config::ccr_store_with_kind();
        // Cloned before the move into `Compressors::with_recommendations` below, so
        // `serve` has a handle to spawn the periodic purge task against — see
        // `spawn_ccr_purge_task`.
        let ccr_store_for_purge = Arc::clone(&ccr_store);
        if !ccr_store_kind.survives_restart() && Config::persistent_store_requested() {
            tracing::warn!(
                "a persistent CCR store was configured and could not be used; markers \
                 handed out from now on will not be redeemable after a restart"
            );
        }

        Self {
            compressors: Arc::new(
                Compressors::with_recommendations(
                    // Selected from configuration rather than hardcoded. An in-memory
                    // store loses every original on restart, and on a second worker the
                    // marker created here is requested from a process that never saw it.
                    ccr_store,
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
            upstream_health: Arc::new(std::sync::Mutex::new(None)),
            ccr_store_kind,
            ccr_store: ccr_store_for_purge,
        }
    }

    /// Which CCR store this process actually built.
    pub fn ccr_store_kind(&self) -> crate::config::CcrStoreKind {
        self.ccr_store_kind
    }

    /// The CCR store backing this state's compressors.
    ///
    /// Exposed so [`serve`] can spawn the periodic purge task against the exact store
    /// requests are writing to, rather than one built separately.
    pub(crate) fn ccr_store(&self) -> &Arc<dyn CcrStore> {
        &self.ccr_store
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

    /// Where the relay actually forwards, or `None` when there is no relay.
    ///
    /// Read from the built client rather than from configuration, because the two
    /// disagree after `POST /admin/runtime-env` stores a new `HEADROOM_UPSTREAM`: the
    /// override lands in the map, nothing rebuilds this client, and every request keeps
    /// going where it was going. `/health` reported the configured value and so confirmed
    /// a change that had not happened.
    pub fn upstream_base(&self) -> Option<&str> {
        self.upstream.as_ref().map(|upstream| upstream.base())
    }

    /// Whether the upstream is reachable, answered from a short-lived cache.
    ///
    /// # Why this is cached
    ///
    /// `/healthz/upstream` is scraped, often every few seconds by more than one prober.
    /// Probing on each call would turn a health check into an outbound request flood
    /// against the provider — and during an outage, when every probe is also retrying,
    /// that is the least welcome moment to add load.
    ///
    /// The window is short enough that an operator watching a dashboard sees a recovery
    /// within one refresh, and long enough that a scrape interval cannot amplify.
    ///
    /// `Instant` rather than a wall clock: this must not jump when the system clock is
    /// adjusted. Nothing here feeds compression, so I4's no-clocks rule does not apply —
    /// that rule is about the bytes sent upstream, and no byte of a request depends on
    /// this.
    pub async fn upstream_reachable(&self) -> bool {
        let Some(upstream) = self.upstream.as_ref() else {
            // No relay was built, so nothing is reachable through it. Answering from
            // configuration here would report a provider this process cannot call.
            return false;
        };

        if let Ok(cached) = self.upstream_health.lock() {
            if let Some((checked_at, reachable)) = *cached {
                if checked_at.elapsed() < UPSTREAM_PROBE_CACHE {
                    return reachable;
                }
            }
        }

        let reachable = upstream.is_reachable().await;

        // A poisoned lock loses the caching, not the answer. Refusing to report
        // reachability because a mutex is poisoned would take out the endpoint an
        // operator is using to find out what is wrong.
        if let Ok(mut cached) = self.upstream_health.lock() {
            *cached = Some((std::time::Instant::now(), reachable));
        }

        reachable
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
        // Liveness and upstream reachability, split. `/health` stays as it was so
        // anything already deployed against it keeps working. See `health::healthz`.
        .route("/healthz", get(crate::health::healthz))
        .route("/healthz/upstream", get(crate::health::upstream_health))
        .route("/admin/upstream", get(crate::admin::upstream))
        // CCR over HTTP, so a client that does not speak MCP can still redeem a marker
        // this proxy handed it. See `crate::ccr_api`.
        .route("/v1/compress", post(crate::ccr_api::compress))
        .route("/v1/retrieve", post(crate::ccr_api::retrieve_batch))
        .route("/v1/retrieve/stats", get(crate::ccr_api::retrieve_stats))
        .route("/v1/retrieve/{hash}", get(crate::ccr_api::retrieve_one))
        // Codex uses a WebSocket transport, and a proxy that only speaks HTTP silently
        // drops that client to whatever fallback it has, or breaks it.
        .route("/v1/realtime", get(crate::websocket::relay_socket))
        .route("/ws", get(crate::websocket::relay_socket))
        // Everything else is forwarded untouched. The routes above are fast paths for
        // the dialects this proxy compresses; they are not the boundary of what it
        // serves. See [`forward_any`].
        .fallback(forward_any)
        .with_state(state)
}

/// Forwards any request no route above claimed, untouched.
///
/// # Why a proxy needs this
///
/// `headroom wrap` and `headroom env` point an agent's **base URL** here. That sends us
/// the agent's entire API surface, not the handful of endpoints this proxy knows how to
/// compress. Without a fallback every other path answered 404 — from the client's side
/// indistinguishable from the provider having removed the endpoint:
///
/// - `GET /v1/models` — SDK model listing, called on startup by several clients
/// - `POST /v1/messages/count_tokens` — Anthropic token counting
/// - `POST /v1/messages/batches` — the batch API
/// - `POST /v1/embeddings` — OpenAI embeddings
/// - every provider endpoint shipped after this was written
///
/// That last one is the point. Enumerating the missing paths would fix today's list and
/// re-break on the next provider release; forwarding by default cannot go stale.
///
/// # Nothing here compresses
///
/// This path parses no bodies and re-serializes nothing — the bytes that arrive are the
/// bytes that leave (I1). Compression requires knowing a dialect's message shape well
/// enough to identify the live zone, and by construction this handler runs only for
/// paths where that is unknown. Guessing would be how a request arrives at the provider
/// subtly altered.
///
/// Header hygiene, the rate limiter and the request log still apply: they are properties
/// of relaying at all, not of compressing.
async fn forward_any(
    State(state): State<AppState>,
    method: Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    PeerAddr(peer): PeerAddr,
    MaybeUpgrade(upgrade): MaybeUpgrade,
    body: Bytes,
) -> Result<Response, RelayError> {
    // A WebSocket upgrade on an unregistered path is still a WebSocket. Forwarding it as
    // HTTP would relay the upgrade *handshake* as an ordinary request and hand the client
    // the provider's rejection of it, which reads as "the provider does not support
    // sockets" rather than "this proxy dropped the upgrade".
    //
    // The header is checked as well as the extractor because they can disagree: a client
    // that sends `Upgrade: websocket` without `Sec-WebSocket-Key` gets `None` from axum.
    // Falling through to HTTP then lets the upstream state the real problem, which is
    // more useful than an error invented here.
    if is_websocket_upgrade(&headers) {
        if let Some(upgrade) = upgrade {
            return Ok(crate::websocket::relay_socket(upgrade, State(state)).await);
        }
    }

    let policy = CompressionPolicy::for_mode(classify_auth_mode(&headers));
    let header_policy = HeaderPolicy {
        forwarded_headers: policy.forwarded_headers,
        strip_accept_encoding: policy.may_strip_accept_encoding,
    };
    let mut upstream_headers = sanitize(&headers, header_policy);
    if let Some(peer) = peer {
        apply_forwarded(&mut upstream_headers, header_policy, &peer.ip().to_string());
    }

    // Counted as passthrough because that is what it is. Leaving it uncounted would make
    // the savings ratio a statement about a subset of traffic while reading as a
    // statement about all of it.
    state.metrics().record_passthrough();

    // `uri.path()` is the fallback rather than a route literal: there is no route here,
    // and the query has to survive (`/v1/models?limit=5` is not `/v1/models`).
    let target = forwarded_target(&uri, uri.path());
    relay(&state, method, &target, &upstream_headers, body.to_vec()).await
}

/// Whether the request headers ask for a WebSocket upgrade.
///
/// Both header values are case-insensitive per RFC 6455, and `Connection` is a
/// comma-separated list — `Connection: keep-alive, Upgrade` is valid and common enough
/// that an equality check against `"upgrade"` misses real clients.
fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    let upgrade_to_websocket = headers
        .get(axum::http::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));

    let connection_upgrades = headers
        .get(axum::http::header::CONNECTION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        });

    upgrade_to_websocket && connection_upgrades
}

/// `GET /metrics` — Prometheus text exposition.
async fn metrics_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    (
        [("content-type", "text/plain; version=0.0.4")],
        state.metrics.render(),
    )
}

/// The client's peer address, when the connection carries one.
///
/// Reads `ConnectInfo<SocketAddr>` straight out of the request extensions instead of
/// taking `ConnectInfo<SocketAddr>` itself as an extractor, for the same reason
/// `admin::runtime_env` reads it by hand: `ConnectInfo` implements `FromRequestParts`
/// but not `OptionalFromRequestParts`, so `Option<ConnectInfo<SocketAddr>>` does not
/// compile as an extractor, and the bare `ConnectInfo<SocketAddr>` extractor rejects
/// the whole request outright when the server was not run through
/// `into_make_service_with_connect_info` — which is exactly how every
/// `Router::oneshot` test in this module and in `openai` drives these handlers.
/// `X-Forwarded-For` is a policy-gated courtesy, not a security control, so its
/// absence must never fail the request; this extractor reads the extension and
/// yields `None` instead of rejecting.
#[derive(Clone, Copy)]
pub struct PeerAddr(pub Option<SocketAddr>);

impl<S> FromRequestParts<S> for PeerAddr
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|info| info.0),
        ))
    }
}

/// A WebSocket upgrade, when the request is one.
///
/// Axum 0.8 implements `FromRequestParts` for `WebSocketUpgrade` but not
/// `OptionalFromRequestParts`, so `Option<WebSocketUpgrade>` does not compile as an
/// extractor and the bare one rejects every non-upgrade request outright — which on a
/// *fallback* would reject all ordinary traffic. Same shape as [`PeerAddr`], and for the
/// same reason: the handler needs to know whether the extraction succeeded, not to be
/// short-circuited when it did not.
pub struct MaybeUpgrade(pub Option<axum::extract::ws::WebSocketUpgrade>);

impl<S> FromRequestParts<S> for MaybeUpgrade
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            axum::extract::ws::WebSocketUpgrade::from_request_parts(parts, state)
                .await
                .ok(),
        ))
    }
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
    uri: axum::http::Uri,
    headers: HeaderMap,
    PeerAddr(peer): PeerAddr,
    body: Bytes,
) -> Result<Response, RelayError> {
    let config = Config::from_env();

    let auth_mode = classify_auth_mode(&headers);
    let policy = CompressionPolicy::for_mode(auth_mode);
    let header_policy = HeaderPolicy {
        forwarded_headers: policy.forwarded_headers,
        strip_accept_encoding: policy.may_strip_accept_encoding,
    };
    let mut upstream_headers = sanitize(&headers, header_policy);
    if let Some(peer) = peer {
        apply_forwarded(&mut upstream_headers, header_policy, &peer.ip().to_string());
    }
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
        state.metrics.record_rewritten(
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
        forwarded_target(&uri, "/v1/messages").as_str(),
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
/// The path and query to forward, falling back to `route` when the URI carries neither.
///
/// Handlers passed their route literal, which drops everything after the `?`. Measured
/// against a capture server: Claude Code calls `/v1/messages?beta=true` and the provider
/// received `/v1/messages`. Nothing failed — the request simply stopped being the one the
/// client sent, which is the shape of defect I1 exists to prevent on the body and nothing
/// was checking on the target.
pub(crate) fn forwarded_target(uri: &axum::http::Uri, route: &str) -> String {
    uri.path_and_query()
        .map(|target| target.as_str().to_owned())
        .unwrap_or_else(|| route.to_owned())
}

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
    // Read from the provider's own `content-type` rather than from the request's
    // `"stream"` flag: what matters is how the reply is framed, and the provider is the
    // one that decided. A non-streaming reply carries its usage in the body, which the
    // frame parser never sees.
    let event_stream = relayed
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    let observed = ObservingStream::with_framing(
        relayed.into_stream(),
        state.metrics.clone(),
        path,
        event_stream,
    );

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

    // Built and spawned before the server starts accepting requests, so the purge
    // task is present for the state's whole lifetime rather than raced in afterward.
    let state = AppState::default();
    spawn_ccr_purge_task(Arc::clone(state.ccr_store()));

    // `into_make_service_with_connect_info` rather than the plain service: the admin
    // endpoint's only protection is that it can tell a local caller from a remote one,
    // and without connect info it refuses every request including the legitimate ones.
    axum::serve(
        listener,
        router_with(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
}

/// How often the background task sweeps the CCR store for expired entries.
///
/// [`CcrStore::get`] already treats an expired entry as absent, so nothing on the
/// request path depends on this running promptly. What depends on it is process memory
/// (or disk, for a file-backed store): every lossy compression writes a new TTL'd entry,
/// and without a sweep the backing map/directory grows for the life of the process.
/// Every compressor sets its `CCR_TTL` to 24 hours, so five minutes is well inside a
/// tenth of the shortest TTL in use — frequent enough that the store never accumulates
/// more than a few minutes' worth of expired entries, without costing anything
/// measurable.
const CCR_PURGE_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Removes expired entries from `store` once, returning and logging how many were
/// removed.
///
/// Split out from [`spawn_ccr_purge_task`]'s scheduling loop so a test can call this
/// directly against a store it seeded with expired entries, without waiting on a real
/// timer to prove the wiring works.
fn purge_ccr_once(store: &dyn CcrStore) -> usize {
    let purged = store.purge_expired();
    if purged > 0 {
        tracing::debug!(purged, "purged expired CCR entries");
    } else {
        tracing::trace!("CCR purge pass found nothing expired");
    }
    purged
}

/// Spawns the background task that keeps the CCR store from growing without bound.
///
/// # Why this exists
///
/// [`CcrStore::get`] filters expired entries out of read results but never removes them
/// from the backing map or directory — [`CcrStore::purge_expired`] is the only thing
/// that does, and nothing in this binary called it before this task existed. Under
/// ordinary sustained traffic that meant an in-memory or file-backed store grew for the
/// life of the process, since every lossy compression writes a new TTL'd entry. A
/// Redis-backed store is unaffected — it expires keys natively — but is not the default
/// backend.
///
/// Spawned rather than awaited: [`serve`] starts accepting requests immediately, and
/// this task runs alongside them for as long as the process does. It never holds a lock
/// across an `.await` point and never blocks shutdown — dropping the runtime simply
/// stops it mid-sleep.
fn spawn_ccr_purge_task(store: Arc<dyn CcrStore>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(CCR_PURGE_INTERVAL);
        // The first tick fires immediately; nothing has had time to expire yet, so
        // consume it here rather than logging a pass that can only ever find zero
        // entries.
        interval.tick().await;
        loop {
            interval.tick().await;
            purge_ccr_once(store.as_ref());
        }
    });
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
                    (status, [("content-type", content_type_of(reply))], reply)
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

    /// The `content-type` a real provider would set for `reply`.
    ///
    /// These fakes returned a bare `&str`, which axum serves as `text/plain`. No fixture
    /// ever carried `text/event-stream`, so every SSE test was exercising a provider that
    /// does not exist — and it went unnoticed until the relay started reading the header
    /// to tell a stream from a body. A fixture that models the transport wrongly asserts
    /// something about a system nobody runs.
    fn content_type_of(reply: &str) -> &'static str {
        if reply.starts_with("event:") || reply.starts_with("data:") {
            "text/event-stream"
        } else {
            "application/json"
        }
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

    /// Starts a fake provider on loopback that captures the request headers it
    /// received, rather than the body.
    ///
    /// Companion to `fake_provider`: proving `X-Forwarded-For` reaches the wire needs
    /// to look at what actually left the process, not just what `sanitize`/
    /// `apply_forwarded` produced in memory.
    async fn fake_provider_capturing_headers(
        status: StatusCode,
        reply: &'static str,
    ) -> (String, Arc<Mutex<Option<HeaderMap>>>) {
        let captured: Arc<Mutex<Option<HeaderMap>>> = Arc::new(Mutex::new(None));
        let seen = captured.clone();

        let app = Router::new().route(
            "/v1/messages",
            post(move |headers: HeaderMap, _body: Bytes| {
                let seen = seen.clone();
                async move {
                    if let Ok(mut slot) = seen.lock() {
                        *slot = Some(headers);
                    }
                    (status, [("content-type", content_type_of(reply))], reply)
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

    /// Like `post_messages`, but attaches `ConnectInfo` to the request extensions the
    /// same way `into_make_service_with_connect_info` does in production — the only
    /// way a `oneshot`-driven test can supply one. See `PeerAddr`.
    async fn post_messages_from(
        app: Router,
        body: String,
        api_key: &str,
        peer: SocketAddr,
    ) -> Response {
        let mut request = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header("x-api-key", api_key)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(peer));

        app.oneshot(request).await.unwrap()
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
    async fn payg_traffic_carries_x_forwarded_for_and_subscription_traffic_does_not() {
        // Regression: `apply_forwarded` was fully implemented and unit-tested in
        // `headers.rs`, but nothing on the request path ever called it, so
        // `CompressionPolicy::forwarded_headers` had no observable effect — the policy
        // permitted `X-Forwarded-For` for PayAsYouGo and OAuth traffic and nothing
        // ever added it. A PayAsYouGo request must now carry the caller's address
        // upstream, and a Subscription request — where `forwarded_headers` is
        // `false` — must not.
        let peer: SocketAddr = "203.0.113.5:54321".parse().unwrap();
        let (base, captured) = fake_provider_capturing_headers(StatusCode::OK, "{}").await;
        let state = AppState::new(&base);
        let body = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#.to_owned();

        let response = post_messages_from(
            router_with(state.clone()),
            body.clone(),
            "sk-ant-api03-x",
            peer,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let headers = captured.lock().unwrap().clone().expect("nothing forwarded");
        assert_eq!(
            headers.get("x-forwarded-for").unwrap(),
            "203.0.113.5",
            "a PayAsYouGo request did not carry the caller's address upstream"
        );

        let response = post_messages_from(router_with(state), body, "", peer).await;
        assert_eq!(response.status(), StatusCode::OK);
        let headers = captured.lock().unwrap().clone().expect("nothing forwarded");
        assert!(
            headers.get("x-forwarded-for").is_none(),
            "a Subscription request carried the caller's address upstream"
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
    async fn a_non_streaming_response_feeds_the_cache_metrics_too() {
        // The framing must not decide whether the headline metric exists. Usage arrives
        // in a body rather than a frame here, and nothing was reading it — so batch work,
        // evaluation harnesses and any SDK call without `stream=True` reported no cache
        // data at all.
        const BODY: &str = concat!(
            r#"{"id":"msg_1","type":"message","role":"assistant","#,
            r#""content":[{"type":"text","text":"ok"}],"#,
            r#""usage":{"input_tokens":10,"cache_read_input_tokens":900,"#,
            r#""cache_creation_input_tokens":100}}"#,
        );
        let (base, _) = fake_provider(StatusCode::OK, BODY).await;
        let state = AppState::new(&base);

        let response = post_messages(
            router_with(state.clone()),
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#.to_owned(),
            "sk-ant-api03-x",
        )
        .await;

        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        // I9 still: accumulating a copy to read it must not change what the client gets.
        assert_eq!(bytes.as_ref(), BODY.as_bytes(), "the body was altered");

        assert_eq!(state.metrics().cache_hit_rate(), Some(0.9));
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
                    // Path *and query*. This recorded only the path, so a fixture built
                    // to prove the proxy relays to the right target could not see half
                    // of it — the same defect the handlers had, in the double meant to
                    // catch it.
                    let target = uri
                        .path_and_query()
                        .map(|target| target.as_str().to_owned())
                        .unwrap_or_else(|| uri.path().to_owned());
                    *slot = Some((target, body.to_vec()));
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

    /// Starts a fake provider that answers any path with `body`.
    async fn fake_any_path_answering(body: &'static str) -> String {
        let app = Router::new()
            .fallback(move || async move { ([("content-type", content_type_of(body))], body) });

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        format!("http://{addr}")
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

    /// Starts a fake provider that answers any path, capturing the headers it received.
    ///
    /// Companion to `fake_any_path`, which only inspects body and path — proving
    /// `X-Forwarded-For` reaches the wire requires looking at what actually left the
    /// process, not just what `sanitize`/`apply_forwarded` produced in memory.
    async fn fake_any_path_capturing_headers() -> (String, Arc<Mutex<Option<HeaderMap>>>) {
        let captured: Arc<Mutex<Option<HeaderMap>>> = Arc::new(Mutex::new(None));
        let seen = captured.clone();

        let app = Router::new().fallback(move |headers: HeaderMap, _body: Bytes| {
            let seen = seen.clone();
            async move {
                if let Ok(mut slot) = seen.lock() {
                    *slot = Some(headers);
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

    /// Like `post_to`, but with control over the caller's credential and peer address.
    ///
    /// `post_to` always sends the same PayAsYouGo API key, which is right for every
    /// test that only cares about compression. Proving `X-Forwarded-For` needs both a
    /// Subscription-mode call (no recognized credential) and a known peer, attached to
    /// the request extensions the same way `into_make_service_with_connect_info` does
    /// in production — see `PeerAddr`.
    async fn post_to_from(
        app: Router,
        path: &str,
        body: String,
        api_key: &str,
        peer: SocketAddr,
    ) -> Response {
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header("x-api-key", api_key)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(peer));

        app.oneshot(request).await.unwrap()
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
    async fn both_openai_surfaces_feed_the_cache_metrics_end_to_end() {
        // Companion to the Anthropic case above, and the reason it was not enough on its
        // own: the dispatcher answered a hardcoded zero for both OpenAI dialects, so
        // every request through these two routes reported no cache data while the
        // Anthropic test kept the metric looking covered.
        //
        // Driven through the whole path — route, relay, parser, observer, metrics —
        // because that is the only thing that shows the number reaching the gauge.
        const CHAT: &str = concat!(
            r#"data: {"choices":[{"index":0,"delta":{"content":"hi"}}],"usage":null}"#,
            "\n\n",
            r#"data: {"choices":[],"usage":{"prompt_tokens":1000,"#,
            r#""prompt_tokens_details":{"cached_tokens":900,"cache_write_tokens":100}}}"#,
            "\n\n",
            "data: [DONE]\n\n",
        );
        const RESPONSES: &str = concat!(
            "event: response.completed\n",
            r#"data: {"type":"response.completed","response":{"usage":{"output_tokens":12,"#,
            r#""input_tokens_details":{"cached_tokens":900,"cache_write_tokens":100}}}}"#,
            "\n\n",
        );

        for (path, stream) in [("/v1/chat/completions", CHAT), ("/v1/responses", RESPONSES)] {
            let base = fake_any_path_answering(stream).await;
            let state = AppState::new(&base);

            let response = post_to(
                router_with(state.clone()),
                path,
                r#"{"model":"gpt-4o","stream":true,"messages":[{"role":"user","content":"hi"}]}"#
                    .to_owned(),
            )
            .await;

            // Draining is what makes the observer see it; nothing is buffered on the
            // proxy's behalf.
            let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap();
            assert_eq!(
                bytes.as_ref(),
                stream.as_bytes(),
                "{path} altered the stream"
            );

            let text = state.metrics().render();
            assert!(
                text.contains("headroom_cache_read_tokens_total 900"),
                "{path} did not report its cache reads:\n{text}"
            );
            assert!(
                text.contains("headroom_cache_creation_tokens_total 100"),
                "{path} did not report its cache writes:\n{text}"
            );
            assert_eq!(state.metrics().cache_hit_rate(), Some(0.9), "{path}");
        }
    }

    #[tokio::test]
    async fn openai_routes_forward_the_caller_only_under_the_permissive_policy() {
        // Regression: `apply_forwarded` was fully implemented and unit-tested in
        // `headers.rs`, but nothing on the request path ever called it, so
        // `CompressionPolicy::forwarded_headers` had no observable effect. A
        // PayAsYouGo request through either OpenAI surface must now carry
        // `X-Forwarded-For` with the caller's address, and a Subscription request
        // — where `forwarded_headers` is `false` — must not.
        let peer: SocketAddr = "203.0.113.9:54321".parse().unwrap();

        for (path, body) in [
            (
                "/v1/chat/completions",
                r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#,
            ),
            ("/v1/responses", r#"{"model":"gpt-4o","input":"hi"}"#),
        ] {
            let (base, captured) = fake_any_path_capturing_headers().await;
            let state = AppState::new(&base);

            let response = post_to_from(
                router_with(state.clone()),
                path,
                body.to_owned(),
                "sk-ant-api03-x",
                peer,
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let headers = captured.lock().unwrap().clone().expect("nothing forwarded");
            assert_eq!(
                headers.get("x-forwarded-for").unwrap(),
                "203.0.113.9",
                "{path} did not carry the caller's address under PayAsYouGo"
            );

            let response = post_to_from(router_with(state), path, body.to_owned(), "", peer).await;
            assert_eq!(response.status(), StatusCode::OK);
            let headers = captured.lock().unwrap().clone().expect("nothing forwarded");
            assert!(
                headers.get("x-forwarded-for").is_none(),
                "{path} carried the caller's address under Subscription mode"
            );
        }
    }

    #[tokio::test]
    async fn passthrough_is_byte_identical_only_where_nothing_enriches() {
        // `headroom_passthrough_total` was documented as "Requests forwarded unchanged".
        // It is not that, on two of the three surfaces: `shape_openai` adds
        // `prompt_cache_key` and `reasoning_effort` after compression declines, so a
        // request nothing compressed still goes out larger than it came in.
        //
        // The third claim in a row that held for `/v1/messages` and failed for both
        // OpenAI routes, after the volatile scan and the SSE cache accounting. So this
        // walks all three rather than asserting the Anthropic case and generalizing.
        //
        // Both outcomes are pinned, not just the surprising one. A test that only
        // asserted "chat is not identical" would pass if the proxy started rewriting
        // `/v1/messages` too, which is the failure that would actually matter.
        for (path, body, identical) in [
            (
                "/v1/messages",
                r#"{"model":"claude-opus-4","messages":[{"role":"user","content":"hi"}]}"#,
                true,
            ),
            (
                "/v1/chat/completions",
                r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#,
                false,
            ),
            (
                "/v1/responses",
                r#"{"model":"gpt-4o","input":[{"role":"user","content":"hi"}]}"#,
                false,
            ),
        ] {
            let (base, captured) = fake_any_path().await;
            let state = AppState::new(&base);

            post_to(router_with(state.clone()), path, body.to_owned()).await;
            let (_, sent) = captured.lock().unwrap().clone().expect("nothing forwarded");

            // The vacuity guard. Without it a request that failed to route at all would
            // satisfy the identity assertion for `/v1/messages` while proving nothing.
            assert!(
                state
                    .metrics()
                    .render()
                    .contains("headroom_passthrough_total 1"),
                "{path} was not counted as passthrough, so this proves nothing"
            );

            assert_eq!(
                sent == body.as_bytes(),
                identical,
                "{path}: expected byte-identical={identical}, got {} bytes for {}\n{}",
                sent.len(),
                body.len(),
                String::from_utf8_lossy(&sent)
            );

            // What the enrichment actually adds, named rather than merely permitted —
            // so a *different* mutation on these routes fails here instead of passing
            // as "not identical, as expected".
            if !identical {
                let parsed: serde_json::Value = serde_json::from_slice(&sent).unwrap();
                let original: serde_json::Value = serde_json::from_str(body).unwrap();
                let added: Vec<&String> = parsed
                    .as_object()
                    .unwrap()
                    .keys()
                    .filter(|key| original.get(key.as_str()).is_none())
                    .collect();
                assert_eq!(
                    added,
                    ["reasoning_effort"],
                    "{path} added something other than the documented enrichment"
                );
                // Everything the customer sent has to survive it untouched — I1 applies
                // to the bytes this proxy did not set out to change.
                for (key, value) in original.as_object().unwrap() {
                    assert_eq!(parsed.get(key), Some(value), "{path} altered {key}");
                }
            }
        }
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
    async fn every_relaying_route_forwards_its_query_string() {
        // Handlers passed their route literal, which drops everything after the `?`.
        // Found by pointing Claude Code at a capture server: it calls
        // `/v1/messages?beta=true` and the provider received `/v1/messages`. Nothing
        // failed — the request simply stopped being the one the client sent.
        //
        // A table over the routes, because the literal was per-handler and so was the
        // defect; `/v1/conversations` had a `Uri` and still called `.path()` on it.
        for (route, sent) in [
            ("/v1/messages", "/v1/messages?beta=true"),
            ("/v1/chat/completions", "/v1/chat/completions?beta=true"),
            ("/v1/responses", "/v1/responses?beta=true"),
            ("/v1/conversations", "/v1/conversations?limit=5"),
        ] {
            let (base, captured) = fake_any_path().await;
            post_to(
                router_with(AppState::new(&base)),
                sent,
                r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#.to_owned(),
            )
            .await;

            let (arrived, _) = captured.lock().unwrap().clone().expect("nothing forwarded");
            assert_eq!(arrived, sent, "{route} dropped its query string");
        }
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

    /// Every path `router_with` registers, with the method it is registered under.
    ///
    /// Kept in step with the router by check 8 of `scripts/reachability-audit.sh`, which
    /// reads the `.route(` calls out of this file and fails if one is missing here. A
    /// hand-maintained copy of a list is exactly what this project keeps getting wrong,
    /// and axum's `Router` cannot be enumerated, so the list is checked instead of shared.
    const ROUTES: [(&str, &str); 17] = [
        ("POST", "/v1/compress"),
        ("POST", "/v1/retrieve"),
        ("GET", "/v1/retrieve/stats"),
        ("GET", "/v1/retrieve/{hash}"),
        ("GET", "/health"),
        ("GET", "/healthz"),
        ("GET", "/healthz/upstream"),
        ("GET", "/admin/upstream"),
        ("GET", "/metrics"),
        ("POST", "/v1/messages"),
        ("POST", "/v1/chat/completions"),
        ("POST", "/v1/responses"),
        ("POST", "/v1/responses/compact"),
        ("POST", "/v1/conversations"),
        ("POST", "/admin/runtime-env"),
        ("GET", "/v1/realtime"),
        ("GET", "/ws"),
    ];

    /// A provider that answers 200 on *any* path.
    ///
    /// [`fake_provider`] registers `/v1/messages` only, so a relayed request to any other
    /// path comes back 404 — from the provider, not from this proxy's router. The first
    /// version of `every_declared_route_is_actually_reachable` used it and reported
    /// `/v1/chat/completions` as unrouted, which was the fake upstream's 404 wearing the
    /// proxy's clothes. A test that cannot tell those two apart is worse than no test,
    /// because it accuses the wrong component.
    async fn permissive_upstream() -> String {
        let app = Router::new().fallback(|| async { (StatusCode::OK, "{}") });

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        format!("http://{addr}")
    }

    /// The routes that relay to the provider, and therefore have a path to preserve.
    ///
    /// A subset of [`ROUTES`], checked to be one by
    /// `every_relaying_route_forwards_its_own_path` — the rest either answer locally
    /// (`/health`, `/metrics`, `/admin/runtime-env`) or upgrade to a socket.
    const RELAYING: [&str; 5] = [
        "/v1/messages",
        "/v1/chat/completions",
        "/v1/responses",
        "/v1/responses/compact",
        "/v1/conversations",
    ];

    #[tokio::test]
    async fn every_relaying_route_forwards_its_own_path() {
        // Each OpenAI handler hands `relay` a hardcoded upstream path —
        // `"/v1/chat/completions"` in `chat_completions`, `"/v1/responses"` in
        // `responses`. A literal that drifted from its route would send the provider a
        // path it does not serve, and the client would get the provider's 404 for a
        // request the proxy accepted.
        //
        // Three of these five were checked individually. This covers all of them, and
        // covers a route added later without anyone remembering to write the assertion.
        //
        // The path also picks the SSE vocabulary: `Observer::for_path` falls back to the
        // Anthropic classifier, so an OpenAI path arriving misspelled would be read with
        // the wrong grammar and report a healthy stream as unfinished (D18).
        for path in RELAYING {
            assert!(
                ROUTES.iter().any(|(_, declared)| *declared == path),
                "{path} relays and is not a registered route"
            );

            let (base, captured) = fake_any_path().await;
            post_to(
                router_with(AppState::new(&base)),
                path,
                r#"{"model":"m","messages":[]}"#.to_owned(),
            )
            .await;

            let (seen, _) = captured
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| panic!("{path} forwarded nothing"));
            assert_eq!(seen, path, "{path} reached the provider as {seen}");
        }
    }

    #[tokio::test]
    async fn every_declared_route_is_actually_reachable() {
        // A route registered and never requested is a route a typo silently disables.
        //
        // `/v1/realtime` was in that state: the comment beside it says it exists because
        // Codex speaks WebSocket and a proxy that only speaks HTTP breaks that client,
        // and nothing in the suite ever asked the router for it. The `/ws` in
        // `websocket.rs`'s tests is that test's own echo server, not this router's route
        // — so both WebSocket paths were unverified while the handler underneath was
        // well covered. A test proves a function works, not that anything routes to it.
        //
        // 404 and 405 are the failures worth catching. Everything else — 400 on a
        // WebSocket path without upgrade headers, 502 with no upstream, 403 from the
        // admin loopback guard — means the request reached a handler, which is the claim.
        let base = permissive_upstream().await;

        for (method, path) in ROUTES {
            let response = router_with(AppState::new(&base))
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_ne!(
                response.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} {path} is registered under a different method"
            );

            // 404 needs a second look rather than a flat rejection. `GET
            // /v1/retrieve/{hash}` answers 404 for a hash the store does not hold, which
            // is a *handler* saying "not here" — the store is empty in this fixture, so
            // it is also the only answer it can give. Axum's own 404 for an unrouted path
            // carries an empty body; ours carries a JSON `error`. That difference is what
            // separates "reached a handler" from "no route", and checking it keeps this
            // test meaningful for the one route whose success case is a miss.
            if response.status() == StatusCode::NOT_FOUND {
                let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
                    .await
                    .unwrap();
                let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
                assert!(
                    body.get("error").is_some(),
                    "{method} {path} 404'd with no handler body, so nothing routed to it"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_registered_route_is_not_merely_served_by_the_fallback() {
        // The test above used to be kept honest by a companion asserting that an
        // unregistered path 404s — so "reached a handler" meant something. Once every
        // path reaches the fallback (#175) that companion could no longer tell the two
        // apart, and the reachability test would have passed on a router whose dialect
        // routes had all been deleted.
        //
        // The discriminator that survives: a dialect route *compresses*, and the
        // fallback forwards bytes untouched by construction. Same bulky body down both
        // paths, and what the provider receives has to differ.
        let (base, captured) = fake_any_path().await;
        let body = compressible_request();

        post_to(
            router_with(AppState::new(&base)),
            "/v1/messages",
            body.clone(),
        )
        .await;
        let (_, through_route) = captured.lock().unwrap().clone().expect("route forwarded");

        post_to(
            router_with(AppState::new(&base)),
            "/v1/definitely-not-a-route",
            body.clone(),
        )
        .await;
        let (_, through_fallback) = captured
            .lock()
            .unwrap()
            .clone()
            .expect("fallback forwarded");

        assert!(
            through_route.len() < body.len(),
            "/v1/messages did not compress — it may now be served by the fallback"
        );
        assert_eq!(
            through_fallback,
            body.as_bytes(),
            "the fallback altered a body it must forward untouched"
        );
    }

    // ---- the catch-all fallback (#175) ----

    #[tokio::test]
    async fn an_unregistered_path_is_forwarded_rather_than_404() {
        // The proxy is pointed at by an agent's *base URL*, so it receives that agent's
        // whole API surface. Answering 404 for `/v1/models` is indistinguishable, from
        // the client's side, from the provider having removed the endpoint.
        let (base, captured) = fake_any_path().await;

        let response = router_with(AppState::new(&base))
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let (seen, _) = captured.lock().unwrap().clone().expect("forwarded nothing");
        assert_eq!(seen, "/v1/models");
    }

    #[tokio::test]
    async fn the_fallback_preserves_the_method_and_the_query() {
        // A relayed request that arrives with the wrong verb, or with the query dropped,
        // is not the request the client sent — the defect `forwarded_target` was written
        // for, on the one path that has no route literal to fall back to.
        for method in ["GET", "POST", "DELETE", "PATCH"] {
            let (base, captured) = fake_any_path().await;

            router_with(AppState::new(&base))
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri("/v1/models?limit=5&after=abc")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            let (seen, _) = captured
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| panic!("{method} forwarded nothing"));
            assert_eq!(
                seen, "/v1/models?limit=5&after=abc",
                "{method} lost part of its target"
            );
        }
    }

    #[tokio::test]
    async fn the_fallback_forwards_the_body_byte_for_byte() {
        // Invariant I1 on the path that has the least excuse to break it: this handler
        // parses nothing, so anything other than byte equality means bytes were touched
        // by a layer that had no business touching them.
        let (base, captured) = fake_any_path().await;
        let body = r#"{"unicode":"héllo 🎉","precision":1.0000000000000002,"order":{"b":1,"a":2}}"#;

        post_to(
            router_with(AppState::new(&base)),
            "/v1/some/unknown/endpoint",
            body.to_owned(),
        )
        .await;

        let (_, seen) = captured.lock().unwrap().clone().expect("forwarded nothing");
        assert_eq!(seen, body.as_bytes());
    }

    #[tokio::test]
    async fn the_fallback_still_strips_the_proxys_own_headers() {
        // Header hygiene (X3) is a property of relaying, not of compressing. A header
        // that leaks upstream here leaks just as far as one leaking from `/v1/messages`.
        let (base, headers) = fake_any_path_capturing_headers().await;

        router_with(AppState::new(&base))
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/models")
                    .header("x-headroom-debug", "1")
                    .header("x-api-key", "sk-ant-api03-x")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let seen = headers.lock().unwrap().clone().expect("forwarded nothing");
        assert!(
            !seen.contains_key("x-headroom-debug"),
            "a proxy-internal header reached the provider"
        );
        assert!(
            seen.contains_key("x-api-key"),
            "the client's credential did not survive the hop"
        );
    }

    #[tokio::test]
    async fn the_fallback_is_rate_limited_like_every_other_relay() {
        // Otherwise the 600/min backstop guards the routes and leaves an unmetered hole
        // next to them, which is the same as not having it.
        let base = permissive_upstream().await;
        let state = AppState::with_rate_limit(&base, 1, Duration::from_secs(60));

        let first = router_with(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let second = router_with(state)
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn a_websocket_upgrade_is_recognized_in_a_header_list() {
        // `Connection: keep-alive, Upgrade` is valid and common. An equality check
        // against "upgrade" misses it and drops the client to HTTP forwarding, where the
        // upgrade handshake is relayed as an ordinary request.
        let mut headers = HeaderMap::new();
        headers.insert("upgrade", "WebSocket".parse().unwrap());
        headers.insert("connection", "keep-alive, Upgrade".parse().unwrap());
        assert!(is_websocket_upgrade(&headers));

        let mut plain = HeaderMap::new();
        plain.insert("connection", "keep-alive".parse().unwrap());
        assert!(!is_websocket_upgrade(&plain));

        // `Upgrade` without `Connection: upgrade` is not a valid upgrade request.
        let mut half = HeaderMap::new();
        half.insert("upgrade", "websocket".parse().unwrap());
        assert!(!is_websocket_upgrade(&half));
    }

    // ---- liveness vs upstream reachability (#186) ----

    #[tokio::test]
    async fn liveness_succeeds_even_when_the_upstream_is_unreachable() {
        // The whole reason `/healthz` is separate from `/healthz/upstream`. If liveness
        // depended on the provider, an orchestrator would restart a perfectly healthy
        // proxy during someone else's outage — dropping in-flight requests to fix
        // nothing a restart can fix.
        //
        // Port 1 on loopback with nothing bound is a connection refused, which is the
        // cheapest unreachable upstream available offline.
        let state = AppState::new("http://127.0.0.1:1");

        let live = router_with(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(live.status(), StatusCode::OK);

        let upstream = router_with(state)
            .oneshot(
                Request::builder()
                    .uri("/healthz/upstream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            upstream.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "an unreachable upstream reported as reachable"
        );
    }

    #[tokio::test]
    async fn upstream_health_succeeds_against_a_reachable_provider() {
        // The control. Without it the test above passes on a probe that always fails.
        let base = permissive_upstream().await;

        let response = router_with(AppState::new(&base))
            .oneshot(
                Request::builder()
                    .uri("/healthz/upstream")
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
        assert_eq!(json["reachable"], true);
        assert_eq!(json["status"], "ok");
    }

    #[tokio::test]
    async fn a_non_2xx_upstream_is_still_reachable() {
        // Reachability is not authorization. A provider answering 401 to an
        // unauthenticated probe is *up* — DNS, TLS and the network path all worked.
        // Reading that as "down" would have every proxy report an outage permanently,
        // since the probe deliberately carries no credential.
        let app = Router::new().fallback(|| async { (StatusCode::UNAUTHORIZED, "{}") });
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let response = router_with(AppState::new(&format!("http://{addr}")))
            .oneshot(
                Request::builder()
                    .uri("/healthz/upstream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn repeated_upstream_probes_do_not_flood_the_provider() {
        // `/healthz/upstream` is scraped. Probing per call would turn a health check
        // into outbound load against a provider that, when this endpoint matters most,
        // is already struggling.
        let hits = Arc::new(Mutex::new(0usize));
        let seen = hits.clone();

        let app = Router::new().fallback(move || {
            let seen = seen.clone();
            async move {
                if let Ok(mut count) = seen.lock() {
                    *count += 1;
                }
                (StatusCode::OK, "{}")
            }
        });
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let state = AppState::new(&format!("http://{addr}"));
        for _ in 0..5 {
            router_with(state.clone())
                .oneshot(
                    Request::builder()
                        .uri("/healthz/upstream")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
        }

        let count = *hits.lock().unwrap();
        assert_eq!(count, 1, "5 scrapes produced {count} outbound probes");
    }

    #[tokio::test]
    async fn health_is_unchanged_by_the_new_endpoints() {
        // `/health` predates these and is what anything already deployed reads. Adding
        // siblings must not move it.
        let base = permissive_upstream().await;
        let response = router_with(AppState::new(&base))
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
        assert!(json.get("relay_available").is_some());
    }

    #[tokio::test]
    async fn admin_upstream_refuses_a_caller_it_cannot_place() {
        // Same gate as `/admin/runtime-env`. A `oneshot` test carries no `ConnectInfo`,
        // which is exactly the "cannot establish the caller is local" case — and this
        // endpoint's only protection is that it can.
        let base = permissive_upstream().await;
        let response = router_with(AppState::new(&base))
            .oneshot(
                Request::builder()
                    .uri("/admin/upstream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
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
    async fn a_404_on_an_unknown_route_now_comes_from_the_provider() {
        // This asserted a locally-produced 404 until #175 gave the router a catch-all.
        // The status can still be 404 — but it has to be the *provider's*, because only
        // the provider knows which of its endpoints exist. The distinction is the whole
        // point of the change: a path we do not recognize is not a path that does not
        // exist.
        //
        // Written against a fake provider rather than `router()`, which resolves its
        // upstream from the environment and would put a real network call in a unit
        // test — the reason the earlier version of this test read 404 for a while
        // without anyone noticing what was answering it.
        //
        // The fake answers *any* path. `fake_provider` serves only `POST /v1/messages`,
        // so it would have produced its own empty-bodied 404 here and the assertion
        // below would have been checking axum's rejection rather than a provider reply.
        let app = Router::new().fallback(|| async {
            (
                StatusCode::NOT_FOUND,
                [("content-type", "application/json")],
                r#"{"type":"error"}"#,
            )
        });
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let base = format!("http://{addr}");

        let response = router_with(AppState::new(&base))
            .oneshot(Request::builder().uri("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert_eq!(
            bytes,
            r#"{"type":"error"}"#.as_bytes(),
            "the client got a locally-invented 404 rather than the provider's answer"
        );
    }

    #[test]
    fn purge_ccr_once_removes_expired_entries_from_the_store() {
        // Regression: `get` already treats an expired entry as absent, but nothing in
        // this file ever called `purge_expired` on the store it built, so an in-memory
        // or file-backed store's expired entries lived for the process's whole life.
        // This exercises `purge_ccr_once` directly — the piece `spawn_ccr_purge_task`
        // schedules on a `tokio::time::interval` — so the fix is proven without
        // sleeping for a real timer tick.
        use headroom_core::ccr::{ContentHash, InMemoryCcrStore};

        let store = InMemoryCcrStore::new();
        store
            .put(ContentHash::of(b"stale"), b"stale", Duration::ZERO)
            .unwrap();
        store
            .put(
                ContentHash::of(b"fresh"),
                b"fresh",
                Duration::from_secs(300),
            )
            .unwrap();
        assert_eq!(
            store.len(),
            1,
            "len() already excludes the expired entry, before any purge runs"
        );

        let purged = purge_ccr_once(&store);

        assert_eq!(purged, 1, "exactly the expired entry was removed");
        assert_eq!(store.len(), 1, "the live entry is still retrievable");
        assert_eq!(
            purge_ccr_once(&store),
            0,
            "a second pass has nothing left to collect"
        );
    }
}
