//! The `headroom-mcp` binary: an MCP server over stdio.
//!
//! MCP clients speak newline-delimited JSON-RPC over a child process's stdin and
//! stdout. Nothing but protocol may go to stdout — a stray `println!` would be read as
//! a malformed message and desynchronize the session — so all diagnostics go to
//! stderr.

#![forbid(unsafe_code)]

use std::io::{BufRead, Write};
use std::sync::Arc;

use headroom_core::ccr::{CcrStore, FileCcrStore, InMemoryCcrStore};
use headroom_mcp::protocol::{failure, ErrorCode, Line, Request};
use headroom_mcp::McpServer;

/// Where originals are cached, if a persistent location is configured.
const STORE_DIR_VAR: &str = "HEADROOM_CCR_DIR";

/// A shared CCR store, for retrieving originals another process compressed.
const REDIS_URL_VAR: &str = "HEADROOM_REDIS_URL";

/// How often the background thread sweeps the CCR store for expired entries.
///
/// [`CcrStore::get`] already treats an expired entry as absent, so nothing on the
/// retrieval path depends on this running promptly. What depends on it is process
/// memory (or disk, for a file-backed store): every lossy compression the proxy does
/// writes a new TTL'd entry here, and without a sweep the backing map/directory grows
/// for the life of the process. Every compressor sets its `CCR_TTL` to 24 hours, so
/// five minutes is well inside a tenth of the shortest TTL in use — frequent enough
/// that the store never accumulates more than a few minutes' worth of expired
/// entries, without costing anything measurable.
const CCR_PURGE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Removes expired entries from `store` once, returning and logging how many were
/// removed.
///
/// Split out from [`spawn_ccr_purge_thread`]'s scheduling loop so a test can call this
/// directly against a store it seeded with expired entries, without waiting on a real
/// sleep to prove the wiring works.
fn purge_ccr_once(store: &dyn CcrStore) -> usize {
    let purged = store.purge_expired();
    if purged > 0 {
        tracing::debug!(purged, "purged expired CCR entries");
    } else {
        tracing::trace!("CCR purge pass found nothing expired");
    }
    purged
}

/// Spawns the background thread that keeps the CCR store from growing without bound.
///
/// # Why this exists
///
/// [`CcrStore::get`] filters expired entries out of read results but never removes
/// them from the backing map or directory — [`CcrStore::purge_expired`] is the only
/// thing that does, and nothing in this binary called it before this thread existed.
/// Under ordinary sustained traffic that meant an in-memory or file-backed store grew
/// for the life of the process. A Redis-backed store is unaffected — it expires keys
/// natively — but is not the default backend.
///
/// # Why a thread rather than a task
///
/// This binary has no async runtime — `main` blocks reading stdin line by line — so
/// there is nowhere to `tokio::spawn` this onto. A plain OS thread is not joined; it
/// runs alongside the read loop for as long as the process does, and is torn down
/// with everything else when `main` returns.
fn spawn_ccr_purge_thread(store: Arc<dyn CcrStore>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(CCR_PURGE_INTERVAL);
        purge_ccr_once(store.as_ref());
    });
}

/// Opens the shared store named by [`REDIS_URL_VAR`].
///
/// # Why this matters more here than in the proxy
///
/// This binary is the *retrieval* half. The proxy stores an original and sends the model
/// a `<<ccr:HASH>>` marker; the model later calls `headroom_retrieve` with that hash, and
/// it arrives here — in a different process. A local store answers "expired" for
/// everything the proxy compressed, which reads to the model as content that vanished.
#[cfg(feature = "redis")]
fn open_redis(url: &str) -> Option<Arc<dyn headroom_core::ccr::CcrStore>> {
    match headroom_core::ccr::RedisCcrStore::connect(url) {
        Ok(store) => Some(Arc::new(store)),
        Err(err) => {
            eprintln!("headroom-mcp: could not connect to {url}: {err}; falling back");
            None
        }
    }
}

/// The same, for a build without the `redis` feature.
///
/// Named explicitly rather than silently ignored: an operator who set the variable and
/// got a local store would see retrievals fail exactly as if the server were down.
#[cfg(not(feature = "redis"))]
fn open_redis(_url: &str) -> Option<Arc<dyn headroom_core::ccr::CcrStore>> {
    eprintln!("headroom-mcp: this build has no redis support; rebuild with --features redis");
    None
}

fn main() -> std::io::Result<()> {
    // Persistence is opt-in. A model that asks to retrieve content after a restart
    // should get it, but writing files to an unasked-for location is worse than not,
    // so this only happens when a directory is named.
    // A shared store wins over a local directory: anyone who configured one has more
    // than one process, which is the case a local directory cannot serve.
    let shared = std::env::var(REDIS_URL_VAR)
        .ok()
        .filter(|url| !url.trim().is_empty())
        .and_then(|url| open_redis(url.trim()));

    let store: Arc<dyn headroom_core::ccr::CcrStore> = match (shared, std::env::var(STORE_DIR_VAR))
    {
        (Some(store), _) => store,
        (None, Ok(dir)) if !dir.trim().is_empty() => match FileCcrStore::open(dir.trim()) {
            Ok(store) => Arc::new(store),
            Err(err) => {
                eprintln!("headroom-mcp: could not open {dir}: {err}; using memory");
                Arc::new(InMemoryCcrStore::new())
            }
        },
        _ => Arc::new(InMemoryCcrStore::new()),
    };

    spawn_ccr_purge_thread(Arc::clone(&store));

    let server = McpServer::new(store);
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => server.handle(&request),
            // A malformed line still gets a reply. Staying silent would leave a client
            // waiting forever for a response that is never coming.
            Err(err) => Some(failure(
                None,
                ErrorCode::ParseError,
                &format!("could not parse request: {err}"),
            )),
        };

        if let Some(response) = response {
            writeln!(stdout, "{}", Line(response))?;
            // Flushed per message: a client blocks until it sees the reply, so a
            // buffered response is a hang rather than a delay.
            stdout.flush()?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use headroom_core::ccr::ContentHash;
    use std::time::Duration;

    #[test]
    fn purge_ccr_once_removes_expired_entries_from_the_store() {
        // Regression: `get` already treats an expired entry as absent, but nothing in
        // this binary ever called `purge_expired` on the store it built, so an
        // in-memory or file-backed store's expired entries lived for the process's
        // whole life. This exercises `purge_ccr_once` directly — the piece
        // `spawn_ccr_purge_thread` schedules on a sleeping loop — so the fix is
        // proven without sleeping for a real interval.
        let store = InMemoryCcrStore::new();
        store
            .put(ContentHash::of(b"stale"), b"stale", Duration::ZERO)
            .unwrap();
        store
            .put(
                ContentHash::of(b"fresh"),
                b"fresh",
                Duration::from_secs(300),
            )
            .unwrap();
        assert_eq!(
            store.len(),
            1,
            "len() already excludes the expired entry, before any purge runs"
        );

        let purged = purge_ccr_once(&store);

        assert_eq!(purged, 1, "exactly the expired entry was removed");
        assert_eq!(store.len(), 1, "the live entry is still retrievable");
        assert_eq!(
            purge_ccr_once(&store),
            0,
            "a second pass has nothing left to collect"
        );
    }
}
