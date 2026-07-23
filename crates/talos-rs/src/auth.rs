//! mTLS authentication for Talos API
//!
//! Handles certificate loading and TLS configuration for connecting to Talos nodes.

use crate::config::Context;
use crate::error::TalosError;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::sync::Arc;
use std::time::Duration;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity};

/// Per-endpoint connection timeout.
///
/// Bounds each dial attempt so an unreachable endpoint is abandoned quickly
/// instead of hanging on the OS-default TCP connect timeout (~21s+). All
/// endpoints are dialed concurrently, so this also bounds how long startup
/// waits when every configured endpoint is unreachable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Convert Ed25519 private key PEM to standard PKCS8 format
/// Talos uses "ED25519 PRIVATE KEY" header but tonic/rustls expects "PRIVATE KEY"
fn convert_ed25519_key_to_pkcs8(pem: &[u8]) -> Vec<u8> {
    let pem_str = String::from_utf8_lossy(pem);
    if pem_str.contains("ED25519 PRIVATE KEY") {
        pem_str
            .replace(
                "-----BEGIN ED25519 PRIVATE KEY-----",
                "-----BEGIN PRIVATE KEY-----",
            )
            .replace(
                "-----END ED25519 PRIVATE KEY-----",
                "-----END PRIVATE KEY-----",
            )
            .into_bytes()
    } else {
        pem.to_vec()
    }
}

/// Create a TLS-enabled gRPC channel from a talosconfig context.
///
/// When the context lists multiple endpoints, all of them are dialed
/// concurrently and the first one that connects wins (talosctl-style failover).
/// Each dial is bounded by [`CONNECT_TIMEOUT`], so an unreachable endpoint is
/// skipped in favor of a reachable one instead of hanging the client. Returns
/// an error only when every configured endpoint is unreachable.
pub async fn create_channel(ctx: &Context) -> Result<Channel, TalosError> {
    let endpoint_urls = ctx.endpoint_urls();
    if endpoint_urls.is_empty() {
        return Err(TalosError::ConfigInvalid(
            "No endpoints configured".to_string(),
        ));
    }

    // Decode certificates from base64
    let ca_pem = ctx.ca_pem()?;
    let client_cert_pem = ctx.client_cert_pem()?;
    let client_key_pem = ctx.client_key_pem()?;

    tracing::debug!("CA cert size: {} bytes", ca_pem.len());
    tracing::debug!("Client cert size: {} bytes", client_cert_pem.len());
    tracing::debug!("Client key size: {} bytes", client_key_pem.len());

    // Convert Ed25519 key header to standard PKCS8 format if needed
    // Tonic expects "PRIVATE KEY" not "ED25519 PRIVATE KEY"
    let client_key_pem = convert_ed25519_key_to_pkcs8(&client_key_pem);

    // Create TLS config (shared across every endpoint - Talos uses one
    // cluster-wide PKI, so the same CA/identity authenticates all of them).
    let ca = Certificate::from_pem(&ca_pem);
    let identity = Identity::from_pem(&client_cert_pem, &client_key_pem);
    let tls_config = ClientTlsConfig::new().ca_certificate(ca).identity(identity);

    // Build one Endpoint per configured endpoint, all sharing the mTLS config
    // and a bounded connect timeout.
    let mut endpoints = Vec::with_capacity(endpoint_urls.len());
    for url in &endpoint_urls {
        let endpoint = Channel::from_shared(url.clone())
            .map_err(|e| TalosError::Connection(format!("Invalid endpoint URL '{}': {}", url, e)))?
            .tls_config(tls_config.clone())
            .map_err(|e| TalosError::Tls(format!("TLS config error: {:?}", e)))?
            .connect_timeout(CONNECT_TIMEOUT)
            .tcp_keepalive(Some(Duration::from_secs(60)));
        endpoints.push((url.clone(), endpoint));
    }

    tracing::debug!(
        "Dialing {} endpoint(s), first reachable wins: {:?}",
        endpoints.len(),
        endpoint_urls
    );

    // Eagerly connect to every endpoint concurrently and use the first that
    // succeeds. This is true failover: a lazy `balance_list` reports its
    // subchannels ready before they have actually connected, so a request can
    // be routed to a dead endpoint and time out; racing real `connect()` calls
    // guarantees we land on a reachable endpoint. Losing in-flight dials are
    // dropped (cancelled) once the first one wins.
    let attempts: Vec<_> = endpoints
        .into_iter()
        .map(|(url, endpoint)| {
            Box::pin(async move {
                match endpoint.connect().await {
                    Ok(channel) => Ok((url, channel)),
                    Err(e) => {
                        tracing::warn!("Endpoint {} not reachable: {}", url, e);
                        Err(e)
                    }
                }
            })
        })
        .collect();

    match futures::future::select_ok(attempts).await {
        Ok(((url, channel), _losers)) => {
            tracing::debug!("Connected via endpoint {}", url);
            Ok(channel)
        }
        Err(e) => Err(e.into()),
    }
}

/// Parse PEM-encoded certificates into rustls types
pub fn parse_certificates(pem_data: &[u8]) -> Result<Vec<CertificateDer<'static>>, TalosError> {
    let mut reader = std::io::BufReader::new(pem_data);
    let certs: Vec<_> = rustls_pemfile::certs(&mut reader)
        .filter_map(|r| r.ok())
        .collect();

    if certs.is_empty() {
        return Err(TalosError::Tls(
            "No certificates found in PEM data".to_string(),
        ));
    }

    Ok(certs)
}

/// Parse PEM-encoded private key into rustls type
pub fn parse_private_key(pem_data: &[u8]) -> Result<PrivateKeyDer<'static>, TalosError> {
    let mut reader = std::io::BufReader::new(pem_data);

    // Try to read private key (handles PKCS8, EC, and RSA formats)
    let key = rustls_pemfile::private_key(&mut reader)
        .map_err(|e| TalosError::Tls(format!("Failed to parse private key: {}", e)))?
        .ok_or_else(|| TalosError::Tls("No private key found in PEM data".to_string()))?;

    Ok(key)
}

/// Build a rustls ClientConfig for mTLS
pub fn build_rustls_config(
    ca_pem: &[u8],
    client_cert_pem: &[u8],
    client_key_pem: &[u8],
) -> Result<Arc<rustls::ClientConfig>, TalosError> {
    // Parse CA certificates
    let ca_certs = parse_certificates(ca_pem)?;

    // Build root cert store
    let mut root_store = rustls::RootCertStore::empty();
    for cert in ca_certs {
        root_store
            .add(cert)
            .map_err(|e| TalosError::Tls(format!("Failed to add CA cert: {}", e)))?;
    }

    // Parse client certificate and key
    let client_certs = parse_certificates(client_cert_pem)?;
    let client_key = parse_private_key(client_key_pem)?;

    // Build client config
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_client_auth_cert(client_certs, client_key)
        .map_err(|e| TalosError::Tls(format!("Failed to configure client auth: {}", e)))?;

    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_empty_pem() {
        let result = parse_certificates(b"");
        assert!(result.is_err());
    }
}
