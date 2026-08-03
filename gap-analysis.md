# rusty_headroom — Parity Gap Analysis

**Target:** `baileyrd/rusty_headroom` (empty repo — single bare commit, no source)
**Reference:** [`headroomlabs-ai/headroom`](https://github.com/headroomlabs-ai/headroom) @ `main`, pinned at commit `HEAD` of 2026-08-03 (Apache-2.0)
**Assessment path:** `spec` — clean-room, documentation-driven. Capabilities extracted from the
reference's published docs (`README.md`, `REALIGNMENT/*.md`, `RUST_DEV.md`, `docs/content/docs/*.mdx`,
`wiki/*.md`) and its public CLI/API surface. Upstream Rust source was **not** used as an
implementation source.

## Scope decisions (settled before assessment)

| Question | Answer |
| --- | --- |
| Parity target | **Full surface** — core + proxy + MCP + CLI + memory + telemetry + Python bindings |
| Derivation | **Clean-room from docs** — architecture and invariants taken from published design docs; implementation written fresh |
| Explicitly OUT | ONNX / Kompress ML compressor; dashboard & web UI; native Bedrock + Vertex routes |
| Explicitly IN | Python interop via pyo3/maturin |

### Deliberately out of scope for this round

- **Kompress-v2-base / ONNX** (`ort` crate, HF model artifacts) — reference §2.5. Excluded by scope decision.
- **Dashboard / savings web UI** — reference README "Configuration & Monitoring". Excluded by scope decision.
- **Bedrock (SigV4) and Vertex (ADC) native routes** — reference REALIGNMENT Phase D. Excluded by scope decision.
- **Image / base64 / audio compression** — the reference architecture itself declares this out of scope (REALIGNMENT §2.6), so it is not a gap.
- **Anthropic ↔ OpenAI shape translation** — likewise declared a non-goal by the reference architecture (§2.6). Each provider gets a native handler.
- **Python/TS framework integrations** (LangChain, Vercel AI, LiteLLM, Agno, CrewAI, AutoGen) — these are host-language adapters, not Rust surface. They become buildable once `headroom-py` (B1) lands; deferred, not filed.

## Architecture invariants carried over

The reference's cache-safety invariants (REALIGNMENT §2.2) are treated as **acceptance criteria on
every proxy-side issue**, not as separate work items:

| ID | Invariant |
| --- | --- |
| I1 | Byte-faithful passthrough on unmutated bytes (SHA-256 equality; no re-serialization) |
| I2 | Cache hot zone (`system`, `tools[*]`, frozen messages, signed/encrypted blocks) never modified |
| I3 | Append-only — compression touches the live zone only |
| I4 | Determinism — same `(bytes, frozen_count, auth_mode)` ⇒ byte-equal output; no clocks, no RNG |
| I5 | Token-aware, not byte-aware — validate post-compression, fall back to original if not smaller |
| I6 | Position-preserving — in-place block edits; side-channel metadata only |
| I7 | Tool definitions normalized (deterministic sort), never compressed |
| I8 | `signature`, `encrypted_content`, `redacted_thinking.data` are passthrough-only |
| I9 | TOIN observes, never mutates request bytes |
| I10 | Auth mode gates compression policy |

## Planned workspace layout

```
crates/
├── headroom-core/        # compression engine, tokenizers, CCR, signals
├── headroom-proxy/       # axum proxy, SSE, cache stabilization, observability
├── headroom-mcp/         # MCP server (stdio JSON-RPC)
├── headroom-cli/         # clap CLI — proxy/doctor/wrap/perf/learn/update/mcp
├── headroom-simulators/  # upstream-provider fakes for e2e tests
└── headroom-py/          # pyo3/maturin extension module
```

---

## Reachability audit

**2026-08-03.** Three rows in this table were marked done on the strength of "the module
exists and is tested", when nothing on the request path called them. An audit over every
public symbol in `headroom-core` and every module in `headroom-proxy` — checking for
references *outside the defining file* — found them, and they are now wired:

| Found unreachable | Closed by |
| --- | --- |
| S4, S5, X12 | #71 |
| Y1–Y3 (`memory`) | #73 |
| X15, I7 normalization (`stabilization`) | #75 |

**A blind spot in that audit, found later.** It asked for references *outside the defining
file*, and `CodeCompressor` had them — from the CLI and the MCP server. Reachable from
*somewhere* is not reachable from the *request path*: the proxy held no code compressor at
all, so every source file a tool returned was forwarded whole while `headroom compress`
reported a saving for the same content. Closed by #81, which also collapsed the three
copies of the routing decision into one.

The audit is now clean: every remaining public symbol is either reached from a request,
dispatched from the CLI (all commands verified against `main.rs`), listed in the MCP tool
table (all three), used internally by a compressor that is itself reached, or documented
here as deliberately library-only.

The lesson is recorded rather than just the fix: *a test proves a function works, not that
anything calls it.* This document now says what reaches each row, not merely that it was
built.

## Gap table

Every row is a gap: the target repo is empty, so all rows are new implementation.
`Breaking?` is `no` throughout — there is no existing public surface to break.
`Platforms` uses `all` for portable Rust; OS-specific rows are called out.

### Foundation

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| F1 | workspace scaffold | infra | spec | all | `Cargo.toml` workspace, `rust-toolchain.toml` | no | S | 6 crates, edition 2021, MSRV 1.80. Apache-2.0 + `NOTICE` crediting the reference project. Done. |
| F2 | CI pipeline | infra | spec | all | `.github/workflows` | no | S | build + test + clippy `-D warnings` + fmt check. Must be a required status check. Done. |
| F3 | `Error` / `Result` | type | spec | all | REALIGNMENT §2.3 | no | S | `thiserror`-based, one error enum per crate, no `unwrap`/`expect` outside tests. Done. |
| F4 | `Config` + env loading | type | spec | all | `docs/configuration.mdx` | no | M | `HEADROOM_*` env vars, read live per request. Done. |
| F5 | `POST /admin/runtime-env` | fn | spec | all | README "hot-sync" | no | S | Runtime config hot-reload without restart. Depends on F4, X1. Done via `admin::runtime_env` + `config` override map (DECISIONS D10); gated on a loopback peer address. |

### Tokenization

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| T1 | `Tokenizer` trait + `estimator` | trait/fn | spec | all | REALIGNMENT §2.3 `tokenizer/` | no | S | Heuristic byte→token estimator as the always-available fallback. Done — `HeuristicEstimator`, documented never to under-count. |
| T2 | tiktoken BPE impl | fn | spec | all | `tokenizer/tiktoken_impl.rs` | no | M | OpenAI model families. Done as `tokenizer::TiktokenCounter` (o200k_base, cl100k_base) via `tiktoken-rs` 0.11 — embedded tables, exact offline, registered by default and selected from the request's `model`. |
| T3 | HuggingFace tokenizer impl | fn | spec | all | `tokenizer/hf_impl.rs` | no | M | Via `tokenizers` crate. **Deliberately deferred** — needs a per-model `tokenizer.json` fetched at runtime, making the tokenizer a network dependency of the request path, and Anthropic publishes no tokenizer to be exact against. See DECISIONS D16. |
| T4 | tokenizer registry | fn | spec | all | `tokenizer/registry.rs` | no | S | model-id → tokenizer resolution with fallback to T1. Done as `tokenizer::registry::{Family, Registry}` — always resolves, never `None`; `with_defaults()` registers the tiktoken counters and the proxy selects through it. |

### Content detection

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| D1 | `ContentRouter` / `detect_content_type` | fn | spec | all | README "ContentRouter"; `docs/how-compression-works.mdx` | no | M | JSON / code / log / diff / search-results / prose. The routing brain — keep it a pure function. Done — `detection::detect`, a pure function returning a type and a confidence. |
| D2 | unified-diff detector | fn | spec | all | `transforms/unidiff_detector.rs` | no | S | Recognize `---/+++/@@` hunks. Done. |
| D3 | code-language detector | fn | spec | all | `transforms/magika_detector.rs` | no | M | Heuristic/extension + content sniffing. No ML model (out of scope). Done — heuristic, no ML model (out of scope). |
| D4 | `AdaptiveSizer` thresholds | fn | spec | all | REALIGNMENT I5 | no | S | code>2KB, JSON>1KB, logs>500B, text>5KB — below threshold, do not compress. Done — `AdaptiveSizer`. |

### Signals

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| S1 | line importance scoring | fn | spec | all | `signals/line_importance.rs` | no | M | Drives which lines survive lossy passes. Done. |
| S2 | keyword / error detector | fn | spec | all | `signals/keyword_detector.rs` | no | S | Error/warning keyword sets; never drop error lines. Done. |
| S3 | tiered signal aggregation | fn | spec | all | `signals/tiered.rs` | no | S | Combines S1+S2 into keep/drop tiers. Done. |
| S4 | `AnchorSelector` | fn | spec | all | `transforms/anchor_selector.rs` | no | M | Picks stable anchor points so output stays position-preserving (I6). Done as `signals::anchors::select_anchors` — hunk headers, headings, fences, stack frames, structure opens, boundaries. Consulted by `TextSummarizer` via `signals::keep_with_required`, which treats the anchor set as a floor the line budget cannot cut into. |
| S5 | `TagProtector` | fn | spec | all | `transforms/tag_protector.rs` | no | S | Never break XML/markup tags mid-compression. Done as `signals::tags::{protected_lines, breaks_markup}`; balance check over tag-shaped tokens, not an XML parser. Unioned with the anchor set in `TextSummarizer`, so a lossy pass cannot drop a tag delimiter. |

### SmartCrusher (JSON) — split into 6 issues to keep them small

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| C1 | SmartCrusher types + config | type | spec | all | `docs/smart-crusher.mdx` | no | S | Shared IR/config; the base other C rows build on. Done. |
| C2 | structural analyzer + classifier | fn | spec | all | `docs/smart-crusher.mdx` | no | M | Detect record arrays, homogeneous objects, key cardinality. Done. |
| C3 | statistics + outlier detection | fn | spec | all | `docs/smart-crusher.mdx` | no | M | Summarize repetitive records; keep statistical outliers verbatim. Done — outlier rarity rule fixed in development; see RELEASE_NOTES. |
| C4 | anchors + planning | fn | spec | all | `docs/smart-crusher.mdx` | no | M | Decide what to keep/elide before mutating anything. Done. |
| C5 | compaction IR + walker + formatter | fn | spec | all | `docs/smart-crusher.mdx` | no | L | **Split candidate** — walker and formatter may become separate issues if C5 runs long. Done. |
| C6 | crusher orchestration | fn | spec | all | `docs/smart-crusher.mdx` | no | M | Wires C1–C5 behind one entry point; enforces I4 determinism. Done. |

### Other compressors

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| C7 | `LogCompressor` | fn | spec | all | `docs/text-and-logs.mdx` | no | M | Template extraction + repeat collapsing. Done. |
| C8 | `DiffCompressor` | fn | spec | all | `transforms/diff_compressor.rs` | no | M | Elide unchanged context, keep hunk headers. Depends on D2. Done. |
| C9 | `SearchCompressor` | fn | spec | all | README "Code search 92%" | no | M | Grep/ripgrep-style result sets — the headline benchmark case. Done. |
| C10 | `TextCrusher` | fn | spec | all | `docs/text-and-logs.mdx` | no | M | Lossless plain-text pass (whitespace, repetition). Done — `TextCrusher` (lossless) and `TextSummarizer` (lossy), split per I10. |
| C11 | `CodeCompressor` core + Rust/Python | fn | spec | all | `docs/code-compression.mdx` | no | L | AST-aware skeletonization. **Split** — core trait + 2 languages. Done — heuristic skeletonizer, not tree-sitter; see DECISIONS D3. Registered in `Orchestrator` and reached from the request path — it was not, until #81. |
| C12 | `CodeCompressor` JS/TS + Go | fn | spec | all | `docs/code-compression.mdx` | no | M | Depends on C11. Done — heuristic; see DECISIONS D3. Reached via C11's registration. |
| C13 | `CodeCompressor` Java + C/C++ + Perl | fn | spec | all | `docs/code-compression.mdx` | no | M | Depends on C11. Perl has no tree-sitter-grade grammar — may degrade to heuristic. Done — heuristic; see DECISIONS D3. Reached via C11's registration. |

### Pipeline

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| P1 | `LosslessTransform` / `LossyTransform` | trait | spec | all | REALIGNMENT §2.3 `pipeline/traits.rs` | no | S | In-place `fn(&mut Block) -> Result<()>` per I6. Done. |
| P2 | `live_zone` block dispatcher | fn | spec | all | REALIGNMENT §2.2 I2/I3 | no | L | **Core of the whole design.** Walks messages from tail; identifies latest user msg, tool_result, function_call_output, local_shell_call_output, apply_patch_call_output. Done. |
| P3 | pipeline orchestrator | fn | spec | all | `pipeline/orchestrator.rs` | no | M | Live-zone-only; routes via D1 to C*. Done as `pipeline::Orchestrator`; `Routing` names the decline reason. The proxy's `Compressors` is a thin wrapper over it. |
| P4 | offloads (json/log/diff/search/prose) | fn | spec | all | `pipeline/offloads/` | no | M | Move bulky sub-values to CCR, leave markers. **Covered by the existing compressors** — SmartCrusher/Log/Search/Diff already offload to CCR and leave markers; a separate layer would be a second name for the same mechanism. |
| P5 | reformats (json minifier, log template) | fn | spec | all | `pipeline/reformats/` | no | S | Lossless byte reduction. Done as `pipeline::reformats::{minify_json, tidy_lines}`, exposed as `Reformatter` and routed by the orchestrator for policies permitting lossless transforms (D14). |
| P6 | `safety` checks | fn | spec | all | `transforms/safety.rs` | no | S | Guards against pathological/adversarial input. Done as `pipeline::safety::check` — size, depth, line length, line count; declines to compress rather than rejecting the request. |
| P7 | token validation + fallback | fn | spec | all | REALIGNMENT I5 | no | S | If `compressed.tokens >= original.tokens`, forward original. Depends on T1. Done — `validated_apply`. |

### CCR (Compress-Cache-Retrieve)

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| R1 | `CcrStore` trait + in-memory backend | trait | spec | all | REALIGNMENT §2.5 | no | S | `put/get/purge_expired` with TTL. Done. |
| R2 | content hashing + `<<ccr:HASH>>` marker | fn | spec | all | REALIGNMENT §2.5 | no | S | BLAKE3, content-addressed ⇒ replay-safe and deterministic (I4). Done. |
| R3 | SQLite backend | fn | spec | all | REALIGNMENT §2.5 | no | M | Primary persistent backend. Done as `FileCcrStore` — one file per hash with an expiry sidecar and an atomic rename, a deliberate substitution rather than SQLite; see DECISIONS D6. |
| R4 | Redis backend | fn | spec | all | REALIGNMENT §2.5 | no | M | Optional, multi-worker deployments. Done as `ccr::RedisCcrStore`, behind an off-by-default `redis` feature. Selected by the proxy and the MCP server via `HEADROOM_REDIS_URL`, which also fixed the proxy hardcoding an in-memory store. Server-side expiry rather than a purge sweep. See DECISIONS D22. |
| R5 | always-on `ccr_retrieve` tool registration | fn | spec | all | REALIGNMENT §2.6 | no | M | Must never toggle between requests — toggling busts the tools array. Done — registered unconditionally. |

### Auth mode

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| A1 | `classify_auth_mode(headers)` | fn | spec | all | REALIGNMENT §2.4 | no | S | → `payg` \| `oauth` \| `subscription`. Done — OAuth prefix ordering bug fixed in development. |
| A2 | policy matrix | type | spec | all | REALIGNMENT §2.4 table | no | M | Gates compression aggressiveness, header handling, auto-cache-control. Implements I10. Done — includes `lossless_transforms`; see DECISIONS D14. |

### Proxy

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| X1 | axum skeleton + `/health` | fn | spec | all | `docs/proxy.mdx` | no | S | Listen addr/port config, graceful shutdown. Done. |
| X2 | byte-faithful body buffering | fn | spec | all | REALIGNMENT I1 | no | M | `serde_json` `raw_value` + `arbitrary_precision` + `preserve_order`. **Gate:** SHA-256 round-trip test. Done — `FaithfulBody`, SHA-256 round-trip gated in `tests/invariants.rs`. |
| X3 | header hygiene | fn | spec | all | REALIGNMENT §2.4 | no | S | Strip `x-headroom-*` upstream-bound; conditional `X-Forwarded-*`; never touch `User-Agent`. Done. |
| X4 | `cache_control` → `frozen_message_count` | fn | spec | all | REALIGNMENT §2.1 step 4 | no | M | Honor customer-set markers. Depends on X2. Done. |
| X5 | `/v1/messages` (Anthropic) handler | fn | spec | all | `docs/proxy.mdx` | no | L | Done. Passthrough first, then live-zone compression. Depends on P2, X2, X4. Includes upstream relay (`upstream::Upstream`) — streamed response body, per-hop header rebuild, provider-shaped 502 on failure. |
| X6 | `/v1/chat/completions` (OpenAI) handler | fn | spec | all | `docs/openai-sdk.mdx` | no | L | Depends on P2, X2. Done. |
| X7 | `/v1/responses` handler | fn | spec | all | `docs/openai-sdk.mdx` | no | L | Output items, reasoning summary; per-item-type passthrough preservation. Done — `Dialect::OpenAiResponses`; `function_call_output` compressed, `function_call` never. |
| X8 | `/v1/conversations` + `/v1/responses/compact` passthrough | fn | spec | all | REALIGNMENT §2.6 | no | S | Explicitly never compressed. Done. |
| X9 | SSE framing + byte-level state machine | fn | spec | all | REALIGNMENT §2.1 step 10 | no | L | **High risk.** Must survive UTF-8 splits mid-codepoint and single-`\n` splits. Done. |
| X10 | SSE Anthropic events | fn | spec | all | REALIGNMENT Phase C | no | M | All delta types incl. `thinking_delta`, `signature_delta`, `citations_delta`. Depends on X9. Done. |
| X11 | SSE OpenAI chat events | fn | spec | all | REALIGNMENT Phase C | no | M | `tool_call` accumulation across chunks. Depends on X9. Done. |
| X12 | SSE OpenAI responses events | fn | spec | all | REALIGNMENT Phase C | no | M | Output items + reasoning summary. Depends on X9. Done as `sse::responses`; stem/suffix split so future event types stay classifiable. Attached by `sse::Observer::for_path`, which picks the vocabulary from the request path — so `/v1/responses` and `/v1/chat/completions` are read by their own classifiers rather than Anthropic's. |
| X13 | WebSocket flow | fn | spec | all | `crates/headroom-proxy/src/websocket.rs` (name only) | no | M | Codex WS transport. Done as `websocket::relay_socket` — bidirectional faithful relay, frame kinds preserved. **Deliberately does not compress**; see DECISIONS D15. |
| X14 | tool array sort + JSON Schema key sort | fn | spec | all | REALIGNMENT I7 | no | M | Deterministic recursive sort. Normalize, never compress. Done. |
| X15 | `cache_control` auto-placement | fn | spec | all | REALIGNMENT Phase E | no | M | Anthropic, ≤4 ephemeral breakpoints. PAYG only per I10. Done as `stabilization::stabilize`, reached from the `/v1/messages` handler — **opt-in via `HEADROOM_STABILIZE`**, because placing a marker modifies the hot zone that I2 protects (D20). Breakpoints sit at fixed anchors so they never move as the conversation grows. |
| X16 | `prompt_cache_key` injection | fn | spec | all | REALIGNMENT Phase E | no | S | OpenAI, only when not customer-set. PAYG only. Done via `body::insert_top_level_member` in `openai::shape_openai` — byte-faithful, key derived from every message but the newest. (`stabilization::inject_prompt_cache_key` is a `Value`-shaped variant, not the request path.) |
| X17 | volatile-content detector | fn | spec | all | REALIGNMENT Phase E | no | M | Warn only — never rewrite (that was the original bug). Done as `volatile::scan`, wired into `/v1/messages`; no function returns modified content. Anthropic route only. |
| X18 | cache-drift telemetry | fn | spec | all | REALIGNMENT Phase E | no | S | Detect and report prefix busts. Done via `observe::ObservingStream` — cache read/creation tokens read from the `message_start` usage block feed `headroom_cache_hit_rate`. |
| X19 | Prometheus metrics + observability | fn | spec | all | `docs/metrics.mdx` | no | M | cache hit rate, compression ratio, token savings. Done — Prometheus text exposition at `GET /metrics`, fed by the request path and the stream observer. |
| X20 | loopback guard + rate limit + request log | fn | spec | all | REALIGNMENT Phase H file list | no | M | Prevent proxy-to-self loops; redact `Authorization` to first 12 chars. Done via `guard::{is_self_referential, RateLimiter}` — startup loop check (see DECISIONS D11), 600 req/min backstop answering 429. |

### MCP server

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| M1 | MCP server skeleton (stdio JSON-RPC) | fn | spec | all | `docs/mcp.mdx` | no | M | Protocol handshake, tool listing. Done. |
| M2 | `headroom_compress` tool | fn | spec | all | README "MCP Tools" | no | S | Depends on M1, P3. Done. |
| M3 | `headroom_retrieve` tool | fn | spec | all | README "MCP Tools" | no | S | Depends on M1, R1. Done. |
| M4 | `headroom_stats` tool | fn | spec | all | README "MCP Tools" | no | S | Depends on M1, X19. Done. |
| M5 | `headroom mcp install` registry writers | fn | spec | all | `mcp_registry/` (claude, codex, grok, opencode) | no | M | Writes MCP server entries into each agent's config file. Done — `headroom mcp --config`; writes an absolute binary path, preserves other servers, idempotent. |

### CLI

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| L1 | clap CLI skeleton + `--version` | fn | spec | all | `wiki/cli.md` | no | S | Subcommand tree. Done. |
| L2 | `headroom proxy` | fn | spec | all | README CLI | no | S | `--port`. Depends on X1, L1. Done. |
| L3 | `headroom doctor` | fn | spec | all | README CLI | no | M | Health/config/connectivity checks. Done — real compression round-trip, not a version dump. |
| L4 | `headroom deploy` | fn | spec | all | README CLI | no | M | Turnkey local deployment. Done — prints systemd/compose/direct manifests rather than daemonizing; compose publishes on loopback only, with a test. |
| L5 | `headroom wrap` core + `claude` | fn | spec | all | README "Supported Agents" | no | L | Env-var injection + config rewrite. **Split** — framework + first agent. Done as `wrap::{Agent, wrap_settings_file}`. |
| L6 | `headroom wrap` codex/cursor/aider | fn | spec | all | README | no | M | Depends on L5. Done; cursor reports as env-unsupported rather than printing no-op exports. |
| L7 | `headroom wrap` cline/continue/goose/openhands | fn | spec | all | REALIGNMENT Phase G | no | M | Depends on L5. Done. |
| L8 | `headroom unwrap` | fn | spec | all | README CLI | no | S | Must fully restore pre-wrap config. Depends on L5. Done — byte-exact restore from a whole-file backup, verified SHA-256 identical through the binary. |
| L9 | `headroom perf` | fn | spec | all | README CLI | no | S | Latency/throughput metrics. Done — measures the compressor, not the network; warm-up pass discarded. |
| L10 | `headroom learn` | fn | spec | all | `docs/failure-learning.mdx` | no | L | Mines failed sessions; `--verbosity`. Done as a **corpus** miner rather than a session miner — no session-log format exists to read. Runs request bodies through the real pipeline and publishes recommendations. |
| L11 | `headroom update` | fn | spec | all | README CLI | no | M | `--check`, `--pre`; in-place upgrade. Done for `--check`. In-place upgrade **deliberately not implemented** — a credential-holding binary is the wrong one to give a self-replacing updater. |
| L12 | `headroom savings` / `output-savings` | fn | spec | all | `docs/savings.mdx` | no | M | Savings ledger reporting. Done — reads the proxy's `/metrics` exposition from stdin; no currency figure by design. |
| L13 | `headroom init` / `inspect` / `tools` | fn | spec | all | `headroom/cli/` | no | M | Scaffolding + introspection helpers. Done — `init` refuses to overwrite, `tools` lists compressors/thresholds/MCP tools, `inspect` already existed. |

### Memory & shared context

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Y1 | cross-agent memory store | type | spec | all | `docs/memory.mdx` | no | L | Auto-dedup + provenance tracking. **Split candidate.** Done as `memory::MemoryStore` — content-addressed dedup, provenance list, corroboration. In-memory only; no persistence or eviction. |
| Y2 | `SharedContext` put/get | fn | spec | all | `docs/shared-context.mdx` | no | M | Multi-agent shared context. Depends on Y1. Done as `memory::SharedContext`, namespaced with a unit separator so path-shaped keys cannot collide. **Library surface only** — no request path reaches it, and exposing it would mean a fourth MCP tool the reference does not have (same reasoning as D19). |
| Y3 | live-zone-tail memory injection | fn | spec | all | REALIGNMENT §2.6 | no | M | Memory goes in the live-zone tail — never the system prompt (I2). Done as `memory::inject_block`, reached from the proxy via `memory::inject_append` in `compress_dialect` — appended to the newest user message's last text block, gated on the lossy permission (D19), fed by `HEADROOM_MEMORY` read once at startup. |

### Telemetry (TOIN)

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| N1 | `Telemetry` trait (observation-only) | trait | spec | all | REALIGNMENT §2.5 | no | S | **No request-time hint API** — that is invariant I9. Done: `telemetry::Telemetry`, every method returns `()`, asserted structurally by a test. |
| N2 | structure hashing + aggregation key | fn | spec | all | REALIGNMENT §2.5 | no | M | Key = `(auth_mode, model_family, structure_hash)`. Done: `telemetry::{StructureHash, AggregationKey}`; FNV-1a for cross-build stability, values discarded before hashing. |
| N3 | recommendations publish + startup load | fn | spec | all | REALIGNMENT §2.5 | no | M | `recommendations.toml`, read at startup only. Done as `telemetry::Recommendations`, published as JSON (DECISIONS D12); `headroom learn` writes one and `HEADROOM_RECOMMENDATIONS` loads it at startup, gating routing via `Routing::MeasuredUseless`. |

### Output shaping

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| O1 | verbosity steering | fn | spec | all | README "Output Token Reduction" | no | M | `HEADROOM_OUTPUT_SHAPER=1`. Appends terseness note **without** busting the prompt cache. Done: `output_shaping::verbosity_append`, note lands in the live-zone tail, wired into `compress_dialect`. |
| O2 | effort routing | fn | spec | all | README | no | M | `reasoning_effort` (OpenAI) / `thinking.budget_tokens` (Anthropic); full effort on new questions and errors. Done as `output_shaping::route_effort`, applied to outgoing OpenAI requests in `proxy::openai`. |

### Python bindings

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| B1 | pyo3 module + `compress()` | fn | spec | all | README Python API | no | M | abi3-py310, built via maturin. Done as `crates/headroom-py` — `compress()`, `count_tokens()`, `detect_content_type()`, routing through `Orchestrator` so Python and the proxy agree by construction. D4's deferral is reversed; see D21. |
| B2 | `pyo3-log` bridging | fn | spec | all | reference workspace deps | no | S | Rust `tracing`/`log` → Python `logging`. Done via `pyo3-log`, initialized at module import. See D21. |

### Test infrastructure

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| E1 | upstream simulators | infra | spec | all | `crates/headroom-simulators` (name only) | no | M | Fake Anthropic/OpenAI endpoints for e2e without network. Done — real loopback server recording exact bytes; binds port 0 so tests parallelize. |
| E2 | invariant test gates I1–I4 | infra | spec | all | REALIGNMENT §2.2 | no | M | SHA-256 round-trip; hot-zone-unchanged; append-only; determinism. Done as `tests/invariants.rs`, run end to end through the relay rather than against `compress_request`. |
| E3 | SSE corner-case fixtures | infra | spec | all | REALIGNMENT Phase I | no | M | UTF-8 split, ping, all delta types, `[DONE]`, mid-stream error. Done as `headroom_simulators::fixtures` — ten cases, each documenting the defect it guards. |
| E4 | property tests | infra | spec | all | REALIGNMENT Phase I | no | M | No-panic SSE parser; tokens-non-increasing. Done as `tests/properties.rs`; fixed-seed generator so failures reproduce from the test name. |

---

## Summary

| Workstream | Rows |
| --- | ---: |
| Foundation | 5 |
| Tokenization | 4 |
| Content detection | 4 |
| Signals | 5 |
| SmartCrusher | 6 |
| Other compressors | 7 |
| Pipeline | 7 |
| CCR | 5 |
| Auth mode | 2 |
| Proxy | 20 |
| MCP | 5 |
| CLI | 13 |
| Memory | 3 |
| Telemetry | 3 |
| Output shaping | 2 |
| Python bindings | 2 |
| Test infra | 4 |
| **Total** | **97** |

No row is flagged `Breaking? yes` — the target has no existing public API, so every item is a pure
addition. No new-third-party-dependency stop-and-ask is pre-flagged either, but individual issues
that need one (`tokenizers`, `tree-sitter`, `rusqlite`, `redis`, `pyo3`) call it out in their body.

### Suggested implementation order

Dependency-first, so each layer has something real to build on:

1. **F1–F3** — workspace, CI, error types
2. **T1, T4** — token estimation (needed by I5 validation everywhere)
3. **D1, D4** — content routing and thresholds
4. **P1, P6, P7** — transform traits, safety, token-validated fallback
5. **R1, R2** — CCR store + markers
6. **C1–C6** — SmartCrusher (the highest-value compressor: 60–95% on JSON)
7. **P2, P3** — live-zone dispatcher and orchestrator
8. **X1–X5** — proxy skeleton through the Anthropic handler
9. **X9–X10** — SSE
10. Everything else, breadth-first by workstream
