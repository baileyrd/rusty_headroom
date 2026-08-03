//! The `headroom-mcp` binary: an MCP server over stdio.
//!
//! MCP clients speak newline-delimited JSON-RPC over a child process's stdin and
//! stdout. Nothing but protocol may go to stdout — a stray `println!` would be read as
//! a malformed message and desynchronize the session — so all diagnostics go to
//! stderr.

#![forbid(unsafe_code)]

use std::io::{BufRead, Write};
use std::sync::Arc;

use headroom_core::ccr::{FileCcrStore, InMemoryCcrStore};
use headroom_mcp::protocol::{failure, ErrorCode, Line, Request};
use headroom_mcp::McpServer;

/// Where originals are cached, if a persistent location is configured.
const STORE_DIR_VAR: &str = "HEADROOM_CCR_DIR";

fn main() -> std::io::Result<()> {
    // Persistence is opt-in. A model that asks to retrieve content after a restart
    // should get it, but writing files to an unasked-for location is worse than not,
    // so this only happens when a directory is named.
    let store: Arc<dyn headroom_core::ccr::CcrStore> = match std::env::var(STORE_DIR_VAR) {
        Ok(dir) if !dir.trim().is_empty() => match FileCcrStore::open(dir.trim()) {
            Ok(store) => Arc::new(store),
            Err(err) => {
                eprintln!("headroom-mcp: could not open {dir}: {err}; using memory");
                Arc::new(InMemoryCcrStore::new())
            }
        },
        _ => Arc::new(InMemoryCcrStore::new()),
    };

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
