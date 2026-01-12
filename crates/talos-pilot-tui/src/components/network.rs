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
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState},
    Frame,
};
use std::collections::HashMap;
use std::time::Instant;
use talos_pilot_core::format_bytes;
use talos_rs::{
    get_kubespan_peers, is_kubespan_enabled, ConnectionCounts, ConnectionInfo, ConnectionState,
    KubeSpanPeerStatus, NetDevRate, NetDevStats, NetstatFilter, ServiceInfo, TalosClient,
};

/// Well-known Talos/Kubernetes service ports
fn port_to_service(port: u32) -> Option<&'static str> {
    talos_pilot_core::network::port_to_service_u32(port)
}

/// Auto-refresh interval in seconds (faster than processes for responsive rates)
const AUTO_REFRESH_INTERVAL_SECS: u64 = 2;

/// Max capture size before auto-stop (40 MB)
const MAX_CAPTURE_SIZE: usize = 40 * 1024 * 1024;

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
    KubeSpan,     // KubeSpan peer status
}

impl ViewMode {
    /// Get the next view mode (for Tab cycling)
    /// Note: Connections is not part of Tab rotation - it's accessed via Enter
    pub fn next(&self) -> Self {
        match self {
            ViewMode::Interfaces => ViewMode::KubeSpan,
            ViewMode::Connections => ViewMode::KubeSpan, // Exit to KubeSpan if tabbing from connections
            ViewMode::KubeSpan => ViewMode::Interfaces,
        }
    }

    /// Get the previous view mode (for Shift+Tab cycling)
    /// Note: Connections is not part of Tab rotation - it's accessed via Enter
    pub fn prev(&self) -> Self {
        match self {
            ViewMode::Interfaces => ViewMode::KubeSpan,
            ViewMode::Connections => ViewMode::Interfaces, // Exit to Interfaces if shift-tabbing from connections
            ViewMode::KubeSpan => ViewMode::Interfaces,
        }
    }

    /// Get display label for the tab
    pub fn label(&self) -> &'static str {
        match self {
            ViewMode::Interfaces => "Interfaces",
            ViewMode::Connections => "Connections",
            ViewMode::KubeSpan => "KubeSpan",
        }
    }
}

/// Sort order for connection list
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnSortBy {
    #[default]
    State,  // Sort by connection state
    Port,   // Sort by local port
}

/// Pending action requiring confirmation
#[derive(Debug, Clone)]
pub enum PendingAction {
    /// Restart a service (service_id, service_name)
    RestartService(String, String),
}

/// File viewer overlay type
#[derive(Debug, Clone)]
pub enum FileViewerType {
    /// DNS configuration (/etc/resolv.conf)
    DnsConfig,
    /// Routing table (/proc/net/route)
    RoutingTable,
}

/// File viewer overlay state
#[derive(Debug, Clone)]
pub struct FileViewerOverlay {
    /// Type of file being viewed
    pub viewer_type: FileViewerType,
    /// Title for the overlay
    pub title: String,
    /// Content lines
    pub lines: Vec<String>,
    /// Scroll offset
    pub scroll: usize,
}

/// Command output entry for the output pane
#[derive(Debug, Clone)]
pub struct CommandOutput {
    /// Command description (e.g., "Restart apid")
    pub command: String,
    /// Output lines from the command
    pub lines: Vec<String>,
    /// When the command was executed
    pub timestamp: Instant,
    /// Whether the command succeeded
    pub success: bool,
}

/// Packet capture state
#[derive(Debug, Clone, Default)]
pub enum CaptureState {
    #[default]
    Idle,
    /// Capturing packets (interface, start time)
    Capturing(String, Instant),
}

/// Packet capture data holder
#[derive(Debug)]
pub struct CaptureData {
    /// State of the capture
    pub state: CaptureState,
    /// Captured pcap data
    pub data: Vec<u8>,
    /// Receiver for capture stream (Option to allow taking ownership)
    pub receiver: Option<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,
    /// Whether to use BPF filter to exclude API traffic (port 50000)
    /// Recommended to avoid feedback loop on management interface
    pub use_bpf_filter: bool,
}

impl Default for CaptureData {
    fn default() -> Self {
        Self {
            state: CaptureState::default(),
            data: Vec::new(),
            receiver: None,
            use_bpf_filter: true, // ON by default to prevent feedback loops
        }
    }
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
    /// Selected interface name when in Connections view
    selected_interface: Option<String>,
    /// Filtered connections for the selected interface
    filtered_connections: Vec<ConnectionInfo>,
    /// Selected connection index (for Connections view)
    conn_selected: usize,
    /// Connection table state
    conn_table_state: TableState,
    /// Connection sort order
    conn_sort_by: ConnSortBy,
    /// Filter to listening only
    listening_only: bool,
    /// Show all connections (bypass interface filter)
    show_all_connections: bool,

    /// Visual selection anchor (for V mode) - stores connection index
    conn_selection_start: Option<usize>,
    /// Viewport height for connection table (for page navigation)
    conn_viewport_height: u16,

    /// Pending action requiring confirmation
    pending_action: Option<PendingAction>,
    /// Status message to show (e.g., "Service restarted successfully")
    status_message: Option<(String, Instant)>,
    /// Service ID pending restart (to be executed in update)
    pending_restart_service: Option<String>,

    /// Show command output pane at bottom
    show_output_pane: bool,
    /// Command output history (most recent first)
    command_output: Option<CommandOutput>,

    /// File viewer overlay (DNS config, routing table)
    file_viewer: Option<FileViewerOverlay>,

    /// Packet capture state and data
    capture: CaptureData,

    /// Client for API calls
    client: Option<TalosClient>,

    /// KubeSpan peer status data
    kubespan_peers: Vec<KubeSpanPeerStatus>,
    /// KubeSpan enabled status
    kubespan_enabled: Option<bool>,
    /// Selected KubeSpan peer index
    kubespan_selected: usize,
    /// KubeSpan table state
    kubespan_table_state: TableState,
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
            selected_interface: None,
            filtered_connections: Vec::new(),
            conn_selected: 0,
            conn_table_state,
            conn_sort_by: ConnSortBy::State,
            listening_only: false,
            show_all_connections: false,
            conn_selection_start: None,
            conn_viewport_height: 20, // Will be updated on draw
            pending_action: None,
            status_message: None,
            pending_restart_service: None,
            show_output_pane: false,
            command_output: None,
            file_viewer: None,
            capture: CaptureData::default(),
            client: None,
            kubespan_peers: Vec::new(),
            kubespan_enabled: None,
            kubespan_selected: 0,
            kubespan_table_state: {
                let mut state = TableState::default();
                state.select(Some(0));
                state
            },
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

        // Fetch KubeSpan data via talosctl (runs synchronously in blocking task)
        self.refresh_kubespan_data().await;

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

    /// Refresh KubeSpan peer data via talosctl
    async fn refresh_kubespan_data(&mut self) {
        let node = self.address.clone();

        // Run talosctl commands in a blocking task
        let result = tokio::task::spawn_blocking(move || {
            // First check if KubeSpan is enabled
            let enabled = is_kubespan_enabled(&node);
            if !enabled {
                return (Some(false), Vec::new());
            }

            // Fetch peer status
            match get_kubespan_peers(&node) {
                Ok(peers) => (Some(true), peers),
                Err(_) => (Some(true), Vec::new()), // Enabled but no peers or error
            }
        })
        .await;

        match result {
            Ok((enabled, peers)) => {
                self.kubespan_enabled = enabled;
                self.kubespan_peers = peers;

                // Reset selection if needed
                if !self.kubespan_peers.is_empty() && self.kubespan_selected >= self.kubespan_peers.len() {
                    self.kubespan_selected = 0;
                }
                self.kubespan_table_state.select(Some(self.kubespan_selected));
            }
            Err(_) => {
                // Task panicked, leave data as-is
            }
        }
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
    /// Uses pre-filtered connections based on selected interface
    fn filtered_connections(&self) -> Vec<&ConnectionInfo> {
        // Use the pre-filtered list (filtered by interface) unless showing all
        let source = if self.show_all_connections || self.view_mode == ViewMode::Interfaces {
            // Show all connections
            &self.connections
        } else {
            // In connections view, use interface-filtered list
            &self.filtered_connections
        };

        let mut conns: Vec<_> = source.iter()
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

    /// Enter connection drill-down view for the selected interface
    fn enter_connections_view(&mut self) {
        // Store the selected interface name
        self.selected_interface = self.selected_device().map(|d| d.name.clone());

        // Filter connections for this interface
        self.filter_connections_for_interface();

        self.view_mode = ViewMode::Connections;
        self.conn_selected = 0;
        self.conn_table_state.select(Some(0));
    }

    /// Filter connections based on the selected interface
    fn filter_connections_for_interface(&mut self) {
        let Some(ref iface) = self.selected_interface else {
            // No interface selected - show all connections
            self.filtered_connections = self.connections.clone();
            return;
        };

        self.filtered_connections = self.connections.iter()
            .filter(|conn| Self::connection_matches_interface(conn, iface))
            .cloned()
            .collect();
    }

    /// Check if a connection likely uses the given interface
    fn connection_matches_interface(conn: &ConnectionInfo, iface: &str) -> bool {
        match iface {
            // Loopback - connections to/from localhost
            "lo" => {
                conn.local_ip.starts_with("127.") ||
                conn.local_ip == "::1" ||
                conn.remote_ip.starts_with("127.") ||
                conn.remote_ip == "::1"
            }
            // CNI bridge - pod network connections (typically 10.x.x.x)
            "cni0" => {
                conn.local_ip.starts_with("10.") ||
                conn.remote_ip.starts_with("10.")
            }
            // Flannel overlay - also pod network
            name if name.starts_with("flannel") => {
                conn.local_ip.starts_with("10.") ||
                conn.remote_ip.starts_with("10.")
            }
            // Veth pairs - pod connections
            name if name.starts_with("veth") => {
                conn.local_ip.starts_with("10.") ||
                conn.remote_ip.starts_with("10.")
            }
            // Main interface (eth0, enp0s*, etc.) - non-loopback, non-pod connections
            _ => {
                // Exclude loopback
                let is_loopback = conn.local_ip.starts_with("127.") ||
                    conn.local_ip == "::1" ||
                    conn.remote_ip.starts_with("127.") ||
                    conn.remote_ip == "::1";

                // Include all external connections and listeners on 0.0.0.0
                !is_loopback || conn.local_ip == "0.0.0.0" || conn.local_ip == "::"
            }
        }
    }

    /// Return to interfaces view
    fn exit_connections_view(&mut self) {
        self.view_mode = ViewMode::Interfaces;
        self.selected_interface = None;
        self.filtered_connections.clear();
        self.show_all_connections = false;
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

        // Build tab bar for right side (Connections is a subscreen, not a tab)
        let tab_ifaces = if self.view_mode == ViewMode::Interfaces {
            Span::styled("[Interfaces]", Style::default().fg(Color::Cyan))
        } else if self.view_mode == ViewMode::Connections {
            // When in Connections subscreen, show Interfaces as the parent
            Span::styled(" Interfaces>", Style::default().fg(Color::DarkGray))
        } else {
            Span::styled(" Interfaces ", Style::default().fg(Color::DarkGray))
        };
        let tab_kubespan = if self.view_mode == ViewMode::KubeSpan {
            Span::styled("[KubeSpan]", Style::default().fg(Color::Cyan))
        } else {
            Span::styled(" KubeSpan ", Style::default().fg(Color::DarkGray))
        };

        // Show "Connections" indicator when in that subscreen
        let conns_indicator = if self.view_mode == ViewMode::Connections {
            Span::styled("[Connections]", Style::default().fg(Color::Cyan))
        } else {
            Span::raw("")
        };

        let spans = vec![
            Span::styled("Network: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(&self.hostname),
            Span::styled(" (", Style::default().fg(Color::DarkGray)),
            Span::raw(&self.address),
            Span::styled(")", Style::default().fg(Color::DarkGray)),
            Span::raw("  "),
            Span::styled(&device_count, Style::default().fg(Color::DarkGray)),
            Span::styled(auto_indicator, Style::default().fg(Color::Yellow)),
            Span::raw("  │ "),
            tab_ifaces,
            conns_indicator,
            tab_kubespan,
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

        // BPF filter indicator with color coding
        let (bpf_label, bpf_color) = if self.capture.use_bpf_filter {
            ("BPF:ON", Color::Green)
        } else {
            ("BPF:OFF", Color::Yellow)
        };

        let spans = vec![
            Span::styled("[Tab]", Style::default().fg(Color::Cyan)),
            Span::raw(" views  "),
            Span::styled("[Enter]", Style::default().fg(Color::Cyan)),
            Span::raw(" conns  "),
            Span::styled("[c]", Style::default().fg(Color::Cyan)),
            Span::raw(" capture  "),
            Span::styled("[f]", Style::default().fg(Color::Cyan)),
            Span::raw(" "),
            Span::styled(bpf_label, Style::default().fg(bpf_color)),
            Span::raw("  "),
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

        // Show interface name or "all" if showing all connections
        let iface_display = if self.show_all_connections {
            "all".to_string()
        } else {
            self.selected_interface.clone().unwrap_or_else(|| "all".to_string())
        };

        // Connections is a subscreen of Interfaces - show breadcrumb style
        let mut spans = vec![
            Span::styled("Interfaces", Style::default().fg(Color::DarkGray)),
            Span::styled(" > ", Style::default().fg(Color::DarkGray)),
            Span::styled("Connections: ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(&iface_display, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(" @ ", Style::default().fg(Color::DarkGray)),
            Span::raw(&self.hostname),
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
        let listen_label = if self.listening_only { "all" } else { "listen" };
        let all_label = if self.show_all_connections { "iface" } else { "all" };

        // BPF filter indicator with color coding
        let (bpf_label, bpf_color) = if self.capture.use_bpf_filter {
            ("BPF:ON", Color::Green)
        } else {
            ("BPF:OFF", Color::Yellow)
        };

        let spans = if self.conn_in_visual_mode() {
            // Visual mode footer
            vec![
                Span::styled("[j/k]", Style::default().fg(Color::Cyan)),
                Span::raw(" extend  "),
                Span::styled("[y]", Style::default().fg(Color::Cyan)),
                Span::raw(" yank  "),
                Span::styled("[Esc/V]", Style::default().fg(Color::Cyan)),
                Span::raw(" cancel  "),
                Span::styled("[q/Tab]", Style::default().fg(Color::Cyan)),
                Span::raw(" back"),
            ]
        } else {
            // Normal mode footer
            vec![
                Span::styled("[j/k]", Style::default().fg(Color::Cyan)),
                Span::raw(" navigate  "),
                Span::styled("[c]", Style::default().fg(Color::Cyan)),
                Span::raw(" capture  "),
                Span::styled("[f]", Style::default().fg(Color::Cyan)),
                Span::raw(" "),
                Span::styled(bpf_label, Style::default().fg(bpf_color)),
                Span::raw("  "),
                Span::styled("[l]", Style::default().fg(Color::Cyan)),
                Span::raw(format!(" {}  ", listen_label)),
                Span::styled("[a]", Style::default().fg(Color::Cyan)),
                Span::raw(format!(" {}  ", all_label)),
                Span::styled("[q/Tab]", Style::default().fg(Color::Cyan)),
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
                Span::styled("[o]", Style::default().fg(Color::Cyan)),
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

    /// Draw the KubeSpan peer status view
    fn draw_kubespan_view(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::vertical([
            Constraint::Length(1), // Tab bar
            Constraint::Min(5),    // Content
            Constraint::Length(1), // Footer
        ])
        .split(area);

        // Tab bar (only Interfaces and KubeSpan - Connections is a subscreen)
        let tabs = Line::from(vec![
            Span::raw(" "),
            if self.view_mode == ViewMode::Interfaces {
                Span::styled(" Interfaces ", Style::default().fg(Color::Black).bg(Color::Cyan))
            } else {
                Span::styled(" Interfaces ", Style::default().fg(Color::DarkGray))
            },
            Span::raw(" "),
            if self.view_mode == ViewMode::KubeSpan {
                Span::styled(" KubeSpan ", Style::default().fg(Color::Black).bg(Color::Cyan))
            } else {
                Span::styled(" KubeSpan ", Style::default().fg(Color::DarkGray))
            },
        ]);
        frame.render_widget(Paragraph::new(tabs), chunks[0]);

        // Check if KubeSpan is enabled
        match self.kubespan_enabled {
            Some(false) => {
                // KubeSpan not enabled
                self.draw_kubespan_disabled(frame, chunks[1]);
            }
            Some(true) if self.kubespan_peers.is_empty() => {
                // Enabled but no peers
                self.draw_kubespan_no_peers(frame, chunks[1]);
            }
            Some(true) => {
                // Draw peer table and detail
                self.draw_kubespan_peers(frame, chunks[1]);
            }
            None => {
                // Loading/checking status
                self.draw_kubespan_loading(frame, chunks[1]);
            }
        }

        // Footer
        let footer = Line::from(vec![
            Span::styled(" Tab", Style::default().fg(Color::Cyan)),
            Span::raw(" cycle views "),
            Span::styled("j/k", Style::default().fg(Color::Cyan)),
            Span::raw(" navigate "),
            Span::styled("r", Style::default().fg(Color::Cyan)),
            Span::raw(" refresh "),
            Span::styled("q", Style::default().fg(Color::Cyan)),
            Span::raw(" back"),
        ]);
        frame.render_widget(Paragraph::new(footer), chunks[2]);
    }

    /// Draw KubeSpan disabled message
    fn draw_kubespan_disabled(&self, frame: &mut Frame, area: Rect) {
        let content_chunks = Layout::vertical([
            Constraint::Length(2), // Header
            Constraint::Min(5),    // Info section
        ])
        .split(area);

        let header = Paragraph::new(format!(" KubeSpan │ {} ({})", self.hostname, self.address))
            .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
        frame.render_widget(header, content_chunks[0]);

        let info_lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  KubeSpan Status: ", Style::default().fg(Color::Yellow)),
                Span::styled("Not Enabled", Style::default().fg(Color::Gray)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::raw("  KubeSpan provides encrypted WireGuard tunnels between cluster nodes."),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  To enable KubeSpan:", Style::default().fg(Color::Cyan)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("    machine:", Style::default().fg(Color::White)),
            ]),
            Line::from(vec![
                Span::styled("      network:", Style::default().fg(Color::White)),
            ]),
            Line::from(vec![
                Span::styled("        kubespan:", Style::default().fg(Color::White)),
            ]),
            Line::from(vec![
                Span::styled("          enabled: true", Style::default().fg(Color::Green)),
            ]),
        ];

        let info = Paragraph::new(info_lines)
            .block(Block::default().borders(Borders::NONE));
        frame.render_widget(info, content_chunks[1]);
    }

    /// Draw KubeSpan no peers message
    fn draw_kubespan_no_peers(&self, frame: &mut Frame, area: Rect) {
        let content_chunks = Layout::vertical([
            Constraint::Length(2), // Header
            Constraint::Min(5),    // Info section
        ])
        .split(area);

        let header = Paragraph::new(format!(" KubeSpan │ {} ({})", self.hostname, self.address))
            .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
        frame.render_widget(header, content_chunks[0]);

        let info_lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  KubeSpan Status: ", Style::default().fg(Color::Yellow)),
                Span::styled("Enabled", Style::default().fg(Color::Green)),
                Span::raw(" │ "),
                Span::styled("No peers connected", Style::default().fg(Color::Gray)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::raw("  Waiting for other nodes to establish KubeSpan connections."),
            ]),
        ];

        let info = Paragraph::new(info_lines)
            .block(Block::default().borders(Borders::NONE));
        frame.render_widget(info, content_chunks[1]);
    }

    /// Draw KubeSpan loading message
    fn draw_kubespan_loading(&self, frame: &mut Frame, area: Rect) {
        let content_chunks = Layout::vertical([
            Constraint::Length(2), // Header
            Constraint::Min(5),    // Info section
        ])
        .split(area);

        let header = Paragraph::new(format!(" KubeSpan │ {} ({})", self.hostname, self.address))
            .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
        frame.render_widget(header, content_chunks[0]);

        let info_lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  Checking KubeSpan status...", Style::default().fg(Color::Yellow)),
            ]),
        ];

        let info = Paragraph::new(info_lines)
            .block(Block::default().borders(Borders::NONE));
        frame.render_widget(info, content_chunks[1]);
    }

    /// Draw KubeSpan peers table and detail
    fn draw_kubespan_peers(&mut self, frame: &mut Frame, area: Rect) {
        let content_chunks = Layout::vertical([
            Constraint::Length(2),  // Header with summary
            Constraint::Min(8),     // Peer table
            Constraint::Length(7),  // Detail section
        ])
        .split(area);

        // Count connected peers
        let connected = self.kubespan_peers.iter().filter(|p| p.state == "up").count();
        let total = self.kubespan_peers.len();

        // Header with summary
        let header = Line::from(vec![
            Span::styled(
                format!(" KubeSpan │ {} ({}) │ ", self.hostname, self.address),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{}/{} peers connected", connected, total),
                if connected == total {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::Yellow)
                },
            ),
        ]);
        frame.render_widget(Paragraph::new(header), content_chunks[0]);

        // Peer table
        let header_row = Row::new(vec![
            Cell::from("HOSTNAME").style(Style::default().fg(Color::DarkGray)),
            Cell::from("ENDPOINT").style(Style::default().fg(Color::DarkGray)),
            Cell::from("RTT").style(Style::default().fg(Color::DarkGray)),
            Cell::from("STATE").style(Style::default().fg(Color::DarkGray)),
            Cell::from("RX").style(Style::default().fg(Color::DarkGray)),
            Cell::from("TX").style(Style::default().fg(Color::DarkGray)),
        ]);

        let rows: Vec<Row> = self
            .kubespan_peers
            .iter()
            .enumerate()
            .map(|(i, peer)| {
                let is_selected = i == self.kubespan_selected;
                let style = if is_selected {
                    Style::default().bg(Color::DarkGray)
                } else {
                    Style::default()
                };

                let state_style = match peer.state.as_str() {
                    "up" => Style::default().fg(Color::Green),
                    "down" => Style::default().fg(Color::Red),
                    _ => Style::default().fg(Color::Yellow),
                };

                let rtt_str = peer.rtt_ms.map(|r| format!("{:.1}ms", r)).unwrap_or_else(|| "--".to_string());
                let endpoint_str = peer.endpoint.clone().unwrap_or_else(|| "--".to_string());

                Row::new(vec![
                    Cell::from(peer.label.clone()).style(style),
                    Cell::from(endpoint_str).style(style),
                    Cell::from(rtt_str).style(style),
                    Cell::from(peer.state.clone()).style(state_style.patch(style)),
                    Cell::from(format_bytes(peer.rx_bytes)).style(style),
                    Cell::from(format_bytes(peer.tx_bytes)).style(style),
                ])
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Min(20),      // Hostname
                Constraint::Length(22),   // Endpoint
                Constraint::Length(10),   // RTT
                Constraint::Length(10),   // State
                Constraint::Length(10),   // RX
                Constraint::Length(10),   // TX
            ],
        )
        .header(header_row)
        .block(Block::default().borders(Borders::TOP));

        frame.render_stateful_widget(table, content_chunks[1], &mut self.kubespan_table_state);

        // Detail section for selected peer
        if let Some(peer) = self.kubespan_peers.get(self.kubespan_selected) {
            let detail_lines = vec![
                Line::from(vec![
                    Span::styled(" Selected: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(&peer.label, Style::default().fg(Color::Cyan)),
                ]),
                Line::from(vec![
                    Span::styled("   ID: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(&peer.id),
                ]),
                Line::from(vec![
                    Span::styled("   Endpoint: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(peer.endpoint.as_deref().unwrap_or("--")),
                ]),
                Line::from(vec![
                    Span::styled("   Last Handshake: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(peer.last_handshake.as_deref().unwrap_or("--")),
                ]),
                Line::from(vec![
                    Span::styled("   Transfer: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(format!(
                        "RX {} / TX {}",
                        format_bytes(peer.rx_bytes),
                        format_bytes(peer.tx_bytes)
                    )),
                ]),
            ];

            let detail = Paragraph::new(detail_lines)
                .block(Block::default().borders(Borders::TOP));
            frame.render_widget(detail, content_chunks[2]);
        }
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
            KeyCode::Tab => {
                self.view_mode = self.view_mode.next();
                Ok(None)
            }
            KeyCode::BackTab => {
                self.view_mode = self.view_mode.prev();
                Ok(None)
            }
            // Packet capture (uses BPF filter to exclude port 50000, preventing feedback loop)
            KeyCode::Char('c') => {
                self.toggle_capture();
                Ok(None)
            }
            KeyCode::Char('s') => {
                self.save_capture();
                Ok(None)
            }
            // Toggle BPF filter for packet capture
            KeyCode::Char('f') => {
                self.toggle_bpf_filter();
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Handle key events in KubeSpan view
    fn handle_kubespan_key(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Ok(Some(Action::Back)),
            KeyCode::Char('r') => Ok(Some(Action::Refresh)),
            KeyCode::Char('j') | KeyCode::Down => {
                if !self.kubespan_peers.is_empty() {
                    self.kubespan_selected = (self.kubespan_selected + 1) % self.kubespan_peers.len();
                    self.kubespan_table_state.select(Some(self.kubespan_selected));
                }
                Ok(None)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if !self.kubespan_peers.is_empty() {
                    self.kubespan_selected = if self.kubespan_selected == 0 {
                        self.kubespan_peers.len() - 1
                    } else {
                        self.kubespan_selected - 1
                    };
                    self.kubespan_table_state.select(Some(self.kubespan_selected));
                }
                Ok(None)
            }
            KeyCode::Char('g') => {
                if !self.kubespan_peers.is_empty() {
                    self.kubespan_selected = 0;
                    self.kubespan_table_state.select(Some(0));
                }
                Ok(None)
            }
            KeyCode::Char('G') => {
                if !self.kubespan_peers.is_empty() {
                    self.kubespan_selected = self.kubespan_peers.len() - 1;
                    self.kubespan_table_state.select(Some(self.kubespan_selected));
                }
                Ok(None)
            }
            KeyCode::Tab => {
                self.view_mode = self.view_mode.next();
                Ok(None)
            }
            KeyCode::BackTab => {
                self.view_mode = self.view_mode.prev();
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

            // Toggle show all connections (bypass interface filter)
            KeyCode::Char('a') => {
                self.show_all_connections = !self.show_all_connections;
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

            // Open service logs (for known service ports)
            KeyCode::Char('o') => {
                self.open_service_logs()
            }

            // Restart service (for known service ports) - requires confirmation
            KeyCode::Char('R') => {
                self.initiate_service_restart();
                Ok(None)
            }

            // Show DNS config (/etc/resolv.conf)
            KeyCode::Char('d') => {
                self.show_dns_config();
                Ok(Some(Action::Refresh)) // Trigger fetch immediately
            }

            // Show routing table (/proc/net/route)
            KeyCode::Char('t') => {
                self.show_routing_table();
                Ok(Some(Action::Refresh)) // Trigger fetch immediately
            }

            // Tab exits connections view back to Interfaces (connections is a subscreen)
            KeyCode::Tab | KeyCode::BackTab => {
                self.exit_connections_view();
                Ok(None)
            }

            // Packet capture (uses BPF filter to exclude port 50000, preventing feedback loop)
            KeyCode::Char('c') => {
                self.toggle_capture();
                Ok(None)
            }
            KeyCode::Char('s') => {
                self.save_capture();
                Ok(None)
            }
            // Toggle BPF filter for packet capture
            KeyCode::Char('f') => {
                self.toggle_bpf_filter();
                Ok(None)
            }

            _ => Ok(None),
        }
    }

    /// Handle key events when a confirmation is pending
    fn handle_confirmation_key(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                // Confirmed - execute the action
                if let Some(action) = self.pending_action.take() {
                    return self.execute_pending_action(action);
                }
                Ok(None)
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                // Cancelled
                self.pending_action = None;
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Open logs for the currently selected connection's service
    fn open_service_logs(&self) -> Result<Option<Action>> {
        let conns = self.filtered_connections();
        let Some(conn) = conns.get(self.conn_selected) else {
            return Ok(None);
        };

        // Only for known service ports
        let Some(service_name) = port_to_service(conn.local_port) else {
            return Ok(None);
        };

        // Return action to show logs for this service
        let service_vec = vec![service_name.to_string()];
        Ok(Some(Action::ShowMultiLogs(
            self.address.clone(),
            "".to_string(), // role not needed for single service
            service_vec.clone(),
            service_vec, // from network view we only know about this service
        )))
    }

    /// Initiate service restart - sets pending_action for confirmation
    fn initiate_service_restart(&mut self) {
        let conns = self.filtered_connections();
        let Some(conn) = conns.get(self.conn_selected) else {
            return;
        };

        // Only for known service ports
        let Some(service_name) = port_to_service(conn.local_port) else {
            return;
        };

        // Check if service exists
        if !self.services.contains_key(service_name) {
            self.status_message = Some((
                format!("Service '{}' not found", service_name),
                Instant::now(),
            ));
            return;
        }

        // Set pending action - will require confirmation
        self.pending_action = Some(PendingAction::RestartService(
            service_name.to_string(),
            service_name.to_string(),
        ));
    }

    /// Execute a pending action after confirmation
    fn execute_pending_action(&mut self, action: PendingAction) -> Result<Option<Action>> {
        match action {
            PendingAction::RestartService(service_id, service_name) => {
                // Show status message while waiting
                self.status_message = Some((
                    format!("Restarting {}...", service_name),
                    Instant::now(),
                ));

                // Store that we need to restart - will be executed on next refresh
                self.pending_restart_service = Some(service_id);

                // Trigger immediate refresh to execute the restart
                Ok(Some(Action::Refresh))
            }
        }
    }

    /// Draw the interfaces view (main view)
    fn draw_interfaces_view(&mut self, frame: &mut Frame, area: Rect) {
        // Build constraints dynamically based on what we need to show
        let has_warning = self.total_errors > 0 || self.total_dropped > 0 || self.conn_counts.has_warnings();
        let has_connections = !self.connections.is_empty();
        let is_capturing = self.is_capturing();

        let mut constraints = vec![
            Constraint::Length(1),  // Header
            Constraint::Length(1),  // Traffic summary bar
        ];

        if is_capturing {
            constraints.push(Constraint::Length(1));  // Capture status bar
        }

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

        // Capture status bar (when capturing)
        if is_capturing {
            self.draw_capture_status(frame, chunks[idx]);
            idx += 1;
        }

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
        // If file viewer is shown, handle its keys
        if self.file_viewer.is_some() {
            return self.handle_file_viewer_key(key);
        }

        // If output pane is shown, Esc closes it
        if self.show_output_pane {
            if matches!(key.code, KeyCode::Esc) {
                self.show_output_pane = false;
                return Ok(None);
            }
        }

        // If there's a pending confirmation, handle that first
        if self.pending_action.is_some() {
            return self.handle_confirmation_key(key);
        }

        match self.view_mode {
            ViewMode::Interfaces => self.handle_interfaces_key(key),
            ViewMode::Connections => self.handle_connections_key(key),
            ViewMode::KubeSpan => self.handle_kubespan_key(key),
        }
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if let Action::Tick = action {
            // Clear old status messages (after 3 seconds)
            if let Some((_, time)) = &self.status_message {
                if time.elapsed() > std::time::Duration::from_secs(3) {
                    self.status_message = None;
                }
            }

            // Poll packet capture for new data
            if self.is_capturing() {
                self.poll_capture();
            }

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

        // Split area for output pane if shown
        let (main_area, output_area) = if self.show_output_pane {
            let chunks = Layout::vertical([
                Constraint::Fill(1),
                Constraint::Length(10), // Output pane height
            ])
            .split(area);
            (chunks[0], Some(chunks[1]))
        } else {
            (area, None)
        };

        match self.view_mode {
            ViewMode::Interfaces => self.draw_interfaces_view(frame, main_area),
            ViewMode::Connections => self.draw_connections_view(frame, main_area),
            ViewMode::KubeSpan => self.draw_kubespan_view(frame, main_area),
        }

        // Draw output pane if shown
        if let Some(output_rect) = output_area {
            self.draw_output_pane(frame, output_rect);
        }

        // Draw confirmation dialog overlay if pending
        if let Some(ref action) = self.pending_action {
            self.draw_confirmation_dialog(frame, area, action);
        }

        // Draw status message overlay if present
        if let Some((ref msg, _)) = self.status_message {
            self.draw_status_message(frame, area, msg);
        }

        // Draw file viewer overlay if open
        if self.file_viewer.is_some() {
            self.draw_file_viewer(frame, area);
        }

        Ok(())
    }
}

// Service action methods
impl NetworkStatsComponent {
    /// Perform the service restart (called from app.rs after refresh)
    pub async fn perform_pending_restart(&mut self) -> Result<Option<String>> {
        let Some(service_id) = self.pending_restart_service.take() else {
            return Ok(None);
        };

        let Some(client) = &self.client else {
            let output = CommandOutput {
                command: format!("Restart {}", service_id),
                lines: vec!["Error: No client configured".to_string()],
                timestamp: Instant::now(),
                success: false,
            };
            self.command_output = Some(output);
            self.show_output_pane = true;
            return Ok(None);
        };

        match client.service_restart(&service_id).await {
            Ok(results) => {
                let mut lines = Vec::new();
                lines.push(format!("Restarting service: {}", service_id));
                lines.push(String::new());

                if let Some(result) = results.first() {
                    if !result.response.is_empty() {
                        lines.push(format!("Response: {}", result.response));
                    }
                    // Show metadata if available
                    if !result.node.is_empty() {
                        lines.push(format!("Node: {}", result.node));
                    }
                }

                lines.push(String::new());
                lines.push("Service restart initiated successfully.".to_string());
                lines.push("The service will restart momentarily.".to_string());

                let output = CommandOutput {
                    command: format!("Restart {}", service_id),
                    lines,
                    timestamp: Instant::now(),
                    success: true,
                };
                self.command_output = Some(output);
                self.show_output_pane = true;
                self.status_message = None; // Clear status message, using output pane instead

                Ok(Some(format!("Service '{}' restarted", service_id)))
            }
            Err(e) => {
                let lines = vec![
                    format!("Restarting service: {}", service_id),
                    String::new(),
                    format!("Error: {}", e),
                    String::new(),
                    "The service restart failed. Check the error above.".to_string(),
                ];

                let output = CommandOutput {
                    command: format!("Restart {}", service_id),
                    lines,
                    timestamp: Instant::now(),
                    success: false,
                };
                self.command_output = Some(output);
                self.show_output_pane = true;
                self.status_message = None;

                Ok(Some(format!("Failed to restart '{}': {}", service_id, e)))
            }
        }
    }

    /// Check if there's a pending restart
    pub fn has_pending_restart(&self) -> bool {
        self.pending_restart_service.is_some()
    }

    /// Check if file viewer needs content fetched
    pub fn file_viewer_needs_fetch(&self) -> bool {
        self.file_viewer.as_ref()
            .map(|v| v.lines.len() == 1 && v.lines[0] == "Loading...")
            .unwrap_or(false)
    }

    /// Draw the confirmation dialog
    fn draw_confirmation_dialog(&self, frame: &mut Frame, area: Rect, action: &PendingAction) {
        let (title, message) = match action {
            PendingAction::RestartService(_, name) => (
                "Confirm Restart",
                format!("Restart service '{}'?", name),
            ),
        };

        // Center the dialog
        let dialog_width = 40u16;
        let dialog_height = 5u16;
        let x = area.x + (area.width.saturating_sub(dialog_width)) / 2;
        let y = area.y + (area.height.saturating_sub(dialog_height)) / 2;
        let dialog_area = Rect::new(x, y, dialog_width, dialog_height);

        // Clear the area first (removes any text underneath)
        frame.render_widget(Clear, dialog_area);

        // Draw the dialog block with background
        let block = Block::default()
            .title(Span::styled(
                format!(" {} ", title),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .style(Style::default().bg(Color::Black));

        let inner = block.inner(dialog_area);

        let text = vec![
            Line::from(message),
            Line::from(""),
            Line::from(vec![
                Span::styled("[y]", Style::default().fg(Color::Green)),
                Span::raw(" Yes  "),
                Span::styled("[n/Esc]", Style::default().fg(Color::Red)),
                Span::raw(" No"),
            ]),
        ];

        let paragraph = Paragraph::new(text)
            .alignment(ratatui::layout::Alignment::Center)
            .style(Style::default().bg(Color::Black));

        frame.render_widget(block, dialog_area);
        frame.render_widget(paragraph, inner);
    }

    /// Draw the status message as a floating notification
    fn draw_status_message(&self, frame: &mut Frame, area: Rect, message: &str) {
        // Determine the color based on message content
        let (fg_color, border_color) = if message.contains("Failed") || message.contains("Error") {
            (Color::White, Color::Red)
        } else if message.contains("successfully") {
            (Color::White, Color::Green)
        } else {
            (Color::Black, Color::Yellow)
        };

        // Create a floating notification centered near the top
        let msg_width = (message.len() as u16 + 4).min(area.width.saturating_sub(4));
        let msg_height = 3u16;
        let x = area.x + (area.width.saturating_sub(msg_width)) / 2;
        let y = area.y + 2; // Near the top, below header
        let msg_area = Rect::new(x, y, msg_width, msg_height);

        // Clear the area first
        frame.render_widget(Clear, msg_area);

        // Draw the notification box
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .style(Style::default().bg(border_color));

        let inner = block.inner(msg_area);

        let status = Paragraph::new(message)
            .style(Style::default().fg(fg_color).add_modifier(Modifier::BOLD))
            .alignment(ratatui::layout::Alignment::Center);

        frame.render_widget(block, msg_area);
        frame.render_widget(status, inner);
    }

    /// Draw the command output pane at the bottom
    fn draw_output_pane(&self, frame: &mut Frame, area: Rect) {
        let Some(ref output) = self.command_output else {
            return;
        };

        // Determine border color based on success/failure
        let border_color = if output.success {
            Color::Green
        } else {
            Color::Red
        };

        let title = format!(
            " {} {} ",
            if output.success { "✓" } else { "✗" },
            output.command
        );

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(title)
            .title_style(Style::default().fg(border_color).add_modifier(Modifier::BOLD));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Build output lines
        let mut lines: Vec<Line> = output
            .lines
            .iter()
            .map(|l| {
                let style = if l.starts_with("Error:") {
                    Style::default().fg(Color::Red)
                } else if l.contains("successfully") || l.contains("initiated") {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::White)
                };
                Line::from(Span::styled(l.as_str(), style))
            })
            .collect();

        // Add footer hint
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Press Esc to close",
            Style::default().fg(Color::DarkGray),
        )));

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }
}

// File viewer methods
impl NetworkStatsComponent {
    /// Handle key events when file viewer is open
    fn handle_file_viewer_key(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match key.code {
            // Close the viewer
            KeyCode::Esc | KeyCode::Char('q') => {
                self.file_viewer = None;
                Ok(None)
            }
            // Scroll up
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(ref mut viewer) = self.file_viewer {
                    viewer.scroll = viewer.scroll.saturating_sub(1);
                }
                Ok(None)
            }
            // Scroll down
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(ref mut viewer) = self.file_viewer {
                    let max_scroll = viewer.lines.len().saturating_sub(10);
                    viewer.scroll = (viewer.scroll + 1).min(max_scroll);
                }
                Ok(None)
            }
            // Page up / Ctrl+U (half page)
            KeyCode::PageUp => {
                if let Some(ref mut viewer) = self.file_viewer {
                    viewer.scroll = viewer.scroll.saturating_sub(10);
                }
                Ok(None)
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(ref mut viewer) = self.file_viewer {
                    viewer.scroll = viewer.scroll.saturating_sub(10);
                }
                Ok(None)
            }
            // Page down / Ctrl+D (half page)
            KeyCode::PageDown => {
                if let Some(ref mut viewer) = self.file_viewer {
                    let max_scroll = viewer.lines.len().saturating_sub(10);
                    viewer.scroll = (viewer.scroll + 10).min(max_scroll);
                }
                Ok(None)
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(ref mut viewer) = self.file_viewer {
                    let max_scroll = viewer.lines.len().saturating_sub(10);
                    viewer.scroll = (viewer.scroll + 10).min(max_scroll);
                }
                Ok(None)
            }
            // Go to top
            KeyCode::Char('g') => {
                if let Some(ref mut viewer) = self.file_viewer {
                    viewer.scroll = 0;
                }
                Ok(None)
            }
            // Go to bottom
            KeyCode::Char('G') => {
                if let Some(ref mut viewer) = self.file_viewer {
                    viewer.scroll = viewer.lines.len().saturating_sub(10);
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Show DNS configuration overlay
    fn show_dns_config(&mut self) {
        // Request file fetch via action - actual fetch happens in refresh
        self.file_viewer = Some(FileViewerOverlay {
            viewer_type: FileViewerType::DnsConfig,
            title: "DNS Configuration (/etc/resolv.conf)".to_string(),
            lines: vec!["Loading...".to_string()],
            scroll: 0,
        });
    }

    /// Show routing table overlay
    fn show_routing_table(&mut self) {
        self.file_viewer = Some(FileViewerOverlay {
            viewer_type: FileViewerType::RoutingTable,
            title: "Routing Table (/proc/net/route)".to_string(),
            lines: vec!["Loading...".to_string()],
            scroll: 0,
        });
    }

    /// Fetch file content for the file viewer
    pub async fn fetch_file_content(&mut self) {
        // Get viewer type and path first
        let (viewer_type, path) = {
            let Some(ref viewer) = self.file_viewer else {
                return;
            };
            let path = match viewer.viewer_type {
                FileViewerType::DnsConfig => "/etc/resolv.conf",
                FileViewerType::RoutingTable => "/proc/net/route",
            };
            (viewer.viewer_type.clone(), path)
        };

        let Some(client) = &self.client else {
            if let Some(ref mut viewer) = self.file_viewer {
                viewer.lines = vec!["Error: No client configured".to_string()];
            }
            return;
        };

        let result = client.read_file(path).await;

        // Now parse and update
        let lines = match result {
            Ok(content) => match viewer_type {
                FileViewerType::DnsConfig => Self::parse_dns_config_static(&content),
                FileViewerType::RoutingTable => Self::parse_routing_table_static(&content),
            },
            Err(e) => vec![format!("Error reading {}: {}", path, e)],
        };

        if let Some(ref mut viewer) = self.file_viewer {
            viewer.lines = lines;
        }
    }

    /// Parse DNS configuration for display (static version for borrow checker)
    fn parse_dns_config_static(content: &str) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push("─ DNS Configuration ─".to_string());
        lines.push(String::new());

        let mut nameservers = Vec::new();
        let mut search_domains = Vec::new();
        let mut options = Vec::new();

        for line in content.lines() {
            let line = line.trim();
            if line.starts_with('#') || line.is_empty() {
                continue;
            }

            if let Some(ns) = line.strip_prefix("nameserver ") {
                nameservers.push(ns.trim().to_string());
            } else if let Some(search) = line.strip_prefix("search ") {
                search_domains.extend(search.split_whitespace().map(String::from));
            } else if let Some(opt) = line.strip_prefix("options ") {
                options.extend(opt.split_whitespace().map(String::from));
            }
        }

        if !nameservers.is_empty() {
            lines.push("Nameservers:".to_string());
            for ns in &nameservers {
                lines.push(format!("  • {}", ns));
            }
            lines.push(String::new());
        }

        if !search_domains.is_empty() {
            lines.push("Search Domains:".to_string());
            for domain in &search_domains {
                lines.push(format!("  • {}", domain));
            }
            lines.push(String::new());
        }

        if !options.is_empty() {
            lines.push("Options:".to_string());
            for opt in &options {
                lines.push(format!("  • {}", opt));
            }
            lines.push(String::new());
        }

        lines.push("─ Raw Content ─".to_string());
        for line in content.lines() {
            lines.push(format!("  {}", line));
        }

        lines
    }

    /// Parse routing table for display (static version for borrow checker)
    fn parse_routing_table_static(content: &str) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push("─ Routing Table ─".to_string());
        lines.push(String::new());

        // Header
        lines.push(format!(
            "{:<18} {:<18} {:<18} {:>6} {:>6} {:>6} {:<10}",
            "Destination", "Gateway", "Genmask", "Flags", "Metric", "Ref", "Iface"
        ));
        lines.push("─".repeat(90));

        let mut route_lines: Vec<&str> = content.lines().skip(1).collect(); // Skip header
        route_lines.sort(); // Sort for consistent display

        for line in route_lines {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 8 {
                let iface = parts[0];
                let destination = Self::hex_to_ip(parts[1]);
                let gateway = Self::hex_to_ip(parts[2]);
                let flags = parts[3];
                let _refcnt = parts[4];
                let _use = parts[5];
                let metric = parts[6];
                let mask = Self::hex_to_ip(parts[7]);

                // Decode flags
                let flag_val = u32::from_str_radix(flags, 16).unwrap_or(0);
                let flag_str = Self::decode_route_flags(flag_val);

                lines.push(format!(
                    "{:<18} {:<18} {:<18} {:>6} {:>6} {:>6} {:<10}",
                    destination, gateway, mask, flag_str, metric, "0", iface
                ));
            }
        }

        lines.push(String::new());
        lines.push("Flags: U=Up, G=Gateway, H=Host".to_string());

        lines
    }

    /// Convert hex IP to dotted decimal
    fn hex_to_ip(hex: &str) -> String {
        if let Ok(val) = u32::from_str_radix(hex, 16) {
            // Little-endian format in /proc/net/route
            format!(
                "{}.{}.{}.{}",
                val & 0xFF,
                (val >> 8) & 0xFF,
                (val >> 16) & 0xFF,
                (val >> 24) & 0xFF
            )
        } else {
            hex.to_string()
        }
    }

    /// Decode route flags
    fn decode_route_flags(flags: u32) -> String {
        let mut s = String::new();
        if flags & 0x0001 != 0 { s.push('U'); } // RTF_UP
        if flags & 0x0002 != 0 { s.push('G'); } // RTF_GATEWAY
        if flags & 0x0004 != 0 { s.push('H'); } // RTF_HOST
        if s.is_empty() { s.push('-'); }
        s
    }

    /// Draw the file viewer overlay
    fn draw_file_viewer(&self, frame: &mut Frame, area: Rect) {
        let Some(ref viewer) = self.file_viewer else {
            return;
        };

        // Create a centered overlay
        let overlay_width = (area.width * 4 / 5).min(100);
        let overlay_height = (area.height * 3 / 4).min(30);
        let x = area.x + (area.width.saturating_sub(overlay_width)) / 2;
        let y = area.y + (area.height.saturating_sub(overlay_height)) / 2;
        let overlay_area = Rect::new(x, y, overlay_width, overlay_height);

        // Clear the area
        frame.render_widget(Clear, overlay_area);

        // Draw the block
        let block = Block::default()
            .title(Span::styled(
                format!(" {} ", viewer.title),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .style(Style::default().bg(Color::Black));

        let inner = block.inner(overlay_area);
        frame.render_widget(block, overlay_area);

        // Build visible lines
        let visible_height = inner.height.saturating_sub(2) as usize; // Leave room for footer
        let visible_lines: Vec<Line> = viewer
            .lines
            .iter()
            .skip(viewer.scroll)
            .take(visible_height)
            .map(|l| {
                let style = if l.starts_with("─") {
                    Style::default().fg(Color::DarkGray)
                } else if l.starts_with("Error") {
                    Style::default().fg(Color::Red)
                } else if l.starts_with("  •") {
                    Style::default().fg(Color::Green)
                } else if l.contains(':') && !l.contains('.') {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                Line::from(Span::styled(l.as_str(), style))
            })
            .collect();

        let content = Paragraph::new(visible_lines);
        frame.render_widget(content, Rect::new(inner.x, inner.y, inner.width, inner.height.saturating_sub(2)));

        // Draw footer
        let footer_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        let scroll_info = if viewer.lines.len() > visible_height {
            format!(
                "Lines {}-{} of {} | ",
                viewer.scroll + 1,
                (viewer.scroll + visible_height).min(viewer.lines.len()),
                viewer.lines.len()
            )
        } else {
            String::new()
        };

        let footer = Paragraph::new(Line::from(vec![
            Span::styled(&scroll_info, Style::default().fg(Color::DarkGray)),
            Span::styled("[j/k]", Style::default().fg(Color::Cyan)),
            Span::raw(" scroll "),
            Span::styled("[g/G]", Style::default().fg(Color::Cyan)),
            Span::raw(" top/bottom "),
            Span::styled("[q/Esc]", Style::default().fg(Color::Cyan)),
            Span::raw(" close"),
        ]));
        frame.render_widget(footer, footer_area);
    }
}

// Packet capture methods
impl NetworkStatsComponent {
    /// Toggle packet capture on/off
    fn toggle_capture(&mut self) {
        match &self.capture.state {
            CaptureState::Idle => {
                // Get selected interface name
                if let Some(dev) = self.selected_device() {
                    self.capture.state = CaptureState::Capturing(
                        dev.name.clone(),
                        Instant::now(),
                    );
                    self.capture.data.clear();
                    // Actual capture start happens in start_capture_async
                }
            }
            CaptureState::Capturing(..) => {
                // Stop capture
                let bytes = self.capture.data.len();
                self.capture.state = CaptureState::Idle;
                self.capture.receiver = None;

                let bytes_str = NetDevStats::format_bytes(bytes as u64);
                self.status_message = Some((
                    format!("Capture stopped: {} - press 's' to save, open in Wireshark", bytes_str),
                    Instant::now(),
                ));
            }
        }
    }

    /// Start the async capture (called from app.rs)
    pub async fn start_capture_async(&mut self) {
        if let CaptureState::Capturing(ref interface, _) = self.capture.state {
            if self.capture.receiver.is_some() {
                return; // Already started
            }

            let Some(client) = &self.client else {
                self.status_message = Some((
                    "Error: No client configured".to_string(),
                    Instant::now(),
                ));
                self.capture.state = CaptureState::Idle;
                return;
            };

            // Use snap_len=65535 to capture full packets
            // Optionally use BPF filter to exclude port 50000 (Talos API)
            // to prevent feedback loop when capturing on management interface
            let capture_result = if self.capture.use_bpf_filter {
                client.packet_capture_exclude_api(interface, false, 65535).await
            } else {
                client.packet_capture(interface, false, 65535).await
            };

            match capture_result {
                Ok(receiver) => {
                    self.capture.receiver = Some(receiver);
                    let filter_status = if self.capture.use_bpf_filter {
                        " [BPF filter on]"
                    } else {
                        " [NO filter]"
                    };
                    self.status_message = Some((
                        format!("Capturing on {}{} (save with 's')", interface, filter_status),
                        Instant::now(),
                    ));
                }
                Err(e) => {
                    self.status_message = Some((
                        format!("Capture failed: {}", e),
                        Instant::now(),
                    ));
                    self.capture.state = CaptureState::Idle;
                }
            }
        }
    }

    /// Check if capture needs to be started
    pub fn needs_capture_start(&self) -> bool {
        matches!(self.capture.state, CaptureState::Capturing(..)) && self.capture.receiver.is_none()
    }

    /// Poll capture receiver for new data (non-blocking)
    pub fn poll_capture(&mut self) {
        if let Some(ref mut receiver) = self.capture.receiver {
            // Non-blocking receive all available data
            while let Ok(data) = receiver.try_recv() {
                self.capture.data.extend_from_slice(&data);

                // Auto-stop if capture exceeds size limit
                if self.capture.data.len() >= MAX_CAPTURE_SIZE {
                    let bytes_str = NetDevStats::format_bytes(self.capture.data.len() as u64);
                    self.capture.state = CaptureState::Idle;
                    self.capture.receiver = None;
                    self.status_message = Some((
                        format!("Capture auto-stopped at {} - press 's' to save", bytes_str),
                        Instant::now(),
                    ));
                    return;
                }
            }
        }
    }

    /// Check if capture is active
    pub fn is_capturing(&self) -> bool {
        matches!(self.capture.state, CaptureState::Capturing(..))
    }

    /// Save capture data to file (also stops capture if running)
    fn save_capture(&mut self) {
        // Get interface name before stopping
        let interface = match &self.capture.state {
            CaptureState::Capturing(iface, ..) => iface.clone(),
            CaptureState::Idle => "capture".to_string(),
        };

        // Stop capture if running
        if self.is_capturing() {
            self.capture.state = CaptureState::Idle;
            self.capture.receiver = None;
        }

        if self.capture.data.is_empty() {
            self.status_message = Some((
                "No capture data to save".to_string(),
                Instant::now(),
            ));
            return;
        }

        // Generate filename with timestamp
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let filename = format!("{}_{}.pcap", interface, timestamp);
        let path = std::path::Path::new("/tmp").join(&filename);

        let bytes_saved = self.capture.data.len();
        match std::fs::write(&path, &self.capture.data) {
            Ok(_) => {
                let size_str = if bytes_saved >= 1_048_576 {
                    format!("{:.1} MB", bytes_saved as f64 / 1_048_576.0)
                } else if bytes_saved >= 1024 {
                    format!("{:.1} KB", bytes_saved as f64 / 1024.0)
                } else {
                    format!("{} B", bytes_saved)
                };
                self.status_message = Some((
                    format!("Saved {} to {}", size_str, path.display()),
                    Instant::now(),
                ));
                // Clear capture data after successful save
                self.capture.data.clear();
            }
            Err(e) => {
                self.status_message = Some((
                    format!("Save failed: {}", e),
                    Instant::now(),
                ));
            }
        }
    }

    /// Toggle BPF filter on/off for packet capture
    fn toggle_bpf_filter(&mut self) {
        // Only allow toggle when not capturing
        if self.is_capturing() {
            self.status_message = Some((
                "Stop capture first before changing filter setting".to_string(),
                Instant::now(),
            ));
            return;
        }

        self.capture.use_bpf_filter = !self.capture.use_bpf_filter;
        let status = if self.capture.use_bpf_filter {
            "BPF filter ON - excludes API port 50000"
        } else {
            "BPF filter OFF - WARNING: may cause feedback loop on mgmt interface"
        };
        self.status_message = Some((status.to_string(), Instant::now()));
    }

    /// Draw capture status bar at the top of connections view
    fn draw_capture_status(&self, frame: &mut Frame, area: Rect) {
        if let CaptureState::Capturing(ref iface, start) = self.capture.state {
            let duration = start.elapsed().as_secs();
            let mins = duration / 60;
            let secs = duration % 60;
            let bytes = self.capture.data.len() as u64;
            let bytes_str = NetDevStats::format_bytes(bytes);

            let status = Line::from(vec![
                Span::styled(" ● ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                Span::styled("REC ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                Span::styled(iface, Style::default().fg(Color::Cyan)),
                Span::raw("  "),
                Span::styled(format!("{:02}:{:02}", mins, secs), Style::default().fg(Color::Yellow)),
                Span::raw("  "),
                Span::styled(&bytes_str, Style::default().fg(Color::Blue)),
                Span::raw("  "),
                Span::styled("[c]", Style::default().fg(Color::DarkGray)),
                Span::styled(" stop  ", Style::default().fg(Color::DarkGray)),
                Span::styled("[s]", Style::default().fg(Color::DarkGray)),
                Span::styled(" save to /tmp", Style::default().fg(Color::DarkGray)),
            ]);

            let bar = Paragraph::new(status)
                .style(Style::default().bg(Color::Black));
            frame.render_widget(bar, area);
        }
    }
}
