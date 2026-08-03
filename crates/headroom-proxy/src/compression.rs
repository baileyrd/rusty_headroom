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
//! Compression disabled, a streaming request, an unparseable body, no live zone, a
//! compressor that declines, a result that is not smaller — all of them forward the
//! original bytes. There is no input for which this function errors; the worst case
//! is that it does nothing.

use std::borrow::Cow;
use std::sync::Arc;

use headroom_core::block::{Block, BlockKind};
use headroom_core::ccr::CcrStore;
use headroom_core::conversation::{Conversation, Message, Role};
use headroom_core::detection::{detect, ContentType};
use headroom_core::live_zone::live_zone;
use headroom_core::tokenizer::HeuristicEstimator;
use headroom_core::validate::validated_apply;
use headroom_core::{LogCompressor, SearchCompressor, SmartCrusher, Transform};
use serde_json::Value;

use crate::body::FaithfulBody;
use crate::frozen::frozen_message_count;

/// The type-aware compressors, dispatched by content detection.
///
/// A local dispatcher rather than the general pipeline orchestrator, which is its own
/// unimplemented gap row. When that lands this moves to `headroom-core` and the proxy
/// calls it instead.
pub struct Compressors {
    smart_crusher: SmartCrusher,
    log: LogCompressor,
    search: SearchCompressor,
}

impl Compressors {
    /// Builds the set, sharing one CCR store between them.
    pub fn new(store: Arc<dyn CcrStore>) -> Self {
        Self {
            smart_crusher: SmartCrusher::new(store.clone()),
            log: LogCompressor::new(store.clone()),
            search: SearchCompressor::new(store),
        }
    }

    /// The compressor for `content`, if any handles its type.
    fn route(&self, content: &str) -> Option<&dyn Transform> {
        match detect(content.as_bytes()).content_type {
            ContentType::Json => Some(&self.smart_crusher),
            ContentType::Log => Some(&self.log),
            ContentType::SearchResults => Some(&self.search),
            // Diffs, code, and prose have no compressor yet, and `Unknown` never
            // gets one. Returning `None` forwards the block unchanged.
            _ => None,
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
) -> Cow<'a, [u8]> {
    if !enabled || is_streaming(body) {
        return Cow::Borrowed(body);
    }

    let faithful = FaithfulBody::parse(body);
    if !faithful.is_understood() {
        return Cow::Borrowed(body);
    }

    let frozen = frozen_message_count(body);

    let Some((conversation, shapes)) = read_conversation(&faithful) else {
        return Cow::Borrowed(body);
    };

    let zone = live_zone(&conversation, frozen);
    if zone.is_empty() {
        return Cow::Borrowed(body);
    }

    // Decide everything before writing anything. `validated_apply` enforces I5 per
    // block, so a compressor that declines or fails to help leaves no trace here.
    let estimator = HeuristicEstimator::new();
    let mut edits: Vec<(usize, usize, String)> = Vec::new();

    for location in zone.locations() {
        let Some(block) = conversation
            .messages()
            .get(location.message)
            .and_then(|m| m.blocks().get(location.block))
        else {
            continue;
        };
        let Some(transform) = compressors.route(block.content()) else {
            continue;
        };

        let mut candidate = block.clone();
        match validated_apply(transform, &mut candidate, &estimator) {
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
}

/// Whether the request asked for a streaming response.
///
/// Streaming is passthrough until the SSE state machine exists. Buffering a stream to
/// compress it would break the very thing the client asked for, and doing it badly is
/// worse than not doing it.
fn is_streaming(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("stream").and_then(Value::as_bool))
        .unwrap_or(false)
}

/// Builds a [`Conversation`] view for deciding what is live.
///
/// The conversation is only ever used to *decide*. Edits are applied back to the raw
/// JSON, so a message this view models imperfectly is still forwarded byte-exact
/// unless something explicitly changed it.
fn read_conversation(faithful: &FaithfulBody<'_>) -> Option<(Conversation, Vec<ContentShape>)> {
    let mut messages = Vec::with_capacity(faithful.message_count());
    let mut shapes = Vec::with_capacity(faithful.message_count());

    for index in 0..faithful.message_count() {
        let raw = faithful.message(index)?;
        let value: Value = serde_json::from_str(raw).ok()?;

        let role = match value.get("role").and_then(Value::as_str) {
            Some("assistant") => Role::Assistant,
            // Anything else is treated as user-side. Tool results arrive in
            // user-role messages, and an unrecognized role should not accidentally
            // widen what is considered live.
            _ => Role::User,
        };

        let (blocks, shape) = match value.get("content") {
            Some(Value::String(text)) => (
                vec![Block::new(BlockKind::Text, text.clone())],
                ContentShape::Scalar,
            ),
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
    use headroom_core::tokenizer::Tokenizer;
    use sha2::{Digest, Sha256};

    fn compressors() -> Compressors {
        Compressors::new(Arc::new(InMemoryCcrStore::new()))
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
        let out = compress_request(source.as_bytes(), &compressors(), true);
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
        let out = compress_request(source.as_bytes(), &compressors(), true);

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
        let out = compress_request(source.as_bytes(), &compressors(), true);
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
        let out = compress_request(source.as_bytes(), &compressors(), false);
        assert!(matches!(out, Cow::Borrowed(_)), "should not have allocated");
        assert_eq!(sha(&out), sha(source.as_bytes()));
    }

    #[test]
    fn a_streaming_request_forwards_untouched() {
        // SSE handling does not exist yet. Buffering a stream to compress it would
        // break exactly what the client asked for.
        let source =
            request().replace(r#""max_tokens":4096"#, r#""max_tokens":4096,"stream":true"#);
        let out = compress_request(source.as_bytes(), &compressors(), true);
        assert_eq!(sha(&out), sha(source.as_bytes()));
    }

    #[test]
    fn a_malformed_body_forwards_untouched() {
        for source in [&b"{not json"[..], &b""[..], &b"{\"no\":\"messages\"}"[..]] {
            let out = compress_request(source, &compressors(), true);
            assert_eq!(sha(&out), sha(source));
        }
    }

    #[test]
    fn a_request_with_nothing_worth_compressing_forwards_untouched() {
        let source = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
        let out = compress_request(source.as_bytes(), &compressors(), true);
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
        let out = compress_request(source.as_bytes(), &compressors(), true);
        assert_eq!(
            sha(&out),
            sha(source.as_bytes()),
            "a pinned request was modified"
        );
    }

    // ---- invariants ----

    #[test]
    fn sacrosanct_blocks_are_never_touched() {
        let source = format!(
            r#"{{"messages":[{{"role":"assistant","content":[{{"type":"thinking","thinking":"...","signature":"sig","content":"{}"}}]}}]}}"#,
            bulky_tool_output()
        );
        let out = compress_request(source.as_bytes(), &compressors(), true);
        assert_eq!(sha(&out), sha(source.as_bytes()));
    }

    #[test]
    fn compression_is_deterministic() {
        // Invariant I4, end to end through the proxy path.
        let source = request();
        let first = compress_request(source.as_bytes(), &compressors(), true).into_owned();
        for _ in 0..20 {
            let again = compress_request(source.as_bytes(), &compressors(), true).into_owned();
            assert_eq!(sha(&again), sha(&first));
        }
    }

    #[test]
    fn compressing_twice_is_stable() {
        // Invariant I3. A second pass over already-compressed output must not reach
        // further back than the first did.
        let source = request();
        let once = compress_request(source.as_bytes(), &compressors(), true).into_owned();
        let twice = compress_request(&once, &compressors(), true).into_owned();
        assert_eq!(sha(&twice), sha(&once));
    }

    #[test]
    fn the_output_remains_valid_json_with_its_structure_intact() {
        let source = request();
        let out = compress_request(source.as_bytes(), &compressors(), true);
        let parsed: Value = serde_json::from_slice(&out).expect("valid json");

        assert_eq!(parsed["messages"].as_array().unwrap().len(), 6);
        // Sibling fields on the rewritten block survive.
        let block = &parsed["messages"][5]["content"][0];
        assert_eq!(block["type"], "tool_result");
        assert_eq!(block["tool_use_id"], "t_new");
    }
}
