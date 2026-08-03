//! The content router.

use std::fmt;

/// The kind of content a block holds, which determines the compressor it routes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContentType {
    /// JSON objects or arrays. Handled by SmartCrusher.
    Json,
    /// Source code in any supported language. Handled by the code compressor.
    Code,
    /// Log lines, typically timestamped and highly repetitive.
    Log,
    /// A unified diff or patch.
    Diff,
    /// Search or grep output: `path:line:match` triples and similar shapes.
    SearchResults,
    /// Natural-language text.
    Prose,
    /// Nothing recognizable, or empty. Always forwarded unchanged.
    Unknown,
}

impl ContentType {
    /// Every variant, in declaration order.
    ///
    /// Exists so a caller that wants to say something about all of them — `headroom
    /// tools` listing what compresses, the reachability audit — enumerates them from
    /// here instead of writing the list out. A hand-written list is one that silently
    /// stops being complete: `headroom tools` carried one, and it reported code and
    /// prose as "detected but not compressed" for as long as it took anyone to notice,
    /// which was well after both had compressors.
    ///
    /// Adding a variant without adding it here is possible, so the exhaustiveness is
    /// pinned by `every_variant_is_in_all` rather than assumed.
    pub const ALL: [Self; 7] = [
        Self::Json,
        Self::Code,
        Self::Log,
        Self::Diff,
        Self::SearchResults,
        Self::Prose,
        Self::Unknown,
    ];

    /// A stable identifier for telemetry and error messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Code => "code",
            Self::Log => "log",
            Self::Diff => "diff",
            Self::SearchResults => "search",
            Self::Prose => "prose",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for ContentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The router's verdict, with a confidence signal.
///
/// Confidence is reported rather than thresholded away because misrouting is cheap
/// to recover from but expensive to diagnose. A compressor handed the wrong content
/// type declines, invariant I5 forwards the original, and nothing breaks — but
/// without a confidence signal there is no way to tell a confidently-routed block
/// from a coin flip when investigating why compression ratios dropped.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Detection {
    /// The detected content type.
    pub content_type: ContentType,
    /// How strongly the evidence supported this classification, in `0.0..=1.0`.
    ///
    /// Roughly: `1.0` means a structural guarantee (the bytes parse as JSON),
    /// mid-range means converging heuristics, and low values mean the type was
    /// picked as a default with little positive evidence.
    pub confidence: f32,
}

impl Detection {
    fn new(content_type: ContentType, confidence: f32) -> Self {
        Self {
            content_type,
            confidence,
        }
    }

    /// Whether the classification is strong enough to act on without hesitation.
    pub fn is_confident(&self) -> bool {
        self.confidence >= 0.75
    }
}

/// Classifies `bytes` and returns the content type with a confidence signal.
///
/// Detection order matters. The checks run cheapest-and-most-certain first, so a
/// block that structurally *is* JSON is never reclassified by a later heuristic
/// that happens to also match.
///
/// # Example
///
/// ```
/// use headroom_core::detection::{detect, ContentType};
///
/// let d = detect(br#"{"status": "ok", "count": 42}"#);
/// assert_eq!(d.content_type, ContentType::Json);
/// assert!(d.is_confident());
/// ```
pub fn detect(bytes: &[u8]) -> Detection {
    // Non-UTF-8 input is binary as far as this project is concerned. The
    // architecture explicitly excludes images and base64 blobs from compression, so
    // there is nothing useful to do but pass it through.
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Detection::new(ContentType::Unknown, 1.0);
    };

    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Detection::new(ContentType::Unknown, 1.0);
    }

    // JSON first: it is the only type that can be confirmed rather than guessed, and
    // it is the highest-value compression target (60-95% on structured data).
    if looks_like_json(trimmed) && serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return Detection::new(ContentType::Json, 1.0);
    }

    // Diffs are next because their markers are unambiguous and they would otherwise
    // be misread as code — a diff of a Rust file is full of Rust syntax.
    if let Some(confidence) = diff_confidence(trimmed) {
        return Detection::new(ContentType::Diff, confidence);
    }

    // Search results before logs and code: `path/to/file.rs:42:    let x = 1;`
    // contains both code-like and log-like features, so the more specific shape has
    // to win.
    if let Some(confidence) = search_results_confidence(trimmed) {
        return Detection::new(ContentType::SearchResults, confidence);
    }

    if let Some(confidence) = log_confidence(trimmed) {
        return Detection::new(ContentType::Log, confidence);
    }

    if let Some(confidence) = code_confidence(trimmed) {
        return Detection::new(ContentType::Code, confidence);
    }

    // Prose is the residual class. Low confidence is honest here: nothing positively
    // identified this as natural language, it simply failed to look like anything
    // else.
    Detection::new(ContentType::Prose, 0.4)
}

/// Cheap structural pre-check so we do not hand every block to the JSON parser.
fn looks_like_json(trimmed: &str) -> bool {
    let first = trimmed.as_bytes().first().copied();
    let last = trimmed.as_bytes().last().copied();
    matches!(
        (first, last),
        (Some(b'{'), Some(b'}')) | (Some(b'['), Some(b']'))
    )
}

/// Unified diff detection.
fn diff_confidence(trimmed: &str) -> Option<f32> {
    let mut has_file_markers = false;
    let mut hunk_headers = 0usize;
    let mut change_lines = 0usize;

    let mut lines = trimmed.lines().peekable();
    while let Some(line) = lines.next() {
        if line.starts_with("--- ") {
            // `---` alone is also a Markdown horizontal rule and a YAML document
            // separator. Requiring the paired `+++` on the next line is what makes
            // this a diff rather than either of those.
            if lines.peek().is_some_and(|next| next.starts_with("+++ ")) {
                has_file_markers = true;
            }
        } else if line.starts_with("@@") && line.trim_end().ends_with("@@") {
            hunk_headers += 1;
        } else if line.starts_with("diff --git ") {
            has_file_markers = true;
        } else if (line.starts_with('+') || line.starts_with('-')) && line.len() > 1 {
            change_lines += 1;
        }
    }

    match (has_file_markers, hunk_headers, change_lines) {
        // File markers and hunk headers together are conclusive.
        (true, h, _) if h > 0 => Some(1.0),
        (true, 0, c) if c > 0 => Some(0.85),
        // Hunk headers alone: a diff fragment without its file header, which is
        // common when a tool returns just the changed region.
        (false, h, c) if h > 0 && c > 0 => Some(0.8),
        _ => None,
    }
}

/// Search / grep output detection: repeated `path:line:` prefixes.
fn search_results_confidence(trimmed: &str) -> Option<f32> {
    let lines: Vec<&str> = trimmed.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() < 2 {
        return None;
    }

    let matching = lines.iter().filter(|l| has_path_line_prefix(l)).count();
    let ratio = matching as f32 / lines.len() as f32;

    // Requiring a strong majority avoids claiming a code block is search output just
    // because a couple of lines contain a colon.
    if ratio >= 0.8 {
        Some(0.9)
    } else if ratio >= 0.6 {
        Some(0.7)
    } else {
        None
    }
}

/// Matches `some/path.rs:42:` and `some/path.rs:42:7:` prefixes.
fn has_path_line_prefix(line: &str) -> bool {
    let mut parts = line.splitn(3, ':');
    let (Some(path), Some(number)) = (parts.next(), parts.next()) else {
        return false;
    };
    if path.is_empty() || number.is_empty() {
        return false;
    }
    // A path here means something with a separator or an extension; a bare word
    // followed by a colon is far more likely to be prose or a log level.
    let path_like = path.contains('/') || path.contains('.') || path.contains('\\');
    path_like && number.bytes().all(|b| b.is_ascii_digit())
}

/// Log detection: leading timestamps or severity levels on most lines.
fn log_confidence(trimmed: &str) -> Option<f32> {
    let lines: Vec<&str> = trimmed.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() < 2 {
        return None;
    }

    let timestamped = lines.iter().filter(|l| starts_with_timestamp(l)).count();
    let levelled = lines.iter().filter(|l| contains_log_level(l)).count();

    let timestamp_ratio = timestamped as f32 / lines.len() as f32;
    let level_ratio = levelled as f32 / lines.len() as f32;

    if timestamp_ratio >= 0.8 {
        Some(0.95)
    } else if timestamp_ratio >= 0.5 || level_ratio >= 0.8 {
        Some(0.8)
    } else if level_ratio >= 0.5 {
        Some(0.65)
    } else {
        None
    }
}

/// Recognizes `2026-08-03...`, `[2026-08-03...`, and `12:34:56` line openings.
fn starts_with_timestamp(line: &str) -> bool {
    let line = line.trim_start().trim_start_matches(['[', '(']);
    let bytes = line.as_bytes();

    // ISO-8601-ish date: `YYYY-MM-DD`.
    let iso_date = bytes.len() >= 10
        && bytes[0..4].iter().all(|b| b.is_ascii_digit())
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(|b| b.is_ascii_digit())
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(|b| b.is_ascii_digit());

    // Bare clock time: `HH:MM:SS`.
    let clock = bytes.len() >= 8
        && bytes[0..2].iter().all(|b| b.is_ascii_digit())
        && bytes[2] == b':'
        && bytes[3..5].iter().all(|b| b.is_ascii_digit())
        && bytes[5] == b':'
        && bytes[6..8].iter().all(|b| b.is_ascii_digit());

    iso_date || clock
}

/// Severity levels, matched as whole uppercase words to avoid firing on prose.
fn contains_log_level(line: &str) -> bool {
    const LEVELS: [&str; 7] = [
        "TRACE", "DEBUG", "INFO", "WARN", "WARNING", "ERROR", "FATAL",
    ];
    LEVELS.iter().any(|level| {
        line.match_indices(level).any(|(idx, _)| {
            let before_ok = idx == 0
                || !line.as_bytes()[idx - 1].is_ascii_alphanumeric()
                    && line.as_bytes()[idx - 1] != b'_';
            let after = idx + level.len();
            let after_ok = after >= line.len()
                || !line.as_bytes()[after].is_ascii_alphanumeric()
                    && line.as_bytes()[after] != b'_';
            before_ok && after_ok
        })
    })
}

/// Code detection by syntactic density.
///
/// Rather than trying to identify a specific language, this measures how much the
/// text looks like *any* programming language: keyword hits and structural
/// punctuation. Per-language identification belongs to the code compressor, which
/// needs it for parsing; the router only needs to know it is code.
fn code_confidence(trimmed: &str) -> Option<f32> {
    const KEYWORDS: [&str; 24] = [
        "fn ",
        "def ",
        "class ",
        "function ",
        "import ",
        "from ",
        "return ",
        "const ",
        "let ",
        "var ",
        "if (",
        "if(",
        "for (",
        "for(",
        "while (",
        "while(",
        "public ",
        "private ",
        "struct ",
        "impl ",
        "package ",
        "func ",
        "#include",
        "=>",
    ];

    let keyword_hits = KEYWORDS.iter().filter(|kw| trimmed.contains(*kw)).count();

    let structural = trimmed
        .bytes()
        .filter(|b| matches!(b, b'{' | b'}' | b'(' | b')' | b'[' | b']' | b';'))
        .count();
    let density = structural as f32 / trimmed.len().max(1) as f32;

    // Indentation is weak evidence on its own — Markdown and email quoting are also
    // indented — but it corroborates the other two signals well.
    let lines: Vec<&str> = trimmed.lines().filter(|l| !l.trim().is_empty()).collect();
    let indented = lines
        .iter()
        .filter(|l| l.starts_with("  ") || l.starts_with('\t'))
        .count();
    let indent_ratio = if lines.is_empty() {
        0.0
    } else {
        indented as f32 / lines.len() as f32
    };

    match (keyword_hits, density) {
        (k, d) if k >= 3 && d >= 0.02 => Some(0.95),
        (k, d) if k >= 2 && d >= 0.03 => Some(0.85),
        (k, _) if k >= 3 => Some(0.75),
        (k, d) if k >= 1 && d >= 0.05 && indent_ratio >= 0.3 => Some(0.7),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ty(bytes: &[u8]) -> ContentType {
        detect(bytes).content_type
    }

    #[test]
    fn every_variant_is_in_all() {
        // Two halves, and both are load-bearing.
        //
        // The match below is exhaustive, so adding a variant to the enum stops this file
        // compiling until somebody comes here — and what they find is this comment
        // telling them to add it to `ALL` too.
        //
        // The assertion then catches the other order: a variant added to the match and
        // not to `ALL`. Without it, `ALL` could quietly go stale and every caller that
        // iterates it would go quietly incomplete, which is the failure `headroom tools`
        // shipped for two content types.
        let mut seen = Vec::new();
        for content_type in ContentType::ALL {
            match content_type {
                ContentType::Json
                | ContentType::Code
                | ContentType::Log
                | ContentType::Diff
                | ContentType::SearchResults
                | ContentType::Prose
                | ContentType::Unknown => seen.push(content_type.as_str()),
            }
        }

        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            ContentType::ALL.len(),
            "ALL has a duplicate or a variant is missing: {seen:?}"
        );
    }

    #[test]
    fn empty_and_whitespace_are_unknown() {
        assert_eq!(ty(b""), ContentType::Unknown);
        assert_eq!(ty(b"   \n\t  "), ContentType::Unknown);
    }

    #[test]
    fn invalid_utf8_is_unknown() {
        // Binary payloads are explicitly out of scope for compression.
        assert_eq!(ty(&[0xff, 0xfe, 0x00, 0x01]), ContentType::Unknown);
    }

    #[test]
    fn detection_is_deterministic() {
        let sample = br#"{"a": [1, 2, 3], "b": {"c": true}}"#;
        let first = detect(sample);
        for _ in 0..50 {
            assert_eq!(detect(sample), first);
        }
    }

    // ---- JSON ----

    #[test]
    fn json_object_and_array_detected_with_full_confidence() {
        let d = detect(br#"{"status":"ok","items":[1,2,3]}"#);
        assert_eq!(d.content_type, ContentType::Json);
        assert_eq!(d.confidence, 1.0);

        assert_eq!(ty(br#"[{"id":1},{"id":2}]"#), ContentType::Json);
    }

    #[test]
    fn json_shaped_but_invalid_is_not_json() {
        // The near-miss: looks like JSON structurally, does not parse. Routing this
        // to SmartCrusher would guarantee a parse failure downstream.
        assert_ne!(ty(br#"{"a": 1, "b":}"#), ContentType::Json);
    }

    // ---- Diff ----

    #[test]
    fn unified_diff_detected() {
        let diff = b"--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,3 +1,3 @@\n-old line\n+new line\n context\n";
        let d = detect(diff);
        assert_eq!(d.content_type, ContentType::Diff);
        assert!(d.is_confident());
    }

    #[test]
    fn markdown_horizontal_rule_is_not_a_diff() {
        // The near-miss for diffs: `---` with no paired `+++` and no hunk header.
        let md = b"Some heading\n---\nBody text follows here and continues.\n";
        assert_ne!(ty(md), ContentType::Diff);
    }

    #[test]
    fn diff_of_code_is_a_diff_not_code() {
        // Ordering check: a diff whose content is Rust must not classify as Code.
        let diff = b"diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n@@ -1,2 +1,2 @@\n-fn old() { return 1; }\n+fn new() { return 2; }\n";
        assert_eq!(ty(diff), ContentType::Diff);
    }

    // ---- Search results ----

    #[test]
    fn grep_output_detected() {
        let grep = b"src/main.rs:12:    let x = compute();\nsrc/lib.rs:44:    let y = compute();\nsrc/util.rs:7:fn compute() {}\n";
        let d = detect(grep);
        assert_eq!(d.content_type, ContentType::SearchResults);
        assert!(d.is_confident());
    }

    #[test]
    fn prose_with_colons_is_not_search_results() {
        // The near-miss: colons are everywhere in ordinary text.
        let prose = b"Note: this is important.\nWarning: read carefully.\nSummary: all done.\n";
        assert_ne!(ty(prose), ContentType::SearchResults);
    }

    // ---- Logs ----

    #[test]
    fn timestamped_logs_detected() {
        let log = b"2026-08-03 10:15:00 INFO starting service\n2026-08-03 10:15:01 INFO listening on 8080\n2026-08-03 10:15:02 ERROR upstream timeout\n";
        let d = detect(log);
        assert_eq!(d.content_type, ContentType::Log);
        assert!(d.is_confident());
    }

    #[test]
    fn level_only_logs_detected() {
        let log =
            b"INFO service started\nWARN retrying connection\nERROR gave up after 3 attempts\n";
        assert_eq!(ty(log), ContentType::Log);
    }

    #[test]
    fn prose_mentioning_error_is_not_a_log() {
        // The near-miss: the word "error" in a sentence, and "information"
        // containing "INFO" as a substring.
        let prose = b"There was an error in the information we received.\nPlease review the document carefully before responding.\n";
        assert_ne!(ty(prose), ContentType::Log);
    }

    // ---- Code ----

    #[test]
    fn rust_source_detected_as_code() {
        let code = br#"
fn main() {
    let config = load_config();
    if (config.verbose) {
        println!("starting");
    }
}

struct Config { verbose: bool }
"#;
        let d = detect(code);
        assert_eq!(d.content_type, ContentType::Code);
        assert!(d.is_confident());
    }

    #[test]
    fn python_source_detected_as_code() {
        let code = br#"
import os
from pathlib import Path

def main():
    for entry in os.listdir("."):
        if (entry.endswith(".py")):
            return Path(entry)
"#;
        assert_eq!(ty(code), ContentType::Code);
    }

    #[test]
    fn plain_english_is_prose_not_code() {
        // The near-miss for code: prose containing the word "class" and parentheses.
        let prose = b"The class began at noon (as scheduled) and the students returned afterwards. Everyone agreed it went well.";
        assert_eq!(ty(prose), ContentType::Prose);
    }

    #[test]
    fn prose_confidence_is_low_because_it_is_the_residual_class() {
        // Prose is what is left when nothing matched, and the confidence should say
        // so rather than overstating the evidence.
        let d = detect(b"Just an ordinary sentence with nothing distinctive about it at all.");
        assert_eq!(d.content_type, ContentType::Prose);
        assert!(!d.is_confident());
    }
}
