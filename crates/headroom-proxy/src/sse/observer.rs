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
//! - **Cache usage is Anthropic-only.** Neither OpenAI surface reports cache hits in its
//!   stream. Reporting zero for them is the truth; synthesizing a number would corrupt
//!   the one metric this proxy exists to move.
//! - **OpenAI chat has no unknown-type list.** Its chunks are not tagged with an event
//!   name to collect, so the list is empty rather than fabricated.

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
    /// Zero for both OpenAI surfaces because neither reports cache usage in its stream —
    /// the honest answer, not a gap. Anything else would be a number this proxy made up
    /// about the metric it exists to move.
    pub fn cache_tokens(&self) -> (u64, u64) {
        match self {
            Self::Anthropic(observer) => {
                (observer.cache_read_tokens, observer.cache_creation_tokens)
            }
            Self::Chat(_) | Self::Responses(_) => (0, 0),
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

    #[test]
    fn cache_usage_is_reported_only_where_the_provider_sends_it() {
        let anthropic = observe(
            "/v1/messages",
            concat!(
                "event: message_start\n",
                r#"data: {"type":"message_start","message":{"usage":"#,
                r#"{"cache_read_input_tokens":900,"cache_creation_input_tokens":100}}}"#,
                "\n\n",
            ),
        );
        assert_eq!(anthropic.cache_tokens(), (900, 100));

        // Neither OpenAI surface reports cache usage in its stream. Zero is the truth;
        // a synthesized number would corrupt the one metric this proxy exists to move.
        let chat = observe("/v1/chat/completions", "data: [DONE]\n\n");
        assert_eq!(chat.cache_tokens(), (0, 0));
    }

    #[test]
    fn openai_chat_reports_no_unknown_types_rather_than_a_wrong_list() {
        // Its chunks carry no event name to collect. An empty list is the accurate
        // answer, not a list that failed to populate.
        let observer = observe("/v1/chat/completions", "data: {\"nonsense\":true}\n\n");
        assert!(observer.unknown_types().is_empty());
    }
}
