//! The health endpoint.

use axum::Json;
use serde::Serialize;

use crate::config::Config;

/// What `/health` reports.
///
/// More than a bare `200`, deliberately. An operator hitting this during an incident
/// wants to know *which* proxy answered and what it currently believes its
/// configuration to be — a liveness check that only proves the socket is open leaves
/// them no better off.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Health {
    /// Always `"ok"` when the server can answer at all.
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
}

impl Health {
    /// Builds the current health report.
    pub fn current(config: &Config) -> Self {
        Self {
            status: "ok",
            version: env!("CARGO_PKG_VERSION"),
            upstream: config.upstream().to_owned(),
            compression_enabled: config.compression_enabled(),
        }
    }
}

/// `GET /health`.
pub async fn health() -> Json<Health> {
    Json(Health::current(&Config::from_env()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_reports_the_live_configuration() {
        let config = Config::default();
        let health = Health::current(&config);

        assert_eq!(health.status, "ok");
        assert_eq!(health.upstream, config.upstream());
        assert_eq!(health.compression_enabled, config.compression_enabled());
        assert!(!health.version.is_empty());
    }

    #[test]
    fn health_serializes_to_the_documented_shape() {
        let json = serde_json::to_value(Health::current(&Config::default())).unwrap();
        for key in ["status", "version", "upstream", "compression_enabled"] {
            assert!(json.get(key).is_some(), "missing {key}");
        }
    }
}
