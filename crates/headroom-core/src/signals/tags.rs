//! Never leaving markup half-closed — gap row S5.
//!
//! # Why an unbalanced tag is worse than the tokens it saves
//!
//! Agent prompts are full of markup: `<thinking>`, `<result>`, `<file path="...">`.
//! A line-oriented compressor that drops a closing `</result>` leaves the model reading
//! a document where everything after that point is *inside* a section that never ends.
//!
//! That is not a degraded result — it is a differently-structured one. The model will
//! attribute the following content to the wrong section and act on it, and nothing in
//! the output says the structure was damaged rather than authored that way.
//!
//! # This does not parse XML
//!
//! It is a balance check over tag-shaped tokens, and it says so. Real markup has
//! namespaces, CDATA, and attribute values containing `>`. A compressor operating on a
//! tool result does not need a parser — it needs to know which lines it must not drop,
//! and to be wrong in the direction of keeping them.

use std::collections::BTreeMap;

/// A tag-shaped token found in content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    /// Line the tag appears on.
    pub line: usize,
    /// The tag name, lowercased.
    pub name: String,
    /// Whether it closes rather than opens.
    pub closing: bool,
}

/// Finds tag-shaped tokens in `lines`.
///
/// Self-closing tags (`<br/>`) are ignored: they open and close in one place, so no
/// other line depends on them.
pub fn find_tags(lines: &[&str]) -> Vec<Tag> {
    let mut tags = Vec::new();

    for (index, line) in lines.iter().enumerate() {
        let bytes = line.as_bytes();
        let mut position = 0usize;

        while position < bytes.len() {
            let Some(open) = bytes[position..].iter().position(|b| *b == b'<') else {
                break;
            };
            let start = position + open;
            let Some(close) = bytes[start..].iter().position(|b| *b == b'>') else {
                break;
            };
            let end = start + close;

            // Slicing by byte index into a `&str` panics on a non-boundary, and content
            // here is arbitrary UTF-8. `get` returns `None` instead, which skips the
            // token rather than taking down the request.
            if let Some(inner) = line.get(start + 1..end) {
                if let Some(tag) = parse_tag(inner, index) {
                    tags.push(tag);
                }
            }

            position = end + 1;
        }
    }

    tags
}

/// Parses the text between `<` and `>`.
fn parse_tag(inner: &str, line: usize) -> Option<Tag> {
    let inner = inner.trim();
    if inner.is_empty() {
        return None;
    }

    // Self-closing. Opens and closes in one place, so nothing else depends on it.
    if inner.ends_with('/') {
        return None;
    }
    // Comments, doctypes, and processing instructions are not paired tags.
    if inner.starts_with('!') || inner.starts_with('?') {
        return None;
    }

    let (closing, rest) = match inner.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, inner),
    };

    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | ':'))
        .collect();

    // A name must start with a letter. `<3` in prose is a heart, not a tag, and
    // treating it as one would make every casual message look like damaged markup.
    if name.is_empty() || !name.starts_with(|c: char| c.is_alphabetic()) {
        return None;
    }

    Some(Tag {
        line,
        name: name.to_ascii_lowercase(),
        closing,
    })
}

/// Lines that must survive so no tag is left unbalanced.
///
/// Returned ascending and deduplicated.
///
/// # What "protected" means here
///
/// A line is protected when dropping it would leave a tag open that was closed, or
/// closed that was never opened. Content *between* a matched pair is not protected —
/// that is the compressible part, and protecting it would mean protecting the whole
/// document.
///
/// # Example
///
/// ```
/// use headroom_core::signals::tags::protected_lines;
///
/// let lines = ["<result>", "  bulky content", "  more content", "</result>"];
/// let protected = protected_lines(&lines);
///
/// // The delimiters must stay; what they wrap is fair game.
/// assert_eq!(protected, vec![0, 3]);
/// ```
pub fn protected_lines(lines: &[&str]) -> Vec<usize> {
    let tags = find_tags(lines);
    let mut protected = Vec::new();

    // One stack per tag name rather than a single stack, because interleaved markup is
    // common in generated content — `<a><b></a></b>` is malformed, and a single stack
    // would report the wrong lines for it rather than simply protecting both.
    let mut open_stacks: BTreeMap<String, Vec<usize>> = BTreeMap::new();

    for tag in &tags {
        if tag.closing {
            match open_stacks.get_mut(&tag.name).and_then(Vec::pop) {
                // A matched pair: both delimiters are load-bearing.
                Some(open_line) => {
                    protected.push(open_line);
                    protected.push(tag.line);
                }
                // A close with no open. The content is already unbalanced, and dropping
                // the line would change *how* it is unbalanced — so it stays.
                None => protected.push(tag.line),
            }
        } else {
            open_stacks
                .entry(tag.name.clone())
                .or_default()
                .push(tag.line);
        }
    }

    // Opens that never closed. Same reasoning as the unmatched close.
    for (_, unclosed) in open_stacks {
        protected.extend(unclosed);
    }

    protected.sort_unstable();
    protected.dedup();
    protected
}

/// Whether removing `removed` from `lines` would leave markup unbalanced.
///
/// Cheaper to answer once over a whole plan than to reason about per line — which is what
/// it was written for, and no compressor does it. See the "exports with no caller" list in
/// [`crate::signals`]; the diff compressor has a measured case this would catch.
pub fn breaks_markup(lines: &[&str], removed: &[usize]) -> bool {
    let protected = protected_lines(lines);
    removed
        .iter()
        .any(|line| protected.binary_search(line).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- finding tags ----

    #[test]
    fn opening_and_closing_tags_are_found() {
        let lines = ["<result>", "body", "</result>"];
        let tags = find_tags(&lines);

        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].name, "result");
        assert!(!tags[0].closing);
        assert!(tags[1].closing);
    }

    #[test]
    fn attributes_do_not_become_part_of_the_name() {
        let lines = [r#"<file path="src/main.rs" mode="rw">"#];
        assert_eq!(find_tags(&lines)[0].name, "file");
    }

    #[test]
    fn tag_names_are_matched_case_insensitively() {
        // `<Result>` closed by `</result>` is a matched pair as far as a reader is
        // concerned, and treating it as unbalanced would protect both lines for no
        // reason.
        let lines = ["<Result>", "body", "</RESULT>"];
        assert_eq!(protected_lines(&lines), vec![0, 2]);
    }

    #[test]
    fn a_self_closing_tag_is_ignored() {
        // It opens and closes in one place, so no other line depends on it.
        let lines = ["<br/>", "body", "<img src='x' />"];
        assert!(find_tags(&lines).is_empty());
    }

    #[test]
    fn comments_and_doctypes_are_not_tags() {
        let lines = [
            "<!-- a comment -->",
            "<!DOCTYPE html>",
            "<?xml version='1'?>",
        ];
        assert!(find_tags(&lines).is_empty(), "{:?}", find_tags(&lines));
    }

    #[test]
    fn prose_containing_angle_brackets_is_not_markup() {
        // `<3` is a heart and `a < b` is a comparison. Treating either as a tag would
        // make every casual message look like damaged markup and protect lines at
        // random.
        for line in ["I <3 this", "if a < b then", "x -> y", "<>", "< spaced >"] {
            let lines = [line];
            let tags = find_tags(&lines);
            assert!(
                tags.is_empty() || tags[0].name.starts_with(|c: char| c.is_alphabetic()),
                "{line:?} produced {tags:?}"
            );
        }
        assert!(find_tags(&["I <3 this"]).is_empty());
    }

    #[test]
    fn several_tags_on_one_line_are_all_found() {
        let lines = ["<a><b></b></a>"];
        assert_eq!(find_tags(&lines).len(), 4);
    }

    // ---- protection ----

    #[test]
    fn the_delimiters_are_protected_and_the_content_is_not() {
        // Protecting the content too would mean protecting the whole document, and
        // nothing could ever be compressed.
        let lines = ["<result>", "  bulky", "  more", "</result>"];
        assert_eq!(protected_lines(&lines), vec![0, 3]);
    }

    #[test]
    fn nested_tags_are_matched_to_their_own_pair() {
        let lines = ["<outer>", "<inner>", "body", "</inner>", "</outer>"];
        assert_eq!(protected_lines(&lines), vec![0, 1, 3, 4]);
    }

    #[test]
    fn an_unclosed_tag_is_protected() {
        // The content is already unbalanced. Dropping the line would change *how* it is
        // unbalanced, which is a different document rather than a shorter one.
        let lines = ["<result>", "body", "more"];
        assert_eq!(protected_lines(&lines), vec![0]);
    }

    #[test]
    fn a_close_with_no_open_is_protected() {
        let lines = ["body", "</result>", "more"];
        assert_eq!(protected_lines(&lines), vec![1]);
    }

    #[test]
    fn interleaved_markup_protects_every_delimiter() {
        // `<a><b></a></b>` is malformed. A single shared stack would pair `</a>` with
        // `<b>` and report the wrong lines; per-name stacks simply protect all four.
        let lines = ["<a>", "<b>", "</a>", "</b>"];
        assert_eq!(protected_lines(&lines), vec![0, 1, 2, 3]);
    }

    #[test]
    fn repeated_pairs_are_each_protected() {
        let lines = ["<p>", "one", "</p>", "<p>", "two", "</p>"];
        assert_eq!(protected_lines(&lines), vec![0, 2, 3, 5]);
    }

    #[test]
    fn content_with_no_markup_protects_nothing() {
        let lines = ["plain text", "more plain text", "and more"];
        assert!(protected_lines(&lines).is_empty());
    }

    // ---- the caller's question ----

    #[test]
    fn removing_content_between_tags_is_allowed() {
        let lines = ["<result>", "  bulky", "  more", "</result>"];
        assert!(!breaks_markup(&lines, &[1, 2]));
    }

    #[test]
    fn removing_a_closing_tag_is_refused() {
        // The failure this module exists for: everything after the dropped close reads
        // as though it were inside a section that never ends, and the model acts on it.
        let lines = ["<result>", "  bulky", "</result>", "after"];
        assert!(breaks_markup(&lines, &[2]));
    }

    #[test]
    fn removing_nothing_never_breaks_anything() {
        let lines = ["<result>", "body", "</result>"];
        assert!(!breaks_markup(&lines, &[]));
    }

    // ---- shape and edges ----

    #[test]
    fn protected_lines_are_ascending_and_deduplicated() {
        let lines = ["<a><a>", "body", "</a></a>"];
        let protected = protected_lines(&lines);

        let mut sorted = protected.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(protected, sorted);
    }

    #[test]
    fn protection_is_deterministic() {
        // Invariant I4. Which lines survive must not vary between runs.
        let lines = ["<a>", "<b>", "body", "</a>", "</b>"];
        let first = protected_lines(&lines);
        for _ in 0..25 {
            assert_eq!(protected_lines(&lines), first);
        }
    }

    #[test]
    fn multibyte_content_does_not_panic_the_scanner() {
        // The scan finds `<` and `>` by byte and then slices the string between them.
        // A naive slice on a non-boundary would panic on exactly this input.
        let lines = ["<result>日本語 😀</result>", "café < naïve", "<日本語>"];
        let _ = protected_lines(&lines);
    }

    #[test]
    fn an_unterminated_tag_does_not_loop_forever() {
        // `<` with no `>` — the scan must stop rather than rescanning from the same
        // position.
        let lines = ["<unterminated", "body <also unterminated"];
        assert!(find_tags(&lines).is_empty());
    }

    #[test]
    fn empty_input_is_handled() {
        assert!(find_tags(&[]).is_empty());
        assert!(protected_lines(&[]).is_empty());
        assert!(!breaks_markup(&[], &[]));
    }
}
