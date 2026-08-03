# Contributing

## Before you start
- Match surrounding conventions when editing existing code.
- Keep diffs focused — one logical change per PR.
- For large or hard-to-reverse changes (schema/data migrations, public API changes,
  deletions, dependency/toolchain bumps), open an issue or draft PR to discuss first.

## Workflow
1. Branch off the default branch.
2. Make your change. State the *why* in commit messages or PR description for any
   non-obvious decision.
3. Add tests for non-trivial logic — happy path and at least one failure/boundary case.
   Spikes/prototypes are exempt but should say so in the PR.
   **Watch the test fail before you trust it.** Break the thing it guards and confirm it
   goes red; a test that passes with the feature removed is worse than none, because it
   reads as evidence. See the two sections below for what this has cost here.
4. Add or update docstrings on any public surface you touched.
5. Open a PR — pick the template that matches (feature / bug fix / docs / chore).

## Code style
- Explicit over implicit; type hints/annotations always.
- Flat control flow — guard clauses, early returns, avoid >3 levels of nesting.
- Short, single-purpose functions.
- Minimal dependencies — justify any new third-party one in the PR description.
- Never commit or log secrets/credentials. Validate external input at the boundary.
- Never silently swallow exceptions — handle, propagate with context, or log.

## What this codebase has been bitten by

Every one below was found by mutation testing — changing the code to break a guarantee and
checking whether anything failed. Every one had passing tests throughout.

(This heading counted the entries until the count was wrong. Prose that restates something
countable goes stale silently, which is the third entry's subject in miniature.)

### A test proves a function works, not that anything calls it

Five capabilities here were shipped, tested, documented as done, and reached by nothing:
the SSE observers, the memory module, cache stabilization, the code compressor and the
prose compressors. Between the last two, the proxy silently forwarded most of what agent
tools return — source files and prose — while `/metrics` showed a passthrough count that
made it look deliberate.

`scripts/reachability-audit.sh` runs in CI ahead of the build and checks the mechanical
part of this. It cannot check the rest. When you add a capability, name the caller.

### A self-consistency test is not coverage for anything that crosses a process boundary

`of(x) == of(x)` and `parse(format(x)) == x` and `of(a) != of(b)` all survive a format
change intact — both halves move together. So does a hasher seeded once per process:
perfectly stable within a run, different in the next one. Swapping FNV-1a for
`OnceLock<RandomState>` passed the entire workspace suite.

Anything written by one process and read by another must be **pinned to a literal**:

| format | crosses | pinned in |
| --- | --- | --- |
| `ContentHash` | proxy → MCP retrieve, and across restarts | `ccr::hash` |
| CCR marker | outlives its request; redeemed against a later binary | `ccr::hash` |
| `StructureHash` | `headroom learn` → proxy | `telemetry` |
| `AggregationKey::as_str` | the key in `recommendations.json` | `telemetry` |
| `model_family` | same file | `telemetry` |

The failures these prevent are silent. Nothing errors; a lookup simply stops matching,
and the loop it feeds quietly stops working while every dashboard looks healthy.

### Assert that the behaviour happened, before asserting anything about it

Most of this project's guarantees are of the form *"X was not changed"*. Every one of them
is satisfied by changing nothing at all, so the assertion is empty unless something would
otherwise have changed it. A sweep found this everywhere:

| test | why it passed | what it needed |
| --- | --- | --- |
| I4, I5, I10 property tests | the generator made random ASCII too short to clear any threshold, so **nothing ever compressed** | content sized past each compressor's threshold, and a count of how many cases actually changed |
| I7 | the protected system block was 29 bytes — below every threshold | a hot zone big enough that leaving it alone is a decision |
| I3, I6, both I4 gates | a passthrough is a fixed point, is deterministic, and preserves every position | the "compression actually happened" guard I2 and I9 already had |
| `headroom doctor` | had no diff or search sample, and printed `all checks passed` | a sample per compressor, plus a test that every compressor has one |

The pattern to copy is the one I2 carried from the start: assert the precondition first,
with a message saying what it is for.

```rust
assert!(
    received.body.len() < source.len(),
    "nothing was compressed, so this assertion proves nothing"
);
```

Two specific traps, both of which cost a wrong fixture here:

- **Prose is compressed on a line budget.** The same words joined with spaces are one
  line, and one line is never reduced. A 19 KB single-line sample compressed to 19 KB.
- **Every compressor has a size threshold** — 1 KB for JSON, 2 KB for code, 5 KB for
  prose, 500 bytes for the rest. Fixtures below it decline for a reason that has nothing
  to do with what the test is about.

A decline test is exempt only when it asserts the *specific* reason. `Declined::Sacrosanct`
distinguishes "refused because signed" from "refused because small"; a bare `is_err()`
does not.

## Review & merge
- Every change lands through a PR — no direct pushes to the default branch.
- CI must be green before merge.
- At least one approval required (see CODEOWNERS if present).
- Reviewers: check for scope creep, missing tests, and unexplained non-obvious decisions.
- Ask what mutation a new test would catch. If the answer is "none that could plausibly
  happen", it is documentation, not a gate — which is fine, but say so.
- Merge with a **merge commit** ("Create a merge commit" — merge and sync). Do **not**
  squash-merge or rebase-merge: full commit history is preserved deliberately.
