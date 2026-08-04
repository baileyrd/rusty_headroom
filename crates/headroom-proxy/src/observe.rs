//! Watching a relayed response go past.
//!
//! [`ObservingStream`] wraps the byte stream coming back from a provider, parses the
//! SSE frames as they flow, and records what they say. Every byte it yields is the
//! byte it received — invariant I9: telemetry observes, it never alters.
//!
//! # Why this is a stream wrapper rather than a callback after the fact
//!
//! The numbers that matter arrive at opposite ends of the response. `message_start`
//! carries the cache usage — the figure that says whether this proxy is helping or
//! quietly making every request more expensive — and it is the very first frame.
//! `message_delta` carries the output-token count and arrives near the last. Waiting
//! for the response to finish before reading either would mean buffering it, which is
//! the one thing the relay exists not to do.
//!
//! Which end depends on the dialect, and neither end may be assumed: OpenAI puts its
//! cache figures in the *final* chunk, Anthropic in the first. That is a reason to read
//! every frame as it passes rather than to reach for whichever one seems to matter.
//!
//! # A cancelled stream still counts
//!
//! When a client disconnects mid-generation the stream is dropped, not exhausted, so
//! `poll_next` never returns `None`. Recording only on clean termination would
//! silently drop the telemetry for every cancelled request — and cancellation is
//! routine for an interactive agent, not an edge case. So the recording also happens
//! on `Drop`, guarded by a flag so a stream that ended cleanly is not counted twice.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_core::Stream;

use crate::metrics::Metrics;
use crate::sse::{Observer, SseParser};

/// A relayed byte stream that reports on itself as it passes.
pub struct ObservingStream<S> {
    /// Boxed so this type is unconditionally `Unpin`, which keeps `poll_next` free of
    /// the unsafe projection this crate forbids. One allocation per response, against
    /// a network round trip.
    inner: Pin<Box<S>>,
    parser: SseParser,
    /// Chosen from the request path, because the three proxied surfaces frame their
    /// streams differently and the wrong classifier reports a healthy stream as
    /// unfinished and unrecognizable rather than failing. See [`Observer`].
    observer: Observer,
    metrics: Arc<Metrics>,
    recorded: bool,
    /// Accumulated body, for a reply that is not an event stream.
    ///
    /// `None` for a stream, which must never be held. A non-streaming reply is a single
    /// object the client will buffer anyway, so accumulating a bounded copy costs nothing
    /// it was not already going to pay — and it is the only way to read usage that
    /// arrives in a body rather than a frame.
    buffered: Option<Vec<u8>>,
}

/// Most a non-streaming reply may accumulate before its usage is given up on.
///
/// A provider reply is kilobytes. This is a backstop against a body that is not what it
/// claimed to be, not a limit anything real should meet: past it the bytes still pass
/// through untouched and only the telemetry is lost.
const MAX_BUFFERED_BODY: usize = 4 * 1024 * 1024;

impl<S> ObservingStream<S> {
    /// Wraps `inner`, reporting into `metrics`, reading the stream as `path`'s
    /// vocabulary.
    pub fn new(inner: S, metrics: Arc<Metrics>, path: &str) -> Self {
        Self::with_framing(inner, metrics, path, true)
    }

    /// The same, told whether the reply is an event stream.
    ///
    /// `event_stream` false makes this accumulate the body so usage can be read from it.
    /// Nothing did that, so every non-streaming client reported no cache data at all —
    /// the metric this proxy exists to move, blank for batch work, evaluation harnesses,
    /// and any SDK call without `stream=True`.
    pub fn with_framing(inner: S, metrics: Arc<Metrics>, path: &str, event_stream: bool) -> Self {
        Self {
            inner: Box::pin(inner),
            parser: SseParser::new(),
            observer: Observer::for_path(path),
            metrics,
            recorded: false,
            buffered: (!event_stream).then(Vec::new),
        }
    }

    /// Writes what was observed into the metrics, at most once.
    fn record(&mut self) {
        if self.recorded {
            return;
        }
        self.recorded = true;

        // A stream reports through its frames; a body reports through itself. Whichever
        // this reply was, the numbers land in the same counters.
        let (read, creation) = match &self.buffered {
            Some(body) => self.observer.cache_tokens_in_body(body),
            None => self.observer.cache_tokens(),
        };
        self.metrics.record_cache_usage(read, creation);

        // A stream that ended in a provider error did not succeed however many events
        // it produced. Counting it as a success is how a real failure rate stays
        // invisible.
        if self.observer.failure().is_some() {
            self.metrics.record_stream_error();
        }

        if !self.observer.unknown_types().is_empty() {
            // Not an error — the provider is allowed to add event types. But it should
            // surface as a line in a log rather than as silence, because silence is
            // indistinguishable from handling it.
            tracing::info!(
                dialect = self.observer.dialect(),
                unknown_event_types = ?self.observer.unknown_types(),
                "provider sent stream event types this build does not model"
            );
        }
    }
}

impl<S, E> Stream for ObservingStream<S>
where
    S: Stream<Item = Result<Bytes, E>>,
{
    type Item = Result<Bytes, E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                // Observation happens on a clone of the handle, not on the bytes. The
                // chunk yielded below is the one that arrived — this function has no
                // path that can alter it.
                match &mut this.buffered {
                    // Bounded. Past the cap the bytes still pass through untouched and
                    // only this reply's telemetry is given up on.
                    Some(body) if body.len() + bytes.len() <= MAX_BUFFERED_BODY => {
                        body.extend_from_slice(&bytes);
                    }
                    Some(_) => {}
                    None => {
                        for event in this.parser.feed(&bytes) {
                            this.observer.observe(&event);
                        }
                    }
                }
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(err))) => {
                // A transport failure mid-stream is a stream that did not complete,
                // whatever the provider had said up to that point.
                this.metrics.record_stream_error();
                this.record();
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(None) => {
                this.record();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S> Drop for ObservingStream<S> {
    fn drop(&mut self) {
        // The cancelled-mid-generation case. Routine for an interactive agent.
        self.record();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::poll_fn;
    use std::pin::pin;

    /// The parts of a realistic Anthropic stream that carry numbers.
    const STREAM: &str = concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"usage":{"input_tokens":10,"#,
        r#""cache_read_input_tokens":900,"cache_creation_input_tokens":100}}}"#,
        "\n\n",
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        "\n\n",
        "event: message_delta\n",
        r#"data: {"type":"message_delta","usage":{"output_tokens":42}}"#,
        "\n\n",
        "event: message_stop\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n",
    );

    /// A stream that yields `chunks` in order and then ends.
    fn from_chunks(chunks: Vec<Vec<u8>>) -> impl Stream<Item = Result<Bytes, std::io::Error>> {
        let mut remaining = chunks.into_iter();
        futures_util_stub::poll_fn_stream(move || remaining.next().map(|c| Ok(Bytes::from(c))))
    }

    /// A minimal `Stream` built from a closure, so these tests need no extra crate.
    mod futures_util_stub {
        use super::*;

        pub struct PollFnStream<F>(F);

        pub fn poll_fn_stream<F, T>(f: F) -> PollFnStream<F>
        where
            F: FnMut() -> Option<T>,
        {
            PollFnStream(f)
        }

        impl<F, T> Stream for PollFnStream<F>
        where
            F: FnMut() -> Option<T> + Unpin,
        {
            type Item = T;

            fn poll_next(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                Poll::Ready((self.0)())
            }
        }
    }

    /// Drains `stream`, returning everything it yielded concatenated.
    async fn drain(stream: impl Stream<Item = Result<Bytes, std::io::Error>>) -> Vec<u8> {
        let mut stream = pin!(stream);
        let mut out = Vec::new();
        while let Some(item) = poll_fn(|cx: &mut Context<'_>| stream.as_mut().poll_next(cx)).await {
            out.extend_from_slice(&item.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn every_byte_passes_through_unaltered() {
        // Invariant I9. Observation must be observation.
        let metrics = Arc::new(Metrics::new());
        let source = STREAM.as_bytes().to_vec();

        let out = drain(ObservingStream::new(
            from_chunks(vec![source.clone()]),
            metrics,
            "/v1/messages",
        ))
        .await;

        assert_eq!(out, source);
    }

    #[tokio::test]
    async fn cache_usage_is_recorded_from_the_first_frame() {
        let metrics = Arc::new(Metrics::new());
        drain(ObservingStream::new(
            from_chunks(vec![STREAM.as_bytes().to_vec()]),
            metrics.clone(),
            "/v1/messages",
        ))
        .await;

        // 900 read against 100 created: the proxy is preserving the prefix.
        assert_eq!(metrics.cache_hit_rate(), Some(0.9));
    }

    #[tokio::test]
    async fn usage_is_found_however_the_network_split_the_stream() {
        // The bug this guards: `message_start` is one frame, but nothing says it
        // arrives as one chunk. A parser that only sees whole chunks reports a
        // permanently empty cache metric, which reads as "no traffic" rather than as a
        // defect.
        let source = STREAM.as_bytes().to_vec();

        for split in 1..source.len() {
            let metrics = Arc::new(Metrics::new());
            drain(ObservingStream::new(
                from_chunks(vec![source[..split].to_vec(), source[split..].to_vec()]),
                metrics.clone(),
                "/v1/messages",
            ))
            .await;

            assert_eq!(
                metrics.cache_hit_rate(),
                Some(0.9),
                "usage lost when the stream split at byte {split}"
            );
        }
    }

    #[tokio::test]
    async fn a_provider_error_mid_stream_is_counted() {
        let metrics = Arc::new(Metrics::new());
        let stream = concat!(
            "event: message_start\n",
            r#"data: {"type":"message_start","message":{"usage":{}}}"#,
            "\n\n",
            "event: error\n",
            r#"data: {"type":"error","error":{"message":"overloaded"}}"#,
            "\n\n",
        );

        drain(ObservingStream::new(
            from_chunks(vec![stream.as_bytes().to_vec()]),
            metrics.clone(),
            "/v1/messages",
        ))
        .await;

        assert!(
            metrics.render().contains("headroom_stream_errors_total 1"),
            "{}",
            metrics.render()
        );
    }

    #[tokio::test]
    async fn a_stream_dropped_mid_flight_still_reports_what_it_saw() {
        // A client that cancels mid-generation has still spent the cache tokens the
        // provider reported. Recording only on clean termination would drop the
        // telemetry for every cancelled request, and cancellation is routine.
        let metrics = Arc::new(Metrics::new());

        {
            let stream = ObservingStream::new(
                from_chunks(vec![STREAM.as_bytes().to_vec()]),
                metrics.clone(),
                "/v1/messages",
            );
            let mut stream = pin!(stream);
            // Exactly one chunk, then walk away.
            let _ = poll_fn(|cx: &mut Context<'_>| stream.as_mut().poll_next(cx)).await;
        }

        assert_eq!(metrics.cache_hit_rate(), Some(0.9));
    }

    #[tokio::test]
    async fn a_completed_stream_is_not_counted_twice_by_the_drop() {
        // `record` runs on clean termination and again on drop; the flag is what keeps
        // every cache figure from being doubled.
        let metrics = Arc::new(Metrics::new());
        drain(ObservingStream::new(
            from_chunks(vec![STREAM.as_bytes().to_vec()]),
            metrics.clone(),
            "/v1/messages",
        ))
        .await;

        let rendered = metrics.render();
        assert!(
            rendered.contains("headroom_cache_read_tokens_total 900"),
            "{rendered}"
        );
        assert!(
            rendered.contains("headroom_cache_creation_tokens_total 100"),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn a_non_sse_body_passes_through_without_recording_anything() {
        // The common non-streaming case: a single JSON object. It parses as no events,
        // which must be uneventful rather than wrong.
        let metrics = Arc::new(Metrics::new());
        let body = br#"{"id":"msg_1","content":[{"type":"text","text":"hi"}]}"#.to_vec();

        let out = drain(ObservingStream::new(
            from_chunks(vec![body.clone()]),
            metrics.clone(),
            "/v1/messages",
        ))
        .await;

        assert_eq!(out, body);
        assert_eq!(
            metrics.cache_hit_rate(),
            None,
            "a non-SSE body invented cache data"
        );
    }
}
