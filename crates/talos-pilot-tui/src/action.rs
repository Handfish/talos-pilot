//! Actions for the talos-pilot TUI
//!
//! Actions represent events that can modify application state.

use talos_pilot_core::{Cluster, Node};

/// Actions that can be dispatched in the application
#[derive(Debug, Clone)]
pub enum Action {
    // Navigation
    Quit,
    Back,
    NavigateUp,
    NavigateDown,
    Select,

    // Data loading
    LoadClusters,
    ClustersLoaded(Vec<Cluster>),
    LoadNodes(String), // cluster name
    NodesLoaded(Vec<Node>),
    LoadError(String),

    // View transitions
    ShowClusterList,
    ShowNodeList(String), // cluster name
    ShowNodeDetails(String, String), // cluster, node
    /// Show multi-service logs: (node_ip, node_role, active_service_ids, all_service_ids)
    ShowMultiLogs(String, String, Vec<String>, Vec<String>),
    /// Show etcd cluster status
    ShowEtcd,
    /// Show processes for a node: (hostname, address)
    ShowProcesses(String, String),
    /// Show network stats for a node: (hostname, address)
    ShowNetwork(String, String),
    /// Show diagnostics for a node: (hostname, address, role)
    ShowDiagnostics(String, String, String),
    /// Apply a diagnostic fix (triggered from confirmation dialog)
    ApplyDiagnosticFix,
    /// Show security/certificates view
    ShowSecurity,
    /// Show lifecycle/version view
    ShowLifecycle,
    /// Show workload health view
    ShowWorkloads,
    /// Show node operations overlay: (hostname, address, is_controlplane)
    ShowNodeOperations(String, String, bool),

    // UI state
    Tick,
    Resize(u16, u16),
    Refresh,

    // Effects
    StartFadeIn,
    StartFadeOut,
}
