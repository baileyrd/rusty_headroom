//! Tiered line scoring.

use super::keywords::keyword_score;

/// How much a line matters, coarsely.
///
/// A tier rather than a raw number, because callers make a keep/drop decision and a
/// continuous score invites arbitrary thresholds scattered across compressors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Importance {
    /// Ordinary content. First to go when a budget bites.
    Routine,
    /// Structurally notable — a header, a boundary, a summary line.
    Notable,
    /// Reports something going wrong. Kept unless there is no budget at all.
    Critical,
}

/// A line with its assessed importance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoredLine<'a> {
    /// Position in the original input.
    pub index: usize,
    /// The line itself.
    pub text: &'a str,
    /// How much it matters.
    pub importance: Importance,
}

/// Scores every line of `input`.
///
/// Blank lines are retained with `Routine` importance rather than dropped, so indices
/// still correspond to the source. A compressor that renumbered lines here would make
/// every downstream reference off-by-something.
///
/// # Example
///
/// ```
/// use headroom_core::signals::{score_lines, Importance};
///
/// let scored = score_lines("starting up\nERROR: disk full\ndone");
/// assert_eq!(scored[1].importance, Importance::Critical);
/// assert_eq!(scored[0].importance, Importance::Routine);
/// ```
pub fn score_lines(input: &str) -> Vec<ScoredLine<'_>> {
    input
        .lines()
        .enumerate()
        .map(|(index, text)| ScoredLine {
            index,
            text,
            importance: classify(text),
        })
        .collect()
}

/// Assesses one line.
fn classify(line: &str) -> Importance {
    if keyword_score(line) >= 3 {
        return Importance::Critical;
    }

    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Importance::Routine;
    }

    // Structural markers: section headers, separators, and summary lines. These are
    // cheap to keep and disproportionately useful for orienting in truncated output.
    let notable = keyword_score(line) > 0
        || trimmed.starts_with('#')
        || trimmed.starts_with("==")
        || trimmed.starts_with("--")
        || trimmed.ends_with(':')
        || is_summary_line(trimmed);

    if notable {
        Importance::Notable
    } else {
        Importance::Routine
    }
}

/// Lines that report a total or a count, which orient a reader in elided output.
fn is_summary_line(trimmed: &str) -> bool {
    const MARKERS: [&str; 6] = ["total", "summary", "passed", "failed", "skipped", "elapsed"];
    let lowered = trimmed.to_ascii_lowercase();
    MARKERS.iter().any(|marker| lowered.starts_with(marker))
}

/// Keeps the `budget` most important lines, preserving source order.
///
/// Ties break toward the earlier line. Deterministic ordering matters here for the
/// usual reason: a compressor whose output depends on sort instability produces
/// different bytes run to run and busts the provider's prompt cache.
pub fn keep_most_important<'a>(scored: &[ScoredLine<'a>], budget: usize) -> Vec<ScoredLine<'a>> {
    if scored.len() <= budget {
        return scored.to_vec();
    }

    let mut ranked: Vec<&ScoredLine<'a>> = scored.iter().collect();
    ranked.sort_by(|a, b| b.importance.cmp(&a.importance).then(a.index.cmp(&b.index)));
    ranked.truncate(budget);
    ranked.sort_by_key(|line| line.index);
    ranked.into_iter().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_line_is_critical() {
        let scored = score_lines("ok\nERROR: disk full\nok");
        assert_eq!(scored[1].importance, Importance::Critical);
    }

    #[test]
    fn headers_and_summaries_are_notable() {
        for line in [
            "# Section",
            "== Results ==",
            "Total: 42",
            "failed: 3",
            "Note:",
        ] {
            let scored = score_lines(line);
            assert!(
                scored[0].importance >= Importance::Notable,
                "{line} should be at least notable"
            );
        }
    }

    #[test]
    fn ordinary_lines_are_routine() {
        let scored = score_lines("processing item 12 of 400");
        assert_eq!(scored[0].importance, Importance::Routine);
    }

    #[test]
    fn blank_lines_keep_their_index() {
        // Renumbering here would make every downstream line reference wrong.
        let scored = score_lines("first\n\nthird");
        assert_eq!(scored.len(), 3);
        assert_eq!(scored[2].index, 2);
        assert_eq!(scored[2].text, "third");
    }

    #[test]
    fn a_budget_keeps_the_critical_lines_first() {
        let input = "routine one\nroutine two\nERROR: boom\nroutine three\nroutine four";
        let kept = keep_most_important(&score_lines(input), 2);

        assert_eq!(kept.len(), 2);
        assert!(
            kept.iter().any(|l| l.text.contains("ERROR")),
            "the error was dropped: {kept:?}"
        );
    }

    #[test]
    fn kept_lines_stay_in_source_order() {
        let input = "ERROR: a\nroutine\nERROR: b";
        let kept = keep_most_important(&score_lines(input), 2);
        assert!(kept[0].index < kept[1].index);
    }

    #[test]
    fn a_budget_at_or_above_the_line_count_keeps_everything() {
        let scored = score_lines("a\nb\nc");
        assert_eq!(keep_most_important(&scored, 3).len(), 3);
        assert_eq!(keep_most_important(&scored, 99).len(), 3);
    }

    #[test]
    fn a_zero_budget_keeps_nothing_without_panicking() {
        assert!(keep_most_important(&score_lines("a\nb"), 0).is_empty());
    }

    #[test]
    fn selection_is_deterministic_including_ties() {
        // All-routine input means every line ties, so the tie-break is the only thing
        // deciding the result.
        let input = (0..50)
            .map(|i| format!("routine line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let scored = score_lines(&input);
        let first = keep_most_important(&scored, 10);
        for _ in 0..25 {
            assert_eq!(keep_most_important(&scored, 10), first);
        }
        // And the tie-break is "earlier wins", not an arbitrary order.
        assert_eq!(first[0].index, 0);
    }
}
