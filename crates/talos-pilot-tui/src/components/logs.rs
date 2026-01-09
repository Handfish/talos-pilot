//! Logs component - displays service logs with visual hierarchy and search

use crate::action::Action;
use crate::components::Component;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent};
use std::collections::HashSet;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};

/// Parsed log entry with visual components
#[derive(Debug, Clone)]
struct LogEntry {
    timestamp: String,
    level: LogLevel,
    message: String,
    /// Pre-computed lowercase of full line for efficient searching
    search_text: String,
}

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
            LogLevel::Error => " ERR ",
            LogLevel::Warn => " WRN ",
            LogLevel::Info => " INF ",
            LogLevel::Debug => " DBG ",
            LogLevel::Unknown => " --- ",
        }
    }
}

/// Search mode state
#[derive(Debug, Clone, PartialEq)]
enum SearchMode {
    /// Not searching
    Off,
    /// Typing search query
    Input,
    /// Navigating matches
    Active,
}

/// Component for displaying service logs
pub struct LogsComponent {
    /// Service ID being viewed
    service_id: String,
    /// Parsed log entries
    entries: Vec<LogEntry>,
    /// Current scroll position (in entries)
    scroll: u16,
    /// Whether logs are loading
    loading: bool,
    /// Error message if any
    error: Option<String>,
    /// Search mode
    search_mode: SearchMode,
    /// Current search query
    search_query: String,
    /// Set of entry indices that match (O(1) lookup for rendering)
    match_set: HashSet<usize>,
    /// Ordered list of matches for n/N navigation
    match_order: Vec<usize>,
    /// Current match index (into match_order vec)
    current_match: usize,
}

impl LogsComponent {
    pub fn new(service_id: String) -> Self {
        Self {
            service_id,
            entries: Vec::new(),
            scroll: 0,
            loading: true,
            error: None,
            search_mode: SearchMode::Off,
            search_query: String::new(),
            match_set: HashSet::new(),
            match_order: Vec::new(),
            current_match: 0,
        }
    }

    /// Set the log content - parses into structured entries
    pub fn set_logs(&mut self, content: String) {
        self.entries = content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(Self::parse_line)
            .collect();
        self.loading = false;
        // Scroll to bottom by default
        self.scroll = self.entries.len().saturating_sub(1) as u16;
    }

    /// Parse a log line into structured components
    fn parse_line(line: &str) -> LogEntry {
        let line = line.trim();
        // Pre-compute lowercase for O(1) search (no allocation during search)
        let search_text = line.to_lowercase();

        // Try to extract timestamp (looks for time-like pattern at start)
        let (timestamp, rest) = Self::extract_timestamp(line);

        // Detect level from the remaining text
        let level = LogLevel::from_str(rest);

        // Clean up the message - remove redundant level indicators
        let message = Self::clean_message(rest);

        LogEntry {
            timestamp,
            level,
            message,
            search_text,
        }
    }

    /// Extract timestamp from start of line
    fn extract_timestamp(line: &str) -> (String, &str) {
        // Look for patterns like:
        // "2026/01/09 16:40:59.776940 ..."
        // "2026-01-09T16:40:59 ..."
        // "16:40:59 ..."

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
                // Space could be part of timestamp (between date and time) or end of it
                // Check if next char continues the timestamp pattern
                if let Some(next) = chars.get(i + 1)
                    && next.is_ascii_digit()
                {
                    end = i + 1;
                    continue;
                }
                // End of timestamp
                break;
            } else {
                // Non-timestamp character
                break;
            }
        }

        // Need at least a time pattern (HH:MM:SS = 8 chars with colons)
        if end >= 8 && has_colon && end < line.len() {
            let ts = line[..end].trim();
            let rest = line[end..].trim();
            let short_ts = Self::shorten_timestamp(ts);
            (short_ts, rest)
        } else {
            (String::new(), line)
        }
    }

    /// Shorten timestamp to just HH:MM:SS
    fn shorten_timestamp(ts: &str) -> String {
        // Find the time portion - look for HH:MM:SS pattern
        // Could be after a date like "2026/01/09 " or "2026-01-09T"

        // Find first occurrence of NN:NN pattern
        let bytes = ts.as_bytes();
        for i in 0..bytes.len().saturating_sub(4) {
            if bytes[i].is_ascii_digit()
                && bytes[i + 1].is_ascii_digit()
                && bytes[i + 2] == b':'
                && bytes[i + 3].is_ascii_digit()
                && bytes[i + 4].is_ascii_digit()
            {
                // Found start of time, extract HH:MM:SS
                let time_start = i;
                let time_end = (time_start + 8).min(ts.len());
                return ts[time_start..time_end].to_string();
            }
        }

        // No time pattern found, return as-is (truncated)
        if ts.len() > 8 {
            ts[..8].to_string()
        } else {
            ts.to_string()
        }
    }

    /// Clean up message text
    fn clean_message(text: &str) -> String {
        let text = text.trim();
        // Remove common prefixes like "log.go:94:" or "[INFO]"
        let text = if let Some(pos) = text.find(": ") {
            if pos < 20 {
                text[pos + 2..].trim()
            } else {
                text
            }
        } else {
            text
        };
        // Remove [LEVEL] prefix
        let text = text
            .trim_start_matches("[INFO]")
            .trim_start_matches("[WARN]")
            .trim_start_matches("[ERROR]")
            .trim_start_matches("[DEBUG]")
            .trim_start_matches("INFO")
            .trim_start_matches("WARN")
            .trim_start_matches("ERROR")
            .trim_start_matches("DEBUG")
            .trim_start_matches("OK")
            .trim();
        text.to_string()
    }

    /// Set an error message
    pub fn set_error(&mut self, error: String) {
        self.error = Some(error);
        self.loading = false;
    }

    /// Scroll up
    fn scroll_up(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_sub(amount);
    }

    /// Scroll down
    fn scroll_down(&mut self, amount: u16, max: u16) {
        self.scroll = (self.scroll + amount).min(max);
    }

    /// Update search matches based on current query
    fn update_matches(&mut self) {
        self.match_set.clear();
        self.match_order.clear();
        self.current_match = 0;

        if self.search_query.is_empty() {
            return;
        }

        let query_lower = self.search_query.to_lowercase();
        for (i, entry) in self.entries.iter().enumerate() {
            // Use pre-computed lowercase - no allocation per entry!
            if entry.search_text.contains(&query_lower) {
                self.match_set.insert(i);
                self.match_order.push(i);
            }
        }

        // Jump to first match
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
    }

    /// Clear search
    fn clear_search(&mut self) {
        self.search_mode = SearchMode::Off;
        self.search_query.clear();
        self.match_set.clear();
        self.match_order.clear();
        self.current_match = 0;
    }

    /// Check if an entry index is the current match
    fn is_current_match(&self, entry_idx: usize) -> bool {
        if self.match_order.is_empty() {
            return false;
        }
        self.match_order.get(self.current_match) == Some(&entry_idx)
    }

    /// Check if an entry matches the search - O(1) with HashSet
    fn entry_matches(&self, entry_idx: usize) -> bool {
        self.match_set.contains(&entry_idx)
    }

    /// Render message with search highlighting (returns owned spans)
    fn render_message_with_highlight(
        &self,
        message: &str,
        is_current: bool,
    ) -> Vec<Span<'static>> {
        if self.search_query.is_empty() {
            return vec![Span::raw(message.to_string())];
        }

        let query_lower = self.search_query.to_lowercase();
        let message_lower = message.to_lowercase();
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut last_end = 0;

        // Find all occurrences and highlight them
        for (start, _) in message_lower.match_indices(&query_lower) {
            // Add text before match
            if start > last_end {
                spans.push(Span::raw(message[last_end..start].to_string()));
            }
            // Add highlighted match
            let end = start + self.search_query.len();
            let style = if is_current {
                Style::default().bg(Color::Yellow).fg(Color::Black).bold()
            } else {
                Style::default().bg(Color::DarkGray).fg(Color::White)
            };
            spans.push(Span::styled(message[start..end].to_string(), style));
            last_end = end;
        }

        // Add remaining text
        if last_end < message.len() {
            spans.push(Span::raw(message[last_end..].to_string()));
        }

        if spans.is_empty() {
            vec![Span::raw(message.to_string())]
        } else {
            spans
        }
    }
}

impl Component for LogsComponent {
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        let max_scroll = self.entries.len().saturating_sub(1) as u16;

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

        // Normal mode or active search mode
        match key.code {
            KeyCode::Char('q') => {
                if self.search_mode == SearchMode::Active {
                    self.clear_search();
                    Ok(None)
                } else {
                    Ok(Some(Action::Back))
                }
            }
            KeyCode::Esc => {
                if self.search_mode == SearchMode::Active {
                    self.clear_search();
                    Ok(None)
                } else {
                    Ok(Some(Action::Back))
                }
            }
            KeyCode::Char('/') => {
                self.search_mode = SearchMode::Input;
                self.search_query.clear();
                self.match_set.clear();
                self.match_order.clear();
                Ok(None)
            }
            KeyCode::Char('n') => {
                if self.search_mode == SearchMode::Active {
                    self.next_match();
                }
                Ok(None)
            }
            KeyCode::Char('N') => {
                if self.search_mode == SearchMode::Active {
                    self.prev_match();
                }
                Ok(None)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_up(1);
                Ok(None)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_down(1, max_scroll);
                Ok(None)
            }
            KeyCode::PageUp => {
                self.scroll_up(20);
                Ok(None)
            }
            KeyCode::PageDown => {
                self.scroll_down(20, max_scroll);
                Ok(None)
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.scroll = 0;
                Ok(None)
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.scroll = max_scroll;
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn update(&mut self, _action: Action) -> Result<Option<Action>> {
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        // Adjust layout based on search mode
        let has_search_bar = self.search_mode != SearchMode::Off;
        let layout = if has_search_bar {
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

        // Header with entry count
        let mut header_spans = vec![
            Span::raw(" Logs: ").bold().fg(Color::Cyan),
            Span::raw(&self.service_id).fg(Color::White),
            Span::raw(format!("  ({} entries)", self.entries.len())).dim(),
        ];

        // Show match count in header when searching
        if !self.match_order.is_empty() {
            header_spans.push(Span::raw("  "));
            header_spans.push(Span::styled(
                format!("[{}/{}]", self.current_match + 1, self.match_order.len()),
                Style::default().fg(Color::Yellow).bold(),
            ));
        } else if self.search_mode != SearchMode::Off && !self.search_query.is_empty() {
            header_spans.push(Span::raw("  "));
            header_spans.push(Span::styled(
                "[no matches]",
                Style::default().fg(Color::Red),
            ));
        }

        let header = Paragraph::new(Line::from(header_spans))
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(Color::DarkGray)),
            );
        frame.render_widget(header, layout[0]);

        // Content area
        let content_area = layout[1];
        let visible_height = content_area.height as usize;
        let content_width = content_area.width.saturating_sub(2) as usize;

        if self.loading {
            let loading = Paragraph::new(Line::from(Span::raw(" Loading logs...").dim()));
            frame.render_widget(loading, content_area);
        } else if let Some(error) = &self.error {
            let error_msg = Paragraph::new(vec![
                Line::from(vec![Span::raw(" Error: ").fg(Color::Red).bold()]),
                Line::from(vec![
                    Span::raw(" ").dim(),
                    Span::raw(error).fg(Color::White),
                ]),
            ]);
            frame.render_widget(error_msg, content_area);
        } else if self.entries.is_empty() {
            let empty = Paragraph::new(Line::from(Span::raw(" No log entries").dim()));
            frame.render_widget(empty, content_area);
        } else {
            // Build lines with visual hierarchy
            let mut lines: Vec<Line> = Vec::new();

            // Calculate visible range
            let start = self.scroll as usize;
            let end = (start + visible_height).min(self.entries.len());

            for (idx, entry) in self.entries[start..end].iter().enumerate() {
                let entry_idx = start + idx;
                let is_current_match = self.is_current_match(entry_idx);
                let is_match = self.entry_matches(entry_idx);

                let mut spans = Vec::new();

                // Line indicator for current match
                if is_current_match {
                    spans.push(Span::styled("▶", Style::default().fg(Color::Yellow)));
                } else {
                    spans.push(Span::raw(" "));
                }

                // Timestamp (dim, fixed width)
                if !entry.timestamp.is_empty() {
                    spans.push(Span::styled(
                        format!("{:>8}", entry.timestamp),
                        Style::default().fg(Color::DarkGray),
                    ));
                    spans.push(Span::raw(" "));
                } else {
                    spans.push(Span::raw("         ")); // 9 chars placeholder
                }

                // Level badge (colored background)
                let level_style = Style::default()
                    .fg(Color::Black)
                    .bg(entry.level.color())
                    .add_modifier(Modifier::BOLD);
                spans.push(Span::styled(entry.level.badge(), level_style));
                spans.push(Span::raw(" "));

                // Message with optional highlight
                let message_start_col = 1 + 9 + 5 + 2; // indicator + timestamp + badge + spaces
                let available_width = content_width.saturating_sub(message_start_col);

                if entry.message.len() <= available_width {
                    // Fits on one line
                    if is_match && !self.search_query.is_empty() {
                        spans.extend(self.render_message_with_highlight(&entry.message, is_current_match));
                    } else {
                        spans.push(Span::raw(entry.message.clone()));
                    }
                    lines.push(Line::from(spans));
                } else {
                    // Needs wrapping - first line
                    let first_part: String = entry.message.chars().take(available_width).collect();
                    if is_match && !self.search_query.is_empty() {
                        spans.extend(self.render_message_with_highlight(&first_part, is_current_match));
                    } else {
                        spans.push(Span::raw(first_part));
                    }
                    lines.push(Line::from(spans));

                    // Continuation lines with indent
                    let indent = "                    "; // 20 chars to align with message
                    let remaining: String = entry.message.chars().skip(available_width).collect();

                    for chunk in remaining.as_bytes().chunks(available_width.max(1)) {
                        if let Ok(chunk_str) = std::str::from_utf8(chunk) {
                            let mut cont_spans = vec![Span::raw(indent)];
                            if is_match && !self.search_query.is_empty() {
                                cont_spans.extend(self.render_message_with_highlight(chunk_str, is_current_match));
                            } else {
                                cont_spans.push(Span::styled(
                                    chunk_str.to_string(),
                                    Style::default().fg(Color::Gray),
                                ));
                            }
                            lines.push(Line::from(cont_spans));
                        }
                    }
                }
            }

            let logs = Paragraph::new(lines);
            frame.render_widget(logs, content_area);

            // Scrollbar
            if self.entries.len() > visible_height {
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(Some("▲"))
                    .end_symbol(Some("▼"))
                    .track_symbol(Some("│"))
                    .thumb_symbol("█");
                let mut scrollbar_state = ScrollbarState::new(self.entries.len())
                    .position(self.scroll as usize)
                    .viewport_content_length(visible_height);
                frame.render_stateful_widget(scrollbar, content_area, &mut scrollbar_state);
            }
        }

        // Search bar (if active)
        if has_search_bar {
            let search_area = layout[2];
            let cursor_char = if self.search_mode == SearchMode::Input { "█" } else { "" };
            let search_line = Line::from(vec![
                Span::styled(" /", Style::default().fg(Color::Yellow)),
                Span::raw(&self.search_query),
                Span::styled(cursor_char, Style::default().fg(Color::Yellow)),
            ]);
            frame.render_widget(Paragraph::new(search_line), search_area);
        }

        // Footer
        let footer_area = if has_search_bar { layout[3] } else { layout[2] };

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
                Span::raw(" [n]").fg(Color::Yellow),
                Span::raw(" next").dim(),
                Span::raw("  "),
                Span::raw("[N]").fg(Color::Yellow),
                Span::raw(" prev").dim(),
                Span::raw("  "),
                Span::raw("[/]").fg(Color::Yellow),
                Span::raw(" new search").dim(),
                Span::raw("  "),
                Span::raw("[q/Esc]").fg(Color::Yellow),
                Span::raw(" clear").dim(),
            ]
        } else {
            vec![
                Span::raw(" [q]").fg(Color::Yellow),
                Span::raw(" back").dim(),
                Span::raw("  "),
                Span::raw("[/]").fg(Color::Yellow),
                Span::raw(" search").dim(),
                Span::raw("  "),
                Span::raw("[↑↓]").fg(Color::Yellow),
                Span::raw(" scroll").dim(),
                Span::raw("  "),
                Span::raw("[g/G]").fg(Color::Yellow),
                Span::raw(" top/end").dim(),
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
