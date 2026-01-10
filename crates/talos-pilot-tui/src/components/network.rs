//! Network Stats component - displays network interface statistics for a node
//!
//! "Is the network the problem?"

use crate::action::Action;
use crate::components::Component;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
    Frame,
};
use std::collections::HashMap;
use std::time::Instant;
use talos_rs::{
    ConnectionCounts, ConnectionInfo, ConnectionState, NetDevRate, NetDevStats, NetstatFilter,
    ServiceInfo, TalosClient,
};

/// Well-known Talos/Kubernetes service ports
fn port_to_service(port: u32) -> Option<&'static str> {
    match port {
        50000 => Some("apid"),
        50001 => Some("trustd"),
        2379 => Some("etcd-client"),
        2380 => Some("etcd-peer"),
        6443 => Some("kube-apiserver"),
        10250 => Some("kubelet"),
        10259 => Some("kube-scheduler"),
        10257 => Some("kube-controller-manager"),
        51821 => Some("kubernetesd"),
        _ => None,
    }
}

/// Auto-refresh interval in seconds (faster than processes for responsive rates)
const AUTO_REFRESH_INTERVAL_SECS: u64 = 2;

/// Sort order for device list
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortBy {
    #[default]
    Traffic,  // rx_bytes + tx_bytes descending
    Errors,   // errors + dropped descending
}

impl SortBy {
    pub fn label(&self) -> &'static str {
        match self {
            SortBy::Traffic => "TRAFFIC",
            SortBy::Errors => "ERRORS",
        }
    }
}

/// View mode for the network component
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    #[default]
    Interfaces,   // Main view showing interfaces
    Connections,  // Drill-down view showing connections
}

/// Sort order for connection list
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnSortBy {
    #[default]
    State,  // Sort by connection state
    Port,   // Sort by local port
}

/// Network stats component for viewing node network interfaces
pub struct NetworkStatsComponent {
    /// Node hostname
    hostname: String,
    /// Node address
    address: String,

    /// Current device statistics
    devices: Vec<NetDevStats>,
    /// Previous device stats (for rate calculation)
    prev_devices: HashMap<String, NetDevStats>,
    /// Calculated rates per device
    rates: HashMap<String, NetDevRate>,
    /// Time of last sample
    last_sample: Option<Instant>,

    /// Selected device index
    selected: usize,
    /// Table state for rendering
    table_state: TableState,
    /// Current sort order
    sort_by: SortBy,

    /// Total RX rate (bytes/sec)
    total_rx_rate: u64,
    /// Total TX rate (bytes/sec)
    total_tx_rate: u64,
    /// Total errors across all devices
    total_errors: u64,
    /// Total dropped across all devices
    total_dropped: u64,

    /// Connection data from netstat
    connections: Vec<ConnectionInfo>,
    /// Connection counts by state
    conn_counts: ConnectionCounts,
    /// Service health status (port -> is_healthy)
    service_health: HashMap<u16, bool>,
    /// Service info from services API (service_id -> ServiceInfo)
    services: HashMap<String, ServiceInfo>,

    /// Loading state
    loading: bool,
    /// Error message
    error: Option<String>,

    /// Auto-refresh enabled
    auto_refresh: bool,
    /// Last refresh time
    last_refresh: Option<Instant>,

    /// Current view mode (Interfaces or Connections drill-down)
    view_mode: ViewMode,
    /// Selected connection index (for Connections view)
    conn_selected: usize,
    /// Connection table state
    conn_table_state: TableState,
    /// Connection sort order
    conn_sort_by: ConnSortBy,
    /// Filter to listening only
    listening_only: bool,

    /// Visual selection anchor (for V mode) - stores connection index
    conn_selection_start: Option<usize>,
    /// Viewport height for connection table (for page navigation)
    conn_viewport_height: u16,

    /// Client for API calls
    client: Option<TalosClient>,
}

impl Default for NetworkStatsComponent {
    fn default() -> Self {
        Self::new("".to_string(), "".to_string())
    }
}

impl NetworkStatsComponent {
    pub fn new(hostname: String, address: String) -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));
        let mut conn_table_state = TableState::default();
        conn_table_state.select(Some(0));

        Self {
            hostname,
            address,
            devices: Vec::new(),
            prev_devices: HashMap::new(),
            rates: HashMap::new(),
            last_sample: None,
            selected: 0,
            table_state,
            sort_by: SortBy::Traffic,
            total_rx_rate: 0,
            total_tx_rate: 0,
            total_errors: 0,
            total_dropped: 0,
            connections: Vec::new(),
            conn_counts: ConnectionCounts::default(),
            service_health: HashMap::new(),
            services: HashMap::new(),
            loading: true,
            error: None,
            auto_refresh: true,
            last_refresh: None,
            view_mode: ViewMode::Interfaces,
            conn_selected: 0,
            conn_table_state,
            conn_sort_by: ConnSortBy::State,
            listening_only: false,
            conn_selection_start: None,
            conn_viewport_height: 20, // Will be updated on draw
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

    /// Refresh network data from the node
    pub async fn refresh(&mut self) -> Result<()> {
        let Some(client) = &self.client else {
            self.set_error("No client configured".to_string());
            return Ok(());
        };

        self.loading = true;

        let timeout = std::time::Duration::from_secs(10);

        // Fetch interface stats, netstat data, and services concurrently
        let dev_future = client.network_device_stats();
        let conn_future = client.netstat(NetstatFilter::All);
        let svc_future = client.services();

        let (dev_result, conn_result, svc_result) = tokio::join!(
            tokio::time::timeout(timeout, dev_future),
            tokio::time::timeout(timeout, conn_future),
            tokio::time::timeout(timeout, svc_future)
        );

        // Process interface stats
        match dev_result {
            Ok(Ok(stats)) => {
                if let Some(node_data) = stats.into_iter().next() {
                    self.update_devices(node_data.devices);
                } else {
                    self.devices.clear();
                    self.rates.clear();
                }
            }
            Ok(Err(e)) => {
                self.set_error(format!("Failed to fetch network stats: {} (node: {})", e, self.address));
                return Ok(());
            }
            Err(_) => {
                self.set_error(format!("Request timed out after {}s", timeout.as_secs()));
                return Ok(());
            }
        }

        // Process netstat data (don't fail if this errors, connection data is supplementary)
        match conn_result {
            Ok(Ok(conn_data)) => {
                if let Some(node_conns) = conn_data.into_iter().next() {
                    self.update_connections(node_conns.connections);
                } else {
                    self.connections.clear();
                    self.conn_counts = ConnectionCounts::default();
                }
            }
            Ok(Err(_)) => {
                // Silently ignore netstat errors - interface data still useful
                self.connections.clear();
                self.conn_counts = ConnectionCounts::default();
            }
            Err(_) => {
                // Timeout on netstat - continue with interface data
                self.connections.clear();
                self.conn_counts = ConnectionCounts::default();
            }
        }

        // Process services data (don't fail if this errors)
        match svc_result {
            Ok(Ok(svc_data)) => {
                if let Some(node_svcs) = svc_data.into_iter().next() {
                    self.services = node_svcs.services
                        .into_iter()
                        .map(|s| (s.id.clone(), s))
                        .collect();
                } else {
                    self.services.clear();
                }
            }
            Ok(Err(_)) | Err(_) => {
                // Silently ignore service errors
                self.services.clear();
            }
        }

        // Update service health based on connection data
        self.update_service_health();

        // Reset selection if needed
        if !self.devices.is_empty() && self.selected >= self.devices.len() {
            self.selected = 0;
        }
        self.table_state.select(Some(self.selected));

        self.loading = false;
        self.error = None;
        self.last_refresh = Some(Instant::now());

        Ok(())
    }

    /// Update connections and calculate counts
    fn update_connections(&mut self, connections: Vec<ConnectionInfo>) {
        self.conn_counts = ConnectionCounts::count_by_state(&connections);
        self.connections = connections;
    }

    /// Update service health indicators based on connection data
    fn update_service_health(&mut self) {
        self.service_health.clear();

        // Key ports to monitor for Kubernetes
        let key_ports: &[u16] = &[6443, 2379, 10250, 10259, 10257];

        for port in key_ports {
            // Check if port is listening
            let is_listening = self.connections.iter().any(|c| {
                c.local_port == *port as u32 && c.state == ConnectionState::Listen
            });
            self.service_health.insert(*port, is_listening);
        }
    }

    /// Update devices and calculate rates
    fn update_devices(&mut self, new_devices: Vec<NetDevStats>) {
        let now = Instant::now();
        let elapsed_secs = self.last_sample
            .map(|t| now.duration_since(t).as_secs_f64())
            .unwrap_or(0.0);

        // Calculate rates if we have previous data
        if elapsed_secs > 0.1 {
            for dev in &new_devices {
                if let Some(prev) = self.prev_devices.get(&dev.name) {
                    let rate = NetDevRate::from_delta(prev, dev, elapsed_secs);
                    self.rates.insert(dev.name.clone(), rate);
                }
            }
        }

        // Store current as previous for next calculation
        self.prev_devices.clear();
        for dev in &new_devices {
            self.prev_devices.insert(dev.name.clone(), dev.clone());
        }
        self.last_sample = Some(now);

        // Calculate totals
        self.total_rx_rate = self.rates.values().map(|r| r.rx_bytes_per_sec).sum();
        self.total_tx_rate = self.rates.values().map(|r| r.tx_bytes_per_sec).sum();
        self.total_errors = new_devices.iter().map(|d| d.total_errors()).sum();
        self.total_dropped = new_devices.iter().map(|d| d.total_dropped()).sum();

        // Sort and store devices
        self.devices = new_devices;
        self.sort_devices();
    }

    /// Sort devices based on current sort order
    fn sort_devices(&mut self) {
        match self.sort_by {
            SortBy::Traffic => {
                // Sort by rate if available, otherwise by cumulative traffic
                self.devices.sort_by(|a, b| {
                    let rate_a = self.rates.get(&a.name).map(|r| r.total_rate()).unwrap_or(0);
                    let rate_b = self.rates.get(&b.name).map(|r| r.total_rate()).unwrap_or(0);
                    if rate_a != rate_b {
                        rate_b.cmp(&rate_a)
                    } else {
                        b.total_traffic().cmp(&a.total_traffic())
                    }
                });
            }
            SortBy::Errors => {
                self.devices.sort_by(|a, b| {
                    let err_a = a.total_errors() + a.total_dropped();
                    let err_b = b.total_errors() + b.total_dropped();
                    err_b.cmp(&err_a)
                });
            }
        }
    }

    /// Navigate to previous device
    fn select_prev(&mut self) {
        if !self.devices.is_empty() && self.selected > 0 {
            self.selected -= 1;
            self.table_state.select(Some(self.selected));
        }
    }

    /// Navigate to next device
    fn select_next(&mut self) {
        if !self.devices.is_empty() {
            self.selected = (self.selected + 1).min(self.devices.len() - 1);
            self.table_state.select(Some(self.selected));
        }
    }

    /// Jump to top of list
    fn select_first(&mut self) {
        if !self.devices.is_empty() {
            self.selected = 0;
            self.table_state.select(Some(self.selected));
        }
    }

    /// Jump to bottom of list
    fn select_last(&mut self) {
        if !self.devices.is_empty() {
            self.selected = self.devices.len() - 1;
            self.table_state.select(Some(self.selected));
        }
    }

    /// Get selected device
    fn selected_device(&self) -> Option<&NetDevStats> {
        self.devices.get(self.selected)
    }

    /// Get rate for a device
    fn get_rate(&self, name: &str) -> Option<&NetDevRate> {
        self.rates.get(name)
    }

    /// Get filtered and sorted connections for display
    fn filtered_connections(&self) -> Vec<&ConnectionInfo> {
        let mut conns: Vec<_> = self.connections.iter()
            .filter(|c| !self.listening_only || c.state == ConnectionState::Listen)
            .collect();

        match self.conn_sort_by {
            ConnSortBy::State => {
                // Sort by state priority: LISTEN, ESTABLISHED, TIME_WAIT, CLOSE_WAIT, others
                conns.sort_by(|a, b| {
                    let priority = |s: &ConnectionState| match s {
                        ConnectionState::Listen => 0,
                        ConnectionState::Established => 1,
                        ConnectionState::TimeWait => 2,
                        ConnectionState::CloseWait => 3,
                        ConnectionState::SynSent => 4,
                        _ => 5,
                    };
                    priority(&a.state).cmp(&priority(&b.state))
                        .then_with(|| a.local_port.cmp(&b.local_port))
                });
            }
            ConnSortBy::Port => {
                conns.sort_by(|a, b| a.local_port.cmp(&b.local_port));
            }
        }

        conns
    }

    /// Navigate to previous connection
    fn conn_select_prev(&mut self) {
        let count = self.filtered_connections().len();
        if count > 0 && self.conn_selected > 0 {
            self.conn_selected -= 1;
            self.conn_table_state.select(Some(self.conn_selected));
        }
    }

    /// Navigate to next connection
    fn conn_select_next(&mut self) {
        let count = self.filtered_connections().len();
        if count > 0 {
            self.conn_selected = (self.conn_selected + 1).min(count - 1);
            self.conn_table_state.select(Some(self.conn_selected));
        }
    }

    /// Jump to first connection
    fn conn_select_first(&mut self) {
        let count = self.filtered_connections().len();
        if count > 0 {
            self.conn_selected = 0;
            self.conn_table_state.select(Some(self.conn_selected));
        }
    }

    /// Jump to last connection
    fn conn_select_last(&mut self) {
        let count = self.filtered_connections().len();
        if count > 0 {
            self.conn_selected = count - 1;
            self.conn_table_state.select(Some(self.conn_selected));
        }
    }

    /// Enter connection drill-down view
    fn enter_connections_view(&mut self) {
        self.view_mode = ViewMode::Connections;
        self.conn_selected = 0;
        self.conn_table_state.select(Some(0));
    }

    /// Return to interfaces view
    fn exit_connections_view(&mut self) {
        self.view_mode = ViewMode::Interfaces;
        self.conn_selection_start = None;
    }

    /// Check if visual selection mode is active for connections
    fn conn_in_visual_mode(&self) -> bool {
        self.conn_selection_start.is_some()
    }

    /// Get the selection range (start, end) inclusive
    fn conn_selection_range(&self) -> Option<(usize, usize)> {
        self.conn_selection_start.map(|anchor| {
            (anchor.min(self.conn_selected), anchor.max(self.conn_selected))
        })
    }

    /// Check if a connection index is within the selection
    fn is_conn_selected(&self, idx: usize) -> bool {
        if let Some((start, end)) = self.conn_selection_range() {
            idx >= start && idx <= end
        } else {
            false
        }
    }

    /// Get service info for a listening connection (if known)
    fn get_service_for_conn(&self, conn: &ConnectionInfo) -> Option<(&'static str, Option<&ServiceInfo>)> {
        if conn.state != ConnectionState::Listen {
            return None;
        }

        // Check if this port maps to a known service
        port_to_service(conn.local_port).map(|service_name| {
            let service_info = self.services.get(service_name);
            (service_name, service_info)
        })
    }

    /// Format a connection as a string for copying
    fn format_connection(conn: &ConnectionInfo) -> String {
        let local = if !conn.local_ip.is_empty() {
            format!("{}:{}", conn.local_ip, conn.local_port)
        } else {
            format!(":{}", conn.local_port)
        };

        let remote = if conn.remote_port > 0 {
            format!("{}:{}", conn.remote_ip, conn.remote_port)
        } else {
            "*:*".to_string()
        };

        let state = match conn.state {
            ConnectionState::Established => "ESTABLISHED",
            ConnectionState::Listen => "LISTEN",
            ConnectionState::TimeWait => "TIME_WAIT",
            ConnectionState::CloseWait => "CLOSE_WAIT",
            ConnectionState::SynSent => "SYN_SENT",
            ConnectionState::SynRecv => "SYN_RECV",
            ConnectionState::FinWait1 => "FIN_WAIT1",
            ConnectionState::FinWait2 => "FIN_WAIT2",
            ConnectionState::Closing => "CLOSING",
            ConnectionState::LastAck => "LAST_ACK",
            ConnectionState::Close => "CLOSE",
            ConnectionState::Unknown => "UNKNOWN",
        };

        // Format process info
        let process = match (&conn.process_name, conn.process_pid) {
            (Some(name), Some(pid)) => format!("{} ({})", name, pid),
            (Some(name), None) => name.clone(),
            (None, Some(pid)) => format!("pid:{}", pid),
            (None, None) => "-".to_string(),
        };

        format!(
            "{:<6} {:<22} {:<24} {:<12} {}",
            conn.protocol, local, remote, state, process
        )
    }

    /// Yank (copy) selected connections or current connection to clipboard
    fn yank_conn_selection(&self) -> (bool, usize) {
        let conns = self.filtered_connections();

        let lines: Vec<String> = if let Some((start, end)) = self.conn_selection_range() {
            // Yank all selected connections
            (start..=end)
                .filter_map(|idx| conns.get(idx).map(|c| Self::format_connection(c)))
                .collect()
        } else {
            // Yank current connection only
            conns.get(self.conn_selected)
                .map(|c| vec![Self::format_connection(c)])
                .unwrap_or_default()
        };

        if lines.is_empty() {
            return (false, 0);
        }

        let count = lines.len();
        let content = lines.join("\n");

        // Copy to clipboard
        let success = crate::clipboard::copy_to_clipboard(content).is_ok();

        (success, count)
    }

    /// Page up in connection list
    fn conn_page_up(&mut self) {
        let page_size = self.conn_viewport_height.saturating_sub(2) as usize;
        self.conn_selected = self.conn_selected.saturating_sub(page_size);
        self.conn_table_state.select(Some(self.conn_selected));
    }

    /// Page down in connection list
    fn conn_page_down(&mut self) {
        let count = self.filtered_connections().len();
        let page_size = self.conn_viewport_height.saturating_sub(2) as usize;
        if count > 0 {
            self.conn_selected = (self.conn_selected + page_size).min(count - 1);
            self.conn_table_state.select(Some(self.conn_selected));
        }
    }

    /// Half page up in connection list
    fn conn_half_page_up(&mut self) {
        let half = (self.conn_viewport_height / 2).max(1) as usize;
        self.conn_selected = self.conn_selected.saturating_sub(half);
        self.conn_table_state.select(Some(self.conn_selected));
    }

    /// Half page down in connection list
    fn conn_half_page_down(&mut self) {
        let count = self.filtered_connections().len();
        let half = (self.conn_viewport_height / 2).max(1) as usize;
        if count > 0 {
            self.conn_selected = (self.conn_selected + half).min(count - 1);
            self.conn_table_state.select(Some(self.conn_selected));
        }
    }

    /// Draw the header
    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let device_count = format!("{} ifaces", self.devices.len());

        let auto_indicator = if self.auto_refresh { "" } else { " [AUTO:OFF]" };

        let spans = vec![
            Span::styled("Network: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(&self.hostname),
            Span::styled(" (", Style::default().fg(Color::DarkGray)),
            Span::raw(&self.address),
            Span::styled(")", Style::default().fg(Color::DarkGray)),
            Span::raw("  "),
            Span::styled(&device_count, Style::default().fg(Color::DarkGray)),
            Span::styled(auto_indicator, Style::default().fg(Color::Yellow)),
        ];

        let header = Paragraph::new(Line::from(spans));
        frame.render_widget(header, area);
    }

    /// Draw the summary bar
    fn draw_summary_bar(&self, frame: &mut Frame, area: Rect) {
        let has_errors = self.total_errors > 0 || self.total_dropped > 0;
        let warning = if has_errors { "! " } else { "" };

        let rx_rate = NetDevStats::format_rate(self.total_rx_rate);
        let tx_rate = NetDevStats::format_rate(self.total_tx_rate);

        let mut spans = vec![
            Span::styled(warning, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled("Total:  ", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled("RX ", Style::default().fg(Color::Green)),
            Span::raw(&rx_rate),
            Span::raw("  "),
            Span::styled("TX ", Style::default().fg(Color::Blue)),
            Span::raw(&tx_rate),
        ];

        // Add errors/dropped if any
        spans.push(Span::raw("   "));
        let errors_style = if self.total_errors > 0 {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(format!("Errors: {}", self.total_errors), errors_style));

        spans.push(Span::raw("   "));
        let dropped_style = if self.total_dropped > 0 {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(format!("Dropped: {}", self.total_dropped), dropped_style));

        let summary = Paragraph::new(Line::from(spans));
        frame.render_widget(summary, area);
    }

    /// Draw the connection summary bar
    fn draw_connection_summary(&self, frame: &mut Frame, area: Rect) {
        let cc = &self.conn_counts;
        let has_warnings = cc.has_warnings();
        let warning = if has_warnings { "! " } else { "" };

        let mut spans = vec![
            Span::styled(warning, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled("Conns: ", Style::default().add_modifier(Modifier::BOLD)),
        ];

        // ESTABLISHED count
        spans.push(Span::styled(
            format!("{} ", cc.established),
            Style::default().fg(Color::Green),
        ));
        spans.push(Span::styled("EST", Style::default().fg(Color::DarkGray)));
        spans.push(Span::raw("  "));

        // LISTEN count
        spans.push(Span::styled(
            format!("{} ", cc.listen),
            Style::default().fg(Color::Cyan),
        ));
        spans.push(Span::styled("LISTEN", Style::default().fg(Color::DarkGray)));
        spans.push(Span::raw("  "));

        // TIME_WAIT count (yellow if > 100)
        let tw_style = if cc.time_wait > 100 {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };
        spans.push(Span::styled(format!("{} ", cc.time_wait), tw_style));
        spans.push(Span::styled("TIME_WAIT", Style::default().fg(Color::DarkGray)));
        spans.push(Span::raw("  "));

        // CLOSE_WAIT count (red if > 0)
        let cw_style = if cc.close_wait > 0 {
            Style::default().fg(Color::Red)
        } else {
            Style::default()
        };
        spans.push(Span::styled(format!("{} ", cc.close_wait), cw_style));
        spans.push(Span::styled("CLOSE_WAIT", Style::default().fg(Color::DarkGray)));

        let summary = Paragraph::new(Line::from(spans));
        frame.render_widget(summary, area);
    }

    /// Draw service health indicators
    fn draw_service_health(&self, frame: &mut Frame, area: Rect) {
        // Define services with their expected ports
        let services = [
            ("API", 6443_u16),
            ("Etcd", 2379),
            ("Kubelet", 10250),
            ("Scheduler", 10259),
            ("Controller", 10257),
        ];

        let mut spans = Vec::new();

        for (name, port) in services {
            let is_healthy = self.service_health.get(&port).copied().unwrap_or(false);
            let indicator = if is_healthy { "●" } else { "○" };
            let color = if is_healthy { Color::Green } else { Color::Red };

            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }

            spans.push(Span::styled(
                format!("{}:{} ", name, port),
                Style::default().fg(Color::DarkGray),
            ));
            spans.push(Span::styled(indicator, Style::default().fg(color)));
        }

        let health = Paragraph::new(Line::from(spans));
        frame.render_widget(health, area);
    }

    /// Draw warning banner if there are errors
    fn draw_warning(&self, frame: &mut Frame, area: Rect) {
        let mut messages = Vec::new();

        // Interface warnings
        if self.total_errors > 0 {
            messages.push(format!("{} interface errors", self.total_errors));
        }
        if self.total_dropped > 0 {
            messages.push(format!("{} dropped", self.total_dropped));
        }

        // Connection warnings
        if self.conn_counts.time_wait > 100 {
            messages.push(format!("High TIME_WAIT ({})", self.conn_counts.time_wait));
        }
        if self.conn_counts.close_wait > 0 {
            messages.push(format!("CLOSE_WAIT ({})", self.conn_counts.close_wait));
        }
        if self.conn_counts.syn_sent > 0 {
            messages.push(format!("SYN_SENT stuck ({})", self.conn_counts.syn_sent));
        }

        if !messages.is_empty() {
            let warning = Paragraph::new(Line::from(vec![
                Span::styled("! ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::styled(messages.join(" | "), Style::default().fg(Color::Yellow)),
            ]));
            frame.render_widget(warning, area);
        }
    }

    /// Draw the device table
    fn draw_device_table(&mut self, frame: &mut Frame, area: Rect) {
        // Build column headers with sort indicators
        let rx_rate_header = if self.sort_by == SortBy::Traffic { "RX RATE▼" } else { "RX RATE" };
        let rx_err_header = if self.sort_by == SortBy::Errors { "RX ERR▼" } else { "RX ERR" };

        let header_cells = [
            Cell::from("INTERFACE"),
            Cell::from(rx_rate_header),
            Cell::from("TX RATE"),
            Cell::from(rx_err_header),
            Cell::from("TX ERR"),
            Cell::from("RX DROP"),
            Cell::from("TX DROP"),
        ];
        let header = Row::new(header_cells)
            .style(Style::default().add_modifier(Modifier::DIM))
            .bottom_margin(1);

        let rows: Vec<Row> = self.devices.iter().enumerate().map(|(idx, dev)| {
            let rate = self.get_rate(&dev.name);
            let rx_rate = rate.map(|r| NetDevStats::format_rate(r.rx_bytes_per_sec))
                .unwrap_or_else(|| "0 B/s".to_string());
            let tx_rate = rate.map(|r| NetDevStats::format_rate(r.tx_bytes_per_sec))
                .unwrap_or_else(|| "0 B/s".to_string());

            let has_errors = dev.has_errors();
            let is_selected = idx == self.selected;

            // Row style based on errors and selection
            let row_style = if has_errors {
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else if is_selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            // Error column styles
            let rx_err_style = if dev.rx_errors > 0 {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let tx_err_style = if dev.tx_errors > 0 {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let rx_drop_style = if dev.rx_dropped > 0 {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let tx_drop_style = if dev.tx_dropped > 0 {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            Row::new([
                Cell::from(dev.name.clone()).style(row_style),
                Cell::from(rx_rate).style(Style::default().fg(Color::Green)),
                Cell::from(tx_rate).style(Style::default().fg(Color::Blue)),
                Cell::from(dev.rx_errors.to_string()).style(rx_err_style),
                Cell::from(dev.tx_errors.to_string()).style(tx_err_style),
                Cell::from(dev.rx_dropped.to_string()).style(rx_drop_style),
                Cell::from(dev.tx_dropped.to_string()).style(tx_drop_style),
            ])
        }).collect();

        let widths = [
            Constraint::Length(14),   // INTERFACE
            Constraint::Length(12),   // RX RATE
            Constraint::Length(12),   // TX RATE
            Constraint::Length(8),    // RX ERR
            Constraint::Length(8),    // TX ERR
            Constraint::Length(8),    // RX DROP
            Constraint::Length(8),    // TX DROP
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

    /// Draw the detail section for selected device
    fn draw_detail_section(&self, frame: &mut Frame, area: Rect) {
        let Some(dev) = self.selected_device() else {
            return;
        };

        let rate = self.get_rate(&dev.name);
        let rx_rate = rate.map(|r| NetDevStats::format_rate(r.rx_bytes_per_sec))
            .unwrap_or_else(|| "0 B/s".to_string());
        let tx_rate = rate.map(|r| NetDevStats::format_rate(r.tx_bytes_per_sec))
            .unwrap_or_else(|| "0 B/s".to_string());

        let rx_total = NetDevStats::format_bytes(dev.rx_bytes);
        let tx_total = NetDevStats::format_bytes(dev.tx_bytes);

        let has_errors = dev.has_errors();
        let border_style = if has_errors {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let lines = vec![
            Line::from(vec![
                Span::styled("RX: ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::raw(format!("{} total", rx_total)),
                Span::styled(format!(" ({})", rx_rate), Style::default().fg(Color::DarkGray)),
                Span::raw(format!("    Packets: {}M", dev.rx_packets / 1_000_000)),
                Span::raw("    "),
                Span::styled(format!("Errors: {}", dev.rx_errors),
                    if dev.rx_errors > 0 { Style::default().fg(Color::Red) } else { Style::default().fg(Color::DarkGray) }),
                Span::raw("    "),
                Span::styled(format!("Dropped: {}", dev.rx_dropped),
                    if dev.rx_dropped > 0 { Style::default().fg(Color::Yellow) } else { Style::default().fg(Color::DarkGray) }),
            ]),
            Line::from(vec![
                Span::styled("TX: ", Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)),
                Span::raw(format!("{} total", tx_total)),
                Span::styled(format!(" ({})", tx_rate), Style::default().fg(Color::DarkGray)),
                Span::raw(format!("    Packets: {}M", dev.tx_packets / 1_000_000)),
                Span::raw("    "),
                Span::styled(format!("Errors: {}", dev.tx_errors),
                    if dev.tx_errors > 0 { Style::default().fg(Color::Red) } else { Style::default().fg(Color::DarkGray) }),
                Span::raw("    "),
                Span::styled(format!("Dropped: {}", dev.tx_dropped),
                    if dev.tx_dropped > 0 { Style::default().fg(Color::Yellow) } else { Style::default().fg(Color::DarkGray) }),
            ]),
        ];

        // Add connection summary line if we have connection data
        let mut lines = lines;
        if !self.connections.is_empty() {
            let cc = &self.conn_counts;
            lines.push(Line::from(vec![
                Span::styled("Connections: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(format!("{} ", cc.established), Style::default().fg(Color::Green)),
                Span::styled("EST  ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{} ", cc.listen), Style::default().fg(Color::Cyan)),
                Span::styled("LISTEN  ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{} ", cc.time_wait),
                    if cc.time_wait > 100 { Style::default().fg(Color::Yellow) } else { Style::default() }),
                Span::styled("TIME_WAIT  ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{} ", cc.close_wait),
                    if cc.close_wait > 0 { Style::default().fg(Color::Red) } else { Style::default() }),
                Span::styled("CLOSE_WAIT", Style::default().fg(Color::DarkGray)),
            ]));
        }

        // Add warning line if there are errors
        if has_errors {
            lines.push(Line::from(vec![
                Span::styled("! ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::styled("Interface has errors - check cable/driver/hardware", Style::default().fg(Color::Yellow)),
            ]));
        }

        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(border_style)
            .title(Span::styled(
                format!(" {} ", dev.name),
                Style::default().add_modifier(Modifier::BOLD),
            ));

        let detail = Paragraph::new(lines).block(block);
        frame.render_widget(detail, area);
    }

    /// Draw the footer with keybindings (interfaces view)
    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let auto_label = if self.auto_refresh { "auto:ON" } else { "auto:OFF" };

        let spans = vec![
            Span::styled("[Enter]", Style::default().fg(Color::Cyan)),
            Span::raw(" connections  "),
            Span::styled("[1]", Style::default().fg(Color::Cyan)),
            Span::raw(" traffic  "),
            Span::styled("[2]", Style::default().fg(Color::Cyan)),
            Span::raw(" errors  "),
            Span::styled("[a]", Style::default().fg(Color::Cyan)),
            Span::raw(format!(" {}  ", auto_label)),
            Span::styled("[q]", Style::default().fg(Color::Cyan)),
            Span::raw(" back"),
        ];

        let footer = Paragraph::new(Line::from(spans))
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(footer, area);
    }

    // ========== Connection Drill-Down View ==========

    /// Draw the connection view header
    fn draw_conn_header(&self, frame: &mut Frame, area: Rect) {
        let conn_count = self.filtered_connections().len();
        let filter_label = if self.listening_only { " [LISTEN ONLY]" } else { "" };

        let mut spans = vec![
            Span::styled("Connections: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(&self.hostname),
            Span::styled(" (", Style::default().fg(Color::DarkGray)),
            Span::raw(&self.address),
            Span::styled(")", Style::default().fg(Color::DarkGray)),
            Span::raw("  "),
            Span::styled(format!("{} connections", conn_count), Style::default().fg(Color::DarkGray)),
            Span::styled(filter_label, Style::default().fg(Color::Yellow)),
        ];

        // Show visual mode indicator
        if let Some((start, end)) = self.conn_selection_range() {
            let count = end - start + 1;
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("-- VISUAL ({} lines) --", count),
                Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            ));
        }

        let header = Paragraph::new(Line::from(spans));
        frame.render_widget(header, area);
    }

    /// Draw the connection summary bar
    fn draw_conn_summary_bar(&self, frame: &mut Frame, area: Rect) {
        let cc = &self.conn_counts;

        let spans = vec![
            Span::styled(format!("{} ", cc.established), Style::default().fg(Color::Green)),
            Span::styled("ESTABLISHED", Style::default().fg(Color::DarkGray)),
            Span::raw("   "),
            Span::styled(format!("{} ", cc.listen), Style::default().fg(Color::Cyan)),
            Span::styled("LISTEN", Style::default().fg(Color::DarkGray)),
            Span::raw("   "),
            Span::styled(format!("{} ", cc.time_wait),
                if cc.time_wait > 100 { Style::default().fg(Color::Yellow) } else { Style::default() }),
            Span::styled("TIME_WAIT", Style::default().fg(Color::DarkGray)),
            Span::raw("   "),
            Span::styled(format!("{} ", cc.close_wait),
                if cc.close_wait > 0 { Style::default().fg(Color::Red) } else { Style::default() }),
            Span::styled("CLOSE_WAIT", Style::default().fg(Color::DarkGray)),
        ];

        let summary = Paragraph::new(Line::from(spans));
        frame.render_widget(summary, area);
    }

    /// Draw the connection table
    fn draw_conn_table(&mut self, frame: &mut Frame, area: Rect) {
        // Update viewport height for page navigation
        self.conn_viewport_height = area.height;

        // Build column headers with sort indicators
        let state_header = if self.conn_sort_by == ConnSortBy::State { "STATE▼" } else { "STATE" };
        let local_header = if self.conn_sort_by == ConnSortBy::Port { "LOCAL▼" } else { "LOCAL" };

        let header_cells = [
            Cell::from("PROTO"),
            Cell::from(local_header),
            Cell::from("REMOTE"),
            Cell::from(state_header),
            Cell::from("PROCESS"),
        ];
        let header = Row::new(header_cells)
            .style(Style::default().add_modifier(Modifier::DIM))
            .bottom_margin(1);

        let conns = self.filtered_connections();
        let in_visual = self.conn_in_visual_mode();

        let rows: Vec<Row> = conns.iter().enumerate().map(|(idx, conn)| {
            // Format local address with IP
            let local = if !conn.local_ip.is_empty() && conn.local_port > 0 {
                format!("{}:{}", conn.local_ip, conn.local_port)
            } else if conn.local_port > 0 {
                format!("*:{}", conn.local_port)
            } else {
                "*:*".to_string()
            };

            // Format remote address
            let remote = if conn.remote_port > 0 {
                format!("{}:{}", conn.remote_ip, conn.remote_port)
            } else {
                "*:*".to_string()
            };

            // Format state with color
            let (state_str, state_color) = match conn.state {
                ConnectionState::Established => ("ESTABLISHED", Color::Green),
                ConnectionState::Listen => ("LISTEN", Color::Cyan),
                ConnectionState::TimeWait => ("TIME_WAIT", if self.conn_counts.time_wait > 100 { Color::Yellow } else { Color::White }),
                ConnectionState::CloseWait => ("CLOSE_WAIT", Color::Red),
                ConnectionState::SynSent => ("SYN_SENT", Color::Yellow),
                ConnectionState::SynRecv => ("SYN_RECV", Color::Yellow),
                ConnectionState::FinWait1 => ("FIN_WAIT1", Color::DarkGray),
                ConnectionState::FinWait2 => ("FIN_WAIT2", Color::DarkGray),
                ConnectionState::Closing => ("CLOSING", Color::DarkGray),
                ConnectionState::LastAck => ("LAST_ACK", Color::DarkGray),
                ConnectionState::Close => ("CLOSE", Color::DarkGray),
                ConnectionState::Unknown => ("UNKNOWN", Color::DarkGray),
            };

            // Format process/service info
            // For listening ports, show service name if known
            let (owner_text, owner_color) = if let Some(service_name) = port_to_service(conn.local_port) {
                // Check service health
                let health_status = self.services.get(service_name)
                    .and_then(|s| s.health.as_ref())
                    .map(|h| if h.healthy { "+" } else { "!" })
                    .unwrap_or("?");

                let process_suffix = conn.process_name.as_ref()
                    .map(|p| format!(" ({})", p))
                    .unwrap_or_default();

                (format!("[{}{}]{}", health_status, service_name, process_suffix), Color::Cyan)
            } else {
                // Regular process info
                let process = conn.process_name.as_ref()
                    .map(|name| {
                        if let Some(pid) = conn.process_pid {
                            format!("{} ({})", name, pid)
                        } else {
                            name.clone()
                        }
                    })
                    .unwrap_or_else(|| "-".to_string());
                (process, Color::Yellow)
            };

            // Check if this row is selected in visual mode
            let is_selected = in_visual && self.is_conn_selected(idx);

            // Row style - highlight selection with magenta background
            let row_style = if is_selected {
                Style::default().bg(Color::Rgb(60, 20, 60)) // Dark magenta
            } else {
                Style::default()
            };

            Row::new([
                Cell::from(conn.protocol.clone()),
                Cell::from(local),
                Cell::from(remote),
                Cell::from(state_str).style(Style::default().fg(state_color)),
                Cell::from(owner_text).style(Style::default().fg(owner_color)),
            ]).style(row_style)
        }).collect();

        let widths = [
            Constraint::Length(6),    // PROTO
            Constraint::Length(22),   // LOCAL (IP:port)
            Constraint::Length(24),   // REMOTE
            Constraint::Length(12),   // STATE
            Constraint::Min(16),      // PROCESS (takes remaining space)
        ];

        let table = Table::new(rows, widths)
            .header(header)
            .row_highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        frame.render_stateful_widget(table, area, &mut self.conn_table_state);
    }

    /// Draw footer for connection view
    fn draw_conn_footer(&self, frame: &mut Frame, area: Rect) {
        let listen_label = if self.listening_only { "all" } else { "listen only" };

        let spans = if self.conn_in_visual_mode() {
            // Visual mode footer
            vec![
                Span::styled("[j/k]", Style::default().fg(Color::Cyan)),
                Span::raw(" extend  "),
                Span::styled("[y]", Style::default().fg(Color::Cyan)),
                Span::raw(" yank  "),
                Span::styled("[Esc/V]", Style::default().fg(Color::Cyan)),
                Span::raw(" cancel  "),
                Span::styled("[q]", Style::default().fg(Color::Cyan)),
                Span::raw(" back"),
            ]
        } else {
            // Normal mode footer
            vec![
                Span::styled("[1]", Style::default().fg(Color::Cyan)),
                Span::raw(" state  "),
                Span::styled("[2]", Style::default().fg(Color::Cyan)),
                Span::raw(" port  "),
                Span::styled("[l]", Style::default().fg(Color::Cyan)),
                Span::raw(format!(" {}  ", listen_label)),
                Span::styled("[V]", Style::default().fg(Color::Cyan)),
                Span::raw(" visual  "),
                Span::styled("[y]", Style::default().fg(Color::Cyan)),
                Span::raw(" yank  "),
                Span::styled("[r]", Style::default().fg(Color::Cyan)),
                Span::raw(" refresh  "),
                Span::styled("[q]", Style::default().fg(Color::Cyan)),
                Span::raw(" back"),
            ]
        };

        let footer = Paragraph::new(Line::from(spans))
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(footer, area);
    }

    /// Draw the selected connection detail section
    fn draw_conn_detail(&self, frame: &mut Frame, area: Rect) {
        let conns = self.filtered_connections();
        let Some(conn) = conns.get(self.conn_selected) else {
            return;
        };

        // Format local address
        let local_addr = if !conn.local_ip.is_empty() {
            format!("{}:{}", conn.local_ip, conn.local_port)
        } else {
            format!("*:{}", conn.local_port)
        };

        // Format remote address
        let remote_addr = if conn.remote_port > 0 {
            format!("{}:{}", conn.remote_ip, conn.remote_port)
        } else {
            "*:*".to_string()
        };

        // State with color
        let (state_str, state_color) = match conn.state {
            ConnectionState::Established => ("ESTABLISHED", Color::Green),
            ConnectionState::Listen => ("LISTEN", Color::Cyan),
            ConnectionState::TimeWait => ("TIME_WAIT", Color::Yellow),
            ConnectionState::CloseWait => ("CLOSE_WAIT", Color::Red),
            ConnectionState::SynSent => ("SYN_SENT", Color::Yellow),
            ConnectionState::SynRecv => ("SYN_RECV", Color::Yellow),
            _ => ("OTHER", Color::DarkGray),
        };

        // Format process info
        let process_info = match (&conn.process_name, conn.process_pid) {
            (Some(name), Some(pid)) => format!("{} (PID: {})", name, pid),
            (Some(name), None) => name.clone(),
            (None, Some(pid)) => format!("PID: {}", pid),
            (None, None) => "-".to_string(),
        };

        // Format namespace
        let netns_info = conn.netns.as_ref()
            .map(|ns| ns.as_str())
            .unwrap_or("host");

        let mut lines = vec![
            Line::from(vec![
                Span::styled("Protocol: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(&conn.protocol),
                Span::raw("   "),
                Span::styled("State: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(state_str, Style::default().fg(state_color).add_modifier(Modifier::BOLD)),
                Span::raw("   "),
                Span::styled("Process: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(&process_info, Style::default().fg(Color::Yellow)),
            ]),
            Line::from(vec![
                Span::styled("Local:  ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::raw(&local_addr),
                Span::raw("   "),
                Span::styled("Remote: ", Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)),
                Span::raw(&remote_addr),
                Span::raw("   "),
                Span::styled("Netns: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(netns_info, Style::default().fg(Color::DarkGray)),
            ]),
            Line::from(vec![
                Span::styled("RX Queue: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(
                    conn.rx_queue.to_string(),
                    if conn.rx_queue > 0 { Style::default().fg(Color::Yellow) } else { Style::default().fg(Color::DarkGray) }
                ),
                Span::raw(" bytes   "),
                Span::styled("TX Queue: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(
                    conn.tx_queue.to_string(),
                    if conn.tx_queue > 0 { Style::default().fg(Color::Yellow) } else { Style::default().fg(Color::DarkGray) }
                ),
                Span::raw(" bytes"),
            ]),
        ];

        // Show service info and available actions
        if let Some(service_name) = port_to_service(conn.local_port) {
            let service_info = self.services.get(service_name);
            let (health_text, health_color) = service_info
                .and_then(|s| s.health.as_ref())
                .map(|h| {
                    if h.healthy {
                        ("healthy", Color::Green)
                    } else {
                        ("unhealthy", Color::Red)
                    }
                })
                .unwrap_or(("unknown", Color::DarkGray));

            let state_text = service_info
                .map(|s| s.state.as_str())
                .unwrap_or("unknown");

            lines.push(Line::from(vec![
                Span::styled("Service: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(service_name, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::raw(" ("),
                Span::styled(state_text, Style::default().fg(Color::DarkGray)),
                Span::raw(", "),
                Span::styled(health_text, Style::default().fg(health_color)),
                Span::raw(")   "),
                Span::styled("[l]", Style::default().fg(Color::Cyan)),
                Span::styled(" logs  ", Style::default().fg(Color::DarkGray)),
                Span::styled("[R]", Style::default().fg(Color::Cyan)),
                Span::styled(" restart", Style::default().fg(Color::DarkGray)),
            ]));
        } else {
            // No service - show process info hint
            lines.push(Line::from(vec![
                Span::styled("Actions: ", Style::default().fg(Color::DarkGray)),
                Span::styled("[y]", Style::default().fg(Color::Cyan)),
                Span::styled(" yank  ", Style::default().fg(Color::DarkGray)),
                Span::styled("[V]", Style::default().fg(Color::Cyan)),
                Span::styled(" visual select", Style::default().fg(Color::DarkGray)),
            ]));
        }

        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                " Selected Connection ",
                Style::default().add_modifier(Modifier::BOLD),
            ));

        let detail = Paragraph::new(lines).block(block);
        frame.render_widget(detail, area);
    }

    /// Draw the connection drill-down view
    fn draw_connections_view(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::vertical([
            Constraint::Length(1),  // Header
            Constraint::Length(1),  // Summary bar
            Constraint::Min(5),     // Connection table
            Constraint::Length(5),  // Detail section (4 lines + border)
            Constraint::Length(1),  // Footer
        ])
        .split(area);

        self.draw_conn_header(frame, chunks[0]);
        self.draw_conn_summary_bar(frame, chunks[1]);
        self.draw_conn_table(frame, chunks[2]);
        self.draw_conn_detail(frame, chunks[3]);
        self.draw_conn_footer(frame, chunks[4]);
    }
}

impl NetworkStatsComponent {
    /// Handle key events in Interfaces view
    fn handle_interfaces_key(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Ok(Some(Action::Back)),
            KeyCode::Enter => {
                if !self.connections.is_empty() {
                    self.enter_connections_view();
                }
                Ok(None)
            }
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
                self.sort_by = SortBy::Traffic;
                self.sort_devices();
                Ok(None)
            }
            KeyCode::Char('2') => {
                self.sort_by = SortBy::Errors;
                self.sort_devices();
                Ok(None)
            }
            KeyCode::Char('r') => Ok(Some(Action::Refresh)),
            KeyCode::Char('a') => {
                self.auto_refresh = !self.auto_refresh;
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Handle key events in Connections view
    fn handle_connections_key(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match key.code {
            // Quit/back
            KeyCode::Char('q') => {
                self.exit_connections_view();
                Ok(None)
            }
            KeyCode::Esc => {
                if self.conn_in_visual_mode() {
                    self.conn_selection_start = None;
                } else {
                    self.exit_connections_view();
                }
                Ok(None)
            }

            // Navigation
            KeyCode::Char('j') | KeyCode::Down => {
                self.conn_select_next();
                Ok(None)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.conn_select_prev();
                Ok(None)
            }
            KeyCode::Char('g') => {
                self.conn_select_first();
                Ok(None)
            }
            KeyCode::Char('G') => {
                self.conn_select_last();
                Ok(None)
            }

            // Page navigation
            KeyCode::PageUp => {
                self.conn_page_up();
                Ok(None)
            }
            KeyCode::PageDown => {
                self.conn_page_down();
                Ok(None)
            }

            // Half-page scroll (Ctrl+U / Ctrl+D)
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.conn_half_page_up();
                Ok(None)
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.conn_half_page_down();
                Ok(None)
            }

            // Sorting
            KeyCode::Char('1') => {
                self.conn_sort_by = ConnSortBy::State;
                self.conn_selected = 0;
                self.conn_table_state.select(Some(0));
                self.conn_selection_start = None;
                Ok(None)
            }
            KeyCode::Char('2') => {
                self.conn_sort_by = ConnSortBy::Port;
                self.conn_selected = 0;
                self.conn_table_state.select(Some(0));
                self.conn_selection_start = None;
                Ok(None)
            }

            // Filter
            KeyCode::Char('l') => {
                self.listening_only = !self.listening_only;
                self.conn_selected = 0;
                self.conn_table_state.select(Some(0));
                self.conn_selection_start = None;
                Ok(None)
            }

            // Visual line selection mode
            KeyCode::Char('V') => {
                if self.conn_in_visual_mode() {
                    self.conn_selection_start = None;
                } else {
                    self.conn_selection_start = Some(self.conn_selected);
                }
                Ok(None)
            }

            // Yank selection or current line to clipboard
            KeyCode::Char('y') => {
                let (success, count) = self.yank_conn_selection();
                if success {
                    tracing::info!("Copied {} connection(s) to clipboard", count);
                }
                // Clear visual selection after yank
                self.conn_selection_start = None;
                Ok(None)
            }

            // Refresh
            KeyCode::Char('r') => Ok(Some(Action::Refresh)),

            _ => Ok(None),
        }
    }

    /// Draw the interfaces view (main view)
    fn draw_interfaces_view(&mut self, frame: &mut Frame, area: Rect) {
        // Build constraints dynamically based on what we need to show
        let has_warning = self.total_errors > 0 || self.total_dropped > 0 || self.conn_counts.has_warnings();
        let has_connections = !self.connections.is_empty();

        let mut constraints = vec![
            Constraint::Length(1),  // Header
            Constraint::Length(1),  // Traffic summary bar
        ];

        if has_connections {
            constraints.push(Constraint::Length(1));  // Connection summary bar
            constraints.push(Constraint::Length(1));  // Service health indicators
        }

        if has_warning {
            constraints.push(Constraint::Length(1));  // Warning
        }

        constraints.push(Constraint::Min(5));     // Device table (takes remaining space)
        constraints.push(Constraint::Length(4));  // Detail section
        constraints.push(Constraint::Length(1));  // Footer

        let chunks = Layout::vertical(constraints).split(area);

        let mut idx = 0;

        // Header
        self.draw_header(frame, chunks[idx]);
        idx += 1;

        // Traffic summary
        self.draw_summary_bar(frame, chunks[idx]);
        idx += 1;

        // Connection data (if available)
        if has_connections {
            self.draw_connection_summary(frame, chunks[idx]);
            idx += 1;
            self.draw_service_health(frame, chunks[idx]);
            idx += 1;
        }

        // Warning (if any)
        if has_warning {
            self.draw_warning(frame, chunks[idx]);
            idx += 1;
        }

        // Device table
        self.draw_device_table(frame, chunks[idx]);
        idx += 1;

        // Detail section
        self.draw_detail_section(frame, chunks[idx]);
        idx += 1;

        // Footer
        self.draw_footer(frame, chunks[idx]);
    }
}

impl Component for NetworkStatsComponent {
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match self.view_mode {
            ViewMode::Interfaces => self.handle_interfaces_key(key),
            ViewMode::Connections => self.handle_connections_key(key),
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
            let loading = Paragraph::new("Loading network stats...")
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

        match self.view_mode {
            ViewMode::Interfaces => self.draw_interfaces_view(frame, area),
            ViewMode::Connections => self.draw_connections_view(frame, area),
        }

        Ok(())
    }
}
