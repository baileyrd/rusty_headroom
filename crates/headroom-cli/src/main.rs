//! The `headroom` command-line interface.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod commands;
mod wrap;

use clap::{Parser, Subcommand, ValueEnum};

/// Context compression for AI agents.
#[derive(Debug, Parser)]
#[command(name = "headroom", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Where content handed to `headroom compress` came from.
///
/// Only the prose summarizer distinguishes them; every other content type compresses the
/// same either way. See `Command::Compress`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Kind {
    /// A command's output, a scraped page, a file dump. The default, because it is what
    /// somebody piping into this command is nearly always holding.
    ToolOutput,
    /// Something a person wrote. The prose summarizer declines it.
    Text,
}

impl From<Kind> for headroom_core::BlockKind {
    fn from(kind: Kind) -> Self {
        match kind {
            Kind::ToolOutput => Self::ToolResult,
            Kind::Text => Self::Text,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Check that the local install is working.
    Doctor,
    /// Compress content read from stdin and write the result to stdout.
    Compress {
        /// Report what would happen without emitting the compressed form.
        #[arg(long)]
        dry_run: bool,
        /// Where the content came from.
        ///
        /// One compressor — the prose summarizer — runs only on tool output, because
        /// `text` is what somebody typed and summarizing a person's own words is a
        /// different act from summarizing a command's output. This store is discarded
        /// when the process exits, so that summary would not be recoverable.
        #[arg(long, value_enum, default_value_t = Kind::ToolOutput)]
        kind: Kind,
    },
    /// Show what a piece of content is detected as, and why.
    Inspect,
    /// Print the environment an agent needs to route through the proxy.
    Env {
        /// Proxy address to point at.
        #[arg(long, default_value = "http://127.0.0.1:8787")]
        proxy: String,
    },
    /// Point an agent at the proxy.
    Wrap {
        /// Agent name — claude, codex, cursor, aider, cline, continue, goose, openhands.
        agent: String,
        /// Proxy address to point at.
        #[arg(long, default_value = "http://127.0.0.1:8787")]
        proxy: String,
        /// A JSON settings file to rewrite, backed up so `unwrap` restores it exactly.
        #[arg(long)]
        settings: Option<std::path::PathBuf>,
    },
    /// Undo `wrap`, restoring any settings file byte for byte.
    Unwrap {
        /// Agent name.
        agent: String,
        /// The settings file `wrap` rewrote.
        #[arg(long)]
        settings: Option<std::path::PathBuf>,
    },
    /// Summarize what the proxy has saved, reading its `/metrics` from stdin.
    Savings,
    /// Measure compression throughput on this machine.
    Perf {
        /// How many compressions to time.
        #[arg(long, default_value_t = 200)]
        iterations: usize,
    },
    /// Print a ready-to-run local deployment.
    Deploy {
        /// Port the proxy should listen on.
        #[arg(long, default_value_t = 8787)]
        port: u16,
        /// Provider to forward to.
        #[arg(long, default_value = "https://api.anthropic.com")]
        upstream: String,
    },
    /// Report the installed version and where to get a newer one.
    Update {
        /// Only report the version; do not explain the upgrade path.
        #[arg(long)]
        check: bool,
    },
    /// List the compressors, content types and MCP tools this build carries.
    Tools,
    /// Write a starter configuration file.
    Init {
        /// Where to write it.
        #[arg(long, default_value = ".headroom.env")]
        path: std::path::PathBuf,
        /// Replace an existing file.
        #[arg(long)]
        force: bool,
    },
    /// Register the headroom MCP server in a host's config.
    Mcp {
        /// The host's MCP config file.
        #[arg(long)]
        config: std::path::PathBuf,
        /// Remove the entry instead of adding it.
        #[arg(long)]
        uninstall: bool,
    },
    /// Aggregate request bodies from stdin and publish compression recommendations.
    Learn {
        /// How many observations a shape needs before it is published.
        #[arg(long, default_value_t = 5)]
        min_samples: u64,
    },
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    let outcome = match cli.command {
        Command::Doctor => commands::doctor(),
        Command::Compress { dry_run, kind } => commands::compress(dry_run, kind.into()),
        Command::Inspect => commands::inspect(),
        Command::Env { proxy } => commands::env(&proxy),
        Command::Wrap {
            agent,
            proxy,
            settings,
        } => commands::wrap(&agent, &proxy, settings.as_deref()),
        Command::Unwrap { agent, settings } => commands::unwrap(&agent, settings.as_deref()),
        Command::Savings => commands::savings(),
        Command::Perf { iterations } => commands::perf(iterations),
        Command::Deploy { port, upstream } => commands::deploy(port, &upstream),
        Command::Update { check } => commands::update(check),
        Command::Tools => commands::tools(),
        Command::Init { path, force } => commands::init(&path, force),
        Command::Mcp { config, uninstall } => commands::mcp_install(&config, uninstall),
        Command::Learn { min_samples } => commands::learn(min_samples),
    };

    match outcome {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            // Diagnostics to stderr so `headroom compress` stays pipeable.
            eprintln!("headroom: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}
