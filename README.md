# rusty_headroom

<!-- One or two sentences: what this is and why it exists. -->
<one-line description — fill in once the project's shape is settled>

## Status
Experimental — no code yet. This repo currently contains governance scaffolding
only; the first real commit of source hasn't landed. Owner: @baileyrd.

## Getting started
```bash
git clone https://github.com/baileyrd/rusty_headroom.git
cd rusty_headroom
# Nothing to build yet — no Cargo.toml. Replace this block once `cargo init` runs.
```

## Architecture
See [ARCHITECTURE.md](./ARCHITECTURE.md) for boundaries, key decisions, and data flow.

## Development
The repo is set up to be a Rust crate — CI (`.github/workflows/ci-rust.yml`) already
runs these three, and will fail until a `Cargo.toml` exists. That's deliberate: the
gate is in place before the first line of code rather than bolted on after.

```bash
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all
```

## Contributing
See [CONTRIBUTING.md](./CONTRIBUTING.md).

## Security
See [SECURITY.md](./SECURITY.md) to report a vulnerability.

## License
Internal — not for external distribution
