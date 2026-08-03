# Release Notes

One entry per merged PR against `main`, reverse chronological. No version tags
exist yet, so PRs are the unit of change; switch to `## vX.Y.Z` headers if and when
the crate starts publishing releases.

---

## SmartCrusher outlier detection — keep what stands out
**2026-08-03** · closes [#22](https://github.com/baileyrd/rusty_headroom/issues/22)

- **Added:** `rank_outliers` — ranks records by how much they stand out, most
  anomalous first, with an `OutlierReason` explaining each contribution.
- **Why it matters:** summarizing 500 near-identical records is only safe if the ones
  that are *not* near-identical survive. The interesting record in tool output is
  almost always the anomalous one — the failed test among 200 passes, the file with a
  permission error. Compressing that away yields output that is smaller, cheaper, and
  useless.
- **Signals:** rare values of a repeated field, a field the record's peers lack,
  error-shaped fields (double weight), numeric outliers, and size outliers.
- **Design note:** scores are fixed-point integers, not floats. Ranking must be
  deterministic down to tie-breaking, and an integer score has a total order by
  construction with no `NaN` to decide about. Ties break on record index (I4).
- **Design note:** numeric outliers use median and median-absolute-deviation rather
  than mean and standard deviation. Tool-output distributions are routinely skewed,
  and a single extreme value drags the mean toward itself, masking the very outlier it
  should expose.
- **Design note:** error field names match whole words, case-insensitively. Substring
  matching would fire on `error_rate` and rank ordinary telemetry as anomalous.
- **Design note:** records that stand out in no way are omitted rather than ranked
  last, so a genuinely uniform array yields nothing instead of an arbitrary pick.
- **Known limitation:** `Unique` and `Constant` fields contribute no signal — the
  first distinguishes every record equally, the second none. Correct, but it means a
  record anomalous *only* in an identifier field goes unflagged.
- **Known limitation:** size scoring serializes every record, which is a full pass
  over the document on top of analysis.
- **Known limitation:** still analysis. Nothing consumes these rankings yet — anchor
  selection and the compaction formatter remain open.

## SmartCrusher analyzer — classification and field statistics
**2026-08-03** · closes [#20](https://github.com/baileyrd/rusty_headroom/issues/20)

- **Added:** `classify(&Shape, &CrushConfig) -> Pattern` naming the overall pattern —
  `RecordSet`, `ScalarHeavy`, `WideObject`, `DeepNest`, or `Unremarkable`.
- **Added:** `analyze_record_set` computing per-field statistics across an array of
  objects — `Constant`, `LowCardinality`, `Unique`, or `Varied`, each with a
  `present_in` count.
- **Added:** `Shape::depth` and `Shape::string_bytes`, and three `CrushConfig`
  knobs (`max_low_cardinality`, `wide_object_fields`, `scalar_heavy_bytes`).
- **Correctness note:** a field is reported `Constant` only when it is present in
  *every* record **and** equal in every record. A field that is uniform where present
  but absent from one record is optional, not constant — reporting it constant would
  tell the model every record carries it, which is false. Asserting something untrue
  to the model is worse than not compressing.
- **Design note:** `Unique` fields are identifiers and are never elided. They are how
  the model refers back to a specific record, so summarizing them away costs it the
  ability to ask about anything it can see.
- **Design note:** classification and analysis disagree on strictness deliberately.
  `Shape::is_record_set` treats one odd record as making the array heterogeneous;
  `analyze_record_set` still analyzes it, because an array where one record carries
  `error` and the rest do not is exactly what field statistics exist to surface.
- **Design note:** cardinality keys on the serialized form of each value, so `1` and
  `"1"` stay distinct. Counting accumulates in a `BTreeMap` — sorting is safe here
  because these counts drive decisions rather than output ordering.
- **Known limitation:** statistics are refused for arrays mixing objects and scalars.
  Analyzing only the object elements would produce numbers that read as though they
  described the whole array.
- **Known limitation:** analysis only. Nothing acts on these findings yet — anchor
  selection, planning, and the compaction formatter are still open, so no JSON is
  compressed.

## SmartCrusher foundations — config and structural IR
**2026-08-03** · closes [#15](https://github.com/baileyrd/rusty_headroom/issues/15)

- **Added:** `CrushConfig` — tuning for JSON compression, with documented defaults
  aimed at the shape that dominates agent tool output: an array of many
  near-identical records.
- **Added:** `Document` and `Shape` — the IR that analysis, planning, and formatting
  share. The document is order- and literal-preserving `serde_json::Value`; the shape
  is a structural summary derived from it. Keeping them separate means a planning bug
  cannot corrupt data.
- **Design note:** object fields are held in `Vec<(String, Shape)>`, not a
  `BTreeMap`. A `BTreeMap` would be deterministic but would silently sort keys,
  changing the bytes sent upstream. The `Vec` is deterministic *and* order-preserving.
  No `HashMap` appears on any path influencing output (invariant I4).
- **Design note:** an array with one odd record out is treated as heterogeneous.
  Calling it homogeneous would let the record that differs be summarized away as
  ordinary — and that record is usually the one worth reading.
- **Design note:** `CrushConfig::max_depth` bounds analysis recursion. Tool output is
  not trusted input, and unbounded recursion over it is a stack overflow waiting to
  happen.
- **Known limitation:** compact JSON round-trips byte-exactly, but insignificant
  whitespace does not — pretty-printed input comes back compact. Safe, because this
  path is only reached for documents SmartCrusher is actually rewriting; a declined
  document is restored from the caller's untouched original by the I5 fallback, which
  never re-serializes.
- **Known limitation:** foundations only. Analysis, statistics, outlier detection,
  anchor selection, and the compaction formatter are still open, so no JSON is
  actually compressed yet.

## Live-zone dispatcher (invariants I2, I3)
**2026-08-03** · closes [#14](https://github.com/baileyrd/rusty_headroom/issues/14)

- **Added:** `Conversation`, `Message`, and `Role` — the conversation model. `system`
  and `tools` are exposed immutably and have no mutable accessor at all, so the
  compression path cannot reach the cache hot zone. Invariant I2 becomes a function
  that does not exist rather than a rule to remember.
- **Added:** `live_zone()` — computes which blocks are eligible for compression by
  scanning from the tail: the newest user message's text, plus the newest instance of
  each tool-output shape.
- **Added:** `BlockKind::FunctionCallOutput`, `LocalShellCallOutput`, and
  `ApplyPatchCallOutput`, the OpenAI Responses output shapes named in the live-zone
  definition.
- **Design note:** the newest-instance rule is applied *on top of*
  `frozen_message_count`, not instead of it. A message can sit above the floor and
  still have been sent upstream already. Compressing too little costs tokens;
  compressing too much invalidates a cached prefix, costing tokens *and* context,
  silently. The failure directions are not symmetric.
- **Bug found and fixed during development:** an early version treated "the latest
  user text" as its own category, which reached back to prose several turns old
  whenever the newest user message carried only tool results — the exact
  cache-busting this module exists to prevent. Corrected to "the text of the latest
  user message", with tests covering both directions.
- **Known limitation:** `frozen_message_count` is supplied by the caller. Nothing
  derives it from `cache_control` markers yet, so today every caller passes `0` and
  the newest-instance rule is doing all the work.
- **Known limitation:** no compressor is wired to the dispatcher yet. It computes the
  eligible set and applies a closure; routing to type-aware compressors arrives with
  the pipeline orchestrator.

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
