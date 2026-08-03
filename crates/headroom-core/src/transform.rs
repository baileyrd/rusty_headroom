//! Transform traits.
//!
//! A transform compresses the content of a single [`Block`], in place. The shape of
//! these traits is where invariants I6 and I10 are enforced.
//!
//! # I6 — position-preserving, by signature
//!
//! Every transform is `fn(&mut Block) -> Result<()>`. It receives one block and
//! returns nothing. There is no way to express "reorder the content array", "split
//! this block into two", or "attach a field to this block", because a transform is
//! never handed the array and never returns a replacement.
//!
//! This matters because the reference project's original defect class came from
//! compressors that *were* free to restructure the message array — dropping old
//! turns, replacing spans with summaries, reordering blocks. Every one of those
//! busts the provider's prompt cache. Making the restructuring unrepresentable is a
//! stronger guarantee than forbidding it in review.
//!
//! # I10 — lossless and lossy are different types
//!
//! Auth mode gates which transforms may run: pay-as-you-go traffic can use lossy
//! compression, while OAuth and subscription traffic is restricted to lossless
//! transforms only. Splitting that into two traits means the policy gate is a type
//! signature rather than a runtime `if`, so a lossy transform cannot reach a
//! restricted path even by mistake.

use crate::block::Block;
use crate::error::{Declined, Error, Result};

/// Shared behavior for every transform.
pub trait Transform {
    /// A short identifier for telemetry and error messages.
    fn name(&self) -> &'static str;

    /// Compresses `block` in place.
    ///
    /// Returning [`Error::Declined`] is a normal outcome meaning "leave this block
    /// alone" — the caller forwards the original content and carries on.
    ///
    /// # Contract
    ///
    /// An implementation may only call [`Block::content_mut`] or
    /// [`Block::replace_content`]. It must not depend on wall-clock time, randomness,
    /// or any state carried between calls: invariant I4 requires the same block to
    /// transform identically on every run.
    fn apply(&self, block: &mut Block) -> Result<()>;
}

/// A transform whose output preserves all information in the input.
///
/// Lossless here means *no information is discarded* — whitespace normalization,
/// JSON minification, removal of redundant structure. The model can recover
/// everything it could have learned from the original.
///
/// These are safe on every auth mode, including subscription traffic where any
/// detectable modification is a risk.
pub trait LosslessTransform: Transform {}

/// A transform that discards information to save tokens.
///
/// Lossy transforms elide, summarize, or sample. What they remove is recoverable
/// only through CCR: the original is stored, the compressed block carries a
/// `<<ccr:HASH>>` marker, and the model can retrieve it on demand.
///
/// Restricted to pay-as-you-go traffic under invariant I10.
pub trait LossyTransform: Transform {
    /// Whether the original must be stored in CCR before this transform runs.
    ///
    /// Defaults to `true`, and that default is the safe direction: a lossy transform
    /// that discards content with no retrieval path has permanently removed
    /// information from the conversation.
    fn requires_ccr(&self) -> bool {
        true
    }
}

/// Runs `transform` on `block`, rejecting blocks it must never touch.
///
/// This is the gate every transform goes through. It exists so the sacrosanct-block
/// check lives in exactly one place — a check duplicated across a dozen transforms
/// is a check that will eventually be omitted from the thirteenth.
///
/// # Example
///
/// ```
/// use headroom_core::block::{Block, BlockKind};
/// use headroom_core::transform::{apply_guarded, Transform};
/// use headroom_core::{Error, Result};
///
/// struct Upper;
/// impl Transform for Upper {
///     fn name(&self) -> &'static str { "upper" }
///     fn apply(&self, block: &mut Block) -> Result<()> {
///         let upper = block.content().to_uppercase();
///         block.replace_content(upper);
///         Ok(())
///     }
/// }
///
/// // A thinking block carries a provider signature and is refused outright.
/// let mut thinking = Block::new(BlockKind::Thinking, "reasoning");
/// assert!(apply_guarded(&Upper, &mut thinking).is_err());
/// assert_eq!(thinking.content(), "reasoning");
/// ```
pub fn apply_guarded<T: Transform + ?Sized>(transform: &T, block: &mut Block) -> Result<()> {
    if block.kind().is_sacrosanct() {
        return Err(Error::declined(Declined::Sacrosanct));
    }
    if !block.kind().is_compressible() {
        return Err(Error::declined(Declined::WrongContentType));
    }
    transform.apply(block)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::BlockKind;

    /// Uppercases content. Stands in for any well-behaved transform.
    struct Shouty;

    impl Transform for Shouty {
        fn name(&self) -> &'static str {
            "shouty"
        }
        fn apply(&self, block: &mut Block) -> Result<()> {
            let upper = block.content().to_uppercase();
            block.replace_content(upper);
            Ok(())
        }
    }

    impl LosslessTransform for Shouty {}

    /// Always declines. Stands in for a transform handed content it does not handle.
    struct Abstains;

    impl Transform for Abstains {
        fn name(&self) -> &'static str {
            "abstains"
        }
        fn apply(&self, _block: &mut Block) -> Result<()> {
            Err(Error::declined(Declined::WrongContentType))
        }
    }

    #[test]
    fn a_transform_can_rewrite_content() {
        let mut block = Block::new(BlockKind::Text, "hello");
        apply_guarded(&Shouty, &mut block).unwrap();
        assert_eq!(block.content(), "HELLO");
    }

    #[test]
    fn every_sacrosanct_kind_is_refused_and_left_byte_identical() {
        // Invariant I8. The content must be untouched, not merely "mostly" untouched
        // — a signature is invalidated by a single byte.
        for kind in [
            BlockKind::Thinking,
            BlockKind::RedactedThinking,
            BlockKind::Reasoning,
            BlockKind::Compaction,
            BlockKind::Attachment,
        ] {
            let original = "opaque provider material";
            let mut block = Block::new(kind, original);
            let err = apply_guarded(&Shouty, &mut block).unwrap_err();

            assert!(
                matches!(err, Error::Declined(Declined::Sacrosanct)),
                "{kind}"
            );
            assert_eq!(block.content(), original, "{kind} content was modified");
            assert!(err.is_recoverable(), "{kind} refusal must be recoverable");
        }
    }

    #[test]
    fn tool_use_blocks_are_refused() {
        // Not sacrosanct, but still passthrough: re-serializing tool arguments
        // reorders JSON keys and busts the cache.
        let mut block = Block::new(BlockKind::ToolUse, r#"{"path":"/tmp","limit":10}"#);
        let err = apply_guarded(&Shouty, &mut block).unwrap_err();
        assert!(matches!(err, Error::Declined(Declined::WrongContentType)));
        assert_eq!(block.content(), r#"{"path":"/tmp","limit":10}"#);
    }

    #[test]
    fn tool_results_are_compressible_and_keep_their_call_binding() {
        let mut block = Block::tool_result("some output", "toolu_xyz").with_error(true);
        apply_guarded(&Shouty, &mut block).unwrap();

        assert_eq!(block.content(), "SOME OUTPUT");
        // The association with the originating call must survive compression, or the
        // provider cannot match the result to its call.
        assert_eq!(block.tool_use_id(), Some("toolu_xyz"));
        assert!(block.is_error());
        assert_eq!(block.kind(), BlockKind::ToolResult);
    }

    #[test]
    fn declining_leaves_the_block_untouched() {
        let mut block = Block::new(BlockKind::Text, "unchanged");
        let err = apply_guarded(&Abstains, &mut block).unwrap_err();
        assert!(err.is_recoverable());
        assert_eq!(block.content(), "unchanged");
    }

    #[test]
    fn transforms_are_deterministic_across_repeated_application() {
        // Invariant I4: same input, same output, every time.
        let run = || {
            let mut b = Block::new(BlockKind::Text, "repeatable input");
            apply_guarded(&Shouty, &mut b).unwrap();
            b.content().to_owned()
        };
        let first = run();
        for _ in 0..50 {
            assert_eq!(run(), first);
        }
    }

    #[test]
    fn lossy_transforms_require_ccr_by_default() {
        struct Truncate;
        impl Transform for Truncate {
            fn name(&self) -> &'static str {
                "truncate"
            }
            fn apply(&self, block: &mut Block) -> Result<()> {
                let head: String = block.content().chars().take(4).collect();
                block.replace_content(head);
                Ok(())
            }
        }
        impl LossyTransform for Truncate {}

        // The default must be `true`. A lossy transform with no retrieval path has
        // permanently destroyed information.
        assert!(Truncate.requires_ccr());
    }
}
