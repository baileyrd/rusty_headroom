"""Smoke tests for the Python bindings — gap rows B1 and B2.

These run against an installed wheel, which is the point: the Rust unit tests already
cover the engine, and what is unproven until a wheel is built and imported is that the
*boundary* works. `DECISIONS.md` D4 deferred these bindings on the grounds that they
"would compile at best and never be exercised" — this file is what makes that no longer
true.

Run with:  maturin build --release && pip install <wheel> && pytest
"""

import json

import headroom


def a_log(lines=400):
    return "\n".join(
        f"2026-08-03T12:{i % 60:02d}:00Z INFO worker {i} handled request in {i}ms"
        for i in range(lines)
    )


def records(count=300):
    return json.dumps(
        [{"path": f"src/f{i}.rs", "size": i * 10, "kind": "file"} for i in range(count)]
    )


def test_the_module_imports_and_reports_a_version():
    assert headroom.__version__


def test_a_log_compresses():
    result = headroom.compress(a_log(), model="gpt-4o")

    assert result.compressed
    assert result.content_type == "log"
    assert result.reason == "compress"
    assert result.tokens_after < result.tokens_before
    assert result.tokens_saved == result.tokens_before - result.tokens_after


def test_json_records_compress():
    result = headroom.compress(records(), model="gpt-4o")

    assert result.compressed
    assert result.content_type == "json"


def test_content_below_the_threshold_comes_back_unchanged():
    # Invariant I5. A caller can send the result unconditionally, which is only true if
    # "nothing happened" still returns usable content.
    source = "just a sentence"
    result = headroom.compress(source)

    assert not result.compressed
    assert result.content == source


def test_subscription_mode_forbids_compression():
    # Invariant I10, across the boundary. Subscription mode buys safety by giving up
    # compression, and a binding that quietly ignored the policy would be the most
    # dangerous possible bug in this crate.
    source = a_log()
    result = headroom.compress(source, auth_mode="subscription")

    assert not result.compressed
    assert result.content == source
    assert result.reason == "policy-forbids"


def test_an_unknown_auth_mode_is_rejected_rather_than_defaulted():
    # Defaulting would hand the most permissive policy to a caller who misspelled the
    # most restrictive one — invariant I10 decided by a typo.
    try:
        headroom.compress("x", auth_mode="pay-as-you-gp")
    except ValueError as err:
        assert "unknown auth_mode" in str(err)
    else:
        raise AssertionError("an unknown auth mode was accepted")


def test_compression_is_deterministic():
    # Invariant I4. Output that varied run to run would bust the prompt cache this
    # project exists to protect.
    source = a_log()
    outputs = {headroom.compress(source, model="gpt-4o").content for _ in range(20)}

    assert len(outputs) == 1


def test_the_reason_distinguishes_why_nothing_happened():
    # "Nothing handles this content type", "a compressor ran and did not help", and
    # "policy forbids it" are three different problems with three different fixes, and a
    # caller seeing only compressed=False cannot tell them apart.
    #
    # Whitespace only, because short prose is no longer an example of "nothing handles
    # this": prose routes to the text compressor as of gap row C10's wiring, and then
    # declines because it is far below the size threshold. That distinction is the
    # reason this test exists rather than an inconvenience to it.
    assert headroom.compress("   \n\t  \n").reason == "no-compressor"
    assert headroom.compress("short prose").reason == "not-smaller"
    assert headroom.compress(a_log(), auth_mode="subscription").reason == "policy-forbids"


def test_token_counting_matches_what_compression_used():
    source = a_log()

    assert headroom.count_tokens(source, model="gpt-4o") == (
        headroom.compress(source, model="gpt-4o").tokens_before
    )


def test_detection_is_exposed_on_its_own():
    assert headroom.detect_content_type(records()) == "json"
    assert headroom.detect_content_type(a_log()) == "log"


def test_multibyte_content_survives_the_boundary():
    # The FFI layer hands Rust a `&str`. Anything that mishandled encoding would show up
    # here rather than as corrupted content in someone's prompt.
    source = "日本語のテキスト 😀 café naïve"
    result = headroom.compress(source)

    assert result.content == source
