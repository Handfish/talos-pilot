//! Insecure mode component - for connecting to maintenance mode nodes
//!
//! This component provides a simplified UI for nodes that haven't been
//! bootstrapped yet. It shows disk and volume information without requiring
//! TLS client certificates, and supports generating/applying machine configs.

use crate::action::Action;
use crate::components::Component;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState},
};
use talos_pilot_core::AsyncState;
use talos_rs::{
    DiskInfo, GenConfigResult, InsecureVersionInfo, VolumeStatus, apply_config_insecure,
    gen_config, get_disks_insecure, get_version_insecure, get_volume_status_insecure,
};

/// View mode for the insecure component
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InsecureViewMode {
    #[default]
    Disks,
    Volumes,
}

impl InsecureViewMode {
    pub fn next(&self) -> Self {
        match self {
            InsecureViewMode::Disks => InsecureViewMode::Volumes,
            InsecureViewMode::Volumes => InsecureViewMode::Disks,
        }
    }
}

/// Dialog mode for input
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DialogMode {
    #[default]
    None,
    /// Generate config dialog with fields: cluster_name, k8s_endpoint, output_dir
    GenerateConfig {
        cluster_name: String,
        k8s_endpoint: String,
        output_dir: String,
        active_field: usize, // 0=cluster_name, 1=k8s_endpoint, 2=output_dir
    },
    /// Apply config dialog with field: config_path
    ApplyConfig {
        config_path: String,
        node_type: NodeType, // controlplane or worker
    },
    /// Show result of an operation
    ShowResult {
        title: String,
        message: String,
        success: bool,
    },
    /// Confirm action
    Confirm {
        title: String,
        message: String,
        action: ConfirmAction,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NodeType {
    #[default]
    Controlplane,
    Worker,
}

impl NodeType {
    fn toggle(&self) -> Self {
        match self {
            NodeType::Controlplane => NodeType::Worker,
            NodeType::Worker => NodeType::Controlplane,
        }
    }

    fn config_filename(&self) -> &'static str {
        match self {
            NodeType::Controlplane => "controlplane.yaml",
            NodeType::Worker => "worker.yaml",
        }
    }

    #[allow(dead_code)]
    fn label(&self) -> &'static str {
        match self {
            NodeType::Controlplane => "Control Plane",
            NodeType::Worker => "Worker",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmAction {
    ApplyConfig(String), // config_path
}

/// Data loaded in insecure mode
#[derive(Debug, Clone, Default)]
pub struct InsecureData {
    /// Endpoint address
    pub endpoint: String,
    /// Version info (if available)
    pub version: Option<InsecureVersionInfo>,
    /// Physical disks
    pub disks: Vec<DiskInfo>,
    /// Volume status
    pub volumes: Vec<VolumeStatus>,
    /// Whether connected
    pub connected: bool,
}

/// Insecure mode component for maintenance mode nodes
pub struct InsecureComponent {
    /// Async state wrapping all data
    state: AsyncState<InsecureData>,

    /// Endpoint to connect to
    endpoint: String,

    /// Current view mode (Disks or Volumes)
    view_mode: InsecureViewMode,

    /// Current dialog mode
    dialog_mode: DialogMode,

    /// Table state for disk list
    disk_table_state: TableState,

    /// Table state for volume list
    volume_table_state: TableState,

    /// Last generated config result (for apply default path)
    last_gen_result: Option<GenConfigResult>,
}

impl InsecureComponent {
    pub fn new(endpoint: String) -> Self {
        let mut disk_table_state = TableState::default();
        disk_table_state.select(Some(0));
        let mut volume_table_state = TableState::default();
        volume_table_state.select(Some(0));

        let initial_data = InsecureData {
            endpoint: endpoint.clone(),
            ..Default::default()
        };

        Self {
            state: AsyncState::with_data(initial_data),
            endpoint,
            view_mode: InsecureViewMode::Disks,
            dialog_mode: DialogMode::None,
            disk_table_state,
            volume_table_state,
            last_gen_result: None,
        }
    }

    /// Extract just the IP/hostname from endpoint (strip port if present)
    fn endpoint_for_talosctl(endpoint: &str) -> String {
        if let Some(idx) = endpoint.rfind(':') {
            if endpoint.matches(':').count() > 1 {
                endpoint.to_string()
            } else {
                endpoint[..idx].to_string()
            }
        } else {
            endpoint.to_string()
        }
    }

    /// Get the endpoint IP for use in defaults
    fn endpoint_ip(&self) -> String {
        Self::endpoint_for_talosctl(&self.endpoint)
    }

    /// Connect and load data
    pub async fn connect(&mut self) -> Result<()> {
        self.state.start_loading();

        let endpoint = Self::endpoint_for_talosctl(&self.endpoint);
        let mut data = InsecureData {
            endpoint: endpoint.clone(),
            ..Default::default()
        };

        match get_version_insecure(&endpoint).await {
            Ok(version) => {
                data.version = Some(version);
            }
            Err(e) => {
                tracing::debug!("Version info not available: {}", e);
            }
        }

        match get_disks_insecure(&endpoint).await {
            Ok(disks) => {
                data.disks = disks;
                data.connected = true;
            }
            Err(e) => {
                self.state.set_error(format!("Failed to connect: {}", e));
                return Ok(());
            }
        }

        match get_volume_status_insecure(&endpoint).await {
            Ok(volumes) => {
                data.volumes = volumes;
            }
            Err(e) => {
                tracing::debug!("Volume info not available: {}", e);
            }
        }

        self.state.set_data(data);
        Ok(())
    }

    /// Refresh data
    pub async fn refresh(&mut self) -> Result<()> {
        self.connect().await
    }

    /// Generate config with the provided parameters
    pub async fn do_generate_config(
        &mut self,
        cluster_name: &str,
        k8s_endpoint: &str,
        output_dir: &str,
    ) {
        let endpoint_ip = self.endpoint_ip();
        let sans: Vec<&str> = vec![&endpoint_ip, "127.0.0.1"];

        match gen_config(cluster_name, k8s_endpoint, output_dir, Some(&sans), true).await {
            Ok(result) => {
                self.last_gen_result = Some(result.clone());
                self.dialog_mode = DialogMode::ShowResult {
                    title: "Config Generated".to_string(),
                    message: format!(
                        "Generated configuration files:\n\n\
                         - {}\n\
                         - {}\n\
                         - {}\n\n\
                         Press 'a' to apply the controlplane config to this node.",
                        result.controlplane_path, result.worker_path, result.talosconfig_path
                    ),
                    success: true,
                };
            }
            Err(e) => {
                self.dialog_mode = DialogMode::ShowResult {
                    title: "Generation Failed".to_string(),
                    message: format!("Failed to generate config:\n\n{}", e),
                    success: false,
                };
            }
        }
    }

    /// Apply config to the node
    pub async fn do_apply_config(&mut self, config_path: &str) {
        let endpoint = self.endpoint_ip();

        match apply_config_insecure(&endpoint, config_path).await {
            Ok(result) => {
                self.dialog_mode = DialogMode::ShowResult {
                    title: if result.success {
                        "Config Applied".to_string()
                    } else {
                        "Apply Failed".to_string()
                    },
                    message: result.message,
                    success: result.success,
                };
            }
            Err(e) => {
                self.dialog_mode = DialogMode::ShowResult {
                    title: "Apply Failed".to_string(),
                    message: format!("Failed to apply config:\n\n{}", e),
                    success: false,
                };
            }
        }
    }

    /// Open generate config dialog with smart defaults
    fn open_generate_dialog(&mut self) {
        let endpoint_ip = self.endpoint_ip();
        self.dialog_mode = DialogMode::GenerateConfig {
            cluster_name: "talos-cluster".to_string(),
            k8s_endpoint: format!("https://{}:6443", endpoint_ip),
            output_dir: ".".to_string(),
            active_field: 0,
        };
    }

    /// Open apply config dialog with smart defaults
    fn open_apply_dialog(&mut self) {
        let default_path = self
            .last_gen_result
            .as_ref()
            .map(|r| r.controlplane_path.clone())
            .unwrap_or_else(|| "./controlplane.yaml".to_string());

        self.dialog_mode = DialogMode::ApplyConfig {
            config_path: default_path,
            node_type: NodeType::Controlplane,
        };
    }

    fn data(&self) -> Option<&InsecureData> {
        self.state.data()
    }

    fn selected_disk_index(&self) -> usize {
        self.disk_table_state.selected().unwrap_or(0)
    }

    fn selected_volume_index(&self) -> usize {
        self.volume_table_state.selected().unwrap_or(0)
    }

    fn select_prev(&mut self) {
        match self.view_mode {
            InsecureViewMode::Disks => {
                if let Some(data) = self.data()
                    && !data.disks.is_empty()
                {
                    let i = self.selected_disk_index();
                    let new_i = if i == 0 { data.disks.len() - 1 } else { i - 1 };
                    self.disk_table_state.select(Some(new_i));
                }
            }
            InsecureViewMode::Volumes => {
                if let Some(data) = self.data()
                    && !data.volumes.is_empty()
                {
                    let i = self.selected_volume_index();
                    let new_i = if i == 0 {
                        data.volumes.len() - 1
                    } else {
                        i - 1
                    };
                    self.volume_table_state.select(Some(new_i));
                }
            }
        }
    }

    fn select_next(&mut self) {
        match self.view_mode {
            InsecureViewMode::Disks => {
                if let Some(data) = self.data()
                    && !data.disks.is_empty()
                {
                    let i = self.selected_disk_index();
                    let new_i = (i + 1) % data.disks.len();
                    self.disk_table_state.select(Some(new_i));
                }
            }
            InsecureViewMode::Volumes => {
                if let Some(data) = self.data()
                    && !data.volumes.is_empty()
                {
                    let i = self.selected_volume_index();
                    let new_i = (i + 1) % data.volumes.len();
                    self.volume_table_state.select(Some(new_i));
                }
            }
        }
    }

    /// Handle key events in dialog mode
    fn handle_dialog_key(&mut self, key: KeyEvent) -> Option<Action> {
        match &mut self.dialog_mode {
            DialogMode::None => None,

            DialogMode::GenerateConfig {
                cluster_name,
                k8s_endpoint,
                output_dir,
                active_field,
            } => {
                match key.code {
                    KeyCode::Esc => {
                        self.dialog_mode = DialogMode::None;
                        None
                    }
                    KeyCode::Tab | KeyCode::Down => {
                        *active_field = (*active_field + 1) % 3;
                        None
                    }
                    KeyCode::BackTab | KeyCode::Up => {
                        *active_field = if *active_field == 0 {
                            2
                        } else {
                            *active_field - 1
                        };
                        None
                    }
                    KeyCode::Enter => {
                        // Trigger generate action
                        Some(Action::InsecureGenConfig(
                            cluster_name.clone(),
                            k8s_endpoint.clone(),
                            output_dir.clone(),
                        ))
                    }
                    KeyCode::Char(c) => {
                        let field = match *active_field {
                            0 => cluster_name,
                            1 => k8s_endpoint,
                            _ => output_dir,
                        };
                        field.push(c);
                        None
                    }
                    KeyCode::Backspace => {
                        let field = match *active_field {
                            0 => cluster_name,
                            1 => k8s_endpoint,
                            _ => output_dir,
                        };
                        field.pop();
                        None
                    }
                    _ => None,
                }
            }

            DialogMode::ApplyConfig {
                config_path,
                node_type,
            } => {
                match key.code {
                    KeyCode::Esc => {
                        self.dialog_mode = DialogMode::None;
                        None
                    }
                    KeyCode::Tab => {
                        // Toggle between controlplane and worker
                        *node_type = node_type.toggle();
                        // Update path based on type
                        if let Some(result) = &self.last_gen_result {
                            *config_path = match node_type {
                                NodeType::Controlplane => result.controlplane_path.clone(),
                                NodeType::Worker => result.worker_path.clone(),
                            };
                        } else {
                            let dir = std::path::Path::new(config_path.as_str())
                                .parent()
                                .map(|p| p.to_string_lossy().to_string())
                                .unwrap_or_else(|| ".".to_string());
                            *config_path = format!("{}/{}", dir, node_type.config_filename());
                        }
                        None
                    }
                    KeyCode::Enter => {
                        // Show confirmation
                        let path = config_path.clone();
                        self.dialog_mode = DialogMode::Confirm {
                            title: "Apply Configuration?".to_string(),
                            message: format!(
                                "This will apply the configuration from:\n\n  {}\n\n\
                                 The node will install Talos to disk and REBOOT.\n\n\
                                 Press Enter to confirm, Esc to cancel.",
                                path
                            ),
                            action: ConfirmAction::ApplyConfig(path),
                        };
                        None
                    }
                    KeyCode::Char(c) => {
                        config_path.push(c);
                        None
                    }
                    KeyCode::Backspace => {
                        config_path.pop();
                        None
                    }
                    _ => None,
                }
            }

            DialogMode::ShowResult { .. } => match key.code {
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => {
                    self.dialog_mode = DialogMode::None;
                    None
                }
                _ => None,
            },

            DialogMode::Confirm { action, .. } => match key.code {
                KeyCode::Enter => {
                    let action = action.clone();
                    self.dialog_mode = DialogMode::None;
                    match action {
                        ConfirmAction::ApplyConfig(path) => Some(Action::InsecureApplyConfig(path)),
                    }
                }
                KeyCode::Esc => {
                    self.dialog_mode = DialogMode::None;
                    None
                }
                _ => None,
            },
        }
    }

    fn draw_warning_banner(&self, frame: &mut Frame, area: Rect) {
        let warning = Paragraph::new(Line::from(vec![
            Span::styled(
                " INSECURE MODE ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Connected without TLS authentication - "),
            Span::styled("Maintenance Mode", Style::default().fg(Color::Yellow)),
        ]))
        .style(Style::default().bg(Color::DarkGray));

        frame.render_widget(warning, area);
    }

    fn draw_connection_info(&self, frame: &mut Frame, area: Rect) {
        let data = self.data();

        let status = if let Some(d) = data {
            if d.connected {
                let version_str = d
                    .version
                    .as_ref()
                    .map(|v| {
                        if v.maintenance_mode {
                            "Maintenance Mode".to_string()
                        } else {
                            format!("Talos {}", v.tag)
                        }
                    })
                    .unwrap_or_else(|| "Maintenance Mode".to_string());

                Line::from(vec![
                    Span::styled("Endpoint: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(&d.endpoint, Style::default().fg(Color::White)),
                    Span::raw("  |  "),
                    Span::styled("Status: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(version_str, Style::default().fg(Color::Green)),
                ])
            } else {
                Line::from(vec![
                    Span::styled("Endpoint: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(&self.endpoint, Style::default().fg(Color::White)),
                    Span::raw("  |  "),
                    Span::styled("Status: ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Disconnected", Style::default().fg(Color::Red)),
                ])
            }
        } else if self.state.is_loading() {
            Line::from(vec![
                Span::styled("Connecting to ", Style::default().fg(Color::DarkGray)),
                Span::styled(&self.endpoint, Style::default().fg(Color::White)),
                Span::raw("..."),
            ])
        } else {
            Line::from(vec![Span::styled(
                "Not connected",
                Style::default().fg(Color::Red),
            )])
        };

        let info = Paragraph::new(status);
        frame.render_widget(info, area);
    }

    fn draw_tabs(&self, frame: &mut Frame, area: Rect) {
        let tabs = Line::from(vec![
            Span::raw(" "),
            if self.view_mode == InsecureViewMode::Disks {
                Span::styled(
                    " Disks ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(" Disks ", Style::default().fg(Color::DarkGray))
            },
            Span::raw(" "),
            if self.view_mode == InsecureViewMode::Volumes {
                Span::styled(
                    " Volumes ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(" Volumes ", Style::default().fg(Color::DarkGray))
            },
            Span::raw("  "),
            Span::styled("[Tab]", Style::default().fg(Color::DarkGray)),
            Span::styled(" switch", Style::default().fg(Color::DarkGray)),
        ]);

        frame.render_widget(Paragraph::new(tabs), area);
    }

    fn draw_disks(&mut self, frame: &mut Frame, area: Rect) {
        let data = self.data();
        let empty_disks: Vec<DiskInfo> = vec![];
        let disks = data.map(|d| &d.disks).unwrap_or(&empty_disks);

        let header = Row::new(vec![
            Cell::from("Device").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Size").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Type").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Transport").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Model").style(Style::default().add_modifier(Modifier::BOLD)),
        ])
        .height(1)
        .style(Style::default().fg(Color::Cyan));

        let rows: Vec<Row> = disks
            .iter()
            .map(|disk| {
                let disk_type = if disk.readonly {
                    ("CD-ROM", Color::Magenta)
                } else if disk.rotational {
                    ("HDD", Color::Yellow)
                } else {
                    ("SSD", Color::Green)
                };

                Row::new(vec![
                    Cell::from(disk.dev_path.clone()),
                    Cell::from(disk.size_pretty.clone()),
                    Cell::from(disk_type.0).style(Style::default().fg(disk_type.1)),
                    Cell::from(disk.transport.clone().unwrap_or_default()),
                    Cell::from(disk.model.clone().unwrap_or_default()),
                ])
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(15),
                Constraint::Length(10),
                Constraint::Length(8),
                Constraint::Length(12),
                Constraint::Fill(1),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Disks ({}) ", disks.len())),
        )
        .row_highlight_style(Style::default().bg(Color::DarkGray));

        frame.render_stateful_widget(table, area, &mut self.disk_table_state);
    }

    fn draw_volumes(&mut self, frame: &mut Frame, area: Rect) {
        let data = self.data();
        let empty_volumes: Vec<VolumeStatus> = vec![];
        let volumes = data.map(|d| &d.volumes).unwrap_or(&empty_volumes);

        let header = Row::new(vec![
            Cell::from("Volume").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Size").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Phase").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Location").style(Style::default().add_modifier(Modifier::BOLD)),
        ])
        .height(1)
        .style(Style::default().fg(Color::Cyan));

        let rows: Vec<Row> = volumes
            .iter()
            .map(|vol| {
                let phase_color = match vol.phase.as_str() {
                    "ready" => Color::Green,
                    "waiting" => Color::Yellow,
                    _ => Color::Red,
                };

                Row::new(vec![
                    Cell::from(vol.id.clone()),
                    Cell::from(vol.size.clone()),
                    Cell::from(vol.phase.clone()).style(Style::default().fg(phase_color)),
                    Cell::from(vol.mount_location.clone().unwrap_or_default()),
                ])
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(20),
                Constraint::Length(12),
                Constraint::Length(10),
                Constraint::Fill(1),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Volumes ({}) ", volumes.len())),
        )
        .row_highlight_style(Style::default().bg(Color::DarkGray));

        frame.render_stateful_widget(table, area, &mut self.volume_table_state);
    }

    fn draw_help(&self, frame: &mut Frame, area: Rect) {
        let help = Line::from(vec![
            Span::styled(" [g] ", Style::default().fg(Color::Green)),
            Span::raw("Generate Config"),
            Span::raw("  "),
            Span::styled(" [a] ", Style::default().fg(Color::Yellow)),
            Span::raw("Apply Config"),
            Span::raw("  "),
            Span::styled(" [Tab] ", Style::default().fg(Color::Cyan)),
            Span::raw("Switch"),
            Span::raw("  "),
            Span::styled(" [r] ", Style::default().fg(Color::Cyan)),
            Span::raw("Refresh"),
            Span::raw("  "),
            Span::styled(" [q] ", Style::default().fg(Color::Cyan)),
            Span::raw("Quit"),
        ]);

        frame.render_widget(Paragraph::new(help), area);
    }

    fn draw_dialog(&self, frame: &mut Frame, area: Rect) {
        match &self.dialog_mode {
            DialogMode::None => {}

            DialogMode::GenerateConfig {
                cluster_name,
                k8s_endpoint,
                output_dir,
                active_field,
            } => {
                let dialog_area = centered_rect(60, 14, area);
                frame.render_widget(Clear, dialog_area);

                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Green))
                    .title(" Generate Talos Config ");

                let inner = block.inner(dialog_area);
                frame.render_widget(block, dialog_area);

                let layout = Layout::vertical([
                    Constraint::Length(1), // Instructions
                    Constraint::Length(1), // Spacer
                    Constraint::Length(2), // Cluster name
                    Constraint::Length(2), // K8s endpoint
                    Constraint::Length(2), // Output dir
                    Constraint::Length(1), // Spacer
                    Constraint::Length(1), // Help
                ])
                .split(inner);

                let instructions =
                    Paragraph::new("Enter configuration details (press Enter to generate):")
                        .style(Style::default().fg(Color::DarkGray));
                frame.render_widget(instructions, layout[0]);

                // Cluster name field
                let cluster_style = if *active_field == 0 {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::White)
                };
                let cluster_field = Paragraph::new(Line::from(vec![
                    Span::styled("Cluster Name: ", Style::default().fg(Color::Cyan)),
                    Span::styled(cluster_name.as_str(), cluster_style),
                    if *active_field == 0 {
                        Span::styled("_", Style::default().fg(Color::Yellow))
                    } else {
                        Span::raw("")
                    },
                ]));
                frame.render_widget(cluster_field, layout[2]);

                // K8s endpoint field
                let endpoint_style = if *active_field == 1 {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::White)
                };
                let endpoint_field = Paragraph::new(Line::from(vec![
                    Span::styled("K8s Endpoint: ", Style::default().fg(Color::Cyan)),
                    Span::styled(k8s_endpoint.as_str(), endpoint_style),
                    if *active_field == 1 {
                        Span::styled("_", Style::default().fg(Color::Yellow))
                    } else {
                        Span::raw("")
                    },
                ]));
                frame.render_widget(endpoint_field, layout[3]);

                // Output dir field
                let dir_style = if *active_field == 2 {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::White)
                };
                let dir_field = Paragraph::new(Line::from(vec![
                    Span::styled("Output Dir:   ", Style::default().fg(Color::Cyan)),
                    Span::styled(output_dir.as_str(), dir_style),
                    if *active_field == 2 {
                        Span::styled("_", Style::default().fg(Color::Yellow))
                    } else {
                        Span::raw("")
                    },
                ]));
                frame.render_widget(dir_field, layout[4]);

                let help = Paragraph::new("[Tab] Next field  [Enter] Generate  [Esc] Cancel")
                    .style(Style::default().fg(Color::DarkGray));
                frame.render_widget(help, layout[6]);
            }

            DialogMode::ApplyConfig {
                config_path,
                node_type,
            } => {
                let dialog_area = centered_rect(60, 10, area);
                frame.render_widget(Clear, dialog_area);

                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow))
                    .title(" Apply Configuration ");

                let inner = block.inner(dialog_area);
                frame.render_widget(block, dialog_area);

                let layout = Layout::vertical([
                    Constraint::Length(1), // Instructions
                    Constraint::Length(1), // Spacer
                    Constraint::Length(1), // Node type
                    Constraint::Length(2), // Config path
                    Constraint::Length(1), // Spacer
                    Constraint::Length(1), // Help
                ])
                .split(inner);

                let instructions = Paragraph::new("Apply machine configuration to this node:")
                    .style(Style::default().fg(Color::DarkGray));
                frame.render_widget(instructions, layout[0]);

                // Node type selector
                let type_line = Paragraph::new(Line::from(vec![
                    Span::styled("Node Type:    ", Style::default().fg(Color::Cyan)),
                    if *node_type == NodeType::Controlplane {
                        Span::styled(
                            " Control Plane ",
                            Style::default().fg(Color::Black).bg(Color::Green),
                        )
                    } else {
                        Span::styled(" Control Plane ", Style::default().fg(Color::DarkGray))
                    },
                    Span::raw(" "),
                    if *node_type == NodeType::Worker {
                        Span::styled(
                            " Worker ",
                            Style::default().fg(Color::Black).bg(Color::Blue),
                        )
                    } else {
                        Span::styled(" Worker ", Style::default().fg(Color::DarkGray))
                    },
                    Span::styled("  [Tab to switch]", Style::default().fg(Color::DarkGray)),
                ]));
                frame.render_widget(type_line, layout[2]);

                // Config path field
                let path_field = Paragraph::new(Line::from(vec![
                    Span::styled("Config File:  ", Style::default().fg(Color::Cyan)),
                    Span::styled(config_path.as_str(), Style::default().fg(Color::Yellow)),
                    Span::styled("_", Style::default().fg(Color::Yellow)),
                ]));
                frame.render_widget(path_field, layout[3]);

                let help = Paragraph::new("[Tab] Switch type  [Enter] Apply  [Esc] Cancel")
                    .style(Style::default().fg(Color::DarkGray));
                frame.render_widget(help, layout[5]);
            }

            DialogMode::ShowResult {
                title,
                message,
                success,
            } => {
                let dialog_area = centered_rect(60, 12, area);
                frame.render_widget(Clear, dialog_area);

                let border_color = if *success { Color::Green } else { Color::Red };
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_color))
                    .title(format!(" {} ", title));

                let inner = block.inner(dialog_area);
                frame.render_widget(block, dialog_area);

                let layout = Layout::vertical([
                    Constraint::Fill(1),   // Message
                    Constraint::Length(1), // Help
                ])
                .split(inner);

                let msg = Paragraph::new(message.as_str()).style(Style::default().fg(Color::White));
                frame.render_widget(msg, layout[0]);

                let help = Paragraph::new("[Enter] or [Esc] to close")
                    .style(Style::default().fg(Color::DarkGray));
                frame.render_widget(help, layout[1]);
            }

            DialogMode::Confirm {
                title,
                message,
                action: _,
            } => {
                let dialog_area = centered_rect(60, 12, area);
                frame.render_widget(Clear, dialog_area);

                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow))
                    .title(format!(" {} ", title));

                let inner = block.inner(dialog_area);
                frame.render_widget(block, dialog_area);

                let layout = Layout::vertical([
                    Constraint::Fill(1),   // Message
                    Constraint::Length(1), // Help
                ])
                .split(inner);

                let msg = Paragraph::new(message.as_str()).style(Style::default().fg(Color::White));
                frame.render_widget(msg, layout[0]);

                let help = Paragraph::new("[Enter] Confirm  [Esc] Cancel")
                    .style(Style::default().fg(Color::DarkGray));
                frame.render_widget(help, layout[1]);
            }
        }
    }
}

/// Helper function to create a centered rectangle
fn centered_rect(percent_x: u16, height: u16, r: Rect) -> Rect {
    let popup_layout = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height),
        Constraint::Fill(1),
    ])
    .split(r);

    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(popup_layout[1])[1]
}

impl Component for InsecureComponent {
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        // If in dialog mode, handle dialog keys first
        if self.dialog_mode != DialogMode::None {
            return Ok(self.handle_dialog_key(key));
        }

        // Normal mode key handling
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Ok(Some(Action::Quit)),
            KeyCode::Char('r') => Ok(Some(Action::Refresh)),
            KeyCode::Char('g') => {
                self.open_generate_dialog();
                Ok(None)
            }
            KeyCode::Char('a') => {
                self.open_apply_dialog();
                Ok(None)
            }
            KeyCode::Tab => {
                self.view_mode = self.view_mode.next();
                Ok(None)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.select_prev();
                Ok(None)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.select_next();
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn update(&mut self, _action: Action) -> Result<Option<Action>> {
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        let layout = Layout::vertical([
            Constraint::Length(1), // Warning banner
            Constraint::Length(1), // Connection info
            Constraint::Length(1), // Tabs
            Constraint::Fill(1),   // Content
            Constraint::Length(1), // Help
        ])
        .split(area);

        self.draw_warning_banner(frame, layout[0]);
        self.draw_connection_info(frame, layout[1]);
        self.draw_tabs(frame, layout[2]);

        if self.state.is_loading() && !self.state.has_data() {
            let loading = Paragraph::new("Connecting...")
                .style(Style::default().fg(Color::Yellow))
                .block(Block::default().borders(Borders::ALL));
            frame.render_widget(loading, layout[3]);
        } else if let Some(error) = self.state.error() {
            let error_widget = Paragraph::new(error.to_string())
                .style(Style::default().fg(Color::Red))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Error ")
                        .border_style(Style::default().fg(Color::Red)),
                );
            frame.render_widget(error_widget, layout[3]);
        } else {
            match self.view_mode {
                InsecureViewMode::Disks => self.draw_disks(frame, layout[3]),
                InsecureViewMode::Volumes => self.draw_volumes(frame, layout[3]),
            }
        }

        self.draw_help(frame, layout[4]);

        // Draw dialog overlay if active
        if self.dialog_mode != DialogMode::None {
            self.draw_dialog(frame, area);
        }

        Ok(())
    }
}
