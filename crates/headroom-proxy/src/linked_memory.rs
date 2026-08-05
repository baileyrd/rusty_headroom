//! A linked, in-process query against a `rusty_remind_me` memory store (gap #215).
//!
//! See DECISIONS D44 for the full reasoning. The short version: `remind_me_core`'s own
//! `search_memories` is not safe to call from a compression path as-is — its vitality
//! filter reads the wall clock, and its embedder tier is an optional, probed daemon
//! whose absence silently changes the ranking. Both are invariant I4 hazards (the same
//! request must compress to the same bytes on every run). This module closes both:
//!
//! - Every call passes `include_dormant: true, min_vitality: 0.0`, so the vitality
//!   predicate is never emitted and the wall clock is never read.
//! - The embedder is [`headroom_embed::LocalEmbedder`] — a pinned local model file,
//!   loaded once — rather than the daemon-probed one `remind_me_core::embedder` offers.
//!   If it will not load, or its vectors do not match what is already stored, the
//!   semantic tier is off **for the process**, never per request; the proxy falls back
//!   to `remind_me_core`'s own FTS5 keyword search.
//! - `record_accesses` is never called. Reads stay pure.
//!
//! The database connection and the embedder are both resolved once, at startup, by
//! [`LinkedMemory::resolve`] — matching how `HEADROOM_MEMORY`'s static store is loaded
//! once by [`crate::config::Config::memories`], for the same reason: a data source that
//! could change between requests would make the same request compress differently
//! depending on when it arrived.
//!
//! # Without the `linked-memory` feature
//!
//! [`LinkedMemory`] still exists, as an uninhabited type — [`LinkedMemory::resolve`]
//! always returns `None`, the same "off unless configured, and now impossible to turn
//! on by accident" shape [`headroom_embed::LocalEmbedder`] uses for its own `local`
//! feature. A caller's code is identical either way.

#[cfg(not(feature = "linked-memory"))]
pub use disabled::LinkedMemory;
#[cfg(feature = "linked-memory")]
pub use enabled::LinkedMemory;

#[cfg(feature = "linked-memory")]
mod enabled {
    use std::sync::Mutex;

    use remind_me_core::embedder::{EmbedError, EmbedRole, Embedder, EmbeddingIdentity};
    use rusqlite::{Connection, OpenFlags};

    /// Adapts [`headroom_embed::LocalEmbedder`] to `remind_me_core`'s [`Embedder`] trait.
    ///
    /// Lives here, not in `headroom-embed` — that crate deliberately does not depend on
    /// `remind_me_core` (it carries SQLite and tokio; an inference crate needs neither),
    /// so the adapter belongs with the code that already has the dependency.
    struct EmbedderAdapter(headroom_embed::LocalEmbedder);

    impl Embedder for EmbedderAdapter {
        fn embed(&self, texts: &[String], role: EmbedRole) -> Result<Vec<Vec<f32>>, EmbedError> {
            let role = match role {
                EmbedRole::Query => headroom_embed::EmbedRole::Query,
                EmbedRole::Passage => headroom_embed::EmbedRole::Passage,
            };
            self.0
                .embed(texts, role)
                .map_err(|err| EmbedError(err.to_string()))
        }

        fn dim(&self) -> usize {
            self.0.dim()
        }

        fn identity(&self) -> EmbeddingIdentity {
            let identity = self.0.identity();
            EmbeddingIdentity {
                backend: identity.backend,
                model: identity.model,
                dim: identity.dim,
            }
        }
    }

    /// Accepts `embedder` for the semantic tier only if its vectors would be comparable
    /// to whatever is already stored — `None` (keyword-only) on a mismatch or on a
    /// failure to check, logged either way.
    ///
    /// Split out from [`LinkedMemory::resolve_embedder`] so this policy is testable
    /// against any [`Embedder`], not only [`headroom_embed::LocalEmbedder`] — which
    /// needs a real model file this environment does not have. See DECISIONS D44.
    fn accept_if_compatible(
        conn: &Connection,
        embedder: Box<dyn Embedder + Send + Sync>,
    ) -> Option<Box<dyn Embedder + Send + Sync>> {
        // The vectors already stored in this database may have been written by a
        // different embedder (`rusty_remind_me`'s own Ollama backend, most likely).
        // Comparing our query vector against them would not be "worse retrieval" — it
        // would be a number with no meaning, per `EmbeddingIdentity`'s own doc. Detected,
        // not corrected: correcting it means writing (clearing the stale vectors), and
        // this connection is read-only by design.
        match remind_me_core::vectors::embedding_mismatch_info(conn, &embedder.identity()) {
            Ok(Some(mismatch)) => {
                tracing::warn!(
                    stored_backend = %mismatch.stored.backend,
                    stored_model = %mismatch.stored.model,
                    current_backend = %mismatch.current.backend,
                    current_model = %mismatch.current.model,
                    "the linked memory database's stored vectors were computed by a \
                     different embedder; falling back to keyword-only for this process"
                );
                None
            }
            Ok(None) => Some(embedder),
            Err(err) => {
                tracing::warn!(
                    %err,
                    "could not check the linked memory database's embedding identity; \
                     falling back to keyword-only for this process"
                );
                None
            }
        }
    }

    /// A linked memory backend: a read-only connection to a `rusty_remind_me` SQLite
    /// store, plus an optional pinned embedder for the semantic tier.
    pub struct LinkedMemory {
        conn: Mutex<Connection>,
        embedder: Option<Box<dyn Embedder + Send + Sync>>,
    }

    impl LinkedMemory {
        /// Builds an already-resolved instance directly, for tests elsewhere in this
        /// crate that need to exercise the `Compressors` call site without going through
        /// environment variables or a real model file — see `compression.rs`'s own
        /// linked-memory tests.
        #[cfg(test)]
        pub(crate) fn for_test(
            conn: Connection,
            embedder: Option<Box<dyn Embedder + Send + Sync>>,
        ) -> Self {
            Self {
                conn: Mutex::new(conn),
                embedder,
            }
        }

        /// Resolves the linked backend from `HEADROOM_LINKED_MEMORY_*`, once.
        ///
        /// `None` when `HEADROOM_LINKED_MEMORY_DB` is unset (no linked backend
        /// configured) or when the database cannot be opened read-only — in both cases
        /// the proxy behaves as if this feature did not exist, falling back to whatever
        /// `HEADROOM_MEMORY` provides.
        ///
        /// A configured but unloadable *embedder* is different: it does not fail this
        /// resolution. The keyword tier over the linked database still works, so the
        /// process starts with the semantic tier off and a warning logged, rather than
        /// losing linked memory entirely over the smaller of its two capabilities.
        pub fn resolve() -> Option<Self> {
            let db_path = crate::config::linked_memory_db_path()?;

            let conn = match Connection::open_with_flags(
                &db_path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            ) {
                Ok(conn) => conn,
                Err(err) => {
                    tracing::warn!(
                        path = %db_path.display(),
                        %err,
                        "could not open the linked memory database read-only; linked memory is off"
                    );
                    return None;
                }
            };

            let embedder = Self::resolve_embedder(&conn);

            Some(Self {
                conn: Mutex::new(conn),
                embedder,
            })
        }

        /// Loads the pinned local embedder, if both paths are set — `None` (keyword-only)
        /// on any failure, logged rather than propagated. Never retried per request; see
        /// the module doc for why.
        fn resolve_embedder(conn: &Connection) -> Option<Box<dyn Embedder + Send + Sync>> {
            let model_path = crate::config::linked_memory_model_path();
            let tokenizer_path = crate::config::linked_memory_tokenizer_path();
            let (model_path, tokenizer_path) = match (model_path, tokenizer_path) {
                (Some(model), Some(tokenizer)) => (model, tokenizer),
                _ => return None,
            };

            let local = match headroom_embed::LocalEmbedder::load(&model_path, &tokenizer_path) {
                Ok(local) => local,
                Err(err) => {
                    tracing::warn!(
                        %err,
                        "the linked memory embedder could not load; falling back to keyword-only \
                         for this process"
                    );
                    return None;
                }
            };
            accept_if_compatible(conn, Box::new(EmbedderAdapter(local)))
        }

        /// Searches the linked store, returning content strings ranked best-first.
        ///
        /// `include_dormant: true` and `min_vitality: 0.0` on every call — see the module
        /// doc for why. `record_accesses` is never called; this is a read.
        ///
        /// Any failure (a malformed query, a locked database) degrades to no results
        /// rather than propagating — the same "a search must not fail the request it
        /// runs inside" contract the static `HEADROOM_MEMORY` path already has.
        pub fn search(&self, query: &str, limit: usize) -> Vec<String> {
            let input = remind_me_core::MemorySearchInput {
                strategy: Default::default(),
                include_sensitive: false,
                query: query.to_string(),
                category: None,
                tags: None,
                limit,
                token_budget: usize::MAX,
                response_format: Default::default(),
                include_dormant: true,
                min_vitality: 0.0,
                verbose: false,
                expand_entities: false,
                include_neighbors: false,
                expand_co_retrieval: false,
            };

            let embedder = self
                .embedder
                .as_deref()
                .map(|embedder| embedder as &dyn Embedder);
            let conn = match self.conn.lock() {
                Ok(conn) => conn,
                Err(poisoned) => poisoned.into_inner(),
            };

            match remind_me_core::db::queries::search_memories_with_embedder(
                &conn, &input, embedder,
            ) {
                Ok(results) => results
                    .into_iter()
                    .take(limit)
                    .map(|result| result.memory.content)
                    .collect(),
                Err(err) => {
                    tracing::warn!(%err, "linked memory search failed; injecting nothing");
                    Vec::new()
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use remind_me_core::db::queries;
        use remind_me_core::vectors::embed_and_store;
        use remind_me_core::{Database, MemoryAddInput};

        /// A double with no daemon, no network, and no model file — the same shape
        /// `rusty_remind_me`'s own `injectable_embedder_test.rs` uses for exactly this
        /// reason: this environment has no `.rten` model to load, so exercising the
        /// wiring at all requires a deterministic stand-in. A bag-of-characters
        /// histogram, so the same text always embeds to the same point.
        struct CharHistogramEmbedder {
            identity: EmbeddingIdentity,
        }

        impl CharHistogramEmbedder {
            fn named(model: &str) -> Self {
                Self {
                    identity: EmbeddingIdentity {
                        backend: "char-histogram".into(),
                        model: model.into(),
                        dim: 26,
                    },
                }
            }
        }

        impl Embedder for CharHistogramEmbedder {
            fn embed(
                &self,
                texts: &[String],
                _role: EmbedRole,
            ) -> Result<Vec<Vec<f32>>, EmbedError> {
                Ok(texts
                    .iter()
                    .map(|text| {
                        let mut counts = vec![0f32; 26];
                        for ch in text.to_ascii_lowercase().chars() {
                            if ch.is_ascii_lowercase() {
                                counts[(ch as u8 - b'a') as usize] += 1.0;
                            }
                        }
                        let norm = counts.iter().map(|c| c * c).sum::<f32>().sqrt();
                        if norm > 0.0 {
                            for count in &mut counts {
                                *count /= norm;
                            }
                        }
                        counts
                    })
                    .collect())
            }

            fn dim(&self) -> usize {
                26
            }

            fn identity(&self) -> EmbeddingIdentity {
                self.identity.clone()
            }
        }

        /// A path under the OS temp directory, unique enough for one test process.
        fn temp_db_path(name: &str) -> std::path::PathBuf {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "headroom-linked-memory-test-{name}-{}.sqlite3",
                std::process::id()
            ));
            path
        }

        fn add(conn: &rusqlite::Connection, content: &str) -> String {
            queries::add_memory(
                conn,
                MemoryAddInput {
                    sensitive: false,
                    content: content.to_string(),
                    category: "general".into(),
                    tags: vec![],
                    source: "manual".into(),
                    metadata: serde_json::json!({}),
                    subject: None,
                    predicate: None,
                    object: None,
                    entities: vec![],
                },
            )
            .unwrap()
            .id
        }

        fn base_input(query: &str, limit: usize) -> remind_me_core::MemorySearchInput {
            remind_me_core::MemorySearchInput {
                strategy: Default::default(),
                include_sensitive: false,
                query: query.to_string(),
                category: None,
                tags: None,
                limit,
                token_budget: usize::MAX,
                response_format: Default::default(),
                include_dormant: false,
                min_vitality: 0.0,
                verbose: false,
                expand_entities: false,
                include_neighbors: false,
                expand_co_retrieval: false,
            }
        }

        /// Writes a fresh store at a temp path, seeded via `setup`, then reopens it
        /// read-only — the same open the production code performs — for the test.
        fn seeded(name: &str, setup: impl FnOnce(&rusqlite::Connection)) -> LinkedMemory {
            let path = temp_db_path(name);
            let _ = std::fs::remove_file(&path);
            {
                let db = Database::open(&path).unwrap();
                setup(&db.conn());
            }
            let conn =
                Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
            LinkedMemory {
                conn: Mutex::new(conn),
                embedder: None,
            }
        }

        #[test]
        fn a_supplied_embedder_is_actually_consulted() {
            // The query shares no term with any memory, so the keyword half of
            // `search_memories_with_embedder` returns nothing and cannot account for the
            // result. If this passed with the embedder silently unused — a wiring bug
            // that would make the whole "linked, in-process semantic retrieval" feature a
            // no-op — it would pass for the wrong reason. It must fail without the fix.
            let query = "ydolep yek rotaets";
            let mut linked = seeded("consulted", |conn| {
                let embedder = CharHistogramEmbedder::named("test");
                for content in [
                    "the deploy key rotates every ninety days",
                    "staging mirrors production except the cache tier",
                ] {
                    let id = add(conn, content);
                    embed_and_store(conn, &embedder, &id, content).unwrap();
                }

                // Sanity-checked here, on the writable setup connection: it is the one
                // with `effective_vitality` registered (via `schema::initialize_schema`),
                // which the query below needs since `include_dormant: false` puts the
                // predicate in the WHERE clause. `LinkedMemory`'s own read-only connection
                // never registers it — deliberately, since `search()` never emits that
                // predicate — so this check cannot run against it.
                let without =
                    queries::search_memories_with_embedder(conn, &base_input(query, 20), None)
                        .unwrap();
                assert!(
                    without.is_empty(),
                    "the keyword half matched, so this query cannot isolate the embedder"
                );
            });

            linked.embedder = Some(Box::new(CharHistogramEmbedder::named("test")));
            let results = linked.search(query, 20);
            assert!(
                !results.is_empty(),
                "the supplied embedder was not consulted through LinkedMemory::search"
            );
        }

        #[test]
        fn a_mismatched_embedder_identity_falls_back_to_keyword_only() {
            // The corpus was indexed by one embedder; a different one is offered at
            // startup — e.g. the model file changed underneath an unchanged database.
            // Comparing across the two would be a number with no meaning, so this must
            // reject the offered embedder rather than silently mixing vector spaces.
            let path = temp_db_path("mismatched");
            let _ = std::fs::remove_file(&path);
            {
                let db = Database::open(&path).unwrap();
                let conn = db.conn();
                let indexer = CharHistogramEmbedder::named("indexer-v1");
                let id = add(&conn, "some content");
                embed_and_store(&conn, &indexer, &id, "some content").unwrap();
            }
            let conn =
                Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();

            let offered: Box<dyn Embedder + Send + Sync> =
                Box::new(CharHistogramEmbedder::named("indexer-v2"));
            assert!(
                accept_if_compatible(&conn, offered).is_none(),
                "a mismatched embedder identity must be rejected, not silently accepted"
            );

            let matching: Box<dyn Embedder + Send + Sync> =
                Box::new(CharHistogramEmbedder::named("indexer-v1"));
            assert!(
                accept_if_compatible(&conn, matching).is_some(),
                "a matching embedder identity must be accepted"
            );
        }

        #[test]
        fn dormant_memories_still_surface() {
            // I4 hazard #1 from #215's own comment thread: `search_memories`'s default
            // (`include_dormant: false`) puts `effective_vitality(...) >= 0.05` in the
            // WHERE clause, which reads the wall clock. `LinkedMemory::search` must pass
            // `include_dormant: true, min_vitality: 0.0` so that predicate is never
            // emitted at all — proven here by manufacturing a memory whose vitality has
            // decayed far below the floor and confirming the *default* excludes it while
            // `LinkedMemory::search` still returns it.
            let linked = seeded("dormant", |conn| {
                let id = add(conn, "a fact nobody has touched in decades");
                conn.execute(
                    "UPDATE memories SET accessed_at = '2000-01-01T00:00:00+00:00' WHERE id = ?",
                    rusqlite::params![id],
                )
                .unwrap();

                // As above: checked on the writable setup connection, the one with
                // `effective_vitality` registered.
                let default_search = queries::search_memories_with_embedder(
                    conn,
                    &base_input("nobody touched", 20),
                    None,
                )
                .unwrap();
                assert!(
                    default_search.is_empty(),
                    "the manufactured memory was not actually dormant, so this test proves nothing"
                );
            });

            let results = linked.search("nobody touched", 20);
            assert!(
                !results.is_empty(),
                "a dormant memory was dropped: LinkedMemory::search is not passing \
                 include_dormant: true, min_vitality: 0.0"
            );
        }
    }
}

#[cfg(not(feature = "linked-memory"))]
mod disabled {
    /// The `linked-memory`-feature-off stand-in. Uninhabited: there is no such thing as
    /// a degraded linked backend, so no value of this type can exist, and a caller's
    /// code is identical whether or not the feature is on.
    #[derive(Debug)]
    pub enum LinkedMemory {}

    impl LinkedMemory {
        /// Always `None` — no linked backend without the `linked-memory` feature.
        pub fn resolve() -> Option<Self> {
            None
        }

        /// Unreachable — no value of this type exists.
        pub fn search(&self, _query: &str, _limit: usize) -> Vec<String> {
            match *self {}
        }
    }
}
