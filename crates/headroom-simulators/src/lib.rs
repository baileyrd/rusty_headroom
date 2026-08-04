//! # headroom-simulators
//!
//! Fake Anthropic and OpenAI endpoints so end-to-end tests can exercise the proxy
//! without network access, credentials, or spend.
//!
//! # Why a real server rather than a mock
//!
//! The proxy's central claim is about *bytes crossing a network boundary*: what the
//! client sent is what the provider receives. A mock that stands in for the transport
//! asserts everything except the thing under test — it cannot show that hyper's
//! framing, the relay's header handling, and the chunked encoding all preserve the
//! payload, because none of them ran.
//!
//! So these simulators bind a real loopback socket and record the exact bytes that
//! arrived. A test then compares hashes.
//!
//! # Fixtures
//!
//! [`fixtures`] holds the SSE corner cases the parser has to survive: a UTF-8 sequence
//! split mid-codepoint, CRLF terminators, keep-alive comments, every Anthropic delta
//! type, the OpenAI `[DONE]` sentinel, and a stream that ends in a provider error.
//! They live here rather than inline in one test module so the proxy, the CLI, and any
//! future consumer assert against the same bytes.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{any, post};
use axum::Router;

pub mod fixtures;

/// One request a simulator received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    /// The path it arrived on.
    pub path: String,
    /// The body, byte for byte.
    pub body: Vec<u8>,
    /// The headers, as received.
    pub headers: HeaderMap,
}

impl Recorded {
    /// The body as a UTF-8 string, lossily.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// A named header, if present and UTF-8.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

/// Everything a running simulator has seen.
#[derive(Debug, Clone, Default)]
pub struct Recorder {
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl Recorder {
    /// Creates an empty recorder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every request received, in order.
    ///
    /// A poisoned lock yields an empty list rather than panicking. A simulator whose
    /// recorder panics turns an assertion failure in one test into a confusing failure
    /// in the next.
    pub fn requests(&self) -> Vec<Recorded> {
        self.requests
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// The most recent request, if any.
    pub fn last(&self) -> Option<Recorded> {
        self.requests().pop()
    }

    /// How many requests were received.
    pub fn count(&self) -> usize {
        self.requests().len()
    }

    fn record(&self, entry: Recorded) {
        if let Ok(mut guard) = self.requests.lock() {
            guard.push(entry);
        }
    }
}

/// What a simulator answers with.
#[derive(Debug, Clone)]
pub struct Reply {
    /// Status code.
    pub status: StatusCode,
    /// Body bytes.
    pub body: Vec<u8>,
    /// `content-type` to set.
    pub content_type: &'static str,
    /// A pause inserted partway through the body, if any.
    ///
    /// Models the shape a real long generation has: bytes, a think, more bytes. A
    /// provider that answered instantly can never exercise the path where something
    /// between here and the client gives up waiting.
    pub stall: Option<(usize, std::time::Duration)>,
}

impl Reply {
    /// A JSON reply.
    pub fn json(body: impl Into<String>) -> Self {
        Self {
            status: StatusCode::OK,
            body: body.into().into_bytes(),
            content_type: "application/json",
            stall: None,
        }
    }

    /// A server-sent-event stream.
    pub fn sse(body: impl Into<String>) -> Self {
        Self {
            status: StatusCode::OK,
            body: body.into().into_bytes(),
            content_type: "text/event-stream",
            stall: None,
        }
    }

    /// Pauses for `pause` after the first `after` bytes of the body.
    ///
    /// The reply still completes; only its timing changes. That distinction is the point —
    /// the failure being modelled is something giving up on a stream that was always
    /// going to finish.
    pub fn stalling(mut self, after: usize, pause: std::time::Duration) -> Self {
        self.stall = Some((after, pause));
        self
    }

    /// An error reply in the provider's own shape.
    ///
    /// Provider-shaped rather than plain text, because a client is a provider SDK: an
    /// error in any other shape reaches the user as a parse failure rather than as the
    /// outage it is.
    pub fn error(status: StatusCode, message: &str) -> Self {
        Self {
            status,
            body: serde_json::json!({
                "type": "error",
                "error": { "type": "api_error", "message": message },
            })
            .to_string()
            .into_bytes(),
            content_type: "application/json",
            stall: None,
        }
    }
}

impl Default for Reply {
    fn default() -> Self {
        Self::json(r#"{"id":"msg_sim","type":"message"}"#)
    }
}

/// A running fake provider.
#[derive(Debug)]
pub struct Simulator {
    base: String,
    recorder: Recorder,
}

impl Simulator {
    /// Starts a simulator on an ephemeral loopback port, answering every request with
    /// `reply`.
    ///
    /// Binds port 0 rather than a fixed port so tests can run in parallel — a fixed
    /// port turns concurrent tests into an intermittent bind failure that looks like a
    /// flake in whichever test lost the race.
    ///
    /// # Errors
    ///
    /// Returns an error if the loopback socket cannot be bound.
    pub async fn start(reply: Reply) -> std::io::Result<Self> {
        let recorder = Recorder::new();
        let sink = recorder.clone();

        let app = Router::new().fallback(any(
            move |uri: axum::http::Uri, headers: HeaderMap, body: Bytes| {
                let sink = sink.clone();
                let reply = reply.clone();
                async move {
                    sink.record(Recorded {
                        path: uri.path().to_owned(),
                        body: body.to_vec(),
                        headers,
                    });
                    respond(reply)
                }
            },
        ));

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Ok(Self {
            base: format!("http://{addr}"),
            recorder,
        })
    }

    /// Starts a simulator that answers `/v1/messages` and records everything else.
    ///
    /// # Errors
    ///
    /// Returns an error if the loopback socket cannot be bound.
    pub async fn anthropic() -> std::io::Result<Self> {
        Self::start(Reply::default()).await
    }

    /// Starts a simulator answering with an OpenAI-shaped completion.
    ///
    /// # Errors
    ///
    /// Returns an error if the loopback socket cannot be bound.
    pub async fn openai() -> std::io::Result<Self> {
        Self::start(Reply::json(
            r#"{"id":"chatcmpl-sim","object":"chat.completion","choices":[]}"#,
        ))
        .await
    }

    /// The base URL to point a proxy at.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// What this simulator has recorded.
    pub fn recorder(&self) -> &Recorder {
        &self.recorder
    }
}

/// Turns a [`Reply`] into a response, honouring its stall.
///
/// Shared by both handlers deliberately. They each built their own response once, and a
/// capability added to one silently did nothing in the other — which is how
/// `Reply::stalling` would have appeared to work in `strict_router` while every test
/// using `Simulator::start` got an instant answer.
fn respond(reply: Reply) -> axum::response::Response {
    use axum::response::IntoResponse;

    let Some((after, pause)) = reply.stall else {
        return (
            reply.status,
            [("content-type", reply.content_type)],
            axum::body::Body::from(reply.body),
        )
            .into_response();
    };

    // Streamed in two chunks with a real pause between them, rather than returned whole
    // after a sleep. Sleeping first and answering afterwards tests nothing: the client
    // sees one fast response that happened to arrive late, which is not the shape of a
    // long generation on the wire.
    let split = after.min(reply.body.len());
    let (head, tail) = reply.body.split_at(split);
    let (head, tail) = (head.to_vec(), tail.to_vec());

    use futures_util::StreamExt;
    let stream =
        futures_util::stream::once(async move { Ok::<_, std::io::Error>(Bytes::from(head)) })
            .chain(futures_util::stream::once(async move {
                tokio::time::sleep(pause).await;
                Ok::<_, std::io::Error>(Bytes::from(tail))
            }));

    (
        reply.status,
        [("content-type", reply.content_type)],
        axum::body::Body::from_stream(stream),
    )
        .into_response()
}

/// Builds a router that answers a specific path and 404s the rest.
///
/// For tests that need to prove the proxy routed to the *right* path, where a catch-all
/// fallback would hide a wrong one.
///
/// # Nothing calls this
///
/// Measured: no caller outside this crate, and none inside it either. The need is real —
/// a `fake_provider` registering only `/v1/messages` once made a `/v1/chat/completions`
/// test fail against the *upstream's* 404 and read as a proxy bug — but the proxy's route
/// tests solve it a different way, capturing the path a catch-all received and asserting
/// it, which catches a wrong path just as surely.
///
/// Kept rather than deleted, and said out loud rather than left to a reader to discover:
/// it is a public surface, and the day a test needs a *server* that refuses the wrong path
/// rather than an assertion that notices afterwards, this is that server. Those tests also
/// build their own routers because they wire in `AppState`, which this cannot do from
/// here — closing that gap is what would make this worth using rather than worth
/// documenting.
pub fn strict_router(path: &'static str, reply: Reply, recorder: Recorder) -> Router {
    Router::new().route(
        path,
        post(move |headers: HeaderMap, body: Bytes| {
            let recorder = recorder.clone();
            let reply = reply.clone();
            async move {
                recorder.record(Recorded {
                    path: path.to_owned(),
                    body: body.to_vec(),
                    headers,
                });
                respond(reply)
            }
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn post(base: &str, path: &str, body: &str) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{base}{path}"))
            .header("x-api-key", "sk-ant-api03-test")
            .body(body.to_owned())
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_request_is_recorded_byte_for_byte() {
        // The property the simulator exists to provide. Anything less exact and it
        // cannot be used to assert invariant I1.
        let simulator = Simulator::anthropic().await.unwrap();
        let body = r#"{"a":1.0,"b":"日本語","c":9007199254740993}"#;

        post(simulator.base_url(), "/v1/messages", body).await;

        let recorded = simulator.recorder().last().unwrap();
        assert_eq!(recorded.body, body.as_bytes());
        assert_eq!(recorded.path, "/v1/messages");
    }

    #[tokio::test]
    async fn headers_including_the_credential_are_recorded() {
        let simulator = Simulator::anthropic().await.unwrap();
        post(simulator.base_url(), "/v1/messages", "{}").await;

        assert_eq!(
            simulator.recorder().last().unwrap().header("x-api-key"),
            Some("sk-ant-api03-test")
        );
    }

    #[tokio::test]
    async fn requests_are_recorded_in_order() {
        let simulator = Simulator::anthropic().await.unwrap();
        for index in 0..3 {
            post(simulator.base_url(), "/v1/messages", &format!("{index}")).await;
        }

        let requests = simulator.recorder().requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].text(), "0");
        assert_eq!(requests[2].text(), "2");
    }

    #[tokio::test]
    async fn two_simulators_do_not_collide_on_a_port() {
        // Port 0 rather than a fixed port. A fixed port turns concurrent tests into an
        // intermittent bind failure that looks like a flake in whichever lost the race.
        let first = Simulator::anthropic().await.unwrap();
        let second = Simulator::anthropic().await.unwrap();
        assert_ne!(first.base_url(), second.base_url());
    }

    #[tokio::test]
    async fn an_error_reply_uses_the_provider_shape() {
        let simulator = Simulator::start(Reply::error(StatusCode::TOO_MANY_REQUESTS, "slow down"))
            .await
            .unwrap();

        let response = post(simulator.base_url(), "/v1/messages", "{}").await;
        assert_eq!(response.status(), 429);

        // Parsed by hand rather than through `Response::json`, since reqwest's `json`
        // feature is off — it pulls a serde_json configured independently of this
        // workspace's, which is exactly the second parser invariant I1 avoids.
        let body: serde_json::Value =
            serde_json::from_str(&response.text().await.unwrap()).unwrap();
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["message"], "slow down");
    }

    #[tokio::test]
    async fn an_sse_reply_is_served_as_an_event_stream() {
        let simulator = Simulator::start(Reply::sse(fixtures::ANTHROPIC_COMPLETE))
            .await
            .unwrap();

        let response = post(simulator.base_url(), "/v1/messages", "{}").await;
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
        assert_eq!(response.text().await.unwrap(), fixtures::ANTHROPIC_COMPLETE);
    }

    #[tokio::test]
    async fn a_recorder_with_nothing_recorded_answers_empty_rather_than_panicking() {
        let recorder = Recorder::new();
        assert_eq!(recorder.count(), 0);
        assert_eq!(recorder.last(), None);
    }
}
