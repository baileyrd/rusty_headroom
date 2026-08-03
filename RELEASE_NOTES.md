# Release Notes

One entry per merged PR against `main`, reverse chronological. No version tags
exist yet, so PRs are the unit of change; switch to `## vX.Y.Z` headers if and when
the crate starts publishing releases.

---

## Text compression, persistent CCR, and the retrieval tool
**2026-08-03** · gap rows C10, R3, R5

- **Added:** `TextCrusher` (lossless whitespace normalization) and `TextSummarizer`
  (lossy line dropping). Split deliberately — the lossless pass is safe on the auth
  modes that forbid lossy transforms, so it must be a separate type (invariant I10).
- **Added:** `FileCcrStore`, a persistent CCR backend. Content survives a proxy
  restart, so a model asking for content it was promised is not told it is gone.
- **Added:** `ccr_retrieve` tool definition and handler — the mechanism that makes lossy
  compression reversible from the model's side.
- **Design note:** the retrieval tool must be registered on **every** request, not only
  when something was compressed. The tools array is part of the cached prompt prefix; a
  tool that appears and disappears invalidates the cache on every state flip. A fixed
  handful of tokens once beats a full cache miss each time compression starts or stops.
- **Design note:** `Retrieval` distinguishes expired from malformed, because the model
  should be told different things — one means "this existed and is gone", the other
  "check what you sent".
- **Design note:** the file store writes to a temporary name and renames into place, so
  a reader never observes a half-written entry and hands a model truncated content
  while calling it the original.
- **Fixed:** an MSRV violation caught by clippy — `is_none_or` is stable from 1.82 and
  this crate declares 1.80.
- **Known limitation:** `FileCcrStore` substitutes for the SQLite backend gap row R3;
  see `DECISIONS.md` D6. The Redis backend (R4) remains deliberately unimplemented.
- **Known limitation:** nothing injects the `ccr_retrieve` tool into outgoing requests
  yet. The definition exists and is tested; wiring it into the proxy's tools array is
  still open.

## Signals and the diff compressor
**2026-08-03** · gap rows S1-S3, C8

- **Added:** `signals` — keyword scoring and tiered line importance, factored out so
  every line-oriented compressor makes the same keep/drop judgment rather than
  re-inventing it slightly differently.
- **Added:** `DiffCompressor` — elides unchanged context, keeping hunk headers, every
  changed line, and two lines of surrounding context.
- **Design note:** every signal heuristic leans toward keeping a line. One wrongly kept
  costs a few tokens; one wrongly dropped may be the error being looked for.
- **Design note:** hunk headers are never elided. They carry the line numbers, and a
  diff without them cannot be located against a file.
- **Design note:** `keep_most_important` breaks ties on source index, so an all-routine
  input produces the same selection every run rather than depending on sort stability.
- **Added:** `DECISIONS.md`, logging choices taken autonomously — batching gap rows into
  PRs, skipping the Redis backend, heuristic rather than tree-sitter code compression,
  and deferring the Python bindings.
- **Known limitation:** signals are English-keyword based. A non-English log gets no
  keyword signal and falls back to structural cues alone.

## Live-zone compression on the wire — /v1/messages
**2026-08-03** · closes [#35](https://github.com/baileyrd/rusty_headroom/issues/35), [#36](https://github.com/baileyrd/rusty_headroom/issues/36)

- **Added (#35):** `frozen_message_count` — derives the live-zone floor from customer
  `cache_control` markers, on a message or on a content block, last breakpoint wins.
- **Added (#36):** `compress_request`, a pure function over bytes running the whole
  pipeline, plus `POST /v1/messages` and a content-type dispatcher over the three
  compressors.
- **Added:** `SmartCrusher` re-exported at the `headroom-core` root, matching
  `LogCompressor` and `SearchCompressor`.
- **The guarantee, tested:** on a request with system, tools, five historical turns
  and a bulky live tool result, the hot zone and every frozen turn come back
  SHA-256-identical while the live result shrinks by more than half. The frozen prefix
  is asserted to appear as a literal substring of the output, not merely to parse
  equal.
- **Design note:** unparseable input yields a floor that freezes *everything*, not
  nothing. Freezing too much costs some compression; freezing too little modifies a
  message the provider has cached, silently. The safe direction is not symmetric.
- **Design note:** passthrough is the fallback for every path — disabled, streaming,
  malformed, no live zone, compressor declines, result not smaller. There is no input
  for which `compress_request` errors; the worst case is that it does nothing.
- **Known limitation:** the handler does **not forward upstream yet.** It returns the
  transformed request. Relay needs the SSE state machine first, since a forwarding
  handler would have to buffer streaming responses and break what clients rely on.
- **Known limitation:** `"stream": true` forwards untouched, so the common agent case
  is currently uncompressed.
- **Known limitation:** a `tool_result` whose content is an array of blocks rather
  than a string reads as empty and is never compressed.

## Byte-faithful bodies and header hygiene
**2026-08-03** · closes [#33](https://github.com/baileyrd/rusty_headroom/issues/33), [#34](https://github.com/baileyrd/rusty_headroom/issues/34)

- **Added (#33):** `FaithfulBody` — parses a request while retaining the original
  bytes, so untouched `messages[*]` forward as exact copies. Passthrough returns
  `Cow::Borrowed`: the original bytes are handed back, not rebuilt and hoped to match.
- **Added (#34):** `sanitize` for upstream-bound headers, `HeaderPolicy`, and
  `Redacted` for credentials.
- **Invariant I1 is now testable and tested.** SHA-256 round-trip across
  pretty-printed input, unusual key order, `1.0`, integers past 2^53, CJK and emoji,
  and escaped strings.
- **Design note:** the workspace's `preserve_order` and `arbitrary_precision` flags
  are necessary but not sufficient. A `Value` round trip still normalizes whitespace
  and may differ on escapes, so untouched content is never round-tripped at all.
- **Design note:** `serde_json` cannot deserialize an object into an ordered sequence
  of raw pairs — `Vec<(K,V)>` wants an array, and `serde_json::Map` is fixed to
  `Value` and would re-serialize everything. A small `Deserialize` visitor collects
  members in document order with values left as `RawValue`.
- **Design note:** `Redacted` implements `Debug` *and* `Display` identically, both
  truncating. A `Debug` that dumped the full value would make `tracing::debug!(?x)` on
  any containing struct a credential leak on some error path nobody exercised.
- **Design note:** a client-supplied `X-Forwarded-For` is stripped when the policy
  forbids adding one — forwarding it would defeat the policy while technically
  honoring it.
- **Known limitation:** a body with an escaped top-level key cannot be borrowed and
  routes to verbatim passthrough. Safe direction, but such requests are never
  compressed.
- **Known limitation:** `HeaderPolicy` is supplied by the caller. Auth-mode
  classification, which decides it, is a separate unimplemented row (A1), so
  everything currently gets the quiet default.

## Proxy skeleton — config, /health, graceful shutdown
**2026-08-03** · closes [#32](https://github.com/baileyrd/rusty_headroom/issues/32)

- **Added:** `headroom-proxy` is now a real binary — axum server, `Config` from
  `HEADROOM_*` environment variables, `/health`, graceful shutdown on SIGTERM/SIGINT.
- **Design note:** the default bind is **loopback, not 0.0.0.0**. The proxy forwards
  provider credentials, and a default binding every interface would make an open
  credential relay the out-of-the-box behavior. Widening it is a deliberate act.
- **Design note:** configuration is read from the environment on every access rather
  than cached at startup, so an operator can turn compression off or repoint the
  upstream without a restart — and without dropping in-flight streaming responses.
- **Design note:** unparseable config values fall back to defaults rather than
  failing. A malformed `HEADROOM_PORT` should not take down a running proxy on its
  next config read.
- **Design note:** graceful shutdown matters here specifically because the proxy sits
  in the middle of streaming responses. Dropping one mid-flight truncates a model's
  output mid-token, which reaches the user as a corrupt answer rather than a
  retryable error.
- **Known limitation:** no provider routes yet. `/health` is the only endpoint —
  `/v1/messages` needs byte-faithful body handling to exist first, and standing it up
  before that would mean writing the request path twice.

## SearchCompressor — grep result sets
**2026-08-03** · closes [#29](https://github.com/baileyrd/rusty_headroom/issues/29)

- **Added:** `SearchCompressor`, a `Transform` + `LossyTransform` that groups matches
  under their file so each path is stated once instead of on every matching line.
- **Added:** `parse_match` and the `Match` type as public helpers.
- **Measured:** estimated token reduction of **66% on 100 matches across 20 files,
  71% on 100 across 10 files, 84% on 336 matches**.
- **Honest gap:** the reference reports **92%** on its 100-result code-search
  benchmark. This lands at 66-71% on comparable input. The difference is deliberate
  rather than a defect — the caps here keep 40 matches where the reference apparently
  keeps far fewer, trading ratio for fidelity. `SearchConfig::max_total_matches` makes
  it tunable, but the shipped default does not reach the reference's number.
- **Design note:** line numbers are preserved for every shown match. They are how the
  agent's next action gets targeted; losing them forces a re-search that costs more
  than was saved.
- **Design note:** match text is never truncated. Whole files are elided past a cap
  instead, because the matched line is the thing being searched for and the path
  repetition is the waste.
- **Design note:** file order follows the search output rather than being sorted, so
  whatever relevance ordering the tool applied survives.
- **Known limitation:** grouping reorders. An interleaved original ordering is lost —
  acceptable for search output, since ripgrep groups by file itself, but it is genuine
  information loss.
- **Known limitation:** file grouping is a linear scan per match, so cost is quadratic
  in the number of distinct files. Fine at tool-output scale, wrong for very wide
  result sets.

## LogCompressor — template extraction and repeat collapsing
**2026-08-03** · closes [#28](https://github.com/baileyrd/rusty_headroom/issues/28)

- **Added:** `LogCompressor`, a `Transform` + `LossyTransform` that normalizes each
  log line into a template, groups lines sharing a template, and reports each with a
  count and an example.
- **Added:** `templatize` and `has_severity` as public helpers.
- **Measured:** estimated token reduction of **93% at 50 lines, 98% at 200**.
- **The rule that matters:** error and warning lines are preserved verbatim, *in
  addition to* the template summary rather than as a replacement. A summary that
  reports `x1000 INFO ok` and drops the one `ERROR upstream timeout` is smaller,
  cheaper, and actively harmful — the agent then believes nothing went wrong.
- **Design note:** values are normalized, words are not. Over-eager normalization
  would collapse `disk full` into `disk ok`, making the summary confidently wrong.
  Timestamps, numbers-with-units, hex, UUIDs, and paths become placeholders; ordinary
  words stay.
- **Design note:** templates are reported in first-appearance order. Alphabetical
  ordering would scramble the log's narrative.
- **Known limitation:** output size is roughly constant regardless of input size, so
  a 1000-line log and a 200-line log summarize to about the same thing. Safe, because
  severity lines are always kept, but information density falls as logs grow.
- **Known limitation:** template extraction is heuristic and unaware of log formats.
  Structured JSON logs are routed to SmartCrusher instead, but an unusual text format
  may over- or under-normalize.

## SmartCrusher formatter and transform — JSON actually compresses
**2026-08-03** · closes [#25](https://github.com/baileyrd/rusty_headroom/issues/25)

- **Added:** `format_plan` — renders a `CrushPlan` into text for a language model:
  record and elision counts, constants stated once, low-cardinality fields
  enumerated, anchor records verbatim with their original indices, and the CCR marker.
- **Added:** `SmartCrusher`, implementing `Transform` + `LossyTransform` and holding a
  `CcrStore`. The pipeline now runs end to end — detect, analyze, rank, plan, format,
  store, token-validate.
- **Measured:** on a realistic file-listing tool result, estimated token reduction of
  **77% at 20 records, 91% at 50, 98% at 200**. The reference claims 60–95% on
  structured data.
- **Design note:** anchors are serialized from their original values, so key order and
  numeric literals survive exactly. An anchor that came back reformatted would not be
  the record the output promised.
- **Design note:** the original goes to the CCR store via `store_and_mark` before the
  marker is emitted, so a marker can never advertise a hash nothing was stored under.
- **Design note:** no special casing for the flagship compressor — it goes through
  `apply_guarded` (I8) and `validated_apply` (I5) like any other transform.
- **Known limitation:** the head sample is a fixed count, not a proportion. A
  1000-record array is summarized to the same handful of anchors as a 50-record one,
  so information density falls as arrays grow. Outliers are still always kept, which
  is what keeps this safe rather than merely aggressive.
- **Known limitation:** output is written for a model to read, not to be parsed. There
  is no path back from the rendered text to JSON — recovery is via CCR retrieval, and
  only while the entry lives (24h TTL).
- **Known limitation:** only record-set JSON compresses. Wide objects, deep nests, and
  scalar-heavy documents are classified but have no compressor, so they decline.

## SmartCrusher planning — decide before mutating
**2026-08-03** · closes [#24](https://github.com/baileyrd/rusty_headroom/issues/24)

- **Added:** `plan(...) -> Option<CrushPlan>` — a complete, inert decision about what
  to keep and what to say about the rest. Building one mutates nothing.
- **Added:** `CrushPlan` with anchors (records kept verbatim, sorted and deduplicated)
  and `FieldPlan`s (constants stated once, low-cardinality fields enumerated).
- **Design note:** outliers are anchored unconditionally. If they exceed the sample
  budget the head sample yields — an outlier is never dropped to make room, because
  dropping the anomalous record is the one failure that makes compressed output
  actively worse than no compression.
- **Design note:** `plan` returns `None` when compression would not pay. Invariant I5
  would catch a bad plan afterwards, but burning a format-and-tokenize pass to learn
  what the planner already knew is waste.
- **Design note:** optional low-cardinality fields are not enumerated. Describing one
  by its value set needs an "on some records" qualification that costs more than the
  enumeration saves.
- **Bug fixed in already-merged code:** outlier rarity scoring asked only whether a
  value was held by fewer than half the records. True for a two-valued field, wrong
  from three onwards — a 10/10/10 split across 30 records flagged *every* record as
  anomalous, so the planner saw an all-outlier array and declined. Rarity is now
  measured against the share an even split would give. Found by this issue's tests,
  with regression tests in both directions.
- **Known limitation:** the head sample is taken from the front only. A record set
  whose interesting structure is at the tail relies entirely on outlier detection to
  surface it.
- **Known limitation:** still no output. The formatter that renders a plan is #25.

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
