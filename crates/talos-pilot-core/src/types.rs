//! Domain types for talos-pilot
//!
//! These types represent the core domain model for Talos clusters.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Cluster representation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cluster {
    pub name: String,
    pub endpoints: Vec<String>,
    pub nodes: Vec<Node>,
    pub health: ClusterHealth,
    pub talos_version: Option<String>,
    pub kubernetes_version: Option<String>,
    pub cert_expiry: Option<DateTime<Utc>>,
}

/// Node representation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub hostname: String,
    pub addresses: Vec<String>,
    pub role: NodeRole,
    pub status: NodeStatus,
    pub talos_version: String,
    pub kubernetes_version: Option<String>,
    pub services: Vec<Service>,
    pub resources: ResourceUsage,
}

/// Node role in the cluster
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NodeRole {
    ControlPlane,
    Worker,
    Unknown,
}

impl std::fmt::Display for NodeRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeRole::ControlPlane => write!(f, "CP"),
            NodeRole::Worker => write!(f, "Worker"),
            NodeRole::Unknown => write!(f, "?"),
        }
    }
}

/// Node health status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeStatus {
    Healthy,
    Degraded { reason: String },
    Unreachable { since: DateTime<Utc> },
    Unknown,
}

impl NodeStatus {
    pub fn is_healthy(&self) -> bool {
        matches!(self, NodeStatus::Healthy)
    }

    pub fn symbol(&self) -> &'static str {
        match self {
            NodeStatus::Healthy => "●",
            NodeStatus::Degraded { .. } => "◐",
            NodeStatus::Unreachable { .. } => "○",
            NodeStatus::Unknown => "?",
        }
    }
}

/// Service running on a node
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Service {
    pub id: String,
    pub state: ServiceState,
    pub health: ServiceHealth,
}

/// Service running state
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServiceState {
    Running,
    Starting,
    Stopping,
    Stopped,
    Failed,
    Unknown,
}

impl std::fmt::Display for ServiceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceState::Running => write!(f, "Running"),
            ServiceState::Starting => write!(f, "Starting"),
            ServiceState::Stopping => write!(f, "Stopping"),
            ServiceState::Stopped => write!(f, "Stopped"),
            ServiceState::Failed => write!(f, "Failed"),
            ServiceState::Unknown => write!(f, "Unknown"),
        }
    }
}

/// Service health information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceHealth {
    pub healthy: bool,
    pub last_check: Option<DateTime<Utc>>,
    pub message: Option<String>,
}

/// Resource usage metrics
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceUsage {
    pub cpu_percent: f32,
    pub memory_used: u64,
    pub memory_total: u64,
    pub load_avg: [f32; 3],
}

impl ResourceUsage {
    pub fn memory_percent(&self) -> f32 {
        if self.memory_total == 0 {
            0.0
        } else {
            (self.memory_used as f32 / self.memory_total as f32) * 100.0
        }
    }
}

/// Overall cluster health
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClusterHealth {
    Healthy,
    Degraded {
        unhealthy_nodes: usize,
        total_nodes: usize,
    },
    Critical {
        reason: String,
    },
    Unknown,
}

impl ClusterHealth {
    pub fn symbol(&self) -> &'static str {
        match self {
            ClusterHealth::Healthy => "●",
            ClusterHealth::Degraded { .. } => "◐",
            ClusterHealth::Critical { .. } => "○",
            ClusterHealth::Unknown => "?",
        }
    }

    pub fn label(&self) -> String {
        match self {
            ClusterHealth::Healthy => "Healthy".to_string(),
            ClusterHealth::Degraded {
                unhealthy_nodes,
                total_nodes,
            } => format!("Degraded ({}/{})", unhealthy_nodes, total_nodes),
            ClusterHealth::Critical { reason } => format!("Critical: {}", reason),
            ClusterHealth::Unknown => "Unknown".to_string(),
        }
    }
}

/// Log line from Talos
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub timestamp: DateTime<Utc>,
    pub node: String,
    pub service: String,
    pub level: LogLevel,
    pub message: String,
}

/// Log level
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LogLevel {
    Debug,
    Info,
    Warning,
    Error,
    Unknown,
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogLevel::Debug => write!(f, "DEBUG"),
            LogLevel::Info => write!(f, "INFO"),
            LogLevel::Warning => write!(f, "WARN"),
            LogLevel::Error => write!(f, "ERROR"),
            LogLevel::Unknown => write!(f, "???"),
        }
    }
}
