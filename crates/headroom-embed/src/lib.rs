//! In-process text embedding, with no daemon and no network.
//!
//! # Why this exists rather than calling a local embedding server
//!
//! `rusty_remind_me`'s embedding backend is Ollama over loopback. Nothing leaves
//! the machine, so locality is not the objection — availability is. Its
//! `available_embedder` probes the daemon and yields `None` when the probe
//! fails, and its search treats a missing semantic tier as "that tier did not
//! run" rather than as an error. Excellent behaviour for an interactive memory
//! tool: search degrades instead of breaking.
//!
//! It is the wrong contract on a compression path. Invariant **I4** requires the
//! same request to compress to the same bytes on every run, and a ranking that
//! silently changes depending on whether a daemon answered a ping is exactly the
//! non-determinism I4 forbids. Running on loopback does not help.
//!
//! # The two properties that make this usable under I4
//!
//! **Pinned.** A model file and a tokenizer file, both required, both loaded
//! once by [`LocalEmbedder::load`]. Nothing here downloads anything, at build
//! time or run time — the same rule `rusty_remind_me` applies to its reranker.
//! Same model plus same text yields the same vector, so the query embedded on
//! the thousandth request is the vector it was on the first.
//!
//! **Fails closed.** [`LocalEmbedder::load`] returns an error rather than a
//! degraded embedder. A caller is expected to resolve that error *at startup*
//! and decide, once, whether the semantic tier is on for the life of the
//! process. What must never happen is a per-request fallback: a tier that
//! switches off for one request and back on for the next makes two identical
//! requests compress differently, which is the failure this module was built to
//! avoid rather than a graceful degradation of it.
//!
//! # Without the `local` feature
//!
//! Everything here still compiles and [`LocalEmbedder::load`] returns
//! [`LoadError::BackendUnavailable`]. A caller's code is identical either way,
//! so the feature cannot silently change what a call site looks like — only
//! whether it can succeed.

use std::fmt;
use std::path::{Path, PathBuf};

/// Whether text is being embedded as a search query or as stored content.
///
/// Several model families were trained with asymmetric instruction prefixes —
/// `nomic-embed-text`'s `search_query:`/`search_document:`, `e5`'s
/// `query:`/`passage:`. Embedding both sides unprefixed still works, but
/// measurably worse than the model's own convention, and the two sides must
/// agree or the vectors are not comparable at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedRole {
    /// The text being searched *for*.
    Query,
    /// The text being searched *among*.
    Passage,
}

impl EmbedRole {
    /// The instruction prefix for `model_name`, or `""` when it wants none.
    ///
    /// Matched by substring on the model name, the same way `rusty_remind_me`
    /// does it, so a locally renamed file still gets the right prefix as long as
    /// the family is recognizable. An unrecognized model gets no prefix, which
    /// is the correct guess: a prefix a model was not trained on is noise
    /// prepended to every input.
    pub fn prefix_for(self, model_name: &str) -> &'static str {
        let name = model_name.to_ascii_lowercase();
        if name.contains("nomic") {
            match self {
                Self::Query => "search_query: ",
                Self::Passage => "search_document: ",
            }
        } else if name.contains("e5") {
            match self {
                Self::Query => "query: ",
                Self::Passage => "passage: ",
            }
        } else if name.contains("bge") {
            // BGE prefixes the query only; a prefixed passage is worse than an
            // unprefixed one for this family.
            match self {
                Self::Query => "Represent this sentence for searching relevant passages: ",
                Self::Passage => "",
            }
        } else {
            ""
        }
    }
}

/// Why an embedder could not be loaded.
///
/// Every variant is a startup condition. None of them is something a caller
/// should retry per request — that would reintroduce the availability-dependent
/// behaviour this module exists to remove.
#[derive(Debug)]
pub enum LoadError {
    /// Built without the `local` feature.
    BackendUnavailable,
    /// A configured path does not exist or cannot be read.
    Missing { what: &'static str, path: PathBuf },
    /// The file exists but is not the thing it was supposed to be.
    Invalid { what: &'static str, detail: String },
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BackendUnavailable => write!(
                f,
                "in-process embedding is not compiled in (build with the `local` feature)"
            ),
            Self::Missing { what, path } => {
                write!(f, "the {what} was not found at {}", path.display())
            }
            Self::Invalid { what, detail } => write!(f, "the {what} could not be read: {detail}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// Why an embedding call failed.
#[derive(Debug)]
pub enum EmbedError {
    /// The model's graph does not declare an input this code must supply.
    MissingInput(String),
    /// Tokenization failed.
    Tokenize(String),
    /// The forward pass failed.
    Inference(String),
    /// The output tensor was not the shape a sentence encoder produces.
    UnexpectedOutput(String),
}

impl fmt::Display for EmbedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingInput(name) => write!(f, "the model has no `{name}` input"),
            Self::Tokenize(detail) => write!(f, "tokenization failed: {detail}"),
            Self::Inference(detail) => write!(f, "inference failed: {detail}"),
            Self::UnexpectedOutput(detail) => write!(f, "unexpected model output: {detail}"),
        }
    }
}

impl std::error::Error for EmbedError {}

/// Which model produced a set of vectors.
///
/// Recorded alongside stored vectors so a later mismatch is detectable. Two
/// embedders that disagree on any field produce vectors that must not be
/// compared: cosine similarity between different models' spaces is a number
/// with no meaning, and nothing about the vectors themselves says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The backend that produced them.
    pub backend: String,
    /// The model file's stem.
    pub model: String,
    /// Vector width.
    pub dim: usize,
}

/// The backend name this crate reports, distinct from `ollama`.
pub const BACKEND: &str = "rten-local";

/// Mean-pools `hidden` over the non-padding positions and L2-normalizes.
///
/// # Why mean pooling and not the CLS token
///
/// Sentence-transformer checkpoints are almost universally trained with mean
/// pooling; taking CLS from one of them yields a vector that is a valid tensor
/// and a poor embedding, with nothing failing to indicate it.
///
/// # Why the mask is not optional
///
/// A batch is padded to its longest member, and padding positions still carry
/// activations. Averaging them in makes a short text's vector depend on what
/// else happened to be in its batch — the same text would embed differently
/// alongside a long neighbour, which breaks the determinism this crate is for.
///
/// `hidden` is `[tokens][width]` for one sequence; `mask` is one entry per token.
fn mean_pool(hidden: &[Vec<f32>], mask: &[i32]) -> Vec<f32> {
    let width = hidden.first().map_or(0, Vec::len);
    let mut pooled = vec![0f32; width];
    let mut counted = 0f32;

    for (row, keep) in hidden.iter().zip(mask.iter()) {
        if *keep == 0 {
            continue;
        }
        counted += 1.0;
        for (slot, value) in pooled.iter_mut().zip(row.iter()) {
            *slot += value;
        }
    }

    // An all-padding row is possible for empty input text. Returning zeros is
    // right — it is the honest answer for "no content" — and callers treat a
    // zero vector as "no semantic signal" rather than as a point in the space.
    if counted == 0.0 {
        return pooled;
    }
    for slot in &mut pooled {
        *slot /= counted;
    }
    l2_normalize(&mut pooled);
    pooled
}

/// Scales `vector` to unit length, leaving a zero vector alone.
///
/// Callers compare with a dot product and expect it to *be* cosine similarity;
/// that identity holds only for unit vectors. Normalizing here rather than at
/// each comparison means a vector cannot be stored un-normalized and silently
/// compared as though it were.
fn l2_normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in vector.iter_mut() {
            *value /= norm;
        }
    }
}

/// Checks that both paths exist before anything heavier is attempted.
///
/// Separated from loading so the "you configured this wrong" case is reported as
/// itself, rather than as whatever error a model parser produces when handed a
/// missing file.
fn require_files(model: &Path, tokenizer: &Path) -> Result<(), LoadError> {
    if !model.is_file() {
        return Err(LoadError::Missing {
            what: "model file",
            path: model.to_path_buf(),
        });
    }
    if !tokenizer.is_file() {
        return Err(LoadError::Missing {
            what: "tokenizer file",
            path: tokenizer.to_path_buf(),
        });
    }
    Ok(())
}

/// The model file's stem, for [`Identity::model`] and prefix matching.
fn model_name(model: &Path) -> String {
    model
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(feature = "local")]
pub use local::LocalEmbedder;

#[cfg(feature = "local")]
mod local {
    use super::*;
    use rten::{Model, NodeId, ValueOrView};
    use rten_tensor::prelude::*;
    use tokenizers::Tokenizer;

    /// A sentence encoder loaded from local files.
    ///
    /// Construct once at startup and share it; loading is expensive and the
    /// result is immutable, so a single instance serves every request.
    pub struct LocalEmbedder {
        model: Model,
        tokenizer: Tokenizer,
        name: String,
        dim: usize,
    }

    impl std::fmt::Debug for LocalEmbedder {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            // `Model` and `Tokenizer` are not Debug, and dumping a model would be
            // useless anyway. The identity is what a log line wants.
            f.debug_struct("LocalEmbedder")
                .field("model", &self.name)
                .field("dim", &self.dim)
                .finish()
        }
    }

    impl LocalEmbedder {
        /// Loads a model and tokenizer, or fails.
        ///
        /// The dimension is discovered by embedding a probe string rather than
        /// configured, because a configured dimension that disagrees with the
        /// model is undetectable until vectors are compared and produce
        /// nonsense. Discovery costs one forward pass, once.
        pub fn load(model_path: &Path, tokenizer_path: &Path) -> Result<Self, LoadError> {
            require_files(model_path, tokenizer_path)?;

            let model = Model::load_file(model_path).map_err(|error| LoadError::Invalid {
                what: "model file",
                detail: error.to_string(),
            })?;
            let tokenizer =
                Tokenizer::from_file(tokenizer_path).map_err(|error| LoadError::Invalid {
                    what: "tokenizer file",
                    detail: error.to_string(),
                })?;

            let mut embedder = Self {
                model,
                tokenizer,
                name: model_name(model_path),
                dim: 0,
            };

            let probe = embedder
                .embed(&["dimension probe".to_owned()], EmbedRole::Passage)
                .map_err(|error| LoadError::Invalid {
                    what: "model file",
                    detail: format!("it loaded but could not embed: {error}"),
                })?;
            embedder.dim = probe.first().map_or(0, Vec::len);
            if embedder.dim == 0 {
                return Err(LoadError::Invalid {
                    what: "model file",
                    detail: "it produced a zero-width vector".to_owned(),
                });
            }
            Ok(embedder)
        }

        /// The vector width this embedder produces.
        pub fn dim(&self) -> usize {
            self.dim
        }

        /// Which model this is.
        pub fn identity(&self) -> Identity {
            Identity {
                backend: BACKEND.to_owned(),
                model: self.name.clone(),
                dim: self.dim,
            }
        }

        /// Embeds `texts`, returning one unit vector each, in the same order.
        ///
        /// Order is part of the contract: callers pin results by index, and a
        /// reordered return would silently attach every vector to the wrong text.
        pub fn embed(
            &self,
            texts: &[String],
            role: EmbedRole,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            if texts.is_empty() {
                return Ok(Vec::new());
            }

            let prefix = role.prefix_for(&self.name);
            let prepared: Vec<String> =
                texts.iter().map(|text| format!("{prefix}{text}")).collect();

            let encodings = self
                .tokenizer
                .encode_batch(prepared, true)
                .map_err(|error| EmbedError::Tokenize(error.to_string()))?;

            let rows = encodings.len();
            let width = encodings
                .iter()
                .map(|encoding| encoding.get_ids().len())
                .max()
                .unwrap_or(0);
            if width == 0 {
                return Ok(vec![Vec::new(); rows]);
            }

            let mut ids = Vec::with_capacity(rows * width);
            let mut masks = Vec::with_capacity(rows * width);
            let mut types = Vec::with_capacity(rows * width);
            for encoding in &encodings {
                for column in 0..width {
                    ids.push(*encoding.get_ids().get(column).unwrap_or(&0) as i32);
                    masks.push(*encoding.get_attention_mask().get(column).unwrap_or(&0) as i32);
                    types.push(*encoding.get_type_ids().get(column).unwrap_or(&0) as i32);
                }
            }

            let id_tensor = rten_tensor::NdTensor::from_data([rows, width], ids);
            let mask_tensor = rten_tensor::NdTensor::from_data([rows, width], masks.clone());
            let type_tensor = rten_tensor::NdTensor::from_data([rows, width], types);

            let node = |name: &str| {
                self.model
                    .find_node(name)
                    .ok_or_else(|| EmbedError::MissingInput(name.to_owned()))
            };
            let mut inputs: Vec<(NodeId, ValueOrView)> = vec![
                (node("input_ids")?, id_tensor.view().into()),
                (node("attention_mask")?, mask_tensor.view().into()),
            ];
            // Fed only when the graph declares it: supplying an input a model
            // does not declare is an error, and many sentence-encoder exports
            // drop segment ids entirely.
            if let Some(id) = self.model.find_node("token_type_ids") {
                inputs.push((id, type_tensor.view().into()));
            }

            let output_id =
                *self.model.output_ids().first().ok_or_else(|| {
                    EmbedError::UnexpectedOutput("no declared outputs".to_owned())
                })?;
            let outputs = self
                .model
                .run(inputs, &[output_id], None)
                .map_err(|error| EmbedError::Inference(error.to_string()))?;

            let hidden: rten_tensor::Tensor<f32> = outputs
                .into_iter()
                .next()
                .ok_or_else(|| EmbedError::UnexpectedOutput("no output returned".to_owned()))?
                .try_into()
                .map_err(|error| {
                    EmbedError::UnexpectedOutput(format!("output was not f32: {error}"))
                })?;

            let flat: Vec<f32> = hidden.iter().copied().collect();
            let expected = rows * width;
            if expected == 0 || !flat.len().is_multiple_of(expected) {
                return Err(EmbedError::UnexpectedOutput(format!(
                    "{} values for {rows}x{width} tokens; not a [batch, tokens, hidden] tensor",
                    flat.len()
                )));
            }
            let hidden_width = flat.len() / expected;

            let mut out = Vec::with_capacity(rows);
            for row in 0..rows {
                let sequence: Vec<Vec<f32>> = (0..width)
                    .map(|token| {
                        let start = (row * width + token) * hidden_width;
                        flat[start..start + hidden_width].to_vec()
                    })
                    .collect();
                out.push(mean_pool(&sequence, &masks[row * width..(row + 1) * width]));
            }
            Ok(out)
        }
    }
}

#[cfg(not(feature = "local"))]
pub use unavailable::LocalEmbedder;

#[cfg(not(feature = "local"))]
mod unavailable {
    use super::*;

    /// The `local`-feature-off stand-in.
    ///
    /// It exists so a caller's code is identical whether or not the feature is
    /// on — only [`LocalEmbedder::load`]'s result differs. An uninhabited type
    /// rather than a no-op embedder: there is no such thing as a degraded
    /// embedding here, and a type that cannot be constructed says so at compile
    /// time rather than at the first comparison of meaningless vectors.
    #[derive(Debug)]
    pub enum LocalEmbedder {}

    impl LocalEmbedder {
        /// Always [`LoadError::BackendUnavailable`].
        ///
        /// The paths are still checked, so a deployment that has its files in
        /// order but its build wrong gets told which of the two is wrong.
        pub fn load(model_path: &Path, tokenizer_path: &Path) -> Result<Self, LoadError> {
            require_files(model_path, tokenizer_path)?;
            Err(LoadError::BackendUnavailable)
        }

        /// Unreachable — no value of this type exists.
        pub fn dim(&self) -> usize {
            match *self {}
        }

        /// Unreachable — no value of this type exists.
        pub fn identity(&self) -> Identity {
            match *self {}
        }

        /// Unreachable — no value of this type exists.
        pub fn embed(
            &self,
            _texts: &[String],
            _role: EmbedRole,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            match *self {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_positions_do_not_change_a_vector() {
        // The property that makes batching safe. Without the mask, the same text
        // would embed differently depending on how long its batch-mates were.
        let hidden = vec![vec![1.0, 0.0], vec![0.0, 1.0], vec![9.0, 9.0]];

        let unpadded = mean_pool(&hidden[..2], &[1, 1]);
        let padded = mean_pool(&hidden, &[1, 1, 0]);

        assert_eq!(unpadded, padded);
    }

    #[test]
    fn pooled_vectors_are_unit_length() {
        // Callers compare with a dot product and expect cosine similarity. That
        // identity only holds for unit vectors.
        let pooled = mean_pool(&[vec![3.0, 4.0], vec![1.0, 2.0]], &[1, 1]);
        let norm = pooled.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6, "norm was {norm}");
    }

    #[test]
    fn an_all_padding_row_pools_to_zeros_rather_than_nan() {
        // Empty input text is reachable, and dividing by a zero count would give
        // a vector of NaN that compares false against everything including
        // itself — a bug that looks like "semantic search found nothing".
        let pooled = mean_pool(&[vec![1.0, 2.0]], &[0]);
        assert_eq!(pooled, vec![0.0, 0.0]);
        assert!(pooled.iter().all(|v| !v.is_nan()));
    }

    #[test]
    fn query_and_passage_prefixes_differ_where_the_family_uses_them() {
        // Getting these backwards is silent: both sides embed, and retrieval is
        // merely worse.
        assert_eq!(
            EmbedRole::Query.prefix_for("nomic-embed-text"),
            "search_query: "
        );
        assert_eq!(
            EmbedRole::Passage.prefix_for("nomic-embed-text"),
            "search_document: "
        );
        assert_eq!(EmbedRole::Query.prefix_for("e5-small"), "query: ");
        assert_eq!(EmbedRole::Passage.prefix_for("e5-small"), "passage: ");
    }

    #[test]
    fn bge_prefixes_the_query_only() {
        // The one family where the asymmetry is not a matched pair. Prefixing a
        // BGE passage makes retrieval worse rather than better.
        assert!(EmbedRole::Query
            .prefix_for("bge-small-en")
            .starts_with("Represent"));
        assert_eq!(EmbedRole::Passage.prefix_for("bge-small-en"), "");
    }

    #[test]
    fn an_unrecognized_model_gets_no_prefix() {
        // The right guess: a prefix a model was not trained on is noise on every
        // single input, and unlike a missing prefix it cannot be recovered from.
        assert_eq!(EmbedRole::Query.prefix_for("my-finetune-v3"), "");
        assert_eq!(EmbedRole::Passage.prefix_for("my-finetune-v3"), "");
    }

    #[test]
    fn a_missing_file_is_reported_as_itself() {
        // Reported before anything heavier runs, so "you configured this wrong"
        // does not surface as a model parser's complaint about a missing file.
        let error = LocalEmbedder::load(
            Path::new("/nonexistent/model.rten"),
            Path::new("/nonexistent/tokenizer.json"),
        )
        .expect_err("a missing model must not load");

        assert!(
            matches!(
                error,
                LoadError::Missing {
                    what: "model file",
                    ..
                }
            ),
            "got {error:?}"
        );
        assert!(error.to_string().contains("/nonexistent/model.rten"));
    }

    #[test]
    fn loading_never_returns_a_degraded_embedder() {
        // The invariant behind the whole module: there is no "embedder that
        // sometimes works". A caller resolves this once at startup, so the
        // semantic tier cannot switch off for one request and on for the next.
        let outcome = LocalEmbedder::load(
            Path::new("/nonexistent/model.rten"),
            Path::new("/nonexistent/tokenizer.json"),
        );
        assert!(outcome.is_err());
    }
}
