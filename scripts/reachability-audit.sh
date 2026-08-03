#!/usr/bin/env bash
#
# Reachability audit — is every capability actually reached from a request?
#
# This exists because five separate gaps in this repository were shipped, tested,
# documented as done, and never called:
#
#   - the SSE observers      (#71) — every response read with the Anthropic classifier
#   - the memory module      (#73) — no reference outside memory.rs
#   - cache stabilization    (#75) — all four public functions unreferenced
#   - the code compressor    (#82) — the proxy never compressed source files
#   - the prose compressors  (#84) — nor prose tool results
#
# Every one had passing tests. A test proves a function works, not that anything
# calls it.
#
# Checks 6 and 7 are different failures with the same shape.
#
# Check 6: a *second copy* of a decision, which drifts from the real one and then
# describes the system incorrectly with total confidence. Eight copies of the
# routing table have been found, in `headroom compress`, the MCP server, `route`
# itself, `headroom inspect`, `headroom tools`, the reformat list, the metrics
# reason list, and the Python binding.
#
# Check 7: a *guarantee* nothing checks. The invariants are described in README.md
# and ARCHITECTURE.md as acceptance criteria on every change, with a named file
# gating each one. A renamed or deleted test leaves that claim standing.
#
# The first version of this check asked "is this symbol referenced outside its own
# file?" — which #82 and #84 both passed, because the CLI and the MCP server
# referenced them. Reachable from *somewhere* is not reachable from the *request
# path*. That is why check 1 below exists in the form it does.
#
# Usage:  scripts/reachability-audit.sh
# Exit:   0 clean, 1 if anything looks unreached.

set -uo pipefail
cd "$(dirname "$0")/.."

status=0
note() { printf '  %s\n' "$1"; }
fail() { printf '  ✗ %s\n' "$1"; status=1; }

echo "== 1. every detected content type reaches a compressor"
# The check that would have caught #82 and #84 on day one. `for_type` is the single
# routing table (D23); a content type missing from it is silently forwarded whole.
types=$(grep -oP '^\s+\K[A-Z][A-Za-z]+(?=,)' crates/headroom-core/src/detection/router.rs \
  | sort -u | grep -v Unknown)
for t in $types; do
  if grep -q "ContentType::$t =>" crates/headroom-core/src/pipeline/orchestrator.rs; then
    note "✓ $t"
  else
    fail "$t is detected but reaches no compressor"
  fi
done

echo "== 2. every proxy module is referenced from outside itself"
for f in crates/headroom-proxy/src/*.rs; do
  m=$(basename "$f" .rs)
  case "$m" in main|lib|mod) continue;; esac
  n=$(grep -rl "\b$m::" --include=*.rs crates/headroom-proxy/src crates/headroom-cli/src 2>/dev/null \
    | grep -vc "^$f$")
  [ "$n" -gt 0 ] && note "✓ $m" || fail "$m is referenced by nothing"
done

echo "== 3. every CLI command is dispatched"
for c in $(grep -oP '^pub fn \K\w+' crates/headroom-cli/src/commands.rs); do
  grep -q "commands::$c\b" crates/headroom-cli/src/main.rs \
    && note "✓ $c" || fail "$c is defined but never dispatched"
done

echo "== 4. every declared env var is read"
# Matched on the Rust constant name (`vars::UPSTREAM`), not the string value. An earlier
# version searched for the string and reported every variable as unread — an audit that
# cries wolf is worse than no audit, because people learn to skip the output.
consts=$(grep -oP '^\s+pub const \K\w+(?=: &str = "HEADROOM_)' crates/headroom-proxy/src/config.rs)
for c in $consts; do
  n=$(grep -rn "vars::$c\b" --include=*.rs crates | wc -l)
  [ "$n" -gt 0 ] && note "✓ $c" || fail "$c is declared but never read"
done

echo "== 5. every decline reason can actually be produced"
# `Declined` variants are telemetry: an operator builds a dashboard from them, and a
# reason that cannot fire is a panel that stays empty forever while looking like it means
# something. `OutsideLiveZone` was exactly that — it described a check the transform layer
# cannot perform, because a Block carries no position.
#
# Matched on *construction* (`Error::declined(Declined::X)` / `Error::Declined(Declined::X)`)
# rather than on any mention. A first attempt counted every `Enum::Variant` occurrence and
# reported six AnchorKind variants as dead when all six are built inside their own module —
# the same cry-wolf failure as the env-var check below it.
for v in $(grep -oP '^\s+\K[A-Z][A-Za-z]+(?=,)' crates/headroom-core/src/error.rs \
    | awk 'NR<=20' | sort -u); do
  grep -q "^\s*$v,$" crates/headroom-core/src/error.rs || continue
  n=$(grep -rEn "Error::[dD]eclined\(Declined::$v\b" --include=*.rs crates | wc -l)
  [ "$n" -gt 0 ] && note "✓ $v" || fail "Declined::$v is declared but nothing produces it"
done

echo "== 6. nothing carries a second routing table"
# D23 says there is one routing table and `Orchestrator::for_type` is it. Three separate
# copies have existed — in `headroom compress`, in the MCP server, and in `headroom
# inspect` — and every one of them eventually disagreed with the pipeline. The last was
# the worst: `inspect` exists to answer "why did this not compress", and it answered
# `compressor: none` for prose that `headroom compress` shrank by 70% in the same shell.
#
# A copy is recognizable before it drifts: a match on `ContentType` with enough arms to
# be a table. Two files are allowed one, for different reasons — named here rather than
# skipped silently, so a third has to be argued for:
#
#   orchestrator.rs   the routing table itself
#   adaptive_sizer.rs a size threshold per type — a different question about the same
#                     enum, and one that has never had a compressor's name in it
allowed='pipeline/orchestrator.rs|detection/adaptive_sizer.rs'
compressors='smart_crusher|log_compressor|search_compressor|diff_compressor|code_compressor|text_summarizer'

# 6a. A content type named on the same line as a compressor's name, as a string literal.
#
# This is the shape of a routing-table entry in either form a copy has actually taken:
#   ContentType::Json => "smart_crusher"     (a match arm)
#   (ContentType::Json, "smart_crusher"),    (a tuple in an array)
#
# The first version of this check counted match arms only, and so missed the second form
# — which is the form `headroom tools` used. That copy was found by reading the file, not
# by the check written to find it, which is the whole reason 6a is anchored on the
# compressor name instead of on the syntax around it. A real compressor name only comes
# from `Transform::name()`; a string literal of one is somebody restating the table.
restated=$(grep -rnE "ContentType::[A-Za-z]*.*\"($compressors)\"" --include=*.rs crates \
  | grep -vE "$allowed" || true)
if [ -n "$restated" ]; then
  while IFS= read -r line; do
    fail "routing table restated: $line"
  done <<< "$restated"
else
  note "✓ no content type is paired with a compressor name literal"
fi

# 6b. And a match on ContentType with enough arms to be a table, whatever it maps to.
# Catches a copy that stores compressor *values* rather than names. Two files are
# allowed one, for different reasons — named here rather than skipped silently, so a
# third has to be argued for:
#
#   orchestrator.rs   the routing table itself
#   adaptive_sizer.rs a size threshold per type — a different question about the same
#                     enum, and one that has never had a compressor's name in it
for f in $(grep -rl "ContentType::" --include=*.rs crates); do
  n=$(grep -c "ContentType::[A-Za-z]* *=>" "$f")
  [ "$n" -lt 3 ] && continue
  if printf '%s' "$f" | grep -qE "$allowed"; then
    note "✓ $f (allowed)"
  else
    fail "$f matches on $n ContentType arms — a second routing table? (D23)"
  fi
done

echo "== 7. every invariant has a test that names it"
# README.md and ARCHITECTURE.md both say which file gates which invariant, and that claim
# is the load-bearing one in this repository: the invariants are described as acceptance
# criteria on every change rather than aspirations. A deleted or renamed test with the
# documentation still claiming coverage is the worst version of everything above — not a
# capability nothing reaches, but a *guarantee* nothing checks.
#
# Checked at the time of writing rather than assumed: invariants.rs carries `i1_`..`i4_`,
# `i6_`..`i9_` (eight), and properties.rs covers I5 and I10, which are claims about many
# inputs and cannot be established by one fixture. That is exactly what the docs say.
e2e=crates/headroom-proxy/tests/invariants.rs
prop=crates/headroom-proxy/tests/properties.rs
for n in 1 2 3 4 6 7 8 9; do
  grep -qE "^(async )?fn i${n}_" "$e2e" \
    && note "✓ I$n gated end to end" \
    || fail "I$n is documented as gated in $e2e and no test there names it"
done
# I5 and I10 by property rather than by fixture, so they are matched on the claim in the
# test body — a property test's name describes the property, not the invariant number.
for n in 5 10; do
  grep -q "I$n" "$prop" \
    && note "✓ I$n gated by property" \
    || fail "I$n is documented as gated in $prop and nothing there cites it"
done

echo "== 8. every registered route is in the reachability test's list"
# `every_declared_route_is_actually_reachable` asks the router for each path and fails on
# a 404 or 405. It needs a list, and axum's `Router` cannot be enumerated, so the list is
# hand-maintained — which is the thing this script exists to distrust. This check reads
# the `.route(` calls straight out of `server.rs` and fails if one is missing from it.
#
# `/v1/realtime` is why. The comment beside it says it exists because Codex speaks
# WebSocket and a proxy that only speaks HTTP breaks that client, and nothing in the suite
# ever asked the router for it — the `/ws` in `websocket.rs`'s tests is that test's own
# echo server. A typo in either path would have disabled Codex support silently.
#
# Wildcard routes are matched on their prefix: `/v1/conversations/{*rest}` is covered by
# requesting `/v1/conversations`, and listing both spellings would be noise.
routes=$(grep -oP '\.route\("\K[^"]+' crates/headroom-proxy/src/server.rs | sed 's|/{\*.*||')
for r in $(printf '%s\n' $routes | sort -u); do
  if grep -q "\"$r\")" crates/headroom-proxy/src/server.rs; then
    note "✓ $r"
  else
    fail "$r is registered and missing from the test's ROUTES list"
  fi
done

echo "== 9. the README marks every startup-only setting as needing a restart"
# README.md's configuration table says which settings a running proxy will not pick up.
# That is a second copy of `config::STARTUP_ONLY`, and the table is what an operator
# actually reads — so it is the copy most worth checking.
#
# `HEADROOM_UPSTREAM` is why. The README listed it as live and it is not: the relay client
# is built once with its base URL baked in, so a new value landed in the override map and
# changed nothing while `/admin/runtime-env` and `/health` both confirmed the change.
#
# The init template is checked the same way, by a test rather than here — see
# `the_generated_config_marks_every_startup_only_setting`.
consts=$(grep -oP '^\s+vars::\K[A-Z_]+(?=,)' crates/headroom-proxy/src/config.rs \
  | awk 'NR<=20' | sort -u)
for c in $consts; do
  value=$(grep -oP "pub const $c: &str = \"\K[^\"]+" crates/headroom-proxy/src/config.rs)
  [ -z "$value" ] && continue
  # The row for this variable, if the table has one, and whether it is marked.
  row=$(grep -F "\`$value\`" README.md | grep -F '|' | head -1)
  if [ -z "$row" ]; then
    fail "$value is startup-only and absent from the README's configuration table"
  elif printf '%s' "$row" | grep -qE '\|\s*yes\s*\|'; then
    note "✓ $value"
  else
    fail "$value is startup-only and the README does not mark it as needing a restart"
  fi
done

echo "== 10. every dialect handler scans for cache-busting content"
# `volatile::scan` reports content in the cached prefix that changes every request — a
# timestamp in a system prompt means the provider's cache never matches and the customer
# pays full price for the whole conversation every turn, while the savings metric looks
# healthy throughout. It is the most expensive silent failure this proxy can have.
#
# It ran on `/v1/messages` only. Both OpenAI handlers compressed without it, and the
# detector could not have found anything there anyway: it knew `system` and `tools`, both
# Anthropic-shaped, and an OpenAI system prompt lives in `instructions` or in a
# `role: "system"` message. Measured before the fix — 0 findings for both OpenAI shapes.
#
# So: a *handler* that compresses a dialect must also scan it. Counted outside test
# modules, and only in files that define axum handlers — `State<AppState>` is what makes
# a function a request entry point.
#
# That last restriction is not incidental. A first version checked every file calling
# `compress_dialect` and flagged `compression.rs`, which calls it from `compress_request`
# — a pure function, reached only through the handlers that do scan. An audit that cries
# wolf is worse than no audit, because people learn to skip its output; this script has
# recorded that lesson twice already and nearly earned a third.
for f in $(grep -rl "compress_dialect(" --include=*.rs crates/headroom-proxy/src); do
  grep -q "State<AppState>" "$f" || continue
  body=$(awk '/^#\[cfg\(test\)\]/{exit} {print}' "$f")
  calls=$(printf '%s\n' "$body" | grep "compress_dialect(" | grep -vc "fn compress_dialect")
  scans=$(printf '%s\n' "$body" | grep -c "volatile::scan(")
  [ "$calls" -eq 0 ] && continue
  if [ "$calls" -eq "$scans" ]; then
    note "✓ $(basename "$f") ($calls compressed, $scans scanned)"
  else
    fail "$(basename "$f") compresses $calls time(s) and scans $scans — a dialect is unguarded"
  fi
done

echo
[ "$status" -eq 0 ] && echo "clean" || echo "findings above — see the header for why this matters"
exit "$status"
