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

use crate::compression::{compress_dialect, Dialect};
use crate::config::Config;
use crate::headers::{sanitize, HeaderPolicy};
use crate::server::{relay, AppState};
use crate::upstream::RelayError;
use headroom_core::auth_mode::{classify_auth_mode, CompressionPolicy};
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

    // `prompt_cache_key` injection (gap row X16) is deliberately not applied here.
    // `stabilization::inject_prompt_cache_key` operates on a `serde_json::Value`, so
    // using it means re-serializing the whole body — and re-serializing a body to add
    // one member is the exact thing invariant I1 forbids, since it rewrites every byte
    // the customer sent in order to change none of them. Doing it faithfully needs a
    // surgical insert against the raw bytes, which is its own change.
    relay_to(
        &state,
        &headers,
        "/v1/chat/completions",
        compressed.into_owned(),
        policy,
    )
    .await
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
