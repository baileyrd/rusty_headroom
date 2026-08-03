//! Upstream relay — the half of the proxy that was missing.
//!
//! Everything before this module decided *what bytes to send*. This module sends
//! them and hands the provider's answer back to the client.
//!
//! # Streaming is not an optimization here
//!
//! The response body is relayed as a stream, never buffered. For a non-streaming
//! JSON response the difference is a few milliseconds and some memory. For an SSE
//! response it is the whole feature: buffering means holding the model's entire
//! answer until generation finishes and then releasing it in one burst, so the user
//! watches a spinner for thirty seconds instead of reading tokens as they arrive.
//! A proxy that does that has broken streaming while reporting success.
//!
//! # Which headers survive the hop
//!
//! [`crate::headers::sanitize`] already strips the proxy's own headers, the
//! hop-by-hop set, and — where policy permits — `accept-encoding`. Two more have to
//! go here, for a reason that only exists at the relay boundary: they describe the
//! *client-to-proxy* connection and would be wrong on the *proxy-to-upstream* one.
//!
//! - `host` points at the proxy. Forwarded verbatim it tells the provider's router
//!   to look up a virtual host that does not exist there.
//! - `content-length` describes the body the client sent. Compression changed that
//!   body, so the original length is now a lie — and an under-declared
//!   `content-length` truncates the request server-side, which surfaces as the model
//!   answering a question that was cut off mid-sentence.

use std::time::Duration;

use bytes::Bytes;
use futures_core::Stream;
use http::header::{HeaderMap, HeaderName};
use http::{Method, StatusCode};

/// Headers that describe the client-to-proxy hop and must be rebuilt for the
/// proxy-to-upstream one.
const REBUILT_PER_HOP: [&str; 2] = ["host", "content-length"];

/// How long to wait for the TCP and TLS handshake.
///
/// Bounded because a connect that hangs is never going to succeed, unlike a response
/// that is merely slow.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors from the relay.
///
/// Deliberately coarse. A caller can only do one thing with any of these — return a
/// gateway error — so finer distinctions would be detail nobody acts on.
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    /// The upstream could not be reached, or the connection failed mid-request.
    #[error("upstream request failed: {0}")]
    Transport(String),
    /// The configured upstream is not a usable URL.
    #[error("invalid upstream url: {0}")]
    InvalidUrl(String),
}

impl RelayError {
    /// The status to return to the client.
    ///
    /// Always in the 5xx *gateway* range, never a plain 500. The distinction matters
    /// to whoever is paged: `502`/`504` says the dependency failed, `500` says this
    /// process has a bug. Collapsing them sends people to read proxy source when the
    /// provider is down.
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Transport(_) => StatusCode::BAD_GATEWAY,
            // A misconfigured upstream is this proxy's fault, but it is still not a
            // panic — 502 keeps every relay failure in one bucket for alerting, and
            // the message says which kind it was.
            Self::InvalidUrl(_) => StatusCode::BAD_GATEWAY,
        }
    }
}

/// A relayed response, with its body still un-consumed.
#[derive(Debug)]
pub struct RelayedResponse {
    status: StatusCode,
    headers: HeaderMap,
    inner: reqwest::Response,
}

impl RelayedResponse {
    /// The upstream status code.
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// Headers safe to return to the client.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Consumes the response into a byte stream.
    ///
    /// A stream rather than a `Bytes`, so an SSE response reaches the client chunk by
    /// chunk as the provider produces it.
    pub fn into_stream(self) -> impl Stream<Item = reqwest::Result<Bytes>> {
        self.inner.bytes_stream()
    }

    /// Consumes the response into its full body.
    ///
    /// For callers that genuinely need the whole thing — tests, and the non-streaming
    /// paths where the body is a single JSON object anyway.
    ///
    /// # Errors
    ///
    /// Returns [`RelayError::Transport`] if the connection fails while the body is
    /// being read.
    pub async fn into_bytes(self) -> Result<Bytes, RelayError> {
        self.inner
            .bytes()
            .await
            .map_err(|err| RelayError::Transport(err.to_string()))
    }
}

/// A client for one upstream provider.
///
/// Holds a connection pool, so it is built once and shared. Building one per request
/// would open a fresh TLS connection every time — measurable latency on every call,
/// against a provider that is already the slow part.
#[derive(Debug, Clone)]
pub struct Upstream {
    client: reqwest::Client,
    base: String,
}

impl Upstream {
    /// Builds a relay for `base`, e.g. `https://api.anthropic.com`.
    ///
    /// # Errors
    ///
    /// Returns [`RelayError::Transport`] if the HTTP client cannot be constructed,
    /// which in practice means the TLS backend failed to initialize.
    pub fn new(base: impl Into<String>) -> Result<Self, RelayError> {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            // No total-request timeout, deliberately. A long generation is a normal
            // outcome, not a stuck request, and a timeout generous enough never to cut
            // one off is too generous to catch anything else. The connect timeout is
            // where a hang actually gets caught.
            .build()
            .map_err(|err| RelayError::Transport(err.to_string()))?;

        Ok(Self {
            client,
            base: base.into().trim_end_matches('/').to_owned(),
        })
    }

    /// Forwards a request and returns the response with its body unread.
    ///
    /// `headers` should already have been through [`crate::headers::sanitize`]; the
    /// per-hop headers listed in this module's docs are removed here.
    ///
    /// # Errors
    ///
    /// Returns [`RelayError`] if the upstream URL is unusable or the request fails.
    /// A non-2xx *response* is not an error — it is the provider's answer and is
    /// relayed to the client unchanged, because a 429 the client cannot see is a 429
    /// it cannot back off from.
    pub async fn forward(
        &self,
        method: Method,
        path: &str,
        headers: &HeaderMap,
        body: Vec<u8>,
    ) -> Result<RelayedResponse, RelayError> {
        let url = format!("{}/{}", self.base, path.trim_start_matches('/'));

        let response = self
            .client
            .request(method, &url)
            .headers(relay_headers(headers))
            .body(body)
            .send()
            .await
            .map_err(|err| {
                // The URL is echoed but the headers are not — they carry the
                // customer's provider credential, and an error string is the least
                // controlled place in the system for one to end up.
                RelayError::Transport(format!("{url}: {err}"))
            })?;

        Ok(RelayedResponse {
            status: response.status(),
            headers: response_headers(response.headers()),
            inner: response,
        })
    }
}

/// Strips the headers that describe the client-to-proxy hop.
fn relay_headers(headers: &HeaderMap) -> HeaderMap {
    let mut outgoing = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        if REBUILT_PER_HOP.contains(&name.as_str()) {
            continue;
        }
        outgoing.append(name.clone(), value.clone());
    }
    outgoing
}

/// Filters upstream response headers down to what may be returned to the client.
///
/// `content-length` and `transfer-encoding` are dropped because the server that
/// returns this response re-frames the body itself. Leaving upstream's framing
/// headers in place alongside the new framing is how a response arrives truncated at
/// exactly the length the *old* header claimed.
fn response_headers(headers: &HeaderMap) -> HeaderMap {
    const DROPPED: [&str; 4] = [
        "content-length",
        "transfer-encoding",
        "connection",
        "keep-alive",
    ];

    let mut outgoing = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        if DROPPED.contains(&name.as_str()) {
            continue;
        }
        outgoing.append(name.clone(), value.clone());
    }
    outgoing
}

/// Whether a header name is one the relay rebuilds rather than forwards.
///
/// Exposed so the request path can assert it rather than duplicate the list.
pub fn is_rebuilt_per_hop(name: &HeaderName) -> bool {
    REBUILT_PER_HOP.contains(&name.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::{any, post};
    use axum::Router;
    use http::header::HeaderValue;
    use std::net::SocketAddr;

    /// Starts `router` on an ephemeral loopback port and returns its base URL.
    ///
    /// A real server on a real socket rather than a mock. The whole point of this
    /// module is that bytes cross a network boundary correctly, and a mock that stands
    /// in for the transport cannot show that they do.
    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn a_request_body_arrives_upstream_byte_identical() {
        // Invariant I1 across the relay boundary. Everything upstream of here works
        // hard to preserve bytes; this asserts the relay does not undo it.
        let router = Router::new().route(
            "/v1/messages",
            post(|body: Bytes| async move { body }), // echo
        );
        let base = serve(router).await;

        // CJK, a trailing-zero float, and an integer past 2^53 — the three shapes a
        // careless JSON round-trip corrupts.
        let source = r#"{"a":1.0,"b":"日本語","c":9007199254740993}"#.as_bytes().to_vec();
        let upstream = Upstream::new(&base).unwrap();
        let response = upstream
            .forward(
                Method::POST,
                "/v1/messages",
                &HeaderMap::new(),
                source.clone(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.into_bytes().await.unwrap().as_ref(), &source[..]);
    }

    #[tokio::test]
    async fn the_provider_credential_is_forwarded() {
        // The proxy is useless if the key does not reach the provider, and a test that
        // only checks the body would not notice.
        let router = Router::new().route(
            "/v1/messages",
            post(|headers: HeaderMap| async move {
                headers
                    .get("x-api-key")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("absent")
                    .to_owned()
            }),
        );
        let base = serve(router).await;

        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("sk-ant-api03-secret"));

        let body = Upstream::new(&base)
            .unwrap()
            .forward(Method::POST, "/v1/messages", &headers, Vec::new())
            .await
            .unwrap()
            .into_bytes()
            .await
            .unwrap();

        assert_eq!(body.as_ref(), b"sk-ant-api03-secret");
    }

    #[tokio::test]
    async fn a_stale_host_header_does_not_reach_upstream() {
        // The client's `host` names the proxy. Forwarded verbatim it routes the
        // request to a virtual host the provider does not have.
        let router = Router::new().route(
            "/v1/messages",
            post(|headers: HeaderMap| async move {
                headers
                    .get("host")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("absent")
                    .to_owned()
            }),
        );
        let base = serve(router).await;

        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("localhost:8787"));

        let body = Upstream::new(&base)
            .unwrap()
            .forward(Method::POST, "/v1/messages", &headers, Vec::new())
            .await
            .unwrap()
            .into_bytes()
            .await
            .unwrap();

        assert_ne!(
            body.as_ref(),
            b"localhost:8787",
            "the proxy's own host header was forwarded"
        );
    }

    #[tokio::test]
    async fn a_stale_content_length_does_not_truncate_the_request() {
        // The regression this guards: compression changes the body length, so the
        // client's `content-length` is now short. Forwarded, it truncates the request
        // server-side and the model answers a question that was cut off mid-sentence.
        let router = Router::new().route("/v1/messages", post(|body: Bytes| async move { body }));
        let base = serve(router).await;

        let compressed = b"a much longer body than the client originally sent".to_vec();
        let mut headers = HeaderMap::new();
        headers.insert("content-length", HeaderValue::from_static("5"));

        let echoed = Upstream::new(&base)
            .unwrap()
            .forward(Method::POST, "/v1/messages", &headers, compressed.clone())
            .await
            .unwrap()
            .into_bytes()
            .await
            .unwrap();

        assert_eq!(echoed.as_ref(), &compressed[..]);
    }

    #[tokio::test]
    async fn an_upstream_error_status_is_relayed_rather_than_swallowed() {
        // A 429 the client never sees is a 429 it cannot back off from, so the proxy
        // would turn rate limiting into an outage.
        let router = Router::new().route(
            "/v1/messages",
            post(|| async { (StatusCode::TOO_MANY_REQUESTS, r#"{"type":"error"}"#) }),
        );
        let base = serve(router).await;

        let response = Upstream::new(&base)
            .unwrap()
            .forward(Method::POST, "/v1/messages", &HeaderMap::new(), Vec::new())
            .await
            .expect("a 429 is an answer, not a transport failure");

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response.into_bytes().await.unwrap().as_ref(),
            br#"{"type":"error"}"#
        );
    }

    #[tokio::test]
    async fn an_unreachable_upstream_is_a_gateway_error_not_a_panic() {
        // Port 1 on loopback: nothing listens there, and connecting fails fast.
        let err = Upstream::new("http://127.0.0.1:1")
            .unwrap()
            .forward(Method::POST, "/v1/messages", &HeaderMap::new(), Vec::new())
            .await
            .expect_err("should not have connected");

        assert_eq!(err.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn a_gateway_error_never_reports_500() {
        // 502 says the dependency failed; 500 says this process has a bug. Whoever is
        // paged reads them differently, and conflating them sends them to the wrong
        // codebase.
        for err in [
            RelayError::Transport("boom".into()),
            RelayError::InvalidUrl("nope".into()),
        ] {
            assert_eq!(err.status(), StatusCode::BAD_GATEWAY);
            assert_ne!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        }
    }

    #[tokio::test]
    async fn an_error_message_never_carries_the_credential() {
        // Error strings get logged, aggregated, and pasted into tickets. A provider
        // key that reaches one has escaped every place it was being guarded.
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("sk-ant-api03-secret"));

        let err = Upstream::new("http://127.0.0.1:1")
            .unwrap()
            .forward(Method::POST, "/v1/messages", &headers, Vec::new())
            .await
            .expect_err("should not have connected");

        assert!(
            !err.to_string().contains("sk-ant-api03-secret"),
            "credential leaked into an error: {err}"
        );
    }

    #[tokio::test]
    async fn upstream_framing_headers_are_not_returned_to_the_client() {
        // The server returning this response to the client frames the body itself.
        // Upstream's `content-length` carried alongside that new framing is how a
        // response arrives truncated at exactly the length the *old* header claimed.
        //
        // The upstream body is sent normally and hyper sets `content-length` for it —
        // deliberately, rather than injecting a wrong one by hand, which hyper rejects
        // at the source anyway. What matters is that a valid header on the way in does
        // not survive to the way out.
        let router = Router::new().route(
            "/v1/messages",
            post(|| async { "a body whose length hyper will announce" }),
        );
        let base = serve(router).await;

        let response = Upstream::new(&base)
            .unwrap()
            .forward(Method::POST, "/v1/messages", &HeaderMap::new(), Vec::new())
            .await
            .unwrap();

        assert!(
            response.headers().get("content-length").is_none(),
            "upstream framing leaked into the client-bound headers"
        );
        assert!(response.headers().get("transfer-encoding").is_none());
        // Not an over-correction: the rest of the response headers are intact.
        assert!(response.headers().get("content-type").is_some());
    }

    #[tokio::test]
    async fn a_provider_response_header_is_preserved() {
        // Rate-limit headers are how a client paces itself. Dropping them while
        // dropping the framing headers would be an easy over-correction.
        let router = Router::new().route(
            "/v1/messages",
            post(|| async { ([("anthropic-ratelimit-requests-remaining", "42")], "{}") }),
        );
        let base = serve(router).await;

        let response = Upstream::new(&base)
            .unwrap()
            .forward(Method::POST, "/v1/messages", &HeaderMap::new(), Vec::new())
            .await
            .unwrap();

        assert_eq!(
            response
                .headers()
                .get("anthropic-ratelimit-requests-remaining")
                .unwrap(),
            "42"
        );
    }

    #[tokio::test]
    async fn a_streaming_response_arrives_before_the_stream_closes() {
        // The guarantee that distinguishes relaying from buffering. If this body were
        // buffered, `next()` would not resolve until the handler finished — which for
        // a real generation is the whole point of the feature, lost.
        use futures_core::Stream as _;
        use std::pin::pin;
        use std::task::{Context, Poll};

        let router = Router::new().route(
            "/v1/messages",
            any(|| async {
                let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
                tokio::spawn(async move {
                    let _ = tx.send(Ok(Bytes::from_static(b"event: first\n\n"))).await;
                    // Held open: a buffering relay cannot deliver the first frame
                    // until this future completes.
                    tokio::time::sleep(Duration::from_secs(30)).await;
                });
                axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx))
            }),
        );
        let base = serve(router).await;

        let response = Upstream::new(&base)
            .unwrap()
            .forward(Method::POST, "/v1/messages", &HeaderMap::new(), Vec::new())
            .await
            .unwrap();

        let mut stream = pin!(response.into_stream());
        let first = tokio::time::timeout(Duration::from_secs(5), async {
            std::future::poll_fn(|cx: &mut Context<'_>| match stream.as_mut().poll_next(cx) {
                Poll::Ready(item) => Poll::Ready(item),
                Poll::Pending => Poll::Pending,
            })
            .await
        })
        .await
        .expect("first frame did not arrive; the body was buffered");

        assert_eq!(first.unwrap().unwrap().as_ref(), b"event: first\n\n");
    }

    #[tokio::test]
    async fn a_trailing_slash_on_the_base_does_not_double_the_path_separator() {
        let upstream = Upstream::new("http://example.com/").unwrap();
        assert_eq!(upstream.base, "http://example.com");
    }

    #[test]
    fn the_per_hop_list_is_what_the_relay_actually_strips() {
        for name in ["host", "content-length"] {
            assert!(is_rebuilt_per_hop(&HeaderName::from_static(name)));
        }
        assert!(!is_rebuilt_per_hop(&HeaderName::from_static("x-api-key")));
    }
}
