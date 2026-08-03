//! Token counting.
//!
//! Invariant I5 — *token-aware, not byte-aware* — requires validating every
//! compression against a tokenizer and forwarding the original whenever the
//! "compressed" form does not actually cost fewer tokens. That check sits on every
//! compression path, which makes this module a dependency of essentially the whole
//! pipeline.
//!
//! Exact tokenizers for specific model families arrive later behind the
//! [`Tokenizer`] trait. What ships here is the estimator that is always available,
//! needs no model files, and is correct enough to make the I5 decision safely.

mod estimator;

pub use estimator::HeuristicEstimator;

/// Counts tokens in text.
///
/// Implementations must be deterministic: the same input always produces the same
/// count, on every run and in every process. Invariant I4 depends on it, because a
/// token count that varies run to run makes the I5 keep-or-discard decision vary
/// with it, which makes the bytes sent upstream vary — and that busts the prompt
/// cache this project exists to protect.
pub trait Tokenizer: Send + Sync {
    /// Returns the number of tokens `text` encodes to.
    fn count(&self, text: &str) -> usize;

    /// A short identifier for this tokenizer, used in telemetry.
    fn name(&self) -> &'static str;

    /// Whether this tokenizer is exact for its model family, or an approximation.
    ///
    /// Approximate tokenizers must never under-count — see [`HeuristicEstimator`]
    /// for why that direction is the safe one.
    fn is_exact(&self) -> bool {
        false
    }
}
