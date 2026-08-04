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

### A capability built for one surface gets described as if it covered all three

Three surfaces are proxied — `/v1/messages`, `/v1/chat/completions`, `/v1/responses`.
Anything that crosses them tends to get built against the Anthropic one, tested against the
Anthropic one, and then written up in prose that says "the proxy does X", with no step in
between where anyone checks the other two. Five times so far — and the fifth says the
surfaces are not only the three HTTP dialects. **Four binaries compress**: the proxy, the
CLI, the MCP server and the Python binding. A rule the proxy enforces is a rule the other
three have to be checked for, one at a time.

| capability | true for | false for | what it cost |
| --- | --- | --- | --- |
| the compressible-type list | one copy | seven others that drifted | every source file forwarded uncompressed while `detect` reported it as code |
| `volatile::scan` | `/v1/messages` | both OpenAI handlers | 0 findings on OpenAI shapes; it knew only Anthropic-shaped `system`/`tools` |
| SSE cache accounting | Anthropic | both OpenAI dialects | `cache_hit_rate` read as *no data* for every OpenAI conversation |
| `passthrough` help text | `/v1/messages` (69→69) | chat (62→88), responses (59→85) | a metric promising byte-identity it does not have |
| the block-kind gate | the proxy | the CLI, the MCP server, the Python binding | prose a person typed, lossily summarized — irreversibly on two of the three, whose stores die with the call |

Each was found by asking "which surfaces does this actually run on?" rather than by a test
failing. **The comment is the tell.** Every one carried prose asserting the gap was
deliberate — *"neither reports cache usage in its stream — the honest answer, not a gap"*
was guarding a claim that was simply false. A sentence explaining why a capability stops at
one surface is the thing to go and measure, not the thing that settles it.

Two habits, both cheap:

- **Write the test as a table over the dialects, not as one case.** A table makes the gap
  visible at the point of writing, and a row that has no assertion to make is itself an
  answer. `every_dialect_reports_the_cache_usage_its_provider_sends` and
  `passthrough_is_byte_identical_only_where_nothing_enriches` are the shape to copy — note
  that the second pins the *expected* outcome per row, including the boring ones, so
  enrichment leaking onto `/v1/messages` fails it too.
- **Where the table can go stale, make the audit force it.** Check 11 requires `Observer`
  to have exactly as many variants as the cache test has rows, so a fourth dialect cannot
  ship reporting untested zeros.

Not everything cross-cutting is broken this way, and guessing is not the point — memory
injection and output shaping were checked under the same suspicion and work on all three
(`memory_and_output_shaping_reach_every_dialect`). Measuring cost less than arguing about
it would have.

The fifth entry is the one to read twice. `transform_for` answers a question about
*content*; `transform_for_block` also applies the gate that says the prose summarizer only
runs on tool output, because `BlockKind::Text` is what somebody typed. Every non-proxy
surface built a `Text` block and then called `transform_for` — declaring the content was
typed and then asking a question that ignores it. Each one had tests, and none of them
compared its answer against the proxy's for the same bytes. `headroom compress` and
`headroom inspect` disagreed *inside one binary*.

### "The type prevents it" is a claim like any other, and it is usually half true

Two places said a signature made something impossible. Both were half right, and the half
they got wrong was the half that mattered:

| claim | true | false |
| --- | --- | --- |
| `blocks_mut` returns a slice, so blocks cannot be "appended, removed, or reordered" — "invariant I6 expressed as a return type" | `push` and `remove` are `Vec` methods, genuinely out of reach | `swap` is a **slice** method, and the sentence named it. So are `reverse`, `rotate_left`, `sort_by` |
| `Telemetry`: "every method returns `()` — observation cannot influence a decision (I9)" | the recording methods do | `cache_hit_rate` and `tokens_saved` return values, and `Compressors` holds the `Arc<Metrics>` |

Neither property was actually broken, and both are tested. What was broken is the reason
given: a reader told the compiler has this covered has no cause to keep the test alive, or
to notice the day a new code path makes the claim matter. A guarantee resting on a test
that nobody knows is load-bearing is one refactor from resting on nothing.

The first was found by writing the code the doc said would not compile — which takes
seconds and settles it outright:

```rust
c.messages_mut().swap(0, 2);   // ["first","second","third"] -> ["third","second","first"]
```

The second was duller: reading the method list. `grep 'pub fn'` was enough, and no
compiler was involved — so the habit is not only "write the code" but "check the claim
against the surface it is about", whichever is cheaper.

So: if a doc says a type forbids something, try it. If it compiles, the type does not
forbid it — say what does. Here that is `i6_surviving_content_keeps_its_position` and
`observing_a_request_does_not_change_what_is_forwarded`, both of which vary the thing their
invariant is about. The second had to be *written*: the existing I9 test sent one request
through two proxies that both had metrics attached, so it compared observed against
observed and established determinism instead.

## Review & merge
- Every change lands through a PR — no direct pushes to the default branch.
- CI must be green before merge.
- At least one approval required (see CODEOWNERS if present).
- Reviewers: check for scope creep, missing tests, and unexplained non-obvious decisions.
- Ask what mutation a new test would catch. If the answer is "none that could plausibly
  happen", it is documentation, not a gate — which is fine, but say so.
- Merge with a **merge commit** ("Create a merge commit" — merge and sync). Do **not**
  squash-merge or rebase-merge: full commit history is preserved deliberately.
