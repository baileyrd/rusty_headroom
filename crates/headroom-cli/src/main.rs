//! The `headroom` command-line interface.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod commands;
mod wrap;

use clap::{Parser, Subcommand};

/// Context compression for AI agents.
#[derive(Debug, Parser)]
#[command(name = "headroom", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
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
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    let outcome = match cli.command {
        Command::Doctor => commands::doctor(),
        Command::Compress { dry_run } => commands::compress(dry_run),
        Command::Inspect => commands::inspect(),
        Command::Env { proxy } => commands::env(&proxy),
        Command::Wrap {
            agent,
            proxy,
            settings,
        } => commands::wrap(&agent, &proxy, settings.as_deref()),
        Command::Unwrap { agent, settings } => commands::unwrap(&agent, settings.as_deref()),
        Command::Savings => commands::savings(),
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
