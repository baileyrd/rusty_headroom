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
