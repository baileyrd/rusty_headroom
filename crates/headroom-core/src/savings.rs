//! The durable savings ledger.
//!
//! `headroom savings` reported a **rate**, never a **total**. It read a `/metrics` scrape
//! off stdin, so it could say "this process has saved 40% since it started" and nothing
//! else — and everything reset on the next deploy. The headline claim of the project
//! ("here is what this saved you") could not be answered over any period longer than one
//! process lifetime.
//!
//! # Why buckets rather than an append-only log
//!
//! The obvious ledger appends one record per compression. On a busy proxy that is an
//! unbounded file growing at request rate, which is a disk-exhaustion bug wearing a
//! feature's clothes — and the endpoint it would take down is the one an operator reaches
//! for when things are already wrong.
//!
//! So entries are **aggregated on write** into hourly buckets keyed by
//! `(hour, model_family, content_type)`. Growth is bounded by the retention window times
//! the number of distinct keys, not by traffic. A month of a busy proxy is a few hundred
//! kilobytes.
//!
//! What that costs: per-request detail is gone. That is the right trade here — the
//! question this answers is "what did we save last month", and #188 (`headroom audit`) is
//! where per-block detail belongs.
//!
//! # No currency figure
//!
//! L12 decided that deliberately and this does not reverse it. A token count is a fact; a
//! dollar figure is a guess about somebody's pricing tier, and a guess printed beside
//! facts reads as one.
//!
//! # Clocks
//!
//! This module reads the wall clock, which invariant I4 forbids — for the compression
//! path. I4 is about the bytes sent upstream, and no byte of a request depends on
//! anything here. The ledger is written *after* a decision, never consulted for one.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Seconds in the bucket period.
///
/// An hour. Fine enough that "yesterday afternoon" is answerable, coarse enough that a
/// year of one key is 8,760 rows.
const BUCKET_SECONDS: u64 = 3600;

/// How long a bucket is kept before pruning.
///
/// 90 days. Long enough for the quarterly question this exists to answer, and bounded so
/// the file cannot grow forever.
const RETENTION: Duration = Duration::from_secs(90 * 24 * 3600);

/// One aggregated period of compression outcomes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bucket {
    /// Compressions recorded in this period.
    pub compressions: u64,
    /// Requests that were forwarded without compression.
    pub passthroughs: u64,
    /// Tokens before compression, summed.
    pub tokens_before: u64,
    /// Tokens after compression, summed.
    pub tokens_after: u64,
}

impl Bucket {
    /// Tokens saved in this period.
    pub fn saved(&self) -> u64 {
        self.tokens_before.saturating_sub(self.tokens_after)
    }
}

/// What a bucket is keyed by.
///
/// Rendered as `hour:model_family:content_type`, so a prefix selects a period and the
/// ordering of a `BTreeMap` is chronological.
fn bucket_key(hour: u64, model_family: &str, content_type: &str) -> String {
    // Zero-padded so lexical order is chronological order — a `BTreeMap` sorted by string
    // would otherwise put hour 9 after hour 10, and every range query would be wrong.
    format!("{hour:012}:{model_family}:{content_type}")
}

/// The hour a timestamp falls in.
fn hour_of(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() / BUCKET_SECONDS)
        // A clock before the epoch is a broken clock, not a reason to lose the record.
        // Bucket zero is wrong and visible, which beats dropping the data silently.
        .unwrap_or(0)
}

/// A durable record of what compression has saved.
///
/// Reads and writes a JSON file. Same substitution reasoning as R3 (`FileCcrStore` over
/// SQLite, D6): a dependency-free file is enough for the access pattern here — append a
/// handful of counters per request, read the whole thing when somebody asks — and adding a
/// database to a proxy that must start reliably is a cost without a matching benefit at
/// this size.
#[derive(Debug, Clone, Default)]
pub struct SavingsLedger {
    buckets: BTreeMap<String, Bucket>,
}

impl SavingsLedger {
    /// An empty ledger.
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads a ledger from `path`, or an empty one if it does not exist.
    ///
    /// A corrupt file yields an empty ledger and a warning rather than an error. A proxy
    /// that refuses to start because its savings history is unreadable has traded a
    /// reporting feature for an outage.
    pub fn load(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::new();
        };

        match serde_json::from_str::<BTreeMap<String, Bucket>>(&text) {
            Ok(buckets) => Self { buckets },
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    %err,
                    "savings ledger is unreadable; starting a fresh one"
                );
                Self::new()
            }
        }
    }

    /// Records a compression outcome.
    ///
    /// `at` is passed rather than read here so a test can place a record in a known
    /// period without sleeping, and so the caller decides what "now" means.
    pub fn record(
        &mut self,
        at: SystemTime,
        model_family: &str,
        content_type: &str,
        tokens_before: u64,
        tokens_after: u64,
    ) {
        let entry = self
            .buckets
            .entry(bucket_key(hour_of(at), model_family, content_type))
            .or_default();

        entry.compressions += 1;
        entry.tokens_before = entry.tokens_before.saturating_add(tokens_before);
        entry.tokens_after = entry.tokens_after.saturating_add(tokens_after);
    }

    /// Records a request that was forwarded without compression.
    ///
    /// Counted so the totals describe all traffic rather than only the part that
    /// compressed. A saved-token figure without a denominator is a number nobody can act
    /// on.
    pub fn record_passthrough(&mut self, at: SystemTime, model_family: &str, content_type: &str) {
        self.buckets
            .entry(bucket_key(hour_of(at), model_family, content_type))
            .or_default()
            .passthroughs += 1;
    }

    /// Every bucket, oldest first.
    pub fn buckets(&self) -> &BTreeMap<String, Bucket> {
        &self.buckets
    }

    /// Totals over the buckets at or after `since`.
    ///
    /// `None` for `since` totals everything.
    pub fn total_since(&self, since: Option<SystemTime>) -> Bucket {
        let floor = since.map(hour_of).unwrap_or(0);

        self.buckets
            .iter()
            .filter(|(key, _)| {
                key.split(':')
                    .next()
                    .and_then(|hour| hour.parse::<u64>().ok())
                    .is_some_and(|hour| hour >= floor)
            })
            .fold(Bucket::default(), |mut total, (_, bucket)| {
                total.compressions += bucket.compressions;
                total.passthroughs += bucket.passthroughs;
                total.tokens_before = total.tokens_before.saturating_add(bucket.tokens_before);
                total.tokens_after = total.tokens_after.saturating_add(bucket.tokens_after);
                total
            })
    }

    /// Drops buckets older than the retention window, returning how many went.
    ///
    /// This is what makes growth bounded rather than merely slow. Called before every
    /// write, so a long-running proxy prunes without anybody scheduling it.
    pub fn prune(&mut self, now: SystemTime) -> usize {
        let Some(cutoff) = now.checked_sub(RETENTION) else {
            return 0;
        };
        let floor = hour_of(cutoff);

        let before = self.buckets.len();
        self.buckets.retain(|key, _| {
            key.split(':')
                .next()
                .and_then(|hour| hour.parse::<u64>().ok())
                .is_some_and(|hour| hour >= floor)
        });
        before - self.buckets.len()
    }

    /// Writes the ledger to `path`.
    ///
    /// Via a temporary file and an atomic rename, the same discipline `FileCcrStore` uses
    /// (D38): a process killed mid-write leaves either the old ledger or the new one, and
    /// never a half-written file that the next load would discard as corrupt.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|err| Error::Malformed {
                    content_type: "savings-ledger",
                    detail: format!("cannot create {}: {err}", parent.display()),
                })?;
            }
        }

        let temporary = temporary_path(path);
        let text = serde_json::to_string(&self.buckets)?;

        {
            let mut file = std::fs::File::create(&temporary).map_err(|err| Error::Malformed {
                content_type: "savings-ledger",
                detail: format!("cannot write {}: {err}", temporary.display()),
            })?;
            file.write_all(text.as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|err| Error::Malformed {
                    content_type: "savings-ledger",
                    detail: format!("cannot write {}: {err}", temporary.display()),
                })?;
        }

        std::fs::rename(&temporary, path).map_err(|err| Error::Malformed {
            content_type: "savings-ledger",
            detail: format!("cannot replace {}: {err}", path.display()),
        })
    }
}

/// The scratch path a save writes through.
///
/// Beside the target rather than in the system temporary directory, because a rename
/// across filesystems is not atomic and `/tmp` is frequently a different filesystem.
fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hours_ago: u64, now: SystemTime) -> SystemTime {
        now - Duration::from_secs(hours_ago * BUCKET_SECONDS)
    }

    #[test]
    fn totals_accumulate_across_periods() {
        let now = SystemTime::now();
        let mut ledger = SavingsLedger::new();

        ledger.record(at(5, now), "claude", "json", 1000, 400);
        ledger.record(at(2, now), "claude", "json", 500, 200);
        ledger.record(now, "gpt", "logs", 300, 100);

        let total = ledger.total_since(None);
        assert_eq!(total.compressions, 3);
        assert_eq!(total.tokens_before, 1800);
        assert_eq!(total.tokens_after, 700);
        assert_eq!(total.saved(), 1100);
    }

    #[test]
    fn a_window_excludes_what_came_before_it() {
        let now = SystemTime::now();
        let mut ledger = SavingsLedger::new();

        ledger.record(at(48, now), "claude", "json", 1000, 400);
        ledger.record(at(1, now), "claude", "json", 200, 50);

        let recent = ledger.total_since(Some(at(24, now)));
        assert_eq!(recent.compressions, 1);
        assert_eq!(recent.tokens_before, 200);
    }

    #[test]
    fn savings_survive_a_restart() {
        // The whole point. Everything before this reset when the process did.
        let dir = std::env::temp_dir().join(format!("headroom-ledger-{}", std::process::id()));
        let path = dir.join("savings.json");
        let now = SystemTime::now();

        let mut ledger = SavingsLedger::new();
        ledger.record(now, "claude", "json", 1000, 250);
        ledger.save(&path).expect("saves");

        let reloaded = SavingsLedger::load(&path);
        assert_eq!(reloaded.total_since(None).saved(), 750);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_same_period_aggregates_rather_than_appending() {
        // The property that bounds growth. A thousand compressions in one hour against
        // one model and one content type is one row, not a thousand.
        let now = SystemTime::now();
        let mut ledger = SavingsLedger::new();

        for _ in 0..1000 {
            ledger.record(now, "claude", "json", 100, 40);
        }

        assert_eq!(ledger.buckets().len(), 1, "the ledger grew per request");
        assert_eq!(ledger.total_since(None).compressions, 1000);
        assert_eq!(ledger.total_since(None).saved(), 60_000);
    }

    #[test]
    fn pruning_drops_what_is_past_retention() {
        let now = SystemTime::now();
        let mut ledger = SavingsLedger::new();

        ledger.record(at(24 * 120, now), "claude", "json", 100, 40);
        ledger.record(now, "claude", "json", 100, 40);
        assert_eq!(ledger.buckets().len(), 2);

        let dropped = ledger.prune(now);
        assert_eq!(dropped, 1);
        assert_eq!(ledger.buckets().len(), 1);
        assert_eq!(ledger.total_since(None).compressions, 1);
    }

    #[test]
    fn buckets_order_chronologically_rather_than_lexically() {
        // Zero-padding is why. Unpadded, hour 9 sorts after hour 10 and every range
        // query reads the wrong set — a defect that only appears once the proxy has been
        // running long enough for the hour count to change digit width.
        let keys = [bucket_key(9, "m", "json"), bucket_key(10, "m", "json")];
        assert!(keys[0] < keys[1], "hour 9 sorted after hour 10");
    }

    #[test]
    fn a_corrupt_ledger_starts_fresh_rather_than_failing() {
        // A proxy that refuses to start because its savings history is unreadable has
        // traded a reporting feature for an outage.
        let dir = std::env::temp_dir().join(format!("headroom-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("savings.json");
        std::fs::write(&path, "{not json").unwrap();

        let ledger = SavingsLedger::load(&path);
        assert_eq!(ledger.total_since(None).compressions, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_absent_ledger_loads_empty() {
        let ledger = SavingsLedger::load(Path::new("/nonexistent/headroom/savings.json"));
        assert_eq!(ledger.buckets().len(), 0);
    }

    #[test]
    fn passthroughs_are_counted_so_the_totals_have_a_denominator() {
        let now = SystemTime::now();
        let mut ledger = SavingsLedger::new();

        ledger.record(now, "claude", "json", 100, 40);
        ledger.record_passthrough(now, "claude", "prose");

        let total = ledger.total_since(None);
        assert_eq!(total.compressions, 1);
        assert_eq!(total.passthroughs, 1);
    }
}
