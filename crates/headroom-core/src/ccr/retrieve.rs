//! The retrieval tool the model uses to get elided content back.

use serde_json::{json, Value};

use super::{CcrStore, ContentHash};

/// Name of the tool exposed to the model.
pub const RETRIEVE_TOOL_NAME: &str = "ccr_retrieve";

/// The tool definition to advertise alongside the customer's own tools.
///
/// # Why this must be registered on every request
///
/// The obvious implementation registers the tool only when something was actually
/// compressed. That is wrong, and the reason is the whole reason this project exists:
/// the tools array is part of the cached prompt prefix. A tool that appears and
/// disappears depending on whether this particular request happened to compress
/// anything changes the prefix on every state flip, invalidating the provider's cache
/// each time.
///
/// Registering it unconditionally costs a fixed handful of tokens once. Toggling it
/// costs a full cache miss every time compression starts or stops.
///
/// # Example
///
/// ```
/// use headroom_core::ccr::{retrieve_tool_definition, RETRIEVE_TOOL_NAME};
///
/// let tool = retrieve_tool_definition();
/// assert_eq!(tool["name"], RETRIEVE_TOOL_NAME);
/// ```
pub fn retrieve_tool_definition() -> Value {
    json!({
        "name": RETRIEVE_TOOL_NAME,
        "description": "Retrieve the full original content behind a <<ccr:HASH>> marker. \
    Content in this conversation may have been summarized to save space; wherever you see \
    such a marker, the complete original is available through this tool.",
        "input_schema": {
            "type": "object",
            "properties": {
                "hash": {
                    "type": "string",
                    "description": "The hash from the marker, without the surrounding <<ccr: and >>."
                }
            },
            "required": ["hash"]
        }
    })
}

/// The result of a retrieval request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Retrieval {
    /// The original content.
    Found(Vec<u8>),
    /// The hash was well-formed but nothing is stored under it.
    ///
    /// Distinct from [`Retrieval::Malformed`] because the model should be told
    /// different things: an expired entry means "this existed and is gone", while a
    /// bad hash means "check what you sent".
    Expired,
    /// The hash could not be parsed.
    Malformed,
}

impl Retrieval {
    /// A message suitable for returning to the model as the tool result.
    pub fn message(&self) -> String {
        match self {
            Self::Found(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            Self::Expired => {
                "That content is no longer available; it has expired from the local cache."
                    .to_owned()
            }
            Self::Malformed => "That is not a valid content hash. Pass the value from inside the \
<<ccr:...>> marker."
                .to_owned(),
        }
    }
}

/// Handles a `ccr_retrieve` call.
///
/// Accepts the bare hash or the full marker, because a model that copies the whole
/// `<<ccr:...>>` string out of the content it is reading is doing the reasonable
/// thing, and rejecting that would be pedantry that costs a round trip.
pub fn handle_retrieve<S: CcrStore + ?Sized>(store: &S, argument: &str) -> Retrieval {
    let trimmed = argument.trim();
    let hex = trimmed
        .strip_prefix("<<ccr:")
        .and_then(|rest| rest.strip_suffix(">>"))
        .unwrap_or(trimmed);

    let Ok(hash) = ContentHash::from_hex(hex) else {
        return Retrieval::Malformed;
    };

    match store.get(hash) {
        Ok(Some(content)) => Retrieval::Found(content),
        // A backend failure and a genuine miss are reported the same way. The model
        // can do nothing different about either, and "expired" is the honest summary
        // of "you cannot have this".
        _ => Retrieval::Expired,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ccr::{marker, InMemoryCcrStore};
    use std::time::Duration;

    const TTL: Duration = Duration::from_secs(300);

    #[test]
    fn the_tool_definition_has_the_shape_providers_expect() {
        let tool = retrieve_tool_definition();
        assert_eq!(tool["name"], RETRIEVE_TOOL_NAME);
        assert!(tool["description"].as_str().unwrap().contains("ccr:"));
        assert_eq!(tool["input_schema"]["properties"]["hash"]["type"], "string");
        assert_eq!(tool["input_schema"]["required"][0], "hash");
    }

    #[test]
    fn the_definition_is_byte_stable_across_calls() {
        // It sits in the cached prompt prefix. A definition that serialized
        // differently between requests would bust the cache by itself.
        let first = serde_json::to_string(&retrieve_tool_definition()).unwrap();
        for _ in 0..25 {
            assert_eq!(
                serde_json::to_string(&retrieve_tool_definition()).unwrap(),
                first
            );
        }
    }

    #[test]
    fn a_stored_original_is_retrieved_by_bare_hash() {
        let store = InMemoryCcrStore::new();
        let content = b"the original content";
        let hash = ContentHash::of(content);
        store.put(hash, content, TTL).unwrap();

        assert_eq!(
            handle_retrieve(&store, &hash.to_hex()),
            Retrieval::Found(content.to_vec())
        );
    }

    #[test]
    fn the_full_marker_is_accepted_too() {
        // A model copying the whole `<<ccr:...>>` string out of the content it is
        // reading is doing the reasonable thing.
        let store = InMemoryCcrStore::new();
        let content = b"content";
        let hash = ContentHash::of(content);
        store.put(hash, content, TTL).unwrap();

        assert_eq!(
            handle_retrieve(&store, &marker(hash)),
            Retrieval::Found(content.to_vec())
        );
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        let store = InMemoryCcrStore::new();
        let hash = ContentHash::of(b"x");
        store.put(hash, b"x", TTL).unwrap();
        assert!(matches!(
            handle_retrieve(&store, &format!("  {}  ", hash.to_hex())),
            Retrieval::Found(_)
        ));
    }

    #[test]
    fn expired_and_malformed_are_reported_differently() {
        // The model should be told different things: one means "this existed and is
        // gone", the other means "check what you sent".
        let store = InMemoryCcrStore::new();

        assert_eq!(
            handle_retrieve(&store, &ContentHash::of(b"never stored").to_hex()),
            Retrieval::Expired
        );
        assert_eq!(handle_retrieve(&store, "not-a-hash"), Retrieval::Malformed);
        assert_eq!(handle_retrieve(&store, ""), Retrieval::Malformed);

        assert_ne!(
            Retrieval::Expired.message(),
            Retrieval::Malformed.message(),
            "the two outcomes must read differently to the model"
        );
    }

    #[test]
    fn a_found_result_renders_as_the_content_itself() {
        let found = Retrieval::Found(b"the content".to_vec());
        assert_eq!(found.message(), "the content");
    }
}
