//! # headroom-core
//!
//! The context compression engine: content routing, type-aware compressors, token
//! counting, and the CCR reversible-compression store.
//!
//! ## The idea
//!
//! Compressing an LLM conversation is not a matter of deciding what to drop from
//! history. Providers cache the prompt prefix, and any edit to an earlier turn
//! invalidates that cache — so "compression" that rewrites history can cost more
//! than it saves, while also degrading the model's context.
//!
//! The approach here is the opposite: **the frozen prefix is sacred, and only the
//! live zone is compressed.** The system prompt, the tool definitions, and every
//! turn the provider has already seen are forwarded byte-for-byte identical. Only
//! the newest content — the latest user message and the latest tool results — is
//! eligible, and even then compression is applied per-block, in place, and is
//! discarded unless it actually reduces the token count.
//!
//! ## Invariants
//!
//! The design is expressed as ten invariants that every component upholds. They are
//! referenced by identifier throughout the codebase:
//!
//! | ID | Invariant |
//! |----|-----------|
//! | I1 | Byte-faithful passthrough on unmutated bytes |
//! | I2 | The cache hot zone is never modified |
//! | I3 | Append-only — compression touches the live zone only |
//! | I4 | Determinism — same input always yields byte-equal output |
//! | I5 | Token-aware, not byte-aware — validate, and fall back to the original |
//! | I6 | Position-preserving — in-place block edits, side-channel metadata only |
//! | I7 | Tool definitions are normalized, never compressed |
//! | I8 | Signed, encrypted, and redacted content is passthrough-only |
//! | I9 | Telemetry observes; it never influences request-time decisions |
//! | I10 | Auth mode gates compression policy |
//!
//! See `ARCHITECTURE.md` for the full discussion.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod auth_mode;
pub mod block;
pub mod ccr;
pub mod code_compressor;
pub mod conversation;
pub mod detection;
pub mod diff_compressor;
pub mod error;
pub mod live_zone;
pub mod log_compressor;
pub mod memory;
pub mod output_shaping;
pub mod pipeline;
pub mod relevance;
pub mod search_compressor;
pub mod signals;
pub mod smart_crusher;
pub mod telemetry;
pub mod text_crusher;
pub mod tokenizer;
pub mod transform;
pub mod validate;

pub use auth_mode::{classify_auth_mode, AuthMode, CompressionPolicy};
pub use block::{Block, BlockKind};
pub use ccr::{CcrStore, ContentHash};
pub use code_compressor::CodeCompressor;
pub use conversation::{Conversation, Message, Role};
pub use diff_compressor::DiffCompressor;
pub use error::{Declined, Error, Result};
pub use live_zone::{live_zone, LiveZone, Location};
pub use log_compressor::LogCompressor;
pub use search_compressor::SearchCompressor;
pub use smart_crusher::SmartCrusher;
pub use text_crusher::{TextCrusher, TextSummarizer};
pub use transform::{LosslessTransform, LossyTransform, Transform};
pub use validate::{validated_apply, Outcome};
