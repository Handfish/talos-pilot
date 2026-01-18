//! Storage component - displays disk and volume information
//!
//! Shows physical disks and Talos volume status for a node.

use crate::action::Action;
use crate::components::Component;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};
use std::time::Duration;
use talos_pilot_core::{AsyncState, format_bytes};
use talos_rs::{
    DiskInfo, TalosClient, VolumeStatus, get_disks_for_node, get_volume_status_for_node,
};

/// Auto-refresh interval in seconds
const AUTO_REFRESH_INTERVAL_SECS: u64 = 30;

/// View mode for the storage component
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StorageViewMode {
    #[default]
    Disks,
    Volumes,
}

impl StorageViewMode {
    pub fn next(&self) -> Self {
        match self {
            StorageViewMode::Disks => StorageViewMode::Volumes,
            StorageViewMode::Volumes => StorageViewMode::Disks,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            StorageViewMode::Disks => "Disks",
            StorageViewMode::Volumes => "Volumes",
        }
    }
}

/// View mode for group storage
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GroupViewMode {
    /// Interleaved storage data from all nodes (default)
    #[default]
    Interleaved,
    /// Storage data organized by node (tabbed view)
    ByNode,
}

/// Per-node storage data for group view
#[derive(Debug, Clone, Default)]
pub struct NodeStorageData {
    /// Node hostname
    pub hostname: String,
    /// Disks from this node
    pub disks: Vec<DiskInfo>,
    /// Volumes from this node
    pub volumes: Vec<VolumeStatus>,
}

/// Loaded storage data (wrapped by AsyncState)
#[derive(Debug, Clone, Default)]
pub struct StorageData {
    /// Node hostname
    pub hostname: String,
    /// Node address
    pub address: String,
    /// Physical disks
    pub disks: Vec<DiskInfo>,
    /// Volume status
    pub volumes: Vec<VolumeStatus>,
}

/// Storage component for viewing disk and volume information
pub struct StorageComponent {
    /// Async state wrapping all storage data
    state: AsyncState<StorageData>,

    /// Current view mode (Disks or Volumes)
    view_mode: StorageViewMode,

    /// Table state for disk list
    disk_table_state: TableState,

    /// Table state for volume list
    volume_table_state: TableState,

    /// Auto-refresh enabled
    auto_refresh: bool,

    /// Client for API calls (unused but kept for consistency)
    #[allow(dead_code)]
    client: Option<TalosClient>,

    /// Node address for talosctl commands
    node_address: Option<String>,

    /// Context name for authentication
    context: Option<String>,

    /// Config path for authentication
    config_path: Option<String>,

    // Group view fields
    /// Whether this is a group view (multiple nodes)
    is_group_view: bool,
    /// Group name (e.g., "Control Plane", "Workers")
    group_name: String,
    /// Nodes in the group: Vec<(hostname, ip)>
    nodes: Vec<(String, String)>,
    /// Current view mode for group storage
    group_view_mode: GroupViewMode,
    /// Per-node storage data
    node_data: std::collections::HashMap<String, NodeStorageData>,
    /// Selected node tab index (for ByNode view mode)
    selected_node_tab: usize,
}

impl Default for StorageComponent {
    fn default() -> Self {
        Self::new("".to_string(), "".to_string(), None, None)
    }
}

impl StorageComponent {
    pub fn new(
        hostname: String,
        address: String,
        context: Option<String>,
        config_path: Option<String>,
    ) -> Self {
        let mut disk_table_state = TableState::default();
        disk_table_state.select(Some(0));
        let mut volume_table_state = TableState::default();
        volume_table_state.select(Some(0));

        let initial_data = StorageData {
            hostname,
            address: address.clone(),
            ..Default::default()
        };

        let node_address = if address.is_empty() {
            None
        } else {
            // Extract IP from address (remove port if present)
            Some(address.split(':').next().unwrap_or(&address).to_string())
        };

        Self {
            state: AsyncState::with_data(initial_data),
            view_mode: StorageViewMode::Disks,
            disk_table_state,
            volume_table_state,
            auto_refresh: true,
            client: None,
            node_address,
            context,
            config_path,
            // Group view fields (not used in single node mode)
            is_group_view: false,
            group_name: String::new(),
            nodes: Vec::new(),
            group_view_mode: GroupViewMode::default(),
            node_data: std::collections::HashMap::new(),
            selected_node_tab: 0,
        }
    }

    /// Create a new storage component for group view (multiple nodes)
    /// - group_name: Name of the group (e.g., "Control Plane", "Workers")
    /// - nodes: Vec of (hostname, ip) for each node
    pub fn new_group(group_name: String, nodes: Vec<(String, String)>) -> Self {
        let mut disk_table_state = TableState::default();
        disk_table_state.select(Some(0));
        let mut volume_table_state = TableState::default();
        volume_table_state.select(Some(0));

        let initial_data = StorageData {
            hostname: group_name.clone(),
            address: String::new(),
            ..Default::default()
        };

        Self {
            state: AsyncState::with_data(initial_data),
            view_mode: StorageViewMode::Disks,
            disk_table_state,
            volume_table_state,
            auto_refresh: true,
            client: None,
            node_address: None,
            context: None,
            config_path: None,
            // Group view fields
            is_group_view: true,
            group_name,
            nodes,
            group_view_mode: GroupViewMode::default(),
            node_data: std::collections::HashMap::new(),
            selected_node_tab: 0,
        }
    }

    /// Add storage data from a node (for group view)
    pub fn add_node_storage(&mut self, hostname: String, disks: Vec<DiskInfo>, volumes: Vec<VolumeStatus>) {
        if !self.is_group_view {
            return;
        }

        // Store node data
        let node_storage = NodeStorageData {
            hostname: hostname.clone(),
            disks,
            volumes,
        };
        self.node_data.insert(hostname, node_storage);

        // Rebuild merged view
        self.rebuild_group_data();
    }

    /// Rebuild merged storage data for group view
    fn rebuild_group_data(&mut self) {
        if !self.is_group_view {
            return;
        }

        // Get or create data
        let mut data = self.state.take_data().unwrap_or_default();

        // Merge all disks and volumes (prefixed with hostname)
        data.disks.clear();
        data.volumes.clear();

        for node_data in self.node_data.values() {
            for disk in &node_data.disks {
                let mut prefixed_disk = disk.clone();
                prefixed_disk.dev_path = format!("{}:{}", node_data.hostname, disk.dev_path);
                data.disks.push(prefixed_disk);
            }
            for volume in &node_data.volumes {
                let mut prefixed_volume = volume.clone();
                prefixed_volume.id = format!("{}:{}", node_data.hostname, volume.id);
                data.volumes.push(prefixed_volume);
            }
        }

        self.state.set_data(data);
    }

    /// Set the client for API calls
    pub fn set_client(&mut self, client: TalosClient) {
        self.client = Some(client);
    }

    /// Set context and config path for talosctl commands (used for group view refresh)
    pub fn set_context(&mut self, context: Option<String>, config_path: Option<String>) {
        self.context = context;
        self.config_path = config_path;
    }

    /// Set error message
    pub fn set_error(&mut self, error: String) {
        self.state.set_error(error);
    }

    /// Helper to get data reference
    fn data(&self) -> Option<&StorageData> {
        self.state.data()
    }

    /// Refresh storage data
    pub async fn refresh(&mut self) -> Result<()> {
        // Handle group view refresh
        if self.is_group_view {
            return self.refresh_group().await;
        }

        self.state.start_loading();

        let Some(node) = &self.node_address else {
            self.state.set_error("No node address configured");
            return Ok(());
        };

        let Some(context) = &self.context else {
            self.state.set_error("No context configured");
            return Ok(());
        };

        // Get or create data
        let mut data = self.state.take_data().unwrap_or_default();

        // Fetch disk information using context-aware async function
        match get_disks_for_node(context, node, self.config_path.as_deref()).await {
            Ok(disks) => {
                data.disks = disks;
            }
            Err(e) => {
                tracing::warn!("Failed to fetch disks: {}", e);
                data.disks.clear();
            }
        }

        // Fetch volume status using context-aware async function
        match get_volume_status_for_node(context, node, self.config_path.as_deref()).await {
            Ok(volumes) => {
                data.volumes = volumes;
            }
            Err(e) => {
                tracing::warn!("Failed to fetch volumes: {}", e);
                data.volumes.clear();
            }
        }

        // Store the data
        self.state.set_data(data);
        Ok(())
    }

    /// Refresh storage data for group view (multiple nodes)
    async fn refresh_group(&mut self) -> Result<()> {
        let Some(context) = self.context.clone() else {
            self.state.set_error("No context configured");
            return Ok(());
        };

        self.state.start_loading();

        // Clear existing node data
        self.node_data.clear();

        // Clone nodes to avoid borrow issues
        let nodes = self.nodes.clone();
        let config_path = self.config_path.clone();

        // Fetch storage info from all nodes using talosctl
        for (hostname, ip) in &nodes {
            // Extract IP without port
            let node_ip = ip.split(':').next().unwrap_or(ip);

            // Fetch disks
            match get_disks_for_node(&context, node_ip, config_path.as_deref()).await {
                Ok(disks) => {
                    // Fetch volumes
                    match get_volume_status_for_node(&context, node_ip, config_path.as_deref()).await {
                        Ok(volumes) => {
                            self.add_node_storage(hostname.clone(), disks, volumes);
                        }
                        Err(e) => {
                            tracing::warn!("Failed to fetch volumes from {}: {}", hostname, e);
                            self.add_node_storage(hostname.clone(), disks, Vec::new());
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to fetch disks from {}: {}", hostname, e);
                }
            }
        }

        // Reset selection if needed
        self.disk_table_state.select(Some(0));
        self.volume_table_state.select(Some(0));

        self.state.mark_loaded();
        Ok(())
    }

    /// Get selected disk index
    fn selected_disk_index(&self) -> usize {
        self.disk_table_state.selected().unwrap_or(0)
    }

    /// Get selected volume index
    fn selected_volume_index(&self) -> usize {
        self.volume_table_state.selected().unwrap_or(0)
    }

    /// Move selection up
    fn select_prev(&mut self) {
        match self.view_mode {
            StorageViewMode::Disks => {
                if let Some(data) = self.data()
                    && !data.disks.is_empty()
                {
                    let i = self.selected_disk_index();
                    let new_i = if i == 0 { data.disks.len() - 1 } else { i - 1 };
                    self.disk_table_state.select(Some(new_i));
                }
            }
            StorageViewMode::Volumes => {
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

    /// Move selection down
    fn select_next(&mut self) {
        match self.view_mode {
            StorageViewMode::Disks => {
                if let Some(data) = self.data()
                    && !data.disks.is_empty()
                {
                    let i = self.selected_disk_index();
                    let new_i = (i + 1) % data.disks.len();
                    self.disk_table_state.select(Some(new_i));
                }
            }
            StorageViewMode::Volumes => {
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

    /// Get disks to display based on view mode (ByNode or Interleaved)
    fn get_display_disks(&self) -> Option<Vec<&DiskInfo>> {
        if self.is_group_view && self.group_view_mode == GroupViewMode::ByNode {
            // ByNode mode: show disks from selected node only
            let (hostname, _) = self.nodes.get(self.selected_node_tab)?;
            let node_data = self.node_data.get(hostname)?;
            Some(node_data.disks.iter().collect())
        } else {
            // Interleaved mode: use merged data
            self.data().map(|d| d.disks.iter().collect())
        }
    }

    /// Get volumes to display based on view mode (ByNode or Interleaved)
    fn get_display_volumes(&self) -> Option<Vec<&VolumeStatus>> {
        if self.is_group_view && self.group_view_mode == GroupViewMode::ByNode {
            // ByNode mode: show volumes from selected node only
            let (hostname, _) = self.nodes.get(self.selected_node_tab)?;
            let node_data = self.node_data.get(hostname)?;
            Some(node_data.volumes.iter().collect())
        } else {
            // Interleaved mode: use merged data
            self.data().map(|d| d.volumes.iter().collect())
        }
    }

    /// Draw the disks view
    fn draw_disks_view(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::vertical([
            Constraint::Min(5),    // Table
            Constraint::Length(5), // Detail section
        ])
        .split(area);

        // Draw disk table
        let header = Row::new(vec![
            Cell::from("DEVICE").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("SIZE").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("TYPE").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("TRANSPORT").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("MODEL").style(Style::default().add_modifier(Modifier::BOLD)),
        ])
        .height(1);

        let rows: Vec<Row> = if let Some(disks) = self.get_display_disks() {
            disks
                .iter()
                .map(|disk| {
                    let disk_type = if disk.cdrom {
                        "CD-ROM"
                    } else if disk.rotational {
                        "HDD"
                    } else {
                        "SSD"
                    };

                    let type_color = if disk.cdrom {
                        Color::Magenta
                    } else if disk.rotational {
                        Color::Yellow
                    } else {
                        Color::Green
                    };

                    Row::new(vec![
                        Cell::from(disk.dev_path.clone()),
                        Cell::from(disk.size_pretty.clone()),
                        Cell::from(disk_type).style(Style::default().fg(type_color)),
                        Cell::from(disk.transport.clone().unwrap_or_default()),
                        Cell::from(disk.model.clone().unwrap_or_default()),
                    ])
                })
                .collect()
        } else {
            vec![]
        };

        let widths = [
            Constraint::Length(14),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Min(20),
        ];

        let table = Table::new(rows, widths)
            .header(header)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Disks ")
                    .title_style(Style::default().fg(Color::Cyan)),
            )
            .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));

        frame.render_stateful_widget(table, chunks[0], &mut self.disk_table_state);

        // Draw detail section
        self.draw_disk_detail(frame, chunks[1]);
    }

    /// Draw disk detail section
    fn draw_disk_detail(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Details ")
            .title_style(Style::default().fg(Color::Yellow));

        let content = if let Some(disks) = self.get_display_disks() {
            if let Some(disk) = disks.get(self.selected_disk_index()) {
                let mut lines = vec![
                    Line::from(vec![
                        Span::styled("Device: ", Style::default().fg(Color::Gray)),
                        Span::raw(&disk.dev_path),
                        Span::raw("  "),
                        Span::styled("Size: ", Style::default().fg(Color::Gray)),
                        Span::raw(format!(
                            "{} ({})",
                            &disk.size_pretty,
                            format_bytes(disk.size)
                        )),
                    ]),
                    Line::from(vec![
                        Span::styled("Serial: ", Style::default().fg(Color::Gray)),
                        Span::raw(disk.serial.clone().unwrap_or_else(|| "N/A".to_string())),
                        Span::raw("  "),
                        Span::styled("WWID: ", Style::default().fg(Color::Gray)),
                        Span::raw(
                            disk.wwid
                                .clone()
                                .map(|w| {
                                    if w.len() > 30 {
                                        format!("{}...", &w[..30])
                                    } else {
                                        w
                                    }
                                })
                                .unwrap_or_else(|| "N/A".to_string()),
                        ),
                    ]),
                ];

                if disk.readonly {
                    lines.push(Line::from(vec![Span::styled(
                        "  [READ-ONLY]",
                        Style::default().fg(Color::Red),
                    )]));
                }

                lines
            } else {
                vec![Line::from("No disk selected")]
            }
        } else {
            vec![Line::from("Loading...")]
        };

        let paragraph = Paragraph::new(content).block(block);
        frame.render_widget(paragraph, area);
    }

    /// Draw the volumes view
    fn draw_volumes_view(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::vertical([
            Constraint::Min(5),    // Table
            Constraint::Length(5), // Detail section
        ])
        .split(area);

        // Draw volume table
        let header = Row::new(vec![
            Cell::from("VOLUME").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("SIZE").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("PHASE").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("FS").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("ENCRYPTION").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("MOUNT").style(Style::default().add_modifier(Modifier::BOLD)),
        ])
        .height(1);

        let rows: Vec<Row> = if let Some(volumes) = self.get_display_volumes() {
            volumes
                .iter()
                .map(|vol| {
                    let phase_color = match vol.phase.as_str() {
                        "ready" => Color::Green,
                        "waiting" => Color::Yellow,
                        _ => Color::Red,
                    };

                    let encryption = vol
                        .encryption_provider
                        .clone()
                        .unwrap_or_else(|| "none".to_string());
                    let encryption_color = if encryption == "none" {
                        Color::Yellow
                    } else {
                        Color::Green
                    };

                    Row::new(vec![
                        Cell::from(vol.id.clone()),
                        Cell::from(vol.size.clone()),
                        Cell::from(vol.phase.clone()).style(Style::default().fg(phase_color)),
                        Cell::from(vol.filesystem.clone().unwrap_or_default()),
                        Cell::from(encryption).style(Style::default().fg(encryption_color)),
                        Cell::from(vol.mount_location.clone().unwrap_or_default()),
                    ])
                })
                .collect()
        } else {
            vec![]
        };

        let widths = [
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(6),
            Constraint::Length(12),
            Constraint::Min(15),
        ];

        let table = Table::new(rows, widths)
            .header(header)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Volumes ")
                    .title_style(Style::default().fg(Color::Cyan)),
            )
            .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));

        frame.render_stateful_widget(table, chunks[0], &mut self.volume_table_state);

        // Draw detail section
        self.draw_volume_detail(frame, chunks[1]);
    }

    /// Draw volume detail section
    fn draw_volume_detail(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Details ")
            .title_style(Style::default().fg(Color::Yellow));

        let content = if let Some(volumes) = self.get_display_volumes() {
            if let Some(vol) = volumes.get(self.selected_volume_index()) {
                vec![
                    Line::from(vec![
                        Span::styled("Volume: ", Style::default().fg(Color::Gray)),
                        Span::raw(&vol.id),
                        Span::raw("  "),
                        Span::styled("Size: ", Style::default().fg(Color::Gray)),
                        Span::raw(&vol.size),
                    ]),
                    Line::from(vec![
                        Span::styled("Mount: ", Style::default().fg(Color::Gray)),
                        Span::raw(
                            vol.mount_location
                                .clone()
                                .unwrap_or_else(|| "N/A".to_string()),
                        ),
                        Span::raw("  "),
                        Span::styled("Filesystem: ", Style::default().fg(Color::Gray)),
                        Span::raw(vol.filesystem.clone().unwrap_or_else(|| "N/A".to_string())),
                    ]),
                    Line::from(vec![
                        Span::styled("Encryption: ", Style::default().fg(Color::Gray)),
                        Span::raw(
                            vol.encryption_provider
                                .clone()
                                .unwrap_or_else(|| "none".to_string()),
                        ),
                    ]),
                ]
            } else {
                vec![Line::from("No volume selected")]
            }
        } else {
            vec![Line::from("Loading...")]
        };

        let paragraph = Paragraph::new(content).block(block);
        frame.render_widget(paragraph, area);
    }

    /// Draw tab bar
    fn draw_tabs(&self, frame: &mut Frame, area: Rect) {
        let tabs = [StorageViewMode::Disks, StorageViewMode::Volumes];

        let tab_spans: Vec<Span> = tabs
            .iter()
            .map(|tab| {
                let style = if *tab == self.view_mode {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                Span::styled(format!(" [{}] ", tab.label()), style)
            })
            .collect();

        let mut line_spans = tab_spans;
        line_spans.push(Span::raw("  "));

        if self.is_group_view {
            // Group view header
            line_spans.push(Span::styled(&self.group_name, Style::default().fg(Color::Cyan)));
            line_spans.push(Span::styled(
                format!(" ({} nodes)", self.nodes.len()),
                Style::default().fg(Color::DarkGray),
            ));

            // View mode indicator
            let view_mode_label = match self.group_view_mode {
                GroupViewMode::Interleaved => "[MERGED]",
                GroupViewMode::ByNode => "[BY NODE]",
            };
            line_spans.push(Span::raw("  "));
            line_spans.push(Span::styled(
                view_mode_label,
                Style::default().fg(Color::Green),
            ));

            // Node tabs for ByNode mode
            if self.group_view_mode == GroupViewMode::ByNode && !self.nodes.is_empty() {
                line_spans.push(Span::raw("  "));
                for (i, (hostname, _)) in self.nodes.iter().enumerate() {
                    if i == self.selected_node_tab {
                        line_spans.push(Span::styled(
                            format!("[{}]", hostname),
                            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                        ));
                    } else {
                        line_spans.push(Span::styled(
                            format!(" {} ", hostname),
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                }
            }
        } else {
            // Single node header
            let hostname = self.data().map(|d| d.hostname.clone()).unwrap_or_default();
            line_spans.push(Span::styled(
                format!("Node: {}", hostname),
                Style::default().fg(Color::DarkGray),
            ));
        }

        let tabs_line = Line::from(line_spans);
        let paragraph = Paragraph::new(tabs_line);
        frame.render_widget(paragraph, area);
    }
}

impl Component for StorageComponent {
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                return Ok(Some(Action::Back));
            }
            KeyCode::Tab => {
                self.view_mode = self.view_mode.next();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.select_prev();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.select_next();
            }
            KeyCode::Char('r') => {
                return Ok(Some(Action::Refresh));
            }
            KeyCode::Char('v') => {
                // Toggle view mode (only in group view)
                if self.is_group_view {
                    self.group_view_mode = match self.group_view_mode {
                        GroupViewMode::Interleaved => GroupViewMode::ByNode,
                        GroupViewMode::ByNode => GroupViewMode::Interleaved,
                    };
                    // Reset selection when changing view mode
                    self.disk_table_state.select(Some(0));
                    self.volume_table_state.select(Some(0));
                }
            }
            KeyCode::Char('[') => {
                // Previous node tab (only in group view with ByNode mode)
                if self.is_group_view && self.group_view_mode == GroupViewMode::ByNode {
                    if self.selected_node_tab > 0 {
                        self.selected_node_tab -= 1;
                        // Reset selection when changing tabs
                        self.disk_table_state.select(Some(0));
                        self.volume_table_state.select(Some(0));
                    }
                }
            }
            KeyCode::Char(']') => {
                // Next node tab (only in group view with ByNode mode)
                if self.is_group_view && self.group_view_mode == GroupViewMode::ByNode {
                    if self.selected_node_tab + 1 < self.nodes.len() {
                        self.selected_node_tab += 1;
                        // Reset selection when changing tabs
                        self.disk_table_state.select(Some(0));
                        self.volume_table_state.select(Some(0));
                    }
                }
            }
            _ => {}
        }
        Ok(None)
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
        // Check loading state
        if self.state.is_loading() && !self.state.has_data() {
            let loading = Paragraph::new("Loading storage info...")
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

        let chunks = Layout::vertical([
            Constraint::Length(1), // Tabs
            Constraint::Min(0),    // Content
            Constraint::Length(1), // Help
        ])
        .split(area);

        // Draw tabs
        self.draw_tabs(frame, chunks[0]);

        // Draw content based on view mode
        match self.view_mode {
            StorageViewMode::Disks => self.draw_disks_view(frame, chunks[1]),
            StorageViewMode::Volumes => self.draw_volumes_view(frame, chunks[1]),
        }

        // Draw help line
        let mut help_spans = vec![];

        // Add view mode toggle hint if in group view
        if self.is_group_view {
            help_spans.push(Span::styled("v", Style::default().fg(Color::Cyan)));
            help_spans.push(Span::raw(" view  "));

            // Add tab navigation hint if in ByNode mode
            if self.group_view_mode == GroupViewMode::ByNode {
                help_spans.push(Span::styled("[", Style::default().fg(Color::Cyan)));
                help_spans.push(Span::styled("/", Style::default().fg(Color::DarkGray)));
                help_spans.push(Span::styled("]", Style::default().fg(Color::Cyan)));
                help_spans.push(Span::raw(" tabs  "));
            }
        }

        help_spans.extend(vec![
            Span::styled(" Tab", Style::default().fg(Color::Cyan)),
            Span::raw(" switch view  "),
            Span::styled("↑/↓", Style::default().fg(Color::Cyan)),
            Span::raw(" navigate  "),
            Span::styled("r", Style::default().fg(Color::Cyan)),
            Span::raw(" refresh  "),
            Span::styled("Esc", Style::default().fg(Color::Cyan)),
            Span::raw(" back"),
        ]);

        let help = Line::from(help_spans);
        let help_paragraph = Paragraph::new(help).style(Style::default().fg(Color::DarkGray));
        frame.render_widget(help_paragraph, chunks[2]);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a test DiskInfo
    fn make_disk(id: &str) -> DiskInfo {
        DiskInfo {
            id: id.to_string(),
            dev_path: format!("/dev/{}", id),
            size: 500_000_000_000,
            size_pretty: "500 GB".to_string(),
            model: Some("Test Disk".to_string()),
            serial: Some("ABC123".to_string()),
            transport: Some("sata".to_string()),
            rotational: false,
            readonly: false,
            cdrom: false,
            wwid: None,
            bus_path: None,
        }
    }

    /// Create a test VolumeStatus
    fn make_volume(id: &str) -> VolumeStatus {
        VolumeStatus {
            id: id.to_string(),
            encryption_provider: None,
            phase: "ready".to_string(),
            size: "10 GB".to_string(),
            filesystem: Some("xfs".to_string()),
            mount_location: Some(format!("/var/{}", id)),
        }
    }

    /// Create a StorageComponent for single node view
    fn create_single_node_component() -> StorageComponent {
        let mut component = StorageComponent::new(
            "test-node".to_string(),
            "10.0.0.1".to_string(),
            None,
            None,
        );

        // Set up data
        let mut data = StorageData::default();
        data.disks = vec![make_disk("sda"), make_disk("sdb")];
        data.volumes = vec![make_volume("STATE"), make_volume("EPHEMERAL")];

        component.state.set_data(data);
        component
    }

    /// Create a StorageComponent for group view
    fn create_group_component() -> StorageComponent {
        let nodes = vec![
            ("node-1".to_string(), "10.0.0.1".to_string()),
            ("node-2".to_string(), "10.0.0.2".to_string()),
        ];
        let mut component = StorageComponent::new_group("Control Plane".to_string(), nodes);

        // Add node data
        component.add_node_storage(
            "node-1".to_string(),
            vec![make_disk("sda"), make_disk("sdb")],
            vec![make_volume("STATE")],
        );
        component.add_node_storage(
            "node-2".to_string(),
            vec![make_disk("nvme0n1")],
            vec![make_volume("STATE"), make_volume("EPHEMERAL")],
        );

        component
    }

    // ==========================================================================
    // Tests for get_display_disks()
    // ==========================================================================

    #[test]
    fn test_get_display_disks_single_node_returns_all_disks() {
        let component = create_single_node_component();

        let result = component.get_display_disks();
        assert!(result.is_some());

        let disks = result.unwrap();
        assert_eq!(disks.len(), 2);
        assert!(disks.iter().any(|d| d.id == "sda"));
        assert!(disks.iter().any(|d| d.id == "sdb"));
    }

    #[test]
    fn test_get_display_disks_group_interleaved_returns_merged_data() {
        let mut component = create_group_component();
        component.group_view_mode = GroupViewMode::Interleaved;

        let result = component.get_display_disks();
        assert!(result.is_some());

        let disks = result.unwrap();
        // Should have merged disks from both nodes (3 total)
        assert_eq!(disks.len(), 3);
        // Dev paths should be prefixed with hostname
        assert!(disks
            .iter()
            .any(|d| d.dev_path.contains("node-1:") || d.dev_path.contains("node-2:")));
    }

    #[test]
    fn test_get_display_disks_group_bynode_returns_selected_node_data() {
        let mut component = create_group_component();
        component.group_view_mode = GroupViewMode::ByNode;
        component.selected_node_tab = 0; // Select first node

        let result = component.get_display_disks();
        assert!(result.is_some());

        let disks = result.unwrap();
        // Should only have disks from node-1
        assert_eq!(disks.len(), 2);
        assert!(disks.iter().any(|d| d.id == "sda"));
        assert!(disks.iter().any(|d| d.id == "sdb"));
        assert!(!disks.iter().any(|d| d.id == "nvme0n1"));
    }

    #[test]
    fn test_get_display_disks_group_bynode_second_node() {
        let mut component = create_group_component();
        component.group_view_mode = GroupViewMode::ByNode;
        component.selected_node_tab = 1; // Select second node

        let result = component.get_display_disks();
        assert!(result.is_some());

        let disks = result.unwrap();
        // Should only have disks from node-2
        assert_eq!(disks.len(), 1);
        assert!(disks.iter().any(|d| d.id == "nvme0n1"));
        assert!(!disks.iter().any(|d| d.id == "sda"));
    }

    #[test]
    fn test_get_display_disks_returns_none_when_tab_out_of_bounds() {
        let mut component = create_group_component();
        component.group_view_mode = GroupViewMode::ByNode;
        component.selected_node_tab = 99; // Invalid tab index

        let result = component.get_display_disks();
        assert!(result.is_none(), "Should return None for invalid tab index");
    }

    // ==========================================================================
    // Tests for get_display_volumes()
    // ==========================================================================

    #[test]
    fn test_get_display_volumes_single_node_returns_all_volumes() {
        let component = create_single_node_component();

        let result = component.get_display_volumes();
        assert!(result.is_some());

        let volumes = result.unwrap();
        assert_eq!(volumes.len(), 2);
        assert!(volumes.iter().any(|v| v.id == "STATE"));
        assert!(volumes.iter().any(|v| v.id == "EPHEMERAL"));
    }

    #[test]
    fn test_get_display_volumes_group_interleaved_returns_merged_data() {
        let mut component = create_group_component();
        component.group_view_mode = GroupViewMode::Interleaved;

        let result = component.get_display_volumes();
        assert!(result.is_some());

        let volumes = result.unwrap();
        // Should have merged volumes from both nodes (3 total)
        assert_eq!(volumes.len(), 3);
        // IDs should be prefixed with hostname
        assert!(volumes
            .iter()
            .any(|v| v.id.contains("node-1:") || v.id.contains("node-2:")));
    }

    #[test]
    fn test_get_display_volumes_group_bynode_returns_selected_node_data() {
        let mut component = create_group_component();
        component.group_view_mode = GroupViewMode::ByNode;
        component.selected_node_tab = 0; // Select first node

        let result = component.get_display_volumes();
        assert!(result.is_some());

        let volumes = result.unwrap();
        // Should only have volumes from node-1 (1 volume: STATE)
        assert_eq!(volumes.len(), 1);
        assert!(volumes.iter().any(|v| v.id == "STATE"));
    }

    #[test]
    fn test_get_display_volumes_group_bynode_second_node() {
        let mut component = create_group_component();
        component.group_view_mode = GroupViewMode::ByNode;
        component.selected_node_tab = 1; // Select second node

        let result = component.get_display_volumes();
        assert!(result.is_some());

        let volumes = result.unwrap();
        // Should only have volumes from node-2 (2 volumes: STATE, EPHEMERAL)
        assert_eq!(volumes.len(), 2);
        assert!(volumes.iter().any(|v| v.id == "STATE"));
        assert!(volumes.iter().any(|v| v.id == "EPHEMERAL"));
    }

    #[test]
    fn test_get_display_volumes_returns_none_when_tab_out_of_bounds() {
        let mut component = create_group_component();
        component.group_view_mode = GroupViewMode::ByNode;
        component.selected_node_tab = 99; // Invalid tab index

        let result = component.get_display_volumes();
        assert!(result.is_none(), "Should return None for invalid tab index");
    }

    #[test]
    fn test_get_display_volumes_group_bynode_with_no_data_for_node() {
        let nodes = vec![
            ("node-1".to_string(), "10.0.0.1".to_string()),
            ("node-2".to_string(), "10.0.0.2".to_string()),
        ];
        let mut component = StorageComponent::new_group("Control Plane".to_string(), nodes);
        component.group_view_mode = GroupViewMode::ByNode;

        // Only add data for node-1
        component.add_node_storage(
            "node-1".to_string(),
            vec![make_disk("sda")],
            vec![make_volume("STATE")],
        );

        // Select node-2 which has no data
        component.selected_node_tab = 1;

        let result = component.get_display_volumes();
        assert!(
            result.is_none(),
            "Should return None when selected node has no data"
        );

        let disk_result = component.get_display_disks();
        assert!(
            disk_result.is_none(),
            "Should return None for disks when selected node has no data"
        );
    }
}
