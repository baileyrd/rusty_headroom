//! The coverage harness — turning "which compressors are real" into a measured number.
//!
//! `gap-analysis.md` is written by hand. Round 2 of it exists precisely because a
//! hand-maintained table drifted: three rows were marked done on a reading the reference's
//! source does not support, and nothing in CI could have noticed. This crate is the
//! beginning of the fix — the same instinct that produced `scripts/reachability-audit.sh`,
//! one level up.
//!
//! # What this is not
//!
//! **It is not a byte-for-byte comparison against the reference.** The reference is
//! Python-primary and this project is deliberately clean-room; running its implementation
//! to record expected outputs would need a Python environment in CI and would make this
//! repo's output a function of upstream's, which is the opposite of how it was built.
//!
//! **It is not a snapshot of our own output.** That would be a *regression* harness
//! wearing a parity harness's name, and CONTRIBUTING.md already records why: `of(x) ==
//! of(x)` survives any change to `of` intact, because both halves move together. A
//! fixture recorded from the compressor it tests agrees with itself no matter what the
//! compressor does.
//!
//! # What it is
//!
//! Each fixture is realistic content plus the **claim the gap analysis makes about it**:
//! that this content type routes to a named compressor, reaches it through the shared
//! [`Orchestrator`], and comes out smaller. The harness runs every fixture and reports
//! three outcomes — covered, failed, or **skipped with a stated reason**.
//!
//! That last one is the load-bearing part. A harness that silently reports 100% because
//! half its comparators are stubs is worse than no harness: it is the "documented as done"
//! failure with a green checkmark on it. Skips are counted separately and named.
//!
//! # Why it goes through the orchestrator
//!
//! The unit tests construct each compressor directly. That proves the compressor works,
//! not that anything routes to it — the exact gap that let #82 and #84 ship. These
//! fixtures go through `transform_for_block`, the same call the proxy makes, so a
//! compressor that stopped being reachable fails here even while its own tests pass.
//!
//! [`Orchestrator`]: headroom_core::pipeline::Orchestrator

use std::sync::Arc;

use headroom_core::auth_mode::CompressionPolicy;
use headroom_core::block::{Block, BlockKind};
use headroom_core::ccr::InMemoryCcrStore;
use headroom_core::pipeline::Orchestrator;
use headroom_core::tokenizer::HeuristicEstimator;
use headroom_core::validate::validated_apply;
use headroom_core::AuthMode;

/// What a fixture claims about its content.
#[derive(Debug, Clone)]
pub struct Fixture {
    /// The content type the gap analysis says this is.
    pub content_type: &'static str,
    /// The compressor it should reach.
    pub compressor: &'static str,
    /// Realistic content of that type.
    pub content: String,
    /// Why this fixture is not checked, when it is not.
    ///
    /// `Some` means the harness reports it as skipped rather than covered. Stating the
    /// reason is what stops a stub from reading as a pass.
    pub skipped: Option<&'static str>,
}

/// How a fixture turned out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Routed to the expected compressor and came out smaller.
    Covered,
    /// Deliberately not checked, with a reason.
    Skipped(&'static str),
    /// Did not reach the expected compressor, or did not shrink.
    Failed(String),
}

impl Outcome {
    /// Whether this outcome should fail the build.
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

/// The result of running every fixture.
#[derive(Debug, Default)]
pub struct Report {
    /// Per-fixture outcomes, in fixture order.
    pub outcomes: Vec<(&'static str, Outcome)>,
}

impl Report {
    /// How many fixtures were genuinely checked.
    pub fn covered(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|(_, outcome)| *outcome == Outcome::Covered)
            .count()
    }

    /// How many were skipped.
    pub fn skipped(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|(_, outcome)| matches!(outcome, Outcome::Skipped(_)))
            .count()
    }

    /// The failures, if any.
    pub fn failures(&self) -> Vec<&(&'static str, Outcome)> {
        self.outcomes
            .iter()
            .filter(|(_, outcome)| outcome.is_failure())
            .collect()
    }

    /// A human-readable summary.
    ///
    /// Counts skips separately from passes and always prints both, so "covered" can never
    /// be read as "all of them" without looking.
    pub fn summary(&self) -> String {
        let mut out = String::new();
        for (name, outcome) in &self.outcomes {
            let line = match outcome {
                Outcome::Covered => format!("  covered  {name}"),
                Outcome::Skipped(reason) => format!("  SKIPPED  {name} — {reason}"),
                Outcome::Failed(detail) => format!("  FAILED   {name} — {detail}"),
            };
            out.push_str(&line);
            out.push('\n');
        }

        out.push_str(&format!(
            "\n{} of {} content types covered, {} skipped, {} failed\n",
            self.covered(),
            self.outcomes.len(),
            self.skipped(),
            self.failures().len()
        ));
        out
    }
}

/// The fixtures this build carries.
///
/// One per content type the gap analysis claims a compressor for. Content is
/// hand-written to be realistic rather than recorded from anything — see the module docs
/// for why a recorded snapshot would prove nothing.
pub fn fixtures() -> Vec<Fixture> {
    vec![
        Fixture {
            content_type: "json",
            compressor: "smart_crusher",
            content: {
                let records: Vec<String> = (0..200)
                    .map(|i| {
                        format!(
                            r#"{{"path":"src/module_{i}.rs","kind":"file","status":"ok","size":{}}}"#,
                            1000 + i
                        )
                    })
                    .collect();
                format!("[{}]", records.join(","))
            },
            skipped: None,
        },
        Fixture {
            content_type: "logs",
            compressor: "log_compressor",
            content: (0..400)
                .map(|i| {
                    format!("2026-08-05T10:{:02}:{:02}Z INFO  worker={} handled request in {}ms", i / 60 % 60, i % 60, i % 8, 10 + i % 40)
                })
                .collect::<Vec<_>>()
                .join("\n"),
            skipped: None,
        },
        Fixture {
            content_type: "diff",
            compressor: "diff_compressor",
            content: {
                let context: Vec<String> = (0..200)
                    .map(|i| format!(" // unchanged context line {i}"))
                    .collect();
                format!(
                    "diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,205 +1,205 @@\n{}\n-    old_call();\n+    new_call();\n",
                    context.join("\n")
                )
            },
            skipped: None,
        },
        Fixture {
            content_type: "search results",
            compressor: "search_compressor",
            // Several matches per file, which is what `grep -rn` over a real codebase
            // produces. One match per file is a shape `SearchCompressor` declines *by
            // design* — every path is already stated once, so grouping saves nothing and
            // the header is pure overhead. The first version of this fixture had exactly
            // that shape and reported a working compressor as broken.
            content: (0..40)
                .flat_map(|file| {
                    (0..8).map(move |hit| {
                        format!(
                            "src/module_{file}.rs:{}:    let handler = build_handler_{hit}();",
                            hit * 11 + 3
                        )
                    })
                })
                .collect::<Vec<_>>()
                .join("\n"),
            skipped: None,
        },
        Fixture {
            content_type: "code",
            compressor: "code_compressor",
            content: (0..120)
                .map(|i| {
                    format!(
                        "/// Does the {i}th thing.\npub fn thing_{i}(input: &str) -> usize {{\n    let trimmed = input.trim();\n    let count = trimmed.len();\n    count + {i}\n}}\n"
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            skipped: None,
        },
        Fixture {
            content_type: "prose",
            compressor: "text_summarizer",
            content: (0..200)
                .map(|i| format!("The deployment step number {i} completed as expected."))
                .collect::<Vec<_>>()
                .join("\n"),
            skipped: None,
        },
    ]
}

/// Runs every fixture through the shared orchestrator.
///
/// Pay-as-you-go, because that is the mode under which every compressor is permitted —
/// running these under a restricted policy would report the compressors as unreachable
/// when the policy, not the routing, is what declined.
pub fn run(fixtures: &[Fixture]) -> Report {
    let store = Arc::new(InMemoryCcrStore::new());
    let orchestrator = Orchestrator::new(store);
    let policy = CompressionPolicy::for_mode(AuthMode::PayAsYouGo);
    let estimator = HeuristicEstimator::new();

    let mut report = Report::default();

    for fixture in fixtures {
        if let Some(reason) = fixture.skipped {
            report
                .outcomes
                .push((fixture.content_type, Outcome::Skipped(reason)));
            continue;
        }

        // Tool output, because that is what the live zone carries and what the prose
        // gate (D24) requires. A `Text` block would report the prose fixture as unrouted
        // for a reason that has nothing to do with routing.
        let mut block = Block::new(BlockKind::ToolResult, fixture.content.clone());

        // `transform_for_block` — the call the proxy makes. Constructing the compressor
        // directly would prove it works without proving anything routes to it, which is
        // the gap this harness exists to close.
        let Some(transform) = orchestrator.transform_for_block(&block, policy, "") else {
            report.outcomes.push((
                fixture.content_type,
                Outcome::Failed(format!(
                    "nothing routed to it; expected {}",
                    fixture.compressor
                )),
            ));
            continue;
        };

        if transform.name() != fixture.compressor {
            report.outcomes.push((
                fixture.content_type,
                Outcome::Failed(format!(
                    "routed to {} rather than {}",
                    transform.name(),
                    fixture.compressor
                )),
            ));
            continue;
        }

        let before = fixture.content.len();
        match validated_apply(transform, &mut block, &estimator) {
            Ok(outcome) if outcome.is_compressed() => {
                report.outcomes.push((
                    fixture.content_type,
                    if block.content().len() < before {
                        Outcome::Covered
                    } else {
                        Outcome::Failed("reported compressed but did not shrink".to_owned())
                    },
                ));
            }
            Ok(_) => report.outcomes.push((
                fixture.content_type,
                Outcome::Failed(format!("{} declined realistic content", fixture.compressor)),
            )),
            Err(err) => report
                .outcomes
                .push((fixture.content_type, Outcome::Failed(err.to_string()))),
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_content_type_reaches_its_compressor_and_shrinks() {
        // The measured signal that replaces a hand-maintained "Done" column. Each of
        // these is a claim `gap-analysis.md` makes; this is CI checking it.
        let report = run(&fixtures());

        assert!(
            report.failures().is_empty(),
            "coverage regressed:\n{}",
            report.summary()
        );
    }

    #[test]
    fn the_harness_reports_a_routing_failure_rather_than_passing() {
        // The harness has to be able to fail, or the test above is decoration. A fixture
        // claiming the wrong compressor must be reported, not tolerated.
        let wrong = vec![Fixture {
            content_type: "json",
            compressor: "log_compressor",
            content: format!(
                "[{}]",
                (0..200)
                    .map(|i| format!(r#"{{"id":{i},"kind":"file"}}"#))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            skipped: None,
        }];

        let report = run(&wrong);

        assert_eq!(report.failures().len(), 1);
        assert_eq!(report.covered(), 0);
    }

    #[test]
    fn a_skip_is_counted_separately_from_a_pass() {
        // The property that stops a stubbed harness reading as full coverage — the
        // failure mode this crate's own docs call out.
        let skipped = vec![Fixture {
            content_type: "images",
            compressor: "none",
            content: String::new(),
            skipped: Some("out of scope by architecture"),
        }];

        let report = run(&skipped);

        assert_eq!(report.covered(), 0, "a skip counted as coverage");
        assert_eq!(report.skipped(), 1);
        assert!(report.failures().is_empty(), "a skip is not a failure");
        assert!(report.summary().contains("SKIPPED"));
        assert!(report.summary().contains("out of scope by architecture"));
    }

    #[test]
    fn the_summary_always_states_the_denominator() {
        // "6 covered" is a number somebody reads as complete. "6 of 6, 0 skipped" is one
        // they can check.
        let report = run(&fixtures());
        let summary = report.summary();

        assert!(summary.contains(&format!("of {} content types covered", fixtures().len())));
        assert!(summary.contains("skipped"));
        assert!(summary.contains("failed"));
    }

    #[test]
    fn every_fixture_is_realistic_enough_to_clear_its_threshold() {
        // A fixture under `AdaptiveSizer`'s threshold declines for a reason that has
        // nothing to do with routing, and would report a working compressor as broken.
        // #181 shipped three such fixtures before this was noticed there.
        for fixture in fixtures() {
            if fixture.skipped.is_some() {
                continue;
            }
            assert!(
                fixture.content.len() > 5 * 1024,
                "{} is {} bytes, which may be below its threshold",
                fixture.content_type,
                fixture.content.len()
            );
        }
    }
}
