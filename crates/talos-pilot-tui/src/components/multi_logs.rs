//! Multi-service logs component - Stern-style interleaved log viewer

use crate::action::Action;
use crate::components::Component;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};
use std::collections::HashSet;

/// Maximum entries to keep in memory (ring buffer)
const MAX_ENTRIES: usize = 5000;

/// Color palette for services (deterministic assignment)
const SERVICE_COLORS: &[Color] = &[
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Magenta,
    Color::Cyan,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightBlue,
    Color::LightMagenta,
    Color::LightCyan,
];

/// Log level (reused from logs.rs pattern)
#[derive(Debug, Clone, Copy, PartialEq)]
enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Unknown,
}

impl LogLevel {
    fn from_str(s: &str) -> Self {
        let lower = s.to_lowercase();
        if lower.contains("error") || lower.contains("err") {
            LogLevel::Error
        } else if lower.contains("warn") {
            LogLevel::Warn
        } else if lower.contains("info") {
            LogLevel::Info
        } else if lower.contains("debug") || lower.contains("trace") {
            LogLevel::Debug
        } else {
            LogLevel::Unknown
        }
    }

    fn color(&self) -> Color {
        match self {
            LogLevel::Error => Color::Red,
            LogLevel::Warn => Color::Yellow,
            LogLevel::Info => Color::Green,
            LogLevel::Debug => Color::DarkGray,
            LogLevel::Unknown => Color::White,
        }
    }

    fn badge(&self) -> &'static str {
        match self {
            LogLevel::Error => "ERR",
            LogLevel::Warn => "WRN",
            LogLevel::Info => "INF",
            LogLevel::Debug => "DBG",
            LogLevel::Unknown => "---",
        }
    }
}

/// A log entry from any service
#[derive(Debug, Clone)]
struct MultiLogEntry {
    /// Service this entry came from
    service_id: String,
    /// Assigned color for this service
    service_color: Color,
    /// Parsed timestamp (display format)
    timestamp: String,
    /// Raw timestamp for sorting (if parseable)
    timestamp_sort: i64,
    /// Log level
    level: LogLevel,
    /// Log message
    message: String,
    /// Pre-computed lowercase for search
    search_text: String,
}

/// Service state for sidebar
#[derive(Debug, Clone)]
struct ServiceState {
    /// Service ID (e.g., "kubelet")
    id: String,
    /// Display color
    color: Color,
    /// Whether this service is active (showing logs)
    active: bool,
    /// Number of entries from this service
    entry_count: usize,
}

/// View mode
#[derive(Debug, Clone, Copy, PartialEq)]
enum ViewMode {
    /// Full mode with sidebar
    Full,
    /// Compact mode without sidebar
    Compact,
}

/// Search mode
#[derive(Debug, Clone, PartialEq)]
enum SearchMode {
    Off,
    Input,
    Active,
}

/// Sidebar focus state
#[derive(Debug, Clone, Copy, PartialEq)]
enum Focus {
    Sidebar,
    Logs,
}

/// Multi-service logs component
pub struct MultiLogsComponent {
    /// Node IP being viewed
    node_ip: String,
    /// Node role (controlplane/worker)
    node_role: String,

    /// All services available
    services: Vec<ServiceState>,
    /// Selected service in sidebar
    selected_service: usize,
    /// Sidebar list state
    sidebar_state: ListState,

    /// All log entries (sorted by timestamp)
    entries: Vec<MultiLogEntry>,
    /// Filtered entries (indices into entries vec, only active services)
    visible_indices: Vec<usize>,

    /// View mode
    view_mode: ViewMode,
    /// Current focus (sidebar or logs)
    focus: Focus,
    /// Scroll position in logs
    scroll: u16,
    /// Following mode (auto-scroll to bottom)
    following: bool,

    /// Whether logs are loading
    loading: bool,
    /// Error message if any
    error: Option<String>,

    /// Search mode
    search_mode: SearchMode,
    /// Search query
    search_query: String,
    /// Set of matching entry indices (into visible_indices)
    match_set: HashSet<usize>,
    /// Ordered matches for n/N navigation
    match_order: Vec<usize>,
    /// Current match index
    current_match: usize,
}

impl MultiLogsComponent {
    /// Create a new multi-logs component
    pub fn new(node_ip: String, node_role: String, service_ids: Vec<String>) -> Self {
        // Assign colors to services deterministically
        let services: Vec<ServiceState> = service_ids
            .into_iter()
            .enumerate()
            .map(|(i, id)| ServiceState {
                color: SERVICE_COLORS[i % SERVICE_COLORS.len()],
                id,
                active: true, // All active by default
                entry_count: 0,
            })
            .collect();

        let mut sidebar_state = ListState::default();
        sidebar_state.select(Some(0));

        Self {
            node_ip,
            node_role,
            services,
            selected_service: 0,
            sidebar_state,
            entries: Vec::new(),
            visible_indices: Vec::new(),
            view_mode: ViewMode::Full,
            focus: Focus::Logs,
            scroll: 0,
            following: true,
            loading: true,
            error: None,
            search_mode: SearchMode::Off,
            search_query: String::new(),
            match_set: HashSet::new(),
            match_order: Vec::new(),
            current_match: 0,
        }
    }

    /// Get color for a service by ID
    fn get_service_color(&self, service_id: &str) -> Color {
        self.services
            .iter()
            .find(|s| s.id == service_id)
            .map(|s| s.color)
            .unwrap_or(Color::White)
    }

    /// Set log content from multiple services
    pub fn set_logs(&mut self, logs: Vec<(String, String)>) {
        self.entries.clear();

        // Parse all logs
        for (service_id, content) in logs {
            let color = self.get_service_color(&service_id);

            for line in content.lines() {
                if line.trim().is_empty() {
                    continue;
                }

                let entry = Self::parse_line(line, &service_id, color);
                self.entries.push(entry);
            }
        }

        // Sort by timestamp
        self.entries.sort_by_key(|e| e.timestamp_sort);

        // Enforce max entries (ring buffer behavior)
        if self.entries.len() > MAX_ENTRIES {
            self.entries.drain(0..self.entries.len() - MAX_ENTRIES);
        }

        // Update service entry counts
        for service in &mut self.services {
            service.entry_count = self.entries.iter().filter(|e| e.service_id == service.id).count();
        }

        // Build visible indices
        self.rebuild_visible_indices();

        self.loading = false;

        // Scroll to bottom if following
        if self.following {
            self.scroll_to_bottom();
        }
    }

    /// Parse a log line into a MultiLogEntry
    fn parse_line(line: &str, service_id: &str, color: Color) -> MultiLogEntry {
        let line = line.trim();
        let search_text = line.to_lowercase();

        let (timestamp, timestamp_sort, rest) = Self::extract_timestamp(line);
        let level = LogLevel::from_str(rest);
        let message = Self::clean_message(rest);

        MultiLogEntry {
            service_id: service_id.to_string(),
            service_color: color,
            timestamp,
            timestamp_sort,
            level,
            message,
            search_text,
        }
    }

    /// Extract timestamp from line start
    fn extract_timestamp(line: &str) -> (String, i64, &str) {
        let chars: Vec<char> = line.chars().collect();
        let mut end = 0;
        let mut has_colon = false;

        for (i, c) in chars.iter().enumerate() {
            if *c == ':' {
                has_colon = true;
            }

            if c.is_ascii_digit() || *c == '/' || *c == '-' || *c == ':' || *c == '.' || *c == 'T' {
                end = i + 1;
            } else if *c == ' ' {
                if let Some(next) = chars.get(i + 1)
                    && next.is_ascii_digit()
                {
                    end = i + 1;
                    continue;
                }
                break;
            } else {
                break;
            }
        }

        if end >= 8 && has_colon && end < line.len() {
            let ts = line[..end].trim();
            let rest = line[end..].trim();
            let short_ts = Self::shorten_timestamp(ts);
            // Use character position as sort key (good enough for same-second ordering)
            let sort_key = Self::parse_sort_key(ts);
            (short_ts, sort_key, rest)
        } else {
            (String::new(), 0, line)
        }
    }

    /// Parse timestamp to sortable integer
    fn parse_sort_key(ts: &str) -> i64 {
        // Simple approach: extract digits and create comparable number
        // Format: YYYYMMDDHHMMSS or just HHMMSS
        let digits: String = ts.chars().filter(|c| c.is_ascii_digit()).collect();
        digits.parse().unwrap_or(0)
    }

    /// Shorten timestamp to HH:MM:SS
    fn shorten_timestamp(ts: &str) -> String {
        let bytes = ts.as_bytes();
        for i in 0..bytes.len().saturating_sub(4) {
            if bytes[i].is_ascii_digit()
                && bytes[i + 1].is_ascii_digit()
                && bytes[i + 2] == b':'
                && bytes[i + 3].is_ascii_digit()
                && bytes[i + 4].is_ascii_digit()
            {
                let time_start = i;
                let time_end = (time_start + 8).min(ts.len());
                return ts[time_start..time_end].to_string();
            }
        }

        if ts.len() > 8 {
            ts[..8].to_string()
        } else {
            ts.to_string()
        }
    }

    /// Clean message text
    fn clean_message(text: &str) -> String {
        let text = text.trim();
        let text = if let Some(pos) = text.find(": ") {
            if pos < 20 {
                text[pos + 2..].trim()
            } else {
                text
            }
        } else {
            text
        };
        text.trim_start_matches("[INFO]")
            .trim_start_matches("[WARN]")
            .trim_start_matches("[ERROR]")
            .trim_start_matches("[DEBUG]")
            .trim_start_matches("INFO")
            .trim_start_matches("WARN")
            .trim_start_matches("ERROR")
            .trim_start_matches("DEBUG")
            .trim_start_matches("OK")
            .trim()
            .to_string()
    }

    /// Set error message
    pub fn set_error(&mut self, error: String) {
        self.error = Some(error);
        self.loading = false;
    }

    /// Rebuild visible indices based on active services
    fn rebuild_visible_indices(&mut self) {
        let active_services: HashSet<&str> = self.services
            .iter()
            .filter(|s| s.active)
            .map(|s| s.id.as_str())
            .collect();

        self.visible_indices = self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| active_services.contains(e.service_id.as_str()))
            .map(|(i, _)| i)
            .collect();

        // Update search if active
        if self.search_mode == SearchMode::Active {
            self.update_matches();
        }
    }

    /// Toggle service active state
    fn toggle_service(&mut self, index: usize) {
        if let Some(service) = self.services.get_mut(index) {
            service.active = !service.active;
            self.rebuild_visible_indices();
        }
    }

    /// Set all services active
    fn activate_all(&mut self) {
        for service in &mut self.services {
            service.active = true;
        }
        self.rebuild_visible_indices();
    }

    /// Set all services inactive
    fn deactivate_all(&mut self) {
        for service in &mut self.services {
            service.active = false;
        }
        self.rebuild_visible_indices();
    }

    /// Count active services
    fn active_count(&self) -> usize {
        self.services.iter().filter(|s| s.active).count()
    }

    /// Scroll up
    fn scroll_up(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_sub(amount);
        self.following = false;
    }

    /// Scroll down
    fn scroll_down(&mut self, amount: u16) {
        let max = self.visible_indices.len().saturating_sub(1) as u16;
        self.scroll = (self.scroll + amount).min(max);
    }

    /// Scroll to bottom and enable following
    fn scroll_to_bottom(&mut self) {
        self.scroll = self.visible_indices.len().saturating_sub(1) as u16;
        self.following = true;
    }

    /// Update search matches
    fn update_matches(&mut self) {
        self.match_set.clear();
        self.match_order.clear();
        self.current_match = 0;

        if self.search_query.is_empty() {
            return;
        }

        let query_lower = self.search_query.to_lowercase();
        for (vi, &entry_idx) in self.visible_indices.iter().enumerate() {
            if self.entries[entry_idx].search_text.contains(&query_lower) {
                self.match_set.insert(vi);
                self.match_order.push(vi);
            }
        }

        if !self.match_order.is_empty() {
            self.scroll = self.match_order[0] as u16;
        }
    }

    /// Go to next match
    fn next_match(&mut self) {
        if self.match_order.is_empty() {
            return;
        }
        self.current_match = (self.current_match + 1) % self.match_order.len();
        self.scroll = self.match_order[self.current_match] as u16;
        self.following = false;
    }

    /// Go to previous match
    fn prev_match(&mut self) {
        if self.match_order.is_empty() {
            return;
        }
        self.current_match = if self.current_match == 0 {
            self.match_order.len() - 1
        } else {
            self.current_match - 1
        };
        self.scroll = self.match_order[self.current_match] as u16;
        self.following = false;
    }

    /// Clear search
    fn clear_search(&mut self) {
        self.search_mode = SearchMode::Off;
        self.search_query.clear();
        self.match_set.clear();
        self.match_order.clear();
        self.current_match = 0;
    }

    /// Check if a visible index is current match
    fn is_current_match(&self, visible_idx: usize) -> bool {
        if self.match_order.is_empty() {
            return false;
        }
        self.match_order.get(self.current_match) == Some(&visible_idx)
    }

    /// Check if a visible index matches search
    fn entry_matches(&self, visible_idx: usize) -> bool {
        self.match_set.contains(&visible_idx)
    }

    /// Render message with search highlighting
    fn render_message_with_highlight(&self, message: &str, is_current: bool) -> Vec<Span<'static>> {
        if self.search_query.is_empty() {
            return vec![Span::raw(message.to_string())];
        }

        let query_lower = self.search_query.to_lowercase();
        let message_lower = message.to_lowercase();
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut last_end = 0;

        for (start, _) in message_lower.match_indices(&query_lower) {
            if start > last_end {
                spans.push(Span::raw(message[last_end..start].to_string()));
            }
            let end = start + self.search_query.len();
            let style = if is_current {
                Style::default().bg(Color::Yellow).fg(Color::Black).bold()
            } else {
                Style::default().bg(Color::DarkGray).fg(Color::White)
            };
            spans.push(Span::styled(message[start..end].to_string(), style));
            last_end = end;
        }

        if last_end < message.len() {
            spans.push(Span::raw(message[last_end..].to_string()));
        }

        if spans.is_empty() {
            vec![Span::raw(message.to_string())]
        } else {
            spans
        }
    }

    /// Draw the sidebar
    fn draw_sidebar(&mut self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self.services
            .iter()
            .map(|s| {
                let indicator = if s.active { "●" } else { "○" };
                let style = Style::default().fg(s.color);
                let count_str = format!(" ({})", s.entry_count);

                ListItem::new(Line::from(vec![
                    Span::styled(indicator, style),
                    Span::raw(" "),
                    Span::styled(&s.id, style),
                    Span::raw(count_str).dim(),
                ]))
            })
            .collect();

        let border_style = if self.focus == Focus::Sidebar {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let list = List::new(items)
            .block(
                Block::default()
                    .title(" Services ")
                    .borders(Borders::ALL)
                    .border_style(border_style),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

        frame.render_stateful_widget(list, area, &mut self.sidebar_state);
    }

    /// Draw the logs area
    fn draw_logs(&self, frame: &mut Frame, area: Rect) {
        if self.loading {
            let loading = Paragraph::new(Line::from(Span::raw(" Loading logs...").dim()));
            frame.render_widget(loading, area);
            return;
        }

        if let Some(error) = &self.error {
            let error_msg = Paragraph::new(vec![
                Line::from(vec![Span::raw(" Error: ").fg(Color::Red).bold()]),
                Line::from(vec![Span::raw(" "), Span::raw(error).fg(Color::White)]),
            ]);
            frame.render_widget(error_msg, area);
            return;
        }

        if self.visible_indices.is_empty() {
            let msg = if self.active_count() == 0 {
                " No services selected. Press 'a' to activate all."
            } else {
                " No log entries"
            };
            let empty = Paragraph::new(Line::from(Span::raw(msg).dim()));
            frame.render_widget(empty, area);
            return;
        }

        let visible_height = area.height as usize;
        let content_width = area.width.saturating_sub(2) as usize;

        let start = self.scroll as usize;
        let end = (start + visible_height).min(self.visible_indices.len());

        let mut lines: Vec<Line> = Vec::new();

        for (vi, &entry_idx) in self.visible_indices[start..end].iter().enumerate() {
            let visible_idx = start + vi;
            let entry = &self.entries[entry_idx];
            let is_current_match = self.is_current_match(visible_idx);
            let is_match = self.entry_matches(visible_idx);

            let mut spans = Vec::new();

            // Match indicator
            if is_current_match {
                spans.push(Span::styled("▶", Style::default().fg(Color::Yellow)));
            } else {
                spans.push(Span::raw(" "));
            }

            // Timestamp
            if !entry.timestamp.is_empty() {
                spans.push(Span::styled(
                    format!("{:>8}", entry.timestamp),
                    Style::default().fg(Color::DarkGray),
                ));
            } else {
                spans.push(Span::raw("        "));
            }
            spans.push(Span::raw(" "));

            // Service name (colored, fixed width)
            spans.push(Span::styled(
                format!("{:<12}", entry.service_id),
                Style::default().fg(entry.service_color),
            ));
            spans.push(Span::raw(" "));

            // Level badge
            let level_style = Style::default()
                .fg(Color::Black)
                .bg(entry.level.color())
                .add_modifier(Modifier::BOLD);
            spans.push(Span::styled(entry.level.badge(), level_style));
            spans.push(Span::raw(" "));

            // Message with optional highlighting
            let prefix_width = 1 + 8 + 1 + 12 + 1 + 3 + 1; // indicator + time + service + level
            let available = content_width.saturating_sub(prefix_width);

            if entry.message.len() <= available {
                if is_match && !self.search_query.is_empty() {
                    spans.extend(self.render_message_with_highlight(&entry.message, is_current_match));
                } else {
                    spans.push(Span::raw(entry.message.clone()));
                }
            } else {
                let truncated: String = entry.message.chars().take(available.saturating_sub(1)).collect();
                if is_match && !self.search_query.is_empty() {
                    spans.extend(self.render_message_with_highlight(&truncated, is_current_match));
                } else {
                    spans.push(Span::raw(truncated));
                }
                spans.push(Span::raw("…").dim());
            }

            lines.push(Line::from(spans));
        }

        let logs = Paragraph::new(lines);
        frame.render_widget(logs, area);

        // Scrollbar
        if self.visible_indices.len() > visible_height {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("▲"))
                .end_symbol(Some("▼"))
                .track_symbol(Some("│"))
                .thumb_symbol("█");
            let mut scrollbar_state = ScrollbarState::new(self.visible_indices.len())
                .position(self.scroll as usize)
                .viewport_content_length(visible_height);
            frame.render_stateful_widget(scrollbar, area, &mut scrollbar_state);
        }
    }
}

impl Component for MultiLogsComponent {
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        // Handle search input mode
        if self.search_mode == SearchMode::Input {
            match key.code {
                KeyCode::Esc => {
                    self.clear_search();
                }
                KeyCode::Enter => {
                    if !self.search_query.is_empty() {
                        self.search_mode = SearchMode::Active;
                    } else {
                        self.clear_search();
                    }
                }
                KeyCode::Backspace => {
                    self.search_query.pop();
                    self.update_matches();
                }
                KeyCode::Char(c) => {
                    self.search_query.push(c);
                    self.update_matches();
                }
                _ => {}
            }
            return Ok(None);
        }

        match key.code {
            // Quit/back
            KeyCode::Char('q') | KeyCode::Esc => {
                if self.search_mode == SearchMode::Active {
                    self.clear_search();
                    Ok(None)
                } else {
                    Ok(Some(Action::Back))
                }
            }

            // Toggle view mode
            KeyCode::Tab => {
                self.view_mode = match self.view_mode {
                    ViewMode::Full => ViewMode::Compact,
                    ViewMode::Compact => ViewMode::Full,
                };
                // Reset focus to logs when switching to compact
                if self.view_mode == ViewMode::Compact {
                    self.focus = Focus::Logs;
                }
                Ok(None)
            }

            // Switch focus (only in full mode)
            KeyCode::Left | KeyCode::Right => {
                if self.view_mode == ViewMode::Full {
                    self.focus = match self.focus {
                        Focus::Sidebar => Focus::Logs,
                        Focus::Logs => Focus::Sidebar,
                    };
                }
                Ok(None)
            }

            // Navigation
            KeyCode::Up | KeyCode::Char('k') => {
                if self.focus == Focus::Sidebar && self.view_mode == ViewMode::Full {
                    self.selected_service = self.selected_service.saturating_sub(1);
                    self.sidebar_state.select(Some(self.selected_service));
                } else {
                    self.scroll_up(1);
                }
                Ok(None)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.focus == Focus::Sidebar && self.view_mode == ViewMode::Full {
                    self.selected_service = (self.selected_service + 1).min(self.services.len().saturating_sub(1));
                    self.sidebar_state.select(Some(self.selected_service));
                } else {
                    self.scroll_down(1);
                }
                Ok(None)
            }
            KeyCode::PageUp => {
                self.scroll_up(20);
                Ok(None)
            }
            KeyCode::PageDown => {
                self.scroll_down(20);
                Ok(None)
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.scroll = 0;
                self.following = false;
                Ok(None)
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.scroll_to_bottom();
                Ok(None)
            }

            // Service toggle
            KeyCode::Char(' ') => {
                if self.view_mode == ViewMode::Full {
                    self.toggle_service(self.selected_service);
                }
                Ok(None)
            }
            KeyCode::Char('a') => {
                self.activate_all();
                Ok(None)
            }

            // Search
            KeyCode::Char('/') => {
                self.search_mode = SearchMode::Input;
                self.search_query.clear();
                self.match_set.clear();
                self.match_order.clear();
                Ok(None)
            }
            KeyCode::Char('n') => {
                // n = next match when searching, deactivate all otherwise
                if self.search_mode == SearchMode::Active {
                    self.next_match();
                } else {
                    self.deactivate_all();
                }
                Ok(None)
            }
            KeyCode::Char('N') => {
                if self.search_mode == SearchMode::Active {
                    self.prev_match();
                }
                Ok(None)
            }

            // Follow mode
            KeyCode::Char('f') => {
                if self.following {
                    self.following = false;
                } else {
                    self.scroll_to_bottom();
                }
                Ok(None)
            }

            _ => Ok(None),
        }
    }

    fn update(&mut self, _action: Action) -> Result<Option<Action>> {
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        // Layout depends on view mode and search state
        let has_search_bar = self.search_mode != SearchMode::Off;

        let main_layout = if has_search_bar {
            Layout::vertical([
                Constraint::Length(2), // Header
                Constraint::Min(0),    // Content
                Constraint::Length(1), // Search bar
                Constraint::Length(2), // Footer
            ])
            .split(area)
        } else {
            Layout::vertical([
                Constraint::Length(2), // Header
                Constraint::Min(0),    // Content
                Constraint::Length(2), // Footer
            ])
            .split(area)
        };

        // Header
        let follow_indicator = if self.following {
            Span::styled(" ● LIVE ", Style::default().fg(Color::Green).bold())
        } else {
            Span::styled(" ○ PAUSED ", Style::default().fg(Color::DarkGray))
        };

        let mut header_spans = vec![
            Span::raw(" Logs: ").bold().fg(Color::Cyan),
            Span::raw(&self.node_ip).fg(Color::White),
            Span::raw(format!(" ({})", self.node_role)).dim(),
            follow_indicator,
            Span::raw(format!("[{} active]", self.active_count())).dim(),
        ];

        // Show match count when searching
        if !self.match_order.is_empty() {
            header_spans.push(Span::raw("  "));
            header_spans.push(Span::styled(
                format!("[{}/{}]", self.current_match + 1, self.match_order.len()),
                Style::default().fg(Color::Yellow).bold(),
            ));
        } else if self.search_mode != SearchMode::Off && !self.search_query.is_empty() {
            header_spans.push(Span::raw("  "));
            header_spans.push(Span::styled("[no matches]", Style::default().fg(Color::Red)));
        }

        let header = Paragraph::new(Line::from(header_spans)).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        frame.render_widget(header, main_layout[0]);

        // Content area
        let content_area = main_layout[1];

        match self.view_mode {
            ViewMode::Full => {
                // Split into sidebar and logs
                let content_layout = Layout::horizontal([
                    Constraint::Length(20), // Sidebar
                    Constraint::Min(0),     // Logs
                ])
                .split(content_area);

                self.draw_sidebar(frame, content_layout[0]);
                self.draw_logs(frame, content_layout[1]);
            }
            ViewMode::Compact => {
                self.draw_logs(frame, content_area);
            }
        }

        // Search bar
        if has_search_bar {
            let search_area = main_layout[2];
            let cursor = if self.search_mode == SearchMode::Input { "█" } else { "" };
            let search_line = Line::from(vec![
                Span::styled(" /", Style::default().fg(Color::Yellow)),
                Span::raw(&self.search_query),
                Span::styled(cursor, Style::default().fg(Color::Yellow)),
            ]);
            frame.render_widget(Paragraph::new(search_line), search_area);
        }

        // Footer
        let footer_area = if has_search_bar { main_layout[3] } else { main_layout[2] };

        let footer_spans = if self.search_mode == SearchMode::Input {
            vec![
                Span::raw(" Type to search").dim(),
                Span::raw("  "),
                Span::raw("[Enter]").fg(Color::Yellow),
                Span::raw(" confirm").dim(),
                Span::raw("  "),
                Span::raw("[Esc]").fg(Color::Yellow),
                Span::raw(" cancel").dim(),
            ]
        } else if self.search_mode == SearchMode::Active {
            vec![
                Span::raw(" [n/N]").fg(Color::Yellow),
                Span::raw(" next/prev").dim(),
                Span::raw("  "),
                Span::raw("[/]").fg(Color::Yellow),
                Span::raw(" new search").dim(),
                Span::raw("  "),
                Span::raw("[Esc]").fg(Color::Yellow),
                Span::raw(" clear").dim(),
            ]
        } else if self.view_mode == ViewMode::Full {
            vec![
                Span::raw(" [Space]").fg(Color::Yellow),
                Span::raw(" toggle").dim(),
                Span::raw("  "),
                Span::raw("[a/n]").fg(Color::Yellow),
                Span::raw(" all/none").dim(),
                Span::raw("  "),
                Span::raw("[Tab]").fg(Color::Yellow),
                Span::raw(" compact").dim(),
                Span::raw("  "),
                Span::raw("[/]").fg(Color::Yellow),
                Span::raw(" search").dim(),
                Span::raw("  "),
                Span::raw("[f]").fg(Color::Yellow),
                Span::raw(" follow").dim(),
                Span::raw("  "),
                Span::raw("[q]").fg(Color::Yellow),
                Span::raw(" back").dim(),
            ]
        } else {
            vec![
                Span::raw(" [Tab]").fg(Color::Yellow),
                Span::raw(" full view").dim(),
                Span::raw("  "),
                Span::raw("[/]").fg(Color::Yellow),
                Span::raw(" search").dim(),
                Span::raw("  "),
                Span::raw("[f]").fg(Color::Yellow),
                Span::raw(" follow").dim(),
                Span::raw("  "),
                Span::raw("[↑↓]").fg(Color::Yellow),
                Span::raw(" scroll").dim(),
                Span::raw("  "),
                Span::raw("[q]").fg(Color::Yellow),
                Span::raw(" back").dim(),
            ]
        };

        let footer = Paragraph::new(Line::from(footer_spans)).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        frame.render_widget(footer, footer_area);

        Ok(())
    }
}
