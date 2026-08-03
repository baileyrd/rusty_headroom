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

pub mod config;
pub mod health;
pub mod server;

pub use config::Config;
