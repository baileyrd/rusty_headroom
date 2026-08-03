//! Cross-agent memory, and where it is allowed to go.
//!
//! A memory store lets one agent record something another can read later — a project
//! convention, a failed approach, a path worth remembering. The storage half is
//! ordinary. The half that matters here is *injection*: how a recalled memory gets into
//! a request.
//!
//! # Memory never goes in the system prompt
//!
//! The obvious place is the system prompt, and it is the one place it must not go.
//! The system prompt heads the cached prefix, so writing a memory into it invalidates
//! the cache on every request where the memory set changed — and a memory store is
//! *designed* to change. An agent that learns one fact per turn would bust the cache
//! every turn, paying full price for the entire conversation each time in exchange for
//! a sentence.
//!
//! Memory goes in the **live-zone tail** instead (invariant I2, and the reference's
//! REALIGNMENT §2.6). It costs a few tokens on a message that was never cached, and
//! invalidates nothing.
//!
//! # Deduplication is not an optimization
//!
//! Agents record the same fact repeatedly — the same convention noticed in five files,
//! the same error hit twice. Without dedup the store grows without bound and injection
//! spends its budget repeating one fact. Dedup is content-addressed, so it is exact
//! rather than approximate: two entries collapse when they say the same thing, not when
//! they look similar.

use std::collections::BTreeMap;

use crate::ccr::ContentHash;

/// Where a memory came from.
///
/// Provenance is stored rather than derived because a recalled memory is being handed
/// to a model as fact. "The auth module uses tokens, not sessions" is worth acting on
/// if a code-reading agent recorded it and worth ignoring if it came from a speculative
/// planning step, and the model cannot tell the difference from the text alone.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Provenance {
    /// Which agent recorded it.
    pub agent: String,
    /// What it was doing — a session, task, or run identifier.
    pub context: String,
}

impl Provenance {
    /// Records `agent` working in `context`.
    pub fn new(agent: impl Into<String>, context: impl Into<String>) -> Self {
        Self {
            agent: agent.into(),
            context: context.into(),
        }
    }
}

/// One remembered fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    content: String,
    /// Every source that recorded this fact, deduplicated and ordered.
    ///
    /// A `Vec` rather than a single value: when two agents independently record the
    /// same thing, that agreement is the most useful signal the store has, and keeping
    /// only the first source would throw it away.
    sources: Vec<Provenance>,
    /// How many times this fact has been recorded.
    occurrences: usize,
}

impl Memory {
    /// The remembered text.
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Everything that recorded this fact.
    pub fn sources(&self) -> &[Provenance] {
        &self.sources
    }

    /// How many times it was recorded.
    pub fn occurrences(&self) -> usize {
        self.occurrences
    }

    /// Whether more than one distinct source recorded it.
    ///
    /// Independent agreement is worth more than repetition: one agent noticing the same
    /// thing five times is one observation seen five times, while two agents noticing
    /// it once each is two observations.
    pub fn corroborated(&self) -> bool {
        self.sources.len() > 1
    }
}

/// A content-addressed memory store.
///
/// # Ordering
///
/// Backed by a `BTreeMap` keyed on the content hash, so iteration order is fixed by
/// content rather than by insertion. Recall must be deterministic — invariant I4 —
/// and a `HashMap` would let the same store yield a different injection order between
/// processes, which would then bust the very cache this module is careful about.
#[derive(Debug, Clone, Default)]
pub struct MemoryStore {
    entries: BTreeMap<String, Memory>,
}

impl MemoryStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `content`, merging with any identical existing entry.
    ///
    /// Returns `true` if this was a new fact rather than a repeat.
    ///
    /// # Normalization before hashing
    ///
    /// Content is trimmed and internal whitespace runs collapsed before the hash is
    /// taken, so `"uses tokens"` and `"uses  tokens\n"` are one fact. Without that, two
    /// agents recording the same sentence with different trailing whitespace produce
    /// two entries, and a content-addressed store that misses exact duplicates has
    /// failed at its only distinctive job. The *stored* text is the original.
    pub fn remember(&mut self, content: impl Into<String>, source: Provenance) -> bool {
        let content = content.into();
        let key = ContentHash::of(normalize(&content).as_bytes()).to_hex();

        match self.entries.get_mut(&key) {
            Some(existing) => {
                existing.occurrences += 1;
                if !existing.sources.contains(&source) {
                    existing.sources.push(source);
                    // Sorted so the source list does not depend on arrival order,
                    // which would make recall non-deterministic across runs.
                    existing.sources.sort();
                }
                false
            }
            None => {
                self.entries.insert(
                    key,
                    Memory {
                        content,
                        sources: vec![source],
                        occurrences: 1,
                    },
                );
                true
            }
        }
    }

    /// How many distinct facts are stored.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every fact, in content-hash order.
    pub fn all(&self) -> impl Iterator<Item = &Memory> {
        self.entries.values()
    }

    /// The `limit` most-corroborated facts, most first.
    ///
    /// Ranked by distinct sources, then by occurrences, then by content hash. The final
    /// tie-break is on the key rather than left to sort stability, so the same store
    /// always recalls the same facts in the same order.
    pub fn recall(&self, limit: usize) -> Vec<&Memory> {
        let mut ranked: Vec<(&String, &Memory)> = self.entries.iter().collect();
        ranked.sort_by(|(a_key, a), (b_key, b)| {
            b.sources
                .len()
                .cmp(&a.sources.len())
                .then(b.occurrences.cmp(&a.occurrences))
                .then(a_key.cmp(b_key))
        });
        ranked.into_iter().take(limit).map(|(_, m)| m).collect()
    }
}

/// Collapses whitespace so trivially different spellings of one fact hash alike.
fn normalize(content: &str) -> String {
    content.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Renders recalled memories as a block to append to the live-zone tail.
///
/// Returns `None` when there is nothing to inject, rather than an empty string — an
/// empty append still modifies the message it lands on, and a modified message is a
/// different message.
///
/// # Example
///
/// ```
/// use headroom_core::memory::{inject_block, MemoryStore, Provenance};
///
/// let mut store = MemoryStore::new();
/// store.remember("The auth module uses tokens, not sessions.", Provenance::new("reader", "s1"));
///
/// let block = inject_block(&store, 4).unwrap();
/// assert!(block.contains("uses tokens"));
/// // Never a system-prompt directive — this is context appended to a live message.
/// assert!(!block.to_lowercase().contains("you are"));
/// ```
pub fn inject_block(store: &MemoryStore, limit: usize) -> Option<String> {
    let recalled = store.recall(limit);
    if recalled.is_empty() {
        return None;
    }

    let mut out = String::from("\n\n<memory>\n");
    for memory in recalled {
        out.push_str("- ");
        out.push_str(memory.content().trim());
        // Corroboration is surfaced because it is the one thing that distinguishes a
        // fact two agents agree on from one agent's guess, and the text alone cannot
        // say which it is.
        if memory.corroborated() {
            out.push_str(" (corroborated)");
        }
        out.push('\n');
    }
    out.push_str("</memory>");
    Some(out)
}

/// A namespaced key-value view over shared state between agents.
///
/// Distinct from [`MemoryStore`], which accumulates observations. This holds current
/// values that are meant to be overwritten — the branch being worked on, the file
/// under edit — where the latest write is the answer and history is noise.
#[derive(Debug, Clone, Default)]
pub struct SharedContext {
    values: BTreeMap<String, String>,
}

impl SharedContext {
    /// Creates an empty context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets `key` within `namespace`, returning the previous value.
    ///
    /// Namespaced because two agents will otherwise collide on names as generic as
    /// `status` or `target` — and a silent overwrite between agents is far harder to
    /// diagnose than a missing key.
    pub fn put(&mut self, namespace: &str, key: &str, value: impl Into<String>) -> Option<String> {
        self.values.insert(qualify(namespace, key), value.into())
    }

    /// Reads `key` within `namespace`.
    pub fn get(&self, namespace: &str, key: &str) -> Option<&str> {
        self.values
            .get(&qualify(namespace, key))
            .map(String::as_str)
    }

    /// Removes `key` within `namespace`, returning what was there.
    pub fn remove(&mut self, namespace: &str, key: &str) -> Option<String> {
        self.values.remove(&qualify(namespace, key))
    }

    /// Every key in `namespace`, with its value, in key order.
    pub fn namespace(&self, namespace: &str) -> Vec<(&str, &str)> {
        let prefix = format!("{namespace}\u{1f}");
        self.values
            .iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .map(|(key, value)| (&key[prefix.len()..], value.as_str()))
            .collect()
    }

    /// How many values are held.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether the context is empty.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// Joins a namespace and key with a separator that cannot appear in either.
///
/// A unit separator rather than `:` or `/`, both of which appear in real keys — a file
/// path as a key would otherwise let `put("a", "b/c")` and `put("a/b", "c")` collide.
fn qualify(namespace: &str, key: &str) -> String {
    format!("{namespace}\u{1f}{key}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(agent: &str) -> Provenance {
        Provenance::new(agent, "session-1")
    }

    // ---- dedup ----

    #[test]
    fn recording_the_same_fact_twice_stores_it_once() {
        let mut store = MemoryStore::new();
        assert!(store.remember("uses tokens, not sessions", source("a")));
        assert!(!store.remember("uses tokens, not sessions", source("a")));

        assert_eq!(store.len(), 1);
        assert_eq!(store.all().next().unwrap().occurrences(), 2);
    }

    #[test]
    fn trivial_whitespace_differences_are_one_fact() {
        // A content-addressed store that misses exact duplicates has failed at its only
        // distinctive job, and two agents writing the same sentence with different
        // trailing whitespace is the common case.
        let mut store = MemoryStore::new();
        store.remember("uses tokens, not sessions", source("a"));
        store.remember("  uses   tokens,  not sessions\n", source("b"));

        assert_eq!(store.len(), 1);
    }

    #[test]
    fn genuinely_different_facts_are_kept_apart() {
        // The over-correction: normalization aggressive enough to merge distinct facts
        // silently loses information the agent recorded on purpose.
        let mut store = MemoryStore::new();
        store.remember("uses tokens, not sessions", source("a"));
        store.remember("uses sessions, not tokens", source("a"));
        store.remember("uses tokens", source("a"));

        assert_eq!(store.len(), 3);
    }

    #[test]
    fn the_stored_text_is_the_original_not_the_normalized_form() {
        // Normalization exists to compute a key, not to edit what the agent said.
        let mut store = MemoryStore::new();
        store.remember("prefer\n  indented continuation", source("a"));

        assert_eq!(
            store.all().next().unwrap().content(),
            "prefer\n  indented continuation"
        );
    }

    // ---- provenance ----

    #[test]
    fn two_agents_recording_one_fact_are_both_remembered() {
        // Independent agreement is the most useful signal the store has, and keeping
        // only the first source would throw it away.
        let mut store = MemoryStore::new();
        store.remember("uses tokens", source("reader"));
        store.remember("uses tokens", source("planner"));

        let memory = store.all().next().unwrap();
        assert_eq!(memory.sources().len(), 2);
        assert!(memory.corroborated());
    }

    #[test]
    fn one_agent_repeating_itself_is_not_corroboration() {
        // One observation seen five times is not five observations.
        let mut store = MemoryStore::new();
        for _ in 0..5 {
            store.remember("uses tokens", source("reader"));
        }

        let memory = store.all().next().unwrap();
        assert_eq!(memory.occurrences(), 5);
        assert!(!memory.corroborated());
    }

    #[test]
    fn the_source_list_does_not_depend_on_arrival_order() {
        let mut first = MemoryStore::new();
        first.remember("x", source("b"));
        first.remember("x", source("a"));

        let mut second = MemoryStore::new();
        second.remember("x", source("a"));
        second.remember("x", source("b"));

        assert_eq!(
            first.all().next().unwrap().sources(),
            second.all().next().unwrap().sources()
        );
    }

    // ---- recall ----

    #[test]
    fn recall_is_deterministic() {
        // Invariant I4. A non-deterministic recall order would bust the very cache this
        // module is careful about.
        let mut store = MemoryStore::new();
        for text in ["alpha", "beta", "gamma", "delta"] {
            store.remember(text, source("a"));
        }

        let first: Vec<&str> = store.recall(3).iter().map(|m| m.content()).collect();
        for _ in 0..25 {
            let again: Vec<&str> = store.recall(3).iter().map(|m| m.content()).collect();
            assert_eq!(again, first);
        }
    }

    #[test]
    fn corroborated_facts_are_recalled_first() {
        let mut store = MemoryStore::new();
        store.remember("uncorroborated", source("a"));
        store.remember("agreed on", source("a"));
        store.remember("agreed on", source("b"));

        assert_eq!(store.recall(1)[0].content(), "agreed on");
    }

    #[test]
    fn recall_respects_its_limit() {
        let mut store = MemoryStore::new();
        for i in 0..20 {
            store.remember(format!("fact {i}"), source("a"));
        }

        assert_eq!(store.recall(5).len(), 5);
        assert_eq!(store.recall(0).len(), 0);
        assert_eq!(store.recall(100).len(), 20);
    }

    // ---- injection ----

    #[test]
    fn the_injected_block_is_context_not_a_system_directive() {
        // Memory goes in the live-zone tail. Phrasing it as an instruction would invite
        // a caller to put it where instructions go — the system prompt — which is the
        // one place it must not be.
        let mut store = MemoryStore::new();
        store.remember("the auth module uses tokens", source("a"));

        let block = inject_block(&store, 4).unwrap();
        assert!(block.contains("uses tokens"));
        assert!(!block.to_lowercase().contains("you are"));
        assert!(!block.to_lowercase().contains("you must"));
    }

    #[test]
    fn an_empty_store_injects_nothing_rather_than_an_empty_block() {
        // An empty append still modifies the message it lands on, and a modified
        // message is a different message.
        assert_eq!(inject_block(&MemoryStore::new(), 4), None);
    }

    #[test]
    fn corroboration_is_visible_in_the_injected_block() {
        // The one thing distinguishing a fact two agents agree on from one agent's
        // guess, which the text alone cannot say.
        let mut store = MemoryStore::new();
        store.remember("agreed on", source("a"));
        store.remember("agreed on", source("b"));
        store.remember("just a guess", source("a"));

        let block = inject_block(&store, 4).unwrap();
        let agreed_line = block
            .lines()
            .find(|line| line.contains("agreed on"))
            .unwrap();
        let guess_line = block
            .lines()
            .find(|line| line.contains("just a guess"))
            .unwrap();

        assert!(agreed_line.contains("(corroborated)"));
        assert!(!guess_line.contains("(corroborated)"));
    }

    #[test]
    fn injection_is_deterministic() {
        let mut store = MemoryStore::new();
        for text in ["alpha", "beta", "gamma"] {
            store.remember(text, source("a"));
        }

        let first = inject_block(&store, 3).unwrap();
        for _ in 0..25 {
            assert_eq!(inject_block(&store, 3).unwrap(), first);
        }
    }

    // ---- shared context ----

    #[test]
    fn values_round_trip_within_a_namespace() {
        let mut context = SharedContext::new();
        assert_eq!(context.put("builder", "branch", "main"), None);
        assert_eq!(context.get("builder", "branch"), Some("main"));
        assert_eq!(context.put("builder", "branch", "dev"), Some("main".into()));
        assert_eq!(context.get("builder", "branch"), Some("dev"));
    }

    #[test]
    fn namespaces_do_not_collide_on_common_key_names() {
        // Two agents will otherwise collide on names as generic as `status`, and a
        // silent overwrite between agents is far harder to diagnose than a missing key.
        let mut context = SharedContext::new();
        context.put("builder", "status", "running");
        context.put("tester", "status", "idle");

        assert_eq!(context.get("builder", "status"), Some("running"));
        assert_eq!(context.get("tester", "status"), Some("idle"));
    }

    #[test]
    fn a_separator_inside_a_key_cannot_forge_another_namespace() {
        // The reason the separator is a unit separator rather than `:` or `/`: a file
        // path as a key would otherwise let these two collide.
        let mut context = SharedContext::new();
        context.put("a", "b/c", "first");
        context.put("a/b", "c", "second");

        assert_eq!(context.get("a", "b/c"), Some("first"));
        assert_eq!(context.get("a/b", "c"), Some("second"));
    }

    #[test]
    fn listing_a_namespace_returns_only_its_own_keys() {
        let mut context = SharedContext::new();
        context.put("builder", "branch", "main");
        context.put("builder", "target", "release");
        context.put("tester", "branch", "dev");

        let listed = context.namespace("builder");
        assert_eq!(listed, vec![("branch", "main"), ("target", "release")]);
    }

    #[test]
    fn a_namespace_that_is_a_prefix_of_another_does_not_absorb_it() {
        // `build` must not list `builder`'s keys.
        let mut context = SharedContext::new();
        context.put("build", "a", "1");
        context.put("builder", "b", "2");

        assert_eq!(context.namespace("build"), vec![("a", "1")]);
    }

    #[test]
    fn removing_a_key_returns_what_was_there() {
        let mut context = SharedContext::new();
        context.put("a", "k", "v");
        assert_eq!(context.remove("a", "k"), Some("v".into()));
        assert_eq!(context.remove("a", "k"), None);
        assert!(context.is_empty());
    }
}
