//! Lossless byte reduction — gap row P5.
//!
//! # Why "lossless" is doing real work in that sentence
//!
//! Every other compressor in this crate is lossy: it drops content and leaves a CCR
//! marker so the model can retrieve what was removed. That is why they are gated behind
//! [`crate::auth_mode::CompressionPolicy::lossy_transforms`] and forbidden on
//! subscription and OAuth traffic.
//!
//! The transforms here remove *only* bytes that carry no information — insignificant
//! whitespace between JSON tokens, trailing spaces at end of line. The decoded meaning
//! is bit-identical, so they are safe on every auth mode, which makes them the only
//! compression restricted traffic ever gets.
//!
//! # What they must not touch
//!
//! Whitespace *inside a string* is content. Minifying `{"text": "hello  world"}` to
//! `{"text":"hello world"}` changes what the model reads while calling itself lossless,
//! which is worse than not running at all — a lossy transform that has escaped its
//! policy gate.

use crate::block::Block;
use crate::error::{Declined, Error, Result};
use crate::transform::{LosslessTransform, Transform};

/// Removes insignificant whitespace from JSON.
///
/// Returns `None` when the input is not JSON this function is confident about, rather
/// than returning something plausible. The caller forwards the original in that case,
/// which is the same outcome as not calling it.
///
/// # Example
///
/// ```
/// use headroom_core::pipeline::reformats::minify_json;
///
/// let pretty = "{\n  \"a\": 1,\n  \"b\": [1, 2]\n}";
/// assert_eq!(minify_json(pretty).unwrap(), r#"{"a":1,"b":[1,2]}"#);
///
/// // Whitespace inside a string is content, not formatting.
/// assert_eq!(minify_json(r#"{"t": "a  b"}"#).unwrap(), r#"{"t":"a  b"}"#);
/// ```
pub fn minify_json(content: &str) -> Option<String> {
    // Confirmed to be JSON before a byte is touched. Scanning an arbitrary payload for
    // quotes and brackets would happily "minify" prose into something shorter and
    // wrong.
    serde_json::from_str::<serde_json::Value>(content).ok()?;

    let mut out = String::with_capacity(content.len());
    let mut in_string = false;
    let mut escaped = false;

    for character in content.chars() {
        if escaped {
            out.push(character);
            escaped = false;
            continue;
        }

        match character {
            '\\' if in_string => {
                out.push(character);
                escaped = true;
            }
            '"' => {
                in_string = !in_string;
                out.push(character);
            }
            // The whole point. Inside a string this is content; outside it is layout.
            c if c.is_whitespace() && !in_string => {}
            c => out.push(c),
        }
    }

    // Never returns something longer. Minification that grows a payload is a bug, and
    // returning it would defeat the caller's own I5 check by shrinking the block count
    // while growing the bytes.
    (out.len() < content.len()).then_some(out)
}

/// Strips trailing whitespace from each line and collapses runs of blank lines.
///
/// Lossless for any consumer that treats the text as lines, which is what every
/// line-oriented tool in this crate does.
///
/// # Why blank runs collapse to one rather than to none
///
/// A blank line is a paragraph or stanza boundary, and removing it entirely reflows a
/// log or a document into a wall of text — losing structure a reader and a model both
/// use. Collapsing a run of twelve to one keeps the boundary and drops the padding.
pub fn tidy_lines(content: &str) -> Option<String> {
    // The trailing newline is stripped before splitting and restored after. `split` on
    // a string ending in `\n` yields a final empty element, and emitting it as a line
    // *adds* a blank line to every input that ended properly — turning a tidier into
    // something that grows the payload it was asked to shrink.
    let had_trailing = content.ends_with('\n');
    let body = content.strip_suffix('\n').unwrap_or(content);

    let mut out = String::with_capacity(content.len());
    let mut blank_run = 0usize;

    for line in body.split('\n') {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(trimmed);
        out.push('\n');
    }

    // The input's own trailing newline is preserved rather than invented: a file that
    // ended without one should not gain one, since a diff would show the change.
    if !had_trailing {
        out.pop();
    }

    (out.len() < content.len()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BlockKind;

    #[test]
    fn every_named_step_is_one_that_actually_runs() {
        // `STEPS` is what `apply` iterates *and* what `headroom tools` prints, so the
        // name and the behaviour cannot drift. What this checks is the thing the shared
        // array does not give for free: that each entry does something — a step named,
        // listed to an operator, and never able to fire is a capability reported that
        // does not exist, which is the failure the reachability audit exists for.
        for (name, reformat) in Reformatter::STEPS {
            let input = match name {
                "minify_json" => "{\n  \"a\": 1\n}",
                "tidy_lines" => "alpha  \n\n\n\nbeta\n",
                other => panic!("no fixture for step `{other}` — add one rather than skipping"),
            };

            let direct =
                reformat(input).unwrap_or_else(|| panic!("`{name}` declined its own fixture"));

            // And the same input through `apply` reaches that step rather than another.
            let mut block = Block::new(BlockKind::ToolResult, input.to_string());
            Reformatter::new()
                .apply(&mut block)
                .unwrap_or_else(|err| panic!("apply declined `{name}`'s fixture: {err}"));
            assert_eq!(
                block.content(),
                direct,
                "`{name}` is named but `apply` produced another step's result"
            );
        }
    }

    // ---- JSON minification ----

    #[test]
    fn insignificant_whitespace_is_removed() {
        let pretty = "{\n  \"a\": 1,\n  \"b\": [1, 2, 3]\n}";
        assert_eq!(minify_json(pretty).unwrap(), r#"{"a":1,"b":[1,2,3]}"#);
    }

    #[test]
    fn whitespace_inside_a_string_is_content_and_survives() {
        // The failure this guards. Collapsing it changes what the model reads while
        // calling itself lossless — a lossy transform that has escaped its policy gate.
        for (input, expected) in [
            (r#"{"t": "hello  world"}"#, r#"{"t":"hello  world"}"#),
            (r#"{"t": "line\nbreak"}"#, r#"{"t":"line\nbreak"}"#),
            (r#"{"t": "  padded  "}"#, r#"{"t":"  padded  "}"#),
        ] {
            assert_eq!(minify_json(input).unwrap(), expected, "{input}");
        }
    }

    #[test]
    fn an_escaped_quote_does_not_end_the_string() {
        // A naive scanner sees the escaped quote as a terminator and starts stripping
        // the spaces that follow — inside what is still a string.
        let input = r#"{"t": "he said \"  hello  \" loudly"}"#;
        let out = minify_json(input).unwrap();

        assert!(out.contains(r#"\"  hello  \""#), "{out}");
    }

    #[test]
    fn minification_preserves_the_decoded_value_exactly() {
        // The property that makes this lossless, checked by comparing parsed values
        // rather than by trusting the scanner.
        for input in [
            r#"{ "a" : 1.0 , "b" : [ 1 , 2 ] }"#,
            r#"{ "nested" : { "deep" : { "x" : "  spaces  " } } }"#,
            r#"[ "日本語 😀" , "café" ]"#,
            r#"{ "n" : 9007199254740993 }"#,
        ] {
            let out = minify_json(input).unwrap();
            let before: serde_json::Value = serde_json::from_str(input).unwrap();
            let after: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(before, after, "{input}");
        }
    }

    #[test]
    fn already_minified_json_returns_none() {
        // Nothing to gain, and returning `Some` would make the caller rebuild a body
        // for no reduction — which under invariant I1 costs a cache miss.
        assert_eq!(minify_json(r#"{"a":1,"b":[1,2]}"#), None);
    }

    #[test]
    fn non_json_returns_none_rather_than_something_plausible() {
        // Scanning arbitrary text for quotes and brackets would happily "minify" prose
        // into something shorter and wrong.
        for input in [
            "The quick brown  fox",
            "{not json",
            "",
            "2026-01-01 INFO  worker ok",
        ] {
            assert_eq!(minify_json(input), None, "{input:?}");
        }
    }

    #[test]
    fn minification_is_deterministic() {
        let input = "{\n  \"a\": 1,\n  \"b\": [1, 2]\n}";
        let first = minify_json(input);
        for _ in 0..25 {
            assert_eq!(minify_json(input), first);
        }
    }

    #[test]
    fn minifying_twice_is_a_fixed_point() {
        // Invariant I3. A second pass must find nothing left to do.
        let once = minify_json("{\n  \"a\": 1\n}").unwrap();
        assert_eq!(minify_json(&once), None);
    }

    // ---- line tidying ----

    #[test]
    fn trailing_whitespace_is_removed() {
        let input = "alpha   \nbeta\t\ngamma  ";
        assert_eq!(tidy_lines(input).unwrap(), "alpha\nbeta\ngamma");
    }

    #[test]
    fn a_run_of_blank_lines_collapses_to_one() {
        // Not to none. A blank line is a paragraph boundary, and removing it entirely
        // reflows the text into a wall — losing structure a reader and a model both use.
        let input = "alpha\n\n\n\n\nbeta";
        assert_eq!(tidy_lines(input).unwrap(), "alpha\n\nbeta");
    }

    #[test]
    fn a_single_blank_line_is_kept() {
        assert_eq!(tidy_lines("alpha\n\nbeta"), None, "nothing to remove");
    }

    #[test]
    fn leading_whitespace_is_never_touched() {
        // Indentation is structure in code, YAML, and stack traces. Stripping it would
        // be lossy while claiming otherwise.
        let input = "def f():\n    return 1   \n";
        assert_eq!(tidy_lines(input).unwrap(), "def f():\n    return 1\n");
    }

    #[test]
    fn a_missing_trailing_newline_is_not_invented() {
        // A file that ended without one should not gain one — a diff would show it as a
        // change nobody made.
        assert_eq!(tidy_lines("alpha  \nbeta  ").unwrap(), "alpha\nbeta");
        assert_eq!(tidy_lines("alpha  \nbeta  \n").unwrap(), "alpha\nbeta\n");
    }

    #[test]
    fn tidy_content_returns_none() {
        assert_eq!(tidy_lines("alpha\nbeta\n"), None);
    }

    #[test]
    fn tidying_twice_is_a_fixed_point() {
        let once = tidy_lines("alpha   \n\n\n\nbeta  ").unwrap();
        assert_eq!(tidy_lines(&once), None);
    }

    #[test]
    fn tidying_never_returns_something_longer() {
        // A reformat that grows a payload would defeat the caller's own I5 check.
        for input in ["", "\n", "a", "a\n", "\n\n\n", "  \n  \n"] {
            if let Some(out) = tidy_lines(input) {
                assert!(out.len() < input.len(), "{input:?} grew to {out:?}");
            }
        }
    }

    #[test]
    fn multibyte_content_survives() {
        let input = "日本語   \n😀  \ncafé";
        assert_eq!(tidy_lines(input).unwrap(), "日本語\n😀\ncafé");
    }
}

/// The lossless reformatter, as a [`Transform`].
///
/// # Why this is the transform restricted traffic gets
///
/// Every other compressor in this crate is lossy and gated behind
/// [`CompressionPolicy::lossy_transforms`], so subscription and OAuth traffic received
/// no compression at all. This one removes only bytes that carry no information, which
/// makes it safe on every auth mode — and it is the difference between "restricted
/// means less compression" and "restricted means none".
///
/// # It is still bounded by invariant I5
///
/// The reduction is small: minification saves the whitespace a pretty-printer added,
/// which on already-minified content is nothing. The transform declines rather than
/// returning an unchanged block, so `validated_apply` never rebuilds a body for a
/// zero-byte saving — which under invariant I1 would cost a cache miss to gain nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct Reformatter;

/// One reformat: the name it is reported under, and the function that performs it.
///
/// `None` from the function means "declined" — either nothing to remove, or the content
/// is not this reformat's shape. See [`Reformatter::STEPS`].
pub type ReformatStep = (&'static str, fn(&str) -> Option<String>);

impl Reformatter {
    /// The reformats it tries, in order, paired with the name each is reported under.
    ///
    /// # One list, because the other place that had it was a `writeln!`
    ///
    /// `headroom tools` printed these two names as string literals. It happened to be
    /// right, which is the only thing separating it from the four routing tables that
    /// were not — and being right by luck is not a property that survives somebody adding
    /// a third reformat.
    ///
    /// The names and the functions are the same array, so `apply` below and any command
    /// listing them cannot disagree: a reformat that is not here does not run, and one
    /// that runs is named here.
    pub const STEPS: [ReformatStep; 2] = [("minify_json", minify_json), ("tidy_lines", tidy_lines)];

    /// Creates the reformatter.
    pub fn new() -> Self {
        Self
    }
}

impl Transform for Reformatter {
    fn name(&self) -> &'static str {
        "reformatter"
    }

    fn apply(&self, block: &mut Block) -> Result<()> {
        // Sacrosanct content is excluded here as well as by the live-zone dispatcher.
        // Whitespace in a signed thinking block is covered by the signature — removing
        // it produces content the provider rejects as tampered-with, which is invariant
        // I8 and is not made safe by the change being lossless.
        if block.kind().is_sacrosanct() {
            return Err(Error::Declined(Declined::Sacrosanct));
        }

        // First one that produces something, in declaration order. The order carries the
        // meaning it did as an `.or_else` chain: on pretty-printed JSON both would
        // succeed, and `minify_json` removes the indentation `tidy_lines` would only
        // trim the trailing edge of, so trying it second would leave the larger result.
        let reformatted = Self::STEPS
            .iter()
            .find_map(|(_, reformat)| reformat(block.content()))
            .ok_or(Error::Declined(Declined::NotSmaller))?;

        *block.content_mut() = reformatted;
        Ok(())
    }
}

impl LosslessTransform for Reformatter {}
