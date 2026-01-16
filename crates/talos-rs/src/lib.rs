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
pub mod talosctl;

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
    // Configuration types
    ApplyConfigResult,
    ApplyMode,
    // Connection types
    ConnectionCounts,
    ConnectionInfo,
    ConnectionState,
    // Node info types
    CpuStat,
    // Etcd types
    EtcdAlarm,
    EtcdAlarmType,
    EtcdMemberInfo,
    EtcdMemberStatus,
    MemInfo,
    // Network types
    NetDevRate,
    NetDevStats,
    NetstatFilter,
    NodeConnections,
    NodeCpuInfo,
    NodeLoadAvg,
    NodeMemory,
    NodeNetworkStats,
    // Process types
    NodeProcesses,
    NodeServices,
    NodeSystemStat,
    // Time types
    NodeTimeInfo,
    ProcessInfo,
    ProcessState,
    // Operation types
    RebootMode,
    RebootResult,
    ServiceHealth,
    ServiceInfo,
    ServiceRestartResult,
    TalosClient,
    VersionInfo,
};
pub use config::{Context, TalosConfig};
pub use error::TalosError;
pub use talosctl::{
    AddressStatus, DiscoveryMember, DiskInfo, GenConfigResult, InsecureApplyResult,
    InsecureVersionInfo, KubeSpanPeerStatus, MachineConfigInfo, VolumeStatus,
    apply_config_insecure, check_insecure_connection, gen_config, get_address_status,
    get_discovery_members, get_discovery_members_for_context, get_disks, get_disks_for_context,
    get_disks_for_node, get_disks_insecure, get_kubespan_peers, get_machine_config,
    get_version_insecure, get_volume_status, get_volume_status_for_node,
    get_volume_status_insecure, is_kubespan_enabled, reboot_insecure, shutdown_insecure,
};
