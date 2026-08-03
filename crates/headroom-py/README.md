# headroom (Python)

Python bindings for the `headroom` compression engine — gap rows B1 and B2.

```python
import headroom

result = headroom.compress(open("build.log").read(), model="gpt-4o")
print(result.tokens_before, "->", result.tokens_after, f"({result.reason})")

# The content is safe to send unconditionally: it comes back unchanged whenever
# compression did not apply or did not help.
send(result.content)
```

## Building

```
pip install maturin
cd crates/headroom-py
maturin develop      # or: maturin build --release
```

The crate is deliberately outside the workspace's `default-members`, so the everyday
`cargo build` and `cargo test` loop stays green without a Python toolchain installed.

## What crosses the boundary

Strings and numbers only. The CCR store is per call, so a `<<ccr:HASH>>` marker in the
returned text is not retrievable through this API. That is deliberate: a store shared
across calls would let one caller fetch content from a request they never made. Use the
proxy or the MCP `headroom_retrieve` tool when retrieval is needed.
