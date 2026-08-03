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

## Two things this codebase has been bitten by

Both were found by mutation testing — changing the code to break a guarantee and checking
whether anything failed. Both had passing tests throughout.

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

## Review & merge
- Every change lands through a PR — no direct pushes to the default branch.
- CI must be green before merge.
- At least one approval required (see CODEOWNERS if present).
- Reviewers: check for scope creep, missing tests, and unexplained non-obvious decisions.
- Ask what mutation a new test would catch. If the answer is "none that could plausibly
  happen", it is documentation, not a gate — which is fine, but say so.
- Merge with a **merge commit** ("Create a merge commit" — merge and sync). Do **not**
  squash-merge or rebase-merge: full commit history is preserved deliberately.
