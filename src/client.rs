//! HTTPS client for the sensor's pre-adoption control API on tcp/8080.
//!
//! Confirmed live: `POST /api/info` with a JSON body of the vendor default
//! credentials returns 200 with device identity. Routes seen in the firmware:
//! `/api/info`, `/api/adopt`, `/api/adopt/layer3`, `/api/locate`, `/api/support`.

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

pub const DEFAULT_USER: &str = "ubnt";
pub const DEFAULT_PASS: &str = "ubnt";
pub const API_PORT: u16 = 8080;

#[derive(Clone, Serialize)]
struct Auth<'a> {
    username: &'a str,
    password: &'a str,
}

fn http() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true) // self-signed CN=ui.com, generated on-device
        .timeout(Duration::from_secs(8))
        .build()
        .context("building HTTPS client")
}

fn base(host: Ipv4Addr) -> String {
    format!("https://{host}:{API_PORT}")
}

/// `POST /api/info` — read-only identity probe. Does not change device state.
pub async fn info(host: Ipv4Addr, user: &str, pass: &str) -> Result<Value> {
    let resp = http()?
        .post(format!("{}/api/info", base(host)))
        .json(&Auth {
            username: user,
            password: pass,
        })
        .send()
        .await
        .context("POST /api/info")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("/api/info returned {status}: {text}");
    }
    serde_json::from_str(&text).context("parsing /api/info JSON")
}

/// Body for `POST /api/adopt`. Fields are those the firmware parses; most are
/// optional. `hosts` tells the device where to dial back (our console:7442).
#[derive(Serialize)]
pub struct AdoptRequest<'a> {
    pub username: &'a str,
    pub password: &'a str,
    pub protocol: &'a str, // "wss"
    pub hosts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>, // SHA-256 of our console cert, or None to skip pinning
}

/// `POST /api/adopt` — **changes device state**: writes adoption config and
/// makes the sensor dial our console. Recoverable via a factory reset.
pub async fn adopt(host: Ipv4Addr, req: &AdoptRequest<'_>) -> Result<Value> {
    let resp = http()?
        .post(format!("{}/api/adopt", base(host)))
        .json(req)
        .send()
        .await
        .context("POST /api/adopt")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("/api/adopt returned {status}: {text}");
    }
    serde_json::from_str(&text)
        .unwrap_or(Value::String(text))
        .into_result()
}

/// `POST /api/locate` — toggle the status LED to identify the unit. Harmless.
/// The firmware requires `isIndicatorEnabled` (bool) and `duration` (ms).
pub async fn locate(
    host: Ipv4Addr,
    user: &str,
    pass: &str,
    enabled: bool,
    duration_ms: u64,
) -> Result<()> {
    let body = serde_json::json!({
        "username": user,
        "password": pass,
        "isIndicatorEnabled": enabled,
        "duration": duration_ms,
    });
    let resp = http()?
        .post(format!("{}/api/locate", base(host)))
        .json(&body)
        .send()
        .await
        .context("POST /api/locate")?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("/api/locate returned {status}: {text}");
    }
    Ok(())
}

/// Which local IPv4 address routes toward the sensor — used as the dial-back
/// host we hand the device during adoption.
pub fn local_ip_towards(target: Ipv4Addr) -> Result<Ipv4Addr> {
    let sock = UdpSocket::bind("0.0.0.0:0").context("bind udp probe")?;
    sock.connect((target, API_PORT))
        .context("connect udp probe")?;
    match sock.local_addr().context("local_addr")? {
        SocketAddr::V4(a) => Ok(*a.ip()),
        other => anyhow::bail!("unexpected local addr {other}"),
    }
}

// Small helper so adopt() can thread the fallback-to-string value through `?`.
trait IntoResult {
    fn into_result(self) -> Result<Value>;
}
impl IntoResult for Value {
    fn into_result(self) -> Result<Value> {
        Ok(self)
    }
}
