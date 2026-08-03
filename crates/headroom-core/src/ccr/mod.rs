//! CCR — Compress, Cache, Retrieve.
//!
//! CCR is what makes lossy compression acceptable. Before a transform discards
//! anything, the original content is stored locally under a content-addressed hash,
//! and the compressed block carries a `<<ccr:HASH>>` marker. If the model needs what
//! was elided, it calls the `ccr_retrieve` tool with that hash and gets the original
//! back verbatim.
//!
//! Without CCR, lossy compression is lossy in the permanent sense — the information
//! is simply gone. With it, compression becomes a bet that the model probably will
//! not need the detail, and a bet that can be unwound when it does.
//!
//! # Time and determinism
//!
//! The store has TTLs, so it reads a clock. That does not conflict with invariant
//! I4, which governs the *compression decision*: the same content must always
//! produce the same hash and the same marker bytes, and it does — hashing never
//! consults the clock. Expiry only governs how long the original stays retrievable,
//! which is a storage-lifecycle concern and never changes the bytes sent upstream.

mod hash;
mod in_memory;

pub use hash::{find_markers, marker, parse_marker, ContentHash};
pub use in_memory::InMemoryCcrStore;

use std::time::Duration;

use crate::error::Result;

/// Storage for originals behind CCR markers.
///
/// `Send + Sync` is a hard requirement rather than a convenience: the proxy is async
/// and multi-threaded, and a store that could not be shared across worker threads
/// would have to be cloned per request, which defeats the point of a cache.
pub trait CcrStore: Send + Sync {
    /// Stores `original` under `hash`, retrievable for at least `ttl`.
    ///
    /// Storing the same hash twice is not an error. Because hashes are
    /// content-addressed, a repeat `put` carries identical content by definition,
    /// so implementations may either overwrite or refresh the TTL.
    fn put(&self, hash: ContentHash, original: &[u8], ttl: Duration) -> Result<()>;

    /// Retrieves the original stored under `hash`, if it is still present.
    ///
    /// A miss is `Ok(None)`, not an error. Entries expire, and a model asking for
    /// something that has aged out is an ordinary occurrence the caller handles by
    /// telling it so.
    fn get(&self, hash: ContentHash) -> Result<Option<Vec<u8>>>;

    /// Removes expired entries, returning how many were removed.
    fn purge_expired(&self) -> usize;

    /// The number of live entries. Primarily for telemetry and tests.
    fn len(&self) -> usize;

    /// Whether the store holds no live entries.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Stores `content` and returns the marker that should replace it.
///
/// This pairs the two halves that must not drift apart — the content goes into the
/// store under exactly the hash the marker advertises. Calling `put` and `marker`
/// separately invites a mismatch where the marker points at a hash nothing was
/// stored under, which surfaces as an unretrievable marker much later.
pub fn store_and_mark<S: CcrStore + ?Sized>(
    store: &S,
    content: &[u8],
    ttl: Duration,
) -> Result<String> {
    let hash = ContentHash::of(content);
    store.put(hash, content, ttl)?;
    Ok(marker(hash))
}
