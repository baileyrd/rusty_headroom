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
use crate::{
    CodeCompressor, DiffCompressor, LogCompressor, SearchCompressor, SmartCrusher, TextSummarizer,
};

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
    code: CodeCompressor,
    text: TextSummarizer,
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
            diff: DiffCompressor::new(store.clone()),
            code: CodeCompressor::new(store.clone()),
            text: TextSummarizer::new(store),
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

        // Asked of `for_type` rather than restated here. This used to be a second list
        // of compressible content types, and the two drifted the moment one gained an
        // arm the other did not — which is how the proxy came to forward every source
        // file uncompressed while still detecting it as code. One list, one answer.
        if self.for_type(content_type).is_none() {
            return Routing::NoCompressor { content_type };
        }

        // Checked last, so a shape only reaches this gate if a compressor would
        // otherwise have run. An unmeasured shape is always attempted: skipping it
        // would never gather the data that would let it be skipped for a reason.
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

    /// The transform for `block`, if one applies.
    ///
    /// # Use this from a request path, not [`Orchestrator::transform_for`]
    ///
    /// The two differ on exactly one thing, and it matters: **prose is compressed only
    /// when the block is tool output**. `BlockKind::Text` is what a user typed or a model
    /// wrote, and the prose compressor is lossy — it drops low-importance lines behind a
    /// CCR marker. Doing that to a directory listing is the product. Doing it to
    /// somebody's message is rewriting what they said, and no token saving is worth that.
    ///
    /// Every other content type is tool-shaped by nature: a user does not type a
    /// 5 KB unified diff into a chat box, and if they do, compressing it is what they
    /// were asking for.
    pub fn transform_for_block(
        &self,
        block: &Block,
        policy: CompressionPolicy,
        model: &str,
    ) -> Option<&dyn Transform> {
        let transform = self.transform_for(block.content(), policy, model)?;

        if self.runs_only_on_tool_output(transform) && !block.kind().is_tool_output() {
            return None;
        }
        Some(transform)
    }

    /// Whether `transform` is one that may only see tool output.
    ///
    /// The D24 rule, in one place. It is asked here by
    /// [`Orchestrator::transform_for_block`], which enforces it, and by
    /// [`Orchestrator::tool_output_only`], which reports it — so a command describing the
    /// rule and the pipeline applying it cannot come apart. `headroom tools` had already
    /// managed to describe prose wrongly in both directions: first as never compressed,
    /// then as always compressed.
    fn runs_only_on_tool_output(&self, transform: &dyn Transform) -> bool {
        transform.name() == self.text.name()
    }

    /// Whether the compressor for `content_type` runs only on tool output.
    ///
    /// For callers reporting on the build rather than compressing. A caller with an
    /// actual block should use [`Orchestrator::transform_for_block`], which applies this
    /// rather than describing it.
    pub fn tool_output_only(&self, content_type: ContentType) -> bool {
        self.for_type(content_type)
            .is_some_and(|transform| self.runs_only_on_tool_output(transform))
    }

    /// The transform for `content`, if one applies.
    ///
    /// A thin wrapper over [`Orchestrator::route`] for callers that want the compressor
    /// rather than the reason. Callers recording telemetry should use `route`, since
    /// `None` here collapses three genuinely different outcomes into one.
    ///
    /// # This one has no block to inspect
    ///
    /// Which makes it right for `headroom compress`, the MCP tool, and the Python
    /// binding — a caller that handed content over has asked for it to be compressed,
    /// whatever it is. A request path has a block and should use
    /// [`Orchestrator::transform_for_block`] instead.
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
    ///
    /// # Public so nothing has to write the table out again
    ///
    /// This was private, and the cost of that was five hand-written copies of the same
    /// mapping: in `headroom compress`, in the MCP server, in `headroom inspect`, in
    /// `headroom tools`, and a second one inside `route`. Every one of them eventually
    /// disagreed with this function, and each disagreement showed up as a command
    /// confidently describing behaviour the pipeline does not have.
    ///
    /// A caller that has content should use [`Orchestrator::route`] or
    /// [`Orchestrator::transform_for_block`], which also apply policy, safety limits and
    /// the block-kind rule. This is for callers that have a *type* and want the table —
    /// which in practice means reporting on the build rather than compressing anything.
    pub fn for_type(&self, content_type: ContentType) -> Option<&dyn Transform> {
        match content_type {
            ContentType::Json => Some(&self.smart_crusher),
            ContentType::Log => Some(&self.log),
            ContentType::SearchResults => Some(&self.search),
            ContentType::Diff => Some(&self.diff),
            // Code was missing here until gap row C11-C13's wiring was checked, so the
            // proxy forwarded every source file uncompressed while `headroom compress`
            // — which carried its own routing table — reported a saving for the same
            // content. Code is the largest category of agent tool-result traffic, so the
            // omission was not a small one.
            ContentType::Code => Some(&self.code),
            // Prose is routed only through `transform_for_block`, which checks that the
            // block is tool output. Reaching it by content alone is correct for a caller
            // that handed content over to be compressed — see `transform_for`.
            ContentType::Prose => Some(&self.text),
            _ => None,
        }
    }

    /// Whether a block is worth offering to a compressor at all.
    ///
    /// # This is a helper, not one of the guards
    ///
    /// The comment here used to claim it was one of "two independent checks" for
    /// invariant I8. It is not called by anything on the request path. The two checks
    /// that genuinely run are [`live_zone`]'s categorizer, which never offers a
    /// sacrosanct block to the pipeline, and [`apply_guarded`], which refuses one even
    /// if it is handed over — and that pair is real defence in depth: removing either
    /// alone leaves signed content protected, which is exactly what it is for.
    ///
    /// Kept as public API for callers assembling their own pipeline, described
    /// accurately so nobody counts it as a guard that is running.
    ///
    /// [`live_zone`]: crate::live_zone::live_zone
    /// [`apply_guarded`]: crate::transform::apply_guarded
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

        // Whitespace only, which detects as `Unknown`. Prose used to be the example
        // here and is no longer one: it routes to the text compressor as of gap row
        // C10's wiring, which is the point of that change.
        let nothing = "   \n\t  \n".repeat(200);

        assert!(matches!(
            orchestrator.route(&nothing, payg(), "claude-opus-4"),
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
                .route("   \n\t  ", payg(), "claude-opus-4")
                .as_str(),
        ];

        assert_eq!(reasons[0], "policy_forbids");
        assert_eq!(reasons[1], "unsafe");
        // Whitespace only, so nothing handles it. `"short"` used to serve here and is
        // now ordinary prose, which does route.
        assert_eq!(reasons[2], "no_compressor");
    }

    #[test]
    fn prose_from_a_tool_result_is_compressed() {
        // Gap row C10. `TextSummarizer` existed, was tested, and was referenced by
        // nothing but the `lib.rs` re-export — so the proxy forwarded every prose tool
        // result whole. The S4/S5 keep-sets were wired into this compressor, which means
        // they never ran either until this routing existed.
        let prose = "The quick brown fox jumps over the lazy dog. ".repeat(400);
        let block = Block::new(BlockKind::ToolResult, prose);

        assert_eq!(
            orchestrator()
                .transform_for_block(&block, payg(), "claude-opus-4")
                .map(|t| t.name()),
            Some("text_summarizer")
        );
    }

    #[test]
    fn prose_a_person_wrote_is_never_lossily_rewritten() {
        // The line this whole entry point exists to draw. `BlockKind::Text` is what a
        // user typed or a model wrote, and the prose compressor is lossy — it drops
        // low-importance lines behind a CCR marker. Doing that to a directory listing is
        // the product. Doing it to somebody's message is rewriting what they said.
        let prose = "The quick brown fox jumps over the lazy dog. ".repeat(400);
        let block = Block::new(BlockKind::Text, prose);

        assert!(
            orchestrator()
                .transform_for_block(&block, payg(), "claude-opus-4")
                .is_none(),
            "a user's own text was routed to a lossy compressor"
        );
    }

    #[test]
    fn the_tool_output_rule_applies_only_to_prose() {
        // Every other type is tool-shaped by nature: a person does not type a 5 KB
        // unified diff into a chat box, and if they do, compressing it is what they were
        // asking for. Narrowing the rule further would exempt content the proxy exists
        // to compress.
        let block = Block::new(BlockKind::Text, bulky_json());

        assert!(orchestrator()
            .transform_for_block(&block, payg(), "claude-opus-4")
            .is_some());
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
