//! Etcd cluster status component
//!
//! Displays etcd cluster health with member list and details.

use crate::action::Action;
use crate::components::Component;
use crate::ui_ext::QuorumStateExt;
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
use talos_pilot_core::{
    AsyncState, QuorumState, SelectableList, format_bytes_signed, format_talos_error,
};
use talos_rs::{EtcdAlarm, EtcdMemberInfo, EtcdMemberStatus, TalosClient};

/// Combined etcd member data (from member list + status)
#[derive(Debug, Clone)]
pub struct EtcdMember {
    /// Member info from member list
    pub info: EtcdMemberInfo,
    /// Status from status call (None if unreachable)
    pub status: Option<EtcdMemberStatus>,
}

/// Default auto-refresh interval in seconds
const AUTO_REFRESH_INTERVAL_SECS: u64 = 5;

/// Loaded etcd data (wrapped by AsyncState)
#[derive(Debug, Clone, Default)]
pub struct EtcdData {
    /// Combined member data with selection
    pub members: SelectableList<EtcdMember>,
    /// Alarms
    pub alarms: Vec<EtcdAlarm>,
    /// Quorum state
    pub quorum_state: QuorumState,
    /// Total DB size (from largest member)
    pub total_db_size: i64,
    /// Current revision (from leader)
    pub revision: u64,
}

/// Etcd cluster status component
pub struct EtcdComponent {
    /// Async state wrapping all etcd data (loading, error, data)
    state: AsyncState<EtcdData>,

    /// Table state for rendering (synced with members selection)
    table_state: TableState,

    /// Auto-refresh enabled
    auto_refresh: bool,

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
        let mut table_state = TableState::default();
        table_state.select(Some(0));

        Self {
            state: AsyncState::new(),
            table_state,
            auto_refresh: true,
            client: None,
        }
    }

    /// Set the client for making API calls
    pub fn set_client(&mut self, client: TalosClient) {
        self.client = Some(client);
    }

    /// Set an error message
    pub fn set_error(&mut self, error: String) {
        self.state.set_error(error);
    }

    /// Helper to get data reference (convenience method)
    fn data(&self) -> Option<&EtcdData> {
        self.state.data()
    }

    /// Refresh etcd data from the cluster
    pub async fn refresh(&mut self) -> Result<()> {
        let Some(client) = &self.client else {
            self.state.set_error("No client configured");
            return Ok(());
        };

        self.state.start_loading();

        let timeout = Duration::from_secs(10);

        // Step 1: Fetch member list first (we need hostnames to target status calls)
        let members_result = tokio::time::timeout(timeout, client.etcd_members()).await;

        let member_infos = match members_result {
            Ok(Ok(members)) => members,
            Ok(Err(e)) => {
                let msg = format_talos_error(&e);
                self.state
                    .set_error_with_retry(format!("Failed to fetch members: {}", msg));
                return Ok(());
            }
            Err(_) => {
                self.state.set_error_with_retry(format!(
                    "Request timed out after {}s",
                    timeout.as_secs()
                ));
                return Ok(());
            }
        };

        // Step 2: Extract control plane IPs from members (hostnames may not be resolvable)
        let cp_ips: Vec<String> = member_infos.iter().filter_map(|m| m.ip_address()).collect();

        tracing::debug!("Fetching etcd status from control planes: {:?}", cp_ips);

        // Step 3: Fetch status (targeting all CPs by IP) and alarms in parallel
        let fetch_result = tokio::time::timeout(timeout, async {
            tokio::join!(client.etcd_status_for_nodes(&cp_ips), client.etcd_alarms())
        })
        .await;

        let (status_result, alarms_result) = match fetch_result {
            Ok(results) => results,
            Err(_) => {
                self.state.set_error_with_retry(format!(
                    "Request timed out after {}s",
                    timeout.as_secs()
                ));
                return Ok(());
            }
        };

        // Process status - non-critical, just log warning
        let statuses = match status_result {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Failed to fetch etcd status: {}", e);
                Vec::new()
            }
        };

        // Process alarms - non-critical
        let alarms = match alarms_result {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!("Failed to fetch etcd alarms: {}", e);
                Vec::new()
            }
        };

        // Combine member info with status
        let members: Vec<EtcdMember> = member_infos
            .into_iter()
            .map(|info| {
                let status = statuses.iter().find(|s| s.member_id == info.id).cloned();
                EtcdMember { info, status }
            })
            .collect();

        // Build EtcdData with calculated fields
        let mut data = if let Some(existing) = self.state.data_mut() {
            // Preserve selection state from existing data
            existing.members.update_items(members);
            std::mem::take(existing)
        } else {
            EtcdData {
                members: SelectableList::new(members),
                alarms: Vec::new(),
                quorum_state: QuorumState::Unknown,
                total_db_size: 0,
                revision: 0,
            }
        };

        // Update alarms
        data.alarms = alarms;

        // Calculate quorum state
        let total = data.members.len();
        let healthy = data
            .members
            .items()
            .iter()
            .filter(|m| m.status.is_some())
            .count();
        data.quorum_state = QuorumState::from_counts(healthy, total);

        // Calculate totals
        data.total_db_size = data
            .members
            .items()
            .iter()
            .filter_map(|m| m.status.as_ref())
            .map(|s| s.db_size)
            .max()
            .unwrap_or(0);

        data.revision = data
            .members
            .items()
            .iter()
            .filter_map(|m| m.status.as_ref())
            .map(|s| s.raft_index)
            .max()
            .unwrap_or(0);

        tracing::info!("Loaded {} etcd members", data.members.len());

        // Sync table state with SelectableList
        self.table_state.select(Some(data.members.selected_index()));

        // Set the data (clears error, resets retry count, updates last_refresh)
        self.state.set_data(data);

        Ok(())
    }

    /// Navigate to previous member
    fn select_prev(&mut self) {
        if let Some(data) = self.state.data_mut() {
            data.members.select_prev_no_wrap();
            self.table_state.select(Some(data.members.selected_index()));
        }
    }

    /// Navigate to next member
    fn select_next(&mut self) {
        if let Some(data) = self.state.data_mut() {
            data.members.select_next_no_wrap();
            self.table_state.select(Some(data.members.selected_index()));
        }
    }

    /// Draw the status bar
    fn draw_status_bar(&self, frame: &mut Frame, area: Rect) {
        let Some(data) = self.data() else {
            return;
        };

        let (indicator, color) = data.quorum_state.indicator_with_color();
        let (state_text, _) = data.quorum_state.display_with_color();

        let member_count = match &data.quorum_state {
            QuorumState::Healthy => format!("{}/{}", data.members.len(), data.members.len()),
            QuorumState::Degraded { healthy, total } => format!("{}/{}", healthy, total),
            QuorumState::NoQuorum { healthy, total } => format!("{}/{}", healthy, total),
            QuorumState::Unknown => "?/?".to_string(),
        };

        let db_size = format_bytes_signed(data.total_db_size);

        let line = Line::from(vec![
            Span::styled(format!("{} ", indicator), Style::default().fg(color)),
            Span::styled(
                state_text,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("  {} members    ", member_count)),
            Span::raw(format!(
                "Quorum: {} ",
                if matches!(data.quorum_state, QuorumState::NoQuorum { .. }) {
                    "No"
                } else {
                    "Yes"
                }
            )),
            Span::raw(format!("   DB: {}    Rev: {}", db_size, data.revision)),
        ]);

        let para = Paragraph::new(line).block(Block::default().borders(Borders::BOTTOM));
        frame.render_widget(para, area);
    }

    /// Draw the member table
    fn draw_member_table(&mut self, frame: &mut Frame, area: Rect) {
        let Some(data) = self.data() else {
            return;
        };

        let rows: Vec<Row> = data
            .members
            .items()
            .iter()
            .map(|member| {
                let (status_indicator, status_text, status_color) = match &member.status {
                    Some(s) if s.is_leader() => ("*", "Leader", Color::Magenta),
                    Some(_) => ("o", "Follow", Color::Green),
                    None => ("x", "DOWN", Color::Red),
                };

                let db_size = member
                    .status
                    .as_ref()
                    .map(|s| format_bytes_signed(s.db_size))
                    .unwrap_or_else(|| "-".to_string());

                let raft_idx = member
                    .status
                    .as_ref()
                    .map(|s| s.raft_index.to_string())
                    .unwrap_or_else(|| "-".to_string());

                let errors = member
                    .status
                    .as_ref()
                    .map(|s| {
                        if s.errors.is_empty() {
                            "-".to_string()
                        } else {
                            s.errors.len().to_string()
                        }
                    })
                    .unwrap_or_else(|| "-".to_string());

                // Get endpoint from client_urls
                let endpoint = member
                    .info
                    .client_urls
                    .first()
                    .map(|u| u.replace("https://", "").replace("http://", ""))
                    .unwrap_or_else(|| "unknown".to_string());

                let status_full = format!("{} {}", status_indicator, status_text);

                Row::new(vec![
                    Cell::from(member.info.hostname.clone()),
                    Cell::from(status_full).style(Style::default().fg(status_color)),
                    Cell::from(endpoint),
                    Cell::from(db_size),
                    Cell::from(raft_idx),
                    Cell::from(errors),
                ])
            })
            .collect();

        let header = Row::new(vec![
            Cell::from("MEMBER"),
            Cell::from("STATUS"),
            Cell::from("ENDPOINT"),
            Cell::from("DB SIZE"),
            Cell::from("RAFT IDX"),
            Cell::from("ERRORS"),
        ])
        .style(Style::default().add_modifier(Modifier::DIM))
        .bottom_margin(1);

        // Use proportional column widths to fill available space
        let widths = [
            Constraint::Min(12),        // MEMBER
            Constraint::Length(10),     // STATUS
            Constraint::Percentage(30), // ENDPOINT (flexible)
            Constraint::Length(10),     // DB SIZE
            Constraint::Length(12),     // RAFT IDX
            Constraint::Length(8),      // ERRORS
        ];

        let table = Table::new(rows, widths)
            .header(header)
            .row_highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        frame.render_stateful_widget(table, area, &mut self.table_state);
    }

    /// Draw the detail section for selected member
    fn draw_detail_section(&self, frame: &mut Frame, area: Rect) {
        let Some(data) = self.data() else {
            return;
        };
        let Some(member) = data.members.selected() else {
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
            let db_in_use = format_bytes_signed(status.db_size_in_use);
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
                    Span::styled(
                        "Raft Term:    ",
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                    Span::raw(status.raft_term.to_string()),
                ]),
                Line::from(vec![
                    Span::styled(
                        "  Peer URL:   ",
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                    Span::raw(format!("{:<30}", peer_url)),
                    Span::styled(
                        "Raft Applied: ",
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                    Span::raw(status.raft_applied_index.to_string()),
                ]),
                Line::from(vec![
                    Span::styled(
                        "  Client URL: ",
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                    Span::raw(format!("{:<30}", client_url)),
                    Span::styled(
                        "DB In Use:    ",
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                    Span::raw(format!("{} ({}%)", db_in_use, db_percent)),
                ]),
                Line::from(vec![
                    Span::styled(
                        "  Is Learner: ",
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                    Span::raw(format!(
                        "{:<30}",
                        if member.info.is_learner { "Yes" } else { "No" }
                    )),
                    Span::styled(
                        "Errors:       ",
                        Style::default().add_modifier(Modifier::DIM),
                    ),
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
                    Span::styled(
                        "  Peer URL:   ",
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                    Span::raw(member.info.peer_urls.first().cloned().unwrap_or_default()),
                ]),
                Line::from(vec![
                    Span::styled(
                        "  Client URL: ",
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                    Span::raw(member.info.client_urls.first().cloned().unwrap_or_default()),
                ]),
                Line::from(vec![
                    Span::styled("  Error: ", Style::default().add_modifier(Modifier::DIM)),
                    Span::styled(
                        "Connection failed - node may be down",
                        Style::default().fg(Color::Red),
                    ),
                ]),
            ]
        };

        let block = Block::default().title(title).borders(Borders::TOP);
        let para = Paragraph::new(content).block(block);
        frame.render_widget(para, area);
    }

    /// Draw the alarms section
    fn draw_alarms(&self, frame: &mut Frame, area: Rect) {
        let Some(data) = self.data() else {
            return;
        };

        let content = if data.alarms.is_empty() {
            Line::from(vec![
                Span::raw("Alarms: "),
                Span::styled("None", Style::default().fg(Color::Green)),
            ])
        } else {
            let alarm_text: Vec<Span> = data
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

        let para =
            Paragraph::new(content).block(Block::default().borders(Borders::TOP | Borders::BOTTOM));
        frame.render_widget(para, area);
    }

    /// Draw the footer with keybindings
    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let auto_status = if self.auto_refresh {
            Span::styled("ON ", Style::default().fg(Color::Green))
        } else {
            Span::styled("OFF", Style::default().fg(Color::DarkGray))
        };

        let line = Line::from(vec![
            Span::styled("[j/k]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" select  "),
            Span::styled("[l]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" etcd logs  "),
            Span::styled("[Enter]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" member logs  "),
            Span::styled("[r]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" refresh  "),
            Span::styled("[a]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" auto:"),
            auto_status,
            Span::raw("  "),
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
            KeyCode::Char('a') => {
                // Toggle auto-refresh
                self.auto_refresh = !self.auto_refresh;
                Ok(None)
            }
            KeyCode::Char('l') => {
                // View etcd logs for all control plane nodes
                let Some(data) = self.data() else {
                    return Ok(None);
                };
                if data.members.is_empty() {
                    return Ok(None);
                }
                // Use first member's IP as the "node" (hostnames may not be resolvable)
                // In practice, with the current API this shows etcd logs from all connected nodes
                let node = data
                    .members
                    .items()
                    .first()
                    .and_then(|m| m.info.ip_address())
                    .unwrap_or_else(|| "controlplane".to_string());
                let etcd_vec = vec!["etcd".to_string()];
                Ok(Some(Action::ShowMultiLogs(
                    node,
                    "controlplane".to_string(),
                    etcd_vec.clone(),
                    etcd_vec,
                )))
            }
            KeyCode::Enter => {
                // View etcd logs for selected member
                // Use IP address since hostnames may not be resolvable
                if let Some(data) = self.data()
                    && let Some(member) = data.members.selected()
                {
                    let node = member
                        .info
                        .ip_address()
                        .unwrap_or_else(|| member.info.hostname.clone());
                    let etcd_vec = vec!["etcd".to_string()];
                    return Ok(Some(Action::ShowMultiLogs(
                        node,
                        "controlplane".to_string(),
                        etcd_vec.clone(),
                        etcd_vec,
                    )));
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if let Action::Tick = action {
            // Check for auto-refresh using AsyncState helper
            let interval = Duration::from_secs(AUTO_REFRESH_INTERVAL_SECS);
            if self.state.should_auto_refresh(self.auto_refresh, interval) {
                return Ok(Some(Action::Refresh));
            }
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        // Show loading state (only if no data yet)
        if self.state.is_loading() && !self.state.has_data() {
            let loading = Paragraph::new("Loading...").style(Style::default().fg(Color::DarkGray));
            frame.render_widget(loading, area);
            return Ok(());
        }

        // Show error state (only if no data to display)
        if let Some(err) = self.state.error()
            && !self.state.has_data()
        {
            let error =
                Paragraph::new(format!("Error: {}", err)).style(Style::default().fg(Color::Red));
            frame.render_widget(error, area);
            return Ok(());
        }

        // Get data for layout calculations
        let Some(data) = self.data() else {
            return Ok(());
        };

        // Calculate dynamic heights
        let member_count = data.members.len().max(1);
        let table_height = (member_count + 2) as u16; // header + members + padding
        let detail_height = 6u16;
        let alarm_height = if data.alarms.is_empty() {
            2
        } else {
            (data.alarms.len() + 2) as u16
        };

        // Total content height (excluding flexible space)
        let content_height = 3 + table_height + 1 + detail_height + 1 + alarm_height + 1;

        // Layout: group content at top with small gaps, footer pinned to bottom
        let chunks = if area.height > content_height + 10 {
            // Large terminal: add breathing room between sections
            Layout::vertical([
                Constraint::Length(3),             // Status bar (with padding)
                Constraint::Length(table_height),  // Member table
                Constraint::Length(2),             // Spacer
                Constraint::Length(detail_height), // Detail section
                Constraint::Length(2),             // Spacer
                Constraint::Length(alarm_height),  // Alarms
                Constraint::Fill(1),               // Flexible space before footer
                Constraint::Length(1),             // Footer
            ])
            .split(area)
        } else {
            // Compact terminal: minimal spacing
            Layout::vertical([
                Constraint::Length(2),             // Status bar
                Constraint::Length(table_height),  // Member table
                Constraint::Length(detail_height), // Detail section
                Constraint::Length(alarm_height),  // Alarms
                Constraint::Fill(1),               // Flexible space
                Constraint::Length(1),             // Footer
            ])
            .split(area)
        };

        if area.height > content_height + 10 {
            // Large terminal layout indices
            self.draw_status_bar(frame, chunks[0]);
            self.draw_member_table(frame, chunks[1]);
            self.draw_detail_section(frame, chunks[3]);
            self.draw_alarms(frame, chunks[5]);
            self.draw_footer(frame, chunks[7]);
        } else {
            // Compact layout indices
            self.draw_status_bar(frame, chunks[0]);
            self.draw_member_table(frame, chunks[1]);
            self.draw_detail_section(frame, chunks[2]);
            self.draw_alarms(frame, chunks[3]);
            self.draw_footer(frame, chunks[5]);
        }

        Ok(())
    }
}
