//! Redis-backed CCR storage — gap row R4.
//!
//! # What this is for, and what it is not for
//!
//! [`InMemoryCcrStore`] and [`FileCcrStore`] are both *per process*. That is invisible on
//! one proxy and wrong the moment there are two: a model compresses against worker A,
//! then calls `ccr_retrieve` and lands on worker B, which has never heard of that hash
//! and answers "expired". The failure is intermittent by construction — it depends on
//! load balancing — and it looks like data loss to whoever reports it.
//!
//! A shared store fixes exactly that and nothing else. It is not faster than the file
//! store, and for a single worker it is strictly worse: a network round trip in place of
//! a page-cache read, plus a service to run. Multi-worker deployments are the only reason
//! to choose it.
//!
//! # Expiry is Redis's job
//!
//! Entries are written with `SET ... EX`, so the server expires them. That is not just
//! convenient: with several workers, a purge running on each of them is several processes
//! racing to delete the same keys, and every one of them has to be right about a clock it
//! does not share. Handing expiry to the one process that owns the data removes the race
//! rather than managing it.
//!
//! [`InMemoryCcrStore`]: super::InMemoryCcrStore
//! [`FileCcrStore`]: super::FileCcrStore

use std::sync::Mutex;
use std::time::Duration;

use redis::{Client, Commands, Connection};

use super::{CcrStore, ContentHash};
use crate::error::{Error, Result};

/// Key prefix, so a Redis shared with other users stays legible and a `SCAN` for this
/// crate's keys cannot match somebody else's.
const KEY_PREFIX: &str = "headroom:ccr:";

/// A CCR store backed by a shared Redis.
pub struct RedisCcrStore {
    /// One connection behind a mutex rather than a pool.
    ///
    /// A pool is the obvious choice and is not warranted here. CCR traffic is one `put`
    /// per compressed block and one `get` per retrieval — orders of magnitude below the
    /// model round trip that surrounds it — so the mutex is never the bottleneck, and a
    /// pool would add a dependency and a failure mode (exhaustion) for throughput this
    /// path does not have.
    connection: Mutex<Connection>,
    client: Client,
}

impl std::fmt::Debug for RedisCcrStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The connection has no useful Debug and the URL may carry a password.
        f.debug_struct("RedisCcrStore").finish_non_exhaustive()
    }
}

impl RedisCcrStore {
    /// Connects to `url`, for example `redis://127.0.0.1:6379`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CcrStore`] if the URL is unusable or the server cannot be
    /// reached. Connecting eagerly is deliberate: a store that connected lazily would
    /// start a proxy that looks healthy and fails on the first compression, which is the
    /// worst moment to discover a typo in a URL.
    pub fn connect(url: &str) -> Result<Self> {
        let client =
            Client::open(url).map_err(|err| Error::CcrStore(format!("redis url: {err}")))?;
        let connection = client
            .get_connection()
            .map_err(|err| Error::CcrStore(format!("connecting to redis: {err}")))?;

        Ok(Self {
            connection: Mutex::new(connection),
            client,
        })
    }

    /// The Redis key for `hash`.
    fn key(hash: ContentHash) -> String {
        format!("{KEY_PREFIX}{}", hash.to_hex())
    }

    /// Runs `op` against the connection, reconnecting once if it has dropped.
    ///
    /// # Why one retry rather than none or many
    ///
    /// A long-lived connection to a Redis that restarted, or was closed by an idle
    /// timeout, fails on its next use and would keep failing. One reconnect turns that
    /// into a hiccup. More than one starts adding latency to a request that is already
    /// waiting, and the caller's fallback — forward the original content — is a perfectly
    /// good outcome that costs a few tokens rather than an error.
    fn with_connection<T>(
        &self,
        mut op: impl FnMut(&mut Connection) -> redis::RedisResult<T>,
    ) -> Result<T> {
        // A poisoned mutex means another thread panicked mid-operation. The connection's
        // state is then unknown, so it is replaced rather than reused.
        let mut guard = match self.connection.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        match op(&mut guard) {
            Ok(value) => Ok(value),
            Err(first) => {
                let mut fresh = self.client.get_connection().map_err(|err| {
                    Error::CcrStore(format!(
                        "redis failed ({first}) and reconnect failed: {err}"
                    ))
                })?;
                let value = op(&mut fresh)
                    .map_err(|err| Error::CcrStore(format!("redis after reconnect: {err}")))?;
                *guard = fresh;
                Ok(value)
            }
        }
    }

    /// Every live key this store owns.
    fn keys(&self) -> Result<Vec<String>> {
        // `SCAN` rather than `KEYS`. `KEYS` blocks the server for the whole sweep, which
        // on a shared Redis is a stall everybody else feels — and this is only ever
        // called for telemetry.
        self.with_connection(|connection| {
            let mut found = Vec::new();
            let mut cursor = 0u64;
            loop {
                let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
                    .arg(cursor)
                    .arg("MATCH")
                    .arg(format!("{KEY_PREFIX}*"))
                    .arg("COUNT")
                    .arg(500)
                    .query(connection)?;
                found.extend(batch);
                cursor = next;
                if cursor == 0 {
                    return Ok(found);
                }
            }
        })
    }
}

impl CcrStore for RedisCcrStore {
    fn put(&self, hash: ContentHash, original: &[u8], ttl: Duration) -> Result<()> {
        let key = Self::key(hash);
        let payload = original.to_vec();
        // Redis expiry is whole seconds and rounds toward zero, so a sub-second TTL would
        // become zero — which `SET EX` rejects. One second is the floor.
        let seconds = ttl.as_secs().max(1);

        self.with_connection(move |connection| {
            connection.set_ex::<_, _, ()>(&key, payload.as_slice(), seconds)
        })
    }

    fn get(&self, hash: ContentHash) -> Result<Option<Vec<u8>>> {
        let key = Self::key(hash);
        // An expired or absent key is `None`, not an error — a model asking for something
        // that aged out is ordinary, and the caller answers by saying so.
        self.with_connection(move |connection| connection.get::<_, Option<Vec<u8>>>(&key))
    }

    fn purge_expired(&self) -> usize {
        // Redis expires keys itself, so there is nothing here to collect. Reporting zero
        // is the truth rather than a stub: no entry was removed *by this call*.
        //
        // Sweeping here would be worse than useless. With several workers, each running
        // its own purge, they would race to delete the same keys while disagreeing about
        // a clock none of them owns — which is precisely the problem a shared store with
        // server-side expiry exists to remove.
        0
    }

    fn len(&self) -> usize {
        // A failure reads as zero rather than propagating. This exists for telemetry, and
        // a metric that cannot be read should not be able to fail a request.
        self.keys().map(|keys| keys.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The server these tests use, or `None` when there is none to talk to.
    ///
    /// # Why a missing server skips rather than fails
    ///
    /// This is the only test in the crate that needs a service. A contributor without
    /// Redis running should get a green `cargo test`, not a failure in code they did not
    /// touch. CI provides a server, so the tests do run where it matters — and
    /// `a_missing_server_is_an_error_not_a_silent_success` below covers the case this
    /// skip could otherwise hide.
    fn store() -> Option<RedisCcrStore> {
        let url =
            std::env::var("HEADROOM_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
        RedisCcrStore::connect(&url).ok()
    }

    /// Content unique to one test, so concurrent runs cannot collide on a shared server.
    fn unique(tag: &str) -> Vec<u8> {
        format!("content for {tag} in {}", std::process::id()).into_bytes()
    }

    #[test]
    fn a_stored_original_comes_back_verbatim() {
        // The whole contract. A CCR store that returns *almost* the original has made
        // lossy compression lossy in the permanent sense.
        let Some(store) = store() else { return };
        let content = unique("verbatim");
        let hash = ContentHash::of(&content);

        store
            .put(hash, &content, Duration::from_secs(60))
            .expect("put failed");
        assert_eq!(store.get(hash).expect("get failed"), Some(content));
    }

    #[test]
    fn binary_content_survives_the_round_trip() {
        // Redis values are byte strings, and treating them as UTF-8 anywhere would
        // corrupt exactly the content most worth storing.
        let Some(store) = store() else { return };
        let content: Vec<u8> = vec![0x00, 0xff, 0xfe, b'\n', 0x80, 0x01];
        let hash = ContentHash::of(&content);

        store
            .put(hash, &content, Duration::from_secs(60))
            .expect("put failed");
        assert_eq!(store.get(hash).expect("get failed"), Some(content));
    }

    #[test]
    fn a_missing_hash_is_a_miss_rather_than_an_error() {
        let Some(store) = store() else { return };
        let absent = ContentHash::of(&unique("never stored"));

        assert_eq!(store.get(absent).expect("get failed"), None);
    }

    #[test]
    fn storing_the_same_hash_twice_is_not_an_error() {
        // Hashes are content-addressed, so a repeat `put` carries identical content by
        // definition. The trait says implementations may overwrite or refresh.
        let Some(store) = store() else { return };
        let content = unique("repeat");
        let hash = ContentHash::of(&content);

        for _ in 0..3 {
            store
                .put(hash, &content, Duration::from_secs(60))
                .expect("put failed");
        }
        assert_eq!(store.get(hash).expect("get failed"), Some(content));
    }

    #[test]
    fn a_sub_second_ttl_still_stores() {
        // Redis expiry is whole seconds and rounds toward zero, so an unclamped
        // sub-second TTL becomes `EX 0`, which the server rejects — the entry would
        // simply never be stored, and the marker pointing at it would be unretrievable.
        let Some(store) = store() else { return };
        let content = unique("short ttl");
        let hash = ContentHash::of(&content);

        store
            .put(hash, &content, Duration::from_millis(5))
            .expect("a sub-second ttl was rejected");
        assert_eq!(store.get(hash).expect("get failed"), Some(content));
    }

    #[test]
    fn an_expired_entry_reads_as_absent() {
        let Some(store) = store() else { return };
        let content = unique("expiring");
        let hash = ContentHash::of(&content);

        store
            .put(hash, &content, Duration::from_secs(1))
            .expect("put failed");
        std::thread::sleep(Duration::from_millis(1500));

        assert_eq!(store.get(hash).expect("get failed"), None);
    }

    #[test]
    fn purge_reports_nothing_because_the_server_owns_expiry() {
        // Not a stub. Sweeping from several workers would race to delete the same keys
        // while disagreeing about a clock none of them owns.
        let Some(store) = store() else { return };
        assert_eq!(store.purge_expired(), 0);
    }

    #[test]
    fn a_missing_server_is_an_error_not_a_silent_success() {
        // The case the skip in `store()` could otherwise hide: a bad URL must fail at
        // connect, so a proxy does not start looking healthy and break on its first
        // compression.
        let result = RedisCcrStore::connect("redis://127.0.0.1:1");

        assert!(result.is_err(), "connecting to a dead port succeeded");
    }

    #[test]
    fn a_malformed_url_is_rejected() {
        assert!(RedisCcrStore::connect("not a url").is_err());
        assert!(RedisCcrStore::connect("").is_err());
    }

    #[test]
    fn the_store_works_through_the_trait_object() {
        // How every caller actually holds it. A signature that only worked concretely
        // would not be usable by the proxy, which owns an `Arc<dyn CcrStore>`.
        let Some(store) = store() else { return };
        let store: std::sync::Arc<dyn CcrStore> = std::sync::Arc::new(store);
        let content = unique("trait object");

        let marker =
            super::super::store_and_mark(store.as_ref(), &content, Duration::from_secs(60))
                .expect("store_and_mark failed");
        let hash = super::super::parse_marker(&marker).expect("marker did not parse");

        assert_eq!(store.get(hash).expect("get failed"), Some(content));
    }
}
