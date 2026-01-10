//! Cluster component - displays cluster overview with nodes

use crate::action::Action;
use crate::components::Component;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Row, Table},
    Frame,
};
use std::collections::HashMap;
use talos_rs::{
    EtcdMemberInfo, MemInfo, NodeCpuInfo, NodeLoadAvg, NodeMemory, NodeServices, ServiceInfo,
    TalosClient, VersionInfo,
};

/// Simple etcd status for header display
#[derive(Debug, Clone, Default)]
struct EtcdSummary {
    /// Number of healthy members
    healthy: usize,
    /// Total number of members
    total: usize,
    /// Whether etcd has quorum
    has_quorum: bool,
}

/// Cluster component showing overview with node list
pub struct ClusterComponent {
    /// Talos client for API calls
    client: Option<TalosClient>,
    /// Context name to use (None = use default context)
    context: Option<String>,
    /// Connection state
    state: ConnectionState,
    /// Version info from nodes
    versions: Vec<VersionInfo>,
    /// Services from nodes
    services: Vec<NodeServices>,
    /// Memory info from nodes
    memory: Vec<NodeMemory>,
    /// Load average from nodes
    load_avg: Vec<NodeLoadAvg>,
    /// CPU info from nodes
    cpu_info: Vec<NodeCpuInfo>,
    /// Etcd members (for extracting node IPs)
    etcd_members: Vec<EtcdMemberInfo>,
    /// Etcd summary for header
    etcd_summary: Option<EtcdSummary>,
    /// Node hostname to IP mapping (discovered from etcd members)
    node_ips: HashMap<String, String>,
    /// Currently selected node index
    selected: usize,
    /// Currently selected service index within the node
    selected_service: usize,
    /// List state for selection
    list_state: ListState,
    /// Error message if any
    error: Option<String>,
    /// Last refresh time
    last_refresh: Option<std::time::Instant>,
}

#[derive(Debug, Clone, PartialEq)]
enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

impl Default for ClusterComponent {
    fn default() -> Self {
        Self::new(None)
    }
}

impl ClusterComponent {
    pub fn new(context: Option<String>) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));

        Self {
            client: None,
            context,
            state: ConnectionState::Disconnected,
            versions: Vec::new(),
            services: Vec::new(),
            memory: Vec::new(),
            load_avg: Vec::new(),
            cpu_info: Vec::new(),
            etcd_members: Vec::new(),
            etcd_summary: None,
            node_ips: HashMap::new(),
            selected: 0,
            selected_service: 0,
            list_state,
            error: None,
            last_refresh: None,
        }
    }

    /// Initialize connection to Talos cluster
    pub async fn connect(&mut self) -> Result<()> {
        self.state = ConnectionState::Connecting;

        // Install crypto provider (needed for rustls)
        let _ = rustls::crypto::ring::default_provider().install_default();

        // Use named context if specified, otherwise default
        let client_result = match &self.context {
            Some(ctx_name) => TalosClient::from_named_context(ctx_name).await,
            None => TalosClient::from_default_config().await,
        };

        match client_result {
            Ok(client) => {
                self.client = Some(client);
                self.state = ConnectionState::Connected;
                self.refresh().await?;
            }
            Err(e) => {
                self.state = ConnectionState::Error(e.to_string());
                self.error = Some(e.to_string());
            }
        }

        Ok(())
    }

    /// Refresh cluster data from API
    pub async fn refresh(&mut self) -> Result<()> {
        if let Some(client) = &self.client {
            // First, fetch etcd members to discover node IPs
            // This is critical for Docker provisioner where metadata.hostname is empty
            match client.etcd_members().await {
                Ok(members) => {
                    // Build hostname -> IP mapping (only hostname keys, not IP->IP)
                    self.node_ips.clear();
                    for member in &members {
                        if let Some(ip) = member.ip_address() {
                            // Map hostname to IP
                            self.node_ips.insert(member.hostname.clone(), ip);
                        }
                    }
                    self.etcd_members = members;
                }
                Err(e) => {
                    tracing::warn!("Failed to fetch etcd members: {}", e);
                }
            }

            // If we have discovered node IPs, use them to get proper data with hostnames
            // Otherwise fall back to the default (which may have empty hostnames)
            if !self.etcd_members.is_empty() {
                // Get all data from each node using discovered IPs
                let mut versions = Vec::new();
                let mut services = Vec::new();
                let mut memory = Vec::new();
                let mut load_avg = Vec::new();
                let mut cpu_info = Vec::new();

                for member in &self.etcd_members {
                    if let Some(ip) = member.ip_address() {
                        let node_client = client.with_node(&ip);
                        let node_name = if !member.hostname.is_empty() {
                            member.hostname.clone()
                        } else {
                            ip.clone()
                        };

                        // Version
                        if let Ok(mut node_versions) = node_client.version().await {
                            for v in &mut node_versions {
                                v.node = node_name.clone();
                            }
                            versions.extend(node_versions);
                        }

                        // Services
                        if let Ok(mut node_services) = node_client.services().await {
                            for s in &mut node_services {
                                s.node = node_name.clone();
                            }
                            services.extend(node_services);
                        }

                        // Memory
                        if let Ok(mut node_memory) = node_client.memory().await {
                            for m in &mut node_memory {
                                m.node = node_name.clone();
                            }
                            memory.extend(node_memory);
                        }

                        // Load average
                        if let Ok(mut node_load) = node_client.load_avg().await {
                            for l in &mut node_load {
                                l.node = node_name.clone();
                            }
                            load_avg.extend(node_load);
                        }

                        // CPU info
                        if let Ok(mut node_cpu) = node_client.cpu_info().await {
                            for c in &mut node_cpu {
                                c.node = node_name.clone();
                            }
                            cpu_info.extend(node_cpu);
                        }
                    }
                }

                if !versions.is_empty() {
                    self.versions = versions;
                }
                if !services.is_empty() {
                    self.services = services;
                }
                if !memory.is_empty() {
                    self.memory = memory;
                }
                if !load_avg.is_empty() {
                    self.load_avg = load_avg;
                }
                if !cpu_info.is_empty() {
                    self.cpu_info = cpu_info;
                }
            } else {
                // Fallback: fetch without targeting (may have empty hostnames)
                match client.version().await {
                    Ok(versions) => self.versions = versions,
                    Err(e) => self.error = Some(format!("Version error: {}", e)),
                }

                match client.services().await {
                    Ok(services) => self.services = services,
                    Err(e) => self.error = Some(format!("Services error: {}", e)),
                }

                match client.memory().await {
                    Ok(memory) => self.memory = memory,
                    Err(e) => self.error = Some(format!("Memory error: {}", e)),
                }

                match client.load_avg().await {
                    Ok(load_avg) => self.load_avg = load_avg,
                    Err(e) => self.error = Some(format!("LoadAvg error: {}", e)),
                }

                match client.cpu_info().await {
                    Ok(cpu_info) => self.cpu_info = cpu_info,
                    Err(e) => self.error = Some(format!("CPUInfo error: {}", e)),
                }
            }

            // Fetch etcd status for header summary
            match client.etcd_status().await {
                Ok(statuses) => {
                    let total = self.etcd_members.len();
                    let healthy = statuses.len();
                    let quorum_needed = total / 2 + 1;
                    self.etcd_summary = Some(EtcdSummary {
                        healthy,
                        total,
                        has_quorum: healthy >= quorum_needed,
                    });
                }
                Err(_) => {
                    // Don't overwrite existing summary on error
                }
            }

            self.last_refresh = Some(std::time::Instant::now());
        }

        Ok(())
    }

    /// Move selection up
    fn select_previous(&mut self) {
        if !self.versions.is_empty() {
            self.selected = self.selected.saturating_sub(1);
            self.list_state.select(Some(self.selected));
        }
    }

    /// Move selection down
    fn select_next(&mut self) {
        if !self.versions.is_empty() {
            self.selected = (self.selected + 1).min(self.versions.len() - 1);
            self.list_state.select(Some(self.selected));
        }
    }

    /// Get services for a node
    fn get_node_services(&self, node_name: &str) -> Option<&Vec<ServiceInfo>> {
        self.services
            .iter()
            .find(|s| s.node == node_name || (s.node.is_empty() && node_name.is_empty()))
            .map(|s| &s.services)
    }

    /// Get memory for a node
    fn get_node_memory(&self, node_name: &str) -> Option<&MemInfo> {
        self.memory
            .iter()
            .find(|m| m.node == node_name || (m.node.is_empty() && node_name.is_empty()))
            .and_then(|m| m.meminfo.as_ref())
    }

    /// Get load average for a node
    fn get_node_load_avg(&self, node_name: &str) -> Option<&NodeLoadAvg> {
        self.load_avg
            .iter()
            .find(|l| l.node == node_name || (l.node.is_empty() && node_name.is_empty()))
    }

    /// Get CPU info for a node
    fn get_node_cpu_info(&self, node_name: &str) -> Option<&NodeCpuInfo> {
        self.cpu_info
            .iter()
            .find(|c| c.node == node_name || (c.node.is_empty() && node_name.is_empty()))
    }

    /// Get the currently selected service ID
    pub fn selected_service_id(&self) -> Option<String> {
        if self.versions.is_empty() {
            return None;
        }
        let node_name = &self.versions[self.selected].node;
        self.get_node_services(node_name)
            .and_then(|services| services.get(self.selected_service))
            .map(|s| s.id.clone())
    }

    /// Get a reference to the client for async operations
    pub fn client(&self) -> Option<&TalosClient> {
        self.client.as_ref()
    }

    /// Get service count for current node
    fn current_service_count(&self) -> usize {
        if self.versions.is_empty() {
            return 0;
        }
        let node_name = &self.versions[self.selected].node;
        self.get_node_services(node_name)
            .map(|s| s.len())
            .unwrap_or(0)
    }

    /// Get all service IDs for current node
    fn current_service_ids(&self) -> Vec<String> {
        if self.versions.is_empty() {
            return Vec::new();
        }
        let node_name = &self.versions[self.selected].node;
        self.get_node_services(node_name)
            .map(|services| services.iter().map(|s| s.id.clone()).collect())
            .unwrap_or_default()
    }

    /// Get current node IP/name
    fn current_node_name(&self) -> Option<String> {
        self.versions.get(self.selected).map(|v| v.node.clone())
    }

    /// Determine node role based on services (etcd = controlplane)
    fn current_node_role(&self) -> String {
        let service_ids = self.current_service_ids();
        if service_ids.iter().any(|s| s == "etcd") {
            "controlplane".to_string()
        } else {
            "worker".to_string()
        }
    }
}

impl Component for ClusterComponent {
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Ok(Some(Action::Quit)),
            KeyCode::Char('r') => Ok(Some(Action::Refresh)),
            KeyCode::Up | KeyCode::Char('k') => {
                self.select_previous();
                self.selected_service = 0; // Reset service selection when changing nodes
                Ok(None)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.select_next();
                self.selected_service = 0; // Reset service selection when changing nodes
                Ok(None)
            }
            KeyCode::Tab => {
                // Cycle through services
                let count = self.current_service_count();
                if count > 0 {
                    self.selected_service = (self.selected_service + 1) % count;
                }
                Ok(None)
            }
            KeyCode::BackTab => {
                // Cycle through services backwards
                let count = self.current_service_count();
                if count > 0 {
                    self.selected_service = if self.selected_service == 0 {
                        count - 1
                    } else {
                        self.selected_service - 1
                    };
                }
                Ok(None)
            }
            KeyCode::Enter | KeyCode::Char('l') => {
                // View multi-service logs for current node
                if let Some(node_name) = self.current_node_name() {
                    let service_ids = self.current_service_ids();
                    if !service_ids.is_empty() {
                        let node_role = self.current_node_role();
                        Ok(Some(Action::ShowMultiLogs(node_name, node_role, service_ids)))
                    } else {
                        Ok(None)
                    }
                } else {
                    Ok(None)
                }
            }
            KeyCode::Char('e') => {
                // View etcd cluster status
                Ok(Some(Action::ShowEtcd))
            }
            KeyCode::Char('p') => {
                // View processes for current node
                if let Some(node_name) = self.current_node_name() {
                    // Look up the IP address for this node
                    let node_ip = self.node_ips.get(&node_name).cloned().unwrap_or(node_name.clone());
                    tracing::info!("Cluster: pressing p, node_name='{}', ip='{}'", node_name, node_ip);
                    Ok(Some(Action::ShowProcesses(node_name, node_ip)))
                } else {
                    tracing::warn!("Cluster: pressing p, but no node selected");
                    Ok(None)
                }
            }
            KeyCode::Char('n') => {
                // View network stats for current node
                if let Some(node_name) = self.current_node_name() {
                    // Look up the IP address for this node
                    let node_ip = self.node_ips.get(&node_name).cloned().unwrap_or(node_name.clone());
                    tracing::info!("Cluster: pressing n, node_name='{}', ip='{}'", node_name, node_ip);
                    Ok(Some(Action::ShowNetwork(node_name, node_ip)))
                } else {
                    tracing::warn!("Cluster: pressing n, but no node selected");
                    Ok(None)
                }
            }
            KeyCode::Char('d') => {
                // View diagnostics for current node
                if let Some(node_name) = self.current_node_name() {
                    let node_ip = self.node_ips.get(&node_name).cloned().unwrap_or(node_name.clone());
                    let node_role = self.current_node_role();
                    tracing::info!("Cluster: pressing d, node_name='{}', ip='{}', role='{}'", node_name, node_ip, node_role);
                    Ok(Some(Action::ShowDiagnostics(node_name, node_ip, node_role)))
                } else {
                    tracing::warn!("Cluster: pressing d, but no node selected");
                    Ok(None)
                }
            }
            KeyCode::Char('c') => {
                // View security/certificates
                tracing::info!("Cluster: pressing c, showing security view");
                Ok(Some(Action::ShowSecurity))
            }
            KeyCode::Char('L') => {
                // View lifecycle/versions
                tracing::info!("Cluster: pressing L, showing lifecycle view");
                Ok(Some(Action::ShowLifecycle))
            }
            _ => Ok(None),
        }
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if let Action::Tick = action {
            // Could trigger auto-refresh here
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        let layout = Layout::vertical([
            Constraint::Length(3), // Header
            Constraint::Min(0),    // Content
            Constraint::Length(3), // Footer
        ])
        .split(area);

        // Header
        let status_indicator = match &self.state {
            ConnectionState::Connected => Span::raw(" ● ").fg(Color::Green),
            ConnectionState::Connecting => Span::raw(" ◐ ").fg(Color::Yellow),
            ConnectionState::Disconnected => Span::raw(" ○ ").fg(Color::DarkGray),
            ConnectionState::Error(_) => Span::raw(" ✗ ").fg(Color::Red),
        };

        // Build etcd status indicator for header
        let etcd_spans = if let Some(etcd) = &self.etcd_summary {
            let (indicator, color) = if etcd.has_quorum && etcd.healthy == etcd.total {
                ("●", Color::Green)
            } else if etcd.has_quorum {
                ("◐", Color::Yellow)
            } else {
                ("✗", Color::Red)
            };
            vec![
                Span::raw("    etcd ").dim(),
                Span::styled(indicator, Style::default().fg(color)),
                Span::raw(format!(" {}/{}", etcd.healthy, etcd.total)).dim(),
            ]
        } else {
            vec![]
        };

        let mut header_spans = vec![
            Span::raw(" talos-pilot ").bold().fg(Color::Cyan),
            status_indicator,
            Span::raw(match &self.state {
                ConnectionState::Connected => "Connected",
                ConnectionState::Connecting => "Connecting...",
                ConnectionState::Disconnected => "Disconnected",
                ConnectionState::Error(e) => e.as_str(),
            })
            .dim(),
        ];
        header_spans.extend(etcd_spans);

        let header = Paragraph::new(Line::from(header_spans))
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        frame.render_widget(header, layout[0]);

        // Content area - split into node list and details
        let content_layout = Layout::horizontal([
            Constraint::Percentage(40), // Node list
            Constraint::Percentage(60), // Details
        ])
        .split(layout[1]);

        // Node list
        self.draw_node_list(frame, content_layout[0]);

        // Node details
        self.draw_node_details(frame, content_layout[1]);

        // Footer
        let footer = Paragraph::new(Line::from(vec![
            Span::raw(" [q]").fg(Color::Yellow),
            Span::raw(" quit").dim(),
            Span::raw("  "),
            Span::raw("[r]").fg(Color::Yellow),
            Span::raw(" refresh").dim(),
            Span::raw("  "),
            Span::raw("[↑↓]").fg(Color::Yellow),
            Span::raw(" nodes").dim(),
            Span::raw("  "),
            Span::raw("[Tab]").fg(Color::Yellow),
            Span::raw(" services").dim(),
            Span::raw("  "),
            Span::raw("[Enter]").fg(Color::Yellow),
            Span::raw(" logs").dim(),
            Span::raw("  "),
            Span::raw("[e]").fg(Color::Yellow),
            Span::raw(" etcd").dim(),
            Span::raw("  "),
            Span::raw("[p]").fg(Color::Yellow),
            Span::raw(" procs").dim(),
            Span::raw("  "),
            Span::raw("[n]").fg(Color::Yellow),
            Span::raw(" net").dim(),
            Span::raw("  "),
            Span::raw("[d]").fg(Color::Yellow),
            Span::raw(" diag").dim(),
            Span::raw("  "),
            Span::raw("[c]").fg(Color::Yellow),
            Span::raw(" certs").dim(),
            Span::raw("  "),
            Span::raw("[L]").fg(Color::Yellow),
            Span::raw(" lifecycle").dim(),
        ]))
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        frame.render_widget(footer, layout[2]);

        Ok(())
    }
}

impl ClusterComponent {
    fn draw_node_list(&mut self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .versions
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let node_name = if v.node.is_empty() {
                    "node-0".to_string()
                } else {
                    v.node.clone()
                };

                // Get the IP for this node (if different from hostname)
                let node_ip = self.node_ips.get(&node_name).cloned();
                let display_name = if let Some(ref ip) = node_ip {
                    if ip != &node_name {
                        format!("{} ({})", node_name, ip)
                    } else {
                        node_name.clone()
                    }
                } else {
                    node_name.clone()
                };

                // Get health status from services
                let health_symbol = self
                    .get_node_services(&v.node)
                    .map(|services| {
                        let unhealthy = services
                            .iter()
                            .filter(|s| s.health.as_ref().map(|h| !h.healthy).unwrap_or(false))
                            .count();
                        if unhealthy > 0 {
                            "◐"
                        } else {
                            "●"
                        }
                    })
                    .unwrap_or("?");

                let health_color = match health_symbol {
                    "●" => Color::Green,
                    "◐" => Color::Yellow,
                    _ => Color::DarkGray,
                };

                let style = if i == self.selected {
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };

                ListItem::new(Line::from(vec![
                    Span::raw(format!(" {} ", health_symbol)).fg(health_color),
                    Span::raw(display_name).style(style),
                ]))
            })
            .collect();

        // If no nodes, show placeholder
        let items = if items.is_empty() {
            vec![ListItem::new(Line::from(
                Span::raw("  No nodes connected").dim(),
            ))]
        } else {
            items
        };

        let list = List::new(items)
            .block(
                Block::default()
                    .title(" Nodes ")
                    .title_style(Style::default().fg(Color::Cyan).bold())
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
            .highlight_style(Style::default().bg(Color::DarkGray));

        frame.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn draw_node_details(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .title(" Details ")
            .title_style(Style::default().fg(Color::Cyan).bold())
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if self.versions.is_empty() {
            let msg = Paragraph::new(Line::from(Span::raw("No node selected").dim()));
            frame.render_widget(msg, inner);
            return;
        }

        let version = &self.versions[self.selected];
        let node_name = &version.node;

        // Build detail layout
        let detail_layout = Layout::vertical([
            Constraint::Length(5), // Version info
            Constraint::Length(6), // Resource usage (memory, load, cpu)
            Constraint::Min(0),    // Services
        ])
        .split(inner);

        // Version info section
        let version_info = vec![
            Line::from(vec![
                Span::raw(" Version:  ").dim(),
                Span::raw(&version.version).fg(Color::White),
            ]),
            Line::from(vec![
                Span::raw(" SHA:      ").dim(),
                Span::raw(&version.sha).fg(Color::DarkGray),
            ]),
            Line::from(vec![
                Span::raw(" OS/Arch:  ").dim(),
                Span::raw(format!("{}/{}", version.os, version.arch)).fg(Color::White),
            ]),
        ];
        frame.render_widget(Paragraph::new(version_info), detail_layout[0]);

        // Resource usage section
        let mut resource_lines = Vec::new();

        // Memory
        if let Some(mem) = self.get_node_memory(node_name) {
            let usage_pct = mem.usage_percent();
            let usage_color = if usage_pct > 90.0 {
                Color::Red
            } else if usage_pct > 70.0 {
                Color::Yellow
            } else {
                Color::Green
            };

            resource_lines.push(Line::from(vec![
                Span::raw(" Memory:   ").dim(),
                Span::raw(format!("{:.1}%", usage_pct)).fg(usage_color),
                Span::raw(format!(
                    " ({} MB / {} MB)",
                    mem.mem_available / 1024 / 1024,
                    mem.mem_total / 1024 / 1024
                ))
                .dim(),
            ]));
        }

        // Load average
        if let Some(load) = self.get_node_load_avg(node_name) {
            let load_color = if load.load1 > 4.0 {
                Color::Red
            } else if load.load1 > 2.0 {
                Color::Yellow
            } else {
                Color::Green
            };

            resource_lines.push(Line::from(vec![
                Span::raw(" Load:     ").dim(),
                Span::raw(format!("{:.2}", load.load1)).fg(load_color),
                Span::raw(format!(" {:.2} {:.2}", load.load5, load.load15)).dim(),
                Span::raw(" (1/5/15m)").fg(Color::DarkGray),
            ]));
        }

        // CPU info
        if let Some(cpu) = self.get_node_cpu_info(node_name) {
            resource_lines.push(Line::from(vec![
                Span::raw(" CPU:      ").dim(),
                Span::raw(format!("{} cores", cpu.cpu_count)).fg(Color::White),
                Span::raw(format!(" @ {:.0} MHz", cpu.mhz)).dim(),
            ]));
            // Truncate model name if too long
            let model = if cpu.model_name.len() > 35 {
                format!("{}...", &cpu.model_name[..32])
            } else {
                cpu.model_name.clone()
            };
            resource_lines.push(Line::from(vec![
                Span::raw("           ").dim(),
                Span::raw(model).fg(Color::DarkGray),
            ]));
        }

        frame.render_widget(Paragraph::new(resource_lines), detail_layout[1]);

        // Services list
        if let Some(services) = self.get_node_services(node_name) {
            let service_rows: Vec<Row> = services
                .iter()
                .enumerate()
                .map(|(i, svc)| {
                    let health_symbol = svc
                        .health
                        .as_ref()
                        .map(|h| if h.healthy { "●" } else { "○" })
                        .unwrap_or("?");
                    let health_color = match health_symbol {
                        "●" => Color::Green,
                        "○" => Color::Red,
                        _ => Color::DarkGray,
                    };

                    let is_selected = i == self.selected_service;
                    let row_style = if is_selected {
                        Style::default().bg(Color::DarkGray)
                    } else {
                        Style::default()
                    };

                    Row::new(vec![
                        Span::raw(format!(" {} ", health_symbol)).fg(health_color),
                        Span::raw(&svc.id).fg(Color::White),
                        Span::raw(&svc.state).dim(),
                    ])
                    .style(row_style)
                })
                .collect();

            let services_table = Table::new(
                service_rows,
                [
                    Constraint::Length(3),
                    Constraint::Min(20),
                    Constraint::Length(12),
                ],
            )
            .header(
                Row::new(vec![
                    Span::raw("").dim(),
                    Span::raw("Service [Tab]").dim(),
                    Span::raw("State").dim(),
                ])
                .style(Style::default().add_modifier(Modifier::UNDERLINED)),
            );

            frame.render_widget(services_table, detail_layout[2]);
        }
    }
}
