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
}

impl Health {
    /// Builds the current health report.
    pub fn current(config: &Config, relay_available: bool) -> Self {
        Self {
            status: if relay_available { "ok" } else { "degraded" },
            version: env!("CARGO_PKG_VERSION"),
            upstream: config.upstream().to_owned(),
            compression_enabled: config.compression_enabled(),
            relay_available,
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
    let report = Health::current(&Config::from_env(), state.relay_available());
    (report.status_code(), Json(report))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_reports_the_live_configuration() {
        let config = Config::default();
        let health = Health::current(&config, true);

        assert_eq!(health.status, "ok");
        assert_eq!(health.upstream, config.upstream());
        assert_eq!(health.compression_enabled, config.compression_enabled());
        assert!(!health.version.is_empty());
    }

    #[test]
    fn health_serializes_to_the_documented_shape() {
        let json = serde_json::to_value(Health::current(&Config::default(), true)).unwrap();
        for key in [
            "status",
            "version",
            "upstream",
            "compression_enabled",
            "relay_available",
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
        let degraded = Health::current(&Config::default(), false);

        assert_eq!(degraded.status, "degraded");
        assert!(!degraded.relay_available);
        assert_eq!(degraded.status_code(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn a_working_proxy_still_reports_ok_with_a_200() {
        // The other half: a status that is always "degraded" is as useless as one that is
        // always "ok".
        let healthy = Health::current(&Config::default(), true);

        assert_eq!(healthy.status, "ok");
        assert_eq!(healthy.status_code(), StatusCode::OK);
    }
}
