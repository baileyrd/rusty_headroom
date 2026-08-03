//! OpenAI chat-completion streaming events.
//!
//! The same job as [`super::anthropic`], against a different vocabulary. The proxy
//! forwards these bytes untouched and only needs to understand them well enough to
//! count what happened.
//!
//! # Two things this shape does that Anthropic's does not
//!
//! **The stream terminates with a sentinel, not an event.** OpenAI closes with a
//! literal `data: [DONE]`, which is not JSON. A parser that tries to deserialize every
//! `data:` line treats the terminator as a malformed payload and never records that the
//! stream ended cleanly.
//!
//! **Tool calls arrive in fragments.** A single tool call is spread across many chunks:
//! the first carries the name and an `id`, later ones carry slices of the argument JSON
//! and nothing else. Counting `tool_calls` entries per chunk would count one call many
//! times over, so calls are tracked by index and the count is of distinct indices.

use serde_json::Value;

use super::Event;

/// The sentinel that ends an OpenAI stream.
const DONE: &str = "[DONE]";

/// A classified OpenAI chat-completion event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenAiEvent {
    /// A chunk carrying incremental content.
    Chunk {
        /// Prose appended by this chunk, if any.
        content: Option<String>,
        /// Tool-call fragments in this chunk, by their index in the call array.
        tool_call_indices: Vec<usize>,
        /// Why generation stopped, on the final chunk.
        finish_reason: Option<String>,
    },
    /// The terminating `data: [DONE]` sentinel.
    Done,
    /// The provider reported an error.
    Error {
        /// What the provider said.
        message: String,
    },
    /// Something this code does not recognize.
    Other,
}

/// Classifies one OpenAI stream event.
pub fn classify(event: &Event) -> OpenAiEvent {
    let data = event.data.trim();

    // Checked before parsing. `[DONE]` is not JSON, so a parse-first reader classifies
    // the one frame that says "the stream ended" as malformed.
    if data == DONE {
        return OpenAiEvent::Done;
    }

    let Ok(payload) = serde_json::from_str::<Value>(data) else {
        return OpenAiEvent::Other;
    };

    if let Some(error) = payload.get("error") {
        return OpenAiEvent::Error {
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unspecified")
                .to_owned(),
        };
    }

    let Some(choice) = payload
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
    else {
        return OpenAiEvent::Other;
    };

    let delta = choice.get("delta");

    OpenAiEvent::Chunk {
        content: delta
            .and_then(|d| d.get("content"))
            .and_then(Value::as_str)
            // An explicit `"content": null` is how OpenAI marks a chunk that carries
            // only a tool-call fragment. Treating it as empty prose would count it as
            // text output that never existed.
            .filter(|text| !text.is_empty())
            .map(str::to_owned),
        tool_call_indices: delta
            .and_then(|d| d.get("tool_calls"))
            .and_then(Value::as_array)
            .map(|calls| {
                calls
                    .iter()
                    .filter_map(|call| call.get("index").and_then(Value::as_u64))
                    .map(|index| index as usize)
                    .collect()
            })
            .unwrap_or_default(),
        finish_reason: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

/// What an OpenAI stream reported, accumulated as it passed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenAiObserver {
    /// Events seen.
    pub events: usize,
    /// Chunks that carried prose.
    pub content_chunks: usize,
    /// Distinct tool-call indices seen.
    ///
    /// Indices rather than a count, because a single call is spread across many chunks
    /// and counting per chunk would count one call many times over.
    pub tool_call_indices: Vec<usize>,
    /// Why generation stopped.
    pub finish_reason: Option<String>,
    /// Whether the terminating sentinel arrived.
    pub completed: bool,
    /// The provider's error, if the stream failed.
    pub error: Option<String>,
}

impl OpenAiObserver {
    /// Records one event.
    pub fn observe(&mut self, event: &Event) {
        self.events += 1;

        match classify(event) {
            OpenAiEvent::Chunk {
                content,
                tool_call_indices,
                finish_reason,
            } => {
                if content.is_some() {
                    self.content_chunks += 1;
                }
                for index in tool_call_indices {
                    if !self.tool_call_indices.contains(&index) {
                        self.tool_call_indices.push(index);
                    }
                }
                if finish_reason.is_some() {
                    self.finish_reason = finish_reason;
                }
            }
            OpenAiEvent::Done => self.completed = true,
            OpenAiEvent::Error { message } => self.error = Some(message),
            OpenAiEvent::Other => {}
        }
    }

    /// How many distinct tool calls the stream carried.
    pub fn tool_calls(&self) -> usize {
        self.tool_call_indices.len()
    }

    /// Whether the stream ended successfully.
    ///
    /// A stream that ended in an error did not succeed however many chunks it
    /// produced, and counting it as success would hide real failures in telemetry.
    pub fn succeeded(&self) -> bool {
        self.completed && self.error.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::SseParser;

    fn event(data: &str) -> Event {
        Event {
            name: None,
            data: data.into(),
            id: None,
        }
    }

    #[test]
    fn the_done_sentinel_is_recognized_rather_than_treated_as_malformed() {
        // `[DONE]` is not JSON. A parse-first reader classifies the one frame that says
        // the stream ended cleanly as garbage, so every stream looks unterminated.
        assert_eq!(classify(&event("[DONE]")), OpenAiEvent::Done);
        assert_eq!(classify(&event("  [DONE]  ")), OpenAiEvent::Done);
    }

    #[test]
    fn a_content_chunk_yields_its_text() {
        let data = r#"{"choices":[{"index":0,"delta":{"content":"hello"}}]}"#;
        assert_eq!(
            classify(&event(data)),
            OpenAiEvent::Chunk {
                content: Some("hello".into()),
                tool_call_indices: vec![],
                finish_reason: None,
            }
        );
    }

    #[test]
    fn a_null_content_chunk_is_not_counted_as_prose() {
        // `"content": null` marks a chunk carrying only a tool-call fragment. Counting
        // it as empty prose reports text output that never existed.
        let data = r#"{"choices":[{"index":0,"delta":{"content":null,"tool_calls":[{"index":0,"function":{"arguments":"{\"a\""}}]}}]}"#;
        let OpenAiEvent::Chunk {
            content,
            tool_call_indices,
            ..
        } = classify(&event(data))
        else {
            panic!("should have been a chunk");
        };

        assert_eq!(content, None);
        assert_eq!(tool_call_indices, vec![0]);
    }

    #[test]
    fn one_tool_call_split_across_chunks_is_counted_once() {
        // The defect this guards. A single call arrives as a name chunk followed by
        // many argument-fragment chunks; counting `tool_calls` entries per chunk
        // reports one call as five.
        let mut observer = OpenAiObserver::default();
        for fragment in [
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":""}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":".rs\"}"}}]}}]}"#,
        ] {
            observer.observe(&event(fragment));
        }

        assert_eq!(observer.tool_calls(), 1, "one call was counted many times");
    }

    #[test]
    fn parallel_tool_calls_are_counted_separately() {
        // The over-correction the fix must not become: deduplicating by index must
        // still distinguish genuinely different calls.
        let mut observer = OpenAiObserver::default();
        observer.observe(&event(
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a"},{"index":1,"id":"b"}]}}]}"#,
        ));
        observer.observe(&event(
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":2,"id":"c"}]}}]}"#,
        ));

        assert_eq!(observer.tool_calls(), 3);
    }

    #[test]
    fn the_finish_reason_is_captured() {
        let mut observer = OpenAiObserver::default();
        observer.observe(&event(
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ));
        assert_eq!(observer.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn a_stream_ending_in_an_error_did_not_succeed() {
        let mut observer = OpenAiObserver::default();
        observer.observe(&event(
            r#"{"choices":[{"index":0,"delta":{"content":"partial"}}]}"#,
        ));
        observer.observe(&event(r#"{"error":{"message":"context_length_exceeded"}}"#));
        observer.observe(&event("[DONE]"));

        assert!(observer.completed);
        assert!(!observer.succeeded(), "an errored stream is not a success");
        assert_eq!(observer.error.as_deref(), Some("context_length_exceeded"));
    }

    #[test]
    fn malformed_payloads_classify_rather_than_failing() {
        // The proxy forwards these either way; erroring here would be an error about
        // something that is not the proxy's business.
        for data in ["not json", "", "{}", r#"{"choices":[]}"#] {
            assert_eq!(classify(&event(data)), OpenAiEvent::Other, "{data:?}");
        }
    }

    #[test]
    fn a_realistic_stream_is_observed_end_to_end() {
        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\" there\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );

        let mut parser = SseParser::new();
        let mut observer = OpenAiObserver::default();
        for event in parser.feed(raw.as_bytes()) {
            observer.observe(&event);
        }

        assert_eq!(observer.content_chunks, 2, "the empty opener is not prose");
        assert_eq!(observer.finish_reason.as_deref(), Some("stop"));
        assert!(observer.succeeded());
    }

    #[test]
    fn the_sentinel_is_found_however_the_network_split_the_stream() {
        // `[DONE]` is short enough to land across a chunk boundary easily, and losing
        // it makes every stream look unterminated.
        let raw = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";

        for split in 1..raw.len() {
            let mut parser = SseParser::new();
            let mut observer = OpenAiObserver::default();
            for chunk in [&raw.as_bytes()[..split], &raw.as_bytes()[split..]] {
                for event in parser.feed(chunk) {
                    observer.observe(&event);
                }
            }
            assert!(observer.completed, "terminator lost at split {split}");
        }
    }
}
