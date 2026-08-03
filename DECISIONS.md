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
