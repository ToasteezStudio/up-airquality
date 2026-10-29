//! TLS client configuration shared by the Postgres and MQTT connections.

use anyhow::{bail, Context, Result};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;

/// A rustls client config that verifies servers against the PEM CA bundle
/// named by the `ca_env` environment variable (for a self-signed server CA),
/// or the system trust store when it is unset.
pub fn client_config(ca_env: &str) -> Result<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    match std::env::var_os(ca_env) {
        Some(path) => {
            for cert in
                CertificateDer::pem_file_iter(&path).with_context(|| format!("opening {ca_env}"))?
            {
                roots.add(cert.with_context(|| format!("reading {ca_env}"))?)?;
            }
        }
        None => {
            for cert in rustls_native_certs::load_native_certs().certs {
                let _ = roots.add(cert);
            }
        }
    }
    if roots.is_empty() {
        bail!("no CA certificates to verify the server (set {ca_env})");
    }
    Ok(rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth())
}
