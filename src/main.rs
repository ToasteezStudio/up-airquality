//! up-airquality — reverse-engineered client for the Ubiquiti UniFi Protect
//! UP-AirQuality sensor. Acts as a minimal "fake Protect console" so the sensor
//! can be adopted and read without a Protect-capable console.
//!
//! Subcommands:
//!   info    Read-only identity probe (POST /api/info). Safe.
//!   locate  Toggle the status LED (adoption-gated).
//!   serve   Run the console + dashboard; prints readings from an adopted sensor.
//!   run     Start the console + dashboard, then adopt the sensor. CHANGES DEVICE STATE.

mod client;
mod console;
mod http;
mod model;
mod mqtt;
mod pg;
mod proto;
mod state;
mod tls;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "up-airquality", version, about)]
struct Cli {
    /// Sensor IP address. Repeat it, or comma-separate, for several sensors.
    /// Required for info/locate/run, which act on each sensor in turn. With
    /// serve, the console only accepts connections from these addresses
    /// (recommended).
    #[arg(long, value_delimiter = ',')]
    host: Vec<Ipv4Addr>,
    /// Login username (vendor default).
    #[arg(long, default_value = client::DEFAULT_USER)]
    user: String,
    /// Login password (vendor default).
    #[arg(long, default_value = client::DEFAULT_PASS)]
    pass: String,
    /// Port for the dashboard / metrics HTTP server (serve/run).
    #[arg(long, default_value_t = 8099, global = true)]
    http_port: u16,
    /// Postgres/Timescale DSN to persist readings, e.g.
    /// "host=192.168.x.y user=upaq password=... dbname=airquality".
    /// Add sslmode=require for TLS (CA bundle via UPAQ_PG_CA if self-signed).
    /// Falls back to the UPAQ_PG_DSN env var; prefer that, since command-line
    /// arguments are visible to other local users. Omit to disable DB writes.
    #[arg(long, global = true)]
    pg: Option<String>,
    /// MQTT broker to publish readings to, with Home Assistant discovery:
    /// "mqtt://[user:pass@]host[:1883]" (MQTT over TLS is not supported yet).
    /// Falls back to the UPAQ_MQTT_URL env var; prefer that, since
    /// command-line arguments are visible to other local users. Omit to
    /// disable MQTT.
    #[arg(long, global = true)]
    mqtt: Option<String>,
    /// Home Assistant MQTT discovery prefix.
    #[arg(long, global = true, default_value = "homeassistant")]
    mqtt_discovery_prefix: String,
    #[command(subcommand)]
    cmd: Cmd,
}

impl Cli {
    /// The sensors named with `--host`, which info/locate/run require.
    fn hosts(&self) -> Result<&[Ipv4Addr]> {
        if self.host.is_empty() {
            anyhow::bail!("set --host <sensor-ip> (the UP-AirQuality sensor's IP)");
        }
        Ok(&self.host)
    }
}

/// `flag`, else the env var `env`, ignoring empty values.
fn setting(flag: &Option<String>, env: &str) -> Option<String> {
    flag.clone()
        .or_else(|| std::env::var(env).ok())
        .filter(|v| !v.is_empty())
}

/// Build the shared metrics state, wiring in a Postgres writer (--pg or
/// UPAQ_PG_DSN) and an MQTT publisher (--mqtt or UPAQ_MQTT_URL) when set.
fn make_state(cli: &Cli) -> Result<Arc<state::Metrics>> {
    let mut metrics = state::Metrics::new();
    if let Some(dsn) = setting(&cli.pg, "UPAQ_PG_DSN") {
        tracing::info!("persisting readings to Postgres/Timescale");
        metrics = metrics.with_writer(pg::spawn(dsn));
    }
    if let Some(url) = setting(&cli.mqtt, "UPAQ_MQTT_URL") {
        let tx = mqtt::spawn(&url, cli.mqtt_discovery_prefix.clone())
            .context("starting the MQTT publisher")?;
        metrics = metrics.with_mqtt(tx);
    }
    Ok(Arc::new(metrics))
}

#[derive(Subcommand)]
enum Cmd {
    /// Read-only identity probe.
    Info,
    /// Toggle the status LED to identify the unit (only works once adopted).
    Locate {
        #[arg(long, default_value_t = 30)]
        secs: u64,
        #[arg(long)]
        off: bool,
    },
    /// Run the console + dashboard and print readings (sensor already adopted to us).
    Serve,
    /// Start the console + dashboard, then adopt the sensor so it dials back to us.
    Run {
        /// Console IP to advertise to the device (defaults to the auto-detected local IP).
        #[arg(long)]
        console_ip: Option<Ipv4Addr>,
        /// Pin our console cert fingerprint in the adopt request (not required).
        #[arg(long)]
        pin_cert: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // rustls 0.23 needs a process-wide crypto provider chosen explicitly when
    // more than one backend is linked (reqwest pulls aws-lc-rs).
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cli = Cli::parse();

    match cli.cmd {
        Cmd::Info => {
            let hosts = cli.hosts()?;
            if let [host] = hosts {
                let v = client::info(*host, &cli.user, &cli.pass).await?;
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                // Several sensors: one object keyed by address.
                let mut all = serde_json::Map::new();
                for host in hosts {
                    let v = client::info(*host, &cli.user, &cli.pass)
                        .await
                        .with_context(|| format!("info from {host}"))?;
                    all.insert(host.to_string(), v);
                }
                println!("{}", serde_json::to_string_pretty(&all)?);
            }
        }
        Cmd::Locate { secs, off } => {
            let st = if off {
                "off".to_string()
            } else {
                format!("on for {secs}s")
            };
            for host in cli.hosts()? {
                client::locate(*host, &cli.user, &cli.pass, !off, secs * 1000)
                    .await
                    .with_context(|| format!("locate on {host}"))?;
                println!("{host}: locate sent ({st}); the status LED should change");
            }
        }
        Cmd::Serve => {
            let state = make_state(&cli)?;
            let cert = Arc::new(console::generate_cert()?);
            spawn_http(state.clone(), cli.http_port);
            let allowed = cli.host.iter().copied().map(IpAddr::V4).collect();
            console::serve(cert, state, allowed).await?;
        }
        Cmd::Run {
            console_ip,
            pin_cert,
        } => {
            let hosts = cli.hosts()?.to_vec();
            let state = make_state(&cli)?;
            let cert = Arc::new(console::generate_cert()?);

            spawn_http(state.clone(), cli.http_port);
            let server_cert = cert.clone();
            let server_state = state.clone();
            let allowed = hosts.iter().copied().map(IpAddr::V4).collect();
            let server =
                tokio::spawn(
                    async move { console::serve(server_cert, server_state, allowed).await },
                );

            tokio::time::sleep(std::time::Duration::from_millis(300)).await;

            for host in hosts {
                // Each sensor may sit on a different network, so detect the
                // address it should dial per sensor unless one was given.
                let ip = match console_ip {
                    Some(ip) => ip,
                    None => client::local_ip_towards(host)
                        .context("auto-detecting console IP (pass --console-ip to override)")?,
                };
                let dial_back = format!("{ip}:{}", console::CONSOLE_PORT);
                tracing::warn!(%host, %dial_back, "about to ADOPT the sensor — changes device state (recover via factory reset)");
                let req = client::AdoptRequest {
                    username: &cli.user,
                    password: &cli.pass,
                    protocol: "wss",
                    hosts: vec![dial_back],
                    token: Some(uuid::Uuid::new_v4().to_string()),
                    fingerprint: pin_cert.then(|| cert.fingerprint_hex.clone()),
                };
                match client::adopt(host, &req).await {
                    Ok(v) => {
                        tracing::info!(%host, response = %v, "adopt accepted; waiting for dial-back")
                    }
                    Err(e) => tracing::error!(%host, error = %e, "adopt failed"),
                }
            }

            server.await.context("console task")??;
        }
    }
    Ok(())
}

fn spawn_http(state: Arc<state::Metrics>, port: u16) {
    tokio::spawn(async move {
        if let Err(e) = http::serve(state, port).await {
            tracing::error!(error = %e, "http server exited");
        }
    });
}
