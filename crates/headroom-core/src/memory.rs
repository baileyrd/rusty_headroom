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

use crate::block::BlockKind;
use crate::ccr::ContentHash;
use crate::conversation::{Conversation, Role};
use crate::relevance::{Bm25Scorer, RelevanceScore, RelevanceScorer};

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

    /// The `limit` facts most relevant to `query`, most first.
    ///
    /// Falls back to [`MemoryStore::recall`] when there is no query, or when no fact
    /// shares a term with it. Both are the same situation from the store's side: the
    /// query carries no signal about which facts matter, so the corroboration ranking
    /// is the best available answer rather than a degraded one.
    ///
    /// # Why keyword scoring and not embeddings
    ///
    /// Scoring must be deterministic and in-process. Compression that varied with a
    /// model's mood, or that waited on a network call, would break I4 twice over — and
    /// the request path is the one place neither is negotiable. [`Bm25Scorer`] is both.
    ///
    /// # Ranking
    ///
    /// Score, then distinct sources, then occurrences, then content hash. Facts that
    /// score zero are ranked last rather than dropped: `limit` is a budget, and leaving
    /// it unspent to enforce a keyword match would hand back less context than the same
    /// store returns today for the same request.
    ///
    /// # Example
    ///
    /// ```
    /// use headroom_core::memory::{MemoryStore, Provenance};
    ///
    /// let mut store = MemoryStore::new();
    /// store.remember("The auth module uses tokens, not sessions.", Provenance::new("a", "s"));
    /// store.remember("Staging mirrors prod except for the cache tier.", Provenance::new("a", "s"));
    ///
    /// let hits = store.recall_for_query(Some("how does auth work"), 1);
    /// assert!(hits[0].content().contains("auth module"));
    /// ```
    pub fn recall_for_query(&self, query: Option<&str>, limit: usize) -> Vec<&Memory> {
        let Some(query) = query.map(str::trim).filter(|query| !query.is_empty()) else {
            return self.recall(limit);
        };

        let entries: Vec<(&String, &Memory)> = self.entries.iter().collect();
        let contents: Vec<String> = entries
            .iter()
            .map(|(_, memory)| memory.content.clone())
            .collect();
        let scores = Bm25Scorer::new().score_all(query, &contents);

        if scores.iter().all(|score| score.value() <= 0.0) {
            return self.recall(limit);
        }

        let mut ranked: Vec<((&String, &Memory), RelevanceScore)> =
            entries.into_iter().zip(scores).collect();
        ranked.sort_by(|((a_key, a), a_score), ((b_key, b), b_score)| {
            b_score
                .value()
                .total_cmp(&a_score.value())
                .then(b.sources.len().cmp(&a.sources.len()))
                .then(b.occurrences.cmp(&a.occurrences))
                .then(a_key.cmp(b_key))
        });
        ranked
            .into_iter()
            .take(limit)
            .map(|((_, memory), _)| memory)
            .collect()
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
    inject_block_for_query(store, limit, None)
}

/// Renders the memories most relevant to `query` as an injectable block.
///
/// [`inject_block`] with a query. Selection is [`MemoryStore::recall_for_query`]; the
/// rendering, the corroboration marker, and the `None`-when-empty contract are the same.
///
/// # Why the query only reaches selection
///
/// The rendered block does not mention the query, and must not. It is appended to a
/// message the user just wrote; a block that restated their question back at them would
/// read as an instruction rather than as context, which is the line D19 draws.
pub fn inject_block_for_query(
    store: &MemoryStore,
    limit: usize,
    query: Option<&str>,
) -> Option<String> {
    let recalled = store.recall_for_query(query, limit);
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

impl MemoryStore {
    /// Builds a store from JSON-lines, skipping anything unreadable.
    ///
    /// One object per line: `{"content": "...", "agent": "...", "context": "..."}`.
    /// `agent` and `context` default to `"unknown"` — provenance the file did not record
    /// is missing, and inventing a plausible source would be worse than admitting it,
    /// since provenance is what a reader uses to decide whether to act on a memory.
    ///
    /// # Reading another tool's export
    ///
    /// `agent` falls back to `source` and `context` to `category` before defaulting.
    /// Those are the column names a `remind_me` export carries, and it is the export
    /// format most likely to be pointed at `HEADROOM_MEMORY` — its records are
    /// `{"role": "assistant", ...every column of the memories table}`, which already
    /// satisfies the `content` requirement above. Without the fallback every fact from
    /// such a file arrives as `unknown`/`unknown`, which is the one provenance value
    /// that tells a reader nothing, on the import path most likely to be used.
    ///
    /// Records from that export that are *not* memories — its entity graph — carry no
    /// `content` and are skipped by the rule above. That is the right outcome, but it
    /// costs one warning per skipped line, so exports meant for this reader are better
    /// taken with the graph excluded.
    ///
    /// # Two kinds of record are refused rather than skipped for lacking a field
    ///
    /// `"sensitive": true` marks a memory the owner flagged as *do not surface by
    /// default*. `remind_me`'s own search honors it; its exporter does not filter on it,
    /// so a routine export carries those rows. Injection sends them to a third-party
    /// model, which is precisely the disclosure the flag exists to prevent — and unlike a
    /// search result, nothing here is shown to the person who set it before it leaves.
    ///
    /// A non-null `superseded_by` marks a fact that has already been replaced. Its
    /// exporter excludes those by default but includes them for a full backup, and a
    /// backup is exactly the file somebody points at this reader. Injecting one hands the
    /// model a fact its own source knows to be stale, with the corroboration marker
    /// vouching for it.
    ///
    /// # Why lossy, and why line-oriented
    ///
    /// A malformed line is skipped with a warning rather than failing the load. This file
    /// is an input to an optimization; refusing to start without a perfect one would turn
    /// a convenience into a hard startup dependency. Line-oriented so an agent can append
    /// to it without rewriting — and so one bad append costs one memory rather than all
    /// of them.
    pub fn from_jsonl_lossy(source: &str) -> Self {
        let mut store = Self::new();

        for (number, line) in source.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                tracing::warn!(line = number + 1, "memory line is not JSON; skipped");
                continue;
            };
            let Some(content) = value.get("content").and_then(|v| v.as_str()) else {
                tracing::warn!(line = number + 1, "memory line has no content; skipped");
                continue;
            };

            // Logged at debug rather than warn: these are well-formed records this reader
            // is declining on purpose, and a full export can carry many of them. A warning
            // per line would train an operator to ignore the warnings that mean something.
            if value.get("sensitive").and_then(|v| v.as_bool()) == Some(true) {
                tracing::debug!(line = number + 1, "memory is marked sensitive; not loaded");
                continue;
            }
            if value.get("superseded_by").is_some_and(|v| !v.is_null()) {
                tracing::debug!(line = number + 1, "memory was superseded; not loaded");
                continue;
            }

            // First name that is present and a string wins, so a file written for this
            // reader keeps its own vocabulary and an export written for something else
            // still lands with usable provenance.
            let field = |names: &[&str]| {
                names
                    .iter()
                    .find_map(|name| value.get(*name).and_then(|v| v.as_str()))
                    .unwrap_or("unknown")
                    .to_owned()
            };
            store.remember(
                content,
                Provenance::new(field(&["agent", "source"]), field(&["context", "category"])),
            );
        }

        store
    }
}

/// Appends the memory block to the newest user message's last text block.
///
/// Returns the index of the message and block that changed, so a caller rewriting raw
/// JSON knows exactly what to touch — the same contract as
/// [`output_shaping::verbosity_append`], and for the same reason: this function must not
/// need to know how the request was serialized.
///
/// Returns `None` when nothing should change: an empty store, no user message, no text
/// block to append to, or a block that already carries the memories.
///
/// # Why the newest user message and nothing else
///
/// The block *must* land in the live zone. The system prompt is the hot cache zone and
/// is never modified (**I2**); an earlier message is part of the frozen prefix, and
/// editing one would rewrite bytes a provider has already cached (**I3**). The newest
/// user message is the one part of the request that was never sent before, so appending
/// there costs nothing that was already paid for.
///
/// # Why re-injection is guarded against
///
/// An agent loop calls this every turn. Without the check a long session accumulates the
/// same facts a dozen times over — wasted tokens, and a genuinely worse prompt, since
/// repetition reads as emphasis.
///
/// [`output_shaping::verbosity_append`]: crate::output_shaping::verbosity_append
pub fn inject_append(
    conversation: &Conversation,
    store: &MemoryStore,
    limit: usize,
    frozen: usize,
    query: Option<&str>,
) -> Option<(usize, usize, String)> {
    let block_text = inject_block_for_query(store, limit, query)?;

    let (message_index, message) = conversation
        .messages()
        .iter()
        .enumerate()
        .rev()
        .find(|(_, message)| message.role() == Role::User)?;

    // Invariant I2. `frozen` is the count of messages the provider may already have
    // cached, so anything below it is off limits — appending a memory there rewrites a
    // block the customer paid to cache and invalidates the prefix on the turn it lands.
    //
    // This was protected only by accident. `compress_dialect` returns early when the live
    // zone is empty, and that early return sits above the injection site; its comment says
    // it is there because there is nothing to compress, which is a different reason that
    // happens to cover this one. Moving it, or injecting on a request with no live zone —
    // both reasonable changes, since injection does not need anything to compress — would
    // have removed the protection with nothing to notice.
    //
    // I could not construct a request that reaches this with a frozen target: the newest
    // user message is at or after the floor whenever anything else is compressible. That
    // is an argument, not a guarantee, and it is the kind of argument this project has
    // been wrong about before.
    if message_index < frozen {
        return None;
    }

    let (block_index, block) = message
        .blocks()
        .iter()
        .enumerate()
        .rev()
        .find(|(_, block)| block.kind() == BlockKind::Text)?;

    // Matched on the opening tag rather than the whole block: the memory set can grow
    // between turns, and comparing the full text would re-inject a superset alongside
    // the subset already there.
    if block.content().contains("<memory>") {
        return None;
    }

    Some((
        message_index,
        block_index,
        format!("{}{block_text}", block.content()),
    ))
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
    use crate::Block;

    fn source(agent: &str) -> Provenance {
        Provenance::new(agent, "session-1")
    }

    /// A store with one fact, which is all injection needs to produce a block.
    fn one_memory() -> MemoryStore {
        let mut store = MemoryStore::new();
        store.remember("the user prefers concise answers", source("a"));
        store
    }

    /// `messages` user turns, each carrying one text block.
    fn conversation_of(turns: usize) -> Conversation {
        Conversation::new(
            None,
            vec![],
            (0..turns)
                .map(|i| {
                    crate::conversation::Message::new(
                        Role::User,
                        vec![Block::new(BlockKind::Text, format!("turn {i}"))],
                    )
                })
                .collect(),
        )
    }

    // ---- injection and the frozen floor ----

    #[test]
    fn injection_never_targets_a_frozen_message() {
        // Invariant I2. Appending a memory to a message the provider has cached rewrites
        // a block the customer paid for and invalidates the prefix on the turn it lands.
        //
        // This was protected only by an early return in `compress_dialect` — the one that
        // gives up when the live zone is empty, whose comment says it is there because
        // there is nothing to compress. A different reason that happened to cover this
        // one, sitting in a different crate.
        let store = one_memory();
        let conversation = conversation_of(3);

        // Every floor that covers the newest user message, which is the only message
        // injection ever targets.
        for frozen in [3, 4, usize::MAX] {
            assert!(
                inject_append(&conversation, &store, 8, frozen, None).is_none(),
                "injected into a message below a floor of {frozen}"
            );
        }
    }

    #[test]
    fn injection_still_lands_on_a_live_message() {
        // The other half. A guard that refuses everything would disable the feature and
        // look exactly like one that works.
        let store = one_memory();
        let conversation = conversation_of(3);

        let (message, block, content) =
            inject_append(&conversation, &store, 8, 2, None).expect("nothing was injected");

        assert_eq!(message, 2, "injected into a message behind the floor");
        assert_eq!(block, 0);
        assert!(content.starts_with("turn 2"), "the original text was lost");
        assert!(content.contains("<memory>"), "no memory block was appended");
    }

    #[test]
    fn a_floor_of_zero_leaves_injection_alone() {
        // No breakpoints means nothing is cached, which is the ordinary case and must be
        // unaffected by the guard.
        let store = one_memory();
        let conversation = conversation_of(1);

        assert!(inject_append(&conversation, &store, 8, 0, None).is_some());
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

    // ---- reading an export written for something else ----

    /// The shape `remind_me_mcp.exporter` emits for JSONL: `{"role": "assistant",
    /// ...every column of the memories table}` for memories, then `record_type`-tagged
    /// graph records.
    const REMIND_ME_EXPORT: &str = concat!(
        r#"{"role":"assistant","id":"01HX","content":"The deploy key rotates every 90 days.","#,
        r#""category":"ops","tags":["infra"],"source":"chat","created_at":"2026-01-02T03:04:05Z"}"#,
        "\n",
        r#"{"record_type":"entity","id":"e1","name":"deploy key","kind":"thing"}"#,
        "\n",
        r#"{"record_type":"memory_entity","memory_id":"01HX","entity_id":"e1"}"#,
    );

    #[test]
    fn a_remind_me_export_lands_with_usable_provenance() {
        let store = MemoryStore::from_jsonl_lossy(REMIND_ME_EXPORT);

        assert_eq!(
            store.len(),
            1,
            "graph records carry no content and are skipped"
        );
        let memory = store.all().next().expect("one memory");
        assert_eq!(memory.content(), "The deploy key rotates every 90 days.");
        // `source` and `category` stand in for `agent` and `context`. Without this the
        // whole file arrives as unknown/unknown, which is the one provenance value that
        // tells a reader nothing.
        assert_eq!(memory.sources()[0], Provenance::new("chat", "ops"));
    }

    #[test]
    fn a_sensitive_memory_is_never_loaded() {
        // The flag means "do not surface by default", and the exporter does not filter on
        // it. Injection sends it to a third-party model with nobody shown it first.
        let line = r#"{"content":"the recovery phrase is ...","source":"chat","sensitive":true}"#;
        assert!(MemoryStore::from_jsonl_lossy(line).is_empty());

        let ordinary = r#"{"content":"c","source":"chat","sensitive":false}"#;
        assert_eq!(MemoryStore::from_jsonl_lossy(ordinary).len(), 1);
    }

    #[test]
    fn a_superseded_memory_is_never_loaded() {
        // A full backup carries these. Injecting one hands the model a fact its own
        // source knows to be stale, with the corroboration marker vouching for it.
        let stale = r#"{"content":"the API lives at v1","source":"chat","superseded_by":"01HZ"}"#;
        assert!(MemoryStore::from_jsonl_lossy(stale).is_empty());

        // A null `superseded_by` is the live case, and every row carries the column.
        let live = r#"{"content":"c","source":"chat","superseded_by":null}"#;
        assert_eq!(MemoryStore::from_jsonl_lossy(live).len(), 1);
    }

    #[test]
    fn a_file_written_for_this_reader_keeps_its_own_vocabulary() {
        // The fallback must not outrank the native names when both are present.
        let line =
            r#"{"content":"c","agent":"reader","context":"s1","source":"chat","category":"ops"}"#;
        let store = MemoryStore::from_jsonl_lossy(line);

        let memory = store.all().next().expect("one memory");
        assert_eq!(memory.sources()[0], Provenance::new("reader", "s1"));
    }

    // ---- query-aware recall ----

    /// Two well-corroborated decoys and one singly-sourced fact about hashing.
    fn ranked_store() -> MemoryStore {
        let mut store = MemoryStore::new();
        for agent in ["reader", "planner"] {
            store.remember(
                "Tests live under crates/*/tests.",
                Provenance::new(agent, "s1"),
            );
            store.remember("Releases are cut on Fridays.", Provenance::new(agent, "s1"));
        }
        store.remember(
            "Content is normalized before hashing.",
            Provenance::new("reader", "s1"),
        );
        store
    }

    #[test]
    fn a_query_outranks_corroboration() {
        let store = ranked_store();
        // The decoys have two sources each, so `recall` prefers one of them outright —
        // which is what makes this assertion about the query rather than about luck.
        assert!(!store.recall(1)[0].content().contains("hashing"));

        let hits = store.recall_for_query(Some("how is content hashed"), 1);
        assert_eq!(hits[0].content(), "Content is normalized before hashing.");
    }

    #[test]
    fn a_query_with_no_signal_recalls_exactly_what_it_would_without_one() {
        // The invariant that makes this safe to turn on: a request that carries no usable
        // query gets the same block it got before selection existed.
        let store = ranked_store();
        let baseline: Vec<&str> = store.recall(3).iter().map(|m| m.content()).collect();

        for query in [None, Some(""), Some("   "), Some("zzz qqq")] {
            let hits: Vec<&str> = store
                .recall_for_query(query, 3)
                .iter()
                .map(|m| m.content())
                .collect();
            assert_eq!(
                hits, baseline,
                "query {query:?} changed the queryless answer"
            );
        }
    }

    #[test]
    fn a_partial_match_still_spends_the_whole_budget() {
        // Zero-scoring facts rank last rather than dropping out. Enforcing a keyword
        // match would hand back less context than the same store returns today.
        let store = ranked_store();
        let hits = store.recall_for_query(Some("how is content hashed"), 3);

        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].content(), "Content is normalized before hashing.");
    }

    #[test]
    fn query_ranking_is_stable_across_stores_built_in_different_orders() {
        // Invariant I4. Two stores holding the same facts must inject the same bytes, or
        // the injection busts the cache it was designed to protect.
        let forward = ranked_store();

        let mut reverse = MemoryStore::new();
        reverse.remember(
            "Content is normalized before hashing.",
            Provenance::new("reader", "s1"),
        );
        for agent in ["planner", "reader"] {
            reverse.remember("Releases are cut on Fridays.", Provenance::new(agent, "s1"));
            reverse.remember(
                "Tests live under crates/*/tests.",
                Provenance::new(agent, "s1"),
            );
        }

        let render = |store: &MemoryStore| {
            inject_block_for_query(store, 8, Some("how is content hashed")).expect("a block")
        };
        assert_eq!(render(&forward), render(&reverse));
    }
}
