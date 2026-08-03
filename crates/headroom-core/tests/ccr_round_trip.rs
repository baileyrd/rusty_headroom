//! Every lossy compressor's marker must be redeemable.
//!
//! # The promise this checks
//!
//! README.md: *"Anything lossy is stored first under a content hash, and the compressed
//! block carries a `<<ccr:HASH>>` marker the model can redeem through the
//! `headroom_retrieve` MCP tool. So compression is a bet that the detail will not be
//! needed, and a bet that can be unwound."*
//!
//! The bet is only unwindable if the marker parses and the store holds what it points at.
//! A compressor that formatted its marker slightly differently, or stored the wrong bytes,
//! would lose content permanently while every other test passed — the compressed output
//! would still be smaller, still be valid, and still look right.
//!
//! # Why one test rather than six
//!
//! Three of the six lossy compressors had a marker test — SmartCrusher, the log
//! compressor and the search compressor. Code, diffs and prose did not. Adding three more
//! per-compressor tests would leave the same gap open for the seventh compressor, so this
//! walks whatever the orchestrator routes to instead: a new content type with a
//! compressor is covered the day it is wired, and one with no sample fails the coverage
//! assertion at the bottom.

use std::sync::Arc;

use headroom_core::auth_mode::{AuthMode, CompressionPolicy};
use headroom_core::ccr::{handle_retrieve, parse_marker, InMemoryCcrStore, Retrieval};
use headroom_core::detection::ContentType;
use headroom_core::pipeline::Orchestrator;
use headroom_core::tokenizer::HeuristicEstimator;
use headroom_core::{validated_apply, Block, BlockKind};

/// One sample per content type that reaches a lossy compressor, each large enough to
/// clear that compressor's size threshold and compressible enough to be worth storing.
fn samples() -> Vec<(&'static str, String)> {
    vec![
        (
            "json",
            format!(
                "[{}]",
                (0..80)
                    .map(|i| format!(r#"{{"id":{i},"kind":"file","ok":true}}"#))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        ),
        (
            "log",
            (0..200)
                .map(|i| format!("2026-08-03T12:00:00Z INFO worker {i} handled a request"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        (
            "code",
            (0..60)
                .map(|i| {
                    format!(
                        "/// Handles {i}.\n\
                         pub fn handle_{i}(input: &str) -> Result<String, Error> {{\n\
                         \x20   let parsed = parse_{i}(input)?;\n\
                         \x20   let checked = validate(&parsed)?;\n\
                         \x20   Ok(render(&checked))\n\
                         }}\n"
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        (
            "prose",
            (0..300)
                .map(|i| format!("The quick brown fox jumps over the lazy dog, sentence {i}."))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        (
            "diff",
            (0..12)
                .map(|i| {
                    let context: String = (0..14)
                        .map(|line| format!(" \x20   let step_{line} = stage_{line}(&state);\n"))
                        .collect();
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
                .join(""),
        ),
        (
            "search",
            (0..120)
                .map(|i| {
                    format!(
                        "src/module_{}.rs:{}:    let parsed = parse(input)?;",
                        i % 12,
                        i + 1
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    ]
}

/// Every `<<ccr:...>>` marker in `text`, in the order they appear.
fn markers_in(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;

    while let Some(start) = rest.find("<<ccr:") {
        let after = &rest[start..];
        let Some(end) = after.find(">>") else { break };
        found.push(after[..end + 2].to_string());
        rest = &after[end + 2..];
    }
    found
}

#[test]
fn every_lossy_compressor_leaves_a_marker_that_redeems_the_original() {
    let store = Arc::new(InMemoryCcrStore::new());
    let orchestrator = Orchestrator::new(store.clone());
    let policy = CompressionPolicy::for_mode(AuthMode::PayAsYouGo);
    let estimator = HeuristicEstimator::new();

    let mut covered = Vec::new();

    for (label, source) in samples() {
        let mut block = Block::new(BlockKind::ToolResult, source.clone());
        let transform = orchestrator
            .transform_for_block(&block, policy, "")
            .unwrap_or_else(|| panic!("{label} reaches no compressor"));

        let outcome = validated_apply(transform, &mut block, &estimator)
            .unwrap_or_else(|err| panic!("{label} failed to compress: {err}"));
        assert!(
            outcome.is_compressed(),
            "{label} did not compress, so there is no marker to redeem"
        );
        covered.push(transform.name());

        let markers = markers_in(block.content());
        assert!(
            !markers.is_empty(),
            "{label} compressed with {} and left no marker — the original is unrecoverable",
            transform.name()
        );

        for marker in markers {
            let hash = parse_marker(&marker).unwrap_or_else(|err| {
                panic!("{label} wrote an unparseable marker {marker}: {err}")
            });

            match handle_retrieve(store.as_ref(), &hash.to_hex()) {
                Retrieval::Found(bytes) => assert_eq!(
                    bytes,
                    source.as_bytes(),
                    "{label} redeemed something other than what it was given"
                ),
                other => panic!("{label}'s marker {marker} did not redeem: {other:?}"),
            }
        }
    }

    // A lossy compressor with no sample here is one whose markers nobody checks, which is
    // the state code, diffs and prose were in. The reformatter is excluded because it is
    // lossless and stores nothing — there is no bet to unwind.
    for content_type in ContentType::ALL {
        let Some(transform) = orchestrator.for_type(content_type) else {
            continue;
        };
        assert!(
            covered.contains(&transform.name()),
            "{} compresses {} and no sample here redeems its marker",
            transform.name(),
            content_type.as_str()
        );
    }
}

#[test]
fn a_marker_from_one_compressor_is_not_redeemable_from_an_empty_store() {
    // The guard that keeps the test above honest. If `handle_retrieve` answered `Found`
    // for anything, every assertion up there would pass without the store doing its job.
    let store = Arc::new(InMemoryCcrStore::new());
    let orchestrator = Orchestrator::new(store.clone());
    let policy = CompressionPolicy::for_mode(AuthMode::PayAsYouGo);
    let estimator = HeuristicEstimator::new();

    let (_, source) = samples().into_iter().next().expect("no samples");
    let mut block = Block::new(BlockKind::ToolResult, source);
    let transform = orchestrator
        .transform_for_block(&block, policy, "")
        .expect("no compressor");
    validated_apply(transform, &mut block, &estimator).expect("compression failed");

    let marker = markers_in(block.content())
        .into_iter()
        .next()
        .expect("no marker");
    let hash = parse_marker(&marker).expect("unparseable marker");

    // A different store, which never saw this content.
    let elsewhere = InMemoryCcrStore::new();
    assert!(
        !matches!(
            handle_retrieve(&elsewhere, &hash.to_hex()),
            Retrieval::Found(_)
        ),
        "an empty store answered Found, so the test above proves nothing"
    );
}
