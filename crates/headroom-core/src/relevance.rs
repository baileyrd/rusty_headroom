//! Relevance scoring — how well an item answers the question that produced it.
//!
//! Every compression decision in this crate before now was **structural**: how
//! repetitive is this record set, which fields are constant, which records are
//! statistically anomalous. Structure is a property of the data alone, which means a
//! tool result is compressed identically whether the user asked "how many files are
//! there" or "show me `src/parser.rs`".
//!
//! That is the gap this module closes. A tool result exists because something asked
//! for it, and the thing that asked is right there in the conversation — the user's
//! newest messages, and the arguments of the tool call this output answers. Scoring
//! items against that lets the planner *pin* what was actually asked about, so it
//! survives an elision that removes its structural peers.
//!
//! # Why BM25 and not embeddings
//!
//! BM25 is keyword overlap with two corrections that matter here:
//!
//! - **Term rarity (IDF).** A term appearing in every item says nothing about which
//!   item to keep. A term appearing in one says a great deal.
//! - **Length normalization.** Without it, longer items score higher purely by
//!   containing more words, and the planner would keep the verbose records rather
//!   than the relevant ones.
//!
//! It needs no model artifact, no network, and no floating-point non-determinism
//! beyond ordinary arithmetic — which matters because invariant I4 requires the same
//! bytes to compress identically on every run, and a compressor that phoned a model
//! server would be neither deterministic nor offline.
//!
//! It also fits the actual traffic. Tool-call arguments are overwhelmingly literal:
//! identifiers, paths, `field=value` filters, error strings. Those appear *verbatim*
//! in the response, which is the case exact-match scoring is best at and semantic
//! matching adds nothing to.
//!
//! A semantic tier would need an embedding model, which is out of scope for this
//! project (ONNX is an explicit exclusion). The reference's own embedding scorer is a
//! stub that returns an error, and its hybrid scorer degrades to BM25 — so what is
//! excluded here is a tier upstream has not finished either.
//!
//! # Determinism
//!
//! Scoring is a pure function of `(query, items)`. No clocks, no randomness, no
//! iteration over a hash map whose order varies between runs. [`Bm25Scorer::score_all`]
//! returns scores positionally, so a caller comparing them never depends on sort
//! stability for equal scores.

use std::collections::BTreeMap;

/// Term-frequency saturation. Standard BM25 `k1`.
///
/// Controls how fast repeated occurrences of a term stop adding score. At 1.2 the
/// fifth occurrence of a term adds very little over the fourth, which is the desired
/// behavior: an item that mentions the query term once is relevant, and one that
/// mentions it twenty times is not twenty times more relevant.
const K1: f64 = 1.2;

/// Length normalization strength. Standard BM25 `b`.
///
/// At 0.75, an item twice the average length is penalized, but not to the point that
/// only the shortest items can ever win.
const B: f64 = 0.75;

/// How relevant one item is to a query.
///
/// A newtype rather than a bare `f64` so a raw BM25 score cannot be mistaken for a
/// normalized one — they are on entirely different scales, and the threshold a caller
/// applies is meaningless without knowing which it holds.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct RelevanceScore(f64);

impl RelevanceScore {
    /// The zero score, meaning no measurable relationship to the query.
    pub const ZERO: Self = Self(0.0);

    /// Wraps a raw score.
    ///
    /// Negative inputs and NaN both clamp to zero. BM25 can produce a negative IDF
    /// for a term appearing in more than half the corpus, and a negative "relevance"
    /// is not a concept any caller here has a use for — it would only invert a
    /// comparison somewhere downstream.
    pub fn new(value: f64) -> Self {
        if value.is_nan() || value < 0.0 {
            Self(0.0)
        } else {
            Self(value)
        }
    }

    /// The raw score.
    pub fn value(self) -> f64 {
        self.0
    }

    /// Whether this score clears `threshold`.
    ///
    /// A zero score never clears any threshold, including zero. "Nothing in common
    /// with the query" must not read as relevant just because a caller passed a
    /// permissive threshold.
    pub fn clears(self, threshold: f64) -> bool {
        self.0 > 0.0 && self.0 >= threshold
    }
}

/// Scores items against a query.
///
/// One method is required. [`RelevanceScorer::score_all`] has a default that scores
/// each item independently; implementations whose scoring depends on the corpus —
/// BM25 does, through term rarity — override it.
pub trait RelevanceScorer {
    /// A short identifier, for telemetry and error messages.
    fn name(&self) -> &'static str;

    /// Scores one item against `query`, with no corpus context.
    fn score(&self, query: &str, item: &str) -> RelevanceScore;

    /// Scores every item against `query`, returning scores positionally.
    ///
    /// Positional rather than sorted, deliberately: the caller pins records by index,
    /// and a sorted return would make it reconstruct the mapping — with ties broken
    /// by whatever the sort happened to do, which is exactly the kind of detail that
    /// makes output differ between runs.
    fn score_all(&self, query: &str, items: &[String]) -> Vec<RelevanceScore> {
        items.iter().map(|item| self.score(query, item)).collect()
    }
}

/// Keyword relevance with TF-IDF weighting and length normalization.
///
/// Stateless and cheap to construct. Corpus statistics are computed per
/// [`RelevanceScorer::score_all`] call rather than held, because the "corpus" here is
/// one tool result — it is different on every request, and caching it across requests
/// would make compression depend on what was compressed before it (I4).
#[derive(Debug, Clone, Copy, Default)]
pub struct Bm25Scorer;

impl Bm25Scorer {
    /// Creates a scorer.
    pub fn new() -> Self {
        Self
    }
}

impl RelevanceScorer for Bm25Scorer {
    fn name(&self) -> &'static str {
        "bm25"
    }

    /// Scores against a single-item corpus.
    ///
    /// With one document every term has the same rarity, so this degrades to term
    /// overlap with length normalization. Useful for a caller holding one item;
    /// [`RelevanceScorer::score_all`] is where BM25 earns its keep.
    fn score(&self, query: &str, item: &str) -> RelevanceScore {
        self.score_all(query, std::slice::from_ref(&item.to_owned()))
            .first()
            .copied()
            .unwrap_or(RelevanceScore::ZERO)
    }

    fn score_all(&self, query: &str, items: &[String]) -> Vec<RelevanceScore> {
        let query_terms = tokenize(query);
        if query_terms.is_empty() || items.is_empty() {
            return vec![RelevanceScore::ZERO; items.len()];
        }

        let documents: Vec<Vec<String>> = items.iter().map(|item| tokenize(item)).collect();
        let total = documents.len() as f64;

        let average_length = {
            let sum: usize = documents.iter().map(Vec::len).sum();
            // Guarded because every item being empty is possible — a record set of
            // `{}` objects tokenizes to nothing — and dividing by it would make every
            // score NaN, which `RelevanceScore::new` would then clamp to zero anyway,
            // but silently and for the wrong reason.
            if sum == 0 {
                return vec![RelevanceScore::ZERO; items.len()];
            }
            sum as f64 / total
        };

        // `BTreeMap` rather than `HashMap`: iteration order never affects the result
        // here, but a deterministic container removes the question entirely, and I4 is
        // the invariant this crate is least willing to leave to argument.
        let mut document_frequency: BTreeMap<&str, usize> = BTreeMap::new();
        for terms in &documents {
            let mut counted: Vec<&str> = Vec::new();
            for term in terms {
                if !counted.contains(&term.as_str()) {
                    counted.push(term.as_str());
                    *document_frequency.entry(term.as_str()).or_insert(0) += 1;
                }
            }
        }

        documents
            .iter()
            .map(|terms| {
                if terms.is_empty() {
                    return RelevanceScore::ZERO;
                }
                let length = terms.len() as f64;
                let mut score = 0.0;

                for query_term in &query_terms {
                    let occurrences =
                        terms.iter().filter(|term| *term == query_term).count() as f64;
                    if occurrences == 0.0 {
                        continue;
                    }

                    let containing =
                        *document_frequency.get(query_term.as_str()).unwrap_or(&0) as f64;

                    // Smoothed IDF. The +0.5 terms and the outer +1.0 keep this
                    // positive even for a term present in every item, where the
                    // unsmoothed form goes negative and would subtract score from an
                    // item for containing what was asked for.
                    let idf = (((total - containing + 0.5) / (containing + 0.5)) + 1.0).ln();

                    let saturated = occurrences * (K1 + 1.0);
                    let normalizer = occurrences + K1 * (1.0 - B + B * (length / average_length));

                    score += idf * (saturated / normalizer);
                }

                RelevanceScore::new(score)
            })
            .collect()
    }
}

/// Splits text into lowercase alphanumeric terms.
///
/// Punctuation separates rather than being stripped, so `"src/parser.rs"` yields
/// `["src", "parser", "rs"]` and matches a query naming any of them. A tokenizer that
/// kept the path whole would only match a query that reproduced it exactly, which is
/// the case least in need of help — an exact string is already findable.
///
/// Underscores and hyphens split for the same reason: `order_id` should match a query
/// saying `order`.
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn an_exact_match_outscores_an_unrelated_item() {
        let scorer = Bm25Scorer::new();
        let corpus = items(&[
            r#"{"id":"a3f9","status":"shipped"}"#,
            r#"{"id":"b7c2","status":"pending"}"#,
            r#"{"id":"c1d8","status":"pending"}"#,
        ]);

        let scores = scorer.score_all("a3f9", &corpus);

        assert!(scores[0] > scores[1], "the matching record did not win");
        assert_eq!(scores[1], RelevanceScore::ZERO);
        assert_eq!(scores[2], RelevanceScore::ZERO);
    }

    #[test]
    fn a_term_in_every_item_does_not_decide_anything() {
        // The property IDF exists for. `status` appears everywhere, so on its own it
        // must not make any item look more relevant than another — and crucially must
        // not push a score negative, which the unsmoothed IDF formula does.
        let scorer = Bm25Scorer::new();
        let corpus = items(&[
            r#"{"status":"ok","id":1}"#,
            r#"{"status":"ok","id":2}"#,
            r#"{"status":"ok","id":3}"#,
        ]);

        let scores = scorer.score_all("status", &corpus);

        for score in &scores {
            assert!(
                score.value() >= 0.0,
                "a universal term produced a negative score: {score:?}"
            );
        }
        assert_eq!(scores[0], scores[1]);
        assert_eq!(scores[1], scores[2]);
    }

    #[test]
    fn a_longer_item_does_not_win_on_length_alone() {
        // Without length normalization the padded record scores higher purely for
        // containing more words, and the planner would keep verbose records rather
        // than relevant ones.
        let scorer = Bm25Scorer::new();
        let padded = format!(r#"{{"id":"target","filler":"{}"}}"#, "lorem ".repeat(200));
        let corpus = items(&[r#"{"id":"target"}"#, &padded]);

        let scores = scorer.score_all("target", &corpus);

        assert!(
            scores[0] > scores[1],
            "the padded item won on length: {:?} vs {:?}",
            scores[0],
            scores[1]
        );
    }

    #[test]
    fn punctuation_separates_rather_than_blocking_a_match() {
        let scorer = Bm25Scorer::new();
        let corpus = items(&[r#"{"path":"src/parser.rs"}"#, r#"{"path":"src/writer.rs"}"#]);

        let scores = scorer.score_all("parser", &corpus);

        assert!(scores[0].clears(0.0));
        assert_eq!(scores[1], RelevanceScore::ZERO);
    }

    #[test]
    fn an_empty_query_scores_nothing_rather_than_everything() {
        // The absent-query path. If this returned nonzero scores, every record would
        // clear the threshold and the planner would pin the entire set — compression
        // silently switching itself off.
        let scorer = Bm25Scorer::new();
        let corpus = items(&[r#"{"id":1}"#, r#"{"id":2}"#]);

        for query in ["", "   ", "!!!"] {
            let scores = scorer.score_all(query, &corpus);
            assert_eq!(scores.len(), 2);
            assert!(
                scores.iter().all(|s| *s == RelevanceScore::ZERO),
                "query {query:?} scored something"
            );
        }
    }

    #[test]
    fn scores_are_returned_positionally() {
        // The planner pins by index. A scorer that sorted its output would silently
        // pin the wrong records.
        let scorer = Bm25Scorer::new();
        let corpus = items(&[r#"{"id":"zzz"}"#, r#"{"id":"match"}"#, r#"{"id":"aaa"}"#]);

        let scores = scorer.score_all("match", &corpus);

        assert_eq!(scores.len(), 3);
        assert_eq!(scores[0], RelevanceScore::ZERO);
        assert!(scores[1].clears(0.0));
        assert_eq!(scores[2], RelevanceScore::ZERO);
    }

    #[test]
    fn scoring_is_deterministic() {
        // I4. Run the same input repeatedly; the scores must be bit-identical, not
        // merely close.
        let scorer = Bm25Scorer::new();
        let corpus: Vec<String> = (0..60)
            .map(|i| format!(r#"{{"id":{i},"path":"src/module_{i}.rs","kind":"file"}}"#))
            .collect();

        let first = scorer.score_all("module_17 file", &corpus);
        for _ in 0..5 {
            let again = scorer.score_all("module_17 file", &corpus);
            assert_eq!(first, again);
        }
    }

    #[test]
    fn an_empty_corpus_and_empty_items_do_not_divide_by_zero() {
        let scorer = Bm25Scorer::new();

        assert!(scorer.score_all("anything", &[]).is_empty());

        let blank = items(&["", "", ""]);
        let scores = scorer.score_all("anything", &blank);
        assert_eq!(scores.len(), 3);
        assert!(scores.iter().all(|s| *s == RelevanceScore::ZERO));
    }

    #[test]
    fn a_zero_score_never_clears_a_threshold() {
        assert!(!RelevanceScore::ZERO.clears(0.0));
        assert!(!RelevanceScore::new(-5.0).clears(0.0));
        assert!(RelevanceScore::new(0.1).clears(0.0));
        assert!(!RelevanceScore::new(0.1).clears(0.5));
    }

    #[test]
    fn a_nan_score_is_not_relevant() {
        // Guards the clamp in `RelevanceScore::new`: NaN compares false against every
        // threshold, so an unclamped NaN would be "not relevant" by accident rather
        // than by rule — and would flip the moment a comparison was written the other
        // way round.
        assert_eq!(RelevanceScore::new(f64::NAN), RelevanceScore::ZERO);
    }
}
