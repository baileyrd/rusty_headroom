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
//!
//! # What this does *not* guarantee
//!
//! This module used to say it never under-counts. That was written down and never
//! checked against the tokenizer it approximates. Measured against `gpt-4o`, it is false,
//! and the correction is worth stating precisely rather than softening.
//!
//! **On realistic content it now over-counts**, and that is pinned by
//! `tests/estimator_never_under_counts.rs` over prose, JSON, code, logs, diffs, base64,
//! hex digests, indentation, and eight scripts. Four of those classes under-counted before
//! this was measured — logs at 0.92, hex at 0.83, base64 at 0.52, and whitespace runs at
//! 0.00, where 1500 characters were charged as a single token.
//!
//! **On random alphanumeric strings it under-counts, and no character-class heuristic can
//! fix that.** 25.8% of 12,000 generated inputs came out low, worst case `"EYM3Dgnc6"` at
//! 3 estimated against 7 actual. The reason is not a tuning miss: `"Dgnc"` and `"Word"`
//! are the same string to a classifier that cannot consult the merge tables, and they
//! cost 4 tokens and 1. Charging every short run at the dense rate would put ordinary
//! prose at roughly eight times its true count and suppress compression everywhere.
//!
//! That exposure is bounded rather than hidden — `the_under_count_on_random_strings_is_no
//! _worse_than_measured` fails if it grows — and D29 records why the trade was left here.
//! A caller that needs a true bound wants an exact tokenizer: [`super::registry::Registry`]
//! resolves one for every OpenAI family, and `is_exact_for` reports which it got.

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

/// The longest unbroken alphanumeric run still treated as a word.
///
/// Beyond this it is a hash, a base64 blob, a UUID or an identifier with no separators,
/// and those tokenize far denser than prose — measured at 1.7 characters per token for
/// base64 against `gpt-4o`, where the prose ratio assumes 3.5.
const LONGEST_WORDLIKE_RUN: usize = 12;

/// Tokens per character charged to a long alphanumeric run, as a fraction.
///
/// 2/3 sits above the 0.585 tokens per character measured for base64, which is the
/// densest realistic case. Over the true figure is the safe direction.
const DENSE_RUN_NUMER: usize = 2;
const DENSE_RUN_DENOM: usize = 3;

/// Tokens per digit, as a fraction.
///
/// Digits group into runs of at most three under `cl100k`/`o200k`, so a long number costs
/// roughly one token per one to three digits — far denser than the 3.5 characters that
/// prose assumes. Timestamps make log lines mostly digits, which is why logs were the
/// realistic content type this estimator under-counted.
const DIGIT_NUMER: usize = 1;
const DIGIT_DENOM: usize = 2;

/// Characters of a *uniform* whitespace run charged as one token.
///
/// Runs of one repeated character merge extremely well — measured against `gpt-4o`, 64
/// spaces is a single token and 1500 spaces is 13. The worst uniform case is newlines at
/// 15.6 characters per token, so 8 leaves margin without inflating indentation.
const UNIFORM_WHITESPACE_CHARS_PER_TOKEN: usize = 8;

/// Characters of *variety* within a whitespace run charged as one token.
///
/// Mixed whitespace does not merge: `" \n\t"` repeated measured 2.99 characters per
/// token, against 115 for the same length of plain spaces. Length alone cannot tell the
/// two apart, so the changes between adjacent characters are counted instead.
///
/// Charging by length alone was tried first and over-corrected badly — 24 spaces of
/// indentation per line came out at 12 tokens against an actual 1, and
/// `the_estimate_stays_close_enough_to_be_useful` caught it at 3.3x on exactly the
/// content this proxy exists to compress.
const WHITESPACE_CHANGES_PER_TOKEN: usize = 2;

/// Tokens charged to one whitespace run of `length` characters with `changes` places
/// where an adjacent pair differs.
///
/// One token for the run itself — a single space merges with the word after it and costs
/// nothing extra. Then length, sparsely, because even uniform runs eventually split. Then
/// variety, densely, because a run that alternates does not merge at all.
fn whitespace_run_tokens(length: usize, changes: usize) -> usize {
    1 + length / UNIFORM_WHITESPACE_CHARS_PER_TOKEN + changes.div_ceil(WHITESPACE_CHANGES_PER_TOKEN)
}

/// Ends an unbroken alphanumeric run, moving it to the dense pool if it is too long to
/// be a word.
///
/// Charged in full rather than only the excess: a 40-character base64 blob is dense from
/// its first character, not from its thirteenth.
fn close_run(run: &mut usize, word_chars: &mut usize, dense_chars: &mut usize) {
    if *run > LONGEST_WORDLIKE_RUN {
        *dense_chars += *run;
        *word_chars -= *run;
    }
    *run = 0;
}

impl Tokenizer for HeuristicEstimator {
    fn count(&self, text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }

        // Each class is accumulated separately because their token densities differ
        // by an order of magnitude; a single global ratio badly misestimates any
        // mixed content, which tool output almost always is.
        let mut word_chars = 0usize;
        let mut digits = 0usize;
        let mut dense_chars = 0usize;
        let mut punctuation = 0usize;
        let mut wide_chars = 0usize;
        let mut whitespace_tokens = 0usize;
        let mut whitespace_run = 0usize;
        // Adjacent characters in the run that differ, which is what separates
        // well-merging indentation from whitespace that tokenizes one character at a
        // time.
        let mut whitespace_changes = 0usize;
        let mut previous_whitespace = '\0';
        // The current unbroken alphanumeric run, so it can be re-charged as dense once
        // it grows past a word's length.
        let mut alnum_run = 0usize;

        for ch in text.chars() {
            if ch.is_whitespace() {
                close_run(&mut alnum_run, &mut word_chars, &mut dense_chars);
                if whitespace_run > 0 && ch != previous_whitespace {
                    whitespace_changes += 1;
                }
                previous_whitespace = ch;
                whitespace_run += 1;
                continue;
            }
            if whitespace_run > 0 {
                whitespace_tokens += whitespace_run_tokens(whitespace_run, whitespace_changes);
                whitespace_run = 0;
                whitespace_changes = 0;
                previous_whitespace = '\0';
            }

            if ch.is_ascii_digit() {
                // Counted apart from letters: digits group in threes at most, so they
                // cost several times what the prose ratio assumes. A run of digits does
                // not make a word, so it also breaks the alphanumeric run.
                close_run(&mut alnum_run, &mut word_chars, &mut dense_chars);
                digits += 1;
            } else if ch.is_ascii_alphanumeric() {
                word_chars += 1;
                alnum_run += 1;
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

        // The loop above only closes a run when something else follows it, so text
        // ending in one of these would drop it.
        close_run(&mut alnum_run, &mut word_chars, &mut dense_chars);
        if whitespace_run > 0 {
            whitespace_tokens += whitespace_run_tokens(whitespace_run, whitespace_changes);
        }

        // Divide rounding up: any partial token is a whole token.
        let word_tokens =
            (word_chars * ASCII_WORD_CHARS_NUMER).div_ceil(ASCII_WORD_CHARS_DENOM.max(1));
        let digit_tokens = (digits * DIGIT_NUMER).div_ceil(DIGIT_DENOM.max(1));
        let dense_tokens = (dense_chars * DENSE_RUN_NUMER).div_ceil(DENSE_RUN_DENOM.max(1));

        // Emoji and other astral-plane codepoints commonly cost 2+ tokens. Charging
        // 2 per non-ASCII character keeps the estimate on the safe side for emoji
        // without wildly inflating ordinary CJK text, which is the common case.
        let wide_tokens = wide_chars * 2;

        let total = word_tokens
            + digit_tokens
            + dense_tokens
            + punctuation
            + wide_tokens
            + whitespace_tokens;

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
