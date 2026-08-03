//! Error taxonomy for the compression engine.
//!
//! The split between the variants here is deliberate and load-bearing rather than
//! cosmetic. Invariant I5 requires that a compression which fails to help is
//! *recoverable* — the pipeline forwards the original bytes and carries on. That
//! only works if "this transform declined" is distinguishable from "this input is
//! malformed" and from "this is a bug in our code". Collapsing the three into one
//! variant makes the fallback path impossible to express without also swallowing
//! real defects.

use std::fmt;

/// The reason a transform chose not to compress a block.
///
/// A [`Declined`](Error::Declined) outcome is a normal, expected result — not a
/// failure. Every variant here means "forward the original bytes unchanged".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Declined {
    /// The block is below the size threshold for its content type, so compression
    /// overhead would exceed the savings. See the adaptive sizer.
    BelowThreshold,
    /// The content type is one this transform does not handle.
    WrongContentType,
    /// Compression ran but did not reduce the token count, so invariant I5
    /// requires forwarding the original.
    NotSmaller,
    /// The block carries a signature, encrypted content, or redacted thinking data,
    /// all of which are passthrough-only under invariant I8.
    Sacrosanct,
}

impl fmt::Display for Declined {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            Self::BelowThreshold => "content is below the compression size threshold",
            Self::WrongContentType => "content type is not handled by this transform",
            Self::NotSmaller => "compression did not reduce the token count",
            Self::Sacrosanct => "block is signed, encrypted, or redacted and is passthrough-only",
        };
        f.write_str(reason)
    }
}

/// Errors produced by the compression engine.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A transform declined to compress. This is an ordinary outcome: the caller
    /// forwards the original bytes. It is an error variant only so that transforms
    /// can return early with `?` rather than threading an `Option` through every
    /// layer.
    #[error("compression declined: {0}")]
    Declined(Declined),

    /// The input could not be parsed as the content type it was routed to.
    ///
    /// Distinct from [`Error::Declined`] because it means detection was wrong about
    /// this block. The block is still forwarded unchanged, but a rising rate here
    /// is a signal that the content router needs attention.
    #[error("malformed {content_type} input: {detail}")]
    Malformed {
        /// The content type the block was routed to.
        content_type: &'static str,
        /// What specifically failed to parse.
        detail: String,
    },

    /// An invariant was violated. This is always a bug in this codebase, never
    /// something a caller can cause with unusual input.
    ///
    /// These must not be silently swallowed by the fallback path — a transform that
    /// mutated the frozen prefix has already done the damage the fallback was meant
    /// to prevent, and the loud failure is the point.
    #[error("invariant {invariant} violated: {detail}")]
    InvariantViolation {
        /// The invariant identifier, for example `"I3"`.
        invariant: &'static str,
        /// What was observed.
        detail: String,
    },

    /// A CCR backend operation failed.
    #[error("CCR store: {0}")]
    CcrStore(String),

    /// JSON serialization or deserialization failed.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// An I/O operation failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    /// Returns `true` when this error means "forward the original bytes and carry
    /// on" rather than "something is wrong".
    ///
    /// The fallback path in the pipeline uses this to decide whether to recover or
    /// propagate. [`Error::InvariantViolation`] deliberately returns `false`: it
    /// signals that damage has already occurred, and recovering from it would hide
    /// exactly the class of bug this project is built to prevent.
    pub fn is_recoverable(&self) -> bool {
        matches!(self, Self::Declined(_) | Self::Malformed { .. })
    }

    /// Shorthand for declining with a reason.
    pub fn declined(reason: Declined) -> Self {
        Self::Declined(reason)
    }
}

/// The crate's result alias.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declined_and_malformed_are_recoverable() {
        assert!(Error::declined(Declined::NotSmaller).is_recoverable());
        assert!(Error::Malformed {
            content_type: "json",
            detail: "unexpected end of input".into(),
        }
        .is_recoverable());
    }

    #[test]
    fn invariant_violations_are_not_recoverable() {
        // This is the important one. An invariant violation must never be absorbed
        // by the compression fallback path — see the doc comment on `is_recoverable`.
        let err = Error::InvariantViolation {
            invariant: "I3",
            detail: "transform wrote to message index 2 with frozen_message_count 5".into(),
        };
        assert!(!err.is_recoverable());
    }

    #[test]
    fn ccr_and_io_failures_are_not_recoverable() {
        assert!(!Error::CcrStore("backend unreachable".into()).is_recoverable());
    }

    #[test]
    fn declined_reasons_render_distinctly() {
        // Each reason should read as a distinct sentence in logs; a duplicated
        // message would make two different decline paths indistinguishable in
        // production telemetry.
        let all = [
            Declined::BelowThreshold,
            Declined::WrongContentType,
            Declined::NotSmaller,
            Declined::Sacrosanct,
        ];
        let rendered: Vec<String> = all.iter().map(|d| d.to_string()).collect();
        let mut deduped = rendered.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(rendered.len(), deduped.len());
    }
}
