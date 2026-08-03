# Release Notes

One entry per merged PR against `main`, reverse chronological. No version tags
exist yet, so PRs are the unit of change; switch to `## vX.Y.Z` headers if and when
the crate starts publishing releases.

---

## PR #16 — Token validation with fallback to the original (invariant I5)
**2026-08-03** · [#16](https://github.com/baileyrd/rusty_headroom/pull/16) · closes [#8](https://github.com/baileyrd/rusty_headroom/issues/8)

- **Added:** `validated_apply` — if a compression does not reduce the token count,
  the original is forwarded. Wraps transform dispatch, so no compressor can opt out.
- **Behavior worth knowing:** equal token counts are treated as *not* an improvement
  and discarded. A compression saving zero tokens still costs a CCR entry and a
  possible retrieval round-trip.
- **Behavior worth knowing:** a transform that mutates a block and then declines has
  its partial mutation reverted unconditionally. Half-finished work never reaches
  upstream.
- **Behavior worth knowing:** invariant violations propagate rather than being
  absorbed by the fallback. Only *declined* and *malformed* outcomes are recoverable.
- **Known limitation:** counts come from the heuristic estimator, which is
  deliberately conservative, so some genuine compressions are declined that an exact
  tokenizer would accept. Safe direction, but savings are left on the table until the
  tiktoken and HuggingFace backends land.
- **Known limitation:** two tokenizer passes per attempted compression. Acceptable
  with the heuristic estimator; worth revisiting alongside an exact BPE tokenizer.

## PR #13 — CCR content addressing, marker format, and in-memory store
**2026-08-03** · [#13](https://github.com/baileyrd/rusty_headroom/pull/13) · closes [#9](https://github.com/baileyrd/rusty_headroom/issues/9), [#10](https://github.com/baileyrd/rusty_headroom/issues/10)

- **Added:** `ContentHash` (BLAKE3 truncated to 128 bits), the `<<ccr:HASH>>` marker
  format, the `CcrStore` trait, and an in-memory backend with TTL expiry.
- **Added:** `store_and_mark`, which stores content under exactly the hash its marker
  advertises — the two halves cannot drift apart.
- **Design note:** hashes derive from content alone, with no counter, timestamp, or
  session identifier. That is what makes markers replay-safe and keeps the provider's
  prompt cache hitting across identical requests.
- **Design note:** the store reads a clock for TTL, which does not conflict with
  invariant I4 — hashing never consults it, so the bytes sent upstream are unaffected.
- **Known limitation:** in-memory only; SQLite and Redis backends are still open.
- **Known limitation:** no eviction beyond TTL, and nothing schedules `purge_expired`
  yet. Expired entries correctly read as absent, but stay resident until something
  sweeps them.

## PR #12 — Block type and transform traits
**2026-08-03** · [#12](https://github.com/baileyrd/rusty_headroom/pull/12) · closes [#7](https://github.com/baileyrd/rusty_headroom/issues/7)

- **Added:** `Block` — sibling fields private, one mutable accessor. A transform
  cannot change what binds a tool result to the call it answers.
- **Added:** `Transform` as `fn(&mut Block) -> Result<()>`, which makes "reorder the
  content array", "split this block", and "add a field" unrepresentable rather than
  merely forbidden.
- **Added:** `LosslessTransform` / `LossyTransform` as separate traits, so the
  auth-mode policy gate is a type signature rather than a runtime flag.
- **Added:** `apply_guarded`, centralizing the refusal of signed, encrypted, and
  redacted blocks so the check exists in exactly one place.
- **Known limitation:** `Block` holds content as `String`. Byte-faithful passthrough
  at the proxy boundary will need `RawValue`-backed storage for untouched blocks.

## Parity loop — workspace foundation and the first core modules
**2026-08-03** · closes [#2](https://github.com/baileyrd/rusty_headroom/issues/2), [#3](https://github.com/baileyrd/rusty_headroom/issues/3), [#4](https://github.com/baileyrd/rusty_headroom/issues/4), [#5](https://github.com/baileyrd/rusty_headroom/issues/5), [#6](https://github.com/baileyrd/rusty_headroom/issues/6)

- **Added:** `gap-analysis.md` — a clean-room, documentation-derived assessment of
  the capability surface needed to reach parity with
  [headroomlabs-ai/headroom](https://github.com/headroomlabs-ai/headroom). 97 rows
  across 17 workstreams, dependency-ordered, with the reference's ten cache-safety
  invariants carried over as per-issue acceptance criteria.
- **Added (#2):** Cargo workspace with five crates — `headroom-core`,
  `headroom-proxy`, `headroom-mcp`, `headroom-cli`, `headroom-simulators`. MSRV
  1.80, edition 2021. `serde_json` is configured workspace-wide with
  `preserve_order` + `arbitrary_precision` + `raw_value`, which invariant I1
  (byte-faithful passthrough) is unimplementable without.
- **Added (#2):** `NOTICE` recording the clean-room derivation and crediting the
  reference project for the design.
- **Added (#3):** error taxonomy that separates *declined* compression from
  *malformed* input from *invariant violation*. The split is load-bearing:
  invariant I5's fallback path must recover from the first two and must never
  swallow the third.
- **Added (#4):** `Tokenizer` trait and a dependency-free heuristic estimator.
  Documented to never under-count — over-counting costs a missed compression,
  under-counting silently breaks the I5 guarantee and grows the user's prompt.
- **Added (#5):** `ContentRouter` — a pure `detect(bytes) -> Detection` classifying
  JSON, code, logs, diffs, search results, and prose, each with a confidence
  signal. Detection order is deliberate: a diff of a Rust file must classify as a
  diff, not as code.
- **Added (#6):** `AdaptiveSizer` with the documented per-type floors (code > 2 KB,
  JSON > 1 KB, logs > 500 B, text > 5 KB), boundaries exclusive.
- **Testing:** 36 unit tests and 3 doc-tests. Each detector carries both a positive
  case and a near-miss (a Markdown rule that is not a diff, prose containing
  "error" that is not a log, prose with colons that is not grep output).
- **Known limitation:** `headroom-proxy`, `headroom-mcp`, `headroom-cli`, and
  `headroom-simulators` are documented stubs. They compile and carry their module
  docs, but the handlers, MCP tools, and subcommands land in later issues.
- **Known limitation:** the tokenizer is heuristic only. Exact tiktoken (#T2) and
  HuggingFace (#T3) backends are filed but not yet implemented, so token counts are
  conservative approximations rather than exact.

---

## PR #1 — Bootstrap the repo with the standard governance file set
**2026-08-03** · [#1](https://github.com/baileyrd/rusty_headroom/pull/1)

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
