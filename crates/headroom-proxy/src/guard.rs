//! Operational guards: loop detection and rate limiting.
//!
//! Neither of these makes the proxy compress better. Both exist because the failure
//! they prevent is one the proxy is uniquely able to cause.
//!
//! # The loop
//!
//! Point `HEADROOM_UPSTREAM` at the proxy's own listen address and every request
//! forwards to itself, forever. It is a plausible misconfiguration — the natural way to
//! chain two proxies is to set one's upstream to the other's address, and getting it
//! backwards is one transposition away — and the symptom is a machine that pins a core
//! and exhausts its file descriptors rather than an error anyone can read.
//!
//! ## Why the check is at startup rather than per request
//!
//! The usual way to catch a proxy loop is a hop-count header: add one on the way out,
//! refuse the request when it comes back too high. That is not available here.
//! [`crate::headers`] exists because a header revealing that a proxy is present is a
//! subscription-revocation hazard, and a loop-detection header is exactly such a
//! header. Trading a fingerprint leak on *every* request for detection of a
//! misconfiguration that a startup check already catches is not a trade worth making.
//!
//! # The rate limit
//!
//! Protects the *provider*, not this proxy. If something upstream of the proxy goes
//! into a retry loop, the proxy will faithfully relay every attempt with the customer's
//! credential attached — and the customer discovers it on a bill or a suspended
//! account.

use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Whether `upstream` points back at `listen`.
///
/// Compares host and port textually, and deliberately treats the loopback spellings as
/// equivalent: `localhost`, `127.0.0.1`, and `::1` all reach the same socket, so
/// checking only the literal string would catch the least likely spelling of the
/// mistake.
///
/// # Example
///
/// ```
/// use headroom_proxy::guard::is_self_referential;
/// use std::net::SocketAddr;
///
/// let listen: SocketAddr = "127.0.0.1:8787".parse().unwrap();
/// assert!(is_self_referential("http://localhost:8787", listen));
/// assert!(!is_self_referential("https://api.anthropic.com", listen));
/// ```
pub fn is_self_referential(upstream: &str, listen: SocketAddr) -> bool {
    let Some((host, port)) = split_authority(upstream) else {
        return false;
    };

    // A different port is a different service, whatever the host.
    if port != listen.port() {
        return false;
    }

    let listen_ip = listen.ip();

    // Bound to every interface, which is what `HEADROOM_HOST=0.0.0.0` means and what a
    // container deployment almost always sets. The proxy then answers on loopback as
    // well, so an upstream of `http://127.0.0.1:<same port>` is itself.
    //
    // This branch is the whole point of the commit that added it. Without it, `0.0.0.0`
    // fell through to the exact-string comparison below — `"127.0.0.1" == "0.0.0.0"`,
    // false — and the proxy started happily and relayed to itself. Measured: one request
    // came back `429` in 0.26s, the rate limiter catching roughly six hundred self-relays,
    // which is a confusing quota error in place of the clear startup refusal this
    // function exists to produce.
    //
    // Loopback spellings only. A machine's own routable address (`10.0.0.5`, say) is also
    // itself, and catching that needs interface enumeration — a syscall at startup whose
    // answer varies with network conditions, which is the same reason the DNS lookup
    // below is not attempted. Named as a limit rather than left to be discovered.
    if listen_ip.is_unspecified() {
        return is_loopback_host(host);
    }

    if !listen_ip.is_loopback() {
        // A specific bind address only collides with that exact address, or with a
        // hostname that resolves to it — which this deliberately does not attempt,
        // since a DNS lookup at startup would make the check fail differently
        // depending on network conditions.
        return host == listen_ip.to_string();
    }

    is_loopback_host(host)
}

/// Whether `host` names the loopback interface.
fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    matches!(host, "localhost" | "::1" | "0.0.0.0")
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

/// Splits `host` and `port` out of a URL, defaulting the port by scheme.
fn split_authority(url: &str) -> Option<(&str, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    // Credentials in a URL are legal and would otherwise be mistaken for the host.
    let authority = authority.rsplit('@').next()?;

    let default_port = match scheme {
        "https" => 443,
        "http" => 80,
        _ => return None,
    };

    // An IPv6 literal is bracketed, so the last colon only delimits a port when it
    // falls outside the brackets.
    if let Some(close) = authority.rfind(']') {
        let (host, tail) = authority.split_at(close + 1);
        let port = tail
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(default_port);
        return Some((host, port));
    }

    match authority.rsplit_once(':') {
        Some((host, port)) => Some((host, port.parse().ok()?)),
        None => Some((authority, default_port)),
    }
}

/// A token bucket over a fixed window.
///
/// Deliberately process-wide rather than per client. The proxy binds loopback by
/// default and fronts one customer's credential; the thing worth limiting is the total
/// rate reaching the provider, not any one caller's share of it.
#[derive(Debug)]
pub struct RateLimiter {
    state: Mutex<Bucket>,
    capacity: u32,
    refill: Duration,
}

#[derive(Debug)]
struct Bucket {
    tokens: u32,
    last_refill: Instant,
}

impl RateLimiter {
    /// Allows `capacity` requests per `refill` window.
    pub fn new(capacity: u32, refill: Duration) -> Self {
        Self {
            state: Mutex::new(Bucket {
                tokens: capacity,
                last_refill: Instant::now(),
            }),
            capacity,
            refill,
        }
    }

    /// Takes a token, returning whether the request may proceed.
    ///
    /// # Fails open
    ///
    /// A poisoned lock — another thread panicked mid-update — permits the request.
    /// The limiter exists to catch a runaway loop, and a limiter that starts rejecting
    /// live traffic because of an unrelated panic has become the outage it was meant to
    /// prevent.
    pub fn allow(&self) -> bool {
        let Ok(mut bucket) = self.state.lock() else {
            return true;
        };

        self.refill_if_due(&mut bucket);

        if bucket.tokens == 0 {
            return false;
        }
        bucket.tokens -= 1;
        true
    }

    /// Tokens currently available.
    ///
    /// Refills first, deliberately. A reader that reported the stored count would say
    /// "0 tokens left" for a bucket whose window elapsed an hour ago and which will
    /// admit the very next request — a gauge that is wrong exactly when someone is
    /// looking at it to find out whether the limiter is the problem.
    pub fn available(&self) -> u32 {
        let Ok(mut bucket) = self.state.lock() else {
            return self.capacity;
        };
        self.refill_if_due(&mut bucket);
        bucket.tokens
    }

    /// Restores the bucket if at least one whole window has passed.
    fn refill_if_due(&self, bucket: &mut Bucket) {
        let elapsed = bucket.last_refill.elapsed();
        if elapsed < self.refill {
            return;
        }

        // Whole windows only, and the refill is a *reset* to capacity rather than an
        // addition. Adding a window's worth per elapsed window would let an idle
        // minute become a sixty-fold burst allowance the moment traffic resumed —
        // which is precisely the runaway this limiter exists to bound.
        let windows = (elapsed.as_nanos() / self.refill.as_nanos().max(1)) as u32;
        bucket.tokens = self.capacity;
        bucket.last_refill += self.refill * windows.max(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listen(addr: &str) -> SocketAddr {
        addr.parse().unwrap()
    }

    // ---- loop detection ----

    #[test]
    fn every_loopback_spelling_of_the_same_socket_is_caught() {
        // Checking only the literal string would catch the least likely spelling of
        // the mistake and miss the ones people actually type.
        let listen = listen("127.0.0.1:8787");
        for upstream in [
            "http://localhost:8787",
            "http://127.0.0.1:8787",
            "http://[::1]:8787",
            "http://127.0.0.1:8787/",
            "http://127.0.0.2:8787",
        ] {
            assert!(
                is_self_referential(upstream, listen),
                "{upstream} should have been caught"
            );
        }
    }

    #[test]
    fn a_real_provider_is_not_mistaken_for_the_proxy() {
        let listen = listen("127.0.0.1:8787");
        for upstream in [
            "https://api.anthropic.com",
            "https://api.openai.com/v1",
            "http://localhost:9999",
            "http://192.168.1.10:8787",
        ] {
            assert!(
                !is_self_referential(upstream, listen),
                "{upstream} is not the proxy"
            );
        }
    }

    #[test]
    fn a_different_port_on_the_same_host_is_a_different_service() {
        // Chaining two proxies on one machine is legitimate and must keep working.
        assert!(!is_self_referential(
            "http://localhost:8788",
            listen("127.0.0.1:8787")
        ));
    }

    #[test]
    fn the_default_port_is_taken_from_the_scheme() {
        // `https://example.com` is port 443, and a proxy listening on 443 would loop.
        assert!(is_self_referential(
            "https://localhost",
            listen("127.0.0.1:443")
        ));
        assert!(!is_self_referential(
            "https://localhost",
            listen("127.0.0.1:80")
        ));
        assert!(is_self_referential(
            "http://localhost",
            listen("127.0.0.1:80")
        ));
    }

    #[test]
    fn credentials_in_the_url_are_not_mistaken_for_the_host() {
        assert!(is_self_referential(
            "http://user:pass@localhost:8787",
            listen("127.0.0.1:8787")
        ));
    }

    #[test]
    fn a_non_loopback_bind_only_collides_with_its_own_address() {
        let listen = listen("192.168.1.10:8787");
        assert!(is_self_referential("http://192.168.1.10:8787", listen));
        assert!(
            !is_self_referential("http://localhost:8787", listen),
            "a specific bind does not answer on loopback"
        );
    }

    #[test]
    fn a_bind_to_every_interface_collides_with_loopback() {
        // `HEADROOM_HOST=0.0.0.0` is what a container deployment sets, and it means the
        // proxy answers on loopback too — so `http://127.0.0.1:<same port>` is itself.
        //
        // This fell through to the exact-string comparison for a specific bind:
        // `"127.0.0.1" == "0.0.0.0"`, false. The proxy started and relayed to itself.
        // Measured before the fix: one request returned 429 in 0.26s, the rate limiter
        // catching roughly six hundred self-relays — a confusing quota error instead of
        // the clear startup refusal this function exists to produce.
        for listen_addr in ["0.0.0.0:8787", "[::]:8787"] {
            let listen = listen(listen_addr);
            for upstream in [
                "http://127.0.0.1:8787",
                "http://localhost:8787",
                "http://[::1]:8787",
                "http://0.0.0.0:8787",
            ] {
                assert!(
                    is_self_referential(upstream, listen),
                    "{upstream} against {listen_addr}"
                );
            }

            // And the check still lets a real deployment start. A guard that refuses
            // everything is as useless as one that refuses nothing.
            assert!(!is_self_referential("https://api.anthropic.com", listen));
            assert!(
                !is_self_referential("http://127.0.0.1:9999", listen),
                "a different port is a different service, even bound to everything"
            );
        }
    }

    #[test]
    fn an_unparseable_upstream_is_not_reported_as_a_loop() {
        // Wrong in the safe direction. A false positive here refuses to start a proxy
        // that would have worked; a false negative costs one spun core on a
        // misconfiguration the operator can see in the logs.
        for upstream in ["", "not a url", "ftp://localhost:8787", "localhost:8787"] {
            assert!(
                !is_self_referential(upstream, listen("127.0.0.1:8787")),
                "{upstream:?}"
            );
        }
    }

    // ---- rate limiting ----

    #[test]
    fn requests_are_allowed_up_to_the_capacity_and_then_refused() {
        let limiter = RateLimiter::new(3, Duration::from_secs(60));
        assert!(limiter.allow());
        assert!(limiter.allow());
        assert!(limiter.allow());
        assert!(!limiter.allow(), "the fourth exceeded a capacity of three");
    }

    #[test]
    fn the_bucket_refills_after_its_window() {
        let limiter = RateLimiter::new(1, Duration::from_millis(1));
        assert!(limiter.allow());
        assert!(!limiter.allow());

        std::thread::sleep(Duration::from_millis(5));
        assert!(limiter.allow(), "the window elapsed and did not refill");
    }

    #[test]
    fn a_refill_does_not_exceed_the_capacity() {
        // Several windows elapsing at once must not stack into a burst allowance far
        // above the configured rate.
        let limiter = RateLimiter::new(2, Duration::from_millis(1));
        assert!(limiter.allow());
        std::thread::sleep(Duration::from_millis(20));

        assert_eq!(limiter.available(), 2);
        assert!(limiter.allow());
        assert!(limiter.allow());
        assert!(!limiter.allow(), "twenty windows became twenty tokens");
    }

    #[test]
    fn concurrent_callers_do_not_exceed_the_capacity_between_them() {
        // The counter is shared, so the limit is on the total rate reaching the
        // provider rather than on any one thread's share of it.
        let limiter = Arc::new(RateLimiter::new(50, Duration::from_secs(60)));
        let allowed = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let limiter = limiter.clone();
                let allowed = allowed.clone();
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        if limiter.allow() {
                            allowed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(
            allowed.load(std::sync::atomic::Ordering::Relaxed),
            50,
            "200 attempts against a capacity of 50"
        );
    }

    use std::sync::Arc;
}
