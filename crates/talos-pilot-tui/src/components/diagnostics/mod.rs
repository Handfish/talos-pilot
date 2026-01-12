//! Diagnostics component for Talos node health checks
//!
//! Provides system health checks, Kubernetes component status,
//! and actionable fixes for common issues.
//!
//! Architecture:
//! - `core.rs` - Core checks that run on any Talos cluster
//! - `cni/` - CNI-specific checks (Flannel, Cilium, Calico)
//! - `addons/` - Addon-specific checks (cert-manager, etc.)
//! - `types.rs` - Shared types

pub mod addons;
pub mod cni;
pub mod core;
pub mod k8s;
pub mod pki;
pub mod types;

use crate::action::Action;
use crate::components::Component;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Row, Table, TableState},
};
use std::time::{Duration, Instant};
use talos_pilot_core::AsyncState;
use talos_rs::{ApplyConfigResult, ApplyMode, TalosClient};

use crate::ui_ext::CheckStatusExt;
pub use types::*;

/// Default auto-refresh interval in seconds
const AUTO_REFRESH_INTERVAL_SECS: u64 = 10;

/// Data loaded asynchronously for the diagnostics component
#[derive(Debug, Clone, Default)]
pub struct DiagnosticsData {
    /// Node hostname
    pub hostname: String,
    /// Node IP address
    pub address: String,

    /// Diagnostic context (platform, CNI type, etc.)
    pub context: DiagnosticContext,

    /// System health checks
    pub system_checks: Vec<DiagnosticCheck>,
    /// Kubernetes component checks
    pub kubernetes_checks: Vec<DiagnosticCheck>,
    /// Service health checks
    pub service_checks: Vec<DiagnosticCheck>,
    /// CNI-specific checks
    pub cni_checks: Vec<DiagnosticCheck>,
    /// Addon-specific checks
    pub addon_checks: Vec<DiagnosticCheck>,
    /// Detected addons
    pub detected_addons: addons::DetectedAddons,
}

/// Diagnostics component for node health checks
pub struct DiagnosticsComponent {
    /// Async state for loaded data
    state: AsyncState<DiagnosticsData>,

    /// Currently selected category
    selected_category: usize,
    /// Currently selected check within category
    selected_check: usize,
    /// Table state for rendering
    table_state: TableState,

    /// Pending action (waiting for confirmation)
    pending_action: Option<PendingAction>,
    /// Whether we're in the confirmation dialog
    show_confirmation: bool,
    /// Confirmation dialog selection (0 = Cancel, 1 = Apply; for host commands: 0 = Copy, 1 = Close)
    confirmation_selection: usize,
    /// Time when command was copied (for showing feedback)
    copy_feedback_until: Option<Instant>,

    /// Whether we're showing a details popup (for checks without fixes)
    show_details: bool,
    /// Title of the details popup
    details_title: String,
    /// Content of the details popup
    details_content: String,

    /// Whether we're applying a fix
    applying_fix: bool,
    /// Result of the last apply
    apply_result: Option<Result<Vec<ApplyConfigResult>, String>>,

    /// Auto-refresh enabled
    auto_refresh: bool,

    /// Client for API calls
    client: Option<TalosClient>,
    /// Control plane endpoint for fetching kubeconfig (used for worker nodes)
    controlplane_endpoint: Option<String>,
}

impl Default for DiagnosticsComponent {
    fn default() -> Self {
        Self::new("".to_string(), "".to_string(), "".to_string())
    }
}

impl DiagnosticsComponent {
    pub fn new(hostname: String, address: String, node_role: String) -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));

        let mut context = DiagnosticContext::new();
        context.node_role = node_role.clone();
        context.hostname = hostname.clone();
        context.node_endpoint = Some(address.clone());

        let initial_data = DiagnosticsData {
            hostname,
            address,
            context,
            ..Default::default()
        };

        Self {
            state: AsyncState::with_data(initial_data),
            selected_category: 0,
            selected_check: 0,
            table_state,
            pending_action: None,
            show_confirmation: false,
            confirmation_selection: 1,
            copy_feedback_until: None,
            show_details: false,
            details_title: String::new(),
            details_content: String::new(),
            applying_fix: false,
            apply_result: None,
            auto_refresh: true,
            client: None,
            controlplane_endpoint: None,
        }
    }

    /// Access loaded data immutably
    fn data(&self) -> Option<&DiagnosticsData> {
        self.state.data()
    }

    /// Access loaded data mutably
    fn data_mut(&mut self) -> Option<&mut DiagnosticsData> {
        self.state.data_mut()
    }

    /// Set the client for making API calls
    pub fn set_client(&mut self, client: TalosClient) {
        self.client = Some(client);
    }

    /// Set the control plane endpoint for fetching kubeconfig (used for worker nodes)
    pub fn set_controlplane_endpoint(&mut self, endpoint: Option<String>) {
        self.controlplane_endpoint = endpoint;
    }

    /// Set an error message
    pub fn set_error(&mut self, error: String) {
        self.state.set_error(error);
    }

    /// Get all checks in the current category
    fn current_checks(&self) -> &[DiagnosticCheck] {
        let Some(data) = self.data() else {
            return &[];
        };
        match self.selected_category {
            0 => &data.system_checks,
            1 => &data.kubernetes_checks,
            2 => &data.cni_checks,
            3 => &data.service_checks,
            4 => &data.addon_checks,
            _ => &[],
        }
    }

    /// Get the currently selected check
    fn selected_check(&self) -> Option<&DiagnosticCheck> {
        self.current_checks().get(self.selected_check)
    }

    /// Get total number of categories
    fn category_count(&self) -> usize {
        5 // System, Kubernetes, CNI, Services, Addons
    }

    /// Select next category
    fn next_category(&mut self) {
        self.selected_category = (self.selected_category + 1) % self.category_count();
        self.selected_check = 0;
        self.update_table_state();
    }

    /// Select previous category
    fn prev_category(&mut self) {
        self.selected_category = if self.selected_category == 0 {
            self.category_count() - 1
        } else {
            self.selected_category - 1
        };
        self.selected_check = 0;
        self.update_table_state();
    }

    /// Select next check in current category (clamps at end, no wrapping)
    fn next_check(&mut self) {
        let count = self.current_checks().len();
        if count > 0 && self.selected_check < count - 1 {
            self.selected_check += 1;
            self.update_table_state();
        }
    }

    /// Select previous check in current category (clamps at start, no wrapping)
    fn prev_check(&mut self) {
        if self.selected_check > 0 {
            self.selected_check -= 1;
            self.update_table_state();
        }
    }

    /// Update table state to match selection
    fn update_table_state(&mut self) {
        self.table_state.select(Some(self.selected_check));
    }

    /// Ensure selected_check is within bounds for current category
    fn ensure_valid_selection(&mut self) {
        let count = self.current_checks().len();
        if count == 0 {
            self.selected_check = 0;
        } else if self.selected_check >= count {
            self.selected_check = count - 1;
        }
        self.update_table_state();
    }

    /// Initiate a fix action or show details for the currently selected check
    fn initiate_fix(&mut self) {
        // Extract info from check first to avoid borrow issues
        let check_info = self.selected_check().map(|check| {
            (
                check.id.clone(),
                check.name.clone(),
                check.fix.clone(),
                check.details.clone(),
            )
        });

        if let Some((check_id, check_name, fix_opt, details_opt)) = check_info {
            if let Some(fix) = fix_opt {
                // Has a fix - show confirmation dialog
                let preview = match &fix.action {
                    FixAction::AddKernelModule(name) => Some(format!(
                        "machine:\n  kernel:\n    modules:\n      - name: {}",
                        name
                    )),
                    FixAction::ApplyConfigPatch { yaml, .. } => Some(yaml.clone()),
                    _ => None,
                };

                let is_host_cmd = fix.action.is_host_command();

                self.pending_action = Some(PendingAction {
                    check_id,
                    fix,
                    preview,
                });
                self.show_confirmation = true;
                self.confirmation_selection = if is_host_cmd { 0 } else { 1 };
                self.copy_feedback_until = None;
            } else if let Some(details) = details_opt {
                // No fix but has details - show details popup
                self.details_title = check_name;
                self.details_content = details;
                self.show_details = true;
            }
        }
    }

    /// Apply the pending fix action
    pub async fn apply_pending_fix(&mut self) -> Result<()> {
        tracing::info!("apply_pending_fix called");

        let Some(pending) = self.pending_action.take() else {
            tracing::info!("No pending action to apply");
            return Ok(());
        };

        let Some(client) = &self.client else {
            tracing::error!("No client configured");
            self.set_error("No client configured".to_string());
            return Ok(());
        };

        // Get address for talosctl commands
        let address = self.data().map(|d| d.address.clone()).unwrap_or_default();

        self.applying_fix = true;
        self.show_confirmation = false;

        match &pending.fix.action {
            FixAction::AddKernelModule(name) => {
                tracing::info!("Applying kernel module fix: {}", name);
                let patch_yaml =
                    format!("machine:\n  kernel:\n    modules:\n      - name: {}", name);

                let patch_file = "/tmp/talos-pilot-patch.yaml";
                if let Err(e) = std::fs::write(patch_file, &patch_yaml) {
                    tracing::error!("Failed to write patch file: {}", e);
                    self.apply_result = Some(Err(format!("Failed to write patch file: {}", e)));
                } else {
                    let output = std::process::Command::new("talosctl")
                        .args([
                            "-n",
                            &address,
                            "patch",
                            "machineconfig",
                            "--mode=reboot",
                            "-p",
                            &format!("@{}", patch_file),
                        ])
                        .output();

                    match output {
                        Ok(result) => {
                            if result.status.success() {
                                let stdout = String::from_utf8_lossy(&result.stdout);
                                tracing::info!("Patch succeeded: {}", stdout);
                                self.apply_result = Some(Ok(vec![]));
                            } else {
                                let stderr = String::from_utf8_lossy(&result.stderr);
                                tracing::error!("Patch failed: {}", stderr);
                                self.apply_result = Some(Err(stderr.to_string()));
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to run talosctl: {}", e);
                            self.apply_result = Some(Err(format!("Failed to run talosctl: {}", e)));
                        }
                    }

                    let _ = std::fs::remove_file(patch_file);
                }
            }
            FixAction::ApplyConfigPatch {
                yaml,
                requires_reboot,
            } => {
                let mode = if *requires_reboot {
                    ApplyMode::Reboot
                } else {
                    ApplyMode::Auto
                };
                match client.apply_configuration(yaml, mode, false).await {
                    Ok(results) => {
                        self.apply_result = Some(Ok(results));
                    }
                    Err(e) => {
                        self.apply_result = Some(Err(e.to_string()));
                    }
                }
            }
            FixAction::RestartService(service) => match client.service_restart(service).await {
                Ok(_) => {
                    self.apply_result = Some(Ok(vec![]));
                }
                Err(e) => {
                    self.apply_result = Some(Err(e.to_string()));
                }
            },
            FixAction::ShowDetails(_)
            | FixAction::InstallCilium
            | FixAction::HostCommand { .. } => {
                // These don't apply directly
            }
        }

        self.applying_fix = false;
        Ok(())
    }

    /// Refresh diagnostics data from the node
    pub async fn refresh(&mut self) -> Result<()> {
        let client = match &self.client {
            Some(c) => c.clone(),
            None => {
                self.set_error("No client configured".to_string());
                return Ok(());
            }
        };

        self.state.start_loading();

        let timeout = std::time::Duration::from_secs(15);

        // Fetch platform info first
        if let Ok(versions) = client.version().await
            && let Some(v) = versions.first()
            && let Some(data) = self.data_mut()
        {
            data.context.platform = v.platform.clone();
            data.context.is_container = v.platform == "container";
            tracing::info!("Detected platform: {}", data.context.platform);
        }

        // Get CPU count for load threshold scaling
        if let Ok(cpu_info) = client.cpu_info().await
            && let Some(info) = cpu_info.first()
            && let Some(data) = self.data_mut()
        {
            data.context.cpu_count = info.cpu_count.max(1);
            tracing::info!("Detected {} CPUs", data.context.cpu_count);
        }

        // Try to create K8s client once for all K8s-based checks
        // For worker nodes, use the control plane endpoint to fetch kubeconfig
        let kubeconfig_client = if let Some(ref cp_endpoint) = self.controlplane_endpoint {
            tracing::info!(
                "Worker node: using control plane {} for kubeconfig",
                cp_endpoint
            );
            Some(client.with_node(cp_endpoint))
        } else {
            None
        };

        let k8s_client = match k8s::create_k8s_client_with_kubeconfig_source(
            &client,
            kubeconfig_client.as_ref(),
        )
        .await
        {
            Ok(client) => {
                tracing::info!("K8s client created successfully");
                if let Some(data) = self.data_mut() {
                    data.context.k8s_error = None;
                }
                Some(client)
            }
            Err(e) => {
                let error_msg = format!("{}", e);
                tracing::warn!(
                    "Failed to create K8s client: {} - K8s-based checks will be limited",
                    error_msg
                );
                if let Some(data) = self.data_mut() {
                    data.context.k8s_error = Some(error_msg);
                }
                None
            }
        };

        // Detect CNI type (uses K8s API if available, falls back to file checks)
        let (cni_type, cni_info) = cni::detect_cni_with_client(&client, k8s_client.as_ref()).await;
        if let Some(data) = self.data_mut() {
            data.context.cni_type = cni_type;
            data.context.cni_info = cni_info.clone();
            tracing::info!("Detected CNI: {:?}", data.context.cni_type);
        }

        // Get pod health from K8s API (reusing the same client)
        if let Some(ref kc) = k8s_client {
            match k8s::check_pod_health(kc).await {
                Ok(health) => {
                    // Convert k8s::PodHealthInfo to types::PodHealthInfo
                    let pod_health = PodHealthInfo {
                        crashing: health
                            .crashing
                            .iter()
                            .map(|p| UnhealthyPodInfo {
                                name: p.name.clone(),
                                namespace: p.namespace.clone(),
                                state: p.state.clone(),
                                restart_count: p.restart_count,
                            })
                            .collect(),
                        image_pull_errors: health
                            .image_pull_errors
                            .iter()
                            .map(|p| UnhealthyPodInfo {
                                name: p.name.clone(),
                                namespace: p.namespace.clone(),
                                state: p.state.clone(),
                                restart_count: p.restart_count,
                            })
                            .collect(),
                        total_pods: health.total_pods,
                    };
                    if let Some(data) = self.data_mut() {
                        data.context.pod_health = Some(pod_health);
                    }
                    tracing::info!("Pod health check complete: {} pods", health.total_pods);
                }
                Err(e) => {
                    tracing::warn!("Failed to check pod health via K8s API: {}", e);
                }
            }

            // Detect installed addons
            let detected_addons = addons::detect_addons(kc).await;
            if let Some(data) = self.data_mut() {
                data.detected_addons = detected_addons;
            }
        }

        // Get context and detected_addons for use in async block
        let context = self.data().map(|d| d.context.clone()).unwrap_or_default();
        let detected_addons = self
            .data()
            .map(|d| d.detected_addons.clone())
            .unwrap_or_default();

        let result = tokio::time::timeout(timeout, async {
            // Run core checks
            let mut system_checks = core::run_system_checks(&client, &context).await;
            let kubernetes_checks = core::run_kubernetes_checks(&client, &context).await;
            let service_checks = core::run_service_checks(&client, &context).await;

            // Run certificate checks and add to system checks
            let cert_checks = core::run_certificate_checks(&client, &context).await;
            system_checks.extend(cert_checks);

            // Run CNI-specific checks
            let cni_checks = cni::run_cni_checks(&client, &context, k8s_client.as_ref()).await;

            // Run addon-specific checks
            let addon_checks =
                addons::run_addon_checks(k8s_client.as_ref(), &detected_addons, &context).await;

            (
                system_checks,
                kubernetes_checks,
                service_checks,
                cni_checks,
                addon_checks,
            )
        })
        .await;

        match result {
            Ok((system, kubernetes, services, cni, addons_result)) => {
                if let Some(data) = self.data_mut() {
                    data.system_checks = system;
                    data.kubernetes_checks = kubernetes;
                    data.service_checks = services;
                    data.cni_checks = cni;
                    data.addon_checks = addons_result;
                }
                // Ensure selection is valid after checks change
                self.ensure_valid_selection();
                self.state.mark_loaded();
            }
            Err(_) => {
                self.set_error("Timeout fetching diagnostics".to_string());
            }
        }

        Ok(())
    }

    /// Get category title
    fn category_title(&self, idx: usize) -> &'static str {
        match idx {
            0 => "System Health",
            1 => "Kubernetes Components",
            2 => {
                let cni_type = self
                    .data()
                    .map(|d| d.context.cni_type.clone())
                    .unwrap_or(CniType::Unknown);
                match cni_type {
                    CniType::Flannel => "CNI (Flannel)",
                    CniType::Cilium => "CNI (Cilium)",
                    CniType::Calico => "CNI (Calico)",
                    _ => "CNI",
                }
            }
            3 => "Services",
            4 => "Addons",
            _ => "Unknown",
        }
    }

    /// Render a category section
    fn render_category(
        &self,
        frame: &mut Frame,
        area: Rect,
        category_idx: usize,
        checks: &[DiagnosticCheck],
        is_selected: bool,
    ) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.category_title(category_idx))
            .border_style(if is_selected {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            });

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if checks.is_empty() {
            let loading = Paragraph::new("Loading...").style(Style::default().fg(Color::DarkGray));
            frame.render_widget(loading, inner);
            return;
        }

        let rows: Vec<Row> = checks
            .iter()
            .enumerate()
            .map(|(i, check)| {
                let (indicator, color) = check.status.indicator();
                let is_current = is_selected && i == self.selected_check;

                let style = if is_current {
                    Style::default().bg(Color::DarkGray)
                } else {
                    Style::default()
                };

                Row::new(vec![
                    ratatui::widgets::Cell::from(Span::styled(
                        indicator,
                        Style::default().fg(color),
                    )),
                    ratatui::widgets::Cell::from(check.name.clone()),
                    ratatui::widgets::Cell::from(Span::styled(
                        check.message.clone(),
                        Style::default().fg(if check.status == CheckStatus::Pass {
                            Color::Green
                        } else if check.status == CheckStatus::Fail {
                            Color::Red
                        } else if check.status == CheckStatus::Warn {
                            Color::Yellow
                        } else {
                            Color::DarkGray
                        }),
                    )),
                ])
                .style(style)
            })
            .collect();

        let widths = [
            Constraint::Length(2),
            Constraint::Length(20),
            Constraint::Fill(1),
        ];

        let table = Table::new(rows, widths);
        frame.render_widget(table, inner);
    }

    /// Render the confirmation dialog
    fn render_confirmation(&self, frame: &mut Frame, area: Rect) {
        let Some(pending) = &self.pending_action else {
            return;
        };

        let dialog_width = 70.min(area.width.saturating_sub(4));
        let dialog_height = 15.min(area.height.saturating_sub(4));
        let dialog_x = (area.width.saturating_sub(dialog_width)) / 2;
        let dialog_y = (area.height.saturating_sub(dialog_height)) / 2;

        let dialog_area = Rect::new(
            area.x + dialog_x,
            area.y + dialog_y,
            dialog_width,
            dialog_height,
        );

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Confirm Action ")
            .border_style(Style::default().fg(Color::Yellow));

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let is_host_command = pending.fix.action.is_host_command();

        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("Fix: {}", pending.fix.description),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
        ];

        if let FixAction::HostCommand { command, .. } = &pending.fix.action {
            lines.push(Line::from(Span::styled(
                "Run this command on your Docker host:",
                Style::default().fg(Color::Yellow),
            )));
            lines.push(Line::from(""));
            // Display each line of the command
            for cmd_line in command.lines() {
                lines.push(Line::from(Span::styled(
                    format!("  {}", cmd_line),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )));
            }
            lines.push(Line::from(""));
        } else if let Some(preview) = &pending.preview {
            lines.push(Line::from("This will apply the following configuration:"));
            lines.push(Line::from(""));
            for line in preview.lines().take(5) {
                lines.push(Line::from(Span::styled(
                    format!("  {}", line),
                    Style::default().fg(Color::Cyan),
                )));
            }
            lines.push(Line::from(""));
        }

        if !is_host_command && pending.fix.action.requires_reboot() {
            lines.push(Line::from(Span::styled(
                "⚠ This requires a node reboot to take effect.",
                Style::default().fg(Color::Yellow),
            )));
            lines.push(Line::from(""));
        }

        if is_host_command {
            let show_copied = self
                .copy_feedback_until
                .map(|t| t.elapsed() < std::time::Duration::from_secs(2))
                .unwrap_or(false);

            let copy_style = if show_copied {
                Style::default()
                    .bg(Color::Green)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD)
            } else if self.confirmation_selection == 0 {
                Style::default()
                    .bg(Color::Cyan)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Cyan)
            };

            let close_style = if self.confirmation_selection == 1 {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            let copy_text = if show_copied { " Copied! " } else { " Copy " };

            lines.push(Line::from(vec![
                Span::raw("         "),
                Span::styled(copy_text, copy_style),
                Span::raw("     "),
                Span::styled(" Close ", close_style),
            ]));
        } else {
            let cancel_style = if self.confirmation_selection == 0 {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let apply_style = if self.confirmation_selection == 1 {
                Style::default()
                    .bg(Color::Green)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Green)
            };

            lines.push(Line::from(vec![
                Span::raw("         "),
                Span::styled(" Cancel ", cancel_style),
                Span::raw("     "),
                Span::styled(
                    if pending.fix.action.requires_reboot() {
                        " Apply & Reboot "
                    } else {
                        " Apply "
                    },
                    apply_style,
                ),
            ]));
        }

        let content = Paragraph::new(lines);
        frame.render_widget(content, inner);
    }

    /// Render the details popup
    fn render_details(&self, frame: &mut Frame, area: Rect) {
        if !self.show_details {
            return;
        }

        // Calculate dialog size based on content
        let content_lines: Vec<&str> = self.details_content.lines().collect();
        let max_line_len = content_lines.iter().map(|l| l.len()).max().unwrap_or(40);

        let dialog_width = (max_line_len as u16 + 6)
            .clamp(50, 80)
            .min(area.width.saturating_sub(4));
        let dialog_height = (content_lines.len() as u16 + 6)
            .min(20)
            .min(area.height.saturating_sub(4));
        let dialog_x = (area.width.saturating_sub(dialog_width)) / 2;
        let dialog_y = (area.height.saturating_sub(dialog_height)) / 2;

        let dialog_area = Rect::new(
            area.x + dialog_x,
            area.y + dialog_y,
            dialog_width,
            dialog_height,
        );

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", self.details_title))
            .border_style(Style::default().fg(Color::Cyan));

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let mut lines = vec![Line::from("")];

        for line in self.details_content.lines() {
            lines.push(Line::from(Span::styled(
                format!(" {}", line),
                Style::default().fg(Color::White),
            )));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " Press Enter or Esc to close ",
            Style::default().fg(Color::DarkGray),
        )));

        let content = Paragraph::new(lines);
        frame.render_widget(content, inner);
    }
}

impl Component for DiagnosticsComponent {
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        // Handle details popup (if showing)
        if self.show_details {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                    self.show_details = false;
                    return Ok(None);
                }
                _ => return Ok(None),
            }
        }

        if self.show_confirmation {
            let is_host_command = self
                .pending_action
                .as_ref()
                .map(|p| p.fix.action.is_host_command())
                .unwrap_or(false);

            match key.code {
                KeyCode::Left | KeyCode::Char('h') => {
                    self.confirmation_selection = 0;
                    return Ok(None);
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    self.confirmation_selection = 1;
                    return Ok(None);
                }
                KeyCode::Enter => {
                    if is_host_command {
                        if self.confirmation_selection == 0 {
                            if let Some(pending) = &self.pending_action
                                && let FixAction::HostCommand { command, .. } = &pending.fix.action
                            {
                                // Spawn a thread to copy to clipboard and keep it alive
                                // This prevents the "clipboard dropped quickly" warning
                                let cmd = command.clone();
                                std::thread::spawn(move || {
                                    if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                        let _ = clipboard.set_text(cmd);
                                        // Keep clipboard alive for a bit so clipboard managers can read
                                        std::thread::sleep(std::time::Duration::from_millis(100));
                                    }
                                });
                                self.copy_feedback_until = Some(Instant::now());
                            }
                        } else {
                            self.show_confirmation = false;
                            self.pending_action = None;
                        }
                    } else if self.confirmation_selection == 0 {
                        self.show_confirmation = false;
                        self.pending_action = None;
                    } else {
                        return Ok(Some(Action::ApplyDiagnosticFix));
                    }
                    return Ok(None);
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.show_confirmation = false;
                    self.pending_action = None;
                    return Ok(None);
                }
                _ => return Ok(None),
            }
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                return Ok(Some(Action::Back));
            }
            KeyCode::Char('r') => {
                return Ok(Some(Action::Refresh));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.next_check();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.prev_check();
            }
            KeyCode::Tab => {
                self.next_category();
            }
            KeyCode::BackTab => {
                self.prev_category();
            }
            KeyCode::Enter => {
                self.initiate_fix();
            }
            _ => {}
        }

        Ok(None)
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if let Action::Tick = action {
            let interval = Duration::from_secs(AUTO_REFRESH_INTERVAL_SECS);
            if self.state.should_auto_refresh(self.auto_refresh, interval) {
                return Ok(Some(Action::Refresh));
            }
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        let chunks = Layout::vertical([
            Constraint::Length(3),
            Constraint::Fill(1),
            Constraint::Length(2),
        ])
        .split(area);

        // Get header info from data
        let (hostname, address, cni_label) = self
            .data()
            .map(|d| {
                (
                    d.hostname.clone(),
                    d.address.clone(),
                    d.context.cni_type.name(),
                )
            })
            .unwrap_or_else(|| (String::new(), String::new(), "Unknown"));

        // Header
        let header_text = format!(" Diagnostics: {} ({}) [{}] ", hostname, address, cni_label);
        let header = Paragraph::new(header_text)
            .style(Style::default().add_modifier(Modifier::BOLD))
            .block(Block::default().borders(Borders::BOTTOM));
        frame.render_widget(header, chunks[0]);

        if let Some(error) = self.state.error() {
            let error_msg =
                Paragraph::new(format!("Error: {}", error)).style(Style::default().fg(Color::Red));
            frame.render_widget(error_msg, chunks[1]);
        } else if let Some(data) = self.data() {
            // Dynamically size Addons section based on whether addons are detected
            let addons_height = if data.detected_addons.any_detected() {
                Constraint::Length(5)
            } else {
                Constraint::Length(0)
            };

            let content_chunks = Layout::vertical([
                Constraint::Length(7), // System Health (Memory, CPU, 3 certs = 5 items + 2 border)
                Constraint::Length(4), // Kubernetes Components (etcd, pod_health = 2 items + 2 border)
                Constraint::Length(5), // CNI
                Constraint::Fill(1),   // Services
                addons_height,         // Addons (if any)
            ])
            .split(chunks[1]);

            // Clone checks for rendering to avoid borrow issues
            let system_checks = data.system_checks.clone();
            let kubernetes_checks = data.kubernetes_checks.clone();
            let cni_checks = data.cni_checks.clone();
            let service_checks = data.service_checks.clone();
            let addon_checks = data.addon_checks.clone();
            let any_addons = data.detected_addons.any_detected();

            self.render_category(
                frame,
                content_chunks[0],
                0,
                &system_checks,
                self.selected_category == 0,
            );
            self.render_category(
                frame,
                content_chunks[1],
                1,
                &kubernetes_checks,
                self.selected_category == 1,
            );
            self.render_category(
                frame,
                content_chunks[2],
                2,
                &cni_checks,
                self.selected_category == 2,
            );
            self.render_category(
                frame,
                content_chunks[3],
                3,
                &service_checks,
                self.selected_category == 3,
            );

            // Only render Addons section if addons are detected
            if any_addons {
                self.render_category(
                    frame,
                    content_chunks[4],
                    4,
                    &addon_checks,
                    self.selected_category == 4,
                );
            }
        }

        // Footer
        let footer = Paragraph::new(Line::from(vec![
            Span::styled("[j/k]", Style::default().fg(Color::Cyan)),
            Span::raw(" Navigate  "),
            Span::styled("[Tab]", Style::default().fg(Color::Cyan)),
            Span::raw(" Section  "),
            Span::styled("[Enter]", Style::default().fg(Color::Cyan)),
            Span::raw(" Details/Fix  "),
            Span::styled("[r]", Style::default().fg(Color::Cyan)),
            Span::raw(" Refresh  "),
            Span::styled("[q]", Style::default().fg(Color::Cyan)),
            Span::raw(" Back"),
        ]));
        frame.render_widget(footer, chunks[2]);

        if self.show_confirmation {
            self.render_confirmation(frame, area);
        }

        if self.show_details {
            self.render_details(frame, area);
        }

        Ok(())
    }
}
