//! Auth-mode classification and the compression policy it gates — invariant I10.
//!
//! Not every request may be compressed the same way, and the reason is not
//! performance. A subscription CLI authenticates differently from a pay-as-you-go API
//! key, and for subscription traffic the presence of a proxy is itself a risk: the
//! reference classifies proxy-revealing modifications as a
//! subscription-revocation hazard.
//!
//! So the aggressiveness of compression is decided by *how the request authenticated*,
//! and the classification is deliberately conservative — an unrecognized shape is
//! treated as the most restricted mode, not the least.

use http::header::HeaderMap;

/// How a request authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// A plain API key. The customer pays per token and has no account state to risk.
    PayAsYouGo,
    /// An OAuth token. Scope-bound; modifications could void the grant.
    OAuth,
    /// A subscription CLI. The most restricted mode.
    Subscription,
}

impl AuthMode {
    /// A stable identifier for telemetry.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PayAsYouGo => "payg",
            Self::OAuth => "oauth",
            Self::Subscription => "subscription",
        }
    }
}

/// Classifies a request by its headers.
///
/// # Conservative by default
///
/// Anything unrecognized classifies as [`AuthMode::Subscription`], the most restricted
/// mode. The two directions are not symmetric: misclassifying subscription traffic as
/// pay-as-you-go applies aggressive compression to the account that can least afford
/// the exposure, while the reverse merely leaves some tokens uncompressed.
///
/// # Example
///
/// ```
/// use headroom_core::auth_mode::{classify_auth_mode, AuthMode};
/// use http::header::{HeaderMap, HeaderValue};
///
/// let mut headers = HeaderMap::new();
/// headers.insert("x-api-key", HeaderValue::from_static("sk-ant-api03-xxx"));
/// assert_eq!(classify_auth_mode(&headers), AuthMode::PayAsYouGo);
///
/// // Nothing recognizable falls to the most restricted mode.
/// assert_eq!(classify_auth_mode(&HeaderMap::new()), AuthMode::Subscription);
/// ```
pub fn classify_auth_mode(headers: &HeaderMap) -> AuthMode {
    // A direct API key is the unambiguous pay-as-you-go signal.
    if headers.contains_key("x-api-key") {
        return AuthMode::PayAsYouGo;
    }

    let Some(authorization) = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return AuthMode::Subscription;
    };

    let token = authorization
        .strip_prefix("Bearer ")
        .or_else(|| authorization.strip_prefix("bearer "))
        .unwrap_or(authorization)
        .trim();

    // Specific prefixes before general ones, and this ordering is load-bearing rather
    // than stylistic. An OAuth token `sk-ant-oat...` also starts with `sk-`, so a
    // generic `sk-` test placed first swallows it and hands OAuth traffic the
    // aggressive pay-as-you-go policy — a misclassification in the one direction that
    // matters.
    if token.starts_with("sk-ant-oat") || token.starts_with("ya29.") {
        AuthMode::OAuth
    } else if token.starts_with("sk-ant-api") || token.starts_with("sk-") {
        AuthMode::PayAsYouGo
    } else {
        // A bearer token this code does not recognize. Could be anything, including a
        // subscription session token, so it gets the restricted treatment.
        AuthMode::Subscription
    }
}

/// What compression is permitted for a given auth mode.
///
/// Every field is phrased as a permission rather than a prohibition, so the
/// most-restrictive policy is the one where everything is `false` — which is also what
/// [`Default`] gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CompressionPolicy {
    /// Whether lossy transforms may run.
    ///
    /// Off outside pay-as-you-go. Lossy compression visibly rewrites content, and on
    /// subscription traffic that is the disclosure to avoid.
    pub lossy_transforms: bool,
    /// Whether `cache_control` breakpoints may be placed automatically.
    ///
    /// Off outside pay-as-you-go: an injected breakpoint on OAuth traffic could fall
    /// outside the granted scope.
    pub auto_cache_control: bool,
    /// Whether `prompt_cache_key` may be injected on OpenAI-shaped requests.
    pub auto_prompt_cache_key: bool,
    /// Whether `X-Forwarded-*` may be added.
    pub forwarded_headers: bool,
    /// Whether `accept-encoding` may be stripped.
    ///
    /// Preserved on subscription traffic: the header a client sends is part of how it
    /// looks, and normalizing it is a fingerprint change.
    pub may_strip_accept_encoding: bool,
}

impl CompressionPolicy {
    /// The policy for `mode`.
    pub fn for_mode(mode: AuthMode) -> Self {
        match mode {
            AuthMode::PayAsYouGo => Self {
                lossy_transforms: true,
                auto_cache_control: true,
                auto_prompt_cache_key: true,
                forwarded_headers: true,
                may_strip_accept_encoding: true,
            },
            AuthMode::OAuth => Self {
                // Lossless only, and no automatic markers that could fall outside the
                // granted scope.
                lossy_transforms: false,
                auto_cache_control: false,
                auto_prompt_cache_key: false,
                forwarded_headers: true,
                may_strip_accept_encoding: true,
            },
            AuthMode::Subscription => Self::default(),
        }
    }

    /// Whether any compression at all is permitted.
    ///
    /// Lossless transforms run on every mode, so this is always `true`. It exists so
    /// callers do not infer "subscription means no compression" from the all-false
    /// policy — the restriction is on *lossy* work and on proxy-revealing changes.
    pub fn compression_permitted(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn an_api_key_header_is_pay_as_you_go() {
        assert_eq!(
            classify_auth_mode(&headers(&[("x-api-key", "sk-ant-api03-xxx")])),
            AuthMode::PayAsYouGo
        );
    }

    #[test]
    fn an_api_key_bearer_token_is_pay_as_you_go() {
        assert_eq!(
            classify_auth_mode(&headers(&[("authorization", "Bearer sk-ant-api03-xxx")])),
            AuthMode::PayAsYouGo
        );
    }

    #[test]
    fn an_oauth_token_is_recognized() {
        assert_eq!(
            classify_auth_mode(&headers(&[("authorization", "Bearer sk-ant-oat01-xxx")])),
            AuthMode::OAuth
        );
    }

    #[test]
    fn anything_unrecognized_falls_to_the_most_restricted_mode() {
        // The direction that matters. Misclassifying subscription traffic as
        // pay-as-you-go applies aggressive compression to the account least able to
        // afford the exposure; the reverse just leaves tokens uncompressed.
        for case in [
            vec![],
            vec![("authorization", "Bearer some-opaque-session-token")],
            vec![("authorization", "Basic dXNlcjpwYXNz")],
            vec![("content-type", "application/json")],
        ] {
            assert_eq!(
                classify_auth_mode(&headers(&case)),
                AuthMode::Subscription,
                "{case:?} should be restricted"
            );
        }
    }

    #[test]
    fn a_non_utf8_authorization_value_does_not_panic() {
        let mut map = HeaderMap::new();
        map.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        assert_eq!(classify_auth_mode(&map), AuthMode::Subscription);
    }

    #[test]
    fn an_oauth_token_is_not_swallowed_by_the_generic_api_key_prefix() {
        // Regression. `sk-ant-oat...` also starts with `sk-`, so testing the generic
        // prefix first classified OAuth traffic as pay-as-you-go and handed it the
        // aggressive policy — the misclassification direction that actually matters.
        let oauth = classify_auth_mode(&headers(&[("authorization", "Bearer sk-ant-oat01-xxx")]));
        assert_eq!(oauth, AuthMode::OAuth);
        assert!(!CompressionPolicy::for_mode(oauth).lossy_transforms);
    }

    #[test]
    fn classification_is_deterministic() {
        let map = headers(&[("authorization", "Bearer sk-ant-api03-xxx")]);
        let first = classify_auth_mode(&map);
        for _ in 0..25 {
            assert_eq!(classify_auth_mode(&map), first);
        }
    }

    // ---- policy ----

    #[test]
    fn pay_as_you_go_permits_everything() {
        let policy = CompressionPolicy::for_mode(AuthMode::PayAsYouGo);
        assert!(policy.lossy_transforms);
        assert!(policy.auto_cache_control);
        assert!(policy.auto_prompt_cache_key);
    }

    #[test]
    fn oauth_is_lossless_only_and_places_no_markers() {
        let policy = CompressionPolicy::for_mode(AuthMode::OAuth);
        assert!(!policy.lossy_transforms);
        assert!(!policy.auto_cache_control, "could fall outside the grant");
        assert!(!policy.auto_prompt_cache_key);
    }

    #[test]
    fn subscription_permits_nothing_beyond_lossless() {
        let policy = CompressionPolicy::for_mode(AuthMode::Subscription);
        assert_eq!(policy, CompressionPolicy::default());
        assert!(!policy.lossy_transforms);
        assert!(!policy.forwarded_headers);
        assert!(
            !policy.may_strip_accept_encoding,
            "accept-encoding is part of how a client looks"
        );
    }

    #[test]
    fn the_default_policy_is_the_most_restrictive_one() {
        // Every field is a permission, so all-false is the safe default and matches
        // subscription mode. A policy nobody configured should not be the permissive
        // one.
        assert_eq!(
            CompressionPolicy::default(),
            CompressionPolicy::for_mode(AuthMode::Subscription)
        );
    }

    #[test]
    fn lossless_compression_is_permitted_on_every_mode() {
        // "Subscription" restricts lossy work and proxy-revealing changes, not
        // compression as such.
        for mode in [
            AuthMode::PayAsYouGo,
            AuthMode::OAuth,
            AuthMode::Subscription,
        ] {
            assert!(CompressionPolicy::for_mode(mode).compression_permitted());
        }
    }
}
