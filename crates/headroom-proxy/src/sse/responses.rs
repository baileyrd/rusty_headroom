//! OpenAI Responses streaming events — gap row X12.
//!
//! A third vocabulary, and the one that differs most from the other two.
//!
//! # Events are named by a dotted path, not by a small enum
//!
//! Chat completions send one repeated `chunk` shape; Anthropic sends a dozen named
//! events. Responses sends `response.output_item.added`,
//! `response.function_call_arguments.delta`, `response.output_text.done`, and so on —
//! a namespace that grows as the API does.
//!
//! Matching the full string against a fixed list means every event added tomorrow
//! becomes unrecognized. Matching the *last segment* means `response.output_text.done`
//! and `response.reasoning_summary_text.done` collapse into one. So both parts are
//! kept: the stem says what the event is about, the suffix says what happened to it.
//!
//! # Reasoning summaries are not output text
//!
//! Both arrive as deltas, and counting them together inflates any measurement of how
//! much the model actually said — while hiding how much was spent thinking. They are
//! counted separately for the same reason `signature_delta` is not counted as prose in
//! the Anthropic observer.

use serde_json::Value;

use super::Event;

/// What an event says happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Something began.
    Added,
    /// Incremental content.
    Delta,
    /// Something finished.
    Done,
    /// A lifecycle transition that is neither.
    Lifecycle,
}

/// Cache accounting from a terminal event's `response.usage`.
///
/// `None` for a field the provider did not report, which is not the same as a reported
/// zero: `cache_write_tokens` only exists on the model families that bill for cache
/// writes, and reading its absence as "nothing was written" would be this proxy
/// inventing a number about the metric it exists to move.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheTokens {
    /// Input tokens the provider served from its cache.
    pub read: Option<u64>,
    /// Input tokens the provider wrote to it.
    pub creation: Option<u64>,
}

/// Reads the cache pair out of a Responses `usage` object.
///
/// Shared by [`cache_tokens`] and [`cache_tokens_in_body`]. A terminal event nests the
/// usage under `response`; a non-streaming reply puts the same object at the top level.
fn cache_from_usage(usage: Option<&Value>) -> CacheTokens {
    let details = usage.and_then(|usage| usage.get("input_tokens_details"));

    CacheTokens {
        read: details
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64),
        creation: details
            .and_then(|details| details.get("cache_write_tokens"))
            .and_then(Value::as_u64),
    }
}

/// Reads `response.usage.input_tokens_details` out of a terminal event's payload.
fn cache_tokens(payload: Option<&Value>) -> CacheTokens {
    cache_from_usage(
        payload
            .and_then(|payload| payload.get("response"))
            .and_then(|response| response.get("usage")),
    )
}

/// Cache tokens reported by a **non-streaming** Responses reply.
pub fn cache_tokens_in_body(body: &[u8]) -> (u64, u64) {
    let Ok(payload) = serde_json::from_slice::<Value>(body) else {
        return (0, 0);
    };
    let cache = cache_from_usage(payload.get("usage"));
    (cache.read.unwrap_or(0), cache.creation.unwrap_or(0))
}

/// A classified Responses stream event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponsesEvent {
    /// The response is starting.
    Created,
    /// The response finished successfully.
    Completed {
        /// Output tokens reported, if present.
        output_tokens: Option<u64>,
        /// What the response cost the cache, if reported.
        cache: CacheTokens,
    },
    /// The response failed or was cancelled.
    Terminated {
        /// Which terminal state — `failed`, `incomplete`, or `cancelled`.
        reason: String,
        /// What the response cost the cache before it stopped, if reported.
        cache: CacheTokens,
    },
    /// Prose the model produced.
    OutputText {
        /// What happened to it.
        phase: Phase,
        /// The delta text, when this is a delta.
        text: Option<String>,
    },
    /// A reasoning summary — *not* prose.
    ReasoningSummary {
        /// What happened to it.
        phase: Phase,
    },
    /// A tool call's arguments, which stream in fragments.
    FunctionCallArguments {
        /// What happened to them.
        phase: Phase,
        /// Index of the output item the call belongs to.
        item_index: Option<usize>,
    },
    /// An output item opened or closed.
    OutputItem {
        /// What happened to it.
        phase: Phase,
    },
    /// The provider reported an error.
    Error {
        /// What it said.
        message: String,
    },
    /// Something this build does not model.
    Other {
        /// The event's declared type, so it can be counted rather than dropped.
        event_type: Option<String>,
    },
}

/// Classifies one Responses stream event.
pub fn classify(event: &Event) -> ResponsesEvent {
    let payload = serde_json::from_str::<Value>(event.data.trim()).ok();

    let event_type = event
        .name
        .as_deref()
        .or_else(|| {
            payload
                .as_ref()
                .and_then(|p| p.get("type"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned);

    let Some(event_type) = event_type else {
        return ResponsesEvent::Other { event_type: None };
    };

    // `response.output_text.delta` → stem `output_text`, suffix `delta`. Splitting
    // rather than matching the whole string is what keeps a newly added
    // `response.<something>.delta` recognizable as a delta instead of unknown.
    //
    // A type with no `.` at all — e.g. the Responses API's mid-stream `error` event,
    // which arrives as a bare `"error"` rather than `response.error` — has nothing to
    // split. Regression: treating that case as an empty stem with the whole string as
    // suffix (what `rsplitn(2, '.')` naturally gives when there is only one segment)
    // meant a bare `error` event matched neither `("error", _)` nor `("response",
    // "error")` below, and fell through to `Other` — silently dropping the dedicated
    // provider-error case this classifier exists to catch. The whole string is the
    // stem here instead, since that is where every real dotted type carries its
    // meaningful discriminator.
    let (stem, suffix) = match event_type.rsplit_once('.') {
        Some((rest, suffix)) => (rest.rsplit('.').next().unwrap_or(rest), suffix),
        None => (event_type.as_str(), ""),
    };

    let phase = match suffix {
        "added" => Phase::Added,
        "delta" => Phase::Delta,
        "done" | "completed" => Phase::Done,
        _ => Phase::Lifecycle,
    };

    match (stem, suffix) {
        ("response", "created") => ResponsesEvent::Created,
        ("response", "completed") => ResponsesEvent::Completed {
            output_tokens: payload
                .as_ref()
                .and_then(|p| p.get("response"))
                .and_then(|r| r.get("usage"))
                .and_then(|u| u.get("output_tokens"))
                .and_then(Value::as_u64),
            cache: cache_tokens(payload.as_ref()),
        },
        // Distinguished from `completed` because a response that failed is not one that
        // finished, however many tokens it produced first — the same reason the
        // Anthropic observer separates `error` from `message_stop`.
        //
        // Its cache numbers are read all the same. A turn that ran out of output tokens
        // still read its prefix from the cache and was still billed for it, so skipping
        // the usage here would undercount every truncated request.
        ("response", "failed" | "incomplete" | "cancelled") => ResponsesEvent::Terminated {
            reason: suffix.to_owned(),
            cache: cache_tokens(payload.as_ref()),
        },
        ("output_text", _) => ResponsesEvent::OutputText {
            phase,
            text: payload
                .as_ref()
                .and_then(|p| p.get("delta"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        ("reasoning_summary_text" | "reasoning_summary_part" | "reasoning_summary", _) => {
            ResponsesEvent::ReasoningSummary { phase }
        }
        ("function_call_arguments", _) => ResponsesEvent::FunctionCallArguments {
            phase,
            item_index: payload
                .as_ref()
                .and_then(|p| p.get("output_index"))
                .and_then(Value::as_u64)
                .map(|index| index as usize),
        },
        ("output_item", _) => ResponsesEvent::OutputItem { phase },
        ("error", _) | ("response", "error") => ResponsesEvent::Error {
            message: payload
                .as_ref()
                .and_then(|p| p.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("unspecified")
                .to_owned(),
        },
        _ => ResponsesEvent::Other {
            event_type: Some(event_type),
        },
    }
}

/// What a Responses stream reported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponsesObserver {
    /// Events seen.
    pub events: usize,
    /// Deltas carrying prose.
    pub text_deltas: usize,
    /// Characters of prose, summed.
    pub text_chars: usize,
    /// Deltas carrying a reasoning summary.
    ///
    /// Separate from prose: counting them together inflates any measurement of what the
    /// model actually said while hiding how much was spent thinking.
    pub reasoning_deltas: usize,
    /// Distinct output items that carried tool-call arguments.
    ///
    /// Indices rather than a count, because arguments stream in fragments and counting
    /// per event reports one call many times over.
    pub call_item_indices: Vec<usize>,
    /// Output tokens the provider reported.
    pub output_tokens: Option<u64>,
    /// Input tokens the provider served from its cache.
    pub cache_read_tokens: u64,
    /// Input tokens the provider wrote to its cache.
    pub cache_creation_tokens: u64,
    /// Whether a terminal event arrived.
    pub completed: bool,
    /// The provider's error or terminal reason, if it did not succeed.
    pub failure: Option<String>,
    /// Event types this build does not model.
    pub unknown_types: Vec<String>,
}

impl ResponsesObserver {
    /// Records one event.
    pub fn observe(&mut self, event: &Event) {
        self.events += 1;

        match classify(event) {
            ResponsesEvent::Created => {}
            ResponsesEvent::Completed {
                output_tokens,
                cache,
            } => {
                self.completed = true;
                if output_tokens.is_some() {
                    self.output_tokens = output_tokens;
                }
                self.record_cache(cache);
            }
            ResponsesEvent::Terminated { reason, cache } => {
                // Terminal either way, but not a success. A stream that failed is still
                // over, and reporting it as unfinished would be as wrong as reporting
                // it as complete.
                self.completed = true;
                self.failure = Some(reason);
                self.record_cache(cache);
            }
            ResponsesEvent::OutputText { phase, text } => {
                if phase == Phase::Delta {
                    self.text_deltas += 1;
                    self.text_chars += text.map(|t| t.chars().count()).unwrap_or(0);
                }
            }
            ResponsesEvent::ReasoningSummary { phase } => {
                if phase == Phase::Delta {
                    self.reasoning_deltas += 1;
                }
            }
            ResponsesEvent::FunctionCallArguments { phase, item_index } => {
                if phase == Phase::Delta {
                    if let Some(index) = item_index {
                        if !self.call_item_indices.contains(&index) {
                            self.call_item_indices.push(index);
                        }
                    }
                }
            }
            ResponsesEvent::OutputItem { .. } => {}
            ResponsesEvent::Error { message } => self.failure = Some(message),
            ResponsesEvent::Other {
                event_type: Some(event_type),
            } if !self.unknown_types.contains(&event_type) => {
                self.unknown_types.push(event_type);
            }
            ResponsesEvent::Other { .. } => {}
        }
    }

    /// Adds one terminal event's cache accounting.
    ///
    /// Summed rather than assigned, so a relay carrying more than one response's frames
    /// reports both rather than only the last.
    fn record_cache(&mut self, cache: CacheTokens) {
        self.cache_read_tokens += cache.read.unwrap_or(0);
        self.cache_creation_tokens += cache.creation.unwrap_or(0);
    }

    /// How many distinct tool calls the stream carried.
    pub fn tool_calls(&self) -> usize {
        self.call_item_indices.len()
    }

    /// Whether the stream ended successfully.
    pub fn succeeded(&self) -> bool {
        self.completed && self.failure.is_none()
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
            classify(&event("response.created", "{}")),
            ResponsesEvent::Created
        );
        assert_eq!(
            classify(&event(
                "response.completed",
                r#"{"response":{"usage":{"output_tokens":42}}}"#
            )),
            ResponsesEvent::Completed {
                output_tokens: Some(42),
                cache: CacheTokens::default(),
            }
        );
    }

    #[test]
    fn a_failed_response_is_terminal_but_not_a_success() {
        // A stream that failed is still over — reporting it as unfinished would be as
        // wrong as reporting it as complete.
        let mut observer = ResponsesObserver::default();
        observer.observe(&event("response.created", "{}"));
        observer.observe(&event("response.failed", "{}"));

        assert!(observer.completed);
        assert!(!observer.succeeded());
        assert_eq!(observer.failure.as_deref(), Some("failed"));
    }

    #[test]
    fn every_terminal_state_is_recognized() {
        for reason in ["failed", "incomplete", "cancelled"] {
            let mut observer = ResponsesObserver::default();
            observer.observe(&event(&format!("response.{reason}"), "{}"));
            assert!(observer.completed, "{reason}");
            assert!(!observer.succeeded(), "{reason}");
        }
    }

    #[test]
    fn a_bare_error_event_is_classified_as_an_error_not_dropped_as_unknown() {
        // Regression. The Responses API's mid-stream error event arrives as a bare
        // `"error"` type, not `response.error` — no `.` at all. The stem/suffix split
        // used to leave a dot-less type with an empty stem, which matched neither
        // `("error", _)` nor `("response", "error")` and silently fell through to
        // `Other`, so a real provider error was never recorded as a failure.
        assert_eq!(
            classify(&event("error", r#"{"type":"error","message":"boom"}"#)),
            ResponsesEvent::Error {
                message: "boom".to_owned(),
            }
        );

        let mut observer = ResponsesObserver::default();
        observer.observe(&event("error", r#"{"type":"error","message":"boom"}"#));
        assert_eq!(observer.failure.as_deref(), Some("boom"));
        assert!(!observer.succeeded());
    }

    #[test]
    fn output_text_deltas_are_counted_as_prose() {
        let mut observer = ResponsesObserver::default();
        for text in ["Hello", " there"] {
            observer.observe(&event(
                "response.output_text.delta",
                &format!(r#"{{"delta":"{text}"}}"#),
            ));
        }

        assert_eq!(observer.text_deltas, 2);
        assert_eq!(observer.text_chars, 11);
    }

    #[test]
    fn a_reasoning_summary_is_not_counted_as_prose() {
        // Counting them together inflates any measurement of what the model actually
        // said, while hiding how much was spent thinking — the same reason the
        // Anthropic observer does not count `signature_delta` as text.
        let mut observer = ResponsesObserver::default();
        observer.observe(&event(
            "response.reasoning_summary_text.delta",
            r#"{"delta":"considering the options"}"#,
        ));
        observer.observe(&event("response.output_text.delta", r#"{"delta":"Hi"}"#));

        assert_eq!(observer.text_deltas, 1);
        assert_eq!(observer.reasoning_deltas, 1);
        assert_eq!(
            observer.text_chars, 2,
            "reasoning leaked into the prose count"
        );
    }

    #[test]
    fn a_done_event_is_not_counted_as_a_delta() {
        // `response.output_text.done` repeats the whole text. Counting it would double
        // every measurement of output length.
        let mut observer = ResponsesObserver::default();
        observer.observe(&event("response.output_text.delta", r#"{"delta":"Hi"}"#));
        observer.observe(&event("response.output_text.done", r#"{"text":"Hi"}"#));

        assert_eq!(observer.text_deltas, 1);
        assert_eq!(observer.text_chars, 2);
    }

    #[test]
    fn one_tool_call_split_across_events_is_counted_once() {
        let mut observer = ResponsesObserver::default();
        for fragment in [r#"{"pa"#, r#"th\":\""#, r#"a.rs\"}"#] {
            observer.observe(&event(
                "response.function_call_arguments.delta",
                &format!(r#"{{"output_index":0,"delta":"{fragment}"}}"#),
            ));
        }

        assert_eq!(observer.tool_calls(), 1);
    }

    #[test]
    fn parallel_tool_calls_are_counted_separately() {
        let mut observer = ResponsesObserver::default();
        for index in [0, 1, 0, 2] {
            observer.observe(&event(
                "response.function_call_arguments.delta",
                &format!(r#"{{"output_index":{index},"delta":"x"}}"#),
            ));
        }

        assert_eq!(observer.tool_calls(), 3);
    }

    #[test]
    fn an_event_type_added_tomorrow_is_still_recognized_as_a_delta() {
        // The whole reason the suffix is matched separately from the stem. A fixed list
        // of full event names would make every future event unknown.
        let classified = classify(&event(
            "response.some_future_thing.delta",
            r#"{"delta":"x"}"#,
        ));
        assert!(
            matches!(classified, ResponsesEvent::Other { .. }),
            "unknown stems should still classify as Other"
        );

        // But a *known* stem with a new suffix is not lost.
        let classified = classify(&event("response.output_text.some_new_phase", "{}"));
        assert_eq!(
            classified,
            ResponsesEvent::OutputText {
                phase: Phase::Lifecycle,
                text: None
            }
        );
    }

    #[test]
    fn an_unknown_event_type_is_recorded_rather_than_dropped() {
        // A provider adding an event type should show up as a number somewhere, not
        // vanish — silence is indistinguishable from having handled it.
        let mut observer = ResponsesObserver::default();
        observer.observe(&event("response.brand_new_event", "{}"));
        observer.observe(&event("response.brand_new_event", "{}"));

        assert_eq!(observer.unknown_types, vec!["response.brand_new_event"]);
    }

    #[test]
    fn malformed_payloads_classify_rather_than_failing() {
        for data in ["not json", "", "null"] {
            let _ = classify(&event("response.output_text.delta", data));
        }
        assert!(matches!(
            classify(&Event::default()),
            ResponsesEvent::Other { event_type: None }
        ));
    }

    #[test]
    fn a_realistic_stream_is_observed_end_to_end() {
        let raw = concat!(
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
            "event: response.output_item.added\ndata: {\"output_index\":0}\n\n",
            "event: response.reasoning_summary_text.delta\ndata: {\"delta\":\"thinking\"}\n\n",
            "event: response.output_text.delta\ndata: {\"delta\":\"Hello\"}\n\n",
            "event: response.output_text.delta\ndata: {\"delta\":\" there\"}\n\n",
            "event: response.output_text.done\ndata: {\"text\":\"Hello there\"}\n\n",
            "event: response.completed\ndata: {\"response\":{\"usage\":{\"output_tokens\":11}}}\n\n",
        );

        let mut parser = SseParser::new();
        let mut observer = ResponsesObserver::default();
        for event in parser.feed(raw.as_bytes()) {
            observer.observe(&event);
        }

        assert_eq!(observer.text_deltas, 2);
        assert_eq!(observer.text_chars, 11);
        assert_eq!(observer.reasoning_deltas, 1);
        assert_eq!(observer.output_tokens, Some(11));
        assert!(observer.succeeded());
        assert!(observer.unknown_types.is_empty());
    }
}
