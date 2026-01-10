//! Lifecycle component - displays version status, time sync, and config drift
//!
//! Provides a consolidated view of cluster lifecycle operations.

use crate::action::Action;
use crate::components::Component;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Row, Table, TableState},
    Frame,
};
use std::time::Instant;
use talos_rs::{TalosClient, VersionInfo};

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

impl AlertSeverity {
    fn indicator(&self) -> (&'static str, Color) {
        match self {
            AlertSeverity::Info => ("i", Color::Cyan),
            AlertSeverity::Warning => ("!", Color::Yellow),
            AlertSeverity::Error => ("X", Color::Red),
        }
    }
}

/// Lifecycle component for viewing version and lifecycle status
pub struct LifecycleComponent {
    /// Context name (cluster identifier)
    context_name: String,

    /// Talos versions per node
    versions: Vec<VersionInfo>,

    /// Node statuses
    node_statuses: Vec<NodeStatus>,

    /// Alerts
    alerts: Vec<Alert>,

    /// Currently selected node index
    selected: usize,

    /// Table state for rendering
    table_state: TableState,

    /// Loading state
    loading: bool,

    /// Error message
    error: Option<String>,

    /// Last refresh time
    last_refresh: Option<Instant>,

    /// Auto-refresh enabled
    auto_refresh: bool,

    /// Client for API calls
    client: Option<TalosClient>,
}

impl Default for LifecycleComponent {
    fn default() -> Self {
        Self::new("".to_string())
    }
}

impl LifecycleComponent {
    pub fn new(context_name: String) -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));

        Self {
            context_name,
            versions: Vec::new(),
            node_statuses: Vec::new(),
            alerts: Vec::new(),
            selected: 0,
            table_state,
            loading: true,
            error: None,
            last_refresh: None,
            auto_refresh: true,
            client: None,
        }
    }

    /// Set the client for API calls
    pub fn set_client(&mut self, client: TalosClient) {
        self.client = Some(client);
    }

    /// Set error message
    pub fn set_error(&mut self, error: String) {
        self.error = Some(error);
        self.loading = false;
    }

    /// Refresh lifecycle data
    pub async fn refresh(&mut self) -> Result<()> {
        self.loading = true;
        self.error = None;

        let Some(client) = &self.client else {
            self.error = Some("No client configured".to_string());
            self.loading = false;
            return Ok(());
        };

        // Fetch version information
        match client.version().await {
            Ok(versions) => {
                // Get context name from first node
                if versions.first().is_some() {
                    if self.context_name.is_empty() {
                        // Try to get from talosconfig
                        if let Ok(config) = talos_rs::TalosConfig::load_default() {
                            self.context_name = config.context;
                        }
                    }
                }

                // Build node statuses from version info
                self.node_statuses = versions
                    .iter()
                    .map(|v| NodeStatus {
                        hostname: v.node.clone(),
                        version: v.version.clone(),
                        config_hash: None, // Needs MachineConfig API
                        time_synced: None, // Needs TimeStatus API
                        ready: true,       // Assume ready for now
                        platform: v.platform.clone(),
                    })
                    .collect();

                self.versions = versions;
                self.generate_alerts();
            }
            Err(e) => {
                self.error = Some(format!("Failed to fetch versions: {}", e));
            }
        }

        self.loading = false;
        self.last_refresh = Some(Instant::now());
        Ok(())
    }

    /// Generate alerts based on current state
    fn generate_alerts(&mut self) {
        self.alerts.clear();

        // Check for version mismatches
        let versions: Vec<&str> = self.versions.iter().map(|v| v.version.as_str()).collect();
        let unique_versions: std::collections::HashSet<_> = versions.iter().collect();
        if unique_versions.len() > 1 {
            self.alerts.push(Alert {
                severity: AlertSeverity::Warning,
                message: "Version mismatch detected across nodes".to_string(),
            });
        }

        // Placeholder alerts for features needing API extensions
        self.alerts.push(Alert {
            severity: AlertSeverity::Info,
            message: "Time sync status requires TimeStatus API (run: talosctl get timestatus)".to_string(),
        });

        self.alerts.push(Alert {
            severity: AlertSeverity::Info,
            message: "Config hash requires MachineConfig API (run: talosctl get machineconfig)".to_string(),
        });
    }

    /// Get current Talos version (from first node)
    fn current_talos_version(&self) -> &str {
        self.versions.first().map(|v| v.version.as_str()).unwrap_or("unknown")
    }

    /// Get K8s version support range for current Talos
    fn k8s_support_range(&self) -> &str {
        // Talos 1.9.x supports k8s 1.27-1.32
        // This is hardcoded for now - could be fetched from release API
        let version = self.current_talos_version();
        if version.starts_with("v1.9") {
            "v1.27 - v1.32"
        } else if version.starts_with("v1.8") {
            "v1.26 - v1.31"
        } else if version.starts_with("v1.7") {
            "v1.25 - v1.30"
        } else {
            "unknown"
        }
    }

    /// Navigation: select next node
    fn select_next(&mut self) {
        if self.node_statuses.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.node_statuses.len();
        self.table_state.select(Some(self.selected));
    }

    /// Navigation: select previous node
    fn select_prev(&mut self) {
        if self.node_statuses.is_empty() {
            return;
        }
        if self.selected == 0 {
            self.selected = self.node_statuses.len() - 1;
        } else {
            self.selected -= 1;
        }
        self.table_state.select(Some(self.selected));
    }

    /// Draw the cluster versions section
    fn draw_versions_section(&self, frame: &mut Frame, area: Rect) {
        let version = self.current_talos_version();
        let k8s_range = self.k8s_support_range();

        // Get platform from first node
        let platform = self
            .versions
            .first()
            .map(|v| v.platform.as_str())
            .unwrap_or("unknown");

        let lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  Talos Version", Style::default().fg(Color::Yellow)),
            ]),
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
            Line::from(vec![
                Span::styled("  Kubernetes Support", Style::default().fg(Color::Yellow)),
            ]),
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
                ratatui::widgets::Cell::from(*h)
                    .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
            });
        let header = Row::new(header_cells).height(1);

        let rows = self.node_statuses.iter().map(|node| {
            let config_cell = match &node.config_hash {
                Some(hash) => Span::styled(&hash[..8.min(hash.len())], Style::default().fg(Color::White)),
                None => Span::styled("-", Style::default().fg(Color::DarkGray)),
            };

            let time_cell = match node.time_synced {
                Some(true) => Span::styled("synced", Style::default().fg(Color::Green)),
                Some(false) => Span::styled("not synced", Style::default().fg(Color::Yellow)),
                None => Span::styled("-", Style::default().fg(Color::DarkGray)),
            };

            let status_cell = if node.ready {
                Span::styled("Ready", Style::default().fg(Color::Green))
            } else {
                Span::styled("NotReady", Style::default().fg(Color::Red))
            };

            Row::new(vec![
                ratatui::widgets::Cell::from(node.hostname.clone()),
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
        let lines: Vec<Line> = self
            .alerts
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

        let footer = Paragraph::new(Line::from(spans))
            .style(Style::default().fg(Color::DarkGray));
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
            // Check for auto-refresh
            if self.auto_refresh && !self.loading {
                if let Some(last) = self.last_refresh {
                    let interval = std::time::Duration::from_secs(AUTO_REFRESH_INTERVAL_SECS);
                    if last.elapsed() >= interval {
                        return Ok(Some(Action::Refresh));
                    }
                }
            }
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        if self.loading && self.node_statuses.is_empty() {
            let loading = Paragraph::new("Loading lifecycle status...")
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(loading, area);
            return Ok(());
        }

        if let Some(ref err) = self.error {
            let error = Paragraph::new(format!("Error: {}", err))
                .style(Style::default().fg(Color::Red));
            frame.render_widget(error, area);
            return Ok(());
        }

        // Layout
        let chunks = Layout::vertical([
            Constraint::Length(1),  // Header
            Constraint::Length(10), // Versions section
            Constraint::Min(8),     // Node table
            Constraint::Length(6),  // Alerts (3 lines + border)
            Constraint::Length(1),  // Footer
        ])
        .split(area);

        // Header
        let header = Paragraph::new(format!(" Lifecycle │ {}", self.context_name))
            .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
        frame.render_widget(header, chunks[0]);

        // Versions section
        self.draw_versions_section(frame, chunks[1]);

        // Node table
        self.draw_node_table(frame, chunks[2]);

        // Alerts
        self.draw_alerts_section(frame, chunks[3]);

        // Footer
        self.draw_footer(frame, chunks[4]);

        Ok(())
    }
}
