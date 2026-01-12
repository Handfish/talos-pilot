//! Processes component - displays running processes on a node
//!
//! "Show me what's wrong in 5 seconds"

use crate::action::Action;
use crate::components::Component;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use talos_pilot_core::{AsyncState, format_bytes_compact};
use talos_rs::{CpuStat, ProcessInfo, ProcessState, TalosClient};

/// Auto-refresh interval in seconds
const AUTO_REFRESH_INTERVAL_SECS: u64 = 5;

/// Sort order for process list
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortBy {
    #[default]
    CpuPercent,
    CpuTime,
    Mem,
}

impl SortBy {
    pub fn label(&self) -> &'static str {
        match self {
            SortBy::CpuPercent => "CPU%",
            SortBy::CpuTime => "TIME",
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

/// Loaded process data (wrapped by AsyncState)
#[derive(Debug, Clone, Default)]
pub(crate) struct ProcessesData {
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

    /// State counts for summary
    state_counts: StateCounts,
    /// Total memory on node (for percentage calc)
    total_memory: u64,
    /// Memory used on node
    memory_used: u64,
    /// Memory usage percentage (from system, not process sum)
    memory_usage_percent: f32,

    /// Previous CPU stats (for calculating usage delta)
    prev_cpu_stat: Option<CpuStat>,
    /// Current CPU usage percentage
    cpu_usage_percent: f32,
    /// Number of CPUs on node
    cpu_count: usize,
    /// Load average (1, 5, 15 min)
    load_avg: (f64, f64, f64),

    /// Previous CPU time per process (for calculating per-process CPU %)
    prev_cpu_times: HashMap<i32, f64>,
    /// Calculated CPU percentage per process
    cpu_percentages: HashMap<i32, f32>,
    /// Time of last CPU measurement
    last_cpu_sample: Option<Instant>,
}

/// Processes component for viewing node processes
pub struct ProcessesComponent {
    /// Async state wrapping all process data
    state: AsyncState<ProcessesData>,

    /// Selected index in filtered list
    selected: usize,
    /// Table state for rendering
    table_state: TableState,
    /// Current sort order
    sort_by: SortBy,
    /// Tree view enabled
    tree_view: bool,
    /// Tree root PID (None = full tree from init, Some(pid) = subtree)
    tree_root: Option<i32>,

    /// State filter (None = show all, Some(state) = show only that state)
    state_filter: Option<ProcessState>,

    /// Current mode
    mode: Mode,
    /// Filter input text
    filter_input: String,
    /// Active filter (applied)
    filter: Option<String>,

    /// Auto-refresh enabled
    auto_refresh: bool,

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

        // Initialize with hostname/address in the data
        let initial_data = ProcessesData {
            hostname,
            address,
            ..Default::default()
        };
        let mut state = AsyncState::new();
        state.set_data(initial_data);

        Self {
            state,
            selected: 0,
            table_state,
            sort_by: SortBy::CpuPercent,
            tree_view: false,
            tree_root: None,
            state_filter: None,
            mode: Mode::Normal,
            filter_input: String::new(),
            filter: None,
            auto_refresh: true,
            client: None,
        }
    }

    /// Set the client for API calls
    pub fn set_client(&mut self, client: TalosClient) {
        self.client = Some(client);
    }

    /// Set error message
    pub fn set_error(&mut self, error: String) {
        self.state.set_error(error);
    }

    /// Helper to get data reference
    fn data(&self) -> Option<&ProcessesData> {
        self.state.data()
    }

    /// Helper to get mutable data reference
    fn data_mut(&mut self) -> Option<&mut ProcessesData> {
        self.state.data_mut()
    }

    /// Sort processes (wrapper that operates on current data)
    fn sort_processes(&mut self) {
        let sort_by = self.sort_by;
        if let Some(data) = self.data_mut() {
            match sort_by {
                SortBy::CpuPercent => {
                    let cpu_pcts = data.cpu_percentages.clone();
                    data.processes.sort_by(|a, b| {
                        let a_pct = cpu_pcts.get(&a.pid).unwrap_or(&0.0);
                        let b_pct = cpu_pcts.get(&b.pid).unwrap_or(&0.0);
                        b_pct
                            .partial_cmp(a_pct)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                }
                SortBy::CpuTime => {
                    data.processes.sort_by(|a, b| {
                        b.cpu_time
                            .partial_cmp(&a.cpu_time)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                }
                SortBy::Mem => {
                    data.processes
                        .sort_by(|a, b| b.resident_memory.cmp(&a.resident_memory));
                }
            }
        }
    }

    /// Apply filter (wrapper that operates on current data)
    fn apply_filter(&mut self) {
        let filter = self.filter.clone();
        let state_filter = self.state_filter.clone();
        let tree_view = self.tree_view;
        let tree_root = self.tree_root;

        if let Some(data) = self.data_mut() {
            data.filtered_indices = data
                .processes
                .iter()
                .enumerate()
                .filter(|(_, p)| {
                    // Apply text filter
                    let text_match = if let Some(ref filter) = filter {
                        let filter_lower = filter.to_lowercase();
                        p.command.to_lowercase().contains(&filter_lower)
                            || p.args.to_lowercase().contains(&filter_lower)
                            || p.executable.to_lowercase().contains(&filter_lower)
                    } else {
                        true
                    };

                    // Apply state filter
                    let state_match = if let Some(ref state_filter) = state_filter {
                        std::mem::discriminant(&p.state) == std::mem::discriminant(state_filter)
                    } else {
                        true
                    };

                    text_match && state_match
                })
                .map(|(i, _)| i)
                .collect();

            // Rebuild display list
            Self::rebuild_display_list_static(data, tree_view, tree_root);
        }
    }

    /// Rebuild display list (wrapper that operates on current data)
    fn rebuild_display_list(&mut self) {
        let tree_view = self.tree_view;
        let tree_root = self.tree_root;
        if let Some(data) = self.data_mut() {
            Self::rebuild_display_list_static(data, tree_view, tree_root);
        }
    }

    /// Static version of rebuild_display_list for use in async contexts
    fn rebuild_display_list_static(
        data: &mut ProcessesData,
        tree_view: bool,
        tree_root: Option<i32>,
    ) {
        use std::collections::HashMap;

        if !tree_view {
            // Flat view - just use filtered indices directly
            data.display_entries = data
                .filtered_indices
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
        let mut pid_to_idx: HashMap<i32, usize> = HashMap::new();
        for (idx, proc) in data.processes.iter().enumerate() {
            pid_to_idx.insert(proc.pid, idx);
        }

        let mut children_map: HashMap<i32, Vec<usize>> = HashMap::new();
        let mut root_indices: Vec<usize> = Vec::new();

        let descendants: Option<std::collections::HashSet<i32>> = tree_root.map(|root_pid| {
            let mut desc = std::collections::HashSet::new();
            desc.insert(root_pid);
            let mut changed = true;
            while changed {
                changed = false;
                for proc in &data.processes {
                    if desc.contains(&proc.ppid) && !desc.contains(&proc.pid) {
                        desc.insert(proc.pid);
                        changed = true;
                    }
                }
            }
            desc
        });

        for &idx in &data.filtered_indices {
            let proc = &data.processes[idx];

            if let Some(ref desc) = descendants
                && !desc.contains(&proc.pid)
            {
                continue;
            }

            let ppid = proc.ppid;
            let parent_in_list = pid_to_idx
                .get(&ppid)
                .map(|&parent_idx| data.filtered_indices.contains(&parent_idx))
                .unwrap_or(false);

            let is_root = if let Some(root_pid) = tree_root {
                proc.pid == root_pid
            } else {
                ppid == 0 || ppid == proc.pid || !parent_in_list
            };

            if is_root {
                root_indices.push(idx);
            } else if parent_in_list {
                if descendants.as_ref().is_none_or(|d| d.contains(&ppid)) {
                    children_map.entry(ppid).or_default().push(idx);
                } else {
                    root_indices.push(idx);
                }
            } else {
                root_indices.push(idx);
            }
        }

        data.display_entries.clear();

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
            &mut data.display_entries,
            &data.processes,
            &children_map,
            &root_indices,
            0,
            vec![],
        );
    }

    /// Refresh process data from the node
    pub async fn refresh(&mut self) -> Result<()> {
        let Some(client) = self.client.clone() else {
            self.set_error("No client configured".to_string());
            return Ok(());
        };

        self.state.start_loading();

        // Get or create data, preserving previous CPU state for delta calculations
        let mut data = self.state.take_data().unwrap_or_default();

        // Fetch processes, memory info, system stats, CPU info, and load avg in parallel
        let timeout = std::time::Duration::from_secs(10);
        let (procs_result, mem_result, stat_result, cpu_info_result, load_result) = tokio::join!(
            tokio::time::timeout(timeout, client.processes()),
            tokio::time::timeout(timeout, client.memory()),
            tokio::time::timeout(timeout, client.system_stat()),
            tokio::time::timeout(timeout, client.cpu_info()),
            tokio::time::timeout(timeout, client.load_avg()),
        );

        // Handle memory result (for total memory and usage)
        if let Ok(Ok(mem_info)) = mem_result
            && let Some(node_mem) = mem_info.into_iter().next()
            && let Some(meminfo) = node_mem.meminfo
        {
            data.total_memory = meminfo.mem_total;
            data.memory_used = meminfo.mem_total.saturating_sub(meminfo.mem_available);
            data.memory_usage_percent = meminfo.usage_percent();
        }

        // Handle CPU info result (for CPU count)
        if let Ok(Ok(cpu_info)) = cpu_info_result
            && let Some(node_cpu) = cpu_info.into_iter().next()
        {
            data.cpu_count = node_cpu.cpu_count;
        }

        // Handle load average result
        if let Ok(Ok(load_info)) = load_result
            && let Some(node_load) = load_info.into_iter().next()
        {
            data.load_avg = (node_load.load1, node_load.load5, node_load.load15);
        }

        // Handle system stat result (for CPU usage)
        if let Ok(Ok(stats)) = stat_result
            && let Some(node_stat) = stats.into_iter().next()
        {
            let curr_cpu = node_stat.cpu_total;
            // Calculate CPU usage from delta if we have previous stats
            if let Some(ref prev_cpu) = data.prev_cpu_stat {
                data.cpu_usage_percent = CpuStat::usage_percent_from(prev_cpu, &curr_cpu);
            }
            data.prev_cpu_stat = Some(curr_cpu);
        }

        // Handle processes result
        let node_processes = match procs_result {
            Ok(Ok(procs)) => procs,
            Ok(Err(e)) => {
                self.state.set_error(format!(
                    "Failed to fetch processes: {} (node: {})",
                    e, data.address
                ));
                // Re-store data so far
                self.state.set_data(data);
                return Ok(());
            }
            Err(_) => {
                self.state
                    .set_error(format!("Request timed out after {}s", timeout.as_secs()));
                // Re-store data so far
                self.state.set_data(data);
                return Ok(());
            }
        };

        // Find processes for our node
        if let Some(node_data) = node_processes.into_iter().next() {
            data.processes = node_data.processes;
            Self::calculate_cpu_percentages_into(&mut data);
            Self::calculate_state_counts_into(&mut data);
            self.sort_processes_in_data(&mut data);
            self.apply_filter_to_data(&mut data);
        } else {
            data.processes.clear();
            data.filtered_indices.clear();
        }

        // Reset selection if needed
        if !data.display_entries.is_empty() && self.selected >= data.display_entries.len() {
            self.selected = 0;
        }
        self.table_state.select(Some(self.selected));

        // Store the data
        self.state.set_data(data);
        Ok(())
    }

    /// Calculate state counts from processes (static method)
    fn calculate_state_counts_into(data: &mut ProcessesData) {
        data.state_counts = StateCounts::default();
        for proc in &data.processes {
            match proc.state {
                ProcessState::Running => data.state_counts.running += 1,
                ProcessState::Sleeping => data.state_counts.sleeping += 1,
                ProcessState::DiskSleep => data.state_counts.disk_wait += 1,
                ProcessState::Zombie => data.state_counts.zombie += 1,
                _ => {}
            }
        }
    }

    /// Calculate per-process CPU percentages from delta (static method)
    fn calculate_cpu_percentages_into(data: &mut ProcessesData) {
        let now = Instant::now();

        // Calculate elapsed time since last sample
        let elapsed_secs = data
            .last_cpu_sample
            .map(|t| now.duration_since(t).as_secs_f64())
            .unwrap_or(0.0);

        // Need at least some time elapsed to calculate percentage
        if elapsed_secs > 0.1 && data.cpu_count > 0 {
            // Calculate percentage for each process
            for proc in &data.processes {
                if let Some(&prev_time) = data.prev_cpu_times.get(&proc.pid) {
                    let delta = proc.cpu_time - prev_time;
                    // CPU % = (delta_cpu_time / elapsed_wall_time) * 100 / num_cpus
                    // Actually for "percentage of one CPU", we don't divide by num_cpus
                    // This matches htop behavior where a process can show >100% on multi-core
                    let pct = ((delta / elapsed_secs) * 100.0) as f32;
                    data.cpu_percentages.insert(proc.pid, pct.max(0.0));
                }
            }
        }

        // Store current CPU times for next calculation
        data.prev_cpu_times.clear();
        for proc in &data.processes {
            data.prev_cpu_times.insert(proc.pid, proc.cpu_time);
        }
        data.last_cpu_sample = Some(now);
    }

    /// Sort processes based on current sort order (operates on data)
    fn sort_processes_in_data(&self, data: &mut ProcessesData) {
        match self.sort_by {
            SortBy::CpuPercent => {
                // Sort by CPU percentage
                let cpu_pcts = &data.cpu_percentages;
                data.processes.sort_by(|a, b| {
                    let a_pct = cpu_pcts.get(&a.pid).unwrap_or(&0.0);
                    let b_pct = cpu_pcts.get(&b.pid).unwrap_or(&0.0);
                    b_pct
                        .partial_cmp(a_pct)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            SortBy::CpuTime => {
                // Sort by cumulative CPU time
                data.processes.sort_by(|a, b| {
                    b.cpu_time
                        .partial_cmp(&a.cpu_time)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            SortBy::Mem => {
                data.processes
                    .sort_by(|a, b| b.resident_memory.cmp(&a.resident_memory));
            }
        }
    }

    /// Apply current filter to processes (operates on data)
    fn apply_filter_to_data(&self, data: &mut ProcessesData) {
        data.filtered_indices = data
            .processes
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                // Apply text filter
                let text_match = if let Some(ref filter) = self.filter {
                    let filter_lower = filter.to_lowercase();
                    p.command.to_lowercase().contains(&filter_lower)
                        || p.args.to_lowercase().contains(&filter_lower)
                        || p.executable.to_lowercase().contains(&filter_lower)
                } else {
                    true
                };

                // Apply state filter
                let state_match = if let Some(ref state_filter) = self.state_filter {
                    std::mem::discriminant(&p.state) == std::mem::discriminant(state_filter)
                } else {
                    true
                };

                text_match && state_match
            })
            .map(|(i, _)| i)
            .collect();
        // Rebuild display list after filtering
        self.rebuild_display_list_into(data);
    }

    /// Rebuild the display list (either flat or tree view) - operates on data
    fn rebuild_display_list_into(&self, data: &mut ProcessesData) {
        use std::collections::HashMap;

        if !self.tree_view {
            // Flat view - just use filtered indices directly
            data.display_entries = data
                .filtered_indices
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
        for (idx, proc) in data.processes.iter().enumerate() {
            pid_to_idx.insert(proc.pid, idx);
        }

        // Group children by parent pid
        let mut children_map: HashMap<i32, Vec<usize>> = HashMap::new();
        let mut root_indices: Vec<usize> = Vec::new();

        // Determine which processes are descendants of tree_root (if set)
        let descendants: Option<std::collections::HashSet<i32>> = self.tree_root.map(|root_pid| {
            let mut desc = std::collections::HashSet::new();
            desc.insert(root_pid);
            // Keep adding children until no more found
            let mut changed = true;
            while changed {
                changed = false;
                for proc in &data.processes {
                    if desc.contains(&proc.ppid) && !desc.contains(&proc.pid) {
                        desc.insert(proc.pid);
                        changed = true;
                    }
                }
            }
            desc
        });

        for &idx in &data.filtered_indices {
            let proc = &data.processes[idx];

            // If subtree mode, skip processes not in the subtree
            if let Some(ref desc) = descendants
                && !desc.contains(&proc.pid)
            {
                continue;
            }

            let ppid = proc.ppid;

            // Check if parent exists in our filtered list
            let parent_in_list = pid_to_idx
                .get(&ppid)
                .map(|&parent_idx| data.filtered_indices.contains(&parent_idx))
                .unwrap_or(false);

            // In subtree mode, root is the tree_root process
            let is_root = if let Some(root_pid) = self.tree_root {
                proc.pid == root_pid
            } else {
                ppid == 0 || ppid == proc.pid || !parent_in_list
            };

            if is_root {
                root_indices.push(idx);
            } else if parent_in_list {
                // Has a visible parent (and not filtered out by subtree)
                if descendants.as_ref().is_none_or(|d| d.contains(&ppid)) {
                    children_map.entry(ppid).or_default().push(idx);
                } else {
                    // Parent not in subtree, treat as root
                    root_indices.push(idx);
                }
            } else {
                root_indices.push(idx);
            }
        }

        // Build display list recursively
        data.display_entries.clear();

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
            &mut data.display_entries,
            &data.processes,
            &children_map,
            &root_indices,
            0,
            vec![],
        );
    }

    /// Get currently selected process
    fn selected_process(&self) -> Option<&ProcessInfo> {
        self.data().and_then(|data| {
            data.display_entries
                .get(self.selected)
                .and_then(|entry| data.processes.get(entry.process_idx))
        })
    }

    /// Navigate to previous process
    fn select_prev(&mut self) {
        let display_len = self.data().map(|d| d.display_entries.len()).unwrap_or(0);
        if display_len > 0 {
            self.selected = self.selected.saturating_sub(1);
            self.table_state.select(Some(self.selected));
        }
    }

    /// Navigate to next process
    fn select_next(&mut self) {
        let display_len = self.data().map(|d| d.display_entries.len()).unwrap_or(0);
        if display_len > 0 {
            self.selected = (self.selected + 1).min(display_len - 1);
            self.table_state.select(Some(self.selected));
        }
    }

    /// Jump to top of list
    fn select_first(&mut self) {
        let display_len = self.data().map(|d| d.display_entries.len()).unwrap_or(0);
        if display_len > 0 {
            self.selected = 0;
            self.table_state.select(Some(self.selected));
        }
    }

    /// Jump to bottom of list
    fn select_last(&mut self) {
        let display_len = self.data().map(|d| d.display_entries.len()).unwrap_or(0);
        if display_len > 0 {
            self.selected = display_len - 1;
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
        let data = match self.data() {
            Some(d) => d,
            None => return,
        };

        let sort_indicator = format!("[{}▼]", self.sort_by.label());
        let tree_indicator = if self.tree_view {
            if let Some(root_pid) = self.tree_root {
                // Find root process name
                let root_name = data
                    .processes
                    .iter()
                    .find(|p| p.pid == root_pid)
                    .map(|p| p.command.as_str())
                    .unwrap_or("?");
                format!("[TREE:{}/{}]", root_pid, root_name)
            } else {
                "[TREE]".to_string()
            }
        } else {
            String::new()
        };
        let proc_count = format!("{} procs", data.display_entries.len());

        // System resources info
        let cpu_info = if data.cpu_count > 0 {
            format!("{} CPU", data.cpu_count)
        } else {
            String::new()
        };
        let mem_info = if data.total_memory > 0 {
            format!("{} RAM", format_bytes_compact(data.total_memory))
        } else {
            String::new()
        };

        let mut spans = vec![
            Span::styled("Processes: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(&data.hostname),
            Span::styled(" (", Style::default().fg(Color::DarkGray)),
            Span::raw(&data.address),
            Span::styled(")", Style::default().fg(Color::DarkGray)),
        ];

        // Add system info
        if !cpu_info.is_empty() || !mem_info.is_empty() {
            spans.push(Span::raw("  "));
            spans.push(Span::styled("[", Style::default().fg(Color::DarkGray)));
            if !cpu_info.is_empty() {
                spans.push(Span::styled(&cpu_info, Style::default().fg(Color::Cyan)));
            }
            if !cpu_info.is_empty() && !mem_info.is_empty() {
                spans.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
            }
            if !mem_info.is_empty() {
                spans.push(Span::styled(&mem_info, Style::default().fg(Color::Magenta)));
            }
            spans.push(Span::styled("]", Style::default().fg(Color::DarkGray)));
        }

        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            sort_indicator,
            Style::default().fg(Color::Cyan),
        ));

        if self.tree_view {
            spans.push(Span::raw(" "));
            let tree_color = if self.tree_root.is_some() {
                Color::Cyan
            } else {
                Color::Green
            };
            spans.push(Span::styled(
                tree_indicator,
                Style::default().fg(tree_color),
            ));
        }

        // State filter indicator
        if let Some(ref state_filter) = self.state_filter {
            spans.push(Span::raw(" "));
            let (label, color) = match state_filter {
                ProcessState::Zombie => ("ZOMBIE", Color::Red),
                ProcessState::DiskSleep => ("DISK-WAIT", Color::Yellow),
                _ => ("FILTER", Color::White),
            };
            spans.push(Span::styled(
                format!("[{}]", label),
                Style::default().fg(color),
            ));
        }

        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            proc_count,
            Style::default().fg(Color::DarkGray),
        ));

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
        let data = match self.data() {
            Some(d) => d,
            None => return,
        };

        // CPU bar with actual percentage (shows "--%" on first refresh since we need delta)
        let (cpu_bar, cpu_color) = Self::usage_bar(data.cpu_usage_percent);
        let cpu_pct = if data.prev_cpu_stat.is_some() {
            format!("{:>3.0}%", data.cpu_usage_percent)
        } else {
            " --%".to_string()
        };

        // Memory bar with actual percentage
        let (mem_bar, mem_color) = Self::usage_bar(data.memory_usage_percent);
        let mem_pct = format!("{:>3.0}%", data.memory_usage_percent);

        // Load average with color coding (red if > cpu_count, yellow if > cpu_count*0.7)
        let cpu_count = data.cpu_count;
        let load_color = |load: f64| {
            let threshold = cpu_count as f64;
            if load > threshold {
                Color::Red
            } else if load > threshold * 0.7 {
                Color::Yellow
            } else {
                Color::Green
            }
        };

        let mut spans = vec![
            Span::raw("CPU "),
            Span::styled(cpu_bar, Style::default().fg(cpu_color)),
            Span::raw(format!(" {} ", cpu_pct)),
            Span::raw("  MEM "),
            Span::styled(mem_bar, Style::default().fg(mem_color)),
            Span::raw(format!(" {} ", mem_pct)),
            Span::raw("  "),
            // Load average
            Span::styled("Load: ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled(
                format!("{:.2}", data.load_avg.0),
                Style::default().fg(load_color(data.load_avg.0)),
            ),
            Span::raw(" "),
            Span::styled(
                format!("{:.2}", data.load_avg.1),
                Style::default().fg(load_color(data.load_avg.1)),
            ),
            Span::raw(" "),
            Span::styled(
                format!("{:.2}", data.load_avg.2),
                Style::default().fg(load_color(data.load_avg.2)),
            ),
            Span::raw("  "),
        ];

        // Add state counts with colors
        spans.push(Span::styled(
            format!("R:{}", data.state_counts.running),
            Style::default().fg(Color::Green),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("S:{}", data.state_counts.sleeping),
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("D:{}", data.state_counts.disk_wait),
            Style::default().fg(if data.state_counts.disk_wait > 0 {
                Color::Yellow
            } else {
                Color::DarkGray
            }),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("Z:{}", data.state_counts.zombie),
            Style::default().fg(if data.state_counts.zombie > 0 {
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
        let data = self.data()?;
        if data.total_memory == 0 {
            return None;
        }
        let threshold = 0.85;
        for proc in &data.processes {
            let mem_percent = proc.resident_memory as f64 / data.total_memory as f64;
            if mem_percent > threshold {
                return Some((proc.command.clone(), mem_percent * 100.0));
            }
        }
        None
    }

    /// Get system memory usage percentage (from meminfo, not process sum)
    fn total_memory_usage_percent(&self) -> f64 {
        self.data()
            .map(|d| d.memory_usage_percent as f64)
            .unwrap_or(0.0)
    }

    /// Draw warning banner if needed
    fn draw_warning(&self, frame: &mut Frame, area: Rect) -> bool {
        let Some(data) = self.data() else {
            return false;
        };

        let mut warnings = Vec::new();

        // Check for zombie processes
        if data.state_counts.zombie > 0 {
            warnings.push(format!(
                "{} zombie process(es) detected",
                data.state_counts.zombie
            ));
        }

        // Check for high memory usage (total > 85%)
        let mem_usage = self.total_memory_usage_percent();
        if mem_usage > 85.0 {
            warnings.push(format!("High memory usage: {:.1}%", mem_usage));
        } else if let Some((proc_name, percent)) = self.has_high_memory_process() {
            // Individual process using >85% of total memory
            warnings.push(format!(
                "Process '{}' using {:.1}% memory",
                proc_name, percent
            ));
        }

        if warnings.is_empty() {
            return false;
        }

        let warning_text = format!("⚠ {}", warnings.join(" • "));
        let para = Paragraph::new(warning_text).style(Style::default().fg(Color::Yellow));
        frame.render_widget(para, area);
        true
    }

    /// Collect process row data for rendering (helper to limit borrow scope)
    #[allow(clippy::type_complexity)]
    fn collect_process_row_data(
        &self,
        area: Rect,
    ) -> Option<
        Vec<(
            i32,
            String,
            String,
            String,
            String,
            Color,
            Color,
            Color,
            bool,
        )>,
    > {
        let data = self.data()?;
        let tree_view = self.tree_view;
        let max_cmd_len = area.width.saturating_sub(45) as usize;
        let max_mem = data
            .processes
            .iter()
            .map(|p| p.resident_memory)
            .max()
            .unwrap_or(0);

        Some(
            data.display_entries
                .iter()
                .filter_map(|entry| {
                    let proc = data.processes.get(entry.process_idx)?;

                    let cpu_pct = data.cpu_percentages.get(&proc.pid).copied().unwrap_or(0.0);
                    let cpu_color = if cpu_pct > 50.0 {
                        Color::Red
                    } else if cpu_pct > 10.0 {
                        Color::Yellow
                    } else if cpu_pct > 0.1 {
                        Color::Green
                    } else {
                        Color::default()
                    };

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
                    let cpu_str = format!("{:>5.1}%  {}", cpu_pct, proc.cpu_time_human());
                    let mem_str = proc.resident_memory_human();
                    let command = proc.display_command();

                    let tree_prefix = if !tree_view || entry.depth == 0 {
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

                    let is_interesting = matches!(
                        proc.state,
                        ProcessState::Running | ProcessState::Zombie | ProcessState::DiskSleep
                    );
                    Some((
                        proc.pid,
                        cpu_str,
                        mem_str,
                        proc.state.short().to_string(),
                        cmd_display,
                        cpu_color,
                        mem_color,
                        state_color,
                        is_interesting,
                    ))
                })
                .collect(),
        )
    }

    /// Draw the process table
    fn draw_process_table(&mut self, frame: &mut Frame, area: Rect) {
        // Collect row data using helper (borrow ends when helper returns)
        let row_data = match self.collect_process_row_data(area) {
            Some(data) => data,
            None => return,
        };

        let rows: Vec<Row> = row_data
            .into_iter()
            .map(
                |(
                    pid,
                    cpu_str,
                    mem_str,
                    state,
                    cmd_display,
                    cpu_color,
                    mem_color,
                    state_color,
                    is_interesting,
                )| {
                    // Highlight interesting processes (running, zombie, disk-wait) with bold
                    let base_style = if is_interesting {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    Row::new(vec![
                        Cell::from(format!("{:>6}", pid)).style(base_style),
                        Cell::from(cpu_str).style(base_style.fg(cpu_color)),
                        Cell::from(format!("{:>8}", mem_str)).style(base_style.fg(mem_color)),
                        Cell::from(state).style(base_style.fg(state_color)),
                        Cell::from(cmd_display).style(base_style),
                    ])
                },
            )
            .collect();

        // Build header with sort indicators
        let cpu_header = match self.sort_by {
            SortBy::CpuPercent => "CPU%▼  TIME",
            SortBy::CpuTime => "CPU%   TIME▼",
            _ => "CPU%   TIME",
        };
        let mem_header = if self.sort_by == SortBy::Mem {
            "MEM▼"
        } else {
            "MEM"
        };

        let header = Row::new(vec![
            Cell::from("PID"),
            Cell::from(cpu_header),
            Cell::from(mem_header),
            Cell::from("S"),
            Cell::from("COMMAND"),
        ])
        .style(Style::default().add_modifier(Modifier::DIM))
        .bottom_margin(1);

        let widths = [
            Constraint::Length(7),      // PID
            Constraint::Length(14),     // CPU% + TIME
            Constraint::Length(9),      // MEM
            Constraint::Length(2),      // State
            Constraint::Percentage(55), // Command
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
        let Some(data) = self.data() else {
            return;
        };

        let title = format!(" {} (PID {}) ", proc.command, proc.pid);

        // Full command line with wrapping
        let full_cmd = proc.display_command();

        // Find parent process name
        let parent_name = data
            .processes
            .iter()
            .find(|p| p.pid == proc.ppid)
            .map(|p| p.command.as_str())
            .unwrap_or("-");

        // Split area: command takes most space, stats at bottom
        let inner_area = {
            let block = Block::default().title(title.clone()).borders(Borders::TOP);
            block.inner(area)
        };

        let detail_chunks = Layout::vertical([
            Constraint::Min(3),    // Command (wrapped)
            Constraint::Length(2), // Stats lines (now 2 lines)
        ])
        .split(inner_area);

        // Draw the block border
        let block = Block::default().title(title).borders(Borders::TOP);
        frame.render_widget(block, area);

        // Command paragraph with wrapping
        let cmd_para = Paragraph::new(full_cmd)
            .style(Style::default().fg(Color::White))
            .wrap(Wrap { trim: false });
        frame.render_widget(cmd_para, detail_chunks[0]);

        // Stats lines
        let stats_line1 = Line::from(vec![
            Span::styled("State: ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled(
                proc.state.description(),
                Style::default().fg(Self::state_color(&proc.state)),
            ),
            Span::raw("  "),
            Span::styled("PPID: ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw(format!("{} ", proc.ppid)),
            Span::styled("(", Style::default().fg(Color::DarkGray)),
            Span::styled(parent_name, Style::default().fg(Color::Cyan)),
            Span::styled(")", Style::default().fg(Color::DarkGray)),
            Span::raw("  "),
            Span::styled("Threads: ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw(proc.threads.to_string()),
        ]);
        let stats_line2 = Line::from(vec![
            Span::styled("Virt: ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw(proc.virtual_memory_human()),
            Span::raw("  "),
            Span::styled("Res: ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw(proc.resident_memory_human()),
            Span::raw("  "),
            Span::styled("[y] yank", Style::default().fg(Color::DarkGray)),
        ]);
        let stats_para = Paragraph::new(vec![stats_line1, stats_line2]);
        frame.render_widget(stats_para, detail_chunks[1]);
    }

    /// Draw the footer with keybindings
    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let auto_status = if self.auto_refresh {
            Span::styled("ON ", Style::default().fg(Color::Green))
        } else {
            Span::styled("OFF", Style::default().fg(Color::DarkGray))
        };

        let tree_status = if self.tree_view {
            if self.tree_root.is_some() {
                Span::styled("SUB", Style::default().fg(Color::Cyan))
            } else {
                Span::styled("ALL", Style::default().fg(Color::Green))
            }
        } else {
            Span::styled("OFF", Style::default().fg(Color::DarkGray))
        };

        // State filter status
        let filter_status = match &self.state_filter {
            Some(ProcessState::Zombie) => Span::styled(" [Z]", Style::default().fg(Color::Red)),
            Some(ProcessState::DiskSleep) => {
                Span::styled(" [D]", Style::default().fg(Color::Yellow))
            }
            _ => Span::raw(""),
        };

        let line = Line::from(vec![
            Span::styled("[1]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" cpu "),
            Span::styled("[2]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" mem "),
            Span::styled("[t]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" subtree "),
            Span::styled("[T]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" tree:"),
            tree_status,
            Span::raw(" "),
            Span::styled("[z]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" zombie "),
            Span::styled("[d]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" disk"),
            filter_status,
            Span::raw(" "),
            Span::styled("[/]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" filter "),
            Span::styled("[a]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" auto:"),
            auto_status,
            Span::styled("[q]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" back"),
        ]);

        let para = Paragraph::new(line);
        frame.render_widget(para, area);
    }

    /// Draw the filter input bar
    fn draw_filter_bar(&self, frame: &mut Frame, area: Rect) {
        let (match_count, total) = self
            .data()
            .map(|d| (d.display_entries.len(), d.processes.len()))
            .unwrap_or((0, 0));

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
            let loading =
                Paragraph::new("Loading processes...").style(Style::default().fg(Color::DarkGray));
            frame.render_widget(loading, area);
            return Ok(());
        }

        if let Some(err) = self.state.error() {
            let error =
                Paragraph::new(format!("Error: {}", err)).style(Style::default().fg(Color::Red));
            frame.render_widget(error, area);
            return Ok(());
        }

        // Calculate layout - check for any warning condition
        let zombie_count = self.data().map(|d| d.state_counts.zombie).unwrap_or(0);
        let has_warning = zombie_count > 0
            || self.total_memory_usage_percent() > 85.0
            || self.has_high_memory_process().is_some();
        let warning_height = if has_warning { 1 } else { 0 };

        let chunks = Layout::vertical([
            Constraint::Length(1),              // Header
            Constraint::Length(1),              // Summary bar
            Constraint::Length(warning_height), // Warning (if any)
            Constraint::Length(1),              // Filter bar (in filter mode) or spacer
            Constraint::Min(5),                 // Process table
            Constraint::Length(7),              // Detail section (increased for wrapped cmd)
            Constraint::Length(1),              // Footer
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
                // Toggle between CPU% and CPU time
                self.sort_by = match self.sort_by {
                    SortBy::CpuPercent => SortBy::CpuTime,
                    SortBy::CpuTime => SortBy::CpuPercent,
                    _ => SortBy::CpuPercent,
                };
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
                // Toggle subtree from selected process
                if let Some(proc) = self.selected_process() {
                    if self.tree_view && self.tree_root == Some(proc.pid) {
                        // Already in subtree mode for this process - clear it
                        self.tree_view = false;
                        self.tree_root = None;
                    } else {
                        // Enter subtree mode for selected process
                        self.tree_root = Some(proc.pid);
                        self.tree_view = true;
                    }
                    self.rebuild_display_list();
                    self.selected = 0;
                    self.table_state.select(Some(0));
                }
                Ok(None)
            }
            KeyCode::Char('T') => {
                // Full tree from init (pid 1)
                self.tree_root = None;
                self.tree_view = !self.tree_view;
                self.rebuild_display_list();
                self.selected = 0;
                self.table_state.select(Some(0));
                Ok(None)
            }
            KeyCode::Char('z') => {
                // Toggle zombie filter
                if self.state_filter == Some(ProcessState::Zombie) {
                    self.state_filter = None;
                } else {
                    self.state_filter = Some(ProcessState::Zombie);
                }
                self.apply_filter();
                self.selected = 0;
                self.table_state.select(Some(0));
                Ok(None)
            }
            KeyCode::Char('d') => {
                // Toggle disk-wait filter
                if self.state_filter == Some(ProcessState::DiskSleep) {
                    self.state_filter = None;
                } else {
                    self.state_filter = Some(ProcessState::DiskSleep);
                }
                self.apply_filter();
                self.selected = 0;
                self.table_state.select(Some(0));
                Ok(None)
            }
            KeyCode::Char('y') => {
                // Yank (copy) the selected process command to clipboard
                if let Some(proc) = self.selected_process() {
                    let full_cmd = proc.display_command().to_string();
                    let preview_len = full_cmd.len().min(50);
                    let preview = full_cmd[..preview_len].to_string();
                    if crate::clipboard::copy_to_clipboard(full_cmd).is_ok() {
                        tracing::info!("Copied to clipboard: {}...", preview);
                    }
                }
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
