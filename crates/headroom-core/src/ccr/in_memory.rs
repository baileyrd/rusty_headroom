//! An in-process CCR store.
//!
//! This is the default backend and the one tests use. It is also the right choice
//! for a single-process proxy: originals only need to outlive the conversation that
//! produced them, and a restart invalidates the provider's cache anyway.
//!
//! Persistent backends (SQLite, Redis) implement the same trait for multi-worker
//! deployments.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::{CcrStore, ContentHash};
use crate::error::Result;

struct Entry {
    content: Vec<u8>,
    expires_at: Instant,
}

/// A CCR store backed by an in-memory map.
///
/// Cloning shares the underlying storage, so a single store can be handed to every
/// request handler.
#[derive(Default)]
pub struct InMemoryCcrStore {
    entries: Mutex<HashMap<ContentHash, Entry>>,
}

impl InMemoryCcrStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs `f` against the entry map.
    ///
    /// A poisoned lock is recovered from rather than propagated. Poisoning means
    /// some other thread panicked while holding the lock; the map itself is a plain
    /// `HashMap` with no cross-entry invariant to corrupt, so the worst case is one
    /// half-written entry. Refusing all future CCR operations because of that would
    /// turn a single panic into a permanent loss of retrieval for the whole process
    /// — a much worse outcome than a possibly-missing entry, which `get` already
    /// treats as ordinary.
    fn with_entries<T>(&self, f: impl FnOnce(&mut HashMap<ContentHash, Entry>) -> T) -> T {
        let mut guard = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut guard)
    }
}

impl CcrStore for InMemoryCcrStore {
    fn put(&self, hash: ContentHash, original: &[u8], ttl: Duration) -> Result<()> {
        let expires_at = Instant::now()
            .checked_add(ttl)
            // A TTL large enough to overflow `Instant` means "effectively forever".
            // Saturating is friendlier than panicking on an absurd but harmless
            // configuration value.
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(365 * 24 * 3600));

        self.with_entries(|entries| {
            entries.insert(
                hash,
                Entry {
                    content: original.to_vec(),
                    expires_at,
                },
            );
        });
        Ok(())
    }

    fn get(&self, hash: ContentHash) -> Result<Option<Vec<u8>>> {
        let now = Instant::now();
        Ok(self.with_entries(|entries| {
            // An entry past its TTL is treated as absent even before `purge_expired`
            // collects it, so retrieval never depends on purge having run.
            match entries.get(&hash) {
                Some(entry) if entry.expires_at > now => Some(entry.content.clone()),
                _ => None,
            }
        }))
    }

    fn purge_expired(&self) -> usize {
        let now = Instant::now();
        self.with_entries(|entries| {
            let before = entries.len();
            entries.retain(|_, entry| entry.expires_at > now);
            before - entries.len()
        })
    }

    fn len(&self) -> usize {
        let now = Instant::now();
        self.with_entries(|entries| {
            entries
                .values()
                .filter(|entry| entry.expires_at > now)
                .count()
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;
    use crate::ccr::store_and_mark;

    const TTL: Duration = Duration::from_secs(300);

    #[test]
    fn round_trips_content() {
        let store = InMemoryCcrStore::new();
        let content = b"the original tool output, in full";
        let hash = ContentHash::of(content);

        store.put(hash, content, TTL).unwrap();
        assert_eq!(store.get(hash).unwrap().as_deref(), Some(&content[..]));
    }

    #[test]
    fn retrieval_is_byte_exact() {
        // The retrieved original must be identical, including trailing whitespace
        // and embedded NULs — the model is told this is the original.
        let store = InMemoryCcrStore::new();
        let content = b"line one\n\nline two\t\0trailing   \n";
        let hash = ContentHash::of(content);
        store.put(hash, content, TTL).unwrap();
        assert_eq!(store.get(hash).unwrap().unwrap(), content.to_vec());
    }

    #[test]
    fn a_miss_is_not_an_error() {
        let store = InMemoryCcrStore::new();
        let absent = ContentHash::of(b"never stored");
        assert_eq!(store.get(absent).unwrap(), None);
    }

    #[test]
    fn expired_entries_read_as_absent_before_purge_runs() {
        let store = InMemoryCcrStore::new();
        let content = b"short lived";
        let hash = ContentHash::of(content);

        store.put(hash, content, Duration::ZERO).unwrap();

        // Deliberately not calling purge_expired first: retrieval must not depend on
        // a collection pass having happened.
        assert_eq!(store.get(hash).unwrap(), None);
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn purge_reports_how_many_it_removed() {
        let store = InMemoryCcrStore::new();

        store
            .put(ContentHash::of(b"a"), b"a", Duration::ZERO)
            .unwrap();
        store
            .put(ContentHash::of(b"b"), b"b", Duration::ZERO)
            .unwrap();
        store.put(ContentHash::of(b"c"), b"c", TTL).unwrap();

        assert_eq!(store.purge_expired(), 2);
        assert_eq!(store.len(), 1);
        // A second purge has nothing left to do.
        assert_eq!(store.purge_expired(), 0);
    }

    #[test]
    fn storing_the_same_hash_twice_is_fine() {
        // Content-addressed, so a repeat put carries identical content by definition.
        let store = InMemoryCcrStore::new();
        let content = b"idempotent";
        let hash = ContentHash::of(content);

        store.put(hash, content, TTL).unwrap();
        store.put(hash, content, TTL).unwrap();

        assert_eq!(store.len(), 1);
        assert_eq!(store.get(hash).unwrap().as_deref(), Some(&content[..]));
    }

    #[test]
    fn empty_reports_correctly() {
        let store = InMemoryCcrStore::new();
        assert!(store.is_empty());
        store.put(ContentHash::of(b"x"), b"x", TTL).unwrap();
        assert!(!store.is_empty());
    }

    #[test]
    fn store_and_mark_stores_under_the_hash_the_marker_advertises() {
        // The pairing that must never drift: the marker's hash is exactly what the
        // content was stored under.
        let store = InMemoryCcrStore::new();
        let content = b"content that will be elided";

        let m = store_and_mark(&store, content, TTL).unwrap();
        let hash = crate::ccr::parse_marker(&m).unwrap();

        assert_eq!(store.get(hash).unwrap().as_deref(), Some(&content[..]));
    }

    #[test]
    fn concurrent_writers_and_readers_all_succeed() {
        // The store is shared across the proxy's worker threads, so this is the
        // real usage pattern rather than a synthetic stress test.
        let store = Arc::new(InMemoryCcrStore::new());
        let mut handles = Vec::new();

        for i in 0..8 {
            let store = Arc::clone(&store);
            handles.push(thread::spawn(move || {
                for j in 0..64 {
                    let content = format!("thread {i} item {j}").into_bytes();
                    let hash = ContentHash::of(&content);
                    store.put(hash, &content, TTL).unwrap();
                    assert_eq!(store.get(hash).unwrap(), Some(content));
                }
            }));
        }

        for handle in handles {
            handle.join().expect("worker thread panicked");
        }

        assert_eq!(store.len(), 8 * 64);
    }

    #[test]
    fn a_panicking_writer_does_not_disable_the_store() {
        // Lock poisoning must not turn one panic into permanent loss of retrieval
        // for the whole process. See `with_entries`.
        let store = Arc::new(InMemoryCcrStore::new());
        let content = b"survives a poisoned lock";
        let hash = ContentHash::of(content);
        store.put(hash, content, TTL).unwrap();

        let poisoner = Arc::clone(&store);
        let _ = thread::spawn(move || {
            poisoner.with_entries(|_| panic!("deliberate panic while holding the lock"));
        })
        .join();

        assert_eq!(store.get(hash).unwrap().as_deref(), Some(&content[..]));
        store.put(ContentHash::of(b"after"), b"after", TTL).unwrap();
    }
}
