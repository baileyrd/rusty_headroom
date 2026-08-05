//! `POST /admin/runtime-env` — retune a running proxy without restarting it.
//!
//! Restarting is not free here. The proxy sits in the middle of streaming responses,
//! and a restart truncates every one in flight — which reaches a user as a corrupt
//! answer rather than as an error they can retry. Turning compression off during an
//! incident should not cost that.
//!
//! # Why this endpoint is gated on the peer address
//!
//! It can change `HEADROOM_UPSTREAM`. Anyone who can reach it can therefore point the
//! proxy at a server they control, and every subsequent request carries the customer's
//! provider credential to it. That makes an unauthenticated admin endpoint a
//! credential-exfiltration primitive, not merely a configuration surface.
//!
//! The proxy binds loopback by default, which makes that unreachable — but the bind
//! address is configurable, and a control that only holds under the default
//! configuration is not a control. So the handler checks the peer address itself and
//! refuses anything that is not loopback, whatever the proxy is bound to.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Request};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;

use crate::config;
use crate::guard::is_self_referential;

/// Largest body this endpoint will read.
///
/// A configuration object is a handful of short strings; 64 KiB is generous by three
/// orders of magnitude and still bounded.
const MAX_BODY_BYTES: usize = 64 * 1024;

/// Applies runtime configuration overrides.
///
/// The body is a flat object of `HEADROOM_*` names to string values. Names outside
/// that namespace, or within it but not one of [`crate::config::KNOWN`] (a typo, or a
/// variable no code in this crate reads), are ignored rather than rejected wholesale,
/// so a caller sending one bad key still gets the rest applied — and the response says
/// exactly which were taken, so "ignored" is never something the caller has to infer.
pub async fn runtime_env(request: Request) -> Response {
    // Read straight from the extensions rather than through an `Option<ConnectInfo>`
    // extractor, which axum will not build: `ConnectInfo` implements
    // `FromRequestParts` but not its optional counterpart, so the extractor form
    // rejects the request before this function can decide what to do about it — and
    // deciding is the whole job here.
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0);

    // Absent connection info means the server was not built with
    // `into_make_service_with_connect_info`. Refusing is the only safe reading: the
    // handler cannot establish that the caller is local, and this endpoint's entire
    // protection is that it can.
    let Some(peer) = peer else {
        return refuse("connection information unavailable; refusing to apply overrides");
    };

    if !peer.ip().is_loopback() {
        tracing::warn!(
            peer = %peer.ip(),
            "refused a non-local runtime-env request"
        );
        return refuse("runtime-env may only be set from the local host");
    }

    // Bounded. A configuration object is a handful of short strings, and an unbounded
    // read on an endpoint that changes process behavior is a way to exhaust memory
    // without ever sending anything valid.
    let Ok(bytes) = axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES).await else {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({
                "error": format!("body exceeds {MAX_BODY_BYTES} bytes"),
            })),
        )
            .into_response();
    };

    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "body is not valid JSON" })),
        )
            .into_response();
    };

    let Some(object) = body.as_object() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "expected a JSON object of HEADROOM_* names to string values",
            })),
        )
            .into_response();
    };

    let requested: BTreeMap<String, String> = object
        .iter()
        .filter_map(|(name, value)| {
            // Numbers and booleans are accepted and stringified, because an operator
            // typing `{"HEADROOM_PORT": 8788}` means the obvious thing and a 400 here
            // would be pedantry during an incident.
            let value = match value {
                Value::String(text) => text.clone(),
                Value::Number(number) => number.to_string(),
                Value::Bool(flag) => flag.to_string(),
                _ => return None,
            };
            Some((name.clone(), value))
        })
        .collect();

    // The same guard `serve` runs at startup, re-run here.
    //
    // D11 justified having no per-request loop-detection header on the grounds that a
    // startup check already catches a self-referential upstream. D10 then added runtime
    // config, which can set one *after* startup — so the premise stopped holding and
    // nothing noticed. A proxy pointed at itself forwards every request to itself
    // forever, and the symptom is a pinned core and exhausted file descriptors rather
    // than an error anyone can read.
    //
    // Checked against the config the overrides *would* produce, not the one requested,
    // because the listen address may itself be overridden in the same call.
    let candidate = config::preview_overrides(&requested);
    if is_self_referential(candidate.upstream(), candidate.listen_addr()) {
        tracing::warn!(
            upstream = %candidate.upstream(),
            listen = %candidate.listen_addr(),
            "refused a runtime-env change that would point the proxy at itself"
        );
        // A 400 rather than `refuse`'s 403. The caller is local and allowed to be here;
        // the configuration they sent is the problem. A permission error would send an
        // operator hunting for an access issue during the incident they are already
        // trying to fix.
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "type": "error",
                "error": {
                    "type": "invalid_request_error",
                    "message": format!(
                        "upstream {} would be this proxy's own listen address ({}); \
                         every request would forward to itself",
                        candidate.upstream(),
                        candidate.listen_addr()
                    ),
                },
            })),
        )
            .into_response();
    }

    let applied = config::set_overrides(requested);

    // Stored is not the same as in effect. These are read once at startup — the CCR
    // store is opened once, memories and recommendations are loaded once so the same
    // request cannot compress differently depending on when it arrived (I4), and the
    // listen socket is bound once. Reporting them as applied and nothing more is a lie an
    // operator acts on: they believe the change took and move on.
    let needs_restart: Vec<String> = applied
        .iter()
        .filter(|name| config::STARTUP_ONLY.contains(&name.as_str()))
        .cloned()
        .collect();

    if needs_restart.is_empty() {
        tracing::info!(?applied, "runtime overrides applied");
    } else {
        tracing::warn!(
            ?applied,
            ?needs_restart,
            "runtime overrides stored; some take effect only after a restart"
        );
    }

    // The values are echoed back as *names only*. Configuration can carry an upstream
    // URL with credentials in it, and an endpoint that reflects what it was given is
    // the easiest way for one to end up in a log.
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "applied": applied,
            // Named explicitly rather than left for the operator to infer. An empty list
            // is the common case and says "everything you set is live now".
            "needs_restart": needs_restart,
        })),
    )
        .into_response()
}

/// Builds the refusal, in the shape the rest of the proxy uses for errors.
fn refuse(message: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "type": "error",
            "error": { "type": "permission_error", "message": message },
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {

    // Async purely so it can take `SERIAL`, the one lock this module already uses to
    // serialize tests that touch process-global overrides. A second mechanism beside it
    // is how two guards drift apart.
    #[tokio::test]
    async fn a_preview_that_would_point_the_proxy_at_itself_is_recognized() {
        let _guard = SERIAL.lock().await;
        // The hole this guard closes. D11 justified having no per-request loop-detection
        // header because a startup check already catches a self-referential upstream;
        // D10 then added runtime config, which can set one *after* startup. The premise
        // stopped holding and nothing noticed until it was probed.
        let listen = config::Config::from_env().listen_addr();
        let mut requested = BTreeMap::new();
        requested.insert("HEADROOM_UPSTREAM".to_owned(), format!("http://{listen}"));

        let candidate = config::preview_overrides(&requested);
        assert!(is_self_referential(
            candidate.upstream(),
            candidate.listen_addr()
        ));
    }

    // Async purely so it can take `SERIAL`, the one lock this module already uses to
    // serialize tests that touch process-global overrides. A second mechanism beside it
    // is how two guards drift apart.
    #[tokio::test]
    async fn a_preview_does_not_apply_anything() {
        let _guard = SERIAL.lock().await;
        // Applying and rolling back would leave the bad configuration live for the
        // duration of the check, and config is read per request from a thread pool — an
        // in-flight request could pick it up in that window and start the loop.
        let before = config::Config::from_env().upstream().to_owned();

        let mut requested = BTreeMap::new();
        requested.insert(
            "HEADROOM_UPSTREAM".to_owned(),
            "http://somewhere-else.invalid".to_owned(),
        );
        let _ = config::preview_overrides(&requested);

        assert_eq!(config::Config::from_env().upstream(), before);
    }

    // Async purely so it can take `SERIAL`, the one lock this module already uses to
    // serialize tests that touch process-global overrides. A second mechanism beside it
    // is how two guards drift apart.
    #[tokio::test]
    async fn an_ordinary_upstream_change_previews_clean() {
        let _guard = SERIAL.lock().await;
        // The guard must not refuse the thing this endpoint exists to do.
        let mut requested = BTreeMap::new();
        requested.insert(
            "HEADROOM_UPSTREAM".to_owned(),
            "https://api.anthropic.com".to_owned(),
        );

        let candidate = config::preview_overrides(&requested);
        assert!(!is_self_referential(
            candidate.upstream(),
            candidate.listen_addr()
        ));
    }
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::post;
    use axum::Router;
    use tower::ServiceExt;

    /// Serializes these tests against each other.
    ///
    /// The override map is process-global, which is the point of it — but `cargo test`
    /// runs tests on a thread pool, so without this two tests clear and set the same
    /// map concurrently and fail in whichever order the scheduler picked. That is a
    /// flake in the tests, not in the code, and it deserves a lock rather than a retry.
    ///
    /// An async mutex rather than `std::sync::Mutex`, because each test holds it across
    /// an `.await`. These are single-task tests so a blocking guard could not actually
    /// deadlock, but "it happens to be safe here" is not a property that survives
    /// someone adding a second task to one of them.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn app() -> Router {
        Router::new().route("/admin/runtime-env", post(runtime_env))
    }

    /// Sends `body` from `peer`, or with no connection info when `peer` is `None`.
    async fn call(peer: Option<&str>, body: &str) -> Response {
        let mut request = Request::builder()
            .method("POST")
            .uri("/admin/runtime-env")
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap();

        if let Some(peer) = peer {
            let addr: SocketAddr = peer.parse().unwrap();
            request.extensions_mut().insert(ConnectInfo(addr));
        }

        app().oneshot(request).await.unwrap()
    }

    async fn body_of(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn a_local_request_applies_the_overrides() {
        let _guard = SERIAL.lock().await;
        config::clear_overrides();

        let response = call(Some("127.0.0.1:5555"), r#"{"HEADROOM_COMPRESSION":"0"}"#).await;
        assert_eq!(response.status(), StatusCode::OK);

        assert!(!crate::Config::from_env().compression_enabled());
        config::clear_overrides();
    }

    #[tokio::test]
    async fn retuning_one_setting_leaves_the_others_alone() {
        let _guard = SERIAL.lock().await;
        config::clear_overrides();

        // The scenario this module opens by naming: "turning compression off during an
        // incident should not cost" a restart. While `set_overrides` replaced rather than
        // merged, any later retune of anything else silently turned it back on, and the
        // response said only which name it had just applied.
        //
        // Measured against a running proxy before the fix: compression off forwarded
        // 19500 bytes, then `{"HEADROOM_STABILIZE":"1"}` forwarded 1895 — compression
        // back on, unasked, with `applied: ["HEADROOM_STABILIZE"]` as the only report.
        call(Some("127.0.0.1:5555"), r#"{"HEADROOM_COMPRESSION":"0"}"#).await;
        assert!(
            !crate::Config::from_env().compression_enabled(),
            "compression did not go off, so nothing below is being tested"
        );

        call(Some("127.0.0.1:5555"), r#"{"HEADROOM_STABILIZE":"1"}"#).await;

        assert!(
            !crate::Config::from_env().compression_enabled(),
            "an unrelated retune silently re-enabled compression"
        );
        assert!(
            config::overrides().contains_key(config::vars::STABILIZE),
            "the new setting was lost instead, which is the same bug mirrored"
        );

        config::clear_overrides();
    }

    #[tokio::test]
    async fn an_override_is_removed_by_sending_it_empty() {
        let _guard = SERIAL.lock().await;
        config::clear_overrides();

        // The control for the test above, and the replacement for what `{}` used to do.
        // Without a way to take one back, merging would be a trap rather than a fix.
        call(
            Some("127.0.0.1:5555"),
            r#"{"HEADROOM_UPSTREAM":"http://example.invalid"}"#,
        )
        .await;
        assert_eq!(
            crate::Config::from_env().upstream(),
            "http://example.invalid",
            "the override never took, so clearing it proves nothing"
        );

        call(Some("127.0.0.1:5555"), r#"{"HEADROOM_UPSTREAM":""}"#).await;
        assert_eq!(
            crate::Config::from_env().upstream(),
            config::DEFAULT_UPSTREAM,
            "an emptied override did not fall back to the default"
        );

        config::clear_overrides();
    }

    #[tokio::test]
    async fn a_change_that_would_point_the_proxy_at_itself_is_refused() {
        let _guard = SERIAL.lock().await;
        // Through the handler, not through `preview_overrides` — the three unit tests
        // above pass with the guard deleted from the handler, because they exercise the
        // helper rather than the wiring. That is the same "is it actually called?" bug
        // this repository keeps producing, so this one goes through the endpoint.
        config::clear_overrides();
        let listen = config::Config::from_env().listen_addr();

        let response = call(
            Some("127.0.0.1:5000"),
            &format!(r#"{{"HEADROOM_UPSTREAM":"http://{listen}"}}"#),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        // And nothing was applied — a refusal that still changed the config would be
        // worse than no check at all.
        assert!(config::overrides().is_empty());
        config::clear_overrides();
    }

    #[tokio::test]
    async fn an_ordinary_upstream_change_is_still_applied() {
        let _guard = SERIAL.lock().await;
        // The guard must not refuse the thing this endpoint exists to do.
        config::clear_overrides();

        let response = call(
            Some("127.0.0.1:5000"),
            r#"{"HEADROOM_UPSTREAM":"https://api.anthropic.com"}"#,
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            config::Config::from_env().upstream(),
            "https://api.anthropic.com"
        );
        config::clear_overrides();
    }

    #[tokio::test]
    async fn a_startup_only_override_is_reported_as_needing_a_restart() {
        // The endpoint used to answer `applied` for these and nothing else, which is a
        // lie an operator acts on: the value is stored, nothing ever reads it again, and
        // they believe the change took effect and move on.
        let _guard = SERIAL.lock().await;
        config::clear_overrides();

        let response = call(
            Some("127.0.0.1:5000"),
            r#"{"HEADROOM_MEMORY":"/tmp/memories.jsonl"}"#,
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_of(response).await;
        assert_eq!(body["applied"], serde_json::json!(["HEADROOM_MEMORY"]));
        assert_eq!(
            body["needs_restart"],
            serde_json::json!(["HEADROOM_MEMORY"])
        );
        config::clear_overrides();
    }

    #[tokio::test]
    async fn a_live_override_reports_an_empty_restart_list() {
        // The common case, and it has to be visibly different from the one above or the
        // field tells an operator nothing.
        let _guard = SERIAL.lock().await;
        config::clear_overrides();

        let response = call(Some("127.0.0.1:5000"), r#"{"HEADROOM_COMPRESSION":"0"}"#).await;

        let body = body_of(response).await;
        assert_eq!(body["applied"], serde_json::json!(["HEADROOM_COMPRESSION"]));
        assert_eq!(body["needs_restart"], serde_json::json!([]));
        config::clear_overrides();
    }

    #[test]
    fn every_startup_only_name_is_a_real_setting() {
        // A typo here would silently drop a name from the warning — the value would be
        // stored, reported as live, and never read. Checked against `config::KNOWN`
        // rather than trusted.
        for name in config::STARTUP_ONLY {
            assert!(
                config::KNOWN.contains(&name),
                "{name} is not a known setting"
            );
        }
    }

    #[tokio::test]
    async fn an_unrecognized_headroom_name_is_not_reported_as_applied() {
        // `set_overrides` used to accept and store any `HEADROOM_`-prefixed name,
        // checked against nothing. A typo (`HEADROOM_MADE_UP_NAME`) or a documented
        // variable this crate has never wired to `config` (`HEADROOM_LOG`, read once
        // by `main` at startup, never by this module) was stored in the override map,
        // echoed back in `applied` with an empty `needs_restart`, and never read
        // again. Mixed with a real setting in the same call so the fix is proven to be
        // selective rather than a blanket rejection of the whole request.
        let _guard = SERIAL.lock().await;
        config::clear_overrides();

        let response = call(
            Some("127.0.0.1:5555"),
            r#"{"HEADROOM_MADE_UP_NAME":"x","HEADROOM_LOG":"debug","HEADROOM_COMPRESSION":"0"}"#,
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_of(response).await;
        assert_eq!(body["applied"], serde_json::json!(["HEADROOM_COMPRESSION"]));
        assert!(!config::overrides().contains_key("HEADROOM_MADE_UP_NAME"));
        assert!(!config::overrides().contains_key("HEADROOM_LOG"));
        assert!(config::overrides().contains_key(config::vars::COMPRESSION));

        config::clear_overrides();
    }

    #[tokio::test]
    async fn a_remote_request_is_refused() {
        // This endpoint can repoint `HEADROOM_UPSTREAM`, so anyone who can reach it can
        // redirect every subsequent request — credential attached — to a server they
        // control.
        let _guard = SERIAL.lock().await;
        config::clear_overrides();

        let response = call(
            Some("203.0.113.9:5555"),
            r#"{"HEADROOM_UPSTREAM":"http://evil"}"#,
        )
        .await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            crate::Config::from_env().upstream(),
            crate::config::DEFAULT_UPSTREAM,
            "a remote caller changed the upstream"
        );
        config::clear_overrides();
    }

    #[tokio::test]
    async fn a_request_without_connection_information_is_refused() {
        // The handler cannot establish that the caller is local, and being able to is
        // this endpoint's entire protection. Failing closed is the only safe reading.
        let _guard = SERIAL.lock().await;
        config::clear_overrides();

        let response = call(None, r#"{"HEADROOM_UPSTREAM":"http://evil"}"#).await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            crate::Config::from_env().upstream(),
            crate::config::DEFAULT_UPSTREAM
        );
        config::clear_overrides();
    }

    #[tokio::test]
    async fn names_outside_the_headroom_namespace_are_ignored() {
        // Otherwise this is a general-purpose lever on the process for anyone who can
        // reach it, rather than a way to retune the proxy.
        let _guard = SERIAL.lock().await;
        config::clear_overrides();

        let response = call(
            Some("127.0.0.1:5555"),
            r#"{"HEADROOM_COMPRESSION":"0","PATH":"/evil","LD_PRELOAD":"/x.so"}"#,
        )
        .await;

        let applied = body_of(response).await;
        let applied = applied["applied"].as_array().unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0], "HEADROOM_COMPRESSION");
        assert!(!config::overrides().contains_key("PATH"));

        config::clear_overrides();
    }

    #[tokio::test]
    async fn the_response_echoes_names_but_never_values() {
        // Configuration can carry an upstream URL with credentials in it, and an
        // endpoint that reflects what it was given is the easiest way for one to reach
        // a log.
        let _guard = SERIAL.lock().await;
        config::clear_overrides();

        let response = call(
            Some("127.0.0.1:5555"),
            r#"{"HEADROOM_UPSTREAM":"https://user:secret@example.com"}"#,
        )
        .await;

        let rendered = body_of(response).await.to_string();
        assert!(rendered.contains("HEADROOM_UPSTREAM"));
        assert!(
            !rendered.contains("secret"),
            "a value was echoed: {rendered}"
        );

        config::clear_overrides();
    }

    #[tokio::test]
    async fn numbers_and_booleans_are_accepted_rather_than_rejected() {
        // `{"HEADROOM_PORT": 8788}` means the obvious thing, and a 400 here would be
        // pedantry during an incident.
        let _guard = SERIAL.lock().await;
        config::clear_overrides();

        let response = call(
            Some("127.0.0.1:5555"),
            r#"{"HEADROOM_PORT":8788,"HEADROOM_COMPRESSION":false}"#,
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(config::overrides().get("HEADROOM_PORT").unwrap(), "8788");
        assert_eq!(
            config::overrides().get("HEADROOM_COMPRESSION").unwrap(),
            "false"
        );

        config::clear_overrides();
    }

    #[tokio::test]
    async fn a_non_object_body_is_a_400_not_a_panic() {
        let _guard = SERIAL.lock().await;
        config::clear_overrides();
        let response = call(Some("127.0.0.1:5555"), r#"["not","an","object"]"#).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        config::clear_overrides();
    }
}
