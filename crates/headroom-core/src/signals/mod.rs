//! Line-level importance signals.
//!
//! Compressors that work line-by-line — logs, diffs, plain text — all need the same
//! judgment: given more lines than fit, which ones matter? This module is that
//! judgment, factored out so the answer is consistent across compressors rather than
//! re-invented, slightly differently, in each.
//!
//! # The bias
//!
//! Every heuristic here leans toward *keeping* a line. A line wrongly kept costs a few
//! tokens; a line wrongly dropped may be the error the user is looking for. That
//! asymmetry decides every threshold below.

pub mod anchors;
mod keywords;
pub mod tags;
mod tiered;

pub use anchors::{is_removable, select_anchors, Anchor, AnchorKind};
pub use keywords::{is_error_line, keyword_score, ERROR_KEYWORDS};
pub use tags::{breaks_markup, find_tags, protected_lines, Tag};
pub use tiered::{keep_most_important, score_lines, Importance, ScoredLine};
