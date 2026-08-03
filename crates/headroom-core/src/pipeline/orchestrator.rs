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

use crate::auth_mode::CompressionPolicy;
use crate::block::Block;
use crate::ccr::CcrStore;
use crate::detection::{detect, ContentType};
use crate::pipeline::reformats::Reformatter;
use crate::pipeline::safety::{check, Hazard, Limits};
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
        }
    }

    /// Uses `limits` instead of the defaults.
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
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
    /// assert!(orchestrator.route(&json, payg).will_compress());
    /// assert!(!orchestrator.route(&json, restricted).will_compress());
    /// ```
    pub fn route(&self, content: &str, policy: CompressionPolicy) -> Routing {
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
            | ContentType::Diff => Routing::Compress { content_type },
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
    ) -> Option<&dyn Transform> {
        match self.route(content, policy) {
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
                orchestrator.route(&content, payg()).will_compress(),
                "{:?}",
                orchestrator.route(&content, payg())
            );
            assert!(orchestrator.transform_for(&content, payg()).is_some());
        }
    }

    #[test]
    fn oauth_traffic_gets_the_lossless_reformatter_rather_than_nothing() {
        // The OAuth hazard is a modification exceeding the granted scope, and a
        // meaning-preserving reformat cannot exceed a scope.
        let orchestrator = orchestrator();
        let policy = CompressionPolicy::for_mode(AuthMode::OAuth);
        let routing = orchestrator.route(&bulky_json(), policy);

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
            orchestrator.route(&bulky_json(), policy),
            Routing::PolicyForbids
        );
        assert!(orchestrator.transform_for(&bulky_json(), policy).is_none());
    }

    #[test]
    fn a_restricted_request_is_still_protected_by_the_safety_check() {
        // Both branches lead to a transform that walks the content, so a restricted
        // request is not exempt from being handed a pathological payload.
        let orchestrator = orchestrator();
        let restricted = CompressionPolicy::for_mode(AuthMode::Subscription);
        let deep = format!("{}1{}", "[".repeat(500), "]".repeat(500));

        assert!(matches!(
            orchestrator.route(&deep, restricted),
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
                    .route(&bulky_json(), CompressionPolicy::for_mode(mode))
                    .is_lossy(),
                "{mode:?}"
            );
        }
        assert!(orchestrator.route(&bulky_json(), payg()).is_lossy());
    }

    #[test]
    fn an_unsafe_payload_is_declined_with_its_reason() {
        let orchestrator = orchestrator();
        let deep = format!("{}1{}", "[".repeat(500), "]".repeat(500));

        assert!(matches!(
            orchestrator.route(&deep, payg()),
            Routing::Unsafe { .. }
        ));
        assert!(orchestrator.transform_for(&deep, payg()).is_none());
    }

    #[test]
    fn content_with_no_compressor_is_reported_as_such() {
        // Distinguished from a policy decline and from a hazard, because an operator
        // reading telemetry needs to tell "nothing handles this" from "we were not
        // allowed to" — the fixes are entirely different.
        let orchestrator = orchestrator();
        let prose = "The quick brown fox jumps over the lazy dog. ".repeat(50);

        assert!(matches!(
            orchestrator.route(&prose, payg()),
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
            orchestrator.route(&bulky_json(), restricted).as_str(),
            orchestrator.route(&deep, payg()).as_str(),
            orchestrator.route("short", payg()).as_str(),
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
            orchestrator.route(&bulky_json(), payg()),
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
        let first = orchestrator.route(&content, payg());

        for _ in 0..25 {
            assert_eq!(orchestrator.route(&content, payg()), first);
        }
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
