//! # headroom-mcp
//!
//! An MCP server exposing compression to agents that speak the Model Context
//! Protocol: `headroom_compress`, `headroom_retrieve`, and `headroom_stats`.
//!
//! `headroom_retrieve` is what makes lossy compression safe to use — the model can
//! always ask for the original content behind a CCR marker.
//!
//! The server implementation lands in later issues.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
