//! The `headroom-proxy` binary.

#![forbid(unsafe_code)]

use headroom_proxy::{config::Config, server};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let config = Config::from_env();
    server::serve(&config).await
}
