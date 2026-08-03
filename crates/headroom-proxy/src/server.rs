//! Server construction and lifecycle.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use headroom_core::ccr::InMemoryCcrStore;

use crate::compression::{compress_request, Compressors};
use crate::config::Config;
use crate::headers::{sanitize, HeaderPolicy};
use crate::health::health;

/// Shared state: the compressors and the CCR store behind them.
#[derive(Clone)]
pub struct AppState {
    compressors: Arc<Compressors>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            compressors: Arc::new(Compressors::new(Arc::new(InMemoryCcrStore::new()))),
        }
    }
}

/// Builds the application router.
///
/// Separated from [`serve`] so tests can exercise routes without binding a socket.
pub fn router() -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/messages", post(messages))
        .with_state(AppState::default())
}

/// `POST /v1/messages`.
///
/// Compresses the live zone and returns the bytes that would go upstream.
///
/// # Not yet forwarding
///
/// This returns the transformed request rather than relaying it to a provider.
/// Upstream relay needs the SSE state machine to exist first — a handler that
/// forwarded now would have to buffer streaming responses, which breaks the thing
/// clients most rely on. Every invariant lives in [`compress_request`], which is
/// fully tested; this wrapper is the part still missing its other half.
async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let config = Config::from_env();

    // Sanitized here so the header path is exercised on the real request, even while
    // the relay itself is still to come.
    let upstream_headers = sanitize(&headers, HeaderPolicy::default());
    tracing::debug!(
        header_count = upstream_headers.len(),
        auth = ?crate::headers::redacted_authorization(&headers),
        "prepared upstream headers"
    );

    let compressed = compress_request(&body, &state.compressors, config.compression_enabled());
    (axum::http::StatusCode::OK, compressed.into_owned())
}

/// Runs the proxy until a shutdown signal arrives.
///
/// # Errors
///
/// Returns an error if the listen socket cannot be bound.
pub async fn serve(config: &Config) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(config.listen_addr()).await?;
    tracing::info!(
        addr = %config.listen_addr(),
        upstream = config.upstream(),
        compression = config.compression_enabled(),
        "headroom-proxy listening"
    );

    axum::serve(listener, router())
        .with_graceful_shutdown(shutdown_signal())
        .await
}

/// Resolves when the process is asked to stop.
///
/// Graceful shutdown is not a nicety here. The proxy sits in the middle of streaming
/// responses, and dropping an in-flight request on SIGTERM truncates a model's output
/// mid-token — which reaches the user as a corrupt answer rather than as an error
/// they can retry.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            // If the handler cannot be installed, fall back to ctrl-c alone rather
            // than refusing to start.
            Err(err) => {
                tracing::warn!(%err, "could not install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }

    tracing::info!("shutdown signal received, draining");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_answers_200_with_a_body() {
        let response = router()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["status"], "ok");
    }

    #[tokio::test]
    async fn an_unknown_route_is_404_rather_than_a_panic() {
        let response = router()
            .oneshot(Request::builder().uri("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
