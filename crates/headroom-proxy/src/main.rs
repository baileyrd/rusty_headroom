//! The `headroom-proxy` binary.

#![forbid(unsafe_code)]

use headroom_proxy::{config::Config, server};

/// Environment variable controlling log verbosity.
const LOG_VAR: &str = "HEADROOM_LOG";

#[tokio::main]
async fn main() -> std::io::Result<()> {
    init_logging();

    let config = Config::from_env();
    server::serve(&config).await
}

/// Installs the log subscriber.
///
/// # Why this is not optional
///
/// `tracing` macros compile to nothing observable unless a subscriber is installed.
/// Without this call the proxy emits no startup line, no request log, and no warning —
/// including the volatile-content warning whose entire purpose is to tell an operator
/// why their cache is missing. Every one of those calls silently succeeds, so nothing
/// in a test or a code review reveals the omission; it shows up only as a process that
/// runs and says nothing.
///
/// # Logs go to stderr
///
/// stdout belongs to the response path in every other binary in this workspace, and
/// keeping the convention here means `headroom-proxy 2>/dev/null` is a meaningful thing
/// to type.
fn init_logging() {
    use tracing_subscriber::filter::EnvFilter;

    // `warn` by default rather than `info`. The proxy logs a line per request, and a
    // default that fills a terminal with one line per API call is a default people turn
    // off entirely — taking the warnings with it.
    let filter = EnvFilter::try_from_env(LOG_VAR)
        .or_else(|_| EnvFilter::try_new("headroom_proxy=warn"))
        // The last resort still installs *something*: a proxy that fails to start
        // because a log filter would not parse is a worse outcome than one logging at
        // the wrong level.
        .unwrap_or_default();

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}
