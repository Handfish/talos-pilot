//! mTLS authentication for Talos API
//!
//! Handles certificate loading and TLS configuration for connecting to Talos nodes.

use crate::config::Context;
use crate::error::TalosError;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::sync::Arc;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity};

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

/// Create a TLS-enabled gRPC channel from a talosconfig context
pub async fn create_channel(ctx: &Context) -> Result<Channel, TalosError> {
    let endpoint_url = ctx
        .endpoint_url()
        .ok_or_else(|| TalosError::ConfigInvalid("No endpoints configured".to_string()))?;

    tracing::debug!("Connecting to endpoint: {}", endpoint_url);

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

    // Create TLS config
    let ca = Certificate::from_pem(&ca_pem);
    let identity = Identity::from_pem(&client_cert_pem, &client_key_pem);

    let tls_config = ClientTlsConfig::new().ca_certificate(ca).identity(identity);

    // Build the channel (use connect_lazy to defer TLS handshake)
    let endpoint = Channel::from_shared(endpoint_url.clone())
        .map_err(|e| {
            TalosError::Connection(format!("Invalid endpoint URL '{}': {}", endpoint_url, e))
        })?
        .tls_config(tls_config)
        .map_err(|e| TalosError::Tls(format!("TLS config error: {:?}", e)))?;

    // Use connect_lazy - connection happens on first request
    let channel = endpoint.connect_lazy();

    tracing::debug!("Successfully connected to {}", endpoint_url);
    Ok(channel)
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
