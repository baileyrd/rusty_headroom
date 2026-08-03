//! Search-result compression by grouping matches under their file.
//!
//! The single most common bulky tool output a coding agent produces. Every `grep`,
//! `rg`, and codebase search lands here, and the waste is structural:
//!
//! ```text
//! src/very/long/path/to/module.rs:12:    let x = compute();
//! src/very/long/path/to/module.rs:44:    compute(y)
//! src/very/long/path/to/module.rs:91:    // compute
//! ```
//!
//! The path is most of the bytes, and it is the same path three times. Stating it
//! once per file is nearly all of the win.
//!
//! # What is kept and what is not
//!
//! **Line numbers are never dropped.** They are how the agent's next action — reading
//! or editing that file — gets targeted. A result set that loses them forces a
//! re-search, costing more than was saved.
//!
//! **Match text is not truncated.** The matched line is the thing being searched for;
//! the path repetition is the waste. Whole files are elided past a cap rather than
//! mangling the matches that are shown.
//!
//! **Grouping reorders.** Matches are regrouped by file, so an interleaved original
//! ordering is lost. Acceptable for search output — ripgrep groups by file itself —
//! but it is genuine information loss and is stated rather than glossed.

use std::sync::Arc;

use crate::block::Block;
use crate::ccr::{store_and_mark, CcrStore};
use crate::detection::{detect, AdaptiveSizer, ContentType};
use crate::error::{Declined, Error, Result};
use crate::transform::{LossyTransform, Transform};

/// How long an original stays retrievable.
const CCR_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// Tuning for search-result compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchConfig {
    /// Most files shown before the rest are elided.
    pub max_files: usize,
    /// Most matches shown per file.
    pub max_matches_per_file: usize,
    /// Most matches shown in total, across all files.
    ///
    /// The cap that does most of the work on a large result set. Deduplicating paths
    /// alone saves roughly 40% on a hundred-hit search — worthwhile, but the agent
    /// rarely needs all hundred matches to decide where to look next. A global budget
    /// spread across files gives it the map without the transcript, and the exact
    /// count of what was elided is always reported.
    pub max_total_matches: usize,
    /// Fewest matches before compression is attempted.
    ///
    /// Three matches in one file have nothing to gain from grouping.
    pub min_matches: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            max_files: 40,
            max_matches_per_file: 5,
            max_total_matches: 40,
            min_matches: 6,
        }
    }
}

/// One parsed search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// The file the match is in.
    pub path: String,
    /// The line number, as it appeared. Kept as text so `007` stays `007`.
    pub line: String,
    /// Everything after the line (and optional column) prefix.
    pub text: String,
}

/// Parses one `path:line:text` or `path:line:col:text` line.
///
/// Returns `None` for anything not shaped like a search hit — a summary footer, a
/// blank separator, a warning from the search tool. Those are preserved as-is rather
/// than being forced into the grouping, because a mis-parsed line would attach real
/// match text to the wrong file.
///
/// # Example
///
/// ```
/// use headroom_core::search_compressor::parse_match;
///
/// let m = parse_match("src/main.rs:12:    let x = compute();").unwrap();
/// assert_eq!(m.path, "src/main.rs");
/// assert_eq!(m.line, "12");
/// assert_eq!(m.text, "    let x = compute();");
/// ```
pub fn parse_match(line: &str) -> Option<Match> {
    let (path, rest) = line.split_once(':')?;
    if path.is_empty() || !looks_like_path(path) {
        return None;
    }

    let (number, rest) = rest.split_once(':')?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }

    // An optional column follows the same shape. Consuming it keeps `path:12:5:text`
    // from reporting the column as part of the match text.
    let text = match rest.split_once(':') {
        Some((column, after))
            if !column.is_empty() && column.bytes().all(|b| b.is_ascii_digit()) =>
        {
            after
        }
        _ => rest,
    };

    Some(Match {
        path: path.to_owned(),
        line: number.to_owned(),
        text: text.to_owned(),
    })
}

/// Whether a prefix looks like a file path rather than a word before a colon.
fn looks_like_path(candidate: &str) -> bool {
    (candidate.contains('/') || candidate.contains('.') || candidate.contains('\\'))
        && candidate.chars().any(|c| c.is_ascii_alphanumeric())
}

/// Compresses search output by grouping matches under their file.
pub struct SearchCompressor {
    config: SearchConfig,
    store: Arc<dyn CcrStore>,
    sizer: AdaptiveSizer,
}

impl SearchCompressor {
    /// Creates a compressor backed by `store`.
    pub fn new(store: Arc<dyn CcrStore>) -> Self {
        Self {
            config: SearchConfig::default(),
            store,
            sizer: AdaptiveSizer::default(),
        }
    }

    /// Overrides the configuration.
    pub fn with_config(mut self, config: SearchConfig) -> Self {
        self.config = config;
        self
    }

    /// Compresses `source`, or explains why it declined.
    pub fn compress(&self, source: &str) -> Result<String> {
        if detect(source.as_bytes()).content_type != ContentType::SearchResults {
            return Err(Error::declined(Declined::WrongContentType));
        }
        if !self
            .sizer
            .should_attempt(ContentType::SearchResults, source.len())
        {
            return Err(Error::declined(Declined::BelowThreshold));
        }

        // Grouped as a Vec keyed by first appearance rather than a map, so file order
        // follows the search output. A sorted map would alphabetize the results,
        // which discards whatever relevance ordering the search tool applied.
        let mut files: Vec<(String, Vec<Match>)> = Vec::new();
        let mut unparsed: Vec<&str> = Vec::new();
        let mut total_matches = 0usize;

        for line in source.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match parse_match(line) {
                Some(hit) => {
                    total_matches += 1;
                    match files.iter_mut().find(|(path, _)| *path == hit.path) {
                        Some((_, hits)) => hits.push(hit),
                        None => files.push((hit.path.clone(), vec![hit])),
                    }
                }
                // Kept rather than dropped: this is usually a tool summary or a
                // permission warning, and either is worth the handful of tokens.
                None => unparsed.push(line),
            }
        }

        if total_matches < self.config.min_matches {
            return Err(Error::declined(Declined::BelowThreshold));
        }

        // One match per file means every path is stated once already, so grouping
        // saves nothing and the header is pure overhead.
        if files.len() == total_matches {
            return Err(Error::declined(Declined::NotSmaller));
        }

        let marker = store_and_mark(self.store.as_ref(), source.as_bytes(), CCR_TTL)?;

        let shown_files = self.config.max_files.min(files.len());
        let mut out = format!("[{total_matches} matches in {} files]\n", files.len());

        let mut shown_matches = 0usize;
        for (path, hits) in files.iter().take(shown_files) {
            out.push_str(&format!("{path}\n"));

            // Both caps apply: per-file, and whatever remains of the global budget.
            // The global budget is spread by exhaustion rather than divided evenly,
            // so early files — which search tools generally order by relevance — get
            // their matches before later ones.
            let remaining_budget = self.config.max_total_matches.saturating_sub(shown_matches);
            let shown = self
                .config
                .max_matches_per_file
                .min(hits.len())
                .min(remaining_budget);

            for hit in hits.iter().take(shown) {
                out.push_str(&format!("  {}:{}\n", hit.line, hit.text));
                shown_matches += 1;
            }
            if hits.len() > shown {
                out.push_str(&format!("  ... {} more in this file\n", hits.len() - shown));
            }
        }

        if files.len() > shown_files {
            out.push_str(&format!(
                "... {} more files not shown\n",
                files.len() - shown_files
            ));
        }

        if !unparsed.is_empty() {
            for line in &unparsed {
                out.push_str(&format!("{line}\n"));
            }
        }

        let elided = total_matches.saturating_sub(shown_matches);
        if elided > 0 {
            out.push_str(&format!("{elided} matches elided\n"));
        }
        out.push_str(&format!("full content: {marker}\n"));

        Ok(out)
    }
}

impl Transform for SearchCompressor {
    fn name(&self) -> &'static str {
        "search_compressor"
    }

    fn apply(&self, block: &mut Block) -> Result<()> {
        let compressed = self.compress(block.content())?;
        block.replace_content(compressed);
        Ok(())
    }
}

impl LossyTransform for SearchCompressor {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ccr::{parse_marker, InMemoryCcrStore};
    use crate::tokenizer::{HeuristicEstimator, Tokenizer};

    fn compressor() -> (SearchCompressor, Arc<InMemoryCcrStore>) {
        let store = Arc::new(InMemoryCcrStore::new());
        (SearchCompressor::new(store.clone()), store)
    }

    /// `files` × `per_file` hits with realistically long paths.
    fn grep_output(files: usize, per_file: usize) -> String {
        let mut lines = Vec::new();
        for f in 0..files {
            for m in 0..per_file {
                lines.push(format!(
                    "crates/headroom-core/src/deeply/nested/module_{f}.rs:{}:    let value = compute_something({m});",
                    10 + m * 7
                ));
            }
        }
        lines.join("\n")
    }

    // ---- parsing ----

    #[test]
    fn the_standard_grep_form_parses() {
        let m = parse_match("src/main.rs:12:    let x = compute();").unwrap();
        assert_eq!(m.path, "src/main.rs");
        assert_eq!(m.line, "12");
        assert_eq!(m.text, "    let x = compute();");
    }

    #[test]
    fn the_column_form_parses_without_leaking_the_column() {
        let m = parse_match("src/main.rs:12:5:    let x = compute();").unwrap();
        assert_eq!(m.line, "12");
        assert_eq!(m.text, "    let x = compute();");
    }

    #[test]
    fn match_text_containing_colons_survives_intact() {
        // Source code is full of colons. Splitting greedily would truncate the match.
        let m = parse_match("src/a.rs:7:    let map: BTreeMap<String, u32> = x;").unwrap();
        assert_eq!(m.text, "    let map: BTreeMap<String, u32> = x;");
    }

    #[test]
    fn non_match_lines_do_not_parse() {
        // A mis-parse would attach real match text to the wrong file.
        assert!(parse_match("").is_none());
        assert!(parse_match("Note: something happened").is_none());
        assert!(parse_match("src/main.rs:notanumber:text").is_none());
        assert!(parse_match("just some prose").is_none());
    }

    // ---- compression ----

    #[test]
    fn a_hundred_result_search_gets_dramatically_smaller() {
        // The reference's headline benchmark: 100 results, 92% reduction.
        let (compressor, _store) = compressor();
        let source = grep_output(20, 5);
        let compressed = compressor.compress(&source).expect("should compress");

        let estimator = HeuristicEstimator::new();
        let before = estimator.count(&source);
        let after = estimator.count(&compressed);

        assert!(
            after < before / 2,
            "expected a large reduction, {before} -> {after}"
        );
    }

    #[test]
    fn every_path_is_stated_once_per_file() {
        let (compressor, _store) = compressor();
        let compressed = compressor.compress(&grep_output(5, 8)).unwrap();

        let occurrences = compressed
            .matches("crates/headroom-core/src/deeply/nested/module_0.rs")
            .count();
        assert_eq!(occurrences, 1, "path repeated:\n{compressed}");
    }

    #[test]
    fn line_numbers_are_preserved_for_every_shown_match() {
        // Losing these forces a re-search, costing more than was saved.
        let (compressor, _store) = compressor();
        let compressed = compressor.compress(&grep_output(3, 4)).unwrap();

        for m in 0..4 {
            let expected = format!("  {}:", 10 + m * 7);
            assert!(compressed.contains(&expected), "missing {expected}");
        }
    }

    #[test]
    fn match_text_is_not_truncated() {
        let (compressor, _store) = compressor();
        let compressed = compressor.compress(&grep_output(3, 4)).unwrap();
        assert!(compressed.contains("let value = compute_something(0);"));
    }

    #[test]
    fn files_beyond_the_cap_are_reported_not_silently_dropped() {
        let (compressor, _store) = compressor();
        let compressor = compressor.with_config(SearchConfig {
            max_files: 3,
            ..SearchConfig::default()
        });
        let compressed = compressor.compress(&grep_output(10, 3)).unwrap();

        assert!(
            compressed.contains("7 more files not shown"),
            "{compressed}"
        );
        assert!(compressed.contains("matches elided"), "{compressed}");
    }

    #[test]
    fn matches_beyond_the_per_file_cap_are_reported() {
        let (compressor, _store) = compressor();
        let compressor = compressor.with_config(SearchConfig {
            max_matches_per_file: 2,
            ..SearchConfig::default()
        });
        let compressed = compressor.compress(&grep_output(3, 6)).unwrap();
        assert!(compressed.contains("4 more in this file"), "{compressed}");
    }

    #[test]
    fn the_match_and_file_totals_are_reported() {
        let (compressor, _store) = compressor();
        let compressed = compressor.compress(&grep_output(6, 5)).unwrap();
        assert!(
            compressed.starts_with("[30 matches in 6 files]"),
            "{compressed}"
        );
    }

    #[test]
    fn a_file_with_a_single_match_is_handled() {
        let (compressor, _store) = compressor();
        let mut source = grep_output(4, 4);
        source.push_str("\nsrc/solitary.rs:1:only match here");

        let compressed = compressor.compress(&source).unwrap();
        assert!(compressed.contains("src/solitary.rs"), "{compressed}");
        assert!(compressed.contains("  1:only match here"), "{compressed}");
    }

    #[test]
    fn unparsed_lines_are_kept_rather_than_dropped() {
        // Usually a tool summary or a permission warning — worth the few tokens.
        let (compressor, _store) = compressor();
        let mut source = grep_output(5, 4);
        source.push_str("\nrg: ./restricted: Permission denied (os error 13)");

        let compressed = compressor.compress(&source).unwrap();
        assert!(
            compressed.contains("Permission denied (os error 13)"),
            "{compressed}"
        );
    }

    #[test]
    fn file_order_follows_the_search_output() {
        // Alphabetizing would discard whatever relevance ordering the tool applied.
        let (compressor, _store) = compressor();
        // Padded past the 500-byte floor; the point is the ordering, not the size.
        let mut lines = Vec::new();
        for name in ["zebra", "alpha", "middle"] {
            for i in 0..4 {
                lines.push(format!(
                    "{name}/some/reasonably/long/path/file.rs:{i}:    a matching line of source here"
                ));
            }
        }
        let source = lines.join("\n");
        let compressed = compressor.compress(&source).unwrap();

        let zebra = compressed.find("zebra/").unwrap();
        let alpha = compressed.find("alpha/").unwrap();
        assert!(zebra < alpha, "file order should follow the input");
    }

    #[test]
    fn the_original_is_retrievable() {
        let (compressor, store) = compressor();
        let source = grep_output(10, 5);
        let compressed = compressor.compress(&source).unwrap();

        let marker = compressed
            .lines()
            .find(|l| l.starts_with("full content: "))
            .unwrap()
            .trim_start_matches("full content: ");
        let hash = parse_marker(marker).unwrap();

        assert_eq!(
            String::from_utf8(store.get(hash).unwrap().unwrap()).unwrap(),
            source
        );
    }

    // ---- declining ----

    #[test]
    fn non_search_content_is_declined() {
        let (compressor, store) = compressor();
        assert!(compressor
            .compress(&"ordinary prose. ".repeat(200))
            .is_err());
        assert!(store.is_empty());
    }

    #[test]
    fn one_match_per_file_saves_nothing_and_declines() {
        // Every path already appears once; grouping adds a header for no gain.
        let (compressor, _store) = compressor();
        let source: String = (0..30)
            .map(|i| format!("src/module_{i}.rs:{i}:a match on this line here\n"))
            .collect();
        let err = compressor.compress(&source).unwrap_err();
        assert!(matches!(err, Error::Declined(_)));
    }

    #[test]
    fn a_tiny_result_set_is_declined() {
        let (compressor, _store) = compressor();
        assert!(compressor.compress("src/a.rs:1:x\nsrc/a.rs:2:y").is_err());
    }

    #[test]
    fn compression_is_deterministic() {
        let source = grep_output(12, 6);
        let first = {
            let (c, _s) = compressor();
            c.compress(&source).unwrap()
        };
        for _ in 0..25 {
            let (c, _s) = compressor();
            assert_eq!(c.compress(&source).unwrap(), first);
        }
    }
}
