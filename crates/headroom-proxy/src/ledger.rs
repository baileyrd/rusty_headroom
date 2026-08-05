//! The savings ledger's write side.
//!
//! [`SavingsLedger`] is the data structure; this is what keeps it current on a running
//! proxy without putting a disk write on the request path.
//!
//! # Why writes are buffered
//!
//! Persisting per request would put a file write, an fsync and a rename between the model
//! and the user on every call. That is a latency cost paid on live traffic to improve a
//! report nobody is reading at that moment.
//!
//! So records accumulate in memory — a `BTreeMap` update under a short-lived lock — and a
//! background task flushes on an interval, the same shape as the CCR purge task beside it.
//!
//! What that costs: a proxy killed between flushes loses at most one interval of savings.
//! That is the right trade for a reporting feature. The alternative is charging every
//! request for durability that only a dashboard needs.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use headroom_core::savings::SavingsLedger;

/// How often buffered savings reach the disk.
///
/// A minute. Short enough that a crash loses little, long enough that a busy proxy is not
/// rewriting the file constantly.
pub const FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Accumulates savings in memory and flushes them to a file.
#[derive(Debug)]
pub struct LedgerWriter {
    ledger: Mutex<SavingsLedger>,
    path: PathBuf,
}

impl LedgerWriter {
    /// Opens the ledger at `path`, loading whatever is already there.
    ///
    /// Loading rather than starting fresh is the whole point: the totals have to span
    /// restarts, which means the first act of a new process is to pick up where the last
    /// one left off.
    pub fn open(path: PathBuf) -> Self {
        Self {
            ledger: Mutex::new(SavingsLedger::load(&path)),
            path,
        }
    }

    /// Records a compression.
    ///
    /// Never blocks on I/O and never fails the request: a poisoned lock drops the record
    /// rather than propagating. Losing one row of a report is a smaller harm than failing
    /// the request it describes.
    pub fn record(&self, model_family: &str, content_type: &str, before: u64, after: u64) {
        if let Ok(mut ledger) = self.ledger.lock() {
            ledger.record(SystemTime::now(), model_family, content_type, before, after);
        }
    }

    /// Records a request forwarded without compression.
    pub fn record_passthrough(&self, model_family: &str, content_type: &str) {
        if let Ok(mut ledger) = self.ledger.lock() {
            ledger.record_passthrough(SystemTime::now(), model_family, content_type);
        }
    }

    /// Prunes past-retention buckets and writes the ledger out.
    ///
    /// Pruning here rather than on a separate schedule is what makes growth bounded
    /// without anybody remembering to arrange it.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written.
    pub fn flush(&self) -> headroom_core::error::Result<()> {
        let snapshot = {
            let Ok(mut ledger) = self.ledger.lock() else {
                return Ok(());
            };
            ledger.prune(SystemTime::now());
            ledger.clone()
        };

        // Written from a snapshot, with the lock released. Holding it across an fsync
        // would stall every request recording a saving behind the disk.
        snapshot.save(&self.path)
    }

    /// The current totals, for a caller that wants them without reading the file.
    pub fn snapshot(&self) -> SavingsLedger {
        self.ledger
            .lock()
            .map(|ledger| ledger.clone())
            .unwrap_or_default()
    }
}

/// Flushes `writer` on an interval until the process ends.
pub fn spawn_flush_task(writer: Arc<LedgerWriter>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
        // The first tick fires immediately; skip it so startup does not rewrite a file it
        // has just read.
        ticker.tick().await;

        loop {
            ticker.tick().await;
            if let Err(err) = writer.flush() {
                // Logged and continued. A ledger that cannot be written is a reporting
                // problem, and taking the proxy down over one would be a far larger one.
                tracing::warn!(%err, "could not flush the savings ledger");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("headroom-ledger-{}-{name}", std::process::id()))
            .join("savings.json")
    }

    #[test]
    fn records_survive_a_flush_and_reopen() {
        let path = scratch("reopen");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());

        let writer = LedgerWriter::open(path.clone());
        writer.record("claude", "json", 1000, 300);
        writer.flush().expect("flushes");

        let reopened = LedgerWriter::open(path.clone());
        assert_eq!(reopened.snapshot().total_since(None).saved(), 700);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_reopen_adds_to_what_was_there_rather_than_replacing_it() {
        // The claim the whole feature rests on: totals span restarts. If a reopen started
        // from zero, `headroom savings` would report the current process again.
        let path = scratch("accumulate");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());

        let first = LedgerWriter::open(path.clone());
        first.record("claude", "json", 1000, 400);
        first.flush().expect("flushes");

        let second = LedgerWriter::open(path.clone());
        second.record("claude", "json", 1000, 400);
        second.flush().expect("flushes");

        let third = LedgerWriter::open(path.clone());
        assert_eq!(
            third.snapshot().total_since(None).compressions,
            2,
            "a restart discarded what the previous process recorded"
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn recording_never_touches_the_disk() {
        // The latency claim. Nothing is written until `flush`, so a record costs a map
        // update rather than an fsync on the request path.
        let path = scratch("buffered");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());

        let writer = LedgerWriter::open(path.clone());
        writer.record("claude", "json", 100, 40);

        assert!(!path.exists(), "recording wrote to disk before a flush");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
