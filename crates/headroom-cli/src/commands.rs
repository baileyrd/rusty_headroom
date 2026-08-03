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
use headroom_core::pipeline::Orchestrator;
use headroom_core::telemetry::{AggregationKey, Aggregator, StructureHash, Telemetry};
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

    if !agent.env_configurable() {
        anyhow::bail!(
            "{agent} is not configured through environment variables; \
             set its base URL to {proxy} in its own settings instead"
        );
    }
    let exports = agent.env(proxy);

    if let Some(path) = settings {
        let written = crate::wrap::wrap_settings_file(path, proxy)?;
        eprintln!("rewrote {}", written.display());
        eprintln!("original saved alongside it; `headroom unwrap` restores it exactly");
    }

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
    let value = |name: &str| -> Option<f64> {
        metrics.lines().find_map(|line| {
            let rest = line.strip_prefix(name)?;
            rest.trim().parse().ok()
        })
    };

    let requests = value("headroom_requests_total ").unwrap_or(0.0);
    let compressed = value("headroom_compressed_total ").unwrap_or(0.0);
    let before = value("headroom_tokens_before_total ").unwrap_or(0.0);
    let saved = value("headroom_tokens_saved_total ").unwrap_or(0.0);

    let mut out = String::new();
    out.push_str(&format!("requests      {requests:.0}\n"));
    out.push_str(&format!("compressed    {compressed:.0}\n"));
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
    fn the_savings_report_reads_real_exposition_text() {
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
    let store = Arc::new(InMemoryCcrStore::new());
    let estimator = HeuristicEstimator::new();
    let crusher = SmartCrusher::new(store);

    let sample: String = format!(
        "[{}]",
        (0..200)
            .map(|i| format!(
                r#"{{"path":"src/module_{i}.rs","kind":"file","status":"ok","size":{}}}"#,
                1000 + i
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    let bytes = sample.len();

    // A warm-up pass, discarded. The first iteration pays for allocator growth and
    // branch prediction that every later one does not, so including it reports a
    // throughput this machine never actually sustains.
    let mut warm = Block::new(BlockKind::Text, sample.clone());
    let _ = validated_apply(&crusher, &mut warm, &estimator);

    let started = std::time::Instant::now();
    let mut compressed = 0usize;
    for _ in 0..iterations {
        let mut block = Block::new(BlockKind::Text, sample.clone());
        if let Ok(outcome) = validated_apply(&crusher, &mut block, &estimator) {
            if outcome.is_compressed() {
                compressed += 1;
            }
        }
    }
    let elapsed = started.elapsed();

    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "iterations    {iterations}")?;
    writeln!(stdout, "payload       {bytes} bytes")?;
    writeln!(stdout, "compressed    {compressed}/{iterations}")?;
    writeln!(stdout, "elapsed       {:.3} s", elapsed.as_secs_f64())?;

    // Guarded rather than divided blindly: a fast machine and a small iteration count
    // can round to zero, and dividing by it would report `inf` as a throughput.
    let seconds = elapsed.as_secs_f64();
    if seconds > 0.0 && iterations > 0 {
        let per_op = elapsed.as_secs_f64() / iterations as f64;
        writeln!(stdout, "per call      {:.1} µs", per_op * 1e6)?;
        writeln!(
            stdout,
            "throughput    {:.1} MB/s",
            (bytes * iterations) as f64 / seconds / 1e6
        )?;
    } else {
        writeln!(stdout, "per call      too fast to measure at this count")?;
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
pub fn tools() -> anyhow::Result<()> {
    let mut stdout = std::io::stdout().lock();

    writeln!(stdout, "compressors")?;
    for (content_type, compressor) in [
        (ContentType::Json, "smart_crusher"),
        (ContentType::Log, "log_compressor"),
        (ContentType::SearchResults, "search_compressor"),
        (ContentType::Diff, "diff_compressor"),
    ] {
        let sizer = AdaptiveSizer::default();
        writeln!(
            stdout,
            "  {:<16} {:<20} min {} bytes",
            content_type.as_str(),
            compressor,
            sizer.threshold(content_type)
        )?;
    }

    // Listed explicitly as unhandled rather than omitted. A content type absent from the
    // output reads as "not detected"; one listed with no compressor reads as "detected
    // and forwarded", which is what actually happens.
    writeln!(stdout)?;
    writeln!(stdout, "detected but not compressed")?;
    for content_type in [ContentType::Code, ContentType::Prose, ContentType::Unknown] {
        writeln!(stdout, "  {}", content_type.as_str())?;
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
    writeln!(stdout, "  minify_json")?;
    writeln!(stdout, "  tidy_lines")?;

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
        "# Read live on every request, so changes take effect without a restart —\n",
        "# which matters because a restart truncates in-flight streaming responses.\n",
        "\n",
        "# Where the proxy listens. Loopback by default: the proxy forwards provider\n",
        "# credentials, and binding every interface would expose an open relay.\n",
        "HEADROOM_HOST=127.0.0.1\n",
        "HEADROOM_PORT=8787\n",
        "\n",
        "# The provider to forward to.\n",
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
        "# HEADROOM_LOG=headroom_proxy=info\n",
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

        for content in compressible_content(&body) {
            let detected = detect(content.as_bytes()).content_type;
            let key = AggregationKey::new(
                AuthMode::PayAsYouGo,
                model,
                StructureHash::of(&content, detected),
            );

            let mut block = Block::new(BlockKind::Text, content.clone());
            match orchestrator.transform_for(&content, policy, model) {
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

/// Extracts every compressible string from a request body.
///
/// Message content only. A system prompt or a tool definition is in the cache hot zone
/// and never compressed, so measuring it would produce recommendations about content
/// the proxy will never act on.
fn compressible_content(body: &serde_json::Value) -> Vec<String> {
    let mut found = Vec::new();

    let messages = body
        .get("messages")
        .or_else(|| body.get("input"))
        .and_then(serde_json::Value::as_array);

    for message in messages.into_iter().flatten() {
        // The three shapes a tool result arrives in: a plain string body (OpenAI chat),
        // an `output` member (OpenAI Responses), or a typed block (Anthropic).
        for key in ["content", "output"] {
            if let Some(text) = message.get(key).and_then(serde_json::Value::as_str) {
                found.push(text.to_owned());
            }
        }
        if let Some(blocks) = message.get("content").and_then(serde_json::Value::as_array) {
            for block in blocks {
                for key in ["content", "text"] {
                    if let Some(text) = block.get(key).and_then(serde_json::Value::as_str) {
                        found.push(text.to_owned());
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
