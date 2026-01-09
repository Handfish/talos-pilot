//! talos-rs: Rust SDK for Talos Linux API
//!
//! This crate provides a high-level client for interacting with Talos Linux
//! clusters via their gRPC API.
//!
//! # Example
//!
//! ```no_run
//! use talos_rs::TalosClient;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Connect using default talosconfig
//!     let client = TalosClient::from_default_config().await?;
//!
//!     // Get version info
//!     let versions = client.version().await?;
//!     for v in versions {
//!         println!("{}: {}", v.node, v.version);
//!     }
//!
//!     Ok(())
//! }
//! ```

// TalosError contains tonic types which are large, but this is fine for an API client
// where errors are on the cold path
#![allow(clippy::result_large_err)]

pub mod auth;
pub mod client;
pub mod config;
pub mod error;

/// Generated protobuf types and gRPC clients
pub mod proto {
    pub mod google {
        pub mod rpc {
            include!("generated/google.rpc.rs");
        }
    }

    pub mod common {
        include!("generated/common.rs");
    }

    pub mod machine {
        include!("generated/machine.rs");
    }

    pub mod storage {
        include!("generated/storage.rs");
    }

    pub mod time {
        include!("generated/time.rs");
    }

    pub mod inspect {
        include!("generated/inspect.rs");
    }
}

pub use client::{
    MemInfo, NodeCpuInfo, NodeLoadAvg, NodeMemory, NodeServices, ServiceHealth, ServiceInfo,
    TalosClient, VersionInfo,
};
pub use config::{Context, TalosConfig};
pub use error::TalosError;
