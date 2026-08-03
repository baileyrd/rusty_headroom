//! Refusing to work on input that would make the proxy the problem — gap row P6.
//!
//! # Why a compressor needs a guard at all
//!
//! Every compressor here is a pure function over a customer's content, and that content
//! is not a promise. It arrives from a model, from a tool that scraped a page, from a
//! file nobody read. A compressor that recurses on nesting depth, or scans quadratically
//! over line count, turns a large tool result into a stalled request — and the proxy
//! sits in the path of *every* request, so one pathological payload becomes an outage
//! rather than a slow response.
//!
//! # The check answers "should this run", not "is this valid"
//!
//! Nothing here rejects a request. The proxy is not a validator: a payload that fails
//! these checks is forwarded **uncompressed**, which is the outcome the customer would
//! have had without a proxy at all. Refusing the request instead would break traffic
//! that works fine today because this crate was cautious about it.
//!
//! That asymmetry is why the limits can be set generously. Being wrong costs a missed
//! compression on an unusual payload; being absent costs a stall on the request path.

use crate::detection::ContentType;

/// Why a payload was declined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hazard {
    /// Larger than any compressor is expected to handle.
    TooLarge {
        /// Its size in bytes.
        bytes: usize,
    },
    /// Nested deeper than the analyzers walk.
    TooDeep {
        /// The depth reached.
        depth: usize,
    },
    /// A single line long enough to defeat line-oriented compressors.
    LineTooLong {
        /// The longest line's length.
        bytes: usize,
    },
    /// So many lines that per-line work becomes the dominant cost.
    TooManyLines {
        /// How many.
        lines: usize,
    },
}

impl Hazard {
    /// A stable identifier, for telemetry.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TooLarge { .. } => "too_large",
            Self::TooDeep { .. } => "too_deep",
            Self::LineTooLong { .. } => "line_too_long",
            Self::TooManyLines { .. } => "too_many_lines",
        }
    }
}

/// Limits beyond which compression is not attempted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Largest payload to compress.
    pub max_bytes: usize,
    /// Deepest JSON nesting to analyze.
    pub max_depth: usize,
    /// Longest single line.
    pub max_line_bytes: usize,
    /// Most lines.
    pub max_lines: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            // 8 MiB. A real tool result is kilobytes; anything past this is a file dump
            // that was never going to compress usefully, and scanning it costs more
            // than forwarding it.
            max_bytes: 8 * 1024 * 1024,
            // 64 levels. Legitimate JSON is rarely past 10, and the analyzers walk
            // recursively — this is the depth at which a hand-crafted payload starts
            // being about the stack rather than about data.
            max_depth: 64,
            // 1 MiB on one line. A minified bundle or a base64 blob arrives this way,
            // and line-oriented compressors have nothing to do with it but will still
            // scan every byte looking.
            max_line_bytes: 1024 * 1024,
            // 200k lines. Past this, per-line scoring dominates and the saving does not
            // grow with it.
            max_lines: 200_000,
        }
    }
}

/// Checks `content` against `limits`, returning the first hazard found.
///
/// Ordered cheapest-first: size is a length read, line counts need one pass, depth
/// needs a parse. A payload rejected on size never pays for the parse — which matters,
/// because the payloads most likely to be rejected are the most expensive to analyze.
///
/// # Example
///
/// ```
/// use headroom_core::pipeline::safety::{check, Limits};
/// use headroom_core::detection::ContentType;
///
/// // Ordinary content passes.
/// assert!(check("[{\"a\":1}]", ContentType::Json, Limits::default()).is_none());
///
/// // A payload nested past the limit is declined rather than walked.
/// let deep = format!("{}1{}", "[".repeat(500), "]".repeat(500));
/// assert!(check(&deep, ContentType::Json, Limits::default()).is_some());
/// ```
pub fn check(content: &str, content_type: ContentType, limits: Limits) -> Option<Hazard> {
    if content.len() > limits.max_bytes {
        return Some(Hazard::TooLarge {
            bytes: content.len(),
        });
    }

    // One pass for both line facts, since walking twice over a large payload to answer
    // two questions is the kind of cost this module exists to avoid.
    let mut lines = 0usize;
    let mut longest = 0usize;
    for line in content.split('\n') {
        lines += 1;
        longest = longest.max(line.len());
    }

    if longest > limits.max_line_bytes {
        return Some(Hazard::LineTooLong { bytes: longest });
    }
    if lines > limits.max_lines {
        return Some(Hazard::TooManyLines { lines });
    }

    // Depth is measured for recognized JSON *and* for anything bracket-shaped that
    // detection did not recognize — which is precisely what a hand-crafted payload
    // looks like. Gating on `ContentType::Json` alone would skip the check on exactly
    // the input it exists for, since 500 nested brackets carrying no data do not
    // classify as JSON. Still skipped for a log file, which is the common case and has
    // no depth to speak of.
    let bracket_shaped = matches!(content.trim_start().as_bytes().first(), Some(b'[' | b'{'));
    if content_type == ContentType::Json || bracket_shaped {
        let depth = bracket_depth(content);
        if depth > limits.max_depth {
            return Some(Hazard::TooDeep { depth });
        }
    }

    None
}

/// The maximum bracket nesting in `content`.
///
/// Counted by scanning rather than by parsing, because the payload this exists to catch
/// is precisely the one a recursive parser would blow the stack on. Brackets inside
/// string literals are skipped, or a JSON document containing `"[[[["` would report a
/// depth it does not have.
fn bracket_depth(content: &str) -> usize {
    let mut depth = 0usize;
    let mut max = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for byte in content.bytes() {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' if in_string => escaped = true,
            b'"' => in_string = !in_string,
            b'[' | b'{' if !in_string => {
                depth += 1;
                max = max.max(depth);
            }
            b']' | b'}' if !in_string => depth = depth.saturating_sub(1),
            _ => {}
        }
    }

    max
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits::default()
    }

    // ---- ordinary content passes ----

    #[test]
    fn realistic_payloads_are_not_declined() {
        // The direction that matters most. A guard that trips on real traffic exempts
        // that traffic from compression while looking like it is protecting something.
        let records: Vec<String> = (0..2_000)
            .map(|i| format!(r#"{{"path":"src/module_{i}.rs","size":{i}}}"#))
            .collect();
        let json = format!("[{}]", records.join(","));
        assert_eq!(check(&json, ContentType::Json, limits()), None);

        let log: String = (0..20_000)
            .map(|i| format!("2026-01-01T00:00:00Z INFO worker {i} ok\n"))
            .collect();
        assert_eq!(check(&log, ContentType::Log, limits()), None);

        assert_eq!(check("", ContentType::Prose, limits()), None);
    }

    #[test]
    fn deeply_but_legitimately_nested_json_still_passes() {
        // 20 levels is unusual and legal. The limit is for payloads that are about the
        // stack rather than about data.
        let nested = format!("{}1{}", "[".repeat(20), "]".repeat(20));
        assert_eq!(check(&nested, ContentType::Json, limits()), None);
    }

    // ---- hazards ----

    #[test]
    fn an_oversized_payload_is_declined() {
        let huge = "a".repeat(limits().max_bytes + 1);
        assert!(matches!(
            check(&huge, ContentType::Prose, limits()),
            Some(Hazard::TooLarge { .. })
        ));
    }

    #[test]
    fn pathological_nesting_is_declined_rather_than_walked() {
        // The payload this module exists for: a recursive analyzer would blow the stack
        // on it, and the proxy sits in the path of every request.
        let deep = format!("{}1{}", "[".repeat(500), "]".repeat(500));
        assert!(matches!(
            check(&deep, ContentType::Json, limits()),
            Some(Hazard::TooDeep { .. })
        ));
    }

    #[test]
    fn a_single_enormous_line_is_declined() {
        // A minified bundle or a base64 blob. Line-oriented compressors have nothing to
        // do with it but will still scan every byte looking.
        let one_line = "x".repeat(limits().max_line_bytes + 1);
        assert!(matches!(
            check(&one_line, ContentType::Prose, limits()),
            Some(Hazard::LineTooLong { .. })
        ));
    }

    #[test]
    fn too_many_lines_is_declined() {
        let many = "x\n".repeat(limits().max_lines + 1);
        assert!(matches!(
            check(&many, ContentType::Log, limits()),
            Some(Hazard::TooManyLines { .. })
        ));
    }

    // ---- the depth scanner ----

    #[test]
    fn brackets_inside_strings_do_not_count_toward_depth() {
        // A JSON document containing `"[[[["` as data would otherwise report a depth it
        // does not have, and be declined for being adversarial when it is ordinary.
        let payload = format!(r#"{{"pattern":"{}"}}"#, "[".repeat(500));
        assert_eq!(bracket_depth(&payload), 1);
        assert_eq!(check(&payload, ContentType::Json, limits()), None);
    }

    #[test]
    fn an_escaped_quote_does_not_end_the_string() {
        // `"he said \" [[[["` — a naive scanner sees the escaped quote as a terminator
        // and starts counting the brackets that follow.
        let payload = format!(r#"{{"text":"he said \" {}"}}"#, "[".repeat(500));
        assert_eq!(bracket_depth(&payload), 1);
    }

    #[test]
    fn depth_is_the_maximum_not_the_final_value() {
        // A document that closes everything it opens ends at zero. Measuring the final
        // value would report every payload as flat.
        assert_eq!(bracket_depth("[[[]]]"), 3);
        assert_eq!(bracket_depth("[][][]"), 1);
    }

    #[test]
    fn unbalanced_brackets_do_not_underflow() {
        // More closing than opening. A `usize` subtraction here would panic in debug
        // and wrap in release, reporting a depth of 18 quintillion.
        assert_eq!(bracket_depth("]]]]"), 0);
        assert_eq!(bracket_depth("}}}}"), 0);
    }

    #[test]
    fn depth_is_measured_on_bracket_shaped_content_whatever_detection_said() {
        // The check has to fire on input detection did *not* recognize, because 500
        // nested brackets carrying no data do not classify as JSON — and that is
        // precisely the payload the guard exists for. Gating on `ContentType::Json`
        // alone would skip it on exactly the adversarial case.
        let deep = format!("{}1{}", "[".repeat(500), "]".repeat(500));
        assert!(check(&deep, ContentType::Unknown, limits()).is_some());
        assert!(check(&deep, ContentType::Json, limits()).is_some());
    }

    #[test]
    fn a_log_line_mentioning_a_bracket_is_not_scanned_for_depth() {
        // The common case, and the reason the check is not simply unconditional.
        let log = "2026-01-01 INFO parsed [1, 2, 3]\n".repeat(100);
        assert_eq!(check(&log, ContentType::Log, limits()), None);
    }

    // ---- behavior ----

    #[test]
    fn the_cheapest_check_runs_first() {
        // A payload rejected on size never pays for the depth scan — which matters,
        // because the payloads most likely to be rejected are the most expensive to
        // analyze.
        let huge_and_deep = format!(
            "{}{}{}",
            "[".repeat(500),
            "a".repeat(limits().max_bytes),
            "]".repeat(500)
        );
        assert!(matches!(
            check(&huge_and_deep, ContentType::Json, limits()),
            Some(Hazard::TooLarge { .. })
        ));
    }

    #[test]
    fn checking_is_deterministic() {
        // Invariant I4 — whether compression is attempted must not vary between runs.
        let payload = format!("{}1{}", "[".repeat(500), "]".repeat(500));
        let first = check(&payload, ContentType::Json, limits());
        for _ in 0..25 {
            assert_eq!(check(&payload, ContentType::Json, limits()), first);
        }
    }

    #[test]
    fn every_hazard_has_a_stable_identifier() {
        for hazard in [
            Hazard::TooLarge { bytes: 1 },
            Hazard::TooDeep { depth: 1 },
            Hazard::LineTooLong { bytes: 1 },
            Hazard::TooManyLines { lines: 1 },
        ] {
            assert!(!hazard.as_str().is_empty());
        }
    }

    #[test]
    fn multibyte_content_does_not_panic_the_scanner() {
        // The scanner works on bytes; a UTF-8 payload must not trip it.
        let payload = r#"{"text":"日本語 😀 café [[["}"#;
        assert_eq!(check(payload, ContentType::Json, limits()), None);
    }
}
