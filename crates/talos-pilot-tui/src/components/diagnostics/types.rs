//! Shared types for the diagnostics system
//!
//! Core types (CheckStatus, CniType, etc.) are imported from talos-pilot-core.
//! This module contains UI-specific types for fixes and diagnostic context.

// Re-export core types for use by diagnostics modules
pub use talos_pilot_core::{
    CheckCategory, CheckStatus, CniInfo, CniPodInfo, CniType, PodHealthInfo, UnhealthyPodInfo,
};

/// Action that can be taken to fix an issue
#[derive(Debug, Clone)]
pub enum FixAction {
    /// Add a kernel module via machine config
    AddKernelModule(String),
    /// Restart a service
    RestartService(String),
    /// Apply a YAML config patch
    ApplyConfigPatch { yaml: String, requires_reboot: bool },
    /// Show more details (navigate to logs, etc.)
    ShowDetails(String),
    /// Install Cilium CNI
    InstallCilium,
    /// Run a command on the host (for Docker environments)
    HostCommand {
        command: String,
        description: String,
    },
}

impl FixAction {
    /// Get a short description of the fix
    pub fn description(&self) -> String {
        match self {
            FixAction::AddKernelModule(name) => format!("Add {} kernel module", name),
            FixAction::RestartService(name) => format!("Restart {} service", name),
            FixAction::ApplyConfigPatch {
                requires_reboot, ..
            } => {
                if *requires_reboot {
                    "Apply config patch (requires reboot)".to_string()
                } else {
                    "Apply config patch".to_string()
                }
            }
            FixAction::ShowDetails(_) => "View details".to_string(),
            FixAction::InstallCilium => "Install Cilium CNI".to_string(),
            FixAction::HostCommand { description, .. } => description.clone(),
        }
    }

    /// Check if this action requires reboot
    pub fn requires_reboot(&self) -> bool {
        matches!(
            self,
            FixAction::AddKernelModule(_)
                | FixAction::ApplyConfigPatch {
                    requires_reboot: true,
                    ..
                }
        )
    }

    /// Check if this is a host command (manual action)
    pub fn is_host_command(&self) -> bool {
        matches!(self, FixAction::HostCommand { .. })
    }
}

/// A diagnostic fix with description
#[derive(Debug, Clone)]
pub struct DiagnosticFix {
    /// Description of what this fix does
    pub description: String,
    /// The action to take
    pub action: FixAction,
}

/// A single diagnostic check result
#[derive(Debug, Clone)]
pub struct DiagnosticCheck {
    /// Unique identifier for this check
    pub id: String,
    /// Display name of the check
    pub name: String,
    /// Current status
    pub status: CheckStatus,
    /// Status message (e.g., "2.1 GB / 4.0 GB")
    pub message: String,
    /// Additional details (shown when selected)
    pub details: Option<String>,
    /// Available fix if status is Fail or Warn
    pub fix: Option<DiagnosticFix>,
}

impl DiagnosticCheck {
    /// Create a new check with Pass status
    pub fn pass(id: &str, name: &str, message: &str) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            status: CheckStatus::Pass,
            message: message.to_string(),
            details: None,
            fix: None,
        }
    }

    /// Create a new check with Fail status and optional fix
    pub fn fail(id: &str, name: &str, message: &str, fix: Option<DiagnosticFix>) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            status: CheckStatus::Fail,
            message: message.to_string(),
            details: None,
            fix,
        }
    }

    /// Create a new check with Warn status
    pub fn warn(id: &str, name: &str, message: &str) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            status: CheckStatus::Warn,
            message: message.to_string(),
            details: None,
            fix: None,
        }
    }

    /// Create a new check with Unknown status
    pub fn unknown(id: &str, name: &str) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            status: CheckStatus::Unknown,
            message: "Unknown".to_string(),
            details: None,
            fix: None,
        }
    }

    /// Set details for this check
    pub fn with_details(mut self, details: &str) -> Self {
        self.details = Some(details.to_string());
        self
    }

    /// Set a fix for this check
    pub fn with_fix(mut self, fix: DiagnosticFix) -> Self {
        self.fix = Some(fix);
        self
    }
}

/// Pending action waiting for confirmation
#[derive(Debug, Clone)]
pub struct PendingAction {
    /// The check this action is for
    pub check_id: String,
    /// The fix to apply
    pub fix: DiagnosticFix,
    /// Preview YAML (for config patches)
    pub preview: Option<String>,
}

/// Context passed to diagnostic providers
#[derive(Debug, Clone)]
pub struct DiagnosticContext {
    /// Platform type (e.g., "container", "metal", "aws")
    pub platform: String,
    /// Whether running in a container environment
    pub is_container: bool,
    /// Detected CNI type
    pub cni_type: CniType,
    /// Node role (controlplane or worker)
    pub node_role: String,
    /// Node hostname (used as container name in Docker)
    pub hostname: String,
    /// Node endpoint (IP address for talosctl commands)
    pub node_endpoint: Option<String>,
    /// Talos context of the node's cluster (`--context` for talosctl commands)
    pub talos_context: Option<String>,
    /// Custom talosconfig path (`--talosconfig` for talosctl commands)
    pub config_path: Option<String>,
    /// CNI information from K8s API (if available)
    pub cni_info: Option<CniInfo>,
    /// Pod health information from K8s API (if available)
    pub pod_health: Option<PodHealthInfo>,
    /// Number of CPU cores (for load threshold scaling)
    pub cpu_count: usize,
    /// K8s client error (if client creation failed)
    pub k8s_error: Option<String>,
}

impl DiagnosticContext {
    pub fn new() -> Self {
        Self {
            platform: String::new(),
            is_container: false,
            cni_type: CniType::Unknown,
            node_role: String::new(),
            hostname: String::new(),
            node_endpoint: None,
            talos_context: None,
            config_path: None,
            cni_info: None,
            pod_health: None,
            cpu_count: 1,
            k8s_error: None,
        }
    }
}

impl Default for DiagnosticContext {
    fn default() -> Self {
        Self::new()
    }
}
