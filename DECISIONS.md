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

## D27 — the Python binding's reason strings change to match the rest
**2026-08-03**

`headroom-py` mapped the `Routing` variants itself and spelled three of the six with
hyphens: `policy-forbids`, `no-compressor`, `measured-useless`. The proxy reports the same
decisions as `policy_forbids` under `headroom_routing_total{reason=...}`, and `headroom
inspect` prints the same. So a caller correlating a Python result against a dashboard
matched nothing — and the reason field exists precisely so it can be correlated.

The underscored spelling wins: it is `Routing::as_str`, it is the Prometheus label, it is
what every other surface already says. Nothing forces hyphens on the Python side.

`compress()` now reports `routing.as_str()` directly. The one reason routing cannot
produce — a transform ran and its output was not smaller — becomes `not_smaller`, and the
module exports `headroom.REASONS`, built from `Routing::REASONS`, so a caller can
enumerate the vocabulary rather than write it down a ninth time.

**This changes values a published package returns, and is logged rather than asked
about.** The standing instruction is to decide and log. The package has never been
published — no version tags, `headroom-py` is outside `default-members`, and the wheel is
built in CI and thrown away — so there is no caller to break. Had it shipped, this would
have been a stop-and-ask, and the right fix then would have been to report both spellings
for a release before removing one.

**Would change if:** the module is published. After that, a value in `REASONS` is a
compatibility surface, and the exhaustiveness test in core is what tells you a change is
about to reach it.

## D28 — the upstream stays startup-only; the reports change instead
**2026-08-03**

`HEADROOM_UPSTREAM` was absent from `STARTUP_ONLY`, and it is startup-only in fact:
`AppState::new` builds one `Upstream` and bakes the base URL into it, and the request path
uses that client without re-reading configuration.

Measured against two loopback providers, with the proxy started on A and told to use B:

```
admin       : {"applied":["HEADROOM_UPSTREAM"],"needs_restart":[]}
after       : {"served_by": "UPSTREAM-A"}
health says : http://127.0.0.1:9102
```

Three self-reports agreeing with each other and all three wrong, on the single setting an
operator is most likely to change mid-incident and the one whose silent failure is
hardest to spot — traffic keeps flowing, to the wrong provider.

**Two ways to fix it, and the reasons for choosing this one.** The relay's client is
reusable across base URLs; only the `base` string is per-destination. So `forward` could
take the base per request, making the setting genuinely live and matching what the README
already claimed.

Not taken. Every other once-read setting here is deliberately startup-only, with the
reasoning recorded: the CCR store is opened once, and memories and recommendations are
loaded once so the same request cannot compress differently depending on when it arrived
(I4). The upstream being fixed for a process's life is consistent with that, and the bug
was never the lifecycle — it was that three components claimed otherwise. Making it live
would also have meant either changing `Upstream::new`'s signature or giving `AppState` a
test-only override path, and a test-only path through the request handler is how tests
stop reflecting production.

So: `UPSTREAM` joins `STARTUP_ONLY`, `/health` reports the base from the built relay
rather than from configuration, and the README marks which settings need a restart.

**Would change if:** an operator actually needs to repoint a running proxy without
dropping connections. Then `forward` takes the base per request, `AppState` keeps an
explicit override for tests rather than an implicit one, and the self-referential check in
`admin` becomes the only thing standing between a live change and a proxy pointed at
itself — which is worth designing deliberately rather than inheriting.

## D29 — the estimator's "never under-counts" claim is corrected, not rescued
**2026-08-03**

Four files stated that `HeuristicEstimator` never under-counts, and invariant I5's safety
rested on it: `validated_apply` discards a compression whose result is not smaller *in
estimated tokens*, so an estimator that under-counts the compressed form forwards a
"compression" that grew the prompt. The module's own header called that failure "much
worse" than the alternative, and silent.

It had never been checked against the tokenizer it approximates. Measured against
`gpt-4o`, it under-counted four realistic content classes:

| content | heuristic | tiktoken | ratio |
| --- | --- | --- | --- |
| log lines | 1051 | 1139 | 0.92 |
| hex digests | 183 | 220 | 0.83 |
| base64 | 220 | 421 | 0.52 |
| whitespace runs | 1 | 501 | 0.00 |

Logs mattered most: a first-class content type with its own compressor, under-counted by
8%, so a log compression measuring a 5% saving could have grown the real prompt and been
forwarded anyway.

**Fixed, for realistic content.** Digits are charged separately (they group in threes at
most, and timestamps make a log line mostly digits); alphanumeric runs longer than a word
are charged at the dense rate measured for base64; whitespace runs are sized rather than
counted, splitting uniform runs — which merge, 64 spaces being one token — from mixed
runs, which do not.

**Not fixed, and not fixable this way.** Random alphanumeric strings still under-count:
25.8% of 12,000 generated inputs, worst `"EYM3Dgnc6"` at 3 estimated against 7 actual.
`"Dgnc"` and `"Word"` are the same string to a classifier that cannot consult the merge
tables, and they cost 4 tokens and 1. Charging every short run at the dense rate would put
ordinary prose at roughly eight times its true count and suppress compression everywhere,
which is the failure the estimator exists to avoid in the other direction.

So the claim is corrected in all four places rather than softened, the realistic property
is pinned by a differential test, and the remaining exposure is pinned by a bound that can
only be tightened. A caller needing a true bound wants an exact tokenizer — every OpenAI
family resolves to one, and `is_exact_for` reports which it got.

**A first attempt over-corrected**, and the second test caught it: charging whitespace by
length alone put 24 spaces of indentation at 12 tokens against an actual 1, a 3.3x
over-count on exactly the content this proxy compresses. A safety fix that suppresses
compression everywhere is not a safety fix.

**Would change if:** an exact tokenizer for the Anthropic families becomes available, at
which point the fallback stops carrying I5 for the traffic this proxy mostly sees. Failing
that, the honest next step is measuring how often real agent traffic contains the shapes
that under-count, rather than assuming it does not.

---

## D30 — cache accounting is read from all three dialects, not just Anthropic

**Decision:** parse provider-reported cache tokens from the OpenAI chat-completion and
Responses streams as well as the Anthropic one, and delete the claim that neither OpenAI
surface reports them.

`Observer::cache_tokens` returned a hardcoded `(0, 0)` for both OpenAI dialects, under a
comment that read: *"Zero for both OpenAI surfaces because neither reports cache usage in
its stream — the honest answer, not a gap. Anything else would be a number this proxy made
up about the metric it exists to move."* The module doc said the same thing in the same
confident register, and a test named
`cache_usage_is_reported_only_where_the_provider_sends_it` asserted it.

Both providers do report it. Chat completions carry
`usage.prompt_tokens_details.{cached_tokens,cache_write_tokens}`; Responses carries the
same pair under `usage.input_tokens_details`. So every request through two of the three
proxied surfaces reported no cache data on the one metric this proxy exists to move, and
`headroom_cache_hit_rate` read as *no data at all* rather than as a number, which is the
form the metric takes when it wants an operator to know it has nothing to say.

**Why nothing caught it.** The test asserting `(0, 0)` fed
`data: [DONE]\n\n` — a stream carrying no usage in the first place. It passes with the
parser and without it. That is the vacuity failure this repository keeps re-learning:
asserting *nothing happened* proves nothing unless something is also shown to make it
happen. The end-to-end cache test covered `/v1/messages` only, so the metric looked
exercised.

**Two things about the shape of the frame mattered.** OpenAI's cache numbers ride in an
extra final chunk whose `choices` array is *empty*, so the choice-first classifier reached
`Other` and dropped it — the parse had to move ahead of the `choices` lookup. And once a
client sets `include_usage`, every ordinary chunk carries an explicit `"usage": null`, so
the new branch tests for an object rather than for the key; presence alone reclassifies the
whole stream as usage frames and silently drops all of its prose.

**What is still zero, and honestly so.** Chat completions send the usage chunk only when
the client sets `stream_options.include_usage` — this proxy will not edit someone else's
request to improve its own telemetry. And `cache_write_tokens` exists only on the model
families that bill for cache writes, so on older models a fully-cached prompt yields reads
with no writes and a hit rate of 1.0. Both are recorded in the README rather than papered
over; a zero here means the provider said zero *or* said nothing, and the counter cannot
separate them.

**Also fixed:** cache usage is now read from `response.incomplete` and the other terminal
Responses events, not just `response.completed`. A turn that hit its output-token limit
still read its prefix from cache and was still billed for it, and truncation is the common
shape for a long agent turn.

**The third gap with this shape**, after the routing table (audit check 6) and the volatile
scan (check 10): a capability built for one surface out of three, with a comment asserting
the others did not need it. The replacement test is a table over the dialects, and check 11
requires one row per `Observer` variant — because a table only proves what its rows cover,
and the previous version of this file is what happens when nobody checks.

**Would change if:** a provider starts reporting a cache figure this proxy cannot map onto
reads and writes. The counter pair is the shared vocabulary; a third kind of number needs
its own metric rather than a reinterpretation of these two.

---

## D31 — the signals module has one caller, and now says so

**Decision:** correct `signals/`'s documentation to describe its actual reach, keep the
seven unreached exports, and check the list of them mechanically.

The module doc read: *"Compressors that work line-by-line — logs, diffs, plain text — all
need the same judgment... This module is that judgment, factored out so the answer is
consistent across compressors rather than re-invented, slightly differently, in each."*
Only `text_crusher` has ever imported it. Two functions carried the same shape of claim at
a smaller scale — `breaks_markup` as *"the check a compressor makes before committing a
plan"* and `is_removable` as *"the question a compressor actually asks"* — and no
compressor makes or asks either.

**Nothing was rewired, because the measurement said not to.** A shared-helper module with
one caller is usually a gap. Here the other compressors turned out not to need this
judgment, which is a different fact from not having it:

| compressor | what it does instead | measured |
| --- | --- | --- |
| logs | pattern digest; rare lines print verbatim with a count | 3 planted failures — a connection error, a panic, a Python traceback — all survived a 400-line log intact |
| diffs | keeps every `@@` header, marks each elision `... N unchanged lines ...` | 2 hunk headers in, 2 out |
| search | groups by file rather than ranking lines |  |

So `is_error_line` is unreached and log errors survive anyway; `is_removable` is unreached
and diffs keep their anchors anyway. Wiring them in would have been motion, not a fix.

**One measured case is real and still not wired.** The diff compressor drops the final line
of its input when that line is trailing context, so a document whose closing delimiter
lands there ships unbalanced: 8 opening tags in and 8 out, against 8 closing tags in and 7
out. `breaks_markup` is exactly that guard. It stays unwired because a diff announces its
own elisions and a reader is not misled the way they would be in prose — but that is a
judgment about diffs, not evidence the guard is unnecessary, and the number is recorded so
the next person does not have to re-derive it.

**The exports stay.** Deleting seven public functions is an API change, and this loop
merges pure additions unattended and nothing else. Leaving them undocumented is how the
false claim survived in the first place, so the doc now lists them and audit **check 12**
compares that list against reality in both directions: wire one up without updating the
doc and it fires; add an export and forget it and it fires. Verified by mutation both ways.

Types are out of scope for the check. `ScoredLine` and `Importance` appear in the
signatures of functions that *are* called, so a caller uses them without naming them, and
demanding a textual reference would cry wolf — the failure this script has now recorded
three times.

**Would change if:** a second line-dropping compressor appears. That is the moment the
original claim becomes worth making true rather than worth correcting, and `breaks_markup`
gets its caller.

---

## D32 — `passthrough` counts what was not compressed, not what was not touched

**Decision:** correct `headroom_passthrough_total`'s help text from *"Requests forwarded
unchanged"* to *"Requests where no compression applied"*, and pin the difference with a
test over all three surfaces.

It is not "forwarded unchanged" on two of them. `shape_openai` runs after compression
declines and adds `prompt_cache_key` and `reasoning_effort` (gap row O2), so a request
nothing compressed still leaves larger than it arrived. Measured, with the smallest input
that reaches the route at all:

| route | in | out | byte-identical |
| --- | --- | --- | --- |
| `/v1/messages` | 69 | 69 | yes |
| `/v1/chat/completions` | 62 | 88 | no |
| `/v1/responses` | 59 | 85 | no |

**Why the wording mattered.** The counter is described two sections below invariant I1 —
*"Byte-faithful passthrough — unmutated bytes arrive SHA-256 identical"* — and read as the
same guarantee. I1 still holds: every byte the client sent survives, `insert_top_level_member`
adds members without a `Value` round-trip precisely so it does. But "unchanged" is a
stronger claim than the counter can support, and an operator reconciling proxy egress
against client egress would have found it false.

**The enrichment is not the bug and was not changed.** `prompt_cache_key` improves the
metric this proxy exists to move, and `reasoning_effort` is only ever *added* — a
customer-supplied value is a deliberate choice about answer quality and is left alone.
`route_effort` returning `High` for a two-token opening turn looks wrong and is not: an
opening turn is a new problem by definition, and the documented asymmetry is that routing
too low costs the whole exchange twice over.

**The third claim in a row that held for `/v1/messages` and failed for both OpenAI
routes**, after the volatile scan (D-era #124) and SSE cache accounting (D30). The test
walks all three surfaces rather than asserting the Anthropic case and generalizing, and it
pins *both* outcomes — a version asserting only "chat is not identical" would pass if the
proxy started rewriting `/v1/messages` too, which is the failure that would actually
matter. It also names the key added, so a *different* mutation on those routes fails
rather than passing as "not identical, as expected".

Verified by mutation in three directions: disabling the enrichment, renaming its key, and
leaking it onto `/v1/messages` each turn the test red. The third needed a probe at the
insertion point first — the obvious version of it did not compile, and a non-compiling
mutation reads exactly like a passing one if only the test result is checked.

**Would change if:** the enrichment moves ahead of the compression decision, at which point
an enriched request is no longer a passthrough in any sense and the counter needs splitting
rather than relabelling.

---

## D33 — `/health` reports the CCR store that was built, not the one configured

**Decision:** add `ccr_store` and `ccr_store_persistent` to `/health`, sourced from the
store `AppState` actually constructed. This changes `Health::current`'s signature — see
"the public-signature call" below.

`Config::ccr_store` falls back to memory on any failure and logs a warning. That trade is
right and is not what changed: CCR is a recovery path, and refusing to start costs the
customer their whole service rather than one retrievable block. What was missing is the
*report*.

Measured, with `HEADROOM_CCR_DIR` pointed at a path that cannot be opened:

| configuration | value survives a store rebuild |
| --- | --- |
| unopenable directory | **no** |
| usable directory | yes |
| unset | no |

`/health` in the first case answered
`{"status":"ok","version":"0.1.0","upstream":"http://x","compression_enabled":true,"relay_available":true}`
— no mention of a store at all. So the proxy relays, compresses, and hands the model
`<<ccr:HASH>>` markers that stop resolving the moment the process restarts, while every
self-report says it is healthy. README.md's promise for CCR is that compression is "a bet
that can be unwound"; in that state it quietly is not.

**Two fields, not one.** `"memory"` is the correct answer for a default install and a
silent failure for a configured one, and a single field cannot separate them.
`ccr_store_persistent` is the one to alert on, and `Config::persistent_store_requested`
records the *request* so the mismatch is expressible at all.

**The kind travels as a value, not as a second read of the configuration.** That is the
lesson `Health::upstream` already carries in its own doc — a downstream reader of the
config reports what was asked for, which is precisely the case where the operator needs to
be told it did not happen. Re-deriving it in `health.rs` would have reproduced the exact
bug being fixed.

**The public-signature call.** `Health::current` gained a third parameter, which the parity
loop's own rules say to stop and ask about. Logged rather than asked, under the standing
instruction to keep going and record decisions. The blast radius is small and was checked
first: six call sites, all in this repository, five of them tests. The alternatives were
worse — a second constructor leaves `current` reporting `"unknown"` forever, and a
process-global set at construction hides the dependency the parameter makes obvious.

**Would change if:** a caller outside this repository turns up using `Health::current`. It
would want the old signature back and a builder instead.

---

## D34 — runtime overrides merge, because the check that guards them already did

**Decision:** `set_overrides` merges over the overrides already in force instead of
replacing them. Sending a setting as an empty value takes it back; `clear_overrides` drops
everything; `{}` is now a no-op rather than an undocumented wipe.

Two functions modelled two different configurations. `preview_overrides` merges, and its
doc says why — *"a call that sets only the upstream still has to be judged against whatever
listen address is currently overridden."* `set_overrides` replaced. So the self-reference
check that runs before every apply was validating a configuration that would never exist.

The operator-visible half is worse, and is the scenario `admin.rs` opens by naming:
*"Turning compression off during an incident should not cost"* a restart. Measured against
a running proxy, forwarding to a loopback capture upstream:

| step | response | forwarded |
| --- | --- | --- |
| `{"HEADROOM_COMPRESSION":"0"}` | `applied: [HEADROOM_COMPRESSION]` | 19500 bytes, compression off |
| `{"HEADROOM_STABILIZE":"1"}` | `applied: [HEADROOM_STABILIZE]` | 1895 bytes, **compression back on** |

`/health` then reported `compression_enabled: true` — accurate, and nobody asked for it.
Nothing in the second response mentions that the first setting was dropped. An operator
retuning a proxy mid-incident is exactly who this hurts, and exactly who cannot afford it.

After the change, the same three steps hold both settings at once: 19500 off, 19606 off
with `cache_control` present, 1895 on with `cache_control` still present.

**Why merge rather than report what was cleared.** Reporting would leave the preview
unsound — it would still be judging a merged configuration against a replacing apply. Merge
fixes both halves with one change, and it is the semantics the surviving function already
documents.

**Nothing depended on replace.** One non-test caller, and the tests reset with
`clear_overrides` rather than with an empty POST. The wipe-by-`{}` behaviour was never
documented or tested; it was a consequence of the implementation.

**Found by checking a different claim.** `STARTUP_ONLY` is a second copy of a decision and
has been wrong once already — the README records `HEADROOM_UPSTREAM` being listed as live
when it was not. So all three settings *not* on the list were driven end to end against a
running proxy, in both directions. All three are genuinely live; that check is clean, and
this turned up on the way past.

**Emptying a setting works, and was checked rather than reasoned about.** This nearly
shipped a note claiming the behaviour was non-uniform — that `HEADROOM_UPSTREAM` filters
empty and falls back while `HEADROOM_COMPRESSION` reads `""` as *enabled*. It does, and
enabled is also its default, so there is no divergence. Measured for all six settings that
reach somewhere observable: an empty value is indistinguishable from unset in every case.

That holds by six parsers coinciding, not by a shared rule — one filters empty, one fails
to parse it, one does not match it against the on-spellings — so `tests/runtime_overrides.rs`
pins it. Its `HEADROOM_CCR_DIR` case needs a different witness than the others: an
unopenable directory falls back to memory, which is also the default, so the override map
is the only evidence anything was set at all.

**Would change if:** the endpoint grows an explicit way to express "replace everything", at
which point `{}` could mean that again — deliberately, and with a test.

---

## D35 — the CCR round trip is verified across processes, and the two names that make it work are pinned

**Decision:** add audit check 13, comparing `headroom-mcp`'s CCR environment-variable
literals against `headroom-proxy`'s constants.

CCR's promise, in README.md, is that compression is *"a bet that can be unwound"*. The
write half runs in the proxy and the read half in the MCP server — a different process,
often started separately, sometimes after the proxy has restarted. CONTRIBUTING's second
lesson is precisely that a self-consistency test proves nothing across that boundary.

**Verified the hard way, on release binaries.** The proxy, configured with a
`HEADROOM_CCR_DIR`, compressed a 21-message request and emitted
`<<ccr:3e6aa03878452e4b854f0feeb6a7c835>>`. The proxy was **stopped**. The MCP binary was
then started against the same directory and asked for that hash over stdio; it returned the
original tool result. The bet unwinds, across a process boundary and across a shutdown.

**What is not guaranteed by construction.** `headroom-mcp` cannot depend on
`headroom-proxy`, so it carries its own `"HEADROOM_CCR_DIR"` and `"HEADROOM_REDIS_URL"`
literals. That is a second copy of a decision — check 6's subject, in a place check 6
cannot see. The drift would be silent and total: rename the proxy's constant and the MCP
binary reads a variable nobody sets, falls back to memory, and answers "not found" for
every marker the proxy ever wrote, while both processes start cleanly and report nothing
wrong. The model is simply told the content is gone.

Check 13 keeps the two names together. Verified by mutation: renaming `CCR_DIR` to
`HEADROOM_CCR_DIRECTORY` fails it with *"every marker would be unredeemable"*, exit 1.

**Not turned into a cargo test.** A real two-binary test cannot use `CARGO_BIN_EXE_` across
crates, and simulating it inside one process is the self-consistency trap the exercise
exists to avoid. The property is instead held by three things that are each checkable:
`ContentHash`'s format pinned to a literal in `ccr::hash`, the store layout living in
`headroom-core` rather than being reimplemented per binary, and check 13 on the variable
names.

**Would change if:** a third binary grows a CCR store. It would need adding to check 13
rather than being trusted to copy the strings correctly.

---

## D36 — the Python binding honours the block-kind gate, and lets the caller set it

**Decision:** `compress()` gains a `kind` argument, defaulting to `"tool_output"`, used for
both the block it builds and the routing question it asks.

The module built a `BlockKind::Text` block and then routed with `transform_for`, which
ignores block kind. Measured on 300 lines of prose:

| call | result |
| --- | --- |
| `transform_for` (what this used) | `Some("text_summarizer")` |
| `transform_for_block` with a `Text` block (what the proxy uses) | `None` |
| `transform_for_block` with a `ToolResult` block | `Some("text_summarizer")` |

So the binding lossily summarized prose the proxy refuses to touch, while telling
`validated_apply` the block was text — internally inconsistent, and inconsistent with the
proxy for the same input.

**Why this is worse in a library than in the proxy.** The gate exists because
`BlockKind::Text` is *what somebody typed*. In the proxy, lossy prose compression is at
least recoverable: the store persists and the model can redeem the marker. Here the store
is deliberately per-call and discarded — this module's own docs say the marker "refers to
content nothing can return". So the binding was doing irreversibly what the proxy declines
to do reversibly.

**Why the default is `"tool_output"` rather than the safe value.** A library caller is
almost always holding a scraped page, a command's output, a file dump. Defaulting to
`"text"` would silently stop compressing for every existing caller — turning a correctness
fix into a capability removal — and only the prose summarizer is gated, so the default
changes nothing for any other content type. `"text"` is now reachable for callers passing a
person's words, and declines with `reason == "tool_output_only"`.

**A second binding-local reason, and why it is needed.** `route` answers a question about
*content*; the gate is about where the content came from, which only the caller knows. So
routing says `compress` for prose the gate then declines, and reporting that unchanged
would tell a caller their text was compressed when nothing touched it. `TEXT_ONLY_COMPRESSOR`
joins `NOT_SMALLER` in the exported `REASONS` list, so a caller branching on it finds it
documented.

**Verified against a built wheel**, not against the Rust surface: `maturin build --release`,
install, `pytest`. 15 pass. Reverting to `transform_for` fails
`test_typed_text_is_not_summarised_but_tool_output_is` on `assert not as_text.compressed`.
The test carries its control — compressing the same prose as tool output must succeed
first, or declining proves nothing.

**Would change if:** a second compressor becomes tool-output-only. The reason string would
need to name which one, rather than reporting the category.

---

## D37 — the CLI and the MCP server honour the block-kind gate too

**Decision:** `headroom compress` gains `--kind`, the `headroom_compress` tool gains a
`kind` property, both defaulting to tool output and both routing through
`transform_for_block`. The same fix as D36, in the two surfaces D36 did not cover.

D36 fixed the Python binding. The identical construction was in two more places, found by
asking the question the fix raised rather than by anything failing:

| surface | construction | store outlives the call |
| --- | --- | --- |
| Python binding | `Block::new(Text, …)` + `transform_for` | no |
| `headroom compress` | `Block::new(Text, …)` + `transform_for` | no |
| `headroom_compress` (MCP) | `Block::new(Text, …)` + `transform_for` | yes, when configured |

Each declared the content was typed text and then asked a routing question that ignores
kind, so the lossy prose summarizer ran on it. On the two whose stores die with the call,
irreversibly.

**Both had the answer already in the same file.** `headroom inspect` uses
`transform_for_block` and prints the verdict *both ways* — "as tool output" and "as a typed
message" — so one binary gave two answers about the same bytes depending on which
subcommand you ran. The MCP server's own doc says a model asking `headroom_compress` "should
get the number the proxy would"; this is a place it did not.

**The MCP's schema is checked against its handler.** A model can only pass what the schema
offers, so an enum that drifts from the match arms is a capability nothing can reach — this
repository's oldest failure, in the one place a model is actually reading. The test walks
the advertised enum and calls the tool with each value; adding `"prose"` to the schema fails
it with *"the schema advertises \"prose\" and the handler refuses it"*.

**An unknown kind is refused, not guessed.** Treating `"Text"` as tool output would
summarize precisely the thing the caller was asking to protect. clap does this for the CLI
for free, with the valid values in the error.

**No audit check for this one, deliberately.** The mechanical form — "a file that calls
`transform_for` and `validated_apply`" — flags `headroom doctor`, which compresses one
sample per compressor to check the build works and is right to ignore block kind. An audit
that cries wolf is worse than no audit, and this script has recorded that lesson three
times. CONTRIBUTING's fourth lesson carries the instance instead.

**Would change if:** a second compressor becomes tool-output-only, or a third block kind
appears. Either turns the boolean gate into a table, which is worth checking mechanically
in a way this is not.

---

## D38 — the file store heals what an interrupted write leaves, rather than skipping it

**Decision:** `purge_expired` re-stamps a body whose expiry it cannot read, and collects
abandoned `.tmp` files and orphaned `.exp` sidecars. `get` still fails open, unchanged.

Three states were uncollectable, and each is what an unclean shutdown leaves. Measured over
three purge rounds, nothing removed and nothing counted:

| state | why it leaked | size |
| --- | --- | --- |
| body with no sidecar | `expiry_of` returns `None` and the loop skipped it | full content |
| abandoned `.tmp` | the loop skipped `.tmp` names | full content |
| `.exp` with no body | the loop skipped `.exp` names | tiny |

**The first is not exotic.** `put` renames the body into place and *then* writes the
sidecar, so a crash, power loss, or ENOSPC between those two lines produces it. For a proxy
configured with `HEADROOM_CCR_DIR`, an unclean restart is the ordinary case, and the
directory then grows without bound while `purge_expired` reports `0` forever.

**What could not change.** `get` fails open when it cannot read an expiry — an unknown
expiry must not discard content a model was promised it could retrieve. Verified across a
deleted, corrupt, and empty sidecar: all three still return the content. So the fix had to
keep the entry and make it *datable*, not decide it had expired. Re-stamping does exactly
that, and the recovery TTL is deliberately long: the original expiry is unknowable by then,
so a re-stamp must never shorten the life of something that would have lived longer.

**Judged by modification time, not by presence.** A live `put` renames its temporary file
within milliseconds, so an hour is far beyond any in-flight write. Deleting `.tmp` files on
sight would race a concurrent writer, which is why the test asserts a *fresh* `.tmp`
survives — without that half it would pass on a build that deleted every one it saw.

**The count still means expired entries.** Folding recovery work into the return value
would make a store healing itself look like a store expiring content, and an existing test
asserts that number.

Verified by mutation three ways — restoring the skip, deleting `.tmp` regardless of age, and
dropping orphan collection — each turning its own test red.

**Would change if:** the sidecar moves into the body file as a header. That removes the
window entirely rather than recovering from it, at the cost of rewriting the whole entry to
change an expiry.

---

## D39 — two invariant claims corrected: what the type prevents, and what observation means

**Decision:** correct `Message::blocks_mut` and `Conversation::messages_mut`'s docs, and add
an I9 test that varies the thing I9 is about.

**The type does not express I6.** `blocks_mut` claimed a slice meant blocks "can be modified
in place but not appended, removed, or reordered", and called that "invariant I6 expressed
as a return type: a compressor holding a `&mut Vec<Block>` could `push`, `remove`, or
`swap`, and none of those are things compression is allowed to do."

`push` and `remove` are `Vec` methods and genuinely out of reach. `swap` is a **slice**
method. So is `reverse`, `rotate_left`, `sort_by`. Measured — three of them reorder a
conversation through nothing but the public accessor:

```
before: ["first", "second", "third"]
after:  ["second", "third", "first"]
```

The type carries the no-add-no-remove half of I6 and not the ordering half. The sentence
named the one operation it did not prevent.

**The property still holds**, which is why this is a doc fix and not a behaviour fix: the
compression path addresses edits by `(message, block)` index and writes in place, and
`i6_surviving_content_keeps_its_position` checks message count, role-by-index and
block-type-by-index end to end through the relay, with a vacuity guard. Since roles
alternate in the fixture, a swap fails it. What was wrong was the account of *why* — a
reader was told the compiler had this covered.

**I9's test did not vary observation.** `i9_telemetry_records_without_altering_the_request`
sends the same request through two proxies and SHA-compares. But `AppState::new` always
attaches metrics, so it compares observed against observed — determinism, which is I4.
`observing_a_request_does_not_change_what_is_forwarded` runs `compress_dialect` with and
without a metrics sink, which is the difference the invariant is about. Bytes identical,
13 KB down to 470.

Both of its guards are load-bearing, and both were earned. The first version used a fixture
whose newest message is typed prose, so the block-kind gate declined it and 245 bytes came
back as 245 — identical, and proving nothing. And with the "something was recorded" guard
removed, a build where metrics record nothing **passes**; verified by mutation.

**Would change if:** a `trybuild` dependency becomes worth it, at which point "this does not
compile" could be asserted rather than described.

---

## D40 — `wrap` handles the settings file before it asks about the environment

**Decision:** move `--settings` handling ahead of the `env_configurable` check in
`commands::wrap`, and reword the remaining error to name the flag.

`wrap` bailed on `!env_configurable()` before reaching the settings branch, so the branch
was unreachable for any agent without environment variables. Measured across all eight:

| agent | `--settings` |
| --- | --- |
| claude, codex, aider, cline, continue, goose, openhands | honoured |
| **cursor** | **refused** |

Cursor is the only agent with no environment variables, which makes `--settings` the only
way to point it at the proxy — and the only case that did not work. The error even named
the fix: *"set its base URL to {proxy} in its own settings instead"*, which is what the flag
does, with a backup.

**The asymmetry is the sharp part.** `unwrap` never had the check, so
`unwrap cursor --settings X` worked against a backup `wrap cursor --settings X` refused to
create. This module opens by saying *"Unwrap is the feature"* and treats byte-exact
restoration as the thing that must be right; this was the half that stopped it being
available. A cursor user following the message edits by hand, gets no backup, and is told
*"was not wrapped; nothing to restore"* when they try to undo it.

**Why the tests missed it.** `wrap.rs` covers the file functions thoroughly — double-wrap
refusal, byte-exact restore, backup removal, non-JSON refusal — and every one passes. The
defect is one layer up, in the command that never calls them for this agent. A tested unit
with an untested caller, which is this repository's oldest failure and its first audit
check.

The replacement test is a table over `Agent::ALL` rather than a case for cursor, so the next
agent with no environment variables fails here rather than in someone's hands. Both halves
are asserted — the file changed *and* the backup holds the original bytes — because a
version checking only the first would pass on a build that backed up the rewritten file,
which is precisely what the double-wrap guard exists to prevent.

**The predicate is reused, not restated.** The first version of this fix wrote
`exports.is_empty()`, which is what `env_configurable` already computes; clippy caught it as
dead code and it would have been a second copy of one decision — the duplication this
repository keeps paying for.

**Would change if:** an agent appears that needs both a settings file and environment
variables, in which case the early return after wrapping the file is wrong and both paths
have to run.

---

## D41 — usage is read from the reply's body when the reply is not a stream

**Decision:** `ObservingStream` accumulates a bounded copy of a non-streaming reply and
reads cache usage from it; framing comes from the provider's `content-type`. Streams are
still never buffered.

Cache accounting was parsed only from SSE frames. Measured against one stand-in returning
identical usage both ways — same provider, same numbers, same proxy:

| response | `cache_read_tokens_total` | `cache_creation_tokens_total` |
| --- | --- | --- |
| streaming | 900 | 100 |
| non-streaming | **0** | **0** |

So `headroom_cache_hit_rate` read as *no data* for every client that does not stream. Same
symptom as D30 and a different cause: that was the wrong dialects, this is the wrong
framing.

**Found by the measurement harness, on the proxy.** `scripts/live-cache-measurement.py`
sends non-streaming requests; the provider reported 4,700 cache reads across a run and the
proxy reported none. The tool written to measure whether the proxy helps caught the proxy
failing to notice that it had.

**Buffering, which this module says never to do.** The rule is about *streams* — holding a
generation back turns a readable answer into a stall. A non-streaming reply is a single
object the client buffers anyway, so a bounded copy costs nothing that was not already
being paid. The cap is a backstop against a body that is not what it claimed to be: past it
the bytes still pass through untouched and only the telemetry is given up.

**Framing from `content-type`, not from the request's `stream` flag.** What matters is how
the reply came back, and the provider is the one that decided. A client asking for a stream
and getting an error object is exactly the case where the two disagree.

**One reader per dialect, not one per framing.** Each dialect's `cache_from_usage` is now
shared by its SSE classifier and its body reader. Two readers spelling `cache_read_input_tokens`
separately is how one of them ends up reading a field the provider stopped sending — the
duplication this repository keeps paying for.

**The fixtures were not modelling a provider.** `fake_provider` returns a bare `&str`, which
axum serves as `text/plain`; **no fixture had ever set `text/event-stream`**. Every SSE test
was exercising a provider that does not exist, and it went unnoticed until the relay started
reading the header — at which point two existing streaming tests failed. They were right to
fail. A fixture that models the transport wrongly asserts something about a system nobody
runs.

**Would change if:** a provider streams without declaring it. Detection would have to fall
back to sniffing the first bytes, which is worth doing only when a real provider does it —
guessing now would mean buffering streams on a mistake.

---

## D42 — the relay forwards the query string, and `claude -p` is not a measurement

**Decision:** every relaying handler forwards the incoming path *and query*, not its route
literal. Also records why running the live measurement through `claude -p` would not have
measured anything.

**The credential decides the answer before the measurement starts.** Claude Code
authenticates with `Authorization: Bearer sk-ant-oat…`, which `classify_auth_mode` reads as
`AuthMode::OAuth` — captured locally, classified in the capture server so the token itself
was never recorded. Under I10, OAuth gets lossless transforms only: no lossy compression at
all, deliberately, because a modification could exceed the granted scope. So routing Claude
Code through this proxy does almost nothing *by design*, and a two-arm comparison would
have shown ≈no difference and confirmed a policy readable in the source. At roughly $0.22 a
call — Claude Code sends 39 tools and a 151 KB body every turn — that is a expensive way to
re-read `auth_mode.rs`.

**What the attempt did find.** Pointing Claude Code at a capture server showed
`/v1/messages?beta=true` going out and `/v1/messages` arriving. Every handler passed its
route literal to `relay`, and the one that had a `Uri` called `.path()` on it, so the query
was dropped on all four relaying routes. Nothing failed; the request simply stopped being
the one the client sent. That is the shape of defect I1 exists to prevent on the body, and
nothing was checking it on the target.

Whether `?beta=true` changes Anthropic's behaviour is not the point — the proxy does not
get to decide that. Its job is to relay what the client sent.

**The fixture had the same bug.** `fake_any_path` recorded `uri.path()`, so the double
built to prove the proxy relays to the right target could not see half of the target. The
new test failed against the fixture before it failed against the code, which is the second
time this session that a test double turned out not to model the transport
(`fake_provider` never set `text/event-stream`, D41).

**Would change if:** a provider requires a query parameter the proxy must add or strip. It
would belong in one place with a reason, rather than arriving as the accident of a literal.

## D40 — the savings ledger buckets by hour rather than logging per request

**Decision:** persist savings as hourly buckets in a JSON file, aggregated on write,
pruned at 90 days, buffered in memory and flushed on a timer.

**The problem.** `headroom savings` read a `/metrics` scrape from stdin. That reports a
*rate* for the current process and nothing else: no total, and everything resets on
deploy. The headline claim of the project — here is what this saved you — could not be
answered over any period longer than one process lifetime.

**Why not an append-only log.** The obvious ledger writes one record per compression.
On a busy proxy that is an unbounded file growing at request rate, which is a
disk-exhaustion bug wearing a feature's clothes. Worse, the thing it takes down is a proxy
somebody is already debugging.

So entries aggregate on write into buckets keyed by `(hour, model_family, content_type)`.
Growth is bounded by retention times distinct keys rather than by traffic: a year of one
key is 8,760 rows, and a month of a busy proxy is a few hundred kilobytes.

**What that costs.** Per-request detail is gone. That is the right trade for the question
this answers — "what did we save last month" — and per-block detail is what #188
(`headroom audit`) is for. Recording both here would be building #188 badly inside #187.

**Why a JSON file and not SQLite.** The same reasoning as D6, which substituted
`FileCcrStore` for the reference's SQLite backend. The access pattern is: update a
handful of counters in memory per request, write the whole thing on a timer, read all of
it when somebody asks. A dependency-free file covers that. Adding a database to a proxy
whose first requirement is starting reliably is a cost with no matching benefit at this
size, and D6 already established that this project pays for storage engines only where
the access pattern needs one.

**Writes are buffered.** Persisting synchronously would put a file write, an fsync and a
rename between the model and the user on every request — latency charged to live traffic
to improve a report nobody is reading at that moment. Records accumulate under a
short-lived lock and a background task flushes every 60 seconds, the same shape as the CCR
purge task beside it. A proxy killed between flushes loses at most one interval of
*reporting*. No request is affected, and no compressed byte depends on any of it.

**The clock.** This module reads the wall clock, which I4 forbids on the compression path.
I4 is about the bytes sent upstream; the ledger is written *after* a decision and never
consulted for one, so nothing a request sends depends on it. Buckets are keyed by
zero-padded hour so lexical order is chronological — unpadded, hour 9 sorts after hour 10
and every range query silently reads the wrong set once the hour count changes digit width.

**No currency figure.** L12 decided that and this does not reverse it. A token count is a
fact; a dollar amount is a guess about somebody's pricing tier, and a guess printed beside
facts reads as one. A test asserts the report mentions no currency.

**Opt-in.** Without `HEADROOM_SAVINGS` the proxy behaves exactly as it did and
`headroom savings` still reads a scrape from stdin. `--since-days` refuses in that mode
rather than answering from data that cannot support it.

## D43 — memories are selected against the request, and remind_me feeds the file rather than the request

**Closes #193.** The original issue asked for persistence, eviction, similarity retrieval,
and extraction — and stopped, because similarity retrieval meant a vector index and that
is a dependency decision. Evaluating [`remind_me`](https://github.com/baileyrd/remind_me)
as the backend split the question in two, and the two halves have different answers.

**As the memory source: adopted, and it needed almost nothing.** `remind_me`'s exporter
emits one JSON object per memory carrying every column of its `memories` table, and that
table's `content` is `TEXT NOT NULL` — which is exactly and only what `from_jsonl_lossy`
requires. Its entity-graph records carry no `content` and are skipped by the rule that was
already there. The one real gap was provenance: `remind_me` has no `agent` or `context`
column, so every fact arrived as `unknown`/`unknown` — the single provenance value that
tells a reader nothing, on the import path most likely to be used. `source` and `category`
are now read as fallbacks. Native names still win when both are present, so a file written
for this reader is unaffected.

That also settles persistence without building any. The store is still loaded once at
startup from a file; what writes that file is now somebody else's problem, and there is a
good answer for it.

**As a request-time backend: not here, and the successor is why the question is live.**
The Python server would have meant a network call on the compression path — the objection
D16 raised against fetching a tokenizer, and the end of the argument. Its successor,
[`rusty_remind_me`](https://github.com/baileyrd/rusty_remind_me), removes that objection
entirely: `remind_me_core` is a Rust library, and `db::queries::search_memories` is a
*synchronous* function over a `rusqlite::Connection`. It would link in, not dial out.

Three things still argue against calling it per request, and none of them is "it is
Python":

- **The vitality filter reads the wall clock.** `register_sql_functions` registers an
  `effective_vitality` scalar that calls `Utc::now()`, and `search_memories` puts it in the
  `WHERE` clause unless `include_dormant` is set. The same query returns different rows as
  the afternoon wears on. That is avoidable — `include_dormant: true` with `min_vitality:
  0.0` omits both predicates — but it is avoidable by *discipline*, and I4 is currently
  enforced by there being nothing to get wrong.
- **The database is live.** A file another process writes is a different input on every
  request by construction. `HEADROOM_MEMORY` is `STARTUP_ONLY` precisely so the memory set
  is pinned for the process's lifetime; querying a mutable store un-pins it.
- **The semantic tier dials out anyway.** `available_embedder` is Ollama over HTTP, off
  unless `REMIND_ME_EMBEDDING_BACKEND` is set. Off, the search is FTS5 BM25 — which is what
  we already do. On, it is a network call on the compression path.

So the retrieval quality that would justify the dependency is either what we have, or the
thing we cannot have. Filed separately rather than decided here; this is the same shape as
refusing `HEADROOM_COMPRESSION_DEADLINE_MS`, where the reference can afford request-time
nondeterminism because it does not promise I4.

**Two record kinds are refused outright, and the successor's schema is what surfaced
them.** `sensitive` is a flag the owner sets to mean *do not surface by default*;
`remind_me`'s own search honors it and its exporter does not filter on it, so a routine
export carries those rows. Injection would send them to a third-party model with nobody
shown them first — a worse disclosure than the search this flag already guards. A non-null
`superseded_by` marks a fact its own source knows to be stale; the exporter excludes those
by default and includes them for a full backup, which is exactly the file somebody points
at this reader. Injecting one would hand the model a stale fact with the corroboration
marker vouching for it. Both are skipped at load, logged at debug rather than warn so a
large export cannot train an operator to ignore the warnings that mean something.

**So selection had to be ours, and BM25 is not a consolation prize.** `recall` ranked by
corroboration and took no query at all, which meant importing `remind_me`'s corpus would
have discarded the half that makes it good. `recall_for_query` scores with the same
`Bm25Scorer` the record-set compressors use (#187) — deterministic, in-process, no new
dependency, and unaffected by the ONNX exclusion that blocked the embedding tier. The
reference itself runs a text index alongside its vector index, so keyword scoring is a tier
it has too, not a substitute for the one it has.

**Zero-scoring facts rank last rather than dropping out.** `limit` is a budget. Enforcing a
keyword match would hand back *less* context than the same store returns today for the same
request — a relevance feature that makes the output worse whenever the query is narrow. For
the same reason, a query with no signal (absent, empty, or sharing no term with any fact)
falls back to the corroboration ranking rather than returning nothing.

**The query is returned from `read_conversation` rather than derived again.** It is a
property of the whole conversation and was already computed there for block scoring. A
second walk over the same messages at the injection site would be free to drift from the
first, and nothing would fail when it did. The wiring is asserted by a test through
`compress_dialect` — the proxy's own path — with decoy memories that outrank the relevant
one on corroboration, so an unwired query cannot produce the expected answer by luck. That
test was confirmed to fail with the argument removed; #71's failure class is capabilities
that pass every unit test they have and are called by nothing.

**Not done here, deliberately.** A reload path (it needs its own I4 argument and does not
come free with this), extraction from traffic, and the vector tier.

## D44 — the linked memory backend, and why D43's three objections do not survive an in-process embedder

**Closes #215.** D43 evaluated `rusty_remind_me`'s `remind_me_core` as a request-time
backend and filed the question separately rather than deciding it, on three objections: the
vitality filter reads the wall clock, the database is live and un-pins the memory set
`HEADROOM_MEMORY`'s `STARTUP_ONLY` contract exists to pin, and the semantic tier that would
justify the dependency dials out over HTTP to an optionally-present daemon. #215's own
comment thread corrected the third: `remind_me_core`'s Ollama backend is loopback-only, not
a cloud dependency — but loopback does not fix the objection, because it is availability,
not locality, that breaks I4. `available_embedder` probes the daemon and caches the
verdict; a failed probe silently degrades the ranking to keyword-only. The same request
compresses differently depending on whether a ping answered, on loopback exactly as over
the network.

**The shape that survives is option 4 from that thread: a linked query, with an in-process,
startup-pinned embedder standing in for the daemon-probed one.** Two design moves close all
three of D43's objections at once, and both were already available elsewhere in this
codebase before this issue:

**The vitality clock: closed by argument, not discipline.** D43 flagged that
`include_dormant: true, min_vitality: 0.0` avoids the wall-clock read only if the caller
remembers to pass it — "I4 enforced by there being nothing to get wrong" was the standard
being given up. `linked_memory::LinkedMemory::search` (`crates/headroom-proxy/src/linked_memory.rs`)
hardcodes both on every call; there is exactly one call site and it cannot be reached with
different values. `dormant_memories_still_surface` proves the predicate is actually never
emitted, not merely that the code compiles as if it were: it manufactures a memory decayed
far below `VITALITY_FLOOR`, confirms `remind_me_core`'s own default search excludes it (so
the memory is genuinely dormant, not just old), then confirms `LinkedMemory::search` returns
it anyway. Reverting the two literals to their defaults was confirmed to make this test fail
— #71's failure class is a change that passes every test written before it existed.

**The live database: pinned exactly like `HEADROOM_MEMORY` already is.** The connection is
opened once, read-only, in `LinkedMemory::resolve` — called once from `AppState::new`
(`crates/headroom-proxy/src/server.rs`) and stored in `Compressors`, the same place and the
same lifetime `Config::memories()` already occupies for the JSONL path. `HEADROOM_LINKED_MEMORY_DB`
is `STARTUP_ONLY`, alongside the two paths for the embedder. A write to the underlying file
after the process starts is invisible to it for the rest of the process's life — the same
staleness `HEADROOM_MEMORY` already accepts, not a new one.

**The semantic tier: `headroom-embed`, not `remind_me_core::embedder`.** `crates/headroom-embed`
(added by the PR that carried #215's exploratory work, before this decision) is `rten` +
`tokenizers` behind an off-by-default `local` feature — a model file and a tokenizer file,
both required, both loaded once by `LocalEmbedder::load`, which fails closed: an error
result at startup rather than a degraded embedder that could later become undegraded. It
deliberately does not depend on `remind_me_core` (SQLite and tokio, which an inference crate
needs neither of), so `linked_memory::enabled::EmbedderAdapter` — the few lines mapping
`headroom_embed::{EmbedRole, Identity}` onto `remind_me_core::embedder::{EmbedRole,
EmbeddingIdentity}` — lives in `headroom-proxy`, the crate that already carries the
`remind_me_core` dependency. If the model will not load, `LinkedMemory::resolve` logs and
proceeds with `embedder: None`: the keyword tier over the linked database still answers, for
the life of the process, never re-attempted per request.

**A fourth hazard, not named in #215 or D43, surfaced during implementation and is closed
the same way.** The vectors already stored under `HEADROOM_LINKED_MEMORY_DB` may have been
written by a different embedder than the one just loaded — `remind_me_core`'s own Ollama
backend, most likely, since that is the only backend its own tooling can index with today.
Comparing a query vector from one embedding space against corpus vectors from another is not
degraded retrieval; per `EmbeddingIdentity`'s own doc, it is "a number with no meaning."
`remind_me_core::vectors::embedding_mismatch_info` — a read-only check the reference calls
`_reconcile_embedding_meta`, already present for exactly this versioning problem — is
consulted once at startup. A mismatch disables the semantic tier for the process the same
way a load failure does; correcting it would mean writing (clearing the stale vectors), and
the connection is deliberately read-only. `a_mismatched_embedder_identity_falls_back_to_keyword_only`
covers both branches directly against `accept_if_compatible`, the function `resolve_embedder`
delegates the policy to.

**Ranking order is preserved, not re-derived.** `headroom_core::memory::MemoryStore` exists
to rank by corroboration and, since D43, by BM25 against the query — neither of which
applies to a search result that already carries a fused BM25-plus-semantic rank from
`remind_me_core`'s own `rank_rrf`. Routing linked results back through `recall_for_query`
would have re-ranked every candidate to a content-hash tiebreak (each result has exactly one
source and one occurrence, so every tier above the tiebreak ties) — silently discarding the
retrieval quality this decision exists to gain. `headroom_core::memory::inject_block_ranked`
/ `inject_append_ranked` render an already-ordered list directly, sharing the live-zone
splice logic (`splice_into_tail`) with the unranked path rather than duplicating it. The
static `HEADROOM_MEMORY` path is untouched: it still renders through `inject_block_for_query`,
corroboration marker included, since that is a property the static store can actually
observe and a one-shot search result cannot honestly report. `Compressors::inject_memory_append`
/ `inject_memory_block` (`crates/headroom-proxy/src/compression.rs`) select between the two
render paths on whether a linked backend is configured, and
`a_configured_linked_backend_is_used_instead_of_the_static_store` proves the call site
actually reaches the linked path when both a static store and a linked backend are
configured — confirmed to fail (silently serving the static store) with the branch removed.

**Feature-gated, off by default, cost measured rather than estimated.** `linked-memory` on
`headroom-proxy` adds `dep:remind_me_core` (`rusqlite` with `bundled` — SQLite compiled from
source — and `tokio` with `full`, neither optional in `remind_me_core`'s own manifest),
`dep:headroom-embed`, and `headroom-embed/local` (`rten` + `tokenizers`). A clean
`cargo build -p headroom-proxy --release`: 97s with default features, 171s with
`--features linked-memory` — +74s, +76%, on this repository's CI runners. `remind_me_core`
is pinned by commit (`rev = "69014cc91aaa83605c8696dbe90c588efce28ef9"`, the tip of
`baileyrd/rusty_remind_me#188`, "Let a caller supply the embedder to search_memories" — the
PR that gave this crate a seam to inject `headroom-embed`'s embedder into at all, since
`search_memories` previously reached for `remind_me_core::embedder::available_embedder()`
itself). Re-pin to `main` once that PR merges, per this repository's "merge with merge
commits" convention.

**`remind_me_core::db::queries::search_memories_with_embedder` is not exercised by CI, and is
not claimed to be.** `rten`'s forward pass ships unexercised in this environment for the same
reason `headroom-embed` itself shipped that way: there is no `.rten` model file here and no
way to fetch one. `LinkedMemory`'s own tests use a deterministic bag-of-characters double —
the same pattern `rusty_remind_me`'s `injectable_embedder_test.rs` uses for the identical
reason — to prove the wiring, not the model.

**Not done here, deliberately.** A reload path for the linked database (the same argument
D43 deferred for `HEADROOM_MEMORY`, and it does not come free with this either); merging the
static and linked sources rather than one replacing the other, since nobody configures both
meaning "use only one, some of the time"; and a CI job with a real `.rten` model, since none
is available to check in.
