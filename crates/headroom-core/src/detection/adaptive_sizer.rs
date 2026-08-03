//! Per-content-type size thresholds.
//!
//! Below a certain size, compressing a block is not worth it: the CCR marker, the
//! side-channel metadata, and the structural overhead of the compressed form add
//! tokens that a small input cannot amortize. The sizer is the cheapest guard on
//! the invariant I5 path — declining here costs one integer comparison, whereas
//! discovering the same thing after compressing costs a full tokenizer pass over
//! both the original and the compressed form.

use super::ContentType;

/// Size floors below which compression is not attempted.
///
/// # Example
///
/// ```
/// use headroom_core::detection::{AdaptiveSizer, ContentType};
///
/// let sizer = AdaptiveSizer::new();
/// assert!(!sizer.should_attempt(ContentType::Json, 500));
/// assert!(sizer.should_attempt(ContentType::Json, 2_000));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdaptiveSizer {
    /// Minimum bytes before source code is compressed.
    ///
    /// Highest floor of the structured types: code compression is AST-based, and a
    /// short fragment is usually all signature and no elidable body.
    pub code: usize,
    /// Minimum bytes before JSON is compressed.
    ///
    /// SmartCrusher works by summarizing repeated record structure, which needs
    /// enough records present to have repetition to find.
    pub json: usize,
    /// Minimum bytes before logs are compressed.
    ///
    /// Lowest floor, because log compression works by collapsing repeated line
    /// templates and pays off after only a handful of similar lines.
    pub log: usize,
    /// Minimum bytes before plain text is compressed.
    ///
    /// Highest floor overall: prose compression is the lossiest and least
    /// predictable transform, so it should only run when the payoff is substantial.
    pub text: usize,
}

impl Default for AdaptiveSizer {
    fn default() -> Self {
        Self {
            code: 2 * 1024,
            json: 1024,
            log: 500,
            text: 5 * 1024,
        }
    }
}

impl AdaptiveSizer {
    /// Creates a sizer with the documented default thresholds.
    pub fn new() -> Self {
        Self::default()
    }

    /// The threshold for a given content type, in bytes.
    ///
    /// [`ContentType::Unknown`] returns [`usize::MAX`]: unrecognized content is
    /// never compressed at any size.
    pub fn threshold(&self, content_type: ContentType) -> usize {
        match content_type {
            ContentType::Code => self.code,
            ContentType::Json => self.json,
            ContentType::Log => self.log,
            // Diffs and search results are line-oriented and compress by the same
            // repetition-collapsing mechanism as logs, so they share its floor.
            ContentType::Diff | ContentType::SearchResults => self.log,
            ContentType::Prose => self.text,
            ContentType::Unknown => usize::MAX,
        }
    }

    /// Whether a block of `byte_len` bytes is worth attempting to compress.
    ///
    /// The comparison is strictly greater-than: a block exactly at the threshold is
    /// not compressed. The thresholds are documented as "> 2 KB", "> 1 KB", and so
    /// on, and at the boundary itself the expected saving is indistinguishable from
    /// the overhead.
    pub fn should_attempt(&self, content_type: ContentType, byte_len: usize) -> bool {
        byte_len > self.threshold(content_type)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_thresholds() {
        let s = AdaptiveSizer::new();
        assert_eq!(s.threshold(ContentType::Code), 2048);
        assert_eq!(s.threshold(ContentType::Json), 1024);
        assert_eq!(s.threshold(ContentType::Log), 500);
        assert_eq!(s.threshold(ContentType::Prose), 5120);
    }

    #[test]
    fn boundaries_are_exclusive() {
        let s = AdaptiveSizer::new();
        for ct in [
            ContentType::Code,
            ContentType::Json,
            ContentType::Log,
            ContentType::Prose,
        ] {
            let t = s.threshold(ct);
            assert!(!s.should_attempt(ct, t - 1), "{ct}: one byte under");
            assert!(!s.should_attempt(ct, t), "{ct}: exactly at threshold");
            assert!(s.should_attempt(ct, t + 1), "{ct}: one byte over");
        }
    }

    #[test]
    fn unknown_content_is_never_compressed() {
        let s = AdaptiveSizer::new();
        assert!(!s.should_attempt(ContentType::Unknown, usize::MAX));
        assert!(!s.should_attempt(ContentType::Unknown, 0));
    }

    #[test]
    fn empty_blocks_are_never_compressed() {
        let s = AdaptiveSizer::new();
        for ct in [
            ContentType::Code,
            ContentType::Json,
            ContentType::Log,
            ContentType::Diff,
            ContentType::SearchResults,
            ContentType::Prose,
        ] {
            assert!(!s.should_attempt(ct, 0), "{ct}");
        }
    }

    #[test]
    fn line_oriented_types_share_the_log_floor() {
        let s = AdaptiveSizer::new();
        assert_eq!(
            s.threshold(ContentType::Diff),
            s.threshold(ContentType::Log)
        );
        assert_eq!(
            s.threshold(ContentType::SearchResults),
            s.threshold(ContentType::Log)
        );
    }

    #[test]
    fn thresholds_are_configurable() {
        let s = AdaptiveSizer {
            json: 10,
            ..AdaptiveSizer::new()
        };
        assert!(s.should_attempt(ContentType::Json, 11));
        // Overriding one field must not disturb the others.
        assert_eq!(s.threshold(ContentType::Code), 2048);
    }
}
