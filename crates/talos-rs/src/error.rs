//! Error types for talos-rs

use thiserror::Error;

/// Errors that can occur when interacting with Talos API
#[derive(Error, Debug)]
pub enum TalosError {
    /// Configuration file not found
    #[error("Config file not found: {0}")]
    ConfigNotFound(String),

    /// Failed to parse configuration
    #[error("Failed to parse config: {0}")]
    ConfigParse(#[from] serde_yaml::Error),

    /// Invalid configuration
    #[error("Invalid config: {0}")]
    ConfigInvalid(String),

    /// Context not found in config
    #[error("Context not found: {0}")]
    ContextNotFound(String),

    /// Base64 decoding error
    #[error("Base64 decode error: {0}")]
    Base64Decode(#[from] base64::DecodeError),

    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// TLS error
    #[error("TLS error: {0}")]
    Tls(String),

    /// gRPC transport error
    #[error("Transport error: {0}")]
    Transport(#[from] tonic::transport::Error),

    /// gRPC status error
    #[error("gRPC error: {0}")]
    Grpc(#[from] tonic::Status),

    /// Connection failed
    #[error("Connection failed: {0}")]
    Connection(String),

    /// No home directory found
    #[error("Could not determine home directory")]
    NoHomeDirectory,
}
