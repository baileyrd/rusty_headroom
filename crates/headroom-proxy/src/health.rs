//! The health endpoint.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;

use crate::config::Config;
use crate::server::AppState;

/// What `/health` reports.
///
/// More than a bare `200`, deliberately. An operator hitting this during an incident
/// wants to know *which* proxy answered and what it currently believes its
/// configuration to be — a liveness check that only proves the socket is open leaves
/// them no better off.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Health {
    /// `"ok"`, or `"degraded"` when the proxy cannot actually relay.
    ///
    /// # Why this is not always `"ok"`
    ///
    /// It used to be. If the upstream client fails to build at startup the relay is
    /// disabled and *every* request returns an error, while this endpoint answered
    /// `"ok"` with a `200`.
    ///
    /// The trigger is narrower than it sounds and worth naming, so an operator seeing
    /// `"degraded"` knows where to look: `Upstream::new` fails only when
    /// `reqwest::Client::builder().build()` does, which is a TLS backend that will not
    /// initialize — missing root certificates, a broken rustls setup. **A malformed
    /// upstream URL does not trigger it**; that is parsed per request, checked by
    /// pointing a proxy at `"not a url at all"` and watching it report `relay_available:
    /// true`. So this state is covered by unit test rather than by an end-to-end
    /// reproduction.
    ///
    /// That is the worst possible place for a component to misreport itself. Load
    /// balancers and orchestrators route traffic on this signal, so a proxy that cannot
    /// serve a single request would keep being handed them, and the operator debugging it
    /// would have a health check telling them to look elsewhere.
    pub status: &'static str,
    /// The running version.
    pub version: &'static str,
    /// Where requests are being forwarded.
    ///
    /// Read from the relay that will actually carry them, not from configuration. The
    /// two disagree the moment `POST /admin/runtime-env` accepts a new
    /// `HEADROOM_UPSTREAM`: the value lands in the override map, nothing rebuilds the
    /// client, and every request keeps going to the old provider. This field reported
    /// the configured value, so an operator retuning a proxy mid-incident got a second
    /// confirmation of a change that had not happened.
    ///
    /// `"none"` when no relay was built — see `relay_available`.
    pub upstream: String,
    /// Whether compression is currently active.
    ///
    /// The single most useful field here: "is the proxy actually doing anything" is
    /// the first question during an incident, and it is runtime-configurable.
    pub compression_enabled: bool,
    /// Whether an upstream client exists at all.
    ///
    /// Named rather than folded into `status`, because "degraded" tells an operator to
    /// look and this tells them where.
    pub relay_available: bool,
    /// Which CCR store was built — `"memory"`, `"file"` or `"redis"`.
    ///
    /// Reported for the same reason as `upstream`, and discovered the same way. A
    /// `HEADROOM_CCR_DIR` the process cannot open falls back to memory and logs a
    /// warning; from then on the proxy relays, compresses, and hands the model
    /// `<<ccr:...>>` markers that will not survive a restart — while this endpoint said
    /// `"ok"` and named no store at all. Measured: put a value, rebuild the store from
    /// the same configuration, and it is gone.
    ///
    /// `"memory"` is the correct answer when nobody configured anything, so this field
    /// alone does not say whether something is wrong. `ccr_store_persistent` is what
    /// answers that.
    pub ccr_store: &'static str,
    /// Whether a marker written now is still redeemable after a restart.
    ///
    /// Separate from `ccr_store` because "memory" means two different things: the default
    /// nobody changed, and a configured store that failed to open. This is the field to
    /// alert on.
    pub ccr_store_persistent: bool,
}

impl Health {
    /// Builds the current health report.
    ///
    /// `relay_base` is where the built relay forwards, or `None` when none was built.
    /// `store` is the CCR store that was actually constructed. Both are taken as
    /// parameters rather than read from `config` so this cannot drift back to reporting
    /// what was asked for instead of what happened — the failure `upstream` records, and
    /// the one `ccr_store` was added for.
    pub fn current(
        config: &Config,
        relay_base: Option<&str>,
        store: crate::config::CcrStoreKind,
    ) -> Self {
        Self {
            status: if relay_base.is_some() {
                "ok"
            } else {
                "degraded"
            },
            version: env!("CARGO_PKG_VERSION"),
            upstream: relay_base.unwrap_or("none").to_owned(),
            compression_enabled: config.compression_enabled(),
            relay_available: relay_base.is_some(),
            ccr_store: store.as_str(),
            ccr_store_persistent: store.survives_restart(),
        }
    }

    /// The status code this report should be served with.
    ///
    /// A degraded proxy answers `503`, not `200`. Most orchestrators read the code and
    /// never look at the body, so reporting the problem only in JSON would be reporting
    /// it to nobody.
    pub fn status_code(&self) -> StatusCode {
        if self.relay_available {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

/// `GET /health`.
pub async fn health(State(state): State<AppState>) -> (StatusCode, Json<Health>) {
    let report = Health::current(
        &Config::from_env(),
        state.upstream_base(),
        state.ccr_store_kind(),
    );
    (report.status_code(), Json(report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CcrStoreKind;

    #[test]
    fn health_reports_the_relays_upstream_not_the_configured_one() {
        // The two disagree after `POST /admin/runtime-env` accepts a new
        // `HEADROOM_UPSTREAM`: the override lands in the map and nothing rebuilds the
        // relay. Reporting the configured value confirmed a change that had not
        // happened, so the fields are deliberately driven from different sources here.
        let config = Config::default();
        let health = Health::current(&config, Some("http://relay.example"), CcrStoreKind::Memory);

        assert_eq!(health.status, "ok");
        assert_eq!(health.upstream, "http://relay.example");
        assert_eq!(health.compression_enabled, config.compression_enabled());
        assert!(!health.version.is_empty());

        // Not vacuous: the configured upstream must actually differ, or reporting one
        // in place of the other would be indistinguishable.
        assert_ne!(
            config.upstream(),
            "http://relay.example",
            "the fixture matches the configured upstream, so this proves nothing"
        );
    }

    #[test]
    fn a_proxy_with_no_relay_names_no_upstream() {
        // Better than reporting a URL nothing will use. The status and code already say
        // degraded; this stops the body from contradicting them.
        let degraded = Health::current(&Config::default(), None, CcrStoreKind::Memory);

        assert_eq!(degraded.upstream, "none");
    }

    #[test]
    fn health_serializes_to_the_documented_shape() {
        let json = serde_json::to_value(Health::current(
            &Config::default(),
            Some("http://x"),
            CcrStoreKind::Memory,
        ))
        .unwrap();
        for key in [
            "status",
            "version",
            "upstream",
            "compression_enabled",
            "relay_available",
            "ccr_store",
            "ccr_store_persistent",
        ] {
            assert!(json.get(key).is_some(), "missing {key}");
        }
    }

    #[test]
    fn a_proxy_that_cannot_relay_does_not_report_ok() {
        // This endpoint used to answer `"ok"` with a `200` even when the upstream client
        // failed to build at startup — a state in which every request returns an error.
        //
        // Load balancers and orchestrators route traffic on this signal, so a proxy that
        // cannot serve a single request would keep being handed them, and the operator
        // debugging it would have a health check pointing them elsewhere.
        let degraded = Health::current(&Config::default(), None, CcrStoreKind::Memory);

        assert_eq!(degraded.status, "degraded");
        assert!(!degraded.relay_available);
        assert_eq!(degraded.status_code(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn a_working_proxy_still_reports_ok_with_a_200() {
        // The other half: a status that is always "degraded" is as useless as one that is
        // always "ok".
        let healthy = Health::current(&Config::default(), Some("http://x"), CcrStoreKind::Memory);

        assert_eq!(healthy.status, "ok");
        assert_eq!(healthy.status_code(), StatusCode::OK);
    }
}
