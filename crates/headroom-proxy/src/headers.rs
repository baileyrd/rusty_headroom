//! Header hygiene for upstream-bound requests.
//!
//! The reference classifies leaking `X-Headroom-*` headers to a provider as a
//! **fingerprint-class, subscription-revocation risk**. A provider that can see a
//! proxy sitting in front of a subscription CLI is in a position to act on that. So
//! headers the proxy uses internally must not reach upstream — not as a tidiness
//! measure, but because their presence is itself the disclosure.
//!
//! The same reasoning drives leaving `User-Agent` completely alone. Appending
//! `headroom/0.1` to it would be polite and would announce exactly what must not be
//! announced.

use std::fmt;

use http::header::{HeaderMap, HeaderName, HeaderValue};

/// Header-name prefix reserved for the proxy's own use.
const INTERNAL_PREFIX: &str = "x-headroom-";

/// Headers that describe a single transport hop and must not be forwarded.
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// How many characters of an `Authorization` value may appear in logs.
const AUTH_VISIBLE_CHARS: usize = 12;

/// What the proxy is permitted to add to an upstream request.
///
/// Supplied by the caller rather than derived here. Auth-mode classification is its
/// own concern; keeping it out means the classifier can land later without this
/// module changing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HeaderPolicy {
    /// Whether `X-Forwarded-*` may be added.
    ///
    /// Off for subscription-mode traffic: those headers announce that a proxy is
    /// present, which is the disclosure this module exists to prevent.
    ///
    /// Defaults to `false`, and the direction is deliberate: a policy nobody has
    /// thought about should not be the one that discloses the most. The derived
    /// `Default` gives that for free — `derivable_impls` is right that a hand-written
    /// impl would add nothing but a place for the two to drift apart.
    pub forwarded_headers: bool,
}

impl HeaderPolicy {
    /// Permits `X-Forwarded-*`.
    pub fn with_forwarded() -> Self {
        Self {
            forwarded_headers: true,
        }
    }
}

/// Builds the header map to send upstream from the one the client sent.
///
/// Removes the proxy's own headers and hop-by-hop headers. Everything else —
/// including `Authorization`, `User-Agent`, and every `anthropic-*` header — is
/// forwarded byte-for-byte in its original order.
///
/// # Example
///
/// ```
/// use headroom_proxy::headers::{sanitize, HeaderPolicy};
/// use http::header::{HeaderMap, HeaderValue};
///
/// let mut incoming = HeaderMap::new();
/// incoming.insert("X-Headroom-Debug", HeaderValue::from_static("1"));
/// incoming.insert("user-agent", HeaderValue::from_static("claude-cli/1.2"));
///
/// let outgoing = sanitize(&incoming, HeaderPolicy::default());
/// assert!(outgoing.get("x-headroom-debug").is_none());
/// assert_eq!(outgoing.get("user-agent").unwrap(), "claude-cli/1.2");
/// ```
pub fn sanitize(incoming: &HeaderMap, policy: HeaderPolicy) -> HeaderMap {
    let mut outgoing = HeaderMap::with_capacity(incoming.len());

    for (name, value) in incoming {
        if is_internal(name) || is_hop_by_hop(name) {
            continue;
        }
        outgoing.append(name.clone(), value.clone());
    }

    if !policy.forwarded_headers {
        // Strip any the client happened to send too. Forwarding a client-supplied
        // `X-Forwarded-For` under a policy that forbids adding one would defeat the
        // policy while technically honoring it.
        for name in ["x-forwarded-for", "x-forwarded-host", "x-forwarded-proto"] {
            outgoing.remove(name);
        }
    }

    outgoing
}

/// Whether `name` belongs to the proxy's internal namespace.
///
/// Compared against the lowercase form. `HeaderName` normalizes on construction, but
/// relying on that implicitly is how a case-sensitive check survives review and then
/// leaks `X-Headroom-Foo`.
fn is_internal(name: &HeaderName) -> bool {
    name.as_str()
        .to_ascii_lowercase()
        .starts_with(INTERNAL_PREFIX)
}

/// Whether `name` describes a single transport hop.
fn is_hop_by_hop(name: &HeaderName) -> bool {
    let lowered = name.as_str().to_ascii_lowercase();
    HOP_BY_HOP.contains(&lowered.as_str())
}

/// An `Authorization` value that cannot be logged in full.
///
/// Wrapping rather than remembering: a bare `String` in a struct will eventually be
/// caught by a `{:?}` on the struct that contains it, on an error path nobody
/// exercised. This type has no rendering that reveals the whole secret, so there is
/// no path — happy or otherwise — that leaks it.
#[derive(Clone, PartialEq, Eq)]
pub struct Redacted(String);

impl Redacted {
    /// Wraps a credential.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The visible prefix, enough to correlate requests without exposing the secret.
    pub fn prefix(&self) -> &str {
        let end = self
            .0
            .char_indices()
            .nth(AUTH_VISIBLE_CHARS)
            .map_or(self.0.len(), |(index, _)| index);
        &self.0[..end]
    }
}

impl fmt::Display for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}...", self.prefix())
    }
}

impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Debug and Display agree deliberately. A `Debug` that dumped the full value
        // would make `tracing::debug!(?headers)` a credential leak.
        write!(f, "Redacted({}...)", self.prefix())
    }
}

/// Extracts the `Authorization` header in a form safe to log.
pub fn redacted_authorization(headers: &HeaderMap) -> Option<Redacted> {
    headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(Redacted::new)
}

/// Adds `X-Forwarded-*`, when the policy allows it.
pub fn apply_forwarded(headers: &mut HeaderMap, policy: HeaderPolicy, client_host: &str) {
    if !policy.forwarded_headers {
        return;
    }
    if let Ok(value) = HeaderValue::from_str(client_host) {
        headers.insert("x-forwarded-for", value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn internal_headers_are_stripped_whatever_their_case() {
        // A case-sensitive check would leak `X-Headroom-Foo`, which is the same
        // header as the one it strips.
        let incoming = headers(&[
            ("x-headroom-debug", "1"),
            ("X-Headroom-Session", "abc"),
            ("X-HEADROOM-MODE", "payg"),
            ("x-other", "keep"),
        ]);
        let outgoing = sanitize(&incoming, HeaderPolicy::default());

        for name in ["x-headroom-debug", "x-headroom-session", "x-headroom-mode"] {
            assert!(outgoing.get(name).is_none(), "{name} leaked");
        }
        assert_eq!(outgoing.get("x-other").unwrap(), "keep");
    }

    #[test]
    fn the_user_agent_passes_through_verbatim() {
        // Appending `headroom/0.1` would be polite and would announce exactly what
        // must not be announced.
        let incoming = headers(&[("user-agent", "claude-cli/1.2.3 (darwin; arm64)")]);
        let outgoing = sanitize(&incoming, HeaderPolicy::default());
        assert_eq!(
            outgoing.get("user-agent").unwrap(),
            "claude-cli/1.2.3 (darwin; arm64)"
        );
    }

    #[test]
    fn provider_headers_are_forwarded_untouched() {
        let incoming = headers(&[
            ("authorization", "Bearer sk-ant-secret"),
            ("anthropic-version", "2023-06-01"),
            ("anthropic-beta", "prompt-caching-2024-07-31"),
            ("content-type", "application/json"),
        ]);
        let outgoing = sanitize(&incoming, HeaderPolicy::default());

        assert_eq!(
            outgoing.get("authorization").unwrap(),
            "Bearer sk-ant-secret"
        );
        assert_eq!(outgoing.get("anthropic-version").unwrap(), "2023-06-01");
        assert_eq!(
            outgoing.get("anthropic-beta").unwrap(),
            "prompt-caching-2024-07-31"
        );
    }

    #[test]
    fn hop_by_hop_headers_are_dropped() {
        let incoming = headers(&[
            ("connection", "keep-alive"),
            ("Transfer-Encoding", "chunked"),
            ("keep-alive", "timeout=5"),
            ("content-type", "application/json"),
        ]);
        let outgoing = sanitize(&incoming, HeaderPolicy::default());

        assert!(outgoing.get("connection").is_none());
        assert!(outgoing.get("transfer-encoding").is_none());
        assert!(outgoing.get("keep-alive").is_none());
        assert!(outgoing.get("content-type").is_some());
    }

    #[test]
    fn forwarded_headers_are_absent_under_the_default_policy() {
        let mut outgoing = sanitize(&HeaderMap::new(), HeaderPolicy::default());
        apply_forwarded(&mut outgoing, HeaderPolicy::default(), "203.0.113.5");
        assert!(outgoing.get("x-forwarded-for").is_none());
    }

    #[test]
    fn a_client_supplied_forwarded_header_is_stripped_when_the_policy_forbids_them() {
        // Forwarding one the client sent would defeat the policy while technically
        // honoring "do not add".
        let incoming = headers(&[("x-forwarded-for", "198.51.100.9")]);
        let outgoing = sanitize(&incoming, HeaderPolicy::default());
        assert!(outgoing.get("x-forwarded-for").is_none());
    }

    #[test]
    fn forwarded_headers_are_added_when_the_policy_allows() {
        let mut outgoing = sanitize(&HeaderMap::new(), HeaderPolicy::with_forwarded());
        apply_forwarded(&mut outgoing, HeaderPolicy::with_forwarded(), "203.0.113.5");
        assert_eq!(outgoing.get("x-forwarded-for").unwrap(), "203.0.113.5");
    }

    #[test]
    fn the_default_policy_is_the_quiet_one() {
        assert!(!HeaderPolicy::default().forwarded_headers);
    }

    // ---- redaction ----

    #[test]
    fn a_credential_never_renders_in_full() {
        let secret = "Bearer sk-ant-api03-THIS-IS-THE-SECRET-PART";
        let redacted = Redacted::new(secret);

        for rendered in [format!("{redacted}"), format!("{redacted:?}")] {
            assert!(
                !rendered.contains("THIS-IS-THE-SECRET-PART"),
                "secret leaked in {rendered}"
            );
            assert!(rendered.contains("Bearer sk-an"), "no usable prefix");
        }
    }

    #[test]
    fn debug_and_display_agree_so_a_struct_dump_cannot_leak() {
        // `tracing::debug!(?value)` uses Debug. A Debug that dumped the full secret
        // would make any struct containing this type a leak waiting for an error
        // path nobody exercised.
        let redacted = Redacted::new("Bearer sk-ant-secret-material");
        assert!(!format!("{redacted:?}").contains("secret-material"));
    }

    #[test]
    fn redaction_handles_short_and_multibyte_values() {
        // Slicing by byte offset would panic mid-codepoint.
        assert_eq!(Redacted::new("short").prefix(), "short");
        assert_eq!(Redacted::new("").prefix(), "");
        let unicode = Redacted::new("Bearer 日本語のトークンです");
        assert!(!format!("{unicode}").is_empty());
    }

    #[test]
    fn authorization_is_extracted_in_redacted_form() {
        let incoming = headers(&[("authorization", "Bearer sk-ant-api03-longsecret")]);
        let redacted = redacted_authorization(&incoming).expect("present");
        assert!(!format!("{redacted:?}").contains("longsecret"));
        assert!(redacted_authorization(&HeaderMap::new()).is_none());
    }

    #[test]
    fn repeated_headers_all_survive() {
        // `Set-Cookie` and friends can appear more than once; collapsing them would
        // silently drop data.
        let mut incoming = HeaderMap::new();
        incoming.append("accept", HeaderValue::from_static("text/event-stream"));
        incoming.append("accept", HeaderValue::from_static("application/json"));

        let outgoing = sanitize(&incoming, HeaderPolicy::default());
        assert_eq!(outgoing.get_all("accept").iter().count(), 2);
    }
}
