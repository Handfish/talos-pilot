//! Node operations overlay component
//!
//! Shows safety checks and available operations for a selected node.

use crate::action::Action;
use crate::components::Component;
use crate::components::diagnostics::k8s::{
    DrainOptions, PdbHealthInfo, check_pdb_health, create_k8s_client,
};
use crate::ui_ext::SafetyStatusExt;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent};
use kube::Client;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::sync::{Arc, Mutex};
use talos_pilot_core::{AsyncState, SafetyStatus};
use talos_rs::TalosClient;
use tokio::task::JoinHandle;

/// etcd quorum information for the node
#[derive(Debug, Clone, Default)]
pub struct NodeEtcdInfo {
    /// Whether this node is an etcd member
    pub is_member: bool,
    /// Whether this node is the etcd leader
    pub is_leader: bool,
    /// Total etcd members
    pub total_members: usize,
    /// Healthy etcd members
    pub healthy_members: usize,
    /// Quorum needed
    pub quorum_needed: usize,
    /// Members remaining after this node goes down
    pub members_after: usize,
    /// Whether quorum is maintained after this node goes down
    pub quorum_maintained: bool,
}

impl NodeEtcdInfo {
    fn safety_status(&self) -> SafetyStatus {
        if !self.is_member || self.quorum_maintained {
            SafetyStatus::Safe
        } else if self.total_members == 1 {
            SafetyStatus::Unsafe("Single etcd member - quorum will be lost".to_string())
        } else {
            SafetyStatus::Unsafe(format!(
                "Quorum will be lost ({}/{} members after)",
                self.members_after, self.total_members
            ))
        }
    }
}

/// Current operation state
#[derive(Debug, Clone, PartialEq)]
pub enum OperationState {
    /// Showing safety checks, ready for operation selection
    Ready,
    /// Confirming an operation
    Confirming(OperationType),
    /// Executing an operation
    Executing(OperationType, String),
    /// Operation completed
    Completed(OperationType, bool, String),
}

/// Type of operation
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OperationType {
    Drain,
    Reboot,
}

impl OperationType {
    fn name(&self) -> &'static str {
        match self {
            OperationType::Drain => "Drain",
            OperationType::Reboot => "Reboot",
        }
    }
}

/// Result of a background operation
#[derive(Debug, Clone)]
pub struct OperationResult {
    pub success: bool,
    pub message: String,
}

/// Shared progress state for background operations
#[derive(Debug, Clone, Default)]
pub struct OperationProgress {
    pub message: String,
}

/// Run drain operation in background
async fn run_drain_operation(
    progress: Arc<Mutex<OperationProgress>>,
    hostname: String,
    k8s_client: Option<Client>,
    options: DrainOptions,
) -> OperationResult {
    use crate::audit::{audit_failure, audit_start, audit_success};
    use crate::components::diagnostics::k8s::{
        DrainProgressCallback, cordon_node, drain_node_with_progress, uncordon_node,
    };

    audit_start("DRAIN", &hostname, "Starting drain operation");

    let Some(k8s) = k8s_client else {
        audit_failure("DRAIN", &hostname, "No K8s client available");
        return OperationResult {
            success: false,
            message: "No K8s client available".to_string(),
        };
    };

    // Step 1: Cordon the node
    {
        let mut p = progress.lock().unwrap();
        p.message = "Cordoning node...".to_string();
    }

    let cordon_result = cordon_node(&k8s, &hostname).await;
    match cordon_result {
        Ok(result) if result.success => {
            tracing::info!("Cordoned node {}", hostname);
        }
        Ok(result) => {
            return OperationResult {
                success: false,
                message: format!("Failed to cordon: {}", result.error.unwrap_or_default()),
            };
        }
        Err(e) => {
            return OperationResult {
                success: false,
                message: format!("Failed to cordon: {}", e),
            };
        }
    }

    // Step 2: Drain the node with progress updates
    {
        let mut p = progress.lock().unwrap();
        p.message = "Listing pods...".to_string();
    }

    // Create progress callback that updates shared state
    let progress_clone = progress.clone();
    let callback: DrainProgressCallback = Box::new(move |msg: &str| {
        if let Ok(mut p) = progress_clone.lock() {
            p.message = msg.to_string();
        }
    });

    let drain_result = drain_node_with_progress(&k8s, &hostname, &options, Some(callback)).await;
    match drain_result {
        Ok(result) => {
            if result.success {
                let mut msg = format!("Drained {} pods", result.pods_evicted);
                if !result.force_deleted_pods.is_empty() {
                    msg.push_str(&format!(
                        " ({} force-deleted)",
                        result.force_deleted_pods.len()
                    ));
                }
                audit_success("DRAIN", &hostname, &msg);
                OperationResult {
                    success: true,
                    message: msg,
                }
            } else {
                // Drain failed - uncordon the node to restore scheduling
                {
                    let mut p = progress.lock().unwrap();
                    p.message = "Drain failed, uncordoning node...".to_string();
                }
                let _ = uncordon_node(&k8s, &hostname).await;

                let msg = format!(
                    "Evicted {} pods, {} failed (node uncordoned): {}",
                    result.pods_evicted,
                    result.failed_pods.len(),
                    result.failed_pods.join(", ")
                );
                audit_failure("DRAIN", &hostname, &msg);
                OperationResult {
                    success: false,
                    message: msg,
                }
            }
        }
        Err(e) => {
            // Drain error - uncordon the node
            {
                let mut p = progress.lock().unwrap();
                p.message = "Drain error, uncordoning node...".to_string();
            }
            let _ = uncordon_node(&k8s, &hostname).await;

            let msg = format!("Failed to drain (node uncordoned): {}", e);
            audit_failure("DRAIN", &hostname, &msg);
            OperationResult {
                success: false,
                message: msg,
            }
        }
    }
}

/// Run reboot operation in background (cordon, drain, reboot)
async fn run_reboot_operation(
    progress: Arc<Mutex<OperationProgress>>,
    hostname: String,
    address: String,
    k8s_client: Option<Client>,
    talos_client: Option<TalosClient>,
    options: DrainOptions,
) -> OperationResult {
    use crate::audit::{audit_failure, audit_start, audit_success};
    use crate::components::diagnostics::k8s::{
        DrainProgressCallback, cordon_node, drain_node_with_progress, uncordon_node,
    };
    use talos_rs::RebootMode;

    audit_start("REBOOT", &hostname, "Starting reboot operation");

    let Some(k8s) = k8s_client else {
        audit_failure("REBOOT", &hostname, "No K8s client available");
        return OperationResult {
            success: false,
            message: "No K8s client available".to_string(),
        };
    };

    let Some(client) = talos_client else {
        audit_failure("REBOOT", &hostname, "No Talos client available");
        return OperationResult {
            success: false,
            message: "No Talos client available".to_string(),
        };
    };

    // Step 1: Cordon the node
    {
        let mut p = progress.lock().unwrap();
        p.message = "Cordoning node...".to_string();
    }

    let cordon_result = cordon_node(&k8s, &hostname).await;
    match cordon_result {
        Ok(result) if result.success => {
            tracing::info!("Cordoned node {}", hostname);
        }
        Ok(result) => {
            return OperationResult {
                success: false,
                message: format!("Failed to cordon: {}", result.error.unwrap_or_default()),
            };
        }
        Err(e) => {
            return OperationResult {
                success: false,
                message: format!("Failed to cordon: {}", e),
            };
        }
    }

    // Step 2: Drain the node with progress updates
    {
        let mut p = progress.lock().unwrap();
        p.message = "Listing pods...".to_string();
    }

    // Create progress callback that updates shared state
    let progress_clone = progress.clone();
    let callback: DrainProgressCallback = Box::new(move |msg: &str| {
        if let Ok(mut p) = progress_clone.lock() {
            p.message = msg.to_string();
        }
    });

    let drain_result = drain_node_with_progress(&k8s, &hostname, &options, Some(callback)).await;
    match drain_result {
        Ok(result) if !result.success => {
            // Drain had failures - uncordon and abort reboot
            {
                let mut p = progress.lock().unwrap();
                p.message = "Drain failed, uncordoning node...".to_string();
            }
            let _ = uncordon_node(&k8s, &hostname).await;

            let msg = format!(
                "Reboot aborted - drain failed (node uncordoned): {}",
                result.failed_pods.join(", ")
            );
            audit_failure("REBOOT", &hostname, &msg);
            return OperationResult {
                success: false,
                message: msg,
            };
        }
        Err(e) => {
            // Drain error - uncordon and abort
            {
                let mut p = progress.lock().unwrap();
                p.message = "Drain error, uncordoning node...".to_string();
            }
            let _ = uncordon_node(&k8s, &hostname).await;

            let msg = format!("Reboot aborted - drain failed (node uncordoned): {}", e);
            audit_failure("REBOOT", &hostname, &msg);
            return OperationResult {
                success: false,
                message: msg,
            };
        }
        _ => {}
    }

    // Step 3: Reboot the node via Talos API
    {
        let mut p = progress.lock().unwrap();
        p.message = "Sending reboot command...".to_string();
    }

    let node_client = client.with_node(&address);
    match node_client.reboot(RebootMode::Default).await {
        Ok(result) if !result.success => {
            // Reboot request failed - uncordon
            {
                let mut p = progress.lock().unwrap();
                p.message = "Reboot failed, uncordoning node...".to_string();
            }
            let _ = uncordon_node(&k8s, &hostname).await;

            let msg = "Reboot request failed (node uncordoned)".to_string();
            audit_failure("REBOOT", &hostname, &msg);
            return OperationResult {
                success: false,
                message: msg,
            };
        }
        Err(e) => {
            // Reboot failed - uncordon the node
            {
                let mut p = progress.lock().unwrap();
                p.message = "Reboot failed, uncordoning node...".to_string();
            }
            let _ = uncordon_node(&k8s, &hostname).await;

            let msg = format!("Failed to reboot (node uncordoned): {}", e);
            audit_failure("REBOOT", &hostname, &msg);
            return OperationResult {
                success: false,
                message: msg,
            };
        }
        _ => {}
    }

    // Step 4: Wait for node to come back (if configured)
    if options.wait_for_node_ready {
        use crate::components::diagnostics::k8s::{NodeReadyProgressCallback, wait_for_node_ready};

        let progress_clone = progress.clone();
        let ready_callback: NodeReadyProgressCallback = Box::new(move |msg: &str| {
            if let Ok(mut p) = progress_clone.lock() {
                p.message = msg.to_string();
            }
        });

        let ready_result = wait_for_node_ready(
            &k8s,
            &hostname,
            options.post_reboot_timeout_secs,
            true, // wait for disconnect first
            Some(ready_callback),
        )
        .await;

        match ready_result {
            Ok(result) if result.success => {
                // Node is back and Ready
                if options.uncordon_after_reboot {
                    {
                        let mut p = progress.lock().unwrap();
                        p.message = "Uncordoning node...".to_string();
                    }

                    if let Err(e) = uncordon_node(&k8s, &hostname).await {
                        let msg = format!(
                            "Reboot completed ({}s), but failed to uncordon: {}",
                            result.time_taken_secs, e
                        );
                        audit_success("REBOOT", &hostname, &msg);
                        return OperationResult {
                            success: true, // Reboot succeeded, just uncordon failed
                            message: msg,
                        };
                    }

                    let msg = format!(
                        "Reboot completed successfully ({}s), node uncordoned",
                        result.time_taken_secs
                    );
                    audit_success("REBOOT", &hostname, &msg);
                    return OperationResult {
                        success: true,
                        message: msg,
                    };
                } else {
                    let msg = format!(
                        "Reboot completed successfully ({}s), node still cordoned",
                        result.time_taken_secs
                    );
                    audit_success("REBOOT", &hostname, &msg);
                    return OperationResult {
                        success: true,
                        message: msg,
                    };
                }
            }
            Ok(result) => {
                // Timeout waiting for node
                let msg = format!(
                    "Reboot initiated but node didn't become Ready: {}",
                    result.error.unwrap_or_default()
                );
                audit_failure("REBOOT", &hostname, &msg);
                return OperationResult {
                    success: false,
                    message: msg,
                };
            }
            Err(e) => {
                let msg = format!("Reboot initiated but error checking node status: {}", e);
                audit_failure("REBOOT", &hostname, &msg);
                return OperationResult {
                    success: false,
                    message: msg,
                };
            }
        }
    }

    // No post-reboot verification - just report success
    let msg = "Reboot initiated (verification disabled)".to_string();
    audit_success("REBOOT", &hostname, &msg);
    OperationResult {
        success: true,
        message: msg,
    }
}

/// Async-loaded data for node operations
#[derive(Debug, Clone, Default)]
pub struct NodeOperationsData {
    /// etcd info for this node
    pub etcd_info: Option<NodeEtcdInfo>,
    /// PDB health info
    pub pdb_info: Option<PdbHealthInfo>,
    /// Overall reboot safety status
    pub reboot_safety: SafetyStatus,
    /// Overall drain safety status
    pub drain_safety: SafetyStatus,
}

/// Node operations overlay component
pub struct NodeOperationsComponent {
    /// Node hostname
    hostname: String,
    /// Node IP address
    address: String,
    /// Whether this is a control plane node
    is_controlplane: bool,

    /// Talos client for API calls
    client: Option<TalosClient>,
    /// K8s client for PDB checks
    k8s_client: Option<Client>,

    /// Async state for loaded data
    state: AsyncState<NodeOperationsData>,

    /// Selected operation index
    selected_op: usize,

    /// Current operation state
    operation_state: OperationState,

    /// Background task handle for operations
    operation_task: Option<JoinHandle<OperationResult>>,
    /// Shared progress state for background operations
    operation_progress: Arc<Mutex<OperationProgress>>,

    /// Drain options (configurable timeouts, force delete, etc.)
    drain_options: DrainOptions,
}

impl Default for NodeOperationsComponent {
    fn default() -> Self {
        Self::new("".to_string(), "".to_string(), false)
    }
}

impl NodeOperationsComponent {
    pub fn new(hostname: String, address: String, is_controlplane: bool) -> Self {
        Self {
            hostname,
            address,
            is_controlplane,
            client: None,
            k8s_client: None,
            state: AsyncState::new(),
            selected_op: 0,
            operation_state: OperationState::Ready,
            operation_task: None,
            operation_progress: Arc::new(Mutex::new(OperationProgress::default())),
            drain_options: DrainOptions::default(),
        }
    }

    /// Get a reference to the loaded data
    fn data(&self) -> Option<&NodeOperationsData> {
        self.state.data()
    }

    /// Get a mutable reference to the loaded data
    fn data_mut(&mut self) -> Option<&mut NodeOperationsData> {
        self.state.data_mut()
    }

    /// Get a mutable reference to drain options for configuration
    pub fn drain_options_mut(&mut self) -> &mut DrainOptions {
        &mut self.drain_options
    }

    /// Set the Talos client
    pub fn set_client(&mut self, client: TalosClient) {
        self.client = Some(client);
    }

    /// Set error message
    pub fn set_error(&mut self, error: String) {
        self.state.set_error(error);
    }

    /// Poll background operation and update progress
    pub fn poll_operation(&mut self) {
        // Check if there's a running task
        if let Some(task) = &mut self.operation_task {
            // Check if completed (non-blocking)
            if task.is_finished() {
                // Take ownership of the task
                let task = self.operation_task.take().unwrap();

                // Get result (this won't block since is_finished() was true)
                match futures::executor::block_on(task) {
                    Ok(result) => {
                        if let OperationState::Executing(op_type, _) = &self.operation_state {
                            self.operation_state =
                                OperationState::Completed(*op_type, result.success, result.message);
                        }
                    }
                    Err(e) => {
                        if let OperationState::Executing(op_type, _) = &self.operation_state {
                            self.operation_state = OperationState::Completed(
                                *op_type,
                                false,
                                format!("Task error: {}", e),
                            );
                        }
                    }
                }
            } else {
                // Task still running - update progress message from shared state
                if let OperationState::Executing(op_type, _) = &self.operation_state {
                    let progress = self.operation_progress.lock().unwrap();
                    self.operation_state =
                        OperationState::Executing(*op_type, progress.message.clone());
                }
            }
        }
    }

    /// Start a background operation
    pub fn start_operation(&mut self, op_type: OperationType) {
        // Reset progress
        {
            let mut progress = self.operation_progress.lock().unwrap();
            progress.message = "Starting...".to_string();
        }

        let progress = self.operation_progress.clone();
        let hostname = self.hostname.clone();
        let address = self.address.clone();
        let k8s_client = self.k8s_client.clone();
        let talos_client = self.client.clone();
        let drain_options = self.drain_options.clone();

        let task = tokio::spawn(async move {
            match op_type {
                OperationType::Drain => {
                    run_drain_operation(progress, hostname, k8s_client, drain_options).await
                }
                OperationType::Reboot => {
                    run_reboot_operation(
                        progress,
                        hostname,
                        address,
                        k8s_client,
                        talos_client,
                        drain_options,
                    )
                    .await
                }
            }
        });

        self.operation_task = Some(task);
        self.operation_state = OperationState::Executing(op_type, "Starting...".to_string());
    }

    /// Refresh safety check data
    pub async fn refresh(&mut self) -> Result<()> {
        // If we're executing, just poll the operation
        if matches!(self.operation_state, OperationState::Executing(_, _)) {
            self.poll_operation();
            return Ok(());
        }

        self.state.start_loading();

        // Ensure we have data to update
        if self.state.data().is_none() {
            self.state.set_data(NodeOperationsData::default());
        }

        let Some(client) = self.client.clone() else {
            self.state.set_error("No client configured");
            return Ok(());
        };

        // Fetch etcd info for control plane nodes
        if self.is_controlplane {
            self.fetch_etcd_info(&client).await;
        } else {
            // Worker nodes are not etcd members
            if let Some(data) = self.data_mut() {
                data.etcd_info = Some(NodeEtcdInfo {
                    is_member: false,
                    ..Default::default()
                });
            }
        }

        // Fetch PDB info
        self.fetch_pdb_info(&client).await;

        // Calculate overall safety status
        self.calculate_safety_status();

        self.state.mark_loaded();
        Ok(())
    }

    /// Fetch etcd information
    async fn fetch_etcd_info(&mut self, client: &TalosClient) {
        // Get etcd members
        match client.etcd_members().await {
            Ok(members) => {
                let total = members.len();
                let quorum_needed = total / 2 + 1;

                // Check if this node is a member
                let node_addr = self.address.split(':').next().unwrap_or(&self.address);
                let is_member = members
                    .iter()
                    .any(|m| m.peer_urls.iter().any(|url| url.contains(node_addr)));

                // Extract control plane IPs to target status calls (hostnames may not be resolvable)
                let cp_ips: Vec<String> = members.iter().filter_map(|m| m.ip_address()).collect();

                // Get status from all control planes (single call for both is_leader and healthy)
                let statuses = client
                    .etcd_status_for_nodes(&cp_ips)
                    .await
                    .unwrap_or_default();

                // Check if this node is the leader
                let is_leader = statuses.iter().any(|s| {
                    let member = members.iter().find(|m| m.id == s.member_id);
                    if let Some(m) = member {
                        m.peer_urls.iter().any(|url| url.contains(node_addr)) && s.is_leader()
                    } else {
                        false
                    }
                });

                // Calculate members after this node goes down
                let members_after = if is_member { total - 1 } else { total };
                let quorum_maintained = members_after >= quorum_needed;

                // Count healthy members (those with status)
                // If no statuses returned, assume all healthy
                let healthy = if statuses.is_empty() {
                    total
                } else {
                    members
                        .iter()
                        .filter(|m| statuses.iter().any(|s| s.member_id == m.id))
                        .count()
                };

                if let Some(data) = self.data_mut() {
                    data.etcd_info = Some(NodeEtcdInfo {
                        is_member,
                        is_leader,
                        total_members: total,
                        healthy_members: healthy,
                        quorum_needed,
                        members_after,
                        quorum_maintained,
                    });
                }
            }
            Err(e) => {
                tracing::warn!("Failed to fetch etcd members: {}", e);
                if let Some(data) = self.data_mut() {
                    data.etcd_info = None;
                }
            }
        }
    }

    /// Fetch PDB information
    async fn fetch_pdb_info(&mut self, client: &TalosClient) {
        // Initialize K8s client if needed
        if self.k8s_client.is_none() {
            match create_k8s_client(client).await {
                Ok(k8s) => {
                    self.k8s_client = Some(k8s);
                }
                Err(e) => {
                    tracing::warn!("Failed to create K8s client: {}", e);
                    return;
                }
            }
        }

        // Fetch PDB info
        if let Some(k8s) = &self.k8s_client {
            match check_pdb_health(k8s).await {
                Ok(info) => {
                    if let Some(data) = self.data_mut() {
                        data.pdb_info = Some(info);
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to fetch PDB info: {}", e);
                }
            }
        }
    }

    /// Calculate overall safety status
    fn calculate_safety_status(&mut self) {
        let Some(data) = self.data_mut() else { return };

        // Reboot safety = etcd safety (for CP) + drain safety
        let etcd_safety = data
            .etcd_info
            .as_ref()
            .map(|e| e.safety_status())
            .unwrap_or(SafetyStatus::Unknown);

        // Drain safety = PDB check
        data.drain_safety = if let Some(ref pdb) = data.pdb_info {
            if pdb.has_blocking_pdbs() {
                SafetyStatus::Warning(format!(
                    "{} PDB(s) would block drain",
                    pdb.blocking_pdbs.len()
                ))
            } else {
                SafetyStatus::Safe
            }
        } else {
            SafetyStatus::Unknown
        };

        // Reboot safety combines etcd and drain
        data.reboot_safety = match (&etcd_safety, &data.drain_safety) {
            (SafetyStatus::Unsafe(msg), _) => SafetyStatus::Unsafe(msg.clone()),
            (_, SafetyStatus::Unsafe(msg)) => SafetyStatus::Unsafe(msg.clone()),
            (SafetyStatus::Warning(msg), _) => SafetyStatus::Warning(msg.clone()),
            (_, SafetyStatus::Warning(msg)) => SafetyStatus::Warning(msg.clone()),
            (SafetyStatus::Safe, SafetyStatus::Safe) => SafetyStatus::Safe,
            _ => SafetyStatus::Unknown,
        };
    }

    /// Draw the overlay
    fn draw_overlay(&self, frame: &mut Frame, area: Rect) {
        // Handle different states with different overlays
        match &self.operation_state {
            OperationState::Confirming(op_type) => {
                self.draw_confirmation_dialog(frame, area, *op_type);
            }
            OperationState::Executing(op_type, msg) => {
                self.draw_executing_dialog(frame, area, *op_type, msg);
            }
            OperationState::Completed(op_type, success, msg) => {
                self.draw_completed_dialog(frame, area, *op_type, *success, msg);
            }
            OperationState::Ready => {
                self.draw_ready_overlay(frame, area);
            }
        }
    }

    /// Draw confirmation dialog
    fn draw_confirmation_dialog(&self, frame: &mut Frame, area: Rect, op_type: OperationType) {
        let overlay_width = 50.min(area.width.saturating_sub(4));
        let overlay_height = 10.min(area.height.saturating_sub(4));
        let x = (area.width.saturating_sub(overlay_width)) / 2;
        let y = (area.height.saturating_sub(overlay_height)) / 2;
        let overlay_area = Rect::new(x, y, overlay_width, overlay_height);

        frame.render_widget(Clear, overlay_area);

        let mut lines = Vec::new();
        lines.push(Line::from(""));

        let op_name = op_type.name();
        let warning_color = match op_type {
            OperationType::Reboot => Color::Red,
            OperationType::Drain => Color::Yellow,
        };

        lines.push(Line::from(vec![Span::styled(
            format!("  {} node '{}'?", op_name, self.hostname),
            Style::default()
                .fg(warning_color)
                .add_modifier(Modifier::BOLD),
        )]));

        lines.push(Line::from(""));

        // Show what will happen
        match op_type {
            OperationType::Reboot => {
                lines.push(Line::from(vec![Span::raw("  This will:")]));
                lines.push(Line::from(vec![Span::styled(
                    "    1. Cordon the node",
                    Style::default().fg(Color::DarkGray),
                )]));
                lines.push(Line::from(vec![Span::styled(
                    "    2. Drain all pods",
                    Style::default().fg(Color::DarkGray),
                )]));
                lines.push(Line::from(vec![Span::styled(
                    "    3. Reboot via Talos API",
                    Style::default().fg(Color::DarkGray),
                )]));
            }
            OperationType::Drain => {
                lines.push(Line::from(vec![Span::raw("  This will:")]));
                lines.push(Line::from(vec![Span::styled(
                    "    1. Cordon the node",
                    Style::default().fg(Color::DarkGray),
                )]));
                lines.push(Line::from(vec![Span::styled(
                    "    2. Evict all pods",
                    Style::default().fg(Color::DarkGray),
                )]));
            }
        }

        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                "  [y]",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Confirm    "),
            Span::styled(
                "[n]",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Cancel"),
        ]));

        let title = format!(" Confirm {} ", op_name);
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(warning_color));

        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, overlay_area);
    }

    /// Draw executing dialog
    fn draw_executing_dialog(
        &self,
        frame: &mut Frame,
        area: Rect,
        op_type: OperationType,
        msg: &str,
    ) {
        let overlay_width = 50.min(area.width.saturating_sub(4));
        let overlay_height = 7.min(area.height.saturating_sub(4));
        let x = (area.width.saturating_sub(overlay_width)) / 2;
        let y = (area.height.saturating_sub(overlay_height)) / 2;
        let overlay_area = Rect::new(x, y, overlay_width, overlay_height);

        frame.render_widget(Clear, overlay_area);

        let mut lines = Vec::new();
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            format!("  {} {}...", op_type.name(), self.hostname),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            format!("  {}", msg),
            Style::default().fg(Color::Cyan),
        )]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "  Please wait...",
            Style::default().fg(Color::DarkGray),
        )]));

        let title = format!(" {} in Progress ", op_type.name());
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow));

        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, overlay_area);
    }

    /// Draw completed dialog
    fn draw_completed_dialog(
        &self,
        frame: &mut Frame,
        area: Rect,
        op_type: OperationType,
        success: bool,
        msg: &str,
    ) {
        let overlay_width = 50.min(area.width.saturating_sub(4));
        let overlay_height = 8.min(area.height.saturating_sub(4));
        let x = (area.width.saturating_sub(overlay_width)) / 2;
        let y = (area.height.saturating_sub(overlay_height)) / 2;
        let overlay_area = Rect::new(x, y, overlay_width, overlay_height);

        frame.render_widget(Clear, overlay_area);

        let (status_icon, status_color, status_text) = if success {
            ("✓", Color::Green, "Completed")
        } else {
            ("✗", Color::Red, "Failed")
        };

        let mut lines = Vec::new();
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            format!("  {} {} {}", status_icon, op_type.name(), status_text),
            Style::default()
                .fg(status_color)
                .add_modifier(Modifier::BOLD),
        )]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            format!("  Node: {}", self.hostname),
            Style::default().fg(Color::Cyan),
        )]));
        lines.push(Line::from(vec![Span::raw(format!("  {}", msg))]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "  Press any key to continue...",
            Style::default().fg(Color::DarkGray),
        )]));

        let title = format!(" {} {} ", op_type.name(), status_text);
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(status_color));

        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, overlay_area);
    }

    /// Draw the ready state overlay (safety checks and operation selection)
    fn draw_ready_overlay(&self, frame: &mut Frame, area: Rect) {
        // Calculate centered overlay position
        let overlay_width = 50.min(area.width.saturating_sub(4));
        let overlay_height = 18.min(area.height.saturating_sub(4));
        let x = (area.width.saturating_sub(overlay_width)) / 2;
        let y = (area.height.saturating_sub(overlay_height)) / 2;

        let overlay_area = Rect::new(x, y, overlay_width, overlay_height);

        // Clear the background
        frame.render_widget(Clear, overlay_area);

        // Get data for rendering
        let data = self.data();

        // Build content
        let mut lines = Vec::new();

        // Node info header
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw("  Node: "),
            Span::styled(&self.hostname, Style::default().fg(Color::Cyan)),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  Role: "),
            Span::styled(
                if self.is_controlplane {
                    "controlplane"
                } else {
                    "worker"
                },
                Style::default().fg(if self.is_controlplane {
                    Color::Yellow
                } else {
                    Color::Green
                }),
            ),
        ]));

        // etcd info (for control plane)
        if self.is_controlplane {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![Span::styled(
                "  etcd Status",
                Style::default().fg(Color::Yellow),
            )]));

            if let Some(etcd) = data.and_then(|d| d.etcd_info.as_ref()) {
                let (indicator, color) = etcd.safety_status().indicator_with_color();
                let role = if etcd.is_leader { "leader" } else { "member" };

                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(indicator, Style::default().fg(color)),
                    Span::raw(format!(
                        " {}/{} members ({})",
                        etcd.healthy_members, etcd.total_members, role
                    )),
                ]));

                if etcd.is_member {
                    let after_text = if etcd.quorum_maintained {
                        format!(
                            "After: {}/{} (quorum OK)",
                            etcd.members_after, etcd.total_members
                        )
                    } else {
                        format!(
                            "After: {}/{} (QUORUM LOST)",
                            etcd.members_after, etcd.total_members
                        )
                    };
                    let after_color = if etcd.quorum_maintained {
                        Color::Green
                    } else {
                        Color::Red
                    };
                    lines.push(Line::from(vec![
                        Span::raw("    "),
                        Span::styled(after_text, Style::default().fg(after_color)),
                    ]));
                }
            } else {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(
                        "? Unable to fetch etcd status",
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
        }

        // PDB info
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "  Drain Safety",
            Style::default().fg(Color::Yellow),
        )]));

        if let Some(pdb) = data.and_then(|d| d.pdb_info.as_ref()) {
            let drain_safety = data
                .map(|d| &d.drain_safety)
                .unwrap_or(&SafetyStatus::Unknown);
            let (indicator, color) = drain_safety.indicator_with_color();
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(indicator, Style::default().fg(color)),
                Span::raw(format!(" {}", pdb.summary())),
            ]));

            // List blocking PDBs if any
            for blocking in pdb.blocking_pdbs.iter().take(2) {
                lines.push(Line::from(vec![
                    Span::raw("      "),
                    Span::styled(
                        format!("- {}/{}", blocking.namespace, blocking.name),
                        Style::default().fg(Color::Yellow),
                    ),
                ]));
            }
            if pdb.blocking_pdbs.len() > 2 {
                lines.push(Line::from(vec![
                    Span::raw("      "),
                    Span::styled(
                        format!("  ... and {} more", pdb.blocking_pdbs.len() - 2),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
        } else {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(
                    "? Unable to fetch PDB status",
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }

        // Operations section
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "  Operations",
            Style::default().fg(Color::Yellow),
        )]));

        // Reboot operation
        let reboot_safety = data
            .map(|d| &d.reboot_safety)
            .unwrap_or(&SafetyStatus::Unknown);
        let (reboot_ind, reboot_color) = reboot_safety.indicator_with_color();
        let reboot_style = if self.selected_op == 0 {
            Style::default()
                .fg(reboot_color)
                .add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(reboot_color)
        };
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(reboot_ind, Style::default().fg(reboot_color)),
            Span::styled(" [r] Reboot (with drain)", reboot_style),
        ]));

        // Drain operation
        let drain_safety = data
            .map(|d| &d.drain_safety)
            .unwrap_or(&SafetyStatus::Unknown);
        let (drain_ind, drain_color) = drain_safety.indicator_with_color();
        let drain_style = if self.selected_op == 1 {
            Style::default()
                .fg(drain_color)
                .add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(drain_color)
        };
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(drain_ind, Style::default().fg(drain_color)),
            Span::styled(" [d] Drain only", drain_style),
        ]));

        // Footer
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "  [q] Cancel",
            Style::default().fg(Color::DarkGray),
        )]));

        // Render
        let title = format!(" Node: {} ", self.hostname);
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));

        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, overlay_area);
    }
}

impl Component for NodeOperationsComponent {
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match &self.operation_state {
            OperationState::Ready => {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => Ok(Some(Action::Back)),
                    KeyCode::Up | KeyCode::Char('k') => {
                        if self.selected_op > 0 {
                            self.selected_op -= 1;
                        }
                        Ok(None)
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if self.selected_op < 1 {
                            self.selected_op += 1;
                        }
                        Ok(None)
                    }
                    KeyCode::Enter => {
                        // Trigger the selected operation
                        let reboot_unsafe = self
                            .data()
                            .map(|d| matches!(d.reboot_safety, SafetyStatus::Unsafe(_)))
                            .unwrap_or(true);
                        match self.selected_op {
                            0 => {
                                // Reboot
                                if reboot_unsafe {
                                    tracing::warn!("Reboot blocked due to unsafe status");
                                    Ok(None)
                                } else {
                                    self.operation_state =
                                        OperationState::Confirming(OperationType::Reboot);
                                    Ok(None)
                                }
                            }
                            1 => {
                                // Drain
                                self.operation_state =
                                    OperationState::Confirming(OperationType::Drain);
                                Ok(None)
                            }
                            _ => Ok(None),
                        }
                    }
                    KeyCode::Char('r') => {
                        // Check safety before allowing
                        let reboot_unsafe = self
                            .data()
                            .map(|d| matches!(d.reboot_safety, SafetyStatus::Unsafe(_)))
                            .unwrap_or(true);
                        if reboot_unsafe {
                            // Don't allow unsafe operations without explicit override
                            tracing::warn!("Reboot blocked due to unsafe status");
                            Ok(None)
                        } else {
                            self.operation_state =
                                OperationState::Confirming(OperationType::Reboot);
                            Ok(None)
                        }
                    }
                    KeyCode::Char('d') => {
                        self.operation_state = OperationState::Confirming(OperationType::Drain);
                        Ok(None)
                    }
                    _ => Ok(None),
                }
            }
            OperationState::Confirming(op_type) => {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                        // User confirmed, start the operation in background
                        let op = *op_type;
                        self.start_operation(op);
                        Ok(None)
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                        self.operation_state = OperationState::Ready;
                        Ok(None)
                    }
                    _ => Ok(None),
                }
            }
            OperationState::Executing(_, _) => {
                // Poll for progress updates while executing
                self.poll_operation();
                Ok(None)
            }
            OperationState::Completed(_, _, _) => {
                // Any key returns to close the dialog
                Ok(Some(Action::Back))
            }
        }
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        // Poll operation on every tick while executing
        if matches!(action, Action::Tick)
            && matches!(self.operation_state, OperationState::Executing(_, _))
        {
            self.poll_operation();
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        if self.state.is_loading() && self.data().map(|d| d.etcd_info.is_none()).unwrap_or(true) {
            // Show loading in overlay
            let overlay_width = 40.min(area.width.saturating_sub(4));
            let overlay_height = 5;
            let x = (area.width.saturating_sub(overlay_width)) / 2;
            let y = (area.height.saturating_sub(overlay_height)) / 2;
            let overlay_area = Rect::new(x, y, overlay_width, overlay_height);

            frame.render_widget(Clear, overlay_area);

            let block = Block::default()
                .title(" Node Operations ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan));

            let loading = Paragraph::new("  Loading safety checks...")
                .style(Style::default().fg(Color::DarkGray))
                .block(block);

            frame.render_widget(loading, overlay_area);
            return Ok(());
        }

        self.draw_overlay(frame, area);
        Ok(())
    }
}
