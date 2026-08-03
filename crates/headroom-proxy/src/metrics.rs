//! Compression and cache metrics, in Prometheus text format.
//!
//! # What is worth measuring
//!
//! Token savings is the obvious metric and the least useful one on its own. The
//! question that actually matters is whether the *cache* is still working: this proxy
//! can save 90% of tokens on the live zone and still cost more overall if it broke the
//! provider's prefix. So cache-hit and cache-miss counters sit alongside the savings.
//!
//! # Observation only
//!
//! Nothing here feeds back into a compression decision — invariant I9. Metrics are
//! read by an operator, not by the compressor.

use std::sync::atomic::{AtomicU64, Ordering};

use headroom_core::pipeline::Routing;

/// Counters for one process.
///
/// Atomics rather than a lock: these are written on every request and read rarely, and
/// a metrics counter must never be able to block request handling.
#[derive(Debug, Default)]
pub struct Metrics {
    requests: AtomicU64,
    compressed: AtomicU64,
    passthrough: AtomicU64,
    tokens_before: AtomicU64,
    tokens_after: AtomicU64,
    cache_reads: AtomicU64,
    cache_creations: AtomicU64,
    stream_errors: AtomicU64,
    /// Blocks counted by why they were routed as they were.
    ///
    /// # Why a fixed array rather than a map
    ///
    /// The reasons are a closed set — gaining one is a deliberate change to
    /// `headroom-core`, and the array is sized from `Routing::REASONS` so that change
    /// carries here on its own. A map would add a lock or an atomic hash on a path that
    /// runs per block, to model a dimension that cannot grow at runtime.
    routing: [AtomicU64; ROUTING_REASONS.len()],
}

/// Every reason a block can be routed, in the order the counters are stored.
///
/// # Built from `Routing::REASONS`, not copied from it
///
/// This was a hand-written array of seven strings with a comment saying they were what
/// `Routing::as_str` produces. Nothing checked that, across a crate boundary, and the
/// failure is quiet by construction: [`Metrics::record_routing`] puts an unrecognized
/// reason in the `other` slot, so renaming a variant in `headroom-core` — or adding a
/// seventh — would merge a whole category into `other` while every test stayed green and
/// the dashboard panel for it went permanently empty.
///
/// `other` is appended here rather than living in core, because it is not a routing
/// outcome. It is this counter's answer to a reason from a build of `headroom-core` that
/// disagrees with this one — visible as a number rather than silently dropped.
const ROUTING_REASONS: [&str; Routing::REASONS.len() + 1] = {
    let mut reasons = [""; Routing::REASONS.len() + 1];
    let mut index = 0;
    // A `while` rather than an iterator: this runs in a const context, where `for` and
    // `copy_from_slice` are not available.
    while index < Routing::REASONS.len() {
        reasons[index] = Routing::REASONS[index];
        index += 1;
    }
    reasons[Routing::REASONS.len()] = "other";
    reasons
};

impl Metrics {
    /// Creates zeroed counters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a request that was compressed.
    pub fn record_compressed(&self, tokens_before: u64, tokens_after: u64) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.compressed.fetch_add(1, Ordering::Relaxed);
        self.tokens_before
            .fetch_add(tokens_before, Ordering::Relaxed);
        self.tokens_after.fetch_add(tokens_after, Ordering::Relaxed);
    }

    /// Records why one block was routed as it was.
    ///
    /// Invariant I9: this observes and nothing else. It takes the reason `route` already
    /// computed, and changes no decision and no byte.
    ///
    /// # Why per block rather than per request
    ///
    /// One request carries many blocks, and they routinely route differently — a JSON
    /// tool result compresses while the prose beside it is below threshold. Counting per
    /// request would force a single label onto a mixed outcome, and the label would be
    /// whichever block happened to come last.
    pub fn record_routing(&self, reason: &str) {
        let index = ROUTING_REASONS
            .iter()
            .position(|known| *known == reason)
            // The `other` slot, last. An unknown reason is a `Routing` variant this
            // build does not know about, which should show up as a number rather than
            // vanish.
            .unwrap_or(ROUTING_REASONS.len() - 1);

        self.routing[index].fetch_add(1, Ordering::Relaxed);
    }

    /// Records a request forwarded unchanged.
    pub fn record_passthrough(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.passthrough.fetch_add(1, Ordering::Relaxed);
    }

    /// Records provider-reported cache usage.
    ///
    /// This is the number that says whether the proxy is helping. Savings without
    /// cache reads means the prefix is being invalidated somewhere.
    pub fn record_cache_usage(&self, read_tokens: u64, creation_tokens: u64) {
        self.cache_reads.fetch_add(read_tokens, Ordering::Relaxed);
        self.cache_creations
            .fetch_add(creation_tokens, Ordering::Relaxed);
    }

    /// Records a stream that ended in an error.
    pub fn record_stream_error(&self) {
        self.stream_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Tokens saved so far.
    pub fn tokens_saved(&self) -> u64 {
        self.tokens_before
            .load(Ordering::Relaxed)
            .saturating_sub(self.tokens_after.load(Ordering::Relaxed))
    }

    /// Fraction of cache-eligible tokens that were served from cache, in `0.0..=1.0`.
    ///
    /// Returns `None` when nothing cacheable has been seen, rather than `0.0`. A rate
    /// of zero and no data at all are different situations, and an operator reading a
    /// dashboard should not have to guess which one they are looking at.
    pub fn cache_hit_rate(&self) -> Option<f64> {
        let reads = self.cache_reads.load(Ordering::Relaxed);
        let creations = self.cache_creations.load(Ordering::Relaxed);
        let total = reads + creations;
        (total > 0).then(|| reads as f64 / total as f64)
    }

    /// Renders the counters in Prometheus text exposition format.
    pub fn render(&self) -> String {
        let mut out = String::new();

        let metrics: [(&str, &str, u64); 8] = [
            (
                "headroom_requests_total",
                "Requests seen.",
                self.requests.load(Ordering::Relaxed),
            ),
            (
                "headroom_compressed_total",
                "Requests where compression applied.",
                self.compressed.load(Ordering::Relaxed),
            ),
            (
                "headroom_passthrough_total",
                "Requests forwarded unchanged.",
                self.passthrough.load(Ordering::Relaxed),
            ),
            (
                "headroom_tokens_before_total",
                "Tokens before compression.",
                self.tokens_before.load(Ordering::Relaxed),
            ),
            (
                "headroom_tokens_after_total",
                "Tokens after compression.",
                self.tokens_after.load(Ordering::Relaxed),
            ),
            (
                "headroom_tokens_saved_total",
                "Tokens saved.",
                self.tokens_saved(),
            ),
            (
                "headroom_cache_read_tokens_total",
                "Tokens served from the provider cache.",
                self.cache_reads.load(Ordering::Relaxed),
            ),
            (
                "headroom_cache_creation_tokens_total",
                "Tokens written to the provider cache.",
                self.cache_creations.load(Ordering::Relaxed),
            ),
        ];

        for (name, help, value) in metrics {
            out.push_str(&format!(
                "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}\n"
            ));
        }

        // A single labelled metric rather than six named ones: the reasons are one
        // dimension of one measurement, and Prometheus is built to sum and filter across
        // a label. Six separate counters would make "how many blocks were declined for
        // any reason" a query nobody writes.
        out.push_str(
            "# HELP headroom_routing_total Blocks by why they were routed as they were.\n\
             # TYPE headroom_routing_total counter\n",
        );
        for (index, reason) in ROUTING_REASONS.iter().enumerate() {
            out.push_str(&format!(
                "headroom_routing_total{{reason=\"{reason}\"}} {}\n",
                self.routing[index].load(Ordering::Relaxed)
            ));
        }

        out.push_str(
            "# HELP headroom_stream_errors_total Streams that ended in a provider error.\n\
             # TYPE headroom_stream_errors_total counter\n",
        );
        out.push_str(&format!(
            "headroom_stream_errors_total {}\n",
            self.stream_errors.load(Ordering::Relaxed)
        ));

        // Emitted only when there is data. A gauge reporting 0.0 for "no requests yet"
        // is indistinguishable on a dashboard from a cache that has completely stopped
        // working, which is the alarm this metric exists to raise.
        if let Some(rate) = self.cache_hit_rate() {
            out.push_str(
                "# HELP headroom_cache_hit_rate Fraction of cacheable tokens served from cache.\n\
                 # TYPE headroom_cache_hit_rate gauge\n",
            );
            out.push_str(&format!("headroom_cache_hit_rate {rate:.4}\n"));
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate() {
        let metrics = Metrics::new();
        metrics.record_compressed(1000, 100);
        metrics.record_compressed(500, 50);
        metrics.record_passthrough();

        assert_eq!(metrics.tokens_saved(), 1350);
        let rendered = metrics.render();
        assert!(rendered.contains("headroom_requests_total 3"), "{rendered}");
        assert!(rendered.contains("headroom_compressed_total 2"));
        assert!(rendered.contains("headroom_passthrough_total 1"));
    }

    #[test]
    fn the_hit_rate_is_absent_rather_than_zero_before_any_data() {
        // A gauge reporting 0.0 for "nothing yet" is indistinguishable on a dashboard
        // from a cache that has completely stopped working — which is exactly the
        // alarm this metric exists to raise.
        let metrics = Metrics::new();
        assert_eq!(metrics.cache_hit_rate(), None);
        assert!(!metrics.render().contains("headroom_cache_hit_rate"));
    }

    #[test]
    fn the_hit_rate_appears_once_there_is_data() {
        let metrics = Metrics::new();
        metrics.record_cache_usage(900, 100);

        assert_eq!(metrics.cache_hit_rate(), Some(0.9));
        assert!(metrics.render().contains("headroom_cache_hit_rate 0.9000"));
    }

    #[test]
    fn a_genuinely_zero_hit_rate_is_reported() {
        // The alarm case: cacheable tokens seen, none served from cache.
        let metrics = Metrics::new();
        metrics.record_cache_usage(0, 500);
        assert_eq!(metrics.cache_hit_rate(), Some(0.0));
        assert!(metrics.render().contains("headroom_cache_hit_rate 0.0000"));
    }

    #[test]
    fn tokens_saved_never_underflows() {
        // A compressor that somehow grew the input must not wrap to an enormous
        // "saving".
        let metrics = Metrics::new();
        metrics.record_compressed(100, 500);
        assert_eq!(metrics.tokens_saved(), 0);
    }

    #[test]
    fn the_output_is_valid_prometheus_exposition() {
        let metrics = Metrics::new();
        metrics.record_compressed(10, 5);
        metrics.record_cache_usage(10, 10);
        let rendered = metrics.render();

        for line in rendered.lines() {
            if line.starts_with('#') {
                assert!(
                    line.starts_with("# HELP ") || line.starts_with("# TYPE "),
                    "bad comment line: {line}"
                );
            } else {
                let parts: Vec<&str> = line.split_whitespace().collect();
                assert_eq!(parts.len(), 2, "bad sample line: {line}");
                parts[1].parse::<f64>().expect("value must be numeric");
            }
        }
    }

    #[test]
    fn routing_reasons_are_counted_separately() {
        // The six reasons have opposite fixes — "nothing handles this type" needs no
        // action, "policy forbids it" means checking the auth mode. Collapsing them into
        // one passthrough counter is how two entire content types went uncompressed
        // without anything surfacing it.
        let metrics = Metrics::new();
        metrics.record_routing("compress");
        metrics.record_routing("compress");
        metrics.record_routing("policy_forbids");

        let rendered = metrics.render();
        assert!(rendered.contains(r#"headroom_routing_total{reason="compress"} 2"#));
        assert!(rendered.contains(r#"headroom_routing_total{reason="policy_forbids"} 1"#));
        assert!(rendered.contains(r#"headroom_routing_total{reason="no_compressor"} 0"#));
    }

    #[test]
    fn an_unknown_reason_is_counted_rather_than_dropped() {
        // A `Routing` variant this build does not know about should show up as a number.
        // Telemetry that quietly loses a category is exactly how a whole content type
        // goes unnoticed.
        let metrics = Metrics::new();
        metrics.record_routing("something_new");

        assert!(metrics
            .render()
            .contains(r#"headroom_routing_total{reason="other"} 1"#));
    }

    #[test]
    fn every_metric_declares_help_and_type() {
        // A scraper accepts samples without them, but an operator reading the endpoint
        // cold has no idea what they are looking at.
        let metrics = Metrics::new();
        metrics.record_cache_usage(1, 1);
        let rendered = metrics.render();

        // The label set is stripped before matching. `HELP` and `TYPE` describe a metric
        // *family*, so one declaration covers every labelled sample under it — a
        // requirement of the exposition format, not a shortcut.
        let names: Vec<&str> = rendered
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| l.split_whitespace().next())
            .map(|name| name.split('{').next().unwrap_or(name))
            .collect();

        for name in names {
            assert!(
                rendered.contains(&format!("# HELP {name} ")),
                "{name} has no HELP"
            );
            assert!(
                rendered.contains(&format!("# TYPE {name} ")),
                "{name} has no TYPE"
            );
        }
    }

    #[test]
    fn concurrent_recording_does_not_lose_counts() {
        use std::sync::Arc;
        let metrics = Arc::new(Metrics::new());
        let mut handles = Vec::new();

        for _ in 0..8 {
            let metrics = Arc::clone(&metrics);
            handles.push(std::thread::spawn(move || {
                for _ in 0..100 {
                    metrics.record_compressed(10, 1);
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(metrics.tokens_saved(), 8 * 100 * 9);
    }
}
