//! Cluster component - displays cluster overview with nodes

use crate::action::Action;
use crate::components::Component;
use crate::components::diagnostics::k8s;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use std::collections::HashMap;
use talos_rs::{
    DiscoveryMember, EtcdMemberInfo, MemInfo, NodeCpuInfo, NodeLoadAvg, NodeMemory, NodeServices,
    ServiceInfo, TalosClient, TalosConfig, VersionInfo, get_discovery_members_with_retry,
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
    Storage,
    Processes,
    Diagnostics,
    Certs,
    Lifecycle,
    Workloads,
}

impl NavMenuItem {
    const ALL: [NavMenuItem; 9] = [
        NavMenuItem::Logs,
        NavMenuItem::Etcd,
        NavMenuItem::Network,
        NavMenuItem::Storage,
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
            NavMenuItem::Storage => "Stor",
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
            NavMenuItem::Storage => "s",
            NavMenuItem::Processes => "p",
            NavMenuItem::Diagnostics => "d",
            NavMenuItem::Certs => "c",
            NavMenuItem::Lifecycle => "y",
            NavMenuItem::Workloads => "w",
        }
    }
}

/// Represents a selectable item in the node list (header or node)
#[derive(Debug, Clone, PartialEq)]
enum NodeListItem {
    /// Cluster header (cluster_idx)
    ClusterHeader(usize),
    /// Control plane group header (cluster_idx)
    ControlPlaneHeader(usize),
    /// A control plane node (cluster_idx, node_idx within controlplane_nodes)
    ControlPlaneNode(usize, usize),
    /// Workers group header (cluster_idx)
    WorkersHeader(usize),
    /// A worker node (cluster_idx, node_idx within worker_nodes)
    WorkerNode(usize, usize),
}

/// Per-cluster data storage
#[derive(Clone, Default)]
struct ClusterData {
    /// Context/cluster name
    name: String,
    /// Talos client for this cluster
    client: Option<TalosClient>,
    /// Connection state
    connected: bool,
    /// Error message if connection failed
    error: Option<String>,
    /// Configured endpoints for this context (shown in connection-error details)
    endpoints: Vec<String>,
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
    /// Etcd members (control plane nodes only)
    etcd_members: Vec<EtcdMemberInfo>,
    /// Discovery members (ALL cluster nodes)
    discovery_members: Vec<DiscoveryMember>,
    /// Etcd summary for header
    etcd_summary: Option<EtcdSummary>,
    /// Node hostname to IP mapping
    node_ips: HashMap<String, String>,
    /// Whether this cluster accordion is expanded
    expanded: bool,
    /// Whether control plane group is expanded
    controlplane_expanded: bool,
    /// Whether workers group is expanded
    workers_expanded: bool,
    /// Non-fatal warning when worker discovery is unavailable, so the node list
    /// (control-plane-only in that case) isn't silently misleading.
    discovery_warning: Option<String>,
    /// Non-fatal warning when an ambient `KUBECONFIG` names a different cluster
    /// than this Talos context (so Kubernetes views ignore it and pin to a
    /// control plane node). Explains why `KUBECONFIG` is being disregarded.
    kubeconfig_warning: Option<String>,
}

impl ClusterData {
    /// Best control plane node IP for fetching a cluster-correct kubeconfig.
    /// Prefers an etcd member (etcd runs only on control planes), then a roster
    /// `controlplane` member, then the configured endpoint.
    fn control_plane_ip(&self) -> Option<String> {
        self.etcd_members
            .iter()
            .find_map(|m| m.ip_address())
            .or_else(|| {
                self.discovery_members
                    .iter()
                    .find(|m| m.machine_type == "controlplane")
                    .and_then(|m| m.addresses.first().cloned())
            })
            .or_else(|| {
                self.endpoints
                    .first()
                    .map(|e| e.split(':').next().unwrap_or(e).to_string())
            })
    }
}

/// Wrap a cluster warning into indented lines so it reads as part of the
/// cluster's expanded tray rather than a detached block clipped at the pane
/// edge. The ⚠ sits on the first line, aligned under the group rows (Control
/// Plane / Workers); continuations hang-indent under the text.
fn warning_lines(text: &str, pane_width: u16) -> Vec<Line<'static>> {
    const INDENT: &str = "    "; // aligns ⚠ under the "▼ Control Plane" group rows
    const HANG: &str = "      "; // continuations align under the wrapped text
    let avail = (pane_width as usize).saturating_sub(HANG.len()).max(12);

    let mut wrapped: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let sep = usize::from(!cur.is_empty());
        if !cur.is_empty() && cur.chars().count() + sep + word.chars().count() > avail {
            wrapped.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        wrapped.push(cur);
    }

    let style = Style::default().fg(Color::Yellow);
    wrapped
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            let rendered = if i == 0 {
                format!("{INDENT}⚠ {line}")
            } else {
                format!("{HANG}{line}")
            };
            Line::from(Span::styled(rendered, style))
        })
        .collect()
}

/// Cluster component showing overview with node list
pub struct ClusterComponent {
    /// All clusters from talosconfig
    clusters: Vec<ClusterData>,
    /// Currently active cluster index (for operations)
    active_cluster: usize,
    /// Currently selected service index within the node
    selected_service: usize,
    /// Last refresh time
    last_refresh: Option<std::time::Instant>,
    /// Which pane is currently focused
    focused_pane: FocusedPane,
    /// Currently selected navigation menu item
    selected_menu_item: usize,
    /// Auto-refresh enabled
    auto_refresh: bool,
    /// Last auto-refresh time for selected node
    last_auto_refresh: Option<std::time::Instant>,
    /// Currently selected item in the node list
    selected_item: NodeListItem,
    /// Custom config file path (from --config flag)
    config_path: Option<String>,
    /// Specific context to use (from --context flag)
    context_filter: Option<String>,
}

impl Default for ClusterComponent {
    fn default() -> Self {
        Self::new(None, None)
    }
}

/// Extract the deepest, most actionable message from an error chain.
///
/// tonic's transport-error `Display` is just "transport error"; the real cause
/// (e.g. "Connection refused", "i/o timeout") lives at the bottom of the
/// `source()` chain.
fn root_cause(e: &dyn std::error::Error) -> String {
    let mut deepest: &dyn std::error::Error = e;
    while let Some(src) = deepest.source() {
        deepest = src;
    }
    deepest.to_string()
}

impl ClusterComponent {
    pub fn new(config_path: Option<String>, context_filter: Option<String>) -> Self {
        Self {
            clusters: Vec::new(),
            active_cluster: 0,
            selected_service: 0,
            last_refresh: None,
            focused_pane: FocusedPane::Nodes,
            selected_menu_item: 0,
            auto_refresh: true,
            last_auto_refresh: None,
            selected_item: NodeListItem::ClusterHeader(0),
            config_path,
            context_filter,
        }
    }

    /// Whether a node is a control plane node.
    ///
    /// Discovery `machineType` is authoritative when available; otherwise we
    /// fall back to the etcd-service heuristic (only control plane nodes run
    /// etcd). Keeping these two in one place ensures the control-plane and
    /// worker groups stay complementary (every node lands in exactly one).
    fn node_is_controlplane(&self, cluster_idx: usize, node: &str) -> bool {
        if let Some(cluster) = self.clusters.get(cluster_idx) {
            let key = node.split(':').next().unwrap_or(node);
            if let Some(member) = cluster.discovery_members.iter().find(|m| {
                m.hostname.eq_ignore_ascii_case(node)
                    || m.hostname.eq_ignore_ascii_case(key)
                    || m.addresses.iter().any(|a| a == node || a == key)
            }) {
                return member.machine_type.eq_ignore_ascii_case("controlplane");
            }
        }
        // Fallback: presence of the etcd service marks a control plane node.
        self.get_node_services_for(cluster_idx, node)
            .map(|s| s.iter().any(|svc| svc.id == "etcd"))
            .unwrap_or(false)
    }

    /// Get control plane nodes for a cluster (discovery machineType, else etcd service)
    fn controlplane_nodes_for(&self, cluster_idx: usize) -> Vec<(usize, &VersionInfo)> {
        let Some(cluster) = self.clusters.get(cluster_idx) else {
            return Vec::new();
        };
        cluster
            .versions
            .iter()
            .enumerate()
            .filter(|(_, v)| self.node_is_controlplane(cluster_idx, &v.node))
            .collect()
    }

    /// Get worker nodes for a cluster (every node that isn't a control plane node)
    fn worker_nodes_for(&self, cluster_idx: usize) -> Vec<(usize, &VersionInfo)> {
        let Some(cluster) = self.clusters.get(cluster_idx) else {
            return Vec::new();
        };
        cluster
            .versions
            .iter()
            .enumerate()
            .filter(|(_, v)| !self.node_is_controlplane(cluster_idx, &v.node))
            .collect()
    }

    /// Build the visible list of items based on expand/collapse state
    fn visible_items(&self) -> Vec<NodeListItem> {
        let mut items = Vec::new();

        for (cluster_idx, cluster) in self.clusters.iter().enumerate() {
            // Cluster header
            items.push(NodeListItem::ClusterHeader(cluster_idx));

            if cluster.expanded {
                let cp_nodes = self.controlplane_nodes_for(cluster_idx);
                let worker_nodes = self.worker_nodes_for(cluster_idx);

                // Control plane section
                if !cp_nodes.is_empty() {
                    items.push(NodeListItem::ControlPlaneHeader(cluster_idx));
                    if cluster.controlplane_expanded {
                        for (i, _) in cp_nodes.iter().enumerate() {
                            items.push(NodeListItem::ControlPlaneNode(cluster_idx, i));
                        }
                    }
                }

                // Workers section
                if !worker_nodes.is_empty() {
                    items.push(NodeListItem::WorkersHeader(cluster_idx));
                    if cluster.workers_expanded {
                        for (i, _) in worker_nodes.iter().enumerate() {
                            items.push(NodeListItem::WorkerNode(cluster_idx, i));
                        }
                    }
                }
            }
        }

        items
    }

    /// Get the cluster index for the currently selected item
    fn selected_cluster_index(&self) -> Option<usize> {
        match &self.selected_item {
            NodeListItem::ClusterHeader(idx) => Some(*idx),
            NodeListItem::ControlPlaneHeader(idx) => Some(*idx),
            NodeListItem::ControlPlaneNode(idx, _) => Some(*idx),
            NodeListItem::WorkersHeader(idx) => Some(*idx),
            NodeListItem::WorkerNode(idx, _) => Some(*idx),
        }
    }

    /// Navigate to the next item in the node list
    fn navigate_down(&mut self) {
        let items = self.visible_items();
        if items.is_empty() {
            return;
        }
        let current_pos = items
            .iter()
            .position(|i| i == &self.selected_item)
            .unwrap_or(0);
        let next_pos = (current_pos + 1).min(items.len() - 1);
        self.selected_item = items[next_pos].clone();
        // Update active cluster
        if let Some(idx) = self.selected_cluster_index() {
            self.active_cluster = idx;
        }
    }

    /// Navigate to the previous item in the node list
    fn navigate_up(&mut self) {
        let items = self.visible_items();
        if items.is_empty() {
            return;
        }
        let current_pos = items
            .iter()
            .position(|i| i == &self.selected_item)
            .unwrap_or(0);
        let prev_pos = current_pos.saturating_sub(1);
        self.selected_item = items[prev_pos].clone();
        // Update active cluster
        if let Some(idx) = self.selected_cluster_index() {
            self.active_cluster = idx;
        }
    }

    /// Toggle expand/collapse of the currently selected group header
    fn toggle_expand(&mut self) {
        match &self.selected_item {
            NodeListItem::ClusterHeader(idx) => {
                if let Some(cluster) = self.clusters.get_mut(*idx) {
                    cluster.expanded = !cluster.expanded;
                }
            }
            NodeListItem::ControlPlaneHeader(idx) => {
                if let Some(cluster) = self.clusters.get_mut(*idx) {
                    cluster.controlplane_expanded = !cluster.controlplane_expanded;
                }
            }
            NodeListItem::WorkersHeader(idx) => {
                if let Some(cluster) = self.clusters.get_mut(*idx) {
                    cluster.workers_expanded = !cluster.workers_expanded;
                }
            }
            _ => {} // No action for node items
        }
    }

    /// Initialize connection to all Talos clusters from talosconfig
    pub async fn connect(&mut self) -> Result<()> {
        // Install crypto provider (needed for rustls)
        let _ = rustls::crypto::ring::default_provider().install_default();

        // Load talosconfig - use custom path if provided via --config flag
        let config = match &self.config_path {
            Some(path) => {
                let path_buf = std::path::PathBuf::from(path);
                match TalosConfig::load_from(&path_buf) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!("Failed to load talosconfig from {}: {}", path, e);
                        return Ok(());
                    }
                }
            }
            None => match TalosConfig::load_default() {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("Failed to load talosconfig: {}", e);
                    return Ok(());
                }
            },
        };

        // Determine which contexts to load based on --context flag
        let context_names: Vec<String> = match &self.context_filter {
            Some(ctx_name) => {
                // Verify the context exists
                if config.contexts.contains_key(ctx_name) {
                    vec![ctx_name.clone()]
                } else {
                    tracing::error!(
                        "Context '{}' not found in talosconfig. Available contexts: {:?}",
                        ctx_name,
                        config.contexts.keys().collect::<Vec<_>>()
                    );
                    return Ok(());
                }
            }
            None => config.contexts.keys().cloned().collect(),
        };

        // Create ClusterData for each context
        self.clusters.clear();
        for (idx, name) in context_names.iter().enumerate() {
            let mut cluster = ClusterData {
                name: name.clone(),
                expanded: idx == 0, // Expand first cluster by default
                controlplane_expanded: true,
                workers_expanded: true,
                ..Default::default()
            };

            // Try to connect to each cluster using the loaded config
            match config.get_context(name) {
                Ok(ctx) => {
                    cluster.endpoints = ctx.endpoints.clone();
                    match TalosClient::from_context(ctx).await {
                        Ok(client) => {
                            cluster.client = Some(client);
                            cluster.connected = true;
                            cluster.error = None;
                        }
                        Err(e) => {
                            cluster.error = Some(format!("Could not connect — {}", root_cause(&e)));
                            cluster.connected = false;
                        }
                    }
                }
                Err(e) => {
                    cluster.error = Some(e.to_string());
                    cluster.connected = false;
                }
            }

            self.clusters.push(cluster);
        }

        // Refresh all connected clusters
        self.refresh().await?;

        // Set initial selection
        if !self.clusters.is_empty() {
            self.selected_item = NodeListItem::ClusterHeader(0);
            self.active_cluster = 0;
        }

        Ok(())
    }

    /// Refresh all cluster data
    pub async fn refresh(&mut self) -> Result<()> {
        // Refresh each cluster
        for cluster_idx in 0..self.clusters.len() {
            self.refresh_cluster(cluster_idx).await;
        }
        self.last_refresh = Some(std::time::Instant::now());
        Ok(())
    }

    /// Refresh a single cluster's data
    async fn refresh_cluster(&mut self, cluster_idx: usize) {
        let Some(cluster) = self.clusters.get_mut(cluster_idx) else {
            return;
        };

        let Some(client) = &cluster.client else {
            return;
        };

        // Clone client to avoid borrow issues
        let client = client.clone();

        // First, fetch etcd members via gRPC
        match client.etcd_members().await {
            Ok(members) => {
                cluster.node_ips.clear();
                for member in &members {
                    if let Some(ip) = member.ip_address() {
                        cluster.node_ips.insert(member.hostname.clone(), ip);
                    }
                }
                cluster.etcd_members = members;
            }
            Err(e) => {
                tracing::warn!("Failed to fetch etcd members for {}: {}", cluster.name, e);
                cluster.etcd_members.clear();
            }
        }

        // Try to get discovery members (ALL nodes including workers)
        // Use context-aware async function with retry to avoid blocking and to use correct certificates
        // Pass etcd member IPs as fallback in case VIP-based discovery fails
        let context_name = cluster.name.clone();
        let fallback_ips: Vec<String> = cluster
            .etcd_members
            .iter()
            .filter_map(|m| m.ip_address())
            .collect();
        match get_discovery_members_with_retry(
            &context_name,
            self.config_path.as_deref(),
            &fallback_ips,
        )
        .await
        {
            Ok(members) if !members.is_empty() => {
                cluster.node_ips.clear();
                for member in &members {
                    if let Some(ip) = member.addresses.first() {
                        cluster.node_ips.insert(member.hostname.clone(), ip.clone());
                    }
                }
                cluster.discovery_members = members;
                cluster.discovery_warning = None;
            }
            outcome => {
                // Talos discovery gave us nothing usable. It's an optional service
                // and commonly disabled, so this is not fatal — log the reason.
                match &outcome {
                    Ok(_) => tracing::warn!(
                        "Discovery returned no members for {} (discovery service likely disabled)",
                        cluster.name
                    ),
                    Err(e) => tracing::warn!(
                        "Failed to fetch discovery members for {} after retries: {}",
                        cluster.name,
                        e
                    ),
                }

                // Only rebuild the roster when we have none cached. If discovery
                // succeeded on an earlier refresh, keep those members for
                // resilience rather than dropping workers on a transient failure.
                if cluster.discovery_members.is_empty() {
                    // Fall back to the Kubernetes API, which knows every node —
                    // control plane AND worker — independently of Talos discovery.
                    // This is what makes workers show up when discovery is disabled;
                    // etcd membership alone would leave the list control-plane-only.
                    // Prefer an etcd member IP (a real control plane node) for the
                    // kubeconfig fetch; fall back to the configured endpoint IP.
                    let cp_ip = cluster
                        .etcd_members
                        .iter()
                        .find_map(|m| m.ip_address())
                        .or_else(|| {
                            cluster
                                .endpoints
                                .first()
                                .map(|e| e.split(':').next().unwrap_or(e).to_string())
                        });

                    match cp_ip {
                        Some(cp_ip) => {
                            match k8s::discovery_members_via_k8s(&client, &cp_ip).await {
                                Ok(members) if !members.is_empty() => {
                                    cluster.node_ips.clear();
                                    for member in &members {
                                        if let Some(ip) = member.addresses.first() {
                                            cluster
                                                .node_ips
                                                .insert(member.hostname.clone(), ip.clone());
                                        }
                                    }
                                    let count = members.len();
                                    cluster.discovery_members = members;
                                    cluster.discovery_warning = None;
                                    tracing::info!(
                                        "Enumerated {} node(s) via the Kubernetes API for {} \
                                     (Talos discovery unavailable)",
                                        count,
                                        cluster.name
                                    );
                                }
                                Ok(_) => {
                                    cluster.discovery_warning = Some(
                                        "Worker nodes unavailable: Talos discovery returned no \
                                     members and the Kubernetes API reported no nodes."
                                            .to_string(),
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "Kubernetes node enumeration failed for {}: {}",
                                        cluster.name,
                                        e
                                    );
                                    cluster.discovery_warning = Some(format!(
                                        "Worker nodes may be missing: Talos discovery is unavailable \
                                     and the Kubernetes API could not be reached ({e})."
                                    ));
                                }
                            }
                        }
                        None => {
                            cluster.discovery_warning = Some(
                                "Worker nodes unavailable: cluster discovery returned no members \
                                 (the discovery service may be disabled)."
                                    .to_string(),
                            );
                        }
                    }
                }
            }
        }

        // Determine which nodes to query
        let nodes_to_query: Vec<(String, String)> = if !cluster.discovery_members.is_empty() {
            cluster
                .discovery_members
                .iter()
                .filter_map(|m| {
                    m.addresses.first().map(|ip| {
                        let name = if !m.hostname.is_empty() {
                            m.hostname.clone()
                        } else {
                            ip.clone()
                        };
                        (name, ip.clone())
                    })
                })
                .collect()
        } else if !cluster.etcd_members.is_empty() {
            cluster
                .etcd_members
                .iter()
                .filter_map(|m| {
                    m.ip_address().map(|ip| {
                        let name = if !m.hostname.is_empty() {
                            m.hostname.clone()
                        } else {
                            ip.clone()
                        };
                        (name, ip)
                    })
                })
                .collect()
        } else if !cluster.versions.is_empty() {
            // Tertiary fallback: use nodes from previous version queries
            tracing::debug!(
                "Using version info as fallback node source for {}",
                cluster.name
            );
            cluster
                .versions
                .iter()
                .map(|v| {
                    let ip = v.node.split(':').next().unwrap_or(&v.node).to_string();
                    (v.node.clone(), ip)
                })
                .collect()
        } else {
            tracing::warn!(
                "No node sources available for {}, skipping queries",
                cluster.name
            );
            Vec::new()
        };

        // Query each node
        if !nodes_to_query.is_empty() {
            let mut versions = Vec::new();
            let mut services = Vec::new();
            let mut memory = Vec::new();
            let mut load_avg = Vec::new();
            let mut cpu_info = Vec::new();

            for (node_name, ip) in &nodes_to_query {
                let node_client = client.with_node(ip);

                if let Ok(mut nv) = node_client.version().await {
                    for v in &mut nv {
                        v.node = node_name.clone();
                    }
                    versions.extend(nv);
                }
                if let Ok(mut ns) = node_client.services().await {
                    for s in &mut ns {
                        s.node = node_name.clone();
                    }
                    services.extend(ns);
                }
                if let Ok(mut nm) = node_client.memory().await {
                    for m in &mut nm {
                        m.node = node_name.clone();
                    }
                    memory.extend(nm);
                }
                if let Ok(mut nl) = node_client.load_avg().await {
                    for l in &mut nl {
                        l.node = node_name.clone();
                    }
                    load_avg.extend(nl);
                }
                if let Ok(mut nc) = node_client.cpu_info().await {
                    for c in &mut nc {
                        c.node = node_name.clone();
                    }
                    cpu_info.extend(nc);
                }
            }

            // Need to re-borrow cluster mutably after async calls
            if let Some(cluster) = self.clusters.get_mut(cluster_idx) {
                cluster.versions = versions;
                cluster.services = services;
                cluster.memory = memory;
                cluster.load_avg = load_avg;
                cluster.cpu_info = cpu_info;

                // Fetch etcd status for header summary (target all control planes)
                if let Some(client) = &cluster.client {
                    // Use IPs instead of hostnames (hostnames may not be resolvable)
                    let cp_ips: Vec<String> = cluster
                        .etcd_members
                        .iter()
                        .filter_map(|m| m.ip_address())
                        .collect();
                    if let Ok(statuses) = client.etcd_status_for_nodes(&cp_ips).await {
                        let total = cluster.etcd_members.len();
                        let healthy = statuses.len();
                        let quorum_needed = total / 2 + 1;
                        cluster.etcd_summary = Some(EtcdSummary {
                            healthy,
                            total,
                            has_quorum: healthy >= quorum_needed,
                        });
                    }
                }
            }
        }

        // Reflect real reachability so the UI shows a clear error instead of an
        // empty/blank cluster when no endpoint can be reached. Healthy clusters
        // have versions/etcd/discovery data; a transient failure keeps the
        // previously cached data, so this only trips when there is nothing at all.
        if let Some(cluster) = self.clusters.get_mut(cluster_idx) {
            let has_any_data = !cluster.versions.is_empty()
                || !cluster.etcd_members.is_empty()
                || !cluster.discovery_members.is_empty();
            if has_any_data {
                cluster.connected = true;
                cluster.error = None;
            } else {
                cluster.connected = false;
                cluster.error = Some(
                    "Unable to reach any configured Talos endpoint. Check network \
                     connectivity and the endpoints in your talosconfig."
                        .to_string(),
                );
            }
        }

        // Warn if an ambient KUBECONFIG names a *different* cluster than this
        // Talos context. Kubernetes views ignore it and pin to a control plane
        // node (see k8s::create_k8s_client_with_source), so this only explains
        // why KUBECONFIG is being disregarded — it never blocks anything. The
        // helper reads the local kubeconfig first and returns None when unset,
        // so there's no extra Talos round-trip in the common case.
        let cp_ip = self
            .clusters
            .get(cluster_idx)
            .and_then(ClusterData::control_plane_ip);
        if let Some(cp_ip) = cp_ip {
            let warning = k8s::kubeconfig_mismatch_warning(&client, &cp_ip).await;
            if let Some(cluster) = self.clusters.get_mut(cluster_idx) {
                cluster.kubeconfig_warning = warning;
            }
        }
    }

    /// Refresh only the selected node's stats (memory, load, services)
    /// This is lighter weight than a full refresh
    pub async fn refresh_selected_node(&mut self) -> Result<()> {
        let cluster_idx = self.active_cluster;
        let Some(cluster) = self.clusters.get(cluster_idx) else {
            return Ok(());
        };
        let Some(client) = &cluster.client else {
            return Ok(());
        };
        let client = client.clone();

        // Get the selected node's name and IP
        let Some(node_name) = self.current_node_name() else {
            return Ok(());
        };
        let Some(cluster) = self.clusters.get(cluster_idx) else {
            return Ok(());
        };
        let Some(node_ip) = cluster.node_ips.get(&node_name).cloned() else {
            return Ok(());
        };

        let node_client = client.with_node(&node_ip);

        // Fetch services, memory, and load for this node
        if let Ok(mut node_services) = node_client.services().await {
            for s in &mut node_services {
                s.node = node_name.clone();
            }
            // Update the services for this node
            if let Some(cluster) = self.clusters.get_mut(cluster_idx) {
                cluster.services.retain(|s| s.node != node_name);
                cluster.services.extend(node_services);
            }
        }

        if let Ok(mut node_memory) = node_client.memory().await {
            for m in &mut node_memory {
                m.node = node_name.clone();
            }
            if let Some(cluster) = self.clusters.get_mut(cluster_idx) {
                cluster.memory.retain(|m| m.node != node_name);
                cluster.memory.extend(node_memory);
            }
        }

        if let Ok(mut node_load) = node_client.load_avg().await {
            for l in &mut node_load {
                l.node = node_name.clone();
            }
            if let Some(cluster) = self.clusters.get_mut(cluster_idx) {
                cluster.load_avg.retain(|l| l.node != node_name);
                cluster.load_avg.extend(node_load);
            }
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

    /// Get services for a node in a specific cluster
    fn get_node_services_for(
        &self,
        cluster_idx: usize,
        node_name: &str,
    ) -> Option<&Vec<ServiceInfo>> {
        self.clusters
            .get(cluster_idx)?
            .services
            .iter()
            .find(|s| s.node == node_name || (s.node.is_empty() && node_name.is_empty()))
            .map(|s| &s.services)
    }

    /// Get services for a node in the active cluster
    fn get_node_services(&self, node_name: &str) -> Option<&Vec<ServiceInfo>> {
        self.get_node_services_for(self.active_cluster, node_name)
    }

    /// Get memory for a node in a specific cluster
    fn get_node_memory_for(&self, cluster_idx: usize, node_name: &str) -> Option<&MemInfo> {
        self.clusters
            .get(cluster_idx)?
            .memory
            .iter()
            .find(|m| m.node == node_name || (m.node.is_empty() && node_name.is_empty()))
            .and_then(|m| m.meminfo.as_ref())
    }

    /// Get memory for a node in the active cluster
    fn get_node_memory(&self, node_name: &str) -> Option<&MemInfo> {
        self.get_node_memory_for(self.active_cluster, node_name)
    }

    /// Get load average for a node in a specific cluster
    fn get_node_load_avg_for(&self, cluster_idx: usize, node_name: &str) -> Option<&NodeLoadAvg> {
        self.clusters
            .get(cluster_idx)?
            .load_avg
            .iter()
            .find(|l| l.node == node_name || (l.node.is_empty() && node_name.is_empty()))
    }

    /// Get load average for a node in the active cluster
    fn get_node_load_avg(&self, node_name: &str) -> Option<&NodeLoadAvg> {
        self.get_node_load_avg_for(self.active_cluster, node_name)
    }

    /// Get CPU info for a node in a specific cluster
    fn get_node_cpu_info_for(&self, cluster_idx: usize, node_name: &str) -> Option<&NodeCpuInfo> {
        self.clusters
            .get(cluster_idx)?
            .cpu_info
            .iter()
            .find(|c| c.node == node_name || (c.node.is_empty() && node_name.is_empty()))
    }

    /// Get CPU info for a node in the active cluster
    fn get_node_cpu_info(&self, node_name: &str) -> Option<&NodeCpuInfo> {
        self.get_node_cpu_info_for(self.active_cluster, node_name)
    }

    /// Get the currently selected service ID
    pub fn selected_service_id(&self) -> Option<String> {
        let node_name = self.current_node_name()?;
        self.get_node_services(&node_name)
            .and_then(|services| services.get(self.selected_service))
            .map(|s| s.id.clone())
    }

    /// Get a reference to the client for the active cluster
    pub fn client(&self) -> Option<&TalosClient> {
        self.clusters.get(self.active_cluster)?.client.as_ref()
    }

    /// Best control plane node IP for the active cluster, used to fetch a
    /// cluster-correct kubeconfig (see [`k8s::create_k8s_client_with_source`]).
    ///
    /// Prefers an etcd member address (etcd only runs on control plane nodes, so
    /// this is unambiguously a control plane), then a discovery/roster member
    /// tagged `controlplane`, then the configured endpoint. `None` only when the
    /// cluster has no known address at all.
    pub fn control_plane_ip(&self) -> Option<String> {
        self.clusters.get(self.active_cluster)?.control_plane_ip()
    }

    /// Get context name for active cluster
    pub fn current_context_name(&self) -> Option<&str> {
        self.clusters
            .get(self.active_cluster)
            .map(|c| c.name.as_str())
    }

    /// Get config path
    pub fn config_path(&self) -> Option<&str> {
        self.config_path.as_deref()
    }

    /// Get node_ips for active cluster
    fn node_ips(&self) -> &HashMap<String, String> {
        static EMPTY: std::sync::OnceLock<HashMap<String, String>> = std::sync::OnceLock::new();
        self.clusters
            .get(self.active_cluster)
            .map(|c| &c.node_ips)
            .unwrap_or_else(|| EMPTY.get_or_init(HashMap::new))
    }

    /// Get a control plane node IP from the active cluster
    /// Used to fetch kubeconfig when diagnosing worker nodes
    fn get_controlplane_endpoint(&self) -> Option<String> {
        let cp_nodes = self.controlplane_nodes_for(self.active_cluster);
        if let Some((_, node)) = cp_nodes.first() {
            self.node_ips().get(&node.node).cloned()
        } else {
            None
        }
    }

    /// Get service count for current node
    fn current_service_count(&self) -> usize {
        let Some(node_name) = self.current_node_name() else {
            return 0;
        };
        self.get_node_services(&node_name)
            .map(|s| s.len())
            .unwrap_or(0)
    }

    /// Get all service IDs for current node
    fn current_service_ids(&self) -> Vec<String> {
        let Some(node_name) = self.current_node_name() else {
            return Vec::new();
        };
        self.get_node_services(&node_name)
            .map(|services| services.iter().map(|s| s.id.clone()).collect())
            .unwrap_or_default()
    }

    /// Get current node IP/name based on selected_item
    fn current_node_name(&self) -> Option<String> {
        match &self.selected_item {
            NodeListItem::ControlPlaneNode(cluster_idx, node_idx) => self
                .controlplane_nodes_for(*cluster_idx)
                .get(*node_idx)
                .map(|(_, v)| v.node.clone()),
            NodeListItem::WorkerNode(cluster_idx, node_idx) => self
                .worker_nodes_for(*cluster_idx)
                .get(*node_idx)
                .map(|(_, v)| v.node.clone()),
            _ => None, // Headers don't have a node name
        }
    }

    /// Determine the selected node's role.
    ///
    /// The selection already reflects how the node was grouped (which now uses
    /// discovery machineType with an etcd-service fallback), so read that
    /// directly; only fall back to the service heuristic for non-node selections.
    fn current_node_role(&self) -> String {
        match &self.selected_item {
            NodeListItem::ControlPlaneNode(..) => return "controlplane".to_string(),
            NodeListItem::WorkerNode(..) => return "worker".to_string(),
            _ => {}
        }
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
                        let node_ip = self
                            .node_ips()
                            .get(&node_name)
                            .cloned()
                            .unwrap_or(node_name.clone());
                        Ok(Some(Action::ShowMultiLogs(
                            node_ip,
                            node_role,
                            service_ids.clone(),
                            service_ids,
                        )))
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
                    let node_ip = self
                        .node_ips()
                        .get(&node_name)
                        .cloned()
                        .unwrap_or(node_name.clone());
                    Ok(Some(Action::ShowNetwork(node_name, node_ip)))
                } else {
                    Ok(None)
                }
            }
            NavMenuItem::Storage => {
                if let Some(node_name) = self.current_node_name() {
                    let node_ip = self
                        .node_ips()
                        .get(&node_name)
                        .cloned()
                        .unwrap_or(node_name.clone());
                    Ok(Some(Action::ShowStorage(node_name, node_ip)))
                } else {
                    Ok(None)
                }
            }
            NavMenuItem::Processes => {
                if let Some(node_name) = self.current_node_name() {
                    let node_ip = self
                        .node_ips()
                        .get(&node_name)
                        .cloned()
                        .unwrap_or(node_name.clone());
                    Ok(Some(Action::ShowProcesses(node_name, node_ip)))
                } else {
                    Ok(None)
                }
            }
            NavMenuItem::Diagnostics => {
                if let Some(node_name) = self.current_node_name() {
                    let node_ip = self
                        .node_ips()
                        .get(&node_name)
                        .cloned()
                        .unwrap_or(node_name.clone());
                    let node_role = self.current_node_role();
                    // For worker nodes, provide a control plane endpoint to fetch kubeconfig from
                    let cp_endpoint = if node_role == "worker" {
                        self.get_controlplane_endpoint()
                    } else {
                        None
                    };
                    Ok(Some(Action::ShowDiagnostics(
                        node_name,
                        node_ip,
                        node_role,
                        cp_endpoint,
                    )))
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
                        self.navigate_up();
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
                        self.navigate_down();
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

            // Space: toggle expand/collapse on group headers
            KeyCode::Char(' ') => {
                if self.focused_pane == FocusedPane::Nodes {
                    self.toggle_expand();
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
                        // On a header - toggle expand/collapse
                        // On a node - show all logs for that node
                        match &self.selected_item {
                            NodeListItem::ClusterHeader(_)
                            | NodeListItem::ControlPlaneHeader(_)
                            | NodeListItem::WorkersHeader(_) => {
                                self.toggle_expand();
                                Ok(None)
                            }
                            _ => {
                                if let Some(node_name) = self.current_node_name() {
                                    let service_ids = self.current_service_ids();
                                    if !service_ids.is_empty() {
                                        let node_role = self.current_node_role();
                                        let node_ip = self
                                            .node_ips()
                                            .get(&node_name)
                                            .cloned()
                                            .unwrap_or(node_name.clone());
                                        Ok(Some(Action::ShowMultiLogs(
                                            node_ip,
                                            node_role,
                                            service_ids.clone(),
                                            service_ids,
                                        )))
                                    } else {
                                        Ok(None)
                                    }
                                } else {
                                    Ok(None)
                                }
                            }
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
                                let node_ip = self
                                    .node_ips()
                                    .get(&node_name)
                                    .cloned()
                                    .unwrap_or(node_name.clone());
                                Ok(Some(Action::ShowMultiLogs(
                                    node_ip,
                                    node_role,
                                    vec![service_id],
                                    all_services,
                                )))
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
                        let node_ip = self
                            .node_ips()
                            .get(&node_name)
                            .cloned()
                            .unwrap_or(node_name.clone());
                        Ok(Some(Action::ShowMultiLogs(
                            node_ip,
                            node_role,
                            service_ids.clone(),
                            service_ids,
                        )))
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
                    let node_ip = self
                        .node_ips()
                        .get(&node_name)
                        .cloned()
                        .unwrap_or(node_name.clone());
                    Ok(Some(Action::ShowProcesses(node_name, node_ip)))
                } else {
                    Ok(None)
                }
            }
            KeyCode::Char('n') => {
                if let Some(node_name) = self.current_node_name() {
                    let node_ip = self
                        .node_ips()
                        .get(&node_name)
                        .cloned()
                        .unwrap_or(node_name.clone());
                    Ok(Some(Action::ShowNetwork(node_name, node_ip)))
                } else {
                    Ok(None)
                }
            }
            KeyCode::Char('s') => {
                if let Some(node_name) = self.current_node_name() {
                    let node_ip = self
                        .node_ips()
                        .get(&node_name)
                        .cloned()
                        .unwrap_or(node_name.clone());
                    Ok(Some(Action::ShowStorage(node_name, node_ip)))
                } else {
                    Ok(None)
                }
            }
            KeyCode::Char('d') => {
                if let Some(node_name) = self.current_node_name() {
                    let node_ip = self
                        .node_ips()
                        .get(&node_name)
                        .cloned()
                        .unwrap_or(node_name.clone());
                    let node_role = self.current_node_role();
                    // For worker nodes, provide a control plane endpoint to fetch kubeconfig from
                    let cp_endpoint = if node_role == "worker" {
                        self.get_controlplane_endpoint()
                    } else {
                        None
                    };
                    Ok(Some(Action::ShowDiagnostics(
                        node_name,
                        node_ip,
                        node_role,
                        cp_endpoint,
                    )))
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
                    let node_ip = self
                        .node_ips()
                        .get(&node_name)
                        .cloned()
                        .unwrap_or_else(|| node_name.clone());
                    let is_controlplane = self.current_node_role() == "controlplane";
                    Ok(Some(Action::ShowNodeOperations(
                        node_name,
                        node_ip,
                        is_controlplane,
                    )))
                } else {
                    Ok(None)
                }
            }
            KeyCode::Char('O') => {
                // Show rolling operations overlay with all nodes from active cluster
                let cluster_idx = self.active_cluster;
                let nodes: Vec<(String, String, bool)> =
                    if let Some(cluster) = self.clusters.get(cluster_idx) {
                        cluster
                            .versions
                            .iter()
                            .map(|v| {
                                let hostname = v.node.clone();
                                let ip = cluster
                                    .node_ips
                                    .get(&hostname)
                                    .cloned()
                                    .unwrap_or_else(|| hostname.clone());
                                // Check if node has etcd service (controlplane)
                                let is_controlplane = self
                                    .get_node_services_for(cluster_idx, &hostname)
                                    .map(|s| s.iter().any(|svc| svc.id == "etcd"))
                                    .unwrap_or(false);
                                (hostname, ip, is_controlplane)
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };
                if !nodes.is_empty() {
                    Ok(Some(Action::ShowRollingOperations(nodes)))
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
        let auto_refresh_color = if self.auto_refresh {
            Color::Green
        } else {
            Color::DarkGray
        };
        let footer_line = Line::from(vec![
            Span::styled(" [j/k]", Style::default().fg(Color::Yellow)),
            Span::styled(" nav", Style::default().dim()),
            Span::raw("  "),
            Span::styled("[Space]", Style::default().fg(Color::Yellow)),
            Span::styled(" fold", Style::default().dim()),
            Span::raw("  "),
            Span::styled("[Tab]", Style::default().fg(Color::Yellow)),
            Span::styled(" pane", Style::default().dim()),
            Span::raw("  "),
            Span::styled("[l]", Style::default().fg(Color::Yellow)),
            Span::styled(" logs", Style::default().dim()),
            Span::raw("  "),
            Span::styled("[o]", Style::default().fg(Color::Yellow)),
            Span::styled(" ops", Style::default().dim()),
            Span::raw(" "),
            Span::styled("[O]", Style::default().fg(Color::Yellow)),
            Span::styled(" rolling", Style::default().dim()),
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
        let footer = Paragraph::new(footer_line).block(
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
        // Count connected clusters
        let connected_count = self.clusters.iter().filter(|c| c.connected).count();
        let total_count = self.clusters.len();

        let (status_indicator, status_text) = if connected_count == total_count && total_count > 0 {
            (
                Span::styled(" ● ", Style::default().fg(Color::Green)),
                "Connected",
            )
        } else if connected_count > 0 {
            (
                Span::styled(" ◐ ", Style::default().fg(Color::Yellow)),
                "Partial",
            )
        } else if total_count == 0 {
            (
                Span::styled(" ○ ", Style::default().fg(Color::DarkGray)),
                "No clusters",
            )
        } else {
            (
                Span::styled(" ✗ ", Style::default().fg(Color::Red)),
                "Disconnected",
            )
        };

        // Cluster count
        let cluster_count_span = if total_count > 1 {
            vec![
                Span::raw("   "),
                Span::styled(
                    format!("{} clusters", total_count),
                    Style::default().fg(Color::DarkGray),
                ),
            ]
        } else {
            vec![]
        };

        let mut header_spans = vec![
            Span::styled(
                " talos-pilot ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            status_indicator,
            Span::styled(status_text, Style::default().dim()),
        ];
        header_spans.extend(cluster_count_span);

        // Active cluster name on the right
        let active_name = self
            .clusters
            .get(self.active_cluster)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "none".to_string());

        let left_content = Line::from(header_spans);
        let right_content = Span::styled(
            format!(" {} ", active_name),
            Style::default().fg(Color::DarkGray),
        );

        // Render header
        let header_block = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(Color::DarkGray));

        let inner = header_block.inner(area);
        frame.render_widget(header_block, area);
        frame.render_widget(Paragraph::new(left_content), inner);

        // Right-align active cluster name
        let right_area = Rect {
            x: area.x + area.width.saturating_sub(active_name.len() as u16 + 3),
            y: area.y,
            width: active_name.len() as u16 + 3,
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
            .title_style(
                Style::default().fg(if self.focused_pane == FocusedPane::Nodes {
                    Color::Cyan
                } else {
                    Color::White
                }),
            )
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

        if self.clusters.is_empty() {
            let msg = Paragraph::new(Line::from(Span::styled(
                "  No clusters found",
                Style::default().dim(),
            )));
            frame.render_widget(msg, pane_layout[0]);
        } else {
            // Build multi-cluster accordion-style node list
            let mut lines = Vec::new();
            let nodes_focused = self.focused_pane == FocusedPane::Nodes;

            for (cluster_idx, cluster) in self.clusters.iter().enumerate() {
                // Cluster header
                let is_cluster_selected =
                    self.selected_item == NodeListItem::ClusterHeader(cluster_idx);
                let expand_icon = if cluster.expanded { "▼" } else { "▶" };
                let selector = if is_cluster_selected && nodes_focused {
                    "▸"
                } else {
                    " "
                };

                // Status indicator
                let status_symbol = if cluster.connected { "●" } else { "○" };
                let status_color = if cluster.connected {
                    Color::Green
                } else {
                    Color::Red
                };

                let header_style = if is_cluster_selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Magenta)
                };

                let node_count = cluster.versions.len();

                // Build etcd status for this cluster
                let etcd_spans: Vec<Span> = if let Some(etcd) = &cluster.etcd_summary {
                    let (indicator, color) = if etcd.has_quorum && etcd.healthy == etcd.total {
                        ("●", Color::Green)
                    } else if etcd.has_quorum {
                        ("◐", Color::Yellow)
                    } else {
                        ("✗", Color::Red)
                    };
                    vec![
                        Span::styled("  etcd ", Style::default().dim()),
                        Span::styled(
                            format!("{}/{}", etcd.healthy, etcd.total),
                            Style::default().fg(color),
                        ),
                        Span::styled(indicator, Style::default().fg(color)),
                    ]
                } else {
                    vec![]
                };

                let mut cluster_line = vec![
                    Span::styled(format!("{} {} ", selector, expand_icon), header_style),
                    Span::styled(status_symbol, Style::default().fg(status_color)),
                    Span::raw(" "),
                    Span::styled(&cluster.name, header_style),
                    Span::styled(format!(" ({})", node_count), Style::default().dim()),
                ];
                cluster_line.extend(etcd_spans);
                lines.push(Line::from(cluster_line));

                // Show a connection error under the cluster header (always, even
                // when collapsed) so failures are visible instead of a blank list.
                if let Some(err) = &cluster.error {
                    lines.push(Line::from(vec![
                        Span::raw("    "),
                        Span::styled("⚠ ", Style::default().fg(Color::Red)),
                        Span::styled(err.as_str(), Style::default().fg(Color::Red)),
                    ]));
                }

                // Skip if cluster is collapsed
                if !cluster.expanded {
                    continue;
                }

                let cp_nodes = self.controlplane_nodes_for(cluster_idx);
                let worker_nodes = self.worker_nodes_for(cluster_idx);

                // Control Plane section
                if !cp_nodes.is_empty() {
                    let is_selected =
                        self.selected_item == NodeListItem::ControlPlaneHeader(cluster_idx);
                    let expand_icon = if cluster.controlplane_expanded {
                        "▼"
                    } else {
                        "▶"
                    };
                    let selector = if is_selected && nodes_focused {
                        "▸"
                    } else {
                        " "
                    };
                    let header_style = if is_selected {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Blue)
                    };

                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(format!("{} {} ", selector, expand_icon), header_style),
                        Span::styled(format!("Control Plane ({})", cp_nodes.len()), header_style),
                    ]));

                    // Show control plane nodes if expanded
                    if cluster.controlplane_expanded {
                        for (idx, (_, v)) in cp_nodes.iter().enumerate() {
                            let node_name = if v.node.is_empty() {
                                "node".to_string()
                            } else {
                                v.node.clone()
                            };
                            let is_node_selected = self.selected_item
                                == NodeListItem::ControlPlaneNode(cluster_idx, idx);

                            // Health indicator
                            let mem_pct = self
                                .get_node_memory_for(cluster_idx, &v.node)
                                .map(|m| m.usage_percent())
                                .unwrap_or(0.0);
                            let svc_healthy = self
                                .get_node_services_for(cluster_idx, &v.node)
                                .map(|services| {
                                    services.iter().all(|s| {
                                        s.health.as_ref().map(|h| h.healthy).unwrap_or(true)
                                    })
                                })
                                .unwrap_or(true);
                            let health_symbol = if svc_healthy && mem_pct < 90.0 {
                                "●"
                            } else {
                                "◐"
                            };
                            let health_color = if svc_healthy && mem_pct < 90.0 {
                                Color::Green
                            } else {
                                Color::Yellow
                            };

                            let selector = if is_node_selected && nodes_focused {
                                "▸"
                            } else {
                                " "
                            };
                            let name_style = if is_node_selected {
                                Style::default()
                                    .fg(Color::White)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(Color::White)
                            };

                            lines.push(Line::from(vec![
                                Span::raw("     "),
                                Span::styled(
                                    format!("{} {} ", selector, health_symbol),
                                    Style::default().fg(health_color),
                                ),
                                Span::styled(node_name, name_style),
                            ]));
                        }
                    }
                }

                // Workers section
                if !worker_nodes.is_empty() {
                    let is_selected =
                        self.selected_item == NodeListItem::WorkersHeader(cluster_idx);
                    let expand_icon = if cluster.workers_expanded {
                        "▼"
                    } else {
                        "▶"
                    };
                    let selector = if is_selected && nodes_focused {
                        "▸"
                    } else {
                        " "
                    };
                    let header_style = if is_selected {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Blue)
                    };

                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(format!("{} {} ", selector, expand_icon), header_style),
                        Span::styled(format!("Workers ({})", worker_nodes.len()), header_style),
                    ]));

                    // Show worker nodes if expanded
                    if cluster.workers_expanded {
                        for (idx, (_, v)) in worker_nodes.iter().enumerate() {
                            let node_name = if v.node.is_empty() {
                                "node".to_string()
                            } else {
                                v.node.clone()
                            };
                            let is_node_selected =
                                self.selected_item == NodeListItem::WorkerNode(cluster_idx, idx);

                            // Health indicator
                            let mem_pct = self
                                .get_node_memory_for(cluster_idx, &v.node)
                                .map(|m| m.usage_percent())
                                .unwrap_or(0.0);
                            let svc_healthy = self
                                .get_node_services_for(cluster_idx, &v.node)
                                .map(|services| {
                                    services.iter().all(|s| {
                                        s.health.as_ref().map(|h| h.healthy).unwrap_or(true)
                                    })
                                })
                                .unwrap_or(true);
                            let health_symbol = if svc_healthy && mem_pct < 90.0 {
                                "●"
                            } else {
                                "◐"
                            };
                            let health_color = if svc_healthy && mem_pct < 90.0 {
                                Color::Green
                            } else {
                                Color::Yellow
                            };

                            let selector = if is_node_selected && nodes_focused {
                                "▸"
                            } else {
                                " "
                            };
                            let name_style = if is_node_selected {
                                Style::default()
                                    .fg(Color::White)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(Color::White)
                            };

                            lines.push(Line::from(vec![
                                Span::raw("     "),
                                Span::styled(
                                    format!("{} {} ", selector, health_symbol),
                                    Style::default().fg(health_color),
                                ),
                                Span::styled(node_name, name_style),
                            ]));
                        }
                    }
                }

                // Discovery warning: worker enumeration failed, so the list
                // above may be control-plane-only. Surface it instead of
                // silently hiding workers. Indented + wrapped so it reads as
                // part of this cluster's tray rather than a clipped stray line.
                if let Some(warning) = &cluster.discovery_warning {
                    lines.extend(warning_lines(warning, pane_layout[0].width));
                }

                // KUBECONFIG points at a different cluster than this context, so
                // Kubernetes views ignore it and pin to a control plane node.
                // Explain that rather than let it be a silent surprise.
                if let Some(warning) = &cluster.kubeconfig_warning {
                    lines.extend(warning_lines(warning, pane_layout[0].width));
                }
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
        let sep_color = if menu_focused {
            Color::Cyan
        } else {
            Color::DarkGray
        };
        let sep_text = if menu_focused {
            " Navigate ".to_string()
        } else {
            "─".repeat(area.width as usize)
        };
        lines.push(Line::from(Span::styled(
            sep_text,
            Style::default().fg(sep_color),
        )));

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
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if is_selected && !menu_focused {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };

            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {} ", selector),
                    if show_selector {
                        Style::default().fg(Color::Cyan)
                    } else {
                        Style::default()
                    },
                ),
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
        format!(
            "{}{}{:>3}%",
            "█".repeat(filled),
            "░".repeat(empty),
            pct as u8
        )
    }

    /// Draw the details pane (right column)
    fn draw_details_pane(&self, frame: &mut Frame, area: Rect) {
        // Focus indication - cyan border when focused
        let border_color = if self.focused_pane == FocusedPane::Services {
            Color::Cyan
        } else {
            Color::DarkGray
        };

        // Get active cluster data
        let cluster_idx = self.active_cluster;
        let cluster = self.clusters.get(cluster_idx);

        // A disconnected cluster that failed to connect has no nodes to select,
        // so show the failure details here (endpoints tried + cause + hint).
        if let Some(c) = cluster
            && !c.connected
            && let Some(err) = &c.error
        {
            let block = Block::default()
                .title(" Connection Error ")
                .title_style(Style::default().fg(Color::Red))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color));

            let mut lines = vec![
                Line::from(""),
                Line::from(vec![
                    Span::raw("  Cluster:  "),
                    Span::styled(
                        c.name.clone(),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(vec![
                    Span::raw("  Status:   "),
                    Span::styled("Disconnected", Style::default().fg(Color::Red)),
                ]),
            ];

            if !c.endpoints.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "  Endpoints tried:",
                    Style::default().fg(Color::Yellow),
                )));
                for ep in &c.endpoints {
                    lines.push(Line::from(vec![
                        Span::raw("    "),
                        Span::styled("- ", Style::default().fg(Color::DarkGray)),
                        Span::styled(ep.clone(), Style::default().fg(Color::White)),
                    ]));
                }
            }

            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  Error:",
                Style::default().fg(Color::Yellow),
            )));
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(err.clone(), Style::default().fg(Color::Red)),
            ]));

            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  Verify the endpoint(s) are reachable from this",
                Style::default().dim(),
            )));
            lines.push(Line::from(Span::styled(
                "  machine and that your talosconfig is correct.",
                Style::default().dim(),
            )));

            let msg = Paragraph::new(lines)
                .block(block)
                .wrap(ratatui::widgets::Wrap { trim: false });
            frame.render_widget(msg, area);
            return;
        }

        // Check if cluster is connected but has no etcd members (not bootstrapped)
        let needs_bootstrap = cluster
            .map(|c| c.connected && c.etcd_members.is_empty() && c.versions.is_empty())
            .unwrap_or(false);

        if needs_bootstrap {
            let block = Block::default()
                .title(" Bootstrap Required ")
                .title_style(Style::default().fg(Color::Yellow))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color));

            // Get control plane IP from talosconfig endpoints
            let config_result = match &self.config_path {
                Some(path) => {
                    let path_buf = std::path::PathBuf::from(path);
                    TalosConfig::load_from(&path_buf)
                }
                None => TalosConfig::load_default(),
            };
            let cp_ip = config_result
                .ok()
                .and_then(|config| {
                    config
                        .current_context()
                        .and_then(|ctx| ctx.endpoints.first())
                        .map(|e| e.split(':').next().unwrap_or(e).to_string())
                })
                .unwrap_or_else(|| "<control-plane-ip>".to_string());

            let lines = vec![
                Line::from(""),
                Line::from(vec![Span::styled(
                    "  Cluster not yet bootstrapped.",
                    Style::default().fg(Color::Yellow),
                )]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    "  To bootstrap, run:",
                    Style::default().dim(),
                )]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    format!("    talosctl bootstrap -n {}", cp_ip),
                    Style::default().fg(Color::Cyan),
                )]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    "  This initializes etcd and starts",
                    Style::default().dim(),
                )]),
                Line::from(vec![Span::styled(
                    "  the Kubernetes control plane.",
                    Style::default().dim(),
                )]),
            ];

            let msg = Paragraph::new(lines).block(block);
            frame.render_widget(msg, area);
            return;
        }

        let versions_empty = cluster.map(|c| c.versions.is_empty()).unwrap_or(true);
        if versions_empty {
            let block = Block::default()
                .title(" Details ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color));
            let msg = Paragraph::new(Line::from(Span::styled(
                "  No node selected",
                Style::default().dim(),
            )))
            .block(block);
            frame.render_widget(msg, area);
            return;
        }

        // Check if we have a node selected (vs a header)
        let Some(node_name_str) = self.current_node_name() else {
            // Header selected - show group summary
            let (title, count) = match &self.selected_item {
                NodeListItem::ClusterHeader(idx) => {
                    let name = self
                        .clusters
                        .get(*idx)
                        .map(|c| c.name.as_str())
                        .unwrap_or("Cluster");
                    let node_count = self
                        .clusters
                        .get(*idx)
                        .map(|c| c.versions.len())
                        .unwrap_or(0);
                    (name.to_string(), node_count)
                }
                NodeListItem::ControlPlaneHeader(idx) => (
                    "Control Plane".to_string(),
                    self.controlplane_nodes_for(*idx).len(),
                ),
                NodeListItem::WorkersHeader(idx) => {
                    ("Workers".to_string(), self.worker_nodes_for(*idx).len())
                }
                _ => ("Details".to_string(), 0),
            };
            let block = Block::default()
                .title(format!(" {} ", title))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color));
            let msg = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("  {} nodes in this group", count),
                    Style::default().dim(),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  Press Enter or Space to expand/collapse",
                    Style::default().dim(),
                )),
                Line::from(Span::styled(
                    "  Navigate down to select a node",
                    Style::default().dim(),
                )),
            ])
            .block(block);
            frame.render_widget(msg, area);
            return;
        };

        let node_name = if node_name_str.is_empty() {
            "node-0".to_string()
        } else {
            node_name_str.clone()
        };
        let node_ip = self.node_ips().get(&node_name).cloned().unwrap_or_default();
        let role = if self
            .get_node_services(&node_name)
            .map(|s| s.iter().any(|svc| svc.id == "etcd"))
            .unwrap_or(false)
        {
            "controlplane"
        } else {
            "worker"
        };

        let title = format!(" {} · {} ", node_name, role);
        let block = Block::default()
            .title(title)
            .title_style(
                Style::default().fg(if self.focused_pane == FocusedPane::Services {
                    Color::Cyan
                } else {
                    Color::White
                }),
            )
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
        let mut resource_lines = vec![Line::from(vec![
            Span::styled(" IP: ", Style::default().dim()),
            Span::styled(&node_ip, Style::default().fg(Color::DarkGray)),
        ])];

        // Memory bar
        if let Some(mem) = self.get_node_memory(&node_name) {
            let pct = mem.usage_percent();
            let used_gb = (mem.mem_total - mem.mem_available) as f64 / 1024.0 / 1024.0 / 1024.0;
            let total_gb = mem.mem_total as f64 / 1024.0 / 1024.0 / 1024.0;
            let bar = Self::render_compact_bar(pct, 10);
            let color = if pct > 90.0 {
                Color::Red
            } else if pct > 70.0 {
                Color::Yellow
            } else {
                Color::Green
            };
            resource_lines.push(Line::from(vec![
                Span::styled(" Memory: ", Style::default().dim()),
                Span::styled(bar, Style::default().fg(color)),
                Span::styled(
                    format!(" {:.1}/{:.1}GB", used_gb, total_gb),
                    Style::default().dim(),
                ),
            ]));
        }

        // Load average
        if let Some(load) = self.get_node_load_avg(&node_name) {
            let color = if load.load1 > 4.0 {
                Color::Red
            } else if load.load1 > 2.0 {
                Color::Yellow
            } else {
                Color::Green
            };
            resource_lines.push(Line::from(vec![
                Span::styled(" Load:   ", Style::default().dim()),
                Span::styled(format!("{:.2}", load.load1), Style::default().fg(color)),
                Span::styled(
                    format!(" {:.2} {:.2} (1/5/15m)", load.load5, load.load15),
                    Style::default().dim(),
                ),
            ]));
        }

        // CPU info
        if let Some(cpu) = self.get_node_cpu_info(&node_name) {
            resource_lines.push(Line::from(vec![
                Span::styled(" CPU:    ", Style::default().dim()),
                Span::styled(
                    format!("{} cores", cpu.cpu_count),
                    Style::default().fg(Color::White),
                ),
                Span::styled(format!(" @ {:.0}MHz", cpu.mhz), Style::default().dim()),
            ]));
        }

        frame.render_widget(Paragraph::new(resource_lines), panel_layout[0]);

        // Services section
        if let Some(services) = self.get_node_services(&node_name) {
            let running = services.iter().filter(|s| s.state == "Running").count();
            let mut svc_lines = vec![Line::from(vec![Span::styled(
                format!(" Services ({}/{})", running, services.len()),
                Style::default().fg(Color::Gray),
            )])];

            for (i, svc) in services.iter().enumerate() {
                let health_symbol = svc
                    .health
                    .as_ref()
                    .map(|h| if h.healthy { "●" } else { "○" })
                    .unwrap_or("●");
                let health_color = if health_symbol == "●" {
                    Color::Green
                } else {
                    Color::Red
                };

                // Highlight selected service when services pane is focused
                let is_selected =
                    i == self.selected_service && self.focused_pane == FocusedPane::Services;
                let name_style = if is_selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
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

#[cfg(test)]
mod tests {
    use super::*;
    use talos_rs::{DiscoveryMember, NodeServices, ServiceInfo, VersionInfo};

    fn ver(node: &str) -> VersionInfo {
        VersionInfo {
            node: node.to_string(),
            version: String::new(),
            sha: String::new(),
            built: String::new(),
            go_version: String::new(),
            os: String::new(),
            arch: String::new(),
            platform: String::new(),
        }
    }

    fn member(hostname: &str, ip: &str, machine_type: &str) -> DiscoveryMember {
        DiscoveryMember {
            id: hostname.to_string(),
            addresses: vec![ip.to_string()],
            hostname: hostname.to_string(),
            machine_type: machine_type.to_string(),
            operating_system: String::new(),
        }
    }

    fn services_with(node: &str, ids: &[&str]) -> NodeServices {
        NodeServices {
            node: node.to_string(),
            services: ids
                .iter()
                .map(|id| ServiceInfo {
                    id: id.to_string(),
                    state: "Running".to_string(),
                    health: None,
                })
                .collect(),
        }
    }

    fn names(nodes: &[(usize, &VersionInfo)]) -> Vec<String> {
        nodes.iter().map(|(_, v)| v.node.clone()).collect()
    }

    /// Discovery machineType is authoritative: a discovered worker is classified
    /// as a worker even when no per-node service data is available. This is the
    /// regression guard for "workers missing / shown only as control plane"
    /// (the "worker nodes missing" reports) — workers must land in the worker group.
    #[test]
    fn classifies_by_discovery_machine_type() {
        let mut comp = ClusterComponent::default();
        comp.clusters.push(ClusterData {
            name: "test".to_string(),
            versions: vec![ver("cp1"), ver("worker1")],
            discovery_members: vec![
                member("cp1", "10.0.0.1", "controlplane"),
                member("worker1", "10.0.0.2", "worker"),
            ],
            ..Default::default()
        });

        assert_eq!(names(&comp.controlplane_nodes_for(0)), vec!["cp1"]);
        assert_eq!(names(&comp.worker_nodes_for(0)), vec!["worker1"]);
    }

    /// When discovery is unavailable (empty members), fall back to the
    /// etcd-service heuristic so control plane nodes are still identified.
    #[test]
    fn falls_back_to_etcd_service_without_discovery() {
        let mut comp = ClusterComponent::default();
        comp.clusters.push(ClusterData {
            name: "test".to_string(),
            versions: vec![ver("n1"), ver("n2")],
            services: vec![
                services_with("n1", &["etcd", "kubelet"]),
                services_with("n2", &["kubelet"]),
            ],
            ..Default::default()
        });

        assert_eq!(names(&comp.controlplane_nodes_for(0)), vec!["n1"]);
        assert_eq!(names(&comp.worker_nodes_for(0)), vec!["n2"]);
    }

    /// Control-plane and worker groups must partition the node set: every node
    /// lands in exactly one group, never both or neither.
    #[test]
    fn groups_are_complementary() {
        let mut comp = ClusterComponent::default();
        comp.clusters.push(ClusterData {
            name: "test".to_string(),
            versions: vec![ver("a"), ver("b"), ver("c")],
            discovery_members: vec![
                member("a", "10.0.0.1", "controlplane"),
                member("b", "10.0.0.2", "worker"),
                // "c" is intentionally absent from discovery and has no services
            ],
            ..Default::default()
        });

        let cp = names(&comp.controlplane_nodes_for(0));
        let workers = names(&comp.worker_nodes_for(0));
        assert_eq!(cp.len() + workers.len(), 3, "every node classified once");
        // Unknown node "c" defaults to worker (safe default, still visible).
        assert!(workers.contains(&"c".to_string()));
        assert!(cp.contains(&"a".to_string()));
        assert!(workers.contains(&"b".to_string()));
    }

    /// End-to-end for the discovery-disabled path: nodes enumerated via the
    /// Kubernetes API classify into the right groups, so workers appear even
    /// when Talos discovery is off. Regression guard for the "workers missing
    /// when discovery disabled" bug.
    #[test]
    fn k8s_derived_members_populate_worker_group() {
        use crate::components::diagnostics::k8s::{K8sNodeInfo, k8s_nodes_to_discovery_members};
        let members = k8s_nodes_to_discovery_members(vec![
            K8sNodeInfo {
                name: "cp1".into(),
                internal_ip: Some("10.0.0.1".into()),
                is_control_plane: true,
            },
            K8sNodeInfo {
                name: "w1".into(),
                internal_ip: Some("10.0.0.2".into()),
                is_control_plane: false,
            },
            K8sNodeInfo {
                name: "w2".into(),
                internal_ip: Some("10.0.0.3".into()),
                is_control_plane: false,
            },
        ]);
        let mut comp = ClusterComponent::default();
        comp.clusters.push(ClusterData {
            name: "test".to_string(),
            versions: vec![ver("cp1"), ver("w1"), ver("w2")],
            discovery_members: members,
            ..Default::default()
        });
        assert_eq!(names(&comp.controlplane_nodes_for(0)), vec!["cp1"]);
        assert_eq!(names(&comp.worker_nodes_for(0)), vec!["w1", "w2"]);
    }

    /// `control_plane_ip` picks a real control plane address to fetch a
    /// cluster-correct kubeconfig from. It prefers an etcd member (etcd only
    /// runs on control planes), then a roster `controlplane` member, then the
    /// endpoint. Guards the fix that pins the K8s client to the launched
    /// `--context` instead of an ambient KUBECONFIG.
    #[test]
    fn control_plane_ip_prefers_etcd_then_roster_then_endpoint() {
        let etcd_member = EtcdMemberInfo {
            id: 1,
            hostname: "cp1".into(),
            peer_urls: vec!["https://10.0.0.1:2380".into()],
            client_urls: vec!["https://10.0.0.1:2379".into()],
            is_learner: false,
        };
        let cp_roster = DiscoveryMember {
            id: "cp2".into(),
            addresses: vec!["10.0.0.9".into()],
            hostname: "cp2".into(),
            machine_type: "controlplane".into(),
            operating_system: String::new(),
        };
        let worker_roster = DiscoveryMember {
            id: "w1".into(),
            addresses: vec!["10.0.0.2".into()],
            hostname: "w1".into(),
            machine_type: "worker".into(),
            operating_system: String::new(),
        };

        // etcd member wins over roster and endpoint.
        let mut comp = ClusterComponent::default();
        comp.clusters.push(ClusterData {
            name: "test".to_string(),
            etcd_members: vec![etcd_member],
            discovery_members: vec![cp_roster.clone(), worker_roster.clone()],
            endpoints: vec!["10.0.0.5:50000".to_string()],
            ..Default::default()
        });
        assert_eq!(comp.control_plane_ip().as_deref(), Some("10.0.0.1"));

        // No etcd: a roster controlplane member wins over a worker and endpoint.
        let mut comp = ClusterComponent::default();
        comp.clusters.push(ClusterData {
            name: "test".to_string(),
            discovery_members: vec![worker_roster, cp_roster],
            endpoints: vec!["10.0.0.5:50000".to_string()],
            ..Default::default()
        });
        assert_eq!(comp.control_plane_ip().as_deref(), Some("10.0.0.9"));

        // Nothing but an endpoint: fall back to it, port stripped.
        let mut comp = ClusterComponent::default();
        comp.clusters.push(ClusterData {
            name: "test".to_string(),
            endpoints: vec!["10.0.0.5:50000".to_string()],
            ..Default::default()
        });
        assert_eq!(comp.control_plane_ip().as_deref(), Some("10.0.0.5"));
    }
}
