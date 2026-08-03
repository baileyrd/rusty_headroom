//! A dependency-free token estimator.
//!
//! # Why this deliberately over-counts
//!
//! The estimator is used to decide whether a compression helped. The two ways it
//! can be wrong are not symmetric:
//!
//! - **Over-count the compressed form** → a genuine improvement is discarded and we
//!   forward the original. Cost: one missed compression opportunity.
//! - **Under-count the compressed form** → a "compression" that actually *grew* the
//!   prompt is forwarded upstream. Cost: the user pays for more tokens than they
//!   would have without this project, and invariant I5's guarantee is broken.
//!
//! The second failure is much worse, and it is silent. So every rounding decision
//! here rounds up, and the character classes chosen err toward more tokens rather
//! than fewer.

use super::Tokenizer;

/// Token estimator based on character-class heuristics.
///
/// Real BPE tokenizers split text on learned subword boundaries. Approximating that
/// without the merge tables means leaning on the strong empirical regularities: for
/// English prose a token is roughly four characters, code fragments into far more
/// tokens per character because of punctuation and identifier splitting, and CJK
/// text is close to one token per character since those codepoints rarely merge.
///
/// # Example
///
/// ```
/// use headroom_core::tokenizer::{HeuristicEstimator, Tokenizer};
///
/// let est = HeuristicEstimator::new();
/// assert!(est.count("hello world") > 0);
/// // Never claims zero tokens for non-empty input.
/// assert!(est.count("x") >= 1);
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct HeuristicEstimator {
    _private: (),
}

impl HeuristicEstimator {
    /// Creates an estimator.
    pub fn new() -> Self {
        Self { _private: () }
    }
}

/// Characters per token for text that is mostly ASCII letters and spaces.
///
/// The widely-cited figure for English is ~4. We use 3.5 (expressed as the pair
/// below) so the estimate sits above the true count for typical prose rather than
/// straddling it.
const ASCII_WORD_CHARS_NUMER: usize = 2;
const ASCII_WORD_CHARS_DENOM: usize = 7;

impl Tokenizer for HeuristicEstimator {
    fn count(&self, text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }

        // Each class is accumulated separately because their token densities differ
        // by an order of magnitude; a single global ratio badly misestimates any
        // mixed content, which tool output almost always is.
        let mut word_chars = 0usize;
        let mut punctuation = 0usize;
        let mut wide_chars = 0usize;
        let mut whitespace_runs = 0usize;
        let mut in_whitespace = false;

        for ch in text.chars() {
            if ch.is_whitespace() {
                // A run of whitespace usually merges into one token with whatever
                // follows it, so runs are counted, not individual characters.
                if !in_whitespace {
                    whitespace_runs += 1;
                    in_whitespace = true;
                }
                continue;
            }
            in_whitespace = false;

            if ch.is_ascii_alphanumeric() {
                word_chars += 1;
            } else if ch.is_ascii() {
                // ASCII punctuation and symbols: `{`, `}`, `(`, `)`, `,`, `"`, `:`
                // and friends. In JSON and code these are overwhelmingly their own
                // token, which is exactly why structured data tokenizes so much
                // more densely than prose.
                punctuation += 1;
            } else {
                // Non-ASCII: CJK ideographs, emoji, accented Latin, symbols. These
                // rarely merge, and emoji frequently cost several tokens each
                // because they encode to multiple UTF-8 bytes that the BPE table
                // splits. One token per character is the conservative floor, and
                // for emoji it still under-shoots — hence the surcharge below.
                wide_chars += 1;
            }
        }

        // Divide rounding up: any partial token is a whole token.
        let word_tokens =
            (word_chars * ASCII_WORD_CHARS_NUMER).div_ceil(ASCII_WORD_CHARS_DENOM.max(1));

        // Emoji and other astral-plane codepoints commonly cost 2+ tokens. Charging
        // 2 per non-ASCII character keeps the estimate on the safe side for emoji
        // without wildly inflating ordinary CJK text, which is the common case.
        let wide_tokens = wide_chars * 2;

        let total = word_tokens + punctuation + wide_tokens + whitespace_runs;

        // Non-empty input is never zero tokens.
        total.max(1)
    }

    fn name(&self) -> &'static str {
        "heuristic"
    }

    fn is_exact(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(s: &str) -> usize {
        HeuristicEstimator::new().count(s)
    }

    #[test]
    fn empty_input_is_zero_tokens() {
        assert_eq!(count(""), 0);
    }

    #[test]
    fn non_empty_input_is_never_zero_tokens() {
        // A zero count on real content would let a compressor "prove" any output is
        // an improvement, defeating the I5 check entirely.
        for s in ["x", " ", "\n", ".", "é", "😀"] {
            assert!(count(s) >= 1, "{s:?} estimated as zero tokens");
        }
    }

    #[test]
    fn deterministic_across_repeated_calls() {
        let sample = "the quick brown fox jumps over the lazy dog";
        let first = count(sample);
        for _ in 0..100 {
            assert_eq!(count(sample), first);
        }
    }

    #[test]
    fn prose_lands_near_the_four_characters_per_token_rule() {
        // ~44 characters of ordinary English. A real BPE tokenizer gives roughly
        // 9-11 tokens. We want an estimate at or above that, but not absurdly so —
        // a wildly inflated estimate would suppress compressions that do help.
        let prose = "the quick brown fox jumps over the lazy dog";
        let n = count(prose);
        assert!(
            (10..=25).contains(&n),
            "prose estimate {n} outside the plausible band"
        );
    }

    #[test]
    fn json_costs_more_tokens_per_character_than_prose() {
        // The core reason SmartCrusher pays off: structural punctuation dominates
        // the token count in JSON. If the estimator did not model this, it would
        // undervalue JSON compression and decline improvements that are real.
        let json = r#"{"a":1,"b":2,"c":3,"d":4}"#;
        let prose = "aaaa bbbb cccc dddd eeee";
        assert_eq!(json.len(), 25);
        assert_eq!(prose.len(), 24);
        assert!(
            count(json) > count(prose),
            "json {} should exceed prose {}",
            count(json),
            count(prose)
        );
    }

    #[test]
    fn cjk_is_at_least_one_token_per_character() {
        // CJK codepoints seldom merge under BPE, so anything below 1:1 would
        // under-count — the unsafe direction.
        let cjk = "日本語のテキスト";
        let chars = cjk.chars().count();
        assert!(count(cjk) >= chars);
    }

    #[test]
    fn emoji_are_not_under_counted() {
        // Emoji encode to 4 UTF-8 bytes and typically cost 2+ tokens each.
        let emoji = "😀😀😀😀";
        assert!(count(emoji) >= 8);
    }

    #[test]
    fn whitespace_runs_do_not_inflate_proportionally() {
        // Deeply indented JSON should not be estimated as enormously more expensive
        // than its minified form purely from indentation whitespace, or the
        // estimator would over-credit whitespace-stripping reformats.
        let indented = "{\n        \"a\": 1\n}";
        let minified = "{\"a\":1}";
        let ratio = count(indented) as f64 / count(minified) as f64;
        assert!(ratio < 3.0, "indentation inflated the estimate {ratio}x");
    }

    #[test]
    fn longer_input_never_counts_fewer_tokens() {
        // Monotonicity under append. Without it, the I5 comparison could prefer a
        // longer output over a shorter one.
        let base = "some representative tool output";
        let extended = format!("{base} with more content appended to the end");
        assert!(count(&extended) >= count(base));
    }
}
