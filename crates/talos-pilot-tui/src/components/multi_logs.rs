//! Multi-service logs component - Stern-style interleaved log viewer

use crate::action::Action;
use crate::components::Component;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};
use std::collections::HashSet;

/// Maximum entries to keep in memory (ring buffer)
/// At ~500 bytes per entry average, 5000 entries ≈ 2.5MB
const MAX_ENTRIES: usize = 5000;

/// Maximum lines to process per tick during streaming
/// Higher = more responsive but could block UI if too high
const MAX_LINES_PER_TICK: usize = 500;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

    fn name(&self) -> &'static str {
        match self {
            LogLevel::Error => "Error",
            LogLevel::Warn => "Warn",
            LogLevel::Info => "Info",
            LogLevel::Debug => "Debug",
            LogLevel::Unknown => "Other",
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

/// Search mode
#[derive(Debug, Clone, PartialEq)]
enum SearchMode {
    Off,
    Input,
    Active,
}

/// Which floating pane is open
#[derive(Debug, Clone, Copy, PartialEq)]
enum FloatingPane {
    None,
    Services,
    Levels,
}

/// Level filter state
#[derive(Debug, Clone)]
struct LevelState {
    level: LogLevel,
    active: bool,
    entry_count: usize,
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

    /// Level filters
    levels: Vec<LevelState>,
    /// Selected level in levels pane
    selected_level: usize,
    /// Levels list state
    levels_state: ListState,

    /// All log entries (sorted by timestamp)
    entries: Vec<MultiLogEntry>,
    /// Filtered entries (indices into entries vec, only active services/levels)
    visible_indices: Vec<usize>,

    /// Which floating pane is open
    floating_pane: FloatingPane,
    /// Scroll position in logs
    scroll: u16,
    /// Last known viewport height (for half-page scroll)
    viewport_height: u16,
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

    /// Talos client for streaming
    client: Option<talos_rs::TalosClient>,
    /// Tail lines setting
    tail_lines: i32,
    /// Whether streaming is active
    streaming: bool,
    /// Channel to receive streamed log lines (service_id, line)
    stream_rx: Option<tokio::sync::mpsc::UnboundedReceiver<(String, String)>>,
    /// Sender for stream aggregator (kept alive to prevent channel close)
    #[allow(dead_code)]
    stream_tx: Option<tokio::sync::mpsc::UnboundedSender<(String, String)>>,
    /// Animation frame for pulsing indicator
    pulse_frame: u8,
    /// Whether to wrap long lines
    wrap: bool,
    /// Visual selection anchor (for V mode) - stores visible_index
    selection_start: Option<usize>,
    /// Current cursor position in visible_indices (always tracked, moves with navigation)
    cursor: usize,
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

        // Initialize level filters (all active by default)
        let levels = vec![
            LevelState { level: LogLevel::Error, active: true, entry_count: 0 },
            LevelState { level: LogLevel::Warn, active: true, entry_count: 0 },
            LevelState { level: LogLevel::Info, active: true, entry_count: 0 },
            LevelState { level: LogLevel::Debug, active: true, entry_count: 0 },
            LevelState { level: LogLevel::Unknown, active: true, entry_count: 0 },
        ];
        let mut levels_state = ListState::default();
        levels_state.select(Some(0));

        Self {
            node_ip,
            node_role,
            services,
            selected_service: 0,
            sidebar_state,
            levels,
            selected_level: 0,
            levels_state,
            entries: Vec::new(),
            visible_indices: Vec::new(),
            floating_pane: FloatingPane::None, // Start with pane closed
            scroll: 0,
            viewport_height: 20, // Will be updated on first draw
            following: true,
            loading: true,
            error: None,
            search_mode: SearchMode::Off,
            search_query: String::new(),
            match_set: HashSet::new(),
            match_order: Vec::new(),
            current_match: 0,
            // Streaming fields
            client: None,
            tail_lines: 500,
            streaming: false,
            stream_rx: None,
            stream_tx: None,
            pulse_frame: 0,
            wrap: false,
            selection_start: None,
            cursor: 0,
        }
    }

    /// Set the Talos client for streaming
    pub fn set_client(&mut self, client: talos_rs::TalosClient, tail_lines: i32) {
        self.client = Some(client);
        self.tail_lines = tail_lines;
    }

    /// Start streaming logs from all active services
    pub fn start_streaming(&mut self) {
        let client = match &self.client {
            Some(c) => c.clone(),
            None => return,
        };

        // Stop any existing streams
        self.stop_streaming();

        // Create aggregated channel
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<(String, String)>();
        self.stream_tx = Some(tx.clone());
        self.stream_rx = Some(rx);
        self.streaming = true;

        // Spawn stream tasks for each active service
        for service in &self.services {
            if !service.active {
                continue;
            }

            let service_id = service.id.clone();
            let client = client.clone();
            let tx = tx.clone();
            let tail_lines = self.tail_lines;

            tokio::spawn(async move {
                match client.logs_stream(&service_id, tail_lines).await {
                    Ok(mut stream_rx) => {
                        while let Some(line) = stream_rx.recv().await {
                            if tx.send((service_id.clone(), line)).is_err() {
                                // Channel closed, stop this stream
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to start log stream for {}: {}", service_id, e);
                    }
                }
            });
        }
    }

    /// Stop all log streams
    pub fn stop_streaming(&mut self) {
        self.streaming = false;
        self.stream_tx = None;
        self.stream_rx = None;
    }

    /// Check if streaming is active
    pub fn is_streaming(&self) -> bool {
        self.streaming
    }

    /// Process incoming streamed log entries (call on tick)
    fn process_stream_entries(&mut self) {
        // First, drain the channel into a local vec to avoid borrow issues
        let raw_lines: Vec<(String, String)> = {
            let rx = match &mut self.stream_rx {
                Some(r) => r,
                None => return,
            };

            let mut lines = Vec::with_capacity(MAX_LINES_PER_TICK);
            let mut count = 0;

            // Drain up to MAX_LINES_PER_TICK entries
            while let Ok(item) = rx.try_recv() {
                lines.push(item);
                count += 1;
                if count >= MAX_LINES_PER_TICK {
                    break;
                }
            }

            // If we hit the limit and there's more, log a warning (channel backing up)
            if count >= MAX_LINES_PER_TICK {
                // Check if there's more in the channel (indicates backlog)
                if rx.try_recv().is_ok() {
                    tracing::debug!("Log stream channel has backlog, may be dropping older entries");
                    // Drain remainder to prevent unbounded growth, but don't process
                    while rx.try_recv().is_ok() {
                        // Discard to prevent memory growth
                    }
                }
            }

            lines
        };

        if raw_lines.is_empty() {
            return;
        }

        // Now parse entries (can access self freely)
        let new_entries: Vec<MultiLogEntry> = raw_lines
            .into_iter()
            .map(|(service_id, line)| {
                let color = self.get_service_color(&service_id);
                Self::parse_line(&line, &service_id, color)
            })
            .collect();

        // Add new entries
        self.entries.extend(new_entries);

        // Re-sort (could optimize with insertion sort for streaming)
        self.entries.sort_by_key(|e| e.timestamp_sort);

        // Enforce max entries (ring buffer) - this is the primary memory bound
        if self.entries.len() > MAX_ENTRIES {
            let excess = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(0..excess);
            // Shrink capacity periodically to release memory
            if self.entries.capacity() > MAX_ENTRIES * 2 {
                self.entries.shrink_to(MAX_ENTRIES + 1000);
            }
        }

        // Update counts
        self.update_counts();

        // Rebuild visible indices
        self.rebuild_visible_indices();

        // Auto-scroll if following
        if self.following {
            self.scroll_to_bottom();
        }
    }

    /// Update service and level counts
    /// Service counts show total entries for each service
    /// Level counts show entries filtered by active services (so you see relevant breakdown)
    fn update_counts(&mut self) {
        // Service counts: total entries for each service (not filtered)
        for service in &mut self.services {
            service.entry_count = self.entries.iter().filter(|e| e.service_id == service.id).count();
        }

        // Level counts: filtered by active services
        let active_services: HashSet<&str> = self.services
            .iter()
            .filter(|s| s.active)
            .map(|s| s.id.as_str())
            .collect();

        for level_state in &mut self.levels {
            level_state.entry_count = self.entries
                .iter()
                .filter(|e| active_services.contains(e.service_id.as_str()) && e.level == level_state.level)
                .count();
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

        // Update counts
        self.update_counts();

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

    /// Extract timestamp from line - handles multiple formats
    fn extract_timestamp(line: &str) -> (String, i64, &str) {
        // Try klog format first: I0109 16:42:01.123456 or W0109 16:42:01.123456
        // Format: [IWEF]MMDD HH:MM:SS.microseconds
        if line.len() > 20 {
            let first_char = line.chars().next().unwrap_or(' ');
            if matches!(first_char, 'I' | 'W' | 'E' | 'F')
                && let Some(space_pos) = line[1..].find(' ')
            {
                let date_part = &line[1..space_pos + 1]; // MMDD
                if date_part.len() == 4 && date_part.chars().all(|c| c.is_ascii_digit()) {
                    // Find the time part after the space
                    let after_date = &line[space_pos + 2..];
                    if let Some(time_end) = after_date.find(|c: char| !c.is_ascii_digit() && c != ':' && c != '.') {
                        let time_part = &after_date[..time_end];
                        if time_part.contains(':') {
                            let rest_start = space_pos + 2 + time_end;
                            let rest = line[rest_start..].trim();
                            let short_ts = Self::extract_time_part(time_part);
                            let sort_key = Self::time_to_sort_key(&short_ts);
                            return (short_ts, sort_key, rest);
                        }
                    }
                }
            }
        }

        // Try JSON format with numeric timestamp: {"ts":1767972610784.5803,...}
        if line.starts_with('{') {
            // Try numeric "ts" field first (Unix timestamp in ms)
            if let Some(ts_start) = line.find("\"ts\":") {
                let ts_value_start = ts_start + 5;
                let rest = &line[ts_value_start..];
                // Find end of number (comma, space, or closing brace)
                if let Some(ts_end) = rest.find([',', '}', ' ']) {
                    let ts_str = rest[..ts_end].trim();
                    // Try to parse as float (ms timestamp)
                    if let Ok(ts_ms) = ts_str.parse::<f64>() {
                        let (short_ts, sort_key) = Self::unix_ts_to_time(ts_ms);
                        return (short_ts, sort_key, line);
                    }
                    // Try as quoted string
                    if let Some(stripped) = ts_str.strip_prefix('"')
                        && let Some(end_quote) = stripped.find('"')
                    {
                        let ts = &stripped[..end_quote];
                        let short_ts = Self::extract_time_part(ts);
                        let sort_key = Self::time_to_sort_key(&short_ts);
                        return (short_ts, sort_key, line);
                    }
                }
            }
            // Try "time" field as string
            if let Some(ts_start) = line.find("\"time\":\"") {
                let ts_value_start = ts_start + 8;
                if let Some(ts_end) = line[ts_value_start..].find('"') {
                    let ts = &line[ts_value_start..ts_value_start + ts_end];
                    let short_ts = Self::extract_time_part(ts);
                    let sort_key = Self::time_to_sort_key(&short_ts);
                    return (short_ts, sort_key, line);
                }
            }
        }

        // Try containerd format: time="2024-01-09T16:42:01.123456789Z" level=info msg="..."
        if let Some(ts_start) = line.find("time=\"") {
            let ts_value_start = ts_start + 6;
            if let Some(ts_end) = line[ts_value_start..].find('"') {
                let ts = &line[ts_value_start..ts_value_start + ts_end];
                let short_ts = Self::extract_time_part(ts);
                let sort_key = Self::time_to_sort_key(&short_ts);
                return (short_ts, sort_key, line);
            }
        }

        // Try ISO 8601 or standard date format at start
        // 2024-01-09T16:42:01 or 2024/01/09 16:42:01
        let chars: Vec<char> = line.chars().collect();
        let mut end = 0;
        let mut has_colon = false;

        for (i, c) in chars.iter().enumerate() {
            if *c == ':' {
                has_colon = true;
            }

            if c.is_ascii_digit() || *c == '/' || *c == '-' || *c == ':' || *c == '.' || *c == 'T' || *c == 'Z' {
                end = i + 1;
            } else if *c == ' ' {
                // Allow space between date and time (2024-01-09 16:42:01)
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
            let short_ts = Self::extract_time_part(ts);
            // Reject 00:00:00 - it's likely Go's zero time from event dumps
            if short_ts == "00:00:00" {
                return (String::new(), 0, line);
            }
            let sort_key = Self::time_to_sort_key(&short_ts);
            (short_ts, sort_key, rest)
        } else {
            // No timestamp found - don't search the whole line as that matches
            // embedded times in log content (Go zero times, SHA hashes, etc.)
            (String::new(), 0, line)
        }
    }

    /// Extract HH:MM:SS from any timestamp format
    /// Only searches within the first portion of the string to avoid matching
    /// time-like patterns in content (e.g., sha256:70... would match as 56:70)
    fn extract_time_part(ts: &str) -> String {
        let bytes = ts.as_bytes();
        // Only search the first 30 chars to avoid matching patterns in log content
        let search_len = bytes.len().min(30);

        // Look for HH:MM:SS pattern (must have two colons)
        for i in 0..search_len.saturating_sub(7) {
            if bytes[i].is_ascii_digit()
                && bytes[i + 1].is_ascii_digit()
                && bytes[i + 2] == b':'
                && bytes[i + 3].is_ascii_digit()
                && bytes[i + 4].is_ascii_digit()
                && bytes[i + 5] == b':'
                && bytes[i + 6].is_ascii_digit()
                && bytes[i + 7].is_ascii_digit()
            {
                // Validate it's a real time (HH < 24, MM < 60, SS < 60)
                let hh = (bytes[i] - b'0') * 10 + (bytes[i + 1] - b'0');
                let mm = (bytes[i + 3] - b'0') * 10 + (bytes[i + 4] - b'0');
                let ss = (bytes[i + 6] - b'0') * 10 + (bytes[i + 7] - b'0');
                if hh < 24 && mm < 60 && ss < 60 {
                    let result = ts[i..i + 8].to_string();
                    // Reject 00:00:00 - likely Go's zero time from event dumps
                    if result == "00:00:00" {
                        continue;
                    }
                    return result;
                }
            }
        }
        // Fallback: look for HH:MM pattern at the start only (first 20 chars)
        let fallback_len = bytes.len().min(20);
        for i in 0..fallback_len.saturating_sub(4) {
            if bytes[i].is_ascii_digit()
                && bytes[i + 1].is_ascii_digit()
                && bytes[i + 2] == b':'
                && bytes[i + 3].is_ascii_digit()
                && bytes[i + 4].is_ascii_digit()
            {
                // Validate it's a real time (HH < 24, MM < 60)
                let hh = (bytes[i] - b'0') * 10 + (bytes[i + 1] - b'0');
                let mm = (bytes[i + 3] - b'0') * 10 + (bytes[i + 4] - b'0');
                if hh < 24 && mm < 60 {
                    let result = ts[i..i + 5].to_string();
                    // Reject 00:00 - likely Go's zero time
                    if result == "00:00" {
                        continue;
                    }
                    return result;
                }
            }
        }
        String::new()
    }

    /// Convert HH:MM:SS to sortable integer (HHMMSS)
    fn time_to_sort_key(time: &str) -> i64 {
        // Extract just digits from HH:MM:SS or HH:MM format
        let digits: String = time.chars().filter(|c| c.is_ascii_digit()).take(6).collect();
        digits.parse().unwrap_or(0)
    }

    /// Convert Unix timestamp to (HH:MM:SS, sort_key)
    /// Auto-detects whether timestamp is in seconds or milliseconds
    fn unix_ts_to_time(ts: f64) -> (String, i64) {
        // Auto-detect: timestamps < 10^12 are likely seconds, >= 10^12 are milliseconds
        // Current Unix time in seconds is ~1.7 billion (2024), in ms it's ~1.7 trillion
        let ts_secs = if ts >= 1_000_000_000_000.0 {
            // Milliseconds
            (ts / 1000.0) as i64
        } else {
            // Seconds (with possible fractional part)
            ts as i64
        };

        let secs_in_day = ts_secs % 86400;
        let hours = secs_in_day / 3600;
        let minutes = (secs_in_day % 3600) / 60;
        let seconds = secs_in_day % 60;
        let short_ts = format!("{:02}:{:02}:{:02}", hours, minutes, seconds);
        let sort_key = hours * 10000 + minutes * 100 + seconds;
        (short_ts, sort_key)
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

        let active_levels: HashSet<LogLevel> = self.levels
            .iter()
            .filter(|l| l.active)
            .map(|l| l.level)
            .collect();

        self.visible_indices = self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                active_services.contains(e.service_id.as_str()) && active_levels.contains(&e.level)
            })
            .map(|(i, _)| i)
            .collect();

        // Clamp scroll to valid range (max is where last entry is at viewport bottom)
        let total = self.visible_indices.len();
        let viewport = self.viewport_height as usize;
        let max_scroll = total.saturating_sub(viewport) as u16;
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }

        // Update search if active
        if self.search_mode == SearchMode::Active {
            self.update_matches();
        }
    }

    /// Toggle service active state
    fn toggle_service(&mut self, index: usize) {
        if let Some(service) = self.services.get_mut(index) {
            service.active = !service.active;
            self.update_counts(); // Level counts depend on active services
            self.rebuild_visible_indices();
        }
    }

    /// Set all services active
    fn activate_all(&mut self) {
        for service in &mut self.services {
            service.active = true;
        }
        self.update_counts();
        self.rebuild_visible_indices();
    }

    /// Set all services inactive
    fn deactivate_all(&mut self) {
        for service in &mut self.services {
            service.active = false;
        }
        self.update_counts();
        self.rebuild_visible_indices();
    }

    /// Count active services
    fn active_count(&self) -> usize {
        self.services.iter().filter(|s| s.active).count()
    }

    /// Toggle level active state
    fn toggle_level(&mut self, index: usize) {
        if let Some(level) = self.levels.get_mut(index) {
            level.active = !level.active;
            self.rebuild_visible_indices();
        }
    }

    /// Set all levels active
    fn activate_all_levels(&mut self) {
        for level in &mut self.levels {
            level.active = true;
        }
        self.rebuild_visible_indices();
    }

    /// Set all levels inactive
    fn deactivate_all_levels(&mut self) {
        for level in &mut self.levels {
            level.active = false;
        }
        self.rebuild_visible_indices();
    }

    /// Count active levels
    fn active_level_count(&self) -> usize {
        self.levels.iter().filter(|l| l.active).count()
    }

    /// Scroll up
    fn scroll_up(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_sub(amount);
        self.following = false;
    }

    /// Scroll down
    fn scroll_down(&mut self, amount: u16) {
        let total = self.visible_indices.len();
        let viewport = self.viewport_height as usize;
        // Max scroll is where last entry is at bottom of viewport
        let max = total.saturating_sub(viewport) as u16;
        self.scroll = (self.scroll + amount).min(max);
    }

    /// Scroll half page up (Ctrl+U)
    fn scroll_half_page_up(&mut self) {
        let half = (self.viewport_height / 2).max(1);
        self.scroll_up(half);
    }

    /// Scroll half page down (Ctrl+D)
    fn scroll_half_page_down(&mut self) {
        let half = (self.viewport_height / 2).max(1);
        self.scroll_down(half);
    }

    /// Scroll to bottom and enable following
    /// Sets scroll so the last entries fill the viewport from the bottom
    /// Also moves cursor to the last entry
    fn scroll_to_bottom(&mut self) {
        let total = self.visible_indices.len();
        let viewport = self.viewport_height as usize;
        // Position scroll so last entry is at bottom of viewport
        self.scroll = total.saturating_sub(viewport) as u16;
        // Move cursor to last entry
        self.cursor = total.saturating_sub(1);
        self.following = true;
    }

    /// Get the entry at the cursor position
    fn current_entry(&self) -> Option<&MultiLogEntry> {
        self.visible_indices.get(self.cursor).and_then(|&i| self.entries.get(i))
    }

    /// Check if visual selection mode is active
    fn in_visual_mode(&self) -> bool {
        self.selection_start.is_some()
    }

    /// Get the selection range (start, end) in visible indices, inclusive
    fn selection_range(&self) -> Option<(usize, usize)> {
        self.selection_start.map(|anchor| {
            (anchor.min(self.cursor), anchor.max(self.cursor))
        })
    }

    /// Check if a visible index is the cursor position
    fn is_cursor(&self, visible_idx: usize) -> bool {
        visible_idx == self.cursor
    }

    /// Check if a visible index is within the selection
    fn is_selected(&self, visible_idx: usize) -> bool {
        if let Some((start, end)) = self.selection_range() {
            visible_idx >= start && visible_idx <= end
        } else {
            false
        }
    }

    /// Format a log entry as a string
    fn format_entry(entry: &MultiLogEntry) -> String {
        let ts = if entry.timestamp.is_empty() {
            "NO TIME"
        } else {
            &entry.timestamp
        };
        format!(
            "{} {} {} {}",
            ts,
            entry.service_id,
            entry.level.badge(),
            entry.message
        )
    }

    /// Yank (copy) selected lines or current line to system clipboard
    fn yank_selection(&self) -> (bool, usize) {
        let lines: Vec<String> = if let Some((start, end)) = self.selection_range() {
            // Yank all selected lines
            (start..=end)
                .filter_map(|vi| {
                    self.visible_indices.get(vi)
                        .and_then(|&i| self.entries.get(i))
                        .map(Self::format_entry)
                })
                .collect()
        } else {
            // Yank current line only
            self.current_entry()
                .map(|e| vec![Self::format_entry(e)])
                .unwrap_or_default()
        };

        if lines.is_empty() {
            return (false, 0);
        }

        let count = lines.len();
        let content = lines.join("\n");

        // Copy to clipboard using helper that handles Linux quirks
        let success = crate::clipboard::copy_to_clipboard(content).is_ok();

        (success, count)
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

    /// Draw the floating services panel
    fn draw_services_panel(&mut self, frame: &mut Frame, area: Rect) {
        // Filter services by search query if searching
        let query_lower = self.search_query.to_lowercase();
        let filtered_services: Vec<(usize, &ServiceState)> = if self.search_mode != SearchMode::Off && !self.search_query.is_empty() {
            self.services
                .iter()
                .enumerate()
                .filter(|(_, s)| s.id.to_lowercase().contains(&query_lower))
                .collect()
        } else {
            self.services.iter().enumerate().collect()
        };

        if filtered_services.is_empty() {
            return; // Don't show panel if no services match
        }

        // Calculate panel size based on filtered service names
        let max_name_len = filtered_services.iter().map(|(_, s)| s.id.len()).max().unwrap_or(8);
        let panel_width = (max_name_len + 4).max(16).min(area.width as usize - 4) as u16;
        let panel_height = (filtered_services.len() + 2).min(area.height as usize - 2) as u16;

        // Position in top-left with small margin
        let panel_area = Rect::new(
            area.x + 1,
            area.y,
            panel_width,
            panel_height,
        );

        // Clear the background
        frame.render_widget(ratatui::widgets::Clear, panel_area);

        let items: Vec<ListItem> = filtered_services
            .iter()
            .map(|(_, s)| {
                let indicator = if s.active { "●" } else { "○" };
                let style = Style::default().fg(s.color);

                ListItem::new(Line::from(vec![
                    Span::styled(indicator, style),
                    Span::raw(" "),
                    Span::styled(&s.id, style),
                ]))
            })
            .collect();

        let border_style = Style::default().fg(Color::Cyan);

        let list = List::new(items)
            .block(
                Block::default()
                    .title("─ Services ")
                    .title_style(border_style)
                    .borders(Borders::ALL)
                    .border_style(border_style),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

        // Adjust selected index if filtering
        let mut adjusted_state = ListState::default();
        if let Some(selected) = self.sidebar_state.selected() {
            // Find the position of the originally selected service in filtered list
            let adjusted_idx = filtered_services
                .iter()
                .position(|(orig_idx, _)| *orig_idx == selected)
                .unwrap_or(0);
            adjusted_state.select(Some(adjusted_idx));
        } else {
            adjusted_state.select(Some(0));
        }

        frame.render_stateful_widget(list, panel_area, &mut adjusted_state);
    }

    /// Draw the levels filter panel
    fn draw_levels_panel(&mut self, frame: &mut Frame, area: Rect) {
        let panel_width = 18u16;
        let panel_height = (self.levels.len() + 2).min(area.height as usize - 2) as u16;

        // Position in top-left with small margin
        let panel_area = Rect::new(
            area.x + 1,
            area.y,
            panel_width,
            panel_height,
        );

        // Clear the background
        frame.render_widget(ratatui::widgets::Clear, panel_area);

        let items: Vec<ListItem> = self.levels
            .iter()
            .map(|l| {
                let indicator = if l.active { "●" } else { "○" };
                let style = Style::default().fg(l.level.color());

                ListItem::new(Line::from(vec![
                    Span::styled(indicator, style),
                    Span::raw(" "),
                    Span::styled(l.level.name(), style),
                    Span::raw(format!(" ({})", l.entry_count)).dim(),
                ]))
            })
            .collect();

        let border_style = Style::default().fg(Color::Cyan);

        let list = List::new(items)
            .block(
                Block::default()
                    .title("─ Levels ")
                    .title_style(border_style)
                    .borders(Borders::ALL)
                    .border_style(border_style),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

        frame.render_stateful_widget(list, panel_area, &mut self.levels_state);
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
            let msg = if self.active_count() == 0 || self.active_level_count() == 0 {
                " No services/levels selected. Press 's' or 'l' to open filters."
            } else {
                " No log entries"
            };
            let empty = Paragraph::new(Line::from(Span::raw(msg).dim()));
            frame.render_widget(empty, area);
            return;
        }

        let visible_height = area.height as usize;
        let content_width = area.width.saturating_sub(1) as usize; // -1 for scrollbar

        // Safety clamp scroll to valid range (max is where last entry is at viewport bottom)
        let total = self.visible_indices.len();
        let max_start = total.saturating_sub(visible_height);
        let start = (self.scroll as usize).min(max_start);
        let end = (start + visible_height).min(total);

        let mut lines: Vec<Line> = Vec::new();

        for (vi, &entry_idx) in self.visible_indices[start..end].iter().enumerate() {
            let visible_idx = start + vi;
            let entry = &self.entries[entry_idx];
            let is_current_match = self.is_current_match(visible_idx);
            let is_match = self.entry_matches(visible_idx);
            let is_selected = self.is_selected(visible_idx);

            let mut spans = Vec::new();
            let is_cursor = self.is_cursor(visible_idx);

            // Line style - highlight cursor and selected lines
            let line_style = if is_cursor {
                Style::default().bg(Color::DarkGray)
            } else if is_selected {
                Style::default().bg(Color::Rgb(60, 20, 60)) // Dark magenta
            } else {
                Style::default()
            };

            // Selection/match/cursor indicator
            if is_selected {
                spans.push(Span::styled("█", Style::default().fg(Color::Magenta)));
            } else if is_current_match {
                spans.push(Span::styled("▶", Style::default().fg(Color::Yellow)));
            } else if is_cursor {
                spans.push(Span::styled(">", Style::default().fg(Color::Cyan)));
            } else {
                spans.push(Span::raw(" "));
            }

            // Timestamp - use lighter color on highlighted lines
            let timestamp_color = if is_cursor || is_selected {
                Color::Gray
            } else {
                Color::DarkGray
            };
            if !entry.timestamp.is_empty() {
                spans.push(Span::styled(
                    format!("{:>8}", entry.timestamp),
                    Style::default().fg(timestamp_color),
                ));
            } else {
                spans.push(Span::styled(
                    " NO TIME",
                    Style::default().fg(Color::DarkGray).dim(),
                ));
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

            if self.wrap && entry.message.len() > available {
                // Wrap mode: split message into multiple lines
                let msg_chars: Vec<char> = entry.message.chars().collect();
                let mut chunk_start = 0;

                while chunk_start < msg_chars.len() {
                    let chunk_end = (chunk_start + available).min(msg_chars.len());
                    let chunk: String = msg_chars[chunk_start..chunk_end].iter().collect();

                    if chunk_start == 0 {
                        // First line: use the prefix spans we already built
                        if is_match && !self.search_query.is_empty() {
                            spans.extend(self.render_message_with_highlight(&chunk, is_current_match));
                        } else {
                            spans.push(Span::raw(chunk));
                        }
                        lines.push(Line::from(spans.clone()).style(line_style));
                        spans.clear();
                    } else {
                        // Continuation lines: indent to align with message start
                        let indent = " ".repeat(prefix_width);
                        let mut cont_spans = vec![Span::raw(indent)];
                        if is_match && !self.search_query.is_empty() {
                            cont_spans.extend(self.render_message_with_highlight(&chunk, is_current_match));
                        } else {
                            cont_spans.push(Span::raw(chunk));
                        }
                        lines.push(Line::from(cont_spans).style(line_style));
                    }
                    chunk_start = chunk_end;
                }
            } else if entry.message.len() <= available {
                if is_match && !self.search_query.is_empty() {
                    spans.extend(self.render_message_with_highlight(&entry.message, is_current_match));
                } else {
                    spans.push(Span::raw(entry.message.clone()));
                }
                lines.push(Line::from(spans).style(line_style));
            } else {
                // Truncate mode
                let truncated: String = entry.message.chars().take(available.saturating_sub(1)).collect();
                if is_match && !self.search_query.is_empty() {
                    spans.extend(self.render_message_with_highlight(&truncated, is_current_match));
                } else {
                    spans.push(Span::raw(truncated));
                }
                spans.push(Span::raw("…").dim());
                lines.push(Line::from(spans).style(line_style));
            }
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
            KeyCode::Char('q') => {
                Ok(Some(Action::Back))
            }
            KeyCode::Esc => {
                if self.in_visual_mode() {
                    self.selection_start = None;
                } else if self.search_mode == SearchMode::Active {
                    self.clear_search();
                } else if self.floating_pane != FloatingPane::None {
                    self.floating_pane = FloatingPane::None;
                } else {
                    return Ok(Some(Action::Back));
                }
                Ok(None)
            }

            // Toggle between floating panes
            KeyCode::Tab => {
                self.floating_pane = match self.floating_pane {
                    FloatingPane::Services => FloatingPane::Levels,
                    FloatingPane::Levels => FloatingPane::Services,
                    FloatingPane::None => FloatingPane::Services,
                };
                Ok(None)
            }

            // Open specific panes
            KeyCode::Char('s') => {
                self.floating_pane = if self.floating_pane == FloatingPane::Services {
                    FloatingPane::None
                } else {
                    FloatingPane::Services
                };
                Ok(None)
            }
            KeyCode::Char('l') if self.search_mode != SearchMode::Active => {
                self.floating_pane = if self.floating_pane == FloatingPane::Levels {
                    FloatingPane::None
                } else {
                    FloatingPane::Levels
                };
                Ok(None)
            }

            // Navigation - when a pane is open, navigate the pane; otherwise move cursor
            KeyCode::Up | KeyCode::Char('k') => {
                match self.floating_pane {
                    FloatingPane::Services => {
                        self.selected_service = self.selected_service.saturating_sub(1);
                        self.sidebar_state.select(Some(self.selected_service));
                    }
                    FloatingPane::Levels => {
                        self.selected_level = self.selected_level.saturating_sub(1);
                        self.levels_state.select(Some(self.selected_level));
                    }
                    FloatingPane::None => {
                        // Always move cursor up
                        self.cursor = self.cursor.saturating_sub(1);
                        // Scroll viewport if cursor goes above visible area
                        if self.cursor < self.scroll as usize {
                            self.scroll = self.cursor as u16;
                        }
                        self.following = false;
                    }
                }
                Ok(None)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                match self.floating_pane {
                    FloatingPane::Services => {
                        self.selected_service = (self.selected_service + 1).min(self.services.len().saturating_sub(1));
                        self.sidebar_state.select(Some(self.selected_service));
                    }
                    FloatingPane::Levels => {
                        self.selected_level = (self.selected_level + 1).min(self.levels.len().saturating_sub(1));
                        self.levels_state.select(Some(self.selected_level));
                    }
                    FloatingPane::None => {
                        // Always move cursor down
                        let max_idx = self.visible_indices.len().saturating_sub(1);
                        self.cursor = (self.cursor + 1).min(max_idx);
                        // Scroll viewport if cursor goes below visible area
                        let viewport = self.viewport_height as usize;
                        let visible_end = self.scroll as usize + viewport;
                        if self.cursor >= visible_end {
                            self.scroll = (self.cursor + 1).saturating_sub(viewport) as u16;
                        }
                        self.following = false;
                    }
                }
                Ok(None)
            }
            KeyCode::PageUp => {
                // Always move cursor
                self.cursor = self.cursor.saturating_sub(20);
                if self.cursor < self.scroll as usize {
                    self.scroll = self.cursor as u16;
                }
                self.following = false;
                Ok(None)
            }
            KeyCode::PageDown => {
                // Always move cursor
                let max_idx = self.visible_indices.len().saturating_sub(1);
                self.cursor = (self.cursor + 20).min(max_idx);
                let viewport = self.viewport_height as usize;
                let visible_end = self.scroll as usize + viewport;
                if self.cursor >= visible_end {
                    self.scroll = (self.cursor + 1).saturating_sub(viewport) as u16;
                }
                self.following = false;
                Ok(None)
            }

            // Vim-style half-page scroll
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let half = (self.viewport_height / 2).max(1) as usize;
                // Always move cursor up by half page
                self.cursor = self.cursor.saturating_sub(half);
                // Scroll to keep cursor visible
                if self.cursor < self.scroll as usize {
                    self.scroll = self.cursor as u16;
                }
                self.following = false;
                Ok(None)
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let half = (self.viewport_height / 2).max(1) as usize;
                // Always move cursor down by half page
                let max_idx = self.visible_indices.len().saturating_sub(1);
                self.cursor = (self.cursor + half).min(max_idx);
                // Scroll to keep cursor visible
                let viewport = self.viewport_height as usize;
                let visible_end = self.scroll as usize + viewport;
                if self.cursor >= visible_end {
                    self.scroll = (self.cursor + 1).saturating_sub(viewport) as u16;
                }
                self.following = false;
                Ok(None)
            }
            KeyCode::Home | KeyCode::Char('g') => {
                // Always move cursor to top
                self.cursor = 0;
                self.scroll = 0;
                self.following = false;
                Ok(None)
            }
            KeyCode::Char('G') => {
                // Always move cursor to bottom
                let max_idx = self.visible_indices.len().saturating_sub(1);
                self.cursor = max_idx;
                let viewport = self.viewport_height as usize;
                self.scroll = self.visible_indices.len().saturating_sub(viewport) as u16;
                self.following = true;
                Ok(None)
            }

            // Toggle in current pane
            KeyCode::Char(' ') => {
                match self.floating_pane {
                    FloatingPane::Services => self.toggle_service(self.selected_service),
                    FloatingPane::Levels => self.toggle_level(self.selected_level),
                    FloatingPane::None => {}
                }
                Ok(None)
            }
            KeyCode::Char('a') => {
                match self.floating_pane {
                    FloatingPane::Services => self.activate_all(),
                    FloatingPane::Levels => self.activate_all_levels(),
                    FloatingPane::None => {}
                }
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
                if self.search_mode == SearchMode::Active {
                    self.next_match();
                } else {
                    // n = none (deactivate all) when a pane is open
                    match self.floating_pane {
                        FloatingPane::Services => self.deactivate_all(),
                        FloatingPane::Levels => self.deactivate_all_levels(),
                        FloatingPane::None => {}
                    }
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

            // Toggle streaming (Shift+F)
            KeyCode::Char('F') => {
                if self.streaming {
                    self.stop_streaming();
                } else {
                    self.start_streaming();
                }
                Ok(None)
            }

            // Toggle line wrapping
            KeyCode::Char('w') => {
                self.wrap = !self.wrap;
                Ok(None)
            }

            // Visual line selection mode
            KeyCode::Char('V') => {
                if self.in_visual_mode() {
                    // Toggle off
                    self.selection_start = None;
                } else {
                    // Start selection at current cursor position
                    self.selection_start = Some(self.cursor);
                }
                Ok(None)
            }

            // Yank selection or current line to clipboard
            KeyCode::Char('y') => {
                let (success, count) = self.yank_selection();
                if success {
                    tracing::info!("Copied {} log entries to clipboard", count);
                }
                // Clear visual selection after yank
                self.selection_start = None;
                Ok(None)
            }

            _ => Ok(None),
        }
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if let Action::Tick = action {
            // Process any incoming streamed log entries
            if self.streaming {
                self.process_stream_entries();
                // Animate pulse indicator (cycles through 0-7)
                self.pulse_frame = (self.pulse_frame + 1) % 8;
            }
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        // Layout: Header, Content (logs with floating services), optional search, footer
        let has_search_bar = self.search_mode != SearchMode::Off;

        let main_layout = if has_search_bar {
            Layout::vertical([
                Constraint::Length(2), // Header
                Constraint::Min(0),    // Content (logs + floating services)
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

        // Header with title matching mockup
        let follow_indicator = if self.following && self.streaming {
            // Pulsing LIVE indicator when streaming
            let pulse_color = match self.pulse_frame {
                0 | 4 => Color::Green,
                1 | 3 | 5 | 7 => Color::LightGreen,
                2 | 6 => Color::Rgb(100, 255, 100), // Brighter green
                _ => Color::Green,
            };
            Span::styled("● LIVE ", Style::default().fg(pulse_color).bold())
        } else if self.following {
            Span::styled("● LIVE ", Style::default().fg(Color::Green).bold())
        } else {
            Span::styled("○ PAUSED ", Style::default().fg(Color::DarkGray))
        };

        let mut header_spans = vec![
            Span::raw(" Multi-Service Logs: ").bold().fg(Color::Cyan),
            Span::raw(&self.node_ip).fg(Color::White),
            Span::raw(format!(" ({})", self.node_role)).dim(),
            Span::raw("  "),
            follow_indicator,
            Span::raw(format!(" [{}/{} svcs, {}/{} lvls]",
                self.active_count(), self.services.len(),
                self.active_level_count(), self.levels.len())).dim(),
        ];

        // Show match count when searching
        if !self.match_order.is_empty() {
            header_spans.push(Span::raw("  "));
            header_spans.push(Span::styled(
                format!("[{} matches]", self.match_order.len()),
                Style::default().fg(Color::Yellow).bold(),
            ));
        } else if self.search_mode != SearchMode::Off && !self.search_query.is_empty() {
            header_spans.push(Span::raw("  "));
            header_spans.push(Span::styled("[no matches]", Style::default().fg(Color::Red)));
        }

        // Show visual selection indicator
        if let Some((start, end)) = self.selection_range() {
            let count = end - start + 1;
            header_spans.push(Span::raw("  "));
            header_spans.push(Span::styled(
                format!("-- VISUAL ({} lines) --", count),
                Style::default().fg(Color::Magenta).bold(),
            ));
        }

        let header = Paragraph::new(Line::from(header_spans)).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        frame.render_widget(header, main_layout[0]);

        // Content area - logs, with floating services panel on top in Full mode
        let content_area = main_layout[1];

        // Update viewport height for half-page scrolling
        self.viewport_height = content_area.height;

        // Draw logs (no border)
        self.draw_logs(frame, content_area);

        // Draw floating panel on top based on which pane is open
        match self.floating_pane {
            FloatingPane::Services => self.draw_services_panel(frame, content_area),
            FloatingPane::Levels => self.draw_levels_panel(frame, content_area),
            FloatingPane::None => {}
        }

        // Search bar
        if has_search_bar {
            let search_area = main_layout[2];
            let cursor = if self.search_mode == SearchMode::Input { "█" } else { "" };

            // Match count on the right
            let match_info = if !self.match_order.is_empty() {
                format!("[{}/{}]", self.current_match + 1, self.match_order.len())
            } else {
                String::new()
            };

            let search_line = Line::from(vec![
                Span::styled(" /", Style::default().fg(Color::Yellow)),
                Span::raw(&self.search_query),
                Span::styled(cursor, Style::default().fg(Color::Yellow)),
            ]);
            frame.render_widget(Paragraph::new(search_line), search_area);

            // Render match count on the right side
            if !match_info.is_empty() {
                let match_para = Paragraph::new(Line::from(vec![
                    Span::styled(&match_info, Style::default().fg(Color::Yellow)),
                    Span::raw(" "),
                ])).alignment(ratatui::layout::Alignment::Right);
                frame.render_widget(match_para, search_area);
            }
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
        } else if self.floating_pane != FloatingPane::None {
            vec![
                Span::raw(" [Space]").fg(Color::Yellow),
                Span::raw(" toggle").dim(),
                Span::raw("  "),
                Span::raw("[a]").fg(Color::Yellow),
                Span::raw(" all").dim(),
                Span::raw("  "),
                Span::raw("[n]").fg(Color::Yellow),
                Span::raw(" none").dim(),
                Span::raw("  "),
                Span::raw("[Tab]").fg(Color::Yellow),
                Span::raw(" switch").dim(),
                Span::raw("  "),
                Span::raw("[Esc]").fg(Color::Yellow),
                Span::raw(" close").dim(),
                Span::raw("  "),
                Span::raw("[q]").fg(Color::Yellow),
                Span::raw(" back").dim(),
            ]
        } else if self.in_visual_mode() {
            // Visual mode footer
            vec![
                Span::raw(" [j/k]").fg(Color::Yellow),
                Span::raw(" extend").dim(),
                Span::raw(" "),
                Span::raw("[y]").fg(Color::Yellow),
                Span::raw(" yank").dim(),
                Span::raw(" "),
                Span::raw("[Esc/V]").fg(Color::Yellow),
                Span::raw(" cancel").dim(),
            ]
        } else {
            let stream_text = if self.streaming { " stop" } else { " stream" };
            let wrap_text = if self.wrap { " nowrap" } else { " wrap" };
            vec![
                Span::raw(" [s]").fg(Color::Yellow),
                Span::raw(" svcs").dim(),
                Span::raw(" "),
                Span::raw("[l]").fg(Color::Yellow),
                Span::raw(" lvls").dim(),
                Span::raw(" "),
                Span::raw("[/]").fg(Color::Yellow),
                Span::raw(" search").dim(),
                Span::raw(" "),
                Span::raw("[f]").fg(Color::Yellow),
                Span::raw(" follow").dim(),
                Span::raw(" "),
                Span::raw("[F]").fg(Color::Yellow),
                Span::raw(stream_text).dim(),
                Span::raw(" "),
                Span::raw("[w]").fg(Color::Yellow),
                Span::raw(wrap_text).dim(),
                Span::raw(" "),
                Span::raw("[V]").fg(Color::Yellow),
                Span::raw(" visual").dim(),
                Span::raw(" "),
                Span::raw("[y]").fg(Color::Yellow),
                Span::raw(" yank").dim(),
                Span::raw(" "),
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
