//! # headroom-mcp
//!
//! An MCP server exposing compression to agents that speak the Model Context Protocol:
//! `headroom_compress`, `headroom_retrieve`, and `headroom_stats`.
//!
//! `headroom_retrieve` is what makes the lossy compressors safe to use — the model can
//! always ask for the original content behind a CCR marker.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod protocol;
pub mod server;

pub use server::McpServer;
