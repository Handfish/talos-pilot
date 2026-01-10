//! High-level Talos API client
//!
//! Provides a convenient interface for interacting with Talos clusters.

use crate::auth::create_channel;
use crate::config::{Context, TalosConfig};
use crate::error::TalosError;
use crate::proto::machine::machine_service_client::MachineServiceClient;
use crate::proto::machine::{EtcdMemberListRequest, LogsRequest, NetstatRequest, netstat_request};
use tokio_stream::StreamExt;
use tonic::transport::Channel;
use tonic::Request;

/// High-level client for Talos API
#[derive(Clone)]
pub struct TalosClient {
    channel: Channel,
    /// Target nodes for API requests
    nodes: Vec<String>,
}

impl TalosClient {
    /// Create a new client from a talosconfig context
    pub async fn from_context(ctx: &Context) -> Result<Self, TalosError> {
        let channel = create_channel(ctx).await?;
        let nodes = ctx.target_nodes().to_vec();

        Ok(Self { channel, nodes })
    }

    /// Create a new client from the default talosconfig
    pub async fn from_default_config() -> Result<Self, TalosError> {
        let config = TalosConfig::load_default()?;
        let ctx = config
            .current_context()
            .ok_or_else(|| TalosError::ConfigInvalid("No current context".to_string()))?;
        Self::from_context(ctx).await
    }

    /// Create a new client from a named context in the default talosconfig
    pub async fn from_named_context(context_name: &str) -> Result<Self, TalosError> {
        let config = TalosConfig::load_default()?;
        let ctx = config.get_context(context_name)?;
        Self::from_context(ctx).await
    }

    /// Create a new client targeting a specific node
    ///
    /// This returns a clone of the client with requests directed to the specified node.
    pub fn with_node(&self, node: &str) -> Self {
        Self {
            channel: self.channel.clone(),
            nodes: vec![node.to_string()],
        }
    }

    /// Get a MachineService client
    fn machine_client(&self) -> MachineServiceClient<Channel> {
        MachineServiceClient::new(self.channel.clone())
    }

    /// Add node targeting metadata to a request
    /// If no explicit nodes are configured, don't add the header
    /// (Talos will respond from the endpoint node itself)
    fn with_nodes<T>(&self, mut request: Request<T>) -> Request<T> {
        // Only add nodes metadata if explicitly configured (not just endpoints)
        // When nodes is empty or same as endpoints, skip the header
        if !self.nodes.is_empty() {
            // Filter out localhost/127.0.0.1 entries as these are endpoint proxies
            let valid_nodes: Vec<String> = self.nodes.iter()
                .filter(|n| !n.starts_with("127.0.0.1") && !n.starts_with("localhost"))
                .map(|n| n.split(':').next().unwrap_or(n).to_string())
                .collect();

            if !valid_nodes.is_empty() {
                let nodes_str = valid_nodes.join(",");
                if let Ok(value) = nodes_str.parse() {
                    request.metadata_mut().insert("nodes", value);
                }
            }
        }
        request
    }

    /// Get version information from all configured nodes
    pub async fn version(&self) -> Result<Vec<VersionInfo>, TalosError> {
        let mut client = self.machine_client();
        let request = self.with_nodes(Request::new(()));

        let response = client.version(request).await?;
        let inner = response.into_inner();

        let versions: Vec<VersionInfo> = inner
            .messages
            .into_iter()
            .map(|msg| VersionInfo {
                node: msg.metadata.as_ref().map(|m| m.hostname.clone()).unwrap_or_default(),
                version: msg.version.as_ref().map(|v| v.tag.clone()).unwrap_or_default(),
                sha: msg.version.as_ref().map(|v| v.sha.clone()).unwrap_or_default(),
                built: msg.version.as_ref().map(|v| v.built.clone()).unwrap_or_default(),
                go_version: msg.version.as_ref().map(|v| v.go_version.clone()).unwrap_or_default(),
                os: msg.version.as_ref().map(|v| v.os.clone()).unwrap_or_default(),
                arch: msg.version.as_ref().map(|v| v.arch.clone()).unwrap_or_default(),
            })
            .collect();

        Ok(versions)
    }

    /// Get list of services from all configured nodes
    pub async fn services(&self) -> Result<Vec<NodeServices>, TalosError> {
        let mut client = self.machine_client();
        let request = self.with_nodes(Request::new(()));

        let response = client.service_list(request).await?;
        let inner = response.into_inner();

        let services: Vec<NodeServices> = inner
            .messages
            .into_iter()
            .map(|msg| NodeServices {
                node: msg.metadata.as_ref().map(|m| m.hostname.clone()).unwrap_or_default(),
                services: msg
                    .services
                    .into_iter()
                    .map(|svc| ServiceInfo {
                        id: svc.id,
                        state: svc.state,
                        health: svc.health.map(|h| ServiceHealth {
                            unknown: h.unknown,
                            healthy: h.healthy,
                            last_message: h.last_message,
                        }),
                    })
                    .collect(),
            })
            .collect();

        Ok(services)
    }

    /// Get memory information from all configured nodes
    pub async fn memory(&self) -> Result<Vec<NodeMemory>, TalosError> {
        let mut client = self.machine_client();
        let request = self.with_nodes(Request::new(()));

        let response = client.memory(request).await?;
        let inner = response.into_inner();

        let memories: Vec<NodeMemory> = inner
            .messages
            .into_iter()
            .map(|msg| NodeMemory {
                node: msg.metadata.as_ref().map(|m| m.hostname.clone()).unwrap_or_default(),
                // Note: /proc/meminfo values are in KB, convert to bytes
                meminfo: msg.meminfo.map(|m| MemInfo {
                    mem_total: m.memtotal * 1024,
                    mem_free: m.memfree * 1024,
                    mem_available: m.memavailable * 1024,
                    buffers: m.buffers * 1024,
                    cached: m.cached * 1024,
                }),
            })
            .collect();

        Ok(memories)
    }

    /// Get load average from all configured nodes
    pub async fn load_avg(&self) -> Result<Vec<NodeLoadAvg>, TalosError> {
        let mut client = self.machine_client();
        let request = self.with_nodes(Request::new(()));

        let response = client.load_avg(request).await?;
        let inner = response.into_inner();

        let loads: Vec<NodeLoadAvg> = inner
            .messages
            .into_iter()
            .map(|msg| NodeLoadAvg {
                node: msg.metadata.as_ref().map(|m| m.hostname.clone()).unwrap_or_default(),
                load1: msg.load1,
                load5: msg.load5,
                load15: msg.load15,
            })
            .collect();

        Ok(loads)
    }

    /// Get CPU information from all configured nodes
    pub async fn cpu_info(&self) -> Result<Vec<NodeCpuInfo>, TalosError> {
        let mut client = self.machine_client();
        let request = self.with_nodes(Request::new(()));

        let response = client.cpu_info(request).await?;
        let inner = response.into_inner();

        let cpus: Vec<NodeCpuInfo> = inner
            .messages
            .into_iter()
            .map(|msg| {
                let cpu_count = msg.cpu_info.len();
                let model_name = msg.cpu_info.first().map(|c| c.model_name.clone()).unwrap_or_default();
                let mhz = msg.cpu_info.first().map(|c| c.cpu_mhz).unwrap_or_default();

                NodeCpuInfo {
                    node: msg.metadata.as_ref().map(|m| m.hostname.clone()).unwrap_or_default(),
                    cpu_count,
                    model_name,
                    mhz,
                }
            })
            .collect();

        Ok(cpus)
    }

    /// Get system statistics (CPU usage, process counts) from all configured nodes
    pub async fn system_stat(&self) -> Result<Vec<NodeSystemStat>, TalosError> {
        let mut client = self.machine_client();
        let request = self.with_nodes(Request::new(()));

        let response = client.system_stat(request).await?;
        let inner = response.into_inner();

        let stats: Vec<NodeSystemStat> = inner
            .messages
            .into_iter()
            .map(|msg| {
                let cpu_total = msg.cpu_total.map(|c| CpuStat {
                    user: c.user,
                    nice: c.nice,
                    system: c.system,
                    idle: c.idle,
                    iowait: c.iowait,
                    irq: c.irq,
                    soft_irq: c.soft_irq,
                    steal: c.steal,
                }).unwrap_or_default();

                NodeSystemStat {
                    node: msg.metadata.as_ref().map(|m| m.hostname.clone()).unwrap_or_default(),
                    cpu_total,
                    process_running: msg.process_running,
                    process_blocked: msg.process_blocked,
                }
            })
            .collect();

        Ok(stats)
    }

    /// Get logs for a service (non-streaming, returns last N lines)
    pub async fn logs(&self, service_id: &str, tail_lines: i32) -> Result<String, TalosError> {
        let mut client = self.machine_client();

        let request = self.with_nodes(Request::new(LogsRequest {
            namespace: "system".to_string(),
            id: service_id.to_string(),
            driver: 0, // CONTAINERD
            follow: false,
            tail_lines,
        }));

        let response = client.logs(request).await?;
        let mut stream = response.into_inner();

        let mut logs = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(data) => {
                    if let Ok(text) = String::from_utf8(data.bytes) {
                        logs.push_str(&text);
                    }
                }
                Err(e) => {
                    // Stop on error but return what we have
                    tracing::warn!("Log stream error: {}", e);
                    break;
                }
            }
        }

        Ok(logs)
    }

    /// Stream logs from a service (follow mode)
    /// Returns a receiver that yields log lines as they arrive
    pub async fn logs_stream(
        &self,
        service_id: &str,
        tail_lines: i32,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<String>, TalosError> {
        let mut client = self.machine_client();

        let request = self.with_nodes(Request::new(LogsRequest {
            namespace: "system".to_string(),
            id: service_id.to_string(),
            driver: 0, // CONTAINERD
            follow: true, // Enable streaming
            tail_lines,
        }));

        let response = client.logs(request).await?;
        let mut stream = response.into_inner();

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        // Spawn a task to read from the stream and send to channel
        tokio::spawn(async move {
            // Buffer for incomplete lines that span chunk boundaries
            let mut pending = String::new();

            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(data) => {
                        if let Ok(text) = String::from_utf8(data.bytes) {
                            // Prepend any pending partial line from previous chunk
                            let combined = if pending.is_empty() {
                                text
                            } else {
                                std::mem::take(&mut pending) + &text
                            };

                            // Check if chunk ends with newline (complete line) or not (partial)
                            let ends_with_newline = combined.ends_with('\n');

                            // Split into lines
                            let mut lines: Vec<&str> = combined.lines().collect();

                            // If doesn't end with newline, last "line" is incomplete - save it
                            if !ends_with_newline && !lines.is_empty() {
                                pending = lines.pop().unwrap_or("").to_string();
                            }

                            // Send complete lines
                            for line in lines {
                                if !line.trim().is_empty() && tx.send(line.to_string()).is_err() {
                                    // Receiver dropped, stop streaming
                                    return;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Log stream error: {}", e);
                        break;
                    }
                }
            }

            // Send any remaining pending content when stream ends
            if !pending.trim().is_empty() {
                let _ = tx.send(pending);
            }
        });

        Ok(rx)
    }

    /// Get logs for multiple services in parallel
    /// Returns Vec of (service_id, log_content) tuples
    pub async fn logs_multi(
        &self,
        service_ids: &[&str],
        tail_lines: i32,
    ) -> Result<Vec<(String, String)>, TalosError> {
        use futures::future::join_all;

        let futures: Vec<_> = service_ids
            .iter()
            .map(|&service_id| {
                let service_id = service_id.to_string();
                async move {
                    let result = self.logs(&service_id, tail_lines).await;
                    (service_id, result)
                }
            })
            .collect();

        let results = join_all(futures).await;

        let mut logs = Vec::new();
        for (service_id, result) in results {
            match result {
                Ok(content) => {
                    logs.push((service_id, content));
                }
                Err(e) => {
                    tracing::warn!("Failed to fetch logs for {}: {}", service_id, e);
                    // Continue with other services, just skip this one
                }
            }
        }

        Ok(logs)
    }

    // ==================== Etcd APIs ====================

    /// Get etcd member list from control plane nodes
    pub async fn etcd_members(&self) -> Result<Vec<EtcdMemberInfo>, TalosError> {
        let mut client = self.machine_client();
        let request = self.with_nodes(Request::new(EtcdMemberListRequest {
            query_local: false,
        }));

        let response = client.etcd_member_list(request).await?;
        let inner = response.into_inner();

        let mut members = Vec::new();
        for msg in inner.messages {
            for member in msg.members {
                members.push(EtcdMemberInfo {
                    id: member.id,
                    hostname: member.hostname,
                    peer_urls: member.peer_urls,
                    client_urls: member.client_urls,
                    is_learner: member.is_learner,
                });
            }
        }

        Ok(members)
    }

    /// Get etcd status from control plane nodes
    /// Returns status for each etcd member that responds
    pub async fn etcd_status(&self) -> Result<Vec<EtcdMemberStatus>, TalosError> {
        let mut client = self.machine_client();
        let request = self.with_nodes(Request::new(()));

        let response = client.etcd_status(request).await?;
        let inner = response.into_inner();

        let statuses: Vec<EtcdMemberStatus> = inner
            .messages
            .into_iter()
            .filter_map(|msg| {
                msg.member_status.map(|status| EtcdMemberStatus {
                    node: msg.metadata.as_ref().map(|m| m.hostname.clone()).unwrap_or_default(),
                    member_id: status.member_id,
                    protocol_version: status.protocol_version,
                    db_size: status.db_size,
                    db_size_in_use: status.db_size_in_use,
                    leader_id: status.leader,
                    raft_index: status.raft_index,
                    raft_term: status.raft_term,
                    raft_applied_index: status.raft_applied_index,
                    errors: status.errors,
                    is_learner: status.is_learner,
                })
            })
            .collect();

        Ok(statuses)
    }

    /// Get etcd alarms from control plane nodes
    pub async fn etcd_alarms(&self) -> Result<Vec<EtcdAlarm>, TalosError> {
        let mut client = self.machine_client();
        let request = self.with_nodes(Request::new(()));

        let response = client.etcd_alarm_list(request).await?;
        let inner = response.into_inner();

        let mut alarms = Vec::new();
        for msg in inner.messages {
            let node = msg.metadata.as_ref().map(|m| m.hostname.clone()).unwrap_or_default();
            for member_alarm in msg.member_alarms {
                // Only include non-NONE alarms
                if member_alarm.alarm != 0 {
                    alarms.push(EtcdAlarm {
                        node: node.clone(),
                        member_id: member_alarm.member_id,
                        alarm_type: EtcdAlarmType::from_i32(member_alarm.alarm),
                    });
                }
            }
        }

        Ok(alarms)
    }

    /// Get processes from all configured nodes
    pub async fn processes(&self) -> Result<Vec<NodeProcesses>, TalosError> {
        let mut client = self.machine_client();
        let request = self.with_nodes(Request::new(()));

        let response = client.processes(request).await?;
        let inner = response.into_inner();

        let mut result = Vec::new();
        for msg in inner.messages {
            let hostname = msg.metadata.as_ref().map(|m| m.hostname.clone()).unwrap_or_default();

            let processes: Vec<ProcessInfo> = msg
                .processes
                .into_iter()
                .map(|p| ProcessInfo {
                    pid: p.pid,
                    ppid: p.ppid,
                    state: ProcessState::from_str(&p.state),
                    threads: p.threads,
                    cpu_time: p.cpu_time,
                    virtual_memory: p.virtual_memory,
                    resident_memory: p.resident_memory,
                    command: p.command,
                    executable: p.executable,
                    args: p.args,
                })
                .collect();

            result.push(NodeProcesses { hostname, processes });
        }

        Ok(result)
    }

    /// Get network device statistics from all configured nodes
    pub async fn network_device_stats(&self) -> Result<Vec<NodeNetworkStats>, TalosError> {
        let mut client = self.machine_client();
        let request = self.with_nodes(Request::new(()));

        let response = client.network_device_stats(request).await?;
        let inner = response.into_inner();

        let mut result = Vec::new();
        for msg in inner.messages {
            let hostname = msg.metadata.as_ref().map(|m| m.hostname.clone()).unwrap_or_default();

            let total = msg.total.map(|t| NetDevStats::from_proto(&t));

            let devices: Vec<NetDevStats> = msg
                .devices
                .iter()
                .map(NetDevStats::from_proto)
                .collect();

            result.push(NodeNetworkStats {
                hostname,
                total,
                devices,
            });
        }

        Ok(result)
    }

    /// Get network connections (netstat) from all configured nodes
    ///
    /// Returns TCP connections by default. Use filter to get only listening or connected.
    pub async fn netstat(&self, filter: NetstatFilter) -> Result<Vec<NodeConnections>, TalosError> {
        let mut client = self.machine_client();

        // Must explicitly enable TCP protocols and host network
        let request = self.with_nodes(Request::new(NetstatRequest {
            filter: filter.to_proto(),
            feature: Some(netstat_request::Feature { pid: false }),
            l4proto: Some(netstat_request::L4proto {
                tcp: true,
                tcp6: true,
                udp: false,
                udp6: false,
                udplite: false,
                udplite6: false,
                raw: false,
                raw6: false,
            }),
            netns: Some(netstat_request::NetNs {
                hostnetwork: true,
                netns: vec![],
                allnetns: false,
            }),
        }));

        let response = client.netstat(request).await?;
        let inner = response.into_inner();

        let mut result = Vec::new();
        for msg in inner.messages {
            let hostname = msg.metadata.as_ref().map(|m| m.hostname.clone()).unwrap_or_default();

            let connections: Vec<ConnectionInfo> = msg
                .connectrecord
                .into_iter()
                .map(ConnectionInfo::from_proto)
                .collect();

            result.push(NodeConnections {
                hostname,
                connections,
            });
        }

        Ok(result)
    }
}

/// Version information for a node
#[derive(Debug, Clone)]
pub struct VersionInfo {
    pub node: String,
    pub version: String,
    pub sha: String,
    pub built: String,
    pub go_version: String,
    pub os: String,
    pub arch: String,
}

/// Services running on a node
#[derive(Debug, Clone)]
pub struct NodeServices {
    pub node: String,
    pub services: Vec<ServiceInfo>,
}

/// Information about a single service
#[derive(Debug, Clone)]
pub struct ServiceInfo {
    pub id: String,
    pub state: String,
    pub health: Option<ServiceHealth>,
}

/// Health status of a service
#[derive(Debug, Clone)]
pub struct ServiceHealth {
    pub unknown: bool,
    pub healthy: bool,
    pub last_message: String,
}

/// Memory information for a node
#[derive(Debug, Clone)]
pub struct NodeMemory {
    pub node: String,
    pub meminfo: Option<MemInfo>,
}

/// Memory statistics
#[derive(Debug, Clone)]
pub struct MemInfo {
    pub mem_total: u64,
    pub mem_free: u64,
    pub mem_available: u64,
    pub buffers: u64,
    pub cached: u64,
}

impl MemInfo {
    /// Calculate memory usage percentage
    pub fn usage_percent(&self) -> f32 {
        if self.mem_total == 0 {
            return 0.0;
        }
        let used = self.mem_total - self.mem_available;
        (used as f32 / self.mem_total as f32) * 100.0
    }
}

/// Load average for a node
#[derive(Debug, Clone)]
pub struct NodeLoadAvg {
    pub node: String,
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
}

/// CPU information for a node
#[derive(Debug, Clone)]
pub struct NodeCpuInfo {
    pub node: String,
    pub cpu_count: usize,
    pub model_name: String,
    pub mhz: f64,
}

/// System statistics for a node
#[derive(Debug, Clone)]
pub struct NodeSystemStat {
    pub node: String,
    pub cpu_total: CpuStat,
    pub process_running: u64,
    pub process_blocked: u64,
}

/// CPU statistics (cumulative time values)
#[derive(Debug, Clone, Default)]
pub struct CpuStat {
    pub user: f64,
    pub nice: f64,
    pub system: f64,
    pub idle: f64,
    pub iowait: f64,
    pub irq: f64,
    pub soft_irq: f64,
    pub steal: f64,
}

impl CpuStat {
    /// Calculate total CPU time (all fields)
    pub fn total(&self) -> f64 {
        self.user + self.nice + self.system + self.idle + self.iowait + self.irq + self.soft_irq + self.steal
    }

    /// Calculate busy time (non-idle)
    pub fn busy(&self) -> f64 {
        self.user + self.nice + self.system + self.irq + self.soft_irq + self.steal
    }

    /// Calculate CPU usage percentage from delta between two measurements
    pub fn usage_percent_from(prev: &CpuStat, curr: &CpuStat) -> f32 {
        let delta_total = curr.total() - prev.total();
        if delta_total <= 0.0 {
            return 0.0;
        }
        let delta_busy = curr.busy() - prev.busy();
        ((delta_busy / delta_total) * 100.0) as f32
    }
}

// ==================== Etcd Types ====================

/// Etcd member information (from member list)
#[derive(Debug, Clone)]
pub struct EtcdMemberInfo {
    pub id: u64,
    pub hostname: String,
    pub peer_urls: Vec<String>,
    pub client_urls: Vec<String>,
    pub is_learner: bool,
}

impl EtcdMemberInfo {
    /// Extract IP address from peer_urls
    /// e.g., "https://10.5.0.2:2380" -> "10.5.0.2"
    pub fn ip_address(&self) -> Option<String> {
        self.peer_urls.first().and_then(|url| {
            // Parse URL like "https://10.5.0.2:2380"
            url.split("://")
                .nth(1)
                .and_then(|host_port| host_port.split(':').next())
                .map(|s| s.to_string())
        })
    }
}

/// Etcd member status (from status call)
#[derive(Debug, Clone)]
pub struct EtcdMemberStatus {
    /// Node hostname that reported this status
    pub node: String,
    /// Member ID
    pub member_id: u64,
    /// Protocol version (e.g., "3.5")
    pub protocol_version: String,
    /// Total database size in bytes
    pub db_size: i64,
    /// Database size in use in bytes
    pub db_size_in_use: i64,
    /// Current leader's member ID
    pub leader_id: u64,
    /// Raft index
    pub raft_index: u64,
    /// Raft term
    pub raft_term: u64,
    /// Raft applied index
    pub raft_applied_index: u64,
    /// Any errors reported
    pub errors: Vec<String>,
    /// Whether this member is a learner
    pub is_learner: bool,
}

impl EtcdMemberStatus {
    /// Check if this member is the leader
    pub fn is_leader(&self) -> bool {
        self.member_id == self.leader_id && self.leader_id != 0
    }

    /// Get DB size in human-readable format
    pub fn db_size_human(&self) -> String {
        format_bytes(self.db_size as u64)
    }

    /// Get DB size in use in human-readable format
    pub fn db_size_in_use_human(&self) -> String {
        format_bytes(self.db_size_in_use as u64)
    }

    /// Get DB usage percentage
    pub fn db_usage_percent(&self) -> f32 {
        if self.db_size == 0 {
            return 0.0;
        }
        (self.db_size_in_use as f32 / self.db_size as f32) * 100.0
    }
}

/// Etcd alarm
#[derive(Debug, Clone)]
pub struct EtcdAlarm {
    /// Node that reported this alarm
    pub node: String,
    /// Member ID with the alarm
    pub member_id: u64,
    /// Type of alarm
    pub alarm_type: EtcdAlarmType,
}

/// Type of etcd alarm
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EtcdAlarmType {
    /// No alarm (should not appear in results)
    None,
    /// Database has run out of space
    NoSpace,
    /// Database corruption detected
    Corrupt,
    /// Unknown alarm type
    Unknown(i32),
}

impl EtcdAlarmType {
    pub fn from_i32(value: i32) -> Self {
        match value {
            0 => EtcdAlarmType::None,
            1 => EtcdAlarmType::NoSpace,
            2 => EtcdAlarmType::Corrupt,
            v => EtcdAlarmType::Unknown(v),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            EtcdAlarmType::None => "NONE",
            EtcdAlarmType::NoSpace => "NOSPACE",
            EtcdAlarmType::Corrupt => "CORRUPT",
            EtcdAlarmType::Unknown(_) => "UNKNOWN",
        }
    }
}

/// Format bytes into human-readable string (KB, MB, GB)
fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

// ==================== Process Types ====================

/// Processes running on a node
#[derive(Debug, Clone)]
pub struct NodeProcesses {
    /// Node hostname
    pub hostname: String,
    /// Processes on this node
    pub processes: Vec<ProcessInfo>,
}

/// Information about a single process
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    /// Process ID
    pub pid: i32,
    /// Parent process ID
    pub ppid: i32,
    /// Process state
    pub state: ProcessState,
    /// Number of threads
    pub threads: i32,
    /// Cumulative CPU time in seconds
    pub cpu_time: f64,
    /// Virtual memory size in bytes
    pub virtual_memory: u64,
    /// Resident memory size in bytes
    pub resident_memory: u64,
    /// Short command name
    pub command: String,
    /// Full path to executable
    pub executable: String,
    /// Full command line arguments
    pub args: String,
}

impl ProcessInfo {
    /// Get resident memory in human-readable format
    pub fn resident_memory_human(&self) -> String {
        format_bytes(self.resident_memory)
    }

    /// Get virtual memory in human-readable format
    pub fn virtual_memory_human(&self) -> String {
        format_bytes(self.virtual_memory)
    }

    /// Get CPU time in human-readable format (e.g., "847.2s" or "2h 14m")
    pub fn cpu_time_human(&self) -> String {
        if self.cpu_time < 60.0 {
            format!("{:.1}s", self.cpu_time)
        } else if self.cpu_time < 3600.0 {
            let mins = (self.cpu_time / 60.0) as u32;
            let secs = (self.cpu_time % 60.0) as u32;
            format!("{}m {}s", mins, secs)
        } else {
            let hours = (self.cpu_time / 3600.0) as u32;
            let mins = ((self.cpu_time % 3600.0) / 60.0) as u32;
            format!("{}h {}m", hours, mins)
        }
    }

    /// Get the display command - args if available, otherwise command name
    pub fn display_command(&self) -> &str {
        if !self.args.is_empty() {
            &self.args
        } else if !self.executable.is_empty() {
            &self.executable
        } else {
            &self.command
        }
    }
}

/// Process state (Linux process states)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessState {
    /// R - Running or runnable
    Running,
    /// S - Interruptible sleep
    Sleeping,
    /// D - Uninterruptible sleep (usually I/O)
    DiskSleep,
    /// Z - Zombie (terminated but not reaped by parent)
    Zombie,
    /// T - Stopped (by signal or debugger)
    Stopped,
    /// t - Tracing stop
    TracingStop,
    /// X - Dead (should never be seen)
    Dead,
    /// Unknown state
    Unknown(String),
}

impl ProcessState {
    /// Parse state from string (e.g., "R", "S", "D", "Z")
    pub fn from_str(s: &str) -> Self {
        match s.chars().next() {
            Some('R') => ProcessState::Running,
            Some('S') => ProcessState::Sleeping,
            Some('D') => ProcessState::DiskSleep,
            Some('Z') => ProcessState::Zombie,
            Some('T') => ProcessState::Stopped,
            Some('t') => ProcessState::TracingStop,
            Some('X') => ProcessState::Dead,
            _ => ProcessState::Unknown(s.to_string()),
        }
    }

    /// Get single-character representation
    pub fn short(&self) -> &str {
        match self {
            ProcessState::Running => "R",
            ProcessState::Sleeping => "S",
            ProcessState::DiskSleep => "D",
            ProcessState::Zombie => "Z",
            ProcessState::Stopped => "T",
            ProcessState::TracingStop => "t",
            ProcessState::Dead => "X",
            ProcessState::Unknown(s) => s.get(0..1).unwrap_or("?"),
        }
    }

    /// Get human-readable description
    pub fn description(&self) -> &str {
        match self {
            ProcessState::Running => "Running",
            ProcessState::Sleeping => "Sleeping",
            ProcessState::DiskSleep => "Disk Sleep",
            ProcessState::Zombie => "Zombie",
            ProcessState::Stopped => "Stopped",
            ProcessState::TracingStop => "Tracing",
            ProcessState::Dead => "Dead",
            ProcessState::Unknown(_) => "Unknown",
        }
    }

    /// Check if this is a problematic state (zombie, disk wait)
    pub fn is_problematic(&self) -> bool {
        matches!(self, ProcessState::Zombie | ProcessState::DiskSleep)
    }
}

// ==================== Network Types ====================

/// Network device statistics for a node
#[derive(Debug, Clone)]
pub struct NodeNetworkStats {
    /// Node hostname
    pub hostname: String,
    /// Aggregate totals across all devices
    pub total: Option<NetDevStats>,
    /// Per-device statistics
    pub devices: Vec<NetDevStats>,
}

/// Statistics for a single network device
#[derive(Debug, Clone)]
pub struct NetDevStats {
    /// Device name (e.g., "eth0", "cni0")
    pub name: String,
    /// Bytes received
    pub rx_bytes: u64,
    /// Packets received
    pub rx_packets: u64,
    /// Receive errors
    pub rx_errors: u64,
    /// Receive dropped
    pub rx_dropped: u64,
    /// Bytes transmitted
    pub tx_bytes: u64,
    /// Packets transmitted
    pub tx_packets: u64,
    /// Transmit errors
    pub tx_errors: u64,
    /// Transmit dropped
    pub tx_dropped: u64,
}

impl NetDevStats {
    /// Create from protobuf NetDev
    fn from_proto(dev: &crate::proto::machine::NetDev) -> Self {
        Self {
            name: dev.name.clone(),
            rx_bytes: dev.rx_bytes,
            rx_packets: dev.rx_packets,
            rx_errors: dev.rx_errors,
            rx_dropped: dev.rx_dropped,
            tx_bytes: dev.tx_bytes,
            tx_packets: dev.tx_packets,
            tx_errors: dev.tx_errors,
            tx_dropped: dev.tx_dropped,
        }
    }

    /// Check if device has any errors or dropped packets
    pub fn has_errors(&self) -> bool {
        self.rx_errors > 0 || self.tx_errors > 0 || self.rx_dropped > 0 || self.tx_dropped > 0
    }

    /// Get total errors (rx + tx)
    pub fn total_errors(&self) -> u64 {
        self.rx_errors + self.tx_errors
    }

    /// Get total dropped (rx + tx)
    pub fn total_dropped(&self) -> u64 {
        self.rx_dropped + self.tx_dropped
    }

    /// Get total traffic (rx + tx bytes)
    pub fn total_traffic(&self) -> u64 {
        self.rx_bytes + self.tx_bytes
    }

    /// Format bytes as human-readable (KB, MB, GB, TB)
    pub fn format_bytes(bytes: u64) -> String {
        const KB: u64 = 1024;
        const MB: u64 = KB * 1024;
        const GB: u64 = MB * 1024;
        const TB: u64 = GB * 1024;

        if bytes >= TB {
            format!("{:.1} TB", bytes as f64 / TB as f64)
        } else if bytes >= GB {
            format!("{:.1} GB", bytes as f64 / GB as f64)
        } else if bytes >= MB {
            format!("{:.1} MB", bytes as f64 / MB as f64)
        } else if bytes >= KB {
            format!("{:.1} KB", bytes as f64 / KB as f64)
        } else {
            format!("{} B", bytes)
        }
    }

    /// Format rate as human-readable (KB/s, MB/s, GB/s)
    pub fn format_rate(bytes_per_sec: u64) -> String {
        const KB: u64 = 1024;
        const MB: u64 = KB * 1024;
        const GB: u64 = MB * 1024;

        if bytes_per_sec >= GB {
            format!("{:.1} GB/s", bytes_per_sec as f64 / GB as f64)
        } else if bytes_per_sec >= MB {
            format!("{:.1} MB/s", bytes_per_sec as f64 / MB as f64)
        } else if bytes_per_sec >= KB {
            format!("{:.1} KB/s", bytes_per_sec as f64 / KB as f64)
        } else {
            format!("{} B/s", bytes_per_sec)
        }
    }
}

/// Calculated rate for a network device (from delta between samples)
#[derive(Debug, Clone, Default)]
pub struct NetDevRate {
    /// Device name
    pub name: String,
    /// RX bytes per second
    pub rx_bytes_per_sec: u64,
    /// TX bytes per second
    pub tx_bytes_per_sec: u64,
    /// Current RX errors (cumulative)
    pub rx_errors: u64,
    /// Current TX errors (cumulative)
    pub tx_errors: u64,
    /// Current RX dropped (cumulative)
    pub rx_dropped: u64,
    /// Current TX dropped (cumulative)
    pub tx_dropped: u64,
}

impl NetDevRate {
    /// Calculate rate from previous and current samples
    pub fn from_delta(prev: &NetDevStats, curr: &NetDevStats, elapsed_secs: f64) -> Self {
        let rx_delta = curr.rx_bytes.saturating_sub(prev.rx_bytes);
        let tx_delta = curr.tx_bytes.saturating_sub(prev.tx_bytes);

        Self {
            name: curr.name.clone(),
            rx_bytes_per_sec: if elapsed_secs > 0.0 {
                (rx_delta as f64 / elapsed_secs) as u64
            } else {
                0
            },
            tx_bytes_per_sec: if elapsed_secs > 0.0 {
                (tx_delta as f64 / elapsed_secs) as u64
            } else {
                0
            },
            rx_errors: curr.rx_errors,
            tx_errors: curr.tx_errors,
            rx_dropped: curr.rx_dropped,
            tx_dropped: curr.tx_dropped,
        }
    }

    /// Check if device has any errors or dropped packets
    pub fn has_errors(&self) -> bool {
        self.rx_errors > 0 || self.tx_errors > 0 || self.rx_dropped > 0 || self.tx_dropped > 0
    }

    /// Get total rate (rx + tx)
    pub fn total_rate(&self) -> u64 {
        self.rx_bytes_per_sec + self.tx_bytes_per_sec
    }

    /// Get total errors
    pub fn total_errors(&self) -> u64 {
        self.rx_errors + self.tx_errors
    }

    /// Get total dropped
    pub fn total_dropped(&self) -> u64 {
        self.rx_dropped + self.tx_dropped
    }
}

// ==================== Connection Types ====================

/// Filter for netstat queries
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetstatFilter {
    /// All connections
    #[default]
    All,
    /// Only connected (ESTABLISHED, etc.)
    Connected,
    /// Only listening
    Listening,
}

impl NetstatFilter {
    fn to_proto(self) -> i32 {
        match self {
            NetstatFilter::All => 0,
            NetstatFilter::Connected => 1,
            NetstatFilter::Listening => 2,
        }
    }
}

/// Network connections for a node
#[derive(Debug, Clone)]
pub struct NodeConnections {
    /// Node hostname
    pub hostname: String,
    /// Connections on this node
    pub connections: Vec<ConnectionInfo>,
}

impl NodeConnections {
    /// Count connections by state
    pub fn count_by_state(&self) -> ConnectionCounts {
        let mut counts = ConnectionCounts::default();
        for conn in &self.connections {
            match conn.state {
                ConnectionState::Established => counts.established += 1,
                ConnectionState::Listen => counts.listen += 1,
                ConnectionState::TimeWait => counts.time_wait += 1,
                ConnectionState::CloseWait => counts.close_wait += 1,
                ConnectionState::SynSent => counts.syn_sent += 1,
                _ => counts.other += 1,
            }
        }
        counts
    }
}

/// Connection counts by state
#[derive(Debug, Clone, Default)]
pub struct ConnectionCounts {
    pub established: usize,
    pub listen: usize,
    pub time_wait: usize,
    pub close_wait: usize,
    pub syn_sent: usize,
    pub other: usize,
}

impl ConnectionCounts {
    /// Count connections by state from a slice of ConnectionInfo
    pub fn count_by_state(connections: &[ConnectionInfo]) -> Self {
        let mut counts = ConnectionCounts::default();
        for conn in connections {
            match conn.state {
                ConnectionState::Established => counts.established += 1,
                ConnectionState::Listen => counts.listen += 1,
                ConnectionState::TimeWait => counts.time_wait += 1,
                ConnectionState::CloseWait => counts.close_wait += 1,
                ConnectionState::SynSent => counts.syn_sent += 1,
                _ => counts.other += 1,
            }
        }
        counts
    }

    /// Total number of connections
    pub fn total(&self) -> usize {
        self.established + self.listen + self.time_wait + self.close_wait + self.syn_sent + self.other
    }

    /// Check if there are any warning conditions
    pub fn has_warnings(&self) -> bool {
        self.time_wait > 100 || self.close_wait > 0 || self.syn_sent > 0
    }
}

/// Information about a single network connection
#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    /// Protocol (tcp, tcp6, udp, etc.)
    pub protocol: String,
    /// Local IP address
    pub local_ip: String,
    /// Local port
    pub local_port: u32,
    /// Remote IP address (empty for LISTEN)
    pub remote_ip: String,
    /// Remote port (0 for LISTEN)
    pub remote_port: u32,
    /// Connection state
    pub state: ConnectionState,
    /// Receive queue size
    pub rx_queue: u64,
    /// Transmit queue size
    pub tx_queue: u64,
}

impl ConnectionInfo {
    fn from_proto(record: crate::proto::machine::ConnectRecord) -> Self {
        Self {
            protocol: record.l4proto,
            local_ip: record.localip,
            local_port: record.localport,
            remote_ip: record.remoteip,
            remote_port: record.remoteport,
            state: ConnectionState::from_proto(record.state),
            rx_queue: record.rxqueue,
            tx_queue: record.txqueue,
        }
    }

    /// Check if this is a listening socket
    pub fn is_listening(&self) -> bool {
        self.state == ConnectionState::Listen
    }

    /// Check if this is an established connection
    pub fn is_established(&self) -> bool {
        self.state == ConnectionState::Established
    }

    /// Format local address as "ip:port" or ":port" for listening
    pub fn local_addr(&self) -> String {
        if self.local_ip.is_empty() || self.local_ip == "0.0.0.0" || self.local_ip == "::" {
            format!(":{}", self.local_port)
        } else {
            format!("{}:{}", self.local_ip, self.local_port)
        }
    }

    /// Format remote address as "ip:port" or "-" for listening
    pub fn remote_addr(&self) -> String {
        if self.remote_ip.is_empty() || self.remote_port == 0 {
            "-".to_string()
        } else {
            format!("{}:{}", self.remote_ip, self.remote_port)
        }
    }
}

/// Connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Established,
    SynSent,
    SynRecv,
    FinWait1,
    FinWait2,
    TimeWait,
    Close,
    CloseWait,
    LastAck,
    Listen,
    Closing,
    Unknown,
}

impl ConnectionState {
    fn from_proto(value: i32) -> Self {
        match value {
            1 => ConnectionState::Established,
            2 => ConnectionState::SynSent,
            3 => ConnectionState::SynRecv,
            4 => ConnectionState::FinWait1,
            5 => ConnectionState::FinWait2,
            6 => ConnectionState::TimeWait,
            7 => ConnectionState::Close,
            8 => ConnectionState::CloseWait,
            9 => ConnectionState::LastAck,
            10 => ConnectionState::Listen,
            11 => ConnectionState::Closing,
            _ => ConnectionState::Unknown,
        }
    }

    /// Get short name for display
    pub fn short_name(&self) -> &'static str {
        match self {
            ConnectionState::Established => "ESTABLISHED",
            ConnectionState::SynSent => "SYN_SENT",
            ConnectionState::SynRecv => "SYN_RECV",
            ConnectionState::FinWait1 => "FIN_WAIT1",
            ConnectionState::FinWait2 => "FIN_WAIT2",
            ConnectionState::TimeWait => "TIME_WAIT",
            ConnectionState::Close => "CLOSE",
            ConnectionState::CloseWait => "CLOSE_WAIT",
            ConnectionState::LastAck => "LAST_ACK",
            ConnectionState::Listen => "LISTEN",
            ConnectionState::Closing => "CLOSING",
            ConnectionState::Unknown => "UNKNOWN",
        }
    }

    /// Check if this is a problematic state
    pub fn is_problematic(&self) -> bool {
        matches!(self, ConnectionState::CloseWait | ConnectionState::SynSent)
    }
}
