//! Picking the right stream vocabulary for a response — gap row X12.
//!
//! # Why the relay cannot just use one observer
//!
//! Three surfaces are proxied and each frames its stream differently. Anthropic sends
//! `message_start` / `content_block_delta` / `message_stop`; OpenAI chat sends
//! `chat.completion.chunk` objects and terminates with a literal `data: [DONE]`; the
//! Responses API sends `response.output_text.delta` and friends.
//!
//! Reading one with another's classifier does not fail loudly. Every frame simply lands
//! in the "types this build does not model" bucket: no error is ever recorded for a
//! failed stream, no completion is ever recorded for a successful one, and the log line
//! that exists to report a *new* provider event fires on completely ordinary traffic.
//! Telemetry that is confidently wrong is worse than telemetry that is missing, because
//! nothing about it looks broken.
//!
//! # What is common, and what is genuinely not
//!
//! [`Observer`] exposes the facts every vocabulary can answer — did it finish, did it
//! fail, what did it say that this build does not model. It deliberately does **not**
//! invent the rest:
//!
//! - **Cache usage is common, but spelled three ways.** Every dialect reports it; each
//!   uses different field names, in a different frame. This module's job is to answer
//!   the question once, not to decide the answer is unavailable — see
//!   [`Observer::cache_tokens`], which for a long time returned a hardcoded zero for
//!   both OpenAI surfaces under a comment claiming neither provider reported the number
//!   at all.
//! - **OpenAI chat has no unknown-type list.** Its chunks are not tagged with an event
//!   name to collect, so the list is empty rather than fabricated. Unlike the above,
//!   this one is genuinely absent from the wire rather than merely unparsed.

use super::{Event, OpenAiObserver, ResponsesObserver, StreamObserver};

/// The stream vocabulary a response is written in.
#[derive(Debug, Clone)]
pub enum Observer {
    /// Anthropic Messages streaming.
    Anthropic(StreamObserver),
    /// OpenAI chat-completion streaming.
    Chat(OpenAiObserver),
    /// OpenAI Responses streaming.
    Responses(ResponsesObserver),
}

impl Observer {
    /// The observer for a response to `path`.
    ///
    /// # Why an unrecognized path still gets an observer
    ///
    /// The remaining proxied paths — `/v1/conversations`, `/v1/responses/compact` —
    /// answer with plain JSON rather than an event stream. A JSON body contains no
    /// `data:` frames, so the parser yields nothing and every variant records nothing.
    /// The choice is immaterial for them, which is why this returns a default rather
    /// than growing a fourth variant that would only ever observe silence.
    pub fn for_path(path: &str) -> Self {
        // `/v1/responses` is tested before `/v1/chat/completions` only for readability;
        // the prefixes are disjoint. `/v1/responses/compact` matching here is correct
        // and harmless — see above.
        if path.starts_with("/v1/responses") {
            Self::Responses(ResponsesObserver::default())
        } else if path.starts_with("/v1/chat/completions") {
            Self::Chat(OpenAiObserver::default())
        } else {
            Self::Anthropic(StreamObserver::default())
        }
    }

    /// Records one event.
    pub fn observe(&mut self, event: &Event) {
        match self {
            Self::Anthropic(observer) => observer.observe(event),
            Self::Chat(observer) => observer.observe(event),
            Self::Responses(observer) => observer.observe(event),
        }
    }

    /// Cache tokens the stream reported, as `(read, creation)`.
    ///
    /// All three dialects report this, in three vocabularies: Anthropic as
    /// `cache_read_input_tokens` / `cache_creation_input_tokens` on `message_start`,
    /// chat completions as `cached_tokens` / `cache_write_tokens` under
    /// `usage.prompt_tokens_details`, Responses as the same pair under
    /// `usage.input_tokens_details`.
    ///
    /// A zero here means the provider reported zero *or* reported nothing, and the two
    /// are not separable downstream. Two cases produce the second: chat completions only
    /// send their usage chunk when the client sets `stream_options.include_usage`, and
    /// `cache_write_tokens` exists only on the model families that bill for cache
    /// writes. Neither is something a proxy can make the client or the model do, and
    /// substituting a guess for either would corrupt the one number this exists to move.
    pub fn cache_tokens(&self) -> (u64, u64) {
        match self {
            Self::Anthropic(observer) => {
                (observer.cache_read_tokens, observer.cache_creation_tokens)
            }
            Self::Chat(observer) => (observer.cache_read_tokens, observer.cache_creation_tokens),
            Self::Responses(observer) => {
                (observer.cache_read_tokens, observer.cache_creation_tokens)
            }
        }
    }

    /// Cache tokens reported by a **non-streaming** reply, in this dialect's vocabulary.
    ///
    /// The streaming path reads frames; a reply that is not an event stream carries the
    /// same numbers in its body and nothing was reading them. Measured against one
    /// stand-in returning identical usage both ways: streaming recorded 900 reads and 100
    /// writes, non-streaming recorded nothing — same provider, same numbers, same proxy,
    /// only the framing differed.
    ///
    /// Each dialect reuses the reader its own classifier uses, so the field names are
    /// written once per dialect rather than once per framing.
    pub fn cache_tokens_in_body(&self, body: &[u8]) -> (u64, u64) {
        match self {
            Self::Anthropic(_) => super::anthropic::cache_tokens_in_body(body),
            Self::Chat(_) => super::openai::cache_tokens_in_body(body),
            Self::Responses(_) => super::responses::cache_tokens_in_body(body),
        }
    }

    /// What the provider said went wrong, if anything.
    ///
    /// The Responses vocabulary reports this as a terminal *reason* rather than an
    /// error object, so `response.failed` and `response.incomplete` both surface here.
    /// Both are streams that did not succeed, which is what the caller is asking.
    pub fn failure(&self) -> Option<&str> {
        match self {
            Self::Anthropic(observer) => observer.error.as_deref(),
            Self::Chat(observer) => observer.error.as_deref(),
            Self::Responses(observer) => observer.failure.as_deref(),
        }
    }

    /// Event types this build does not model.
    ///
    /// Empty for OpenAI chat: its chunks carry no event name to collect, so there is no
    /// list to report rather than an empty one hiding something.
    pub fn unknown_types(&self) -> &[String] {
        match self {
            Self::Anthropic(observer) => &observer.unknown_types,
            Self::Responses(observer) => &observer.unknown_types,
            Self::Chat(_) => &[],
        }
    }

    /// Whether a terminal event arrived.
    pub fn completed(&self) -> bool {
        match self {
            Self::Anthropic(observer) => observer.completed,
            Self::Chat(observer) => observer.completed,
            Self::Responses(observer) => observer.completed,
        }
    }

    /// Events seen, whatever the vocabulary.
    pub fn events(&self) -> usize {
        match self {
            Self::Anthropic(observer) => observer.events,
            Self::Chat(observer) => observer.events,
            Self::Responses(observer) => observer.events,
        }
    }

    /// A short identifier for the vocabulary, for logs.
    pub fn dialect(&self) -> &'static str {
        match self {
            Self::Anthropic(_) => "anthropic",
            Self::Chat(_) => "openai-chat",
            Self::Responses(_) => "openai-responses",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::SseParser;

    /// Feeds `stream` through the observer for `path` and returns it.
    fn observe(path: &str, stream: &str) -> Observer {
        let mut observer = Observer::for_path(path);
        let mut parser = SseParser::new();
        for event in parser.feed(stream.as_bytes()) {
            observer.observe(&event);
        }
        observer
    }

    #[test]
    fn each_path_selects_its_own_vocabulary() {
        assert_eq!(Observer::for_path("/v1/messages").dialect(), "anthropic");
        assert_eq!(
            Observer::for_path("/v1/chat/completions").dialect(),
            "openai-chat"
        );
        assert_eq!(
            Observer::for_path("/v1/responses").dialect(),
            "openai-responses"
        );
    }

    #[test]
    fn an_unrecognized_path_still_gets_an_observer() {
        // Those endpoints answer with plain JSON, which yields no frames — so the choice
        // is immaterial, but it must not be a panic or a `None` the relay has to handle.
        for path in ["/v1/conversations", "/health", ""] {
            assert_eq!(Observer::for_path(path).dialect(), "anthropic", "{path}");
        }
    }

    #[test]
    fn an_openai_stream_read_by_its_own_observer_is_understood() {
        // The bug this row exists to fix: read with the Anthropic classifier, every one
        // of these frames is an unmodelled event and the stream never completes.
        let stream = concat!(
            "data: ",
            r#"{"object":"chat.completion.chunk","choices":[{"delta":{"content":"hi"}}]}"#,
            "\n\n",
            "data: [DONE]\n\n",
        );

        let observer = observe("/v1/chat/completions", stream);
        assert!(observer.completed(), "the [DONE] sentinel was not seen");
        assert_eq!(observer.failure(), None);
        assert_eq!(observer.events(), 2);
    }

    #[test]
    fn a_failed_openai_stream_read_as_anthropic_is_recorded_as_healthy() {
        // The concrete damage the wrong classifier does, and the reason this module
        // exists. An OpenAI error frame is `{"error":{...}}` with no `type`, which the
        // Anthropic classifier files under "something else" — so a stream that failed
        // reports no failure, and the proxy's error rate stays at zero while requests
        // are failing.
        let stream = concat!(
            "data: ",
            r#"{"error":{"message":"upstream capacity"}}"#,
            "\n\n",
        );

        assert_eq!(
            observe("/v1/messages", stream).failure(),
            None,
            "the Anthropic classifier was expected to miss this — if it now catches it, \
             this test is the wrong justification for the module"
        );
        assert_eq!(
            observe("/v1/chat/completions", stream).failure(),
            Some("upstream capacity")
        );
    }

    #[test]
    fn a_responses_stream_read_as_anthropic_never_completes() {
        // Responses streams have no `[DONE]` sentinel and no `message_stop`, so the
        // Anthropic observer sees a stream that never ended — every one of its frames
        // instead piling up as an event type this build does not model.
        let stream = concat!(
            "data: ",
            r#"{"type":"response.completed","response":{"usage":{"output_tokens":12}}}"#,
            "\n\n",
        );

        let wrong = observe("/v1/messages", stream);
        assert!(!wrong.completed());
        assert!(!wrong.unknown_types().is_empty());

        assert!(observe("/v1/responses", stream).completed());
    }

    #[test]
    fn a_responses_stream_reports_its_terminal_reason_as_a_failure() {
        // `response.failed` is a terminal reason rather than an error object, and a
        // caller asking "did this stream succeed" needs it to surface as one.
        let stream = concat!(
            "data: ",
            r#"{"type":"response.failed","response":{"error":{"message":"upstream capacity"}}}"#,
            "\n\n",
        );

        let observer = observe("/v1/responses", stream);
        assert!(observer.completed(), "a failed stream is still over");
        assert!(
            observer.failure().is_some(),
            "a failed stream reported no failure"
        );
    }

    /// One stream per dialect, each carrying 900 cache reads and 100 cache writes in
    /// that dialect's own vocabulary.
    fn streams_reporting_900_reads_and_100_writes() -> [(&'static str, String); 3] {
        [
            (
                "/v1/messages",
                concat!(
                    "event: message_start\n",
                    r#"data: {"type":"message_start","message":{"usage":"#,
                    r#"{"cache_read_input_tokens":900,"cache_creation_input_tokens":100}}}"#,
                    "\n\n",
                )
                .to_owned(),
            ),
            (
                "/v1/chat/completions",
                concat!(
                    r#"data: {"choices":[{"index":0,"delta":{"content":"hi"}}],"usage":null}"#,
                    "\n\n",
                    r#"data: {"choices":[],"usage":{"prompt_tokens":1000,"#,
                    r#""prompt_tokens_details":{"cached_tokens":900,"cache_write_tokens":100}}}"#,
                    "\n\n",
                    "data: [DONE]\n\n",
                )
                .to_owned(),
            ),
            (
                "/v1/responses",
                concat!(
                    r#"data: {"type":"response.completed","response":{"usage":"#,
                    r#"{"input_tokens":1000,"output_tokens":12,"input_tokens_details":"#,
                    r#"{"cached_tokens":900,"cache_write_tokens":100}}}}"#,
                    "\n\n",
                )
                .to_owned(),
            ),
        ]
    }

    #[test]
    fn every_dialect_reports_the_cache_usage_its_provider_sends() {
        // Written as a table over all three rather than as one test per dialect,
        // because the defect it guards is a dialect that gets *skipped*: both OpenAI
        // surfaces returned a hardcoded `(0, 0)` under a comment asserting that neither
        // provider reports cache usage at all. Both do. Every OpenAI conversation read
        // as no-cache-data on the one metric this proxy exists to move, and the whole
        // suite was green — the test here fed a stream carrying no usage in the first
        // place, so it could not tell a parser that was missing from one that was
        // absent.
        //
        // A fourth dialect wired up without cache accounting fails on this line rather
        // than shipping quiet zeros.
        for (path, stream) in streams_reporting_900_reads_and_100_writes() {
            let observer = observe(path, &stream);
            assert_eq!(
                observer.cache_tokens(),
                (900, 100),
                "{path} did not report the cache usage its stream carried"
            );
        }
    }

    #[test]
    fn a_dialect_reports_zero_when_its_stream_carries_no_usage() {
        // The control for the test above. If `cache_tokens` returned 900 regardless —
        // or if the reads were coming from somewhere other than the stream — every
        // assertion up there would pass without the parsers doing anything.
        for (path, stream) in [
            ("/v1/messages", "event: message_stop\ndata: {}\n\n"),
            ("/v1/chat/completions", "data: [DONE]\n\n"),
            ("/v1/responses", "data: {\"type\":\"response.created\"}\n\n"),
        ] {
            assert_eq!(
                observe(path, stream).cache_tokens(),
                (0, 0),
                "{path} reported cache usage its stream never carried"
            );
        }
    }

    #[test]
    fn a_truncated_responses_turn_still_reports_what_it_read_from_cache() {
        // `response.incomplete` is how a turn that hit its output-token limit ends. It
        // read its prefix from the cache and was billed for it all the same, so
        // counting cache usage only on `response.completed` undercounts every
        // truncated request — the common shape for a long agent turn.
        let observer = observe(
            "/v1/responses",
            concat!(
                r#"data: {"type":"response.incomplete","response":{"usage":"#,
                r#"{"input_tokens_details":{"cached_tokens":512}}}}"#,
                "\n\n",
            ),
        );

        assert!(observer.failure().is_some(), "not treated as unsuccessful");
        assert_eq!(observer.cache_tokens(), (512, 0));
    }

    #[test]
    fn openai_chat_reports_no_unknown_types_rather_than_a_wrong_list() {
        // Its chunks carry no event name to collect. An empty list is the accurate
        // answer, not a list that failed to populate.
        let observer = observe("/v1/chat/completions", "data: {\"nonsense\":true}\n\n");
        assert!(observer.unknown_types().is_empty());
    }

    #[test]
    fn every_dialect_reads_cache_usage_from_a_non_streaming_body_too() {
        // Found while building `scripts/live-cache-measurement.py`, whose requests do not
        // stream: the provider reported 4,700 cache reads and the proxy reported none.
        // Isolated against a stand-in returning identical usage both ways — streaming
        // recorded 900/100, non-streaming recorded 0/0. Same provider, same numbers, same
        // proxy; only the framing differed, because usage was only ever read from frames.
        //
        // A table over the dialects for the reason the streaming one is: this is the same
        // metric, and it was already wrong for two surfaces out of three once.
        for (path, body) in [
            (
                "/v1/messages",
                r#"{"usage":{"input_tokens":10,"cache_read_input_tokens":900,
                   "cache_creation_input_tokens":100}}"#,
            ),
            (
                "/v1/chat/completions",
                r#"{"usage":{"prompt_tokens":1000,"prompt_tokens_details":
                   {"cached_tokens":900,"cache_write_tokens":100}}}"#,
            ),
            (
                "/v1/responses",
                r#"{"usage":{"input_tokens":1000,"input_tokens_details":
                   {"cached_tokens":900,"cache_write_tokens":100}}}"#,
            ),
        ] {
            assert_eq!(
                Observer::for_path(path).cache_tokens_in_body(body.as_bytes()),
                (900, 100),
                "{path} did not read the cache usage from a non-streaming body"
            );
        }
    }

    #[test]
    fn a_body_with_no_usage_reports_nothing_rather_than_guessing() {
        // The control. Without it a reader hardwired to 900 would satisfy the table
        // above, and an unparseable body must not become a number either — the proxy
        // reporting a figure it did not receive is worse than reporting none.
        for body in [
            r#"{"id":"msg_1","content":[{"type":"text","text":"ok"}]}"#,
            "not json at all",
            "",
        ] {
            for path in ["/v1/messages", "/v1/chat/completions", "/v1/responses"] {
                assert_eq!(
                    Observer::for_path(path).cache_tokens_in_body(body.as_bytes()),
                    (0, 0),
                    "{path} invented cache usage from {body:?}"
                );
            }
        }
    }
}
