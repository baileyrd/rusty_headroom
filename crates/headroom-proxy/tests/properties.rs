//! Property tests — gap row E4.
//!
//! Two properties, checked over generated input rather than hand-picked cases:
//!
//! - The SSE parser never panics, whatever bytes arrive.
//! - Compression never increases the token count.
//!
//! # Why the generator is deterministic
//!
//! A conventional property-test harness seeds from the clock, so a failure reproduces
//! only if the seed is captured and reported. This one is a fixed-seed xorshift: the
//! same inputs run on every machine and every CI job, and a failure is reproducible
//! from the test name alone.
//!
//! The trade is real — a random seed explores more of the space over many runs. But a
//! flaky test that cannot be reproduced gets disabled rather than fixed, and this
//! project's whole thesis is that non-reproducible behavior is the expensive kind. If
//! wider exploration is wanted later, the right move is more seeds enumerated
//! explicitly, not a clock.

use headroom_core::auth_mode::{AuthMode, CompressionPolicy};
use headroom_core::ccr::InMemoryCcrStore;
use headroom_core::tokenizer::{HeuristicEstimator, Tokenizer};
use headroom_proxy::compression::{compress_request, Compressors};
use headroom_proxy::sse::{classify, classify_openai, SseParser, StreamObserver};
use headroom_simulators::fixtures;
use std::sync::Arc;

/// A deterministic pseudo-random source.
///
/// xorshift64* — small, fast, and adequate for generating test bytes. It is not a
/// cryptographic generator and nothing here needs one.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // A zero state is a fixed point for xorshift, so it would emit zeros forever
        // and every generated case would be identical.
        Self(if seed == 0 {
            0x9e37_79b9_7f4a_7c15
        } else {
            seed
        })
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        (self.next() % bound as u64) as usize
    }

    fn byte(&mut self) -> u8 {
        (self.next() & 0xff) as u8
    }
}

// ---- the SSE parser never panics ----

#[test]
fn the_sse_parser_survives_arbitrary_bytes() {
    // Bytes from a network are not a promise. A parser that panics on malformed input
    // takes the whole proxy down mid-stream, which reaches the user as a truncated
    // answer rather than as an error they can retry.
    let mut rng = Rng::new(0x5eed_0001);

    for case in 0..2_000 {
        let length = rng.below(256);
        let bytes: Vec<u8> = (0..length).map(|_| rng.byte()).collect();

        let mut parser = SseParser::new();
        let mut observer = StreamObserver::default();
        for event in parser.feed(&bytes) {
            observer.observe(&event);
            let _ = classify(&event);
            let _ = classify_openai(&event);
        }
        // A second feed, because the parser holds state between calls and a panic on
        // the boundary would otherwise be missed.
        for event in parser.feed(&bytes) {
            let _ = classify(&event);
        }

        assert!(case < 2_000);
    }
}

#[test]
fn the_sse_parser_survives_bytes_biased_toward_frame_syntax() {
    // Uniformly random bytes almost never form a `data:` line, so they exercise the
    // rejection path and little else. This generator emits fragments of real syntax,
    // which is where a state machine actually has states to get wrong.
    const PIECES: [&str; 12] = [
        "event: ",
        "data: ",
        ": ",
        "\n",
        "\r\n",
        "\n\n",
        "{\"type\":",
        "message_start",
        "[DONE]",
        "\"",
        "}",
        "日",
    ];

    let mut rng = Rng::new(0x5eed_0002);

    for _ in 0..2_000 {
        let mut input = String::new();
        for _ in 0..rng.below(24) {
            input.push_str(PIECES[rng.below(PIECES.len())]);
        }

        let mut parser = SseParser::new();
        let mut observer = StreamObserver::default();
        for event in parser.feed(input.as_bytes()) {
            observer.observe(&event);
            let _ = classify(&event);
            let _ = classify_openai(&event);
        }
    }
}

#[test]
fn every_fixture_parses_identically_however_it_is_split() {
    // The property the fixtures exist for. A chunk boundary is a network artifact and
    // must not change what the parser sees — including boundaries inside a multi-byte
    // codepoint, which is where per-chunk `from_utf8` corrupts the model's own output.
    for (name, body) in fixtures::ALL {
        let whole: Vec<String> = {
            let mut parser = SseParser::new();
            parser
                .feed(body.as_bytes())
                .iter()
                .map(|e| e.data.clone())
                .collect()
        };

        for split in 1..body.len() {
            let mut parser = SseParser::new();
            let mut events: Vec<String> = parser
                .feed(&body.as_bytes()[..split])
                .iter()
                .map(|e| e.data.clone())
                .collect();
            events.extend(
                parser
                    .feed(&body.as_bytes()[split..])
                    .iter()
                    .map(|e| e.data.clone()),
            );

            assert_eq!(
                events, whole,
                "{name} parsed differently when split at byte {split}"
            );
        }
    }
}

#[test]
fn every_fixture_parses_identically_one_byte_at_a_time() {
    // The pathological case: a network that delivers a single byte per read. Rare, and
    // exactly the shape that finds a parser assuming a chunk contains a whole field.
    for (name, body) in fixtures::ALL {
        let whole: Vec<String> = {
            let mut parser = SseParser::new();
            parser
                .feed(body.as_bytes())
                .iter()
                .map(|e| e.data.clone())
                .collect()
        };

        let mut parser = SseParser::new();
        let mut events = Vec::new();
        for byte in body.as_bytes() {
            events.extend(parser.feed(&[*byte]).iter().map(|e| e.data.clone()));
        }

        assert_eq!(events, whole, "{name} parsed differently byte-at-a-time");
    }
}

// ---- compression never increases the token count ----

fn compressors() -> Compressors {
    Compressors::new(Arc::new(InMemoryCcrStore::new()))
}

fn payg() -> CompressionPolicy {
    CompressionPolicy::for_mode(AuthMode::PayAsYouGo)
}

/// Generated content of one content type, large enough that a compressor will act on it.
///
/// # Why the sizes are deliberate
///
/// Every compressor holds an `AdaptiveSizer` and declines below its threshold — 1 KB for
/// JSON, 2 KB for code, 5 KB for prose, 500 bytes for the rest. Generated content that
/// does not clear the bar reaches a compressor and is handed straight back, so a property
/// asserted over it is a property of *doing nothing*.
///
/// That is what these tests were. The I4 determinism generator produced random printable
/// ASCII up to 500 characters: measured, all 200 cases detected as prose, and the prose
/// threshold is 5120 bytes, so not one of them compressed. The property asserted that two
/// no-ops agree. I10's generator had the same shape, and I5's covered JSON and logs by
/// accident — half its cases were sub-threshold prose.
///
/// So each arm here is sized past its own threshold, and every test using it asserts that
/// a real share of cases actually changed. A property that never triggers the behaviour
/// it constrains passes forever and means nothing.
fn compressible_shape(rng: &mut Rng) -> String {
    match rng.below(6) {
        0 => {
            let records: Vec<String> = (0..40 + rng.below(200))
                .map(|i| {
                    format!(
                        r#"{{"path":"src/f{i}.rs","size":{},"kind":"file","ok":true}}"#,
                        rng.below(9999)
                    )
                })
                .collect();
            format!("[{}]", records.join(","))
        }
        1 => (0..30 + rng.below(200))
            .map(|i| {
                format!(
                    "2026-01-01T00:00:{:02}Z INFO worker {} handled request {i} in {}ms",
                    i % 60,
                    i % 8,
                    rng.below(500)
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        2 => (0..40 + rng.below(120))
            .map(|i| {
                format!(
                    "src/module_{}.rs:{}:    let parsed = parse(input)?;",
                    i % 12,
                    i + 1
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        3 => (0..20 + rng.below(60))
            .map(|i| {
                format!(
                    "/// Handles request {i}.\n\
                     pub fn handle_{i}(input: &str) -> Result<String, Error> {{\n\
                     \x20   let parsed = parse_{i}(input)?;\n\
                     \x20   let checked = validate(&parsed)?;\n\
                     \x20   Ok(render(&checked))\n\
                     }}\n"
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        4 => {
            // Long context runs either side of the change: eliding context beyond two
            // lines is the whole transform, so tight hunks compress to nothing.
            let context: String = (0..14)
                .map(|line| format!(" \x20   let step_{line} = stage_{line}(&state);\n"))
                .collect();
            (0..6 + rng.below(12))
                .map(|i| {
                    format!(
                        "diff --git a/src/m{i}.rs b/src/m{i}.rs\n\
                         --- a/src/m{i}.rs\n\
                         +++ b/src/m{i}.rs\n\
                         @@ -1,32 +1,32 @@\n\
                         {context}\
                         -    let parsed = parse_old(input)?;\n\
                         +    let parsed = parse_new(input)?;\n\
                         {context}"
                    )
                })
                .collect::<Vec<_>>()
                .join("")
        }
        _ => (0..120 + rng.below(300))
            .map(|i| {
                if i % 29 == 0 {
                    format!("ERROR: batch {i} failed validation: checksum mismatch.")
                } else {
                    "The worker acknowledged the message and kept polling without incident."
                        .to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Builds a request whose newest message carries `content`.
fn request_with(content: &str) -> String {
    let escaped = serde_json::to_string(content).unwrap();
    format!(
        r#"{{"model":"claude-opus-4","messages":[{{"role":"user","content":"earlier"}},{{"role":"assistant","content":"ok"}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t","content":{escaped}}}]}}]}}"#
    )
}

#[test]
fn compression_never_increases_the_token_count() {
    // Invariant I5 as a property rather than as a handful of examples. A compressor
    // that inflates a payload it does not understand is worse than one that declines,
    // because it costs money *and* looks like it worked.
    let estimator = HeuristicEstimator::new();
    let compressors = compressors();
    let mut rng = Rng::new(0x5eed_0003);

    let mut actually_compressed = 0usize;
    for _ in 0..400 {
        // One of every content type that reaches a compressor, each past its own size
        // threshold. This used to be four shapes of which half were sub-threshold prose,
        // so code, diffs and search output were never covered at all.
        let content = compressible_shape(&mut rng);

        let source = request_with(&content);
        let out = compress_request(source.as_bytes(), &compressors, true, payg());

        let before = estimator.count(&source);
        let after = estimator.count(&String::from_utf8_lossy(&out));

        assert!(
            after <= before,
            "compression inflated a payload: {before} -> {after} for {} bytes of shape",
            content.len()
        );
        if out.as_ref() != source.as_bytes() {
            actually_compressed += 1;
        }
    }

    // "Never increases" is satisfied by never changing anything, so the count is what
    // makes the assertion above mean something.
    assert!(
        actually_compressed > 300,
        "only {actually_compressed}/400 cases compressed, so I5 was barely tested"
    );
}

#[test]
fn compression_is_deterministic_over_generated_input() {
    // Invariant I4, over shapes nobody chose by hand.
    let compressors = compressors();
    let mut rng = Rng::new(0x5eed_0004);

    let mut actually_compressed = 0usize;
    for _ in 0..200 {
        let source = request_with(&compressible_shape(&mut rng));

        let first = compress_request(source.as_bytes(), &compressors, true, payg()).into_owned();
        let again = compress_request(source.as_bytes(), &compressors, true, payg()).into_owned();

        assert_eq!(first, again, "compression was not deterministic");
        if first != source.as_bytes() {
            actually_compressed += 1;
        }
    }

    // The assertion above is trivially true when nothing compresses, and that is exactly
    // what this test used to be: random printable ASCII up to 500 characters, all of it
    // detected as prose, against a 5120-byte prose threshold. Not one case compressed, so
    // for as long as it existed it compared two no-ops and passed.
    //
    // Determinism is the one invariant that genuinely differs per compressor — a
    // `HashMap` iterated in the wrong place is nondeterministic in one compressor and not
    // the others — so a generator that reaches none of them is the worst place to have
    // this gap.
    assert!(
        actually_compressed > 150,
        "only {actually_compressed}/200 cases compressed, so determinism was barely tested"
    );
}

#[test]
fn compression_never_errors_on_arbitrary_bodies() {
    // `compress_request` has no failure mode by design: the worst case is that it does
    // nothing. That is what makes it safe on the request path, and it is worth checking
    // against input nobody anticipated rather than trusting the signature.
    let compressors = compressors();
    let mut rng = Rng::new(0x5eed_0005);

    for _ in 0..2_000 {
        let length = rng.below(300);
        let bytes: Vec<u8> = (0..length).map(|_| rng.byte()).collect();

        let out = compress_request(&bytes, &compressors, true, payg());
        // Either it passed the bytes through, or it produced valid JSON. It must never
        // emit something that is neither.
        if out.as_ref() != bytes.as_slice() {
            serde_json::from_slice::<serde_json::Value>(&out)
                .expect("produced output that is neither the input nor valid JSON");
        }
    }
}

#[test]
fn a_restricted_policy_never_modifies_generated_input() {
    // Invariant I10 as a property. Subscription traffic must come back byte-identical
    // whatever arrives, and "whatever arrives" is the part examples cannot cover.
    let compressors = compressors();
    let restricted = CompressionPolicy::for_mode(AuthMode::Subscription);
    let mut rng = Rng::new(0x5eed_0006);

    let mut would_have_compressed = 0usize;
    for _ in 0..400 {
        let source = request_with(&compressible_shape(&mut rng));

        let out = compress_request(source.as_bytes(), &compressors, true, restricted);
        assert_eq!(
            out.as_ref(),
            source.as_bytes(),
            "subscription traffic was modified"
        );

        // The load-bearing half. "Subscription left this alone" says nothing unless
        // something *would* have touched it — and this test used to generate random
        // printable ASCII too short to clear any threshold, so pay-as-you-go would have
        // left it alone as well. It asserted that a policy which forbids compression
        // declines to compress the uncompressible.
        if compress_request(source.as_bytes(), &compressors, true, payg()).as_ref()
            != source.as_bytes()
        {
            would_have_compressed += 1;
        }
    }

    assert!(
        would_have_compressed > 300,
        "only {would_have_compressed}/400 cases were compressible at all, so I10 was \
         barely tested"
    );
}
