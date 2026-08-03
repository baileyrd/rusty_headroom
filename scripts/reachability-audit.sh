#!/usr/bin/env bash
#
# Reachability audit — is every capability actually reached from a request?
#
# This exists because four separate gaps in this repository were shipped, tested,
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

echo
[ "$status" -eq 0 ] && echo "clean" || echo "findings above — see the header for why this matters"
exit "$status"
