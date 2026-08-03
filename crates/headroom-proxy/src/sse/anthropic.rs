//! Anthropic streaming event types.
//!
//! The proxy does not modify streaming responses — it forwards them byte for byte.
//! What it needs is to *understand* them: to know when a stream ended, whether it
//! ended well, and how many tokens it cost, all without altering a byte.
//!
//! # Why every delta type has to be known
//!
//! The reference records a real defect here: a parser that handled only
//! `text_delta` silently dropped `thinking_delta`, `signature_delta`, and
//! `citations_delta`. Anything unrecognized must therefore be recognized *as*
//! unrecognized and passed through, never quietly discarded — which is why
//! [`AnthropicEvent::Other`] exists rather than a catch-all that drops.

use serde_json::Value;

use super::Event;

/// A classified Anthropic stream event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnthropicEvent {
    /// The response is starting.
    MessageStart,
    /// A content block is opening.
    ContentBlockStart {
        /// Index of the block within the message.
        index: usize,
    },
    /// Incremental content.
    ContentBlockDelta {
        /// Index of the block being appended to.
        index: usize,
        /// Which kind of delta this is.
        delta: DeltaKind,
    },
    /// A content block is closing.
    ContentBlockStop {
        /// Index of the block.
        index: usize,
    },
    /// Top-level message metadata changed, usually carrying usage.
    MessageDelta {
        /// Output tokens reported so far, if present.
        output_tokens: Option<u64>,
    },
    /// The response is complete.
    MessageStop,
    /// A keep-alive.
    Ping,
    /// The provider reported an error mid-stream.
    ///
    /// Distinguished because a stream that ends in an error is not a stream that
    /// completed, and telemetry counting it as success would hide real failures.
    Error {
        /// What the provider said.
        message: String,
    },
    /// Something this code does not recognize.
    ///
    /// Carries the raw type so it can be forwarded and counted rather than dropped.
    Other {
        /// The event's declared type, if it had one.
        event_type: Option<String>,
    },
}

/// Which kind of incremental content a delta carries.
///
/// Every variant the reference lists is named. `Unknown` exists so a delta type added
/// by the provider tomorrow is visibly unhandled rather than silently treated as text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    /// Ordinary text.
    Text,
    /// Extended-thinking content.
    Thinking,
    /// The cryptographic signature over a thinking block.
    Signature,
    /// Streaming tool-call arguments.
    InputJson,
    /// Citation metadata.
    Citations,
    /// A delta type this code does not know.
    Unknown,
}

impl DeltaKind {
    fn from_type(raw: Option<&str>) -> Self {
        match raw {
            Some("text_delta") => Self::Text,
            Some("thinking_delta") => Self::Thinking,
            Some("signature_delta") => Self::Signature,
            Some("input_json_delta") => Self::InputJson,
            Some("citations_delta") => Self::Citations,
            _ => Self::Unknown,
        }
    }

    /// Whether this delta carries content the model authored as prose.
    ///
    /// `false` for signatures and citations, which are metadata. Counting them as text
    /// would inflate any output-length measurement.
    pub fn is_prose(self) -> bool {
        matches!(self, Self::Text | Self::Thinking)
    }
}

/// Classifies a raw SSE event.
///
/// Never fails. An event that cannot be parsed classifies as
/// [`AnthropicEvent::Other`], because the proxy forwards it either way and an error
/// here would be an error about something that is not the proxy's business.
///
/// # Example
///
/// ```
/// use headroom_proxy::sse::{classify, AnthropicEvent, DeltaKind, Event};
///
/// let event = Event {
///     name: Some("content_block_delta".into()),
///     data: r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"..."}}"#.into(),
///     id: None,
/// };
///
/// assert_eq!(
///     classify(&event),
///     AnthropicEvent::ContentBlockDelta { index: 0, delta: DeltaKind::Thinking }
/// );
/// ```
pub fn classify(event: &Event) -> AnthropicEvent {
    let payload: Value = serde_json::from_str(&event.data).unwrap_or(Value::Null);

    // The `event:` field is authoritative when present; some servers only set the
    // `type` inside the payload.
    let event_type = event
        .name
        .as_deref()
        .or_else(|| payload.get("type").and_then(Value::as_str));

    match event_type {
        Some("message_start") => AnthropicEvent::MessageStart,
        Some("content_block_start") => AnthropicEvent::ContentBlockStart {
            index: index_of(&payload),
        },
        Some("content_block_delta") => AnthropicEvent::ContentBlockDelta {
            index: index_of(&payload),
            delta: DeltaKind::from_type(
                payload
                    .get("delta")
                    .and_then(|d| d.get("type"))
                    .and_then(Value::as_str),
            ),
        },
        Some("content_block_stop") => AnthropicEvent::ContentBlockStop {
            index: index_of(&payload),
        },
        Some("message_delta") => AnthropicEvent::MessageDelta {
            output_tokens: payload
                .get("usage")
                .and_then(|u| u.get("output_tokens"))
                .and_then(Value::as_u64),
        },
        Some("message_stop") => AnthropicEvent::MessageStop,
        Some("ping") => AnthropicEvent::Ping,
        Some("error") => AnthropicEvent::Error {
            message: payload
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("unspecified error")
                .to_owned(),
        },
        other => AnthropicEvent::Other {
            event_type: other.map(str::to_owned),
        },
    }
}

fn index_of(payload: &Value) -> usize {
    payload
        .get("index")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .try_into()
        .unwrap_or(0)
}

/// Running observation of one stream.
///
/// Observation only. Nothing here alters a byte of what is forwarded — the point is to
/// know what happened, which is what invariant I9 permits telemetry to do.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StreamObserver {
    /// Events seen.
    pub events: usize,
    /// Prose deltas seen.
    pub text_deltas: usize,
    /// Thinking deltas seen.
    pub thinking_deltas: usize,
    /// Output tokens the provider last reported.
    pub output_tokens: Option<u64>,
    /// Whether a terminal event arrived.
    pub completed: bool,
    /// The provider's error, if the stream failed.
    pub error: Option<String>,
    /// Event types this code did not recognize.
    pub unknown_types: Vec<String>,
}

impl StreamObserver {
    /// Records one event.
    pub fn observe(&mut self, event: &Event) {
        self.events += 1;

        match classify(event) {
            AnthropicEvent::ContentBlockDelta { delta, .. } => {
                if delta == DeltaKind::Text {
                    self.text_deltas += 1;
                } else if delta == DeltaKind::Thinking {
                    self.thinking_deltas += 1;
                }
            }
            AnthropicEvent::MessageDelta { output_tokens } => {
                if output_tokens.is_some() {
                    self.output_tokens = output_tokens;
                }
            }
            AnthropicEvent::MessageStop => self.completed = true,
            AnthropicEvent::Error { message } => self.error = Some(message),
            // Recorded rather than ignored. A provider adding an event type should
            // show up as a number somewhere, not vanish. The guard deduplicates so a
            // long stream of one unknown type does not grow the vec unboundedly.
            AnthropicEvent::Other {
                event_type: Some(event_type),
            } if !self.unknown_types.contains(&event_type) => {
                self.unknown_types.push(event_type);
            }
            _ => {}
        }

        if event.is_done() {
            self.completed = true;
        }
    }

    /// Whether the stream ended successfully.
    ///
    /// A stream that ended in an error did not succeed however many events it
    /// produced, and counting it as success would hide real failures in telemetry.
    pub fn succeeded(&self) -> bool {
        self.completed && self.error.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::SseParser;

    fn event(name: &str, data: &str) -> Event {
        Event {
            name: Some(name.into()),
            data: data.into(),
            id: None,
        }
    }

    #[test]
    fn the_lifecycle_events_classify() {
        assert_eq!(
            classify(&event("message_start", r#"{"type":"message_start"}"#)),
            AnthropicEvent::MessageStart
        );
        assert_eq!(
            classify(&event("message_stop", r#"{"type":"message_stop"}"#)),
            AnthropicEvent::MessageStop
        );
        assert_eq!(classify(&event("ping", "{}")), AnthropicEvent::Ping);
    }

    #[test]
    fn every_documented_delta_type_is_recognized() {
        // The defect this exists to prevent: a parser handling only text_delta
        // silently drops thinking, signature, and citations deltas.
        for (raw, expected) in [
            ("text_delta", DeltaKind::Text),
            ("thinking_delta", DeltaKind::Thinking),
            ("signature_delta", DeltaKind::Signature),
            ("input_json_delta", DeltaKind::InputJson),
            ("citations_delta", DeltaKind::Citations),
        ] {
            let data = format!(r#"{{"index":0,"delta":{{"type":"{raw}"}}}}"#);
            assert_eq!(
                classify(&event("content_block_delta", &data)),
                AnthropicEvent::ContentBlockDelta {
                    index: 0,
                    delta: expected
                },
                "{raw} was not recognized"
            );
        }
    }

    #[test]
    fn an_unknown_delta_type_is_visibly_unknown_rather_than_treated_as_text() {
        let data = r#"{"index":0,"delta":{"type":"future_delta_type"}}"#;
        assert_eq!(
            classify(&event("content_block_delta", data)),
            AnthropicEvent::ContentBlockDelta {
                index: 0,
                delta: DeltaKind::Unknown
            }
        );
    }

    #[test]
    fn signatures_and_citations_are_not_counted_as_prose() {
        // Counting metadata as text would inflate any output-length measurement.
        assert!(DeltaKind::Text.is_prose());
        assert!(DeltaKind::Thinking.is_prose());
        assert!(!DeltaKind::Signature.is_prose());
        assert!(!DeltaKind::Citations.is_prose());
        assert!(!DeltaKind::InputJson.is_prose());
    }

    #[test]
    fn the_block_index_is_carried_through() {
        let data = r#"{"index":7,"delta":{"type":"text_delta","text":"x"}}"#;
        assert_eq!(
            classify(&event("content_block_delta", data)),
            AnthropicEvent::ContentBlockDelta {
                index: 7,
                delta: DeltaKind::Text
            }
        );
    }

    #[test]
    fn usage_is_read_from_a_message_delta() {
        let data = r#"{"type":"message_delta","usage":{"output_tokens":1234}}"#;
        assert_eq!(
            classify(&event("message_delta", data)),
            AnthropicEvent::MessageDelta {
                output_tokens: Some(1234)
            }
        );
    }

    #[test]
    fn a_mid_stream_error_is_classified_as_an_error() {
        let data = r#"{"type":"error","error":{"message":"overloaded"}}"#;
        assert_eq!(
            classify(&event("error", data)),
            AnthropicEvent::Error {
                message: "overloaded".into()
            }
        );
    }

    #[test]
    fn malformed_payloads_classify_rather_than_failing() {
        // The proxy forwards these either way; erroring here would be an error about
        // something that is not the proxy's business.
        assert_eq!(
            classify(&event("message_start", "not json at all")),
            AnthropicEvent::MessageStart
        );
        assert!(matches!(
            classify(&Event::default()),
            AnthropicEvent::Other { .. }
        ));
    }

    // ---- observation ----

    #[test]
    fn a_complete_stream_is_observed_end_to_end() {
        let stream = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
            "event: content_block_start\ndata: {\"index\":0}\n\n",
            "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"...\"}}\n\n",
            ": ping\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );

        let mut parser = SseParser::new();
        let mut observer = StreamObserver::default();
        for event in parser.feed(stream.as_bytes()) {
            observer.observe(&event);
        }

        assert_eq!(observer.text_deltas, 1);
        assert_eq!(observer.thinking_deltas, 1);
        assert_eq!(observer.output_tokens, Some(42));
        assert!(observer.succeeded());
        assert!(observer.unknown_types.is_empty());
    }

    #[test]
    fn a_stream_ending_in_an_error_did_not_succeed() {
        // However many events it produced. Counting it as success would hide real
        // failures.
        let mut observer = StreamObserver::default();
        observer.observe(&event("message_start", r#"{"type":"message_start"}"#));
        observer.observe(&event(
            "error",
            r#"{"type":"error","error":{"message":"boom"}}"#,
        ));
        observer.observe(&event("message_stop", r#"{"type":"message_stop"}"#));

        assert!(observer.completed);
        assert!(!observer.succeeded(), "an errored stream is not a success");
        assert_eq!(observer.error.as_deref(), Some("boom"));
    }

    #[test]
    fn an_unterminated_stream_is_not_reported_as_complete() {
        let mut observer = StreamObserver::default();
        observer.observe(&event("message_start", r#"{"type":"message_start"}"#));
        assert!(!observer.completed);
        assert!(!observer.succeeded());
    }

    #[test]
    fn unknown_event_types_are_recorded_once_each() {
        let mut observer = StreamObserver::default();
        observer.observe(&event("some_future_event", "{}"));
        observer.observe(&event("some_future_event", "{}"));
        assert_eq!(
            observer.unknown_types,
            vec!["some_future_event".to_string()]
        );
    }

    #[test]
    fn the_done_sentinel_completes_a_stream() {
        let mut observer = StreamObserver::default();
        observer.observe(&Event {
            name: None,
            data: "[DONE]".into(),
            id: None,
        });
        assert!(observer.completed);
    }
}
