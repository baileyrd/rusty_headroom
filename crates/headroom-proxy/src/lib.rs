//! # headroom-proxy
//!
//! A byte-faithful compressing reverse proxy for LLM provider APIs.
//!
//! The proxy's first duty is to be invisible: bytes it does not deliberately
//! transform reach the upstream provider exactly as the client sent them (invariant
//! I1). Compression is applied only to the live zone, and only when it demonstrably
//! reduces the token count.
//!
//! Handlers, the SSE state machine, and cache stabilization land in later issues.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
