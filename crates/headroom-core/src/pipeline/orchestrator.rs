//! Choosing a compressor for a block — gap row P3.
//!
//! # One decision, one place
//!
//! Detection says what content *is*; this says what to *do* about it. Keeping those
//! separate matters because the second answer depends on things the first knows nothing
//! about — the auth policy, the safety limits, and which compressors this build has.
//!
//! # Order of the gates
//!
//! Policy, then safety, then type. Policy first because it is the cheapest and the most
//! consequential: invariant I10 forbids lossy work on restricted traffic outright, so a
//! restricted request should never pay for a safety scan or a content sniff to reach
//! the same conclusion.

use std::sync::Arc;

use crate::auth_mode::{AuthMode, CompressionPolicy};
use crate::block::Block;
use crate::ccr::CcrStore;
use crate::detection::{detect, ContentType};
use crate::pipeline::reformats::Reformatter;
use crate::pipeline::safety::{check, Hazard, Limits};
use crate::telemetry::{AggregationKey, Recommendations, StructureHash};
use crate::tokenizer::registry::Registry;
use crate::tokenizer::Tokenizer;
use crate::transform::Transform;
use crate::{DiffCompressor, LogCompressor, SearchCompressor, SmartCrusher};

/// What the orchestrator decided to do with a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routing {
    /// Run the compressor for this content type.
    Compress {
        /// What the content was detected as.
        content_type: ContentType,
    },
    /// Run the lossless reformatter only.
    ///
    /// What OAuth traffic gets. Policy forbids lossy work, but reformatting preserves
    /// the decoded meaning exactly, and the OAuth hazard is a modification exceeding
    /// the granted scope — which a meaning-preserving change cannot do.
    Lossless,
    /// Forward unchanged: policy permits no transform at all.
    ///
    /// Subscription mode. Not a limitation of this crate — reflowing bytes is visible
    /// to a provider comparing proxied traffic against unproxied, so subscription buys
    /// safety by giving up compression.
    PolicyForbids,
    /// Forward unchanged: the payload would be unsafe or pointless to analyze.
    Unsafe {
        /// Which limit it exceeded.
        hazard: Hazard,
    },
    /// Forward unchanged: no compressor handles this content type.
    NoCompressor {
        /// What the content was detected as.
        content_type: ContentType,
    },
    /// Forward unchanged: a previous run measured this shape as not worth compressing.
    ///
    /// Distinguished from [`Routing::NoCompressor`] because the cause is opposite. A
    /// compressor *does* handle this content; it was tried, repeatedly, and did not
    /// help. An operator seeing this should look at the recommendations file, not at the
    /// compressor list.
    MeasuredUseless {
        /// What the content was detected as.
        content_type: ContentType,
    },
}

impl Routing {
    /// Whether any compressor should run.
    pub fn will_compress(self) -> bool {
        matches!(self, Self::Compress { .. } | Self::Lossless)
    }

    /// Whether the transform that will run is lossy.
    ///
    /// Separate from [`Routing::will_compress`] because a caller enforcing I10 needs to
    /// know *which* kind ran, and a single boolean would make "compressed" mean two
    /// different things.
    pub fn is_lossy(self) -> bool {
        matches!(self, Self::Compress { .. })
    }

    /// A stable identifier, for telemetry.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compress { .. } => "compress",
            Self::Lossless => "lossless",
            Self::PolicyForbids => "policy_forbids",
            Self::Unsafe { .. } => "unsafe",
            Self::NoCompressor { .. } => "no_compressor",
            Self::MeasuredUseless { .. } => "measured_useless",
        }
    }
}

/// The compressor set, and the decision about which one applies.
pub struct Orchestrator {
    smart_crusher: SmartCrusher,
    log: LogCompressor,
    search: SearchCompressor,
    diff: DiffCompressor,
    reformatter: Reformatter,
    limits: Limits,
    tokenizers: Registry,
    recommendations: Recommendations,
}

impl Orchestrator {
    /// Builds the set, sharing one CCR store between every compressor.
    ///
    /// One store rather than one each: a marker written by the log compressor has to be
    /// retrievable through the same `ccr_retrieve` call as one written by SmartCrusher,
    /// and separate stores would make retrieval depend on which compressor happened to
    /// run.
    pub fn new(store: Arc<dyn CcrStore>) -> Self {
        Self {
            smart_crusher: SmartCrusher::new(store.clone()),
            log: LogCompressor::new(store.clone()),
            search: SearchCompressor::new(store.clone()),
            diff: DiffCompressor::new(store),
            reformatter: Reformatter::new(),
            limits: Limits::default(),
            // The exact OpenAI counters are registered by default. They are already
            // compiled in — leaving them unregistered would mean carrying the
            // vocabularies and then not using them, which is the worst of both.
            tokenizers: Registry::with_defaults(),
            // Empty by default. An unmeasured shape is always attempted — see
            // `Recommendations::worth_compressing`.
            recommendations: Recommendations::default(),
        }
    }

    /// Uses `limits` instead of the defaults.
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Uses `recommendations` learned from a previous run.
    ///
    /// # Read once, never per request
    ///
    /// The recommendations are configuration, fixed for the process lifetime. Consulting
    /// a *live* aggregate per request would make compression depend on accumulated
    /// history — the same request would compress differently depending on what came
    /// before it, and a failure could not be reproduced from the failing request alone.
    /// That is invariant I4, and it is why this is a constructor rather than a setter on
    /// a shared handle.
    pub fn with_recommendations(mut self, recommendations: Recommendations) -> Self {
        self.recommendations = recommendations;
        self
    }

    /// The tokenizer to measure `model` with.
    ///
    /// Exact where one is registered, and the heuristic upper bound otherwise. The
    /// distinction matters to invariant I5: an exact count lets a compressor keep a
    /// result the heuristic's over-count would have rejected.
    pub fn tokenizer_for(&self, model: &str) -> Arc<dyn Tokenizer> {
        self.tokenizers.for_model(model)
    }

    /// Decides what to do with `content` under `policy`.
    ///
    /// # Example
    ///
    /// ```
    /// use std::sync::Arc;
    /// use headroom_core::auth_mode::{AuthMode, CompressionPolicy};
    /// use headroom_core::ccr::InMemoryCcrStore;
    /// use headroom_core::pipeline::Orchestrator;
    ///
    /// let orchestrator = Orchestrator::new(Arc::new(InMemoryCcrStore::new()));
    /// let payg = CompressionPolicy::for_mode(AuthMode::PayAsYouGo);
    /// let restricted = CompressionPolicy::for_mode(AuthMode::Subscription);
    ///
    /// let records: String = (0..80)
    ///     .map(|i| format!(r#"{{"id":{i},"kind":"file"}},"#))
    ///     .collect();
    /// let json = format!("[{}]", records.trim_end_matches(','));
    ///
    /// assert!(orchestrator.route(&json, payg, "claude-opus-4").will_compress());
    /// assert!(!orchestrator.route(&json, restricted, "claude-opus-4").will_compress());
    /// ```
    pub fn route(&self, content: &str, policy: CompressionPolicy, model: &str) -> Routing {
        let content_type = detect(content.as_bytes()).content_type;

        // The safety check runs before the policy branch, because both branches lead to
        // a transform that walks the content. A restricted request is not exempt from
        // being handed a pathological payload.
        if let Some(hazard) = check(content, content_type, self.limits) {
            return Routing::Unsafe { hazard };
        }

        // OAuth traffic gets the lossless reformatter rather than nothing: the hazard
        // there is a modification falling outside the granted scope, and a
        // meaning-preserving reformat cannot exceed a scope.
        //
        // Subscription traffic gets neither, and that is deliberate. Reflowing the
        // bytes of a request preserves its meaning but changes how the client *looks* —
        // the same fingerprint-class disclosure that keeps `accept-encoding` untouched.
        if !policy.lossy_transforms {
            return if policy.lossless_transforms {
                Routing::Lossless
            } else {
                Routing::PolicyForbids
            };
        }

        match content_type {
            ContentType::Json
            | ContentType::Log
            | ContentType::SearchResults
            | ContentType::Diff => {
                // Checked last, so a shape only reaches this gate if a compressor would
                // otherwise have run. An unmeasured shape is always attempted: skipping
                // it would never gather the data that would let it be skipped for a
                // reason.
                let key = AggregationKey::new(
                    AuthMode::PayAsYouGo,
                    model,
                    StructureHash::of(content, content_type),
                );
                if self.recommendations.worth_compressing(&key) {
                    Routing::Compress { content_type }
                } else {
                    Routing::MeasuredUseless { content_type }
                }
            }
            other => Routing::NoCompressor {
                content_type: other,
            },
        }
    }

    /// The transform for `content`, if one applies.
    ///
    /// A thin wrapper over [`Orchestrator::route`] for callers that want the compressor
    /// rather than the reason. Callers recording telemetry should use `route`, since
    /// `None` here collapses three genuinely different outcomes into one.
    pub fn transform_for(
        &self,
        content: &str,
        policy: CompressionPolicy,
        model: &str,
    ) -> Option<&dyn Transform> {
        match self.route(content, policy, model) {
            Routing::Compress { content_type } => self.for_type(content_type),
            Routing::Lossless => Some(&self.reformatter),
            _ => None,
        }
    }

    /// The compressor registered for `content_type`.
    fn for_type(&self, content_type: ContentType) -> Option<&dyn Transform> {
        match content_type {
            ContentType::Json => Some(&self.smart_crusher),
            ContentType::Log => Some(&self.log),
            ContentType::SearchResults => Some(&self.search),
            ContentType::Diff => Some(&self.diff),
            _ => None,
        }
    }

    /// Whether a block is worth offering to a compressor at all.
    ///
    /// Sacrosanct blocks — signed thinking, encrypted content — are excluded here as
    /// well as by the live-zone dispatcher. Two independent checks for invariant I8 is
    /// deliberate: this one is cheap, and the cost of the guarantee failing is content
    /// the provider will reject as tampered-with.
    pub fn is_eligible(block: &Block) -> bool {
        !block.kind().is_sacrosanct()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth_mode::AuthMode;
    use crate::block::BlockKind;
    use crate::ccr::InMemoryCcrStore;

    fn orchestrator() -> Orchestrator {
        Orchestrator::new(Arc::new(InMemoryCcrStore::new()))
    }

    fn payg() -> CompressionPolicy {
        CompressionPolicy::for_mode(AuthMode::PayAsYouGo)
    }

    fn bulky_json() -> String {
        let records: Vec<String> = (0..120)
            .map(|i| format!(r#"{{"path":"src/module_{i}.rs","size":{i}}}"#))
            .collect();
        format!("[{}]", records.join(","))
    }

    fn bulky_log() -> String {
        (0..200)
            .map(|i| format!("2026-01-01T00:00:{:02}Z INFO worker {i} ok\n", i % 60))
            .collect()
    }

    // ---- routing ----

    #[test]
    fn structured_content_routes_to_a_compressor() {
        let orchestrator = orchestrator();
        for content in [bulky_json(), bulky_log()] {
            assert!(
                orchestrator
                    .route(&content, payg(), "claude-opus-4")
                    .will_compress(),
                "{:?}",
                orchestrator.route(&content, payg(), "claude-opus-4")
            );
            assert!(orchestrator
                .transform_for(&content, payg(), "claude-opus-4")
                .is_some());
        }
    }

    #[test]
    fn oauth_traffic_gets_the_lossless_reformatter_rather_than_nothing() {
        // The OAuth hazard is a modification exceeding the granted scope, and a
        // meaning-preserving reformat cannot exceed a scope.
        let orchestrator = orchestrator();
        let policy = CompressionPolicy::for_mode(AuthMode::OAuth);
        let routing = orchestrator.route(&bulky_json(), policy, "claude-opus-4");

        assert_eq!(routing, Routing::Lossless);
        assert!(routing.will_compress());
        assert!(!routing.is_lossy(), "OAuth was routed lossy work");
    }

    #[test]
    fn subscription_traffic_gets_no_transform_at_all() {
        // The tempting mistake this guards: "lossless preserves meaning, so it must be
        // safe everywhere". Reflowing bytes is visible to a provider comparing proxied
        // traffic against unproxied — the same fingerprint-class disclosure that keeps
        // `accept-encoding` untouched on this mode.
        let orchestrator = orchestrator();
        let policy = CompressionPolicy::for_mode(AuthMode::Subscription);

        assert_eq!(
            orchestrator.route(&bulky_json(), policy, "claude-opus-4"),
            Routing::PolicyForbids
        );
        assert!(orchestrator
            .transform_for(&bulky_json(), policy, "claude-opus-4")
            .is_none());
    }

    #[test]
    fn a_restricted_request_is_still_protected_by_the_safety_check() {
        // Both branches lead to a transform that walks the content, so a restricted
        // request is not exempt from being handed a pathological payload.
        let orchestrator = orchestrator();
        let restricted = CompressionPolicy::for_mode(AuthMode::Subscription);
        let deep = format!("{}1{}", "[".repeat(500), "]".repeat(500));

        assert!(matches!(
            orchestrator.route(&deep, restricted, "claude-opus-4"),
            Routing::Unsafe { .. }
        ));
    }

    #[test]
    fn only_pay_as_you_go_is_routed_lossy_work() {
        // Invariant I10, stated as the property rather than as three separate cases.
        let orchestrator = orchestrator();

        for mode in [AuthMode::Subscription, AuthMode::OAuth] {
            assert!(
                !orchestrator
                    .route(
                        &bulky_json(),
                        CompressionPolicy::for_mode(mode),
                        "claude-opus-4"
                    )
                    .is_lossy(),
                "{mode:?}"
            );
        }
        assert!(orchestrator
            .route(&bulky_json(), payg(), "claude-opus-4")
            .is_lossy());
    }

    #[test]
    fn an_unsafe_payload_is_declined_with_its_reason() {
        let orchestrator = orchestrator();
        let deep = format!("{}1{}", "[".repeat(500), "]".repeat(500));

        assert!(matches!(
            orchestrator.route(&deep, payg(), "claude-opus-4"),
            Routing::Unsafe { .. }
        ));
        assert!(orchestrator
            .transform_for(&deep, payg(), "claude-opus-4")
            .is_none());
    }

    #[test]
    fn content_with_no_compressor_is_reported_as_such() {
        // Distinguished from a policy decline and from a hazard, because an operator
        // reading telemetry needs to tell "nothing handles this" from "we were not
        // allowed to" — the fixes are entirely different.
        let orchestrator = orchestrator();
        let prose = "The quick brown fox jumps over the lazy dog. ".repeat(50);

        assert!(matches!(
            orchestrator.route(&prose, payg(), "claude-opus-4"),
            Routing::NoCompressor { .. } | Routing::Unsafe { .. }
        ));
    }

    #[test]
    fn the_three_declines_are_distinguishable() {
        // `transform_for` collapses them into `None`, which is why `route` exists.
        let orchestrator = orchestrator();
        let restricted = CompressionPolicy::for_mode(AuthMode::Subscription);
        let deep = format!("{}1{}", "[".repeat(500), "]".repeat(500));

        let reasons = [
            orchestrator
                .route(&bulky_json(), restricted, "claude-opus-4")
                .as_str(),
            orchestrator.route(&deep, payg(), "claude-opus-4").as_str(),
            orchestrator
                .route("short", payg(), "claude-opus-4")
                .as_str(),
        ];

        assert_eq!(reasons[0], "policy_forbids");
        assert_eq!(reasons[1], "unsafe");
        assert_ne!(reasons[2], "compress");
    }

    #[test]
    fn custom_limits_are_honored() {
        let orchestrator = orchestrator().with_limits(Limits {
            max_bytes: 16,
            ..Limits::default()
        });

        assert!(matches!(
            orchestrator.route(&bulky_json(), payg(), "claude-opus-4"),
            Routing::Unsafe {
                hazard: Hazard::TooLarge { .. }
            }
        ));
    }

    #[test]
    fn routing_is_deterministic() {
        // Invariant I4. The same block must route the same way every time, or the same
        // request compresses differently between runs.
        let orchestrator = orchestrator();
        let content = bulky_json();
        let first = orchestrator.route(&content, payg(), "claude-opus-4");

        for _ in 0..25 {
            assert_eq!(orchestrator.route(&content, payg(), "claude-opus-4"), first);
        }
    }

    // ---- recommendations ----

    #[test]
    fn a_shape_measured_useless_is_skipped() {
        // The other half of N3. Without this, `learn` produces a file nothing reads and
        // the proxy re-attempts a shape it has already measured as hopeless on every
        // request.
        use crate::telemetry::{Aggregator, Telemetry};

        let content = bulky_json();
        let key = AggregationKey::new(
            AuthMode::PayAsYouGo,
            "claude-opus-4",
            StructureHash::of(&content, ContentType::Json),
        );

        let mut aggregator = Aggregator::new();
        for _ in 0..10 {
            aggregator.record_decline(&key);
        }

        let orchestrator = orchestrator().with_recommendations(aggregator.recommend(5));
        assert!(matches!(
            orchestrator.route(&content, payg(), "claude-opus-4"),
            Routing::MeasuredUseless { .. }
        ));
    }

    #[test]
    fn a_shape_measured_useful_still_compresses() {
        // The balancing case: a recommendations file must not turn compression off
        // wholesale, only for the shapes it measured as hopeless.
        use crate::telemetry::{Aggregator, Telemetry};

        let content = bulky_json();
        let key = AggregationKey::new(
            AuthMode::PayAsYouGo,
            "claude-opus-4",
            StructureHash::of(&content, ContentType::Json),
        );

        let mut aggregator = Aggregator::new();
        for _ in 0..10 {
            aggregator.record(&key, 1000, 100);
        }

        let orchestrator = orchestrator().with_recommendations(aggregator.recommend(5));
        assert!(orchestrator
            .route(&content, payg(), "claude-opus-4")
            .will_compress());
    }

    #[test]
    fn an_unmeasured_shape_is_still_attempted() {
        // Skipping it would never gather the data that would let it be skipped for a
        // reason — the recommendations file would only ever grow entries for shapes that
        // already work.
        use crate::telemetry::{Aggregator, Telemetry};

        let mut aggregator = Aggregator::new();
        let other = AggregationKey::new(
            AuthMode::PayAsYouGo,
            "claude-opus-4",
            StructureHash::of("something else entirely", ContentType::Json),
        );
        for _ in 0..10 {
            aggregator.record_decline(&other);
        }

        let orchestrator = orchestrator().with_recommendations(aggregator.recommend(5));
        assert!(orchestrator
            .route(&bulky_json(), payg(), "claude-opus-4")
            .will_compress());
    }

    #[test]
    fn a_recommendation_for_one_model_does_not_silence_another() {
        // The key includes the model family. A shape that compresses badly for one model
        // may compress fine for another, and pooling them would let one bad measurement
        // disable compression everywhere.
        use crate::telemetry::{Aggregator, Telemetry};

        let content = bulky_json();
        let key = AggregationKey::new(
            AuthMode::PayAsYouGo,
            "gpt-4o",
            StructureHash::of(&content, ContentType::Json),
        );

        let mut aggregator = Aggregator::new();
        for _ in 0..10 {
            aggregator.record_decline(&key);
        }

        let orchestrator = orchestrator().with_recommendations(aggregator.recommend(5));
        assert!(matches!(
            orchestrator.route(&content, payg(), "gpt-4o"),
            Routing::MeasuredUseless { .. }
        ));
        assert!(orchestrator
            .route(&content, payg(), "claude-opus-4")
            .will_compress());
    }

    #[test]
    fn the_default_orchestrator_has_no_recommendations() {
        // An empty set means every shape is attempted, which is the right default for a
        // process that has never been given a file.
        assert!(orchestrator()
            .route(&bulky_json(), payg(), "claude-opus-4")
            .will_compress());
    }

    // ---- eligibility ----

    #[test]
    fn sacrosanct_blocks_are_never_eligible() {
        // Invariant I8, checked here as well as in the live-zone dispatcher. Two
        // independent checks is deliberate: this one is cheap, and the cost of the
        // guarantee failing is content the provider rejects as tampered-with.
        for kind in [BlockKind::Thinking, BlockKind::RedactedThinking] {
            assert!(!Orchestrator::is_eligible(&Block::new(kind, "x")));
        }
    }

    #[test]
    fn ordinary_blocks_are_eligible() {
        for kind in [BlockKind::Text, BlockKind::ToolResult] {
            assert!(Orchestrator::is_eligible(&Block::new(kind, "x")));
        }
    }
}
