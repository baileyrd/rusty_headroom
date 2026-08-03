//! A file-backed CCR store.
//!
//! The in-memory store loses everything on restart. That is usually fine — a restart
//! invalidates the provider's cache anyway — but it is wrong in one case that matters:
//! a model asks to retrieve content stored before a proxy restart, and is told the
//! content is gone despite having been promised it was retrievable.
//!
//! # Why a directory of files rather than SQLite
//!
//! Gap row R3 called for SQLite. A content-addressed store with fixed-size hex keys,
//! immutable values, and no queries beyond point lookup uses almost nothing SQLite
//! offers, and `rusqlite` brings a bundled C library and a long build. One file per
//! hash gives the same durability with no dependency at all, and the filesystem
//! already provides the atomic rename this needs.
//!
//! Logged as a decision rather than silently substituted — see `DECISIONS.md`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::{CcrStore, ContentHash};
use crate::error::{Error, Result};

/// Suffix for the sidecar recording an entry's expiry.
const EXPIRY_SUFFIX: &str = ".exp";

/// A CCR store backed by a directory.
#[derive(Debug, Clone)]
pub struct FileCcrStore {
    root: PathBuf,
}

impl FileCcrStore {
    /// Opens (creating if needed) a store rooted at `root`.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn content_path(&self, hash: ContentHash) -> PathBuf {
        self.root.join(hash.to_hex())
    }

    fn expiry_path(&self, hash: ContentHash) -> PathBuf {
        self.root.join(format!("{}{EXPIRY_SUFFIX}", hash.to_hex()))
    }

    /// Reads an entry's expiry, if it has one.
    fn expiry_of(path: &Path) -> Option<SystemTime> {
        let raw = fs::read_to_string(path).ok()?;
        let secs: u64 = raw.trim().parse().ok()?;
        SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(secs))
    }

    /// Seconds since the epoch, or zero if the clock is before it.
    fn epoch_secs(at: SystemTime) -> u64 {
        at.duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

impl CcrStore for FileCcrStore {
    fn put(&self, hash: ContentHash, original: &[u8], ttl: Duration) -> Result<()> {
        let expires_at = SystemTime::now()
            .checked_add(ttl)
            .unwrap_or_else(|| SystemTime::now() + Duration::from_secs(365 * 24 * 3600));

        // Write to a temporary name and rename into place. A reader must never observe
        // a half-written entry and hand a model truncated content while calling it the
        // original.
        let temp = self.root.join(format!("{}.tmp", hash.to_hex()));
        {
            let mut file = fs::File::create(&temp)?;
            file.write_all(original)?;
            file.sync_all()?;
        }
        fs::rename(&temp, self.content_path(hash))?;

        fs::write(
            self.expiry_path(hash),
            Self::epoch_secs(expires_at).to_string(),
        )?;
        Ok(())
    }

    fn get(&self, hash: ContentHash) -> Result<Option<Vec<u8>>> {
        let path = self.content_path(hash);
        if !path.exists() {
            return Ok(None);
        }

        // Expired entries read as absent even before a purge runs, so retrieval never
        // depends on collection having happened.
        if let Some(expiry) = Self::expiry_of(&self.expiry_path(hash)) {
            if expiry <= SystemTime::now() {
                return Ok(None);
            }
        }

        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            // A concurrent purge between the existence check and the read is an
            // ordinary miss, not an error.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(Error::Io(err)),
        }
    }

    fn purge_expired(&self) -> usize {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return 0;
        };
        let now = SystemTime::now();
        let mut removed = 0;

        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.ends_with(EXPIRY_SUFFIX) || name.ends_with(".tmp") {
                continue;
            }
            let Ok(hash) = ContentHash::from_hex(name) else {
                continue;
            };
            if let Some(expiry) = Self::expiry_of(&self.expiry_path(hash)) {
                if expiry <= now {
                    let _ = fs::remove_file(&path);
                    let _ = fs::remove_file(self.expiry_path(hash));
                    removed += 1;
                }
            }
        }

        removed
    }

    fn len(&self) -> usize {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return 0;
        };
        let now = SystemTime::now();

        entries
            .flatten()
            .filter(|entry| {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    return false;
                };
                if name.ends_with(EXPIRY_SUFFIX) || name.ends_with(".tmp") {
                    return false;
                }
                let Ok(hash) = ContentHash::from_hex(name) else {
                    return false;
                };
                // `is_none_or` would read better but is stable only from 1.82, and this
                // crate's MSRV is 1.80.
                Self::expiry_of(&self.expiry_path(hash)).map_or(true, |expiry| expiry > now)
            })
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ccr::store_and_mark;

    const TTL: Duration = Duration::from_secs(300);

    /// A scratch directory that cleans itself up.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!("headroom-ccr-test-{tag}"));
            let _ = fs::remove_dir_all(&path);
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn content_round_trips_through_the_filesystem() {
        let scratch = Scratch::new("roundtrip");
        let store = FileCcrStore::open(&scratch.0).unwrap();

        let content = b"the original tool output";
        let hash = ContentHash::of(content);
        store.put(hash, content, TTL).unwrap();

        assert_eq!(store.get(hash).unwrap().as_deref(), Some(&content[..]));
    }

    #[test]
    fn content_survives_reopening_the_store() {
        // The whole point of a persistent backend: a model asking after a restart is
        // not told the content it was promised is gone.
        let scratch = Scratch::new("reopen");
        let content = b"survives a restart";
        let hash = ContentHash::of(content);

        {
            let store = FileCcrStore::open(&scratch.0).unwrap();
            store.put(hash, content, TTL).unwrap();
        }

        let reopened = FileCcrStore::open(&scratch.0).unwrap();
        assert_eq!(reopened.get(hash).unwrap().as_deref(), Some(&content[..]));
    }

    #[test]
    fn retrieval_is_byte_exact() {
        let scratch = Scratch::new("exact");
        let store = FileCcrStore::open(&scratch.0).unwrap();
        let content = b"line\n\n\ttrailing   \0embedded nul\n";
        let hash = ContentHash::of(content);

        store.put(hash, content, TTL).unwrap();
        assert_eq!(store.get(hash).unwrap().unwrap(), content.to_vec());
    }

    #[test]
    fn a_miss_is_not_an_error() {
        let scratch = Scratch::new("miss");
        let store = FileCcrStore::open(&scratch.0).unwrap();
        assert_eq!(store.get(ContentHash::of(b"absent")).unwrap(), None);
    }

    #[test]
    fn expired_entries_read_as_absent_before_purge_runs() {
        let scratch = Scratch::new("expiry");
        let store = FileCcrStore::open(&scratch.0).unwrap();
        let content = b"short lived";
        let hash = ContentHash::of(content);

        store.put(hash, content, Duration::ZERO).unwrap();
        assert_eq!(store.get(hash).unwrap(), None);
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn purge_removes_expired_entries_and_reports_the_count() {
        let scratch = Scratch::new("purge");
        let store = FileCcrStore::open(&scratch.0).unwrap();

        store
            .put(ContentHash::of(b"a"), b"a", Duration::ZERO)
            .unwrap();
        store
            .put(ContentHash::of(b"b"), b"b", Duration::ZERO)
            .unwrap();
        store.put(ContentHash::of(b"c"), b"c", TTL).unwrap();

        assert_eq!(store.purge_expired(), 2);
        assert_eq!(store.len(), 1);
        assert_eq!(store.purge_expired(), 0);
    }

    #[test]
    fn the_sidecar_files_are_not_counted_as_entries() {
        let scratch = Scratch::new("sidecar");
        let store = FileCcrStore::open(&scratch.0).unwrap();
        store.put(ContentHash::of(b"x"), b"x", TTL).unwrap();
        assert_eq!(store.len(), 1, "expiry sidecar counted as an entry");
    }

    #[test]
    fn store_and_mark_works_against_the_persistent_backend() {
        let scratch = Scratch::new("mark");
        let store = FileCcrStore::open(&scratch.0).unwrap();

        let content = b"content behind a marker";
        let marker = store_and_mark(&store, content, TTL).unwrap();
        let hash = crate::ccr::parse_marker(&marker).unwrap();

        assert_eq!(store.get(hash).unwrap().as_deref(), Some(&content[..]));
    }

    #[test]
    fn writing_the_same_hash_twice_is_fine() {
        let scratch = Scratch::new("idempotent");
        let store = FileCcrStore::open(&scratch.0).unwrap();
        let content = b"idempotent";
        let hash = ContentHash::of(content);

        store.put(hash, content, TTL).unwrap();
        store.put(hash, content, TTL).unwrap();
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn a_stray_file_in_the_directory_does_not_break_anything() {
        let scratch = Scratch::new("stray");
        let store = FileCcrStore::open(&scratch.0).unwrap();
        store.put(ContentHash::of(b"real"), b"real", TTL).unwrap();
        fs::write(scratch.0.join("not-a-hash.txt"), b"junk").unwrap();

        assert_eq!(store.len(), 1);
        assert_eq!(store.purge_expired(), 0);
    }
}
