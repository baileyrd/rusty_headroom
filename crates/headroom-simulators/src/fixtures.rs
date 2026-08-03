//! SSE corner-case fixtures — gap row E3.
//!
//! These are the shapes that break a parser written against the happy path, collected
//! in one place so every consumer asserts against the same bytes rather than each
//! inventing a slightly different "realistic" stream.
//!
//! Each constant names the specific defect it guards against. A fixture whose reason
//! for existing is not written down gets deleted the first time someone tidies up.

/// A complete Anthropic stream, every documented delta type present.
///
/// Guards the defect the reference records: a parser handling only `text_delta`
/// silently dropped `thinking_delta`, `signature_delta`, and `citations_delta`. Those
/// deltas carry real content, so dropping them produced a shorter transcript that
/// looked like the model simply said less.
pub const ANTHROPIC_COMPLETE: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":10,"cache_read_input_tokens":900,"cache_creation_input_tokens":100}}}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"considering"}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
    "\n\n",
    "event: content_block_stop\n",
    r#"data: {"type":"content_block_stop","index":0}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text"}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hello"}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"citations_delta","citation":{"type":"page"}}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}}"#,
    "\n\n",
    "event: ping\n",
    "data: {}\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}"#,
    "\n\n",
    "event: message_stop\n",
    r#"data: {"type":"message_stop"}"#,
    "\n\n",
);

/// A stream carrying multi-byte UTF-8 and emoji.
///
/// The corruption this guards against is invisible in a happy-path test and reaches the
/// user inside the model's own output. A parser calling `from_utf8` per chunk mangles
/// every codepoint that straddles a chunk boundary, and the mojibake looks like
/// something the *model* produced.
///
/// Feed this one byte at a time, or split at every offset — a spot check at two or
/// three offsets leaves exactly the one bad offset that breaks in production.
pub const UTF8_STRADDLING: &str = concat!(
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"日本語 😀 café"}}"#,
    "\n\n",
);

/// A stream using CRLF line terminators.
///
/// Legal per the SSE specification and used by some servers. A parser that splits on
/// `\n` alone leaves a trailing `\r` on every field, so the event *name* becomes
/// `"message_start\r"` and matches nothing — yielding a stream with no recognized
/// events at all rather than a visible parse error.
pub const CRLF_TERMINATED: &str = concat!(
    "event: message_start\r\n",
    "data: {\"type\":\"message_start\"}\r\n",
    "\r\n",
    "event: message_stop\r\n",
    "data: {\"type\":\"message_stop\"}\r\n",
    "\r\n",
);

/// A stream padded with keep-alive comments.
///
/// Comment lines begin with `:` and carry no data. A parser that treats them as fields
/// emits phantom empty events, which inflates any event count used for telemetry.
pub const KEEPALIVE_COMMENTS: &str = concat!(
    ": keep-alive\n\n",
    ": keep-alive\n\n",
    "event: message_start\n",
    "data: {\"type\":\"message_start\"}\n\n",
    ": keep-alive\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// A stream whose `data:` payload spans several lines.
///
/// The specification joins consecutive `data:` lines with a newline. A parser taking
/// only the last line silently truncates the JSON, which then fails to parse and gets
/// classified as an unknown event rather than as the message it is.
pub const MULTILINE_DATA: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\n",
    "data: \"message_start\"}\n",
    "\n",
);

/// A stream that ends without a final blank line.
///
/// A connection closing cleanly after the last frame is normal. A parser that only
/// emits on a terminator loses the final event, which is usually the one carrying the
/// stop reason and the token counts.
pub const UNTERMINATED_TAIL: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\"}\n",
    "\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}",
);

/// A stream that fails partway through.
///
/// A stream ending in an error is not a stream that completed, however many events
/// preceded it. Counting it as a success is how a real failure rate stays invisible.
pub const MID_STREAM_ERROR: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\"}\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}"#,
    "\n\n",
    "event: error\n",
    r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
    "\n\n",
);

/// A stream carrying an event type this build does not model.
///
/// A provider adding an event type must show up as a number somewhere rather than
/// vanishing — silence is indistinguishable from having handled it.
pub const UNKNOWN_EVENT_TYPE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\"}\n\n",
    "event: some_future_event\n",
    "data: {\"type\":\"some_future_event\",\"detail\":1}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// A complete OpenAI chat-completion stream.
///
/// The `[DONE]` sentinel is the point: it is **not JSON**, so a parser that
/// deserializes every `data:` line classifies the one frame announcing a clean end as
/// malformed — and every stream then looks unterminated.
pub const OPENAI_COMPLETE: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\" there\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

/// An OpenAI stream whose tool call is spread across chunks.
///
/// A single call arrives as a name chunk followed by argument fragments. Counting
/// `tool_calls` entries per chunk reports one call as five.
pub const OPENAI_TOOL_CALL_FRAGMENTS: &str = concat!(
    r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":""}}]}}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{"content":null,"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]}}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{"content":null,"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.rs\"}"}}]}}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    "\n\n",
    "data: [DONE]\n\n",
);

/// Every fixture, paired with a short name for use in test failure messages.
pub const ALL: [(&str, &str); 10] = [
    ("anthropic_complete", ANTHROPIC_COMPLETE),
    ("utf8_straddling", UTF8_STRADDLING),
    ("crlf_terminated", CRLF_TERMINATED),
    ("keepalive_comments", KEEPALIVE_COMMENTS),
    ("multiline_data", MULTILINE_DATA),
    ("unterminated_tail", UNTERMINATED_TAIL),
    ("mid_stream_error", MID_STREAM_ERROR),
    ("unknown_event_type", UNKNOWN_EVENT_TYPE),
    ("openai_complete", OPENAI_COMPLETE),
    ("openai_tool_call_fragments", OPENAI_TOOL_CALL_FRAGMENTS),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_fixture_is_listed_in_all() {
        // A fixture nobody iterates over is a fixture the property tests silently skip.
        assert_eq!(ALL.len(), 10);
        for (name, body) in ALL {
            assert!(!body.is_empty(), "{name} is empty");
        }
    }

    #[test]
    fn fixture_names_are_unique() {
        let mut names: Vec<&str> = ALL.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate fixture name");
    }

    #[test]
    fn the_utf8_fixture_actually_contains_multibyte_sequences() {
        // Otherwise it tests nothing and passes forever.
        assert!(UTF8_STRADDLING.chars().any(|c| c.len_utf8() > 1));
        assert!(
            UTF8_STRADDLING.chars().any(|c| c.len_utf8() == 4),
            "no emoji"
        );
    }

    #[test]
    fn the_crlf_fixture_uses_crlf_and_nothing_else() {
        assert!(CRLF_TERMINATED.contains("\r\n"));
        assert!(
            !CRLF_TERMINATED.replace("\r\n", "").contains('\n'),
            "a bare newline crept in, so this no longer tests CRLF handling"
        );
    }

    #[test]
    fn the_unterminated_fixture_really_does_lack_its_terminator() {
        assert!(!UNTERMINATED_TAIL.ends_with("\n\n"));
    }

    #[test]
    fn the_openai_fixtures_carry_the_done_sentinel() {
        assert!(OPENAI_COMPLETE.contains("data: [DONE]"));
        assert!(OPENAI_TOOL_CALL_FRAGMENTS.contains("data: [DONE]"));
    }

    #[test]
    fn the_anthropic_fixture_carries_every_delta_type() {
        for delta in [
            "text_delta",
            "thinking_delta",
            "signature_delta",
            "citations_delta",
            "input_json_delta",
        ] {
            assert!(ANTHROPIC_COMPLETE.contains(delta), "{delta} is missing");
        }
    }
}
