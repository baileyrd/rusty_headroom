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

/// Suffix for a body being written, before it is renamed into place.
const TEMP_SUFFIX: &str = ".tmp";

/// The expiry given to a body whose sidecar is missing or unreadable.
///
/// [`FileCcrStore::get`] fails *open* when it cannot read an expiry — an unknown expiry
/// must not discard content a model was promised it could retrieve. The cost of that,
/// before this existed, was that such an entry became immortal: `purge_expired` skipped
/// anything it could not date, so it was never collected and never counted. A crash
/// between the body's rename and the sidecar's write is enough to produce one, which
/// makes it the ordinary consequence of an unclean shutdown rather than an exotic case.
///
/// Re-stamping keeps the fail-open read and makes the entry collectable again. Long
/// enough that a re-stamp never shortens the life of something whose real expiry was
/// further out than a day, because the original is unknowable by then.
const RECOVERY_TTL: Duration = Duration::from_secs(24 * 3600);

/// How old a `.tmp` must be before it is treated as an abandoned write.
///
/// A live `put` renames its temporary file into place within milliseconds, so an hour is
/// far beyond any in-flight write while still bounding what one crash can strand. Judged
/// by modification time rather than by presence, so this can never delete a file another
/// process is in the middle of writing.
const ABANDONED_AFTER: Duration = Duration::from_secs(3600);

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

    /// Whether `path` was last modified more than `age` ago.
    ///
    /// A file whose modification time cannot be read is treated as *not* old enough, so
    /// an unreadable timestamp never causes a deletion.
    fn older_than(path: &Path, age: Duration, now: SystemTime) -> bool {
        fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|elapsed| elapsed > age)
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

            // An abandoned write. Its body was never renamed into place, so nothing can
            // ever read it, and it is a full copy of the content it failed to store.
            if name.ends_with(TEMP_SUFFIX) {
                if Self::older_than(&path, ABANDONED_AFTER, now) {
                    let _ = fs::remove_file(&path);
                }
                continue;
            }

            // A sidecar whose body is gone — the mirror of the case below, left by a
            // purge or a deletion that removed one of the pair.
            if let Some(stem) = name.strip_suffix(EXPIRY_SUFFIX) {
                if ContentHash::from_hex(stem).is_ok_and(|hash| !self.content_path(hash).exists()) {
                    let _ = fs::remove_file(&path);
                }
                continue;
            }

            let Ok(hash) = ContentHash::from_hex(name) else {
                continue;
            };

            match Self::expiry_of(&self.expiry_path(hash)) {
                Some(expiry) if expiry <= now => {
                    let _ = fs::remove_file(&path);
                    let _ = fs::remove_file(self.expiry_path(hash));
                    removed += 1;
                }
                Some(_) => {}
                // Undatable. Kept — `get` fails open for the same reason — but given an
                // expiry so it stops being immortal. See [`RECOVERY_TTL`].
                None => {
                    let recovered = now.checked_add(RECOVERY_TTL).unwrap_or(now);
                    let _ = fs::write(
                        self.expiry_path(hash),
                        Self::epoch_secs(recovered).to_string(),
                    );
                }
            }
        }

        // Deliberately counts expired *entries* only, not the strays cleaned above. The
        // number is what a caller reports as "collected", and folding recovery work into
        // it would make a store healing itself look like a store expiring content.
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
    fn a_body_whose_sidecar_is_gone_is_kept_but_stops_being_immortal() {
        // What an unclean shutdown leaves. `put` renames the body into place and *then*
        // writes the sidecar, so a crash between the two produces exactly this — and
        // before the recovery below, `purge_expired` skipped anything it could not date,
        // forever. Measured: three purge rounds removed nothing and the body stayed.
        let scratch = Scratch::new("sidecar-gone");
        let dir = &scratch.0;
        let store = FileCcrStore::open(dir).expect("open");
        let hash = ContentHash::of(b"payload");
        store
            .put(hash, b"payload", Duration::from_secs(3600))
            .expect("put");
        fs::remove_file(dir.join(format!("{}{EXPIRY_SUFFIX}", hash.to_hex()))).expect("rm");

        // Fail open on the read: an unknown expiry must not discard content a model was
        // promised. That is the property the recovery must not break.
        assert!(
            store.get(hash).expect("get").is_some(),
            "an undatable entry was discarded rather than kept"
        );

        assert_eq!(store.purge_expired(), 0, "nothing had expired");
        assert!(
            store.get(hash).expect("get").is_some(),
            "the recovery deleted the content instead of dating it"
        );

        // It now has an expiry, so it is collectable. Backdate it and confirm the whole
        // cycle completes rather than stopping at "has a sidecar again".
        let expiry = dir.join(format!("{}{EXPIRY_SUFFIX}", hash.to_hex()));
        assert!(
            expiry.exists(),
            "no expiry was written, so it is still immortal"
        );
        let past = SystemTime::now() - Duration::from_secs(60);
        fs::write(&expiry, FileCcrStore::epoch_secs(past).to_string()).expect("backdate");

        assert!(store.get(hash).expect("get").is_none());
        assert_eq!(
            store.purge_expired(),
            1,
            "the recovered entry never collected"
        );
        assert!(!dir.join(hash.to_hex()).exists());
    }

    #[test]
    fn an_abandoned_temporary_write_is_collected_and_an_in_flight_one_is_not() {
        // A `.tmp` is a full copy of the content it failed to store, and nothing can ever
        // read it — its body was never renamed into place. Judged by modification time,
        // so this can never delete a file another process is mid-write on: that is what
        // the second half asserts, and without it this test would pass on a build that
        // deleted every `.tmp` it saw.
        let scratch = Scratch::new("abandoned-tmp");
        let dir = &scratch.0;
        let store = FileCcrStore::open(dir).expect("open");

        let stale = dir.join(format!(
            "{}{TEMP_SUFFIX}",
            ContentHash::of(b"stale").to_hex()
        ));
        fs::write(&stale, b"interrupted").expect("write");
        let backdated = SystemTime::now() - (ABANDONED_AFTER + Duration::from_secs(60));
        fs::File::options()
            .write(true)
            .open(&stale)
            .expect("open")
            .set_modified(backdated)
            .expect("backdate");

        let fresh = dir.join(format!(
            "{}{TEMP_SUFFIX}",
            ContentHash::of(b"fresh").to_hex()
        ));
        fs::write(&fresh, b"in flight").expect("write");

        store.purge_expired();

        assert!(!stale.exists(), "an abandoned write was left behind");
        assert!(
            fresh.exists(),
            "a write in progress was deleted out from under it"
        );
    }

    #[test]
    fn a_sidecar_whose_body_is_gone_is_collected() {
        let scratch = Scratch::new("orphan-sidecar");
        let dir = &scratch.0;
        let store = FileCcrStore::open(dir).expect("open");
        let hash = ContentHash::of(b"payload");
        store
            .put(hash, b"payload", Duration::from_secs(3600))
            .expect("put");
        fs::remove_file(dir.join(hash.to_hex())).expect("rm body");

        let orphan = dir.join(format!("{}{EXPIRY_SUFFIX}", hash.to_hex()));
        assert!(orphan.exists(), "the fixture did not leave an orphan");
        store.purge_expired();
        assert!(!orphan.exists(), "an orphaned sidecar was left behind");
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
