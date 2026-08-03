//! Python bindings — gap rows B1 and B2.
//!
//! # Why this goes through the orchestrator
//!
//! The compressor set could be assembled here directly, and that is exactly the mistake
//! the proxy already made once: it carried its own copy of the routing decision, the CLI
//! carried another, and nothing failed when they drifted. Routing lives in
//! [`headroom_core::pipeline::Orchestrator`], so `headroom.compress()` from Python and
//! `POST /v1/messages` reach the same answer by construction rather than by review.
//!
//! # What crosses the boundary
//!
//! Strings and numbers. No handles to Rust objects are handed to Python: a `CcrStore`
//! that outlived a call would let a caller retrieve content from a request they never
//! made, and the store is per-call for that reason. The cost is that a `<<ccr:HASH>>`
//! marker in the returned text is not retrievable through this API — see
//! [`compress`] for why that is the honest trade rather than an oversight.

use std::sync::Arc;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use headroom_core::auth_mode::{AuthMode, CompressionPolicy};
use headroom_core::block::{Block, BlockKind};
use headroom_core::ccr::InMemoryCcrStore;
use headroom_core::detection::detect;
use headroom_core::pipeline::{Orchestrator, Routing};
use headroom_core::validate::validated_apply;

/// What a compression attempt did.
///
/// Every field is a plain Python value, readable and immutable. A result object that
/// held a reference back into Rust state would outlive the call that produced it.
#[pyclass(frozen, get_all)]
#[derive(Debug, Clone)]
pub struct CompressionResult {
    /// The content to send. Identical to the input when nothing was compressed.
    pub content: String,
    /// Whether the content actually changed.
    pub compressed: bool,
    /// Tokens the input measured.
    pub tokens_before: usize,
    /// Tokens the output measures.
    pub tokens_after: usize,
    /// What the content was detected as.
    pub content_type: String,
    /// Why it was routed as it was.
    ///
    /// One of `Routing::REASONS` — `"compress"`, `"lossless"`, `"policy_forbids"`,
    /// `"unsafe"`, `"no_compressor"`, `"measured_useless"` — or [`NOT_SMALLER`], the one
    /// outcome this module can report that routing cannot.
    ///
    /// These are the identifiers the proxy uses for the same decisions, under
    /// `headroom_routing_total{reason=...}` and in `headroom inspect`. They were spelled
    /// with hyphens here, so a caller correlating a result against a dashboard matched
    /// nothing.
    ///
    /// Reported rather than collapsed into the boolean above, because "nothing handles
    /// this content type" and "policy forbids it" are opposite problems with opposite
    /// fixes, and a caller seeing only `compressed=False` cannot tell them apart.
    pub reason: String,
}

#[pymethods]
impl CompressionResult {
    fn __repr__(&self) -> String {
        format!(
            "CompressionResult(compressed={}, tokens_before={}, tokens_after={}, \
             content_type='{}', reason='{}')",
            if self.compressed { "True" } else { "False" },
            self.tokens_before,
            self.tokens_after,
            self.content_type,
            self.reason
        )
    }

    /// Tokens saved, never negative.
    ///
    /// Saturating rather than signed: invariant I5 already guarantees a result that is
    /// not smaller is discarded, so a negative saving is unreachable — and returning a
    /// signed type would invite callers to write handling for a case that cannot happen.
    #[getter]
    fn tokens_saved(&self) -> usize {
        self.tokens_before.saturating_sub(self.tokens_after)
    }
}

/// Parses an auth-mode name.
///
/// Rejected rather than defaulted. Silently treating an unrecognized mode as
/// pay-as-you-go would hand the most permissive policy to a caller who asked for
/// something else and misspelled it — invariant I10 decided by a typo.
fn auth_mode_from(name: &str) -> PyResult<AuthMode> {
    match name.trim().to_ascii_lowercase().replace('_', "-").as_str() {
        "pay-as-you-go" | "payg" | "api-key" => Ok(AuthMode::PayAsYouGo),
        "oauth" => Ok(AuthMode::OAuth),
        "subscription" => Ok(AuthMode::Subscription),
        other => Err(PyValueError::new_err(format!(
            "unknown auth_mode {other:?}; expected 'pay-as-you-go', 'oauth', or \
             'subscription'"
        ))),
    }
}

/// The one reason this module reports that routing cannot: a transform ran and its
/// output was not smaller, so the original stands.
///
/// Underscored to match [`Routing::as_str`], which every other reason comes from.
const NOT_SMALLER: &str = "not_smaller";

/// Compresses one blob of content.
///
/// Returns a [`CompressionResult`]. The content comes back **unchanged** whenever
/// compression does not apply or does not help — invariant I5 — so a caller can send the
/// result unconditionally without checking whether anything happened.
///
/// # The CCR marker is not retrievable through this API
///
/// A lossy compressor replaces bulk with a `<<ccr:HASH>>` marker and stores the original
/// so it can be fetched back. The store here lives for the duration of one call, so that
/// marker refers to content nothing can return.
///
/// That is deliberate. A process-lifetime store shared across calls would let one
/// caller retrieve content from a request they never made, which is a worse property for
/// a library than a marker that is opaque. Callers who need retrieval want the proxy or
/// the MCP `headroom_retrieve` tool, both of which own a store with a defined lifetime
/// and scope.
///
/// # Determinism
///
/// The same `(content, model, auth_mode)` always produces the same output — invariant
/// I4. Nothing here reads a clock or a random number.
#[pyfunction]
#[pyo3(signature = (content, *, model = "", auth_mode = "pay-as-you-go"))]
fn compress(content: &str, model: &str, auth_mode: &str) -> PyResult<CompressionResult> {
    let policy = CompressionPolicy::for_mode(auth_mode_from(auth_mode)?);
    // Per call. See the note above on why this is not shared.
    let orchestrator = Orchestrator::new(Arc::new(InMemoryCcrStore::new()));

    let tokenizer = orchestrator.tokenizer_for(model);
    let tokens_before = tokenizer.count(content);
    let content_type = detect(content.as_bytes()).content_type.to_string();

    let routing = orchestrator.route(content, policy, model);
    // `Routing::as_str`, not a name chosen here. This module used to map the variants
    // itself and spelled three of the six with hyphens — `policy-forbids` where the
    // proxy's `headroom_routing_total{reason="policy_forbids"}` and `headroom inspect`
    // both say `policy_forbids`. Reporting the reason is only useful if it is the same
    // reason, and a caller correlating a Python result against a dashboard got no match.
    let mut reason = routing.as_str().to_owned();

    let mut block = Block::new(BlockKind::Text, content);
    let compressed = match orchestrator.transform_for(content, policy, model) {
        Some(transform) => match validated_apply(transform, &mut block, tokenizer.as_ref()) {
            Ok(outcome) if outcome.is_compressed() => true,
            // The transform ran and the result was not smaller in tokens, so the
            // original stands. Distinct from "no transform applied", and reported as
            // such — a caller tuning content wants to know their input reached a
            // compressor and did not benefit.
            Ok(_) => {
                reason = NOT_SMALLER.to_owned();
                false
            }
            // An invariant violation is a bug. Surfaced as an exception rather than
            // swallowed: unlike the proxy, there is no customer request here to protect
            // by carrying on, and a caller who gets silently-uncompressed content back
            // has no way to learn the engine failed.
            Err(err) => {
                return Err(PyValueError::new_err(format!("compression failed: {err}")));
            }
        },
        None => false,
    };

    let tokens_after = tokenizer.count(block.content());

    Ok(CompressionResult {
        content: block.content().to_owned(),
        compressed,
        tokens_before,
        tokens_after,
        content_type,
        reason,
    })
}

/// Counts tokens the way the compressor does.
///
/// Exposed because a caller deciding whether to send something needs the same number the
/// engine used, not an approximation of it. Exact for models with a registered
/// tokenizer, an over-count otherwise — never an under-count, which is the direction
/// that would make a caller send more than they meant to.
#[pyfunction]
#[pyo3(signature = (content, *, model = ""))]
fn count_tokens(content: &str, model: &str) -> usize {
    Orchestrator::new(Arc::new(InMemoryCcrStore::new()))
        .tokenizer_for(model)
        .count(content)
}

/// What the engine thinks `content` is.
#[pyfunction]
fn detect_content_type(content: &str) -> String {
    detect(content.as_bytes()).content_type.to_string()
}

/// The extension module.
#[pymodule]
fn headroom(module: &Bound<'_, PyModule>) -> PyResult<()> {
    // Gap row B2. Without this the engine's `tracing` and `log` output goes nowhere —
    // the same silent-nothing failure as a proxy running with no subscriber installed,
    // which is invisible precisely because every logging call still succeeds.
    //
    // A failure to install is not fatal: the caller may have configured logging
    // themselves, and refusing to import over it would be worse than losing the logs.
    let _ = pyo3_log::try_init();

    module.add_function(wrap_pyfunction!(compress, module)?)?;
    module.add_function(wrap_pyfunction!(count_tokens, module)?)?;
    module.add_function(wrap_pyfunction!(detect_content_type, module)?)?;
    module.add_class::<CompressionResult>()?;

    // Every value `CompressionResult.reason` can take, so a caller can branch on it
    // without reading Rust or writing the list down a ninth time. Built from
    // `Routing::REASONS` rather than typed here — this module already spelled three of
    // those six with hyphens, and nothing noticed because nothing compared them.
    let reasons: Vec<&str> = Routing::REASONS
        .iter()
        .copied()
        .chain(std::iter::once(NOT_SMALLER))
        .collect();
    module.add("REASONS", reasons)?;

    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
