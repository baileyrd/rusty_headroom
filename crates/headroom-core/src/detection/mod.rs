//! Content detection and compressor routing.
//!
//! The content router inspects a block of content and decides which type-aware
//! compressor should handle it. It is a pure function over bytes: no I/O, no
//! clocks, no RNG, no mutable state carried between calls.
//!
//! That purity is a requirement rather than a style preference. Invariant I4 says
//! the same request bytes must always produce the same output bytes; a router that
//! consulted accumulated statistics would make routing — and therefore the bytes
//! sent upstream — depend on traffic history. That is precisely the request-time
//! telemetry feedback loop invariant I9 forbids.

mod adaptive_sizer;
mod router;

pub use adaptive_sizer::AdaptiveSizer;
pub use router::{detect, ContentType, Detection};
