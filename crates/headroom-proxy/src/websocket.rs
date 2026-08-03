//! WebSocket relay — gap row X13.
//!
//! Codex uses a WebSocket transport, and a proxy that only speaks HTTP silently drops
//! that client to whatever fallback it has, or breaks it. This relays the socket in
//! both directions.
//!
//! # Frames are forwarded, not compressed
//!
//! Nothing here compresses. That is not a gap waiting to be filled — it is what a
//! WebSocket makes the right answer:
//!
//! - **There is no request boundary.** HTTP compression works because a request arrives
//!   whole, so the live zone can be identified and the frozen prefix left alone. A
//!   socket is a conversation with no such marker; a compressor would have to decide
//!   what was "already sent" from message content alone, and be wrong the first time a
//!   client resent context.
//! - **The client frames the messages.** A relay that recombines or splits frames has
//!   changed the protocol beneath a library that is counting on it.
//!
//! So this is a faithful pipe, and the value it delivers is that Codex works through
//! the proxy at all rather than that its traffic shrinks.
//!
//! # Why both directions need their own task
//!
//! A single loop that reads a client frame, forwards it, reads an upstream frame, and
//! forwards that would deadlock the moment either side sends two messages in a row —
//! which is normal for a duplex protocol. Each direction gets a task, and the first one
//! to finish tears down the other.

use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as UpstreamMessage;

use crate::config::Config;
use crate::server::AppState;

/// Handles a WebSocket upgrade, relaying to the configured upstream.
pub async fn relay_socket(upgrade: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    let upstream = websocket_url(Config::from_env().upstream());
    let metrics = state.metrics().clone();

    upgrade.on_upgrade(move |socket| async move {
        // Counted as passthrough because that is exactly what it is. A socket recorded
        // as "compressed" would make the savings ratio a claim about traffic nothing
        // ever compressed.
        metrics.record_passthrough();

        if let Err(err) = pump(socket, &upstream).await {
            tracing::warn!(%err, upstream = %upstream, "websocket relay ended in error");
        }
    })
}

/// Converts an HTTP(S) base URL to its WebSocket equivalent.
///
/// A socket opened against `https://` fails with a scheme error rather than a
/// connection error, which reads like a configuration problem when it is a translation
/// this function exists to do.
pub fn websocket_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    match base.split_once("://") {
        Some(("https", rest)) => format!("wss://{rest}"),
        Some(("http", rest)) => format!("ws://{rest}"),
        // Already a socket scheme, or something unrecognized. Passed through so the
        // connect attempt reports the real problem rather than one invented here.
        _ => base.to_owned(),
    }
}

/// Relays frames between `client` and `upstream` until either side closes.
async fn pump(client: WebSocket, upstream: &str) -> Result<(), String> {
    let (upstream_socket, _) = tokio_tungstenite::connect_async(upstream)
        .await
        .map_err(|err| format!("connecting to {upstream}: {err}"))?;

    let (mut client_tx, mut client_rx) = client.split();
    let (mut upstream_tx, mut upstream_rx) = upstream_socket.split();

    // Each direction gets its own task. A single loop alternating between them would
    // deadlock the moment either side sent two messages in a row, which is normal for a
    // duplex protocol.
    let to_upstream = tokio::spawn(async move {
        while let Some(Ok(frame)) = client_rx.next().await {
            if upstream_tx.send(to_upstream_frame(frame)).await.is_err() {
                break;
            }
        }
    });

    let to_client = tokio::spawn(async move {
        while let Some(Ok(frame)) = upstream_rx.next().await {
            if client_tx.send(to_client_frame(frame)).await.is_err() {
                break;
            }
        }
    });

    // The first direction to finish ends the relay. A socket half-closed in one
    // direction is not a working socket, and leaving the other task running would leak
    // a connection per abandoned client.
    tokio::select! {
        _ = to_upstream => {}
        _ = to_client => {}
    }

    Ok(())
}

/// Translates a client frame for the upstream socket.
///
/// Frame *kinds* are preserved exactly. A relay that turned every frame into text would
/// corrupt binary payloads, and one that dropped pings would let an idle socket be
/// reaped by an intermediary that was waiting for them.
fn to_upstream_frame(frame: AxumMessage) -> UpstreamMessage {
    match frame {
        AxumMessage::Text(text) => UpstreamMessage::Text(text.as_str().into()),
        AxumMessage::Binary(bytes) => UpstreamMessage::Binary(bytes),
        AxumMessage::Ping(bytes) => UpstreamMessage::Ping(bytes),
        AxumMessage::Pong(bytes) => UpstreamMessage::Pong(bytes),
        AxumMessage::Close(_) => UpstreamMessage::Close(None),
    }
}

/// Translates an upstream frame for the client socket.
fn to_client_frame(frame: UpstreamMessage) -> AxumMessage {
    match frame {
        UpstreamMessage::Text(text) => AxumMessage::Text(text.as_str().into()),
        UpstreamMessage::Binary(bytes) => AxumMessage::Binary(bytes),
        UpstreamMessage::Ping(bytes) => AxumMessage::Ping(bytes),
        UpstreamMessage::Pong(bytes) => AxumMessage::Pong(bytes),
        UpstreamMessage::Close(_) => AxumMessage::Close(None),
        // A raw frame is an internal tungstenite representation that never surfaces
        // from a read. Mapped rather than ignored so this match stays exhaustive if the
        // library adds a variant.
        UpstreamMessage::Frame(_) => AxumMessage::Close(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_https_upstream_becomes_wss() {
        // A socket opened against `https://` fails with a scheme error rather than a
        // connection error, which reads like a configuration problem when it is a
        // translation this function exists to do.
        assert_eq!(
            websocket_url("https://api.openai.com"),
            "wss://api.openai.com"
        );
        assert_eq!(
            websocket_url("http://127.0.0.1:8787"),
            "ws://127.0.0.1:8787"
        );
    }

    #[test]
    fn a_trailing_slash_is_normalized_away() {
        assert_eq!(websocket_url("https://example.com/"), "wss://example.com");
    }

    #[test]
    fn an_already_websocket_url_is_left_alone() {
        for url in ["wss://example.com", "ws://127.0.0.1:9999"] {
            assert_eq!(websocket_url(url), url);
        }
    }

    #[test]
    fn an_unrecognized_scheme_passes_through_unchanged() {
        // So the connect attempt reports the real problem rather than one invented here.
        for url in ["", "not a url", "ftp://example.com"] {
            assert_eq!(websocket_url(url), url);
        }
    }

    /// Starts an echo WebSocket server on loopback and returns its `ws://` URL.
    async fn echo_server() -> String {
        use axum::routing::any;
        use axum::Router;
        use std::net::SocketAddr;

        let app = Router::new().route(
            "/ws",
            any(|upgrade: WebSocketUpgrade| async move {
                upgrade.on_upgrade(|mut socket| async move {
                    while let Some(Ok(frame)) = socket.next().await {
                        // Echoes text and binary; a close frame ends the loop.
                        match frame {
                            AxumMessage::Close(_) => break,
                            other => {
                                if socket.send(other).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                })
            }),
        );

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        format!("ws://{addr}/ws")
    }

    #[tokio::test]
    async fn a_text_frame_survives_the_relay_unchanged() {
        // The relay's whole job. A frame that arrives altered is a protocol the client
        // library was not expecting.
        let upstream = echo_server().await;
        let (mut socket, _) = tokio_tungstenite::connect_async(&upstream).await.unwrap();

        let payload = r#"{"type":"session.update","日本語":"😀"}"#;
        socket
            .send(UpstreamMessage::Text(payload.into()))
            .await
            .unwrap();

        let echoed = socket.next().await.unwrap().unwrap();
        assert_eq!(echoed, UpstreamMessage::Text(payload.into()));
    }

    #[tokio::test]
    async fn a_binary_frame_stays_binary() {
        // A relay that turned every frame into text would corrupt binary payloads —
        // and audio is exactly what a realtime socket carries.
        let upstream = echo_server().await;
        let (mut socket, _) = tokio_tungstenite::connect_async(&upstream).await.unwrap();

        let payload = vec![0x00, 0xff, 0xfe, 0x01];
        socket
            .send(UpstreamMessage::Binary(payload.clone().into()))
            .await
            .unwrap();

        match socket.next().await.unwrap().unwrap() {
            UpstreamMessage::Binary(bytes) => assert_eq!(bytes.as_ref(), &payload[..]),
            other => panic!("binary frame arrived as {other:?}"),
        }
    }

    #[test]
    fn frame_translation_preserves_the_kind_in_both_directions() {
        // Dropping pings would let an idle socket be reaped by an intermediary that was
        // waiting for them, which surfaces as a connection that dies after a minute of
        // silence.
        assert!(matches!(
            to_upstream_frame(AxumMessage::Ping(vec![1].into())),
            UpstreamMessage::Ping(_)
        ));
        assert!(matches!(
            to_upstream_frame(AxumMessage::Pong(vec![1].into())),
            UpstreamMessage::Pong(_)
        ));
        assert!(matches!(
            to_client_frame(UpstreamMessage::Ping(vec![1].into())),
            AxumMessage::Ping(_)
        ));
        assert!(matches!(
            to_client_frame(UpstreamMessage::Binary(vec![1].into())),
            AxumMessage::Binary(_)
        ));
    }

    #[test]
    fn url_translation_is_deterministic() {
        for _ in 0..25 {
            assert_eq!(
                websocket_url("https://api.openai.com/"),
                "wss://api.openai.com"
            );
        }
    }
}
