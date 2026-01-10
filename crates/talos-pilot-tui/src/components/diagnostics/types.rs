//! Shared types for the diagnostics system

use ratatui::style::Color;

/// Status of a diagnostic check
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckStatus {
    /// Check passed
    Pass,
    /// Warning condition
    Warn,
    /// Check failed - action may be available
    Fail,
    /// Status unknown or still checking
    Unknown,
    /// Currently checking
    Checking,
}

impl CheckStatus {
    /// Get indicator character and color
    pub fn indicator(&self) -> (&'static str, Color) {
        match self {
            CheckStatus::Pass => ("●", Color::Green),
            CheckStatus::Warn => ("◐", Color::Yellow),
            CheckStatus::Fail => ("✗", Color::Red),
            CheckStatus::Unknown => ("?", Color::DarkGray),
            CheckStatus::Checking => ("◌", Color::Cyan),
        }
    }
}

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
    HostCommand { command: String, description: String },
}

impl FixAction {
    /// Get a short description of the fix
    pub fn description(&self) -> String {
        match self {
            FixAction::AddKernelModule(name) => format!("Add {} kernel module", name),
            FixAction::RestartService(name) => format!("Restart {} service", name),
            FixAction::ApplyConfigPatch { requires_reboot, .. } => {
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
            FixAction::AddKernelModule(_) | FixAction::ApplyConfigPatch { requires_reboot: true, .. }
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

/// Category of diagnostic checks
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckCategory {
    System,
    Kubernetes,
    Services,
    Cni,
    Addons,
}

impl CheckCategory {
    pub fn title(&self) -> &'static str {
        match self {
            CheckCategory::System => "System Health",
            CheckCategory::Kubernetes => "Kubernetes Components",
            CheckCategory::Services => "Services",
            CheckCategory::Cni => "CNI",
            CheckCategory::Addons => "Addons",
        }
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

/// Detected CNI type
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CniType {
    #[default]
    Unknown,
    Flannel,
    Cilium,
    Calico,
    None,
}

impl CniType {
    pub fn name(&self) -> &'static str {
        match self {
            CniType::Unknown => "Unknown",
            CniType::Flannel => "Flannel",
            CniType::Cilium => "Cilium",
            CniType::Calico => "Calico",
            CniType::None => "None",
        }
    }
}

/// Information about a CNI pod
#[derive(Debug, Clone)]
pub struct CniPodInfo {
    /// Pod name
    pub name: String,
    /// Pod phase (Running, Pending, etc.)
    pub phase: String,
    /// Whether pod is ready
    pub ready: bool,
    /// Number of restarts
    pub restart_count: i32,
}

/// CNI information from K8s API
#[derive(Debug, Clone, Default)]
pub struct CniInfo {
    /// Detected CNI type
    pub cni_type: CniType,
    /// CNI pods in kube-system
    pub pods: Vec<CniPodInfo>,
}

impl CniInfo {
    /// Check if all CNI pods are healthy
    pub fn are_pods_healthy(&self) -> bool {
        if self.pods.is_empty() {
            return false;
        }
        self.pods.iter().all(|pod| pod.phase == "Running" && pod.ready)
    }

    /// Get summary of CNI pod health
    pub fn pod_health_summary(&self) -> String {
        if self.pods.is_empty() {
            return "No CNI pods found".to_string();
        }

        let total = self.pods.len();
        let healthy = self.pods.iter().filter(|p| p.phase == "Running" && p.ready).count();
        let total_restarts: i32 = self.pods.iter().map(|p| p.restart_count).sum();

        if healthy == total && total_restarts == 0 {
            format!("{}/{} pods healthy", healthy, total)
        } else if healthy == total {
            format!("{}/{} pods healthy ({} restarts)", healthy, total, total_restarts)
        } else {
            format!("{}/{} pods healthy", healthy, total)
        }
    }
}

/// Context passed to diagnostic providers
#[derive(Clone)]
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
    /// CNI information from K8s API (if available)
    pub cni_info: Option<CniInfo>,
}

impl DiagnosticContext {
    pub fn new() -> Self {
        Self {
            platform: String::new(),
            is_container: false,
            cni_type: CniType::Unknown,
            node_role: String::new(),
            hostname: String::new(),
            cni_info: None,
        }
    }
}

impl Default for DiagnosticContext {
    fn default() -> Self {
        Self::new()
    }
}
