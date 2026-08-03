# Autonomous decision log

Decisions taken without checking in, per a standing instruction to work to parity
and log rather than ask. Newest last. Each entry says what was decided, why, and what
would change it.

---

## D1 — Batch gap rows into PRs rather than filing one issue per row
**2026-08-03**

`gap-analysis.md` has ~70 rows outstanding. Filing an issue per row and opening a PR
per issue costs more in overhead than it returns once the pattern is established.

From here: one issue per coherent *batch*, one PR per issue, with the batch body
listing which gap rows it closes. `gap-analysis.md` remains the authoritative roadmap
and the mapping stays explicit, so nothing becomes untraceable.

**Would change if:** a batch grows past roughly 800 lines of diff, in which case it
splits.

## D2 — Skip the Redis CCR backend; ship SQLite only
**2026-08-03**

Gap row R4 is an optional multi-worker backend. It needs a running Redis to test
against meaningfully, and this environment has none, so any implementation would ship
untested against a real server.

SQLite (R3) covers the persistence requirement and is fully testable here. R4 stays
open in `gap-analysis.md` and is called out as deliberately deferred rather than done.

**Would change if:** a multi-worker deployment is actually needed, or a Redis instance
becomes available to test against.

## D3 — Code compression is heuristic, not tree-sitter
**2026-08-03**

Gap rows C11-C13 call for AST-aware compression across seven languages. Full
tree-sitter grammars would add seven native dependencies and a large build surface for
a compressor whose job is to *elide function bodies* — a task a brace-and-indent
heuristic does acceptably.

Shipping a heuristic skeletonizer that handles all seven languages, documented plainly
as heuristic. It will be wrong on pathological input; invariant I5 catches that by
discarding any result that does not help.

**Would change if:** measured quality on real code proves poor enough to justify the
dependency weight.

## D4 — Python bindings deferred
**2026-08-03**

Gap rows B1-B2 need `maturin` and a Python toolchain to build and test. Neither is
available here, so the crate would compile at best and never be exercised.

Left unimplemented and marked as such rather than shipped blind.

**Would change if:** a Python toolchain is available, or Rust-only is confirmed as the
intent.

## D5 — ONNX, dashboard, and Bedrock/Vertex remain out of scope
**2026-08-03**

These were excluded at the start of this run and that has not changed. Noted here so
the decision log is a complete picture of what is deliberately absent.

## D6 — File-backed CCR store instead of SQLite
**2026-08-03**

Gap row R3 called for SQLite. A content-addressed store has fixed-size hex keys,
immutable values, and no query beyond point lookup — almost nothing SQLite offers,
against `rusqlite` bringing a bundled C library and a long build.

One file per hash, with an expiry sidecar and an atomic rename on write, gives the
same durability with no dependency. Shipped as `FileCcrStore` and documented as a
deliberate substitution rather than passed off as the SQLite backend.

**Would change if:** the store needs queries beyond point lookup, or entry counts grow
past what one directory handles comfortably.

## D7 — MSRV 1.80 is a real constraint, not a formality
**2026-08-03**

Twice now I have reached for `Option::is_none_or`, which reads better than
`map_or(true, ...)` and is stable only from Rust 1.82. Both times clippy's
`incompatible_msrv` caught it.

Keeping MSRV at 1.80 rather than raising it to match my habits: the declared floor is
a promise to anyone building this, and moving it to accommodate a stylistic preference
is the wrong trade. Where the older spelling is less clear, a comment says why.

**Would change if:** a dependency forces a higher floor anyway, at which point the
declared MSRV should move deliberately and be documented.

## D8 — Adding `reqwest` for the upstream relay
**2026-08-03**

The parity-loop rules say a new third-party dependency is a stop-and-ask, alongside a
breaking public API change. The standing instruction is to decide and log instead, so
this is the log entry.

The proxy could not forward a request to a provider, which meant it was a compression
library wearing a proxy's routing table. Closing that needs an HTTP client that speaks
TLS. The realistic options were `reqwest` or assembling `hyper-util` +
`hyper-rustls` + a connector by hand — the same transitive tree, minus the part that is
already tested by everyone else using it.

Taken: `reqwest` 0.13.3 with `default-features = false` and `rustls`, `http2`,
`stream`. Defaults off is the load-bearing part — the default feature set pulls
`native-tls`, which links the host OpenSSL and makes the build depend on whatever the
deployment image happens to ship. `stream` is what makes SSE relay possible at all;
without it the only way to read a body is to buffer it whole.

Version 0.13.3 specifically: 0.13.4 requires Rust 1.85 and this workspace declares
1.80 (see D7). Cargo picked the older point release on its own, which is the MSRV
floor doing its job rather than a manual pin.

**Would change if:** the MSRV moves past 1.85 anyway, at which point the pin can lift;
or a review objects to the dependency, in which case the hyper-based assembly is the
fallback and `Upstream` is the only type that changes.

## D9 — The relay has no total-request timeout
**2026-08-03**

A connect timeout is set (10s); a whole-request timeout is not.

A long generation is a normal outcome for this workload, not a stuck request. Any
total timeout generous enough never to truncate a legitimate long completion is far too
generous to catch anything that is genuinely hung — so it would cost real requests
without buying detection. The connect timeout is where a hang actually gets caught,
because a TCP/TLS handshake that has not completed in ten seconds is not going to.

**Would change if:** a deployment needs bounded resource usage more than it needs
uninterrupted long generations, in which case the timeout belongs in `Config` as an
opt-in rather than as a default.

## D10 — Runtime config uses an override map, not `std::env::set_var`
**2026-08-03**

Gap row F5 asks for configuration hot-reload. The direct implementation is to have
`POST /admin/runtime-env` write the process environment, since `Config::from_env`
already reads it live.

That would be a bug rather than a shortcut. `setenv` is not safe to call while another
thread may be in `getenv`, and this proxy reads its configuration on every request from
a thread pool — so a hot-reload would be racing every request in flight, with undefined
behavior rather than a stale read as the failure mode. It would also pass a bare
`cargo test` on most days, which is the worst property a data race can have.

Taken: an `RwLock<BTreeMap>` consulted ahead of the environment. An uncontended read
lock per lookup, and simply correct.

**Would change if:** nothing plausible. The environment is not a mutable global in a
multithreaded process, whatever the API suggests.

## D11 — Loop detection is a startup check, not a per-request header
**2026-08-03**

The usual way to catch a proxy forwarding to itself is a hop-count header: add one on
the way out, refuse when it comes back too high.

That is not available here. `crate::headers` exists because a header revealing that a
proxy is present is a subscription-revocation hazard, and a loop-detection header is
precisely such a header. Paying a fingerprint leak on *every* request to detect a
misconfiguration a startup check already catches is the wrong trade.

Taken: compare the configured upstream against the listen address at startup, treating
`localhost`, `127.0.0.0/8` and `::1` as equivalent, and refuse to start on a match.

**Would change if:** the proxy needs to detect loops through an *intermediate* hop,
which a startup check cannot see. That would need a header, and the fingerprint cost
would have to be weighed deliberately rather than assumed acceptable.

## D12 — Recommendations are published as JSON, not TOML
**2026-08-03**

Gap row N3 names `recommendations.toml`. `serde_json` is already a workspace dependency
with the features this project needs; `toml` is not.

Adding a dependency to change the syntax of a file that nothing outside this repo reads
is not a trade worth making. The content is a flat map of opaque keys to three fields,
which is equally readable either way.

**Would change if:** the file becomes something an operator hand-edits regularly, where
TOML's comment support and friendlier syntax would start to earn the dependency.

## D13 — Effort routing does not enable Anthropic extended thinking
**2026-08-03**

Gap row O2 names both `reasoning_effort` (OpenAI) and `thinking.budget_tokens`
(Anthropic). Only the first is injected.

Adding `reasoning_effort` to an OpenAI request changes how hard the model thinks and
nothing about the response shape. Adding a `thinking` block to an Anthropic request that
did not have one turns extended thinking *on*, and the response then carries thinking
blocks the client never asked for and may not parse. That is not a compression decision
being made on the customer's behalf; it is a change to their application's contract with
the provider.

Taken: inject `reasoning_effort` on the OpenAI route only. Adjusting an existing
`thinking.budget_tokens` would be safe and is the obvious next step; creating one is not.

**Would change if:** the budget adjustment is implemented for requests that already
enable thinking, which is a strictly additive follow-up.

## D14 — Lossless reformatting is permitted on OAuth but not on subscription
**2026-08-03**

Wiring the lossless reformatter, I first routed it for *all* restricted traffic on the
reasoning that a meaning-preserving transform cannot violate I10. A property test —
`a_restricted_policy_never_modifies_generated_input` — failed, and it was right to.

Reflowing a request's whitespace preserves its decoded meaning and still changes the
bytes a provider sees. That is exactly the fingerprint-class disclosure
`may_strip_accept_encoding` is off for: a subscription CLI serializes its JSON a
particular way, and traffic that has been reflowed is distinguishable from the same
client running unproxied.

Taken: `CompressionPolicy` gains an explicit `lossless_transforms` field.

- **PayAsYouGo** — true. Nothing to protect.
- **OAuth** — true. The hazard there is a modification exceeding the granted scope, and
  a meaning-preserving change cannot exceed a scope.
- **Subscription** — false. `compression_permitted()` now returns `false` for this mode,
  which is the honest answer rather than a disappointing one: every transform this crate
  has either rewrites content or reflows its bytes.

**Would change if:** a transform appears that reduces tokens without altering a single
byte of what the client sent, which is a contradiction in terms — so, in practice, not.

## D15 — WebSocket traffic is relayed, never compressed
**2026-08-03**

Gap row X13 asks for the Codex WebSocket flow. The implementation relays frames in both
directions and compresses nothing, which needs saying explicitly rather than reading as
an unfinished feature.

Two reasons, and the first is structural:

**There is no request boundary.** HTTP compression works because a request arrives
whole: the live zone can be identified, and the frozen prefix left alone. A socket is a
conversation with no such marker. A compressor would have to infer what had "already
been sent" from message content alone, and would be wrong the first time a client resent
context — which is exactly when it matters, because that is the expensive case.

**The client frames the messages.** A relay that recombined or split frames would have
changed the protocol beneath a library counting on it, and the failure would look like a
client bug.

Taken: `websocket::relay_socket`, a faithful bidirectional pipe. The value is that Codex
works through the proxy at all, not that its traffic shrinks. It records a passthrough
in the metrics so the savings ratio stays a claim about traffic something actually
compressed.

Dependencies: `tokio-tungstenite` (rustls, no default features) and `futures-util`,
both already in the tree transitively via axum.

**Would change if:** the transport gains a request-boundary marker, or the reference
demonstrates a safe way to identify a frozen prefix inside a socket conversation.

## D16 — tiktoken shipped, HuggingFace tokenizers deferred
**2026-08-03**

Gap rows T2 and T3 both ask for exact tokenizers. They are not comparable in cost.

**T2 (tiktoken) is shipped.** `tiktoken-rs` embeds the BPE vocabularies, so the counter
is exact offline with no downloads and no runtime configuration. Version 0.11 rather than
0.12 — the newer one requires Rust 1.85 and this workspace declares 1.80 (D7), and Cargo
selected the older release on its own.

**T3 (HuggingFace) is deferred.** The `tokenizers` crate does not embed vocabularies: it
loads a `tokenizer.json` per model, fetched from the Hub at runtime. That makes the
tokenizer a *network dependency of the request path* — the one place this project has
been careful to keep free of them — and it cannot be tested in this environment at all,
so it would ship unexercised.

It is also worth less than it looks. The models T3 would cover are Anthropic's, and
Anthropic does not publish a tokenizer; a HuggingFace tokenizer for `claude-*` would be
somebody's approximation with `is_exact()` returning `true`. That is worse than the
heuristic, which is honest about being an upper bound.

**Would change if:** vocabularies can be vendored at build time rather than fetched, or
a provider publishes an authoritative tokenizer worth registering as exact.
