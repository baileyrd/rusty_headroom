//! Diff compression by eliding unchanged context.
//!
//! A unified diff is mostly context — lines that did not change, included so a reader
//! can orient. For a model that already has the file available, most of that context
//! is redundant: what it needs is the hunk headers, the changed lines, and enough
//! surrounding context to place them.
//!
//! # What is never dropped
//!
//! **Hunk headers** (`@@ -1,7 +1,9 @@`) carry the line numbers. Losing them makes the
//! diff unusable for anything except reading, since nothing can be located.
//!
//! **Every added and removed line.** These *are* the diff. A compressor that elided
//! changed lines would have produced a smaller file that no longer describes the
//! change.

use std::sync::Arc;

use crate::block::Block;
use crate::ccr::{store_and_mark, CcrStore};
use crate::detection::{detect, AdaptiveSizer, ContentType};
use crate::error::{Declined, Error, Result};
use crate::transform::{LossyTransform, Transform};

const CCR_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// Tuning for diff compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffConfig {
    /// Unchanged lines kept either side of a change.
    ///
    /// Two is enough to place a change without reproducing the file. Zero would make
    /// the diff hard to read against an unfamiliar file.
    pub context_lines: usize,
}

impl Default for DiffConfig {
    fn default() -> Self {
        Self { context_lines: 2 }
    }
}

/// What a diff line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    /// `diff --git`, `---`, `+++`, `index`, and similar.
    FileHeader,
    /// `@@ ... @@`.
    HunkHeader,
    /// An added or removed line.
    Change,
    /// Unchanged context.
    Context,
}

fn classify(line: &str) -> LineKind {
    if line.starts_with("@@") {
        LineKind::HunkHeader
    } else if line.starts_with("diff ")
        || line.starts_with("index ")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("new file")
        || line.starts_with("deleted file")
        || line.starts_with("similarity index")
        || line.starts_with("rename ")
    {
        LineKind::FileHeader
    } else if line.starts_with('+') || line.starts_with('-') {
        LineKind::Change
    } else {
        LineKind::Context
    }
}

/// Filenames whose diffs are machine-generated churn.
///
/// A dependency bump rewrites thousands of lockfile lines while changing one line in the
/// manifest beside it. The manifest line carries the meaning — "we moved to 4.2.1" — and
/// the lockfile is the mechanical consequence, which the model can neither review nor
/// usefully reason about.
///
/// Matched on the file name rather than a path suffix, so a lockfile in any directory is
/// recognized and a file merely *named* like one in a path segment is not.
const LOCKFILES: [&str; 10] = [
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "Cargo.lock",
    "poetry.lock",
    "uv.lock",
    "go.sum",
    "Gemfile.lock",
    "composer.lock",
    "flake.lock",
];

/// Why a hunk was dropped whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Noise {
    /// Generated dependency churn.
    Lockfile,
    /// Every change is a whitespace difference.
    Whitespace,
}

impl Noise {
    /// How the elision is described in the output.
    fn describe(self) -> &'static str {
        match self {
            Self::Lockfile => "lockfile churn",
            Self::Whitespace => "whitespace-only changes",
        }
    }
}

/// The path a file header names, if it names one.
///
/// Reads `+++ b/path` in preference to `diff --git a/x b/x`, because the `+++` line is
/// the post-image and is present in plain `diff -u` output as well as git's.
/// `/dev/null` is not a path anyone cares about the name of.
fn file_path_of(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("+++ ")?.trim();
    if rest == "/dev/null" {
        return None;
    }
    // Strip the `b/` prefix git adds, and any trailing tab-separated timestamp that
    // plain `diff -u` appends.
    let rest = rest.split('\t').next().unwrap_or(rest);
    Some(rest.strip_prefix("b/").unwrap_or(rest))
}

/// Whether `path` names a generated lockfile.
fn is_lockfile(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    LOCKFILES.contains(&name)
}

/// Whether every change in `hunk` is a whitespace difference.
///
/// Compares the added and removed lines as multisets with all whitespace removed. If
/// they match, every `-` line has a `+` counterpart differing only in spacing — a
/// reformat or lint-fix, which changes no behavior and gives the model nothing to reason
/// about.
///
/// Multiset rather than pairwise: a reformat can reorder lines within a hunk, and a
/// pairwise walk would call that a real change. Requiring *both* sides non-empty means a
/// pure insertion of blank lines is not mistaken for a no-op — it added content, even if
/// that content is empty.
fn is_whitespace_only(hunk: &[&str]) -> bool {
    let mut added: Vec<String> = Vec::new();
    let mut removed: Vec<String> = Vec::new();

    for line in hunk {
        let squeezed =
            |text: &str| -> String { text.chars().filter(|c| !c.is_whitespace()).collect() };
        if let Some(rest) = line.strip_prefix('+') {
            added.push(squeezed(rest));
        } else if let Some(rest) = line.strip_prefix('-') {
            removed.push(squeezed(rest));
        }
    }

    if added.is_empty() || removed.is_empty() {
        return false;
    }

    added.sort();
    removed.sort();
    added == removed
}

/// Splits a diff into hunks, pairing each with the file it belongs to.
///
/// Returns `(start, end, path)` per hunk, where `start` is the `@@` line and `end` is
/// one past the hunk's last line. File headers are not part of any hunk, so they always
/// survive — the reader has to be able to see *which* file was elided.
fn hunks<'a>(lines: &[&'a str], kinds: &[LineKind]) -> Vec<(usize, usize, Option<&'a str>)> {
    let mut found = Vec::new();
    let mut path: Option<&str> = None;
    let mut open: Option<(usize, Option<&str>)> = None;

    for (index, kind) in kinds.iter().enumerate() {
        match kind {
            LineKind::FileHeader => {
                if let Some((start, owner)) = open.take() {
                    found.push((start, index, owner));
                }
                if let Some(named) = file_path_of(lines[index]) {
                    path = Some(named);
                }
            }
            LineKind::HunkHeader => {
                if let Some((start, owner)) = open.take() {
                    found.push((start, index, owner));
                }
                open = Some((index, path));
            }
            LineKind::Change | LineKind::Context => {}
        }
    }

    if let Some((start, owner)) = open {
        found.push((start, lines.len(), owner));
    }

    found
}

/// Compresses unified diffs.
pub struct DiffCompressor {
    config: DiffConfig,
    store: Arc<dyn CcrStore>,
    sizer: AdaptiveSizer,
}

impl DiffCompressor {
    /// Creates a compressor backed by `store`.
    pub fn new(store: Arc<dyn CcrStore>) -> Self {
        Self {
            config: DiffConfig::default(),
            store,
            sizer: AdaptiveSizer::default(),
        }
    }

    /// Overrides the configuration.
    pub fn with_config(mut self, config: DiffConfig) -> Self {
        self.config = config;
        self
    }

    /// Compresses `source`, or explains why it declined.
    pub fn compress(&self, source: &str) -> Result<String> {
        if detect(source.as_bytes()).content_type != ContentType::Diff {
            return Err(Error::declined(Declined::WrongContentType));
        }
        if !self.sizer.should_attempt(ContentType::Diff, source.len()) {
            return Err(Error::declined(Declined::BelowThreshold));
        }

        let lines: Vec<&str> = source.lines().collect();
        let kinds: Vec<LineKind> = lines.iter().map(|l| classify(l)).collect();

        // Whole hunks the model gains nothing from, dropped before the context pass so
        // it never spends a keep-window on lines that are about to go. `noise[i]` names
        // why line `i` is going, and is `None` for everything that stays.
        //
        // Context elision alone cannot reach these: a lockfile hunk is *all* changed
        // lines, and every one of them is "the diff" by the rule this compressor was
        // built on. `npm install foo` reshuffles thousands of `package-lock.json` lines
        // around one meaningful `package.json` line, and before this the whole thing was
        // forwarded.
        let mut noise: Vec<Option<Noise>> = vec![None; lines.len()];
        for (start, end, path) in hunks(&lines, &kinds) {
            let reason = if path.is_some_and(is_lockfile) {
                Some(Noise::Lockfile)
            } else if is_whitespace_only(&lines[start..end]) {
                Some(Noise::Whitespace)
            } else {
                None
            };

            if let Some(reason) = reason {
                for slot in noise.iter_mut().take(end).skip(start) {
                    *slot = Some(reason);
                }
            }
        }

        // Mark what survives before emitting anything, so the context window around a
        // change can look forward as well as back.
        let mut keep = vec![false; lines.len()];
        for (index, kind) in kinds.iter().enumerate() {
            if noise[index].is_some() {
                continue;
            }
            match kind {
                LineKind::FileHeader | LineKind::HunkHeader | LineKind::Change => {
                    keep[index] = true;
                    let low = index.saturating_sub(self.config.context_lines);
                    let high = (index + self.config.context_lines).min(lines.len() - 1);
                    for (offset, slot) in keep.iter_mut().enumerate().take(high + 1).skip(low) {
                        if kinds[offset] == LineKind::Context && noise[offset].is_none() {
                            *slot = true;
                        }
                    }
                }
                LineKind::Context => {}
            }
        }

        let elided = keep.iter().filter(|k| !**k).count();
        if elided == 0 {
            // Every line survived, so the only change would be adding a marker.
            return Err(Error::declined(Declined::NotSmaller));
        }

        let marker = store_and_mark(self.store.as_ref(), source.as_bytes(), CCR_TTL)?;

        let mut out = String::new();
        let mut run = 0usize;
        // A dropped hunk is summarized once, at its end, rather than folded into the
        // unchanged-lines counter. "4,812 unchanged lines" would be wrong twice: those
        // lines did change, and the reason they are gone is the part worth saying.
        let mut dropped: Option<(Noise, usize)> = None;

        for (index, line) in lines.iter().enumerate() {
            if let Some(reason) = noise[index] {
                match dropped {
                    Some((current, count)) if current == reason => {
                        dropped = Some((current, count + 1));
                    }
                    Some((current, count)) => {
                        out.push_str(&format!(
                            "... {count} lines of {} elided ...\n",
                            current.describe()
                        ));
                        dropped = Some((reason, 1));
                    }
                    None => dropped = Some((reason, 1)),
                }
                continue;
            }

            if let Some((reason, count)) = dropped.take() {
                out.push_str(&format!(
                    "... {count} lines of {} elided ...\n",
                    reason.describe()
                ));
            }

            if keep[index] {
                if run > 0 {
                    out.push_str(&format!("... {run} unchanged lines ...\n"));
                    run = 0;
                }
                out.push_str(line);
                out.push('\n');
            } else {
                run += 1;
            }
        }
        if let Some((reason, count)) = dropped {
            out.push_str(&format!(
                "... {count} lines of {} elided ...\n",
                reason.describe()
            ));
        }
        if run > 0 {
            out.push_str(&format!("... {run} unchanged lines ...\n"));
        }
        out.push_str(&format!("full content: {marker}\n"));

        Ok(out)
    }
}

impl Transform for DiffCompressor {
    fn name(&self) -> &'static str {
        "diff_compressor"
    }

    fn apply(&self, block: &mut Block) -> Result<()> {
        let compressed = self.compress(block.content())?;
        block.replace_content(compressed);
        Ok(())
    }
}

impl LossyTransform for DiffCompressor {}

#[cfg(test)]
mod noise_tests {
    use super::*;
    use crate::ccr::InMemoryCcrStore;

    fn compressor() -> DiffCompressor {
        DiffCompressor::new(Arc::new(InMemoryCcrStore::new()))
    }

    /// A dependency bump: one meaningful manifest line, thousands of lockfile lines.
    fn dependency_bump() -> String {
        let churn: Vec<String> = (0..400)
            .map(|i| {
                format!(
                    "-      \"resolved\": \"https://registry.example/pkg-{i}/-/pkg-{i}-1.0.0.tgz\"\n+      \"resolved\": \"https://registry.example/pkg-{i}/-/pkg-{i}-1.0.1.tgz\""
                )
            })
            .collect();

        format!(
            "diff --git a/package.json b/package.json\n\
             --- a/package.json\n\
             +++ b/package.json\n\
             @@ -12,7 +12,7 @@\n\
             \u{20}  \"dependencies\": {{\n\
             -    \"widget\": \"4.2.0\",\n\
             +    \"widget\": \"4.2.1\",\n\
             \u{20}    \"other\": \"1.0.0\"\n\
             diff --git a/package-lock.json b/package-lock.json\n\
             --- a/package-lock.json\n\
             +++ b/package-lock.json\n\
             @@ -1,1200 +1,1200 @@\n{}\n",
            churn.join("\n")
        )
    }

    #[test]
    fn a_lockfile_hunk_is_elided_while_the_manifest_survives() {
        // The headline case. `npm install foo` reshuffles thousands of lockfile lines
        // around one line that carries the actual meaning. Context elision cannot reach
        // it: every lockfile line is a *changed* line, which this compressor was built
        // never to drop.
        let source = dependency_bump();
        let out = compressor().compress(&source).expect("should compress");

        assert!(
            out.contains(r#"+    "widget": "4.2.1","#),
            "the manifest change did not survive"
        );
        assert!(
            !out.contains("pkg-200-1.0.1.tgz"),
            "lockfile churn was forwarded whole"
        );
        assert!(
            out.contains("lockfile churn"),
            "the elision was not explained: {}",
            out.lines().take(20).collect::<Vec<_>>().join(" | ")
        );
        assert!(
            out.contains("package-lock.json"),
            "the reader cannot tell which file was elided"
        );
        assert!(out.len() < source.len() / 4);
    }

    #[test]
    fn a_whitespace_only_hunk_is_elided() {
        // Padded past the size threshold with real context, so this exercises the noise
        // pass rather than asserting the behavior of a below-threshold decline.
        let padding: Vec<String> = (0..60)
            .map(|i| format!(" // untouched context line {i} keeping this above threshold"))
            .collect();
        let source = format!(
            "diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,4 +1,4 @@
 fn main() {{
-    let x=1;
-    let y=2;
+    let x = 1;
+    let y = 2;
 }}
@@ -20,3 +20,3 @@
 fn other() {{
{}
-    old_call();
+    new_call();
 }}
",
            padding.join("\n")
        );
        let out = compressor().compress(&source).expect("should compress");

        assert!(
            out.contains("whitespace-only changes"),
            "the reformat hunk was not recognized"
        );
        assert!(
            !out.contains("let x = 1;"),
            "a whitespace-only change survived"
        );
        assert!(
            out.contains("+    new_call();"),
            "a real change in a sibling hunk was dropped"
        );
    }

    #[test]
    fn a_real_change_among_whitespace_noise_is_kept() {
        // The boundary that matters. A hunk that reformats three lines *and* changes a
        // fourth is a real change, and dropping it would lose the edit entirely.
        let hunk = vec![
            "@@ -1,4 +1,4 @@",
            "-    let x=1;",
            "-    call_this();",
            "+    let x = 1;",
            "+    call_that();",
        ];
        assert!(!is_whitespace_only(&hunk));

        let pure = vec!["@@ -1,2 +1,2 @@", "-    let x=1;", "+    let x = 1;"];
        assert!(is_whitespace_only(&pure));
    }

    #[test]
    fn a_pure_insertion_is_not_whitespace_only() {
        // Added blank lines are still added content. Treating an insertion with no
        // removals as a no-op would drop a hunk that genuinely adds something.
        let inserted = vec!["@@ -1,1 +1,3 @@", "+", "+"];
        assert!(!is_whitespace_only(&inserted));
    }

    #[test]
    fn a_lockfile_is_recognized_in_any_directory() {
        assert!(is_lockfile("Cargo.lock"));
        assert!(is_lockfile("crates/headroom-core/Cargo.lock"));
        assert!(is_lockfile("frontend/app/package-lock.json"));

        // Named like one without being one.
        assert!(!is_lockfile("src/Cargo.lock.rs"));
        assert!(!is_lockfile("docs/yarn.lock.md"));
        // A directory named for a lockfile does not make its contents generated.
        assert!(!is_lockfile("Cargo.lock/notes.txt"));
    }

    #[test]
    fn a_diff_of_only_real_changes_is_untouched_by_the_noise_pass() {
        // The compatibility check. Nothing that was compressed before should compress
        // differently now.
        let padding: Vec<String> = (0..60)
            .map(|i| format!(" // untouched context line {i} keeping this above threshold"))
            .collect();
        let source = format!(
            "diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,6 +1,6 @@
 fn main() {{
{}
-    old_call();
+    new_call();
 }}
",
            padding.join("\n")
        );
        let out = compressor().compress(&source).expect("should compress");

        assert!(
            !out.contains("elided ..."),
            "the noise pass fired on a real change"
        );
        assert!(out.contains("+    new_call();"));
        assert!(out.contains("-    old_call();"));
    }

    #[test]
    fn the_elided_content_is_still_retrievable() {
        // Lossy with a retrieval path, like every other offload here. Dropping a hunk
        // with no way back would be destroying content rather than moving it.
        let store = Arc::new(InMemoryCcrStore::new());
        let source = dependency_bump();
        let out = DiffCompressor::new(store.clone())
            .compress(&source)
            .expect("should compress");

        // `parse_marker` takes the marker alone, so it is lifted off the trailer line
        // rather than being handed the whole compressed body.
        let marker = out
            .lines()
            .find_map(|line| line.strip_prefix("full content: "))
            .expect("a marker was emitted");
        let hash = crate::ccr::parse_marker(marker).expect("the marker parses");
        let stored = store.get(hash).expect("store readable").expect("present");
        assert_eq!(stored, source.as_bytes());
    }

    #[test]
    fn the_noise_pass_is_deterministic() {
        let source = dependency_bump();
        let first = compressor().compress(&source).expect("should compress");
        for _ in 0..5 {
            assert_eq!(
                first,
                compressor().compress(&source).expect("should compress")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ccr::InMemoryCcrStore;
    use crate::tokenizer::{HeuristicEstimator, Tokenizer};

    fn compressor() -> (DiffCompressor, Arc<InMemoryCcrStore>) {
        let store = Arc::new(InMemoryCcrStore::new());
        (DiffCompressor::new(store.clone()), store)
    }

    /// A diff with a lot of untouched context around two small changes.
    fn wide_diff() -> String {
        let mut out = String::from("diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,60 +1,60 @@\n");
        for i in 0..30 {
            out.push_str(&format!(" unchanged context line number {i}\n"));
        }
        out.push_str("-let old = compute();\n+let new = compute_v2();\n");
        for i in 30..60 {
            out.push_str(&format!(" unchanged context line number {i}\n"));
        }
        out
    }

    #[test]
    fn unchanged_context_is_elided_and_counted() {
        let (compressor, _store) = compressor();
        let out = compressor.compress(&wide_diff()).expect("should compress");
        assert!(out.contains("unchanged lines ..."), "{out}");
    }

    #[test]
    fn hunk_headers_and_changes_always_survive() {
        // The headers carry the line numbers; the changes are the diff itself.
        let (compressor, _store) = compressor();
        let out = compressor.compress(&wide_diff()).unwrap();

        assert!(
            out.contains("@@ -1,60 +1,60 @@"),
            "hunk header lost:\n{out}"
        );
        assert!(out.contains("-let old = compute();"), "removal lost");
        assert!(out.contains("+let new = compute_v2();"), "addition lost");
        assert!(out.contains("--- a/src/lib.rs"), "file header lost");
    }

    #[test]
    fn context_immediately_around_a_change_is_kept() {
        let (compressor, _store) = compressor();
        let out = compressor.compress(&wide_diff()).unwrap();
        // Two lines either side, per the default config.
        assert!(
            out.contains("context line number 29"),
            "trailing context lost"
        );
        assert!(
            out.contains("context line number 30"),
            "leading context lost"
        );
    }

    #[test]
    fn it_measurably_shrinks_a_context_heavy_diff() {
        let (compressor, _store) = compressor();
        let source = wide_diff();
        let out = compressor.compress(&source).unwrap();

        let estimator = HeuristicEstimator::new();
        assert!(estimator.count(&out) < estimator.count(&source) / 2);
    }

    #[test]
    fn a_diff_that_is_all_changes_declines() {
        // Nothing to elide, so the only effect would be adding a marker.
        let mut source = String::from("--- a/x\n+++ b/x\n@@ -1,40 +1,40 @@\n");
        for i in 0..40 {
            source.push_str(&format!("-old line {i}\n+new line {i}\n"));
        }
        let (compressor, _store) = compressor();
        assert!(compressor.compress(&source).is_err());
    }

    #[test]
    fn non_diff_content_declines_and_stores_nothing() {
        let (compressor, store) = compressor();
        assert!(compressor.compress(&"just prose. ".repeat(100)).is_err());
        assert!(store.is_empty());
    }

    #[test]
    fn compression_is_deterministic() {
        let source = wide_diff();
        let first = {
            let (c, _s) = compressor();
            c.compress(&source).unwrap()
        };
        for _ in 0..20 {
            let (c, _s) = compressor();
            assert_eq!(c.compress(&source).unwrap(), first);
        }
    }
}
