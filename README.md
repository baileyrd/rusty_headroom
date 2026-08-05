# rusty_headroom

A cache-safe compression proxy for LLM agent traffic. It sits between an agent and a
model provider, shrinks the bulky parts of a request — tool results, logs, search output,
diffs, source files — and is careful never to disturb the bytes the provider has already
cached.

The careful part is the point. Naively compressing a conversation saves tokens on one
request and invalidates the prompt cache for every request after it, which costs more
than it saved. Everything here is built around not doing that.

## Status

Working, and not yet released. Six crates, a Python extension module, and a test suite
that runs the guarantees end to end through a real relay. There are no version tags; the
unit of change is a merged pull request, recorded in [RELEASE_NOTES.md](./RELEASE_NOTES.md).

## Getting started

```bash
git clone https://github.com/baileyrd/rusty_headroom.git
cd rusty_headroom
cargo build --release

# Point it at a provider and run it.
HEADROOM_UPSTREAM=https://api.anthropic.com ./target/release/headroom-proxy

# Then send the agent's traffic to http://127.0.0.1:8787 instead.
```

`headroom wrap <agent>` prints the environment that routes a supported agent through the
proxy, and `headroom unwrap` undoes it.

## What it does to a request

Compression touches the **live zone** only — the newest turn, the part the provider has
not cached. Everything before it is forwarded as the exact bytes that arrived.

| content | compressor |
| --- | --- |
| JSON | `SmartCrusher` — record arrays summarized, outliers kept verbatim |
| logs | `LogCompressor` — repeated lines templated |
| search results | `SearchCompressor` |
| diffs | `DiffCompressor` |
| source code | `CodeCompressor` — heuristic skeletonization |
| prose | `TextSummarizer` — **tool output only**, never what a person typed |

The proxy knows which is which from the conversation. Nothing else does, so every other
surface takes it from the caller and defaults to tool output: `headroom compress
--kind tool-output|text`, a `kind` property on the `headroom_compress` MCP tool, and
`headroom.compress(content, kind=...)` in Python. Passing `text` declines prose
compression — worth doing for anything a person wrote, because the CLI and the Python
module discard their CCR store when the call ends, so a summary there cannot be redeemed.

Anything lossy is stored first under a content hash, and the compressed block carries a
`<<ccr:HASH>>` marker the model can redeem through the `headroom_retrieve` MCP tool. So
compression is a bet that the detail will not be needed, and a bet that can be unwound.

Retrieval checks the hash and nothing else — no session, credential, or requester
identity. That is what lets the MCP server redeem a marker the proxy wrote in a
different process; it also means `HEADROOM_CCR_DIR`/`HEADROOM_REDIS_URL` must point at
a store scoped to one customer's traffic. A store (especially a shared Redis instance)
serving more than one customer's proxy lets any of them retrieve any other's compressed
originals by presenting a hash they obtained or guessed.

## The invariants

These are acceptance criteria on every change, not aspirations. Eight are gated end to
end in `crates/headroom-proxy/tests/invariants.rs`; I5 and I10 are gated by
`tests/properties.rs`, because both are claims about many inputs rather than one.

| | |
| --- | --- |
| I1 | Byte-faithful passthrough — unmutated bytes arrive SHA-256 identical |
| I2 | The cache hot zone — `system`, `tools[*]`, frozen messages — is never modified |
| I3 | Append-only: compression touches the live zone and nothing earlier |
| I4 | Determinism — same input, byte-equal output, every run |
| I5 | Token-aware: validated after compression, original forwarded if not smaller |
| I6 | Position-preserving |
| I7 | Tool definitions are normalized at most, never compressed |
| I8 | Signed, encrypted and redacted blocks are passthrough-only |
| I9 | Telemetry observes and never alters |
| I10 | The auth mode gates what compression is permitted |

**I10 in practice:** a direct API key gets full compression. An OAuth token gets
lossless transforms only, because a modification could exceed the granted scope. A
subscription session token gets nothing — reflowing bytes makes traffic distinguishable
from the same client running unproxied, and that disclosure is not worth a token saving.

## Configuration

Read from the environment. **Restart** marks the ones read once at startup, which a
running proxy will not pick up — the authoritative list is `config::STARTUP_ONLY`, and
`POST /admin/runtime-env` reports exactly these under `needs_restart`.

| variable | restart | effect |
| --- | --- | --- |
| `HEADROOM_UPSTREAM` | yes | provider base URL (default `https://api.anthropic.com`) |
| `HEADROOM_HOST` / `HEADROOM_PORT` | yes | listen address (default loopback, `8787`) |
| `HEADROOM_CCR_DIR` | yes | directory for retrievable originals; memory only if unset |
| `HEADROOM_REDIS_URL` | yes | shared store for multi-worker deployments (needs `--features redis`) |
| `HEADROOM_RECOMMENDATIONS` | yes | file from `headroom learn` |
| `HEADROOM_MEMORY` / `HEADROOM_MEMORY_LIMIT` | yes | JSON-lines memories to inject into the live-zone tail — one object per line with a `content` string; 8 at a time by default |
| `HEADROOM_COMPRESSION` | no | `0` forwards everything untouched |
| `HEADROOM_STABILIZE` | no | `1` normalizes tools and places cache breakpoints — **off by default**, it modifies the zone I2 protects |
| `HEADROOM_OUTPUT_SHAPER` | no | `terse` or `full`; off unless set |
| `HEADROOM_LOG` | yes | log filter (default `warn`; logs go to stderr); not settable via `/admin/runtime-env` at all — it isn't part of `config::vars`, and `main` reads it once through `tracing_subscriber::EnvFilter` before the global subscriber is installed |

`HEADROOM_UPSTREAM` is on that list because the relay client is built once with its base
URL baked in. It was *not* on it until measured: the admin endpoint answered
`needs_restart: []`, `/health` reported the new address, and traffic kept going to the old
one — three self-reports agreeing with each other and all three wrong.

`GET /metrics` reports savings, cache usage, and — the useful one — a per-reason
breakdown of *why* traffic was or was not compressed. `headroom_expanded_total` counts
requests this proxy made **larger**: memory injection adds content by design, so that is a
real outcome and not an error, but it is not compression and is no longer counted as it.

`headroom_passthrough_total` counts requests **no compression applied to** — not requests
forwarded unchanged, which is what it used to say. On the two OpenAI routes a passthrough
request still goes out enriched with `prompt_cache_key` and `reasoning_effort`; measured, a
62-byte chat request leaves as 88. Everything the client sent survives byte-for-byte
either way, which is what I1 promises — but "unchanged" was a stronger claim than the
counter can support, and it sat two sections below I1 where it read as the same guarantee.

`headroom_cache_read_tokens_total` and `headroom_cache_creation_tokens_total` are read off
the response stream, in each provider's own vocabulary, for all three proxied surfaces.
Two cases report a zero that means *the provider said nothing* rather than *the provider
said zero*, and the metric cannot tell them apart:

- **Chat completions send their usage chunk only when the client asks for it** — the
  request has to carry `stream_options: {"include_usage": true}`. A client that omits it
  gets a stream with no totals in it at all, and this proxy will not add the field to
  someone else's request to make its own numbers look better.
- **`cache_write_tokens` exists only on the model families that bill for cache writes.**
  On older models a fully-cached prompt reports reads with no writes, so
  `headroom_cache_hit_rate` reads 1.0 — correct under the ratio's definition (reads over
  reads-plus-writes), and worth knowing before treating it as a fraction of the prompt.

Neither is something a proxy can fix from where it sits, and substituting a guess for
either would corrupt the one number the whole thing exists to move.

`GET /health` reports `ccr_store` (`memory`, `file` or `redis`) and
`ccr_store_persistent`. Two fields rather than one because `memory` means two different
things: the default nobody changed, and a configured store that could not be opened. The
second case is the one worth alerting on — a `HEADROOM_CCR_DIR` the process cannot open
falls back to memory and logs a warning, after which the proxy keeps relaying, keeps
compressing, and keeps handing the model `<<ccr:HASH>>` markers that no longer survive a
restart. Compression is a bet that can be unwound; in that state it quietly is not.

`POST /admin/runtime-env` (loopback only) retunes a running proxy. Settings marked **no**
above take effect on the next request; the rest are stored and named under `needs_restart`
rather than letting you believe the change took. All three live settings were checked
against a running proxy rather than assumed: `HEADROOM_COMPRESSION`,
`HEADROOM_OUTPUT_SHAPER` and `HEADROOM_STABILIZE` each change the next request's forwarded
body in both directions.

Calls **merge** over the overrides already in force. Send a setting as an empty value to
take it back; `{}` changes nothing. It used to replace, which meant an operator who turned
compression off during an incident had it silently turned back on by their next retune of
anything else — with `applied: ["HEADROOM_STABILIZE"]`, or whatever they had just set, as
the only report.

## Development

```bash
scripts/reachability-audit.sh      # is every capability actually reached?
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

CI runs all four, plus a job that builds and imports the Python wheel and a job that
exercises the Redis store against a real server.

**Run the audit before trusting a green test suite.** Five capabilities in this
repository were once shipped, tested, documented as done, and never called by anything.
A test proves a function works; it does not prove that anything calls it. The script's
header records which gap motivated each check.

Two of its checks ask related questions. The sixth: does anything carry a *second copy*
of the routing table? Eight places did, and each eventually disagreed with the pipeline it
was describing — including both commands whose whole purpose is to describe it. The
seventh: does every invariant listed above still have a test naming it, in the file this
README says gates it? A guarantee nothing checks is worse than one nobody claimed.

## Layout

```
crates/
├── headroom-core/        compression engine, tokenizers, CCR, signals, pipeline
├── headroom-proxy/       axum proxy, SSE, cache stabilization, observability
├── headroom-mcp/         MCP server over stdio — compress, retrieve, stats
├── headroom-cli/         the `headroom` binary
├── headroom-simulators/  loopback provider fakes for end-to-end tests
└── headroom-py/          pyo3 extension module (built with maturin, not cargo)
```

`headroom-py` is deliberately outside `default-members`, so the everyday build needs no
Python toolchain.

## Where the reasoning lives

[DECISIONS.md](./DECISIONS.md) is the live decision record — each entry has what was
decided, why, and what would change it. Two of them reverse earlier decisions whose
premises turned out never to have been checked. (This line used to carry a count of the
entries. It was wrong by two, which is a small instance of exactly what `scripts/reachability-audit.sh`
check 6 is about: a second copy of a fact, drifting from the first.)

`docs/adr/` holds an unused ADR template from the repo scaffolding. `DECISIONS.md` is the
one that is maintained; prefer it.

- [ARCHITECTURE.md](./ARCHITECTURE.md) — boundaries and data flow
- [gap-analysis.md](./gap-analysis.md) — the parity tracker, with what reaches each row
- [CONTRIBUTING.md](./CONTRIBUTING.md) · [SECURITY.md](./SECURITY.md)

## License

Apache-2.0. See [NOTICE](./NOTICE) for credit to the reference project this
re-implementation follows.
