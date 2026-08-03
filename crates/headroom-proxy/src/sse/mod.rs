//! Server-sent event handling.
//!
//! Streaming responses are the common case for agent traffic, and the one where a
//! proxy can do most damage. Everything here is built so the bytes a client receives
//! are the bytes the provider sent.

mod anthropic;
mod framing;

pub use anthropic::{classify, AnthropicEvent, DeltaKind, StreamObserver};
pub use framing::{render, Event, SseParser};
