//! Processes component - displays running processes on a node
//!
//! "Show me what's wrong in 5 seconds"

use crate::action::Action;
use crate::components::Component;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
    Frame,
};
use std::time::Instant;
use talos_rs::{ProcessInfo, ProcessState, TalosClient};

/// Auto-refresh interval in seconds
const AUTO_REFRESH_INTERVAL_SECS: u64 = 5;

/// Sort order for process list
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortBy {
    #[default]
    Cpu,
    Mem,
}

impl SortBy {
    pub fn label(&self) -> &'static str {
        match self {
            SortBy::Cpu => "CPU",
            SortBy::Mem => "MEM",
        }
    }
}

/// Component mode
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    Filtering,
}

/// State counts for summary bar
#[derive(Debug, Clone, Default)]
struct StateCounts {
    running: usize,
    sleeping: usize,
    disk_wait: usize,
    zombie: usize,
}

/// Display entry for tree view
#[derive(Debug, Clone)]
struct DisplayEntry {
    /// Index into processes vec
    process_idx: usize,
    /// Depth in tree (0 = root)
    depth: usize,
    /// Whether this is the last child at its level
    is_last: bool,
    /// Ancestry flags for drawing connectors (true = has more siblings below)
    ancestors_have_siblings: Vec<bool>,
}

/// Processes component for viewing node processes
pub struct ProcessesComponent {
    /// Node hostname
    hostname: String,
    /// Node address
    address: String,

    /// All processes from the node
    processes: Vec<ProcessInfo>,
    /// Filtered process indices (into processes vec)
    filtered_indices: Vec<usize>,
    /// Display entries for tree view (with depth and connector info)
    display_entries: Vec<DisplayEntry>,

    /// Selected index in filtered list
    selected: usize,
    /// Table state for rendering
    table_state: TableState,
    /// Current sort order
    sort_by: SortBy,
    /// Tree view enabled
    tree_view: bool,

    /// Current mode
    mode: Mode,
    /// Filter input text
    filter_input: String,
    /// Active filter (applied)
    filter: Option<String>,

    /// State counts for summary
    state_counts: StateCounts,
    /// Total memory on node (for percentage calc)
    total_memory: u64,
    /// Memory used on node
    memory_used: u64,
    /// Memory usage percentage (from system, not process sum)
    memory_usage_percent: f32,

    /// Loading state
    loading: bool,
    /// Error message
    error: Option<String>,

    /// Auto-refresh enabled
    auto_refresh: bool,
    /// Last refresh time
    last_refresh: Option<Instant>,

    /// Client for API calls
    client: Option<TalosClient>,
}

impl Default for ProcessesComponent {
    fn default() -> Self {
        Self::new("".to_string(), "".to_string())
    }
}

impl ProcessesComponent {
    pub fn new(hostname: String, address: String) -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));

        Self {
            hostname,
            address,
            processes: Vec::new(),
            filtered_indices: Vec::new(),
            display_entries: Vec::new(),
            selected: 0,
            table_state,
            sort_by: SortBy::Cpu,
            tree_view: false,
            mode: Mode::Normal,
            filter_input: String::new(),
            filter: None,
            state_counts: StateCounts::default(),
            total_memory: 0,
            memory_used: 0,
            memory_usage_percent: 0.0,
            loading: true,
            error: None,
            auto_refresh: true,
            last_refresh: None,
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

    /// Refresh process data from the node
    pub async fn refresh(&mut self) -> Result<()> {
        let Some(client) = &self.client else {
            self.set_error("No client configured".to_string());
            return Ok(());
        };

        self.loading = true;

        // Fetch processes and memory info in parallel
        let timeout = std::time::Duration::from_secs(10);
        let (procs_result, mem_result) = tokio::join!(
            tokio::time::timeout(timeout, client.processes()),
            tokio::time::timeout(timeout, client.memory()),
        );

        // Handle memory result (for total memory and usage)
        if let Ok(Ok(mem_info)) = mem_result {
            if let Some(node_mem) = mem_info.into_iter().next() {
                if let Some(meminfo) = node_mem.meminfo {
                    self.total_memory = meminfo.mem_total;
                    self.memory_used = meminfo.mem_total.saturating_sub(meminfo.mem_available);
                    self.memory_usage_percent = meminfo.usage_percent();
                }
            }
        }

        // Handle processes result
        let node_processes = match procs_result {
            Ok(Ok(procs)) => procs,
            Ok(Err(e)) => {
                self.set_error(format!("Failed to fetch processes: {} (node: {})", e, self.address));
                return Ok(());
            }
            Err(_) => {
                self.set_error(format!("Request timed out after {}s", timeout.as_secs()));
                return Ok(());
            }
        };

        // Find processes for our node
        if let Some(node_data) = node_processes.into_iter().next() {
            self.processes = node_data.processes;
            self.calculate_state_counts();
            self.sort_processes();
            self.apply_filter();
        } else {
            self.processes.clear();
            self.filtered_indices.clear();
        }

        // Reset selection if needed
        if !self.filtered_indices.is_empty() && self.selected >= self.filtered_indices.len() {
            self.selected = 0;
        }
        self.table_state.select(Some(self.selected));

        self.loading = false;
        self.error = None;
        self.last_refresh = Some(Instant::now());

        Ok(())
    }

    /// Calculate state counts from processes
    fn calculate_state_counts(&mut self) {
        self.state_counts = StateCounts::default();
        for proc in &self.processes {
            match proc.state {
                ProcessState::Running => self.state_counts.running += 1,
                ProcessState::Sleeping => self.state_counts.sleeping += 1,
                ProcessState::DiskSleep => self.state_counts.disk_wait += 1,
                ProcessState::Zombie => self.state_counts.zombie += 1,
                _ => {}
            }
        }
    }

    /// Sort processes based on current sort order
    fn sort_processes(&mut self) {
        match self.sort_by {
            SortBy::Cpu => {
                self.processes.sort_by(|a, b| {
                    b.cpu_time.partial_cmp(&a.cpu_time).unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            SortBy::Mem => {
                self.processes.sort_by(|a, b| b.resident_memory.cmp(&a.resident_memory));
            }
        }
    }

    /// Apply current filter to processes
    fn apply_filter(&mut self) {
        self.filtered_indices = if let Some(ref filter) = self.filter {
            let filter_lower = filter.to_lowercase();
            self.processes
                .iter()
                .enumerate()
                .filter(|(_, p)| {
                    p.command.to_lowercase().contains(&filter_lower)
                        || p.args.to_lowercase().contains(&filter_lower)
                        || p.executable.to_lowercase().contains(&filter_lower)
                })
                .map(|(i, _)| i)
                .collect()
        } else {
            (0..self.processes.len()).collect()
        };
        // Rebuild display list after filtering
        self.rebuild_display_list();
    }

    /// Rebuild the display list (either flat or tree view)
    fn rebuild_display_list(&mut self) {
        use std::collections::HashMap;

        if !self.tree_view {
            // Flat view - just use filtered indices directly
            self.display_entries = self.filtered_indices
                .iter()
                .map(|&idx| DisplayEntry {
                    process_idx: idx,
                    depth: 0,
                    is_last: false,
                    ancestors_have_siblings: vec![],
                })
                .collect();
            return;
        }

        // Tree view - build parent-child hierarchy
        // First, create a map of pid -> index for quick lookup
        let mut pid_to_idx: HashMap<i32, usize> = HashMap::new();
        for (idx, proc) in self.processes.iter().enumerate() {
            pid_to_idx.insert(proc.pid, idx);
        }

        // Group children by parent pid
        let mut children_map: HashMap<i32, Vec<usize>> = HashMap::new();
        let mut root_indices: Vec<usize> = Vec::new();

        for &idx in &self.filtered_indices {
            let proc = &self.processes[idx];
            let ppid = proc.ppid;

            // Check if parent exists in our filtered list
            let parent_in_list = pid_to_idx.get(&ppid)
                .map(|&parent_idx| self.filtered_indices.contains(&parent_idx))
                .unwrap_or(false);

            if ppid == 0 || ppid == proc.pid || !parent_in_list {
                // Root process (no parent, or parent not in list)
                root_indices.push(idx);
            } else {
                // Has a visible parent
                children_map.entry(ppid).or_default().push(idx);
            }
        }

        // Build display list recursively
        self.display_entries.clear();

        fn add_tree_entries(
            entries: &mut Vec<DisplayEntry>,
            processes: &[ProcessInfo],
            children_map: &HashMap<i32, Vec<usize>>,
            indices: &[usize],
            depth: usize,
            ancestors_have_siblings: Vec<bool>,
        ) {
            let count = indices.len();
            for (i, &idx) in indices.iter().enumerate() {
                let is_last = i == count - 1;

                entries.push(DisplayEntry {
                    process_idx: idx,
                    depth,
                    is_last,
                    ancestors_have_siblings: ancestors_have_siblings.clone(),
                });

                // Process children
                let pid = processes[idx].pid;
                if let Some(children) = children_map.get(&pid) {
                    let mut child_ancestors = ancestors_have_siblings.clone();
                    child_ancestors.push(!is_last);
                    add_tree_entries(
                        entries,
                        processes,
                        children_map,
                        children,
                        depth + 1,
                        child_ancestors,
                    );
                }
            }
        }

        add_tree_entries(
            &mut self.display_entries,
            &self.processes,
            &children_map,
            &root_indices,
            0,
            vec![],
        );
    }

    /// Get currently selected process
    fn selected_process(&self) -> Option<&ProcessInfo> {
        self.display_entries
            .get(self.selected)
            .and_then(|entry| self.processes.get(entry.process_idx))
    }

    /// Navigate to previous process
    fn select_prev(&mut self) {
        if !self.display_entries.is_empty() {
            self.selected = self.selected.saturating_sub(1);
            self.table_state.select(Some(self.selected));
        }
    }

    /// Navigate to next process
    fn select_next(&mut self) {
        if !self.display_entries.is_empty() {
            self.selected = (self.selected + 1).min(self.display_entries.len() - 1);
            self.table_state.select(Some(self.selected));
        }
    }

    /// Jump to top of list
    fn select_first(&mut self) {
        if !self.display_entries.is_empty() {
            self.selected = 0;
            self.table_state.select(Some(self.selected));
        }
    }

    /// Jump to bottom of list
    fn select_last(&mut self) {
        if !self.display_entries.is_empty() {
            self.selected = self.display_entries.len() - 1;
            self.table_state.select(Some(self.selected));
        }
    }

    /// Get color for process state
    fn state_color(state: &ProcessState) -> Color {
        match state {
            ProcessState::Running => Color::Green,
            ProcessState::Zombie => Color::Red,
            ProcessState::DiskSleep => Color::Yellow,
            ProcessState::Stopped => Color::Magenta,
            _ => Color::default(),
        }
    }

    /// Draw the header
    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let sort_indicator = format!("[{}▼]", self.sort_by.label());
        let tree_indicator = if self.tree_view { "[TREE]" } else { "" };
        let proc_count = format!("{} procs", self.display_entries.len());

        let mut spans = vec![
            Span::styled("Processes: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(&self.hostname),
            Span::styled(" (", Style::default().fg(Color::DarkGray)),
            Span::raw(&self.address),
            Span::styled(")", Style::default().fg(Color::DarkGray)),
            Span::raw("  "),
            Span::styled(sort_indicator, Style::default().fg(Color::Cyan)),
        ];

        if self.tree_view {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(tree_indicator, Style::default().fg(Color::Green)));
        }

        spans.push(Span::raw(" "));
        spans.push(Span::styled(proc_count, Style::default().fg(Color::DarkGray)));

        let line = Line::from(spans);
        let para = Paragraph::new(line);
        frame.render_widget(para, area);
    }

    /// Generate a usage bar (10 chars) based on percentage
    fn usage_bar(percent: f32) -> (String, Color) {
        let filled = ((percent / 10.0).round() as usize).min(10);
        let empty = 10 - filled;
        let bar = format!("{}{}", "█".repeat(filled), "░".repeat(empty));
        let color = if percent > 85.0 {
            Color::Red
        } else if percent > 60.0 {
            Color::Yellow
        } else {
            Color::Green
        };
        (bar, color)
    }

    /// Draw the summary bar with CPU/MEM bars and state counts
    fn draw_summary_bar(&self, frame: &mut Frame, area: Rect) {
        // Memory bar with actual percentage
        let (mem_bar, mem_color) = Self::usage_bar(self.memory_usage_percent);
        let mem_pct = format!("{:>3.0}%", self.memory_usage_percent);

        let mut spans = vec![
            Span::raw("CPU "),
            Span::styled("░░░░░░░░░░", Style::default().fg(Color::DarkGray)),
            Span::raw(" --% "),
            Span::raw("   MEM "),
            Span::styled(mem_bar, Style::default().fg(mem_color)),
            Span::raw(format!(" {} ", mem_pct)),
            Span::raw("  "),
        ];

        // Add state counts with colors
        spans.push(Span::styled(
            format!("R:{}", self.state_counts.running),
            Style::default().fg(Color::Green),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("S:{}", self.state_counts.sleeping),
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("D:{}", self.state_counts.disk_wait),
            Style::default().fg(if self.state_counts.disk_wait > 0 {
                Color::Yellow
            } else {
                Color::DarkGray
            }),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("Z:{}", self.state_counts.zombie),
            Style::default().fg(if self.state_counts.zombie > 0 {
                Color::Red
            } else {
                Color::DarkGray
            }),
        ));

        let line = Line::from(spans);
        let para = Paragraph::new(line);
        frame.render_widget(para, area);
    }

    /// Check if any process has high memory usage (>85% of total)
    fn has_high_memory_process(&self) -> Option<(String, f64)> {
        if self.total_memory == 0 {
            return None;
        }
        let threshold = 0.85;
        for proc in &self.processes {
            let mem_percent = proc.resident_memory as f64 / self.total_memory as f64;
            if mem_percent > threshold {
                return Some((proc.command.clone(), mem_percent * 100.0));
            }
        }
        None
    }

    /// Get system memory usage percentage (from meminfo, not process sum)
    fn total_memory_usage_percent(&self) -> f64 {
        self.memory_usage_percent as f64
    }

    /// Draw warning banner if needed
    fn draw_warning(&self, frame: &mut Frame, area: Rect) -> bool {
        let mut warnings = Vec::new();

        // Check for zombie processes
        if self.state_counts.zombie > 0 {
            warnings.push(format!("{} zombie process(es) detected", self.state_counts.zombie));
        }

        // Check for high memory usage (total > 85%)
        let mem_usage = self.total_memory_usage_percent();
        if mem_usage > 85.0 {
            warnings.push(format!("High memory usage: {:.1}%", mem_usage));
        } else if let Some((proc_name, percent)) = self.has_high_memory_process() {
            // Individual process using >85% of total memory
            warnings.push(format!("Process '{}' using {:.1}% memory", proc_name, percent));
        }

        if warnings.is_empty() {
            return false;
        }

        let warning_text = format!("⚠ {}", warnings.join(" • "));
        let para = Paragraph::new(warning_text)
            .style(Style::default().fg(Color::Yellow));
        frame.render_widget(para, area);
        true
    }

    /// Draw the process table
    fn draw_process_table(&mut self, frame: &mut Frame, area: Rect) {
        // Collect row data first to avoid borrow conflicts
        let max_cmd_len = area.width.saturating_sub(45) as usize;
        let max_cpu = self.processes.iter().map(|p| p.cpu_time).fold(0.0, f64::max);
        let max_mem = self.processes.iter().map(|p| p.resident_memory).max().unwrap_or(0);

        let row_data: Vec<_> = self
            .display_entries
            .iter()
            .filter_map(|entry| {
                let proc = self.processes.get(entry.process_idx)?;

                // Calculate CPU intensity color inline
                let cpu_color = if max_cpu == 0.0 {
                    Color::default()
                } else {
                    let ratio = proc.cpu_time / max_cpu;
                    if ratio > 0.7 {
                        Color::Red
                    } else if ratio > 0.3 {
                        Color::Yellow
                    } else {
                        Color::default()
                    }
                };

                // Calculate MEM intensity color
                let mem_color = if max_mem == 0 {
                    Color::default()
                } else {
                    let ratio = proc.resident_memory as f64 / max_mem as f64;
                    if ratio > 0.7 {
                        Color::Red
                    } else if ratio > 0.3 {
                        Color::Yellow
                    } else {
                        Color::default()
                    }
                };

                let state_color = Self::state_color(&proc.state);
                let cpu_str = proc.cpu_time_human();
                let mem_str = proc.resident_memory_human();
                let command = proc.display_command();

                // Build tree prefix
                let tree_prefix = if !self.tree_view || entry.depth == 0 {
                    String::new()
                } else {
                    let mut prefix = String::new();
                    for &has_sibling in &entry.ancestors_have_siblings {
                        if has_sibling {
                            prefix.push_str("│ ");
                        } else {
                            prefix.push_str("  ");
                        }
                    }
                    if entry.is_last {
                        prefix.push_str("└─");
                    } else {
                        prefix.push_str("├─");
                    }
                    prefix
                };

                let full_command = format!("{}{}", tree_prefix, command);
                let cmd_display = if full_command.len() > max_cmd_len {
                    format!("{}...", &full_command[..max_cmd_len.saturating_sub(3)])
                } else {
                    full_command
                };

                Some((proc.pid, cpu_str, mem_str, proc.state.short(), cmd_display, cpu_color, mem_color, state_color))
            })
            .collect();

        let rows: Vec<Row> = row_data
            .into_iter()
            .map(|(pid, cpu_str, mem_str, state, cmd_display, cpu_color, mem_color, state_color)| {
                Row::new(vec![
                    Cell::from(format!("{:>6}", pid)),
                    Cell::from(format!("{:>8}", cpu_str)).style(Style::default().fg(cpu_color)),
                    Cell::from(format!("{:>8}", mem_str)).style(Style::default().fg(mem_color)),
                    Cell::from(state).style(Style::default().fg(state_color)),
                    Cell::from(cmd_display),
                ])
            })
            .collect();

        let header = Row::new(vec![
            Cell::from("PID"),
            Cell::from("CPU"),
            Cell::from("MEM"),
            Cell::from("S"),
            Cell::from("COMMAND"),
        ])
        .style(Style::default().add_modifier(Modifier::DIM))
        .bottom_margin(1);

        let widths = [
            Constraint::Length(7),      // PID
            Constraint::Length(9),      // CPU
            Constraint::Length(9),      // MEM
            Constraint::Length(2),      // State
            Constraint::Percentage(60), // Command
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

    /// Draw the detail section for selected process
    fn draw_detail_section(&self, frame: &mut Frame, area: Rect) {
        let Some(proc) = self.selected_process() else {
            return;
        };

        let title = format!(" {} (PID {}) ", proc.command, proc.pid);

        // Full command line (truncated if too long)
        let full_cmd = proc.display_command();
        let max_len = (area.width as usize).saturating_sub(4);
        let cmd_display = if full_cmd.len() > max_len {
            format!("{}...", &full_cmd[..max_len.saturating_sub(3)])
        } else {
            full_cmd.to_string()
        };

        let content = vec![
            Line::from(vec![
                Span::raw("  "),
                Span::raw(&cmd_display),
            ]),
            Line::from(vec![
                Span::styled("  State: ", Style::default().add_modifier(Modifier::DIM)),
                Span::styled(
                    proc.state.description(),
                    Style::default().fg(Self::state_color(&proc.state)),
                ),
                Span::raw("    "),
                Span::styled("Threads: ", Style::default().add_modifier(Modifier::DIM)),
                Span::raw(proc.threads.to_string()),
                Span::raw("    "),
                Span::styled("Virtual: ", Style::default().add_modifier(Modifier::DIM)),
                Span::raw(proc.virtual_memory_human()),
                Span::raw("    "),
                Span::styled("Resident: ", Style::default().add_modifier(Modifier::DIM)),
                Span::raw(proc.resident_memory_human()),
            ]),
        ];

        let block = Block::default().title(title).borders(Borders::TOP);
        let para = Paragraph::new(content).block(block);
        frame.render_widget(para, area);
    }

    /// Draw the footer with keybindings
    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let auto_status = if self.auto_refresh {
            Span::styled("ON ", Style::default().fg(Color::Green))
        } else {
            Span::styled("OFF", Style::default().fg(Color::DarkGray))
        };

        let tree_status = if self.tree_view {
            Span::styled("ON ", Style::default().fg(Color::Green))
        } else {
            Span::styled("OFF", Style::default().fg(Color::DarkGray))
        };

        let line = Line::from(vec![
            Span::styled("[1]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" cpu  "),
            Span::styled("[2]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" mem  "),
            Span::styled("[t]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" tree:"),
            tree_status,
            Span::raw("  "),
            Span::styled("[/]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" filter  "),
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

    /// Draw the filter input bar
    fn draw_filter_bar(&self, frame: &mut Frame, area: Rect) {
        let match_count = self.display_entries.len();
        let total = self.processes.len();

        let line = Line::from(vec![
            Span::styled("Filter: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(&self.filter_input),
            Span::styled("█", Style::default().fg(Color::Cyan)), // Cursor
            Span::raw("  "),
            Span::styled(
                format!("[{}/{}]", match_count, total),
                Style::default().fg(Color::DarkGray),
            ),
        ]);

        let para = Paragraph::new(line);
        frame.render_widget(para, area);
    }
}

impl Component for ProcessesComponent {
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match self.mode {
            Mode::Normal => self.handle_normal_key(key),
            Mode::Filtering => self.handle_filter_key(key),
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
        if self.loading {
            let loading = Paragraph::new("Loading processes...")
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

        // Calculate layout - check for any warning condition
        let has_warning = self.state_counts.zombie > 0
            || self.total_memory_usage_percent() > 85.0
            || self.has_high_memory_process().is_some();
        let warning_height = if has_warning { 1 } else { 0 };

        let chunks = Layout::vertical([
            Constraint::Length(1),                    // Header
            Constraint::Length(1),                    // Summary bar
            Constraint::Length(warning_height),       // Warning (if any)
            Constraint::Length(1),                    // Filter bar (in filter mode) or spacer
            Constraint::Min(5),                       // Process table
            Constraint::Length(4),                    // Detail section
            Constraint::Length(1),                    // Footer
        ])
        .split(area);

        self.draw_header(frame, chunks[0]);
        self.draw_summary_bar(frame, chunks[1]);

        if has_warning {
            self.draw_warning(frame, chunks[2]);
        }

        // Filter bar or spacer
        if self.mode == Mode::Filtering || self.filter.is_some() {
            self.draw_filter_bar(frame, chunks[3]);
        }

        self.draw_process_table(frame, chunks[4]);
        self.draw_detail_section(frame, chunks[5]);
        self.draw_footer(frame, chunks[6]);

        Ok(())
    }
}

impl ProcessesComponent {
    fn handle_normal_key(&mut self, key: KeyEvent) -> Result<Option<Action>> {
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
            KeyCode::Char('g') => {
                self.select_first();
                Ok(None)
            }
            KeyCode::Char('G') => {
                self.select_last();
                Ok(None)
            }
            KeyCode::Char('1') => {
                self.sort_by = SortBy::Cpu;
                self.sort_processes();
                self.apply_filter();
                self.selected = 0;
                self.table_state.select(Some(0));
                Ok(None)
            }
            KeyCode::Char('2') => {
                self.sort_by = SortBy::Mem;
                self.sort_processes();
                self.apply_filter();
                self.selected = 0;
                self.table_state.select(Some(0));
                Ok(None)
            }
            KeyCode::Char('/') => {
                self.mode = Mode::Filtering;
                self.filter_input = self.filter.clone().unwrap_or_default();
                Ok(None)
            }
            KeyCode::Char('r') => Ok(Some(Action::Refresh)),
            KeyCode::Char('a') => {
                self.auto_refresh = !self.auto_refresh;
                Ok(None)
            }
            KeyCode::Char('t') => {
                self.tree_view = !self.tree_view;
                self.rebuild_display_list();
                self.selected = 0;
                self.table_state.select(Some(0));
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match key.code {
            KeyCode::Esc => {
                // Clear filter and exit filter mode
                self.mode = Mode::Normal;
                self.filter = None;
                self.filter_input.clear();
                self.apply_filter();
                self.selected = 0;
                self.table_state.select(Some(0));
                Ok(None)
            }
            KeyCode::Enter => {
                // Apply filter and exit filter mode
                self.mode = Mode::Normal;
                self.filter = if self.filter_input.is_empty() {
                    None
                } else {
                    Some(self.filter_input.clone())
                };
                self.apply_filter();
                self.selected = 0;
                self.table_state.select(Some(0));
                Ok(None)
            }
            KeyCode::Backspace => {
                self.filter_input.pop();
                // Live filter as you type
                self.filter = if self.filter_input.is_empty() {
                    None
                } else {
                    Some(self.filter_input.clone())
                };
                self.apply_filter();
                Ok(None)
            }
            KeyCode::Char(c) => {
                self.filter_input.push(c);
                // Live filter as you type
                self.filter = Some(self.filter_input.clone());
                self.apply_filter();
                Ok(None)
            }
            _ => Ok(None),
        }
    }
}
