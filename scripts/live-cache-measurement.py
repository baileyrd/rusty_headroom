#!/usr/bin/env python3
"""Measure what this proxy actually saves, against a real provider.

Everything else in this repository is measured against fixtures written beside the code
and a loopback simulator. Both are necessary and neither can answer the commercial
question: *does routing an agent through this proxy reduce the bill?* Only the provider
can answer that, because only the provider decides what it cached.

Run:

    ANTHROPIC_API_KEY=sk-ant-... scripts/live-cache-measurement.py --yes

It spends real money. Roughly a cent at the defaults; `--estimate` prints the arithmetic
and exits without calling anything.

# What it measures

Two arms, same conversation, run turn by turn the way an agent grows one — each turn is
the previous turns plus the assistant's reply plus a new user message carrying a bulky
tool result.

    control    client -> provider
    proxied    client -> headroom -> provider

Per turn it records the provider's own `usage`, which is the only account that bills.

# Four things that would make the number meaningless

**Caching must actually engage in the control arm.** Anthropic caches only what a
`cache_control` marker pins, and only above a minimum prompt length. If the control's
second turn reports `cache_read_input_tokens: 0`, caching never happened and the
comparison is between two uncached runs. Asserted before anything is compared — the
lesson this repository has paid for most often.

**The arms must not share a cache.** They send near-identical prefixes, so whichever runs
second would read the cache the first one wrote and look spectacular. Each arm embeds a
distinct nonce in its system prompt, which makes the prefixes different documents to the
provider.

**Hit rate is the wrong headline.** A proxy can raise the fraction of tokens served from
cache while raising the total bill, by writing more cache than it saves. Cache writes
cost more than ordinary input and reads cost far less, so this reports a
billable-equivalent — the weighted sum — and treats that as the answer.

**The model is asked for, not guessed.** Model IDs move; a stale literal here would fail
with a 404 that reads like a network problem.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

# Anthropic's published multipliers, relative to an ordinary input token. A cache write
# costs more than the token would have; a read costs a fraction of it. Reporting a raw
# token count would let a change that writes far more cache than it saves read as a win.
CACHE_WRITE_MULTIPLIER = 1.25
CACHE_READ_MULTIPLIER = 0.1

# Overridable so this harness can be exercised against a local stand-in. The measurement
# itself is only meaningful against the real provider — a stand-in reports whatever it was
# written to report — but the request shapes, the arithmetic and the vacuity guard are all
# checkable without spending anything, and were.
ANTHROPIC = os.environ.get("HEADROOM_MEASURE_BASE", "https://api.anthropic.com")
PROXY_PORT = 8899


def post(url: str, body: dict, headers: dict, timeout: int = 120) -> dict:
    request = urllib.request.Request(
        url, data=json.dumps(body).encode(), headers=headers, method="POST"
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return json.loads(response.read())
    except urllib.error.HTTPError as err:
        detail = err.read().decode("utf8", "replace")[:400]
        raise SystemExit(f"{url} returned {err.code}: {detail}") from err


def pick_model(key: str, requested: str | None) -> str:
    """The newest Sonnet the account can actually use, or `requested` if given."""
    if requested:
        return requested

    request = urllib.request.Request(
        f"{ANTHROPIC}/v1/models",
        headers={"x-api-key": key, "anthropic-version": "2023-06-01"},
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        models = [m["id"] for m in json.loads(response.read()).get("data", [])]

    for want in ("sonnet", "opus", "haiku"):
        matches = [m for m in models if want in m]
        if matches:
            return matches[0]
    raise SystemExit(f"no usable model among: {models}")


def tool_result(turn: int, records: int) -> str:
    """A bulky, uniform tool result — the shape this proxy exists to compress."""
    rows = [
        {
            "id": index,
            "path": f"src/module_{index}.rs",
            "status": "ok",
            "size": 1000 + index % 50,
        }
        for index in range(records)
    ]
    rows[turn % records] = {
        "id": turn % records,
        "path": "src/broken.rs",
        "status": "error",
        "size": 4096,
    }
    return json.dumps(rows)


def conversation(turns: int, records: int, nonce: str) -> tuple[str, list, list]:
    """The system prompt, tools, and the full message list for the final turn."""
    # Long enough to clear the caching minimum on its own, and marked so the provider
    # actually caches it. A real agent's system prompt and tool schemas look like this.
    system = [
        {
            "type": "text",
            "text": (
                "You are a code-analysis assistant. Answer in one short sentence.\n"
                f"Session: {nonce}\n" + ("Context padding for cache eligibility. " * 220)
            ),
            "cache_control": {"type": "ephemeral"},
        }
    ]
    tools = [
        {
            "name": "list_files",
            "description": "List files in the repository. " + ("Detail. " * 60),
            "input_schema": {
                "type": "object",
                "properties": {"glob": {"type": "string"}},
                "required": ["glob"],
            },
        }
    ]

    messages = []
    for turn in range(turns):
        messages.append({"role": "user", "content": f"List batch {turn}."})
        messages.append(
            {
                "role": "assistant",
                "content": [
                    {
                        "type": "tool_use",
                        "id": f"call_{turn}",
                        "name": "list_files",
                        "input": {"glob": f"batch{turn}/**"},
                    }
                ],
            }
        )
        messages.append(
            {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": f"call_{turn}",
                        "content": tool_result(turn, records),
                    }
                ],
            }
        )
    return system, tools, messages


def run_arm(label: str, base: str, key: str, model: str, turns: int, records: int) -> list[dict]:
    """Sends `turns` growing requests and returns the provider's usage for each."""
    # Distinct per arm, so the two cannot read each other's cache entries. Without this
    # the arm that runs second reads what the first wrote and reports a spectacular,
    # entirely fictional saving.
    nonce = f"{label}-{int(time.time())}"
    system, tools, full = conversation(turns, records, nonce)

    usages = []
    for turn in range(1, turns + 1):
        body = {
            "model": model,
            "max_tokens": 64,
            "temperature": 0,
            "system": system,
            "tools": tools,
            "messages": full[: turn * 3],
        }
        reply = post(
            f"{base}/v1/messages",
            body,
            {
                "x-api-key": key,
                "anthropic-version": "2023-06-01",
                "content-type": "application/json",
            },
        )
        usage = reply.get("usage", {})
        usages.append(usage)
        print(
            f"  {label} turn {turn}: input={usage.get('input_tokens', 0)} "
            f"write={usage.get('cache_creation_input_tokens', 0)} "
            f"read={usage.get('cache_read_input_tokens', 0)}"
        )
    return usages


def billable(usages: list[dict]) -> float:
    """Input tokens weighted by what each kind actually costs."""
    return sum(
        u.get("input_tokens", 0)
        + CACHE_WRITE_MULTIPLIER * u.get("cache_creation_input_tokens", 0)
        + CACHE_READ_MULTIPLIER * u.get("cache_read_input_tokens", 0)
        for u in usages
    )


def start_proxy(binary: str, stabilize: bool) -> subprocess.Popen:
    env = {
        **os.environ,
        "HEADROOM_PORT": str(PROXY_PORT),
        "HEADROOM_UPSTREAM": ANTHROPIC,
        "HEADROOM_LOG": "warn",
        "HEADROOM_STABILIZE": "1" if stabilize else "0",
    }
    process = subprocess.Popen([binary], env=env, stdout=subprocess.DEVNULL)
    for _ in range(50):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{PROXY_PORT}/health", timeout=1)
            return process
        except Exception:
            time.sleep(0.2)
    process.terminate()
    raise SystemExit("the proxy never became healthy")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--turns", type=int, default=4)
    parser.add_argument("--records", type=int, default=200, help="rows per tool result")
    parser.add_argument("--model", default=None, help="default: newest Sonnet on the account")
    parser.add_argument("--binary", default="./target/release/headroom-proxy")
    parser.add_argument(
        "--stabilize",
        action="store_true",
        help="let the proxy place cache breakpoints; use when the client sets none",
    )
    parser.add_argument("--estimate", action="store_true", help="print the cost and exit")
    parser.add_argument("--yes", action="store_true", help="required to spend money")
    args = parser.parse_args()

    approx_prompt = args.records * 22 + 1400
    total = approx_prompt * args.turns * 2
    print(f"about {total:,} input tokens across {args.turns * 2} requests")
    print("at Sonnet input pricing that is well under a US cent\n")
    if args.estimate:
        return

    key = os.environ.get("ANTHROPIC_API_KEY")
    if not key:
        raise SystemExit("ANTHROPIC_API_KEY is not set; this measurement needs a real one")
    if not args.yes:
        raise SystemExit("refusing to spend money without --yes")
    if not os.path.exists(args.binary):
        raise SystemExit(f"{args.binary} not found; cargo build --release -p headroom-proxy")

    model = pick_model(key, args.model)
    print(f"model: {model}\n")

    control = run_arm("control", ANTHROPIC, key, model, args.turns, args.records)

    # Before anything is compared. If the provider cached nothing in the control arm,
    # both arms are uncached runs and every number below is about something else.
    later_reads = sum(u.get("cache_read_input_tokens", 0) for u in control[1:])
    if later_reads == 0:
        raise SystemExit(
            "\nthe control arm never read from cache, so there is no caching to compare.\n"
            "raise --records until the prefix clears the provider's minimum, or check that\n"
            f"{model} supports prompt caching."
        )

    print()
    proxy = start_proxy(args.binary, args.stabilize)
    try:
        proxied = run_arm(
            "proxied", f"http://127.0.0.1:{PROXY_PORT}", key, model, args.turns, args.records
        )
        metrics = urllib.request.urlopen(
            f"http://127.0.0.1:{PROXY_PORT}/metrics", timeout=10
        ).read().decode()
    finally:
        proxy.terminate()

    def totals(usages):
        return (
            sum(u.get("input_tokens", 0) for u in usages),
            sum(u.get("cache_creation_input_tokens", 0) for u in usages),
            sum(u.get("cache_read_input_tokens", 0) for u in usages),
        )

    print(f"\n{'':10} {'input':>9} {'write':>9} {'read':>9} {'billable':>10}")
    for label, usages in (("control", control), ("proxied", proxied)):
        inp, write, read = totals(usages)
        print(f"{label:10} {inp:>9,} {write:>9,} {read:>9,} {billable(usages):>10,.0f}")

    before, after = billable(control), billable(proxied)
    delta = before - after
    print(
        f"\n{'better' if delta > 0 else 'WORSE'} by {abs(delta):,.0f} billable-equivalent "
        f"input tokens ({abs(delta) / before * 100:.1f}%)"
    )
    if delta <= 0:
        print("the proxy cost more than it saved on this shape of traffic.")

    print("\nwhat the proxy reported about itself:")
    for line in metrics.splitlines():
        if line.startswith(("headroom_tokens_saved", "headroom_cache_", "headroom_compressed")):
            print(f"  {line}")


if __name__ == "__main__":
    main()
