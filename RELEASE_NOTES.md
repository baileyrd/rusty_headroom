# Release Notes

One entry per merged PR against `main`, reverse chronological. No version tags
exist yet, so PRs are the unit of change; switch to `## vX.Y.Z` headers if and when
the crate starts publishing releases.

---

## Bootstrap the repo with the standard governance file set
**2026-08-03**

- **Added:** the full standard governance set — four PR templates (feature,
  bug_fix, docs, chore), two issue templates plus `config.yml`, README,
  CONTRIBUTING, CODE_OF_CONDUCT, SECURITY, CHANGELOG, this file, ARCHITECTURE, and
  an ADR seed at `docs/adr/0001-template.md`. The repo was empty before this — no
  commits, no source, no scaffolding.
- **Added:** `.github/workflows/ci-rust.yml` running `cargo fmt --check`, `cargo
  clippy -D warnings`, and `cargo test --all-features`. Applied deliberately ahead
  of any manifest so the gate exists before the first line of code rather than
  being retrofitted.
- **Known limitation, stated plainly:** CI will fail on every run until a
  `Cargo.toml` lands — there is nothing for cargo to build. This was a conscious
  tradeoff, not an oversight.
- **Known limitation:** README's one-line description and ARCHITECTURE's boundary
  table are placeholders. Neither could be filled honestly against an empty repo,
  and inventing content for them would be worse than leaving the gap visible.
- **Manual follow-up:** the CI check only gates merges once it's set as a required
  status check under branch protection — that's a GitHub settings change, not
  something this commit can do.
