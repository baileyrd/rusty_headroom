# Architecture

## Overview

A compression proxy between an LLM agent and a model provider. It shrinks the bulky parts
of a request — tool results, logs, search output, diffs, source files, prose — while
leaving untouched every byte the provider may already have cached.

The constraint is the architecture. Compressing a conversation naively saves tokens once
and invalidates the prompt cache for every request after it, which costs more than it
saved. Every boundary below exists to make the careful version structural rather than a
matter of remembering.

## Non-goals

Settled deliberately, with the reasoning in `DECISIONS.md`:

- Image, base64 and audio compression — no useful structure to exploit (reference §2.6).
- Translating between provider request shapes; each provider gets a native handler.
- An ML-based compressor (ONNX/Kompress), a dashboard, native Bedrock/Vertex routes.
- Compressing anything a person typed. Tool output is the target; a user's own message is
  not ours to rewrite (D24).

## Boundaries

Domain logic lives in `headroom-core` and knows nothing about HTTP, axum, or a provider.
The proxy, the CLI, the MCP server and the Python module are adapters over it.

| Port | Adapter(s) | Notes |
| --- | --- | --- |
| `Transform` | `SmartCrusher`, `LogCompressor`, `SearchCompressor`, `DiffCompressor`, `CodeCompressor`, `TextSummarizer`, `Reformatter` | Split into `LossyTransform` and `LosslessTransform` so I10 can gate them separately |
| `CcrStore` | `InMemoryCcrStore`, `FileCcrStore`, `RedisCcrStore` (feature-gated) | Chosen by configuration; Redis exists for multi-worker retrieval (D22) |
| `Tokenizer` | `HeuristicEstimator`, `TiktokenCounter` | Resolved per model by `tokenizer::Registry`; the heuristic never under-counts |
| `Telemetry` | the proxy's `Metrics` | Every method returns `()` — observation cannot influence a decision (I9) |

**One routing table.** `pipeline::Orchestrator` owns the decision of what compresses what.
The proxy, the CLI, the MCP server and the Python binding all route through it. They each
carried a copy once, and the copies drifted: the core's table had no arm for source code,
so the proxy forwarded every file whole while `headroom compress` reported a saving for
the same content (D23).

## Data flow

A request through `POST /v1/messages`:

1. **Classify the credential.** An API key, an OAuth token and a subscription session
   token get different policies. This decides what is permitted before anything is read.
2. **Parse byte-faithfully.** `FaithfulBody` keeps every message as a `RawValue`, so an
   untouched message is forwarded as the exact bytes that arrived (I1).
3. **Find the frozen floor.** Anthropic caches what the customer marks; both OpenAI
   surfaces cache prefixes automatically, so "everything but the newest turn" is the only
   correct reading there.
4. **Compute the live zone** — blocks after the floor that are compressible and not
   sacrosanct. Signed and encrypted blocks are excluded here *and* refused again at
   `apply_guarded`: two independent checks, because a modified signature makes the
   provider reject the whole request (I8).
5. **Route each block, record why, apply the transform.** `validated_apply` measures the
   result in tokens and discards it if it is not smaller (I5).
6. **Rebuild only the messages that changed.** Everything else, including the entire
   frozen prefix, is copied verbatim.
7. **Relay, and observe the response stream** with the classifier matching that surface.
   Anthropic, OpenAI chat and Responses frame their events differently, and reading one
   with another's vocabulary produces confidently wrong numbers rather than an error
   (D18).

## Structure

Modular monolith. Composition over inheritance. Ports-and-adapters keeps domain logic free
of I/O and framework details — the domain defines the interface, the adapter implements
it, and domain code never imports a backend directly.

A component gets extracted into its own service only for a concrete forcing function:
independent scaling, a team or language boundary, or hard fault isolation. Nothing here
has crossed that line. These components share a request path whose entire purpose is to be
cheaper than the model call it wraps, and a network hop between them would eat the saving.

`headroom-py` sits outside the workspace's `default-members` because a pyo3
`extension-module` does not link libpython. That keeps the everyday `cargo build` free of
a Python toolchain, at the cost of a separate CI job that actually builds and imports the
wheel — without which the binding could rot unnoticed.

## Key decisions

[DECISIONS.md](./DECISIONS.md) is the live record — what was decided, why, and what would
change it. Two entries reverse earlier decisions whose premises were written down without
being checked; they are annotated in place rather than rewritten, because that is the
failure worth remembering.

`docs/adr/` holds an unused ADR template from the initial repo scaffolding. It is not
maintained — prefer `DECISIONS.md`.

## What guards the guarantees

- `crates/headroom-proxy/tests/invariants.rs` — I1, I2, I3, I4, I6, I7, I8 and I9
  asserted end to end through a real relay rather than against the pure function, so a
  refactor that upholds every module contract and breaks the system property still fails.
- `crates/headroom-proxy/tests/properties.rs` — I5 and I10, which are claims about *many*
  inputs and cannot be established by one fixture.
- `scripts/reachability-audit.sh` — is every capability actually reached? Five were once
  shipped, tested, documented as done, and called by nothing. Its check 7 closes the loop
  on the two files above: each invariant must still have a test naming it, in the file
  this document says gates it, so deleting one fails the build rather than quietly
  turning these two lines into fiction.
