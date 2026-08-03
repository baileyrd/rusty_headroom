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

    keep_with_required(scored, budget, &[])
}

/// Keeps the `budget` most important lines, plus every line in `required`.
///
/// `required` is the union of the structural keep-sets — [`select_anchors`] for lines
/// whose removal changes what the rest of the output *means*, and [`protected_lines`]
/// for lines whose removal leaves markup unbalanced. It must be ascending; duplicates
/// are tolerated. Both producers already return it that way.
///
/// # The budget is a target and the required set is a floor
///
/// When the two conflict, the required set wins and the output exceeds the budget.
/// That is the whole point: an anchor dropped to satisfy a line count leaves content
/// whose meaning depended on it, and nothing downstream can tell — the remainder reads
/// as though it were always complete. Invariant I5 is what makes overshooting safe,
/// because a result that ends up no smaller in tokens is discarded rather than sent.
///
/// # Determinism
///
/// Same ranking rule as [`keep_most_important`]: importance descending, then source
/// index ascending. Ties break toward the earlier line, and the result is emitted in
/// source order. A compressor whose output depended on sort instability would produce
/// different bytes run to run and bust the provider's prompt cache (invariant I4).
///
/// [`select_anchors`]: super::select_anchors
/// [`protected_lines`]: super::protected_lines
pub fn keep_with_required<'a>(
    scored: &[ScoredLine<'a>],
    budget: usize,
    required: &[usize],
) -> Vec<ScoredLine<'a>> {
    if scored.len() <= budget {
        return scored.to_vec();
    }

    let mut ranked: Vec<&ScoredLine<'a>> = scored.iter().collect();
    // Required lines sort ahead of everything, so they survive the truncation below
    // regardless of what the importance heuristic made of them. A `<result>` delimiter
    // scores as routine prose and is load-bearing anyway.
    ranked.sort_by(|a, b| {
        let a_required = required.binary_search(&a.index).is_ok();
        let b_required = required.binary_search(&b.index).is_ok();
        b_required
            .cmp(&a_required)
            .then(b.importance.cmp(&a.importance))
            .then(a.index.cmp(&b.index))
    });

    // `max` rather than a plain truncate: the required set is a floor, so a block with
    // more required lines than budget keeps them all rather than dropping the ones that
    // happened to sort last.
    let required_present = ranked
        .iter()
        .filter(|line| required.binary_search(&line.index).is_ok())
        .count();
    ranked.truncate(budget.max(required_present));

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
    fn a_required_line_survives_a_budget_it_did_not_earn() {
        // The reason this function exists. A `</result>` delimiter scores as routine
        // prose and is load-bearing anyway; without the required set it is precisely
        // what a tight budget discards.
        let input = "routine one\nroutine two\n</result>\nroutine three\nroutine four";
        let kept = keep_with_required(&score_lines(input), 2, &[2]);

        assert!(
            kept.iter().any(|line| line.text == "</result>"),
            "the required line was dropped: {kept:?}"
        );
    }

    #[test]
    fn the_required_set_is_a_floor_rather_than_a_suggestion() {
        // More required lines than budget. Dropping the ones that happened to sort last
        // would leave content whose meaning depended on them, and nothing downstream can
        // tell — the remainder reads as though it were always complete. Invariant I5 is
        // what makes overshooting safe: a result no smaller in tokens is discarded.
        let input = "a\nb\nc\nd\ne\nf";
        let kept = keep_with_required(&score_lines(input), 1, &[0, 2, 4]);

        let indices: Vec<usize> = kept.iter().map(|line| line.index).collect();
        assert!(
            [0, 2, 4].iter().all(|index| indices.contains(index)),
            "a required line was dropped to fit the budget: {indices:?}"
        );
    }

    #[test]
    fn an_empty_required_set_matches_the_plain_budget() {
        // Otherwise the two entry points would drift and the same content would compress
        // differently depending on which one a compressor happened to call.
        let input = "routine one\nERROR: boom\nroutine two\nroutine three";
        let scored = score_lines(input);

        assert_eq!(
            keep_with_required(&scored, 2, &[]),
            keep_most_important(&scored, 2)
        );
    }

    #[test]
    fn required_selection_is_deterministic() {
        // Invariant I4. The sort has three keys and a stable result matters: output that
        // varied run to run would bust the provider's prompt cache.
        let input = "a\nERROR: b\nc\nd\ne\nf\ng";
        let first = keep_with_required(&score_lines(input), 3, &[0, 5]);

        for _ in 0..25 {
            assert_eq!(keep_with_required(&score_lines(input), 3, &[0, 5]), first);
        }
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
