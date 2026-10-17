//! Cluster component - displays cluster overview with nodes

use crate::action::Action;
use crate::components::diagnostics::k8s;
use crate::components::{Component, ProcessNode};
use crate::reachability::prefer_reachable_addresses;
use crate::zfs::{ZfsPool, fetch_zfs_pools};
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
};
use std::collections::HashMap;
use talos_rs::{
    DiscoveryMember, EtcdMemberInfo, MemInfo, NodeCpuInfo, NodeLoadAvg, NodeMemory, NodeMounts,
    NodeNetworkInfo, NodeServices, NodeSystemStat, ServiceInfo, TalosClient, TalosConfig,
    VersionInfo, get_discovery_members_with_retry,
};

/// The value occurring most often (ties go to the first seen), or `None` if empty
fn most_common<'a>(values: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for v in values {
        match counts.iter_mut().find(|(c, _)| *c == v) {
            Some((_, n)) => *n += 1,
            None => counts.push((v, 1)),
        }
    }
    counts
        .into_iter()
        .rev()
        .max_by_key(|(_, n)| *n)
        .map(|(v, _)| v)
}

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

/// How long a node that no source lists any more (not in discovery or etcd)
/// is still shown as offline before it is dropped from the list. Nodes that
/// discovery or etcd still list stay as offline for as long as they are listed.
const OFFLINE_NODE_GRACE: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Upper bound for the per-node Version call that decides whether a node is
/// online; an unreachable node otherwise waits out apid's ~20s proxy dial.
const NODE_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// A known node that did not answer the last refresh (e.g. rebooting)
#[derive(Debug, Clone)]
struct OfflineNode {
    /// When the node first failed to answer
    since: std::time::Instant,
    /// Role it had while it was online, used when no source lists it any more
    controlplane: bool,
}

/// Version entry standing in for a node that did not answer, so it keeps its
/// place in the node list
fn offline_version(node: &str) -> VersionInfo {
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
    /// talosconfig context, kept so a failed connection can be retried
    context: Option<talos_rs::Context>,
    /// True until the first connect + refresh attempt has finished
    loading: bool,
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
    /// System stats (boot time) from nodes
    system_stat: Vec<NodeSystemStat>,
    /// Filesystem usage from nodes
    mounts: Vec<NodeMounts>,
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
    /// Kubernetes client, created on first use and reused across refreshes
    k8s_client: Option<kube::Client>,
    /// ZFS pools per node name (from OpenEBS ZFSNode resources)
    zfs_pools: HashMap<String, Vec<ZfsPool>>,
    /// Kubelet version per lowercased Kubernetes node name
    kubelet_versions: HashMap<String, String>,
    /// Known nodes that did not answer the last refresh, by node name. They
    /// stay in `versions` (as placeholders) so they show as offline.
    offline: HashMap<String, OfflineNode>,
}

impl ClusterData {
    /// Refresh this cluster's data: etcd, roster, per-node stats.
    ///
    /// Operates only on this `ClusterData`, so it can run on a clone in a
    /// background task while the UI keeps drawing (see `start_background_refresh`).
    async fn refresh(&mut self, config_path: Option<&str>) {
        let cluster = self;
        cluster.loading = false;

        // Not connected yet (startup) or the last attempt failed: (re)connect
        if cluster.client.is_none()
            && let Some(ctx) = &cluster.context
        {
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

        let Some(client) = &cluster.client else {
            return;
        };

        // Clone client to avoid borrow issues
        let client = client.clone();

        // Nodes shown before this refresh, with address and role, so one that
        // stops answering (e.g. rebooting) is kept as offline, not dropped.
        let previous: Vec<(String, String, bool)> = cluster
            .versions
            .iter()
            .filter_map(|v| {
                let ip = cluster.node_ips.get(&v.node)?.clone();
                Some((v.node.clone(), ip, cluster.is_controlplane(&v.node)))
            })
            .collect();

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
        match get_discovery_members_with_retry(&context_name, config_path, &fallback_ips).await {
            Ok(mut members) if !members.is_empty() => {
                prefer_reachable_addresses(&client, &mut members, &cluster.known_good_ips()).await;
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
                                Ok(mut members) if !members.is_empty() => {
                                    prefer_reachable_addresses(
                                        &client,
                                        &mut members,
                                        &cluster.known_good_ips(),
                                    )
                                    .await;
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
        let mut nodes_to_query: Vec<(String, String)> = if !cluster.discovery_members.is_empty() {
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

        // Also query etcd members discovery doesn't list (a rebooting control
        // plane can drop out of discovery while it is still an etcd member) and
        // recently seen nodes nothing lists any more, so they show as offline.
        let listed = |nodes: &[(String, String)], name: &str, ip: &str| {
            nodes.iter().any(|(n, i)| n == name || i == ip)
        };
        for m in &cluster.etcd_members {
            if let Some(ip) = m.ip_address()
                && !m.hostname.is_empty()
                && !listed(&nodes_to_query, &m.hostname, &ip)
            {
                nodes_to_query.push((m.hostname.clone(), ip));
            }
        }
        for (name, ip, _) in &previous {
            let expired = cluster
                .offline
                .get(name)
                .is_some_and(|o| o.since.elapsed() > OFFLINE_NODE_GRACE);
            if !expired && !listed(&nodes_to_query, name, ip) {
                nodes_to_query.push((name.clone(), ip.clone()));
            }
        }
        for (name, ip) in &nodes_to_query {
            cluster
                .node_ips
                .entry(name.clone())
                .or_insert_with(|| ip.clone());
        }

        // Query each node
        if !nodes_to_query.is_empty() {
            let mut versions = Vec::new();
            let mut services = Vec::new();
            let mut memory = Vec::new();
            let mut load_avg = Vec::new();
            let mut cpu_info = Vec::new();
            let mut system_stat = Vec::new();
            let mut mounts = Vec::new();
            let mut offline = HashMap::new();

            for (node_name, ip) in &nodes_to_query {
                let node_client = client.with_node(ip);

                match tokio::time::timeout(NODE_PROBE_TIMEOUT, node_client.version()).await {
                    Ok(Ok(mut nv)) => {
                        for v in &mut nv {
                            v.node = node_name.clone();
                        }
                        versions.extend(nv);
                    }
                    outcome => {
                        // Not answering: keep it listed as offline and skip the
                        // remaining calls, which would only time out as well.
                        if let Ok(Err(e)) = &outcome {
                            tracing::debug!("Node {} ({}) is offline: {}", node_name, ip, e);
                        }
                        let since = cluster
                            .offline
                            .get(node_name)
                            .map_or_else(std::time::Instant::now, |o| o.since);
                        let controlplane = previous
                            .iter()
                            .find(|(n, _, _)| n == node_name)
                            .map_or_else(|| cluster.is_controlplane(node_name), |p| p.2);
                        offline.insert(
                            node_name.clone(),
                            OfflineNode {
                                since,
                                controlplane,
                            },
                        );
                        versions.push(offline_version(node_name));
                        continue;
                    }
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
                if let Ok(mut ns) = node_client.system_stat().await {
                    for s in &mut ns {
                        s.node = node_name.clone();
                    }
                    system_stat.extend(ns);
                }
                if let Ok(mut nm) = node_client.mounts().await {
                    for m in &mut nm {
                        m.node = node_name.clone();
                    }
                    mounts.extend(nm);
                }
            }

            // Need to re-borrow cluster mutably after async calls
            {
                cluster.versions = versions;
                cluster.offline = offline;
                cluster.services = services;
                cluster.memory = memory;
                cluster.load_avg = load_avg;
                cluster.cpu_info = cpu_info;
                cluster.system_stat = system_stat;
                cluster.mounts = mounts;

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
        {
            // Offline placeholders don't count: they are nodes that didn't answer
            let has_any_data = cluster.versions.len() > cluster.offline.len()
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

        let cp_ip = cluster.control_plane_ip();

        // ZFS pool capacity and kubelet versions from the Kubernetes API.
        // Bounded so an unreachable kube-apiserver can't stall the refresh; on
        // failure the previous values are kept.
        let k8s_data = tokio::time::timeout(K8S_QUERY_TIMEOUT, async {
            if cluster.k8s_client.is_none() {
                cluster.k8s_client =
                    k8s::create_k8s_client_with_source(&client, cp_ip.as_deref(), None)
                        .await
                        .ok()
                        .map(|(client, _)| client);
            }
            let k8s = cluster.k8s_client.as_ref()?;
            Some(tokio::join!(
                fetch_zfs_pools(k8s),
                fetch_kubelet_versions(k8s)
            ))
        })
        .await;
        match k8s_data {
            Ok(Some((zfs, kubelets))) => {
                match zfs {
                    Ok(pools) => cluster.zfs_pools = pools,
                    Err(e) => tracing::debug!("ZFS pools unavailable for {}: {}", cluster.name, e),
                }
                match kubelets {
                    Ok(versions) => cluster.kubelet_versions = versions,
                    Err(e) => {
                        tracing::debug!("Kubelet versions unavailable for {}: {}", cluster.name, e)
                    }
                }
            }
            Ok(None) => tracing::debug!("Kubernetes API unavailable for {}", cluster.name),
            Err(_) => tracing::debug!("Kubernetes query timed out for {}", cluster.name),
        }
    }

    /// Whether a node is a control plane node.
    ///
    /// Discovery `machineType` is authoritative when available; then etcd
    /// membership (only control planes run etcd), the role remembered for an
    /// offline node, and finally the etcd service. Keeping these in one place
    /// ensures the control-plane and worker groups stay complementary.
    fn is_controlplane(&self, node: &str) -> bool {
        let key = node.split(':').next().unwrap_or(node);
        if let Some(member) = self.discovery_members.iter().find(|m| {
            m.hostname.eq_ignore_ascii_case(node)
                || m.hostname.eq_ignore_ascii_case(key)
                || m.addresses.iter().any(|a| a == node || a == key)
        }) {
            return member.machine_type.eq_ignore_ascii_case("controlplane");
        }
        if self
            .etcd_members
            .iter()
            .any(|m| m.hostname.eq_ignore_ascii_case(key))
        {
            return true;
        }
        if let Some(offline) = self.offline.get(node) {
            return offline.controlplane;
        }
        self.services
            .iter()
            .find(|s| s.node == node)
            .is_some_and(|s| s.services.iter().any(|svc| svc.id == "etcd"))
    }

    /// Addresses known to reach their node: etcd member IPs (etcd peers talk
    /// over them) and the addresses chosen on the previous refresh. Tried first
    /// so a healthy refresh needs a single probe per node.
    fn known_good_ips(&self) -> Vec<String> {
        self.etcd_members
            .iter()
            .filter_map(|m| m.ip_address())
            .chain(
                self.discovery_members
                    .iter()
                    .filter_map(|m| m.addresses.first().cloned()),
            )
            .collect()
    }

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
    /// In-flight background refresh per cluster, same order as `clusters`
    /// (see `start_background_refresh`)
    background_refresh: Vec<Option<tokio::task::JoinHandle<ClusterData>>>,
    /// When each cluster's last background refresh finished
    cluster_refreshed_at: Vec<Option<std::time::Instant>>,
    /// Network summary per (context, node name), with the time of the last fetch attempt
    network_info: HashMap<(String, String), (std::time::Instant, Option<NodeNetworkInfo>)>,
    /// In-flight network summary fetch for the selected node(s) (see `poll_network_info`)
    network_fetch: Option<NetworkFetch>,
    /// Screen regions from the last draw, for mapping mouse clicks
    hits: std::cell::RefCell<HitMap>,
}

/// Where clickable things were drawn on the last frame
#[derive(Default)]
struct HitMap {
    /// Node list area and the item drawn on each of its rows (`None` for
    /// error/warning lines)
    node_list: Rect,
    node_rows: Vec<Option<NodeListItem>>,
    /// Navigation menu area; row 0 is the separator, row `i` is item `i`
    menu: Rect,
    /// Details pane area
    details: Rect,
    /// Services list area in the node details (row 0 is its heading) and the
    /// number of services drawn
    services: Option<(Rect, usize)>,
}

fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.x + area.width && row >= area.y && row < area.y + area.height
}

/// Background `get_node_network_info` calls, keyed by (context, node name)
type NetworkFetch = tokio::task::JoinHandle<
    Vec<(
        (String, String),
        Result<NodeNetworkInfo, talos_rs::TalosError>,
    )>,
>;

/// How often the selected node's network summary is re-fetched. It changes
/// rarely and costs four talosctl invocations, so this is much slower than
/// the stats auto-refresh.
const NETWORK_INFO_REFRESH: std::time::Duration = std::time::Duration::from_secs(30);

impl Default for ClusterComponent {
    fn default() -> Self {
        Self::new(None, None)
    }
}

/// Whether a service is in a good state: running and not failing its health
/// check, or a one-shot service that finished (e.g. `ext-nvidia-cdi-gen`).
fn service_ok(s: &ServiceInfo) -> bool {
    match s.state.as_str() {
        "Running" => s.is_healthy() != Some(false),
        "Finished" => true,
        _ => false,
    }
}

/// Matrix symbol and style for one service on one node
fn service_symbol(s: &ServiceInfo) -> (&'static str, Style) {
    match (s.state.as_str(), s.is_healthy()) {
        ("Running", Some(true)) => ("●", Style::default().fg(Color::Green)),
        ("Running", None) => ("●", Style::default().fg(Color::DarkGray)),
        ("Running", Some(false)) => ("○", Style::default().fg(Color::Red)),
        ("Finished", _) => ("✓", Style::default().fg(Color::Green)),
        _ => ("✗", Style::default().fg(Color::Red)),
    }
}

/// Current time in seconds since the Unix epoch
fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Format an uptime in seconds, e.g. "5d 17h"
fn format_uptime(secs: u64) -> String {
    talos_pilot_core::format_duration_detailed(chrono::Duration::seconds(secs as i64))
}

/// Color for a memory usage percentage
fn memory_color(pct: f32) -> Color {
    if pct > 90.0 {
        Color::Red
    } else if pct > 70.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}

/// Upper bound for the kube-apiserver queries (ZFS pools, kubelet versions)
/// in a cluster refresh
const K8S_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Kubelet version of every Kubernetes node, keyed by lowercased node name
async fn fetch_kubelet_versions(
    client: &kube::Client,
) -> Result<HashMap<String, String>, kube::Error> {
    let nodes: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(client.clone());
    Ok(nodes
        .list(&kube::api::ListParams::default())
        .await?
        .items
        .into_iter()
        .filter_map(|node| {
            let version = node.status?.node_info?.kubelet_version;
            Some((node.metadata.name?.to_lowercase(), version))
        })
        .collect())
}

/// Bytes to GiB, for display
fn gib(bytes: u64) -> f64 {
    bytes as f64 / 1024.0 / 1024.0 / 1024.0
}

/// Color for a 1-minute load average
fn load_color(load1: f64) -> Color {
    if load1 > 4.0 {
        Color::Red
    } else if load1 > 2.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}

/// Render `label` followed by `items` joined with ", ", breaking only between
/// items so an address is never split. Continuation lines are indented to
/// align with the first item. Returns no lines when `items` is empty.
fn wrap_list(label: &str, items: &[String], width: usize, style: Style) -> Vec<Line<'static>> {
    let indent = label.chars().count();
    let mut rows: Vec<String> = Vec::new();
    let mut row = String::new();
    for (i, item) in items.iter().enumerate() {
        let sep = if i + 1 < items.len() { "," } else { "" };
        let piece = format!("{item}{sep}");
        if !row.is_empty() && indent + row.chars().count() + 1 + piece.chars().count() > width {
            rows.push(std::mem::take(&mut row));
        }
        if !row.is_empty() {
            row.push(' ');
        }
        row.push_str(&piece);
    }
    if !row.is_empty() {
        rows.push(row);
    }

    rows.into_iter()
        .enumerate()
        .map(|(i, row)| {
            let prefix = if i == 0 {
                Span::styled(label.to_string(), Style::default().dim())
            } else {
                Span::raw(" ".repeat(indent))
            };
            Line::from(vec![prefix, Span::styled(row, style)])
        })
        .collect()
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
            background_refresh: Vec::new(),
            cluster_refreshed_at: Vec::new(),
            network_info: HashMap::new(),
            network_fetch: None,
            hits: std::cell::RefCell::new(HitMap::default()),
        }
    }

    /// Processes view for the selection: the selected node, or every online
    /// node under the selected cluster / control plane / workers header
    fn processes_action(&self) -> Option<Action> {
        let (cluster_idx, names) = self.selected_nodes()?;
        let cluster = self.clusters.get(cluster_idx)?;
        let nodes: Vec<ProcessNode> = names
            .into_iter()
            .filter(|name| !cluster.offline.contains_key(name))
            .map(|name| ProcessNode {
                address: cluster
                    .node_ips
                    .get(&name)
                    .cloned()
                    .unwrap_or_else(|| name.clone()),
                hostname: name,
            })
            .collect();
        let title = match &self.selected_item {
            NodeListItem::ClusterHeader(_) => cluster.name.clone(),
            NodeListItem::ControlPlaneHeader(_) => format!("{} control plane", cluster.name),
            NodeListItem::WorkersHeader(_) => format!("{} workers", cluster.name),
            _ => nodes.first()?.hostname.clone(),
        };
        (!nodes.is_empty()).then_some(Action::ShowProcesses(title, nodes))
    }

    /// Workloads view for the selection: the whole cluster for a cluster
    /// header, otherwise only the pods on the selected node(s). Offline nodes
    /// stay in scope, since their pods are the ones most likely in trouble.
    fn workloads_action(&self) -> Action {
        let selected = self
            .selected_nodes()
            .and_then(|(idx, names)| Some((self.clusters.get(idx)?, names)));
        match (selected, &self.selected_item) {
            (Some((cluster, _)), NodeListItem::ClusterHeader(_)) => {
                Action::ShowWorkloads(cluster.name.clone(), None)
            }
            (Some((cluster, names)), item) if !names.is_empty() => {
                let title = match item {
                    NodeListItem::ControlPlaneHeader(_) => {
                        format!("{} control plane", cluster.name)
                    }
                    NodeListItem::WorkersHeader(_) => format!("{} workers", cluster.name),
                    _ => names[0].clone(),
                };
                Action::ShowWorkloads(title, Some(names))
            }
            _ => Action::ShowWorkloads(
                self.current_context_name().unwrap_or("cluster").to_string(),
                None,
            ),
        }
    }

    /// Whether a node is a control plane node (see `ClusterData::is_controlplane`)
    fn node_is_controlplane(&self, cluster_idx: usize, node: &str) -> bool {
        self.clusters
            .get(cluster_idx)
            .is_some_and(|c| c.is_controlplane(node))
    }

    /// How long a node has not been answering, or `None` when it is online
    fn node_offline_for(&self, cluster_idx: usize, node: &str) -> Option<std::time::Duration> {
        self.clusters
            .get(cluster_idx)?
            .offline
            .get(node)
            .map(|o| o.since.elapsed())
    }

    /// Tree row for a node: health symbol and color, plus an "offline" note
    fn node_row_status(&self, cluster_idx: usize, node: &str) -> (&'static str, Color, String) {
        if let Some(down) = self.node_offline_for(cluster_idx, node) {
            return (
                "○",
                Color::Red,
                format!("  offline {}", format_uptime(down.as_secs())),
            );
        }
        let mem_pct = self
            .get_node_memory_for(cluster_idx, node)
            .map(|m| m.usage_percent())
            .unwrap_or(0.0);
        let svc_healthy = self
            .get_node_services_for(cluster_idx, node)
            .map(|services| services.iter().all(|s| s.is_healthy() != Some(false)))
            .unwrap_or(true);
        if svc_healthy && mem_pct < 90.0 {
            ("●", Color::Green, String::new())
        } else {
            ("◐", Color::Yellow, String::new())
        }
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
            None => {
                // talosconfig contexts are a map, so their order is arbitrary:
                // list the current context first, then the rest by name
                let mut names: Vec<String> = config.contexts.keys().cloned().collect();
                names.sort_by_key(|name| (*name != config.context, name.clone()));
                names
            }
        };

        // Create ClusterData for each context. Nothing is dialled here: every
        // cluster connects and loads in its own background task (see
        // `start_background_refresh`), so the UI shows up immediately and a
        // reachable cluster never waits behind an unreachable one.
        self.abort_background_refresh();
        self.clusters.clear();
        for (idx, name) in context_names.iter().enumerate() {
            let mut cluster = ClusterData {
                name: name.clone(),
                expanded: idx == 0, // Expand first cluster by default
                controlplane_expanded: true,
                workers_expanded: true,
                loading: true,
                ..Default::default()
            };
            match config.get_context(name) {
                Ok(ctx) => {
                    cluster.endpoints = ctx.endpoints.clone();
                    cluster.context = Some(ctx.clone());
                }
                Err(e) => {
                    cluster.error = Some(e.to_string());
                    cluster.loading = false;
                }
            }
            self.clusters.push(cluster);
        }
        self.background_refresh = self.clusters.iter().map(|_| None).collect();
        self.cluster_refreshed_at = self.clusters.iter().map(|_| None).collect();
        self.start_background_refresh();

        // Set initial selection
        if !self.clusters.is_empty() {
            self.selected_item = NodeListItem::ClusterHeader(0);
            self.active_cluster = 0;
        }

        Ok(())
    }

    /// Refresh all cluster data now (`r`): restart every cluster's
    /// background refresh. Returns immediately; results land as each cluster
    /// finishes.
    pub async fn refresh(&mut self) -> Result<()> {
        // Older in-flight snapshots would overwrite the fresh results
        self.abort_background_refresh();
        for at in &mut self.cluster_refreshed_at {
            *at = None;
        }
        self.start_background_refresh();
        Ok(())
    }

    fn abort_background_refresh(&mut self) {
        for handle in self.background_refresh.iter_mut().filter_map(Option::take) {
            handle.abort();
        }
    }

    /// Start a background refresh for each cluster that is due and has none
    /// in flight.
    ///
    /// Each cluster connects (if needed) and refreshes on a clone in its own
    /// spawned task, so the UI keeps drawing and a slow or unreachable cluster
    /// never delays the others. This also rebuilds node lists periodically,
    /// so a node that dropped out while rebooting reappears, and retries
    /// clusters that could not be reached. `apply_background_refresh` merges
    /// the results back.
    pub fn start_background_refresh(&mut self) {
        if self.background_refresh.len() != self.clusters.len() {
            return;
        }
        let interval =
            std::time::Duration::from_secs(talos_pilot_core::constants::refresh_intervals::SLOW);
        for (idx, cluster) in self.clusters.iter().enumerate() {
            if self.background_refresh[idx].is_some() || cluster.context.is_none() {
                continue;
            }
            // The first load always runs; after that, only with auto-refresh on
            let due = match self.cluster_refreshed_at[idx] {
                None => true,
                Some(at) => self.auto_refresh && at.elapsed() >= interval,
            };
            if !due {
                continue;
            }
            let mut cluster = cluster.clone();
            let config_path = self.config_path.clone();
            self.background_refresh[idx] = Some(tokio::spawn(async move {
                cluster.refresh(config_path.as_deref()).await;
                cluster
            }));
        }
    }

    /// Merge finished background refreshes, keeping UI state (expand/collapse)
    /// that the user may have changed while they ran.
    pub fn apply_background_refresh(&mut self) {
        use futures::FutureExt;

        let mut changed = false;
        for idx in 0..self.background_refresh.len() {
            if !self.background_refresh[idx]
                .as_ref()
                .is_some_and(|handle| handle.is_finished())
            {
                continue;
            }
            let Some(handle) = self.background_refresh[idx].take() else {
                continue;
            };
            // Always mark the attempt so a failing refresh is retried on
            // schedule, not on every tick.
            self.cluster_refreshed_at[idx] = Some(std::time::Instant::now());

            let mut fresh = match handle.now_or_never() {
                Some(Ok(fresh)) => fresh,
                Some(Err(e)) => {
                    tracing::warn!("Background cluster refresh failed: {}", e);
                    continue;
                }
                None => continue,
            };
            let Some(current) = self.clusters.get_mut(idx) else {
                continue;
            };
            if current.name != fresh.name {
                continue;
            }
            fresh.expanded = current.expanded;
            fresh.controlplane_expanded = current.controlplane_expanded;
            fresh.workers_expanded = current.workers_expanded;
            *current = fresh;
            changed = true;
        }
        if changed {
            self.last_refresh = Some(std::time::Instant::now());
            self.ensure_selection_visible();
        }
    }

    /// Node names covered by the current selection: the selected node, or
    /// every node under the selected cluster / control plane / workers header.
    fn selected_nodes(&self) -> Option<(usize, Vec<String>)> {
        let names = |nodes: Vec<(usize, &VersionInfo)>| -> Vec<String> {
            nodes.into_iter().map(|(_, v)| v.node.clone()).collect()
        };
        match &self.selected_item {
            NodeListItem::ClusterHeader(idx) => {
                let mut nodes = names(self.controlplane_nodes_for(*idx));
                nodes.extend(names(self.worker_nodes_for(*idx)));
                Some((*idx, nodes))
            }
            NodeListItem::ControlPlaneHeader(idx) => {
                Some((*idx, names(self.controlplane_nodes_for(*idx))))
            }
            NodeListItem::WorkersHeader(idx) => Some((*idx, names(self.worker_nodes_for(*idx)))),
            NodeListItem::ControlPlaneNode(idx, _) | NodeListItem::WorkerNode(idx, _) => {
                Some((*idx, vec![self.current_node_name()?]))
            }
        }
    }

    /// Collect a finished network summary fetch and start one for the selected
    /// node(s) whose summary is missing or older than `NETWORK_INFO_REFRESH`.
    ///
    /// Runs in a spawned task so slow talosctl calls never block drawing.
    pub fn poll_network_info(&mut self) {
        use futures::FutureExt;

        if self.network_fetch.as_ref().is_some_and(|f| f.is_finished())
            && let Some(fetch) = self.network_fetch.take()
        {
            let now = std::time::Instant::now();
            let results = match fetch.now_or_never() {
                Some(Ok(results)) => results,
                _ => Vec::new(),
            };
            for (key, result) in results {
                let previous = self.network_info.remove(&key).and_then(|(_, info)| info);
                let info = match result {
                    Ok(info) => Some(info),
                    Err(e) => {
                        tracing::debug!("Network info for {:?} failed: {}", key, e);
                        previous
                    }
                };
                self.network_info.insert(key, (now, info));
            }
        }

        if !self.auto_refresh || self.network_fetch.is_some() {
            return;
        }
        let Some((cluster_idx, nodes)) = self.selected_nodes() else {
            return;
        };
        let Some(cluster) = self.clusters.get(cluster_idx) else {
            return;
        };
        let targets: Vec<((String, String), String)> = nodes
            .into_iter()
            .filter_map(|node| {
                let ip = cluster.node_ips.get(&node)?.clone();
                let key = (cluster.name.clone(), node);
                let fresh = self
                    .network_info
                    .get(&key)
                    .is_some_and(|(at, _)| at.elapsed() < NETWORK_INFO_REFRESH);
                (!fresh).then_some((key, ip))
            })
            .collect();
        if targets.is_empty() {
            return;
        }

        let context = cluster.name.clone();
        let config_path = self.config_path.clone();
        self.network_fetch = Some(tokio::spawn(async move {
            let fetches = targets.into_iter().map(|(key, ip)| {
                let context = &context;
                let config_path = config_path.as_deref();
                async move {
                    let result = talos_rs::get_node_network_info(context, &ip, config_path).await;
                    (key, result)
                }
            });
            futures::future::join_all(fetches).await
        }));
    }

    /// Network summary for a node in a cluster, if fetched
    fn get_node_network_info_for(
        &self,
        cluster_idx: usize,
        node_name: &str,
    ) -> Option<&NodeNetworkInfo> {
        let cluster = self.clusters.get(cluster_idx)?;
        self.network_info
            .get(&(cluster.name.clone(), node_name.to_string()))?
            .1
            .as_ref()
    }

    /// Network summary for a node in the active cluster, if fetched
    fn get_node_network_info(&self, node_name: &str) -> Option<&NodeNetworkInfo> {
        self.get_node_network_info_for(self.active_cluster, node_name)
    }

    /// Reset the selection to its cluster header if a refresh removed the
    /// selected node (e.g. it went down), so the cursor never points at nothing.
    fn ensure_selection_visible(&mut self) {
        if !self.visible_items().contains(&self.selected_item) {
            self.selected_item = NodeListItem::ClusterHeader(self.active_cluster);
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
        // Offline nodes are re-checked by the cluster refresh; querying one
        // here would only wait out a timeout.
        if cluster.offline.contains_key(&node_name) {
            return Ok(());
        }

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

        if let Ok(mut node_stat) = node_client.system_stat().await {
            for s in &mut node_stat {
                s.node = node_name.clone();
            }
            if let Some(cluster) = self.clusters.get_mut(cluster_idx) {
                cluster.system_stat.retain(|s| s.node != node_name);
                cluster.system_stat.extend(node_stat);
            }
        }

        if let Ok(mut node_mounts) = node_client.mounts().await {
            for m in &mut node_mounts {
                m.node = node_name.clone();
            }
            if let Some(cluster) = self.clusters.get_mut(cluster_idx) {
                cluster.mounts.retain(|m| m.node != node_name);
                cluster.mounts.extend(node_mounts);
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

    /// Get system stats for a node in the active cluster
    fn get_node_system_stat(&self, node_name: &str) -> Option<&NodeSystemStat> {
        self.get_node_system_stat_for(self.active_cluster, node_name)
    }

    /// Get system stats for a node in a specific cluster
    fn get_node_system_stat_for(
        &self,
        cluster_idx: usize,
        node_name: &str,
    ) -> Option<&NodeSystemStat> {
        self.clusters
            .get(cluster_idx)?
            .system_stat
            .iter()
            .find(|s| s.node == node_name || (s.node.is_empty() && node_name.is_empty()))
    }

    /// Get filesystem usage for a node in a specific cluster
    fn get_node_mounts_for(&self, cluster_idx: usize, node_name: &str) -> Option<&NodeMounts> {
        self.clusters
            .get(cluster_idx)?
            .mounts
            .iter()
            .find(|m| m.node == node_name || (m.node.is_empty() && node_name.is_empty()))
    }

    /// Get ZFS pools for a node in a specific cluster
    fn get_node_zfs_pools_for(&self, cluster_idx: usize, node_name: &str) -> &[ZfsPool] {
        self.clusters
            .get(cluster_idx)
            .and_then(|c| c.zfs_pools.get(node_name))
            .map_or(&[], Vec::as_slice)
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
    /// Talos version a node reported (none for an offline placeholder)
    fn get_node_talos_version_for(&self, cluster_idx: usize, node_name: &str) -> Option<&str> {
        self.clusters
            .get(cluster_idx)?
            .versions
            .iter()
            .find(|v| v.node == node_name && !v.version.is_empty())
            .map(|v| v.version.as_str())
    }

    /// Kubelet version from the Kubernetes Node object of the same name
    fn get_node_kubelet_version_for(&self, cluster_idx: usize, node_name: &str) -> Option<&str> {
        let key = node_name.split(':').next().unwrap_or(node_name);
        self.clusters
            .get(cluster_idx)?
            .kubelet_versions
            .get(&key.to_lowercase())
            .map(String::as_str)
    }

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
            NavMenuItem::Processes => Ok(self.processes_action()),
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
            NavMenuItem::Workloads => Ok(Some(self.workloads_action())),
        }
    }
}

impl Component for ClusterComponent {
    fn handle_mouse_event(&mut self, mouse: MouseEvent) -> Result<Option<Action>> {
        let (column, row) = (mouse.column, mouse.row);
        let (in_nodes, in_menu, in_details) = {
            let hits = self.hits.borrow();
            (
                contains(hits.node_list, column, row),
                contains(hits.menu, column, row),
                contains(hits.details, column, row),
            )
        };

        match mouse.kind {
            // Wheel scrolls the pane under the pointer
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let pane = if in_details {
                    FocusedPane::Services
                } else if in_menu {
                    FocusedPane::Menu
                } else if in_nodes {
                    FocusedPane::Nodes
                } else {
                    return Ok(None);
                };
                self.focused_pane = pane;
                if pane == FocusedPane::Menu && self.selected_menu_item == 0 {
                    self.selected_menu_item = 1;
                }
                let code = if mouse.kind == MouseEventKind::ScrollUp {
                    KeyCode::Up
                } else {
                    KeyCode::Down
                };
                self.handle_key_event(KeyEvent::new(code, KeyModifiers::NONE))
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if in_nodes {
                    self.focused_pane = FocusedPane::Nodes;
                    let item = {
                        let hits = self.hits.borrow();
                        let idx = (row - hits.node_list.y) as usize;
                        hits.node_rows.get(idx).cloned().flatten()
                    };
                    if let Some(item) = item {
                        let is_header = matches!(
                            item,
                            NodeListItem::ClusterHeader(_)
                                | NodeListItem::ControlPlaneHeader(_)
                                | NodeListItem::WorkersHeader(_)
                        );
                        // Clicking the already-selected header folds/unfolds it
                        if is_header && item == self.selected_item {
                            self.toggle_expand();
                        } else {
                            self.selected_item = item;
                            if let Some(idx) = self.selected_cluster_index() {
                                self.active_cluster = idx;
                            }
                        }
                    }
                    Ok(None)
                } else if in_menu {
                    let idx = (row - self.hits.borrow().menu.y) as usize;
                    if (1..=NavMenuItem::ALL.len()).contains(&idx) {
                        self.focused_pane = FocusedPane::Menu;
                        self.selected_menu_item = idx;
                        return self.navigate_to_selected_menu();
                    }
                    Ok(None)
                } else if in_details {
                    self.focused_pane = FocusedPane::Services;
                    let service = self.hits.borrow().services.and_then(|(area, count)| {
                        let idx = row.checked_sub(area.y + 1)? as usize;
                        (contains(area, column, row) && idx < count).then_some(idx)
                    });
                    if let Some(idx) = service {
                        self.selected_service = idx;
                    }
                    Ok(None)
                } else {
                    Ok(None)
                }
            }
            _ => Ok(None),
        }
    }

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
            KeyCode::Char('p') => Ok(self.processes_action()),
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
            KeyCode::Char('w') => Ok(Some(self.workloads_action())),
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
            Span::raw("  "),
            // Mouse capture is on for click navigation, so the terminal's own
            // selection needs a modifier (Option in iTerm2, Shift elsewhere)
            Span::styled("[⌥/⇧+drag]", Style::default().fg(Color::Yellow)),
            Span::styled(" select text", Style::default().dim()),
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

        let loading_count = self.clusters.iter().filter(|c| c.loading).count();

        let (status_indicator, status_text) = if loading_count > 0 {
            (
                Span::styled(" ◌ ", Style::default().fg(Color::Yellow)),
                "Connecting",
            )
        } else if connected_count == total_count && total_count > 0 {
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
            let mut row_items: Vec<Option<NodeListItem>> = Vec::new();
            // Record which item the line just pushed belongs to
            let mut mark = |lines: &Vec<Line>, item: NodeListItem| {
                row_items.resize(lines.len() - 1, None);
                row_items.push(Some(item));
            };
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
                let (status_symbol, status_color) = if cluster.loading {
                    ("◌", Color::Yellow)
                } else if cluster.connected {
                    ("●", Color::Green)
                } else {
                    ("○", Color::Red)
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
                    Span::styled(
                        if cluster.loading {
                            " (…)".to_string()
                        } else {
                            format!(" ({})", node_count)
                        },
                        Style::default().dim(),
                    ),
                ];
                if !cluster.offline.is_empty() {
                    cluster_line.push(Span::styled(
                        format!(" {} offline", cluster.offline.len()),
                        Style::default().fg(Color::Red),
                    ));
                }
                cluster_line.extend(etcd_spans);
                lines.push(Line::from(cluster_line));
                mark(&lines, NodeListItem::ClusterHeader(cluster_idx));

                if cluster.loading {
                    lines.push(Line::from(Span::styled(
                        "    connecting…",
                        Style::default().fg(Color::Yellow).dim(),
                    )));
                }

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
                    mark(&lines, NodeListItem::ControlPlaneHeader(cluster_idx));

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

                            let (health_symbol, health_color, offline_note) =
                                self.node_row_status(cluster_idx, &v.node);

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
                                Span::styled(offline_note, Style::default().fg(Color::Red).dim()),
                            ]));
                            mark(&lines, NodeListItem::ControlPlaneNode(cluster_idx, idx));
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
                    mark(&lines, NodeListItem::WorkersHeader(cluster_idx));

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

                            let (health_symbol, health_color, offline_note) =
                                self.node_row_status(cluster_idx, &v.node);

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
                                Span::styled(offline_note, Style::default().fg(Color::Red).dim()),
                            ]));
                            mark(&lines, NodeListItem::WorkerNode(cluster_idx, idx));
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
            }

            frame.render_widget(Paragraph::new(lines), pane_layout[0]);
            let mut hits = self.hits.borrow_mut();
            hits.node_list = pane_layout[0];
            hits.node_rows = row_items;
        }
        self.hits.borrow_mut().menu = pane_layout[1];

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
            pct.round() as u8
        )
    }

    /// Draw the details pane (right column)
    /// Table of every node under the selected cluster / control plane /
    /// workers header: memory, load, cores, uptime, gateway, connectivity
    /// and service health side by side.
    fn draw_group_table(&self, frame: &mut Frame, area: Rect, border_color: Color) {
        let Some((cluster_idx, nodes)) = self.selected_nodes() else {
            return;
        };
        let group = match &self.selected_item {
            NodeListItem::ControlPlaneHeader(_) => "Control Plane".to_string(),
            NodeListItem::WorkersHeader(_) => "Workers".to_string(),
            _ => self
                .clusters
                .get(cluster_idx)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| "Cluster".to_string()),
        };
        let block = Block::default()
            .title(format!(" {} · {} nodes ", group, nodes.len()))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let dim = Style::default().dim();
        let pending = || Cell::from(Span::styled("…", dim));
        let now = unix_now_secs();
        // Drop the least important columns on narrow panes rather than truncate
        let wide = inner.width >= 90;
        let has_zfs = nodes
            .iter()
            .any(|n| !self.get_node_zfs_pools_for(cluster_idx, n).is_empty());

        // A version differing from the group's most common one is highlighted,
        // which makes a partly rolled-out upgrade stand out.
        let talos_common = most_common(
            nodes
                .iter()
                .filter_map(|n| self.get_node_talos_version_for(cluster_idx, n)),
        );
        let k8s_common = most_common(
            nodes
                .iter()
                .filter_map(|n| self.get_node_kubelet_version_for(cluster_idx, n)),
        );
        let version_cell = |version: Option<&str>, common: Option<&str>| match version {
            Some(v) if Some(v) == common => Cell::from(v.to_string()),
            Some(v) => Cell::from(Span::styled(
                v.to_string(),
                Style::default().fg(Color::Yellow),
            )),
            None => Cell::from(Span::styled("-", dim)),
        };

        let rows: Vec<Row> = nodes
            .iter()
            .map(|node| {
                let role = if self.node_is_controlplane(cluster_idx, node) {
                    "cp"
                } else {
                    "worker"
                };

                // Offline: nothing current to show; say so in the Uptime column
                if let Some(down) = self.node_offline_for(cluster_idx, node) {
                    let none = || Cell::from(Span::styled("-", dim));
                    let red = Style::default().fg(Color::Red);
                    let mut cells = vec![
                        Cell::from(Span::styled(node.clone(), red)),
                        Cell::from(Span::styled(role, dim)),
                        none(),
                        none(),
                        none(),
                        none(),
                    ];
                    if has_zfs {
                        cells.push(none());
                    }
                    cells.push(none());
                    if wide {
                        cells.push(none());
                    }
                    cells.extend([
                        Cell::from(Span::styled("offline", red)),
                        Cell::from(Span::styled("✗", red)),
                        none(),
                        Cell::from(Span::styled(
                            format!("down {}", format_uptime(down.as_secs())),
                            dim,
                        )),
                    ]);
                    return Row::new(cells);
                }

                let mem = match self.get_node_memory_for(cluster_idx, node) {
                    Some(m) => {
                        let pct = m.usage_percent();
                        let total_gb = m.mem_total as f64 / 1024.0 / 1024.0 / 1024.0;
                        Cell::from(Line::from(vec![
                            Span::styled(
                                format!("{:>3.0}%", pct),
                                Style::default().fg(memory_color(pct)),
                            ),
                            Span::styled(format!(" {:.0}G", total_gb), dim),
                        ]))
                    }
                    None => pending(),
                };

                let load = match self.get_node_load_avg_for(cluster_idx, node) {
                    Some(l) => Cell::from(Span::styled(
                        format!("{:.2}", l.load1),
                        Style::default().fg(load_color(l.load1)),
                    )),
                    None => pending(),
                };

                let cores = match self.get_node_cpu_info_for(cluster_idx, node) {
                    Some(c) => Cell::from(c.cpu_count.to_string()),
                    None => pending(),
                };

                let uptime = match self
                    .get_node_system_stat_for(cluster_idx, node)
                    .and_then(|s| s.uptime_secs(now))
                {
                    Some(secs) => Cell::from(format_uptime(secs)),
                    None => pending(),
                };

                let disk = match self
                    .get_node_mounts_for(cluster_idx, node)
                    .map(|m| m.var().and_then(|v| v.used_pct().map(|pct| (v, pct))))
                {
                    Some(Some((var, pct))) => Cell::from(Line::from(vec![
                        Span::styled(
                            format!("{:>3.0}%", pct),
                            Style::default().fg(memory_color(pct)),
                        ),
                        Span::styled(format!(" {:.0}G free", gib(var.available)), dim),
                    ])),
                    // Node answered but has no /var mount: nothing to show
                    Some(None) => Cell::from(Span::styled("-", dim)),
                    None => pending(),
                };

                // Fullest pool, so the column flags the one that needs attention
                let zfs = self
                    .get_node_zfs_pools_for(cluster_idx, node)
                    .iter()
                    .filter_map(|p| p.used_pct().map(|pct| (p, pct)))
                    .max_by(|a, b| a.1.total_cmp(&b.1));
                let zfs = match zfs {
                    Some((pool, pct)) => Cell::from(Line::from(vec![
                        Span::styled(
                            format!("{:>3.0}%", pct),
                            Style::default().fg(memory_color(pct)),
                        ),
                        Span::styled(format!(" {:.0}G free", gib(pool.free)), dim),
                    ])),
                    None => Cell::from(Span::styled("-", dim)),
                };

                let net = self.get_node_network_info_for(cluster_idx, node);
                let gw = match net {
                    Some(n) if n.gateways.is_empty() => Cell::from(Span::styled("-", dim)),
                    Some(n) => Cell::from(n.gateways.join(", ")),
                    None => pending(),
                };
                let conn = match net.and_then(|n| n.connectivity_ready) {
                    Some(true) => Cell::from(Span::styled("✓", Style::default().fg(Color::Green))),
                    Some(false) => Cell::from(Span::styled("✗", Style::default().fg(Color::Red))),
                    None => pending(),
                };

                let svc = match self.get_node_services_for(cluster_idx, node) {
                    Some(services) => {
                        let ok = services.iter().filter(|s| service_ok(s)).count();
                        let color = if ok == services.len() {
                            Color::Green
                        } else {
                            Color::Red
                        };
                        Cell::from(Span::styled(
                            format!("{}/{}", ok, services.len()),
                            Style::default().fg(color),
                        ))
                    }
                    None => pending(),
                };

                let mut cells = vec![
                    Cell::from(Span::styled(
                        node.clone(),
                        Style::default().fg(Color::White),
                    )),
                    Cell::from(Span::styled(role, dim)),
                    version_cell(
                        self.get_node_talos_version_for(cluster_idx, node),
                        talos_common,
                    ),
                    version_cell(
                        self.get_node_kubelet_version_for(cluster_idx, node),
                        k8s_common,
                    ),
                    mem,
                    disk,
                ];
                if has_zfs {
                    cells.push(zfs);
                }
                cells.push(load);
                if wide {
                    cells.push(cores);
                }
                cells.extend([uptime, conn, svc, gw]);
                Row::new(cells)
            })
            .collect();

        let mut header = vec!["Node", "Role", "Talos", "K8s", "Mem", "Disk"];
        let name_width = nodes.iter().map(|n| n.chars().count()).max().unwrap_or(0);
        let version_width = |versions: Vec<Option<&str>>| {
            versions
                .into_iter()
                .flatten()
                .map(|v| v.chars().count())
                .max()
                .unwrap_or(0)
                .max(5) as u16
        };
        let talos_width = version_width(
            nodes
                .iter()
                .map(|n| self.get_node_talos_version_for(cluster_idx, n))
                .collect(),
        );
        let k8s_width = version_width(
            nodes
                .iter()
                .map(|n| self.get_node_kubelet_version_for(cluster_idx, n))
                .collect(),
        );
        let mut widths = vec![
            Constraint::Length(name_width.max(4) as u16),
            Constraint::Length(6),
            Constraint::Length(talos_width),
            Constraint::Length(k8s_width),
            Constraint::Length(9),
            Constraint::Length(14),
        ];
        if has_zfs {
            header.push("ZFS");
            widths.push(Constraint::Length(15));
        }
        header.push("Load");
        widths.push(Constraint::Length(5));
        if wide {
            header.push("Cores");
            widths.push(Constraint::Length(5));
        }
        header.extend(["Uptime", "Conn", "Svc", "GW"]);
        widths.extend([
            Constraint::Length(8),
            Constraint::Length(4),
            Constraint::Length(5),
            Constraint::Fill(1),
        ]);

        let header = Row::new(header).style(Style::default().fg(Color::Gray).bold());
        let table = Table::new(rows, widths).header(header).column_spacing(2);

        let [table_area, services_area, hint_area] = Layout::vertical([
            Constraint::Length(nodes.len() as u16 + 2), // header + rows + spacer
            Constraint::Min(2),
            Constraint::Length(1),
        ])
        .areas(inner);
        frame.render_widget(table, table_area);
        self.draw_services_matrix(frame, services_area, cluster_idx, &nodes);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ●", Style::default().fg(Color::Green)),
                Span::styled(" healthy  ", dim),
                Span::styled("●", Style::default().fg(Color::DarkGray)),
                Span::styled(" no check  ", dim),
                Span::styled("✓", Style::default().fg(Color::Green)),
                Span::styled(" finished  ", dim),
                Span::styled("○", Style::default().fg(Color::Red)),
                Span::styled(" unhealthy  ", dim),
                Span::styled("✗", Style::default().fg(Color::Red)),
                Span::styled(" not running  ", dim),
                Span::styled("·", dim),
                Span::styled(" absent", dim),
            ])),
            hint_area,
        );
    }

    /// Service × node matrix: one row per service seen on any node in the
    /// group, one column per node. Rows with a problem on any node are listed
    /// first so they are visible even when the list is cut off.
    fn draw_services_matrix(
        &self,
        frame: &mut Frame,
        area: Rect,
        cluster_idx: usize,
        nodes: &[String],
    ) {
        let per_node: Vec<Option<&Vec<ServiceInfo>>> = nodes
            .iter()
            .map(|n| self.get_node_services_for(cluster_idx, n))
            .collect();
        let offline: Vec<bool> = nodes
            .iter()
            .map(|n| self.node_offline_for(cluster_idx, n).is_some())
            .collect();

        let mut ids: Vec<&str> = per_node
            .iter()
            .flatten()
            .flat_map(|services| services.iter().map(|s| s.id.as_str()))
            .collect();
        ids.sort_unstable();
        ids.dedup();

        let find = |services: Option<&Vec<ServiceInfo>>, id: &str| -> Option<ServiceInfo> {
            services?.iter().find(|s| s.id == id).cloned()
        };
        // A service is a problem if it is unhealthy anywhere it runs; being
        // absent from some nodes is normal (control-plane-only services).
        let has_problem = |id: &str| {
            per_node
                .iter()
                .any(|services| find(*services, id).is_some_and(|s| !service_ok(&s)))
        };
        let (mut problems, ok): (Vec<&str>, Vec<&str>) =
            ids.into_iter().partition(|id| has_problem(id));
        let problem_count = problems.len();
        problems.extend(ok);

        let dim = Style::default().dim();
        let rows: Vec<Row> = problems
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let name_style = if i < problem_count {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default().fg(Color::White)
                };
                let mut cells = vec![Cell::from(Span::styled(id.to_string(), name_style))];
                cells.extend(per_node.iter().zip(&offline).map(|(services, &offline)| {
                    let (symbol, style) = match services {
                        None if offline => ("-", dim),
                        None => ("…", dim),
                        Some(_) => match find(*services, id) {
                            None => ("·", dim),
                            Some(s) => service_symbol(&s),
                        },
                    };
                    Cell::from(Span::styled(symbol, style))
                }));
                Row::new(cells)
            })
            .collect();

        let name_width = problems.iter().map(|id| id.len()).max().unwrap_or(7).max(7);
        let mut widths = vec![Constraint::Length(name_width as u16)];
        widths.extend(
            nodes
                .iter()
                .map(|n| Constraint::Length(n.chars().count().max(1) as u16)),
        );
        let mut header = vec![Cell::from(format!(
            "Services ({} with issues)",
            problem_count
        ))];
        if problem_count == 0 {
            header[0] = Cell::from("Services");
        }
        header.extend(nodes.iter().map(|n| Cell::from(n.clone())));
        let header = Row::new(header).style(Style::default().fg(Color::Gray).bold());

        frame.render_widget(
            Table::new(rows, widths).header(header).column_spacing(2),
            area,
        );
    }

    fn draw_details_pane(&self, frame: &mut Frame, area: Rect) {
        {
            let mut hits = self.hits.borrow_mut();
            hits.details = area;
            hits.services = None;
        }
        // Focus indication - cyan border when focused
        let border_color = if self.focused_pane == FocusedPane::Services {
            Color::Cyan
        } else {
            Color::DarkGray
        };

        // Get active cluster data
        let cluster_idx = self.active_cluster;
        let cluster = self.clusters.get(cluster_idx);

        // Still connecting: say so instead of an empty or error pane
        if let Some(c) = cluster
            && c.loading
        {
            let block = Block::default()
                .title(format!(" {} ", c.name))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color));
            let mut lines = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  Connecting…",
                    Style::default().fg(Color::Yellow),
                )),
                Line::from(""),
            ];
            if !c.endpoints.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("  Endpoints: ", Style::default().dim()),
                    Span::raw(c.endpoints.join(", ")),
                ]));
            }
            lines.push(Line::from(Span::styled(
                "  Other clusters stay usable while this one connects.",
                Style::default().dim(),
            )));
            frame.render_widget(
                Paragraph::new(lines)
                    .block(block)
                    .wrap(ratatui::widgets::Wrap { trim: false }),
                area,
            );
            return;
        }

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
            let retry = if self.auto_refresh {
                format!(
                    "  Retrying every {}s — press r to retry now.",
                    talos_pilot_core::constants::refresh_intervals::SLOW
                )
            } else {
                "  Press r to retry.".to_string()
            };
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(retry, Style::default().dim())));

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
            // Header selected - show a table of every node in the group
            self.draw_group_table(frame, area, border_color);
            return;
        };

        let node_name = if node_name_str.is_empty() {
            "node-0".to_string()
        } else {
            node_name_str.clone()
        };
        let node_ip = self.node_ips().get(&node_name).cloned().unwrap_or_default();
        let role = if self.node_is_controlplane(cluster_idx, &node_name) {
            "controlplane"
        } else {
            "worker"
        };

        if let Some(down) = self.node_offline_for(cluster_idx, &node_name) {
            let block = Block::default()
                .title(format!(" {} · {} ", node_name, role))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color));
            let lines = vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled("  Status: ", Style::default().dim()),
                    Span::styled(
                        format!("Offline for {}", format_uptime(down.as_secs())),
                        Style::default().fg(Color::Red).bold(),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  IP:     ", Style::default().dim()),
                    Span::raw(node_ip),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "  The node is not answering the Talos API (rebooting, shut down or",
                    Style::default().dim(),
                )),
                Line::from(Span::styled(
                    "  unreachable). It is checked on every refresh and comes back by itself.",
                    Style::default().dim(),
                )),
            ];
            frame.render_widget(
                Paragraph::new(lines)
                    .block(block)
                    .wrap(ratatui::widgets::Wrap { trim: false }),
                area,
            );
            return;
        }

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

        // Resources section
        let mut resource_lines = vec![Line::from(vec![
            Span::styled(" IP: ", Style::default().dim()),
            Span::styled(&node_ip, Style::default().fg(Color::DarkGray)),
        ])];

        // Talos and Kubernetes (kubelet) versions
        let mut version_spans = Vec::new();
        if let Some(v) = self.get_node_talos_version_for(cluster_idx, &node_name) {
            version_spans.push(Span::styled(" Talos: ", Style::default().dim()));
            version_spans.push(Span::raw(v.to_string()));
        }
        if let Some(v) = self.get_node_kubelet_version_for(cluster_idx, &node_name) {
            version_spans.push(Span::styled("  K8s: ", Style::default().dim()));
            version_spans.push(Span::raw(v.to_string()));
        }
        if !version_spans.is_empty() {
            resource_lines.push(Line::from(version_spans));
        }

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

        // Uptime (computed at draw time so it keeps ticking between refreshes)
        if let Some(uptime) = self
            .get_node_system_stat(&node_name)
            .and_then(|s| s.uptime_secs(unix_now_secs()))
        {
            resource_lines.push(Line::from(vec![
                Span::styled(" Uptime: ", Style::default().dim()),
                Span::styled(format_uptime(uptime), Style::default().fg(Color::White)),
            ]));
        }

        // Host disks: /var (EPHEMERAL) first, then any user volumes
        if let Some(mounts) = self.get_node_mounts_for(self.active_cluster, &node_name) {
            for (i, disk) in mounts.host_disks().into_iter().enumerate() {
                let Some(pct) = disk.used_pct() else {
                    continue;
                };
                let label = if i == 0 { " Disk:   " } else { "         " };
                resource_lines.push(Line::from(vec![
                    Span::styled(label, Style::default().dim()),
                    Span::styled(
                        Self::render_compact_bar(pct, 10),
                        Style::default().fg(memory_color(pct)),
                    ),
                    Span::styled(
                        format!(
                            " {:.1}/{:.1}GB, {:.1}GB free ",
                            gib(disk.used()),
                            gib(disk.size),
                            gib(disk.available)
                        ),
                        Style::default().dim(),
                    ),
                    Span::styled(
                        disk.mounted_on.clone(),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
        }

        // ZFS pools
        for (i, pool) in self
            .get_node_zfs_pools_for(self.active_cluster, &node_name)
            .iter()
            .enumerate()
        {
            let Some(pct) = pool.used_pct() else {
                continue;
            };
            let label = if i == 0 { " ZFS:    " } else { "         " };
            resource_lines.push(Line::from(vec![
                Span::styled(label, Style::default().dim()),
                Span::styled(
                    Self::render_compact_bar(pct, 10),
                    Style::default().fg(memory_color(pct)),
                ),
                Span::styled(
                    format!(
                        " {:.1}/{:.1}GB, {:.1}GB free ",
                        gib(pool.used),
                        gib(pool.size()),
                        gib(pool.free)
                    ),
                    Style::default().dim(),
                ),
                Span::styled(pool.name.clone(), Style::default().fg(Color::DarkGray)),
            ]));
        }

        // Network summary (same fields as the Talos dashboard)
        if let Some(net) = self.get_node_network_info(&node_name) {
            let width = inner.width as usize;
            let value = Style::default().fg(Color::White);
            resource_lines.extend(wrap_list(" Addrs:  ", &net.addresses, width, value));
            resource_lines.extend(wrap_list(" GW:     ", &net.gateways, width, value));
            if let Some(ready) = net.connectivity_ready {
                let (text, color) = if ready {
                    ("✓ OK", Color::Green)
                } else {
                    ("✗ Not ready", Color::Red)
                };
                resource_lines.push(Line::from(vec![
                    Span::styled(" Conn:   ", Style::default().dim()),
                    Span::styled(text, Style::default().fg(color)),
                ]));
            }
            resource_lines.extend(wrap_list(" DNS:    ", &net.dns_servers, width, value));
        }

        // Split into resources (sized to its lines plus a spacer) and services
        let panel_layout = Layout::vertical([
            Constraint::Length(resource_lines.len() as u16 + 1), // Resources
            Constraint::Min(4),                                  // Services
        ])
        .split(inner);

        frame.render_widget(Paragraph::new(resource_lines), panel_layout[0]);

        // Services section
        if let Some(services) = self.get_node_services(&node_name) {
            let running = services.iter().filter(|s| s.state == "Running").count();
            let mut svc_lines = vec![Line::from(vec![Span::styled(
                format!(" Services ({}/{})", running, services.len()),
                Style::default().fg(Color::Gray),
            )])];

            for (i, svc) in services.iter().enumerate() {
                let (health_symbol, health_color) = match svc.is_healthy() {
                    Some(true) => ("●", Color::Green),
                    Some(false) => ("○", Color::Red),
                    // No health check defined - not a failure
                    None => ("●", Color::DarkGray),
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
            self.hits.borrow_mut().services = Some((panel_layout[1], services.len()));
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
    fn offline_nodes_keep_their_group_and_status() {
        let mut comp = ClusterComponent::default();
        let offline = |controlplane| OfflineNode {
            since: std::time::Instant::now(),
            controlplane,
        };
        comp.clusters.push(ClusterData {
            name: "test".to_string(),
            versions: vec![ver("cp1"), ver("cp2"), ver("cp3"), ver("w1"), ver("w2")],
            services: vec![services_with("cp1", &["etcd"])],
            // cp2 left discovery while rebooting but is still an etcd member
            etcd_members: vec![EtcdMemberInfo {
                id: 2,
                hostname: "cp2".to_string(),
                peer_urls: vec!["https://10.0.0.2:2380".to_string()],
                client_urls: Vec::new(),
                is_learner: false,
            }],
            offline: HashMap::from([
                ("cp2".to_string(), offline(false)),
                ("cp3".to_string(), offline(true)),
                ("w2".to_string(), offline(false)),
            ]),
            ..Default::default()
        });

        assert_eq!(
            names(&comp.controlplane_nodes_for(0)),
            vec!["cp1", "cp2", "cp3"]
        );
        assert_eq!(names(&comp.worker_nodes_for(0)), vec!["w1", "w2"]);

        let (symbol, color, note) = comp.node_row_status(0, "cp3");
        assert_eq!((symbol, color), ("○", Color::Red));
        assert!(note.contains("offline"), "{note}");
        assert_eq!(comp.node_row_status(0, "w1").0, "●");
    }

    #[test]
    fn most_common_prefers_majority_then_first_seen() {
        let v = ["v1.11.2", "v1.11.3", "v1.11.2"];
        assert_eq!(most_common(v.into_iter()), Some("v1.11.2"));
        assert_eq!(most_common(["a", "b"].into_iter()), Some("a"));
        assert_eq!(most_common(std::iter::empty()), None);
    }

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

    #[test]
    fn wrap_list_breaks_between_items() {
        let text = |lines: Vec<Line>| -> Vec<String> {
            lines
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect()
        };
        let items: Vec<String> = ["100.64.0.8/32", "65.21.17.62/26", "fd7a:115c:a1e0::8/128"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        assert_eq!(
            text(wrap_list(" IP: ", &items, 200, Style::default())),
            vec![" IP: 100.64.0.8/32, 65.21.17.62/26, fd7a:115c:a1e0::8/128"]
        );
        assert_eq!(
            text(wrap_list(" IP: ", &items, 40, Style::default())),
            vec![
                " IP: 100.64.0.8/32, 65.21.17.62/26,",
                "     fd7a:115c:a1e0::8/128"
            ]
        );
        assert!(wrap_list(" IP: ", &[], 40, Style::default()).is_empty());
    }

    #[test]
    fn finished_one_shot_service_is_ok() {
        let svc = |state: &str, health: Option<(bool, bool)>| ServiceInfo {
            id: "svc".to_string(),
            state: state.to_string(),
            health: health.map(|(unknown, healthy)| talos_rs::ServiceHealth {
                unknown,
                healthy,
                last_message: String::new(),
            }),
        };
        assert!(service_ok(&svc("Finished", None)));
        assert!(service_ok(&svc("Running", Some((true, false)))));
        assert!(service_ok(&svc("Running", Some((false, true)))));
        assert!(!service_ok(&svc("Running", Some((false, false)))));
        assert!(!service_ok(&svc("Failed", None)));
    }
}
