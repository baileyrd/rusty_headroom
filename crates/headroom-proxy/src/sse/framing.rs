//! Byte-level SSE framing.
//!
//! Server-sent events arrive as a byte stream chopped into arbitrary chunks by the
//! network. A parser that assumes chunk boundaries align with anything is wrong, and
//! wrong in ways that only appear under load.
//!
//! # The two failures this is built around
//!
//! **UTF-8 split mid-codepoint.** A chunk can end halfway through a multibyte
//! character. Calling `from_utf8` on each chunk corrupts every emoji and every CJK
//! character that happens to straddle a boundary — and the corruption reaches the
//! user as mojibake in the model's output.
//!
//! **Event split mid-frame.** An SSE frame ends with a blank line, and a chunk can end
//! anywhere. A parser that treats each chunk as a complete frame drops or duplicates
//! events depending on how the network happened to split them.
//!
//! Both are solved the same way: buffer bytes, and only emit a frame once its
//! terminator has actually been seen.

/// One parsed SSE frame.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Event {
    /// The `event:` field, if present.
    pub name: Option<String>,
    /// The `data:` payload, with multiple `data:` lines joined by newlines per spec.
    pub data: String,
    /// The `id:` field, if present.
    pub id: Option<String>,
}

impl Event {
    /// Whether this is the `[DONE]` sentinel some providers use to end a stream.
    pub fn is_done(&self) -> bool {
        self.data.trim() == "[DONE]"
    }
}

/// Accumulates bytes and yields complete events.
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    /// Set once a frame has exceeded [`MAX_FRAME_BYTES`] without a terminator ever
    /// showing up. See [`SseParser::is_overflowed`].
    overflowed: bool,
}

/// Most bytes `feed` will buffer for a single frame before giving up on it.
///
/// # Why bounded
///
/// `feed` only drains the buffer once it sees a frame terminator (`\n\n` or
/// `\r\n\r\n`), and nothing capped that until now — a hung or misbehaving upstream, a
/// compromised/MITM'd one, or a provider bug streaming one unbounded `data:` field
/// would grow the buffer for the entire lifetime of the connection. `observe.rs`
/// guards the equivalent risk on the non-streaming path with `MAX_BUFFERED_BODY`,
/// sized for a whole buffered reply; a single SSE frame realistically never needs
/// anywhere near that much, so this bound is a quarter of it.
const MAX_FRAME_BYTES: usize = 1024 * 1024;

impl SseParser {
    /// Creates an empty parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds a chunk, returning every event it completed.
    ///
    /// Bytes that do not complete a frame are retained for the next call, so a caller
    /// may pass chunks split at any byte offset — including inside a multibyte
    /// character or midway through a field name.
    ///
    /// Once a frame's bytes would exceed [`MAX_FRAME_BYTES`] without a terminator ever
    /// showing up, this stops accumulating, drops what it was holding, and marks the
    /// parser [`overflowed`](Self::is_overflowed) — every later call is then a no-op,
    /// since a frame this large was never going to parse into anything useful anyway.
    /// This mirrors `observe.rs` giving up on a body past `MAX_BUFFERED_BODY`: past
    /// the cap, only the telemetry for this frame is given up on, not the process.
    ///
    /// # Example
    ///
    /// ```
    /// use headroom_proxy::sse::SseParser;
    ///
    /// let mut parser = SseParser::new();
    /// // Split mid-frame; nothing is emitted until the terminator arrives.
    /// assert!(parser.feed(b"event: message\ndata: {\"a\"").is_empty());
    /// let events = parser.feed(b":1}\n\n");
    /// assert_eq!(events.len(), 1);
    /// assert_eq!(events[0].data, r#"{"a":1}"#);
    /// ```
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Event> {
        if self.overflowed {
            return Vec::new();
        }

        self.buffer.extend_from_slice(chunk);

        let mut events = Vec::new();
        while let Some((frame, consumed)) = self.next_frame() {
            self.buffer.drain(..consumed);
            if let Some(event) = parse_frame(&frame) {
                events.push(event);
            }
        }

        if self.buffer.len() > MAX_FRAME_BYTES {
            // No terminator has shown up despite this many bytes: either the
            // upstream hung mid-frame, or it is sending a field far larger than any
            // real SSE event needs. Holding on and hoping is exactly how the buffer
            // grows without bound, so give up on this frame instead.
            self.overflowed = true;
            self.buffer.clear();
            self.buffer.shrink_to_fit();
        }

        events
    }

    /// Emits whatever remains, for a stream that ended without a final terminator.
    ///
    /// Some servers close without a trailing blank line. Discarding the buffer would
    /// silently drop the last event of every such stream.
    pub fn finish(&mut self) -> Option<Event> {
        if self.buffer.iter().all(u8::is_ascii_whitespace) {
            self.buffer.clear();
            return None;
        }
        let frame = std::mem::take(&mut self.buffer);
        parse_frame(&frame)
    }

    /// Whether any bytes are held pending more input.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Whether `feed` has given up on the current frame for exceeding
    /// [`MAX_FRAME_BYTES`] without ever finding a terminator.
    ///
    /// # Why this exists
    ///
    /// A parser that just quietly stopped emitting events would look, to a caller,
    /// identical to a slow stream that simply hasn't finished a frame yet. A caller
    /// that wants to fail a hung or hostile connection rather than sit on it forever
    /// needs a way to tell the two apart.
    pub fn is_overflowed(&self) -> bool {
        self.overflowed
    }

    /// Finds the next complete frame, returning it and how many bytes it consumed.
    ///
    /// Both `\n\n` and `\r\n\r\n` terminate a frame. Handling only the former means a
    /// server using CRLF never yields a single event.
    fn next_frame(&self) -> Option<(Vec<u8>, usize)> {
        let buffer = &self.buffer;
        for index in 0..buffer.len() {
            if buffer[index..].starts_with(b"\n\n") {
                return Some((buffer[..index + 1].to_vec(), index + 2));
            }
            if buffer[index..].starts_with(b"\r\n\r\n") {
                return Some((buffer[..index + 2].to_vec(), index + 4));
            }
        }
        None
    }
}

/// Parses one frame's bytes into an event.
///
/// Returns `None` for a frame carrying no fields — a keep-alive comment, or stray
/// whitespace.
fn parse_frame(frame: &[u8]) -> Option<Event> {
    // Lossy conversion, and deliberately so. By this point the frame is complete, so
    // any invalid sequence is genuinely invalid rather than a split codepoint — and
    // replacing it beats dropping the whole event.
    let text = String::from_utf8_lossy(frame);

    let mut event = Event::default();
    let mut data_lines: Vec<&str> = Vec::new();
    let mut saw_field = false;

    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        // A leading colon marks a comment, which servers use as a keep-alive.
        if line.starts_with(':') {
            continue;
        }

        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };

        match field {
            "event" => {
                event.name = Some(value.to_owned());
                saw_field = true;
            }
            "data" => {
                data_lines.push(value);
                saw_field = true;
            }
            "id" => {
                event.id = Some(value.to_owned());
                saw_field = true;
            }
            // `retry` and unknown fields are ignored per spec, but their presence
            // still means this frame was an event rather than blank.
            _ => saw_field = true,
        }
    }

    if !saw_field {
        return None;
    }

    event.data = data_lines.join("\n");
    Some(event)
}

/// Renders an event back to wire bytes.
pub fn render(event: &Event) -> String {
    let mut out = String::new();
    if let Some(name) = &event.name {
        out.push_str(&format!("event: {name}\n"));
    }
    if let Some(id) = &event.id {
        out.push_str(&format!("id: {id}\n"));
    }
    // Multi-line data becomes one `data:` line each, which is what the spec requires
    // and what rejoining on parse assumed.
    for line in event.data.split('\n') {
        out.push_str(&format!("data: {line}\n"));
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_complete_frame_parses() {
        let mut parser = SseParser::new();
        let events = parser.feed(b"event: message\ndata: hello\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name.as_deref(), Some("message"));
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn a_frame_split_across_chunks_is_held_until_complete() {
        let mut parser = SseParser::new();
        assert!(parser.feed(b"event: mess").is_empty());
        assert!(parser.feed(b"age\ndata: par").is_empty());
        let events = parser.feed(b"tial\n\n");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "partial");
    }

    #[test]
    fn a_multibyte_character_split_across_chunks_is_not_corrupted() {
        // The failure that reaches the user as mojibake. A parser calling from_utf8 on
        // each chunk mangles every emoji unlucky enough to straddle a boundary.
        let payload = "data: 日本語 😀 café\n\n".as_bytes().to_vec();
        let split = 8; // lands inside the first multibyte character

        let mut parser = SseParser::new();
        assert!(parser.feed(&payload[..split]).is_empty());
        let events = parser.feed(&payload[split..]);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "日本語 😀 café");
    }

    #[test]
    fn every_single_byte_split_point_yields_the_same_result() {
        // Exhaustive rather than a spot check: the network may split anywhere, and one
        // bad offset is enough to corrupt a stream in production.
        let payload = "event: message\ndata: {\"text\":\"日本語 😀\"}\n\n"
            .as_bytes()
            .to_vec();

        for split in 0..payload.len() {
            let mut parser = SseParser::new();
            let mut events = parser.feed(&payload[..split]);
            events.extend(parser.feed(&payload[split..]));

            assert_eq!(
                events.len(),
                1,
                "split at {split} produced {} events",
                events.len()
            );
            assert_eq!(
                events[0].data, r#"{"text":"日本語 😀"}"#,
                "split at {split}"
            );
        }
    }

    #[test]
    fn several_events_in_one_chunk_all_emit() {
        let mut parser = SseParser::new();
        let events = parser.feed(b"data: one\n\ndata: two\n\ndata: three\n\n");
        assert_eq!(events.len(), 3);
        assert_eq!(events[2].data, "three");
    }

    #[test]
    fn crlf_terminated_frames_parse() {
        // A server using CRLF would otherwise yield no events at all.
        let mut parser = SseParser::new();
        let events = parser.feed(b"event: message\r\ndata: hello\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn multiple_data_lines_join_with_newlines() {
        let mut parser = SseParser::new();
        let events = parser.feed(b"data: first\ndata: second\n\n");
        assert_eq!(events[0].data, "first\nsecond");
    }

    #[test]
    fn keep_alive_comments_are_skipped_without_emitting() {
        let mut parser = SseParser::new();
        assert!(parser.feed(b": ping\n\n").is_empty());
        // ...and do not disturb a following real event.
        let events = parser.feed(b"data: real\n\n");
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn the_done_sentinel_is_recognized() {
        let mut parser = SseParser::new();
        let events = parser.feed(b"data: [DONE]\n\n");
        assert!(events[0].is_done());
    }

    #[test]
    fn a_stream_ending_without_a_terminator_still_yields_its_last_event() {
        // Some servers just close. Discarding the buffer would silently drop the last
        // event of every such stream.
        let mut parser = SseParser::new();
        assert!(parser.feed(b"data: final").is_empty());
        assert_eq!(parser.finish().unwrap().data, "final");
    }

    #[test]
    fn finishing_an_empty_or_whitespace_buffer_yields_nothing() {
        let mut parser = SseParser::new();
        assert!(parser.finish().is_none());
        parser.feed(b"\n\n");
        assert!(parser.finish().is_none());
    }

    #[test]
    fn an_event_round_trips_through_render_and_parse() {
        let original = Event {
            name: Some("content_block_delta".into()),
            data: r#"{"type":"text_delta","text":"日本語"}"#.into(),
            id: Some("evt_1".into()),
        };

        let mut parser = SseParser::new();
        let events = parser.feed(render(&original).as_bytes());
        assert_eq!(events, vec![original]);
    }

    #[test]
    fn multi_line_data_round_trips() {
        let original = Event {
            name: None,
            data: "line one\nline two".into(),
            id: None,
        };
        let mut parser = SseParser::new();
        assert_eq!(parser.feed(render(&original).as_bytes()), vec![original]);
    }

    #[test]
    fn a_byte_at_a_time_stream_parses_identically() {
        // The pathological case: every chunk one byte.
        let payload = "event: a\ndata: x\n\nevent: b\ndata: y\n\n".as_bytes();
        let mut parser = SseParser::new();
        let mut events = Vec::new();
        for byte in payload {
            events.extend(parser.feed(&[*byte]));
        }
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].name.as_deref(), Some("b"));
        assert!(parser.is_empty());
    }

    #[test]
    fn a_frame_that_never_terminates_stops_growing_instead_of_buffering_forever() {
        // Regression: `feed` used to `extend_from_slice` into `buffer` with no upper
        // bound, draining only once a terminator showed up. A hung/misbehaving
        // upstream, a compromised/MITM'd one, or a provider bug streaming one
        // unbounded `data:` field never sends that terminator, so the buffer would
        // grow for the entire lifetime of the stream. This is the frame-level twin
        // of observe.rs's `MAX_BUFFERED_BODY` guard on the non-streaming path.
        let mut parser = SseParser::new();

        // Well past the bound in one chunk, still with no terminator in sight.
        let chunk = vec![b'a'; MAX_FRAME_BYTES + 1];
        assert!(parser.feed(&chunk).is_empty());

        assert!(parser.is_overflowed());
        // The oversized, unterminated frame is dropped rather than retained forever.
        assert!(parser.is_empty());

        // Once overflowed, further input — even a well-formed frame — is a no-op
        // rather than a fresh attempt to buffer; the parser has given up.
        assert!(parser.feed(b"data: too late\n\n").is_empty());
        assert!(parser.is_empty());
    }

    #[test]
    fn a_frame_approaching_the_bound_across_many_small_chunks_still_overflows_cleanly() {
        // Same failure, but arriving the way a real hung stream would: many small
        // chunks rather than one giant one, none of them ever completing a frame.
        let mut parser = SseParser::new();
        let chunk = [b'x'; 4096];
        let mut total = 0;
        let mut overflowed = false;

        while total <= MAX_FRAME_BYTES {
            let events = parser.feed(&chunk);
            assert!(events.is_empty());
            total += chunk.len();
            if parser.is_overflowed() {
                overflowed = true;
                break;
            }
        }

        assert!(
            overflowed,
            "parser never gave up despite exceeding the bound"
        );
        assert!(parser.is_empty());
    }

    #[test]
    fn a_frame_right_at_the_bound_with_a_terminator_still_parses() {
        // The cap must not misfire on a legitimately large-but-terminated frame —
        // only an unterminated one should ever trip it.
        let padding = "x".repeat(MAX_FRAME_BYTES - 16);
        let payload = format!("data: {padding}\n\n");
        assert!(payload.len() <= MAX_FRAME_BYTES);

        let mut parser = SseParser::new();
        let events = parser.feed(payload.as_bytes());

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, padding);
        assert!(!parser.is_overflowed());
    }
}
