//! Optional TimescaleDB/Postgres writer. Each reading is forwarded over a
//! bounded channel to a background task that inserts it. The task reconnects
//! on failure and never blocks the websocket loop — if the DB is down, readings
//! are dropped (with a throttled warning) rather than stalling the console.

use crate::model::Reading;
use anyhow::{bail, Context, Result};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;
use std::future::Future;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_postgres::config::SslMode;
use tokio_postgres::{Client, Config, NoTls};

pub type Msg = (Reading, String);

/// Connect to Postgres. With `sslmode=require` in the DSN the connection uses
/// TLS, verifying the server against the system trust store, or against the
/// PEM CA bundle named by `UPAQ_PG_CA` (for a self-signed server CA). Any
/// other sslmode connects without TLS.
async fn connect(dsn: &str) -> Result<Client> {
    let config: Config = dsn.parse().context("parsing Postgres DSN")?;
    if config.get_ssl_mode() == SslMode::Require {
        let tls = tokio_postgres_rustls::MakeRustlsConnect::new(tls_config()?);
        let (client, conn) = config.connect(tls).await?;
        watch(conn);
        Ok(client)
    } else {
        let (client, conn) = config.connect(NoTls).await?;
        watch(conn);
        Ok(client)
    }
}

fn watch(conn: impl Future<Output = Result<(), tokio_postgres::Error>> + Send + 'static) {
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::warn!(error = %e, "pg connection closed");
        }
    });
}

fn tls_config() -> Result<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    match std::env::var_os("UPAQ_PG_CA") {
        Some(path) => {
            for cert in CertificateDer::pem_file_iter(&path).context("opening UPAQ_PG_CA")? {
                roots.add(cert.context("reading UPAQ_PG_CA")?)?;
            }
        }
        None => {
            for cert in rustls_native_certs::load_native_certs().certs {
                let _ = roots.add(cert);
            }
        }
    }
    if roots.is_empty() {
        bail!("sslmode=require but no CA certificates to verify the server (set UPAQ_PG_CA)");
    }
    Ok(rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth())
}

const INSERT: &str = "INSERT INTO air_quality \
    (source, aqi, co2, humidity, temperature, pm1p0, pm2p5, pm4p0, pm10p0, voc, nox, tvoc, vape) \
    VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)";

/// Spawn the writer task and return a sender to hand readings to.
pub fn spawn(dsn: String) -> mpsc::Sender<Msg> {
    let (tx, mut rx) = mpsc::channel::<Msg>(2000);
    tokio::spawn(async move {
        let mut client: Option<Client> = None;
        let mut last_attempt = Instant::now() - Duration::from_secs(60);

        while let Some((reading, source)) = rx.recv().await {
            if client.is_none() {
                if last_attempt.elapsed() < Duration::from_secs(5) {
                    continue; // throttle reconnects; drop this reading
                }
                last_attempt = Instant::now();
                match connect(&dsn).await {
                    Ok(c) => {
                        tracing::info!("connected to Postgres/Timescale");
                        client = Some(c);
                    }
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "pg connect failed; readings not persisted");
                        continue;
                    }
                }
            }

            if let Some(c) = &client {
                let r = &reading;
                let res = c
                    .execute(
                        INSERT,
                        &[
                            &source,
                            &r.aqi,
                            &r.co2,
                            &r.humidity,
                            &r.temperature,
                            &r.pm1p0,
                            &r.pm2p5,
                            &r.pm4p0,
                            &r.pm10p0,
                            &r.voc,
                            &r.nox,
                            &r.tvoc,
                            &r.vape,
                        ],
                    )
                    .await;
                if let Err(e) = res {
                    tracing::warn!(error = %e, "pg insert failed; will reconnect");
                    client = None;
                }
            }
        }
    });
    tx
}
