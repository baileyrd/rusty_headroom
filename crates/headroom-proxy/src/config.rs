//! Proxy configuration.
//!
//! # Why this is read live rather than cached
//!
//! Every accessor reads the environment at call time. That looks wasteful and is
//! deliberate: an operator can change the proxy's behavior — turn compression off
//! during an incident, point at a different upstream — without a restart, and
//! without dropping the in-flight streaming responses a restart would truncate.
//!
//! The reads are cheap relative to a network round trip to a model provider, which
//! is what every request costs anyway.

use std::collections::BTreeMap;
use std::env;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::RwLock;

use headroom_core::output_shaping::Verbosity;

/// Environment variable names, in one place so they can be documented and tested
/// rather than scattered as string literals.
pub mod vars {
    /// Address the proxy listens on.
    pub const HOST: &str = "HEADROOM_HOST";
    /// Port the proxy listens on.
    pub const PORT: &str = "HEADROOM_PORT";
    /// Base URL requests are forwarded to.
    pub const UPSTREAM: &str = "HEADROOM_UPSTREAM";
    /// Set to `0` to forward everything untouched.
    pub const COMPRESSION: &str = "HEADROOM_COMPRESSION";
    /// Output verbosity steering: `terse`, `full`, or unset for neither.
    pub const OUTPUT_SHAPER: &str = "HEADROOM_OUTPUT_SHAPER";
    /// Path to a `recommendations.json` produced by `headroom learn`.
    pub const RECOMMENDATIONS: &str = "HEADROOM_RECOMMENDATIONS";
    /// Path to a JSON-lines file of memories to inject into the live-zone tail.
    pub const MEMORY: &str = "HEADROOM_MEMORY";
    /// How many memories one injection may carry.
    pub const MEMORY_LIMIT: &str = "HEADROOM_MEMORY_LIMIT";
    /// Set to `1` to normalize tools and place `cache_control` breakpoints.
    pub const STABILIZE: &str = "HEADROOM_STABILIZE";
    /// Directory for a file-backed CCR store. Unset means memory only.
    pub const CCR_DIR: &str = "HEADROOM_CCR_DIR";
    /// Redis URL for a shared CCR store. Takes precedence over `CCR_DIR`.
    pub const REDIS_URL: &str = "HEADROOM_REDIS_URL";
    /// File for the durable savings ledger. Unset means savings are not persisted.
    pub const SAVINGS: &str = "HEADROOM_SAVINGS";
    /// Largest payload the compressor will attempt, in bytes.
    pub const MAX_BYTES: &str = "HEADROOM_MAX_BYTES";
    /// Most lines the compressor will attempt.
    pub const MAX_LINES: &str = "HEADROOM_MAX_LINES";
    /// Requests per minute before the proxy answers 429.
    pub const RATE_LIMIT: &str = "HEADROOM_RATE_LIMIT";
    /// Path to a `rusty_remind_me` SQLite store to query in-process (needs
    /// `--features linked-memory`). Unset means no linked memory backend, regardless of
    /// the other two `LINKED_MEMORY_*` settings.
    pub const LINKED_MEMORY_DB: &str = "HEADROOM_LINKED_MEMORY_DB";
    /// Path to a local `.rten` sentence-encoder model file for the linked backend's
    /// semantic tier. Both this and `LINKED_MEMORY_TOKENIZER` are required together; the
    /// tier is off (keyword-only) if either is unset or fails to load.
    pub const LINKED_MEMORY_MODEL: &str = "HEADROOM_LINKED_MEMORY_MODEL";
    /// Path to the tokenizer JSON matching `LINKED_MEMORY_MODEL`.
    pub const LINKED_MEMORY_TOKENIZER: &str = "HEADROOM_LINKED_MEMORY_TOKENIZER";
}

/// Every `HEADROOM_*` name that some code in this crate actually reads.
///
/// # Why this list has to exist
///
/// [`set_overrides`] used to accept and store any name with the `HEADROOM_` prefix,
/// checked against nothing. A typo — `HEADROOM_COMPRESION`, missing an `s` — or a
/// documented variable this crate has never wired to `Config` (`HEADROOM_LOG`, read
/// once by `main` through `tracing_subscriber::EnvFilter`, never by this module) was
/// stored in the override map, echoed back by the admin endpoint in its `applied` list
/// with an empty `needs_restart`, and never read again. An operator retuning a proxy
/// during an incident would believe the change had taken and move on.
///
/// Kept beside [`vars`] so a new setting is wired to both in the same edit, rather than
/// readable from `Config` but unreachable through the admin endpoint, or reachable
/// through the admin endpoint but read by nothing.
pub const KNOWN: [&str; 18] = [
    vars::HOST,
    vars::PORT,
    vars::UPSTREAM,
    vars::COMPRESSION,
    vars::OUTPUT_SHAPER,
    vars::RECOMMENDATIONS,
    vars::MEMORY,
    vars::MEMORY_LIMIT,
    vars::STABILIZE,
    vars::CCR_DIR,
    vars::REDIS_URL,
    vars::SAVINGS,
    vars::MAX_BYTES,
    vars::MAX_LINES,
    vars::RATE_LIMIT,
    vars::LINKED_MEMORY_DB,
    vars::LINKED_MEMORY_MODEL,
    vars::LINKED_MEMORY_TOKENIZER,
];

/// Settings that are read once at startup and ignored thereafter.
///
/// # Why this list has to exist
///
/// Most of `Config` is read per request, which is what makes hot-reload work. These are
/// not: the CCR store is opened once, memories and recommendations are loaded once
/// deliberately (a set that changed between requests would make the same request produce
/// different bytes depending on when it arrived — invariant I4), and the listen socket is
/// bound once.
///
/// `POST /admin/runtime-env` will happily store any of them and previously answered
/// `applied`, which was false: the value sat in the override map and nothing ever read it
/// again. An operator retuning a proxy during an incident would believe they had changed
/// something and move on. The endpoint now names them.
///
/// Kept beside the variables themselves so a new startup-only setting is added here in the
/// same edit rather than discovered later by someone whose change silently did nothing.
pub const STARTUP_ONLY: [&str; 13] = [
    vars::HOST,
    vars::PORT,
    // The one that mattered most, and the one this list was missing.
    //
    // `AppState::new` builds an `Upstream` once and stores the base URL inside it; the
    // request path uses that client and never re-reads configuration. So a new
    // `HEADROOM_UPSTREAM` landed in the override map and changed nothing.
    //
    // Measured, against two loopback providers: the admin endpoint answered
    // `{"applied":["HEADROOM_UPSTREAM"],"needs_restart":[]}`, `/health` reported the new
    // address, and the next request was served by the old one. Three self-reports
    // agreeing with each other and all three wrong — during exactly the incident an
    // operator would be using them to resolve.
    vars::UPSTREAM,
    vars::RECOMMENDATIONS,
    vars::MEMORY,
    vars::MEMORY_LIMIT,
    vars::CCR_DIR,
    vars::REDIS_URL,
    // Opened once at startup, like the CCR store beside it. A new path landing in the
    // override map would leave every subsequent write going to the old file.
    vars::SAVINGS,
    // The limiter is constructed once with its capacity baked in, the same as the relay
    // client beside it. A new value landing in the override map would change nothing
    // while the endpoint reported success.
    vars::RATE_LIMIT,
    // The linked memory backend's database connection, and its embedder if configured,
    // are resolved once at startup and pinned for the process lifetime — deliberately,
    // per invariant I4 (DECISIONS D44). A live database re-opened per request would make
    // the same request compress differently depending on when it arrived; an embedder
    // that could load or fail to load per request would do the same to the semantic tier.
    vars::LINKED_MEMORY_DB,
    vars::LINKED_MEMORY_MODEL,
    vars::LINKED_MEMORY_TOKENIZER,
];

/// The safety limits the compressor runs under.
///
/// # Why these are configurable and a *deadline* is not
///
/// The reference exposes `HEADROOM_COMPRESSION_DEADLINE_MS` — a wall-clock budget after
/// which compression stops. That cannot exist here: invariant I4 requires the same bytes
/// to compress identically on every run, and a time-based cutoff makes the output depend
/// on how busy the machine was. The same request would compress differently on a loaded
/// host than on an idle one, and `tests/properties.rs` asserts otherwise.
///
/// These bound the same risk **deterministically**. A payload that would cost unbounded
/// time is refused on a property of the payload — its size, depth, line length, line
/// count — rather than on how long it has already taken. Same protection, and the answer
/// never depends on the clock.
pub fn safety_limits() -> headroom_core::pipeline::safety::Limits {
    let mut limits = headroom_core::pipeline::safety::Limits::default();

    if let Some(bytes) = setting(vars::MAX_BYTES).and_then(|v| v.trim().parse().ok()) {
        limits.max_bytes = bytes;
    }
    if let Some(lines) = setting(vars::MAX_LINES).and_then(|v| v.trim().parse().ok()) {
        limits.max_lines = lines;
    }

    limits
}

/// The default request-per-minute backstop.
///
/// Set well above any human or agent workload. This is a backstop against a retry loop
/// somewhere upstream of the proxy relaying thousands of requests with the customer's
/// credential attached — not a quota, and it should never be the thing a real user meets.
pub const DEFAULT_RATE_LIMIT: u32 = 600;

/// Requests per minute before the proxy answers 429.
///
/// A backstop against a loop, not a quota. Zero is rejected rather than honored: a limiter
/// that refuses everything is indistinguishable from an outage, and somebody typing `0`
/// meaning "no limit" would take their own proxy down.
pub fn rate_limit() -> u32 {
    setting(vars::RATE_LIMIT)
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|capacity| *capacity > 0)
        .unwrap_or(DEFAULT_RATE_LIMIT)
}

/// Where the durable savings ledger lives, if one is configured.
///
/// `None` means savings are not persisted and `headroom savings` falls back to reading a
/// `/metrics` scrape — the behavior every deployment had before the ledger existed.
pub fn savings_path() -> Option<std::path::PathBuf> {
    setting(vars::SAVINGS)
        .filter(|value| !value.trim().is_empty())
        .map(std::path::PathBuf::from)
}

/// Path to a `rusty_remind_me` SQLite store, if a linked memory backend is configured.
///
/// `None` means no linked backend, the same "unconfigured is off" contract every other
/// optional path here follows.
pub fn linked_memory_db_path() -> Option<std::path::PathBuf> {
    setting(vars::LINKED_MEMORY_DB)
        .filter(|value| !value.trim().is_empty())
        .map(std::path::PathBuf::from)
}

/// Path to the local sentence-encoder model file for the linked backend's semantic tier.
pub fn linked_memory_model_path() -> Option<std::path::PathBuf> {
    setting(vars::LINKED_MEMORY_MODEL)
        .filter(|value| !value.trim().is_empty())
        .map(std::path::PathBuf::from)
}

/// Path to the tokenizer JSON matching [`linked_memory_model_path`].
pub fn linked_memory_tokenizer_path() -> Option<std::path::PathBuf> {
    setting(vars::LINKED_MEMORY_TOKENIZER)
        .filter(|value| !value.trim().is_empty())
        .map(std::path::PathBuf::from)
}

/// Which CCR store the proxy actually built.
///
/// Reported by `/health` because the configured store and the built one are not the same
/// thing, and the gap is silent: a `HEADROOM_CCR_DIR` the process cannot open falls back
/// to memory with a startup warning, and every marker handed to the model from then on is
/// unredeemable across a restart while the proxy reports itself healthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcrStoreKind {
    /// Nothing survives a restart, and nothing is shared between workers. The default.
    Memory,
    /// A local directory. Survives a restart; not shared between hosts.
    File,
    /// Shared between workers.
    Redis,
}

impl CcrStoreKind {
    /// A stable identifier, for `/health` and for logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::File => "file",
            Self::Redis => "redis",
        }
    }

    /// Whether a marker written now can still be redeemed after a restart.
    ///
    /// The question an operator is actually asking. `memory` is a correct answer to it
    /// when nobody configured anything, and a silent failure when somebody did.
    pub fn survives_restart(self) -> bool {
        !matches!(self, Self::Memory)
    }
}

/// Default listen port.
pub const DEFAULT_PORT: u16 = 8787;

/// How many memories one injection carries unless configured otherwise.
///
/// Small on purpose. Injection spends live-zone tokens on every request, and a handful
/// of corroborated facts is worth more to a model than a wall of them.
pub const DEFAULT_MEMORY_LIMIT: usize = 8;

/// Default upstream provider.
pub const DEFAULT_UPSTREAM: &str = "https://api.anthropic.com";

/// Runtime overrides, consulted ahead of the process environment.
///
/// # Why an override map rather than `std::env::set_var`
///
/// Hot-reload could be implemented by writing the process environment directly, and
/// that would be a bug rather than a shortcut. `setenv` is not safe to call while
/// another thread may be in `getenv`, and this proxy reads its configuration on every
/// request from a thread pool — so a hot-reload would be racing every in-flight
/// request, with undefined behavior rather than a stale read as the failure mode.
///
/// An `RwLock` map costs an uncontended read lock per lookup and is simply correct.
static OVERRIDES: RwLock<Option<BTreeMap<String, String>>> = RwLock::new(None);

/// Reads a setting, preferring a runtime override over the process environment.
fn setting(name: &str) -> Option<String> {
    // A poisoned lock falls through to the environment rather than propagating. A
    // panic in an unrelated admin request should not take configuration reads with it.
    if let Ok(guard) = OVERRIDES.read() {
        if let Some(value) = guard.as_ref().and_then(|map| map.get(name)) {
            return Some(value.clone());
        }
    }
    env::var(name).ok()
}

/// Applies runtime overrides, merging them over any already in force.
///
/// Returns the names that were accepted. Only names in [`KNOWN`] are accepted — that
/// rejects everything outside the `HEADROOM_` namespace, since every entry in `KNOWN`
/// carries the prefix, but it also rejects an unrecognized `HEADROOM_*` name, such as a
/// typo (`HEADROOM_COMPRESION`) or a variable no code in this crate reads
/// (`HEADROOM_LOG`, startup-only and owned by `main`, never by this module). Accepting
/// either would let the caller believe an unread name is live, and letting an
/// out-of-namespace name through at all would make this endpoint a general-purpose
/// lever on the process for anyone who can reach it.
///
/// # Why merge, when this used to replace
///
/// [`preview_overrides`] merges — deliberately, and its doc says why: *"a call that sets
/// only the upstream still has to be judged against whatever listen address is currently
/// overridden."* While this replaced, the two modelled different configurations, so the
/// check that runs before every apply was validating a configuration that never existed.
///
/// The operator-visible half was worse. Measured against a running proxy:
///
/// | step | response | forwarded |
/// | --- | --- | --- |
/// | `{"HEADROOM_COMPRESSION":"0"}` | `applied: [HEADROOM_COMPRESSION]` | 19500 bytes, compression off |
/// | `{"HEADROOM_STABILIZE":"1"}` | `applied: [HEADROOM_STABILIZE]` | 1895 bytes, **compression back on** |
///
/// Nothing in the second response mentions that the first setting was dropped. That is
/// the scenario [`crate::admin`] opens by naming — *"turning compression off during an
/// incident should not cost"* a restart — undone by any later retune of anything else.
///
/// To remove one override, send it as an empty value; to drop all of them, use
/// [`clear_overrides`]. Sending `{}` is now a no-op rather than an undocumented wipe.
pub fn set_overrides(values: BTreeMap<String, String>) -> Vec<String> {
    let accepted: BTreeMap<String, String> = values
        .into_iter()
        .filter(|(name, _)| KNOWN.contains(&name.as_str()))
        .collect();
    let names = accepted.keys().cloned().collect();

    if let Ok(mut guard) = OVERRIDES.write() {
        let map = guard.get_or_insert_with(BTreeMap::new);
        for (name, value) in accepted {
            // An empty value is "take this override back", not "override it with the
            // empty string". `setting()` only falls through to `env::var()` when the
            // key is *absent* from this map, so leaving an empty-string entry in place
            // permanently shadowed the process environment with whatever every
            // consumer's own empty-input default happens to be — turning
            // `HEADROOM_COMPRESSION=0` (an env-configured safety policy) back on the
            // moment an operator tried to "take back" an unrelated override, since
            // `compression_enabled` reads `Some("")` as `true`.
            if value.is_empty() {
                map.remove(&name);
            } else {
                map.insert(name, value);
            }
        }
    }
    names
}

/// The configuration that `values` *would* produce, without applying them.
///
/// # Why a preview rather than "apply, check, roll back"
///
/// Applying first would make the bad configuration live for the duration of the check,
/// and this proxy reads its config on every request from a thread pool — so an
/// in-flight request could pick up a self-referential upstream in that window and start
/// the very loop the check exists to prevent.
///
/// Overrides already in force are included, because a call that sets only the upstream
/// still has to be judged against whatever listen address is currently overridden.
pub fn preview_overrides(values: &BTreeMap<String, String>) -> Config {
    let mut merged = overrides();
    for (name, value) in values {
        if !name.starts_with("HEADROOM_") {
            continue;
        }
        // Same "empty means take it back" rule as `set_overrides`, so a preview built
        // from a clearing call matches what applying it would actually produce.
        if value.is_empty() {
            merged.remove(name);
        } else {
            merged.insert(name.clone(), value.clone());
        }
    }

    let lookup = |name: &str| -> Option<String> {
        merged.get(name).cloned().or_else(|| env::var(name).ok())
    };
    let defaults = Config::default();

    Config {
        host: lookup(vars::HOST)
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(defaults.host),
        port: lookup(vars::PORT)
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(defaults.port),
        upstream: lookup(vars::UPSTREAM)
            .filter(|raw| !raw.trim().is_empty())
            .map(|raw| raw.trim_end_matches('/').to_owned())
            .unwrap_or(defaults.upstream),
        compression_enabled: lookup(vars::COMPRESSION)
            .map(|raw| !matches!(raw.trim(), "0" | "false" | "off" | "no"))
            .unwrap_or(defaults.compression_enabled),
    }
}

/// Clears every runtime override, restoring the process environment.
pub fn clear_overrides() {
    if let Ok(mut guard) = OVERRIDES.write() {
        *guard = None;
    }
}

/// The overrides currently in force.
pub fn overrides() -> BTreeMap<String, String> {
    OVERRIDES
        .read()
        .ok()
        .and_then(|guard| guard.clone())
        .unwrap_or_default()
}

/// Runtime configuration.
///
/// # Example
///
/// ```
/// use headroom_proxy::config::Config;
///
/// let config = Config::from_env();
/// assert!(config.listen_addr().port() > 0);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    host: IpAddr,
    port: u16,
    upstream: String,
    compression_enabled: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // Loopback, not 0.0.0.0. The proxy forwards provider credentials, and a
            // default that binds every interface would expose an open credential
            // relay to anything that can reach the host. Widening this is a
            // deliberate act, not a default.
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: DEFAULT_PORT,
            upstream: DEFAULT_UPSTREAM.to_owned(),
            compression_enabled: true,
        }
    }
}

impl Config {
    /// Reads configuration from the environment, falling back to defaults.
    ///
    /// Unparseable values fall back rather than failing. A malformed `HEADROOM_PORT`
    /// should not take down a running proxy on the next config read — it should be
    /// ignored and logged.
    pub fn from_env() -> Self {
        let defaults = Self::default();

        Self {
            host: setting(vars::HOST)
                .and_then(|raw| raw.parse().ok())
                .unwrap_or(defaults.host),
            port: setting(vars::PORT)
                .and_then(|raw| raw.parse().ok())
                .unwrap_or(defaults.port),
            upstream: setting(vars::UPSTREAM)
                .filter(|raw| !raw.trim().is_empty())
                .map(|raw| raw.trim_end_matches('/').to_owned())
                .unwrap_or(defaults.upstream),
            compression_enabled: setting(vars::COMPRESSION)
                .map(|raw| !matches!(raw.trim(), "0" | "false" | "off" | "no"))
                .unwrap_or(defaults.compression_enabled),
        }
    }

    /// The socket to bind.
    pub fn listen_addr(&self) -> SocketAddr {
        SocketAddr::new(self.host, self.port)
    }

    /// Base URL requests are forwarded to, without a trailing slash.
    pub fn upstream(&self) -> &str {
        &self.upstream
    }

    /// How much output to ask the model for.
    ///
    /// Off unless explicitly set. Output shaping changes what the model *writes*, which
    /// is a visible change to the customer's product rather than an invisible saving —
    /// a proxy that quietly made every answer terser would be editing someone's
    /// application on their behalf.
    ///
    /// `1` and `on` mean terse, since that is the reason anyone enables this.
    pub fn verbosity(&self) -> Verbosity {
        match setting(vars::OUTPUT_SHAPER)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "terse" | "1" | "on" | "true" | "yes" => Verbosity::Terse,
            "full" | "verbose" => Verbosity::Full,
            _ => Verbosity::Default,
        }
    }

    /// Recommendations learned from a previous run, read at startup.
    ///
    /// # Read once, not per request
    ///
    /// Unlike every other accessor here, this is *not* a live read. The recommendations
    /// are a configuration input fixed for the process lifetime — consulting them per
    /// request would make compression depend on a file that could change mid-flight, so
    /// the same request would compress differently depending on when it arrived. That is
    /// invariant I4, and it is the reason `headroom-core`'s telemetry has no
    /// request-time read at all.
    ///
    /// A missing, unreadable, or corrupt file yields an empty set. The file is an
    /// optimization; refusing to start without a valid one would turn a cache of
    /// statistics into a hard startup dependency.
    pub fn recommendations() -> headroom_core::telemetry::Recommendations {
        let Some(path) = setting(vars::RECOMMENDATIONS) else {
            return Default::default();
        };

        match std::fs::read_to_string(&path) {
            Ok(source) => {
                let parsed = headroom_core::telemetry::Recommendations::from_json_lossy(&source);
                tracing::info!(
                    path = %path,
                    entries = parsed.entries.len(),
                    "loaded compression recommendations"
                );
                parsed
            }
            Err(err) => {
                tracing::warn!(path = %path, %err, "could not read recommendations; starting with none");
                Default::default()
            }
        }
    }

    /// Memories to inject into the live-zone tail, read at startup.
    ///
    /// # Read once, for the same reason as [`Config::recommendations`]
    ///
    /// Reading per request would let the same request produce different bytes depending
    /// on when it arrived, which is invariant I4 — and here the consequence is direct,
    /// because these bytes go upstream and a request that varies busts the prompt cache
    /// the injection was placed in the live zone to protect.
    ///
    /// An unset variable yields an empty store, and an empty store injects nothing. A
    /// proxy with no memory file configured behaves exactly as it did before this
    /// existed.
    pub fn memories() -> headroom_core::memory::MemoryStore {
        let Some(path) = setting(vars::MEMORY) else {
            return Default::default();
        };

        match std::fs::read_to_string(&path) {
            Ok(source) => {
                let parsed = headroom_core::memory::MemoryStore::from_jsonl_lossy(&source);
                tracing::info!(path = %path, memories = parsed.len(), "loaded memories");
                parsed
            }
            Err(err) => {
                tracing::warn!(path = %path, %err, "could not read memories; injecting none");
                Default::default()
            }
        }
    }

    /// How many memories one injection may carry.
    ///
    /// Bounded because recall is ordered but unbounded, and a store that has accumulated
    /// a thousand facts would otherwise put all of them on every request — spending far
    /// more than the compression elsewhere in the pipeline saves.
    pub fn memory_limit() -> usize {
        setting(vars::MEMORY_LIMIT)
            .and_then(|raw| raw.trim().parse().ok())
            .filter(|limit| *limit > 0)
            .unwrap_or(DEFAULT_MEMORY_LIMIT)
    }

    /// The CCR store this process should use.
    ///
    /// # Why the proxy needs a choice here at all
    ///
    /// It used to construct an [`InMemoryCcrStore`] unconditionally, which is wrong in
    /// two ways that only show up in deployment. A restart drops every stored original,
    /// so a `<<ccr:HASH>>` marker the model is still holding becomes unretrievable. And
    /// with more than one worker, the marker is created on one process and requested from
    /// another that has never seen it — an intermittent failure that depends on load
    /// balancing and reads as data loss.
    ///
    /// # Precedence, and why Redis wins
    ///
    /// Redis first, then a directory, then memory. Anyone who has configured a shared
    /// store has a multi-worker deployment, which is the one problem a local directory
    /// cannot solve — so naming both is not ambiguous, it is a deployment that needs the
    /// shared one.
    ///
    /// # A misconfigured store never stops the proxy
    ///
    /// Every failure here falls back to memory and logs. CCR is a *recovery* path for
    /// content a compressor elided; losing it costs retrievability, while refusing to
    /// start costs the customer their whole service. The wrong trade would be to treat a
    /// cache as a hard dependency.
    ///
    /// [`InMemoryCcrStore`]: headroom_core::ccr::InMemoryCcrStore
    pub fn ccr_store() -> std::sync::Arc<dyn headroom_core::ccr::CcrStore> {
        Self::ccr_store_with_kind().0
    }

    /// The same store, with the kind that was actually built.
    ///
    /// Separate from [`Self::ccr_store`] because the two answers differ, and the
    /// difference is invisible: a `HEADROOM_CCR_DIR` that cannot be opened falls back to
    /// memory and logs a warning, after which the proxy relays, compresses, and hands the
    /// model `<<ccr:...>>` markers that no longer survive a restart. Measured — put a
    /// value, rebuild the store from the same configuration, and it is gone.
    ///
    /// The kind travels as a return value rather than being re-derived downstream for the
    /// reason [`crate::health::Health::upstream`] records: a second reader of the
    /// *configuration* reports what was asked for, which is exactly the case where the
    /// operator needs to be told it did not happen.
    pub fn ccr_store_with_kind() -> (
        std::sync::Arc<dyn headroom_core::ccr::CcrStore>,
        CcrStoreKind,
    ) {
        use headroom_core::ccr::{FileCcrStore, InMemoryCcrStore};

        if let Some(url) = setting(vars::REDIS_URL).filter(|raw| !raw.trim().is_empty()) {
            match connect_redis(url.trim()) {
                Ok(store) => return (store, CcrStoreKind::Redis),
                Err(err) => {
                    tracing::warn!(%err, "could not use the redis CCR store; falling back");
                }
            }
        }

        if let Some(dir) = setting(vars::CCR_DIR).filter(|raw| !raw.trim().is_empty()) {
            match FileCcrStore::open(dir.trim()) {
                Ok(store) => {
                    tracing::info!(dir = %dir.trim(), "using a file-backed CCR store");
                    return (std::sync::Arc::new(store), CcrStoreKind::File);
                }
                Err(err) => {
                    tracing::warn!(dir = %dir.trim(), %err, "could not open the CCR directory; using memory");
                }
            }
        }

        (
            std::sync::Arc::new(InMemoryCcrStore::new()),
            CcrStoreKind::Memory,
        )
    }

    /// Whether the operator asked for a store that outlives the process.
    ///
    /// Read from configuration deliberately — this is the *request*, and comparing it
    /// against what [`Self::ccr_store_with_kind`] actually built is the whole point.
    pub fn persistent_store_requested() -> bool {
        [vars::REDIS_URL, vars::CCR_DIR]
            .iter()
            .any(|name| setting(name).is_some_and(|raw| !raw.trim().is_empty()))
    }

    /// Whether cache stabilization may rewrite the hot zone.
    ///
    /// # Off by default, and this one is not timidity
    ///
    /// Invariant I2 says the cache hot zone — `system`, `tools[*]`, frozen messages — is
    /// never modified. Normalizing tools and placing `cache_control` breakpoints both
    /// modify it, so they cannot be on by default without making I2 a slogan rather than
    /// a property the tests enforce.
    ///
    /// The trade they offer is real but it is a *trade*: one cache miss now, in exchange
    /// for hits later. Placing breakpoints is how an Anthropic conversation gets cached
    /// at all, and normalizing tools rescues a client that serializes them inconsistently
    /// between runs. Both are worth it for some deployments and pure cost for others —
    /// a client that already serializes stably pays the miss and gains nothing.
    ///
    /// So the operator decides, and the I2 tests keep running against the default.
    pub fn stabilization_enabled() -> bool {
        matches!(
            setting(vars::STABILIZE).unwrap_or_default().trim(),
            "1" | "true" | "on" | "yes"
        )
    }

    /// Whether compression runs at all.
    ///
    /// When `false` the proxy is a pure passthrough — which is also exactly what the
    /// invariant I1 round-trip test exercises.
    pub fn compression_enabled(&self) -> bool {
        self.compression_enabled
    }
}

/// Opens the Redis-backed CCR store.
///
/// # Why this is two functions rather than one `cfg` block inline
///
/// The feature-off arm has to be a *distinct* outcome from a connection failure. A build
/// without the `redis` feature that silently fell back to memory would leave an operator
/// who set `HEADROOM_REDIS_URL` believing they had a shared store, and the symptom —
/// retrievals failing on some workers — looks identical to a Redis that is down. Naming
/// the real reason is the whole point.
#[cfg(feature = "redis")]
fn connect_redis(url: &str) -> Result<std::sync::Arc<dyn headroom_core::ccr::CcrStore>, String> {
    match headroom_core::ccr::RedisCcrStore::connect(url) {
        Ok(store) => {
            tracing::info!("using a redis-backed CCR store");
            Ok(std::sync::Arc::new(store))
        }
        Err(err) => Err(err.to_string()),
    }
}

/// The same, for a build without the `redis` feature.
#[cfg(not(feature = "redis"))]
fn connect_redis(_url: &str) -> Result<std::sync::Arc<dyn headroom_core::ccr::CcrStore>, String> {
    Err("this build has no redis support; rebuild with --features redis".to_owned())
}

#[cfg(test)]
mod tests {

    use super::*;

    /// Takes the shared settings lock from a synchronous test.
    ///
    /// `blocking_lock` rather than `lock().await` because these are `#[test]`, not
    /// `#[tokio::test]` — and it is the *same* lock `admin`'s tests take, which is the
    /// point: two separate mutexes would not have serialized these against each other,
    /// and the override map is process-wide.
    fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
        crate::settings_test_lock().blocking_lock()
    }

    // ---- limits an operator can now turn (#196) ----

    #[test]
    fn safety_limits_default_to_the_compiled_values() {
        // Unset means unchanged. Every deployment that has not set these behaves exactly
        // as it did before they existed.
        let _guard = env_lock();
        clear_overrides();
        std::env::remove_var(vars::MAX_BYTES);
        std::env::remove_var(vars::MAX_LINES);

        assert_eq!(
            safety_limits(),
            headroom_core::pipeline::safety::Limits::default()
        );
    }

    #[test]
    fn safety_limits_honor_an_override() {
        let _guard = env_lock();
        clear_overrides();
        set_overrides(std::collections::BTreeMap::from([
            (vars::MAX_BYTES.to_owned(), "4096".to_owned()),
            (vars::MAX_LINES.to_owned(), "500".to_owned()),
        ]));

        let limits = safety_limits();
        assert_eq!(limits.max_bytes, 4096);
        assert_eq!(limits.max_lines, 500);
        // Untouched fields keep their compiled values rather than resetting.
        assert_eq!(
            limits.max_depth,
            headroom_core::pipeline::safety::Limits::default().max_depth
        );

        clear_overrides();
    }

    #[test]
    fn an_unparseable_limit_keeps_the_default_rather_than_disabling_the_guard() {
        // A typo must not silently remove a safety bound. Falling back to the compiled
        // value is the safe direction; parsing `"lots"` as zero would turn the guard off.
        let _guard = env_lock();
        clear_overrides();
        set_overrides(std::collections::BTreeMap::from([(
            vars::MAX_BYTES.to_owned(),
            "lots".to_owned(),
        )]));

        assert_eq!(
            safety_limits().max_bytes,
            headroom_core::pipeline::safety::Limits::default().max_bytes
        );

        clear_overrides();
    }

    #[test]
    fn a_zero_rate_limit_is_refused_rather_than_honored() {
        // A limiter with zero capacity refuses every request, which is indistinguishable
        // from an outage. Somebody typing `0` meaning "no limit" would take their own
        // proxy down with a setting that read as permissive.
        let _guard = env_lock();
        clear_overrides();
        set_overrides(std::collections::BTreeMap::from([(
            vars::RATE_LIMIT.to_owned(),
            "0".to_owned(),
        )]));

        assert_eq!(rate_limit(), DEFAULT_RATE_LIMIT);

        set_overrides(std::collections::BTreeMap::from([(
            vars::RATE_LIMIT.to_owned(),
            "50".to_owned(),
        )]));
        assert_eq!(rate_limit(), 50);

        clear_overrides();
    }

    #[test]
    fn every_new_setting_is_reachable_through_the_admin_endpoint() {
        // `KNOWN` is what `set_overrides` accepts. A setting readable from `Config` and
        // absent here is one an operator cannot retune, and the endpoint would report
        // their attempt as ignored.
        for name in [vars::MAX_BYTES, vars::MAX_LINES, vars::RATE_LIMIT] {
            assert!(KNOWN.contains(&name), "{name} is unreachable from /admin");
        }
    }

    #[test]
    fn defaults_are_conservative() {
        let config = Config::default();
        assert_eq!(config.listen_addr().port(), DEFAULT_PORT);
        assert!(config.compression_enabled());
        assert_eq!(config.upstream(), DEFAULT_UPSTREAM);
    }

    #[test]
    fn the_default_bind_is_loopback_not_every_interface() {
        // The proxy forwards provider credentials. A 0.0.0.0 default would make an
        // open credential relay the out-of-the-box behavior.
        assert!(Config::default().listen_addr().ip().is_loopback());
    }

    #[test]
    fn a_trailing_slash_on_the_upstream_is_normalized_away() {
        // Otherwise every forwarded path gets a double slash.
        let config = Config {
            upstream: "https://example.com/".trim_end_matches('/').to_owned(),
            ..Config::default()
        };
        assert_eq!(config.upstream(), "https://example.com");
    }

    #[test]
    fn compression_off_is_recognized_in_its_common_spellings() {
        for raw in ["0", "false", "off", "no", " 0 "] {
            let disabled = !matches!(raw.trim(), "0" | "false" | "off" | "no");
            assert!(!disabled, "{raw:?} should disable compression");
        }
        for raw in ["1", "true", "on", "yes", ""] {
            let enabled = !matches!(raw.trim(), "0" | "false" | "off" | "no");
            assert!(enabled, "{raw:?} should leave compression on");
        }
    }

    #[test]
    fn from_env_is_usable_without_any_variables_set() {
        // The common case: nothing configured, everything defaulted, no panic.
        let config = Config::from_env();
        assert!(config.listen_addr().port() > 0);
        assert!(!config.upstream().is_empty());
    }
}
