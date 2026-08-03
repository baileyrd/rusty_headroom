//! OpenAI-shaped routes.
//!
//! `POST /v1/chat/completions` compresses the live zone the same way the Anthropic
//! route does; the machinery is shared and only the dialect differs. What is *not*
//! shared is the caching model, and that difference is the whole reason this module
//! exists rather than a second call into the same handler with a different path
//! string — see [`crate::compression::Dialect`].
//!
//! # Routes that are passthrough on purpose
//!
//! `/v1/conversations` and `/v1/responses/compact` are relayed untouched. They are not
//! unimplemented — the reference architecture declares them explicitly
//! non-compressible, because both are *about* conversation state rather than being a
//! prompt. Compressing a request whose job is to tell the provider what the
//! conversation contains would corrupt the provider's own record of it.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Uri};
use axum::response::Response;

use crate::body::{insert_top_level_member, FaithfulBody};
use crate::compression::{compress_dialect, Dialect};
use crate::config::Config;
use crate::headers::{sanitize, HeaderPolicy};
use crate::server::{relay, AppState};
use crate::upstream::RelayError;
use headroom_core::auth_mode::{classify_auth_mode, CompressionPolicy};
use headroom_core::block::{Block, BlockKind};
use headroom_core::ccr::ContentHash;
use headroom_core::conversation::{Conversation, Message, Role};
use headroom_core::output_shaping::{route_effort, Effort};
use headroom_core::tokenizer::{HeuristicEstimator, Tokenizer};

/// `POST /v1/chat/completions`.
///
/// # Errors
///
/// Returns [`RelayError`] if the request cannot be relayed to the provider.
pub async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, RelayError> {
    let config = Config::from_env();
    let auth_mode = classify_auth_mode(&headers);
    let policy = CompressionPolicy::for_mode(auth_mode);

    let compressed = compress_dialect(
        Dialect::OpenAi,
        &body,
        state.compressors(),
        config.compression_enabled(),
        policy,
        config.verbosity(),
    );

    // Measured against the bytes that actually leave, before the cache key is added —
    // the key is a routing hint, not compression, and counting it as savings would
    // flatter the number.
    if compressed.as_ref() == body.as_ref() {
        state.metrics().record_passthrough();
    } else {
        let estimator = HeuristicEstimator::new();
        state.metrics().record_compressed(
            estimator.count(&String::from_utf8_lossy(&body)) as u64,
            estimator.count(&String::from_utf8_lossy(&compressed)) as u64,
        );
    }

    let outgoing = shape_openai(&compressed, policy);

    relay_to(&state, &headers, "/v1/chat/completions", outgoing, policy).await
}

/// Adds `prompt_cache_key` and `reasoning_effort` where policy permits.
///
/// Both go in through [`insert_top_level_member`], so every byte the customer sent
/// survives and only the new members are added — a `Value` round-trip to append one
/// field would rewrite the whole body and cost the cache miss this proxy exists to
/// avoid.
fn shape_openai(body: &[u8], policy: CompressionPolicy) -> Vec<u8> {
    let mut outgoing = body.to_vec();

    if policy.auto_prompt_cache_key {
        if let Some(key) = cache_key_for(&outgoing) {
            if let Some(with_key) =
                insert_top_level_member(&outgoing, "prompt_cache_key", &format!("\"{key}\""))
            {
                outgoing = with_key;
            }
        }
    }

    // Effort routing (gap row O2). Only ever *added*, never adjusted — a
    // customer-supplied `reasoning_effort` is a deliberate choice about answer quality,
    // and overriding it is not a compression decision.
    if policy.lossy_transforms {
        if let Some(effort) = effort_for(&outgoing) {
            if let Some(with_effort) = insert_top_level_member(
                &outgoing,
                "reasoning_effort",
                &format!("\"{}\"", effort.as_openai()),
            ) {
                outgoing = with_effort;
            }
        }
    }

    outgoing
}

/// Derives a stable cache key from everything but the newest message.
///
/// # Why the newest message is excluded
///
/// The key names a cache *partition*. It has to be identical across the turns of one
/// conversation or every turn lands in a fresh partition and nothing is ever reused —
/// which is worse than sending no key at all, because it also fragments the provider's
/// own automatic prefix cache. The newest message is the one thing that changes every
/// turn, so including it guarantees the key never repeats.
fn cache_key_for(body: &[u8]) -> Option<String> {
    let faithful = FaithfulBody::parse(body);
    if !faithful.is_understood() || faithful.message_count() < 2 {
        // One message is not yet a conversation, and there is no prefix to partition.
        return None;
    }

    let mut prefix = String::new();
    for index in 0..faithful.message_count() - 1 {
        prefix.push_str(faithful.message(index)?);
    }

    // 32 hex characters of a 16-byte hash: enough that two distinct
    // conversations colliding is not a practical concern, and short enough to read
    // in a log line.
    Some(ContentHash::of(prefix.as_bytes()).to_hex())
}

/// The effort level for a request body, if one can be read from it.
fn effort_for(body: &[u8]) -> Option<Effort> {
    let faithful = FaithfulBody::parse(body);
    if !faithful.is_understood() {
        return None;
    }

    let mut messages = Vec::with_capacity(faithful.message_count());
    for index in 0..faithful.message_count() {
        let value: serde_json::Value = serde_json::from_str(faithful.message(index)?).ok()?;
        let role = match value.get("role").and_then(serde_json::Value::as_str) {
            Some("assistant") => Role::Assistant,
            _ => Role::User,
        };
        let text = value
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        messages.push(Message::new(role, vec![Block::new(BlockKind::Text, text)]));
    }

    Some(route_effort(&Conversation::new(None, Vec::new(), messages)))
}

/// `POST /v1/responses`.
///
/// Compressed on the same terms as chat completions. The body carries `input` rather
/// than `messages`, which [`crate::body::FaithfulBody`] does not model, so in practice
/// this currently relays untouched — stated plainly rather than implied by silence.
///
/// # Errors
///
/// Returns [`RelayError`] if the request cannot be relayed to the provider.
pub async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, RelayError> {
    let config = Config::from_env();
    let policy = CompressionPolicy::for_mode(classify_auth_mode(&headers));

    let compressed = compress_dialect(
        Dialect::OpenAi,
        &body,
        state.compressors(),
        config.compression_enabled(),
        policy,
        config.verbosity(),
    );

    if compressed.as_ref() == body.as_ref() {
        state.metrics().record_passthrough();
    }

    relay_to(
        &state,
        &headers,
        "/v1/responses",
        compressed.into_owned(),
        policy,
    )
    .await
}

/// Relays a request that must never be compressed.
///
/// Covers `/v1/conversations` and `/v1/responses/compact`. Both describe conversation
/// *state* rather than carrying a prompt, so compressing one would corrupt the
/// provider's own record of the conversation rather than merely shortening a message.
///
/// # Errors
///
/// Returns [`RelayError`] if the request cannot be relayed to the provider.
pub async fn passthrough(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, RelayError> {
    let policy = CompressionPolicy::for_mode(classify_auth_mode(&headers));
    state.metrics().record_passthrough();

    // The path is taken from the request rather than hard-coded, so
    // `/v1/conversations/{id}/items` reaches the provider at the path the client used
    // instead of being collapsed to its prefix.
    let path = uri.path().to_owned();
    relay_to(&state, &headers, &path, body.to_vec(), policy).await
}

/// Shared relay tail: sanitize headers, forward, stream the answer back.
async fn relay_to(
    state: &AppState,
    headers: &HeaderMap,
    path: &str,
    body: Vec<u8>,
    policy: CompressionPolicy,
) -> Result<Response, RelayError> {
    let upstream_headers = sanitize(
        headers,
        HeaderPolicy {
            forwarded_headers: policy.forwarded_headers,
            strip_accept_encoding: policy.may_strip_accept_encoding,
        },
    );

    relay(state, Method::POST, path, &upstream_headers, body).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compression::Compressors;
    use headroom_core::ccr::InMemoryCcrStore;
    use headroom_core::output_shaping::Verbosity;
    use std::sync::Arc;

    fn compressors() -> Compressors {
        Compressors::new(Arc::new(InMemoryCcrStore::new()))
    }

    fn payg() -> CompressionPolicy {
        CompressionPolicy::for_mode(headroom_core::AuthMode::PayAsYouGo)
    }

    /// A bulky JSON tool result, escaped for embedding in a JSON string.
    fn bulky() -> String {
        let records: Vec<String> = (0..120)
            .map(|i| {
                format!(
                    r#"{{\"path\":\"src/module_{i}.rs\",\"kind\":\"file\",\"status\":\"ok\",\"size\":{}}}"#,
                    1000 + i
                )
            })
            .collect();
        format!("[{}]", records.join(","))
    }

    /// An OpenAI chat request whose newest message is a bulky tool result.
    fn chat_request() -> String {
        format!(
            r#"{{"model":"gpt-4o","messages":[{{"role":"system","content":"You are careful."}},{{"role":"user","content":"list the files"}},{{"role":"assistant","content":"ok"}},{{"role":"tool","tool_call_id":"call_1","content":"{}"}}]}}"#,
            bulky()
        )
    }

    #[test]
    fn an_openai_tool_message_is_recognized_and_compressed() {
        // OpenAI carries a tool result as a whole message with `role: "tool"` and a
        // plain string body, where Anthropic nests a typed block inside a user message.
        // Read as ordinary text it falls outside the live zone, so the bulkiest thing
        // in the conversation would never be compressed.
        let source = chat_request();
        let out = compress_dialect(
            Dialect::OpenAi,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Default,
        );

        assert!(
            out.len() < source.len(),
            "an OpenAI tool result was left uncompressed: {} -> {}",
            source.len(),
            out.len()
        );
    }

    #[test]
    fn every_message_but_the_newest_survives_byte_identical() {
        // OpenAI caches prompt prefixes automatically, so every earlier message is a
        // candidate cached prefix whether or not anyone marked it.
        let source = chat_request();
        let out = compress_dialect(
            Dialect::OpenAi,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Default,
        );
        let out = String::from_utf8(out.into_owned()).unwrap();

        let before: serde_json::Value = serde_json::from_str(&source).unwrap();
        let after: serde_json::Value = serde_json::from_str(&out).unwrap();

        for index in 0..3 {
            assert_eq!(
                before["messages"][index], after["messages"][index],
                "message {index} was modified"
            );
        }
        assert_eq!(before["model"], after["model"]);
    }

    #[test]
    fn the_openai_floor_does_not_depend_on_cache_control_markers() {
        // The regression this guards: reusing the Anthropic floor reads "no markers, so
        // nothing is frozen" on an OpenAI body — exactly backwards for a provider that
        // caches prefixes without being asked.
        //
        // Two messages, the older one bulky and the newer one trivial. Under the
        // Anthropic rule the floor is 0; under the correct rule it is 1, and the bulky
        // history is off limits.
        let source = format!(
            r#"{{"model":"gpt-4o","messages":[{{"role":"tool","tool_call_id":"c","content":"{}"}},{{"role":"user","content":"thanks"}}]}}"#,
            bulky()
        );
        let out = compress_dialect(
            Dialect::OpenAi,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Default,
        );

        assert_eq!(
            String::from_utf8(out.into_owned()).unwrap(),
            source,
            "compression reached into a message OpenAI may already have cached"
        );
    }

    #[test]
    fn a_restricted_policy_forwards_an_openai_body_untouched() {
        let source = chat_request();
        let restricted = CompressionPolicy::for_mode(headroom_core::AuthMode::Subscription);
        let out = compress_dialect(
            Dialect::OpenAi,
            source.as_bytes(),
            &compressors(),
            true,
            restricted,
            Verbosity::Default,
        );
        assert_eq!(out.as_ref(), source.as_bytes());
    }

    #[test]
    fn openai_compression_is_deterministic() {
        let source = chat_request();
        let first = compress_dialect(
            Dialect::OpenAi,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Default,
        )
        .into_owned();

        for _ in 0..20 {
            let again = compress_dialect(
                Dialect::OpenAi,
                source.as_bytes(),
                &compressors(),
                true,
                payg(),
                Verbosity::Default,
            )
            .into_owned();
            assert_eq!(again, first);
        }
    }

    // ---- cache key and effort ----

    /// A two-turn conversation with a replaceable newest message.
    fn convo(newest: &str) -> String {
        format!(
            r#"{{"model":"gpt-4o","messages":[{{"role":"user","content":"build the parser"}},{{"role":"assistant","content":"done"}},{{"role":"user","content":"{newest}"}}]}}"#
        )
    }

    #[test]
    fn the_cache_key_is_stable_across_turns_of_one_conversation() {
        // The property that makes the key worth sending at all. A key that changes
        // every turn lands each turn in a fresh partition and reuses nothing — worse
        // than sending no key, since it also fragments the provider's own automatic
        // prefix cache.
        let first = cache_key_for(convo("what about errors?").as_bytes()).unwrap();
        let second = cache_key_for(convo("and timeouts?").as_bytes()).unwrap();

        assert_eq!(
            first, second,
            "the key changed when only the newest turn did"
        );
    }

    #[test]
    fn different_conversations_get_different_keys() {
        // The over-correction to guard against: a key stable enough to be useless,
        // pooling unrelated conversations into one partition.
        let a = cache_key_for(convo("x").as_bytes()).unwrap();
        let b = cache_key_for(
            r#"{"model":"gpt-4o","messages":[{"role":"user","content":"something else entirely"},{"role":"assistant","content":"ok"},{"role":"user","content":"x"}]}"#
                .as_bytes(),
        )
        .unwrap();

        assert_ne!(a, b);
    }

    #[test]
    fn a_single_message_request_gets_no_cache_key() {
        // One message is not yet a conversation, and there is no prefix to partition.
        assert_eq!(
            cache_key_for(
                r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#.as_bytes()
            ),
            None
        );
    }

    #[test]
    fn shaping_adds_both_members_and_leaves_the_rest_byte_identical() {
        let source = convo("what about errors?");
        let out = shape_openai(source.as_bytes(), payg());
        let rendered = String::from_utf8(out).unwrap();

        assert!(rendered.contains("prompt_cache_key"));
        assert!(rendered.contains("reasoning_effort"));
        // Every original byte after the opening brace survives, in order.
        assert!(
            rendered.ends_with(&source[1..]),
            "the original body was rewritten"
        );
    }

    #[test]
    fn a_customer_supplied_cache_key_is_never_replaced() {
        // The key partitions the customer's cache. Overwriting one silently moves
        // their traffic to a different partition and cold-starts it.
        let source = r#"{"prompt_cache_key":"theirs","model":"gpt-4o","messages":[{"role":"user","content":"a"},{"role":"assistant","content":"b"},{"role":"user","content":"c"}]}"#;
        let out = shape_openai(source.as_bytes(), payg());
        let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();

        assert_eq!(parsed["prompt_cache_key"], "theirs");
    }

    #[test]
    fn a_customer_supplied_reasoning_effort_is_never_replaced() {
        // A deliberate choice about answer quality. Overriding it is not a compression
        // decision.
        let source = r#"{"reasoning_effort":"low","model":"gpt-4o","messages":[{"role":"user","content":"it errors"},{"role":"assistant","content":"b"},{"role":"user","content":"still errors"}]}"#;
        let out = shape_openai(source.as_bytes(), payg());
        let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();

        assert_eq!(parsed["reasoning_effort"], "low");
    }

    #[test]
    fn a_restricted_policy_adds_neither_member() {
        // Invariant I10. Both are proxy-visible modifications.
        let source = convo("what about errors?");
        let restricted = CompressionPolicy::for_mode(headroom_core::AuthMode::Subscription);
        let out = shape_openai(source.as_bytes(), restricted);

        assert_eq!(out, source.as_bytes(), "a restricted request was modified");
    }

    #[test]
    fn an_error_turn_routes_high_effort() {
        let source = convo("it still panics with an index error");
        let out = shape_openai(source.as_bytes(), payg());
        let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();

        assert_eq!(parsed["reasoning_effort"], "high");
    }

    #[test]
    fn shaping_is_deterministic() {
        // Invariant I4, including the hash that feeds the cache key.
        let source = convo("what about errors?");
        let first = shape_openai(source.as_bytes(), payg());
        for _ in 0..20 {
            assert_eq!(shape_openai(source.as_bytes(), payg()), first);
        }
    }

    #[test]
    fn shaping_a_malformed_body_returns_it_unchanged() {
        for source in [&b"{not json"[..], &b""[..], &b"[1,2,3]"[..]] {
            assert_eq!(shape_openai(source, payg()), source);
        }
    }

    #[test]
    fn a_malformed_openai_body_forwards_untouched() {
        for source in [&b"{not json"[..], &b""[..], &br#"{"model":"gpt-4o"}"#[..]] {
            let out = compress_dialect(
                Dialect::OpenAi,
                source,
                &compressors(),
                true,
                payg(),
                Verbosity::Default,
            );
            assert_eq!(out.as_ref(), source);
        }
    }
}
