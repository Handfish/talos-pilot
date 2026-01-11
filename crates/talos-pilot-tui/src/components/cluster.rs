//! Cluster component - displays cluster overview with nodes

use crate::action::Action;
use crate::components::Component;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, ListState, Paragraph},
    Frame,
};
use std::collections::HashMap;
use talos_rs::{
    get_address_status, EtcdMemberInfo, MemInfo, NodeCpuInfo, NodeLoadAvg, NodeMemory, NodeServices,
    ServiceInfo, TalosClient, VersionInfo,
};

/// Simple etcd status for header display
#[derive(Debug, Clone, Default)]
struct EtcdSummary {
    /// Number of healthy members
    healthy: usize,
    /// Total number of members
    total: usize,
    /// Whether etcd has quorum
    has_quorum: bool,
}

/// VIP (Virtual IP) info
#[derive(Debug, Clone)]
struct VipInfo {
    /// The VIP address
    address: String,
    /// Node currently holding the VIP
    #[allow(dead_code)]
    holder: String,
}

/// Which pane is currently focused
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FocusedPane {
    #[default]
    Nodes,
    Menu,
    Services,
}

/// Navigation menu items for quick screen access
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavMenuItem {
    Logs,
    Etcd,
    Network,
    Processes,
    Diagnostics,
    Certs,
    Lifecycle,
    Workloads,
}

impl NavMenuItem {
    const ALL: [NavMenuItem; 8] = [
        NavMenuItem::Logs,
        NavMenuItem::Etcd,
        NavMenuItem::Network,
        NavMenuItem::Processes,
        NavMenuItem::Diagnostics,
        NavMenuItem::Certs,
        NavMenuItem::Lifecycle,
        NavMenuItem::Workloads,
    ];

    fn label(&self) -> &'static str {
        match self {
            NavMenuItem::Logs => "Logs",
            NavMenuItem::Etcd => "etcd",
            NavMenuItem::Network => "Net",
            NavMenuItem::Processes => "Proc",
            NavMenuItem::Diagnostics => "Diag",
            NavMenuItem::Certs => "Certs",
            NavMenuItem::Lifecycle => "Life",
            NavMenuItem::Workloads => "Work",
        }
    }

    fn hotkey(&self) -> &'static str {
        match self {
            NavMenuItem::Logs => "L",
            NavMenuItem::Etcd => "e",
            NavMenuItem::Network => "n",
            NavMenuItem::Processes => "p",
            NavMenuItem::Diagnostics => "d",
            NavMenuItem::Certs => "c",
            NavMenuItem::Lifecycle => "y",
            NavMenuItem::Workloads => "w",
        }
    }
}

/// Cluster component showing overview with node list
pub struct ClusterComponent {
    /// Talos client for API calls
    client: Option<TalosClient>,
    /// Context name to use (None = use default context)
    context: Option<String>,
    /// Connection state
    state: ConnectionState,
    /// Version info from nodes
    versions: Vec<VersionInfo>,
    /// Services from nodes
    services: Vec<NodeServices>,
    /// Memory info from nodes
    memory: Vec<NodeMemory>,
    /// Load average from nodes
    load_avg: Vec<NodeLoadAvg>,
    /// CPU info from nodes
    cpu_info: Vec<NodeCpuInfo>,
    /// Etcd members (for extracting node IPs)
    etcd_members: Vec<EtcdMemberInfo>,
    /// Etcd summary for header
    etcd_summary: Option<EtcdSummary>,
    /// Node hostname to IP mapping (discovered from etcd members)
    node_ips: HashMap<String, String>,
    /// Currently selected node index
    selected: usize,
    /// Currently selected service index within the node
    selected_service: usize,
    /// List state for selection
    list_state: ListState,
    /// Error message if any
    error: Option<String>,
    /// Last refresh time
    last_refresh: Option<std::time::Instant>,
    /// VIP info (if configured)
    vip_info: Option<VipInfo>,
    /// Which pane is currently focused
    focused_pane: FocusedPane,
    /// Currently selected navigation menu item
    selected_menu_item: usize,
    /// Auto-refresh enabled
    auto_refresh: bool,
    /// Last auto-refresh time for selected node
    last_auto_refresh: Option<std::time::Instant>,
    /// Whether we've attempted to fetch etcd members (to detect not-bootstrapped state)
    etcd_fetch_attempted: bool,
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
        Self::new(None)
    }
}

impl ClusterComponent {
    pub fn new(context: Option<String>) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));

        Self {
            client: None,
            context,
            state: ConnectionState::Disconnected,
            versions: Vec::new(),
            services: Vec::new(),
            memory: Vec::new(),
            load_avg: Vec::new(),
            cpu_info: Vec::new(),
            etcd_members: Vec::new(),
            etcd_summary: None,
            node_ips: HashMap::new(),
            selected: 0,
            selected_service: 0,
            list_state,
            error: None,
            last_refresh: None,
            vip_info: None,
            focused_pane: FocusedPane::Nodes,
            selected_menu_item: 0,
            auto_refresh: true,
            last_auto_refresh: None,
            etcd_fetch_attempted: false,
        }
    }

    /// Initialize connection to Talos cluster
    pub async fn connect(&mut self) -> Result<()> {
        self.state = ConnectionState::Connecting;

        // Install crypto provider (needed for rustls)
        let _ = rustls::crypto::ring::default_provider().install_default();

        // Use named context if specified, otherwise default
        let client_result = match &self.context {
            Some(ctx_name) => TalosClient::from_named_context(ctx_name).await,
            None => TalosClient::from_default_config().await,
        };

        match client_result {
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
            // First, fetch etcd members to discover node IPs
            // This is critical for Docker provisioner where metadata.hostname is empty
            match client.etcd_members().await {
                Ok(members) => {
                    // Build hostname -> IP mapping (only hostname keys, not IP->IP)
                    self.node_ips.clear();
                    for member in &members {
                        if let Some(ip) = member.ip_address() {
                            // Map hostname to IP
                            self.node_ips.insert(member.hostname.clone(), ip);
                        }
                    }
                    self.etcd_members = members;
                }
                Err(e) => {
                    tracing::warn!("Failed to fetch etcd members: {}", e);
                    // Clear members to indicate not bootstrapped or unreachable
                    self.etcd_members.clear();
                }
            }
            self.etcd_fetch_attempted = true;

            // If we have discovered node IPs, use them to get proper data with hostnames
            // Otherwise fall back to the default (which may have empty hostnames)
            if !self.etcd_members.is_empty() {
                // Get all data from each node using discovered IPs
                let mut versions = Vec::new();
                let mut services = Vec::new();
                let mut memory = Vec::new();
                let mut load_avg = Vec::new();
                let mut cpu_info = Vec::new();

                for member in &self.etcd_members {
                    if let Some(ip) = member.ip_address() {
                        let node_client = client.with_node(&ip);
                        let node_name = if !member.hostname.is_empty() {
                            member.hostname.clone()
                        } else {
                            ip.clone()
                        };

                        // Version
                        if let Ok(mut node_versions) = node_client.version().await {
                            for v in &mut node_versions {
                                v.node = node_name.clone();
                            }
                            versions.extend(node_versions);
                        }

                        // Services
                        if let Ok(mut node_services) = node_client.services().await {
                            for s in &mut node_services {
                                s.node = node_name.clone();
                            }
                            services.extend(node_services);
                        }

                        // Memory
                        if let Ok(mut node_memory) = node_client.memory().await {
                            for m in &mut node_memory {
                                m.node = node_name.clone();
                            }
                            memory.extend(node_memory);
                        }

                        // Load average
                        if let Ok(mut node_load) = node_client.load_avg().await {
                            for l in &mut node_load {
                                l.node = node_name.clone();
                            }
                            load_avg.extend(node_load);
                        }

                        // CPU info
                        if let Ok(mut node_cpu) = node_client.cpu_info().await {
                            for c in &mut node_cpu {
                                c.node = node_name.clone();
                            }
                            cpu_info.extend(node_cpu);
                        }
                    }
                }

                if !versions.is_empty() {
                    self.versions = versions;
                }
                if !services.is_empty() {
                    self.services = services;
                }
                if !memory.is_empty() {
                    self.memory = memory;
                }
                if !load_avg.is_empty() {
                    self.load_avg = load_avg;
                }
                if !cpu_info.is_empty() {
                    self.cpu_info = cpu_info;
                }
            } else {
                // Fallback: fetch without targeting (may have empty hostnames)
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

                match client.load_avg().await {
                    Ok(load_avg) => self.load_avg = load_avg,
                    Err(e) => self.error = Some(format!("LoadAvg error: {}", e)),
                }

                match client.cpu_info().await {
                    Ok(cpu_info) => self.cpu_info = cpu_info,
                    Err(e) => self.error = Some(format!("CPUInfo error: {}", e)),
                }
            }

            // Fetch etcd status for header summary
            match client.etcd_status().await {
                Ok(statuses) => {
                    let total = self.etcd_members.len();
                    let healthy = statuses.len();
                    let quorum_needed = total / 2 + 1;
                    self.etcd_summary = Some(EtcdSummary {
                        healthy,
                        total,
                        has_quorum: healthy >= quorum_needed,
                    });
                }
                Err(_) => {
                    // Don't overwrite existing summary on error
                }
            }

            // Check for VIP (Virtual IP) - look for addresses with vip flag
            self.vip_info = None;
            for member in &self.etcd_members {
                if let Some(ip) = member.ip_address() {
                    // Use talosctl to get address status
                    match get_address_status(&ip) {
                        Ok(addresses) => {
                            // Look for addresses with "vip" flag or on a shared interface
                            for addr in addresses {
                                if addr.flags.iter().any(|f| f.to_lowercase().contains("vip"))
                                    || addr.link_name.contains("vip")
                                {
                                    let holder = if !member.hostname.is_empty() {
                                        member.hostname.clone()
                                    } else {
                                        ip.clone()
                                    };
                                    self.vip_info = Some(VipInfo {
                                        address: addr.address.clone(),
                                        holder,
                                    });
                                    break;
                                }
                            }
                        }
                        Err(_) => {
                            // Silently ignore VIP detection errors
                        }
                    }
                    if self.vip_info.is_some() {
                        break;
                    }
                }
            }

            self.last_refresh = Some(std::time::Instant::now());
        }

        Ok(())
    }

    /// Refresh only the selected node's stats (memory, load, services)
    /// This is lighter weight than a full refresh
    pub async fn refresh_selected_node(&mut self) -> Result<()> {
        let Some(client) = &self.client else {
            return Ok(());
        };

        // Get the selected node's name and IP
        let Some(version) = self.versions.get(self.selected) else {
            return Ok(());
        };
        let node_name = version.node.clone();
        let Some(node_ip) = self.node_ips.get(&node_name).cloned() else {
            return Ok(());
        };

        let node_client = client.with_node(&node_ip);

        // Fetch services, memory, and load for this node
        if let Ok(mut node_services) = node_client.services().await {
            for s in &mut node_services {
                s.node = node_name.clone();
            }
            // Update the services for this node
            self.services.retain(|s| s.node != node_name);
            self.services.extend(node_services);
        }

        if let Ok(mut node_memory) = node_client.memory().await {
            for m in &mut node_memory {
                m.node = node_name.clone();
            }
            self.memory.retain(|m| m.node != node_name);
            self.memory.extend(node_memory);
        }

        if let Ok(mut node_load) = node_client.load_avg().await {
            for l in &mut node_load {
                l.node = node_name.clone();
            }
            self.load_avg.retain(|l| l.node != node_name);
            self.load_avg.extend(node_load);
        }

        self.last_auto_refresh = Some(std::time::Instant::now());
        Ok(())
    }

    /// Check if auto-refresh should trigger (every 5 seconds)
    pub fn should_auto_refresh(&self) -> bool {
        if !self.auto_refresh {
            return false;
        }
        match self.last_auto_refresh {
            None => true,
            Some(last) => last.elapsed().as_secs() >= 5,
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

    /// Get load average for a node
    fn get_node_load_avg(&self, node_name: &str) -> Option<&NodeLoadAvg> {
        self.load_avg
            .iter()
            .find(|l| l.node == node_name || (l.node.is_empty() && node_name.is_empty()))
    }

    /// Get CPU info for a node
    fn get_node_cpu_info(&self, node_name: &str) -> Option<&NodeCpuInfo> {
        self.cpu_info
            .iter()
            .find(|c| c.node == node_name || (c.node.is_empty() && node_name.is_empty()))
    }

    /// Get the currently selected service ID
    pub fn selected_service_id(&self) -> Option<String> {
        if self.versions.is_empty() {
            return None;
        }
        let node_name = &self.versions[self.selected].node;
        self.get_node_services(node_name)
            .and_then(|services| services.get(self.selected_service))
            .map(|s| s.id.clone())
    }

    /// Get a reference to the client for async operations
    pub fn client(&self) -> Option<&TalosClient> {
        self.client.as_ref()
    }

    /// Get service count for current node
    fn current_service_count(&self) -> usize {
        if self.versions.is_empty() {
            return 0;
        }
        let node_name = &self.versions[self.selected].node;
        self.get_node_services(node_name)
            .map(|s| s.len())
            .unwrap_or(0)
    }

    /// Get all service IDs for current node
    fn current_service_ids(&self) -> Vec<String> {
        if self.versions.is_empty() {
            return Vec::new();
        }
        let node_name = &self.versions[self.selected].node;
        self.get_node_services(node_name)
            .map(|services| services.iter().map(|s| s.id.clone()).collect())
            .unwrap_or_default()
    }

    /// Get current node IP/name
    fn current_node_name(&self) -> Option<String> {
        self.versions.get(self.selected).map(|v| v.node.clone())
    }

    /// Determine node role based on services (etcd = controlplane)
    fn current_node_role(&self) -> String {
        let service_ids = self.current_service_ids();
        if service_ids.iter().any(|s| s == "etcd") {
            "controlplane".to_string()
        } else {
            "worker".to_string()
        }
    }

    /// Navigate to the currently selected menu item (1-based index, 0 = on node)
    fn navigate_to_selected_menu(&self) -> Result<Option<Action>> {
        if self.selected_menu_item == 0 || self.selected_menu_item > NavMenuItem::ALL.len() {
            return Ok(None);
        }
        let menu_item = NavMenuItem::ALL[self.selected_menu_item - 1];
        match menu_item {
            NavMenuItem::Logs => {
                // Show all logs for selected node
                if let Some(node_name) = self.current_node_name() {
                    let service_ids = self.current_service_ids();
                    if !service_ids.is_empty() {
                        let node_role = self.current_node_role();
                        Ok(Some(Action::ShowMultiLogs(node_name, node_role, service_ids.clone(), service_ids)))
                    } else {
                        Ok(None)
                    }
                } else {
                    Ok(None)
                }
            }
            NavMenuItem::Etcd => Ok(Some(Action::ShowEtcd)),
            NavMenuItem::Network => {
                if let Some(node_name) = self.current_node_name() {
                    let node_ip = self.node_ips.get(&node_name).cloned().unwrap_or(node_name.clone());
                    Ok(Some(Action::ShowNetwork(node_name, node_ip)))
                } else {
                    Ok(None)
                }
            }
            NavMenuItem::Processes => {
                if let Some(node_name) = self.current_node_name() {
                    let node_ip = self.node_ips.get(&node_name).cloned().unwrap_or(node_name.clone());
                    Ok(Some(Action::ShowProcesses(node_name, node_ip)))
                } else {
                    Ok(None)
                }
            }
            NavMenuItem::Diagnostics => {
                if let Some(node_name) = self.current_node_name() {
                    let node_ip = self.node_ips.get(&node_name).cloned().unwrap_or(node_name.clone());
                    let node_role = self.current_node_role();
                    Ok(Some(Action::ShowDiagnostics(node_name, node_ip, node_role)))
                } else {
                    Ok(None)
                }
            }
            NavMenuItem::Certs => Ok(Some(Action::ShowSecurity)),
            NavMenuItem::Lifecycle => Ok(Some(Action::ShowLifecycle)),
            NavMenuItem::Workloads => Ok(Some(Action::ShowWorkloads)),
        }
    }

}

impl Component for ClusterComponent {
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Ok(Some(Action::Quit)),
            KeyCode::Char('r') => Ok(Some(Action::Refresh)),

            // Vertical navigation within focused pane
            KeyCode::Up | KeyCode::Char('k') => {
                match self.focused_pane {
                    FocusedPane::Nodes => {
                        if self.selected > 0 {
                            self.selected -= 1;
                            self.list_state.select(Some(self.selected));
                        }
                    }
                    FocusedPane::Menu => {
                        if self.selected_menu_item > 1 {
                            self.selected_menu_item -= 1;
                        }
                    }
                    FocusedPane::Services => {
                        let count = self.current_service_count();
                        if count > 0 && self.selected_service > 0 {
                            self.selected_service -= 1;
                        }
                    }
                }
                Ok(None)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                match self.focused_pane {
                    FocusedPane::Nodes => {
                        let node_count = self.versions.len();
                        if node_count > 0 && self.selected < node_count - 1 {
                            self.selected += 1;
                            self.list_state.select(Some(self.selected));
                        }
                    }
                    FocusedPane::Menu => {
                        let menu_count = NavMenuItem::ALL.len();
                        if self.selected_menu_item < menu_count {
                            self.selected_menu_item += 1;
                        }
                    }
                    FocusedPane::Services => {
                        let count = self.current_service_count();
                        if count > 0 && self.selected_service < count - 1 {
                            self.selected_service += 1;
                        }
                    }
                }
                Ok(None)
            }

            // Switch focus between panes: Nodes → Menu → Services → Nodes
            KeyCode::Tab => {
                self.focused_pane = match self.focused_pane {
                    FocusedPane::Nodes => FocusedPane::Menu,
                    FocusedPane::Menu => FocusedPane::Services,
                    FocusedPane::Services => FocusedPane::Nodes,
                };
                // Reset menu selection when entering menu
                if self.focused_pane == FocusedPane::Menu && self.selected_menu_item == 0 {
                    self.selected_menu_item = 1;
                }
                Ok(None)
            }
            KeyCode::BackTab => {
                self.focused_pane = match self.focused_pane {
                    FocusedPane::Nodes => FocusedPane::Services,
                    FocusedPane::Menu => FocusedPane::Nodes,
                    FocusedPane::Services => FocusedPane::Menu,
                };
                if self.focused_pane == FocusedPane::Menu && self.selected_menu_item == 0 {
                    self.selected_menu_item = 1;
                }
                Ok(None)
            }

            // Enter: action depends on focused pane
            KeyCode::Enter => {
                match self.focused_pane {
                    FocusedPane::Nodes => {
                        // On a node - show all logs for that node
                        if let Some(node_name) = self.current_node_name() {
                            let service_ids = self.current_service_ids();
                            if !service_ids.is_empty() {
                                let node_role = self.current_node_role();
                                Ok(Some(Action::ShowMultiLogs(node_name, node_role, service_ids.clone(), service_ids)))
                            } else {
                                Ok(None)
                            }
                        } else {
                            Ok(None)
                        }
                    }
                    FocusedPane::Menu => {
                        // Navigate to selected screen
                        self.navigate_to_selected_menu()
                    }
                    FocusedPane::Services => {
                        // Show logs for selected service (but include all services as available)
                        if let Some(node_name) = self.current_node_name() {
                            if let Some(service_id) = self.selected_service_id() {
                                let node_role = self.current_node_role();
                                let all_services = self.current_service_ids();
                                Ok(Some(Action::ShowMultiLogs(node_name, node_role, vec![service_id], all_services)))
                            } else {
                                Ok(None)
                            }
                        } else {
                            Ok(None)
                        }
                    }
                }
            }

            // 'l' / 'L' - show logs (all services for node)
            KeyCode::Char('l') | KeyCode::Char('L') => {
                if let Some(node_name) = self.current_node_name() {
                    let service_ids = self.current_service_ids();
                    if !service_ids.is_empty() {
                        let node_role = self.current_node_role();
                        Ok(Some(Action::ShowMultiLogs(node_name, node_role, service_ids.clone(), service_ids)))
                    } else {
                        Ok(None)
                    }
                } else {
                    Ok(None)
                }
            }

            // Direct hotkeys for screens (always work)
            KeyCode::Char('e') => Ok(Some(Action::ShowEtcd)),
            KeyCode::Char('p') => {
                if let Some(node_name) = self.current_node_name() {
                    let node_ip = self.node_ips.get(&node_name).cloned().unwrap_or(node_name.clone());
                    Ok(Some(Action::ShowProcesses(node_name, node_ip)))
                } else {
                    Ok(None)
                }
            }
            KeyCode::Char('n') => {
                if let Some(node_name) = self.current_node_name() {
                    let node_ip = self.node_ips.get(&node_name).cloned().unwrap_or(node_name.clone());
                    Ok(Some(Action::ShowNetwork(node_name, node_ip)))
                } else {
                    Ok(None)
                }
            }
            KeyCode::Char('d') => {
                if let Some(node_name) = self.current_node_name() {
                    let node_ip = self.node_ips.get(&node_name).cloned().unwrap_or(node_name.clone());
                    let node_role = self.current_node_role();
                    Ok(Some(Action::ShowDiagnostics(node_name, node_ip, node_role)))
                } else {
                    Ok(None)
                }
            }
            KeyCode::Char('c') => Ok(Some(Action::ShowSecurity)),
            KeyCode::Char('y') => Ok(Some(Action::ShowLifecycle)),
            KeyCode::Char('w') => Ok(Some(Action::ShowWorkloads)),
            KeyCode::Char('o') => {
                // Show node operations overlay for selected node
                if let Some(node_name) = self.current_node_name() {
                    let node_ip = self.node_ips.get(&node_name).cloned().unwrap_or_else(|| node_name.clone());
                    let is_controlplane = self.current_node_role() == "controlplane";
                    Ok(Some(Action::ShowNodeOperations(node_name, node_ip, is_controlplane)))
                } else {
                    Ok(None)
                }
            }

            // Toggle auto-refresh
            KeyCode::Char('a') => {
                self.auto_refresh = !self.auto_refresh;
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
        // Two-column layout inspired by lazygit
        let layout = Layout::vertical([
            Constraint::Length(2), // Header
            Constraint::Min(8),    // Main content (two columns)
            Constraint::Length(2), // Footer
        ])
        .split(area);

        // Draw header
        self.draw_header(frame, layout[0]);

        // Two-column layout for main content
        let content_layout = Layout::horizontal([
            Constraint::Percentage(40), // Nodes pane
            Constraint::Percentage(60), // Details pane
        ])
        .split(layout[1]);

        // Draw panes with focus indication
        self.draw_nodes_pane(frame, content_layout[0]);
        self.draw_details_pane(frame, content_layout[1]);

        // Compact footer with essential controls
        let auto_refresh_status = if self.auto_refresh { "ON" } else { "OFF" };
        let auto_refresh_color = if self.auto_refresh { Color::Green } else { Color::DarkGray };
        let footer_line = Line::from(vec![
            Span::styled(" [j/k]", Style::default().fg(Color::Yellow)),
            Span::styled(" navigate", Style::default().dim()),
            Span::raw("  "),
            Span::styled("[Tab]", Style::default().fg(Color::Yellow)),
            Span::styled(" pane", Style::default().dim()),
            Span::raw("  "),
            Span::styled("[Enter]", Style::default().fg(Color::Yellow)),
            Span::styled(" select", Style::default().dim()),
            Span::raw("  "),
            Span::styled("[l]", Style::default().fg(Color::Yellow)),
            Span::styled(" logs", Style::default().dim()),
            Span::raw("  "),
            Span::styled("[o]", Style::default().fg(Color::Yellow)),
            Span::styled(" ops", Style::default().dim()),
            Span::raw("  "),
            Span::styled("[r]", Style::default().fg(Color::Yellow)),
            Span::styled(" refresh", Style::default().dim()),
            Span::raw("  "),
            Span::styled("[a]", Style::default().fg(Color::Yellow)),
            Span::styled(" auto:", Style::default().dim()),
            Span::styled(auto_refresh_status, Style::default().fg(auto_refresh_color)),
            Span::raw("  "),
            Span::styled("[q]", Style::default().fg(Color::Yellow)),
            Span::styled(" quit", Style::default().dim()),
        ]);
        let footer = Paragraph::new(footer_line)
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
    /// Draw compact header with status indicators
    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let status_indicator = match &self.state {
            ConnectionState::Connected => Span::styled(" ● ", Style::default().fg(Color::Green)),
            ConnectionState::Connecting => Span::styled(" ◐ ", Style::default().fg(Color::Yellow)),
            ConnectionState::Disconnected => Span::styled(" ○ ", Style::default().fg(Color::DarkGray)),
            ConnectionState::Error(_) => Span::styled(" ✗ ", Style::default().fg(Color::Red)),
        };

        let status_text = match &self.state {
            ConnectionState::Connected => "Connected",
            ConnectionState::Connecting => "Connecting...",
            ConnectionState::Disconnected => "Disconnected",
            ConnectionState::Error(e) => e.as_str(),
        };

        // Build etcd status
        let etcd_spans = if let Some(etcd) = &self.etcd_summary {
            let (indicator, color) = if etcd.has_quorum && etcd.healthy == etcd.total {
                ("●", Color::Green)
            } else if etcd.has_quorum {
                ("◐", Color::Yellow)
            } else {
                ("✗", Color::Red)
            };
            vec![
                Span::raw("   etcd "),
                Span::styled(format!("{}/{} ", etcd.healthy, etcd.total), Style::default().fg(color)),
                Span::styled(indicator, Style::default().fg(color)),
            ]
        } else if self.etcd_fetch_attempted && self.etcd_members.is_empty() {
            // Cluster is not bootstrapped (we tried to fetch etcd but got nothing)
            vec![
                Span::raw("   "),
                Span::styled("Not Bootstrapped", Style::default().fg(Color::Yellow)),
                Span::styled(" ○", Style::default().fg(Color::Yellow)),
            ]
        } else {
            vec![]
        };

        // Build VIP status
        let vip_spans = if let Some(vip) = &self.vip_info {
            vec![
                Span::raw("   VIP "),
                Span::styled(&vip.address, Style::default().fg(Color::Cyan)),
            ]
        } else {
            vec![]
        };

        // Context name (use first node's hostname prefix or "cluster")
        let context_name = self.context.clone().unwrap_or_else(|| "cluster".to_string());

        let mut header_spans = vec![
            Span::styled(" talos-pilot ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            status_indicator,
            Span::styled(status_text, Style::default().dim()),
        ];
        header_spans.extend(etcd_spans);
        header_spans.extend(vip_spans);

        // Right-align context name
        let left_content = Line::from(header_spans);
        let right_content = Span::styled(format!(" {} ", context_name), Style::default().fg(Color::DarkGray));

        // Render header
        let header_block = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(Color::DarkGray));

        let inner = header_block.inner(area);
        frame.render_widget(header_block, area);
        frame.render_widget(Paragraph::new(left_content), inner);

        // Right-align context
        let right_area = Rect {
            x: area.x + area.width.saturating_sub(context_name.len() as u16 + 3),
            y: area.y,
            width: context_name.len() as u16 + 3,
            height: 1,
        };
        frame.render_widget(Paragraph::new(right_content), right_area);
    }

    /// Draw the nodes pane (left column) with navigation menu below
    fn draw_nodes_pane(&self, frame: &mut Frame, area: Rect) {
        // Focus indication - cyan border when focused
        let border_color = if self.focused_pane == FocusedPane::Nodes {
            Color::Cyan
        } else {
            Color::DarkGray
        };

        let block = Block::default()
            .title(" Nodes ")
            .title_style(Style::default().fg(if self.focused_pane == FocusedPane::Nodes { Color::Cyan } else { Color::White }))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Split inner area: nodes list at top, nav menu at bottom
        let menu_height = NavMenuItem::ALL.len() as u16 + 1; // +1 for separator
        let pane_layout = Layout::vertical([
            Constraint::Min(3),              // Nodes list
            Constraint::Length(menu_height), // Navigation menu (vertical)
        ])
        .split(inner);

        if self.versions.is_empty() {
            let msg = Paragraph::new(Line::from(Span::styled(
                "  No nodes connected",
                Style::default().dim(),
            )));
            frame.render_widget(msg, pane_layout[0]);
        } else {
            // Build node list with inline stats
            let mut lines = Vec::new();

            for (i, v) in self.versions.iter().enumerate() {
                let node_name = if v.node.is_empty() { "node-0".to_string() } else { v.node.clone() };

                // Health indicator based on services and memory
                let mem_pct = self.get_node_memory(&v.node)
                    .map(|m| m.usage_percent())
                    .unwrap_or(0.0);
                let svc_healthy = self.get_node_services(&v.node)
                    .map(|services| services.iter().all(|s| s.health.as_ref().map(|h| h.healthy).unwrap_or(true)))
                    .unwrap_or(true);
                let health_symbol = if svc_healthy && mem_pct < 90.0 { "●" } else { "◐" };
                let health_color = if svc_healthy && mem_pct < 90.0 { Color::Green } else { Color::Yellow };

                // Selection indicator - only show when Nodes pane is focused
                let is_selected = i == self.selected;
                let show_selector = is_selected && self.focused_pane == FocusedPane::Nodes;
                let selector = if show_selector { "▸" } else { " " };
                let name_style = if is_selected {
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };

                lines.push(Line::from(vec![
                    Span::styled(format!(" {} {} ", selector, health_symbol), Style::default().fg(health_color)),
                    Span::styled(node_name, name_style),
                ]));
            }

            frame.render_widget(Paragraph::new(lines), pane_layout[0]);
        }

        // Draw navigation menu
        self.draw_nav_menu(frame, pane_layout[1]);
    }

    /// Draw the navigation menu (vertical list)
    fn draw_nav_menu(&self, frame: &mut Frame, area: Rect) {
        let menu_focused = self.focused_pane == FocusedPane::Menu;
        let mut lines = Vec::new();

        // Separator line with focus color
        let sep_color = if menu_focused { Color::Cyan } else { Color::DarkGray };
        let sep_text = if menu_focused {
            " Navigate ".to_string()
        } else {
            "─".repeat(area.width as usize)
        };
        lines.push(Line::from(Span::styled(sep_text, Style::default().fg(sep_color))));

        // Menu items (1-indexed, 0 means not in menu)
        for (i, item) in NavMenuItem::ALL.iter().enumerate() {
            let menu_index = i + 1; // 1-based for selection
            let is_selected = menu_index == self.selected_menu_item;
            let show_selector = is_selected && menu_focused;

            let selector = if show_selector { "▸" } else { " " };

            let hotkey_style = if show_selector {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default().fg(Color::Yellow)
            };

            let label_style = if show_selector {
                Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else if is_selected && !menu_focused {
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };

            lines.push(Line::from(vec![
                Span::styled(format!(" {} ", selector), if show_selector { Style::default().fg(Color::Cyan) } else { Style::default() }),
                Span::styled(format!("[{}] ", item.hotkey()), hotkey_style),
                Span::styled(item.label(), label_style),
            ]));
        }

        frame.render_widget(Paragraph::new(lines), area);
    }

    /// Render a compact ASCII bar for percentage values
    fn render_compact_bar(pct: f32, width: usize) -> String {
        let filled = ((pct / 100.0) * width as f32).round() as usize;
        let empty = width.saturating_sub(filled);
        format!("{}{}{:>3}%", "█".repeat(filled), "░".repeat(empty), pct as u8)
    }

    /// Draw the details pane (right column)
    fn draw_details_pane(&self, frame: &mut Frame, area: Rect) {
        // Focus indication - cyan border when focused
        let border_color = if self.focused_pane == FocusedPane::Services {
            Color::Cyan
        } else {
            Color::DarkGray
        };

        // Show bootstrap guidance if cluster is not bootstrapped
        if self.etcd_fetch_attempted && self.etcd_members.is_empty() {
            let block = Block::default()
                .title(" Bootstrap Required ")
                .title_style(Style::default().fg(Color::Yellow))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color));

            // Get control plane IP from talosconfig endpoints
            let cp_ip = talos_rs::TalosConfig::load_default()
                .ok()
                .and_then(|config| {
                    config.current_context()
                        .and_then(|ctx| ctx.endpoints.first())
                        .map(|e| e.split(':').next().unwrap_or(e).to_string())
                })
                .unwrap_or_else(|| "<control-plane-ip>".to_string());

            let lines = vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled("  Cluster not yet bootstrapped.", Style::default().fg(Color::Yellow)),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  To bootstrap, run:", Style::default().dim()),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled(format!("    talosctl bootstrap -n {}", cp_ip), Style::default().fg(Color::Cyan)),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  This initializes etcd and starts", Style::default().dim()),
                ]),
                Line::from(vec![
                    Span::styled("  the Kubernetes control plane.", Style::default().dim()),
                ]),
            ];

            let msg = Paragraph::new(lines).block(block);
            frame.render_widget(msg, area);
            return;
        }

        if self.versions.is_empty() {
            let block = Block::default()
                .title(" Details ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color));
            let msg = Paragraph::new(Line::from(Span::styled(
                "  No node selected",
                Style::default().dim(),
            ))).block(block);
            frame.render_widget(msg, area);
            return;
        }

        let version = &self.versions[self.selected];
        let node_name = if version.node.is_empty() { "node-0" } else { &version.node };
        let node_ip = self.node_ips.get(node_name).cloned().unwrap_or_default();
        let role = if self.get_node_services(&version.node)
            .map(|s| s.iter().any(|svc| svc.id == "etcd"))
            .unwrap_or(false)
        { "controlplane" } else { "worker" };

        let title = format!(" {} · {} ", node_name, role);
        let block = Block::default()
            .title(title)
            .title_style(Style::default().fg(if self.focused_pane == FocusedPane::Services { Color::Cyan } else { Color::White }))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Split into resources and services
        let panel_layout = Layout::vertical([
            Constraint::Length(5), // Resources
            Constraint::Min(4),    // Services
        ])
        .split(inner);

        // Resources section
        let mut resource_lines = vec![
            Line::from(vec![
                Span::styled(" IP: ", Style::default().dim()),
                Span::styled(&node_ip, Style::default().fg(Color::DarkGray)),
            ]),
        ];

        // Memory bar
        if let Some(mem) = self.get_node_memory(&version.node) {
            let pct = mem.usage_percent();
            let used_gb = (mem.mem_total - mem.mem_available) as f64 / 1024.0 / 1024.0 / 1024.0;
            let total_gb = mem.mem_total as f64 / 1024.0 / 1024.0 / 1024.0;
            let bar = Self::render_compact_bar(pct, 10);
            let color = if pct > 90.0 { Color::Red } else if pct > 70.0 { Color::Yellow } else { Color::Green };
            resource_lines.push(Line::from(vec![
                Span::styled(" Memory: ", Style::default().dim()),
                Span::styled(bar, Style::default().fg(color)),
                Span::styled(format!(" {:.1}/{:.1}GB", used_gb, total_gb), Style::default().dim()),
            ]));
        }

        // Load average
        if let Some(load) = self.get_node_load_avg(&version.node) {
            let color = if load.load1 > 4.0 { Color::Red } else if load.load1 > 2.0 { Color::Yellow } else { Color::Green };
            resource_lines.push(Line::from(vec![
                Span::styled(" Load:   ", Style::default().dim()),
                Span::styled(format!("{:.2}", load.load1), Style::default().fg(color)),
                Span::styled(format!(" {:.2} {:.2} (1/5/15m)", load.load5, load.load15), Style::default().dim()),
            ]));
        }

        // CPU info
        if let Some(cpu) = self.get_node_cpu_info(&version.node) {
            resource_lines.push(Line::from(vec![
                Span::styled(" CPU:    ", Style::default().dim()),
                Span::styled(format!("{} cores", cpu.cpu_count), Style::default().fg(Color::White)),
                Span::styled(format!(" @ {:.0}MHz", cpu.mhz), Style::default().dim()),
            ]));
        }

        frame.render_widget(Paragraph::new(resource_lines), panel_layout[0]);

        // Services section
        if let Some(services) = self.get_node_services(&version.node) {
            let running = services.iter().filter(|s| s.state == "Running").count();
            let mut svc_lines = vec![
                Line::from(vec![
                    Span::styled(format!(" Services ({}/{})", running, services.len()), Style::default().fg(Color::Gray)),
                ]),
            ];

            for (i, svc) in services.iter().enumerate() {
                let health_symbol = svc.health.as_ref()
                    .map(|h| if h.healthy { "●" } else { "○" })
                    .unwrap_or("●");
                let health_color = if health_symbol == "●" { Color::Green } else { Color::Red };

                // Highlight selected service when services pane is focused
                let is_selected = i == self.selected_service && self.focused_pane == FocusedPane::Services;
                let name_style = if is_selected {
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                let selector = if is_selected { "▸" } else { " " };

                svc_lines.push(Line::from(vec![
                    Span::raw(format!(" {}", selector)),
                    Span::styled(health_symbol, Style::default().fg(health_color)),
                    Span::raw(" "),
                    Span::styled(&svc.id, name_style),
                    Span::styled(format!(" ({})", svc.state), Style::default().dim()),
                ]));
            }

            frame.render_widget(Paragraph::new(svc_lines), panel_layout[1]);
        }
    }
}
