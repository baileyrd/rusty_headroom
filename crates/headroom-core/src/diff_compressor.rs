//! Diff compression by eliding unchanged context.
//!
//! A unified diff is mostly context — lines that did not change, included so a reader
//! can orient. For a model that already has the file available, most of that context
//! is redundant: what it needs is the hunk headers, the changed lines, and enough
//! surrounding context to place them.
//!
//! # What is never dropped
//!
//! **Hunk headers** (`@@ -1,7 +1,9 @@`) carry the line numbers. Losing them makes the
//! diff unusable for anything except reading, since nothing can be located.
//!
//! **Every added and removed line.** These *are* the diff. A compressor that elided
//! changed lines would have produced a smaller file that no longer describes the
//! change.

use std::sync::Arc;

use crate::block::Block;
use crate::ccr::{store_and_mark, CcrStore};
use crate::detection::{detect, AdaptiveSizer, ContentType};
use crate::error::{Declined, Error, Result};
use crate::transform::{LossyTransform, Transform};

const CCR_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// Tuning for diff compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffConfig {
    /// Unchanged lines kept either side of a change.
    ///
    /// Two is enough to place a change without reproducing the file. Zero would make
    /// the diff hard to read against an unfamiliar file.
    pub context_lines: usize,
}

impl Default for DiffConfig {
    fn default() -> Self {
        Self { context_lines: 2 }
    }
}

/// What a diff line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    /// `diff --git`, `---`, `+++`, `index`, and similar.
    FileHeader,
    /// `@@ ... @@`.
    HunkHeader,
    /// An added or removed line.
    Change,
    /// Unchanged context.
    Context,
}

fn classify(line: &str) -> LineKind {
    if line.starts_with("@@") {
        LineKind::HunkHeader
    } else if line.starts_with("diff ")
        || line.starts_with("index ")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("new file")
        || line.starts_with("deleted file")
        || line.starts_with("similarity index")
        || line.starts_with("rename ")
    {
        LineKind::FileHeader
    } else if line.starts_with('+') || line.starts_with('-') {
        LineKind::Change
    } else {
        LineKind::Context
    }
}

/// Compresses unified diffs.
pub struct DiffCompressor {
    config: DiffConfig,
    store: Arc<dyn CcrStore>,
    sizer: AdaptiveSizer,
}

impl DiffCompressor {
    /// Creates a compressor backed by `store`.
    pub fn new(store: Arc<dyn CcrStore>) -> Self {
        Self {
            config: DiffConfig::default(),
            store,
            sizer: AdaptiveSizer::default(),
        }
    }

    /// Overrides the configuration.
    pub fn with_config(mut self, config: DiffConfig) -> Self {
        self.config = config;
        self
    }

    /// Compresses `source`, or explains why it declined.
    pub fn compress(&self, source: &str) -> Result<String> {
        if detect(source.as_bytes()).content_type != ContentType::Diff {
            return Err(Error::declined(Declined::WrongContentType));
        }
        if !self.sizer.should_attempt(ContentType::Diff, source.len()) {
            return Err(Error::declined(Declined::BelowThreshold));
        }

        let lines: Vec<&str> = source.lines().collect();
        let kinds: Vec<LineKind> = lines.iter().map(|l| classify(l)).collect();

        // Mark what survives before emitting anything, so the context window around a
        // change can look forward as well as back.
        let mut keep = vec![false; lines.len()];
        for (index, kind) in kinds.iter().enumerate() {
            match kind {
                LineKind::FileHeader | LineKind::HunkHeader | LineKind::Change => {
                    keep[index] = true;
                    let low = index.saturating_sub(self.config.context_lines);
                    let high = (index + self.config.context_lines).min(lines.len() - 1);
                    for (offset, slot) in keep.iter_mut().enumerate().take(high + 1).skip(low) {
                        if kinds[offset] == LineKind::Context {
                            *slot = true;
                        }
                    }
                }
                LineKind::Context => {}
            }
        }

        let elided = keep.iter().filter(|k| !**k).count();
        if elided == 0 {
            // Every line survived, so the only change would be adding a marker.
            return Err(Error::declined(Declined::NotSmaller));
        }

        let marker = store_and_mark(self.store.as_ref(), source.as_bytes(), CCR_TTL)?;

        let mut out = String::new();
        let mut run = 0usize;
        for (index, line) in lines.iter().enumerate() {
            if keep[index] {
                if run > 0 {
                    out.push_str(&format!("... {run} unchanged lines ...\n"));
                    run = 0;
                }
                out.push_str(line);
                out.push('\n');
            } else {
                run += 1;
            }
        }
        if run > 0 {
            out.push_str(&format!("... {run} unchanged lines ...\n"));
        }
        out.push_str(&format!("full content: {marker}\n"));

        Ok(out)
    }
}

impl Transform for DiffCompressor {
    fn name(&self) -> &'static str {
        "diff_compressor"
    }

    fn apply(&self, block: &mut Block) -> Result<()> {
        let compressed = self.compress(block.content())?;
        block.replace_content(compressed);
        Ok(())
    }
}

impl LossyTransform for DiffCompressor {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ccr::InMemoryCcrStore;
    use crate::tokenizer::{HeuristicEstimator, Tokenizer};

    fn compressor() -> (DiffCompressor, Arc<InMemoryCcrStore>) {
        let store = Arc::new(InMemoryCcrStore::new());
        (DiffCompressor::new(store.clone()), store)
    }

    /// A diff with a lot of untouched context around two small changes.
    fn wide_diff() -> String {
        let mut out = String::from("diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,60 +1,60 @@\n");
        for i in 0..30 {
            out.push_str(&format!(" unchanged context line number {i}\n"));
        }
        out.push_str("-let old = compute();\n+let new = compute_v2();\n");
        for i in 30..60 {
            out.push_str(&format!(" unchanged context line number {i}\n"));
        }
        out
    }

    #[test]
    fn unchanged_context_is_elided_and_counted() {
        let (compressor, _store) = compressor();
        let out = compressor.compress(&wide_diff()).expect("should compress");
        assert!(out.contains("unchanged lines ..."), "{out}");
    }

    #[test]
    fn hunk_headers_and_changes_always_survive() {
        // The headers carry the line numbers; the changes are the diff itself.
        let (compressor, _store) = compressor();
        let out = compressor.compress(&wide_diff()).unwrap();

        assert!(
            out.contains("@@ -1,60 +1,60 @@"),
            "hunk header lost:\n{out}"
        );
        assert!(out.contains("-let old = compute();"), "removal lost");
        assert!(out.contains("+let new = compute_v2();"), "addition lost");
        assert!(out.contains("--- a/src/lib.rs"), "file header lost");
    }

    #[test]
    fn context_immediately_around_a_change_is_kept() {
        let (compressor, _store) = compressor();
        let out = compressor.compress(&wide_diff()).unwrap();
        // Two lines either side, per the default config.
        assert!(
            out.contains("context line number 29"),
            "trailing context lost"
        );
        assert!(
            out.contains("context line number 30"),
            "leading context lost"
        );
    }

    #[test]
    fn it_measurably_shrinks_a_context_heavy_diff() {
        let (compressor, _store) = compressor();
        let source = wide_diff();
        let out = compressor.compress(&source).unwrap();

        let estimator = HeuristicEstimator::new();
        assert!(estimator.count(&out) < estimator.count(&source) / 2);
    }

    #[test]
    fn a_diff_that_is_all_changes_declines() {
        // Nothing to elide, so the only effect would be adding a marker.
        let mut source = String::from("--- a/x\n+++ b/x\n@@ -1,40 +1,40 @@\n");
        for i in 0..40 {
            source.push_str(&format!("-old line {i}\n+new line {i}\n"));
        }
        let (compressor, _store) = compressor();
        assert!(compressor.compress(&source).is_err());
    }

    #[test]
    fn non_diff_content_declines_and_stores_nothing() {
        let (compressor, store) = compressor();
        assert!(compressor.compress(&"just prose. ".repeat(100)).is_err());
        assert!(store.is_empty());
    }

    #[test]
    fn compression_is_deterministic() {
        let source = wide_diff();
        let first = {
            let (c, _s) = compressor();
            c.compress(&source).unwrap()
        };
        for _ in 0..20 {
            let (c, _s) = compressor();
            assert_eq!(c.compress(&source).unwrap(), first);
        }
    }
}
