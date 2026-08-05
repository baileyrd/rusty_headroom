//! Resolving a model identifier to a tokenizer — gap row T4.
//!
//! # Why the fallback is the interesting part
//!
//! Resolution succeeds trivially for models this build knows about. What decides
//! whether the registry is safe is what it does for a model it has never seen — and
//! that case is not rare, because a provider ships a new model well before this crate
//! is rebuilt.
//!
//! The answer is always a tokenizer, never `None`. Invariant I5 validates every
//! compression against a token count, so a caller with no tokenizer has no way to
//! check that compression helped, and the honest options are to skip compression
//! entirely or to skip the check. Both are worse than counting with an approximation
//! that over-counts on the content this proxy actually sees.
//!
//! # Which direction to be wrong in
//!
//! [`HeuristicEstimator`] over-counts by design. Over-counting costs a missed
//! compression — visible, cheap, and self-correcting. Under-counting means a payload
//! that grew is measured as having shrunk, so I5's safety net passes something that
//! made the request *more* expensive. That failure is silent and compounds.
//!
//! This module used to claim the fallback never under-counts. Measured against `gpt-4o`,
//! that is false for random alphanumeric strings, and cannot be fixed by a character-class
//! heuristic — see [`HeuristicEstimator`] for the numbers and D29 for the decision. It
//! holds on every realistic content class, which is pinned by a differential test.
//!
//! `is_exact_for` is how a caller finds out which it got. Every OpenAI family resolves to
//! a real tokenizer; everything else, Anthropic included, gets the estimate.
//!
//! Any tokenizer added here has to hold the same line, which is why
//! [`Tokenizer::is_exact`] exists: it lets a caller tell "this is the real count" from
//! "this is an upper bound", rather than inferring it from the name.

use std::sync::Arc;

use super::{HeuristicEstimator, TiktokenCounter, Tokenizer};

/// A model family, resolved from an identifier.
///
/// Coarser than the model name on purpose: a point release rarely changes the
/// tokenizer, and keying on the exact identifier means every new release date is an
/// unknown model that falls back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Family {
    /// Anthropic Claude.
    Claude,
    /// OpenAI GPT and o-series.
    OpenAi,
    /// Google Gemini.
    Gemini,
    /// Meta Llama.
    Llama,
    /// Mistral.
    Mistral,
    /// Something this build does not recognize.
    Unknown,
}

impl Family {
    /// A stable identifier, for telemetry.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::OpenAi => "openai",
            Self::Gemini => "gemini",
            Self::Llama => "llama",
            Self::Mistral => "mistral",
            Self::Unknown => "unknown",
        }
    }

    /// Classifies a model identifier.
    ///
    /// Matches on substrings rather than exact names, because identifiers arrive in
    /// several shapes for the same model — `claude-opus-4-20250514`,
    /// `anthropic/claude-opus-4`, `bedrock:anthropic.claude-opus-4` — and an exact-match
    /// table would treat all but one spelling as unknown.
    ///
    /// # Example
    ///
    /// ```
    /// use headroom_core::tokenizer::registry::Family;
    ///
    /// assert_eq!(Family::of("claude-opus-4-20250514"), Family::Claude);
    /// assert_eq!(Family::of("anthropic/claude-sonnet-5"), Family::Claude);
    /// assert_eq!(Family::of("gpt-4o-mini"), Family::OpenAi);
    /// assert_eq!(Family::of("something-new-2027"), Family::Unknown);
    /// ```
    pub fn of(model: &str) -> Self {
        let model = model.to_ascii_lowercase();

        // Ordered most-specific first. `claude` appears inside
        // `bedrock:anthropic.claude-...`, and a generic vendor match placed earlier
        // would swallow it.
        if model.contains("claude") {
            Self::Claude
        } else if model.contains("gpt") || model.starts_with('o') && has_o_series_shape(&model) {
            Self::OpenAi
        } else if model.contains("gemini") {
            Self::Gemini
        } else if model.contains("llama") {
            Self::Llama
        } else if model.contains("mistral") || model.contains("mixtral") {
            Self::Mistral
        } else {
            Self::Unknown
        }
    }
}

/// Whether `model` looks like an OpenAI o-series identifier (`o1`, `o3-mini`).
///
/// Deliberately narrow: a bare `starts_with('o')` would claim `openhands`, `olmo`, and
/// anything else beginning with the letter. The o-series shape is `o` followed
/// immediately by a digit.
fn has_o_series_shape(model: &str) -> bool {
    model
        .strip_prefix('o')
        .and_then(|rest| rest.chars().next())
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
}

/// Resolves model identifiers to tokenizers.
///
/// # Always resolves
///
/// [`Registry::for_model`] returns a tokenizer for every input, including an empty
/// string. There is no `Option`, because there is nothing useful a caller could do with
/// `None` — see the module docs.
#[derive(Clone)]
pub struct Registry {
    exact: Vec<(Family, Arc<dyn Tokenizer>)>,
    /// Whether the compiled-in OpenAI BPE tables are available for *per-model*
    /// resolution.
    ///
    /// Kept separate from `exact` rather than folded into one `Arc<dyn Tokenizer>`
    /// registered for `Family::OpenAi`: OpenAI is the one family with more than one
    /// encoding in active use (`cl100k_base` for GPT-4/GPT-3.5, `o200k_base` for
    /// GPT-4o and later), and a single trait-object instance keyed only on the coarse
    /// `Family` cannot represent "pick the encoding from the actual model string" —
    /// which is exactly what `TiktokenCounter::for_model` already does correctly when
    /// called directly. A single fixed instance built once from `"gpt-4o"` used to be
    /// registered for the whole family, so a `gpt-3.5`/`gpt-4` request was measured
    /// with `gpt-4o`'s vocabulary while still reporting itself as exact — under the
    /// hood, `o200k_base` under-counts relative to a model's real `cl100k_base` on
    /// content where they differ, which is the direction invariant I5 exists to
    /// forbid.
    openai_tiktoken: bool,
    fallback: Arc<dyn Tokenizer>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut exact: Vec<(&'static str, &'static str)> = self
            .exact
            .iter()
            .map(|(family, tokenizer)| (family.as_str(), tokenizer.name()))
            .collect();
        if self.openai_tiktoken && !self.exact.iter().any(|(f, _)| *f == Family::OpenAi) {
            exact.push((Family::OpenAi.as_str(), "tiktoken (per-model)"));
            exact.sort_unstable();
        }
        f.debug_struct("Registry")
            .field("exact", &exact)
            .field("fallback", &self.fallback.name())
            .finish()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    /// Builds a registry with the heuristic estimator as its fallback and nothing
    /// registered.
    pub fn new() -> Self {
        Self {
            exact: Vec::new(),
            openai_tiktoken: false,
            fallback: Arc::new(HeuristicEstimator::new()),
        }
    }

    /// Builds a registry with every exact tokenizer this build carries.
    ///
    /// Enabled by default rather than on request: the OpenAI vocabularies are
    /// compiled in either way, so leaving them unregistered would mean carrying the
    /// tables and then not using them — the worst of both.
    ///
    /// The model identifier still decides which encoding applies, so an Anthropic
    /// model is unaffected and still falls back to the heuristic.
    pub fn with_defaults() -> Self {
        let mut registry = Self::new();
        registry.openai_tiktoken = true;
        registry
    }

    /// Registers `tokenizer` for `family`, replacing any previous entry.
    ///
    /// Replacing rather than appending: two tokenizers registered for one family is a
    /// configuration mistake, and silently keeping the first makes it look like the
    /// second call did nothing.
    ///
    /// An explicit registration for [`Family::OpenAi`] takes priority over the
    /// compiled-in per-model tiktoken resolution `with_defaults` enables — this is
    /// still the escape hatch for a caller that wants one fixed tokenizer for the
    /// whole family (tests use it that way).
    pub fn register(&mut self, family: Family, tokenizer: Arc<dyn Tokenizer>) {
        self.exact.retain(|(existing, _)| *existing != family);
        self.exact.push((family, tokenizer));
        // Sorted so `Debug` output and iteration order do not depend on registration
        // order — the same registry should describe itself the same way every run.
        self.exact.sort_by_key(|(family, _)| *family);
    }

    /// The tokenizer for `model`.
    ///
    /// Falls back to the heuristic estimator when the family is unknown or has no
    /// registered tokenizer.
    pub fn for_model(&self, model: &str) -> Arc<dyn Tokenizer> {
        let family = Family::of(model);

        if let Some((_, tokenizer)) = self.exact.iter().find(|(f, _)| *f == family) {
            return tokenizer.clone();
        }

        if family == Family::OpenAi && self.openai_tiktoken {
            // Resolved from the actual model string, not just the family, so
            // `gpt-3.5-turbo`/`gpt-4` get `cl100k_base` and `gpt-4o`/later get
            // `o200k_base` instead of sharing whichever one a fixed instance happened
            // to be built from.
            return Arc::new(TiktokenCounter::for_model(model));
        }

        self.fallback.clone()
    }

    /// The tokenizer for `family`, without a model string to resolve OpenAI's
    /// encoding from.
    ///
    /// Only meaningful for a family with one encoding, or a family with an explicit
    /// [`Self::register`] override. Prefer [`Self::for_model`] wherever a model
    /// identifier is available, which is everywhere this crate calls it.
    pub fn for_family(&self, family: Family) -> Arc<dyn Tokenizer> {
        self.exact
            .iter()
            .find(|(registered, _)| *registered == family)
            .map(|(_, tokenizer)| tokenizer.clone())
            .unwrap_or_else(|| self.fallback.clone())
    }

    /// Whether `model` resolves to an exact tokenizer rather than the fallback.
    ///
    /// Exposed so a caller can report "counted exactly" versus "counted approximately"
    /// instead of presenting an estimate as a measurement.
    pub fn is_exact_for(&self, model: &str) -> bool {
        self.for_model(model).is_exact()
    }

    /// Families with a registered exact tokenizer.
    pub fn registered(&self) -> Vec<Family> {
        let mut families: Vec<Family> = self.exact.iter().map(|(family, _)| *family).collect();
        if self.openai_tiktoken && !families.contains(&Family::OpenAi) {
            families.push(Family::OpenAi);
        }
        families.sort_unstable();
        families
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in exact tokenizer, so the registry can be tested without depending on
    /// a real BPE implementation existing yet.
    #[derive(Debug)]
    struct FakeExact(&'static str);

    impl Tokenizer for FakeExact {
        fn count(&self, text: &str) -> usize {
            text.split_whitespace().count()
        }
        fn name(&self) -> &'static str {
            self.0
        }
        fn is_exact(&self) -> bool {
            true
        }
    }

    // ---- family classification ----

    #[test]
    fn a_model_identifier_resolves_however_it_is_spelled() {
        // Identifiers arrive in several shapes for the same model. An exact-match table
        // would treat all but one spelling as unknown and quietly fall back.
        for model in [
            "claude-opus-4-20250514",
            "anthropic/claude-sonnet-5",
            "bedrock:anthropic.claude-opus-4",
            "CLAUDE-OPUS-4",
        ] {
            assert_eq!(Family::of(model), Family::Claude, "{model}");
        }
    }

    #[test]
    fn each_vendor_is_recognized() {
        for (model, family) in [
            ("gpt-4o-mini", Family::OpenAi),
            ("o3-mini", Family::OpenAi),
            ("gemini-2.5-pro", Family::Gemini),
            ("meta-llama/Llama-3-70b", Family::Llama),
            ("mistral-large", Family::Mistral),
            ("mixtral-8x7b", Family::Mistral),
        ] {
            assert_eq!(Family::of(model), family, "{model}");
        }
    }

    #[test]
    fn the_o_series_match_does_not_claim_every_word_starting_with_o() {
        // A bare `starts_with('o')` would classify `openhands` and `olmo` as OpenAI
        // models and hand them a tokenizer built for a different vocabulary.
        for model in ["openhands", "olmo-7b", "orca-2", "o"] {
            assert_ne!(Family::of(model), Family::OpenAi, "{model}");
        }
        assert_eq!(Family::of("o1-preview"), Family::OpenAi);
    }

    #[test]
    fn an_unrecognized_model_is_unknown_rather_than_a_guess() {
        for model in ["", "something-new-2027", "internal-model-v3", "-"] {
            assert_eq!(Family::of(model), Family::Unknown, "{model:?}");
        }
    }

    #[test]
    fn classification_is_deterministic() {
        // Invariant I4 — the tokenizer chosen must not vary between runs, or the token
        // count feeding I5 varies with it.
        for model in ["claude-opus-4", "gpt-4o", "unknown-thing"] {
            let first = Family::of(model);
            for _ in 0..25 {
                assert_eq!(Family::of(model), first);
            }
        }
    }

    // ---- resolution ----

    #[test]
    fn the_default_registry_counts_openai_models_exactly() {
        // The vocabularies are compiled in either way; leaving them unregistered would
        // mean carrying the tables and then not using them.
        let registry = Registry::with_defaults();

        assert!(registry.is_exact_for("gpt-4o"));
        assert_eq!(registry.for_model("gpt-4o").name(), "o200k_base");
    }

    #[test]
    fn different_openai_models_use_their_own_encoding_not_gpt_4os() {
        // Regression. A single fixed instance built once from "gpt-4o" used to be
        // registered for the whole `Family::OpenAi` bucket, so every OpenAI model —
        // including ones that really use `cl100k_base` — was counted with `gpt-4o`'s
        // `o200k_base` vocabulary while `is_exact_for` still reported `true`.
        let registry = Registry::with_defaults();

        assert_eq!(registry.for_model("gpt-4o").name(), "o200k_base");
        assert_eq!(registry.for_model("gpt-4-turbo").name(), "cl100k_base");
        assert_eq!(registry.for_model("gpt-3.5-turbo").name(), "cl100k_base");
        assert!(registry.is_exact_for("gpt-3.5-turbo"));
    }

    #[test]
    fn an_explicit_openai_registration_still_overrides_per_model_resolution() {
        // The escape hatch `register` documents: a caller that wants one fixed
        // tokenizer for the whole family, rather than the compiled-in per-model
        // resolution, still gets it.
        let mut registry = Registry::with_defaults();
        registry.register(Family::OpenAi, Arc::new(FakeExact("fixed")));

        assert_eq!(registry.for_model("gpt-4o").name(), "fixed");
        assert_eq!(registry.for_model("gpt-3.5-turbo").name(), "fixed");
    }

    #[test]
    fn the_default_registry_leaves_other_families_on_the_heuristic() {
        // An OpenAI vocabulary applied to an Anthropic model would be a wrong count
        // reported as exact, which is worse than an honest upper bound.
        let registry = Registry::with_defaults();

        assert!(!registry.is_exact_for("claude-opus-4"));
        assert!(!registry.is_exact_for("gemini-2.5-pro"));
        assert!(!registry.is_exact_for("something-new"));
    }

    #[test]
    fn an_empty_registry_still_resolves_every_model() {
        // There is no `None` to return. A caller without a tokenizer cannot satisfy
        // invariant I5, and skipping the check is worse than an upper bound.
        let registry = Registry::new();
        for model in ["claude-opus-4", "", "never-heard-of-it"] {
            let tokenizer = registry.for_model(model);
            assert!(tokenizer.count("some text here") > 0, "{model:?}");
        }
    }

    #[test]
    fn a_registered_tokenizer_wins_for_its_family() {
        let mut registry = Registry::new();
        registry.register(Family::Claude, Arc::new(FakeExact("fake-claude")));

        assert_eq!(registry.for_model("claude-opus-4").name(), "fake-claude");
        assert_ne!(registry.for_model("gpt-4o").name(), "fake-claude");
    }

    #[test]
    fn registering_twice_for_one_family_replaces_rather_than_shadows() {
        // Two tokenizers for one family is a configuration mistake. Keeping the first
        // silently makes the second call look like it did nothing.
        let mut registry = Registry::new();
        registry.register(Family::Claude, Arc::new(FakeExact("first")));
        registry.register(Family::Claude, Arc::new(FakeExact("second")));

        assert_eq!(registry.for_model("claude-opus-4").name(), "second");
        assert_eq!(registry.registered().len(), 1);
    }

    #[test]
    fn the_fallback_is_reported_as_approximate() {
        // So a caller can say "counted approximately" rather than presenting an
        // estimate as a measurement.
        let registry = Registry::new();
        assert!(!registry.is_exact_for("claude-opus-4"));

        let mut registry = Registry::new();
        registry.register(Family::Claude, Arc::new(FakeExact("fake")));
        assert!(registry.is_exact_for("claude-opus-4"));
        assert!(
            !registry.is_exact_for("gpt-4o"),
            "fallback claimed exactness"
        );
    }

    #[test]
    fn the_fallback_never_under_counts() {
        // The direction that matters. Over-counting costs a missed compression, which
        // is visible and cheap. Under-counting means a payload that grew is measured as
        // having shrunk, so invariant I5's safety net passes something that made the
        // request more expensive — silently.
        let registry = Registry::new();
        let tokenizer = registry.for_model("unknown-model");

        for text in [
            "hello world",
            "日本語のテキスト",
            "a".repeat(1000).as_str(),
            r#"{"deeply":{"nested":{"json":[1,2,3]}}}"#,
            "",
        ] {
            let counted = tokenizer.count(text);
            // The loosest true lower bound on any real tokenizer: no tokenizer emits
            // more tokens than the text has bytes, and none emits fewer than one per
            // four bytes for text this varied.
            assert!(
                counted * 8 >= text.len(),
                "{text:?} counted {counted} for {} bytes — suspiciously low",
                text.len()
            );
        }
    }

    #[test]
    fn the_registry_describes_itself_the_same_way_every_run() {
        // Registration order must not leak into the description, or two identically
        // configured processes disagree about what they are.
        let mut first = Registry::new();
        first.register(Family::OpenAi, Arc::new(FakeExact("a")));
        first.register(Family::Claude, Arc::new(FakeExact("b")));

        let mut second = Registry::new();
        second.register(Family::Claude, Arc::new(FakeExact("b")));
        second.register(Family::OpenAi, Arc::new(FakeExact("a")));

        assert_eq!(format!("{first:?}"), format!("{second:?}"));
        assert_eq!(first.registered(), second.registered());
    }

    #[test]
    fn resolution_is_deterministic() {
        let mut registry = Registry::new();
        registry.register(Family::Claude, Arc::new(FakeExact("fake")));

        let first = registry.for_model("claude-opus-4").name();
        for _ in 0..25 {
            assert_eq!(registry.for_model("claude-opus-4").name(), first);
        }
    }
}
