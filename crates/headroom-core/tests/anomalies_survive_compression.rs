//! The weird record has to come out the other side.
//!
//! # The promise this checks
//!
//! `signals`' bias, which the whole pipeline inherits: *"A line wrongly kept costs a few
//! tokens; a line wrongly dropped may be the error the user is looking for."* SmartCrusher
//! elides the bulk of a uniform array and keeps what stands out — that keeping is the
//! entire reason it is safe to elide anything.
//!
//! # Why an end-to-end test when `outliers` has eighteen of its own
//!
//! Those prove the detector works. They do not prove it is consulted on the path a request
//! actually takes, which is this repository's most-repeated failure — five capabilities
//! shipped, tested, documented, and reached by nothing. Detection and routing are wired
//! together by a threshold, and a threshold is exactly the kind of thing that gets tuned
//! without anyone noticing that tuning it past a point turns a feature off.
//!
//! So this goes through the orchestrator, the way a tool result does.

use std::sync::Arc;

use headroom_core::auth_mode::{AuthMode, CompressionPolicy};
use headroom_core::ccr::InMemoryCcrStore;
use headroom_core::pipeline::Orchestrator;
use headroom_core::tokenizer::HeuristicEstimator;
use headroom_core::{validated_apply, Block, BlockKind};

/// 300 near-identical records with three planted anomalies, one of each kind the detector
/// distinguishes: a rare enum value, a numeric outlier, and a field nothing else carries.
fn array_with_planted_anomalies() -> String {
    let mut records: Vec<String> = (0..300)
        .map(|i| {
            format!(
                r#"{{"id":{i},"path":"src/module_{i}.rs","status":"ok","size":{}}}"#,
                1000 + i % 50
            )
        })
        .collect();

    records[42] =
        r#"{"id":42,"path":"src/odd.rs","status":"ok","size":1000,"quarantined":true}"#.to_owned();
    records[137] = r#"{"id":137,"path":"src/broken.rs","status":"error","size":1024}"#.to_owned();
    records[201] = r#"{"id":201,"path":"src/huge.rs","status":"ok","size":99999999}"#.to_owned();

    format!("[{}]", records.join(","))
}

/// Routes `source` the way a tool result is routed, and returns what came out.
fn compressed(source: &str) -> String {
    let orchestrator = Orchestrator::new(Arc::new(InMemoryCcrStore::new()));
    let mut block = Block::new(BlockKind::ToolResult, source.to_owned());

    let transform = orchestrator
        .transform_for_block(
            &block,
            CompressionPolicy::for_mode(AuthMode::PayAsYouGo),
            "",
        )
        .expect("nothing routes this array to a compressor");
    let outcome = validated_apply(transform, &mut block, &HeuristicEstimator::new())
        .expect("compression failed");

    assert!(
        outcome.is_compressed(),
        "nothing was compressed, so surviving the compressor means nothing"
    );
    block.content().to_owned()
}

#[test]
fn every_kind_of_planted_anomaly_survives_the_compressor() {
    let source = array_with_planted_anomalies();
    let digest = compressed(&source);

    // Without this the test would pass on any build that simply kept all 300 records —
    // which is not compression, and is the shape a disabled elision threshold takes.
    assert!(
        !digest.contains("src/module_299.rs"),
        "nothing was elided, so keeping the anomalies proves nothing:\n{digest}"
    );
    assert!(
        digest.len() * 10 < source.len(),
        "the digest is {} bytes from {} — too little was elided to be a real test",
        digest.len(),
        source.len()
    );

    for (label, needle) in [
        ("a field no other record carries", "quarantined"),
        ("the record carrying it", "src/odd.rs"),
        ("a rare enum value", "\"error\""),
        ("the record carrying it", "src/broken.rs"),
        ("a numeric outlier", "99999999"),
        ("the record carrying it", "src/huge.rs"),
    ] {
        assert!(
            digest.contains(needle),
            "{label} ({needle}) was elided — the one record worth reading is the one that \
             went missing:\n{digest}"
        );
    }
}

#[test]
fn the_digest_accounts_for_every_record_it_elided() {
    // A digest that says "300 records, 6 shown, 294 elided" is making an arithmetic claim
    // to the model. Wrong numbers there are worse than no numbers: they are a fabricated
    // fact about content the reader can no longer see for themselves.
    let digest = compressed(&array_with_planted_anomalies());
    let header = digest.lines().next().unwrap_or_default().to_owned();

    let numbers: Vec<usize> = header
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect();

    assert_eq!(
        numbers.len(),
        3,
        "expected a total, a shown count and an elided count in {header:?}"
    );
    let (total, shown, elided) = (numbers[0], numbers[1], numbers[2]);

    assert_eq!(total, 300, "the digest miscounted the array: {header:?}");
    assert_eq!(
        shown + elided,
        total,
        "{shown} shown plus {elided} elided is not {total}: {header:?}"
    );
    assert_eq!(
        digest
            .lines()
            .filter(|line| line.starts_with(char::is_numeric) && line.contains("{\""))
            .count(),
        shown,
        "the digest claimed to show {shown} records and did not: {digest}"
    );
}
