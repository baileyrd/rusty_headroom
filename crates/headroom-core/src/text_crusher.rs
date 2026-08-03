//! Plain-text compression.
//!
//! The last-resort compressor: text with no structure a type-aware compressor can
//! exploit. It has two jobs, and the split between them matters.
//!
//! # Lossless first
//!
//! Collapsing runs of blank lines and stripping trailing whitespace discards nothing a
//! reader could use. This runs on any text over the threshold and is safe everywhere,
//! including the auth modes that forbid lossy transforms (invariant I10).
//!
//! # Lossy only when it pays
//!
//! Dropping low-importance lines to fit a budget genuinely loses information. It is a
//! separate entry point, gated behind CCR so the original stays retrievable.

use std::sync::Arc;

use crate::block::Block;
use crate::ccr::{store_and_mark, CcrStore};
use crate::detection::AdaptiveSizer;
use crate::detection::{detect, ContentType};
use crate::error::{Declined, Error, Result};
use crate::signals::{keep_most_important, score_lines};
use crate::transform::{LosslessTransform, LossyTransform, Transform};

const CCR_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// Tuning for text compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextConfig {
    /// Consecutive blank lines kept.
    pub max_blank_run: usize,
    /// Lines kept when the lossy pass runs.
    pub line_budget: usize,
}

impl Default for TextConfig {
    fn default() -> Self {
        Self {
            max_blank_run: 1,
            line_budget: 60,
        }
    }
}

/// Normalizes whitespace without discarding content.
///
/// Collapses runs of blank lines and strips trailing whitespace. Leading whitespace is
/// preserved: indentation carries meaning in anything code-adjacent, and this
/// compressor sees plenty of text that is nearly code.
pub fn normalize_whitespace(source: &str, config: &TextConfig) -> String {
    let mut out = String::with_capacity(source.len());
    let mut blank_run = 0usize;

    for line in source.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            blank_run += 1;
            if blank_run > config.max_blank_run {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(trimmed);
        out.push('\n');
    }

    out
}

/// Lossless plain-text compression.
///
/// Safe on every auth mode, since nothing a reader could use is discarded.
pub struct TextCrusher {
    config: TextConfig,
    sizer: AdaptiveSizer,
}

impl Default for TextCrusher {
    fn default() -> Self {
        Self::new()
    }
}

impl TextCrusher {
    /// Creates a crusher with default configuration.
    pub fn new() -> Self {
        Self {
            config: TextConfig::default(),
            sizer: AdaptiveSizer::default(),
        }
    }

    /// Overrides the configuration.
    pub fn with_config(mut self, config: TextConfig) -> Self {
        self.config = config;
        self
    }
}

impl Transform for TextCrusher {
    fn name(&self) -> &'static str {
        "text_crusher"
    }

    fn apply(&self, block: &mut Block) -> Result<()> {
        if !self
            .sizer
            .should_attempt(ContentType::Prose, block.byte_len())
        {
            return Err(Error::declined(Declined::BelowThreshold));
        }

        let normalized = normalize_whitespace(block.content(), &self.config);
        if normalized.len() >= block.byte_len() {
            return Err(Error::declined(Declined::NotSmaller));
        }
        block.replace_content(normalized);
        Ok(())
    }
}

impl LosslessTransform for TextCrusher {}

/// Lossy plain-text compression: drops low-importance lines to fit a budget.
pub struct TextSummarizer {
    config: TextConfig,
    store: Arc<dyn CcrStore>,
    sizer: AdaptiveSizer,
}

impl TextSummarizer {
    /// Creates a summarizer backed by `store`.
    pub fn new(store: Arc<dyn CcrStore>) -> Self {
        Self {
            config: TextConfig::default(),
            store,
            sizer: AdaptiveSizer::default(),
        }
    }

    /// Overrides the configuration.
    pub fn with_config(mut self, config: TextConfig) -> Self {
        self.config = config;
        self
    }
}

impl Transform for TextSummarizer {
    fn name(&self) -> &'static str {
        "text_summarizer"
    }

    fn apply(&self, block: &mut Block) -> Result<()> {
        // Only genuinely unstructured text. Anything a type-aware compressor handles
        // should have gone there instead, and dropping lines from a JSON document
        // would produce something that is neither valid nor summarized.
        if detect(block.content().as_bytes()).content_type != ContentType::Prose {
            return Err(Error::declined(Declined::WrongContentType));
        }
        if !self
            .sizer
            .should_attempt(ContentType::Prose, block.byte_len())
        {
            return Err(Error::declined(Declined::BelowThreshold));
        }

        let scored = score_lines(block.content());
        if scored.len() <= self.config.line_budget {
            return Err(Error::declined(Declined::NotSmaller));
        }

        let kept = keep_most_important(&scored, self.config.line_budget);
        let marker = store_and_mark(self.store.as_ref(), block.content().as_bytes(), CCR_TTL)?;

        let mut out = format!("[{} lines, {} shown]\n", scored.len(), kept.len());
        let mut previous: Option<usize> = None;
        for line in &kept {
            if let Some(prior) = previous {
                if line.index > prior + 1 {
                    out.push_str(&format!("... {} lines ...\n", line.index - prior - 1));
                }
            }
            out.push_str(line.text);
            out.push('\n');
            previous = Some(line.index);
        }
        out.push_str(&format!("full content: {marker}\n"));

        block.replace_content(out);
        Ok(())
    }
}

impl LossyTransform for TextSummarizer {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::BlockKind;
    use crate::ccr::InMemoryCcrStore;
    use crate::tokenizer::HeuristicEstimator;
    use crate::validate::validated_apply;

    fn prose(lines: usize) -> String {
        (0..lines)
            .map(|i| format!("This is an ordinary sentence of prose, number {i} in the sequence."))
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ---- lossless ----

    #[test]
    fn blank_line_runs_collapse() {
        let config = TextConfig::default();
        let out = normalize_whitespace("a\n\n\n\n\nb", &config);
        assert_eq!(out, "a\n\nb\n");
    }

    #[test]
    fn trailing_whitespace_is_stripped() {
        let out = normalize_whitespace("a   \nb\t\t\n", &TextConfig::default());
        assert_eq!(out, "a\nb\n");
    }

    #[test]
    fn leading_indentation_is_preserved() {
        // Indentation carries meaning in anything code-adjacent, and this compressor
        // sees plenty of text that is nearly code.
        let out = normalize_whitespace("    indented\n\tTabbed\n", &TextConfig::default());
        assert!(out.starts_with("    indented"), "{out:?}");
        assert!(out.contains("\tTabbed"), "{out:?}");
    }

    #[test]
    fn the_lossless_pass_declines_when_there_is_nothing_to_strip() {
        let mut block = Block::new(BlockKind::Text, prose(200));
        let before = block.content().to_owned();
        // Already normalized, so nothing to gain.
        let normalized = normalize_whitespace(&before, &TextConfig::default());
        block.replace_content(normalized.clone());
        assert!(TextCrusher::new().apply(&mut block).is_err());
        assert_eq!(block.content(), normalized);
    }

    #[test]
    fn the_lossless_pass_is_a_lossless_transform() {
        // Compile-time assertion that the marker trait is implemented, which is what
        // lets restricted auth modes accept it.
        fn assert_lossless<T: LosslessTransform>(_: &T) {}
        assert_lossless(&TextCrusher::new());
    }

    // ---- lossy ----

    #[test]
    fn the_lossy_pass_keeps_important_lines_and_reports_gaps() {
        let store = Arc::new(InMemoryCcrStore::new());
        let mut source = prose(200);
        source.push_str("\nERROR: the thing that actually went wrong");

        let mut block = Block::new(BlockKind::Text, source);
        TextSummarizer::new(store)
            .apply(&mut block)
            .expect("should summarize");

        assert!(
            block
                .content()
                .contains("ERROR: the thing that actually went wrong"),
            "the important line was dropped:\n{}",
            block.content()
        );
        assert!(block.content().contains("lines ..."), "no gap markers");
    }

    #[test]
    fn the_lossy_pass_refuses_structured_content() {
        // Dropping lines from JSON produces something neither valid nor summarized.
        let store = Arc::new(InMemoryCcrStore::new());
        let json = format!(
            "[{}]",
            (0..200)
                .map(|i| format!(r#"{{"i":{i}}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        let mut block = Block::new(BlockKind::Text, json.clone());

        assert!(TextSummarizer::new(store).apply(&mut block).is_err());
        assert_eq!(block.content(), json);
    }

    #[test]
    fn short_text_declines_and_stores_nothing() {
        let store = Arc::new(InMemoryCcrStore::new());
        let mut block = Block::new(BlockKind::Text, "just a sentence");
        assert!(TextSummarizer::new(store.clone())
            .apply(&mut block)
            .is_err());
        assert!(store.is_empty());
    }

    #[test]
    fn the_lossy_pass_reduces_tokens_under_validation() {
        let store = Arc::new(InMemoryCcrStore::new());
        let mut block = Block::new(BlockKind::Text, prose(400));
        let outcome = validated_apply(
            &TextSummarizer::new(store),
            &mut block,
            &HeuristicEstimator::new(),
        )
        .unwrap();
        assert!(outcome.is_compressed());
        assert!(outcome.tokens_saved() > 0);
    }

    #[test]
    fn both_passes_are_deterministic() {
        let source = prose(300);
        let store = Arc::new(InMemoryCcrStore::new());

        let run = || {
            let mut block = Block::new(BlockKind::Text, source.clone());
            TextSummarizer::new(store.clone())
                .apply(&mut block)
                .unwrap();
            block.content().to_owned()
        };
        let first = run();
        for _ in 0..20 {
            assert_eq!(run(), first);
        }
    }
}
