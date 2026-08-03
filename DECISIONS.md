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

> **Reversed on 2026-08-03 by D21.** The premise was wrong: Python 3.11 with headers is
> present and `maturin` installs cleanly. Kept here rather than deleted, because the
> failure worth remembering is that this was recorded as a fact without being checked.

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

## D17 — a structural keep-set outranks the line budget
**2026-08-03**

`signals::keep_with_required` takes a set of line indices that survive whatever the
importance heuristic makes of them — currently anchors (S4) and tag delimiters (S5). When
that set is larger than the caller's budget, **the budget loses**.

The alternative — trim the required set to fit — fails in a way nothing downstream can
detect. Dropping `</result>` hands the model markup that opens and never closes. Dropping
the last line of a report turns truncated output into output that reads as complete. The
model cannot tell that something is missing, so the error surfaces as a confidently wrong
answer rather than as a visible gap.

Overshooting is safe because invariant I5 already validates every compression against a
token count and forwards the original when the result is not actually smaller. A keep-set
that swallows the budget produces a block that fails that check and is discarded — a
missed saving, which is the recoverable direction.

**Consequence for future compressors:** a line-dropping compressor should feed
`keep_with_required` rather than `keep_most_important`, and should treat an empty required
set as a claim that nothing in its content type is structurally load-bearing.

**Would change if:** a caller appears whose budget is a hard ceiling rather than a target
— an output-size limit rather than a cost target. That caller needs its own entry point,
not a weakening of this one.

## D18 — the stream classifier is chosen by path, and reports only what it sees
**2026-08-03**

`sse::Observer::for_path` picks the vocabulary from the request path rather than sniffing
the response. Three surfaces are proxied and reading one with another's classifier does
not fail — it produces confidently wrong numbers, which is worse than none. A failing
OpenAI stream reported no failure at all before this, because its error frame carries no
`type` field for the Anthropic classifier to recognize.

Sniffing was rejected: it would have to buffer or guess from the first frame, and the
path is already known, unambiguous, and free.

**Cache usage is reported only where the provider sends it.** Neither OpenAI surface
carries cache figures in its stream, so both report zero. Deriving a number from request
size or a prior response would put a figure this proxy invented into the one metric it
exists to move.

**Would change if:** a provider adds cache reporting to its stream, which is a new field
to read rather than a change to this rule.

## D19 — memory injection is gated on the lossy permission, and fed from a file
**2026-08-03**

Gap row Y3 asks for live-zone-tail memory injection. Two decisions were needed to make
it reachable.

**Where the memories come from.** Nothing in this proxy populates a `MemoryStore`.
`MemoryStore` is in-process and in-memory by design (Y1), and the reference's MCP surface
is three tools — none of them a `remember`. Adding one would be inventing surface rather
than reaching parity. So memories are loaded from a JSON-lines file named by
`HEADROOM_MEMORY`, read **once at startup**, exactly as recommendations are. Reading per
request would let the same request produce different bytes depending on when it arrived,
and those bytes go upstream — busting the very cache the live-zone placement exists to
protect (I4). No file means no injection, so the default behaviour is unchanged.

**Which permission gates it.** `policy.lossy_transforms`, not `lossless_transforms`.
The lossless permission is granted on OAuth because a meaning-preserving change cannot
exceed a granted scope (D14). Injection is not a transform of anything: it adds content
the client never sent, which plainly can exceed a scope. So only pay-as-you-go traffic
gets it. Verified through the release binary — the same memory file injects under
`x-api-key` and injects nothing under an `sk-ant-oat` bearer.

**Would change if:** an agent-facing way to record memories lands, at which point the
file becomes one source among several rather than the only one. The injection point and
the permission gate do not change with it.

## D20 — cache stabilization is opt-in, because it modifies the zone I2 protects
**2026-08-03**

Wiring `stabilization` onto the request path made two I2 integration tests fail. They
were right, and this is the resolution.

Invariant I2 says the cache hot zone — `system`, `tools[*]`, frozen messages — is never
modified. Both remaining stabilization features modify it: normalizing tools rewrites
`tools`, and placing a `cache_control` breakpoint rewrites a frozen message. There is no
placement that avoids this. A marker on the *newest* message would be in the live zone
and legal, and would also be worse than useless: the marker moves next turn, so the
prefix it caused to be cached no longer matches, and Anthropic bills cache writes at a
premium. It would pay to write a cache that is never read.

So the trade is real but it is a trade — one miss now for hits later — and its sign
depends on the deployment. A client that already serializes its tools stably pays the
miss and gains nothing. **The operator decides, via `HEADROOM_STABILIZE`, and the default
is off.** The I2 tests run against the default, so they keep enforcing the invariant
rather than being relaxed to accommodate a feature.

**Breakpoints sit at fixed anchors — 1, 3, 7, 15 — not an even spread.** The even spread
that was implemented recomputes to a *different* set every couple of turns, and the index
that moves first is the earliest one; moving it rewrites the head of the prefix and
invalidates the whole cache. Fixed anchors are monotone: breakpoints are only ever added,
never moved, so every prefix that was cached stays cached. Modelling the old rule across
turns showed the set changing at 6, 10, 14 and 18 messages — the feature would have busted
the cache every two turns on exactly the long conversations it exists to help.

**Would change if:** a provider offers a breakpoint mechanism that lives outside the
request body — a header, or a handle — at which point stabilization stops touching the
hot zone and the gate is no longer needed.


## D21 — Python bindings implemented; D4 reversed
**2026-08-03**

D4 deferred gap rows B1-B2 because "neither maturin nor a Python toolchain is available
here". That was asserted, not checked. Python 3.11.15 with headers is present, `pip` is
present, and `maturin` installs cleanly — and D4's own "would change if" names exactly
this condition. Python interop was explicitly in scope for this run, so the deferral had
been withholding the one in-scope item for a reason that did not hold.

**The binding routes through `Orchestrator`.** Assembling a compressor set inside the
extension module would have been shorter and is the mistake this codebase already made
once: the proxy carried its own copy of the routing decision, the CLI carried another,
and nothing failed when they drifted. Verified rather than asserted — the same log
compresses to a byte-identical result through `headroom.compress()` and through
`headroom compress` on the CLI.

**Nothing but strings and numbers crosses the boundary, and the CCR store is per call.**
A store living for the process would let one caller retrieve content from a request they
never made. The cost is that a `<<ccr:HASH>>` marker in returned text is not retrievable
through this API; callers who need retrieval want the proxy or the MCP
`headroom_retrieve` tool, both of which own a store with a defined lifetime and scope.

**An unknown `auth_mode` raises rather than defaulting.** Defaulting would hand the most
permissive policy to a caller who misspelled the most restrictive one — invariant I10
decided by a typo.

**`headroom-py` stays out of `default-members`.** The everyday `cargo build` and
`cargo test` loop needs no Python toolchain, which also means the Rust CI job never
builds the extension module — so the wheel gets its own CI job that builds, installs and
runs `pytest` against it. A binding that is never imported in CI is exactly the state D4
was worried about.

**Would change if:** nothing foreseeable. If a Python toolchain became unavailable, the
Rust jobs would still pass and only the `python` job would fail, which is the correct
signal rather than a silent regression.