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

> **Premise corrected on 2026-08-03.** "This environment has none" was asserted without
> being checked, and it is false: `redis-server` is installed at `/usr/bin/redis-server`
> and was confirmed to start and answer `PING`. Found by re-checking every environmental
> deferral after D4 turned out to be wrong the same way (see D21).
>
> **The deferral itself still stands, on a different and honest reason.** R4 needs the
> `redis` crate — a third-party dependency the scope decision for this run did not name,
> unlike `pyo3`/`maturin` which it did. The reference calls this backend optional
> (REALIGNMENT §2.5), and adding a dependency for an optional feature is the owner's call
> rather than one to make unattended. Implementation is contained when wanted: a
> `RedisCcrStore` behind the existing `CcrStore` trait, alongside `FileCcrStore` (D6).
>
> Recorded this way deliberately. A deferral resting on a false premise reads as settled
> when it is not, and that is precisely how B1/B2 stayed unbuilt for no reason.
>
> **Superseded on 2026-08-03 by D22.** R4 is implemented behind an off-by-default
> feature, so no build takes the dependency unless it asks for it.

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

> **Now defended by a test, 2026-08-03.** This was a decision with nothing enforcing it.
> Adding `.timeout(30s)` to the upstream client is a natural review suggestion, and it
> would have started truncating real completions with every test still green.
>
> `a_stream_that_pauses_mid_generation_still_arrives_whole` relays a stream that pauses
> mid-frame and asserts it arrives whole; adding a 500 ms total timeout fails it. The
> pause is short, so this cannot prove the absence of a timeout — only of an aggressive
> one. What it pins is that the relay holds a stream open across a gap rather than ending
> it, which is the behaviour this decision chooses.

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

> **Demonstrated end to end on 2026-08-03.** The reasoning above was argued from the data
> flow, not observed, so it was tested with the release binaries across two real processes.
>
> `headroom learn` was run over a 14-request corpus of high-entropy content that
> compresses to nothing, producing one entry with `worth_compressing: false`. A separate
> `headroom-proxy` process was then started with `HEADROOM_RECOMMENDATIONS` pointing at
> that file and sent one request of the same shape:
>
> | build | key `learn` wrote | proxy's routing verdict |
> | --- | --- | --- |
> | FNV-1a, as shipped | `payg\|claude-opus\|f9439d246d8721bc` | `measured_useless 1` |
> | FNV-1a swapped for a per-process seed | `payg\|claude-opus\|e11f96ea63309f44` | `compress 1`, `measured_useless 0` |
>
> The second row is the failure this decision prevents, and it is silent: no error, no
> warning, just a proxy re-compressing a shape it had already measured as useless while
> every counter looks healthy. `the_fingerprint_is_pinned_to_a_literal` is what turns that
> into a build failure.

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

## D22 — the Redis CCR backend ships behind an off-by-default feature
**2026-08-03**

D2 deferred gap row R4 twice over: first on a false premise (no Redis here — there is),
then on a real concern, that the `redis` crate is a dependency this run's scope did not
name for a backend the reference calls optional.

**The feature gate resolves the concern rather than overriding it.** `redis` is off by
default, so a build that does not ask for the backend does not compile it, does not
resolve it, and does not carry it. `default-features = false` on the crate keeps out the
async runtimes and TLS stacks its default build pulls in — CCR does one round trip per
compressed block against a deliberately synchronous store, so none of that is used.

**What R4 actually fixes turned out to be broken already.** The proxy constructed an
in-memory CCR store *unconditionally*. That drops every stored original on restart, so a
`<<ccr:HASH>>` marker the model is still holding becomes unretrievable; and with two
workers the marker is created on one process and requested from another that never saw
it. `Config::ccr_store()` now selects Redis, then a directory, then memory — which also
made `FileCcrStore` reachable from the proxy for the first time.

**The MCP server selects the same way**, because it is the *retrieval* half: the proxy
stores and the model calls `headroom_retrieve`, which arrives in a different process. A
local store there answers "expired" for everything the proxy compressed.

**Expiry is the server's job.** Entries are written with `SET ... EX`, and
`purge_expired` returns zero — the truth, not a stub. Sweeping from every worker would be
several processes racing to delete the same keys while disagreeing about a clock none of
them owns, which is the problem a shared store exists to remove.

**A feature-off build with `HEADROOM_REDIS_URL` set says so.** It does not silently fall
back to memory: the symptom of that — retrievals failing on some workers — is identical
to a Redis that is down, and an operator would debug the wrong thing.

**Verified across processes, not just in tests.** The proxy compressed a tool result to a
marker with Redis configured, then exited; the key survived, and a separate
`headroom-mcp` process retrieved the original byte-identical
(`303310e7145adcb6…`, 24,579 bytes).

**Would change if:** CCR traffic ever approached the volume where one mutexed connection
matters, at which point the store wants a pool. It is far below that today — one round
trip per compressed block, against a model call that costs orders of magnitude more.

## D23 — one routing table, asked rather than restated
**2026-08-03**

The pipeline refactor moved routing into `headroom-core` so the proxy and the CLI could
not disagree. It did not remove their copies. Three existed — `Orchestrator`,
`headroom-cli`'s private `route()`, and `McpServer::compress` — and they had already
drifted: the core's table had no `ContentType::Code` arm while the other two did.

The consequence was not subtle. The proxy forwarded every source file uncompressed, and
`headroom compress --dry-run` — the command whose entire purpose is to predict what the
proxy will do — reported a 32% saving for the same content.

All three now go through `Orchestrator::transform_for`.

**Inside the orchestrator there were two lists as well.** `route` matched on content type
to decide "is this compressible", and `for_type` matched again to pick the compressor.
Adding a code arm to `for_type` alone did not make code route — the first list still said
no. `route` now asks `for_type` rather than restating it, so there is one place that
knows which types have a compressor.

**The CLI and MCP paths pass `AuthMode::PayAsYouGo`.** That is the operator compressing
their own content on their own machine, not a relayed request whose credential decides
what is permitted. Invariant I10 governs traffic the proxy relays, and the proxy still
applies the real policy there.

**Would change if:** a fourth caller appears. It should take an `Orchestrator` rather
than build a compressor set, and this entry is the reason why.

**Amended — there were five, and this entry said there were three.** `headroom inspect`
and `headroom tools` both carried their own table the whole time. Counting the callers by
memory rather than by search is how they were missed, and the entry above then recorded
the sweep as finished.

It had drifted in the worst possible direction: it mapped `ContentType::Prose` to
`"none"`, which stopped being true when the prose compressors were wired in. So the
command whose one purpose is to answer *"why did this not compress"* answered
`compressor: none` for 18 KB of prose that `headroom compress` shrank by 70% — same
content, same shell, seconds apart.

It now reports `Orchestrator::route`'s reason and `transform_for_block`'s transform, for
each auth mode and each block kind, because those two dimensions are what change the
answer and picking one silently is what made a single-line report wrong.

`headroom tools` was worse. Its second list — "detected but not compressed" — named code
and prose, both of which compress, so the command that exists to say what a build can do
reported the two largest categories of agent traffic as forwarded whole. It now reads
`for_type` across `ContentType::ALL`; a type is in the second list exactly when nothing
compresses it.

**`for_type` is public now, and that is the point.** Keeping the table private did not
stop anyone needing it; it made five people write it out again. A caller with content
should still use `route` or `transform_for_block`, which apply policy and the block-kind
rule as well. `for_type` is for callers that have a *type* and want to report on the
build. `tool_output_only` exists for the same reason: D24 was being described in prose by
`tools` while `transform_for_block` enforced it in code, and the two are now one function.

Counting from memory is what failed here, so the replacement does not count. Check 6 of
`scripts/reachability-audit.sh` fails the build on a content type paired with a
compressor's name as a string literal (6a), or on a match with three or more
`ContentType` arms outside two allowlisted files (6b).

**6b was written first, and it does not catch `headroom tools`** — that copy was a tuple
in an array, not a match arm. It was found by reading the file, not by the check written
to find it, which is exactly the cry-wolf-in-reverse failure worth recording: a guard that
passes is not evidence, unless you have watched it fail. 6a anchors on the compressor name
instead, because a real one only ever comes from `Transform::name()`.

## D24 — prose is compressed only when it came from a tool
**2026-08-03**

Gap row C10's compressors were registered nowhere, so the proxy forwarded every prose
tool result whole. Routing them exposed a question the other content types do not raise.

`BlockKind::is_compressible()` includes `Text` — what a user typed or a model wrote — and
`TextSummarizer` is lossy: it drops low-importance lines behind a CCR marker. Applying
that to a directory listing is the product working. Applying it to somebody's message is
rewriting what they said, and no token saving is worth that.

So `Orchestrator::transform_for_block` routes prose only when
`block.kind().is_tool_output()`. Every other content type is exempt from the rule
deliberately: a person does not type a 5 KB unified diff into a chat box, and if they do,
compressing it is what they were asking for. Narrowing further would exempt content the
proxy exists to compress.

**This makes the proxy and the content-only callers differ, on purpose.** `transform_for`
has no block to inspect and is what `headroom compress`, the MCP tool and the Python
binding use — a caller who handed content over has asked for it to be compressed. Given
D23 was about eliminating exactly this kind of divergence, the difference lives in two
named entry points with the reason in both doc comments, rather than in a flag someone
has to remember.

**It also un-broke S4 and S5.** Those rows were closed by wiring the anchor and tag
keep-sets into `TextSummarizer`, and reported as reached from the request path. They were
not: the compressor holding them was itself unreachable. They only started running on
proxied traffic here. Verified end to end — a tagged 22 KB tool result compresses to
4.5 KB with `</result>` intact, and a 21 KB one keeps its final line.

**Would change if:** a block kind appears that carries authored text but is not `Text`.
The check is on `is_tool_output`, so a new tool-output kind is covered automatically and
a new authored kind is protected automatically. That is why the check is phrased
positively.

## D25 — the `accept: */*` leak is documented, not hidden
**2026-08-03**

`headers::sanitize` decides what this crate forwards, and is well tested. Nothing tested
what the *provider actually receives*, which is a different question: an HTTP client adds
headers of its own below the layer this code controls.

An end-to-end probe found one. When the client sends no `accept`, the provider receives
`accept: */*`. It is not added by this crate and is not present on the request reqwest
builds — the client stack injects it further down, and reqwest exposes no option to
suppress it.

**This is a real leak of the class D14 refuses to pay.** The whole subscription policy
exists so that proxied traffic is not distinguishable from the same client running
unproxied, and an added header is exactly that kind of evidence.

**It ships anyway, for one reason:** `accept: */*` is the most common header value on the
internet and identifies no proxy in particular. A second added header, or one that named
this software, would be a different decision. `the_proxy_adds_no_header_the_client_did_not_send`
pins the leak to exactly this header, so it cannot quietly grow — the test passes today
and fails the moment anything else appears.

A client that *does* send `accept` has it forwarded verbatim, covered by
`a_client_supplied_accept_still_reaches_the_provider`. Stripping the injected one must
never strip a real one: a client asking for `text/event-stream` and not getting it is a
client whose streaming silently stops working.

**Why there is no way to suppress it, read rather than assumed.** The first version of
this entry said reqwest "exposes no option" without checking. In `reqwest 0.13.4`:

- `ClientBuilder::new` inserts `ACCEPT: */*` into the client's default headers
  (`async_impl/client.rs:284`), before any caller can intervene.
- `ClientBuilder::default_headers` **extends** that map rather than replacing it
  (`:1166`), so passing an empty `HeaderMap` cannot remove the entry.
- At execute time the defaults fill only *vacant* entries (`:2616`). A header set on the
  request wins — but there is no way to express *absent*, which is what a faithful relay
  needs.

So a header the client did not send is added, and the three mechanisms compose such that
no caller-side workaround exists. That is worth knowing precisely rather than vaguely: it
tells a future reader exactly what to re-check when reqwest updates.

**Would change if:** reqwest lets a client be built without the default, or the leak
grows. The fix is upstream. Removing the header from the built request does nothing —
tried, measured (`on_request=None`), and reverted rather than shipped looking like a fix.

## D26 — the loop guard runs on hot-reload too
**2026-08-03**

D11 argued that no per-request loop-detection header is needed because a startup check
already catches a self-referential upstream, and that paying a fingerprint leak on every
request to detect a misconfiguration was the wrong trade. That reasoning was sound when it
was written.

**D10 then added runtime config, and the premise stopped holding.** `POST
/admin/runtime-env` could set `HEADROOM_UPSTREAM` to the proxy's own listen address after
startup, and nothing re-checked. Demonstrated rather than argued: a probe set the override
and read the resulting config back — `upstream=http://127.0.0.1:8787 listen=127.0.0.1:8787
self_referential=true`. Every request would then forward to itself forever, with a pinned
core and exhausted file descriptors instead of an error anyone can read.

The guard now runs on the hot-reload path as well, against the configuration the overrides
*would* produce rather than the one requested — the listen address may be overridden in the
same call.

**It previews rather than applying and rolling back.** Applying first would make the bad
configuration live for the duration of the check, and this proxy reads its config per
request from a thread pool, so an in-flight request could pick it up in that window and
start the very loop being checked for.

**The refusal is a 400, not `refuse`'s 403.** The caller is local and allowed to be here;
their configuration is what is wrong. A permission error would send an operator hunting
for an access problem during the incident they are already trying to fix.

**Would change if:** loops through an *intermediate* hop become a concern, which neither
check can see.
