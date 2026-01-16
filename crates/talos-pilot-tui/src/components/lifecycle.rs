//! Lifecycle component - displays version status, time sync, and config drift
//!
//! Provides a consolidated view of cluster lifecycle operations.

use crate::action::Action;
use crate::components::Component;
use crate::components::diagnostics::k8s::{
    PdbHealthInfo, PodHealthInfo, check_pdb_health, check_pod_health, create_k8s_client,
};
use crate::ui_ext::HealthIndicatorExt;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent};
use kube::Client;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Row, Table, TableState},
};
use std::time::Duration;
use talos_pilot_core::{AsyncState, HasHealth, HealthIndicator, SelectableList};
use talos_rs::{
    DiscoveryMember, NodeTimeInfo, TalosClient, TalosConfig, VersionInfo,
    get_discovery_members_for_context,
};

/// Auto-refresh interval in seconds
const AUTO_REFRESH_INTERVAL_SECS: u64 = 30;

/// Node lifecycle status
#[derive(Debug, Clone)]
pub struct NodeStatus {
    /// Node hostname
    pub hostname: String,
    /// Talos version
    pub version: String,
    /// Config hash (placeholder - needs MachineConfig API)
    pub config_hash: Option<String>,
    /// Time sync status (placeholder - needs TimeStatus API)
    pub time_synced: Option<bool>,
    /// Node ready status
    pub ready: bool,
    /// Platform (container, metal, etc.)
    pub platform: String,
}

/// Alert item
#[derive(Debug, Clone)]
pub struct Alert {
    /// Alert severity
    pub severity: AlertSeverity,
    /// Alert message
    pub message: String,
}

#[derive(Debug, Clone, Copy)]
pub enum AlertSeverity {
    Info,
    Warning,
    Error,
}

impl HasHealth for AlertSeverity {
    fn health(&self) -> HealthIndicator {
        match self {
            AlertSeverity::Info => HealthIndicator::Info,
            AlertSeverity::Warning => HealthIndicator::Warning,
            AlertSeverity::Error => HealthIndicator::Error,
        }
    }
}

impl AlertSeverity {
    fn indicator(&self) -> (&'static str, Color) {
        self.health().symbol_and_color()
    }
}

/// etcd quorum information for pre-operation checks
#[derive(Debug, Clone, Default)]
pub struct EtcdQuorumInfo {
    /// Total etcd members
    pub total_members: usize,
    /// Healthy etcd members
    pub healthy_members: usize,
    /// How many nodes can be lost while maintaining quorum
    pub can_lose: usize,
    /// Whether the cluster is healthy
    pub is_healthy: bool,
}

impl EtcdQuorumInfo {
    /// Get summary message
    pub fn summary(&self) -> String {
        if self.total_members == 0 {
            "No etcd members found".to_string()
        } else if self.is_healthy {
            format!(
                "{}/{} members, can lose {}",
                self.healthy_members, self.total_members, self.can_lose
            )
        } else {
            format!(
                "{}/{} members healthy (quorum at risk)",
                self.healthy_members, self.total_members
            )
        }
    }
}

/// Pre-operation health check results
#[derive(Debug, Clone, Default)]
pub struct PreOpChecks {
    /// Pod health info
    pub pod_health: Option<PodHealthInfo>,
    /// PDB health info
    pub pdb_health: Option<PdbHealthInfo>,
    /// etcd quorum info
    pub etcd_quorum: Option<EtcdQuorumInfo>,
    /// Whether all checks passed
    pub all_passed: bool,
}

/// Loaded lifecycle data (wrapped by AsyncState)
#[derive(Debug, Clone, Default)]
pub struct LifecycleData {
    /// Context name (cluster identifier)
    pub context_name: String,
    /// Talos versions per node
    pub versions: Vec<VersionInfo>,
    /// Time sync info per node
    pub time_info: Vec<NodeTimeInfo>,
    /// Node statuses with selection
    pub node_statuses: SelectableList<NodeStatus>,
    /// Alerts
    pub alerts: Vec<Alert>,
    /// Discovery members (from talosctl get members)
    pub discovery_members: Vec<DiscoveryMember>,
    /// Pre-operation health checks
    pub pre_op_checks: PreOpChecks,
}

/// Lifecycle component for viewing version and lifecycle status
pub struct LifecycleComponent {
    /// Async state wrapping all lifecycle data
    state: AsyncState<LifecycleData>,

    /// Table state for rendering (synced with node_statuses selection)
    table_state: TableState,

    /// Auto-refresh enabled
    auto_refresh: bool,

    /// Client for API calls
    client: Option<TalosClient>,

    /// K8s client for pod/PDB checks (reusable, not part of loaded data)
    k8s_client: Option<Client>,

    /// Custom config file path (from --config flag)
    config_path: Option<String>,
}

impl Default for LifecycleComponent {
    fn default() -> Self {
        Self::new("".to_string(), None)
    }
}

impl LifecycleComponent {
    pub fn new(context_name: String, config_path: Option<String>) -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));

        // Initialize with context name in the data
        let initial_data = LifecycleData {
            context_name,
            ..Default::default()
        };
        let mut state = AsyncState::new();
        state.set_data(initial_data);

        Self {
            state,
            table_state,
            auto_refresh: true,
            client: None,
            k8s_client: None,
            config_path,
        }
    }

    /// Set the client for API calls
    pub fn set_client(&mut self, client: TalosClient) {
        self.client = Some(client);
    }

    /// Set error message
    pub fn set_error(&mut self, error: String) {
        self.state.set_error(error);
    }

    /// Helper to get data reference
    fn data(&self) -> Option<&LifecycleData> {
        self.state.data()
    }

    /// Helper to get mutable data reference
    fn data_mut(&mut self) -> Option<&mut LifecycleData> {
        self.state.data_mut()
    }

    /// Refresh lifecycle data
    pub async fn refresh(&mut self) -> Result<()> {
        self.state.start_loading();

        let Some(client) = self.client.clone() else {
            self.state.set_error("No client configured");
            return Ok(());
        };

        // Get or create data
        let mut data = self.state.take_data().unwrap_or_default();

        // Fetch version information
        match client.version().await {
            Ok(versions) => {
                // Get context name from first node
                if !versions.is_empty() && data.context_name.is_empty() {
                    // Try to get from talosconfig
                    let config_result = match &self.config_path {
                        Some(path) => {
                            let path_buf = std::path::PathBuf::from(path);
                            TalosConfig::load_from(&path_buf)
                        }
                        None => TalosConfig::load_default(),
                    };
                    if let Ok(config) = config_result {
                        data.context_name = config.context;
                    }
                }

                data.versions = versions;
            }
            Err(e) => {
                self.state
                    .set_error(format!("Failed to fetch versions: {}", e));
                // Re-store the data so far
                self.state.set_data(data);
                return Ok(());
            }
        }

        // Fetch time sync status
        match client.time().await {
            Ok(times) => {
                data.time_info = times;
            }
            Err(e) => {
                // Time fetch failure is not fatal - just log it
                tracing::warn!("Failed to fetch time status: {}", e);
                data.time_info.clear();
            }
        }

        // Fetch discovery members using context-aware async version
        let context_name = if !data.context_name.is_empty() {
            data.context_name.clone()
        } else {
            let config_result = match &self.config_path {
                Some(path) => {
                    let path_buf = std::path::PathBuf::from(path);
                    TalosConfig::load_from(&path_buf)
                }
                None => TalosConfig::load_default(),
            };
            config_result
                .map(|config| config.context)
                .unwrap_or_default()
        };

        if !context_name.is_empty() {
            match get_discovery_members_for_context(&context_name, self.config_path.as_deref())
                .await
            {
                Ok(members) => {
                    data.discovery_members = members;
                }
                Err(e) => {
                    tracing::debug!("Failed to get discovery members: {}", e);
                    data.discovery_members.clear();
                }
            }
        }

        // Build node statuses combining version and time info
        // Fetch config hash for each node individually for drift detection
        let statuses: Vec<NodeStatus> = data
            .versions
            .iter()
            .map(|v| {
                // Look up time sync status for this node
                let time_synced = data
                    .time_info
                    .iter()
                    .find(|t| t.node == v.node)
                    .map(|t| t.synced);

                // Get config hash for this specific node
                let node_addr = v.node.split(':').next().unwrap_or(&v.node);
                let node_config_hash = match talos_rs::get_machine_config(node_addr) {
                    Ok(config) => Some(config.version),
                    Err(_) => None,
                };

                NodeStatus {
                    hostname: v.node.clone(),
                    version: v.version.clone(),
                    config_hash: node_config_hash,
                    time_synced,
                    ready: true, // Assume ready for now
                    platform: v.platform.clone(),
                }
            })
            .collect();

        // Update items, preserving selection if possible
        data.node_statuses.update_items(statuses);
        self.table_state
            .select(Some(data.node_statuses.selected_index()));

        // Fetch pre-operation health checks
        self.fetch_pre_op_checks_into(&mut data, &client).await;

        // Generate alerts
        Self::generate_alerts_into(&mut data);

        // Store the data
        self.state.set_data(data);
        Ok(())
    }

    /// Fetch pre-operation health checks into data
    async fn fetch_pre_op_checks_into(&mut self, data: &mut LifecycleData, client: &TalosClient) {
        // Initialize K8s client if not already done
        if self.k8s_client.is_none() {
            match create_k8s_client(client).await {
                Ok(k8s) => {
                    self.k8s_client = Some(k8s);
                }
                Err(e) => {
                    tracing::warn!("Failed to create K8s client: {}", e);
                }
            }
        }

        // Fetch etcd quorum info
        let etcd_quorum = match client.etcd_members().await {
            Ok(members) => {
                let total = members.len();
                // Try to get status to determine healthy members
                let healthy = match client.etcd_status().await {
                    Ok(statuses) => {
                        // Count members with status
                        members
                            .iter()
                            .filter(|m| statuses.iter().any(|s| s.member_id == m.id))
                            .count()
                    }
                    Err(_) => total, // Assume all healthy if we can't get status
                };

                let quorum_needed = total / 2 + 1;
                let can_lose = healthy.saturating_sub(quorum_needed);

                Some(EtcdQuorumInfo {
                    total_members: total,
                    healthy_members: healthy,
                    can_lose,
                    is_healthy: healthy >= quorum_needed,
                })
            }
            Err(e) => {
                tracing::warn!("Failed to fetch etcd members: {}", e);
                None
            }
        };

        // Fetch pod and PDB health from K8s
        let (pod_health, pdb_health) = if let Some(k8s) = &self.k8s_client {
            let pod_result = check_pod_health(k8s).await;
            let pdb_result = check_pdb_health(k8s).await;

            (pod_result.ok(), pdb_result.ok())
        } else {
            (None, None)
        };

        // Determine if all checks passed
        let all_passed = {
            let pod_ok = pod_health.as_ref().map(|p| !p.has_issues()).unwrap_or(true);
            let pdb_ok = pdb_health
                .as_ref()
                .map(|p| !p.has_blocking_pdbs())
                .unwrap_or(true);
            let etcd_ok = etcd_quorum
                .as_ref()
                .map(|e| e.is_healthy && e.can_lose > 0)
                .unwrap_or(true);
            pod_ok && pdb_ok && etcd_ok
        };

        data.pre_op_checks = PreOpChecks {
            pod_health,
            pdb_health,
            etcd_quorum,
            all_passed,
        };
    }

    /// Generate alerts based on current state (static method operating on data)
    fn generate_alerts_into(data: &mut LifecycleData) {
        data.alerts.clear();

        // Check for version mismatches
        let versions: Vec<&str> = data.versions.iter().map(|v| v.version.as_str()).collect();
        let unique_versions: std::collections::HashSet<_> = versions.iter().collect();
        if unique_versions.len() > 1 {
            data.alerts.push(Alert {
                severity: AlertSeverity::Warning,
                message: "Version mismatch detected across nodes".to_string(),
            });
        }

        // Check for time sync issues
        let unsynced_nodes: Vec<&str> = data
            .node_statuses
            .items()
            .iter()
            .filter(|n| n.time_synced == Some(false))
            .map(|n| n.hostname.as_str())
            .collect();
        if !unsynced_nodes.is_empty() {
            data.alerts.push(Alert {
                severity: AlertSeverity::Warning,
                message: format!("Time not synced on: {}", unsynced_nodes.join(", ")),
            });
        }

        // Check for config drift (compare hashes across nodes)
        let config_hashes: Vec<(&str, Option<&str>)> = data
            .node_statuses
            .items()
            .iter()
            .map(|n| (n.hostname.as_str(), n.config_hash.as_deref()))
            .collect();

        let unique_hashes: std::collections::HashSet<_> =
            config_hashes.iter().filter_map(|(_, h)| *h).collect();

        if unique_hashes.len() > 1 {
            // Config drift detected - list nodes with different hashes
            let drift_details: Vec<String> = config_hashes
                .iter()
                .filter_map(|(hostname, hash)| {
                    hash.map(|h| {
                        format!("{}:{}", hostname.split(':').next().unwrap_or(hostname), h)
                    })
                })
                .collect();
            data.alerts.push(Alert {
                severity: AlertSeverity::Warning,
                message: format!("Config drift detected: {}", drift_details.join(", ")),
            });
        } else if let Some(hash) = unique_hashes.iter().next() {
            // All nodes have same config hash
            data.alerts.push(Alert {
                severity: AlertSeverity::Info,
                message: format!("Config version: {} (all nodes in sync)", hash),
            });
        }

        // Check discovery service health
        if !data.discovery_members.is_empty() {
            let expected_nodes = data.node_statuses.len();
            let discovered_nodes = data.discovery_members.len();

            if discovered_nodes < expected_nodes {
                data.alerts.push(Alert {
                    severity: AlertSeverity::Warning,
                    message: format!(
                        "Discovery: {}/{} nodes visible (some nodes may not be discoverable)",
                        discovered_nodes, expected_nodes
                    ),
                });
            } else {
                data.alerts.push(Alert {
                    severity: AlertSeverity::Info,
                    message: format!(
                        "Discovery: {}/{} nodes healthy",
                        discovered_nodes, expected_nodes
                    ),
                });
            }
        }
    }

    /// Get current Talos version (from first node)
    fn current_talos_version(&self) -> &str {
        self.data()
            .and_then(|d| d.versions.first())
            .map(|v| v.version.as_str())
            .unwrap_or("unknown")
    }

    /// Get K8s version support range for current Talos
    fn k8s_support_range(&self) -> &str {
        // Talos version to K8s support mapping
        // See: https://www.talos.dev/latest/introduction/support-matrix/
        let version = self.current_talos_version();
        if version.starts_with("v1.12") {
            "v1.30 - v1.35"
        } else if version.starts_with("v1.11") {
            "v1.29 - v1.34"
        } else if version.starts_with("v1.10") {
            "v1.28 - v1.33"
        } else if version.starts_with("v1.9") {
            "v1.27 - v1.32"
        } else if version.starts_with("v1.8") {
            "v1.26 - v1.31"
        } else if version.starts_with("v1.7") {
            "v1.25 - v1.30"
        } else if version.starts_with("v1.6") {
            "v1.24 - v1.29"
        } else {
            "unknown"
        }
    }

    /// Navigation: select next node
    fn select_next(&mut self) {
        let new_index = if let Some(data) = self.data_mut() {
            data.node_statuses.select_next();
            Some(data.node_statuses.selected_index())
        } else {
            None
        };
        if let Some(idx) = new_index {
            self.table_state.select(Some(idx));
        }
    }

    /// Navigation: select previous node
    fn select_prev(&mut self) {
        let new_index = if let Some(data) = self.data_mut() {
            data.node_statuses.select_prev();
            Some(data.node_statuses.selected_index())
        } else {
            None
        };
        if let Some(idx) = new_index {
            self.table_state.select(Some(idx));
        }
    }

    /// Draw the cluster versions section
    fn draw_versions_section(&self, frame: &mut Frame, area: Rect) {
        let version = self.current_talos_version();
        let k8s_range = self.k8s_support_range();

        // Get platform from first node
        let platform = self
            .data()
            .and_then(|d| d.versions.first())
            .map(|v| v.platform.as_str())
            .unwrap_or("unknown");

        let lines = vec![
            Line::from(""),
            Line::from(vec![Span::styled(
                "  Talos Version",
                Style::default().fg(Color::Yellow),
            )]),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("|-", Style::default().fg(Color::DarkGray)),
                Span::raw(" Current:   "),
                Span::styled(version, Style::default().fg(Color::Green)),
            ]),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("|-", Style::default().fg(Color::DarkGray)),
                Span::raw(" Platform:  "),
                Span::styled(platform, Style::default().fg(Color::Cyan)),
            ]),
            Line::from(""),
            Line::from(vec![Span::styled(
                "  Kubernetes Support",
                Style::default().fg(Color::Yellow),
            )]),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("|-", Style::default().fg(Color::DarkGray)),
                Span::raw(" Supported: "),
                Span::styled(k8s_range, Style::default().fg(Color::White)),
            ]),
        ];

        let block = Block::default()
            .title(" Cluster Versions ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));

        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, area);
    }

    /// Draw the node status table
    fn draw_node_table(&mut self, frame: &mut Frame, area: Rect) {
        let header_cells = ["Node", "Version", "Config", "Time Sync", "Status"]
            .iter()
            .map(|h| {
                ratatui::widgets::Cell::from(*h).style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            });
        let header = Row::new(header_cells).height(1);

        let node_items: Vec<NodeStatus> = self
            .data()
            .map(|d| d.node_statuses.items().to_vec())
            .unwrap_or_default();

        let rows = node_items.iter().map(|node| {
            let config_cell = match &node.config_hash {
                Some(hash) => Span::styled(
                    &hash[..8.min(hash.len())],
                    Style::default().fg(Color::White),
                ),
                None => Span::styled("-", Style::default().fg(Color::Gray)),
            };

            let time_cell = match node.time_synced {
                Some(true) => Span::styled("synced", Style::default().fg(Color::Green)),
                Some(false) => Span::styled("not synced", Style::default().fg(Color::Yellow)),
                None => Span::styled("-", Style::default().fg(Color::Gray)),
            };

            let status_cell = if node.ready {
                Span::styled("Ready", Style::default().fg(Color::Green))
            } else {
                Span::styled("NotReady", Style::default().fg(Color::Red))
            };

            Row::new(vec![
                ratatui::widgets::Cell::from(node.hostname.clone())
                    .style(Style::default().fg(Color::White)),
                ratatui::widgets::Cell::from(node.version.clone())
                    .style(Style::default().fg(Color::Green)),
                ratatui::widgets::Cell::from(config_cell),
                ratatui::widgets::Cell::from(time_cell),
                ratatui::widgets::Cell::from(status_cell),
            ])
        });

        let widths = [
            Constraint::Min(20),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Length(10),
        ];

        let block = Block::default()
            .title(" Node Status ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));

        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .row_highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        frame.render_stateful_widget(table, area, &mut self.table_state);
    }

    /// Draw the alerts section
    fn draw_alerts_section(&self, frame: &mut Frame, area: Rect) {
        let alerts: Vec<Alert> = self.data().map(|d| d.alerts.clone()).unwrap_or_default();

        let lines: Vec<Line> = alerts
            .iter()
            .map(|alert| {
                let (indicator, color) = alert.severity.indicator();
                Line::from(vec![
                    Span::raw(" "),
                    Span::styled(indicator, Style::default().fg(color)),
                    Span::raw(" "),
                    Span::raw(&alert.message),
                ])
            })
            .collect();

        let block = Block::default()
            .title(" Alerts ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));

        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, area);
    }

    /// Draw the pre-operation health checks section
    fn draw_pre_op_checks(&self, frame: &mut Frame, area: Rect) {
        let pre_op_checks = self
            .data()
            .map(|d| d.pre_op_checks.clone())
            .unwrap_or_default();

        let mut lines = Vec::new();

        // Overall status indicator
        let (overall_indicator, overall_color) = if pre_op_checks.all_passed {
            ("✓", Color::Green)
        } else {
            ("!", Color::Yellow)
        };

        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(overall_indicator, Style::default().fg(overall_color)),
            Span::raw(" "),
            Span::styled(
                if pre_op_checks.all_passed {
                    "All checks passed"
                } else {
                    "Some checks have warnings"
                },
                Style::default().fg(overall_color),
            ),
        ]));

        // etcd quorum check
        if let Some(ref etcd) = pre_op_checks.etcd_quorum {
            let (indicator, color) = if etcd.is_healthy && etcd.can_lose > 0 {
                ("✓", Color::Green)
            } else if etcd.is_healthy {
                ("!", Color::Yellow)
            } else {
                ("✗", Color::Red)
            };

            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(indicator, Style::default().fg(color)),
                Span::raw(" etcd: "),
                Span::styled(etcd.summary(), Style::default().fg(color)),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("?", Style::default().fg(Color::DarkGray)),
                Span::raw(" etcd: "),
                Span::styled("unavailable", Style::default().fg(Color::DarkGray)),
            ]));
        }

        // Pod health check
        if let Some(ref pods) = pre_op_checks.pod_health {
            let has_issues = pods.has_issues();
            let (indicator, color) = if !has_issues && pods.pending.is_empty() {
                ("✓", Color::Green)
            } else if !has_issues {
                ("◐", Color::Yellow)
            } else {
                ("✗", Color::Red)
            };

            let running = pods.total_pods
                - pods.crashing.len()
                - pods.image_pull_errors.len()
                - pods.pending.len();
            let summary = if has_issues || !pods.pending.is_empty() {
                format!("{} running, {}", running, pods.summary())
            } else {
                format!("{} pods healthy", pods.total_pods)
            };

            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(indicator, Style::default().fg(color)),
                Span::raw(" Pods: "),
                Span::styled(summary, Style::default().fg(color)),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("?", Style::default().fg(Color::DarkGray)),
                Span::raw(" Pods: "),
                Span::styled("unavailable", Style::default().fg(Color::DarkGray)),
            ]));
        }

        // PDB check
        if let Some(ref pdbs) = pre_op_checks.pdb_health {
            let (indicator, color) = if !pdbs.has_blocking_pdbs() {
                ("✓", Color::Green)
            } else {
                ("◐", Color::Yellow)
            };

            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(indicator, Style::default().fg(color)),
                Span::raw(" PDBs: "),
                Span::styled(pdbs.summary(), Style::default().fg(color)),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("?", Style::default().fg(Color::DarkGray)),
                Span::raw(" PDBs: "),
                Span::styled("unavailable", Style::default().fg(Color::DarkGray)),
            ]));
        }

        let block = Block::default()
            .title(" Pre-Operation Checks ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));

        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, area);
    }

    /// Draw the footer
    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let spans = vec![
            Span::styled("[j/k]", Style::default().fg(Color::Cyan)),
            Span::raw(" navigate  "),
            Span::styled("[r]", Style::default().fg(Color::Cyan)),
            Span::raw(" refresh  "),
            Span::styled("[q]", Style::default().fg(Color::Cyan)),
            Span::raw(" back"),
        ];

        let footer = Paragraph::new(Line::from(spans)).style(Style::default().fg(Color::DarkGray));
        frame.render_widget(footer, area);
    }
}

impl Component for LifecycleComponent {
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Ok(Some(Action::Back)),
            KeyCode::Char('j') | KeyCode::Down => {
                self.select_next();
                Ok(None)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.select_prev();
                Ok(None)
            }
            KeyCode::Char('r') => Ok(Some(Action::Refresh)),
            _ => Ok(None),
        }
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if let Action::Tick = action {
            // Check for auto-refresh using AsyncState
            let interval = Duration::from_secs(AUTO_REFRESH_INTERVAL_SECS);
            if self.state.should_auto_refresh(self.auto_refresh, interval) {
                return Ok(Some(Action::Refresh));
            }
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        // Check loading state (show loading only if no data yet)
        let has_nodes = self
            .data()
            .map(|d| !d.node_statuses.is_empty())
            .unwrap_or(false);

        if self.state.is_loading() && !has_nodes {
            let loading = Paragraph::new("Loading lifecycle status...")
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(loading, area);
            return Ok(());
        }

        if let Some(err) = self.state.error() {
            let error =
                Paragraph::new(format!("Error: {}", err)).style(Style::default().fg(Color::Red));
            frame.render_widget(error, area);
            return Ok(());
        }

        // Get context name from data
        let context_name = self
            .data()
            .map(|d| d.context_name.clone())
            .unwrap_or_default();

        // Layout
        let chunks = Layout::vertical([
            Constraint::Length(1),  // Header
            Constraint::Length(10), // Versions section
            Constraint::Length(7),  // Pre-Operation Checks (4 lines + border + padding)
            Constraint::Min(6),     // Node table
            Constraint::Length(6),  // Alerts (3 lines + border)
            Constraint::Length(1),  // Footer
        ])
        .split(area);

        // Header
        let header = Paragraph::new(format!(" Lifecycle │ {}", context_name)).style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
        frame.render_widget(header, chunks[0]);

        // Versions section
        self.draw_versions_section(frame, chunks[1]);

        // Pre-Operation Checks
        self.draw_pre_op_checks(frame, chunks[2]);

        // Node table
        self.draw_node_table(frame, chunks[3]);

        // Alerts
        self.draw_alerts_section(frame, chunks[4]);

        // Footer
        self.draw_footer(frame, chunks[5]);

        Ok(())
    }
}
