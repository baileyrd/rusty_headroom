//! Request compression — where every piece finally meets.
//!
//! A real Anthropic request arrives, the live zone is identified, type-aware
//! compressors run on it, the frozen prefix is forwarded byte-identical.
//!
//! # Shape
//!
//! [`compress_request`] is a **pure function over bytes**. The axum handler is a thin
//! wrapper that calls it and forwards the result. Every invariant lives in the pure
//! function, which means every invariant is testable without a network, a mock, or a
//! running server — and the tests below are the actual guarantees, not a proxy for
//! them.
//!
//! # Passthrough is the fallback for everything
//!
//! Compression disabled, an unparseable body, no live zone, a compressor that
//! declines, a result that is not smaller — all of them forward the original bytes.
//! There is no input for which this function errors; the worst case is that it does
//! nothing.
//!
//! # `"stream": true` is not one of those cases
//!
//! It used to be. The reasoning was that compressing a stream would mean buffering it,
//! which breaks the thing the client asked for — and that reasoning conflated two
//! different bodies. The *request* body arrives complete before this function is
//! called, whatever the client wants the *response* to look like; `stream` describes
//! the response framing and says nothing about whether the request can be compressed.
//!
//! The distinction is not academic. Streaming is the common agent case, so treating it
//! as passthrough exempted most real traffic from compression while the tests
//! confirmed compression worked.

use std::borrow::Cow;
use std::sync::Arc;

use headroom_core::auth_mode::CompressionPolicy;
use headroom_core::block::{Block, BlockKind};
use headroom_core::ccr::CcrStore;
use headroom_core::conversation::{Conversation, Message, Role};
use headroom_core::live_zone::live_zone;
use headroom_core::output_shaping::{self, Verbosity};
use headroom_core::pipeline::{Orchestrator, Routing};
use headroom_core::validate::validated_apply;
use headroom_core::Transform;
use serde_json::Value;

use crate::body::FaithfulBody;
use crate::frozen::frozen_message_count;

/// The compressor set the proxy dispatches through.
///
/// A thin wrapper over [`Orchestrator`], which owns the routing decision. It used to be
/// a private dispatcher here — meaning the CLI carried its own copy and the two could
/// drift without anything failing. The decision now lives in `headroom-core`, so
/// `headroom compress` and `POST /v1/messages` route identically by construction.
pub struct Compressors {
    orchestrator: Orchestrator,
    /// Memories to inject into the live-zone tail, loaded once at startup.
    ///
    /// Empty unless configured, and an empty store injects nothing — so a proxy with no
    /// memory file behaves exactly as it did before injection existed.
    memories: headroom_core::memory::MemoryStore,
    /// How many memories one injection may carry.
    memory_limit: usize,
}

impl Compressors {
    /// Builds the set, sharing one CCR store between every compressor.
    pub fn new(store: Arc<dyn CcrStore>) -> Self {
        Self {
            orchestrator: Orchestrator::new(store),
            memories: Default::default(),
            memory_limit: crate::config::DEFAULT_MEMORY_LIMIT,
        }
    }

    /// Builds the set with recommendations learned from a previous run.
    pub fn with_recommendations(
        store: Arc<dyn CcrStore>,
        recommendations: headroom_core::telemetry::Recommendations,
    ) -> Self {
        Self {
            orchestrator: Orchestrator::new(store).with_recommendations(recommendations),
            memories: Default::default(),
            memory_limit: crate::config::DEFAULT_MEMORY_LIMIT,
        }
    }

    /// Attaches memories for live-zone injection.
    pub fn with_memories(
        mut self,
        memories: headroom_core::memory::MemoryStore,
        limit: usize,
    ) -> Self {
        self.memories = memories;
        self.memory_limit = limit;
        self
    }

    /// The transform for `content` under `policy`, if any applies.
    ///
    /// Invariant I10 is enforced inside the orchestrator: restricted traffic is routed
    /// the lossless reformatter, never a lossy compressor.
    fn route_block(
        &self,
        block: &headroom_core::block::Block,
        policy: CompressionPolicy,
        model: &str,
    ) -> Option<&dyn Transform> {
        self.orchestrator.transform_for_block(block, policy, model)
    }

    /// Why `content` was routed as it was, for telemetry.
    pub fn routing(&self, content: &str, policy: CompressionPolicy, model: &str) -> Routing {
        self.orchestrator.route(content, policy, model)
    }

    /// The tokenizer to measure `model` with.
    pub fn tokenizer_for(
        &self,
        model: &str,
    ) -> std::sync::Arc<dyn headroom_core::tokenizer::Tokenizer> {
        self.orchestrator.tokenizer_for(model)
    }
}

/// Which provider's request shape a body is written in.
///
/// The two differ in more than field names, and the difference that matters is how
/// each provider decides what to cache — see [`Dialect::frozen_floor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// `POST /v1/messages`. Caching is explicit, via customer `cache_control` markers.
    Anthropic,
    /// `POST /v1/chat/completions`. Caching is automatic and prefix-based.
    OpenAi,
    /// `POST /v1/responses`. Items in `input`, caching automatic and prefix-based.
    OpenAiResponses,
}

impl Dialect {
    /// The index below which messages are never eligible for compression.
    ///
    /// # Why the two providers need different answers
    ///
    /// Anthropic caches what the customer marks, so the floor is derived from their
    /// `cache_control` breakpoints and the absence of markers legitimately means
    /// nothing is pinned.
    ///
    /// OpenAI caches automatically: any sufficiently long prompt prefix is cached
    /// without anyone asking. Applying the Anthropic rule to an OpenAI body would read
    /// "no markers, so nothing is frozen" — which is exactly backwards, because
    /// *everything* the customer has already sent is a candidate prefix. So the floor
    /// is every message but the newest, and that is not a conservative guess so much
    /// as the only correct reading of automatic prefix caching.
    fn frozen_floor(self, body: &[u8], message_count: usize) -> usize {
        match self {
            Self::Anthropic => frozen_message_count(body),
            // Both OpenAI surfaces cache prefixes automatically, so the reasoning is
            // identical: everything already sent is a candidate cached prefix.
            Self::OpenAi | Self::OpenAiResponses => message_count.saturating_sub(1),
        }
    }
}

/// Compresses an Anthropic-shaped request body.
///
/// Returns the original bytes unchanged whenever compression does not apply or does
/// not help.
pub fn compress_request<'a>(
    body: &'a [u8],
    compressors: &Compressors,
    enabled: bool,
    policy: CompressionPolicy,
) -> Cow<'a, [u8]> {
    compress_dialect(
        Dialect::Anthropic,
        body,
        compressors,
        enabled,
        policy,
        Verbosity::Default,
    )
}

/// Compresses a request body written in `dialect`.
///
/// Returns the original bytes unchanged whenever compression does not apply or does
/// not help.
pub fn compress_dialect<'a>(
    dialect: Dialect,
    body: &'a [u8],
    compressors: &Compressors,
    enabled: bool,
    policy: CompressionPolicy,
    verbosity: Verbosity,
) -> Cow<'a, [u8]> {
    if !enabled {
        return Cow::Borrowed(body);
    }

    let faithful = FaithfulBody::parse(body);
    if !faithful.is_understood() {
        return Cow::Borrowed(body);
    }

    let frozen = dialect.frozen_floor(body, faithful.message_count());

    let Some((conversation, shapes)) = read_conversation(&faithful, dialect) else {
        return Cow::Borrowed(body);
    };

    let zone = live_zone(&conversation, frozen);
    if zone.is_empty() {
        return Cow::Borrowed(body);
    }

    // Decide everything before writing anything. `validated_apply` enforces I5 per
    // block, so a compressor that declines or fails to help leaves no trace here.
    //
    // The tokenizer is chosen by the request's own model. An exact count lets a
    // compressor keep a result the heuristic's over-count would have rejected, and the
    // heuristic remains the answer for any model without one — reporting an
    // approximation as a measurement would be worse than the approximation.
    let model = model_of(body);
    let estimator = compressors.tokenizer_for(model);
    let mut edits: Vec<(usize, usize, String)> = Vec::new();

    for location in zone.locations() {
        let Some(block) = conversation
            .messages()
            .get(location.message)
            .and_then(|m| m.blocks().get(location.block))
        else {
            continue;
        };
        // Block-aware: prose is compressed only when the block is tool output. The
        // prose compressor is lossy, and `BlockKind::Text` is what somebody typed.
        let Some(transform) = compressors.route_block(block, policy, model) else {
            continue;
        };

        let mut candidate = block.clone();
        match validated_apply(transform, &mut candidate, estimator.as_ref()) {
            Ok(outcome) if outcome.is_compressed() => {
                edits.push((
                    location.message,
                    location.block,
                    candidate.content().to_owned(),
                ));
            }
            // Declined, or not smaller. Either way the original stands.
            Ok(_) => {}
            // An invariant violation is a bug, not something to paper over — but it
            // must not take down a customer's request either. Drop the edit, log, and
            // forward what arrived.
            Err(err) => {
                tracing::warn!(%err, "compressor failed; forwarding original block");
            }
        }
    }

    // Memory injection, gated on the *lossy* permission rather than the lossless one.
    // Adding content is not a transform of anything: the lossless permission is granted
    // on OAuth because a meaning-preserving change cannot exceed a granted scope, and
    // injecting facts the client never sent plainly can. So this runs only where lossy
    // rewriting is already permitted — invariant I10.
    if policy.lossy_transforms {
        if let Some((message, block, injected)) = headroom_core::memory::inject_append(
            &conversation,
            &compressors.memories,
            compressors.memory_limit,
        ) {
            match edits
                .iter_mut()
                .find(|(m, b, _)| *m == message && *b == block)
            {
                // A compressor already rewrote this block. Append to *its* output, not
                // to the original, or the compression would be discarded.
                Some((_, _, content)) => {
                    if !content.contains("<memory>") {
                        if let Some(memories) = headroom_core::memory::inject_block(
                            &compressors.memories,
                            compressors.memory_limit,
                        ) {
                            content.push_str(&memories);
                        }
                    }
                }
                None => edits.push((message, block, injected)),
            }
        }
    }

    // Output shaping runs after compression, and after rather than before deliberately:
    // the note must survive into the bytes that go out, and a compressor running over a
    // block that already carries it could summarize the instruction away.
    if let Some((message, block, shaped)) =
        output_shaping::verbosity_append(&conversation, verbosity)
    {
        match edits
            .iter_mut()
            .find(|(m, b, _)| *m == message && *b == block)
        {
            // A compressor already rewrote this block. Append to *its* output, not to
            // the original, or the compression would be discarded.
            Some((_, _, content)) => {
                if let Some(note) = verbosity.note() {
                    content.push_str(note);
                }
            }
            None => edits.push((message, block, shaped)),
        }
    }

    if edits.is_empty() {
        return Cow::Borrowed(body);
    }

    // Rewrite only the messages that actually changed. Everything else — including
    // every frozen message — comes back as its original bytes.
    let mut replacements: Vec<(usize, String)> = Vec::new();
    for message_index in unique_message_indices(&edits) {
        let Some(raw) = faithful.message(message_index) else {
            continue;
        };
        let Some(shape) = shapes.get(message_index) else {
            continue;
        };
        let block_edits: Vec<(usize, &str)> = edits
            .iter()
            .filter(|(m, _, _)| *m == message_index)
            .map(|(_, b, content)| (*b, content.as_str()))
            .collect();

        if let Some(rewritten) = rewrite_message(raw, *shape, &block_edits) {
            replacements.push((message_index, rewritten));
        }
    }

    if replacements.is_empty() {
        return Cow::Borrowed(body);
    }

    faithful.rebuild(&replacements)
}

/// How a message carried its content, so it can be written back the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContentShape {
    /// `"content": "text"`.
    Scalar,
    /// `"content": [ ... ]`.
    Blocks,
    /// A Responses-API item whose payload lives in `output`.
    ResponsesOutput,
}

/// Builds a [`Conversation`] view for deciding what is live.
///
/// The conversation is only ever used to *decide*. Edits are applied back to the raw
/// JSON, so a message this view models imperfectly is still forwarded byte-exact
/// unless something explicitly changed it.
fn read_conversation(
    faithful: &FaithfulBody<'_>,
    dialect: Dialect,
) -> Option<(Conversation, Vec<ContentShape>)> {
    let mut messages = Vec::with_capacity(faithful.message_count());
    let mut shapes = Vec::with_capacity(faithful.message_count());

    for index in 0..faithful.message_count() {
        let raw = faithful.message(index)?;
        let value: Value = serde_json::from_str(raw).ok()?;

        let declared_role = value.get("role").and_then(Value::as_str);
        let role = match declared_role {
            Some("assistant") => Role::Assistant,
            // Anything else is treated as user-side. Tool results arrive in
            // user-role messages, and an unrecognized role should not accidentally
            // widen what is considered live.
            _ => Role::User,
        };

        // The Responses API carries a tool result as a standalone item with no `role`
        // at all: `{"type":"function_call_output","call_id":...,"output":"..."}`. Read
        // through the chat-completions path it has no content and no kind, so the
        // bulkiest item in a Responses conversation would never be compressed.
        if dialect == Dialect::OpenAiResponses {
            if let Some(kind) = responses_item_kind(&value) {
                let output = value
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                messages.push(Message::new(role, vec![Block::new(kind, output)]));
                shapes.push(ContentShape::ResponsesOutput);
                continue;
            }
        }

        let (blocks, shape) = match value.get("content") {
            Some(Value::String(text)) => {
                // OpenAI carries a tool result as a whole message with `role: "tool"`
                // and a plain string body, where Anthropic nests a typed block inside a
                // user message. Reading it as ordinary text would leave it out of the
                // live zone entirely, so the bulkiest thing in an OpenAI conversation —
                // the tool output — would never be compressed.
                let kind = if dialect == Dialect::OpenAi && declared_role == Some("tool") {
                    BlockKind::ToolResult
                } else {
                    BlockKind::Text
                };
                (vec![Block::new(kind, text.clone())], ContentShape::Scalar)
            }
            Some(Value::Array(items)) => {
                (items.iter().map(read_block).collect(), ContentShape::Blocks)
            }
            _ => (Vec::new(), ContentShape::Blocks),
        };

        messages.push(Message::new(role, blocks));
        shapes.push(shape);
    }

    Some((Conversation::new(None, Vec::new(), messages), shapes))
}

/// The block kind for a Responses-API item, if it is one this code compresses.
///
/// Only output-carrying items qualify. A `function_call` (the *request* to run a tool)
/// carries arguments the provider parses as JSON, and compressing those would produce
/// a call the provider rejects rather than a shorter one.
fn responses_item_kind(item: &Value) -> Option<BlockKind> {
    match item.get("type").and_then(Value::as_str)? {
        "function_call_output" => Some(BlockKind::FunctionCallOutput),
        "local_shell_call_output" => Some(BlockKind::LocalShellCallOutput),
        "apply_patch_call_output" => Some(BlockKind::ApplyPatchCallOutput),
        _ => None,
    }
}

/// Reads one content block.
fn read_block(item: &Value) -> Block {
    let kind = match item.get("type").and_then(Value::as_str) {
        Some("text") => BlockKind::Text,
        Some("tool_result") => BlockKind::ToolResult,
        Some("tool_use") => BlockKind::ToolUse,
        Some("thinking") => BlockKind::Thinking,
        Some("redacted_thinking") => BlockKind::RedactedThinking,
        Some("function_call_output") => BlockKind::FunctionCallOutput,
        Some("local_shell_call_output") => BlockKind::LocalShellCallOutput,
        Some("apply_patch_call_output") => BlockKind::ApplyPatchCallOutput,
        // Images, documents, and anything unrecognized. Attachment is not
        // compressible, so an unknown block type is excluded from the live zone —
        // the safe default for a type this code has never seen.
        _ => BlockKind::Attachment,
    };

    let content = match kind {
        BlockKind::Text => item.get("text").and_then(Value::as_str).unwrap_or_default(),
        _ => item
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    };

    Block::new(kind, content)
}

/// Applies block edits to one raw message, returning its new JSON text.
fn rewrite_message(raw: &str, shape: ContentShape, edits: &[(usize, &str)]) -> Option<String> {
    let mut value: Value = serde_json::from_str(raw).ok()?;

    match shape {
        ContentShape::ResponsesOutput => {
            // Written back to `output`, leaving `type` and `call_id` untouched — the
            // provider matches the result to its call by `call_id`, so losing it turns
            // a compressed tool result into an orphan the model cannot attribute.
            let (_, replacement) = edits.iter().find(|(index, _)| *index == 0)?;
            value
                .as_object_mut()?
                .insert("output".into(), Value::String((*replacement).to_owned()));
        }
        ContentShape::Scalar => {
            // A scalar message has exactly one conceptual block.
            let (_, replacement) = edits.iter().find(|(index, _)| *index == 0)?;
            value
                .as_object_mut()?
                .insert("content".into(), Value::String((*replacement).to_owned()));
        }
        ContentShape::Blocks => {
            let items = value.get_mut("content")?.as_array_mut()?;
            for (index, replacement) in edits {
                let item = items.get_mut(*index)?;
                let object = item.as_object_mut()?;
                // Write back to whichever field this block type reads from, leaving
                // every sibling field — `type`, `tool_use_id`, `is_error` — untouched.
                let field = if object.get("type").and_then(Value::as_str) == Some("text") {
                    "text"
                } else {
                    "content"
                };
                object.insert(field.into(), Value::String((*replacement).to_owned()));
            }
        }
    }

    serde_json::to_string(&value).ok()
}

/// The model identifier a request names, for tokenizer selection.
///
/// Read rather than rewritten, so the usual byte-faithfulness concern does not apply —
/// this parse never produces output. An absent or unreadable model yields an empty
/// string, which resolves to the heuristic.
fn model_of(body: &[u8]) -> &str {
    // A borrowed `&str` from the raw bytes rather than an owned `String`, since this is
    // called once per request and the value is used immediately.
    serde_json::from_slice::<&serde_json::value::RawValue>(body)
        .ok()
        .and_then(|_| {
            let text = std::str::from_utf8(body).ok()?;
            let start = text.find(r#""model""#)? + r#""model""#.len();
            let rest = text[start..].trim_start().strip_prefix(':')?.trim_start();
            let quoted = rest.strip_prefix('"')?;
            let end = quoted.find('"')?;
            Some(&quoted[..end])
        })
        .unwrap_or_default()
}

/// Message indices touched by `edits`, ascending and deduplicated.
fn unique_message_indices(edits: &[(usize, usize, String)]) -> Vec<usize> {
    let mut indices: Vec<usize> = edits.iter().map(|(m, _, _)| *m).collect();
    indices.sort_unstable();
    indices.dedup();
    indices
}

#[cfg(test)]
mod tests {
    use super::*;
    use headroom_core::ccr::InMemoryCcrStore;
    use headroom_core::tokenizer::{HeuristicEstimator, Tokenizer};
    use sha2::{Digest, Sha256};

    fn compressors() -> Compressors {
        Compressors::new(Arc::new(InMemoryCcrStore::new()))
    }

    fn payg() -> CompressionPolicy {
        CompressionPolicy::for_mode(headroom_core::AuthMode::PayAsYouGo)
    }

    fn sha(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    /// A bulky JSON tool result, the shape SmartCrusher exists for.
    fn bulky_tool_output() -> String {
        let records: Vec<String> = (0..120)
            .map(|i| {
                format!(
                    r#"{{"path":"src/module_{i}.rs","kind":"file","status":"ok","size":{}}}"#,
                    1000 + i
                )
            })
            .collect();
        format!("[{}]", records.join(",")).replace('"', "\\\"")
    }

    /// system + tools + 5 historical turns + a new bulky tool result.
    fn request() -> String {
        format!(
            r#"{{"model":"claude-opus-4","max_tokens":4096,"system":"You are a careful assistant.","tools":[{{"name":"read_file","input_schema":{{"type":"object","properties":{{"path":{{"type":"string"}}}}}}}}],"messages":[{{"role":"user","content":"turn one question"}},{{"role":"assistant","content":"turn one answer"}},{{"role":"user","content":"turn two question"}},{{"role":"assistant","content":"turn two answer"}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t_old","content":"small older result"}}]}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t_new","content":"{}"}}]}}]}}"#,
            bulky_tool_output()
        )
    }

    // ---- the headline guarantee ----

    #[test]
    fn the_hot_zone_and_every_frozen_turn_survive_byte_identical() {
        let source = request();
        let out = compress_request(source.as_bytes(), &compressors(), true, payg());
        let out = String::from_utf8(out.into_owned()).unwrap();

        // Something must actually have happened, or this passes vacuously.
        assert_ne!(out, source, "nothing was compressed");

        let before: Value = serde_json::from_str(&source).unwrap();
        let after: Value = serde_json::from_str(&out).unwrap();

        // The hot zone, hashed rather than compared loosely.
        for member in ["system", "tools", "model", "max_tokens"] {
            assert_eq!(
                sha(serde_json::to_string(&before[member]).unwrap().as_bytes()),
                sha(serde_json::to_string(&after[member]).unwrap().as_bytes()),
                "{member} was modified"
            );
        }

        // Every historical turn, byte for byte.
        for index in 0..5 {
            assert_eq!(
                sha(serde_json::to_string(&before["messages"][index])
                    .unwrap()
                    .as_bytes()),
                sha(serde_json::to_string(&after["messages"][index])
                    .unwrap()
                    .as_bytes()),
                "historical turn {index} was modified"
            );
        }
    }

    #[test]
    fn the_live_tool_result_measurably_shrinks() {
        let source = request();
        let out = compress_request(source.as_bytes(), &compressors(), true, payg());

        let estimator = HeuristicEstimator::new();
        let before = estimator.count(&source);
        let after = estimator.count(&String::from_utf8_lossy(&out));

        assert!(
            after < before / 2,
            "expected a real cut: {before} -> {after}"
        );
    }

    #[test]
    fn the_untouched_prefix_is_literally_the_original_bytes() {
        // Stronger than the parsed comparison above: the raw substring of the request
        // up to the live message must appear verbatim in the output.
        let source = request();
        let out = compress_request(source.as_bytes(), &compressors(), true, payg());
        let out = String::from_utf8(out.into_owned()).unwrap();

        let prefix_end = source
            .find(r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"t_new""#)
            .unwrap();
        assert!(
            out.starts_with(&source[..prefix_end]),
            "the frozen prefix was re-serialized rather than copied"
        );
    }

    // ---- passthrough cases ----

    #[test]
    fn compression_disabled_returns_the_original_bytes_untouched() {
        let source = request();
        let out = compress_request(source.as_bytes(), &compressors(), false, payg());
        assert!(matches!(out, Cow::Borrowed(_)), "should not have allocated");
        assert_eq!(sha(&out), sha(source.as_bytes()));
    }

    #[test]
    fn a_streaming_request_is_compressed_like_any_other() {
        // Regression against the version of this function that bailed out on
        // `"stream": true`. `stream` describes the *response* framing; the request body
        // arrived complete either way. Since streaming is the common agent case, the
        // bail-out exempted most real traffic from compression while every test kept
        // confirming that compression worked.
        let source =
            request().replace(r#""max_tokens":4096"#, r#""max_tokens":4096,"stream":true"#);
        let out = compress_request(source.as_bytes(), &compressors(), true, payg());

        assert_ne!(
            sha(&out),
            sha(source.as_bytes()),
            "a streaming request was left uncompressed"
        );

        // And the flag itself survives — compressing the body must not change what the
        // client asked the provider for.
        let parsed: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(parsed["stream"], Value::Bool(true));
    }

    #[test]
    fn a_streaming_request_still_protects_the_frozen_prefix() {
        // The guarantee has to hold on the path that now compresses more traffic, not
        // only on the one the original tests covered.
        let source =
            request().replace(r#""max_tokens":4096"#, r#""max_tokens":4096,"stream":true"#);
        let out = compress_request(source.as_bytes(), &compressors(), true, payg());
        let out = String::from_utf8(out.into_owned()).unwrap();

        let before: Value = serde_json::from_str(&source).unwrap();
        let after: Value = serde_json::from_str(&out).unwrap();

        for member in ["system", "tools", "model", "max_tokens"] {
            assert_eq!(before[member], after[member], "{member} was modified");
        }
        for index in 0..5 {
            assert_eq!(
                before["messages"][index], after["messages"][index],
                "historical turn {index} was modified"
            );
        }
    }

    #[test]
    fn a_malformed_body_forwards_untouched() {
        for source in [&b"{not json"[..], &b""[..], &b"{\"no\":\"messages\"}"[..]] {
            let out = compress_request(source, &compressors(), true, payg());
            assert_eq!(sha(&out), sha(source));
        }
    }

    #[test]
    fn a_request_with_nothing_worth_compressing_forwards_untouched() {
        let source = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
        let out = compress_request(source.as_bytes(), &compressors(), true, payg());
        assert_eq!(sha(&out), sha(source.as_bytes()));
    }

    #[test]
    fn a_cache_control_marker_on_the_last_message_freezes_it() {
        // The customer pinned everything. Nothing is live, so nothing is touched even
        // though the payload is bulky enough to be worth compressing.
        let source = request().replace(
            r#""tool_use_id":"t_new""#,
            r#""tool_use_id":"t_new","cache_control":{"type":"ephemeral"}"#,
        );
        // The marker sits on the block, and the block's message is the last one.
        let out = compress_request(source.as_bytes(), &compressors(), true, payg());
        assert_eq!(
            sha(&out),
            sha(source.as_bytes()),
            "a pinned request was modified"
        );
    }

    // ---- code compression (gap rows C11-C13) ----

    /// A source file large enough to clear the code size threshold.
    fn code_request() -> String {
        let code = concat!(
            "pub fn handle(input: &str) -> Result<String, Error> {\n",
            "    let parsed = parse(input)?;\n",
            "    let checked = validate(&parsed)?;\n",
            "    Ok(render(&checked))\n",
            "}\n"
        )
        .repeat(80);

        format!(
            r#"{{"model":"claude-opus-4","messages":[{{"role":"user","content":"a"}},{{"role":"assistant","content":"b"}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":{}}}]}}]}}"#,
            serde_json::to_string(&code).unwrap()
        )
    }

    #[test]
    fn the_proxy_compresses_code() {
        // It did not, for the whole life of the pipeline refactor. `Orchestrator` held
        // no code compressor, so every source file a tool returned was forwarded whole
        // — the largest single category of agent traffic, silently exempt.
        let source = code_request();
        let out = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Default,
        );

        assert!(
            out.len() < source.len(),
            "code was forwarded unchanged: {} -> {} bytes",
            source.len(),
            out.len()
        );
    }

    #[test]
    fn code_routing_agrees_with_the_orchestrator_the_cli_uses() {
        // `headroom compress --dry-run` exists to predict this. It used to carry its own
        // routing table with a code arm the orchestrator lacked, so it promised a saving
        // the proxy never delivered. One table now answers both.
        let code = concat!(
            "pub fn handle(input: &str) -> Result<String, Error> {\n",
            "    let parsed = parse(input)?;\n",
            "    Ok(render(&parsed))\n",
            "}\n"
        )
        .repeat(80);

        assert!(
            compressors().routing(&code, payg(), "").will_compress(),
            "the proxy would not compress content the CLI reports a saving for"
        );
    }

    // ---- memory injection (gap row Y3) ----

    /// A compressor set carrying two memories.
    fn with_memories() -> Compressors {
        use headroom_core::memory::{MemoryStore, Provenance};

        let mut store = MemoryStore::new();
        store.remember(
            "The auth module uses tokens, not sessions.",
            Provenance::new("reader", "s1"),
        );
        store.remember(
            "Tests live under crates/*/tests.",
            Provenance::new("reader", "s1"),
        );

        compressors().with_memories(store, 8)
    }

    #[test]
    fn memories_land_on_the_newest_user_message() {
        let source = prose_request();
        let out = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &with_memories(),
            true,
            payg(),
            Verbosity::Default,
        );
        let parsed: Value = serde_json::from_slice(&out).unwrap();

        let newest = parsed["messages"][2]["content"].as_str().unwrap();
        assert!(newest.starts_with("what does this function do?"));
        assert!(
            newest.contains("uses tokens"),
            "the memory block is missing: {newest}"
        );
    }

    #[test]
    fn memory_injection_never_touches_the_cached_prefix() {
        // Invariants I2 and I3. The system prompt heads the cached prefix and a memory
        // set is *designed* to change, so writing there would bust the cache on every
        // turn that learned something — paying full price for the whole conversation in
        // exchange for a sentence.
        let source = prose_request();
        let out = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &with_memories(),
            true,
            payg(),
            Verbosity::Default,
        );

        let before: Value = serde_json::from_str(&source).unwrap();
        let after: Value = serde_json::from_slice(&out).unwrap();

        for member in ["system", "tools", "model"] {
            assert_eq!(before[member], after[member], "{member} was modified");
        }
        assert_eq!(
            before["messages"][0], after["messages"][0],
            "a frozen turn was modified"
        );
        assert_eq!(before["messages"][1], after["messages"][1]);
    }

    #[test]
    fn an_empty_memory_store_leaves_the_request_byte_identical() {
        // Invariant I1, and the reason this feature is safe to ship on by default: a
        // proxy with no memory file configured must be indistinguishable from one built
        // before injection existed.
        let source = prose_request();
        let out = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Default,
        );

        assert_eq!(sha(&out), sha(source.as_bytes()));
    }

    #[test]
    fn a_restricted_policy_injects_nothing() {
        // Invariant I10. Injection adds content the client never sent, which is not a
        // transform of anything — so the lossless permission does not cover it and only
        // pay-as-you-go traffic gets it.
        let source = prose_request();
        let restricted = CompressionPolicy::for_mode(headroom_core::AuthMode::Subscription);
        let out = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &with_memories(),
            true,
            restricted,
            Verbosity::Default,
        );

        assert_eq!(sha(&out), sha(source.as_bytes()));
    }

    #[test]
    fn memories_are_not_injected_twice_across_turns() {
        // An agent loop calls this every turn. Without the guard a long session
        // accumulates the same facts a dozen times over — wasted tokens, and a worse
        // prompt, since repetition reads as emphasis.
        let source = prose_request();
        let once = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &with_memories(),
            true,
            payg(),
            Verbosity::Default,
        );
        let twice = compress_dialect(
            Dialect::Anthropic,
            &once,
            &with_memories(),
            true,
            payg(),
            Verbosity::Default,
        );

        assert_eq!(
            sha(&twice),
            sha(&once),
            "the memory block was injected again"
        );
    }

    #[test]
    fn injection_is_deterministic() {
        // Invariant I4. These bytes go upstream, so an injection order that varied would
        // bust the very cache the live-zone placement exists to protect.
        let source = prose_request();
        let first = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &with_memories(),
            true,
            payg(),
            Verbosity::Default,
        );

        for _ in 0..25 {
            let again = compress_dialect(
                Dialect::Anthropic,
                source.as_bytes(),
                &with_memories(),
                true,
                payg(),
                Verbosity::Default,
            );
            assert_eq!(sha(&again), sha(&first));
        }
    }

    // ---- output shaping ----

    /// A request whose newest message is plain prose, so the note has somewhere to go.
    fn prose_request() -> String {
        r#"{"model":"claude-opus-4","system":"You are a careful assistant.","tools":[{"name":"read_file"}],"messages":[{"role":"user","content":"turn one"},{"role":"assistant","content":"reply one"},{"role":"user","content":"what does this function do?"}]}"#.to_owned()
    }

    #[test]
    fn the_terseness_note_never_touches_the_cached_prefix() {
        // The headline constraint. The system prompt is the first thing in the cached
        // prefix, so a note appended there invalidates the whole cache on every
        // request — saving a couple of hundred output tokens while re-billing tens of
        // thousands of input ones, and moving the metric people watch in the wrong
        // direction invisibly.
        let source = prose_request();
        let out = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Terse,
        );
        let out = String::from_utf8(out.into_owned()).unwrap();

        let before: Value = serde_json::from_str(&source).unwrap();
        let after: Value = serde_json::from_str(&out).unwrap();

        for member in ["system", "tools", "model"] {
            assert_eq!(before[member], after[member], "{member} was modified");
        }
        assert_eq!(
            before["messages"][0], after["messages"][0],
            "a frozen turn was modified"
        );
        assert_eq!(before["messages"][1], after["messages"][1]);
    }

    #[test]
    fn the_terseness_note_lands_on_the_newest_user_message() {
        let source = prose_request();
        let out = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Terse,
        );
        let parsed: Value = serde_json::from_slice(&out).unwrap();

        let newest = parsed["messages"][2]["content"].as_str().unwrap();
        assert!(newest.starts_with("what does this function do?"));
        assert!(newest.contains("briefly"), "the note is missing: {newest}");
    }

    #[test]
    fn the_default_verbosity_leaves_the_body_byte_identical() {
        // Output shaping changes what the model *writes*, which is a visible change to
        // the customer's product rather than an invisible saving. Off unless asked for.
        let source = prose_request();
        let out = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Default,
        );

        assert_eq!(sha(&out), sha(source.as_bytes()));
        assert!(matches!(out, Cow::Borrowed(_)), "should not have rebuilt");
    }

    #[test]
    fn shaping_does_not_discard_a_compressors_work() {
        // Both want the same block when the newest message is a bulky tool result the
        // compressor rewrote. Appending to the *original* rather than to the compressed
        // output would silently throw the compression away.
        let source = request();
        let out = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Terse,
        );

        let estimator = HeuristicEstimator::new();
        let before = estimator.count(&source);
        let after = estimator.count(&String::from_utf8_lossy(&out));
        assert!(
            after < before / 2,
            "compression was discarded by shaping: {before} -> {after}"
        );
    }

    #[test]
    fn shaping_is_deterministic() {
        // Invariant I4 — the note must not depend on anything but the input.
        let source = prose_request();
        let first = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Terse,
        )
        .into_owned();

        for _ in 0..20 {
            let again = compress_dialect(
                Dialect::Anthropic,
                source.as_bytes(),
                &compressors(),
                true,
                payg(),
                Verbosity::Terse,
            )
            .into_owned();
            assert_eq!(sha(&again), sha(&first));
        }
    }

    #[test]
    fn shaping_twice_does_not_append_the_note_twice() {
        // Invariant I3, and the practical failure it prevents: an agent loop that
        // accumulates the same instruction a dozen times over a long session.
        let source = prose_request();
        let once = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Terse,
        )
        .into_owned();
        let twice = compress_dialect(
            Dialect::Anthropic,
            &once,
            &compressors(),
            true,
            payg(),
            Verbosity::Terse,
        )
        .into_owned();

        assert_eq!(sha(&twice), sha(&once));
    }

    // ---- tokenizer selection ----

    #[test]
    fn the_model_in_the_request_selects_the_tokenizer() {
        // The wiring that makes gap row T2 take effect. Without it the exact
        // vocabularies are compiled in and never consulted.
        let compressors = compressors();

        assert!(compressors.tokenizer_for("gpt-4o").is_exact());
        assert_eq!(compressors.tokenizer_for("gpt-4o").name(), "o200k_base");
    }

    #[test]
    fn a_model_with_no_exact_tokenizer_keeps_the_heuristic() {
        // An OpenAI vocabulary applied to an Anthropic model would be a wrong count
        // reported as exact, which is worse than an honest upper bound.
        let compressors = compressors();

        for model in ["claude-opus-4", "gemini-2.5-pro", "", "something-new"] {
            assert!(
                !compressors.tokenizer_for(model).is_exact(),
                "{model:?} claimed an exact tokenizer"
            );
        }
    }

    #[test]
    fn the_model_is_read_from_the_request_body() {
        assert_eq!(model_of(br#"{"model":"gpt-4o","messages":[]}"#), "gpt-4o");
        assert_eq!(
            model_of(br#"{"messages":[], "model" : "claude-opus-4" }"#),
            "claude-opus-4"
        );
    }

    #[test]
    fn a_body_with_no_model_resolves_to_the_heuristic() {
        // Wrong in the safe direction: no model means no exact vocabulary to claim.
        for body in [
            &br#"{"messages":[]}"#[..],
            &b"{not json"[..],
            &b""[..],
            &br#"{"model":123}"#[..],
        ] {
            assert_eq!(model_of(body), "", "{body:?}");
            assert!(!compressors().tokenizer_for(model_of(body)).is_exact());
        }
    }

    #[test]
    fn compression_still_holds_every_invariant_with_an_exact_tokenizer() {
        // The exact count accepts results the heuristic would have rejected, so the
        // guarantees have to be re-checked on that path rather than assumed to carry.
        let source = request().replace(r#""model":"claude-opus-4""#, r#""model":"gpt-4o""#);
        let out = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Default,
        );

        let before: Value = serde_json::from_str(&source).unwrap();
        let after: Value = serde_json::from_slice(&out).unwrap();

        assert_eq!(before["system"], after["system"], "I2: hot zone modified");
        assert_eq!(before["tools"], after["tools"]);
        for index in 0..5 {
            assert_eq!(
                before["messages"][index], after["messages"][index],
                "I2: frozen turn {index} modified"
            );
        }

        // I4, on the exact path.
        let again = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Default,
        );
        assert_eq!(sha(&out), sha(&again));
    }

    // ---- invariants ----

    #[test]
    fn sacrosanct_blocks_are_never_touched() {
        let source = format!(
            r#"{{"messages":[{{"role":"assistant","content":[{{"type":"thinking","thinking":"...","signature":"sig","content":"{}"}}]}}]}}"#,
            bulky_tool_output()
        );
        let out = compress_request(source.as_bytes(), &compressors(), true, payg());
        assert_eq!(sha(&out), sha(source.as_bytes()));
    }

    #[test]
    fn a_restricted_policy_forwards_everything_untouched() {
        // Invariant I10 at the dispatch point. Every compressor wired here is lossy,
        // so subscription-mode traffic must come back byte-identical however
        // compressible the payload looks.
        let source = request();
        let restricted = CompressionPolicy::for_mode(headroom_core::AuthMode::Subscription);
        let out = compress_request(source.as_bytes(), &compressors(), true, restricted);

        assert_eq!(sha(&out), sha(source.as_bytes()));
        assert!(matches!(out, Cow::Borrowed(_)), "should not have rebuilt");
    }

    #[test]
    fn oauth_traffic_now_gets_lossless_compression() {
        // Before this, OAuth traffic received *no* compression at all: every wired
        // compressor was lossy, so the policy routed nothing. A meaning-preserving
        // reformat cannot exceed the granted scope, which is the OAuth hazard — so
        // OAuth now means less compression rather than none.
        let records: Vec<String> = (0..120)
            .map(|i| format!(r#"{{ \"path\" : \"src/m_{i}.rs\" , \"size\" : {i} }}"#))
            .collect();
        let source = format!(
            r#"{{"model":"claude-opus-4","messages":[{{"role":"user","content":"q"}},{{"role":"assistant","content":"a"}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t","content":"[ {} ]"}}]}}]}}"#,
            records.join(" , ")
        );

        let oauth = CompressionPolicy::for_mode(headroom_core::AuthMode::OAuth);
        let out = compress_request(source.as_bytes(), &compressors(), true, oauth);

        assert!(
            out.len() < source.len(),
            "OAuth traffic got nothing: {} -> {}",
            source.len(),
            out.len()
        );

        // And subscription still gets nothing, deliberately — reflowing bytes is a
        // fingerprint change, whatever it does to the meaning.
        let subscription = CompressionPolicy::for_mode(headroom_core::AuthMode::Subscription);
        let out = compress_request(source.as_bytes(), &compressors(), true, subscription);
        assert_eq!(out.as_ref(), source.as_bytes());
    }

    #[test]
    fn lossless_compression_does_not_change_what_the_model_reads() {
        // The property that makes it safe on restricted traffic at all. Checked by
        // comparing the *decoded* tool result, not the bytes — the bytes are supposed
        // to differ.
        let records: Vec<String> = (0..120)
            .map(|i| format!(r#"{{ \"path\" : \"src/m_{i}.rs\" , \"size\" : {i} }}"#))
            .collect();
        let source = format!(
            r#"{{"model":"claude-opus-4","messages":[{{"role":"user","content":"q"}},{{"role":"assistant","content":"a"}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t","content":"[ {} ]"}}]}}]}}"#,
            records.join(" , ")
        );

        let oauth = CompressionPolicy::for_mode(headroom_core::AuthMode::OAuth);
        let out = compress_request(source.as_bytes(), &compressors(), true, oauth);

        let before: Value = serde_json::from_str(&source).unwrap();
        let after: Value = serde_json::from_slice(&out).unwrap();

        let decode = |v: &Value| -> Value {
            serde_json::from_str(v["messages"][2]["content"][0]["content"].as_str().unwrap())
                .unwrap()
        };
        assert_eq!(
            decode(&before),
            decode(&after),
            "the reformatter changed the decoded content"
        );

        // And the frozen prefix is still untouched, lossless or not.
        assert_eq!(before["messages"][0], after["messages"][0]);
        assert_eq!(before["system"], after["system"]);
    }

    #[test]
    fn an_oauth_policy_still_forbids_the_lossy_compressors() {
        // Lossless reformatting is permitted on OAuth; lossy compression is not. This
        // fixture is already minified, so the reformatter declines and the body comes
        // back byte-identical — which is what proves no *lossy* compressor ran.
        let source = request();
        let oauth = CompressionPolicy::for_mode(headroom_core::AuthMode::OAuth);
        let out = compress_request(source.as_bytes(), &compressors(), true, oauth);
        assert_eq!(sha(&out), sha(source.as_bytes()));
    }

    #[test]
    fn compression_is_deterministic() {
        // Invariant I4, end to end through the proxy path.
        let source = request();
        let first = compress_request(source.as_bytes(), &compressors(), true, payg()).into_owned();
        for _ in 0..20 {
            let again =
                compress_request(source.as_bytes(), &compressors(), true, payg()).into_owned();
            assert_eq!(sha(&again), sha(&first));
        }
    }

    #[test]
    fn compressing_twice_is_stable() {
        // Invariant I3. A second pass over already-compressed output must not reach
        // further back than the first did.
        let source = request();
        let once = compress_request(source.as_bytes(), &compressors(), true, payg()).into_owned();
        let twice = compress_request(&once, &compressors(), true, payg()).into_owned();
        assert_eq!(sha(&twice), sha(&once));
    }

    #[test]
    fn the_output_remains_valid_json_with_its_structure_intact() {
        let source = request();
        let out = compress_request(source.as_bytes(), &compressors(), true, payg());
        let parsed: Value = serde_json::from_slice(&out).expect("valid json");

        assert_eq!(parsed["messages"].as_array().unwrap().len(), 6);
        // Sibling fields on the rewritten block survive.
        let block = &parsed["messages"][5]["content"][0];
        assert_eq!(block["type"], "tool_result");
        assert_eq!(block["tool_use_id"], "t_new");
    }
}
