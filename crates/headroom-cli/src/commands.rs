//! Command implementations.
//!
//! Every command that produces data writes it to stdout and nothing else, so
//! `headroom compress < big.json > small.txt` works. Diagnostics go to stderr.

use anyhow::Context as _;
use std::io::{Read, Write};
use std::sync::Arc;

use headroom_core::auth_mode::{AuthMode, CompressionPolicy};
use headroom_core::ccr::{CcrStore, InMemoryCcrStore};
use headroom_core::detection::{detect, AdaptiveSizer, ContentType};
use headroom_core::pipeline::reformats::Reformatter;
use headroom_core::pipeline::Orchestrator;
use headroom_core::telemetry::{AggregationKey, Aggregator, StructureHash, Telemetry};
use headroom_core::tokenizer::{HeuristicEstimator, Tokenizer};
use headroom_core::{validated_apply, Block, BlockKind};

/// Reads all of stdin.
fn read_stdin() -> anyhow::Result<String> {
    let mut buffer = String::new();
    std::io::stdin().read_to_string(&mut buffer)?;
    Ok(buffer)
}

/// `headroom compress`.
pub fn compress(dry_run: bool, kind: BlockKind) -> anyhow::Result<()> {
    let content = read_stdin()?;

    // Routed through the same `Orchestrator` the proxy uses. This command used to carry
    // its own routing table, and the two drifted: the orchestrator had no code arm, so
    // `--dry-run` reported a saving on source files that the proxy would never deliver.
    // A prediction that disagrees with the thing it predicts is worse than no prediction.
    let orchestrator = Orchestrator::new(Arc::new(InMemoryCcrStore::new()));

    // Pay-as-you-go: this is the operator compressing their own content on their own
    // machine, not a customer's credential deciding what is permitted. The proxy applies
    // the real policy to real traffic.
    let policy = CompressionPolicy::for_mode(AuthMode::PayAsYouGo);
    let model = "";

    let estimator = HeuristicEstimator::new();
    let before = estimator.count(&content);
    let detection = detect(content.as_bytes());

    let mut block = Block::new(kind, content.clone());
    // `transform_for_block`, not `transform_for`. This command used to build a text block
    // and then ask a question that ignores block kind, so it summarized prose that
    // `headroom inspect` — in this same binary, a few lines down — correctly reports as
    // declined. The store here dies with the process, so that summary was unrecoverable.
    let compressed = match orchestrator.transform_for_block(&block, policy, model) {
        Some(transform) => validated_apply(transform, &mut block, &estimator)
            .map(|outcome| outcome.is_compressed())
            .unwrap_or(false),
        None => false,
    };

    let after = estimator.count(block.content());

    if dry_run {
        // The report goes to stdout because in a dry run the report *is* the output.
        println!("content type: {}", detection.content_type);
        println!("tokens before: {before}");
        if compressed {
            let saved = before.saturating_sub(after);
            let percent = (saved * 100).checked_div(before).unwrap_or(0);
            println!("tokens after: {after}");
            println!("would save: {saved} ({percent}%)");
        } else {
            println!("would not compress: no transform improved this content");
        }
        return Ok(());
    }

    // Data to stdout, unadorned, so this composes in a pipeline.
    let mut stdout = std::io::stdout();
    stdout.write_all(block.content().as_bytes())?;
    stdout.flush()?;
    Ok(())
}

/// What `headroom inspect` would report about `content`, one line per line of output.
///
/// Separated from the printing so it can be asserted on. The bug this function exists to
/// prevent is a *disagreement* between what this command claims and what the pipeline
/// does, and a test that cannot read the claim cannot check it.
///
/// # Asked of the orchestrator, never restated here
///
/// This used to be a hand-written copy of the routing table inside `inspect`, and it was
/// already wrong: it mapped prose to "none", which stopped being true the moment the
/// prose compressors were wired into the request path. The same 18 KB of prose got
/// `compressor: none` from `headroom inspect` and `would save: 5205 (70%)` from
/// `headroom compress` in the same shell — the command built to explain routing was the
/// one contradicting it. That is D23 surviving in the last place it should.
///
/// So nothing below decides anything. Every routing line comes from
/// [`Orchestrator::route`] and [`Orchestrator::transform_for_block`], the two functions
/// the proxy itself calls.
fn inspect_report(content: &str) -> Vec<String> {
    let detection = detect(content.as_bytes());
    let sizer = AdaptiveSizer::default();
    let estimator = HeuristicEstimator::new();
    let above_threshold = sizer.should_attempt(detection.content_type, content.len());

    let mut lines = vec![
        format!("bytes: {}", content.len()),
        format!("estimated tokens: {}", estimator.count(content)),
        format!("content type: {}", detection.content_type),
        format!("confidence: {:.2}", detection.confidence),
        format!(
            "size threshold: {} bytes",
            sizer.threshold(detection.content_type)
        ),
        format!("above threshold: {above_threshold}"),
    ];

    let orchestrator = Orchestrator::new(Arc::new(InMemoryCcrStore::new()));

    // The model only matters for the recommendations lookup, and `headroom inspect` reads
    // no recommendations file — an empty model is the "nothing measured about this shape"
    // case, which is what an unconfigured inspection should report.
    let model = "";

    // # Two dimensions, because two things change the answer
    //
    // The credential (I10): an API key gets lossy work, an OAuth token lossless only, a
    // subscription token nothing. And the block kind (D24): prose is compressed from tool
    // output and never from what a person typed. Reporting one compressor would mean
    // picking a credential and a block kind on the operator's behalf without saying so —
    // which is how this command came to be wrong in the first place.
    lines.push("routing:".to_string());
    for (label, mode) in [
        ("api key", AuthMode::PayAsYouGo),
        ("oauth", AuthMode::OAuth),
        ("subscription", AuthMode::Subscription),
    ] {
        let policy = CompressionPolicy::for_mode(mode);
        let routing = orchestrator.route(content, policy, model);

        // The reason is `Routing::as_str` — the same identifier the proxy reports under
        // `headroom_routing_total{reason=...}`, so a `measured_useless` seen on a
        // dashboard and a `measured_useless` seen here are the same fact.
        lines.push(format!("  {label}: {}", routing.as_str()));

        for (kind_label, kind) in [
            ("as tool output", BlockKind::ToolResult),
            ("as a typed message", BlockKind::Text),
        ] {
            let block = Block::new(kind, content.to_string());

            // `route` does not consult the size threshold — each lossy compressor holds
            // its own `AdaptiveSizer` and declines below it. So on a short payload this
            // names a compressor that will be offered the content and do nothing with
            // it, which reads as "this compresses" to anyone who did not also read the
            // threshold line six lines up. Said here, where it is being read.
            //
            // Only when a lossy compressor was actually named: there is nothing for the
            // note to qualify on a block that is forwarded anyway, and `Reformatter` has
            // no sizer, so the threshold does not gate it either.
            let named = match orchestrator.transform_for_block(&block, policy, model) {
                None => "forwarded unchanged".to_string(),
                Some(transform) if routing.is_lossy() && !above_threshold => format!(
                    "{} (below the size threshold — it will decline)",
                    transform.name()
                ),
                Some(transform) => transform.name().to_string(),
            };
            lines.push(format!("    {kind_label}: {named}"));
        }
    }
    lines
}

/// `headroom inspect`.
pub fn inspect() -> anyhow::Result<()> {
    let content = read_stdin()?;
    for line in inspect_report(&content) {
        println!("{line}");
    }
    Ok(())
}

/// One sample per content type that reaches a compressor.
///
/// Shared by `headroom doctor` and `headroom perf`, which ask two questions about the
/// same four compressors — does each one work, and how fast is each one. Two sample sets
/// would let those answers describe different content, so a compressor could pass the
/// self-test on one payload and be benchmarked on another.
///
/// The samples are chosen so a failure means something. Each note below records a case
/// where a badly chosen one made a passing check prove nothing.
fn self_test_samples() -> Vec<(&'static str, String)> {
    vec![
        (
            "json",
            format!(
                "[{}]",
                (0..80)
                    .map(|i| format!(r#"{{"id":{i},"kind":"file","ok":true}}"#))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        ),
        (
            "log",
            (0..200)
                .map(|i| format!("2026-08-03T12:00:00Z INFO worker {i} handled a request"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        (
            // Distinct functions rather than one repeated. A skeletonizer given the same
            // body eighty times has nothing to remove that is not already redundant, so
            // the sample would fail for a reason that says nothing about the install.
            "code",
            (0..60)
                .map(|i| {
                    format!(
                        "/// Handles request number {i}.\n\
                         pub fn handle_{i}(input: &str) -> Result<String, Error> {{\n\
                         \x20   let parsed = parse_{i}(input)?;\n\
                         \x20   let checked = validate(&parsed)?;\n\
                         \x20   Ok(render(&checked))\n\
                         }}\n"
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        (
            // Lines, not one long string. The summarizer works on a line budget, so a
            // single 24 KB line is one line and is left alone — which would read as a
            // broken install rather than as a badly chosen sample.
            "prose",
            (0..300)
                .map(|i| format!("The quick brown fox jumps over the lazy dog, sentence {i}."))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        (
            // Added because `the_self_test_samples_cover_every_compressor` found it
            // missing: `headroom doctor` printed "all checks passed" having never run the
            // diff compressor, and `headroom perf` never timed it.
            "diff",
            (0..12)
                .map(|i| {
                    // Long runs of unchanged context around one change, because eliding
                    // context beyond two lines either side is the entire transform. The
                    // first version of this sample gave each hunk three context lines,
                    // all of them within the keep window, so nothing was dropped and
                    // `doctor` reported a broken compressor that was working correctly.
                    let context: String = (0..14)
                        .map(|line| format!(" \x20   let step_{line} = stage_{line}(&state);\n"))
                        .collect();
                    format!(
                        "diff --git a/src/module_{i}.rs b/src/module_{i}.rs\n\
                         --- a/src/module_{i}.rs\n\
                         +++ b/src/module_{i}.rs\n\
                         @@ -1,32 +1,32 @@\n\
                         {context}\
                         -    let parsed = parse_old(input)?;\n\
                         +    let parsed = parse_new(input)?;\n\
                         {context}"
                    )
                })
                .collect::<Vec<_>>()
                .join(""),
        ),
        (
            // Missing for the same reason as the diff sample above.
            "search",
            (0..120)
                .map(|i| {
                    format!(
                        "src/module_{}.rs:{}:    let parsed = parse(input)?;",
                        i % 12,
                        i + 1
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    ]
}

/// `headroom doctor`.
pub fn doctor() -> anyhow::Result<()> {
    let mut problems = 0;

    println!("headroom {}", env!("CARGO_PKG_VERSION"));

    // A self-test rather than a version dump. Reporting "installed correctly" without
    // exercising anything is how a broken install passes its own health check.
    //
    // # Routed through the orchestrator, not a compressor picked here
    //
    // This used to call `SmartCrusher` directly. That checks that *a* compressor works,
    // which is not the question an operator is asking — they want to know whether the
    // proxy will compress their traffic. The two came apart badly: the orchestrator had
    // no arm for source code or prose for the whole life of the pipeline refactor, so the
    // proxy forwarded both whole while this command reported "compression: ok".
    //
    // Going through `Orchestrator` means a routing gap shows up here, which is where
    // somebody is looking when they suspect one.
    let store = Arc::new(InMemoryCcrStore::new());
    let orchestrator = Orchestrator::new(store.clone());
    let policy = CompressionPolicy::for_mode(AuthMode::PayAsYouGo);
    let estimator = HeuristicEstimator::new();

    let samples = self_test_samples();

    // Kept for the retrieval check below: whichever sample compressed first, and what it
    // originally said.
    let mut retrievable: Option<(String, String)> = None;

    for (label, sample) in &samples {
        let detected = detect(sample.as_bytes()).content_type;

        let Some(transform) = orchestrator.transform_for(sample, policy, "") else {
            println!("compression ({label}): FAILED (detected {detected}, reached no compressor)");
            problems += 1;
            continue;
        };

        let mut block = Block::new(BlockKind::Text, sample.clone());
        match validated_apply(transform, &mut block, &estimator) {
            Ok(outcome) if outcome.is_compressed() => {
                println!(
                    "compression ({label}): ok ({} tokens saved via {})",
                    outcome.tokens_saved(),
                    transform.name()
                );
                if retrievable.is_none() {
                    retrievable = Some((block.content().to_owned(), sample.clone()));
                }
            }
            Ok(_) => {
                println!("compression ({label}): FAILED (sample did not compress)");
                problems += 1;
            }
            Err(err) => {
                println!("compression ({label}): FAILED ({err})");
                problems += 1;
            }
        }
    }

    // Retrieval is the half that makes lossy compression safe, so a doctor that only
    // checked compression would pass on an install where nothing is recoverable.
    match retrievable {
        Some((compressed, original)) => {
            match headroom_core::ccr::find_markers(&compressed).first() {
                Some(hash) => match store.get(*hash) {
                    Ok(Some(bytes)) if bytes == original.as_bytes() => println!("retrieval: ok"),
                    Ok(Some(_)) => {
                        println!("retrieval: FAILED (content did not round-trip)");
                        problems += 1;
                    }
                    _ => {
                        println!("retrieval: FAILED (marker resolved to nothing)");
                        problems += 1;
                    }
                },
                None => {
                    println!("retrieval: FAILED (no marker emitted)");
                    problems += 1;
                }
            }
        }
        None => {
            println!("retrieval: FAILED (nothing compressed, so retrieval was untested)");
            problems += 1;
        }
    }

    if problems == 0 {
        println!("\nall checks passed");
        Ok(())
    } else {
        anyhow::bail!("{problems} check(s) failed")
    }
}

/// Every variable any supported agent needs, deduplicated, in first-seen order.
///
/// # Not written out again
///
/// `headroom env` printed `ANTHROPIC_BASE_URL={proxy}` and `OPENAI_BASE_URL={proxy}/v1`
/// as format strings, restating what `Agent::env` already knows. It had drifted in the
/// small way a copy does: `Agent::env` trims a trailing slash and this did not, so the
/// same input produced different output from two commands that do the same job —
///
/// ```text
/// $ headroom env  --proxy http://127.0.0.1:8787/
/// export OPENAI_BASE_URL=http://127.0.0.1:8787//v1
/// $ headroom wrap aider --proxy http://127.0.0.1:8787/
/// export OPENAI_BASE_URL=http://127.0.0.1:8787/v1
/// ```
///
/// `wrap` has a test for that exact input. `env` did not, because it did not share the
/// code the test covers.
///
/// The union rather than one representative agent: `headroom env` names no agent, so its
/// answer is "whatever any of them might read". An agent added with a new variable then
/// appears here without anyone remembering to come back.
fn agent_env(proxy: &str) -> Vec<(&'static str, String)> {
    let mut vars: Vec<(&'static str, String)> = Vec::new();

    for agent in crate::wrap::Agent::ALL {
        for (name, value) in agent.env(proxy) {
            match vars.iter().find(|(known, _)| *known == name) {
                // Two agents disagreeing on a variable would make a single `eval` wrong
                // for one of them, and silently picking the first is how it would stay
                // unnoticed. Nothing disagrees today; `agents_do_not_disagree_about_a_variable`
                // is what keeps that true.
                Some((_, first)) => debug_assert_eq!(
                    *first, value,
                    "agents disagree about {name}: `{first}` and `{value}`"
                ),
                None => vars.push((name, value)),
            }
        }
    }
    vars
}

/// `headroom env`.
pub fn env(proxy: &str) -> anyhow::Result<()> {
    // Emitted as shell exports so this can be `eval`'d, which is how the reference's
    // wrap command is meant to be used.
    for (name, value) in agent_env(proxy) {
        println!("export {name}={value}");
    }
    println!("# eval \"$(headroom env)\" then run your agent as usual");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The orchestrator this command routes through, as `compress` builds it.
    fn orchestrator() -> Orchestrator {
        Orchestrator::new(Arc::new(InMemoryCcrStore::new()))
    }

    fn payg() -> CompressionPolicy {
        CompressionPolicy::for_mode(AuthMode::PayAsYouGo)
    }

    #[test]
    fn routing_matches_the_detected_type() {
        let json = r#"[{"a":1},{"a":2}]"#;
        assert_eq!(
            orchestrator()
                .transform_for(json, payg(), "")
                .map(|t| t.name()),
            Some("smart_crusher")
        );

        // Prose routes to the text compressor here, and deliberately does *not* on the
        // proxy's path unless the block is tool output — `transform_for_block` draws that
        // line. A caller piping prose into this command has asked for it to be
        // compressed; a user typing a message has not.
        let prose = "just some ordinary words with nothing structural about them";
        assert_eq!(
            orchestrator()
                .transform_for(prose, payg(), "")
                .map(|t| t.name()),
            Some("text_summarizer")
        );

        // And it still declines at apply time, because it is far below the size
        // threshold — routing is not the same as compressing.
        let mut block = Block::new(BlockKind::Text, prose);
        assert!(validated_apply(
            orchestrator().transform_for(prose, payg(), "").unwrap(),
            &mut block,
            &HeuristicEstimator::new()
        )
        .map(|outcome| !outcome.is_compressed())
        .unwrap_or(true));
    }

    #[test]
    fn code_routes_to_a_compressor_here_exactly_as_it_does_in_the_proxy() {
        // This command exists to predict what the proxy will do. It used to carry its
        // own routing table, which had a code arm the orchestrator did not — so
        // `--dry-run` reported a saving on source files that the proxy never delivered.
        // A prediction that disagrees with the thing it predicts is worse than none.
        let code = concat!(
            "pub fn handle(input: &str) -> Result<String, Error> {\n",
            "    let parsed = parse(input)?;\n",
            "    Ok(render(&parsed))\n",
            "}\n"
        )
        .repeat(80);

        assert_eq!(
            detect(code.as_bytes()).content_type,
            ContentType::Code,
            "the fixture is not detected as code, so this proves nothing"
        );
        assert_eq!(
            orchestrator()
                .transform_for(&code, payg(), "")
                .map(|t| t.name()),
            Some("code_compressor"),
            "code reached no compressor"
        );
    }

    /// Prose bulky and repetitive enough that the summarizer genuinely shrinks it.
    ///
    /// Not one long line, and not one line repeated: the summarizer works on a line
    /// budget and keeps what scores as notable, so uniform content gives it nothing to
    /// drop and would make a passing test prove the opposite of what it claims.
    #[test]
    fn compress_declines_typed_prose_and_inspect_agrees() {
        // `headroom compress` used to build a text block and then route with a call that
        // ignores block kind, so it summarized prose that `headroom inspect` — in this
        // same binary — reports as declined. Two commands, one build, two answers about
        // the same bytes. The store here dies with the process, so the summary was not
        // recoverable either.
        let prose = compressible_prose();
        let orchestrator = orchestrator();

        let as_tool_output = Block::new(BlockKind::ToolResult, prose.clone());
        let as_text = Block::new(BlockKind::Text, prose.clone());

        // The control: this content must reach a compressor as tool output, or declining
        // it as text says nothing about the gate.
        assert!(
            orchestrator
                .transform_for_block(&as_tool_output, payg(), "")
                .is_some(),
            "the sample does not compress at all, so this proves nothing"
        );
        assert!(
            orchestrator
                .transform_for_block(&as_text, payg(), "")
                .is_none(),
            "typed prose reached the summarizer"
        );
    }

    fn compressible_prose() -> String {
        (0..220)
            .map(|i| match i % 37 {
                0 => {
                    format!("ERROR: batch {i} failed validation: checksum mismatch, retry queued.")
                }
                _ => "The worker acknowledged the message and kept polling without incident."
                    .to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn compressible_content_tags_openai_chat_by_role() {
        // Regression. `learn` used to hand every extracted string back untagged and
        // route it through `Orchestrator::transform_for`, which skips the
        // tool-output-only gate -- so a plain user message could be measured as
        // compressible prose.
        let body: serde_json::Value = serde_json::from_str(
            r#"{"messages":[
                {"role":"user","content":"hello there"},
                {"role":"tool","tool_call_id":"c","content":"tool output"}
            ]}"#,
        )
        .unwrap();

        assert_eq!(
            compressible_content(&body),
            vec![
                ("hello there".to_owned(), BlockKind::Text),
                ("tool output".to_owned(), BlockKind::ToolResult),
            ]
        );
    }

    #[test]
    fn compressible_content_tags_anthropic_blocks_by_type() {
        let body: serde_json::Value = serde_json::from_str(
            r#"{"messages":[{"role":"user","content":[
                {"type":"text","text":"hello there"},
                {"type":"tool_result","content":"tool output"}
            ]}]}"#,
        )
        .unwrap();

        assert_eq!(
            compressible_content(&body),
            vec![
                ("hello there".to_owned(), BlockKind::Text),
                ("tool output".to_owned(), BlockKind::ToolResult),
            ]
        );
    }

    #[test]
    fn compressible_content_tags_responses_items_by_type() {
        let body: serde_json::Value = serde_json::from_str(
            r#"{"input":[{"type":"function_call_output","call_id":"c","output":"tool output"}]}"#,
        )
        .unwrap();

        assert_eq!(
            compressible_content(&body),
            vec![("tool output".to_owned(), BlockKind::FunctionCallOutput)]
        );
    }

    #[test]
    fn learn_never_routes_a_user_message_through_the_prose_summarizer() {
        // The end-to-end version of the two tests above: content `compressible_content`
        // tags as `BlockKind::Text` must still be declined by the orchestrator, exactly
        // as `learn`'s loop would leave it -- proving the fix all the way through, not
        // just the tagging step.
        let prose = compressible_prose();
        let body: serde_json::Value = serde_json::from_str(&format!(
            r#"{{"messages":[{{"role":"user","content":{}}}]}}"#,
            serde_json::to_string(&prose).unwrap()
        ))
        .unwrap();

        let found = compressible_content(&body);
        assert_eq!(found, vec![(prose, BlockKind::Text)]);

        let orchestrator = orchestrator();
        let block = Block::new(BlockKind::Text, found[0].0.clone());
        assert!(
            orchestrator
                .transform_for_block(&block, payg(), "claude-opus-4")
                .is_none(),
            "a plain user message reached a compressor via the learn corpus path"
        );
    }

    /// The value `inspect_report` printed after `label:`, searching from `from`.
    ///
    /// Read out of the rendered lines rather than returned from a struct, because the
    /// rendering is what an operator reads and therefore what has to be right. A test
    /// against an intermediate value would pass while the printed line said something
    /// else.
    fn value_after(lines: &[String], label: &str, from: usize) -> (usize, String) {
        let prefix = format!("{label}: ");
        let at = lines[from..]
            .iter()
            .position(|line| line.trim_start().starts_with(&prefix))
            .map(|offset| from + offset)
            .unwrap_or_else(|| panic!("no `{label}:` line in {lines:#?}"));

        (at, lines[at].trim_start()[prefix.len()..].to_string())
    }

    /// The reason `inspect_report` gave for `credential`.
    fn line_reason(lines: &[String], credential: &str) -> String {
        value_after(lines, credential, 0).1
    }

    /// The transform `inspect_report` named for one credential and block kind.
    fn reported(lines: &[String], credential: &str, kind: &str) -> String {
        let (at, _) = value_after(lines, credential, 0);
        value_after(lines, kind, at).1
    }

    #[test]
    fn inspect_names_the_compressor_that_actually_compresses_the_content() {
        // The bug, exactly. `inspect` carried its own routing table mapping prose to
        // "none", and it stayed that way after the prose compressors were wired in — so
        // this same content got `compressor: none` from `headroom inspect` and a 70%
        // saving from `headroom compress`, in one shell, seconds apart.
        //
        // The assertion is deliberately anchored to a *measured* saving rather than to
        // the string "text_summarizer": what makes the old output wrong is not the name
        // it printed, it is that something did compress while it said nothing would.
        let prose = compressible_prose();
        let estimator = HeuristicEstimator::new();
        let before = estimator.count(&prose);

        let mut block = Block::new(BlockKind::ToolResult, prose.clone());
        let orchestrator = orchestrator();
        let transform = orchestrator
            .transform_for_block(&block, payg(), "")
            .expect("nothing routed this prose, so the fixture proves nothing");
        let outcome = validated_apply(transform, &mut block, &estimator).expect("apply failed");
        assert!(
            outcome.is_compressed() && estimator.count(block.content()) < before,
            "the fixture did not actually compress, so this test would pass on a stub"
        );

        // Given that it compresses, the command must not claim otherwise.
        let lines = inspect_report(&prose);
        let named = reported(&lines, "api key", "as tool output");
        assert_eq!(
            named,
            transform.name(),
            "inspect named `{named}` for content the pipeline compresses with `{}`",
            transform.name()
        );
    }

    #[test]
    fn inspect_agrees_with_the_orchestrator_on_every_content_type() {
        // The general form: whatever `inspect` prints must be what
        // `transform_for_block` returns. Re-introducing any hand-written table fails
        // here as soon as it drifts by one arm — which is the only way this class of bug
        // has ever appeared.
        let samples: Vec<(&str, String)> = vec![
            (
                "json",
                format!("[{}]", vec![r#"{"a":1,"b":"x"}"#; 300].join(",")),
            ),
            (
                "code",
                concat!(
                    "pub fn handle(input: &str) -> Result<String, Error> {\n",
                    "    let parsed = parse(input)?;\n",
                    "    Ok(render(&parsed))\n",
                    "}\n"
                )
                .repeat(80),
            ),
            ("prose", compressible_prose()),
            ("tiny", "hello".to_string()),
        ];

        for (label, content) in samples {
            let lines = inspect_report(&content);

            for (credential, mode) in [
                ("api key", AuthMode::PayAsYouGo),
                ("oauth", AuthMode::OAuth),
                ("subscription", AuthMode::Subscription),
            ] {
                let policy = CompressionPolicy::for_mode(mode);
                assert_eq!(
                    line_reason(&lines, credential),
                    orchestrator().route(&content, policy, "").as_str(),
                    "{label}/{credential}: reported reason is not the routed one"
                );

                for (kind_label, kind) in [
                    ("as tool output", BlockKind::ToolResult),
                    ("as a typed message", BlockKind::Text),
                ] {
                    let block = Block::new(kind, content.clone());
                    let expected = orchestrator()
                        .transform_for_block(&block, policy, "")
                        .map_or("forwarded unchanged".to_string(), |t| t.name().to_string());

                    // `starts_with` because a below-threshold row carries a trailing
                    // note; the name itself still has to match.
                    let reported = reported(&lines, credential, kind_label);
                    assert!(
                        reported.starts_with(&expected),
                        "{label}/{credential}/{kind_label}: inspect said `{reported}`, \
                         the orchestrator says `{expected}`"
                    );
                }
            }
        }
    }

    #[test]
    fn inspect_does_not_promise_to_compress_a_typed_message() {
        // D24. Prose from a tool is the product; prose a person typed is not ours to
        // rewrite, and a command that told an operator otherwise would be advertising a
        // saving the proxy will never take.
        let lines = inspect_report(&compressible_prose());

        assert_eq!(
            reported(&lines, "api key", "as a typed message"),
            "forwarded unchanged"
        );
    }

    #[test]
    fn inspect_answers_for_the_credential_rather_than_assuming_an_api_key() {
        // I10 is the difference between "this will compress" and "this will not", and
        // the old single-line output picked pay-as-you-go silently. An operator running
        // a subscription token and asking why nothing compresses got the answer for
        // somebody else's credential.
        let lines = inspect_report(&compressible_prose());

        assert_eq!(line_reason(&lines, "subscription"), "policy_forbids");
        assert_eq!(
            reported(&lines, "subscription", "as tool output"),
            "forwarded unchanged"
        );
        assert_eq!(line_reason(&lines, "oauth"), "lossless");
    }

    #[test]
    fn inspect_says_so_when_the_size_threshold_will_stop_the_named_compressor() {
        // `route` does not consult the sizer — each compressor holds its own — so on a
        // short payload the routing line names a compressor that will decline. Naming it
        // with no qualification reads as "this compresses".
        let lines = inspect_report("hello");
        let named = reported(&lines, "api key", "as tool output");

        assert!(
            named.contains("below the size threshold"),
            "no threshold note on a 5-byte payload: `{named}`"
        );
        // And the note must not appear where nothing was named to decline.
        assert_eq!(
            reported(&lines, "subscription", "as tool output"),
            "forwarded unchanged"
        );
    }

    #[test]
    fn tools_never_reports_a_compressed_type_as_forwarded() {
        // The bug. `headroom tools` carried a hand-written pair of lists and the second
        // one — "detected but not compressed" — named code, prose and unknown. Two of
        // the three were wrong, and they were the two that matter: source files and
        // prose tool results are the bulk of agent traffic.
        //
        // Checked against the orchestrator for every variant rather than against the two
        // that were wrong, so a type gaining a compressor cannot reintroduce this.
        let lines = compressor_table();
        let split = lines
            .iter()
            .position(|line| line == "detected but not compressed")
            .expect("no forwarded section");
        let (compressed, forwarded) = lines.split_at(split);
        let orchestrator = orchestrator();

        for content_type in ContentType::ALL {
            let name = content_type.as_str();
            let listed_as = |section: &[String]| {
                section
                    .iter()
                    .any(|line| line.split_whitespace().next() == Some(name))
            };

            if let Some(transform) = orchestrator.for_type(content_type) {
                assert!(
                    listed_as(compressed),
                    "{name} compresses with {} and is not in the compressors list",
                    transform.name()
                );
                assert!(
                    !listed_as(forwarded),
                    "{name} compresses and is reported as forwarded"
                );
            } else {
                assert!(
                    listed_as(forwarded),
                    "{name} reaches no compressor and is not reported as forwarded"
                );
                assert!(
                    !listed_as(compressed),
                    "{name} reaches no compressor and is listed as compressed"
                );
            }
        }
    }

    #[test]
    fn tools_names_the_compressor_the_orchestrator_would_use() {
        // The other half: being in the right section is not enough if the name beside it
        // is a literal somebody typed.
        let lines = compressor_table();
        let orchestrator = orchestrator();

        for content_type in ContentType::ALL {
            let Some(transform) = orchestrator.for_type(content_type) else {
                continue;
            };
            let line = lines
                .iter()
                .find(|line| line.split_whitespace().next() == Some(content_type.as_str()))
                .unwrap_or_else(|| panic!("no line for {}", content_type.as_str()));

            assert!(
                line.contains(transform.name()),
                "`{line}` does not name `{}`",
                transform.name()
            );
        }
    }

    #[test]
    fn tools_says_which_compressors_only_see_tool_output() {
        // D24 in the other direction. Listing prose beside the rest unqualified tells an
        // operator that what their users type gets rewritten, which is the one thing the
        // rule exists to prevent — as wrong as the old list's claim that prose is never
        // compressed, just wrong the other way.
        let lines = compressor_table();
        let orchestrator = orchestrator();

        for content_type in ContentType::ALL {
            if orchestrator.for_type(content_type).is_none() {
                continue;
            }
            let line = lines
                .iter()
                .find(|line| line.split_whitespace().next() == Some(content_type.as_str()))
                .unwrap();

            assert_eq!(
                line.contains("tool output only"),
                orchestrator.tool_output_only(content_type),
                "`{line}` disagrees with the block-kind rule"
            );
        }

        // And the rule is not vacuous — something has to be under it, or the assertion
        // above passes by describing a distinction that does not exist.
        assert!(
            ContentType::ALL
                .iter()
                .any(|ct| orchestrator.tool_output_only(*ct)),
            "no content type is tool-output-only, so the check above proves nothing"
        );
    }

    #[test]
    fn env_and_wrap_agree_on_a_proxy_with_a_trailing_slash() {
        // The bug. `Agent::env` trims the slash and `headroom env` restated the format
        // strings without it, so `headroom env --proxy http://x:8787/` emitted
        // `OPENAI_BASE_URL=http://x:8787//v1` while `headroom wrap aider` with the same
        // input emitted `/v1`. Two commands doing one job, disagreeing.
        for proxy in [
            "http://127.0.0.1:8787/",
            "http://127.0.0.1:8787",
            "http://x/y//",
        ] {
            let from_env = agent_env(proxy);

            for agent in crate::wrap::Agent::ALL {
                for (name, wrapped) in agent.env(proxy) {
                    let (_, printed) = from_env
                        .iter()
                        .find(|(known, _)| *known == name)
                        .unwrap_or_else(|| panic!("`headroom env` omits {name} for {agent}"));
                    assert_eq!(
                        printed, &wrapped,
                        "{proxy}: env says {name}={printed}, wrap {agent} says {wrapped}"
                    );
                }
            }
        }
    }

    #[test]
    fn env_emits_no_variable_twice() {
        // Five agents ask for `ANTHROPIC_BASE_URL`. A duplicated export is harmless to
        // `eval` and reads as a bug to whoever runs the command without one.
        let vars = agent_env("http://127.0.0.1:8787");
        let mut names: Vec<_> = vars.iter().map(|(name, _)| *name).collect();
        let before = names.len();

        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate export in {vars:?}");
    }

    #[test]
    fn agents_do_not_disagree_about_a_variable() {
        // `agent_env` takes the first value for a name. That is only safe while no two
        // agents want different values, and this is what makes the assumption visible
        // rather than latent: an agent needing `OPENAI_BASE_URL` without the `/v1`
        // suffix would make one `eval` wrong for somebody, silently.
        let mut claimed: Vec<(&str, String)> = Vec::new();

        for agent in crate::wrap::Agent::ALL {
            for (name, value) in agent.env("http://127.0.0.1:8787") {
                if let Some((_, first)) = claimed.iter().find(|(known, _)| *known == name) {
                    assert_eq!(first, &value, "{agent} disagrees about {name}");
                } else {
                    claimed.push((name, value));
                }
            }
        }

        // Not vacuous: something has to be claimed twice, or the loop above proves
        // nothing about agreement.
        assert!(
            crate::wrap::Agent::ALL
                .iter()
                .filter(|a| a
                    .env("http://x")
                    .iter()
                    .any(|(n, _)| *n == "ANTHROPIC_BASE_URL"))
                .count()
                > 1,
            "no variable is claimed by two agents, so this test checks nothing"
        );
    }

    #[test]
    fn savings_reads_the_names_the_proxy_actually_emits() {
        // `savings_report` matches four metric names as string literals — a second copy
        // of what `Metrics::render` writes, on the far side of a process boundary where
        // no compiler checks it. Rename a counter in the proxy and this command reports
        // `tokens saved 0` forever: `value` returns `None` for an absent name and the
        // caller defaults it to zero, so the failure is a plausible number rather than
        // an error.
        //
        // Checked: renaming `headroom_tokens_saved_total` in `metrics.rs` left the entire
        // test suite green. The fixture-based test below is written by the same hand as
        // the parser, so it agrees with itself no matter what the proxy emits.
        //
        // This one renders a real `Metrics`, so the two names have to be the same name.
        let metrics = headroom_proxy::metrics::Metrics::new();
        metrics.record_rewritten(1000, 600);
        metrics.record_rewritten(500, 400);
        metrics.record_passthrough();
        // A labelled series in the same scrape, from the same source rather than typed
        // into a fixture — the shape that would break a prefix match.
        metrics.record_routing("compress");
        metrics.record_routing("policy_forbids");

        let report = savings_report(&metrics.render());

        // 3 requests, 2 compressed, 1500 before, 1000 after → 500 saved, 33.3%.
        assert!(report.contains("requests      3"), "{report}");
        assert!(report.contains("compressed    2"), "{report}");
        assert!(
            report.contains(&format!("tokens saved  {}", metrics.tokens_saved())),
            "{report}"
        );
        assert!(report.contains("33.3%"), "{report}");
        // Not the vacuous pass: an empty scrape must not produce these.
        assert!(!savings_report("").contains("requests      3"));
    }

    #[test]
    fn the_self_test_samples_cover_every_compressor() {
        // `doctor` says whether each compressor works and `perf` says how fast each one
        // is, both by walking these samples. A content type that gains a compressor and
        // no sample is a compressor both commands silently stop reporting — and silence
        // from a self-test reads as "fine", which is the failure mode that let five
        // capabilities ship unreached.
        let orchestrator = orchestrator();
        let samples = self_test_samples();

        let mut covered = Vec::new();
        for (_, sample) in &samples {
            let block = Block::new(BlockKind::ToolResult, sample.clone());
            let transform = orchestrator
                .transform_for_block(&block, payg(), "")
                .unwrap_or_else(|| panic!("a sample reaches no compressor: {sample:.60}"));
            covered.push(transform.name());
        }

        for content_type in ContentType::ALL {
            let Some(transform) = orchestrator.for_type(content_type) else {
                continue;
            };
            assert!(
                covered.contains(&transform.name()),
                "{} compresses {} and no self-test sample exercises it",
                transform.name(),
                content_type.as_str()
            );
        }
    }

    #[test]
    fn every_self_test_sample_is_detected_as_its_label() {
        // A sample that stopped being detected as what it is named would move to another
        // compressor, and both commands would keep reporting under the old label — the
        // prose sample was once a single 24 KB line, which is one line to a line-budget
        // summarizer and compressed nothing.
        for (label, sample) in self_test_samples() {
            assert_eq!(
                detect(sample.as_bytes()).content_type.as_str(),
                label,
                "the {label} sample is detected as something else"
            );
        }
    }

    #[test]
    fn every_self_test_sample_actually_compresses() {
        // Otherwise `doctor` reports a failure that is the fixture's fault, and `perf`
        // times a compressor declining rather than working.
        let orchestrator = orchestrator();
        let estimator = HeuristicEstimator::new();

        for (label, sample) in self_test_samples() {
            let mut block = Block::new(BlockKind::ToolResult, sample.clone());
            let transform = orchestrator
                .transform_for_block(&block, payg(), "")
                .unwrap_or_else(|| panic!("{label} reached no compressor"));
            let outcome = validated_apply(transform, &mut block, &estimator)
                .unwrap_or_else(|err| panic!("{label} failed to compress: {err}"));

            assert!(
                outcome.is_compressed(),
                "the {label} sample did not compress"
            );
        }
    }

    #[test]
    fn a_request_the_proxy_made_larger_is_not_reported_as_compressed() {
        // Memory injection adds content by design, so a request can leave larger than it
        // arrived. Measured through the release binary with `HEADROOM_MEMORY` set: 65
        // tokens in, 440 out — and `/metrics` said `compressed_total 1`, `saved_total 0`.
        //
        // "Compression ran and found nothing" and "this request was made 6.8 times
        // bigger" are opposite outcomes, and the counter that answers *is the proxy
        // helping* was reporting the first for the second.
        let metrics = headroom_proxy::metrics::Metrics::new();
        metrics.record_rewritten(65, 440);

        let scrape = metrics.render();
        let report = savings_report(&scrape);

        assert!(report.contains("compressed    0"), "{report}");
        assert!(report.contains("made larger   1"), "{report}");

        // And the ordinary case still reads as before, or the fix would be a new way to
        // misreport.
        let shrunk = headroom_proxy::metrics::Metrics::new();
        shrunk.record_rewritten(1000, 400);
        let report = savings_report(&shrunk.render());

        assert!(report.contains("compressed    1"), "{report}");
        assert!(!report.contains("made larger"), "{report}");
    }

    #[test]
    fn savings_ignores_labelled_series() {
        // `headroom_routing_total{reason=...}` is the first labelled metric the proxy
        // exposes, and this report predates it. A parser that matched on the bare name
        // would read a label set as a value; one that dropped the trailing space would
        // silently report zero for everything.
        let metrics = concat!(
            "# HELP headroom_requests_total Requests seen.\n",
            "# TYPE headroom_requests_total counter\n",
            "headroom_requests_total 4\n",
            "headroom_compressed_total 3\n",
            "headroom_tokens_before_total 1000\n",
            "headroom_tokens_saved_total 400\n",
            "# HELP headroom_routing_total Blocks by why they were routed.\n",
            "# TYPE headroom_routing_total counter\n",
            "headroom_routing_total{reason=\"compress\"} 7\n",
            "headroom_routing_total{reason=\"policy_forbids\"} 2\n",
        );

        let report = savings_report(metrics);
        assert!(report.contains("requests      4"), "{report}");
        assert!(report.contains("compressed    3"), "{report}");
        assert!(report.contains("tokens saved  400"), "{report}");
        // 400/1000 — proof the labelled counters were not mistaken for these.
        assert!(report.contains("40.0%"), "{report}");
    }

    #[test]
    fn savings_reports_nothing_measured_rather_than_zero_percent() {
        // An empty scrape and a broken compressor look identical if this prints "0.0%",
        // and telling them apart is the entire point of the report.
        let report = savings_report("");
        assert!(!report.contains("0.0%"), "{report}");
    }

    #[test]
    fn the_generated_config_marks_every_startup_only_setting() {
        // The template used to open with "Read live on every request, so changes take
        // effect without a restart" and then list `HEADROOM_HOST` and `HEADROOM_PORT`,
        // which are read once — the socket is bound at startup. A config file that
        // misdescribes its own semantics is worse than one that says nothing, because it
        // is the first thing a new operator reads and they have no reason to doubt it.
        let scratch = std::env::temp_dir().join(format!("headroom-init-{}", std::process::id()));
        let path = scratch.join("headroom.env");
        std::fs::create_dir_all(&scratch).unwrap();

        init(&path, true).expect("init failed");
        let contents = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&scratch).ok();

        // Every setting the proxy reads once must be marked where it appears. Checked
        // against the proxy's own list rather than a copy, so adding a startup-only
        // setting there fails here until the template is updated.
        for name in headroom_proxy::config::STARTUP_ONLY {
            let Some(at) = contents.find(name) else {
                continue;
            };
            let preceding = &contents[..at];
            assert!(
                preceding
                    .rsplit("\n\n")
                    .next()
                    .is_some_and(|block| block.contains("RESTART REQUIRED")),
                "{name} appears in the template without a RESTART REQUIRED note"
            );
        }
    }

    #[test]
    fn the_generated_config_does_not_claim_everything_is_live() {
        let scratch = std::env::temp_dir().join(format!("headroom-live-{}", std::process::id()));
        let path = scratch.join("headroom.env");
        std::fs::create_dir_all(&scratch).unwrap();

        init(&path, true).expect("init failed");
        let contents = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&scratch).ok();

        assert!(
            contents.contains("The exceptions are marked"),
            "the header claims everything is read live"
        );
    }

    #[test]
    fn doctor_passes_on_a_working_build() {
        // The self-test must actually pass here, or it is reporting nothing.
        assert!(doctor().is_ok());
    }

    #[test]
    fn env_emits_evaluable_shell() {
        // Rendered rather than captured, but the shape is what matters: anything not
        // an export must be commented, or `eval` fails.
        assert!(env("http://127.0.0.1:8787").is_ok());
    }
}

/// `headroom wrap <agent>` — print the environment that routes an agent through the
/// proxy, and rewrite any settings file it uses.
///
/// # Why the exports are printed rather than written
///
/// A shell profile belongs to its owner. Appending to one means guessing which of
/// `.bashrc`, `.zshrc`, `.profile` or a fish config is live, editing a file the customer
/// maintains by hand, and owning the removal forever. Printing lines they can paste or
/// `eval` leaves the decision where it belongs.
///
/// # Errors
///
/// Returns an error if the agent is unknown, or if a settings file exists but cannot be
/// rewritten.
pub fn wrap(agent: &str, proxy: &str, settings: Option<&std::path::Path>) -> anyhow::Result<()> {
    let Some(agent) = crate::wrap::Agent::parse(agent) else {
        anyhow::bail!(
            "unknown agent {agent:?}; supported: {}",
            crate::wrap::Agent::ALL
                .iter()
                .map(|a| a.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    };

    // The settings file first, and before any question about environment variables.
    //
    // This used to bail on `!env_configurable()` *before* reaching here, which refused
    // the one agent that has no environment variables — cursor — while telling its user
    // to "set its base URL in its own settings instead". That is exactly what `--settings`
    // automates, so the message named the fix and the code declined to perform it. Worse,
    // `unwrap` never had the check, so `unwrap cursor --settings` worked against a backup
    // `wrap cursor --settings` would not create.
    if let Some(path) = settings {
        let written = crate::wrap::wrap_settings_file(path, proxy)?;
        eprintln!("rewrote {}", written.display());
        eprintln!("original saved alongside it; `headroom unwrap` restores it exactly");
    }

    // `env_configurable`, not a second `exports.is_empty()` that means the same thing.
    // One predicate, one place — the duplication this repository keeps paying for.
    if !agent.env_configurable() {
        // Nothing to print, so say why — and only complain when there was also no file
        // to rewrite, because in that case nothing happened at all.
        if settings.is_none() {
            anyhow::bail!(
                "{agent} reads no environment variables; point it at {proxy} with \
                 `headroom wrap {agent} --settings <file>`, which backs the file up so \
                 `headroom unwrap` can restore it exactly"
            );
        }
        return Ok(());
    }

    let exports = agent.env(proxy);
    let mut stdout = std::io::stdout().lock();
    for (name, value) in &exports {
        writeln!(stdout, "export {name}={value}")?;
    }
    stdout.flush()?;

    eprintln!();
    eprintln!("Apply to the current shell with:");
    eprintln!("  eval \"$(headroom wrap {agent} --proxy {proxy})\"");
    Ok(())
}

/// `headroom unwrap <agent>` — restore a settings file and print the variables to unset.
///
/// # Errors
///
/// Returns an error if the agent is unknown, or a backup exists but cannot be restored.
pub fn unwrap(agent: &str, settings: Option<&std::path::Path>) -> anyhow::Result<()> {
    let Some(agent) = crate::wrap::Agent::parse(agent) else {
        anyhow::bail!("unknown agent {agent:?}");
    };

    if let Some(path) = settings {
        // Checked before restoring so the message is about the state the caller found,
        // not about what the restore happened to return.
        if crate::wrap::is_wrapped(path) {
            crate::wrap::unwrap_settings_file(path)?;
            eprintln!("restored {} from its backup", path.display());
        } else {
            // Not an error. The state the caller asked for is the state they have.
            eprintln!("{} was not wrapped; nothing to restore", path.display());
        }
    }

    let mut stdout = std::io::stdout().lock();
    for (name, _) in agent.env("http://placeholder") {
        writeln!(stdout, "unset {name}")?;
    }
    stdout.flush()?;
    Ok(())
}

/// `headroom savings` — report what the proxy has saved, read from its `/metrics`.
///
/// # Errors
///
/// Returns an error if the metrics text cannot be read from stdin.
pub fn savings() -> anyhow::Result<()> {
    let raw = read_stdin()?;
    let report = savings_report(&raw);

    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{report}")?;
    stdout.flush()?;
    Ok(())
}

/// Builds the savings report from Prometheus exposition text.
///
/// Separated from I/O so the formatting is testable without a running proxy.
fn savings_report(metrics: &str) -> String {
    // # Two independent reasons a labelled series cannot corrupt this
    //
    // A Prometheus sample is `name value` for a scalar and `name{label="v"} value` for a
    // labelled series — and `headroom_routing_total{reason=...}` is now among them.
    //
    // First, the trailing space in each name below means a labelled line does not match
    // the prefix at all. Second, `find_map` keeps looking when a match fails to parse, so
    // even without the space the `{reason=...} 2` remainder is rejected and the scalar on
    // a later line is still found.
    //
    // Checked rather than assumed: removing the spaces was tried, and the tests still
    // pass. They are kept as the clearer of the two guards, but the behaviour that
    // matters is pinned by `savings_ignores_labelled_series` rather than by either
    // mechanism, so a refactor is free to change how this reads.
    let value = |name: &str| -> Option<f64> {
        metrics.lines().find_map(|line| {
            let rest = line.strip_prefix(name)?;
            rest.trim().parse().ok()
        })
    };

    let requests = value("headroom_requests_total ").unwrap_or(0.0);
    let compressed = value("headroom_compressed_total ").unwrap_or(0.0);
    // Reported because it is the one outcome an operator would never guess at from the
    // other numbers. Memory injection adds content by design, so a request can leave
    // larger than it arrived — and that used to be counted as a compression that saved
    // nothing, which reads as "found nothing to do" rather than "made it worse".
    let expanded = value("headroom_expanded_total ").unwrap_or(0.0);
    let before = value("headroom_tokens_before_total ").unwrap_or(0.0);
    let saved = value("headroom_tokens_saved_total ").unwrap_or(0.0);

    let mut out = String::new();
    out.push_str(&format!("requests      {requests:.0}\n"));
    out.push_str(&format!("compressed    {compressed:.0}\n"));
    if expanded > 0.0 {
        out.push_str(&format!("made larger   {expanded:.0}\n"));
    }
    out.push_str(&format!("tokens saved  {saved:.0}\n"));

    // A ratio needs a denominator. Reporting "0.0%" when nothing has been measured is
    // indistinguishable from a compressor that has stopped working, which is the one
    // thing this report exists to reveal.
    match (before > 0.0).then(|| saved / before * 100.0) {
        Some(ratio) => out.push_str(&format!("reduction     {ratio:.1}%\n")),
        None => out.push_str("reduction     no data yet\n"),
    }

    // Deliberately absent: a currency figure. It needs a per-model price this program
    // does not have and cannot keep current, and a wrong number about money is worse
    // than no number.
    match (requests > 0.0).then(|| compressed / requests * 100.0) {
        Some(rate) => out.push_str(&format!(
            "hit rate      {rate:.1}% of requests compressed\n"
        )),
        None => out.push_str("hit rate      no data yet\n"),
    }

    out.trim_end().to_owned()
}

#[cfg(test)]
mod command_tests {
    use super::*;

    const METRICS: &str = concat!(
        "# HELP headroom_requests_total Requests seen.\n",
        "headroom_requests_total 10\n",
        "headroom_compressed_total 7\n",
        "headroom_tokens_before_total 1000\n",
        "headroom_tokens_after_total 200\n",
        "headroom_tokens_saved_total 800\n",
    );

    #[test]
    fn the_savings_report_parses_the_exposition_format() {
        // Renamed from `..._reads_real_exposition_text`, which overclaimed: `METRICS` is
        // a fixture written by hand beside the parser, so it agrees with the parser
        // whatever the proxy emits. Renaming a counter in `metrics.rs` left this green.
        //
        // What it does check is the parsing — HELP lines, ratios, the `_total` suffixes —
        // and that is worth having. The names are pinned across the process boundary by
        // `savings_reads_the_names_the_proxy_actually_emits`, which renders a real
        // `Metrics`.
        let report = savings_report(METRICS);
        assert!(report.contains("tokens saved  800"), "{report}");
        assert!(report.contains("reduction     80.0%"), "{report}");
        assert!(report.contains("hit rate      70.0%"), "{report}");
    }

    #[test]
    fn a_help_line_is_not_mistaken_for_a_value() {
        // `# HELP headroom_requests_total ...` shares the metric's prefix. A naive
        // `contains` would parse the help text as the number and report nonsense.
        assert!(savings_report(METRICS).contains("requests      10"));
    }

    #[test]
    fn no_data_is_reported_as_no_data_rather_than_zero_percent() {
        // "0.0%" is indistinguishable from a compressor that has stopped working, which
        // is the one thing this report exists to reveal.
        let report = savings_report("headroom_requests_total 0\n");
        assert!(report.contains("reduction     no data yet"), "{report}");
        assert!(report.contains("hit rate      no data yet"), "{report}");
    }

    #[test]
    fn garbage_input_does_not_panic() {
        for source in ["", "not metrics at all", "headroom_requests_total abc"] {
            let _ = savings_report(source);
        }
    }

    #[test]
    fn the_report_never_claims_a_currency_amount() {
        // It would need a per-model price this program does not have and cannot keep
        // current, and a wrong number about money is worse than no number.
        let report = savings_report(METRICS);
        for marker in ['$', '£', '€'] {
            assert!(!report.contains(marker), "{report}");
        }
    }

    // ---- deploy ----

    #[test]
    fn the_compose_service_publishes_only_on_loopback() {
        // The security property. A bare `"8787:8787"` exposes an open credential relay
        // to the whole network the moment someone copies this onto a server — the same
        // mistake `Config::default` exists to avoid, reintroduced by a template nobody
        // reads closely.
        let rendered = deploy_manifests(8787, "https://api.anthropic.com", "headroom-proxy");

        assert!(
            rendered.contains(r#"ports: ["127.0.0.1:8787:8787"]"#),
            "{rendered}"
        );
        assert!(
            !rendered.contains(r#"ports: ["8787:8787"]"#),
            "the compose service publishes on every interface"
        );
        assert!(!rendered.contains("0.0.0.0"), "{rendered}");
    }

    #[test]
    fn the_systemd_unit_restarts_on_failure() {
        // The proxy sits in the request path of everything routed through it. A crash
        // that is not restarted takes the customer's agents down with it.
        let rendered = deploy_manifests(8787, "https://api.anthropic.com", "headroom-proxy");
        assert!(rendered.contains("Restart=on-failure"), "{rendered}");
    }

    #[test]
    fn the_port_and_upstream_reach_every_manifest() {
        // Three manifests, one set of values. A template where one of them silently
        // keeps the default is worse than no template — it starts, listens on the wrong
        // port, and looks fine.
        let rendered = deploy_manifests(9999, "http://example.test", "headroom-proxy");

        assert_eq!(rendered.matches("9999").count(), 5, "{rendered}");
        assert_eq!(
            rendered.matches("http://example.test").count(),
            3,
            "{rendered}"
        );
    }

    #[test]
    fn the_manifests_carry_no_credential() {
        // Deployment templates get pasted into shared docs and issue threads.
        let rendered = deploy_manifests(8787, "https://api.anthropic.com", "headroom-proxy");
        for marker in ["sk-", "ANTHROPIC_API_KEY", "OPENAI_API_KEY", "x-api-key"] {
            assert!(!rendered.contains(marker), "{marker} appeared: {rendered}");
        }
    }

    #[test]
    fn every_agent_can_be_pointed_at_the_proxy_through_a_settings_file() {
        // `wrap.rs` tests the file functions thoroughly and they were all correct. The
        // defect was one layer up: `commands::wrap` bailed on `!env_configurable()`
        // *before* reaching them, so cursor — the one agent with no environment
        // variables, and therefore the only one for which `--settings` is the only way
        // to wrap — was refused, and told to do by hand what the flag automates.
        //
        // A table over every agent rather than a case for cursor, because the next agent
        // with no environment variables should fail here rather than in a user's hands.
        let dir = std::env::temp_dir().join("headroom-wrap-command-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("could not create the test directory");

        let original = "{\n\t\"zeta\": 1,\n  \"base_url\":   \"https://api.anthropic.com\"\n}\n";

        for agent in crate::wrap::Agent::ALL {
            let path = dir.join(format!("{agent}.json"));
            std::fs::write(&path, original).expect("write");

            wrap(agent.as_str(), "http://127.0.0.1:8787", Some(&path))
                .unwrap_or_else(|err| panic!("{agent} refused a settings file: {err}"));

            let backup = std::fs::read_to_string(
                path.with_file_name(format!("{agent}.json{}", crate::wrap::BACKUP_SUFFIX)),
            )
            .unwrap_or_else(|err| panic!("{agent} left no backup: {err}"));

            // Both halves matter. Without the first, an agent that silently did nothing
            // would pass; without the second, one that "backed up" the rewritten file
            // would — which is the failure the double-wrap guard exists for.
            assert_ne!(
                std::fs::read_to_string(&path).expect("read"),
                original,
                "{agent} left the settings file untouched"
            );
            assert_eq!(backup, original, "{agent} backed up the wrong bytes");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_agent_with_no_environment_variables_and_no_file_names_the_flag() {
        // Still an error — nothing happened — but the message has to point at the thing
        // that would work. The old one said "set its base URL in its own settings
        // instead", which is advice to do manually what `--settings` does, without the
        // backup that makes it undoable.
        let err = wrap("cursor", "http://127.0.0.1:8787", None)
            .expect_err("an agent with no environment variables reported success");
        let message = err.to_string();

        assert!(
            message.contains("--settings"),
            "the error does not name the flag that would work: {message}"
        );
    }
}

/// `headroom perf` — measure compression throughput on this machine.
///
/// # Why this measures the compressor and not the proxy
///
/// A round trip to a model provider is hundreds of milliseconds; compression is
/// microseconds. Measuring end-to-end latency would report the network and bury the
/// only number this program controls. What matters operationally is whether compression
/// is fast enough to be invisible against that round trip, which is a question about
/// throughput on local data.
///
/// # Errors
///
/// Returns an error if stdout cannot be written.
pub fn perf(iterations: usize) -> anyhow::Result<()> {
    // Routed through the `Orchestrator`, one row per compressor.
    //
    // # Why one number was the wrong answer
    //
    // This benchmarked `SmartCrusher` on a JSON payload and printed the result as
    // `throughput`, unqualified. An operator reads that as what the proxy does to their
    // traffic — but their traffic is mostly source files and command output, which go to
    // different compressors with different costs. Measured over all six, per call:
    // 63 µs for diffs, 74 for search output, 194 for JSON, 196 for code, 249 for logs,
    // 1138 for prose. An 18x spread reported as one number, and the number reported was
    // the second slowest.
    //
    // The conclusion happens to survive — even the slowest is a millisecond against a
    // round trip of hundreds, which is the question the header above says matters. But
    // that is the answer, not something to assume from one sample: a compressor that
    // regressed by 100x would still have been invisible here.
    let estimator = HeuristicEstimator::new();
    let orchestrator = Orchestrator::new(Arc::new(InMemoryCcrStore::new()));
    let policy = CompressionPolicy::for_mode(AuthMode::PayAsYouGo);

    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "iterations    {iterations}")?;
    writeln!(stdout)?;
    writeln!(
        stdout,
        "{:<8} {:<18} {:>7} {:>12} {:>10}  compressed",
        "type", "compressor", "bytes", "per call", "throughput"
    )?;

    // The same four samples `headroom doctor` self-tests with, so "it works" and "it is
    // this fast" are statements about the same content.
    for (label, sample) in self_test_samples() {
        // `ToolResult`, not `Text`: prose compresses only from tool output (D24), and a
        // `Text` block would silently drop the slowest compressor from the benchmark.
        let block = Block::new(BlockKind::ToolResult, sample.clone());
        let Some(transform) = orchestrator.transform_for_block(&block, policy, "") else {
            writeln!(stdout, "{label:<8} {:<18}", "no compressor")?;
            continue;
        };

        // A warm-up pass, discarded. The first iteration pays for allocator growth and
        // branch prediction that every later one does not, so including it reports a
        // throughput this machine never actually sustains.
        let mut warm = Block::new(BlockKind::ToolResult, sample.clone());
        let _ = validated_apply(transform, &mut warm, &estimator);

        let started = std::time::Instant::now();
        let mut compressed = 0usize;
        for _ in 0..iterations {
            let mut block = Block::new(BlockKind::ToolResult, sample.clone());
            if let Ok(outcome) = validated_apply(transform, &mut block, &estimator) {
                if outcome.is_compressed() {
                    compressed += 1;
                }
            }
        }
        let seconds = started.elapsed().as_secs_f64();
        let bytes = sample.len();

        // Guarded rather than divided blindly: a fast machine and a small iteration count
        // can round to zero, and dividing by it would report `inf` as a throughput.
        if seconds > 0.0 && iterations > 0 {
            writeln!(
                stdout,
                "{label:<8} {:<18} {bytes:>7} {:>9.1} µs {:>7.1} MB/s  {compressed}/{iterations}",
                transform.name(),
                seconds * 1e6 / iterations as f64,
                (bytes * iterations) as f64 / seconds / 1e6
            )?;
        } else {
            writeln!(
                stdout,
                "{label:<8} {:<18} {bytes:>7}   too fast to measure at this count",
                transform.name()
            )?;
        }
    }

    stdout.flush()?;
    Ok(())
}

/// `headroom deploy` — print a ready-to-run local deployment.
///
/// # Why this prints rather than starts anything
///
/// A `deploy` that daemonizes a process owns stopping it, restarting it on boot, and
/// rotating its logs — and does all three worse than the service manager already on the
/// machine. Printing the unit file, the compose service, and the plain command leaves
/// supervision where it belongs and works the same on a machine with no root.
///
/// # Errors
///
/// Returns an error if stdout cannot be written.
pub fn deploy(port: u16, upstream: &str) -> anyhow::Result<()> {
    let binary = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|dir| dir.join("headroom-proxy")))
        .map(|path| path.display().to_string())
        // Falls back to the bare name rather than guessing an install prefix. A wrong
        // absolute path in a unit file fails at boot, hours after anyone was watching.
        .unwrap_or_else(|| "headroom-proxy".to_owned());

    let mut stdout = std::io::stdout().lock();
    write!(stdout, "{}", deploy_manifests(port, upstream, &binary))?;
    stdout.flush()?;
    Ok(())
}

/// Renders the deployment manifests.
///
/// Separated from I/O so the security-relevant parts — the loopback port binding and
/// the restart policy — are testable rather than reviewed once and forgotten.
fn deploy_manifests(port: u16, upstream: &str, binary: &str) -> String {
    let mut out = String::new();

    out.push_str("# Run it directly\n");
    out.push_str(&format!(
        "HEADROOM_PORT={port} HEADROOM_UPSTREAM={upstream} {binary}\n\n"
    ));

    out.push_str("# systemd user unit — ~/.config/systemd/user/headroom.service\n");
    out.push_str("[Unit]\n");
    out.push_str("Description=headroom compressing proxy\n");
    out.push_str("After=network.target\n\n");
    out.push_str("[Service]\n");
    out.push_str(&format!("ExecStart={binary}\n"));
    out.push_str(&format!("Environment=HEADROOM_PORT={port}\n"));
    out.push_str(&format!("Environment=HEADROOM_UPSTREAM={upstream}\n"));
    // The proxy sits in the request path of everything routed through it: a crash that
    // is not restarted takes the customer's agents down with it.
    out.push_str("Restart=on-failure\n\n");
    out.push_str("[Install]\n");
    out.push_str("WantedBy=default.target\n\n");

    out.push_str("# docker compose service\n");
    out.push_str("services:\n");
    out.push_str("  headroom:\n");
    out.push_str("    image: rusty-headroom:latest\n");
    // Bound to loopback in the published port too. A bare `"PORT:PORT"` exposes an open
    // credential relay to the whole network the moment someone copies this onto a
    // server — the same mistake `Config::default` exists to avoid, reintroduced by a
    // deployment template.
    out.push_str(&format!("    ports: [\"127.0.0.1:{port}:{port}\"]\n"));
    out.push_str("    environment:\n");
    out.push_str(&format!("      HEADROOM_PORT: \"{port}\"\n"));
    out.push_str(&format!("      HEADROOM_UPSTREAM: \"{upstream}\"\n"));
    out.push_str("    restart: unless-stopped\n");

    out
}

/// `headroom update --check` — report the running version and where to look for a newer
/// one.
///
/// # Why there is no self-replacing upgrade
///
/// An in-place update means downloading a binary and overwriting the running one. Doing
/// that safely requires signature verification against a key this program would have to
/// ship and rotate; doing it unsafely turns any compromise of the release host — or any
/// machine on the path — into arbitrary code execution on every install.
///
/// A proxy that already holds provider credentials is the wrong program to give that
/// capability. The package manager that installed it can update it.
///
/// # Errors
///
/// Returns an error if stdout cannot be written.
pub fn update(check_only: bool) -> anyhow::Result<()> {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "installed     {}", env!("CARGO_PKG_VERSION"))?;
    writeln!(stdout, "source        {}", env!("CARGO_PKG_REPOSITORY"))?;

    if !check_only {
        writeln!(stdout)?;
        writeln!(
            stdout,
            "In-place upgrade is deliberately not implemented: this binary holds provider"
        )?;
        writeln!(
            stdout,
            "credentials, and a self-replacing updater turns a compromised release host into"
        )?;
        writeln!(
            stdout,
            "code execution on every install. Update through whatever installed it:"
        )?;
        writeln!(
            stdout,
            "  cargo install --git {}",
            env!("CARGO_PKG_REPOSITORY")
        )?;
    }

    stdout.flush()?;
    Ok(())
}

/// `headroom tools` — list what this build can actually do.
///
/// # Why introspection is worth a command
///
/// Which compressors exist, which content types route to them, and which MCP tools are
/// registered are all compile-time facts that a user otherwise learns by reading source
/// or by trial. A build that was compiled without a compressor, or with a different
/// detection threshold, should be able to say so itself.
///
/// # Errors
///
/// Returns an error if stdout cannot be written.
/// The compressor section of `headroom tools`, one line per line of output.
///
/// # Derived, not stated
///
/// This section used to be a hand-written list of four `(ContentType, &str)` pairs, with
/// a second list below it headed "detected but not compressed" naming code, prose and
/// unknown. Two of those three were wrong: code has compressed since its wiring was
/// fixed, and prose since the prose compressors were routed. So the command that exists
/// to say what this build can do told an operator that source files and prose tool
/// results are forwarded whole — the single largest category of agent traffic there is.
///
/// It is now read out of [`Orchestrator::for_type`] across [`ContentType::ALL`], so the
/// two lists cannot disagree with the pipeline or with each other: a type is in the
/// second list exactly when the orchestrator has no compressor for it.
fn compressor_table() -> Vec<String> {
    let orchestrator = Orchestrator::new(Arc::new(InMemoryCcrStore::new()));
    let sizer = AdaptiveSizer::default();

    let mut compressed = vec!["compressors".to_string()];
    let mut forwarded = Vec::new();

    for content_type in ContentType::ALL {
        match orchestrator.for_type(content_type) {
            Some(transform) => {
                // D24, asked rather than restated. Listing prose next to the others with
                // no qualification is the same error as the old list made in the other
                // direction: it would tell an operator their users' messages get
                // rewritten, which is the one thing this rule exists to prevent.
                let scope = if orchestrator.tool_output_only(content_type) {
                    "  tool output only"
                } else {
                    ""
                };
                compressed.push(format!(
                    "  {:<16} {:<20} min {} bytes{scope}",
                    content_type.as_str(),
                    transform.name(),
                    sizer.threshold(content_type)
                ));
            }
            // Listed explicitly as unhandled rather than omitted. A content type absent
            // from the output reads as "not detected"; one listed with no compressor
            // reads as "detected and forwarded", which is what actually happens.
            None => forwarded.push(format!("  {}", content_type.as_str())),
        }
    }

    compressed.push(String::new());
    compressed.push("detected but not compressed".to_string());
    compressed.extend(forwarded);
    compressed
}

pub fn tools() -> anyhow::Result<()> {
    let mut stdout = std::io::stdout().lock();

    for line in compressor_table() {
        writeln!(stdout, "{line}")?;
    }

    writeln!(stdout)?;
    writeln!(stdout, "mcp tools")?;
    for name in headroom_mcp::TOOL_NAMES {
        writeln!(stdout, "  {name}")?;
    }

    writeln!(stdout)?;
    writeln!(
        stdout,
        "lossless transforms (every auth mode except subscription)"
    )?;
    // Read out of `Reformatter::STEPS`, the array `Reformatter::apply` iterates. These
    // were two string literals — right by luck, which is the only thing that separated
    // them from the routing tables above that were not, and not a property that survives
    // somebody adding a third reformat.
    for (name, _) in Reformatter::STEPS {
        writeln!(stdout, "  {name}")?;
    }

    stdout.flush()?;
    Ok(())
}

/// `headroom init` — write a starter configuration file.
///
/// # Why it refuses to overwrite
///
/// A config file is something a user edits. `init` running a second time — in a script,
/// or because someone forgot they had run it — must not silently replace a file
/// somebody tuned. Refusing costs one error message; overwriting costs work nobody can
/// recover.
///
/// # Errors
///
/// Returns an error if the file already exists or cannot be written.
pub fn init(path: &std::path::Path, force: bool) -> anyhow::Result<()> {
    if path.exists() && !force {
        anyhow::bail!(
            "{} already exists; pass --force to replace it",
            path.display()
        );
    }

    let contents = concat!(
        "# headroom configuration\n",
        "#\n",
        "# Most of this is read live on every request, so a change takes effect without a\n",
        "# restart — which matters because a restart truncates in-flight streaming\n",
        "# responses. The exceptions are marked; they are read once at startup.\n",
        "\n",
        "# Where the proxy listens. Loopback by default: the proxy forwards provider\n",
        "# credentials, and binding every interface would expose an open relay.\n",
        "#\n",
        "# RESTART REQUIRED — the socket is bound once.\n",
        "HEADROOM_HOST=127.0.0.1\n",
        "HEADROOM_PORT=8787\n",
        "\n",
        "# The provider to forward to.\n",
        "#\n",
        "# RESTART REQUIRED — the relay client is built once with this URL baked in.\n",
        "# Changing it through /admin/runtime-env is stored and reported under\n",
        "# needs_restart; it does not repoint a running proxy.\n",
        "HEADROOM_UPSTREAM=https://api.anthropic.com\n",
        "\n",
        "# Set to 0 to forward everything untouched.\n",
        "HEADROOM_COMPRESSION=1\n",
        "\n",
        "# Output verbosity steering: terse, full, or unset.\n",
        "# Off by default — it changes what the model *writes*, which is a visible\n",
        "# change to your application rather than an invisible saving.\n",
        "# HEADROOM_OUTPUT_SHAPER=terse\n",
        "\n",
        "# Log verbosity. Defaults to warn: the proxy logs a line per request at info,\n",
        "# and a default that fills a terminal is one people turn off entirely.\n",
        "#\n",
        "# RESTART REQUIRED — the subscriber is installed once.\n",
        "# HEADROOM_LOG=headroom_proxy=info\n",
        "\n",
        "# Where compressed originals are kept so `headroom_retrieve` can return them.\n",
        "# Unset means memory only, and every marker becomes unredeemable on restart.\n",
        "# A shared store is what makes retrieval work across more than one worker.\n",
        "#\n",
        "# RESTART REQUIRED — the store is opened once.\n",
        "# HEADROOM_CCR_DIR=/var/lib/headroom/ccr\n",
        "# HEADROOM_REDIS_URL=redis://127.0.0.1:6379\n",
        "\n",
        "# Shapes measured as not worth compressing, written by `headroom learn`.\n",
        "#\n",
        "# RESTART REQUIRED — read once, deliberately: a set that changed between\n",
        "# requests would make the same request compress differently depending on when\n",
        "# it arrived, which is what busts the provider cache.\n",
        "# HEADROOM_RECOMMENDATIONS=/var/lib/headroom/recommendations.json\n",
        "\n",
        "# Facts to add to the newest message, as JSON lines. Tool output only.\n",
        "#\n",
        "# RESTART REQUIRED — read once, for the same reason as above.\n",
        "# HEADROOM_MEMORY=/var/lib/headroom/memories.jsonl\n",
        "\n",
        "# Normalize tool definitions and place cache breakpoints. Off by default: both\n",
        "# modify the cache hot zone, trading one miss now for hits later — worth it for\n",
        "# some deployments and pure cost for a client that already serializes stably.\n",
        "# HEADROOM_STABILIZE=0\n",
    );

    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;

    eprintln!("wrote {}", path.display());
    eprintln!("Apply it with:  set -a && . {} && set +a", path.display());
    Ok(())
}

/// `headroom learn` — aggregate request bodies and publish compression recommendations.
///
/// Reads newline-delimited request bodies from stdin.
///
/// # What this mines, and what it does not
///
/// The gap row calls for mining *failed sessions*. There is no session-log format in
/// this project for it to read, and inventing one so there is something to mine would be
/// building the easy half of the feature. What this does instead is real and useful: it
/// runs a corpus of request bodies through the same detection and compression the proxy
/// uses, aggregates the outcome by structural shape, and publishes what it learned.
///
/// The output is a configuration input read at startup, never consulted per request —
/// see [`headroom_core::telemetry`] for why that boundary is load-bearing.
///
/// # Errors
///
/// Returns an error if stdin cannot be read or the report cannot be written.
pub fn learn(min_samples: u64) -> anyhow::Result<()> {
    let corpus = read_stdin()?;
    let store = Arc::new(InMemoryCcrStore::new());
    let orchestrator = Orchestrator::new(store);
    let estimator = HeuristicEstimator::new();
    let policy = CompressionPolicy::for_mode(AuthMode::PayAsYouGo);

    let mut aggregator = Aggregator::new();
    let mut seen = 0usize;

    for line in corpus.lines().filter(|line| !line.trim().is_empty()) {
        seen += 1;
        let Ok(body) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        let model = body
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");

        for (content, kind) in compressible_content(&body) {
            let detected = detect(content.as_bytes()).content_type;
            let key = AggregationKey::new(
                AuthMode::PayAsYouGo,
                model,
                StructureHash::of(&content, detected),
            );

            let mut block = Block::new(kind, content.clone());
            match orchestrator.transform_for_block(&block, policy, model) {
                Some(transform) => match validated_apply(transform, &mut block, &estimator) {
                    Ok(outcome) if outcome.is_compressed() => aggregator.record(
                        &key,
                        estimator.count(&content) as u64,
                        estimator.count(block.content()) as u64,
                    ),
                    // A decline is data. A shape that consistently declines is one worth
                    // not attempting, and recording only successes would make every
                    // measured shape look worth compressing.
                    _ => aggregator.record_decline(&key),
                },
                None => aggregator.record_decline(&key),
            }
        }
    }

    let recommendations = aggregator.recommend(min_samples);

    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{}", recommendations.to_json()?)?;
    stdout.flush()?;

    eprintln!(
        "read {seen} requests, {} distinct shapes, {} met the {min_samples}-sample floor",
        aggregator.observations().len(),
        recommendations.entries.len()
    );
    Ok(())
}

/// Extracts every compressible string from a request body, tagged with the
/// [`BlockKind`] the live proxy would give it.
///
/// Message content only. A system prompt or a tool definition is in the cache hot zone
/// and never compressed, so measuring it would produce recommendations about content
/// the proxy will never act on.
///
/// # Why the kind is derived here rather than reused from the proxy
///
/// Regression (D24/D36/D37's failure mode, again): this used to hand every string
/// back as untagged `BlockKind::Text` and route it through `Orchestrator::transform_for`,
/// which skips the gate that keeps the lossy prose summarizer off content a person or
/// the model typed. `headroom-proxy` classifies exactly this shape correctly in
/// `compression.rs` — `role == "tool"` for OpenAI chat, the Responses item's `type`
/// field, and the Anthropic block's `type` field — but it is only a dev-dependency of
/// this crate (see `Cargo.toml`), not a production one, so its internals cannot be
/// called from here. The rules below mirror it instead.
fn compressible_content(body: &serde_json::Value) -> Vec<(String, BlockKind)> {
    let mut found = Vec::new();

    let messages = body
        .get("messages")
        .or_else(|| body.get("input"))
        .and_then(serde_json::Value::as_array);

    for message in messages.into_iter().flatten() {
        let role = message.get("role").and_then(serde_json::Value::as_str);
        let item_type = message.get("type").and_then(serde_json::Value::as_str);

        // A plain string body (OpenAI chat). `role: "tool"` is a tool result; any
        // other role is text a person or the model wrote, never lossily rewritten.
        if let Some(text) = message.get("content").and_then(serde_json::Value::as_str) {
            let kind = if role == Some("tool") {
                BlockKind::ToolResult
            } else {
                BlockKind::Text
            };
            found.push((text.to_owned(), kind));
        }

        // The OpenAI Responses item shape: a standalone item with no `role`, an
        // `output` string, and a `type` naming which kind of call it answers.
        if let Some(text) = message.get("output").and_then(serde_json::Value::as_str) {
            let kind = match item_type {
                Some("local_shell_call_output") => BlockKind::LocalShellCallOutput,
                Some("apply_patch_call_output") => BlockKind::ApplyPatchCallOutput,
                // `function_call_output` and anything else carrying an `output`
                // string: a Responses item with no `role` at all is tool-shaped by
                // construction, so this defaults to a tool result rather than text.
                _ => BlockKind::FunctionCallOutput,
            };
            found.push((text.to_owned(), kind));
        }

        // Anthropic's typed content-block array.
        if let Some(blocks) = message.get("content").and_then(serde_json::Value::as_array) {
            for block in blocks {
                let kind = if block.get("type").and_then(serde_json::Value::as_str)
                    == Some("tool_result")
                {
                    BlockKind::ToolResult
                } else {
                    BlockKind::Text
                };
                for key in ["content", "text"] {
                    if let Some(text) = block.get(key).and_then(serde_json::Value::as_str) {
                        found.push((text.to_owned(), kind));
                    }
                }
            }
        }
    }

    found
}

/// `headroom mcp install` / `headroom mcp uninstall`.
///
/// # Why the binary path is resolved rather than assumed
///
/// An MCP host launches the server as a subprocess. A bare `headroom-mcp` works only if
/// the host's `PATH` includes wherever this was installed — and a GUI application's
/// `PATH` is frequently not the shell's. Writing the absolute path of the binary sitting
/// beside this one is what makes the entry work when the host is not launched from a
/// terminal.
///
/// # Errors
///
/// Returns an error if the config cannot be read or written.
pub fn mcp_install(path: &std::path::Path, uninstall: bool) -> anyhow::Result<()> {
    if uninstall {
        if crate::wrap::uninstall_mcp_server(path)? {
            eprintln!("removed the headroom server from {}", path.display());
        } else {
            eprintln!(
                "no headroom server in {}; nothing to remove",
                path.display()
            );
        }
        return Ok(());
    }

    let command = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("headroom-mcp")))
        .filter(|candidate| candidate.exists())
        .map(|candidate| candidate.display().to_string())
        // Falls back to the bare name rather than writing a path that does not exist.
        // A host reporting "command not found" is clearer than one reporting a failure
        // to execute a file this command invented.
        .unwrap_or_else(|| "headroom-mcp".to_owned());

    if crate::wrap::install_mcp_server(path, &command)? {
        eprintln!("registered headroom in {}", path.display());
        eprintln!("  command: {command}");
        eprintln!("Restart the host for it to pick up the new server.");
    } else {
        // Not an error. A tuned entry is one somebody wrote deliberately.
        eprintln!(
            "{} already has a headroom server; leaving it as it is",
            path.display()
        );
    }

    Ok(())
}
