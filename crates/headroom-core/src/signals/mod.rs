//! Line-level importance signals.
//!
//! Given more lines than fit, which ones matter? This module is that judgment.
//!
//! # The bias
//!
//! Every heuristic here leans toward *keeping* a line. A line wrongly kept costs a few
//! tokens; a line wrongly dropped may be the error the user is looking for. That
//! asymmetry decides every threshold below.
//!
//! # Who actually calls this
//!
//! [`crate::text_crusher`], and nothing else. This doc used to claim the judgment was
//! "factored out so the answer is consistent across compressors rather than re-invented,
//! slightly differently, in each", and named logs and diffs as consumers. Neither has
//! ever imported this module.
//!
//! That was measured before it was rewritten, because a shared-helper module with one
//! caller is usually a gap and here it turned out not to be — the other compressors do
//! not drop lines the way plain text does, so they need a different judgment rather than
//! this one:
//!
//! - **Logs** produce a pattern digest, not a filtered document. Rare lines are printed
//!   verbatim with their count, so an `ERROR` line buried in 400 routine ones survives by
//!   being rare. Measured: three planted failures — a connection error, a panic, and a
//!   Python traceback — all survived compression of a 400-line log intact.
//! - **Diffs** keep every `@@` header and mark each elision as `... N unchanged lines
//!   ...`, so the output says where it is incomplete. Measured: 2 hunk headers in, 2 out.
//! - **Search results** group by file rather than ranking lines.
//!
//! # Exports with no caller
//!
//! [`is_removable`], [`is_error_line`], [`keyword_score`], [`ERROR_KEYWORDS`],
//! [`breaks_markup`], [`find_tags`], [`keep_most_important`].
//!
//! Kept rather than deleted — they are a public surface, and each is the natural spelling
//! of a question a future line-dropping compressor will ask — but listed here so the list
//! is a fact someone maintains rather than something a reader has to grep for. The
//! reachability audit checks this list against reality; see check 12.
//!
//! One of them has a known live case. [`breaks_markup`] is the guard against keeping
//! `<result>` while dropping `</result>`, and the diff compressor does exactly that to the
//! final line of its input when that line is trailing context: measured 8 opening tags in
//! and 8 out, against 8 closing tags in and 7 out. It is not wired up because a diff
//! already announces its own elisions, so the imbalance does not mislead a reader the way
//! it would in prose — but that is a judgment about diffs, not a reason the guard is
//! unnecessary in general.

pub mod anchors;
mod keywords;
pub mod tags;
mod tiered;

pub use anchors::{is_removable, select_anchors, Anchor, AnchorKind};
pub use keywords::{is_error_line, keyword_score, ERROR_KEYWORDS};
pub use tags::{breaks_markup, find_tags, protected_lines, Tag};
pub use tiered::{keep_most_important, keep_with_required, score_lines, Importance, ScoredLine};
