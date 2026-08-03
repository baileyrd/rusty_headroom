//! Server-sent event handling.
//!
//! Streaming responses are the common case for agent traffic, and the one where a
//! proxy can do most damage. Everything here is built so the bytes a client receives
//! are the bytes the provider sent.

mod anthropic;
mod framing;
mod observer;
mod openai;
mod responses;

pub use anthropic::{classify, AnthropicEvent, DeltaKind, StreamObserver};
pub use framing::{render, Event, SseParser};
pub use observer::Observer;
pub use openai::{classify as classify_openai, OpenAiEvent, OpenAiObserver};
pub use responses::{classify as classify_responses, Phase, ResponsesEvent, ResponsesObserver};
