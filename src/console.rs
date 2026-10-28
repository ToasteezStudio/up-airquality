//! Minimal "fake Protect console": a TLS WebSocket server on :7442 that the
//! adopted sensor dials back into. It negotiates the `ucp4` subprotocol,
//! answers `timeSync`, records `readingEvent` payloads into shared state, and
//! prints them.
//!
//! Confirmed live 2026-10-28: mutual TLS is NOT required (we present a server
//! cert and do not request a client cert) and no cert fingerprint pinning is
//! needed. See docs/PROTOCOL.md §4.

use crate::model::Reading;
use crate::proto::{decode_message, encode_message};
use crate::state::{now_ms, Metrics};
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use sha2::{Digest, Sha256};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::handshake::server::{
    Callback, ErrorResponse, Request, Response,
};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

pub const CONSOLE_PORT: u16 = 7442;

/// Largest WebSocket message or frame accepted; real messages are tiny.
const MAX_WS_MESSAGE: usize = 1024 * 1024;
/// Concurrent console connections. The sensor needs one.
const MAX_CONNS: usize = 8;
/// The TLS + WebSocket handshake must finish within this.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Close a connection that sends nothing for this long. The sensor sends a
/// reading every few seconds.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// A self-signed console certificate plus the SHA-256 fingerprint the sensor
/// may pin via the adopt `fingerprint` field (pinning is not required).
pub struct ConsoleCert {
    cert_der: CertificateDer<'static>,
    key_der: PrivatePkcs8KeyDer<'static>,
    pub fingerprint_hex: String,
}

pub fn generate_cert() -> Result<ConsoleCert> {
    let certified = rcgen::generate_simple_self_signed(vec!["ui.com".to_string()])
        .context("generating self-signed cert")?;
    let cert_der = CertificateDer::from(certified.cert.der().to_vec());
    let key_der = PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der());
    let fingerprint_hex = {
        let mut h = Sha256::new();
        h.update(cert_der.as_ref());
        hex(&h.finalize())
    };
    Ok(ConsoleCert {
        cert_der,
        key_der,
        fingerprint_hex,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn tls_acceptor(cert: &ConsoleCert) -> Result<TlsAcceptor> {
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert_der.clone()],
            PrivateKeyDer::Pkcs8(cert.key_der.clone_key()),
        )
        .context("building rustls ServerConfig")?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Echoes the `ucp4` subprotocol back and logs the device identity headers.
struct Ucp4Handshake;

impl Callback for Ucp4Handshake {
    fn on_request(
        self,
        request: &Request,
        mut response: Response,
    ) -> Result<Response, ErrorResponse> {
        for (name, value) in request.headers() {
            let n = name.as_str().to_ascii_lowercase();
            if n.starts_with("x-") || n == "sec-websocket-protocol" {
                tracing::info!(header = %n, value = %value.to_str().unwrap_or("<binary>"), "device header");
            }
        }
        response
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", HeaderValue::from_static("ucp4"));
        Ok(response)
    }
}

/// Run the console until cancelled. Records readings into `state`. When
/// `allowed` is set, connections from any other address are dropped before
/// the TLS handshake, so nothing else can inject readings.
pub async fn serve(
    cert: Arc<ConsoleCert>,
    state: Arc<Metrics>,
    allowed: Option<IpAddr>,
) -> Result<()> {
    let acceptor = tls_acceptor(&cert)?;
    let addr: SocketAddr = ([0, 0, 0, 0], CONSOLE_PORT).into();
    let listener = TcpListener::bind(addr).await.context("bind :7442")?;
    tracing::info!(%addr, fingerprint = %cert.fingerprint_hex, "console listening (wss, subprotocol ucp4)");
    match allowed {
        Some(ip) => tracing::info!(%ip, "accepting sensor connections only from this address"),
        None => tracing::warn!(
            "accepting sensor connections from any address; pass --host <SENSOR_IP> to restrict"
        ),
    }
    let slots = Arc::new(Semaphore::new(MAX_CONNS));

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                // e.g. EMFILE: back off and keep serving rather than exit.
                tracing::warn!(error = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if allowed.is_some_and(|ip| ip != peer.ip()) {
            tracing::warn!(%peer, "rejected connection from unexpected address");
            continue;
        }
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            tracing::warn!(%peer, "too many console connections; dropping");
            continue;
        };
        let acceptor = acceptor.clone();
        let state = state.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = handle_conn(acceptor, stream, peer, state).await {
                tracing::warn!(%peer, error = %e, "connection ended");
            }
        });
    }
}

async fn handle_conn(
    acceptor: TlsAcceptor,
    stream: TcpStream,
    peer: SocketAddr,
    state: Arc<Metrics>,
) -> Result<()> {
    tracing::info!(%peer, "device connected; starting TLS");
    let handshake = async {
        let tls = acceptor.accept(stream).await.context("tls accept")?;
        let config = WebSocketConfig {
            max_message_size: Some(MAX_WS_MESSAGE),
            max_frame_size: Some(MAX_WS_MESSAGE),
            ..Default::default()
        };
        tokio_tungstenite::accept_hdr_async_with_config(tls, Ucp4Handshake, Some(config))
            .await
            .context("ws handshake")
    };
    let mut ws = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake)
        .await
        .context("handshake timed out")??;
    tracing::info!(%peer, "ucp4 websocket established");

    loop {
        let msg = match tokio::time::timeout(IDLE_TIMEOUT, ws.next()).await {
            Ok(Some(msg)) => msg,
            Ok(None) => break,
            Err(_) => {
                tracing::warn!(%peer, "no messages for {IDLE_TIMEOUT:?}; closing");
                break;
            }
        };
        match msg.context("ws read")? {
            Message::Binary(buf) => {
                if let Err(e) = on_binary(&mut ws, &buf, &peer, &state).await {
                    tracing::warn!(%peer, error = %e, "handling message");
                }
            }
            Message::Ping(p) => {
                let _ = ws.send(Message::Pong(p)).await;
            }
            Message::Close(_) => break,
            other => tracing::debug!(?other, "ignored non-binary frame"),
        }
    }
    Ok(())
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_rustls::server::TlsStream<TcpStream>>;

async fn on_binary(ws: &mut Ws, buf: &[u8], peer: &SocketAddr, state: &Arc<Metrics>) -> Result<()> {
    let msg = decode_message(buf)?;
    let action = msg.action().unwrap_or("");
    let name = msg.name().unwrap_or("");
    tracing::debug!(head = %msg.head, "rx");

    match (action, name) {
        ("timeSync", _) | (_, "timeSync") => {
            let t0 = msg.body.get("t0").cloned().unwrap_or(serde_json::json!(0));
            let now = now_ms();
            let head = serde_json::json!({
                "timestamp": now, "id": msg.id().unwrap_or(""),
                "type": "response", "action": "timeSync",
            });
            let body = serde_json::json!({ "t0": t0, "t1": now, "t2": now });
            ws.send(Message::Binary(encode_message(&head, &body)?))
                .await?;
            tracing::info!("answered timeSync");
        }
        (_, "readingEvent") | ("readingEvent", _) => {
            let reading: Reading = serde_json::from_value(msg.body.clone()).unwrap_or_default();
            println!("READING  {}", reading.summary());
            state.record(reading, &peer.ip().to_string());
        }
        (_, "alertEvent") | ("alertEvent", _) => {
            println!("ALERT    {}", msg.body);
        }
        _ => {
            if msg.msg_type() == Some("request") {
                let head = serde_json::json!({
                    "timestamp": now_ms(), "id": msg.id().unwrap_or(""),
                    "type": "response", "action": action,
                });
                ws.send(Message::Binary(encode_message(
                    &head,
                    &serde_json::json!({}),
                )?))
                .await?;
            }
            tracing::info!(action, name, body = %msg.body, "unhandled message (acked if request)");
        }
    }
    Ok(())
}
