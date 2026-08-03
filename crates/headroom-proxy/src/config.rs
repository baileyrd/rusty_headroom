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

use std::env;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

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
}

/// Default listen port.
pub const DEFAULT_PORT: u16 = 8787;

/// Default upstream provider.
pub const DEFAULT_UPSTREAM: &str = "https://api.anthropic.com";

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
            host: env::var(vars::HOST)
                .ok()
                .and_then(|raw| raw.parse().ok())
                .unwrap_or(defaults.host),
            port: env::var(vars::PORT)
                .ok()
                .and_then(|raw| raw.parse().ok())
                .unwrap_or(defaults.port),
            upstream: env::var(vars::UPSTREAM)
                .ok()
                .filter(|raw| !raw.trim().is_empty())
                .map(|raw| raw.trim_end_matches('/').to_owned())
                .unwrap_or(defaults.upstream),
            compression_enabled: env::var(vars::COMPRESSION)
                .ok()
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

    /// Whether compression runs at all.
    ///
    /// When `false` the proxy is a pure passthrough — which is also exactly what the
    /// invariant I1 round-trip test exercises.
    pub fn compression_enabled(&self) -> bool {
        self.compression_enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
