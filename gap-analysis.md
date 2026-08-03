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

## Gap table

Every row is a gap: the target repo is empty, so all rows are new implementation.
`Breaking?` is `no` throughout — there is no existing public surface to break.
`Platforms` uses `all` for portable Rust; OS-specific rows are called out.

### Foundation

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| F1 | workspace scaffold | infra | spec | all | `Cargo.toml` workspace, `rust-toolchain.toml` | no | S | 6 crates, edition 2021, MSRV 1.80. Apache-2.0 + `NOTICE` crediting the reference project. |
| F2 | CI pipeline | infra | spec | all | `.github/workflows` | no | S | build + test + clippy `-D warnings` + fmt check. Must be a required status check. |
| F3 | `Error` / `Result` | type | spec | all | REALIGNMENT §2.3 | no | S | `thiserror`-based, one error enum per crate, no `unwrap`/`expect` outside tests. |
| F4 | `Config` + env loading | type | spec | all | `docs/configuration.mdx` | no | M | `HEADROOM_*` env vars, read live per request. |
| F5 | `POST /admin/runtime-env` | fn | spec | all | README "hot-sync" | no | S | Runtime config hot-reload without restart. Depends on F4, X1. Done via `admin::runtime_env` + `config` override map (DECISIONS D10); gated on a loopback peer address. |

### Tokenization

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| T1 | `Tokenizer` trait + `estimator` | trait/fn | spec | all | REALIGNMENT §2.3 `tokenizer/` | no | S | Heuristic byte→token estimator as the always-available fallback. |
| T2 | tiktoken BPE impl | fn | spec | all | `tokenizer/tiktoken_impl.rs` | no | M | OpenAI model families. Needs a BPE implementation + encoding tables. |
| T3 | HuggingFace tokenizer impl | fn | spec | all | `tokenizer/hf_impl.rs` | no | M | Via `tokenizers` crate. Anthropic/OSS models. |
| T4 | tokenizer registry | fn | spec | all | `tokenizer/registry.rs` | no | S | model-id → tokenizer resolution with fallback to T1. |

### Content detection

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| D1 | `ContentRouter` / `detect_content_type` | fn | spec | all | README "ContentRouter"; `docs/how-compression-works.mdx` | no | M | JSON / code / log / diff / search-results / prose. The routing brain — keep it a pure function. |
| D2 | unified-diff detector | fn | spec | all | `transforms/unidiff_detector.rs` | no | S | Recognize `---/+++/@@` hunks. |
| D3 | code-language detector | fn | spec | all | `transforms/magika_detector.rs` | no | M | Heuristic/extension + content sniffing. No ML model (out of scope). |
| D4 | `AdaptiveSizer` thresholds | fn | spec | all | REALIGNMENT I5 | no | S | code>2KB, JSON>1KB, logs>500B, text>5KB — below threshold, do not compress. |

### Signals

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| S1 | line importance scoring | fn | spec | all | `signals/line_importance.rs` | no | M | Drives which lines survive lossy passes. |
| S2 | keyword / error detector | fn | spec | all | `signals/keyword_detector.rs` | no | S | Error/warning keyword sets; never drop error lines. |
| S3 | tiered signal aggregation | fn | spec | all | `signals/tiered.rs` | no | S | Combines S1+S2 into keep/drop tiers. |
| S4 | `AnchorSelector` | fn | spec | all | `transforms/anchor_selector.rs` | no | M | Picks stable anchor points so output stays position-preserving (I6). |
| S5 | `TagProtector` | fn | spec | all | `transforms/tag_protector.rs` | no | S | Never break XML/markup tags mid-compression. |

### SmartCrusher (JSON) — split into 6 issues to keep them small

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| C1 | SmartCrusher types + config | type | spec | all | `docs/smart-crusher.mdx` | no | S | Shared IR/config; the base other C rows build on. |
| C2 | structural analyzer + classifier | fn | spec | all | `docs/smart-crusher.mdx` | no | M | Detect record arrays, homogeneous objects, key cardinality. |
| C3 | statistics + outlier detection | fn | spec | all | `docs/smart-crusher.mdx` | no | M | Summarize repetitive records; keep statistical outliers verbatim. |
| C4 | anchors + planning | fn | spec | all | `docs/smart-crusher.mdx` | no | M | Decide what to keep/elide before mutating anything. |
| C5 | compaction IR + walker + formatter | fn | spec | all | `docs/smart-crusher.mdx` | no | L | **Split candidate** — walker and formatter may become separate issues if C5 runs long. |
| C6 | crusher orchestration | fn | spec | all | `docs/smart-crusher.mdx` | no | M | Wires C1–C5 behind one entry point; enforces I4 determinism. |

### Other compressors

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| C7 | `LogCompressor` | fn | spec | all | `docs/text-and-logs.mdx` | no | M | Template extraction + repeat collapsing. |
| C8 | `DiffCompressor` | fn | spec | all | `transforms/diff_compressor.rs` | no | M | Elide unchanged context, keep hunk headers. Depends on D2. |
| C9 | `SearchCompressor` | fn | spec | all | README "Code search 92%" | no | M | Grep/ripgrep-style result sets — the headline benchmark case. |
| C10 | `TextCrusher` | fn | spec | all | `docs/text-and-logs.mdx` | no | M | Lossless plain-text pass (whitespace, repetition). |
| C11 | `CodeCompressor` core + Rust/Python | fn | spec | all | `docs/code-compression.mdx` | no | L | AST-aware skeletonization. **Split** — core trait + 2 languages. |
| C12 | `CodeCompressor` JS/TS + Go | fn | spec | all | `docs/code-compression.mdx` | no | M | Depends on C11. |
| C13 | `CodeCompressor` Java + C/C++ + Perl | fn | spec | all | `docs/code-compression.mdx` | no | M | Depends on C11. Perl has no tree-sitter-grade grammar — may degrade to heuristic. |

### Pipeline

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| P1 | `LosslessTransform` / `LossyTransform` | trait | spec | all | REALIGNMENT §2.3 `pipeline/traits.rs` | no | S | In-place `fn(&mut Block) -> Result<()>` per I6. |
| P2 | `live_zone` block dispatcher | fn | spec | all | REALIGNMENT §2.2 I2/I3 | no | L | **Core of the whole design.** Walks messages from tail; identifies latest user msg, tool_result, function_call_output, local_shell_call_output, apply_patch_call_output. |
| P3 | pipeline orchestrator | fn | spec | all | `pipeline/orchestrator.rs` | no | M | Live-zone-only; routes via D1 to C*. |
| P4 | offloads (json/log/diff/search/prose) | fn | spec | all | `pipeline/offloads/` | no | M | Move bulky sub-values to CCR, leave markers. |
| P5 | reformats (json minifier, log template) | fn | spec | all | `pipeline/reformats/` | no | S | Lossless byte reduction. |
| P6 | `safety` checks | fn | spec | all | `transforms/safety.rs` | no | S | Guards against pathological/adversarial input. |
| P7 | token validation + fallback | fn | spec | all | REALIGNMENT I5 | no | S | If `compressed.tokens >= original.tokens`, forward original. Depends on T1. |

### CCR (Compress-Cache-Retrieve)

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| R1 | `CcrStore` trait + in-memory backend | trait | spec | all | REALIGNMENT §2.5 | no | S | `put/get/purge_expired` with TTL. |
| R2 | content hashing + `<<ccr:HASH>>` marker | fn | spec | all | REALIGNMENT §2.5 | no | S | BLAKE3, content-addressed ⇒ replay-safe and deterministic (I4). |
| R3 | SQLite backend | fn | spec | all | REALIGNMENT §2.5 | no | M | Primary persistent backend. |
| R4 | Redis backend | fn | spec | all | REALIGNMENT §2.5 | no | M | Optional, multi-worker deployments. |
| R5 | always-on `ccr_retrieve` tool registration | fn | spec | all | REALIGNMENT §2.6 | no | M | Must never toggle between requests — toggling busts the tools array. |

### Auth mode

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| A1 | `classify_auth_mode(headers)` | fn | spec | all | REALIGNMENT §2.4 | no | S | → `payg` \| `oauth` \| `subscription`. |
| A2 | policy matrix | type | spec | all | REALIGNMENT §2.4 table | no | M | Gates compression aggressiveness, header handling, auto-cache-control. Implements I10. |

### Proxy

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| X1 | axum skeleton + `/health` | fn | spec | all | `docs/proxy.mdx` | no | S | Listen addr/port config, graceful shutdown. |
| X2 | byte-faithful body buffering | fn | spec | all | REALIGNMENT I1 | no | M | `serde_json` `raw_value` + `arbitrary_precision` + `preserve_order`. **Gate:** SHA-256 round-trip test. |
| X3 | header hygiene | fn | spec | all | REALIGNMENT §2.4 | no | S | Strip `x-headroom-*` upstream-bound; conditional `X-Forwarded-*`; never touch `User-Agent`. |
| X4 | `cache_control` → `frozen_message_count` | fn | spec | all | REALIGNMENT §2.1 step 4 | no | M | Honor customer-set markers. Depends on X2. |
| X5 | `/v1/messages` (Anthropic) handler | fn | spec | all | `docs/proxy.mdx` | no | L | Passthrough first, then live-zone compression. Depends on P2, X2, X4. Includes upstream relay (`upstream::Upstream`) — streamed response body, per-hop header rebuild, provider-shaped 502 on failure. |
| X6 | `/v1/chat/completions` (OpenAI) handler | fn | spec | all | `docs/openai-sdk.mdx` | no | L | Depends on P2, X2. |
| X7 | `/v1/responses` handler | fn | spec | all | `docs/openai-sdk.mdx` | no | L | Output items, reasoning summary; per-item-type passthrough preservation. |
| X8 | `/v1/conversations` + `/v1/responses/compact` passthrough | fn | spec | all | REALIGNMENT §2.6 | no | S | Explicitly never compressed. |
| X9 | SSE framing + byte-level state machine | fn | spec | all | REALIGNMENT §2.1 step 10 | no | L | **High risk.** Must survive UTF-8 splits mid-codepoint and single-`\n` splits. |
| X10 | SSE Anthropic events | fn | spec | all | REALIGNMENT Phase C | no | M | All delta types incl. `thinking_delta`, `signature_delta`, `citations_delta`. Depends on X9. |
| X11 | SSE OpenAI chat events | fn | spec | all | REALIGNMENT Phase C | no | M | `tool_call` accumulation across chunks. Depends on X9. |
| X12 | SSE OpenAI responses events | fn | spec | all | REALIGNMENT Phase C | no | M | Output items + reasoning summary. Depends on X9. |
| X13 | WebSocket flow | fn | spec | all | `crates/headroom-proxy/src/websocket.rs` (name only) | no | M | Codex WS transport. |
| X14 | tool array sort + JSON Schema key sort | fn | spec | all | REALIGNMENT I7 | no | M | Deterministic recursive sort. Normalize, never compress. |
| X15 | `cache_control` auto-placement | fn | spec | all | REALIGNMENT Phase E | no | M | Anthropic, ≤4 ephemeral breakpoints. PAYG only per I10. |
| X16 | `prompt_cache_key` injection | fn | spec | all | REALIGNMENT Phase E | no | S | OpenAI, only when not customer-set. PAYG only. |
| X17 | volatile-content detector | fn | spec | all | REALIGNMENT Phase E | no | M | Warn only — never rewrite (that was the original bug). |
| X18 | cache-drift telemetry | fn | spec | all | REALIGNMENT Phase E | no | S | Detect and report prefix busts. Done via `observe::ObservingStream` — cache read/creation tokens read from the `message_start` usage block feed `headroom_cache_hit_rate`. |
| X19 | Prometheus metrics + observability | fn | spec | all | `docs/metrics.mdx` | no | M | cache hit rate, compression ratio, token savings. |
| X20 | loopback guard + rate limit + request log | fn | spec | all | REALIGNMENT Phase H file list | no | M | Prevent proxy-to-self loops; redact `Authorization` to first 12 chars. Done via `guard::{is_self_referential, RateLimiter}` — startup loop check (see DECISIONS D11), 600 req/min backstop answering 429. |

### MCP server

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| M1 | MCP server skeleton (stdio JSON-RPC) | fn | spec | all | `docs/mcp.mdx` | no | M | Protocol handshake, tool listing. |
| M2 | `headroom_compress` tool | fn | spec | all | README "MCP Tools" | no | S | Depends on M1, P3. |
| M3 | `headroom_retrieve` tool | fn | spec | all | README "MCP Tools" | no | S | Depends on M1, R1. |
| M4 | `headroom_stats` tool | fn | spec | all | README "MCP Tools" | no | S | Depends on M1, X19. |
| M5 | `headroom mcp install` registry writers | fn | spec | all | `mcp_registry/` (claude, codex, grok, opencode) | no | M | Writes MCP server entries into each agent's config file. |

### CLI

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| L1 | clap CLI skeleton + `--version` | fn | spec | all | `wiki/cli.md` | no | S | Subcommand tree. |
| L2 | `headroom proxy` | fn | spec | all | README CLI | no | S | `--port`. Depends on X1, L1. |
| L3 | `headroom doctor` | fn | spec | all | README CLI | no | M | Health/config/connectivity checks. |
| L4 | `headroom deploy` | fn | spec | all | README CLI | no | M | Turnkey local deployment. |
| L5 | `headroom wrap` core + `claude` | fn | spec | all | README "Supported Agents" | no | L | Env-var injection + config rewrite. **Split** — framework + first agent. |
| L6 | `headroom wrap` codex/cursor/aider | fn | spec | all | README | no | M | Depends on L5. |
| L7 | `headroom wrap` cline/continue/goose/openhands | fn | spec | all | REALIGNMENT Phase G | no | M | Depends on L5. |
| L8 | `headroom unwrap` | fn | spec | all | README CLI | no | S | Must fully restore pre-wrap config. Depends on L5. |
| L9 | `headroom perf` | fn | spec | all | README CLI | no | S | Latency/throughput metrics. |
| L10 | `headroom learn` | fn | spec | all | `docs/failure-learning.mdx` | no | L | Mines failed sessions; `--verbosity`. |
| L11 | `headroom update` | fn | spec | all | README CLI | no | M | `--check`, `--pre`; in-place upgrade. |
| L12 | `headroom savings` / `output-savings` | fn | spec | all | `docs/savings.mdx` | no | M | Savings ledger reporting. |
| L13 | `headroom init` / `inspect` / `tools` | fn | spec | all | `headroom/cli/` | no | M | Scaffolding + introspection helpers. |

### Memory & shared context

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Y1 | cross-agent memory store | type | spec | all | `docs/memory.mdx` | no | L | Auto-dedup + provenance tracking. **Split candidate.** |
| Y2 | `SharedContext` put/get | fn | spec | all | `docs/shared-context.mdx` | no | M | Multi-agent shared context. Depends on Y1. |
| Y3 | live-zone-tail memory injection | fn | spec | all | REALIGNMENT §2.6 | no | M | Memory goes in the live-zone tail — never the system prompt (I2). |

### Telemetry (TOIN)

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| N1 | `Telemetry` trait (observation-only) | trait | spec | all | REALIGNMENT §2.5 | no | S | **No request-time hint API** — that is invariant I9. |
| N2 | structure hashing + aggregation key | fn | spec | all | REALIGNMENT §2.5 | no | M | Key = `(auth_mode, model_family, structure_hash)`. |
| N3 | recommendations publish + startup load | fn | spec | all | REALIGNMENT §2.5 | no | M | `recommendations.toml`, read at startup only. |

### Output shaping

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| O1 | verbosity steering | fn | spec | all | README "Output Token Reduction" | no | M | `HEADROOM_OUTPUT_SHAPER=1`. Appends terseness note **without** busting the prompt cache. |
| O2 | effort routing | fn | spec | all | README | no | M | `reasoning_effort` (OpenAI) / `thinking.budget_tokens` (Anthropic); full effort on new questions and errors. |

### Python bindings

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| B1 | pyo3 module + `compress()` | fn | spec | all | README Python API | no | M | abi3-py310, built via maturin. Mirrors `await compress(messages, model=...)`. |
| B2 | `pyo3-log` bridging | fn | spec | all | reference workspace deps | no | S | Rust `tracing`/`log` → Python `logging`. Depends on B1. |

### Test infrastructure

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| E1 | upstream simulators | infra | spec | all | `crates/headroom-simulators` (name only) | no | M | Fake Anthropic/OpenAI endpoints for e2e without network. |
| E2 | invariant test gates I1–I4 | infra | spec | all | REALIGNMENT §2.2 | no | M | SHA-256 round-trip; hot-zone-unchanged; append-only; determinism. |
| E3 | SSE corner-case fixtures | infra | spec | all | REALIGNMENT Phase I | no | M | UTF-8 split, ping, all delta types, `[DONE]`, mid-stream error. |
| E4 | property tests | infra | spec | all | REALIGNMENT Phase I | no | M | No-panic SSE parser; tokens-non-increasing. |

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
