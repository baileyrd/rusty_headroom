//! Choosing where a compressor may cut — gap row S4, invariant I6.
//!
//! # Position-preserving means more than "same order"
//!
//! Invariant I6 says compression edits blocks in place and never reorders them. Within
//! a block there is a second, quieter version of the same requirement: content the
//! model will refer back to by position has to *stay* at a position it can refer to.
//!
//! A model that has seen `line 47` in a stack trace, or a `## Errors` heading, or an
//! opening `{` in a JSON fragment, will reason about what follows it. A compressor that
//! removes the line but keeps its body has left content whose meaning depended on a
//! marker that is now gone — and the model cannot tell, because what remains reads as
//! though it were always the whole thing.
//!
//! # An anchor is a line that must survive
//!
//! Not a line that is *important* — [`crate::signals::tiered`] already scores that.
//! An anchor is a line that other lines depend on. The two overlap and are not the
//! same: a heading may say nothing on its own and still be the only thing that makes
//! the paragraph beneath it interpretable.

/// Why a line was selected as an anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AnchorKind {
    /// A diff hunk header — carries the line numbers the rest of the hunk is relative to.
    HunkHeader,
    /// A Markdown or reStructuredText heading.
    Heading,
    /// A fenced-code delimiter. Removing one leaves the fence unbalanced.
    Fence,
    /// A stack-frame line naming a file and position.
    StackFrame,
    /// A line opening a structure that later lines close.
    StructureOpen,
    /// The first or last line, which bound everything between them.
    Boundary,
}

impl AnchorKind {
    /// A stable identifier, for telemetry.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HunkHeader => "hunk_header",
            Self::Heading => "heading",
            Self::Fence => "fence",
            Self::StackFrame => "stack_frame",
            Self::StructureOpen => "structure_open",
            Self::Boundary => "boundary",
        }
    }
}

/// A line that must survive compression, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    /// Index into the line array.
    pub line: usize,
    /// What makes it an anchor.
    pub kind: AnchorKind,
}

/// Finds every line that must survive compression of `lines`.
///
/// Returned in ascending line order with no duplicates, so a caller can merge the set
/// with its own keep-list without sorting or deduplicating again.
///
/// # Erring toward keeping
///
/// A line wrongly marked as an anchor costs a few tokens. A line wrongly *not* marked
/// leaves content whose meaning depended on it, and the model cannot tell — what remains
/// reads as though it were always the whole thing. The two errors are not comparable,
/// so every ambiguous case is an anchor.
///
/// # Example
///
/// ```
/// use headroom_core::signals::anchors::{select_anchors, AnchorKind};
///
/// let lines = ["@@ -1,3 +1,4 @@", " context", "+added", " more context"];
/// let anchors = select_anchors(&lines);
///
/// // The hunk header carries the line numbers everything else is relative to.
/// assert_eq!(anchors[0].kind, AnchorKind::HunkHeader);
/// ```
pub fn select_anchors(lines: &[&str]) -> Vec<Anchor> {
    let mut anchors = Vec::new();

    if lines.is_empty() {
        return anchors;
    }

    for (index, line) in lines.iter().enumerate() {
        if let Some(kind) = classify(line) {
            anchors.push(Anchor { line: index, kind });
        }
    }

    // The first and last lines bound everything between them. A compressor that drops
    // the first line of a tool result has changed what the output *starts with*, which
    // is the one thing a reader uses to decide what they are looking at.
    for boundary in [0, lines.len() - 1] {
        if !anchors.iter().any(|anchor| anchor.line == boundary) {
            anchors.push(Anchor {
                line: boundary,
                kind: AnchorKind::Boundary,
            });
        }
    }

    // Ascending and deduplicated, so a caller can merge this with its own keep-list
    // without sorting again. Sorted by line then kind so the order does not depend on
    // which rule matched first.
    anchors.sort_by(|a, b| a.line.cmp(&b.line).then(a.kind.cmp(&b.kind)));
    anchors.dedup_by_key(|anchor| anchor.line);
    anchors
}

/// Classifies one line, if it anchors anything.
fn classify(line: &str) -> Option<AnchorKind> {
    let trimmed = line.trim();

    if trimmed.starts_with("@@") {
        return Some(AnchorKind::HunkHeader);
    }
    // A fence is checked before a heading, since ``` and ~~~ both start with characters
    // no heading uses — but a fence carrying a language tag (```rust) would otherwise
    // fall through to the generic checks.
    if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
        return Some(AnchorKind::Fence);
    }
    if is_heading(trimmed) {
        return Some(AnchorKind::Heading);
    }
    if is_stack_frame(trimmed) {
        return Some(AnchorKind::StackFrame);
    }
    // Checked last and on the *original* line, not the trimmed one: a line whose only
    // content is an opening brace is structure, while a line that merely contains one
    // is ordinary code.
    if opens_structure(line) {
        return Some(AnchorKind::StructureOpen);
    }

    None
}

/// Whether `line` is a Markdown or underline-style heading.
fn is_heading(line: &str) -> bool {
    // `#` followed by a space. Requiring the space is what keeps `#!/bin/sh` and a
    // `#include` from being read as headings — both are common in exactly the content
    // this runs on.
    if let Some(rest) = line.strip_prefix('#') {
        let hashes: String = rest.chars().take_while(|c| *c == '#').collect();
        let after = &rest[hashes.len()..];
        return after.starts_with(' ') && !after.trim().is_empty();
    }

    // An underline of `=` or `-`. Requires three or more, since `--` is a flag and `-`
    // alone is a list bullet.
    line.len() >= 3 && (line.chars().all(|c| c == '=') || line.chars().all(|c| c == '-'))
}

/// Whether `line` names a source position, as a stack frame does.
///
/// Deliberately narrow. A path with a line number is the shape that matters —
/// `src/main.rs:42`, `at foo (bar.js:10:5)`, `File "x.py", line 3` — because that is
/// what a model refers back to. Matching any line containing a colon would anchor most
/// of a log file and defeat the point.
fn is_stack_frame(line: &str) -> bool {
    if line.starts_with("at ") || line.starts_with("File \"") {
        return true;
    }

    // `path:line` or `path:line:column`, where the path has an extension. The extension
    // requirement is what separates `src/main.rs:42` from `12:30:05` in a timestamp.
    line.split_whitespace().any(|token| {
        let mut parts = token.rsplit(':');
        let last = parts.next().unwrap_or_default();
        let rest: Vec<&str> = parts.collect();

        !last.is_empty()
            && last.chars().all(|c| c.is_ascii_digit())
            && rest
                .first()
                .map(|head| head.contains('.') && !head.chars().all(|c| c.is_ascii_digit()))
                .unwrap_or(false)
    })
}

/// Whether `line`'s only content is an opening delimiter.
fn opens_structure(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && trimmed.chars().all(|c| matches!(c, '{' | '[' | '('))
}

/// Whether the line at `index` may be removed.
///
/// The question a compressor actually asks. Equivalent to "not in `anchors`", exposed
/// so the check reads as intent at the call site rather than as a set lookup.
pub fn is_removable(anchors: &[Anchor], index: usize) -> bool {
    anchors
        .binary_search_by_key(&index, |anchor| anchor.line)
        .is_err()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(lines: &[&str]) -> Vec<AnchorKind> {
        select_anchors(lines).into_iter().map(|a| a.kind).collect()
    }

    // ---- what anchors ----

    #[test]
    fn a_diff_hunk_header_anchors() {
        // It carries the line numbers everything else in the hunk is relative to. A
        // diff without them cannot be located against a file.
        let lines = ["@@ -1,3 +1,4 @@", " ctx", "+new", " ctx"];
        assert_eq!(select_anchors(&lines)[0].kind, AnchorKind::HunkHeader);
    }

    #[test]
    fn markdown_headings_anchor() {
        for line in ["# Title", "## Errors", "### Deeply nested"] {
            let lines = ["body", line, "body"];
            assert!(
                kinds(&lines).contains(&AnchorKind::Heading),
                "{line:?} did not anchor"
            );
        }
    }

    #[test]
    fn a_code_fence_anchors() {
        // Removing one leaves the fence unbalanced, so everything after it renders as
        // code — or nothing does.
        for line in ["```", "```rust", "~~~"] {
            let lines = ["body", line, "body"];
            assert!(kinds(&lines).contains(&AnchorKind::Fence), "{line:?}");
        }
    }

    #[test]
    fn stack_frames_anchor() {
        for line in [
            "    at handler (src/server.js:10:5)",
            "  File \"app.py\", line 3, in main",
            "  src/main.rs:42",
        ] {
            let lines = ["body", line, "body"];
            assert!(
                kinds(&lines).contains(&AnchorKind::StackFrame),
                "{line:?} did not anchor"
            );
        }
    }

    #[test]
    fn a_line_that_only_opens_a_structure_anchors() {
        let lines = ["body", "{", "  \"a\": 1", "}"];
        assert!(kinds(&lines).contains(&AnchorKind::StructureOpen));
    }

    #[test]
    fn the_first_and_last_lines_always_anchor() {
        // A compressor that drops the first line has changed what the output *starts
        // with*, which is the one thing a reader uses to decide what they are looking
        // at.
        let lines = ["first", "middle", "middle", "last"];
        let anchors = select_anchors(&lines);

        assert!(anchors.iter().any(|a| a.line == 0));
        assert!(anchors.iter().any(|a| a.line == 3));
    }

    // ---- what must not anchor ----

    #[test]
    fn a_shebang_is_not_a_heading() {
        // `#!/bin/sh` and `#include` are common in exactly the content this runs on,
        // and reading them as headings would anchor the top of every script.
        for line in ["#!/bin/sh", "#include <stdio.h>", "#define X 1", "#"] {
            assert!(!is_heading(line), "{line:?}");
        }
    }

    #[test]
    fn an_empty_heading_does_not_anchor() {
        assert!(!is_heading("# "));
        assert!(!is_heading("###"));
    }

    #[test]
    fn a_timestamp_is_not_a_stack_frame() {
        // `12:30:05` has the colon-and-digits shape. Matching it would anchor most of a
        // log file and defeat the point entirely.
        for line in [
            "2026-01-01 12:30:05 INFO started",
            "elapsed 00:01:23",
            "ratio 3:1",
        ] {
            assert!(!is_stack_frame(line), "{line:?}");
        }
    }

    #[test]
    fn a_flag_or_bullet_is_not_a_heading_underline() {
        for line in ["--", "-", "- item", "=="] {
            assert!(!is_heading(line), "{line:?}");
        }
        assert!(is_heading("==="), "three or more is an underline");
    }

    #[test]
    fn a_line_merely_containing_a_brace_is_not_structure() {
        // Only a line whose *whole* content is an opening delimiter. Otherwise every
        // line of code anchors and nothing can be compressed.
        for line in ["fn main() {", "let x = [1, 2];", "if (a) {"] {
            assert!(!opens_structure(line), "{line:?}");
        }
        assert!(opens_structure("  {  "));
    }

    #[test]
    fn ordinary_prose_anchors_nothing_but_the_boundaries() {
        let lines = [
            "The quick brown fox",
            "jumps over",
            "the lazy dog",
            "and keeps going",
        ];
        let anchors = select_anchors(&lines);

        assert_eq!(anchors.len(), 2, "{anchors:?}");
        assert!(anchors.iter().all(|a| a.kind == AnchorKind::Boundary));
    }

    // ---- shape of the result ----

    #[test]
    fn anchors_are_ascending_and_deduplicated() {
        // So a caller can merge this with its own keep-list without sorting again. A
        // line matching two rules must appear once.
        let lines = ["# Title", "@@ -1 +1 @@", "body", "```"];
        let anchors = select_anchors(&lines);

        let indices: Vec<usize> = anchors.iter().map(|a| a.line).collect();
        let mut sorted = indices.clone();
        sorted.sort_unstable();
        sorted.dedup();

        assert_eq!(indices, sorted);
    }

    #[test]
    fn the_result_does_not_depend_on_which_rule_matched_first() {
        // Sorted by line then kind, so a line matching two rules always reports the
        // same one — otherwise the same input compresses differently between builds.
        let lines = ["```rust", "body", "@@ -1 +1 @@"];
        let first = select_anchors(&lines);
        for _ in 0..25 {
            assert_eq!(select_anchors(&lines), first);
        }
    }

    #[test]
    fn is_removable_agrees_with_the_anchor_set() {
        let lines = ["# Title", "body", "more body", "end"];
        let anchors = select_anchors(&lines);

        assert!(!is_removable(&anchors, 0), "a heading is not removable");
        assert!(is_removable(&anchors, 1));
        assert!(is_removable(&anchors, 2));
        assert!(!is_removable(&anchors, 3), "the last line is a boundary");
    }

    // ---- edges ----

    #[test]
    fn an_empty_input_yields_no_anchors() {
        assert!(select_anchors(&[]).is_empty());
    }

    #[test]
    fn a_single_line_anchors_once_not_twice() {
        // It is both the first and the last line. Reporting it twice would make a
        // caller's keep-count wrong.
        let anchors = select_anchors(&["only"]);
        assert_eq!(anchors.len(), 1);
    }

    #[test]
    fn multibyte_content_does_not_panic() {
        let lines = ["# 日本語", "😀 body", "café"];
        let _ = select_anchors(&lines);
    }
}
