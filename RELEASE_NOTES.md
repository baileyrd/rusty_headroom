# Release Notes

One entry per merged PR against `main`, reverse chronological. No version tags
exist yet, so PRs are the unit of change; switch to `## vX.Y.Z` headers if and when
the crate starts publishing releases.

---

## The credential reaching the provider was never checked end to end
**2026-08-03** · 871 tests pass with `x-api-key` stripped from every request

- **The hole:** nothing asserted that a client's credential arrives at the provider. The
  existing header tests use `headers_seen_by_provider`, which returns header *names* — so
  they check what the proxy **adds**, not that what matters **survives**. The one unit test
  covers `authorization` only, against the pure `sanitize`, with header rebuilding, hyper's
  framing and the relay all sitting between it and the provider.
- **Consequence if it broke:** every request 401s. A total outage, not a degradation — and
  the three auth modes take different paths through the header policy, so one can break
  while the others work.
- **Checked, and correct today.** All three credentials arrive byte-identical through the
  release binary.
- **Added:** `the_credential_reaches_the_provider_intact_in_every_auth_mode`, asserting
  byte-equality rather than presence — a truncated or re-cased credential is as useless as
  an absent one, and `redacted_authorization` lives nearby, which is a plausible way for a
  redaction to leak into the forwarded copy.
- **Verified by mutation:** dropping `x-api-key` in `sanitize` leaves **871 tests passing**
  and fails only this one.

---

## Every relaying route is checked to forward its own path
**2026-08-03** · no bug found — three of five routes were guarded, now all five

- Each OpenAI handler hands `relay` a **hardcoded** upstream path — `"/v1/chat/completions"`
  in `chat_completions`, `"/v1/responses"` in `responses`. A literal that drifted from its
  route would send the provider a path it does not serve, and the client would get the
  provider's 404 for a request the proxy accepted.
- **Checked all five against a loopback provider that echoes the path it was given, and
  all five are correct.** `/v1/responses`, `/v1/responses/compact` and `/v1/conversations/*`
  already had individual assertions; `/v1/messages` and `/v1/chat/completions` did not.
- **Added:** `every_relaying_route_forwards_its_own_path`, covering all five and any route
  added later without anyone remembering to write the assertion. It also asserts the
  relaying list is a subset of the registered routes, so it cannot name a route that does
  not exist.
- The path also picks the SSE vocabulary — `Observer::for_path` falls back to the Anthropic
  classifier, so a misspelled OpenAI path would be read with the wrong grammar and report a
  healthy stream as unfinished (D18).
- Verified by misspelling `/v1/chat/completions` in the handler:
  `/v1/chat/completions reached the provider as /v1/chat/completion`.

---

## `/admin/runtime-env` confirmed an upstream change that never happened
**2026-08-03** · three self-reports agreeing with each other, all three wrong

- **The hole:** `HEADROOM_UPSTREAM` was absent from `STARTUP_ONLY`, and it is startup-only
  in fact — `AppState::new` builds one `Upstream` with the base URL baked in, and the
  request path never re-reads configuration.
- **Measured** against two loopback providers, proxy started on A and told to use B:

  ```
  admin       : {"applied":["HEADROOM_UPSTREAM"],"needs_restart":[]}
  after       : {"served_by": "UPSTREAM-A"}
  health says : http://127.0.0.1:9102
  ```

  The admin endpoint confirmed it, `/health` confirmed it, and every request kept going to
  the old provider — on the single setting an operator is most likely to change
  mid-incident, and the one whose silent failure is hardest to spot, because traffic keeps
  flowing.
- **Changed:** `UPSTREAM` joins `STARTUP_ONLY`, so admin now answers
  `needs_restart: ["HEADROOM_UPSTREAM"]`.
- **Changed:** `/health` reports the base from the built relay (`AppState::upstream_base`)
  rather than from configuration, so it cannot confirm a change nothing applied.
- **Changed:** the README's configuration table now marks which settings need a restart.
  It listed the upstream as live.
- **Added:** check 9 of the audit, which fails if a `STARTUP_ONLY` variable is missing from
  that table or not marked. The table is a second copy of the list and it is the copy an
  operator actually reads.
- **Logged as D28**, including the fix not taken: the relay's client is reusable across
  base URLs, so the setting could have been made genuinely live. It was not, because every
  other once-read setting here is deliberately startup-only for I4 reasons, and the
  alternative needed either a signature change or a test-only path through the request
  handler.

---

## Nothing verified the proxy's routes were routed
**2026-08-03** · including the one that exists for Codex

- **The hole:** `router_with` registers ten paths and nothing asked the router for two of
  them. `/v1/realtime` had **zero** references outside its own declaration — and the
  comment beside it says it exists because Codex speaks WebSocket and a proxy that only
  speaks HTTP breaks that client. The `/ws` in `websocket.rs`'s tests is that test's own
  echo server, not this router's route, so both WebSocket paths were unverified while the
  handler underneath was well covered.
- A typo in either path would have disabled Codex support silently. That is CONTRIBUTING's
  first lesson — *a test proves a function works, not that anything calls it* — applied to
  routes rather than functions.
- **Added:** `every_declared_route_is_actually_reachable`, which requests each path and
  fails on 404 or 405. Everything else means a handler was reached, which is the claim.
  Plus `an_unregistered_path_is_still_a_404`, so it cannot pass by everything being
  reachable.
- **Added:** check 8 of the audit, which reads the `.route(` calls out of `server.rs` and
  fails if one is missing from the test's list — the list is hand-maintained because
  axum's `Router` cannot be enumerated, and a hand-maintained list is what this script
  exists to distrust.
- **My first version of the test was wrong.** It used the existing `fake_provider`, which
  registers `/v1/messages` only, so a relayed request to any other path came back 404 from
  the *provider*. It reported `/v1/chat/completions` as unrouted — the fake upstream's 404
  wearing the proxy's clothes. A test that cannot tell those apart accuses the wrong
  component; it now uses a provider that answers any path.
- Both guards verified by mutation: misspelling `/v1/realtime` fails the test, removing it
  from the list fails the audit.

---

## The MCP server could advertise a tool it then rejects
**2026-08-03** · the ninth copy of a name list, and the one nothing checked

- **The hole:** the tool names existed in three places — `TOOL_NAMES`, the advertised
  definitions in `tool_definitions()`, and `call_tool`'s match arms. One test tied the
  first two. **Nothing tied the third.**
- **Demonstrated end to end.** Renaming a tool in `TOOL_NAMES` and its definition produced
  a server that advertised `headroom_squash` on `tools/list` and answered
  `{"code":-32602,"message":"unknown tool: headroom_squash"}` when asked for it — with
  `cargo test --all-features` returning **0**. Every test called the tools by their old
  literal names, and the dispatcher still knew those.
- `headroom tools` prints `TOOL_NAMES`, so the CLI would have listed the broken tool too.
- **Changed:** the definitions take their names from `TOOL_NAMES`, and the dispatcher uses
  match guards against the same const, so all three copies are now one.
- **Added:** `every_advertised_tool_can_actually_be_called`, driven from `tools/list`'s own
  output rather than from `TOOL_NAMES` — what a client can call is what the client was
  told about. It distinguishes a dispatched tool declining (`missing 'content'`) from one
  the dispatcher has never heard of.
- Verified by re-applying the exact original bug and watching it fail.

---

## No invariant gate can now pass by doing nothing
**2026-08-03** · finishing the sweep across the whole file

- I2 and I9 already asserted that compression actually happened before checking what
  survived. **I3, I6 and both I4 tests did not** — and each of them is satisfied by a
  passthrough:
  - I3 asserts a second pass is a fixed point. Never moving is a fixed point.
  - I4 asserts eight runs produce equal bytes. Eight passthroughs are equal.
  - I4's warm/cold variant asserts a populated CCR store changes nothing. An empty one
    changes nothing either.
  - I6 asserts positions are preserved. Nothing moved, so they were.
- None was hollow *today* — the fixture does compress — but nothing held that true, and
  the property-test twin of I4 was hollow for its whole life until yesterday's fix.
- **Added:** the same guard the other two carry, to all four. Verified by shrinking the
  fixture's live tool result to one record and watching each fail with its own reason:
  `the first pass compressed nothing, so the fixed point is trivial`, `nothing was
  compressed in any of the eight runs`, `nothing was compressed, so the warm store was
  never populated`, `nothing was compressed, so preserved positions prove nothing`.

---

## I7 was passing with its guard removed
**2026-08-03** · the hot zone the tests protected was 29 bytes

- **The hole:** I2 and I7 assert that `system` and `tools` come back byte-identical. The
  fixture's system block was `"You are a careful assistant."` — 29 bytes — and its tool
  schema 50. Both are below every compressor's size threshold, so those bytes would have
  survived with the guard deleted.
- **Demonstrated.** With a synthetic mutation that compresses the system block — the guard
  removed, in effect — `i7_tool_definitions_are_never_compressed` **passed** under the old
  fixture and **fails** under the new one. That is the finding, measured rather than
  argued.
- Stated precisely for I2: under the same mutation it failed on its own "nothing was
  compressed" guard rather than on the hot-zone assertion, so its `system`/`tools`
  assertions are strengthened here on the same reasoning, but only I7's vacuity was
  directly demonstrated.
- **Changed:** the fixture's hot zone now carries ~10 KB of prose across both the system
  block and a tool description.
- **Added:** `hot_zone_bulk_is_independently_compressible`, which hands the same bulk to
  the same orchestrator outside the hot zone and requires that it shrinks. "Unchanged in
  the hot zone" is now a fact about the guard rather than about the size.
- The first version of that bulk joined its lines with a space. The prose compressor works
  on a line budget, so one long line is one line and nothing was dropped — 19019 bytes in,
  19019 out. Caught immediately by the new test, which is the same mistake, and the same
  catch, as the doctor prose sample.
- **Changed:** the live tool result grew from 120 records to 500, because the added hot
  zone is protected and therefore incompressible, and
  `compression_measurably_helps_while_every_invariant_holds` correctly read the dilution
  as a compressor that had stopped working.

---

## Three property tests were passing without compressing anything
**2026-08-03** · I4, I5 and I10, gated by a generator that never cleared a threshold

- **The hole:** every compressor declines below its `AdaptiveSizer` threshold. The
  determinism (I4) and I10 generators produced random printable ASCII up to 500 characters
  — measured, **all 200 I4 cases detect as prose**, against a 5120-byte prose threshold.
  Not one compressed. I4 asserted that two no-ops agree; I10 asserted that a policy which
  forbids compression declines to compress the uncompressible.
- I5's generator covered JSON and logs by accident and half its cases were sub-threshold
  prose, so code, diffs and search output were never exercised.
- This matters most for **I4**: determinism is the one invariant that genuinely differs
  per compressor — a `HashMap` iterated in the wrong place is nondeterministic in one and
  not the others — so a generator reaching none of them was the worst place to have it.
- **Added:** `compressible_shape`, one of every content type that reaches a compressor,
  each sized past its own threshold. Coverage now, per test: `I4` 200 cases across all six
  types; `I5` and `I10` 400 each, likewise.
- **Added:** a vacuity guard to each of the three — a property that never triggers the
  behaviour it constrains passes forever and means nothing. I10's is the strongest form:
  the same content must actually be compressed under pay-as-you-go, so "subscription left
  it alone" is a statement about policy rather than about size.
- Verified by restoring the old generator and watching all three fail on the guards.
- Note the interaction with check 7, added an hour earlier: it confirms each invariant is
  *cited* in the file the docs name. It cannot tell whether the test does anything. Both
  checks were needed, and neither substitutes for reading.

---

## `headroom doctor` was passing without checking two of six compressors
**2026-08-03** · found by a guard written for something else

- **Fixed:** the self-test had samples for JSON, logs, code and prose. It had none for
  **diffs or search results**, so `doctor` printed `all checks passed` having never run
  those two compressors, and `perf` never timed them. Found the first time
  `the_self_test_samples_cover_every_compressor` ran — it failed immediately on
  `diff_compressor compresses diff and no self-test sample exercises it`.
- **Fixed:** `headroom perf` benchmarked `SmartCrusher` on a JSON payload and printed the
  result as `throughput`, unqualified. It now routes through the `Orchestrator` and
  reports a row per compressor. The spread is **18×** — 63 µs/call for diffs against
  1138 µs for prose — and the number it used to print was the second slowest.
- The operational conclusion survives (even the slowest is a millisecond against a round
  trip of hundreds), but that is now measured rather than generalized from one sample. A
  compressor that regressed 100× would have stayed invisible.
- **Changed:** `doctor` and `perf` share `self_test_samples()`, so "it works" and "it is
  this fast" are statements about the same content.
- **Added:** three guards — samples cover every compressor the orchestrator routes to,
  each sample is still detected as its own label, and each still actually compresses.
- The first diff sample I wrote did not compress: it gave each hunk three context lines,
  all inside the two-line keep window, so the compressor correctly removed nothing.
  `every_self_test_sample_actually_compresses` caught it, which is the point of asserting
  a measured result rather than a return code.

---

## The audit now checks that the invariants are still gated
**2026-08-03** · no bug found — the claim was true, and is now held true

- README.md and ARCHITECTURE.md both name which file gates which invariant, and that is
  the load-bearing claim in this repository: the invariants are described as acceptance
  criteria on every change rather than aspirations.
- **Checked rather than assumed, and it holds** — `invariants.rs` carries `i1_`–`i4_` and
  `i6_`–`i9_`, eight of them; `properties.rs` covers I5 and I10, which are claims about
  many inputs and cannot be established by one fixture. Exactly what the docs say.
- **Added:** check 7 of `scripts/reachability-audit.sh`, so it stays true. A renamed or
  deleted gate test now fails the build instead of quietly turning two documents into
  fiction. Verified by renaming `i7_tool_definitions_are_never_compressed` and watching
  the audit name I7 and exit 1.
- This is the same shape as checks 1–6, one level up: not a capability nothing reaches or
  a decision copied twice, but a **guarantee nothing checks**.

---

## The Python binding's reasons match the rest of the project
**2026-08-03** · the eighth copy, across an FFI boundary, already wrong

- **Fixed:** `headroom-py` mapped the `Routing` variants itself and spelled three of six
  with hyphens — `policy-forbids`, `no-compressor`, `measured-useless` — where the proxy
  reports `policy_forbids` under `headroom_routing_total{reason=...}` and `headroom
  inspect` prints the same. The `reason` field exists to be correlated, and a caller
  correlating it matched nothing.
- **Changed:** `compress()` reports `Routing::as_str()` directly. `"not-smaller"`, the one
  outcome routing cannot produce, becomes `not_smaller`.
- **Added:** `headroom.REASONS`, built from `Routing::REASONS`, so a caller can enumerate
  the vocabulary instead of writing it down a ninth time.
- **Added:** `test_every_reason_is_one_the_module_declares` — asserts every observed reason
  is in `REASONS`, that at least four distinct reasons were actually produced so the
  subset check is not vacuous, and that nothing in the vocabulary contains a hyphen.
  Verified by putting a hyphen back and watching two tests fail.
- **Logged as D27**, not asked about: this changes values a public API returns. The package
  has never been published — no tags, outside `default-members`, wheel built in CI and
  discarded — so there is no caller to break. Had it shipped, it would have been a
  stop-and-ask.
- **Removed:** a tracked `__pycache__/*.pyc`, and added Python bytecode to `.gitignore`.

---

## The routing-reason list stops being copied across the crate boundary
**2026-08-03** · a telemetry panel that could have gone permanently empty

- **The hole:** `Metrics` held its own array of the seven routing reasons, hand-copied,
  with a comment saying they were what `Routing::as_str` produces. Nothing checked that.
  The failure is quiet by construction — `record_routing` puts an unrecognized reason in
  an `other` slot, so renaming a variant in `headroom-core`, or adding a seventh, would
  merge a whole category into `other` while every test stayed green and the dashboard
  panel for it went permanently empty.
- Same shape as `Declined::OutsideLiveZone`, removed earlier for describing a check
  nothing could perform: a panel that can never fill.
- **Added:** `Routing::REASONS` in core. `ROUTING_REASONS` is now built from it in a const
  block, with `other` appended in the proxy — `other` is not a routing outcome, it is this
  counter's answer to a reason from a `headroom-core` that disagrees with it.
- **Added:** `every_reason_is_in_reasons`, which catches all three ways this goes stale: an
  exhaustive match the compiler will not let go stale on a new variant; a `contains` for a
  renamed string; a length check for a string left in `REASONS` that nothing produces —
  which as a metric is a label that is always zero and reads as "this never happens"
  rather than "this no longer exists".
- Verified by renaming a variant's string and watching the core test name it.

---

## `headroom savings` is pinned to the names the proxy actually emits
**2026-08-03** · a copy across a process boundary, where no compiler was watching

- **The hole:** `savings_report` matches four metric names as string literals — a second
  copy of what `Metrics::render` writes, on the far side of a process boundary. Renaming
  `headroom_tokens_saved_total` in `metrics.rs` was tried, and **the entire test suite
  stayed green** while `headroom savings` would report `tokens saved 0` forever. `value`
  returns `None` for an absent name and the caller defaults it to zero, so the failure is
  a plausible number rather than an error.
- **Added:** `savings_reads_the_names_the_proxy_actually_emits`, which renders a real
  `Metrics` — including a labelled `headroom_routing_total` series from the same source
  rather than a typed fixture — and feeds it to the parser. It is the only test that goes
  red under the rename.
- **Renamed:** `the_savings_report_reads_real_exposition_text` →
  `..._parses_the_exposition_format`. Its `METRICS` is a fixture written by hand beside
  the parser, so it agrees with the parser whatever the proxy does. The parsing it checks
  is worth having; the name claimed more than that.
- This is CONTRIBUTING.md's own lesson — *a self-consistency test is not coverage for
  anything that crosses a process boundary* — found in the repository that wrote it down.

---

## `headroom env` stops emitting a URL `headroom wrap` would not
**2026-08-03** · the seventh copy, and this one was already wrong

- **Fixed:** `env` printed `ANTHROPIC_BASE_URL={proxy}` and `OPENAI_BASE_URL={proxy}/v1`
  as format strings while `Agent::env` — which `wrap` uses — trims a trailing slash. Same
  input, two commands, different answers:
  `headroom env --proxy http://x:8787/` gave `OPENAI_BASE_URL=http://x:8787//v1`;
  `headroom wrap aider` with the same argument gave `/v1`.
- **Why it survived:** `wrap` has a test for exactly that input. `env` did not, because it
  did not share the code the test covers — a test proves the function it calls works.
- **Changed:** `env` is now the union of `Agent::env` over `Agent::ALL`, deduplicated. It
  names no agent, so its answer is "whatever any of them might read", and an agent added
  with a new variable appears without anyone remembering to come back.
- **Added:** `agents_do_not_disagree_about_a_variable`, which makes the take-the-first rule
  visible rather than latent, and asserts a variable *is* claimed twice so it is not
  vacuous.

---

## The reformat list stops being written down twice
**2026-08-03** · the sixth copy, in the command the last PR had just fixed

- **Fixed:** `headroom tools` printed `minify_json` and `tidy_lines` as string literals
  while `Reformatter::apply` called the two functions in an `.or_else` chain. It was
  *right* — which is the only thing separating it from the five routing tables that were
  not, and not a property that survives somebody adding a third reformat.
- **Added:** `Reformatter::STEPS`, an array of `(name, fn)` pairs that `apply` iterates
  and `tools` reads. The name and the behaviour are now the same array, so a reformat that
  is not listed does not run, and one that runs is listed.
- **Added:** `ReformatStep`, the type alias behind it.
- **Guard:** `every_named_step_is_one_that_actually_runs` puts each step's fixture through
  `apply` and checks it reaches *that* step. Verified by making `apply` hardcode the first
  step and watching the test go red. A step with no fixture panics rather than skipping —
  a listed capability that cannot fire is what the reachability audit exists for.

---

## `headroom inspect` and `headroom tools` stop contradicting the pipeline
**2026-08-03** · the fourth and fifth routing tables, in the two commands that describe routing

- **Fixed:** `inspect` carried its own copy of the routing table, mapping prose to
  `"none"` — untrue since the prose compressors were wired in. The same 18 KB of prose
  got `compressor: none` from `headroom inspect` and `would save: 5205 (70%)` from
  `headroom compress`, in one shell, seconds apart.
- **Why it matters:** this is the command an operator runs when they are already
  confused about why something did not compress. Being wrong here sends them looking in
  the wrong place with a confident answer in hand.
- **Changed:** every routing line now comes from `Orchestrator::route` and
  `Orchestrator::transform_for_block` — the two functions the proxy itself calls.
- **Added:** the report now names the credential (I10) and the block kind (D24), because
  both change the answer and the old single line picked pay-as-you-go silently. An
  operator on a subscription token asking why nothing compresses was reading somebody
  else's answer.
- **Added:** a note when the size threshold will stop the named compressor. `route` does
  not consult the sizer — each compressor holds its own — so a short payload named a
  compressor that will decline, which reads as "this compresses".
- **Fixed:** `headroom tools` — the command that says what this build can do — carried a
  **fifth** copy, and its second list, "detected but not compressed", named code and
  prose. Both compress. So it reported the two largest categories of agent traffic as
  forwarded whole. It now reads the table out of `Orchestrator::for_type` across
  `ContentType::ALL`, so a type is in the second list exactly when nothing compresses it.
- **Added:** `ContentType::ALL`, `Orchestrator::for_type` (made public), and
  `Orchestrator::tool_output_only` — the D24 rule, now asked rather than restated, so
  `tools` marks prose `tool output only` instead of implying typed messages get
  rewritten. That is the same error the old list made, in the other direction.
- **Added:** check 6 of `scripts/reachability-audit.sh`, in two parts. **6a** fails on any
  content type paired with a compressor's name as a string literal; **6b** on a match with
  three or more `ContentType` arms outside two allowlisted files.
- **Noted:** 6b was written first and alone, and it does *not* catch `headroom tools` —
  that copy was a tuple in an array, not a match. It was found by reading the file, not by
  the check written to find it, which is why 6a anchors on the compressor name rather than
  on syntax. Both were verified by planting a table and watching the script exit 1.
- **Fixed:** the audit header said "four separate gaps" above a list of five; the README
  said DECISIONS.md had 24 entries when it had 26.

---

## `/health` stops reporting `ok` on a proxy that cannot relay
**2026-08-03** · the self-report with the widest blast radius

- **Fixed:** `/health` answered `{"status":"ok"}` with a `200` regardless. If the upstream
  client failed to build at startup the relay is disabled and **every** request returns an
  error — and the endpoint said the proxy was fine.
- **Why this one matters most:** load balancers and orchestrators route traffic on this
  signal. A proxy that cannot serve a single request would keep being handed them, and the
  operator debugging it would have a health check pointing them elsewhere.
- **Added:** `"degraded"` status, a `relay_available` field, and a **503** — most
  orchestrators read the code and never look at the body, so reporting the problem only in
  JSON reports it to nobody.
- **Named rather than folded in:** `status` tells an operator to look, `relay_available`
  tells them where.
- **The trigger is narrower than it sounds, and the doc comment says so.**
  `Upstream::new` fails only when `reqwest::Client::builder().build()` does — a TLS
  backend that will not initialize. **A malformed upstream URL does not trigger it**,
  checked by pointing a proxy at `"not a url at all"` and watching it report
  `relay_available: true`. So this state is covered by unit test rather than an
  end-to-end reproduction, which is stated instead of implied.

---

## `headroom doctor` checks the router, not a compressor it picked
**2026-08-03** · the self-test was the fourth routing table

- **Fixed:** `doctor` called `SmartCrusher` directly. That checks *a* compressor works,
  which is not the question an operator is asking — they want to know whether the proxy
  will compress their traffic. It reported **"compression: ok" for the entire period the
  proxy compressed neither code nor prose** (#82, #84).
- **Changed:** routed through `Orchestrator`, with one sample per content type the router
  can produce, so a type that reaches no compressor is named rather than averaged away by
  the ones that work.
- **Verified to catch the real defect:** deleting the `ContentType::Code` arm gives
  `compression (code): FAILED (detected code, reached no compressor)` and a non-zero exit.
- **Two bad samples of mine, caught by the tool itself:** the first prose sample was one
  24 KB line — the summarizer works on a line budget, so one line is left alone — and the
  first code sample repeated a single function, which a skeletonizer cannot usefully
  shrink. Both would have read as a broken install rather than a badly chosen sample. The
  new ones are varied and multi-line, with the reasoning in a comment beside each.
- **This was the fourth copy of "which compressor to use."** #82 collapsed three; the
  health check had a fourth, and it was the one an operator consults when they suspect the
  first three.

---

## `headroom init` generated a config that misdescribed itself
**2026-08-03** · the same false claim, one layer further out

- **Fixed:** the template opened with *"Read live on every request, so changes take effect
  without a restart"* and then listed `HEADROOM_HOST` and `HEADROOM_PORT` — which are read
  once, because the socket is bound at startup. The first file a new operator reads told
  them something untrue about the thing they were reading it to learn.
- **Added:** a `RESTART REQUIRED` note wherever it applies, and the five settings the
  template never mentioned at all — `HEADROOM_CCR_DIR`, `HEADROOM_REDIS_URL`,
  `HEADROOM_RECOMMENDATIONS`, `HEADROOM_MEMORY`, `HEADROOM_STABILIZE`. A store left unset
  means every CCR marker becomes unredeemable on restart, which is worth knowing before
  you find out.
- **Checked against the proxy's real list, not a copy.** `headroom-proxy` is now a
  dev-dependency of the CLI purely so the test can read `config::STARTUP_ONLY` directly. A
  copied list is how the template and the code drift apart, which is the bug this test
  exists to prevent.
- **Verified to bite:** removing one `RESTART REQUIRED` marker fails the test.
- **Verified through the release binary:** `headroom init` writes a file where every
  startup-only setting carries the note.
- **Third in a row of the same shape.** #101 was a guard that never ran, #102 an override
  reported as applied that never applied, and this is documentation asserting semantics
  the code does not have. All three were about a component describing itself.

---

## `/admin/runtime-env` stops reporting success for settings it cannot change
**2026-08-03** · the same endpoint, a second false report

- **Fixed:** five settings are read **once at startup** — `HEADROOM_CCR_DIR`,
  `HEADROOM_REDIS_URL`, `HEADROOM_MEMORY`, `HEADROOM_MEMORY_LIMIT`,
  `HEADROOM_RECOMMENDATIONS` — plus the host and port, since the socket is bound once.
  The endpoint stored them and answered `{"applied": [...]}`, which was false: the value
  sat in the override map and nothing ever read it again.
- **Why that matters more than it sounds:** an operator retuning a proxy during an
  incident is told the change took effect, and moves on to look elsewhere.
- **Added:** a `needs_restart` list in the response, and a `warn` log when it is
  non-empty. Empty is the common case and says "everything you set is live now".
- **Design note:** the values are still stored rather than refused. They take effect on
  the next restart, and rejecting them would make the endpoint useless for preparing a
  restart — the problem was the silence, not the storing.
- **Design note:** `STARTUP_ONLY` lives beside the variables it names, so a new
  startup-only setting is added to it in the same edit rather than discovered later by
  someone whose change silently did nothing. A test checks every name in it is a real
  setting, because a typo would drop a name from the warning and restore the original bug.
- **Verified through the release binary:**

```
{"applied":["HEADROOM_MEMORY"],"needs_restart":["HEADROOM_MEMORY"]}
{"applied":["HEADROOM_COMPRESSION"],"needs_restart":[]}
```

---

## Hot-reload could point the proxy at itself
**2026-08-03** · a decision invalidated by a later feature

- **Fixed:** `POST /admin/runtime-env` applied overrides with no validation, so
  `HEADROOM_UPSTREAM` could be set to the proxy's own listen address after startup. Every
  request would then forward to itself forever — pinned core, exhausted file descriptors,
  no readable error.
- **D11 justified having no per-request loop header** because a startup check already
  caught this. **D10 then added hot-reload**, and nothing re-checked. The decision was
  sound when written and quietly stopped being true.
- **Demonstrated, not argued:** a probe set the override and read the config back —
  `upstream=http://127.0.0.1:8787 listen=127.0.0.1:8787 self_referential=true`.
- **Design note:** the guard previews rather than applying and rolling back. Applying
  first would leave the bad config live during the check, and config is read per request
  from a thread pool — an in-flight request could start the loop in that window.
- **Design note:** the refusal is a 400, not the module's 403. The caller is local and
  allowed to be there; their configuration is wrong. A permission error sends an operator
  hunting an access problem during the incident they are trying to fix.
- **Two mistakes of mine, caught before merge:**
  - My first three tests exercised `preview_overrides` directly, so deleting the guard
    *from the handler* left them green — the same "is it actually called?" bug this
    repository keeps producing, reproduced in the fix for it. There is now a test through
    the endpoint.
  - Those tests then made the suite **flaky, failing 3 runs in 6**, by asserting on
    process-global override state. Worse than no test: a suite that goes green on a re-run
    teaches people to re-run it. Root cause was a *second* serialization mechanism added
    beside the `SERIAL` mutex this module already had — clippy caught it as a
    `MutexGuard` held across an await. Now one lock, 12 consecutive clean runs.

---

## D25's "no way to suppress it" checked against the source
**2026-08-03** · argued → measured, again

- D25 said reqwest "exposes no option to suppress" the injected `accept: */*`. That was
  the right conclusion **reached without looking**, which is the habit this session keeps
  catching. It is now read from `reqwest 0.13.4` and cited.
- **Three mechanisms compose to make it unsuppressable:** `ClientBuilder::new` inserts
  `ACCEPT: */*` before any caller can intervene (`client.rs:284`); `default_headers`
  *extends* that map rather than replacing it (`:1166`), so an empty `HeaderMap` cannot
  remove the entry; and at execute time defaults fill only vacant entries (`:2616`), so a
  header can be overridden but never made absent.
- **Why the precision matters:** "no option exists" is a claim that rots silently when a
  dependency updates. Three cited line references tell the next reader exactly what to
  re-check.

---

## D9 gets a test; the simulator learns to stall
**2026-08-03** · a decision with nothing enforcing it

- **Added:** `Reply::stalling(after, pause)`, which streams a body in two chunks with a
  real pause between them, and `a_stream_that_pauses_mid_generation_still_arrives_whole`.
- **The gap:** D9 deliberately sets no total-request timeout, because a long generation is
  a normal outcome rather than a stuck request. **Nothing tested it.** Adding
  `.timeout(30s)` to the upstream client is a natural review suggestion and would have
  started truncating real completions in production with every test green.
- **Verified by mutation:** a 500 ms total timeout fails the new test.
- **Stated limit:** the pause is short, so this cannot prove the absence of a timeout —
  only of an aggressive one. What it pins is that the relay holds a stream open across a
  gap rather than ending it. The test says so rather than implying more.
- **Design note:** the stall streams two chunks rather than sleeping and then answering.
  Sleeping first tests nothing — the client sees one fast response that happened to arrive
  late, which is not the shape of a long generation on the wire.
- **Design note:** the split lands *mid-frame*, not on an event boundary, so a timeout
  that only counted whole frames could not slip through.
- **Found while wiring it:** `Simulator::start` and `strict_router` each built their own
  response, so a capability added to one silently did nothing in the other. They now share
  one responder — the same duplicate-implementation shape as the three routing tables.

---

## The cross-process claim, actually measured
**2026-08-03** · evidence for D12

- I flagged the `StructureHash` finding as **reasoned from the data flow, not observed**.
  This closes that: the `learn` → proxy loop was run end to end with the release binaries
  across two real processes.
- **As shipped:** `headroom learn` wrote `payg|claude-opus|f9439d246d8721bc`; a separate
  proxy process loaded the file and routed the same shape as `measured_useless`. The loop
  closes across a process boundary, which nothing had ever demonstrated.
- **With FNV-1a swapped for a per-process seed:** `learn` wrote
  `payg|claude-opus|e11f96ea63309f44`, the proxy computed a different key, the lookup
  missed, and it routed `compress` instead — **silently**. No error, no warning, a healthy
  set of counters, and the measure-then-skip loop simply not working.
- **D12 now carries the measurement** rather than the argument. The pin added in #94 is
  what makes that a build failure instead of a production one.

---

## Both lessons written into CONTRIBUTING
**2026-08-03** · documentation

- **Added:** two sections recording what mutation testing found here, and a line in the
  workflow: *watch the test fail before you trust it*.
- **"A test proves a function works, not that anything calls it."** Five capabilities were
  shipped, tested, documented as done, and reached by nothing. The audit script checks the
  mechanical part; the rest is a habit, so it is now written where a contributor meets it.
- **"A self-consistency test is not coverage for anything that crosses a process
  boundary."** `of(x) == of(x)`, `parse(format(x)) == x` and `of(a) != of(b)` all survive
  a format change intact — both halves move together. The five formats that cross a
  boundary here are tabulated with where each is pinned.
- **Added to the review checklist:** ask what mutation a new test would catch. If the
  answer is "none that could plausibly happen", it is documentation rather than a gate —
  which is fine, as long as it says so.

---

## Two more wire formats pinned
**2026-08-03** · same shape as the `StructureHash` gap, one layer out

- **Added:** `the_aggregation_key_wire_format_is_pinned` and
  `the_marker_wire_format_is_pinned`.
- **`AggregationKey::as_str()` is the key in `recommendations.json`**, written by
  `headroom learn` in one process and read by the proxy in another, quite possibly from a
  different build. The separator, the field order and the model-family reduction are all
  **wire format**. It was tested only for distinctness — swap `|` for `:` and every
  previously written recommendation silently stops matching.
- **The CCR marker outlives its request.** `marker_round_trips` parses what it just
  formatted, so both halves move together under any format change. But a marker sits in a
  conversation the model still holds and is redeemed later through `headroom_retrieve` —
  possibly against an upgraded binary. Changing the delimiters would make every
  outstanding marker unredeemable, and the model would be told its content had expired.
- **Both verified by mutation:** changing the separator to `:` and the prefix to `[[ccr:`
  fails the new tests (and one existing retrieve test), and passed everything before.
- **Already pinned, and the reason the omissions were visible:** `ContentHash`
  (`000f01ff`) and `model_family` (`claude-opus-4-20250514` → `claude-opus`).

---

## `StructureHash` pinned, closing a silent cross-process failure
**2026-08-03** · mutation-testing determinism

- **Added:** `the_fingerprint_is_pinned_to_a_literal`. `StructureHash` was tested only for
  self-consistency (`of(x) == of(x)`) and distinctness (`of(a) != of(b)`) — **both of
  which a per-process seeded hasher satisfies.**
- **Verified by mutation:** swapping FNV-1a for a `OnceLock<RandomState>` — stable within
  a run, different in the next process — **passed the entire workspace suite**. With the
  pin in place it fails immediately.
- **The failure it would have caused is silent and total.** `headroom learn` writes
  recommendations in one process and the proxy reads them in another; the aggregation key
  contains this hash. If it varied across processes every lookup would miss, and the
  measure-then-skip loop would quietly stop working while looking healthy — compression
  re-attempted forever on shapes already measured as useless. `DECISIONS.md` D12 gives
  cross-build stability as the reason FNV-1a was chosen; nothing enforced it.
- **`ContentHash` was already pinned** the same way (`000f01ff`), which is what made the
  omission visible by comparison.
- **Corrected a comment that claimed more than its test checks.**
  `i4_holds_across_separate_proxy_instances` said "a fresh process must agree with a warm
  one" — both instances live in the same process. It compares warm-vs-cold *state*, which
  is worth checking; cross-process determinism is guarded by the pinned hashes instead,
  and the comment now says so.

---

## I1 now gates whitespace, which nothing did
**2026-08-03** · mutation-testing the headline invariant

- **Added:** `i1_insignificant_whitespace_survives_untouched`, and a hand-written
  pretty-printed fixture carrying spacing no serializer in this build would choose.
- **The gap:** `hostile_request` is fully compact, and every other fixture is built by
  `serde_json`, which also emits compact JSON. So the entire suite could not distinguish
  *forwarded the original bytes* from *re-serialized them and happened to match*. I1 is
  the project's headline guarantee and its gate was blind to the one thing a `Value`
  round-trip actually destroys.
- **Why it matters in practice:** a client that pretty-prints its request bodies would
  have missed the provider's cache on **every single request**, and this suite would have
  stayed green through all of it.
- **Demonstrated rather than asserted.** Forcing `compress_dialect` to rebuild a body it
  had no edits for — semantically invisible, byte-visible — fails the new test and
  **passes every other I1 test in the file**. That is the gap stated exactly.
- **Three layered early returns** make that mutation hard to reach at all:
  `edits.is_empty()`, `replacements.is_empty()`, and `rebuild`'s own. Removing any one
  alone leaves the suite green, so the new test gates the outcome rather than any single
  guard — and the doc comment says so instead of claiming more.

---

## An I2 gate with a frozen prefix that is actually frozen
**2026-08-03** · mutation-testing the existing suite

- **Added:** `i2_bulky_content_behind_a_breakpoint_is_left_alone`, and a fixture with a
  real `cache_control` breakpoint and equally compressible content on both sides of it.
- **Found by mutation.** Replacing the frozen floor with a constant `0` left the entire
  end-to-end suite green. The existing I2 fixture's "frozen turns" were `"turn one"` and
  `"answer one"` — nothing a compressor would touch — so the gates were asserting that
  short strings stayed short. Worse, that fixture carried **no `cache_control` marker at
  all**, and Anthropic caches only what the customer marks: `frozen_message_count`
  returned zero, so nothing in it was ever frozen in the first place.
- **What the mutation actually revealed is a second protection, not a hole.** The live
  zone also applies a newest-claims rule — the newest message holding a category owns it,
  and older messages holding the same category are skipped. For the common agent shape
  that rule alone confines compression to the newest turn, and the frozen floor is
  defence in depth over it. Disabling **either** leaves the suite green; disabling **both**
  fails the new test, which is how the independence was confirmed rather than assumed.
- **The test's comment says exactly that.** Claiming it gates the frozen floor would be
  the same error as calling a module done because its tests pass — it gates the end-to-end
  outcome, and it asserts both halves (frozen content unchanged, live content compressed)
  so it cannot be satisfied by a proxy that compresses nothing.

---

## `headroom savings` pinned against labelled metrics
**2026-08-03** · follow-up to the routing telemetry

- **Added:** `savings_ignores_labelled_series`, which feeds the report a scrape containing
  `headroom_routing_total{reason=...}` and asserts the scalar counters are still read
  correctly. That metric is the first labelled series the proxy exposes and this report
  predates it, so the interaction had no coverage.
- **Added:** `savings_reports_nothing_measured_rather_than_zero_percent` — an empty scrape
  and a broken compressor look identical if the report prints `0.0%`, and telling them
  apart is what it is for.
- **A comment I nearly shipped, corrected by testing it.** The first version claimed the
  trailing space in `"headroom_requests_total "` was load-bearing and that removing it as
  a tidy-up would make every value silently read zero. Removing it was then tried, and
  the tests still passed: `find_map` keeps looking when a match fails to parse, so the
  scalar is found either way. The comment now describes both guards accurately and says
  the behaviour is pinned by the test rather than by either mechanism.
- **Verified through the release binaries**, not only in tests: a real `/metrics` scrape
  containing the labelled series piped into `headroom savings` reports
  `reduction 32.8%` correctly.

---

## A decline reason that could never fire
**2026-08-03** · dead state removed

- **Removed:** `Declined::OutsideLiveZone`. It appeared only in its own declaration, its
  message string, and one test — **nothing produced it, and nothing could**. A `Block`
  carries no position, so the transform layer has no way to know where in a conversation
  it came from. The frozen prefix is protected *structurally*, by the live-zone
  dispatcher never offering those blocks to a transform at all.
- **Why remove rather than document:** these reasons are telemetry. An operator builds a
  dashboard from them, and a reason that cannot fire is a panel that stays empty forever
  while looking like it means something.
- **Added:** check 5 to `scripts/reachability-audit.sh` — every `Declined` variant must
  appear in an actual `Error::declined(...)` construction. Verified to bite: reinstating
  the variant makes the script fail.
- **A false positive, caught while writing it.** The first version counted every
  `Enum::Variant` mention and reported six `AnchorKind` variants as dead — all six are
  built inside their own module, which the check excluded. Matching on *construction*
  rather than mention fixes it. That is the second cry-wolf bug in this script, and both
  are now recorded in its header as the reason the checks are phrased the way they are.

---

## The reachability audit becomes a script in CI
**2026-08-03** · tooling

- **Added:** `scripts/reachability-audit.sh`, run in CI ahead of the build. Four checks:
  every detected content type reaches a compressor, every proxy module is referenced
  from outside itself, every CLI command is dispatched, every declared environment
  variable is read.
- **Why:** five capabilities were shipped, tested, documented as done, and never called —
  the SSE observers, the memory module, cache stabilization, the code compressor and the
  prose compressors. Every one had passing tests. **A test proves a function works, not
  that anything calls it.**
- **Verified to bite:** deleting the `ContentType::Code` arm from the routing table
  reproduces the #82 defect and the script fails. A check nobody has seen fail is not a
  check.
- **A false-positive bug, caught before it shipped.** The first version of check 4
  searched for the variable's *string value* and reported all eleven as unread. An audit
  that cries wolf is worse than no audit, because people learn to skip its output. It now
  matches on the Rust constant name.
- **Design note:** the script's header records *which* gaps motivated each check,
  including why check 1 is phrased as it is — the first audit asked "is this referenced
  outside its own file", which the code and prose compressors both passed, because the
  CLI and MCP server referenced them. Reachable from somewhere is not reachable from the
  request path.

---

## The invariant gate covers I6 through I9
**2026-08-03** · gap row E2, extended

- **Added:** end-to-end gates for **I6** (position-preserving), **I7** (tools never
  compressed), **I8** (signed and encrypted blocks passthrough-only) and **I9**
  (telemetry observes). E2's row scoped itself honestly to I1–I4; the other six were
  acceptance criteria with unit and property coverage but nothing asserting them through
  a real relay.
- **I5 and I10 stay in `properties.rs`** deliberately. Both are statements about *many*
  inputs — "never larger, for any body", "never modified, under any restricted policy" —
  which a single fixture cannot establish.
- **Two of my own tests were wrong, and the guards caught them.** The I8 fixture
  interpolated a JSON array into a JSON *string* without escaping, so the body never
  parsed and the proxy forwarded it untouched — the assertion passed for entirely the
  wrong reason, and the "nothing was compressed" guard is what exposed it. The I7
  assertion scanned the whole body for a CCR marker and failed on the tool result, where
  a marker is compression working correctly.
- **A test that proved nothing, fixed.** The I8 thinking block originally held the string
  `"step one"`. No compressor would touch that regardless of any guard, so the test would
  have passed with I8 entirely absent. It now carries the same bulky JSON as the tool
  result beside it — the exact shape the compressor handles best.
- **Corrected a misleading comment.** `Orchestrator::is_eligible` claimed to be one of
  "two independent checks for invariant I8". It is called by nothing on the request path.
  The two that genuinely run are `live_zone`'s categorizer and `apply_guarded`, and that
  pair is real defence in depth — removing either alone still leaves signed content
  protected, which is why removing one did not fail the new test.

---

## `/metrics` says why traffic did not compress
**2026-08-03** · gap row N1's proxy-side half

- **Added:** `headroom_routing_total{reason=...}`, counting every block by the routing
  decision it received — `compress`, `lossless`, `policy_forbids`, `unsafe`,
  `no_compressor`, `measured_useless`, and `other`.
- **Added:** `Compressors::with_metrics`, attached by the proxy and nothing else.
- **Fixed:** `Compressors::routing()` was called from **one place in the workspace, a
  test**. `Orchestrator::transform_for`'s doc comment says "callers recording telemetry
  should use `route`" — no caller recorded telemetry. Six carefully distinguished reasons
  were computed for the decision and thrown away.
- **Why it matters:** the reasons have opposite fixes. `policy_forbids` means check the
  auth mode; `no_compressor` and `not_smaller` mean no action at all. An operator whose
  traffic was not shrinking saw only `headroom_passthrough_total` and could not tell
  "broken" from "working as intended".
- **This is also how #81 and #83 stayed hidden.** The proxy silently recorded
  `no_compressor` for every source file and every prose tool result, and nothing surfaced
  it. A per-reason counter would have made both visible on day one.
- **Design note:** one labelled metric, not six named ones. The reasons are one dimension
  of one measurement, and six counters would make "how many blocks were declined for any
  reason" a query nobody writes.
- **Design note:** counted per *block*, not per request. One request carries blocks that
  routinely route differently — a JSON tool result compresses while the prose beside it
  is below threshold — and a per-request label would be whichever block came last.
- **Design note:** an unrecognized reason lands in `other` rather than being dropped.
  Telemetry that quietly loses a category is exactly how a content type goes unnoticed.
- **Invariant I9 holds, with a test:** the compressed bytes are byte-identical with and
  without the metrics handle attached, and the test fails if nothing was recorded.
- **Verified through the release binary**, four requests across three auth modes:

```
headroom_routing_total{reason="compress"} 2
headroom_routing_total{reason="lossless"} 1
headroom_routing_total{reason="policy_forbids"} 1
headroom_routing_total{reason="unsafe"} 0
```

---

## The proxy compresses prose from tools — and never what a person wrote
**2026-08-03** · gap row C10, and a correction to S4/S5

- **Fixed:** `Orchestrator` had no arm for `ContentType::Prose`. `TextCrusher` and
  `TextSummarizer` were referenced by **nothing but the `lib.rs` re-export**, so every
  prose tool result was forwarded whole. A 22 KB tagged report now compresses to
  **4,529 bytes**.
- **Added:** `Orchestrator::transform_for_block`, used by the proxy. Prose routes only
  when `block.kind().is_tool_output()`.
- **Correction to an earlier claim.** PR #71 closed **S4** and **S5** by wiring the
  anchor and tag keep-sets into `TextSummarizer`, and I reported them as reached from the
  request path. **They were not** — the compressor holding them was itself unreachable,
  so neither ran on proxied traffic. They only start running here. The wiring was real
  and tested the whole time; what was wrong was my claim about what reached it.
- **Design note:** `BlockKind::is_compressible()` includes `Text` — what a user typed or
  a model wrote — and the prose compressor is lossy. Dropping lines from a directory
  listing is the product; dropping them from somebody's message is rewriting what they
  said. Verified end to end: the same prose compresses 22,331 → 4,529 bytes as a tool
  result and arrives **byte-identical** as a user message.
- **Design note:** the rule is prose-only on purpose. A person does not type a 5 KB
  unified diff into a chat box, and if they do, compressing it is what they were asking
  for. See `DECISIONS.md` D24.
- **Design note:** this makes the proxy and the content-only callers (CLI, MCP, Python)
  differ, deliberately. D23 was about ending exactly that kind of divergence, so the
  difference is two named entry points with the reason in both doc comments, not a flag
  someone has to remember.
- **Documented rather than wired:** `TextCrusher` performs the *same* normalization as
  `pipeline::reformats::tidy_lines`, which is already reached through `Reformatter` on the
  lossless branch. Routing it too would give prose two lossless paths that could disagree
  — the drift D23 exists to end. It stays as public API, marked redundant so nobody
  "fixes" it by wiring it up.
- **Verified end to end:** the tag keep-set holds `</result>` through an 80% reduction,
  and the boundary anchor holds the final line of a 21 KB report — the first time either
  has run against a real request.

---

## The proxy compresses code, and three routing tables become one
**2026-08-03** · gap rows C11–C13

- **Fixed:** `Orchestrator` held **no code compressor**. `ContentType::Code` fell through
  to `NoCompressor`, so the proxy forwarded every source file whole — the largest single
  category of agent tool-result traffic, silently exempt for the entire life of the
  pipeline refactor. Measured on an 11 KB Rust file in a tool result: **11,783 → 7,258
  bytes**, and `headroom_passthrough_total` went 1 → 0.
- **Fixed:** `headroom compress --dry-run` carried its own routing table, *with* a code
  arm, and reported "would save 1752 (32%)" for content the proxy would forward
  untouched. That command exists to predict the proxy; it was contradicting it.
- **Changed:** the CLI and the MCP server now route through `Orchestrator`. Three copies
  of the routing decision existed — `headroom-core`, `headroom-cli`, `headroom-mcp` — and
  they had already drifted.
- **Changed:** `Orchestrator::route` asked `for_type` which content types are
  compressible instead of restating the list. There were **two** lists inside the
  orchestrator alone, and adding the code arm to one was not enough to make code route.
  One list, one answer.
- **A blind spot in the #76 audit, and worth recording.** That audit looked for
  references *outside the defining file*, and `CodeCompressor` had them — from the CLI and
  the MCP server. **Reachable from somewhere is not reachable from the request path.**
  The same check that caught three unreachable subsystems was structurally unable to
  catch this one.
- **Design note:** the CLI and MCP paths pass `AuthMode::PayAsYouGo`. That is the
  operator compressing their own content deliberately, not a relayed request whose
  credential decides what is permitted — the proxy applies the real policy to real
  traffic (I10).

---

## A shared CCR store, and the proxy stops losing originals on restart
**2026-08-03** · gap row R4

- **Added:** `ccr::RedisCcrStore`, behind an **off-by-default** `redis` feature on
  `headroom-core`, forwarded by `headroom-proxy` and `headroom-mcp`.
- **Added:** `Config::ccr_store()` — Redis, then a directory, then memory — plus
  `HEADROOM_REDIS_URL` and `HEADROOM_CCR_DIR` on the proxy.
- **Added:** a CI job with a real Redis service, since the store's tests skip when
  nothing answers and "it compiles" would otherwise be the only thing checked.
- **Fixed, and this is the bigger half:** the proxy constructed an in-memory CCR store
  **unconditionally**. Every stored original was dropped on restart, leaving any
  `<<ccr:HASH>>` marker the model still held unretrievable — and with two workers the
  marker is created on one process and requested from another that never saw it. That
  failure is intermittent by construction and reads as data loss. `FileCcrStore` was
  reachable from the MCP server but never from the proxy.
- **Design note:** the feature gate answers the objection that deferred this row rather
  than overriding it. A build that does not ask for the backend does not compile it or
  carry it, and `default-features = false` keeps out the async runtimes and TLS stacks
  the client's default build pulls in.
- **Design note:** the MCP server selects the same way, because it is the *retrieval*
  half. A local store there answers "expired" for everything the proxy compressed.
- **Design note:** expiry is the server's job — `SET ... EX`, and `purge_expired` returns
  zero as the truth rather than a stub. Sweeping from every worker would be several
  processes racing to delete the same keys while disagreeing about a clock none of them
  owns, which is what a shared store exists to remove.
- **Design note:** a build *without* the feature that finds `HEADROOM_REDIS_URL` set says
  so instead of falling back silently. That symptom — retrievals failing on some workers
  — is identical to a Redis that is down, and an operator would debug the wrong thing.
- **Design note:** one mutexed connection rather than a pool. CCR does one round trip per
  compressed block, against a model call costing orders of magnitude more; a pool would
  add a dependency and an exhaustion failure mode for throughput this path does not have.
- **Verified across processes:** the proxy compressed a tool result to a marker with
  Redis configured and exited; the key survived, and a separate `headroom-mcp` process
  retrieved the original **byte-identical** — `303310e7145adcb6…`, 24,579 bytes.

---

## D2's premise corrected
**2026-08-03** · documentation only

- **Corrected:** D2 deferred the Redis CCR backend (R4) partly because "this environment
  has none". `redis-server` is installed at `/usr/bin/redis-server` and was confirmed to
  start and answer `PING`. Found by re-checking every *environmental* deferral after D4
  turned out to be wrong the same way.
- **The deferral still stands, on an honest reason.** R4 needs the `redis` crate — a
  dependency this run's scope did not name, unlike `pyo3`/`maturin` which it did — for a
  backend the reference itself calls optional. That is the owner's call, not one to make
  unattended.
- **Design note:** the false premise is corrected in place rather than the entry rewritten
  to look like it was always right. A deferral resting on a wrong premise reads as settled
  when it is not, which is exactly how B1/B2 stayed unbuilt for no reason.
- **Checked and standing:** D16's deferral of T3 (HuggingFace tokenizers) rests on
  architecture, not environment — a per-model `tokenizer.json` fetched at runtime would
  make the tokenizer a network dependency of the request path, and Anthropic publishes no
  tokenizer to be exact against. Nothing about this machine changes that.

---

## Python bindings ship; a deferral is reversed
**2026-08-03** · gap rows B1 and B2

- **Added:** `crates/headroom-py`, a pyo3 abi3 extension module — `compress()`,
  `count_tokens()`, `detect_content_type()`, and a frozen `CompressionResult`.
- **Added:** `pyo3-log`, initialized at import, so the engine's `tracing`/`log` output
  reaches Python's `logging` instead of vanishing (B2).
- **Added:** a `python` CI job that builds the wheel, installs it, and runs `pytest`.
- **Reversed:** `DECISIONS.md` D4 deferred these rows because "neither maturin nor a
  Python toolchain is available here". **That was asserted, not checked.** Python 3.11
  with headers is present and `maturin` installs cleanly. D4's own "would change if"
  named exactly this condition. D4 is kept and annotated rather than deleted, because the
  failure worth remembering is that it was recorded as a fact without being verified.
- **Design note:** the binding routes through `Orchestrator` rather than assembling its
  own compressor set. Assembling one would have been shorter and is the mistake this
  codebase already made once — the proxy carried its own copy of the routing decision,
  the CLI carried another, and nothing failed when they drifted. Verified rather than
  assumed: the same log compresses to a **byte-identical** result through
  `headroom.compress()` and through the `headroom compress` CLI
  (`b6d2e747b6148e7d…` both sides).
- **Design note:** the CCR store is per call, so a `<<ccr:HASH>>` marker in returned text
  is not retrievable through this API. A process-lifetime store would let one caller
  fetch content from a request they never made. Retrieval belongs to the proxy and the
  MCP `headroom_retrieve` tool, which own stores with a defined scope.
- **Design note:** an unknown `auth_mode` raises `ValueError` rather than defaulting.
  Defaulting would hand the most permissive policy to a caller who misspelled the most
  restrictive one — invariant I10 decided by a typo.
- **Design note:** `headroom-py` stays out of `default-members`, so the everyday
  `cargo build`/`cargo test` loop still needs no Python toolchain. That also means the
  Rust job never builds the module, which is why the wheel gets its own job — a binding
  never imported in CI is precisely the state D4 was worried about.
- **Measured from Python:** a 400-line log went 9,599 → 80 tokens; 300 JSON records went
  6,502 → 100. Subscription mode returned the input unchanged (I10), and 20 repeat runs
  produced one distinct output (I4).

---

## The reachability audit, recorded
**2026-08-03** · documentation only

- **Added:** a "Reachability audit" section to `gap-analysis.md`, naming the three
  clusters that were marked done while nothing called them (S4/S5/X12, the `memory`
  module, the `stabilization` module) and the PRs that closed each.
- **Changed:** Y2 is now marked **library surface only**. `SharedContext` has no request
  path and exposing it would mean a fourth MCP tool the reference does not have — the
  same reasoning as D19, stated rather than left as an unexplained silence.
- **Design note:** the audit is the deliverable, not just its findings. It checked every
  public symbol in `headroom-core` and every module in `headroom-proxy` for references
  *outside the defining file*, then verified the CLI's commands against `main.rs` and the
  MCP tools against the tool table. It now comes back clean.
- **Why this is written down:** the sweep that declared "97/97, none outstanding" was
  measuring the wrong thing. A test proves a function works, not that anything calls it —
  and three separate subsystems were shipped, tested, documented as done, and unreachable.

---

## Cache stabilization reaches the request path, behind an opt-in
**2026-08-03** · gap row X15 and invariant I7's normalization half

- **Added:** `stabilization::stabilize`, called from all three request handlers, and
  `body::replace_top_level_member` (byte-faithful, the counterpart to the existing
  insert).
- **Added:** `HEADROOM_STABILIZE`, **default off**.
- **Changed:** breakpoints now sit at fixed anchors (1, 3, 7, 15) rather than an even
  spread.
- **Found by the same audit as Y3:** all four public functions in `stabilization.rs` had
  zero references outside their own file. X15 was marked "Done" on the strength of the
  function existing.
- **The invariant tests caught this, and they were right.** Wiring it in made two I2
  tests fail: normalizing tools rewrites `tools`, and placing a breakpoint rewrites a
  frozen message — both the hot zone I2 says is never modified. Rather than relax the
  tests, stabilization is opt-in and the tests run against the default, so I2 stays a
  property rather than a slogan. See `DECISIONS.md` D20.
- **Design note:** there is no placement that avoids the hot zone. A marker on the newest
  message would be legal *and worse than useless* — it moves next turn, so the prefix it
  caused to be cached no longer matches, and Anthropic bills cache writes at a premium.
  It would pay to write a cache that is never read.
- **A latent defect fixed on the way in:** the existing even-spread placement recomputes
  to a different set every couple of turns, and the index that moves first is the
  *earliest* — rewriting the head of the prefix and invalidating the whole cache.
  Modelling it across turns showed the set changing at 6, 10, 14 and 18 messages. Wired
  as written, the feature would have busted the cache every two turns on exactly the long
  conversations it exists to help. Fixed anchors are monotone: only ever added, never
  moved, and there is a test that fails if that stops being true.
- **Corrected:** X16 (`prompt_cache_key`) was *already* on the request path, in
  `openai::shape_openai`, byte-faithfully. `stabilization::inject_prompt_cache_key` is a
  `Value`-shaped duplicate; routing through it would re-serialize the whole body and cost
  the cache miss the key exists to avoid. It is documented as such rather than wired.
- **Verified through the release binary:** with `HEADROOM_STABILIZE` unset the request
  reaches the provider byte-identical; with it set to `1` the tools arrive sorted
  (`zebra, apple` → `apple, zebra`), breakpoints land at indices 1, 3 and 7, and every
  unmarked message is still a verbatim byte copy.

---

## Memory reaches the live-zone tail
**2026-08-03** · gap row Y3

- **Added:** `memory::inject_append` and `MemoryStore::from_jsonl_lossy`.
- **Added:** `HEADROOM_MEMORY` (a JSON-lines file) and `HEADROOM_MEMORY_LIMIT`
  (default 8), read once at startup.
- **Changed:** `compress_dialect` appends the `<memory>` block to the newest user
  message's last text block, merging with a compressor's output rather than replacing it.
- **Found by an audit, not by a test:** the whole `memory` module had **no reference
  outside `memory.rs`** — implemented, tested, and unreachable. The gap row said "proxy
  wiring outstanding" and the sweep counted it as done anyway.
- **Design note:** gated on `policy.lossy_transforms`, not `lossless_transforms`. The
  lossless permission is granted on OAuth because a meaning-preserving change cannot
  exceed a granted scope; injection adds content the client never sent, which plainly
  can. Only pay-as-you-go traffic gets it. See `DECISIONS.md` D19.
- **Design note:** memories come from a file rather than an MCP tool. Nothing in this
  proxy populates a store, and the reference's tool surface has no `remember` — adding
  one would be inventing surface rather than reaching parity.
- **Design note:** read once at startup, not per request. A memory set that changed
  between requests would make the same request produce different bytes depending on when
  it arrived, and those bytes go upstream — busting the very cache the live-zone
  placement exists to protect (I4).
- **Design note:** re-injection is guarded on the `<memory>` opening tag rather than on
  the whole block, because the memory set grows between turns and matching the full text
  would re-inject a superset alongside the subset already there.
- **Verified through the release binary:** with a memory file and `x-api-key`, the block
  arrives on the newest user message with `system` and both frozen turns untouched;
  with an `sk-ant-oat` bearer, nothing is injected; with no file configured, the request
  reaches the provider byte-identical. A malformed line and a line without `content` were
  skipped with warnings, and a fact recorded by two agents arrived marked
  `(corroborated)`.

---

## Each surface is read by its own stream vocabulary; anchors and tags now bind
**2026-08-03** · gap rows X12, S4, S5 — the last three open rows

- **Added:** `sse::Observer`, which picks the stream classifier from the request path,
  and `ObservingStream::new` now takes that path.
- **Added:** `signals::keep_with_required`, and `TextSummarizer` feeds it the union of
  `select_anchors` and `protected_lines`.
- **Fixed:** every relayed response was read with the Anthropic classifier. **A failing
  OpenAI stream reported no failure** — its error frame is `{"error":{…}}` with no
  `type`, which the Anthropic classifier files under "something else". Measured through
  the release binary against a fake provider returning a failing chat stream:
  `headroom_stream_errors_total` went `0` → `1`. The proxy's error rate was pinned at
  zero for two of the three supported surfaces.
- **Design note:** the Responses vocabulary has no `[DONE]` sentinel and no
  `message_stop`, so an Anthropic-read Responses stream also never completed and piled
  every ordinary frame into the unknown-type log. The wrong classifier never errors — it
  reports confidently wrong numbers, which is worse than reporting none.
- **Design note:** `Observer::cache_tokens()` returns zero for both OpenAI surfaces
  because neither reports cache usage in its stream. That is the truth rather than a
  gap; a synthesized number would corrupt the one metric this proxy exists to move.
- **Design note:** the required keep-set is a **floor**, not a suggestion. When it
  exceeds the line budget the budget loses, because an anchor dropped to fit a line
  count leaves content whose meaning depended on it and nothing downstream can tell —
  the remainder reads as though it were always complete. Invariant I5 is what makes
  overshooting safe: a result no smaller in tokens is discarded rather than sent.
- **A test that proved nothing, replaced:** the first S4 test asserted a `# Heading`
  survived the lossy pass. It passed *without* the wiring, because headings already
  score as notable. The real case is the **boundary** anchor: in uniform prose every
  line scores the same, ranking falls back to source order, and the last line is always
  the first thing dropped — quietly turning truncated output into output that reads as
  complete. Both S4 and S5 tests were then confirmed to fail with the wiring removed.

---

## `headroom mcp` registers the MCP server; the gap analysis is swept to closure
**2026-08-03** · gap row M5, plus a documentation pass over every row

- **Added:** `headroom mcp --config <path>` writes a `headroom` entry into a host's
  `mcpServers` map, and `--uninstall` removes it.
- **Added:** `wrap::install_mcp_server` / `wrap::uninstall_mcp_server`.
- **Changed:** `gap-analysis.md` now carries a status note on every row. The sweep
  reports **97 rows, 97 accounted for, none outstanding** — implemented, substituted, or
  deliberately deferred with a decision reference.
- **Design note:** the entry records the *absolute* path of the `headroom-mcp` binary
  sitting beside the CLI. An MCP host launches the server as a subprocess, and a GUI
  application's `PATH` is frequently not the shell's — a bare `headroom-mcp` works from a
  terminal-launched host and fails silently from a dock-launched one. If that binary is
  not found, the bare name is written rather than an invented path: a host reporting
  "command not found" is clearer than one failing to execute a file that never existed.
- **Design note:** installing twice is a no-op that says so rather than overwriting. An
  entry already present is one somebody wrote, possibly with arguments or environment
  this command knows nothing about.
- **Design note:** a config file this command *created* gets no backup. A `{}` backup
  would let a later restore recreate an empty config the user never had.
- **Design note:** uninstall *edits* the config rather than restoring the backup, because
  the user may have added other servers since the install.
- **Known limitation:** the config path is explicit rather than discovered. Host config
  locations differ per host and per platform, and guessing wrong means writing a config
  file nothing reads while reporting success.

---

## OAuth traffic gets lossless compression; the proxy uses the orchestrator
**2026-08-03** · clears two limitations from the pipeline change

- **Added:** `Reformatter`, a `LosslessTransform`, routed by the orchestrator.
- **Changed:** the proxy's `Compressors` is now a thin wrapper over
  `pipeline::Orchestrator`, so the duplicated routing decision is gone.
- **Added:** `CompressionPolicy::lossless_transforms`, and `compression_permitted()` now
  returns `false` for subscription mode.
- **Caught by a property test, and it was right:** the first version routed the
  reformatter for *all* restricted traffic, on the reasoning that a meaning-preserving
  transform cannot violate I10. `a_restricted_policy_never_modifies_generated_input`
  failed. Reflowing whitespace preserves the decoded meaning and **still changes the
  bytes a provider sees** — exactly the fingerprint-class disclosure
  `may_strip_accept_encoding` is off for. A subscription CLI serializes its JSON a
  particular way, and reflowed traffic is distinguishable from the same client running
  unproxied.
- **Design note:** the permission is now explicit per mode rather than inferred.
  PayAsYouGo and **OAuth** permit lossless work — the OAuth hazard is a modification
  exceeding the granted scope, and a meaning-preserving change cannot exceed a scope.
  **Subscription permits neither**, and `compression_permitted()` says so.
- **Design note:** that is the honest answer rather than a disappointing one. Every
  transform this crate has either rewrites content or reflows its bytes, and both are
  visible to a provider comparing proxied traffic against unproxied. Subscription mode
  buys safety by giving up compression. See `DECISIONS.md` D14.
- **Design note:** `Reformatter` declines rather than returning an unchanged block, so
  `validated_apply` never rebuilds a body for a zero-byte saving — which under invariant
  I1 would cost a cache miss to gain nothing.
- **Design note:** `Reformatter` refuses sacrosanct blocks. Whitespace in a signed
  thinking block is covered by the signature; removing it produces content the provider
  rejects as tampered-with, and that is not made safe by the change being lossless.
- **Design note:** the safety check now runs *before* the policy branch. Both branches
  lead to a transform that walks the content, so a restricted request is not exempt from
  being handed a pathological payload.
- **Known limitation:** the reformatter runs only on live-zone blocks the compressors
  would also have seen. A pretty-printed *frozen* message stays as it is, correctly —
  rewriting it would invalidate the cached prefix, which is the whole point.

## The compression pipeline: orchestrator, safety, reformats
**2026-08-03** · gap rows P3, P5, P6

- **Added:** `headroom_core::pipeline` — `Orchestrator`/`Routing` (P3),
  `reformats::{minify_json, tidy_lines}` (P5), and `safety::{check, Limits, Hazard}` (P6).
- **Design note, P3:** the routing decision used to live in the proxy as a private
  dispatcher beside the axum handler. That put a decision every consumer needs behind a
  crate nothing but the proxy depends on — the CLI reimplemented it, and the two could
  drift without anything failing. It lives in core now, so `headroom compress` and
  `POST /v1/messages` route identically by construction.
- **Design note:** `Routing` names *why* a block was declined. `transform_for` collapses
  three genuinely different outcomes into `None`, and an operator reading telemetry needs
  to tell "policy forbade it" from "nothing handles this type" from "the payload was
  hazardous" — the fixes are entirely different.
- **Design note:** policy is checked first, before detection or the safety scan. I10
  forbids lossy work on restricted traffic outright, so a restricted request should not
  pay for two analyses to reach a conclusion policy already determined. Tested by
  routing a payload that would *also* fail the safety check and asserting it reports the
  policy reason.
- **Design note, P6:** the safety check answers "should this run", not "is this valid".
  A payload that fails is forwarded **uncompressed** — the outcome the customer would
  have had with no proxy. Rejecting the request instead would break traffic that works
  fine today because this crate was cautious about it. That asymmetry is why the limits
  are generous: being wrong costs a missed compression, being absent costs a stall on
  the request path.
- **Bug caught in development:** the depth guard was gated on `ContentType::Json`, and
  500 nested brackets carrying no data **do not classify as JSON** — so the check was
  skipped on exactly the payload it exists for. Now it also runs on anything
  bracket-shaped that detection did not recognize, which is what an adversarial payload
  looks like. Still skipped for a log file.
- **Design note:** bracket depth is counted by *scanning*, not parsing — the payload
  this catches is precisely the one a recursive parser would blow the stack on. Brackets
  inside string literals are skipped, and the close-bracket count saturates rather than
  underflowing, since a `usize` subtraction on `]]]]` would report a depth of eighteen
  quintillion.
- **Design note, P5:** these are the only transforms restricted traffic ever gets. They
  remove *only* bytes carrying no information, so the decoded meaning is bit-identical
  and they are safe on every auth mode. Whitespace **inside a string is content** —
  collapsing it would be a lossy transform that had escaped its policy gate, so there
  are tests for an escaped quote inside a string and for value-equality after
  minification.
- **Bug caught in development:** `tidy_lines` emitted the empty element `split('\n')`
  leaves on a trailing newline, so it *added* a blank line to every input that ended
  properly — a tidier that grew what it was asked to shrink.
- **Design note:** blank runs collapse to one, not to none. A blank line is a paragraph
  boundary; removing it reflows a log or document into a wall of text, losing structure
  a reader and a model both use. Leading whitespace is never touched, since indentation
  is structure in code, YAML and stack traces.
- **Known limitation:** nothing calls the orchestrator yet. The proxy still uses its own
  `Compressors::route`, so the duplication P3 exists to remove is still present until a
  follow-up switches the call site over.
- **Known limitation:** the reformats are not wired into any compressor chain. They are
  the lossless half that restricted traffic should receive, and until they are wired,
  subscription and OAuth traffic still gets no compression at all.
- **Known limitation:** gap row P4 (offloads) is not filed as done. Moving bulky
  sub-values to CCR and leaving markers is what `SmartCrusher`, `LogCompressor`,
  `SearchCompressor` and `DiffCompressor` already do; a separate offload layer would be
  a second name for the same mechanism.

## The OpenAI Responses API
**2026-08-03** · gap rows X7, X12

- **Added:** `Dialect::OpenAiResponses`, Responses-item handling in `compress_dialect`,
  and `sse::responses` — `ResponsesEvent`, `ResponsesObserver`, `Phase`.
- **Verified through the release binary:** a 10,464-byte `/v1/responses` request reached
  the provider as **698 bytes** — 93.3% smaller, where before it was forwarded untouched.
- **Design note, and the reason this is not just a field rename:** the Responses API
  carries a tool result as a standalone item with **no `role` at all** —
  `{"type":"function_call_output","call_id":...,"output":"..."}`. Read through the
  chat-completions path it has no content and no recognized kind, so the bulkiest thing
  in a Responses conversation would be forwarded uncompressed while everything looked
  like it was working.
- **Design note:** the rewrite writes back to `output` and leaves `call_id` alone. The
  provider matches a result to its call by `call_id`; losing it turns a compressed tool
  result into an orphan the model cannot attribute — which surfaces as the model
  ignoring its own tool call.
- **Design note:** a `function_call` is never compressed. It is the *request* to run a
  tool, and its `arguments` are JSON the provider parses — compressing them produces a
  call the provider rejects rather than a shorter one.
- **Design note:** a body carrying **both** `messages` and `input` is forwarded
  untouched. Picking one and rewriting it would leave the other alone, and the two would
  then disagree about what the conversation contains — a corruption the provider acts on
  rather than rejects.
- **Design note, SSE:** Responses events are a dotted namespace that grows with the API
  (`response.output_item.added`, `response.function_call_arguments.delta`). Matching the
  full string against a fixed list makes every event added tomorrow unrecognized;
  matching only the last segment collapses `output_text.done` and
  `reasoning_summary_text.done` into one. Both parts are kept — the stem says what the
  event is about, the suffix says what happened to it.
- **Design note:** a **reasoning summary is not output text**. Both arrive as deltas, and
  counting them together inflates any measurement of how much the model actually said
  while hiding how much was spent thinking — the same reason `signature_delta` is not
  counted as prose in the Anthropic observer.
- **Design note:** `response.output_text.done` repeats the whole text, so it is not
  counted as a delta. Counting it would double every measurement of output length.
- **Design note:** `failed`, `incomplete` and `cancelled` are terminal *and* not
  successes. A stream that failed is still over, and reporting it as unfinished would be
  as wrong as reporting it as complete.
- **Known limitation:** `ResponsesObserver` is not attached to the relayed stream.
  `ObservingStream` still models only the Anthropic vocabulary, so Responses streams
  relay correctly and report nothing.
- **Known limitation:** `"input": "just a prompt"` — a plain string rather than an item
  array — is forwarded untouched. There is nothing to compress in it, but it also means
  the string form never benefits.

## Volatile-content detector — and the logging that was going nowhere
**2026-08-03** · gap row X17, and a real defect in X20

- **Added:** `volatile` — `scan`, `Finding`, `VolatileKind`, wired into the
  `/v1/messages` request path.
- **Fixed, and this is the important part:** the proxy binary **installed no tracing
  subscriber**. `tracing` macros compile to nothing observable without one, so the
  startup line, the request log added with X20, and every warning in the crate were
  being silently discarded. Every call succeeded, so nothing in a test or a review
  revealed it — it showed up only as a process that runs and says nothing. The X20
  request log was reported as done in an earlier entry and was in fact emitting nothing.
- **Verified through the release binary** after the fix: the volatile warning fires with
  both findings, and at `HEADROOM_LOG=headroom_proxy=info` the request log reads
  `relaying upstream path="/v1/messages" bytes=111 auth=Some(Redacted(Bearer sk-an...))`
  — twelve visible characters, secret absent.
- **Design note, and the whole reason the module is shaped this way:** it **only
  reports**. There is no function returning modified content. The reference records that
  the original implementation tried to *fix* volatility by rewriting the value, and that
  was the defect — it changes what the model is told without asking, modifies the cache
  hot zone that invariant I2 forbids touching, and busts the cache itself on the turn it
  takes effect. A human decides whether a timestamp in their system prompt is worth
  removing.
- **Design note:** only the hot zone is scanned. Volatile content in the *live* zone is
  expected and harmless — it was never cached, so nothing is invalidated by it changing
  — and reporting it would bury the findings that matter under noise from every request
  carrying a fresh tool result.
- **Design note:** a counter is recognized from its *field name*, not its value. A bare
  `47` could be anything, and flagging every integer in a tool schema would make the
  report useless; `"turn_number": 47` says it increments.
- **Design note:** the timestamp matcher is anchored on the `YYYY-MM-DD` shape rather
  than on digits-and-dashes, because `claude-opus-4-20250514` is not a timestamp and
  flagging it would make every system prompt naming a model a finding. Likewise the hex
  matcher has a 16-character floor, so `deadbeef` and `cafe` do not trip it. A report
  with false positives is one people stop reading, and then the real finding arrives and
  nobody looks.
- **Design note:** samples are truncated. Findings go into log lines, and a system
  prompt is customer content that should not be reproduced wholesale somewhere it will
  be aggregated and retained.
- **Design note:** the default log level is `warn`, not `info`. The proxy logs a line
  per request, and a default that fills a terminal with one line per API call is a
  default people turn off entirely — taking the warnings with it.
- **Known limitation:** detection is pattern-based. A volatile value that looks like
  ordinary prose — a rotating quote of the day, a changing user name — is not detected.
- **Known limitation:** the scan runs on the Anthropic route only; the OpenAI handlers
  do not call it.

## Tokenizer registry
**2026-08-03** · gap row T4

- **Added:** `tokenizer::registry` — `Family`, `Registry`.
- **Design note, and the reason the module is worth having before T2/T3:** resolution
  succeeds trivially for models this build knows about. What decides whether a registry
  is *safe* is what it does for a model it has never seen — and that case is not rare,
  because a provider ships a new model well before this crate is rebuilt.
- **Design note:** `for_model` returns a tokenizer for every input, including an empty
  string. There is no `Option`, because there is nothing useful a caller could do with
  `None`: invariant I5 validates every compression against a token count, so a caller
  without one must either skip compression entirely or skip the check. Both are worse
  than an approximation documented never to under-count.
- **Design note:** over-counting costs a missed compression — visible, cheap,
  self-correcting. Under-counting means a payload that *grew* is measured as having
  shrunk, so I5's safety net passes something that made the request more expensive,
  silently. A test asserts the fallback never returns a suspiciously low count.
- **Design note:** classification keys on *family*, not model identifier. A point
  release rarely changes the tokenizer, and keying on the exact name means every new
  release date is an unknown model that falls back.
- **Design note:** matching is on substrings, because identifiers arrive in several
  shapes for one model — `claude-opus-4-20250514`, `anthropic/claude-sonnet-5`,
  `bedrock:anthropic.claude-opus-4`. An exact-match table would treat all but one
  spelling as unknown.
- **Design note:** the o-series match requires `o` followed by a *digit*. A bare
  `starts_with('o')` would classify `openhands`, `olmo` and `orca` as OpenAI models and
  hand them a tokenizer built for a different vocabulary. Tested in both directions.
- **Design note:** `is_exact` lets a caller report "counted exactly" versus "counted
  approximately" rather than presenting an estimate as a measurement.
- **Design note:** registering twice for one family *replaces*. Two tokenizers for one
  family is a configuration mistake, and silently keeping the first makes the second
  call look like it did nothing. Entries are sorted, so two identically configured
  registries describe themselves identically whatever order they were built in.
- **Known limitation:** no exact tokenizer is registered yet — every model resolves to
  the heuristic estimator. T2 (tiktoken BPE) and T3 (HuggingFace) are still open, and
  until one lands `is_exact_for` correctly answers `false` for everything.
- **Known limitation:** nothing in the proxy consults the registry; `compress_request`
  still constructs a `HeuristicEstimator` directly. Wiring it needs the model
  identifier threaded from the request body, which is a separate change.

## CLI: perf, deploy, update
**2026-08-03** · gap rows L4, L9, L11

- **Added:** `headroom perf`, `headroom deploy`, `headroom update`.
- **Measured on this machine:** 200-record payload, 13,691 bytes, **477 µs per call**
  and 28.7 MB/s. Against a provider round trip of hundreds of milliseconds that is
  invisible, which is the question `perf` exists to answer.
- **Design note:** `perf` measures the *compressor*, not the proxy. A round trip to a
  provider is hundreds of milliseconds and compression is microseconds, so an
  end-to-end latency number would report the network and bury the only figure this
  program controls.
- **Design note:** `perf` discards a warm-up pass. The first iteration pays for
  allocator growth and branch prediction that later ones do not, so including it reports
  a throughput the machine never actually sustains.
- **Design note:** `deploy` **prints** manifests rather than starting anything. A deploy
  that daemonizes a process owns stopping it, restarting it on boot and rotating its
  logs — and does all three worse than the service manager already on the machine.
  Printing works the same on a host with no root.
- **Design note, and the one with a test:** the compose service publishes on
  `127.0.0.1:PORT:PORT`, never a bare `PORT:PORT`. The proxy forwards provider
  credentials, so a template that binds every interface reintroduces exactly the
  open-relay mistake `Config::default` exists to avoid — in a file people copy without
  reading. Three tests cover it, plus one asserting no credential-shaped string appears
  in any manifest, since deployment templates get pasted into shared docs.
- **Design note:** `deploy` falls back to the bare binary name rather than guessing an
  install prefix. A wrong absolute path in a unit file fails at boot, hours after anyone
  was watching.
- **Design note:** `update` reports the version and where to get a newer one; there is
  **no self-replacing upgrade**. Doing it safely needs signature verification against a
  key this program would have to ship and rotate; doing it unsafely turns any compromise
  of the release host into arbitrary code execution on every install. A binary that
  already holds provider credentials is the wrong one to give that capability.
- **Known limitation:** `headroom learn` (L10) is not implemented. It mines *failed
  sessions*, and there is no session-log format for it to read — inventing one to have
  something to mine would be building the easy half of a feature.
- **Known limitation:** `headroom init` (part of L13) is not implemented; `inspect` and
  the introspection half already exist.
- **Known limitation:** `perf` measures one payload shape. A machine that is fast on
  record arrays and slow on prose would not show the difference here.

## Test infrastructure: simulators, invariant gates, fixtures, property tests
**2026-08-03** · gap rows E1, E2, E3, E4

- **Added (E1):** `headroom-simulators` — `Simulator`, `Recorder`, `Reply`. A real
  loopback server that records the exact bytes it received.
- **Added (E2):** `crates/headroom-proxy/tests/invariants.rs` — I1 through I4 asserted
  **end to end through a real proxy talking to a real socket**, not against
  `compress_request`.
- **Added (E3):** `headroom_simulators::fixtures` — ten SSE corner cases, each naming
  the specific defect it guards against.
- **Added (E4):** `crates/headroom-proxy/tests/properties.rs` — 8 property tests over
  generated input.
- **Design note, and the reason E2 exists at all:** `compress_request` was already
  unit-tested for all four invariants. What those tests cannot show is that the property
  survives the *relay* — header rebuilding, hyper's framing, chunked transfer encoding
  and the `Cow` passthrough all sit between the pure function and the provider, and a
  regression in any of them breaks the guarantee while every unit test stays green.
- **Design note:** a real server on a real socket rather than a mock. The proxy's central
  claim is about bytes crossing a network boundary, and a mock that stands in for the
  transport asserts everything except the thing under test.
- **Design note:** `i2_the_hot_zone_survives_a_compressed_request_unchanged` asserts
  compression *actually happened* before checking the hot zone. All four invariants are
  satisfiable by doing nothing, so there is also a test that the proxy is not passing
  them that way — it requires a >50% reduction.
- **Design note:** `i4_holds_across_separate_proxy_instances` sends two requests through
  one warm state and compares against a cold one. If they differ, accumulated CCR state
  is leaking into the output and a recorded hash stops being a property of the request.
- **Design note:** the property generator is a **fixed-seed xorshift**, not a
  clock-seeded harness. A failure reproduces from the test name alone. The trade is real
  — a random seed explores more over many runs — but a flaky test that cannot be
  reproduced gets disabled rather than fixed, and this project's whole thesis is that
  non-reproducible behavior is the expensive kind.
- **Design note:** one generator emits uniformly random bytes and another emits
  *fragments of SSE syntax*. Random bytes almost never form a `data:` line, so they
  exercise the rejection path and little else; a state machine has states to get wrong
  only when it is fed something that looks like input.
- **Design note:** every fixture is parsed at **every byte offset** and again **one byte
  at a time**, and must yield identical events. A chunk boundary is a network artifact
  and must not change what the parser sees.
- **Design note:** `compression_never_errors_on_arbitrary_bodies` asserts the output is
  either the input verbatim or valid JSON — never something that is neither. That is the
  property that makes `compress_request` safe on the request path, and it is worth
  checking rather than trusting the signature.
- **Design note:** simulators bind port 0. A fixed port turns concurrent tests into an
  intermittent bind failure that looks like a flake in whichever test lost the race.
- **Known limitation:** the simulators answer every path with one canned reply. A test
  needing different responses per path builds its own router; `strict_router` is
  provided for the case where the path itself is what needs asserting.
- **Known limitation:** the fixtures cover framing and event vocabulary, not provider
  behavior — no rate-limit headers, retries, or connection resets.

## CLI: wrap, unwrap, and savings
**2026-08-03** · gap rows L5, L6, L7, L8, L12

- **Added:** `headroom wrap <agent>`, `headroom unwrap <agent>`, `headroom savings`, and
  a `wrap` module covering claude, codex, cursor, aider, cline, continue, goose and
  openhands.
- **Verified through the release binary:** a settings file with deliberately unusual key
  order was wrapped and then unwrapped, and came back **SHA-256 identical**, with the
  backup removed.
- **Design note, and the reason the module is shaped this way:** wrapping is easy —
  change a base URL. The part that has to be right is *undoing* it. An `unwrap` that
  leaves an agent half-configured breaks the customer's tooling in a way they will
  attribute to their agent rather than to this program, and they will debug it in the
  wrong place. So the backup holds the **original bytes of the whole file** and restore
  writes those bytes verbatim. Reconstructing the original by reversing each edit sounds
  equivalent and is not: it rewrites formatting, reorders keys, and drops anything the
  writer did not understand.
- **Design note:** wrapping twice **refuses**. The second wrap would capture an
  already-wrapped file, and unwrap would then restore the customer to the wrapped state
  while reporting success — leaving them permanently routed through a proxy they thought
  they had removed.
- **Design note:** the backup is written *before* the original is touched, and removed
  only *after* the restore succeeds. A rewrite that cannot be undone is worse than one
  that never happened, and a backup deleted first with a failing write leaves neither
  version.
- **Design note:** `unwrap` on something never wrapped is a no-op that says so, not an
  error. The state the caller asked for is the state they already have.
- **Design note:** exports are printed rather than written to a shell profile. A profile
  belongs to its owner — appending means guessing which of `.bashrc`, `.zshrc`,
  `.profile` or a fish config is live, editing a file the customer maintains by hand, and
  owning the removal forever.
- **Design note:** OpenAI-shaped agents get a base URL ending in `/v1`; Anthropic-shaped
  ones do not. Getting that backwards produces `/v1/v1/chat/completions`, which fails as
  a 404 that looks like the proxy is broken.
- **Design note:** Cursor reports as unsupported rather than printing exports that would
  do nothing. The customer would otherwise believe they were routed through the proxy,
  see no savings, and have nothing to explain why.
- **Design note:** `savings` reports "no data yet" rather than "0.0%" before anything is
  measured — zero is indistinguishable from a compressor that has stopped working, which
  is the one thing this report exists to reveal. It also parses `headroom_requests_total`
  without matching the `# HELP` line that shares its prefix, with a test for that.
- **Design note:** no currency figure. It needs a per-model price this program does not
  have and cannot keep current, and a wrong number about money is worse than no number.
  A test asserts no currency symbol appears.
- **Known limitation:** a settings file must be supplied with `--settings`; no agent's
  config path is discovered automatically. Guessing a path and rewriting whatever is
  there is not something to do on the strength of an assumption about someone else's
  tool layout.
- **Known limitation:** the settings rewrite sets a `base_url` member and assumes JSON.
  Agents using TOML or a differently-named key need per-agent handling.
- **Known limitation:** `deploy` (L4), `perf` (L9), `learn` (L10), `update` (L11) and
  `init`/`tools` (L13) remain unimplemented.

## Cross-agent memory and shared context
**2026-08-03** · gap rows Y1, Y2, Y3

- **Added:** `headroom_core::memory` — `MemoryStore`, `Memory`, `Provenance`,
  `inject_block`, and `SharedContext`.
- **Design note, and the constraint the module is built around:** memory never goes in
  the system prompt. The system prompt heads the cached prefix, and a memory store is
  *designed* to change — so an agent that learns one fact per turn would invalidate the
  cache every turn, paying full price for the entire conversation each time in exchange
  for a sentence. Memory goes in the live-zone tail (invariant I2), where it costs a few
  tokens on a message that was never cached and invalidates nothing.
- **Design note:** `inject_block` renders context, not instructions. Phrasing memory as
  a directive would invite a caller to put it where directives go — the system prompt —
  which is the one place it must not be. A test asserts the block contains no "you
  are"/"you must" phrasing.
- **Design note:** dedup is content-addressed and exact. Content is whitespace-normalized
  before hashing, so two agents writing the same sentence with different trailing
  whitespace produce one entry — a content-addressed store that misses exact duplicates
  has failed at its only distinctive job. The *stored* text is the original; normalization
  computes a key, it does not edit what the agent said. A balancing test asserts
  genuinely different facts stay apart.
- **Design note:** provenance is a list, not a single value. When two agents
  independently record the same fact, that agreement is the most useful signal the store
  has — and one agent repeating itself five times is one observation seen five times,
  not five observations. `corroborated()` distinguishes them, and recall ranks on it.
- **Design note:** the store is a `BTreeMap` keyed on the content hash, and recall
  tie-breaks on that key rather than relying on sort stability. Recall must be
  deterministic (I4); a non-deterministic injection order would bust the very cache this
  module is careful about.
- **Design note:** `SharedContext` keys are namespaced with a unit separator rather than
  `:` or `/`, both of which appear in real keys. Without it, `put("a", "b/c")` and
  `put("a/b", "c")` collide — and a silent overwrite between two agents is far harder to
  diagnose than a missing key. There is a test for the collision and one for a namespace
  that is a prefix of another.
- **Known limitation:** nothing in the proxy populates or injects from a `MemoryStore`.
  The injection block is built to append to the live-zone tail exactly as the verbosity
  note does, so the wiring is a small follow-up, but it is not done.
- **Known limitation:** the store is in-memory only. Nothing persists across a restart,
  which for a *cross-agent* memory store is a real gap rather than a detail — it works
  within a process, not between runs.
- **Known limitation:** no eviction. A long-lived process accumulates facts without
  bound; `recall(limit)` bounds what is *injected*, not what is stored.

## Byte-faithful member insertion — cache key and effort now reach the provider
**2026-08-03** · closes gap row X16, completes O2

- **Added:** `body::insert_top_level_member`, which adds a member to a JSON object
  while preserving every existing byte in its original order.
- **Wired:** `prompt_cache_key` (X16) and `reasoning_effort` (O2) into
  `POST /v1/chat/completions`, both through the new primitive.
- **Design note, and the reason the primitive exists:** adding one member via
  `serde_json::Value` means deserializing and re-serializing the whole body — rewriting
  every byte the customer sent in order to change none of them. Even with
  `preserve_order` and `arbitrary_precision` set, whitespace and string-escape choices
  become the serializer's rather than the customer's. That is what invariant I1 forbids,
  and it costs a cache miss on every request. Inserting immediately after the opening
  brace preserves the original bytes exactly. Tested against a pretty-printed body, a
  body containing `1.0` and an integer past 2^53, and by asserting the tail of the
  output is literally the tail of the input.
- **Design note:** the cache key is derived from every message *but the newest*. The key
  names a cache partition and has to be identical across the turns of one conversation,
  or every turn lands in a fresh partition and nothing is reused — worse than sending no
  key at all, since it also fragments the provider's own automatic prefix cache. The
  newest message is the one thing that changes every turn. Two tests: one that the key
  survives a changing newest turn, one that different conversations still differ.
- **Design note:** neither member is ever replaced when the customer set it. A
  `prompt_cache_key` partitions their cache deliberately; a `reasoning_effort` is a
  deliberate choice about answer quality, and overriding it is not a compression
  decision.
- **Design note:** an empty object gains no trailing comma. `{"k":v,}` is not JSON, and
  the naive implementation produces exactly that.
- **Known limitation:** effort is injected on the OpenAI route only. Anthropic's
  equivalent is `thinking.budget_tokens`, and *enabling* extended thinking on a request
  that did not ask for it changes the response shape — the client starts receiving
  thinking blocks it does not expect. Adjusting an existing `thinking` block would be
  safe; adding one is not, so neither is done yet.

## Output shaping and observation-only telemetry
**2026-08-03** · gap rows O1, O2, N1, N2, N3

- **Added:** `headroom_core::output_shaping` — `Verbosity`, `Effort`, `route_effort`,
  `verbosity_append` — and wired verbosity into the proxy behind
  `HEADROOM_OUTPUT_SHAPER`.
- **Added:** `headroom_core::telemetry` — `StructureHash`, `AggregationKey`, the
  `Telemetry` trait, `Aggregator`, and `Recommendations`.
- **Design note, and the reason the module exists:** the obvious place for a terseness
  instruction is the system prompt, and that is the one place it must not go. The system
  prompt is the first thing in the cached prefix, so a note there invalidates the whole
  cache on every subsequent request — saving a couple of hundred output tokens while
  re-billing tens of thousands of cached input ones, and moving the metric people watch
  in the wrong direction invisibly. The note goes in the live-zone tail instead, which
  costs a few tokens on an already-uncached message and invalidates nothing.
- **Design note:** shaping runs *after* compression and appends to the compressor's
  output rather than to the original block, since both want the same block when the
  newest message is a bulky tool result. Appending to the original would silently throw
  the compression away. There is a test that measures the ratio to catch exactly that.
- **Design note:** the note is not appended twice. An agent loop calls this every turn,
  and without the guard a long session accumulates the same instruction a dozen times —
  wasted tokens, and a worse prompt, since a repeated instruction reads as emphasis.
- **Design note:** effort and verbosity are separate dials. Conflating them produces
  short wrong answers, which is the failure mode that makes users switch an
  output-shaping feature off entirely — a terse answer to a hard question still needs
  the reasoning.
- **Design note:** effort routing checks error signals *before* routine ones. "thanks,
  but it still errors" is a recovery, not a pleasantry, and testing the routine list
  first would route the turn that most needs thought as the cheapest one.
- **Design note:** output shaping is off unless explicitly enabled. It changes what the
  model *writes*, which is a visible change to the customer's product rather than an
  invisible saving — a proxy that quietly made every answer terser would be editing
  someone's application on their behalf.
- **Design note, telemetry:** there is **no** `Telemetry::hint_for(&request)`, and the
  absence is the design. A request-time hint API breaks invariant I4 immediately — the
  same request would compress differently depending on what happened to be observed
  before it, and a failure could not be reproduced from the failing request alone. The
  loop closes at startup instead, through `Recommendations`. A test asserts structurally
  that every trait method returns `()`.
- **Design note:** observations are keyed by *structure*, not content. Two tool results
  listing different files have identical structure and should aggregate together — and
  since every value is discarded before hashing, a key cannot be reversed into what
  anyone sent. There is a test that a payload containing an API key and an email hashes
  identically to a benign one of the same shape.
- **Design note:** FNV-1a rather than `DefaultHasher`, whose output may change between
  compiler releases — a recommendations file written by one build would key differently
  under the next.
- **Design note:** array length does not affect the fingerprint, so a 10,000-record
  payload aggregates with a 10-record one. A separate test asserts genuinely different
  shapes still hash differently, since a fingerprint that collapses everything
  aggregates perfectly and learns nothing.
- **Design note:** a corrupt recommendations file yields an empty set rather than an
  error. The file is an optimization; refusing to boot without a valid one turns a cache
  of statistics into a hard startup dependency.
- **Design note:** an unmeasured shape is worth attempting. Skipping it would never
  gather the data that would let it be skipped for a reason.
- **Known limitation:** `Effort` is computed and mapped to both provider dialects but
  nothing writes it into an outgoing request. Doing so means adding a top-level member
  to the body, which needs the same byte-faithful surgical insert that `prompt_cache_key`
  is waiting on.
- **Known limitation:** nothing in the proxy feeds the `Aggregator`, and nothing reads a
  `recommendations.json` at startup. Both are implemented and tested as a library; the
  wiring is outstanding.
- **Known limitation:** recommendations are published as JSON rather than the
  `recommendations.toml` the gap row names — `serde_json` is already a workspace
  dependency and `toml` is not, and adding one for a file nothing outside this repo
  reads was not worth it.
- **Known limitation:** effort routing is English-keyword based, like the rest of the
  signals work.

## Operational guards and runtime configuration
**2026-08-03** · gap rows X20, F5

- **Added:** `guard::is_self_referential` and a startup check — the proxy refuses to
  start when its upstream is its own listen address.
- **Added:** `guard::RateLimiter`, wired into the relay. 600 requests/minute, well above
  any real workload.
- **Added:** the request log — path, byte count, and the `Authorization` prefix.
- **Added:** `POST /admin/runtime-env` and a runtime override layer in `config`.
- **Verified through the release binary:** a self-referential upstream refuses to start
  with `upstream http://localhost:8795 is this proxy's own listen address
  (127.0.0.1:8795); every request would forward to itself`; and a live proxy had
  compression turned off over the admin endpoint and forwarded the next 10,420-byte
  request uncompressed, without a restart.
- **Design note:** the admin endpoint is gated on the peer address being loopback,
  checked in the handler rather than relied on from the default bind. It can change
  `HEADROOM_UPSTREAM`, so anyone who can reach it can point the proxy at a server they
  control and every subsequent request carries the customer's credential there. That
  makes it a credential-exfiltration primitive, not merely a configuration surface — and
  a control that only holds under the default configuration is not a control.
- **Design note:** a request with no connection information is refused rather than
  allowed. The handler cannot establish the caller is local, and being able to is the
  endpoint's entire protection.
- **Design note:** the endpoint echoes applied *names* and never values. Configuration
  can carry an upstream URL with credentials in it, and an endpoint that reflects what
  it was given is the easiest way for one to reach a log. Names outside `HEADROOM_*` are
  ignored, so this is a way to retune the proxy rather than a general lever on the
  process.
- **Design note:** the rate limit answers **429, not 503**. A provider SDK already knows
  how to back off and retry a 429; several read 503 as "the service is broken" and give
  up, turning a momentary limit into a failed request. A test asserts the refused
  request never reaches the provider — a limiter that forwarded and then reported 429
  would protect nothing.
- **Design note:** `RateLimiter::available()` refills before reading. A reader that
  reported the stored count would say "0 tokens left" for a bucket whose window elapsed
  an hour ago and which will admit the very next request — wrong exactly when someone is
  looking at it to find out whether the limiter is the problem. Caught by a test, not by
  review.
- **Design note:** loop detection treats `localhost`, `127.0.0.0/8` and `::1` as the same
  socket. Checking only the literal string would catch the least likely spelling of the
  mistake. Startup rather than per-request, because the header-based alternative is a
  fingerprint leak — `DECISIONS.md` D11.
- **Design note:** runtime overrides live in an `RwLock` map rather than being written
  to the process environment. `setenv` races `getenv`, and this proxy reads its config
  per request from a thread pool — see `DECISIONS.md` D10.
- **Known limitation:** the rate limit is process-wide, not per client. The proxy binds
  loopback and fronts one credential, so the total rate reaching the provider is the
  thing worth bounding — but a shared deployment would need per-caller buckets.
- **Known limitation:** the limit is a compile-time constant. It is a backstop against a
  runaway retry loop rather than a quota, and it is not yet configurable.
- **Known limitation:** overrides are process-local and lost on restart, which is
  intentional for an incident lever but means they are not a configuration store.

## OpenAI routes
**2026-08-03** · gap rows X6, X7, X8, X11

- **Added:** `POST /v1/chat/completions` and `POST /v1/responses`, compressed; and
  `POST /v1/conversations` (including sub-paths) and `POST /v1/responses/compact`,
  relayed byte-identical.
- **Added:** `compression::Dialect`, so the OpenAI routes reuse the whole existing
  pipeline rather than getting a parallel implementation that would drift.
- **Added:** `sse::openai` — `OpenAiEvent`, `OpenAiObserver`, `classify_openai`.
- **Design note, and the important one:** the two providers need *different* frozen
  floors. Anthropic caches what the customer marks, so the floor comes from their
  `cache_control` breakpoints and no markers legitimately means nothing is pinned.
  OpenAI caches prompt prefixes automatically — nobody asks for it — so applying the
  Anthropic rule reads "no markers, so nothing is frozen", which is exactly backwards.
  The OpenAI floor is every message but the newest. There is a regression test with a
  bulky older message and a trivial newest one, which the Anthropic rule would have
  happily compressed.
- **Design note:** OpenAI carries a tool result as a whole message with `role: "tool"`
  and a plain string body, where Anthropic nests a typed block inside a user message.
  Read as ordinary text it falls outside the live zone, so the bulkiest thing in an
  OpenAI conversation would never be compressed at all.
- **Design note:** `data: [DONE]` is checked *before* JSON parsing. It is not JSON, so a
  parse-first reader classifies the one frame that says the stream ended cleanly as
  malformed — and every stream then looks unterminated. Tested across every byte split,
  since a six-character sentinel lands on a chunk boundary easily.
- **Design note:** OpenAI tool calls arrive in fragments — the first chunk carries the
  name and `id`, later ones carry slices of the argument JSON. Counting `tool_calls`
  entries per chunk reports one call as five, so calls are tracked by index. A second
  test asserts genuinely parallel calls are still counted separately, so the fix cannot
  become an over-correction.
- **Design note:** `"content": null` marks a chunk carrying only a tool-call fragment.
  Treating it as empty prose would report text output that never existed.
- **Design note:** `/v1/conversations` and `/v1/responses/compact` are passthrough
  *on purpose*, not unimplemented. Both describe conversation state rather than carrying
  a prompt, so compressing one corrupts the provider's own record of the conversation
  rather than merely shortening a message. The passthrough handler reads the path from
  the request, so `/v1/conversations/{id}/items` is not collapsed to its route prefix.
- **Known limitation:** `/v1/responses` uses `input` rather than `messages`, which
  `FaithfulBody` does not model, so it relays untouched today. Wired and tested as a
  route; the body shape (X7) is still to come.
- **Known limitation:** `prompt_cache_key` injection (X16) is deliberately *not* applied.
  `stabilization::inject_prompt_cache_key` works on a `serde_json::Value`, so using it
  means re-serializing the whole body — rewriting every byte the customer sent in order
  to change none of them, which is precisely what invariant I1 forbids. Doing it
  faithfully needs a surgical insert against the raw bytes.
- **Known limitation:** `OpenAiObserver` exists and is tested but is not attached to the
  relayed stream; `ObservingStream` still only models the Anthropic vocabulary, so
  OpenAI responses relay correctly and report nothing.

## Streaming traffic is compressed and observed
**2026-08-03** · gap row X18, completes X19

- **Fixed:** `compress_request` no longer bails out on `"stream": true`. The reasoning
  behind that bail-out — that compressing a stream would mean buffering it — conflated
  two different bodies. The *request* body arrives complete before compression runs,
  whatever the client wants the *response* framing to look like. Streaming is the
  common agent case, so the bail-out exempted most real traffic from compression while
  every test kept confirming compression worked.
- **Added:** `observe::ObservingStream`, wrapping the relayed response so SSE frames are
  parsed as they pass. Every byte it yields is the byte it received (invariant I9).
- **Added:** cache usage to `AnthropicEvent::MessageStart`, which is where the provider
  reports what the prompt cache did. `headroom_cache_hit_rate` now has data.
- **Verified through the release binary against a streaming provider:** a 10,450-byte
  `"stream": true` request arrived upstream as **595 bytes** — 94.3% smaller, where
  before it was forwarded untouched — with frames reaching the client at +0.01s, +0.41s
  and +0.81s rather than all at once, and `headroom_cache_hit_rate 0.9000` afterwards.
- **Design note:** observation is a stream wrapper rather than a callback after the
  fact, because the two numbers that matter sit at opposite ends of the response.
  `message_start` carries the cache usage and is the *first* frame; `message_delta`
  carries output tokens and arrives near the last. Waiting for the response to finish
  before reading either would mean buffering it.
- **Design note:** telemetry is also recorded on `Drop`, not only on clean termination.
  A client that cancels mid-generation drops the stream rather than exhausting it, so
  `poll_next` never returns `None` — and cancellation is routine for an interactive
  agent, not an edge case. A flag keeps a cleanly-ended stream from being counted twice.
- **Design note:** `message_start` nests `usage` under `message`, unlike `message_delta`
  which puts it at the top level. Reading the wrong one yields `None` on every real
  stream, and a permanently empty cache metric reads as "no traffic" rather than as a
  defect. There is a test for each nesting, so a lenient reader that accepted either
  cannot pass.
- **Design note:** cache tokens accumulate across `message_start` events rather than
  being assigned. One connection can carry more than one message, and the second frame
  would otherwise erase what the first reported.
- **Design note:** the observing stream is boxed internally so it is unconditionally
  `Unpin`. That keeps `poll_next` free of the unsafe pin projection this crate forbids,
  at one allocation per response against a network round trip.
- **Known limitation:** only the Anthropic event vocabulary is modelled, so OpenAI
  streams (X11, X12) relay correctly but report nothing. Unrecognized event types are
  logged rather than counted.
- **Known limitation:** `headroom_cache_hit_rate` is only populated by *streaming*
  responses. A non-streaming reply carries the same usage block in its JSON body, which
  nothing currently reads.

## The proxy actually proxies — upstream relay
**2026-08-03** · completes gap row X5, and clears the "does not forward upstream yet"
limitation carried since the `/v1/messages` handler first landed

- **Added:** `upstream` — `Upstream`, `RelayedResponse`, `RelayError`. Requests now
  reach a provider and the provider's answer reaches the client. Until this, the
  handler compressed a request and handed it straight back, which made the crate a
  compression library wearing a proxy's routing table.
- **Added:** `GET /metrics`, and the request path now feeds the counters that landed
  unwired in the previous change.
- **Added:** `HeaderPolicy::strip_accept_encoding`, wired to the auth-mode policy's
  `may_strip_accept_encoding`, which had been declared and never consulted.
- **Verified end to end through the release binary**, not only through the router: an
  11,034-byte request reached a local provider as 596 bytes — 94.6% smaller — with the
  `x-api-key` intact and the provider's response relayed back to the client.
- **Design note:** the response body is a stream, never buffered. For a JSON reply that
  is a few milliseconds; for SSE it is the entire feature, since buffering holds the
  model's whole answer until generation ends and then releases it at once. There is a
  test that keeps an upstream handler open for 30 seconds and asserts the first frame
  arrives anyway — it fails if anyone reintroduces buffering.
- **Design note:** `host` and `content-length` are stripped at the relay boundary
  specifically. They describe the client-to-proxy hop. A forwarded `content-length` is
  the more dangerous one: compression changed the body, so the client's length is now
  short, and an under-declared length truncates the request server-side — surfacing as
  the model answering a question that was cut off mid-sentence.
- **Design note:** upstream's `content-length` and `transfer-encoding` are dropped from
  the *response* too, because the server handing it to the client re-frames the body.
  Old framing headers beside new framing is how a response arrives truncated at exactly
  the length the stale header claimed. A test asserts the rate-limit headers survive,
  so the fix cannot quietly become "drop everything".
- **Design note:** a non-2xx upstream response is not an error. It is the provider's
  answer and is relayed unchanged — a 429 the client cannot see is a 429 it cannot back
  off from, which turns rate limiting into an outage.
- **Design note:** relay failures render in the provider's own `{"type":"error", ...}`
  shape and always as 502, never 500. The client is a provider SDK that knows how to
  parse that shape and nothing about this proxy; and 502 tells whoever is paged that the
  dependency failed rather than sending them to read proxy source.
- **Design note:** a failed relay error string carries the URL but never the headers,
  and a test asserts the credential does not appear in it. Error strings get logged,
  aggregated, and pasted into tickets.
- **Design note:** `AppState.upstream` is an `Option` rather than a startup failure. A
  proxy that refuses to boot when TLS initialization fails takes `/health` down with
  it, so nothing can report *why* it is down.
- **Known limitation:** only `POST /v1/messages` relays. The OpenAI routes (X6-X8) and
  WebSocket (X13) are still unimplemented, and the SSE observer from the previous change
  is still not attached to the relayed stream — so `headroom_cache_*` and
  `headroom_stream_errors_total` remain at zero in production.
- **Known limitation:** `"stream": true` requests relay correctly but are still
  forwarded uncompressed, because `compress_request` declines them. Streaming is the
  common agent case, so the traffic that matters most is passed through.
- **Known limitation:** no total-request timeout; see `DECISIONS.md` D9.
- **New dependency:** `reqwest` 0.13.3, `default-features = false`. See `DECISIONS.md`
  D8 for why, and why not `native-tls`.

## Cache stabilization primitives and proxy observability
**2026-08-03** · gap rows X14, X15, X16, X19

- **Added:** `stabilization` — `sort_keys`, `normalize_tools`, `place_cache_control`,
  and `inject_prompt_cache_key`. These are the transforms that make a request's prefix
  byte-stable across turns, which is what lets the provider's cache actually hit.
- **Added:** `metrics` — a `Metrics` counter set over atomics, rendered in Prometheus
  text exposition format.
- **Design note:** object keys sort recursively, arrays never do. An array is ordered
  data; reordering one changes meaning rather than presentation. The distinction is the
  whole reason this is a hand-written walk rather than a blanket canonicalization.
- **Design note:** `cache_control` breakpoints go on the *earliest* eligible messages,
  not the latest. The prefix before a breakpoint is what gets cached, so a marker placed
  late caches almost nothing. The newest turn is never marked — it is the live zone, and
  pinning it would freeze the only thing compression is allowed to touch.
- **Design note:** a customer-set `cache_control` marker suppresses automatic placement
  entirely rather than adding to it. Anthropic caps breakpoints at four; competing with
  a customer's own placement risks exceeding the cap and losing theirs.
- **Design note:** `cache_hit_rate()` returns `Option<f64>` and is `None` before any
  data, deliberately not `0.0`. A gauge reporting zero for "nothing yet" is
  indistinguishable on a dashboard from a cache that has completely stopped working —
  which is exactly the alarm this metric exists to raise.
- **Design note:** `inject_prompt_cache_key` never overwrites a customer-supplied key.
  The key partitions the provider's cache; overwriting one silently moves a customer's
  traffic to a different partition and cold-starts it.
- **Known limitation:** nothing calls either module from the request path yet. The
  stabilization transforms re-serialize the body, which is in direct tension with
  invariant I1's byte-faithful passthrough — applying them is only safe under a policy
  that permits proxy-visible modification, and deciding where that gate sits is its own
  change rather than a mechanical hookup. Shipped as tested primitives with the wiring
  called out as outstanding.
- **Known limitation:** `MAX_BREAKPOINTS` is hard-coded to 4, matching Anthropic's
  current cap. A provider with a different limit needs this parameterized.

## Auth-mode classification and policy gating
**2026-08-03** · gap rows A1, A2

- **Added:** `classify_auth_mode` and `CompressionPolicy`, and wired both into the
  proxy so compression aggressiveness is now decided by how a request authenticated
  (invariant I10).
- **Added:** `DiffCompressor` to the proxy's compressor dispatch.
- **Design note:** unrecognized auth classifies as `Subscription`, the *most*
  restricted mode. Misclassifying subscription traffic as pay-as-you-go applies
  aggressive compression to the account least able to afford the exposure; the reverse
  merely leaves tokens uncompressed. Not symmetric, so the uncertain path takes the
  safe side.
- **Design note:** every `CompressionPolicy` field is a permission, so all-false is the
  restrictive default and matches subscription mode. A policy nobody configured is not
  the permissive one.
- **Design note:** the policy gate lives at the dispatch point, not inside each
  compressor. Every compressor currently wired is lossy, so a restricted policy routes
  nothing — enforced once rather than trusted to each.
- **Bug caught in development:** `sk-ant-oat...` also starts with `sk-`, so testing the
  generic API-key prefix first classified OAuth tokens as pay-as-you-go and handed them
  the aggressive policy. Specific prefixes now precede general ones, with a regression
  test asserting the OAuth path forbids lossy transforms.
- **Known limitation:** prefix-based classification against Anthropic-shaped tokens
  only. Another provider's key format falls to `Subscription` and is under-compressed —
  the safe direction, but it means the classifier needs extending per provider.

## Text compression, persistent CCR, and the retrieval tool
**2026-08-03** · gap rows C10, R3, R5

- **Added:** `TextCrusher` (lossless whitespace normalization) and `TextSummarizer`
  (lossy line dropping). Split deliberately — the lossless pass is safe on the auth
  modes that forbid lossy transforms, so it must be a separate type (invariant I10).
- **Added:** `FileCcrStore`, a persistent CCR backend. Content survives a proxy
  restart, so a model asking for content it was promised is not told it is gone.
- **Added:** `ccr_retrieve` tool definition and handler — the mechanism that makes lossy
  compression reversible from the model's side.
- **Design note:** the retrieval tool must be registered on **every** request, not only
  when something was compressed. The tools array is part of the cached prompt prefix; a
  tool that appears and disappears invalidates the cache on every state flip. A fixed
  handful of tokens once beats a full cache miss each time compression starts or stops.
- **Design note:** `Retrieval` distinguishes expired from malformed, because the model
  should be told different things — one means "this existed and is gone", the other
  "check what you sent".
- **Design note:** the file store writes to a temporary name and renames into place, so
  a reader never observes a half-written entry and hands a model truncated content
  while calling it the original.
- **Fixed:** an MSRV violation caught by clippy — `is_none_or` is stable from 1.82 and
  this crate declares 1.80.
- **Known limitation:** `FileCcrStore` substitutes for the SQLite backend gap row R3;
  see `DECISIONS.md` D6. The Redis backend (R4) remains deliberately unimplemented.
- **Known limitation:** nothing injects the `ccr_retrieve` tool into outgoing requests
  yet. The definition exists and is tested; wiring it into the proxy's tools array is
  still open.

## Signals and the diff compressor
**2026-08-03** · gap rows S1-S3, C8

- **Added:** `signals` — keyword scoring and tiered line importance, factored out so
  every line-oriented compressor makes the same keep/drop judgment rather than
  re-inventing it slightly differently.
- **Added:** `DiffCompressor` — elides unchanged context, keeping hunk headers, every
  changed line, and two lines of surrounding context.
- **Design note:** every signal heuristic leans toward keeping a line. One wrongly kept
  costs a few tokens; one wrongly dropped may be the error being looked for.
- **Design note:** hunk headers are never elided. They carry the line numbers, and a
  diff without them cannot be located against a file.
- **Design note:** `keep_most_important` breaks ties on source index, so an all-routine
  input produces the same selection every run rather than depending on sort stability.
- **Added:** `DECISIONS.md`, logging choices taken autonomously — batching gap rows into
  PRs, skipping the Redis backend, heuristic rather than tree-sitter code compression,
  and deferring the Python bindings.
- **Known limitation:** signals are English-keyword based. A non-English log gets no
  keyword signal and falls back to structural cues alone.

## Live-zone compression on the wire — /v1/messages
**2026-08-03** · closes [#35](https://github.com/baileyrd/rusty_headroom/issues/35), [#36](https://github.com/baileyrd/rusty_headroom/issues/36)

- **Added (#35):** `frozen_message_count` — derives the live-zone floor from customer
  `cache_control` markers, on a message or on a content block, last breakpoint wins.
- **Added (#36):** `compress_request`, a pure function over bytes running the whole
  pipeline, plus `POST /v1/messages` and a content-type dispatcher over the three
  compressors.
- **Added:** `SmartCrusher` re-exported at the `headroom-core` root, matching
  `LogCompressor` and `SearchCompressor`.
- **The guarantee, tested:** on a request with system, tools, five historical turns
  and a bulky live tool result, the hot zone and every frozen turn come back
  SHA-256-identical while the live result shrinks by more than half. The frozen prefix
  is asserted to appear as a literal substring of the output, not merely to parse
  equal.
- **Design note:** unparseable input yields a floor that freezes *everything*, not
  nothing. Freezing too much costs some compression; freezing too little modifies a
  message the provider has cached, silently. The safe direction is not symmetric.
- **Design note:** passthrough is the fallback for every path — disabled, streaming,
  malformed, no live zone, compressor declines, result not smaller. There is no input
  for which `compress_request` errors; the worst case is that it does nothing.
- **Known limitation:** the handler does **not forward upstream yet.** It returns the
  transformed request. Relay needs the SSE state machine first, since a forwarding
  handler would have to buffer streaming responses and break what clients rely on.
- **Known limitation:** `"stream": true` forwards untouched, so the common agent case
  is currently uncompressed.
- **Known limitation:** a `tool_result` whose content is an array of blocks rather
  than a string reads as empty and is never compressed.

## Byte-faithful bodies and header hygiene
**2026-08-03** · closes [#33](https://github.com/baileyrd/rusty_headroom/issues/33), [#34](https://github.com/baileyrd/rusty_headroom/issues/34)

- **Added (#33):** `FaithfulBody` — parses a request while retaining the original
  bytes, so untouched `messages[*]` forward as exact copies. Passthrough returns
  `Cow::Borrowed`: the original bytes are handed back, not rebuilt and hoped to match.
- **Added (#34):** `sanitize` for upstream-bound headers, `HeaderPolicy`, and
  `Redacted` for credentials.
- **Invariant I1 is now testable and tested.** SHA-256 round-trip across
  pretty-printed input, unusual key order, `1.0`, integers past 2^53, CJK and emoji,
  and escaped strings.
- **Design note:** the workspace's `preserve_order` and `arbitrary_precision` flags
  are necessary but not sufficient. A `Value` round trip still normalizes whitespace
  and may differ on escapes, so untouched content is never round-tripped at all.
- **Design note:** `serde_json` cannot deserialize an object into an ordered sequence
  of raw pairs — `Vec<(K,V)>` wants an array, and `serde_json::Map` is fixed to
  `Value` and would re-serialize everything. A small `Deserialize` visitor collects
  members in document order with values left as `RawValue`.
- **Design note:** `Redacted` implements `Debug` *and* `Display` identically, both
  truncating. A `Debug` that dumped the full value would make `tracing::debug!(?x)` on
  any containing struct a credential leak on some error path nobody exercised.
- **Design note:** a client-supplied `X-Forwarded-For` is stripped when the policy
  forbids adding one — forwarding it would defeat the policy while technically
  honoring it.
- **Known limitation:** a body with an escaped top-level key cannot be borrowed and
  routes to verbatim passthrough. Safe direction, but such requests are never
  compressed.
- **Known limitation:** `HeaderPolicy` is supplied by the caller. Auth-mode
  classification, which decides it, is a separate unimplemented row (A1), so
  everything currently gets the quiet default.

## Proxy skeleton — config, /health, graceful shutdown
**2026-08-03** · closes [#32](https://github.com/baileyrd/rusty_headroom/issues/32)

- **Added:** `headroom-proxy` is now a real binary — axum server, `Config` from
  `HEADROOM_*` environment variables, `/health`, graceful shutdown on SIGTERM/SIGINT.
- **Design note:** the default bind is **loopback, not 0.0.0.0**. The proxy forwards
  provider credentials, and a default binding every interface would make an open
  credential relay the out-of-the-box behavior. Widening it is a deliberate act.
- **Design note:** configuration is read from the environment on every access rather
  than cached at startup, so an operator can turn compression off or repoint the
  upstream without a restart — and without dropping in-flight streaming responses.
- **Design note:** unparseable config values fall back to defaults rather than
  failing. A malformed `HEADROOM_PORT` should not take down a running proxy on its
  next config read.
- **Design note:** graceful shutdown matters here specifically because the proxy sits
  in the middle of streaming responses. Dropping one mid-flight truncates a model's
  output mid-token, which reaches the user as a corrupt answer rather than a
  retryable error.
- **Known limitation:** no provider routes yet. `/health` is the only endpoint —
  `/v1/messages` needs byte-faithful body handling to exist first, and standing it up
  before that would mean writing the request path twice.

## SearchCompressor — grep result sets
**2026-08-03** · closes [#29](https://github.com/baileyrd/rusty_headroom/issues/29)

- **Added:** `SearchCompressor`, a `Transform` + `LossyTransform` that groups matches
  under their file so each path is stated once instead of on every matching line.
- **Added:** `parse_match` and the `Match` type as public helpers.
- **Measured:** estimated token reduction of **66% on 100 matches across 20 files,
  71% on 100 across 10 files, 84% on 336 matches**.
- **Honest gap:** the reference reports **92%** on its 100-result code-search
  benchmark. This lands at 66-71% on comparable input. The difference is deliberate
  rather than a defect — the caps here keep 40 matches where the reference apparently
  keeps far fewer, trading ratio for fidelity. `SearchConfig::max_total_matches` makes
  it tunable, but the shipped default does not reach the reference's number.
- **Design note:** line numbers are preserved for every shown match. They are how the
  agent's next action gets targeted; losing them forces a re-search that costs more
  than was saved.
- **Design note:** match text is never truncated. Whole files are elided past a cap
  instead, because the matched line is the thing being searched for and the path
  repetition is the waste.
- **Design note:** file order follows the search output rather than being sorted, so
  whatever relevance ordering the tool applied survives.
- **Known limitation:** grouping reorders. An interleaved original ordering is lost —
  acceptable for search output, since ripgrep groups by file itself, but it is genuine
  information loss.
- **Known limitation:** file grouping is a linear scan per match, so cost is quadratic
  in the number of distinct files. Fine at tool-output scale, wrong for very wide
  result sets.

## LogCompressor — template extraction and repeat collapsing
**2026-08-03** · closes [#28](https://github.com/baileyrd/rusty_headroom/issues/28)

- **Added:** `LogCompressor`, a `Transform` + `LossyTransform` that normalizes each
  log line into a template, groups lines sharing a template, and reports each with a
  count and an example.
- **Added:** `templatize` and `has_severity` as public helpers.
- **Measured:** estimated token reduction of **93% at 50 lines, 98% at 200**.
- **The rule that matters:** error and warning lines are preserved verbatim, *in
  addition to* the template summary rather than as a replacement. A summary that
  reports `x1000 INFO ok` and drops the one `ERROR upstream timeout` is smaller,
  cheaper, and actively harmful — the agent then believes nothing went wrong.
- **Design note:** values are normalized, words are not. Over-eager normalization
  would collapse `disk full` into `disk ok`, making the summary confidently wrong.
  Timestamps, numbers-with-units, hex, UUIDs, and paths become placeholders; ordinary
  words stay.
- **Design note:** templates are reported in first-appearance order. Alphabetical
  ordering would scramble the log's narrative.
- **Known limitation:** output size is roughly constant regardless of input size, so
  a 1000-line log and a 200-line log summarize to about the same thing. Safe, because
  severity lines are always kept, but information density falls as logs grow.
- **Known limitation:** template extraction is heuristic and unaware of log formats.
  Structured JSON logs are routed to SmartCrusher instead, but an unusual text format
  may over- or under-normalize.

## SmartCrusher formatter and transform — JSON actually compresses
**2026-08-03** · closes [#25](https://github.com/baileyrd/rusty_headroom/issues/25)

- **Added:** `format_plan` — renders a `CrushPlan` into text for a language model:
  record and elision counts, constants stated once, low-cardinality fields
  enumerated, anchor records verbatim with their original indices, and the CCR marker.
- **Added:** `SmartCrusher`, implementing `Transform` + `LossyTransform` and holding a
  `CcrStore`. The pipeline now runs end to end — detect, analyze, rank, plan, format,
  store, token-validate.
- **Measured:** on a realistic file-listing tool result, estimated token reduction of
  **77% at 20 records, 91% at 50, 98% at 200**. The reference claims 60–95% on
  structured data.
- **Design note:** anchors are serialized from their original values, so key order and
  numeric literals survive exactly. An anchor that came back reformatted would not be
  the record the output promised.
- **Design note:** the original goes to the CCR store via `store_and_mark` before the
  marker is emitted, so a marker can never advertise a hash nothing was stored under.
- **Design note:** no special casing for the flagship compressor — it goes through
  `apply_guarded` (I8) and `validated_apply` (I5) like any other transform.
- **Known limitation:** the head sample is a fixed count, not a proportion. A
  1000-record array is summarized to the same handful of anchors as a 50-record one,
  so information density falls as arrays grow. Outliers are still always kept, which
  is what keeps this safe rather than merely aggressive.
- **Known limitation:** output is written for a model to read, not to be parsed. There
  is no path back from the rendered text to JSON — recovery is via CCR retrieval, and
  only while the entry lives (24h TTL).
- **Known limitation:** only record-set JSON compresses. Wide objects, deep nests, and
  scalar-heavy documents are classified but have no compressor, so they decline.

## SmartCrusher planning — decide before mutating
**2026-08-03** · closes [#24](https://github.com/baileyrd/rusty_headroom/issues/24)

- **Added:** `plan(...) -> Option<CrushPlan>` — a complete, inert decision about what
  to keep and what to say about the rest. Building one mutates nothing.
- **Added:** `CrushPlan` with anchors (records kept verbatim, sorted and deduplicated)
  and `FieldPlan`s (constants stated once, low-cardinality fields enumerated).
- **Design note:** outliers are anchored unconditionally. If they exceed the sample
  budget the head sample yields — an outlier is never dropped to make room, because
  dropping the anomalous record is the one failure that makes compressed output
  actively worse than no compression.
- **Design note:** `plan` returns `None` when compression would not pay. Invariant I5
  would catch a bad plan afterwards, but burning a format-and-tokenize pass to learn
  what the planner already knew is waste.
- **Design note:** optional low-cardinality fields are not enumerated. Describing one
  by its value set needs an "on some records" qualification that costs more than the
  enumeration saves.
- **Bug fixed in already-merged code:** outlier rarity scoring asked only whether a
  value was held by fewer than half the records. True for a two-valued field, wrong
  from three onwards — a 10/10/10 split across 30 records flagged *every* record as
  anomalous, so the planner saw an all-outlier array and declined. Rarity is now
  measured against the share an even split would give. Found by this issue's tests,
  with regression tests in both directions.
- **Known limitation:** the head sample is taken from the front only. A record set
  whose interesting structure is at the tail relies entirely on outlier detection to
  surface it.
- **Known limitation:** still no output. The formatter that renders a plan is #25.

## SmartCrusher outlier detection — keep what stands out
**2026-08-03** · closes [#22](https://github.com/baileyrd/rusty_headroom/issues/22)

- **Added:** `rank_outliers` — ranks records by how much they stand out, most
  anomalous first, with an `OutlierReason` explaining each contribution.
- **Why it matters:** summarizing 500 near-identical records is only safe if the ones
  that are *not* near-identical survive. The interesting record in tool output is
  almost always the anomalous one — the failed test among 200 passes, the file with a
  permission error. Compressing that away yields output that is smaller, cheaper, and
  useless.
- **Signals:** rare values of a repeated field, a field the record's peers lack,
  error-shaped fields (double weight), numeric outliers, and size outliers.
- **Design note:** scores are fixed-point integers, not floats. Ranking must be
  deterministic down to tie-breaking, and an integer score has a total order by
  construction with no `NaN` to decide about. Ties break on record index (I4).
- **Design note:** numeric outliers use median and median-absolute-deviation rather
  than mean and standard deviation. Tool-output distributions are routinely skewed,
  and a single extreme value drags the mean toward itself, masking the very outlier it
  should expose.
- **Design note:** error field names match whole words, case-insensitively. Substring
  matching would fire on `error_rate` and rank ordinary telemetry as anomalous.
- **Design note:** records that stand out in no way are omitted rather than ranked
  last, so a genuinely uniform array yields nothing instead of an arbitrary pick.
- **Known limitation:** `Unique` and `Constant` fields contribute no signal — the
  first distinguishes every record equally, the second none. Correct, but it means a
  record anomalous *only* in an identifier field goes unflagged.
- **Known limitation:** size scoring serializes every record, which is a full pass
  over the document on top of analysis.
- **Known limitation:** still analysis. Nothing consumes these rankings yet — anchor
  selection and the compaction formatter remain open.

## SmartCrusher analyzer — classification and field statistics
**2026-08-03** · closes [#20](https://github.com/baileyrd/rusty_headroom/issues/20)

- **Added:** `classify(&Shape, &CrushConfig) -> Pattern` naming the overall pattern —
  `RecordSet`, `ScalarHeavy`, `WideObject`, `DeepNest`, or `Unremarkable`.
- **Added:** `analyze_record_set` computing per-field statistics across an array of
  objects — `Constant`, `LowCardinality`, `Unique`, or `Varied`, each with a
  `present_in` count.
- **Added:** `Shape::depth` and `Shape::string_bytes`, and three `CrushConfig`
  knobs (`max_low_cardinality`, `wide_object_fields`, `scalar_heavy_bytes`).
- **Correctness note:** a field is reported `Constant` only when it is present in
  *every* record **and** equal in every record. A field that is uniform where present
  but absent from one record is optional, not constant — reporting it constant would
  tell the model every record carries it, which is false. Asserting something untrue
  to the model is worse than not compressing.
- **Design note:** `Unique` fields are identifiers and are never elided. They are how
  the model refers back to a specific record, so summarizing them away costs it the
  ability to ask about anything it can see.
- **Design note:** classification and analysis disagree on strictness deliberately.
  `Shape::is_record_set` treats one odd record as making the array heterogeneous;
  `analyze_record_set` still analyzes it, because an array where one record carries
  `error` and the rest do not is exactly what field statistics exist to surface.
- **Design note:** cardinality keys on the serialized form of each value, so `1` and
  `"1"` stay distinct. Counting accumulates in a `BTreeMap` — sorting is safe here
  because these counts drive decisions rather than output ordering.
- **Known limitation:** statistics are refused for arrays mixing objects and scalars.
  Analyzing only the object elements would produce numbers that read as though they
  described the whole array.
- **Known limitation:** analysis only. Nothing acts on these findings yet — anchor
  selection, planning, and the compaction formatter are still open, so no JSON is
  compressed.

## SmartCrusher foundations — config and structural IR
**2026-08-03** · closes [#15](https://github.com/baileyrd/rusty_headroom/issues/15)

- **Added:** `CrushConfig` — tuning for JSON compression, with documented defaults
  aimed at the shape that dominates agent tool output: an array of many
  near-identical records.
- **Added:** `Document` and `Shape` — the IR that analysis, planning, and formatting
  share. The document is order- and literal-preserving `serde_json::Value`; the shape
  is a structural summary derived from it. Keeping them separate means a planning bug
  cannot corrupt data.
- **Design note:** object fields are held in `Vec<(String, Shape)>`, not a
  `BTreeMap`. A `BTreeMap` would be deterministic but would silently sort keys,
  changing the bytes sent upstream. The `Vec` is deterministic *and* order-preserving.
  No `HashMap` appears on any path influencing output (invariant I4).
- **Design note:** an array with one odd record out is treated as heterogeneous.
  Calling it homogeneous would let the record that differs be summarized away as
  ordinary — and that record is usually the one worth reading.
- **Design note:** `CrushConfig::max_depth` bounds analysis recursion. Tool output is
  not trusted input, and unbounded recursion over it is a stack overflow waiting to
  happen.
- **Known limitation:** compact JSON round-trips byte-exactly, but insignificant
  whitespace does not — pretty-printed input comes back compact. Safe, because this
  path is only reached for documents SmartCrusher is actually rewriting; a declined
  document is restored from the caller's untouched original by the I5 fallback, which
  never re-serializes.
- **Known limitation:** foundations only. Analysis, statistics, outlier detection,
  anchor selection, and the compaction formatter are still open, so no JSON is
  actually compressed yet.

## Live-zone dispatcher (invariants I2, I3)
**2026-08-03** · closes [#14](https://github.com/baileyrd/rusty_headroom/issues/14)

- **Added:** `Conversation`, `Message`, and `Role` — the conversation model. `system`
  and `tools` are exposed immutably and have no mutable accessor at all, so the
  compression path cannot reach the cache hot zone. Invariant I2 becomes a function
  that does not exist rather than a rule to remember.
- **Added:** `live_zone()` — computes which blocks are eligible for compression by
  scanning from the tail: the newest user message's text, plus the newest instance of
  each tool-output shape.
- **Added:** `BlockKind::FunctionCallOutput`, `LocalShellCallOutput`, and
  `ApplyPatchCallOutput`, the OpenAI Responses output shapes named in the live-zone
  definition.
- **Design note:** the newest-instance rule is applied *on top of*
  `frozen_message_count`, not instead of it. A message can sit above the floor and
  still have been sent upstream already. Compressing too little costs tokens;
  compressing too much invalidates a cached prefix, costing tokens *and* context,
  silently. The failure directions are not symmetric.
- **Bug found and fixed during development:** an early version treated "the latest
  user text" as its own category, which reached back to prose several turns old
  whenever the newest user message carried only tool results — the exact
  cache-busting this module exists to prevent. Corrected to "the text of the latest
  user message", with tests covering both directions.
- **Known limitation:** `frozen_message_count` is supplied by the caller. Nothing
  derives it from `cache_control` markers yet, so today every caller passes `0` and
  the newest-instance rule is doing all the work.
- **Known limitation:** no compressor is wired to the dispatcher yet. It computes the
  eligible set and applies a closure; routing to type-aware compressors arrives with
  the pipeline orchestrator.

## PR #16 — Token validation with fallback to the original (invariant I5)
**2026-08-03** · [#16](https://github.com/baileyrd/rusty_headroom/pull/16) · closes [#8](https://github.com/baileyrd/rusty_headroom/issues/8)

- **Added:** `validated_apply` — if a compression does not reduce the token count,
  the original is forwarded. Wraps transform dispatch, so no compressor can opt out.
- **Behavior worth knowing:** equal token counts are treated as *not* an improvement
  and discarded. A compression saving zero tokens still costs a CCR entry and a
  possible retrieval round-trip.
- **Behavior worth knowing:** a transform that mutates a block and then declines has
  its partial mutation reverted unconditionally. Half-finished work never reaches
  upstream.
- **Behavior worth knowing:** invariant violations propagate rather than being
  absorbed by the fallback. Only *declined* and *malformed* outcomes are recoverable.
- **Known limitation:** counts come from the heuristic estimator, which is
  deliberately conservative, so some genuine compressions are declined that an exact
  tokenizer would accept. Safe direction, but savings are left on the table until the
  tiktoken and HuggingFace backends land.
- **Known limitation:** two tokenizer passes per attempted compression. Acceptable
  with the heuristic estimator; worth revisiting alongside an exact BPE tokenizer.

## PR #13 — CCR content addressing, marker format, and in-memory store
**2026-08-03** · [#13](https://github.com/baileyrd/rusty_headroom/pull/13) · closes [#9](https://github.com/baileyrd/rusty_headroom/issues/9), [#10](https://github.com/baileyrd/rusty_headroom/issues/10)

- **Added:** `ContentHash` (BLAKE3 truncated to 128 bits), the `<<ccr:HASH>>` marker
  format, the `CcrStore` trait, and an in-memory backend with TTL expiry.
- **Added:** `store_and_mark`, which stores content under exactly the hash its marker
  advertises — the two halves cannot drift apart.
- **Design note:** hashes derive from content alone, with no counter, timestamp, or
  session identifier. That is what makes markers replay-safe and keeps the provider's
  prompt cache hitting across identical requests.
- **Design note:** the store reads a clock for TTL, which does not conflict with
  invariant I4 — hashing never consults it, so the bytes sent upstream are unaffected.
- **Known limitation:** in-memory only; SQLite and Redis backends are still open.
- **Known limitation:** no eviction beyond TTL, and nothing schedules `purge_expired`
  yet. Expired entries correctly read as absent, but stay resident until something
  sweeps them.

## PR #12 — Block type and transform traits
**2026-08-03** · [#12](https://github.com/baileyrd/rusty_headroom/pull/12) · closes [#7](https://github.com/baileyrd/rusty_headroom/issues/7)

- **Added:** `Block` — sibling fields private, one mutable accessor. A transform
  cannot change what binds a tool result to the call it answers.
- **Added:** `Transform` as `fn(&mut Block) -> Result<()>`, which makes "reorder the
  content array", "split this block", and "add a field" unrepresentable rather than
  merely forbidden.
- **Added:** `LosslessTransform` / `LossyTransform` as separate traits, so the
  auth-mode policy gate is a type signature rather than a runtime flag.
- **Added:** `apply_guarded`, centralizing the refusal of signed, encrypted, and
  redacted blocks so the check exists in exactly one place.
- **Known limitation:** `Block` holds content as `String`. Byte-faithful passthrough
  at the proxy boundary will need `RawValue`-backed storage for untouched blocks.

## Parity loop — workspace foundation and the first core modules
**2026-08-03** · closes [#2](https://github.com/baileyrd/rusty_headroom/issues/2), [#3](https://github.com/baileyrd/rusty_headroom/issues/3), [#4](https://github.com/baileyrd/rusty_headroom/issues/4), [#5](https://github.com/baileyrd/rusty_headroom/issues/5), [#6](https://github.com/baileyrd/rusty_headroom/issues/6)

- **Added:** `gap-analysis.md` — a clean-room, documentation-derived assessment of
  the capability surface needed to reach parity with
  [headroomlabs-ai/headroom](https://github.com/headroomlabs-ai/headroom). 97 rows
  across 17 workstreams, dependency-ordered, with the reference's ten cache-safety
  invariants carried over as per-issue acceptance criteria.
- **Added (#2):** Cargo workspace with five crates — `headroom-core`,
  `headroom-proxy`, `headroom-mcp`, `headroom-cli`, `headroom-simulators`. MSRV
  1.80, edition 2021. `serde_json` is configured workspace-wide with
  `preserve_order` + `arbitrary_precision` + `raw_value`, which invariant I1
  (byte-faithful passthrough) is unimplementable without.
- **Added (#2):** `NOTICE` recording the clean-room derivation and crediting the
  reference project for the design.
- **Added (#3):** error taxonomy that separates *declined* compression from
  *malformed* input from *invariant violation*. The split is load-bearing:
  invariant I5's fallback path must recover from the first two and must never
  swallow the third.
- **Added (#4):** `Tokenizer` trait and a dependency-free heuristic estimator.
  Documented to never under-count — over-counting costs a missed compression,
  under-counting silently breaks the I5 guarantee and grows the user's prompt.
- **Added (#5):** `ContentRouter` — a pure `detect(bytes) -> Detection` classifying
  JSON, code, logs, diffs, search results, and prose, each with a confidence
  signal. Detection order is deliberate: a diff of a Rust file must classify as a
  diff, not as code.
- **Added (#6):** `AdaptiveSizer` with the documented per-type floors (code > 2 KB,
  JSON > 1 KB, logs > 500 B, text > 5 KB), boundaries exclusive.
- **Testing:** 36 unit tests and 3 doc-tests. Each detector carries both a positive
  case and a near-miss (a Markdown rule that is not a diff, prose containing
  "error" that is not a log, prose with colons that is not grep output).
- **Known limitation:** `headroom-proxy`, `headroom-mcp`, `headroom-cli`, and
  `headroom-simulators` are documented stubs. They compile and carry their module
  docs, but the handlers, MCP tools, and subcommands land in later issues.
- **Known limitation:** the tokenizer is heuristic only. Exact tiktoken (#T2) and
  HuggingFace (#T3) backends are filed but not yet implemented, so token counts are
  conservative approximations rather than exact.

---

## PR #1 — Bootstrap the repo with the standard governance file set
**2026-08-03** · [#1](https://github.com/baileyrd/rusty_headroom/pull/1)

- **Added:** the full standard governance set — four PR templates (feature,
  bug_fix, docs, chore), two issue templates plus `config.yml`, README,
  CONTRIBUTING, CODE_OF_CONDUCT, SECURITY, CHANGELOG, this file, ARCHITECTURE, and
  an ADR seed at `docs/adr/0001-template.md`. The repo was empty before this — no
  commits, no source, no scaffolding.
- **Added:** `.github/workflows/ci-rust.yml` running `cargo fmt --check`, `cargo
  clippy -D warnings`, and `cargo test --all-features`. Applied deliberately ahead
  of any manifest so the gate exists before the first line of code rather than
  being retrofitted.
- **Known limitation, stated plainly:** CI will fail on every run until a
  `Cargo.toml` lands — there is nothing for cargo to build. This was a conscious
  tradeoff, not an oversight.
- **Known limitation:** README's one-line description and ARCHITECTURE's boundary
  table are placeholders. Neither could be filled honestly against an empty repo,
  and inventing content for them would be worse than leaving the gap visible.
- **Manual follow-up:** the CI check only gates merges once it's set as a required
  status check under branch protection — that's a GitHub settings change, not
  something this commit can do.
