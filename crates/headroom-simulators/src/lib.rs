//! # headroom-simulators
//!
//! Fake Anthropic and OpenAI endpoints so end-to-end tests can exercise the proxy
//! without network access, credentials, or spend.
//!
//! These simulators are also where byte-equality assertions live: a test sends a
//! recorded payload through the proxy and the simulator asserts on the exact bytes
//! that arrived, which is how invariant I1 is enforced in CI.
//!
//! The simulators land in issue E1.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
