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
    /// Show multi-service logs: (node_ip, node_role, service_ids)
    ShowMultiLogs(String, String, Vec<String>),
    /// Show etcd cluster status
    ShowEtcd,

    // UI state
    Tick,
    Resize(u16, u16),
    Refresh,

    // Effects
    StartFadeIn,
    StartFadeOut,
}
