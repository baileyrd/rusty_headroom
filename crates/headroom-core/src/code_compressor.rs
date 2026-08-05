//! Code compression by eliding function bodies.
//!
//! A file an agent reads to understand structure is mostly bodies. What it usually
//! needs is the shape: what is defined, with what signature, in what order. Keeping
//! signatures and eliding bodies is most of the win, and unlike the other lossy
//! compressors it degrades gracefully — a reader left with signatures alone still
//! knows what exists.
//!
//! # Heuristic, not a parser
//!
//! This uses brace depth and indentation rather than a real grammar. Seven languages
//! would mean seven parser dependencies for a job that amounts to *find the end of
//! this block*, and the failure mode of getting it wrong is bounded: invariant I5
//! discards any result that does not reduce the token count, so a misparse costs a
//! wasted pass rather than corrupt output.
//!
//! Recorded as decision D3 rather than presented as AST-aware.

use std::sync::Arc;

use crate::block::Block;
use crate::ccr::{store_and_mark, CcrStore};
use crate::detection::{detect, AdaptiveSizer, ContentType};
use crate::error::{Declined, Error, Result};
use crate::transform::{LossyTransform, Transform};

const CCR_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// Languages the compressor recognizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    /// Braces, `fn`.
    Rust,
    /// Indentation, `def` / `class`.
    Python,
    /// Braces, `function` / `=>`.
    JavaScript,
    /// Braces, `func`.
    Go,
    /// Braces, visibility modifiers.
    Java,
    /// Braces, `#include`.
    Cpp,
    /// Braces, `sub`.
    Perl,
}

impl Language {
    /// Whether blocks are delimited by indentation rather than braces.
    fn indentation_scoped(self) -> bool {
        matches!(self, Self::Python)
    }

    /// Keywords that open a definition worth keeping the signature of.
    fn definition_markers(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["fn ", "impl ", "struct ", "enum ", "trait ", "mod ", "pub "],
            Self::Python => &["def ", "class ", "async def "],
            Self::JavaScript => &["function ", "class ", "const ", "let ", "export ", "async "],
            Self::Go => &["func ", "type ", "package "],
            Self::Java => &["public ", "private ", "protected ", "class ", "interface "],
            Self::Cpp => &["#include", "class ", "struct ", "namespace ", "template"],
            Self::Perl => &["sub ", "package ", "use "],
        }
    }

    /// A stable identifier for telemetry.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::Go => "go",
            Self::Java => "java",
            Self::Cpp => "cpp",
            Self::Perl => "perl",
        }
    }
}

/// Guesses the language of `source`.
///
/// Returns `None` when nothing matches strongly enough. Guessing wrong would apply the
/// wrong block-delimiter rule, which for indentation-versus-brace languages produces
/// nonsense — so an uncertain answer declines rather than picking the most likely.
///
/// # Example
///
/// ```
/// use headroom_core::code_compressor::{detect_language, Language};
///
/// // Two distinct markers are required, so one `fn ` alone is not enough.
/// let rust = "fn main() -> u32 {\n    let mut x = 1;\n    x\n}\n";
/// assert_eq!(detect_language(rust), Some(Language::Rust));
/// assert_eq!(detect_language("just some prose"), None);
/// ```
pub fn detect_language(source: &str) -> Option<Language> {
    // Ordered most-distinctive first. `fn ` and `impl ` together are unambiguous;
    // `class ` alone is shared by four languages and settles nothing.
    let candidates: [(Language, &[&str]); 7] = [
        (
            Language::Rust,
            &["fn ", "impl ", "let mut ", "pub fn", "->"],
        ),
        (
            Language::Python,
            &["def ", "elif ", "self.", "import ", "__init__"],
        ),
        (Language::Go, &["func ", "package ", ":=", "nil"]),
        (Language::Perl, &["sub ", "my $", "use strict", "@_"]),
        (Language::Cpp, &["#include", "std::", "->", "template<"]),
        (
            Language::JavaScript,
            &["function ", "const ", "=>", "require(", "export "],
        ),
        (
            Language::Java,
            &["public class", "private ", "void ", "System.out"],
        ),
    ];

    let mut best: Option<(Language, usize)> = None;
    for (language, markers) in candidates {
        let hits = markers.iter().filter(|m| source.contains(**m)).count();
        // `map_or` rather than the clearer `is_none_or`, which is stable only from
        // 1.82 against this crate's 1.80 MSRV.
        if hits >= 2 && best.map_or(true, |(_, score)| hits > score) {
            best = Some((language, hits));
        }
    }

    best.map(|(language, _)| language)
}

/// Tuning for code compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeConfig {
    /// Longest body kept verbatim, in lines.
    ///
    /// Short bodies are elided at a loss: the placeholder costs nearly as much as the
    /// lines it replaces, and the reader loses the detail for nothing.
    pub min_body_lines: usize,
}

impl Default for CodeConfig {
    fn default() -> Self {
        Self { min_body_lines: 4 }
    }
}

/// Compresses source code by eliding function bodies.
pub struct CodeCompressor {
    config: CodeConfig,
    store: Arc<dyn CcrStore>,
    sizer: AdaptiveSizer,
}

impl CodeCompressor {
    /// Creates a compressor backed by `store`.
    pub fn new(store: Arc<dyn CcrStore>) -> Self {
        Self {
            config: CodeConfig::default(),
            store,
            sizer: AdaptiveSizer::default(),
        }
    }

    /// Overrides the configuration.
    pub fn with_config(mut self, config: CodeConfig) -> Self {
        self.config = config;
        self
    }

    /// Compresses `source`, or explains why it declined.
    pub fn compress(&self, source: &str) -> Result<String> {
        if detect(source.as_bytes()).content_type != ContentType::Code {
            return Err(Error::declined(Declined::WrongContentType));
        }
        if !self.sizer.should_attempt(ContentType::Code, source.len()) {
            return Err(Error::declined(Declined::BelowThreshold));
        }
        let Some(language) = detect_language(source) else {
            return Err(Error::declined(Declined::WrongContentType));
        };

        let lines: Vec<&str> = source.lines().collect();
        let keep = self.mark_kept(&lines, language);

        let elided = keep.iter().filter(|k| !**k).count();
        if elided == 0 {
            return Err(Error::declined(Declined::NotSmaller));
        }

        let marker = store_and_mark(self.store.as_ref(), source.as_bytes(), CCR_TTL)?;

        let mut out = format!("[{} lines, {language} — bodies elided]\n", lines.len());
        let mut run = 0usize;
        for (index, line) in lines.iter().enumerate() {
            if keep[index] {
                if run > 0 {
                    out.push_str(&format!("    ... {run} lines ...\n"));
                    run = 0;
                }
                out.push_str(line);
                out.push('\n');
            } else {
                run += 1;
            }
        }
        if run > 0 {
            out.push_str(&format!("    ... {run} lines ...\n"));
        }
        out.push_str(&format!("full content: {marker}\n"));

        Ok(out)
    }

    /// Decides which lines survive.
    fn mark_kept(&self, lines: &[&str], language: Language) -> Vec<bool> {
        let mut keep = vec![true; lines.len()];
        let mut index = 0;

        while index < lines.len() {
            let line = lines[index];
            if !is_definition(line, language) {
                index += 1;
                continue;
            }

            // The signature is kept; the body after it is a candidate for elision.
            let body_start = index + 1;
            // For brace-delimited languages, `scan_braced_block` also reports how far
            // it looked before giving up. When it never found an opening brace at
            // all, every line it examined — not just this one — is known to be
            // brace-less too, so the outer loop can jump straight past all of them
            // instead of re-scanning the same tail from each one in turn. See the
            // "# Why report `scanned_to`" note on `scan_braced_block` for the O(n^2)
            // regression this closes.
            let (body_end, skip_rescan_to) = if language.indentation_scoped() {
                (
                    end_of_indented_block(lines, body_start, indent_of(line)),
                    None,
                )
            } else {
                let scan = scan_braced_block(lines, index);
                let skip = (!scan.found_open).then_some(scan.scanned_to);
                (scan.end, skip)
            };

            if body_end > body_start && body_end - body_start >= self.config.min_body_lines {
                // The closing line is kept for braced languages so the structure still
                // reads as balanced.
                let last = if language.indentation_scoped() {
                    body_end
                } else {
                    body_end.saturating_sub(1)
                };
                for slot in keep.iter_mut().take(last).skip(body_start) {
                    *slot = false;
                }
            }

            index = match skip_rescan_to {
                Some(scanned_to) => scanned_to.max(index + 1),
                None => body_end.max(index + 1),
            };
        }

        keep
    }
}

/// Whether a line opens a definition.
fn is_definition(line: &str, language: Language) -> bool {
    let trimmed = line.trim_start();
    language
        .definition_markers()
        .iter()
        .any(|marker| trimmed.starts_with(marker))
}

/// Leading whitespace width, counting a tab as one.
fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Index just past an indentation-delimited block.
fn end_of_indented_block(lines: &[&str], start: usize, opener_indent: usize) -> usize {
    let mut index = start;
    while index < lines.len() {
        let line = lines[index];
        // Blank lines belong to the block; they do not end it.
        if !line.trim().is_empty() && indent_of(line) <= opener_indent {
            break;
        }
        index += 1;
    }
    index
}

/// Outcome of scanning forward from `opener` for a brace-delimited block's end.
struct BraceScan {
    /// Index just past the block, or `opener + 1` when no balanced block was found —
    /// same convention this crate has always used for "no block found".
    end: usize,
    /// How far the forward scan actually looked before it returned.
    ///
    /// # Why this exists
    ///
    /// `mark_kept` used to call this scan once per definition line and advance its
    /// own index by exactly one line each time. A definition with no opening brace
    /// anywhere in the rest of the file — a C++ forward declaration such as
    /// `class Foo;`, say — makes the scan below run to end-of-file, and a run of N
    /// such lines in a row then cost O(n) work N times over, i.e. O(n^2) total.
    /// Reporting how far this scan traveled lets the caller skip straight past every
    /// line it already examined instead of rediscovering "no brace here either" one
    /// line at a time — but only when `found_open` is false: every line up to
    /// `scanned_to` was checked for a `{` and none had one, so none of them could
    /// open a block of their own either. When `found_open` is true (an unbalanced
    /// rather than absent brace), later lines may still legitimately open real
    /// blocks, so the caller must not skip in that case.
    scanned_to: usize,
    /// Whether an opening brace was seen anywhere in `[opener, scanned_to)`.
    found_open: bool,
}

/// Index just past a brace-delimited block.
///
/// Counts braces from the opening line. Braces inside string literals and comments are
/// not excluded, which is the main way this heuristic misjudges — and why the caller
/// treats an implausible result as "no block found" rather than trusting it. See
/// `BraceScan::scanned_to` for why the scan extent is reported alongside the result.
fn scan_braced_block(lines: &[&str], opener: usize) -> BraceScan {
    let mut depth = 0i32;
    let mut seen_open = false;

    for (offset, line) in lines.iter().enumerate().skip(opener) {
        for byte in line.bytes() {
            match byte {
                b'{' => {
                    depth += 1;
                    seen_open = true;
                }
                b'}' => depth -= 1,
                _ => {}
            }
        }
        if seen_open && depth <= 0 {
            return BraceScan {
                end: offset + 1,
                scanned_to: offset + 1,
                found_open: true,
            };
        }
    }

    // Unbalanced or brace-less. Reporting the end of input would elide the rest of
    // the file on a single stray brace, so report "nothing found" instead.
    BraceScan {
        end: opener + 1,
        scanned_to: lines.len(),
        found_open: seen_open,
    }
}

impl Transform for CodeCompressor {
    fn name(&self) -> &'static str {
        "code_compressor"
    }

    fn apply(&self, block: &mut Block) -> Result<()> {
        let compressed = self.compress(block.content())?;
        block.replace_content(compressed);
        Ok(())
    }
}

impl LossyTransform for CodeCompressor {}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ccr::InMemoryCcrStore;
    use crate::tokenizer::{HeuristicEstimator, Tokenizer};

    fn compressor() -> (CodeCompressor, Arc<InMemoryCcrStore>) {
        let store = Arc::new(InMemoryCcrStore::new());
        (CodeCompressor::new(store.clone()), store)
    }

    fn rust_source() -> String {
        let mut out = String::from("use std::collections::HashMap;\n\n");
        for i in 0..12 {
            out.push_str(&format!(
                "pub fn operation_{i}(input: &str) -> Result<String, Error> {{\n    let mut buffer = String::new();\n    let parsed = parse(input)?;\n    for item in parsed {{\n        buffer.push_str(item);\n    }}\n    let mut x = buffer.len();\n    x += 1;\n    Ok(buffer)\n}}\n\n"
            ));
        }
        out
    }

    fn python_source() -> String {
        let mut out = String::from("import os\nfrom pathlib import Path\n\n");
        // Twenty functions rather than a dozen, so the fixture clears the 2 KB code
        // threshold — otherwise this tests the sizer, not the compressor.
        for i in 0..20 {
            out.push_str(&format!(
                "def operation_{i}(value):\n    result = []\n    for item in value:\n        if item:\n            result.append(item)\n    self.total = len(result)\n    return result\n\n"
            ));
        }
        out
    }

    // ---- language detection ----

    #[test]
    fn each_language_is_recognized() {
        assert_eq!(detect_language(&rust_source()), Some(Language::Rust));
        assert_eq!(detect_language(&python_source()), Some(Language::Python));
        assert_eq!(
            detect_language("package main\nfunc main() {\n\tx := 1\n\tif x == nil {}\n}\n"),
            Some(Language::Go)
        );
        assert_eq!(
            detect_language("sub handler {\n    my $self = shift;\n    use strict;\n}\n"),
            Some(Language::Perl)
        );
    }

    #[test]
    fn ambiguous_or_non_code_input_declines_to_guess() {
        // Guessing wrong picks the wrong block-delimiter rule, and for
        // indentation-versus-brace that produces nonsense.
        assert_eq!(detect_language("just some ordinary prose here"), None);
        assert_eq!(detect_language(""), None);
        assert_eq!(
            detect_language("class Foo"),
            None,
            "one weak marker is not enough"
        );
    }

    // ---- compression ----

    #[test]
    fn signatures_survive_and_bodies_are_elided() {
        let (compressor, _store) = compressor();
        let out = compressor
            .compress(&rust_source())
            .expect("should compress");

        assert!(out.contains("pub fn operation_0"), "signature lost:\n{out}");
        assert!(out.contains("pub fn operation_11"), "signature lost");
        assert!(out.contains("lines ..."), "nothing elided");
        assert!(!out.contains("buffer.push_str(item)"), "body kept");
    }

    #[test]
    fn imports_and_top_level_lines_survive() {
        let (compressor, _store) = compressor();
        let out = compressor.compress(&rust_source()).unwrap();
        assert!(out.contains("use std::collections::HashMap;"));
    }

    #[test]
    fn indentation_scoped_languages_work_too() {
        let (compressor, _store) = compressor();
        let out = compressor
            .compress(&python_source())
            .expect("should compress");

        assert!(
            out.contains("def operation_0(value):"),
            "signature lost:\n{out}"
        );
        assert!(out.contains("import os"));
        assert!(out.contains("lines ..."), "nothing elided");
    }

    #[test]
    fn it_measurably_shrinks_real_source() {
        let (compressor, _store) = compressor();
        let source = rust_source();
        let out = compressor.compress(&source).unwrap();

        let estimator = HeuristicEstimator::new();
        assert!(estimator.count(&out) < estimator.count(&source));
    }

    #[test]
    fn short_bodies_are_left_alone() {
        // The placeholder would cost nearly as much as the lines it replaced, and the
        // reader would lose the detail for nothing.
        let (compressor, _store) = compressor();
        let mut source = String::from("use std::io;\n\n");
        for i in 0..40 {
            source.push_str(&format!("pub fn tiny_{i}() -> u32 {{\n    {i}\n}}\n\n"));
        }
        let out = compressor.compress(&source);
        if let Ok(out) = out {
            assert!(!out.contains("... 1 lines ..."), "elided a one-line body");
        }
    }

    #[test]
    fn an_unbalanced_brace_does_not_swallow_the_file() {
        // Reporting end-of-input on an unbalanced block would elide everything after a
        // single stray brace, most likely one inside a string literal.
        let lines = vec!["fn broken() {", "    let s = \"{\";", "    do_thing();"];
        assert_eq!(
            scan_braced_block(&lines, 0).end,
            1,
            "should report no block"
        );
    }

    #[test]
    fn a_long_run_of_brace_less_definitions_stays_linear() {
        // Regression for the O(n^2) blowup this file used to have: a C++ forward
        // declaration (`class Foo;`) matches `is_definition` but never opens a brace,
        // so `end_of_braced_block`/`scan_braced_block` used to scan all the way to
        // end-of-file to discover that, and `mark_kept`'s outer loop only advanced by
        // one line afterward — a run of N such lines cost O(n) work N times over. The
        // fix has `scan_braced_block` report how far it looked, and `mark_kept` jumps
        // its index past that whole range in one step when no brace was found, which
        // makes each line get examined by the brace scan at most once across the
        // entire pass. Before the fix, 15,000 lines here cost on the order of 100
        // million line-visits and took well over a second; after the fix it is a
        // single linear scan and completes near-instantly, which this test's normal
        // pass (well inside `cargo test`'s default timeout) is the evidence for.
        let (compressor, _store) = compressor();
        let lines: Vec<&str> = std::iter::repeat("class Forward;").take(15_000).collect();

        let keep = compressor.mark_kept(&lines, Language::Cpp);

        // A brace-less definition never has a body to elide, so every line survives.
        assert_eq!(keep.len(), lines.len());
        assert!(
            keep.iter().all(|k| *k),
            "brace-less definitions have no body to elide"
        );
    }

    #[test]
    fn blank_lines_do_not_end_an_indented_block() {
        let lines = vec!["def f():", "    a = 1", "", "    b = 2", "c = 3"];
        assert_eq!(end_of_indented_block(&lines, 1, 0), 4);
    }

    // ---- declining ----

    #[test]
    fn non_code_declines_and_stores_nothing() {
        let (compressor, store) = compressor();
        assert!(compressor
            .compress(&"ordinary prose. ".repeat(200))
            .is_err());
        assert!(store.is_empty());
    }

    #[test]
    fn compression_is_deterministic() {
        let source = rust_source();
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
