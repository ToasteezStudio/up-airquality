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
use std::sync::{Arc, Mutex};
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
/// Concurrent console connections. Each sensor needs one.
const MAX_CONNS: usize = 32;
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

/// Echoes the `ucp4` subprotocol back, logs the device identity headers
/// (redacting its adoption secrets) and captures the sensor's MAC from
/// `x-ident` as its id.
struct Ucp4Handshake {
    ident: Arc<Mutex<Option<String>>>,
}

/// Headers carrying the device's adoption secrets; never logged.
const SECRET_HEADERS: [&str; 2] = ["x-token", "x-key"];

/// Normalise a MAC (`02AABBCCDDEE`, `02:aa:bb:...`) to 12 upper-case hex
/// digits. Anything else is rejected, since the id ends up in URLs, metric
/// labels and MQTT topics.
pub fn sensor_id(raw: &str) -> Option<String> {
    let id: String = raw
        .chars()
        .filter(|c| !matches!(c, ':' | '-'))
        .collect::<String>()
        .to_ascii_uppercase();
    (id.len() == 12 && id.chars().all(|c| c.is_ascii_hexdigit())).then_some(id)
}

impl Callback for Ucp4Handshake {
    fn on_request(
        self,
        request: &Request,
        mut response: Response,
    ) -> Result<Response, ErrorResponse> {
        for (name, value) in request.headers() {
            let n = name.as_str().to_ascii_lowercase();
            let value = value.to_str().unwrap_or("<binary>");
            if n == "x-ident" {
                *self.ident.lock().unwrap() = sensor_id(value);
            }
            if SECRET_HEADERS.contains(&n.as_str()) {
                tracing::info!(header = %n, "device header (value redacted)");
            } else if n.starts_with("x-") || n == "sec-websocket-protocol" {
                tracing::info!(header = %n, value = %value, "device header");
            }
        }
        response
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", HeaderValue::from_static("ucp4"));
        Ok(response)
    }
}

/// Run the console until cancelled. Records readings into `state`. When
/// `allowed` is non-empty, connections from any other address are dropped
/// before the TLS handshake, so nothing else can inject readings.
pub async fn serve(
    cert: Arc<ConsoleCert>,
    state: Arc<Metrics>,
    allowed: Vec<IpAddr>,
) -> Result<()> {
    let acceptor = tls_acceptor(&cert)?;
    let addr: SocketAddr = ([0, 0, 0, 0], CONSOLE_PORT).into();
    let listener = TcpListener::bind(addr).await.context("bind :7442")?;
    tracing::info!(%addr, fingerprint = %cert.fingerprint_hex, "console listening (wss, subprotocol ucp4)");
    if allowed.is_empty() {
        tracing::warn!(
            "accepting sensor connections from any address; pass --host <SENSOR_IP> to restrict"
        );
    } else {
        for ip in &allowed {
            tracing::info!(%ip, "accepting sensor connections from this address");
        }
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
        if !allowed.is_empty() && !allowed.contains(&peer.ip()) {
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
    let ident = Arc::new(Mutex::new(None));
    let callback = Ucp4Handshake {
        ident: ident.clone(),
    };
    let handshake = async {
        let tls = acceptor.accept(stream).await.context("tls accept")?;
        let config = WebSocketConfig {
            max_message_size: Some(MAX_WS_MESSAGE),
            max_frame_size: Some(MAX_WS_MESSAGE),
            ..Default::default()
        };
        tokio_tungstenite::accept_hdr_async_with_config(tls, callback, Some(config))
            .await
            .context("ws handshake")
    };
    let mut ws = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake)
        .await
        .context("handshake timed out")??;
    // The sensor's MAC when it sent a valid one, else its address.
    let id = ident
        .lock()
        .unwrap()
        .take()
        .unwrap_or_else(|| peer.ip().to_string());
    tracing::info!(%peer, sensor = %id, "ucp4 websocket established");

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
                if let Err(e) = on_binary(&mut ws, &buf, &id, &peer, &state).await {
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

async fn on_binary(
    ws: &mut Ws,
    buf: &[u8],
    id: &str,
    peer: &SocketAddr,
    state: &Arc<Metrics>,
) -> Result<()> {
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
            println!("READING  [{id}] {}", reading.summary());
            state.record(id, reading, &peer.ip().to_string());
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

#[cfg(test)]
mod tests {
    use super::sensor_id;

    #[test]
    fn sensor_id_normalises_macs() {
        assert_eq!(sensor_id("02AABBCCDDEE").as_deref(), Some("02AABBCCDDEE"));
        assert_eq!(
            sensor_id("02:aa:bb:cc:dd:ee").as_deref(),
            Some("02AABBCCDDEE")
        );
        assert_eq!(
            sensor_id("02-aa-bb-cc-dd-ee").as_deref(),
            Some("02AABBCCDDEE")
        );
    }

    #[test]
    fn sensor_id_rejects_anything_else() {
        for bad in [
            "",
            "02AABBCCDD",
            "02AABBCCDDEEFF",
            "ZZAABBCCDDEE",
            "<script>",
            "../../etc",
        ] {
            assert_eq!(sensor_id(bad), None, "{bad}");
        }
    }
}
