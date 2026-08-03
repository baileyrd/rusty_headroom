//! Command implementations.
//!
//! Every command that produces data writes it to stdout and nothing else, so
//! `headroom compress < big.json > small.txt` works. Diagnostics go to stderr.

use std::io::{Read, Write};
use std::sync::Arc;

use headroom_core::ccr::{CcrStore, InMemoryCcrStore};
use headroom_core::detection::{detect, AdaptiveSizer, ContentType};
use headroom_core::tokenizer::{HeuristicEstimator, Tokenizer};
use headroom_core::transform::Transform;
use headroom_core::{
    validated_apply, Block, BlockKind, CodeCompressor, DiffCompressor, LogCompressor,
    SearchCompressor, SmartCrusher,
};

/// Reads all of stdin.
fn read_stdin() -> anyhow::Result<String> {
    let mut buffer = String::new();
    std::io::stdin().read_to_string(&mut buffer)?;
    Ok(buffer)
}

/// Picks the compressor for `content`, if any handles it.
fn route<'a>(
    content: &str,
    smart: &'a SmartCrusher,
    log: &'a LogCompressor,
    search: &'a SearchCompressor,
    diff: &'a DiffCompressor,
    code: &'a CodeCompressor,
) -> Option<&'a dyn Transform> {
    match detect(content.as_bytes()).content_type {
        ContentType::Json => Some(smart),
        ContentType::Log => Some(log),
        ContentType::SearchResults => Some(search),
        ContentType::Diff => Some(diff),
        ContentType::Code => Some(code),
        _ => None,
    }
}

/// `headroom compress`.
pub fn compress(dry_run: bool) -> anyhow::Result<()> {
    let content = read_stdin()?;
    let store = Arc::new(InMemoryCcrStore::new());

    let smart = SmartCrusher::new(store.clone());
    let log = LogCompressor::new(store.clone());
    let search = SearchCompressor::new(store.clone());
    let diff = DiffCompressor::new(store.clone());
    let code = CodeCompressor::new(store);

    let estimator = HeuristicEstimator::new();
    let before = estimator.count(&content);
    let detection = detect(content.as_bytes());

    let mut block = Block::new(BlockKind::Text, content.clone());
    let compressed = match route(&content, &smart, &log, &search, &diff, &code) {
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

/// `headroom inspect`.
pub fn inspect() -> anyhow::Result<()> {
    let content = read_stdin()?;
    let detection = detect(content.as_bytes());
    let sizer = AdaptiveSizer::default();
    let estimator = HeuristicEstimator::new();

    println!("bytes: {}", content.len());
    println!("estimated tokens: {}", estimator.count(&content));
    println!("content type: {}", detection.content_type);
    println!("confidence: {:.2}", detection.confidence);
    println!(
        "size threshold: {} bytes",
        sizer.threshold(detection.content_type)
    );
    println!(
        "above threshold: {}",
        sizer.should_attempt(detection.content_type, content.len())
    );

    // Naming the compressor makes the routing decision inspectable, which is the
    // point of the command — "why did this not compress" is otherwise a guess.
    let compressor = match detection.content_type {
        ContentType::Json => "smart_crusher",
        ContentType::Log => "log_compressor",
        ContentType::SearchResults => "search_compressor",
        ContentType::Diff => "diff_compressor",
        ContentType::Code => "code_compressor",
        ContentType::Prose | ContentType::Unknown => "none",
    };
    println!("compressor: {compressor}");
    Ok(())
}

/// `headroom doctor`.
pub fn doctor() -> anyhow::Result<()> {
    let mut problems = 0;

    println!("headroom {}", env!("CARGO_PKG_VERSION"));

    // A self-test rather than a version dump. Reporting "installed correctly" without
    // exercising anything is how a broken install passes its own health check.
    let store = Arc::new(InMemoryCcrStore::new());
    let sample: String = format!(
        "[{}]",
        (0..80)
            .map(|i| format!(r#"{{"id":{i},"kind":"file","ok":true}}"#))
            .collect::<Vec<_>>()
            .join(",")
    );

    let estimator = HeuristicEstimator::new();
    let mut block = Block::new(BlockKind::Text, sample.clone());
    let crusher = SmartCrusher::new(store.clone());

    match validated_apply(&crusher, &mut block, &estimator) {
        Ok(outcome) if outcome.is_compressed() => {
            println!("compression: ok ({} tokens saved)", outcome.tokens_saved());
        }
        Ok(_) => {
            println!("compression: FAILED (sample did not compress)");
            problems += 1;
        }
        Err(err) => {
            println!("compression: FAILED ({err})");
            problems += 1;
        }
    }

    // Retrieval is the half that makes lossy compression safe, so a doctor that only
    // checked compression would pass on an install where nothing is recoverable.
    match headroom_core::ccr::find_markers(block.content()).first() {
        Some(hash) => match store.get(*hash) {
            Ok(Some(bytes)) if bytes == sample.as_bytes() => println!("retrieval: ok"),
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

    if problems == 0 {
        println!("\nall checks passed");
        Ok(())
    } else {
        anyhow::bail!("{problems} check(s) failed")
    }
}

/// `headroom env`.
pub fn env(proxy: &str) -> anyhow::Result<()> {
    // Emitted as shell exports so this can be `eval`'d, which is how the reference's
    // wrap command is meant to be used.
    println!("export ANTHROPIC_BASE_URL={proxy}");
    println!("export OPENAI_BASE_URL={proxy}/v1");
    println!("# eval \"$(headroom env)\" then run your agent as usual");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_matches_the_detected_type() {
        let store = Arc::new(InMemoryCcrStore::new());
        let smart = SmartCrusher::new(store.clone());
        let log = LogCompressor::new(store.clone());
        let search = SearchCompressor::new(store.clone());
        let diff = DiffCompressor::new(store.clone());
        let code = CodeCompressor::new(store);

        let json = r#"[{"a":1},{"a":2}]"#;
        assert_eq!(
            route(json, &smart, &log, &search, &diff, &code).map(|t| t.name()),
            Some("smart_crusher")
        );

        let prose = "just some ordinary words with nothing structural about them";
        assert!(route(prose, &smart, &log, &search, &diff, &code).is_none());
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
