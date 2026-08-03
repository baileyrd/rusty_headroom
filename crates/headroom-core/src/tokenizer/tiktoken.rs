//! Exact BPE token counting for OpenAI models — gap row T2.
//!
//! # What "exact" buys, and what it costs
//!
//! Every other tokenizer here over-counts deliberately, because invariant I5 validates
//! compression against a token count and under-counting is the silent failure. An exact
//! counter removes the slack: compression that the heuristic's over-count would have
//! rejected as "not smaller enough" now gets accepted, because the measurement is the
//! real one.
//!
//! The cost is the BPE tables, which are hundreds of kilobytes of embedded data. That is
//! why this is a per-model-family registration rather than the default: a build that
//! never sees OpenAI traffic should not carry OpenAI's vocabulary.
//!
//! # This one really does not under-count
//!
//! `is_exact()` returns `true`, and the encoder is the same one OpenAI uses — so the
//! count is the count. The one place it could go wrong is a *newer* model whose encoding
//! this build does not know; that case falls back to `cl100k_base` rather than guessing,
//! and `cl100k_base` over-counts relative to `o200k_base` on the content that differs.
//! Wrong in the safe direction, by construction rather than by luck.

use std::sync::OnceLock;

use tiktoken_rs::CoreBPE;

use super::Tokenizer;

/// Which BPE encoding a model uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// GPT-4o, o-series, and later.
    O200kBase,
    /// GPT-4, GPT-3.5.
    Cl100kBase,
}

impl Encoding {
    /// A stable identifier.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::O200kBase => "o200k_base",
            Self::Cl100kBase => "cl100k_base",
        }
    }

    /// The encoding for `model`.
    ///
    /// # Unknown models fall back rather than guess
    ///
    /// A model this build has never seen gets `cl100k_base`. It is the older, coarser
    /// encoding, so it *over*-counts relative to `o200k_base` on the content where they
    /// differ — which is the safe direction for invariant I5, and the reason the fallback
    /// is this one rather than the newer one.
    pub fn for_model(model: &str) -> Self {
        let model = model.to_ascii_lowercase();

        // `o200k` is the newer encoding, so it is matched first: `gpt-4o` contains
        // `gpt-4`, and testing the older family first would hand every GPT-4o request a
        // vocabulary it does not use.
        if model.contains("gpt-4o")
            || model.contains("gpt-5")
            || model.contains("o1")
            || model.contains("o3")
            || model.contains("o4")
        {
            Self::O200kBase
        } else {
            Self::Cl100kBase
        }
    }
}

/// An exact BPE tokenizer.
///
/// # Construction is lazy and shared
///
/// Building a `CoreBPE` parses the embedded vocabulary — hundreds of thousands of
/// entries. Doing that per request would dominate the cost of compression itself, and
/// doing it at startup would pay for an encoding a process may never use. A `OnceLock`
/// per encoding gives one parse on first use, shared thereafter.
pub struct TiktokenCounter {
    encoding: Encoding,
}

impl std::fmt::Debug for TiktokenCounter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TiktokenCounter")
            .field("encoding", &self.encoding.as_str())
            .finish()
    }
}

/// The parsed `o200k_base` vocabulary.
static O200K: OnceLock<Option<CoreBPE>> = OnceLock::new();
/// The parsed `cl100k_base` vocabulary.
static CL100K: OnceLock<Option<CoreBPE>> = OnceLock::new();

impl TiktokenCounter {
    /// Builds a counter for `encoding`.
    pub fn new(encoding: Encoding) -> Self {
        Self { encoding }
    }

    /// Builds the counter `model` should use.
    pub fn for_model(model: &str) -> Self {
        Self::new(Encoding::for_model(model))
    }

    /// Which encoding this counter uses.
    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// The shared encoder, or `None` if the vocabulary could not be parsed.
    fn encoder(&self) -> Option<&'static CoreBPE> {
        let slot = match self.encoding {
            Encoding::O200kBase => &O200K,
            Encoding::Cl100kBase => &CL100K,
        };

        slot.get_or_init(|| match self.encoding {
            Encoding::O200kBase => tiktoken_rs::o200k_base().ok(),
            Encoding::Cl100kBase => tiktoken_rs::cl100k_base().ok(),
        })
        .as_ref()
    }
}

impl Tokenizer for TiktokenCounter {
    /// Counts tokens exactly.
    ///
    /// # Special tokens are counted as ordinary text
    ///
    /// `encode_ordinary` rather than `encode_with_special_tokens`. Customer content
    /// containing the literal text `<|endoftext|>` is *text* — a compressor measuring it
    /// as one special token would under-count a payload by hundreds, and under-counting
    /// is the failure invariant I5 cannot catch.
    fn count(&self, text: &str) -> usize {
        match self.encoder() {
            Some(encoder) => encoder.encode_ordinary(text).len(),
            // The vocabulary failed to parse, which should not happen with embedded
            // data. Falling back to the heuristic rather than to zero: a count of zero
            // would make every compression look infinitely effective and defeat I5
            // entirely.
            None => super::HeuristicEstimator::new().count(text),
        }
    }

    fn name(&self) -> &'static str {
        self.encoding.as_str()
    }

    fn is_exact(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o200k() -> TiktokenCounter {
        TiktokenCounter::new(Encoding::O200kBase)
    }

    // ---- encoding selection ----

    #[test]
    fn newer_models_get_the_newer_encoding() {
        for model in ["gpt-4o", "gpt-4o-mini", "gpt-5", "o1-preview", "o3-mini"] {
            assert_eq!(Encoding::for_model(model), Encoding::O200kBase, "{model}");
        }
    }

    #[test]
    fn gpt_4o_is_not_swallowed_by_the_gpt_4_family() {
        // `gpt-4o` contains `gpt-4`. Testing the older family first would hand every
        // GPT-4o request a vocabulary it does not use, and the counts would be quietly
        // wrong rather than obviously so.
        assert_eq!(Encoding::for_model("gpt-4o"), Encoding::O200kBase);
        assert_eq!(Encoding::for_model("gpt-4-turbo"), Encoding::Cl100kBase);
    }

    #[test]
    fn an_unknown_model_falls_back_to_the_coarser_encoding() {
        // `cl100k_base` over-counts relative to `o200k_base` on the content where they
        // differ, which is the safe direction for invariant I5.
        for model in ["something-new-2028", "", "internal-v3"] {
            assert_eq!(
                Encoding::for_model(model),
                Encoding::Cl100kBase,
                "{model:?}"
            );
        }
    }

    // ---- counting ----

    #[test]
    fn a_known_string_counts_exactly() {
        // The value an exact tokenizer provides: a real number, not an upper bound.
        // "hello world" is two tokens in both encodings.
        assert_eq!(o200k().count("hello world"), 2);
        assert_eq!(
            TiktokenCounter::new(Encoding::Cl100kBase).count("hello world"),
            2
        );
    }

    #[test]
    fn the_count_is_reported_as_exact() {
        assert!(o200k().is_exact());
        assert!(!super::super::HeuristicEstimator::new().is_exact());
    }

    #[test]
    fn an_exact_count_is_at_or_below_the_heuristic() {
        // The heuristic over-counts by design. An exact count that came out *higher*
        // would mean the heuristic under-counts — the failure invariant I5 cannot catch,
        // since it validates against exactly this number.
        let heuristic = super::super::HeuristicEstimator::new();
        let exact = o200k();

        for text in [
            "hello world",
            "The quick brown fox jumps over the lazy dog.",
            r#"{"path":"src/main.rs","size":1024,"kind":"file"}"#,
            &"a".repeat(500),
            "2026-01-01T00:00:00Z INFO worker started",
        ] {
            assert!(
                exact.count(text) <= heuristic.count(text),
                "{:?}: exact {} > heuristic {}",
                &text[..text.len().min(40)],
                exact.count(text),
                heuristic.count(text)
            );
        }
    }

    #[test]
    fn a_special_token_in_customer_content_is_counted_as_text() {
        // `encode_ordinary` rather than `encode_with_special_tokens`. Content containing
        // the literal `<|endoftext|>` is text — measuring it as one special token would
        // under-count a payload by hundreds, and under-counting is the failure I5 cannot
        // catch.
        let counter = o200k();
        let literal = counter.count("<|endoftext|>");

        assert!(
            literal > 1,
            "a literal special token counted as {literal} token(s)"
        );
    }

    #[test]
    fn multibyte_content_counts_without_panicking() {
        let counter = o200k();
        for text in ["日本語のテキスト", "😀😀😀", "café naïve"] {
            assert!(counter.count(text) > 0, "{text}");
        }
    }

    #[test]
    fn empty_content_counts_zero() {
        assert_eq!(o200k().count(""), 0);
    }

    #[test]
    fn counting_is_deterministic() {
        // Invariant I4. The token count feeds I5's accept/reject decision, so a count
        // that varied would make compression vary with it.
        let counter = o200k();
        let text = r#"[{"path":"src/main.rs","size":1024}]"#;
        let first = counter.count(text);

        for _ in 0..50 {
            assert_eq!(counter.count(text), first);
        }
    }

    #[test]
    fn the_vocabulary_is_parsed_once_and_shared() {
        // Building a `CoreBPE` parses hundreds of thousands of entries. Doing it per
        // request would dominate the cost of compression itself. Two counters for one
        // encoding must reach the same instance.
        let first = TiktokenCounter::new(Encoding::O200kBase);
        let second = TiktokenCounter::new(Encoding::O200kBase);

        let a = first.encoder().expect("vocabulary failed to parse") as *const CoreBPE;
        let b = second.encoder().expect("vocabulary failed to parse") as *const CoreBPE;
        assert!(std::ptr::eq(a, b), "the vocabulary was parsed twice");
    }

    #[test]
    fn the_two_encodings_are_genuinely_different() {
        // Otherwise the selection logic is elaborate machinery producing one answer.
        let o200k = TiktokenCounter::new(Encoding::O200kBase);
        let cl100k = TiktokenCounter::new(Encoding::Cl100kBase);

        assert_ne!(o200k.name(), cl100k.name());

        // A string the two genuinely tokenize differently, chosen by measuring rather
        // than by assuming: `o200k_base` has far better non-Latin coverage. Digits
        // looked like the obvious example and turned out to count identically in both.
        let cjk = "日本語のテキストです";
        assert_eq!(o200k.count(cjk), 7);
        assert_eq!(cl100k.count(cjk), 9);
    }

    // ---- registry integration ----

    #[test]
    fn a_registered_counter_wins_for_its_family() {
        use super::super::registry::{Family, Registry};
        use std::sync::Arc;

        let mut registry = Registry::new();
        registry.register(
            Family::OpenAi,
            Arc::new(TiktokenCounter::new(Encoding::O200kBase)),
        );

        assert!(registry.is_exact_for("gpt-4o"));
        assert_eq!(registry.for_model("gpt-4o").name(), "o200k_base");

        // And a family with nothing registered still falls back, rather than borrowing
        // OpenAI's vocabulary for an Anthropic model.
        assert!(!registry.is_exact_for("claude-opus-4"));
    }
}
