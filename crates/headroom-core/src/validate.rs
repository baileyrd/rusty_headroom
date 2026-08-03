//! Token validation — invariant I5's safety net.
//!
//! > If `compressed.tokens >= original.tokens`, forward the original.
//!
//! This is the single check that makes every compressor safe to add. A transform
//! that misbehaves on some unanticipated input shape costs a wasted tokenizer pass
//! instead of a larger prompt and a worse conversation. Without it, every new
//! compressor is a potential regression on input nobody thought to test.
//!
//! Two properties matter as much as the comparison itself:
//!
//! - **No transform can opt out.** Validation wraps dispatch rather than being
//!   something each transform calls, so there is no path around it.
//! - **The fallback restores the exact original bytes**, not a re-rendering of them.
//!   Invariant I1 is about byte-faithfulness, and a fallback that reconstructed
//!   equivalent-but-different bytes would violate the thing it exists to protect.

use crate::block::Block;
use crate::error::{Declined, Error, Result};
use crate::tokenizer::Tokenizer;
use crate::transform::{apply_guarded, Transform};

/// What happened when a transform was applied under validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The transform ran and reduced the token count. The block now holds the
    /// compressed content.
    Compressed {
        /// Token count before compression.
        before: usize,
        /// Token count after compression.
        after: usize,
    },
    /// The transform declined, or its output was not smaller. The block holds its
    /// original content, byte for byte.
    Unchanged {
        /// Why nothing was applied.
        reason: Declined,
    },
}

impl Outcome {
    /// Tokens saved. Zero when nothing was applied.
    pub fn tokens_saved(&self) -> usize {
        match self {
            // Saturating rather than subtracting directly: `after > before` cannot
            // reach here, but a future change to the comparison should degrade to
            // "no saving" rather than panic in release or wrap in debug.
            Self::Compressed { before, after } => before.saturating_sub(*after),
            Self::Unchanged { .. } => 0,
        }
    }

    /// Whether the block's content was modified.
    pub fn is_compressed(&self) -> bool {
        matches!(self, Self::Compressed { .. })
    }
}

/// Applies `transform` to `block`, keeping the result only if it costs fewer tokens.
///
/// The block is left byte-identical unless the transform both succeeded and produced
/// strictly fewer tokens. Errors that are not recoverable — an invariant violation,
/// a store failure — propagate rather than being absorbed, because those mean
/// something is wrong rather than "this content was not worth compressing".
///
/// # Example
///
/// ```
/// use headroom_core::block::{Block, BlockKind};
/// use headroom_core::tokenizer::HeuristicEstimator;
/// use headroom_core::transform::Transform;
/// use headroom_core::validate::{validated_apply, Outcome};
/// use headroom_core::Result;
///
/// // A transform that makes things worse.
/// struct Inflate;
/// impl Transform for Inflate {
///     fn name(&self) -> &'static str { "inflate" }
///     fn apply(&self, block: &mut Block) -> Result<()> {
///         let bigger = format!("{} {}", block.content(), block.content());
///         block.replace_content(bigger);
///         Ok(())
///     }
/// }
///
/// let mut block = Block::new(BlockKind::Text, "some content here");
/// let outcome = validated_apply(&Inflate, &mut block, &HeuristicEstimator::new()).unwrap();
///
/// // The inflated output is discarded and the original survives untouched.
/// assert!(!outcome.is_compressed());
/// assert_eq!(block.content(), "some content here");
/// ```
pub fn validated_apply<T, K>(transform: &T, block: &mut Block, tokenizer: &K) -> Result<Outcome>
where
    T: Transform + ?Sized,
    K: Tokenizer + ?Sized,
{
    let before = tokenizer.count(block.content());

    // Hold the original so it can be restored verbatim. This is the byte-exact
    // content, not a re-serialization of it — see the module docs on I1.
    let original = block.content().to_owned();

    match apply_guarded(transform, block) {
        Ok(()) => {}
        Err(err) if err.is_recoverable() => {
            // A transform may have mutated the block before deciding to give up, so
            // restore unconditionally rather than trusting it to have left things
            // alone.
            block.replace_content(original);
            let reason = match err {
                Error::Declined(reason) => reason,
                // A malformed-input error means detection routed this block wrongly.
                _ => Declined::WrongContentType,
            };
            return Ok(Outcome::Unchanged { reason });
        }
        Err(err) => return Err(err),
    }

    let after = tokenizer.count(block.content());

    if after < before {
        Ok(Outcome::Compressed { before, after })
    } else {
        // Equal counts fall here too. A compression that saves nothing is pure risk:
        // it costs a CCR entry and a retrieval round-trip for zero token benefit.
        block.replace_content(original);
        Ok(Outcome::Unchanged {
            reason: Declined::NotSmaller,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::BlockKind;
    use crate::tokenizer::HeuristicEstimator;

    fn est() -> HeuristicEstimator {
        HeuristicEstimator::new()
    }

    /// Genuinely reduces content.
    struct Shrink;
    impl Transform for Shrink {
        fn name(&self) -> &'static str {
            "shrink"
        }
        fn apply(&self, block: &mut Block) -> Result<()> {
            let short: String = block.content().chars().take(5).collect();
            block.replace_content(short);
            Ok(())
        }
    }

    /// Doubles the content — the misbehaving-compressor case.
    struct Inflate;
    impl Transform for Inflate {
        fn name(&self) -> &'static str {
            "inflate"
        }
        fn apply(&self, block: &mut Block) -> Result<()> {
            let bigger = format!("{} {}", block.content(), block.content());
            block.replace_content(bigger);
            Ok(())
        }
    }

    /// Leaves content exactly as it was.
    struct NoOp;
    impl Transform for NoOp {
        fn name(&self) -> &'static str {
            "noop"
        }
        fn apply(&self, _block: &mut Block) -> Result<()> {
            Ok(())
        }
    }

    /// Mutates, then declines — the case a naive implementation gets wrong.
    struct MutateThenDecline;
    impl Transform for MutateThenDecline {
        fn name(&self) -> &'static str {
            "mutate-then-decline"
        }
        fn apply(&self, block: &mut Block) -> Result<()> {
            block.replace_content("clobbered");
            Err(Error::declined(Declined::BelowThreshold))
        }
    }

    /// Reports an invariant violation — must not be swallowed.
    struct Violates;
    impl Transform for Violates {
        fn name(&self) -> &'static str {
            "violates"
        }
        fn apply(&self, _block: &mut Block) -> Result<()> {
            Err(Error::InvariantViolation {
                invariant: "I3",
                detail: "wrote below frozen_message_count".into(),
            })
        }
    }

    const SAMPLE: &str = "a reasonably long piece of representative tool output text";

    #[test]
    fn a_real_reduction_is_kept() {
        let mut block = Block::new(BlockKind::Text, SAMPLE);
        let outcome = validated_apply(&Shrink, &mut block, &est()).unwrap();

        assert!(outcome.is_compressed());
        assert!(outcome.tokens_saved() > 0);
        assert_eq!(block.content(), "a rea");
    }

    #[test]
    fn an_inflating_transform_is_discarded_and_the_original_survives_byte_exact() {
        // The whole point of I5. Without this the user pays for more tokens than
        // they would have without us.
        let mut block = Block::new(BlockKind::Text, SAMPLE);
        let outcome = validated_apply(&Inflate, &mut block, &est()).unwrap();

        assert!(!outcome.is_compressed());
        assert_eq!(outcome.tokens_saved(), 0);
        assert_eq!(block.content(), SAMPLE);
        assert!(matches!(
            outcome,
            Outcome::Unchanged {
                reason: Declined::NotSmaller
            }
        ));
    }

    #[test]
    fn a_transform_that_saves_nothing_is_discarded() {
        // Equal token counts must fall on the discard side: a compression saving
        // zero tokens is pure risk, costing a CCR entry and a possible retrieval
        // round-trip for no benefit.
        let mut block = Block::new(BlockKind::Text, SAMPLE);
        let outcome = validated_apply(&NoOp, &mut block, &est()).unwrap();

        assert!(!outcome.is_compressed());
        assert_eq!(block.content(), SAMPLE);
    }

    #[test]
    fn a_transform_that_mutates_then_declines_does_not_leak_its_mutation() {
        // A transform may give up partway through. Restoring unconditionally is
        // what stops half-finished work from reaching upstream.
        let mut block = Block::new(BlockKind::Text, SAMPLE);
        let outcome = validated_apply(&MutateThenDecline, &mut block, &est()).unwrap();

        assert!(!outcome.is_compressed());
        assert_eq!(block.content(), SAMPLE, "partial mutation leaked");
        assert!(matches!(
            outcome,
            Outcome::Unchanged {
                reason: Declined::BelowThreshold
            }
        ));
    }

    #[test]
    fn invariant_violations_propagate_instead_of_being_absorbed() {
        // The fallback path must not hide a real defect. By the time an invariant
        // violation is reported the damage it describes has already happened, and
        // silently continuing is how that class of bug survives to production.
        let mut block = Block::new(BlockKind::Text, SAMPLE);
        let err = validated_apply(&Violates, &mut block, &est()).unwrap_err();

        assert!(matches!(err, Error::InvariantViolation { .. }));
        assert!(!err.is_recoverable());
    }

    #[test]
    fn sacrosanct_blocks_are_refused_before_any_tokenizing() {
        for kind in [
            BlockKind::Thinking,
            BlockKind::RedactedThinking,
            BlockKind::Reasoning,
            BlockKind::Compaction,
            BlockKind::Attachment,
        ] {
            let mut block = Block::new(kind, SAMPLE);
            let outcome = validated_apply(&Shrink, &mut block, &est()).unwrap();

            assert!(!outcome.is_compressed(), "{kind}");
            assert_eq!(block.content(), SAMPLE, "{kind}");
            assert!(matches!(
                outcome,
                Outcome::Unchanged {
                    reason: Declined::Sacrosanct
                }
            ));
        }
    }

    #[test]
    fn sibling_fields_survive_validation_in_both_directions() {
        for transform in [&Shrink as &dyn Transform, &Inflate as &dyn Transform] {
            let mut block = Block::tool_result(SAMPLE, "toolu_abc").with_error(true);
            validated_apply(transform, &mut block, &est()).unwrap();

            assert_eq!(block.tool_use_id(), Some("toolu_abc"));
            assert!(block.is_error());
            assert_eq!(block.kind(), BlockKind::ToolResult);
        }
    }

    #[test]
    fn token_count_never_increases_across_a_range_of_inputs() {
        // The I5 property, checked over varied shapes rather than one sample.
        let inputs = [
            "",
            "x",
            "short",
            SAMPLE,
            r#"{"key": "value", "nested": {"a": [1, 2, 3]}}"#,
            "日本語のテキストが含まれる場合",
            "😀😀😀 emoji heavy content 😀😀😀",
            &"repeated ".repeat(200),
        ];

        for input in inputs {
            for transform in [
                &Shrink as &dyn Transform,
                &Inflate as &dyn Transform,
                &NoOp as &dyn Transform,
            ] {
                let mut block = Block::new(BlockKind::Text, input);
                let before = est().count(block.content());
                validated_apply(transform, &mut block, &est()).unwrap();
                let after = est().count(block.content());

                assert!(
                    after <= before,
                    "{:?} on {input:?}: {before} -> {after}",
                    transform.name()
                );
            }
        }
    }

    #[test]
    fn validation_is_deterministic() {
        let run = || {
            let mut block = Block::new(BlockKind::Text, SAMPLE);
            let outcome = validated_apply(&Shrink, &mut block, &est()).unwrap();
            (outcome, block.content().to_owned())
        };
        let first = run();
        for _ in 0..25 {
            assert_eq!(run(), first);
        }
    }
}
