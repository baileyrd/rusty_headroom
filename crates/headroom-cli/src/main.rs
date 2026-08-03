//! The `headroom` command-line interface.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod commands;

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
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    let outcome = match cli.command {
        Command::Doctor => commands::doctor(),
        Command::Compress { dry_run } => commands::compress(dry_run),
        Command::Inspect => commands::inspect(),
        Command::Env { proxy } => commands::env(&proxy),
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
