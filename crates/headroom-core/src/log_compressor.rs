//! Log compression by template extraction.
//!
//! A thousand lines of a service starting up are a handful of distinct *shapes* with
//! different values plugged in. The model needs the shapes, the counts, and the lines
//! that broke the pattern — not a thousand near-identical strings.
//!
//! Each line is normalized into a template by replacing its variable parts with
//! placeholders. Lines sharing a template are counted together and reported once.
//!
//! # The rule that matters most
//!
//! **Error and warning lines are never collapsed away.** This is the entire reason
//! someone is reading a log. A summarizer that reports `×847 INFO request completed`
//! and drops the one `ERROR upstream timeout` has produced output that is smaller,
//! cheaper, and actively harmful: the agent now believes nothing went wrong.
//!
//! Severity lines are preserved verbatim, *in addition to* the template summary
//! rather than as a replacement for it.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::block::Block;
use crate::ccr::{store_and_mark, CcrStore};
use crate::detection::{detect, AdaptiveSizer, ContentType};
use crate::error::{Declined, Error, Result};
use crate::transform::{LossyTransform, Transform};

/// How long an original stays retrievable.
const CCR_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// Tuning for log compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogConfig {
    /// Example lines kept per template.
    ///
    /// One is usually enough: the template already conveys the shape, and the example
    /// only has to show what real values look like.
    pub examples_per_template: usize,

    /// Most severity lines kept verbatim.
    ///
    /// A bound rather than a preference. A log that is *all* errors would otherwise
    /// be reproduced in full, which is not compression — but the bound is high enough
    /// that ordinary failure counts pass through untouched.
    pub max_verbatim_severity: usize,

    /// Fewest lines before compression is attempted.
    pub min_lines: usize,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            examples_per_template: 1,
            max_verbatim_severity: 50,
            min_lines: 8,
        }
    }
}

/// Severity markers that make a line worth keeping verbatim.
///
/// Matched as whole uppercase words. Substring matching would fire on `TERROR` and
/// on the word "information", pulling ordinary lines into the verbatim set and
/// undoing the compression.
const SEVERITY: [&str; 5] = ["ERROR", "FATAL", "PANIC", "WARN", "WARNING"];

/// Normalizes a log line into a template.
///
/// Values become placeholders; words stay words. The distinction is the whole art
/// here: normalize too eagerly and `disk full` collapses into `disk ok`, at which
/// point the summary is worse than useless because it is confidently wrong.
///
/// # Example
///
/// ```
/// use headroom_core::log_compressor::templatize;
///
/// let a = templatize("2026-08-03 10:15:00 INFO request 4821 took 32ms");
/// let b = templatize("2026-08-03 10:15:01 INFO request 4822 took 47ms");
/// assert_eq!(a, b);
/// ```
pub fn templatize(line: &str) -> String {
    let mut out = String::with_capacity(line.len());

    for token in line.split_inclusive(char::is_whitespace) {
        let trimmed = token.trim_end();
        let trailing = &token[trimmed.len()..];

        out.push_str(&normalize_token(trimmed));
        out.push_str(trailing);
    }

    out
}

/// Replaces a single whitespace-delimited token if it looks like a value.
fn normalize_token(token: &str) -> String {
    if token.is_empty() {
        return String::new();
    }

    // Strip surrounding punctuation so `(4821)` and `4821,` normalize the same way as
    // a bare `4821`, while the punctuation itself survives to keep the shape legible.
    let lead: String = token
        .chars()
        .take_while(|c| matches!(c, '(' | '[' | '{' | '"' | '\'' | '<'))
        .collect();
    let tail: String = token
        .chars()
        .rev()
        .take_while(|c| {
            matches!(
                c,
                ')' | ']' | '}' | '"' | '\'' | '>' | ',' | ';' | ':' | '.'
            )
        })
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let core = &token[lead.len()..token.len() - tail.len()];
    if core.is_empty() {
        return token.to_owned();
    }

    let replacement = if is_timestamp(core) {
        Some("<ts>")
    } else if is_uuid(core) {
        Some("<uuid>")
    } else if is_hex(core) {
        Some("<hex>")
    } else if is_numeric_valued(core) {
        // Covers bare numbers and number-with-unit forms like `32ms`, `1.5s`, `80%`.
        Some("<n>")
    } else if is_path_like(core) {
        Some("<path>")
    } else {
        // A word. Left alone — this is what stops `full` and `ok` collapsing together.
        None
    };

    match replacement {
        Some(placeholder) => format!("{lead}{placeholder}{tail}"),
        None => token.to_owned(),
    }
}

/// `2026-08-03`, `10:15:00`, `2026-08-03T10:15:00Z` and similar.
fn is_timestamp(token: &str) -> bool {
    let digits = token.chars().filter(char::is_ascii_digit).count();
    let separators = token
        .chars()
        .filter(|c| matches!(c, '-' | ':' | 'T' | 'Z' | '.' | '+'))
        .count();

    // A timestamp is mostly digits with a couple of separators, and long enough to
    // not be an ordinary hyphenated number.
    digits >= 4
        && separators >= 2
        && token.len() >= 8
        && token
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '-' | ':' | 'T' | 'Z' | '.' | '+'))
}

/// `550e8400-e29b-41d4-a716-446655440000`.
fn is_uuid(token: &str) -> bool {
    let groups: Vec<&str> = token.split('-').collect();
    groups.len() == 5
        && groups.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12])
        && groups
            .iter()
            .all(|g| g.chars().all(|c| c.is_ascii_hexdigit()))
}

/// `0x1f3a`, or a long bare hex run such as a commit SHA.
fn is_hex(token: &str) -> bool {
    let body = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"));
    if let Some(body) = body {
        return !body.is_empty() && body.chars().all(|c| c.is_ascii_hexdigit());
    }
    // A bare run only counts as hex when it is long enough not to be an ordinary
    // word, and contains at least one non-decimal digit so plain numbers fall to the
    // numeric rule instead.
    token.len() >= 12
        && token.chars().all(|c| c.is_ascii_hexdigit())
        && token.chars().any(|c| c.is_ascii_alphabetic())
}

/// A number, optionally with a unit suffix: `42`, `1.5`, `32ms`, `80%`, `-7`.
fn is_numeric_valued(token: &str) -> bool {
    let body = token.strip_prefix(['-', '+']).unwrap_or(token);
    let digits_end = body
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(body.len());

    let (number, unit) = body.split_at(digits_end);
    if number.is_empty() || !number.chars().any(|c| c.is_ascii_digit()) {
        return false;
    }

    // Either a bare number, or a number with a short alphabetic or `%` unit. The
    // length bound stops `404handler` from reading as a value.
    unit.is_empty()
        || unit == "%"
        || (unit.len() <= 3 && unit.chars().all(|c| c.is_ascii_alphabetic()))
}

/// A filesystem-ish path.
fn is_path_like(token: &str) -> bool {
    (token.contains('/') || token.contains('\\'))
        && token.len() > 1
        && !token.starts_with("//")
        && token.chars().any(|c| c.is_ascii_alphanumeric())
}

/// Whether a line carries an error or warning severity marker.
pub fn has_severity(line: &str) -> bool {
    SEVERITY.iter().any(|level| {
        line.match_indices(level).any(|(idx, _)| {
            let bytes = line.as_bytes();
            let before_ok =
                idx == 0 || (!bytes[idx - 1].is_ascii_alphanumeric() && bytes[idx - 1] != b'_');
            let after = idx + level.len();
            let after_ok = after >= line.len()
                || (!bytes[after].is_ascii_alphanumeric() && bytes[after] != b'_');
            before_ok && after_ok
        })
    })
}

/// Compresses log output by collapsing repeated line shapes.
pub struct LogCompressor {
    config: LogConfig,
    store: Arc<dyn CcrStore>,
    sizer: AdaptiveSizer,
}

impl LogCompressor {
    /// Creates a compressor backed by `store`.
    pub fn new(store: Arc<dyn CcrStore>) -> Self {
        Self {
            config: LogConfig::default(),
            store,
            sizer: AdaptiveSizer::default(),
        }
    }

    /// Overrides the configuration.
    pub fn with_config(mut self, config: LogConfig) -> Self {
        self.config = config;
        self
    }

    /// Compresses `source`, or explains why it declined.
    pub fn compress(&self, source: &str) -> Result<String> {
        if detect(source.as_bytes()).content_type != ContentType::Log {
            return Err(Error::declined(Declined::WrongContentType));
        }
        if !self.sizer.should_attempt(ContentType::Log, source.len()) {
            return Err(Error::declined(Declined::BelowThreshold));
        }

        let lines: Vec<&str> = source.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.len() < self.config.min_lines {
            return Err(Error::declined(Declined::BelowThreshold));
        }

        // Templates keyed for lookup, but ordering comes from `order` — first
        // appearance, not the map's collation. A BTreeMap would otherwise sort
        // templates alphabetically and scramble the log's narrative.
        let mut groups: BTreeMap<String, (usize, Vec<&str>)> = BTreeMap::new();
        let mut order: Vec<String> = Vec::new();
        let mut severity_lines: Vec<&str> = Vec::new();

        for line in &lines {
            if has_severity(line) && severity_lines.len() < self.config.max_verbatim_severity {
                severity_lines.push(line);
            }

            let template = templatize(line);
            let entry = groups.entry(template.clone()).or_insert_with(|| {
                order.push(template);
                (0, Vec::new())
            });
            entry.0 += 1;
            if entry.1.len() < self.config.examples_per_template {
                entry.1.push(line);
            }
        }

        // Nothing repeated means nothing to collapse — the "summary" would be the log
        // plus a header.
        if order.len() == lines.len() {
            return Err(Error::declined(Declined::NotSmaller));
        }

        let marker = store_and_mark(self.store.as_ref(), source.as_bytes(), CCR_TTL)?;

        let mut out = format!("[{} lines, {} patterns]\n", lines.len(), order.len());

        for template in &order {
            let (count, examples) = &groups[template];
            out.push_str(&format!("x{count} {template}\n"));
            for example in examples {
                // Only worth showing when it differs from the template itself.
                if example != template {
                    out.push_str(&format!("     e.g. {example}\n"));
                }
            }
        }

        if !severity_lines.is_empty() {
            out.push_str(&format!(
                "\nerrors and warnings ({}):\n",
                severity_lines.len()
            ));
            for line in &severity_lines {
                out.push_str(&format!("  {line}\n"));
            }
        }

        out.push_str(&format!("full content: {marker}\n"));
        Ok(out)
    }
}

impl Transform for LogCompressor {
    fn name(&self) -> &'static str {
        "log_compressor"
    }

    fn apply(&self, block: &mut Block) -> Result<()> {
        let compressed = self.compress(block.content())?;
        block.replace_content(compressed);
        Ok(())
    }
}

impl LossyTransform for LogCompressor {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ccr::{parse_marker, InMemoryCcrStore};
    use crate::tokenizer::{HeuristicEstimator, Tokenizer};

    fn compressor() -> (LogCompressor, Arc<InMemoryCcrStore>) {
        let store = Arc::new(InMemoryCcrStore::new());
        (LogCompressor::new(store.clone()), store)
    }

    fn service_log(lines: usize) -> String {
        (0..lines)
            .map(|i| {
                format!(
                    "2026-08-03 10:{:02}:{:02} INFO request {} completed in {}ms",
                    i / 60 % 60,
                    i % 60,
                    4000 + i,
                    20 + i % 40
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ---- templating ----

    #[test]
    fn lines_differing_only_in_values_share_a_template() {
        assert_eq!(
            templatize("2026-08-03 10:15:00 INFO request 4821 took 32ms"),
            templatize("2026-08-03 11:02:59 INFO request 9999 took 7ms")
        );
    }

    #[test]
    fn words_are_not_normalized_away() {
        // The over-normalization failure. If `full` and `ok` collapsed, the summary
        // would be confidently wrong about the state of the disk.
        assert_ne!(templatize("disk full"), templatize("disk ok"));
        assert_ne!(
            templatize("connection established"),
            templatize("connection refused")
        );
    }

    #[test]
    fn values_of_each_recognized_kind_normalize() {
        assert!(templatize("took 32ms").contains("<n>"));
        assert!(templatize("at 2026-08-03T10:15:00Z").contains("<ts>"));
        assert!(templatize("id 550e8400-e29b-41d4-a716-446655440000").contains("<uuid>"));
        assert!(templatize("addr 0x1f3a2b").contains("<hex>"));
        assert!(templatize("reading src/main.rs").contains("<path>"));
        assert!(templatize("usage 80%").contains("<n>"));
    }

    #[test]
    fn punctuation_around_a_value_survives() {
        // `(4821)` and `(9999)` share a template, and the parentheses remain so the
        // shape is still legible.
        let a = templatize("request (4821) done");
        assert_eq!(a, templatize("request (9999) done"));
        assert!(a.contains("(<n>)"), "{a}");
    }

    #[test]
    fn an_identifier_containing_digits_is_not_a_number() {
        // `404handler` is a name, not a value.
        assert!(!templatize("calling 404handler now").contains("<n>"));
    }

    #[test]
    fn templating_is_deterministic() {
        let line = "2026-08-03 10:15:00 INFO request 4821 took 32ms from src/a.rs";
        let first = templatize(line);
        for _ in 0..50 {
            assert_eq!(templatize(line), first);
        }
    }

    // ---- severity ----

    #[test]
    fn severity_is_matched_as_whole_words() {
        assert!(has_severity("2026-01-01 ERROR boom"));
        assert!(has_severity("WARN retrying"));
        assert!(!has_severity("TERROR is not a level"));
        assert!(!has_severity("information about the run"));
    }

    #[test]
    fn the_one_error_among_a_thousand_lines_survives_verbatim() {
        // The rule the whole module is built around. Reporting "x1000 INFO ok" and
        // dropping this line would leave the agent believing nothing went wrong.
        let (compressor, _store) = compressor();
        let mut source = service_log(1000);
        source.push_str("\n2026-08-03 11:00:00 ERROR upstream timeout after 30000ms");

        let compressed = compressor.compress(&source).expect("should compress");

        assert!(
            compressed.contains("ERROR upstream timeout after 30000ms"),
            "the error line was collapsed away:\n{compressed}"
        );
    }

    #[test]
    fn severity_lines_appear_in_addition_to_the_summary() {
        let (compressor, _store) = compressor();
        let mut source = service_log(200);
        source.push_str("\n2026-08-03 11:00:00 ERROR disk full");

        let compressed = compressor.compress(&source).unwrap();
        assert!(
            compressed.contains("errors and warnings (1)"),
            "{compressed}"
        );
        assert!(compressed.contains("[201 lines"), "{compressed}");
    }

    // ---- compression ----

    #[test]
    fn a_realistic_log_gets_measurably_smaller() {
        let (compressor, _store) = compressor();
        let source = service_log(500);
        let compressed = compressor.compress(&source).expect("should compress");

        let estimator = HeuristicEstimator::new();
        let before = estimator.count(&source);
        let after = estimator.count(&compressed);
        assert!(
            after < before / 4,
            "expected a large cut, {before} -> {after}"
        );
    }

    #[test]
    fn the_original_is_retrievable() {
        let (compressor, store) = compressor();
        let source = service_log(300);
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

    #[test]
    fn patterns_are_reported_in_first_appearance_order() {
        // Alphabetical ordering would scramble the log's narrative.
        let (compressor, _store) = compressor();
        let mut lines: Vec<String> = (0..20)
            .map(|i| format!("2026-08-03 10:00:{i:02} INFO zulu step {i}"))
            .collect();
        lines.extend((0..20).map(|i| format!("2026-08-03 10:01:{i:02} INFO alpha step {i}")));
        let compressed = compressor.compress(&lines.join("\n")).unwrap();

        let zulu = compressed.find("zulu").expect("zulu present");
        let alpha = compressed.find("alpha").expect("alpha present");
        assert!(zulu < alpha, "first-seen pattern should come first");
    }

    // ---- declining ----

    #[test]
    fn non_log_content_is_declined() {
        let (compressor, _store) = compressor();
        let err = compressor
            .compress(&r#"{"a":1,"b":2}"#.repeat(200))
            .unwrap_err();
        assert!(err.is_recoverable());
    }

    #[test]
    fn a_log_with_no_repetition_is_declined() {
        // Every line its own template means the "summary" is the log plus a header.
        let (compressor, _store) = compressor();
        let source: String = (0..40)
            .map(|i| {
                format!(
                    "2026-08-03 10:00:00 INFO distinct message number {}\n",
                    "x".repeat(i + 1)
                )
            })
            .collect();
        let err = compressor.compress(&source).unwrap_err();
        assert!(matches!(err, Error::Declined(_)));
    }

    #[test]
    fn short_input_is_declined_and_stores_nothing() {
        let (compressor, store) = compressor();
        assert!(compressor.compress("2026-08-03 10:00:00 INFO up").is_err());
        assert!(store.is_empty());
    }

    #[test]
    fn compression_is_deterministic() {
        let source = service_log(200);
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
