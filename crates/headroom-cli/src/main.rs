//! The `headroom` command-line interface.
//!
//! Subcommands (`proxy`, `doctor`, `wrap`, `perf`, `learn`, `update`, `mcp`) land in
//! the L-series issues.

#![forbid(unsafe_code)]

fn main() {
    println!(
        "headroom {} — subcommands land in the L-series issues",
        env!("CARGO_PKG_VERSION")
    );
}
