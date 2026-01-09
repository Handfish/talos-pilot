//! Etcd cluster status component
//!
//! Displays etcd cluster health with member list and details.

use crate::action::Action;
use crate::components::Component;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};
use talos_rs::{EtcdAlarm, EtcdMemberInfo, EtcdMemberStatus, TalosClient};
use std::time::Instant;

/// Combined etcd member data (from member list + status)
#[derive(Debug, Clone)]
pub struct EtcdMember {
    /// Member info from member list
    pub info: EtcdMemberInfo,
    /// Status from status call (None if unreachable)
    pub status: Option<EtcdMemberStatus>,
}

/// Quorum state of the etcd cluster
#[derive(Debug, Clone, PartialEq)]
pub enum QuorumState {
    /// All members healthy
    Healthy,
    /// Some members down but quorum maintained
    Degraded { healthy: usize, total: usize },
    /// Quorum lost - critical
    NoQuorum { healthy: usize, total: usize },
    /// Unknown state (loading or error)
    Unknown,
}

impl QuorumState {
    /// Get display text for the quorum state
    pub fn display(&self) -> (&'static str, Color) {
        match self {
            QuorumState::Healthy => ("HEALTHY", Color::Green),
            QuorumState::Degraded { .. } => ("DEGRADED", Color::Yellow),
            QuorumState::NoQuorum { .. } => ("NO QUORUM", Color::Red),
            QuorumState::Unknown => ("UNKNOWN", Color::DarkGray),
        }
    }

    /// Get the status indicator
    pub fn indicator(&self) -> (&'static str, Color) {
        match self {
            QuorumState::Healthy => ("●", Color::Green),
            QuorumState::Degraded { .. } => ("◐", Color::Yellow),
            QuorumState::NoQuorum { .. } => ("✗", Color::Red),
            QuorumState::Unknown => ("?", Color::DarkGray),
        }
    }
}

/// Etcd cluster status component
pub struct EtcdComponent {
    /// Combined member data
    members: Vec<EtcdMember>,
    /// Alarms
    alarms: Vec<EtcdAlarm>,
    /// Quorum state
    quorum_state: QuorumState,
    /// Total DB size (sum of all members)
    total_db_size: i64,
    /// Current revision (from leader)
    revision: u64,

    /// Selected member index
    selected: usize,
    /// List state for rendering
    list_state: ListState,

    /// Loading state
    loading: bool,
    /// Error message
    error: Option<String>,

    /// Last refresh time
    last_refresh: Option<Instant>,

    /// Client for API calls
    client: Option<TalosClient>,
}

impl Default for EtcdComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl EtcdComponent {
    pub fn new() -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));

        Self {
            members: Vec::new(),
            alarms: Vec::new(),
            quorum_state: QuorumState::Unknown,
            total_db_size: 0,
            revision: 0,
            selected: 0,
            list_state,
            loading: true,
            error: None,
            last_refresh: None,
            client: None,
        }
    }

    /// Set the client for making API calls
    pub fn set_client(&mut self, client: TalosClient) {
        self.client = Some(client);
    }

    /// Set an error message
    pub fn set_error(&mut self, error: String) {
        self.error = Some(error);
        self.loading = false;
    }

    /// Refresh etcd data from the cluster
    pub async fn refresh(&mut self) -> Result<()> {
        let Some(client) = &self.client else {
            self.error = Some("No client configured".to_string());
            return Ok(());
        };

        self.loading = true;
        self.error = None;

        // Fetch member list, status, and alarms in parallel
        let (members_result, status_result, alarms_result) = tokio::join!(
            client.etcd_members(),
            client.etcd_status(),
            client.etcd_alarms()
        );

        // Process member list
        let member_infos = match members_result {
            Ok(members) => members,
            Err(e) => {
                self.error = Some(format!("Failed to fetch members: {}", e));
                self.loading = false;
                return Ok(());
            }
        };

        // Process status
        let statuses = match status_result {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Failed to fetch etcd status: {}", e);
                Vec::new()
            }
        };

        // Process alarms
        self.alarms = match alarms_result {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!("Failed to fetch etcd alarms: {}", e);
                Vec::new()
            }
        };

        // Combine member info with status
        self.members = member_infos
            .into_iter()
            .map(|info| {
                let status = statuses.iter().find(|s| s.member_id == info.id).cloned();
                EtcdMember { info, status }
            })
            .collect();

        // Calculate quorum state
        self.calculate_quorum_state();

        // Calculate totals
        self.calculate_totals();

        self.loading = false;
        self.last_refresh = Some(Instant::now());

        Ok(())
    }

    /// Calculate the quorum state based on member statuses
    fn calculate_quorum_state(&mut self) {
        let total = self.members.len();
        let healthy = self.members.iter().filter(|m| m.status.is_some()).count();
        let quorum_needed = total / 2 + 1;

        self.quorum_state = if healthy == total {
            QuorumState::Healthy
        } else if healthy >= quorum_needed {
            QuorumState::Degraded { healthy, total }
        } else {
            QuorumState::NoQuorum { healthy, total }
        };
    }

    /// Calculate total DB size and revision
    fn calculate_totals(&mut self) {
        self.total_db_size = self
            .members
            .iter()
            .filter_map(|m| m.status.as_ref())
            .map(|s| s.db_size)
            .max()
            .unwrap_or(0);

        self.revision = self
            .members
            .iter()
            .filter_map(|m| m.status.as_ref())
            .map(|s| s.raft_index)
            .max()
            .unwrap_or(0);
    }

    /// Get the currently selected member
    fn selected_member(&self) -> Option<&EtcdMember> {
        self.members.get(self.selected)
    }

    /// Navigate to previous member
    fn select_prev(&mut self) {
        if !self.members.is_empty() {
            self.selected = self.selected.saturating_sub(1);
            self.list_state.select(Some(self.selected));
        }
    }

    /// Navigate to next member
    fn select_next(&mut self) {
        if !self.members.is_empty() {
            self.selected = (self.selected + 1).min(self.members.len() - 1);
            self.list_state.select(Some(self.selected));
        }
    }

    /// Format bytes for display
    fn format_bytes(bytes: i64) -> String {
        const KB: i64 = 1024;
        const MB: i64 = KB * 1024;
        const GB: i64 = MB * 1024;

        if bytes >= GB {
            format!("{:.1} GB", bytes as f64 / GB as f64)
        } else if bytes >= MB {
            format!("{:.1} MB", bytes as f64 / MB as f64)
        } else if bytes >= KB {
            format!("{:.1} KB", bytes as f64 / KB as f64)
        } else {
            format!("{} B", bytes)
        }
    }

    /// Draw the status bar
    fn draw_status_bar(&self, frame: &mut Frame, area: Rect) {
        let (indicator, color) = self.quorum_state.indicator();
        let (state_text, _) = self.quorum_state.display();

        let member_count = match &self.quorum_state {
            QuorumState::Healthy => format!("{}/{}", self.members.len(), self.members.len()),
            QuorumState::Degraded { healthy, total } => format!("{}/{}", healthy, total),
            QuorumState::NoQuorum { healthy, total } => format!("{}/{}", healthy, total),
            QuorumState::Unknown => "?/?".to_string(),
        };

        let db_size = Self::format_bytes(self.total_db_size);

        let line = Line::from(vec![
            Span::styled(format!("{} ", indicator), Style::default().fg(color)),
            Span::styled(state_text, Style::default().fg(color).add_modifier(Modifier::BOLD)),
            Span::raw(format!("  {} members    ", member_count)),
            Span::raw(format!("Quorum: {} ", if matches!(self.quorum_state, QuorumState::NoQuorum { .. }) { "No" } else { "Yes" })),
            Span::raw(format!("   DB: {}    Rev: {}", db_size, self.revision)),
        ]);

        let para = Paragraph::new(line)
            .block(Block::default().borders(Borders::BOTTOM));
        frame.render_widget(para, area);
    }

    /// Draw the member table
    fn draw_member_table(&mut self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .members
            .iter()
            .enumerate()
            .map(|(idx, member)| {
                let is_selected = idx == self.selected;
                let cursor = if is_selected { ">" } else { " " };

                let (status_indicator, status_text, status_color) = match &member.status {
                    Some(s) if s.is_leader() => ("★", "Leader", Color::Magenta),
                    Some(_) => ("●", "Follow", Color::Green),
                    None => ("✗", "DOWN", Color::Red),
                };

                let db_size = member
                    .status
                    .as_ref()
                    .map(|s| Self::format_bytes(s.db_size))
                    .unwrap_or_else(|| "-".to_string());

                let raft_idx = member
                    .status
                    .as_ref()
                    .map(|s| s.raft_index.to_string())
                    .unwrap_or_else(|| "-".to_string());

                let errors = member
                    .status
                    .as_ref()
                    .map(|s| s.errors.len().to_string())
                    .unwrap_or_else(|| "-".to_string());

                // Get endpoint from client_urls
                let endpoint = member
                    .info
                    .client_urls
                    .first()
                    .map(|u| u.replace("https://", "").replace("http://", ""))
                    .unwrap_or_else(|| "unknown".to_string());

                let line = Line::from(vec![
                    Span::styled(cursor, Style::default().fg(Color::Cyan)),
                    Span::raw(" "),
                    Span::styled(
                        format!("{:<12}", member.info.hostname),
                        Style::default().add_modifier(if is_selected { Modifier::BOLD } else { Modifier::empty() }),
                    ),
                    Span::raw(" "),
                    Span::styled(status_indicator, Style::default().fg(status_color)),
                    Span::raw(" "),
                    Span::styled(format!("{:<8}", status_text), Style::default().fg(status_color)),
                    Span::raw(format!("{:<20}", endpoint)),
                    Span::raw(format!("{:>10}", db_size)),
                    Span::raw(format!("{:>12}", raft_idx)),
                    Span::raw(format!("{:>8}", errors)),
                ]);

                ListItem::new(line)
            })
            .collect();

        // Header
        let header = Line::from(vec![
            Span::raw("  "),
            Span::styled("MEMBER      ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw("   "),
            Span::styled("STATUS  ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled(format!("{:<20}", "ENDPOINT"), Style::default().add_modifier(Modifier::DIM)),
            Span::styled(format!("{:>10}", "DB SIZE"), Style::default().add_modifier(Modifier::DIM)),
            Span::styled(format!("{:>12}", "RAFT IDX"), Style::default().add_modifier(Modifier::DIM)),
            Span::styled(format!("{:>8}", "ERRORS"), Style::default().add_modifier(Modifier::DIM)),
        ]);

        let header_para = Paragraph::new(header);
        let header_area = Rect { height: 1, ..area };
        frame.render_widget(header_para, header_area);

        // List
        let list_area = Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(1),
            ..area
        };
        let list = List::new(items);
        frame.render_stateful_widget(list, list_area, &mut self.list_state);
    }

    /// Draw the detail section for selected member
    fn draw_detail_section(&self, frame: &mut Frame, area: Rect) {
        let Some(member) = self.selected_member() else {
            return;
        };

        let role = match &member.status {
            Some(s) if s.is_leader() => "Leader",
            Some(s) if s.is_learner => "Learner",
            Some(_) => "Follower",
            None => "Unreachable",
        };

        let title = format!(" {} ({}) ", member.info.hostname, role);

        let content = if let Some(status) = &member.status {
            let peer_url = member.info.peer_urls.first().cloned().unwrap_or_default();
            let client_url = member.info.client_urls.first().cloned().unwrap_or_default();
            let db_in_use = Self::format_bytes(status.db_size_in_use);
            let db_percent = if status.db_size > 0 {
                (status.db_size_in_use as f64 / status.db_size as f64 * 100.0) as u32
            } else {
                0
            };
            let errors = if status.errors.is_empty() {
                "None".to_string()
            } else {
                status.errors.join(", ")
            };

            vec![
                Line::from(vec![
                    Span::styled("  ID: ", Style::default().add_modifier(Modifier::DIM)),
                    Span::raw(format!("{:x}", status.member_id)),
                    Span::raw("                        "),
                    Span::styled("Raft Term:    ", Style::default().add_modifier(Modifier::DIM)),
                    Span::raw(status.raft_term.to_string()),
                ]),
                Line::from(vec![
                    Span::styled("  Peer URL:   ", Style::default().add_modifier(Modifier::DIM)),
                    Span::raw(format!("{:<30}", peer_url)),
                    Span::styled("Raft Applied: ", Style::default().add_modifier(Modifier::DIM)),
                    Span::raw(status.raft_applied_index.to_string()),
                ]),
                Line::from(vec![
                    Span::styled("  Client URL: ", Style::default().add_modifier(Modifier::DIM)),
                    Span::raw(format!("{:<30}", client_url)),
                    Span::styled("DB In Use:    ", Style::default().add_modifier(Modifier::DIM)),
                    Span::raw(format!("{} ({}%)", db_in_use, db_percent)),
                ]),
                Line::from(vec![
                    Span::styled("  Is Learner: ", Style::default().add_modifier(Modifier::DIM)),
                    Span::raw(format!("{:<30}", if member.info.is_learner { "Yes" } else { "No" })),
                    Span::styled("Errors:       ", Style::default().add_modifier(Modifier::DIM)),
                    Span::raw(errors),
                ]),
            ]
        } else {
            vec![
                Line::from(vec![
                    Span::styled("  ID: ", Style::default().add_modifier(Modifier::DIM)),
                    Span::raw(format!("{:x}", member.info.id)),
                ]),
                Line::from(vec![
                    Span::styled("  Peer URL:   ", Style::default().add_modifier(Modifier::DIM)),
                    Span::raw(member.info.peer_urls.first().cloned().unwrap_or_default()),
                ]),
                Line::from(vec![
                    Span::styled("  Client URL: ", Style::default().add_modifier(Modifier::DIM)),
                    Span::raw(member.info.client_urls.first().cloned().unwrap_or_default()),
                ]),
                Line::from(vec![
                    Span::styled("  Error: ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled("Connection failed - node may be down", Style::default().fg(Color::Red)),
                ]),
            ]
        };

        let block = Block::default()
            .title(title)
            .borders(Borders::TOP);
        let para = Paragraph::new(content).block(block);
        frame.render_widget(para, area);
    }

    /// Draw the alarms section
    fn draw_alarms(&self, frame: &mut Frame, area: Rect) {
        let content = if self.alarms.is_empty() {
            Line::from(vec![
                Span::raw("Alarms: "),
                Span::styled("None", Style::default().fg(Color::Green)),
            ])
        } else {
            let alarm_text: Vec<Span> = self
                .alarms
                .iter()
                .flat_map(|a| {
                    vec![
                        Span::styled("⚠ ", Style::default().fg(Color::Yellow)),
                        Span::styled(
                            format!("{}: {} ", a.alarm_type.as_str(), a.node),
                            Style::default().fg(Color::Yellow),
                        ),
                    ]
                })
                .collect();
            Line::from(alarm_text)
        };

        let para = Paragraph::new(content)
            .block(Block::default().borders(Borders::TOP | Borders::BOTTOM));
        frame.render_widget(para, area);
    }

    /// Draw the footer with keybindings
    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let line = Line::from(vec![
            Span::styled("[j/k]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" select  "),
            Span::styled("[l]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" etcd logs  "),
            Span::styled("[Enter]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" member logs  "),
            Span::styled("[r]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" refresh  "),
            Span::styled("[q]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" back"),
        ]);

        let para = Paragraph::new(line);
        frame.render_widget(para, area);
    }
}

impl Component for EtcdComponent {
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
            // TODO: Add log viewing actions
            KeyCode::Char('l') => {
                // View etcd logs for all control plane nodes
                Ok(None)
            }
            KeyCode::Enter => {
                // View etcd logs for selected member
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn update(&mut self, _action: Action) -> Result<Option<Action>> {
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        // Layout:
        // - Header/title: 1 line (handled by parent)
        // - Status bar: 2 lines
        // - Member table: dynamic (header + members)
        // - Detail section: 6 lines
        // - Alarms: 2 lines
        // - Footer: 1 line

        let member_count = self.members.len().max(1);
        let table_height = (member_count + 2) as u16; // header + members + padding

        let chunks = Layout::vertical([
            Constraint::Length(2),              // Status bar
            Constraint::Length(table_height),   // Member table
            Constraint::Length(6),              // Detail section
            Constraint::Length(2),              // Alarms
            Constraint::Length(1),              // Footer
        ])
        .split(area);

        if self.loading {
            let loading = Paragraph::new("Loading...")
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

        self.draw_status_bar(frame, chunks[0]);
        self.draw_member_table(frame, chunks[1]);
        self.draw_detail_section(frame, chunks[2]);
        self.draw_alarms(frame, chunks[3]);
        self.draw_footer(frame, chunks[4]);

        Ok(())
    }
}
