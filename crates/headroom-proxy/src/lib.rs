//! # headroom-proxy
//!
//! A byte-faithful compressing reverse proxy for LLM provider APIs.
//!
//! The proxy's first duty is to be invisible. Bytes it does not deliberately
//! transform reach the upstream provider exactly as the client sent them — invariant
//! I1 — because a proxy that re-serializes JSON in passing has already invalidated
//! the provider's cached prefix and made every request more expensive while
//! appearing to do nothing.
//!
//! Compression applies only to the live zone, and only when it demonstrably reduces
//! the token count.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod admin;
pub mod body;
pub mod ccr_api;
pub mod compression;
pub mod config;
pub mod frozen;
pub mod guard;
pub mod headers;
pub mod health;
pub mod ledger;
pub mod linked_memory;
pub mod metrics;
pub mod observe;
pub mod openai;
pub mod server;
pub mod sse;
pub mod stabilization;
pub mod toin;
pub mod upstream;
pub mod volatile;
pub mod websocket;

pub use config::Config;

/// The lock every test that mutates the process-wide override map must hold.
///
/// One lock rather than one per module. `config` and `admin` both drive
/// `set_overrides`/`clear_overrides`, and while each held its own mutex they serialized
/// against themselves and raced each other — which surfaced as `admin`'s
/// upstream-divergence test reading a `config` test's overrides.
#[cfg(test)]
pub(crate) fn settings_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    &LOCK
}
