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
use talos_rs::{MemInfo, NodeMemory, NodeServices, ServiceInfo, TalosClient, VersionInfo};

/// Cluster component showing overview with node list
pub struct ClusterComponent {
    /// Talos client for API calls
    client: Option<TalosClient>,
    /// Connection state
    state: ConnectionState,
    /// Version info from nodes
    versions: Vec<VersionInfo>,
    /// Services from nodes
    services: Vec<NodeServices>,
    /// Memory info from nodes
    memory: Vec<NodeMemory>,
    /// Currently selected node index
    selected: usize,
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
        Self::new()
    }
}

impl ClusterComponent {
    pub fn new() -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));

        Self {
            client: None,
            state: ConnectionState::Disconnected,
            versions: Vec::new(),
            services: Vec::new(),
            memory: Vec::new(),
            selected: 0,
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

        match TalosClient::from_default_config().await {
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
            // Fetch all data
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
}

impl Component for ClusterComponent {
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Ok(Some(Action::Quit)),
            KeyCode::Char('r') => Ok(Some(Action::Refresh)),
            KeyCode::Up | KeyCode::Char('k') => {
                self.select_previous();
                Ok(None)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.select_next();
                Ok(None)
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

        let header = Paragraph::new(Line::from(vec![
            Span::raw(" talos-pilot ").bold().fg(Color::Cyan),
            status_indicator,
            Span::raw(match &self.state {
                ConnectionState::Connected => "Connected",
                ConnectionState::Connecting => "Connecting...",
                ConnectionState::Disconnected => "Disconnected",
                ConnectionState::Error(e) => e.as_str(),
            })
            .dim(),
        ]))
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
            Span::raw("[↑↓/jk]").fg(Color::Yellow),
            Span::raw(" navigate").dim(),
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
                    Span::raw(node_name).style(style),
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
            Constraint::Length(6), // Version info
            Constraint::Length(4), // Resource usage
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
            Line::from(vec![
                Span::raw(" Go:       ").dim(),
                Span::raw(&version.go_version).fg(Color::DarkGray),
            ]),
        ];
        frame.render_widget(Paragraph::new(version_info), detail_layout[0]);

        // Memory info
        if let Some(mem) = self.get_node_memory(node_name) {
            let usage_pct = mem.usage_percent();
            let usage_color = if usage_pct > 90.0 {
                Color::Red
            } else if usage_pct > 70.0 {
                Color::Yellow
            } else {
                Color::Green
            };

            let mem_info = vec![
                Line::from(vec![
                    Span::raw(" Memory:   ").dim(),
                    Span::raw(format!("{:.1}%", usage_pct)).fg(usage_color),
                    Span::raw(format!(
                        " ({} MB / {} MB)",
                        mem.mem_available / 1024 / 1024,
                        mem.mem_total / 1024 / 1024
                    ))
                    .dim(),
                ]),
            ];
            frame.render_widget(Paragraph::new(mem_info), detail_layout[1]);
        }

        // Services list
        if let Some(services) = self.get_node_services(node_name) {
            let service_rows: Vec<Row> = services
                .iter()
                .map(|svc| {
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

                    Row::new(vec![
                        Span::raw(format!(" {} ", health_symbol)).fg(health_color),
                        Span::raw(&svc.id).fg(Color::White),
                        Span::raw(&svc.state).dim(),
                    ])
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
                    Span::raw("Service").dim(),
                    Span::raw("State").dim(),
                ])
                .style(Style::default().add_modifier(Modifier::UNDERLINED)),
            );

            frame.render_widget(services_table, detail_layout[2]);
        }
    }
}
