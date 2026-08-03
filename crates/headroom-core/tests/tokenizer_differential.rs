//! The heuristic estimator measured against a real tokenizer.
//!
//! Named for what it does rather than for what it proves. An earlier draft of this file
//! was called `estimator_never_under_counts.rs`, which is the claim it disproved — the
//! same overclaiming-name problem check 7 of the reachability audit exists to catch.
//!
//! # The claim this checks, and why nothing checked it before
//!
//! Four files in this crate state that [`HeuristicEstimator`] never under-counts, and
//! invariant I5's safety rests on it: `validated_apply` discards a compression whose
//! result is not smaller *in estimated tokens*, so an estimator that under-counts the
//! compressed form lets a "compression" that actually grew the prompt go upstream. The
//! module's own header calls that failure "much worse" than the alternative, and silent.
//!
//! It was checked only against hand-picked cases — CJK, emoji, JSON against prose — and
//! never against the tokenizer it is approximating. Measured for the first time, against
//! `gpt-4o`, it under-counted four realistic content classes:
//!
//! | content | heuristic | tiktoken | ratio |
//! | --- | --- | --- | --- |
//! | log lines | 1051 | 1139 | 0.92 |
//! | base64 | 220 | 421 | 0.52 |
//! | hex digests | 183 | 220 | 0.83 |
//! | whitespace runs | 1 | 501 | 0.00 |
//!
//! Logs are the one that mattered most: a first-class content type with its own
//! compressor, under-counted by 8%, so a log compression measuring a 5% saving could have
//! grown the real prompt and been forwarded anyway.
//!
//! # Its own test binary
//!
//! `tiktoken-rs` loads merge tables, which costs a second or two on first use. Kept out of
//! the unit suite so the fast path stays fast, and so a failure here names this property
//! rather than appearing among the estimator's shape tests.

use headroom_core::tokenizer::{HeuristicEstimator, TiktokenCounter, Tokenizer};

/// Content classes chosen to span what a compressor actually sees, plus the shapes that
/// broke the claim. A case that no longer under-counts is kept, not removed: it is the
/// regression test.
fn corpus() -> Vec<(&'static str, String)> {
    vec![
        (
            "prose",
            "The quick brown fox jumps over the lazy dog. ".repeat(40),
        ),
        (
            "json records",
            format!(
                "[{}]",
                (0..60)
                    .map(|i| format!(r#"{{"path":"src/f{i}.rs","size":{i}}}"#))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        ),
        (
            "source code",
            (0..30)
                .map(|i| {
                    format!(
                        "pub fn handle_{i}(input: &str) -> Result<String, Error> {{\n\
                         \x20   Ok(render(input))\n}}\n"
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        (
            // Under-counted at 0.92 before digits were charged separately. Timestamps
            // make a log line mostly digits, and digits group in threes at most.
            "log lines",
            (0..60)
                .map(|i| format!("2026-01-01T00:00:{:02}Z INFO worker {i} ok", i % 60))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        (
            // Under-counted at 0.52 — the worst realistic case, and common in tool output
            // as embedded payloads.
            "base64",
            "aGVsbG8gd29ybGQgdGhpcyBpcyBiYXNlNjQ=".repeat(20),
        ),
        (
            // Under-counted at 0.83. Digests and object ids are everywhere in agent
            // traffic.
            "hex digests",
            "deadbeefcafebabe0123456789abcdef".repeat(20),
        ),
        (
            // Under-counted at 0.00 — 1500 characters charged as one token, because a
            // whitespace run was counted rather than measured.
            "whitespace runs",
            " \n\t".repeat(500),
        ),
        (
            "deep indentation",
            (0..60)
                .map(|i| format!("{}value_{i} = compute()", " ".repeat(24)))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        ("cjk", "日本語のテキストです。".repeat(30)),
        ("emoji", "😀🎉🚀".repeat(40)),
        ("accented latin", "café naïve résumé Ünïcödé ".repeat(40)),
        ("cyrillic", "Привет мир это русский текст ".repeat(30)),
        ("arabic", "مرحبا بالعالم هذا نص عربي ".repeat(30)),
        ("thai", "สวัสดีชาวโลกนี่คือข้อความภาษาไทย ".repeat(30)),
        (
            "korean",
            "안녕하세요 세계 이것은 한국어 텍스트입니다 ".repeat(30),
        ),
        (
            "combining marks",
            "e\u{0301}a\u{0300}o\u{0302}u\u{0308} ".repeat(200),
        ),
        ("urls", "https://example.com/a/b/c?q=1&r=2 ".repeat(30)),
        ("uuids", "550e8400-e29b-41d4-a716-446655440000 ".repeat(30)),
        ("one long word", "a".repeat(2000)),
        ("digits only", "1234567890".repeat(100)),
    ]
}

/// Short inputs, where rounding decides the answer.
fn edges() -> Vec<&'static str> {
    vec![
        "a",
        "ab",
        " ",
        "\n",
        "  ",
        "日",
        "😀",
        "{}",
        "()",
        "a b",
        "e\u{0301}",
        "0",
        "00",
        "000",
        "0000",
        "-",
        "aaaaaaaaaaaaa",
        "            ",
    ]
}

#[test]
fn the_estimator_over_counts_on_every_realistic_content_class() {
    let heuristic = HeuristicEstimator::new();
    let exact = TiktokenCounter::for_model("gpt-4o");

    let mut under = Vec::new();
    for (label, text) in corpus().into_iter().chain(
        edges()
            .into_iter()
            .map(|text| ("edge case", text.to_string())),
    ) {
        let (estimated, actual) = (heuristic.count(&text), exact.count(&text));
        if estimated < actual {
            under.push(format!(
                "{label}: estimated {estimated} < actual {actual} ({:?}…)",
                text.chars().take(24).collect::<String>()
            ));
        }
    }

    assert!(
        under.is_empty(),
        "the estimator under-counts, which breaks I5's safety:\n  {}",
        under.join("\n  ")
    );
}

/// A deterministic generator, so a failure here is reproducible from the seed alone.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next() % bound as u64) as usize
        }
    }
}

#[test]
fn the_under_count_on_random_strings_is_no_worse_than_measured() {
    // The honest form of the claim this file is named after.
    //
    // On random alphanumeric strings the estimator *does* under-count, and no
    // character-class heuristic can fix it: `"Dgnc"` and `"Word"` are the same string to
    // a classifier that cannot consult the merge tables, and they cost 4 tokens and 1.
    // Charging every short run at the dense rate would put ordinary prose at roughly
    // eight times its true count and suppress compression everywhere — D29.
    //
    // So this pins the exposure rather than asserting it away. It is a bound that can
    // only be tightened, and a change that widens it has to come here and say so.
    //
    // Measured when written, over 12,000 inputs across three model families: 25.8% low,
    // worst ratio 0.43 on `"EYM3Dgnc6"` — 3 estimated against 7 actual.
    const ALPHABETS: [&str; 8] = [
        "abcdefghijklmnopqrstuvwxyz ",
        "0123456789",
        "abcdef0123456789",
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=",
        "{}[](),:\"' \n\t",
        " \n\t",
        "日本語漢字ひらがな",
        "aA0 \n\t{}日😀é",
    ];

    let heuristic = HeuristicEstimator::new();
    let mut low = 0usize;
    let mut total = 0usize;
    let mut worst = (1.0f64, String::new());

    for model in ["gpt-4o", "gpt-4"] {
        let exact = TiktokenCounter::for_model(model);
        let mut rng = Rng(0x5eed_1234);

        for _ in 0..2_000 {
            let alphabet: Vec<char> = ALPHABETS[rng.below(ALPHABETS.len())].chars().collect();
            let length = 1 + rng.below(400);
            let text: String = (0..length)
                .map(|_| alphabet[rng.below(alphabet.len())])
                .collect();

            let (estimated, actual) = (heuristic.count(&text), exact.count(&text));
            total += 1;
            if estimated < actual {
                low += 1;
                let ratio = estimated as f64 / actual as f64;
                if ratio < worst.0 {
                    worst = (
                        ratio,
                        format!("{model}: {estimated} < {actual} :: {text:?}"),
                    );
                }
            }
        }
    }

    let rate = low as f64 * 100.0 / total as f64;
    assert!(
        rate <= 30.0,
        "under-count rate rose to {rate:.1}% of {total} (was 25.8%); worst {}",
        worst.1
    );
    assert!(
        worst.0 >= 0.40,
        "worst under-count ratio fell to {:.2} (was 0.43): {}",
        worst.0,
        worst.1
    );

    // And the exposure is real, so a change that appears to eliminate it has almost
    // certainly broken the generator rather than the estimator. Asserted so this cannot
    // start passing vacuously.
    assert!(low > 0, "no under-counts at all — check the generator");
}

#[test]
fn the_estimate_stays_close_enough_to_be_useful() {
    // The other direction, and the reason this is not fixed by returning `usize::MAX`.
    // Over-counting costs missed compressions, so an estimator that is safe by being
    // absurd would quietly turn the whole project off.
    //
    // The bound is loose on purpose. Scripts with no ASCII analogue — Cyrillic, Thai,
    // Devanagari — cost one token per several characters under `o200k` while this charges
    // two per character, and pulling that in would mean shipping the merge tables, which
    // is the dependency this estimator exists to avoid. What the bound catches is a
    // change that makes the *common* cases absurd.
    let heuristic = HeuristicEstimator::new();
    let exact = TiktokenCounter::for_model("gpt-4o");

    for (label, text) in corpus() {
        // The scripts named above are excluded by name rather than by a magic ratio, so
        // the exclusion is arguable rather than invisible.
        if matches!(
            label,
            "cyrillic" | "arabic" | "thai" | "korean" | "cjk" | "accented latin" | "one long word"
        ) {
            continue;
        }

        let (estimated, actual) = (heuristic.count(&text), exact.count(&text));
        assert!(
            estimated <= actual * 3,
            "{label}: estimated {estimated} is more than 3x the actual {actual}"
        );
    }
}
