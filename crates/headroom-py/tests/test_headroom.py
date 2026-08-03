"""Smoke tests for the Python bindings — gap rows B1 and B2.

These run against an installed wheel, which is the point: the Rust unit tests already
cover the engine, and what is unproven until a wheel is built and imported is that the
*boundary* works. `DECISIONS.md` D4 deferred these bindings on the grounds that they
"would compile at best and never be exercised" — this file is what makes that no longer
true.

Run with:  maturin build --release && pip install <wheel> && pytest
"""

import json

import pytest

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
    assert result.reason == "policy_forbids"


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
    assert headroom.compress("   \n\t  \n").reason == "no_compressor"
    assert headroom.compress("short prose").reason == "not_smaller"
    assert headroom.compress(a_log(), auth_mode="subscription").reason == "policy_forbids"


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


def test_every_reason_is_one_the_module_declares():
    # `reason` is only useful if a caller can enumerate what it might be, and this module
    # used to map the routing variants itself — spelling three of six with hyphens, so a
    # Python result read `policy-forbids` while the proxy's
    # `headroom_routing_total{reason="policy_forbids"}` and `headroom inspect` both said
    # otherwise. Anyone correlating the two matched nothing.
    #
    # REASONS is built from Routing::REASONS in Rust. Asserting every observed reason is
    # in it ties this surface to core across the FFI boundary, where no compiler looks.
    observed = {
        headroom.compress(records()).reason,
        headroom.compress("   \n\t  \n").reason,
        headroom.compress("short prose").reason,
        headroom.compress(a_log(), auth_mode="subscription").reason,
        headroom.compress(a_log(), auth_mode="oauth").reason,
    }

    assert observed <= set(headroom.REASONS), observed - set(headroom.REASONS)
    # Not vacuous: the calls above have to actually produce distinct reasons, or an
    # empty-ish set would satisfy the subset check while proving nothing.
    assert len(observed) >= 4, observed
    # And the vocabulary is underscored throughout, which is what the drift was about.
    assert not any("-" in reason for reason in headroom.REASONS), headroom.REASONS


def test_typed_text_is_not_summarised_but_tool_output_is():
    """The prose summariser is lossy and this module's CCR store is discarded, so
    compressing somebody's own words here loses them for good. The proxy declines that
    even though its store persists; this used to do it anyway, because it built a text
    block and then routed with a call that ignores block kind.
    """
    prose = "\n".join(
        f"The quick brown fox jumps over the lazy dog, sentence {i}." for i in range(300)
    )

    # The control. Without it, a build where nothing compresses at all would satisfy
    # every assertion below.
    as_tool_output = headroom.compress(prose, kind="tool_output")
    assert as_tool_output.compressed, "nothing compressed, so declining proves nothing"
    assert len(as_tool_output.content) < len(prose)

    as_text = headroom.compress(prose, kind="text")
    assert not as_text.compressed
    assert as_text.content == prose, "typed text came back altered"
    assert as_text.reason == "tool_output_only"
    assert as_text.reason in headroom.REASONS, "a reason absent from the documented list"


def test_kind_defaults_to_tool_output():
    """Existing callers passed no kind and got prose compression; that still holds."""
    prose = "\n".join(
        f"The quick brown fox jumps over the lazy dog, sentence {i}." for i in range(300)
    )
    assert headroom.compress(prose).compressed


def test_an_unknown_kind_is_refused_rather_than_guessed():
    with pytest.raises(ValueError):
        headroom.compress("hello", kind="tool-output")
