//! High-level Talos API client
//!
//! Provides a convenient interface for interacting with Talos clusters.

use crate::auth::create_channel;
use crate::config::{Context, TalosConfig};
use crate::error::TalosError;
use crate::proto::machine::machine_service_client::MachineServiceClient;
use crate::proto::machine::{EtcdMemberListRequest, LogsRequest, NetstatRequest, netstat_request};
use crate::proto::time::time_service_client::TimeServiceClient;
use tokio_stream::StreamExt;
use tonic::transport::Channel;
use tonic::Request;

/// Link type for BPF filter generation.
///
/// Different interface types require different BPF filter offsets:
/// - EN10MB: Ethernet interfaces with 14-byte Ethernet header
/// - RAW: Tunnel interfaces (wireguard, kubespan) with raw IP packets
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkType {
    /// Ethernet link type (DLT_EN10MB) - 14-byte Ethernet header
    EN10MB,
    /// Raw IP link type (DLT_RAW) - no link-layer header
    RAW,
}

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

    /// Get a TimeService client
    fn time_client(&self) -> TimeServiceClient<Channel> {
        TimeServiceClient::new(self.channel.clone())
    }

    /// Extract node name from response metadata, with fallback to configured nodes
    ///
    /// The hostname field is only populated when going through the apid proxy.
    /// When connecting directly to a node, we fall back to the configured node address.
    fn node_from_metadata(&self, metadata: Option<&crate::proto::common::Metadata>, index: usize) -> String {
        metadata
            .map(|m| m.hostname.clone())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| {
                self.nodes.get(index)
                    .map(|n| n.split(':').next().unwrap_or(n).to_string())
                    .unwrap_or_else(|| {
                        self.nodes.first()
                            .map(|n| n.split(':').next().unwrap_or(n).to_string())
                            .unwrap_or_else(|| "node".to_string())
                    })
            })
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
            .enumerate()
            .map(|(i, msg)| {
                VersionInfo {
                    node: self.node_from_metadata(msg.metadata.as_ref(), i),
                    version: msg.version.as_ref().map(|v| v.tag.clone()).unwrap_or_default(),
                    sha: msg.version.as_ref().map(|v| v.sha.clone()).unwrap_or_default(),
                    built: msg.version.as_ref().map(|v| v.built.clone()).unwrap_or_default(),
                    go_version: msg.version.as_ref().map(|v| v.go_version.clone()).unwrap_or_default(),
                    os: msg.version.as_ref().map(|v| v.os.clone()).unwrap_or_default(),
                    arch: msg.version.as_ref().map(|v| v.arch.clone()).unwrap_or_default(),
                    platform: msg.platform.as_ref().map(|p| p.name.clone()).unwrap_or_default(),
                }
            })
            .collect();

        Ok(versions)
    }

    /// Get time synchronization status from all configured nodes
    ///
    /// Returns NTP server info, local and remote times, and sync status.
    pub async fn time(&self) -> Result<Vec<NodeTimeInfo>, TalosError> {
        let mut client = self.time_client();
        let request = self.with_nodes(Request::new(()));

        let response = client.time(request).await?;
        let inner = response.into_inner();

        // Tolerance for considering time "synced" (in seconds)
        const SYNC_TOLERANCE_SECS: f64 = 1.0;

        let times: Vec<NodeTimeInfo> = inner
            .messages
            .into_iter()
            .enumerate()
            .map(|(i, msg)| {
                let local_time = msg.localtime.map(|t| {
                    std::time::UNIX_EPOCH + std::time::Duration::new(
                        t.seconds as u64,
                        t.nanos as u32,
                    )
                });
                let remote_time = msg.remotetime.map(|t| {
                    std::time::UNIX_EPOCH + std::time::Duration::new(
                        t.seconds as u64,
                        t.nanos as u32,
                    )
                });

                // Calculate offset
                let offset_seconds = match (msg.localtime, msg.remotetime) {
                    (Some(local), Some(remote)) => {
                        let local_nanos = local.seconds as f64 * 1_000_000_000.0 + local.nanos as f64;
                        let remote_nanos = remote.seconds as f64 * 1_000_000_000.0 + remote.nanos as f64;
                        (local_nanos - remote_nanos) / 1_000_000_000.0
                    }
                    _ => 0.0,
                };

                let synced = offset_seconds.abs() < SYNC_TOLERANCE_SECS;

                NodeTimeInfo {
                    node: self.node_from_metadata(msg.metadata.as_ref(), i),
                    server: msg.server,
                    local_time,
                    remote_time,
                    offset_seconds,
                    synced,
                }
            })
            .collect();

        Ok(times)
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
                node: self.node_from_metadata(msg.metadata.as_ref(), 0),
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

    /// Restart a service on all configured nodes
    ///
    /// Returns the response message from each node.
    pub async fn service_restart(&self, service_id: &str) -> Result<Vec<ServiceRestartResult>, TalosError> {
        use crate::proto::machine::ServiceRestartRequest;

        let mut client = self.machine_client();
        let request = self.with_nodes(Request::new(ServiceRestartRequest {
            id: service_id.to_string(),
        }));

        let response = client.service_restart(request).await?;
        let inner = response.into_inner();

        let results: Vec<ServiceRestartResult> = inner
            .messages
            .into_iter()
            .map(|msg| ServiceRestartResult {
                node: self.node_from_metadata(msg.metadata.as_ref(), 0),
                response: msg.resp,
            })
            .collect();

        Ok(results)
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
                node: self.node_from_metadata(msg.metadata.as_ref(), 0),
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
                node: self.node_from_metadata(msg.metadata.as_ref(), 0),
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
                    node: self.node_from_metadata(msg.metadata.as_ref(), 0),
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
                    node: self.node_from_metadata(msg.metadata.as_ref(), 0),
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
                    node: self.node_from_metadata(msg.metadata.as_ref(), 0),
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
            let node = self.node_from_metadata(msg.metadata.as_ref(), 0);
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
            let hostname = self.node_from_metadata(msg.metadata.as_ref(), 0);

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
            let hostname = self.node_from_metadata(msg.metadata.as_ref(), 0);

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
        // Enable pid feature to get process info for each connection
        let request = self.with_nodes(Request::new(NetstatRequest {
            filter: filter.to_proto(),
            feature: Some(netstat_request::Feature { pid: true }),
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
            let hostname = self.node_from_metadata(msg.metadata.as_ref(), 0);

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

    /// Get dmesg (kernel ring buffer) output
    ///
    /// # Arguments
    /// * `follow` - If true, continue streaming new messages (not recommended for non-async use)
    /// * `tail` - If true, only return recent messages
    pub async fn dmesg(&self, follow: bool, tail: bool) -> Result<String, TalosError> {
        use crate::proto::machine::DmesgRequest;

        let mut client = self.machine_client();
        let request = self.with_nodes(Request::new(DmesgRequest { follow, tail }));

        let response = client.dmesg(request).await?;
        let mut stream = response.into_inner();

        let mut output = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(data) => {
                    if let Ok(text) = String::from_utf8(data.bytes) {
                        output.push_str(&text);
                    }
                }
                Err(e) => {
                    tracing::warn!("Dmesg stream error: {}", e);
                    break;
                }
            }
        }

        Ok(output)
    }

    /// Read a file from the node's filesystem
    ///
    /// Returns the file contents as a string, or an error if the file doesn't exist
    /// or cannot be read.
    pub async fn read_file(&self, path: &str) -> Result<String, TalosError> {
        use crate::proto::machine::ReadRequest;

        let mut client = self.machine_client();
        let request = self.with_nodes(Request::new(ReadRequest {
            path: path.to_string(),
        }));

        let response = client.read(request).await?;
        let mut stream = response.into_inner();

        let mut output = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(data) => {
                    // Check for errors in the metadata
                    if let Some(metadata) = &data.metadata {
                        if !metadata.error.is_empty() {
                            return Err(TalosError::Connection(metadata.error.clone()));
                        }
                    }
                    if let Ok(text) = String::from_utf8(data.bytes) {
                        output.push_str(&text);
                    }
                }
                Err(e) => {
                    // If we get an error (like file not found), return it
                    return Err(e.into());
                }
            }
        }

        Ok(output)
    }

    /// Check if the br_netfilter kernel module is loaded
    ///
    /// Returns true if the module is loaded, false otherwise.
    /// This checks by reading /proc/sys/net/bridge/bridge-nf-call-iptables
    /// which only exists when br_netfilter is loaded.
    pub async fn is_br_netfilter_loaded(&self) -> Result<bool, TalosError> {
        match self.read_file("/proc/sys/net/bridge/bridge-nf-call-iptables").await {
            Ok(content) => {
                tracing::info!("br_netfilter sysctl file exists, content: {:?}", content.trim());
                Ok(true)
            }
            Err(e) => {
                tracing::info!("br_netfilter sysctl file not found: {}", e);
                Ok(false)
            }
        }
    }

    /// Get kubeconfig from the cluster
    ///
    /// Returns the kubeconfig YAML that can be used to access the Kubernetes API.
    /// This kubeconfig is generated by Talos and includes proper certificates.
    ///
    /// Note: Talos returns the kubeconfig as a gzip-compressed tarball, so we
    /// decompress and extract it here.
    pub async fn kubeconfig(&self) -> Result<String, TalosError> {
        let mut client = self.machine_client();
        let request = self.with_nodes(Request::new(()));

        let response = client.kubeconfig(request).await?;
        let mut stream = response.into_inner();

        // Collect the gzipped tarball data
        let mut compressed_data = Vec::new();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(data) => {
                    compressed_data.extend_from_slice(&data.bytes);
                }
                Err(e) => {
                    tracing::warn!("Kubeconfig stream error: {}", e);
                    break;
                }
            }
        }

        if compressed_data.is_empty() {
            return Err(TalosError::Connection(
                "No kubeconfig data received from Talos".to_string(),
            ));
        }

        // Decompress gzip
        use flate2::read::GzDecoder;
        use std::io::Read;
        use tar::Archive;

        let decoder = GzDecoder::new(&compressed_data[..]);
        let mut archive = Archive::new(decoder);

        // Extract kubeconfig from the tarball
        let entries = archive.entries().map_err(|e| {
            TalosError::Connection(format!("Failed to read kubeconfig tarball: {}", e))
        })?;

        for entry in entries {
            let mut entry = entry.map_err(|e| {
                TalosError::Connection(format!("Failed to read tarball entry: {}", e))
            })?;

            // The kubeconfig file is typically named "kubeconfig" in the archive
            let path = entry.path().map_err(|e| {
                TalosError::Connection(format!("Failed to get entry path: {}", e))
            })?;

            if path.to_string_lossy().contains("kubeconfig") {
                let mut content = String::new();
                entry.read_to_string(&mut content).map_err(|e| {
                    TalosError::Connection(format!("Failed to read kubeconfig: {}", e))
                })?;
                return Ok(content);
            }
        }

        Err(TalosError::Connection(
            "Kubeconfig file not found in tarball".to_string(),
        ))
    }

    /// Apply a configuration patch to the node
    ///
    /// # Arguments
    /// * `config_yaml` - YAML configuration to apply (can be a patch or full config)
    /// * `mode` - How to apply the configuration
    /// * `dry_run` - If true, validate but don't apply
    pub async fn apply_configuration(
        &self,
        config_yaml: &str,
        mode: ApplyMode,
        dry_run: bool,
    ) -> Result<Vec<ApplyConfigResult>, TalosError> {
        use crate::proto::machine::{ApplyConfigurationRequest, apply_configuration_request::Mode};

        let proto_mode = match mode {
            ApplyMode::Reboot => Mode::Reboot,
            ApplyMode::Auto => Mode::Auto,
            ApplyMode::NoReboot => Mode::NoReboot,
            ApplyMode::Staged => Mode::Staged,
        };

        let mut client = self.machine_client();
        let request = self.with_nodes(Request::new(ApplyConfigurationRequest {
            data: config_yaml.as_bytes().to_vec(),
            mode: proto_mode as i32,
            dry_run,
            try_mode_timeout: None,
        }));

        let response = client.apply_configuration(request).await?;
        let inner = response.into_inner();

        let results: Vec<ApplyConfigResult> = inner
            .messages
            .into_iter()
            .map(|msg| ApplyConfigResult {
                node: self.node_from_metadata(msg.metadata.as_ref(), 0),
                mode_result: msg.mode_details,
                warnings: msg.warnings,
            })
            .collect();

        Ok(results)
    }

    /// Stream packet capture from an interface
    ///
    /// Returns a receiver that yields raw pcap data chunks.
    /// The first chunk contains the pcap file header.
    ///
    /// # Arguments
    /// * `interface` - Network interface name (e.g., "eth0")
    /// * `promiscuous` - Enable promiscuous mode
    /// * `snap_len` - Maximum bytes to capture per packet (0 = use default 65535)
    pub async fn packet_capture(
        &self,
        interface: &str,
        promiscuous: bool,
        snap_len: u32,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>, TalosError> {
        self.packet_capture_with_filter(interface, promiscuous, snap_len, Vec::new())
            .await
    }

    /// Determine the appropriate link type for a network interface.
    ///
    /// - Tunnel interfaces (kubespan, wg*, tun*, tap*) use RAW
    /// - Standard Ethernet interfaces (eth*, ens*, bond*, etc.) use EN10MB
    /// - Loopback (lo) uses EN10MB on Linux (has pseudo-Ethernet header)
    pub fn detect_link_type(interface: &str) -> LinkType {
        // Wireguard and tunnel interfaces use RAW (no Ethernet header)
        if interface.starts_with("kubespan")
            || interface.starts_with("wg")
            || interface.starts_with("tun")
        {
            LinkType::RAW
        } else {
            // Most interfaces (eth*, ens*, bond*, veth*, lo, etc.) use Ethernet framing
            LinkType::EN10MB
        }
    }

    /// Start packet capture with BPF filter to exclude the Talos API port.
    ///
    /// This prevents feedback loops when capturing on the management interface
    /// by filtering out traffic on port 50000 (Talos apid).
    ///
    /// Automatically detects the link type based on interface name:
    /// - EN10MB for Ethernet interfaces (eth*, ens*, bond*, lo, etc.)
    /// - RAW for tunnel interfaces (kubespan, wg*, tun*)
    pub async fn packet_capture_exclude_api(
        &self,
        interface: &str,
        promiscuous: bool,
        snap_len: u32,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>, TalosError> {
        let link_type = Self::detect_link_type(interface);
        let bpf_filter = Self::build_port_exclusion_filter(50000, link_type);
        self.packet_capture_with_filter(interface, promiscuous, snap_len, bpf_filter)
            .await
    }

    /// Build BPF filter to exclude a specific TCP/UDP port.
    ///
    /// Returns BPF bytecode that accepts all packets EXCEPT those with
    /// the specified port as either source or destination (TCP or UDP).
    ///
    /// # Arguments
    /// * `port` - Port number to exclude
    /// * `link_type` - Link type determines header offsets
    fn build_port_exclusion_filter(
        port: u16,
        link_type: LinkType,
    ) -> Vec<crate::proto::machine::BpfInstruction> {
        match link_type {
            LinkType::EN10MB => Self::build_port_exclusion_filter_ethernet(port),
            LinkType::RAW => Self::build_port_exclusion_filter_raw(port),
        }
    }

    /// Build BPF filter for Ethernet-framed packets (EN10MB/DLT_EN10MB).
    ///
    /// Used for standard Ethernet interfaces (eth*, ens*, bond*, lo, etc.)
    /// Generated from: tcpdump -dd -y EN10MB 'not port 50000'
    fn build_port_exclusion_filter_ethernet(port: u16) -> Vec<crate::proto::machine::BpfInstruction> {
        use crate::proto::machine::BpfInstruction;

        let port_k = port as u32;

        // BPF bytecode from: tcpdump -dd -y EN10MB 'not port <port>'
        // Handles IPv4, IPv6, TCP, UDP, SCTP, and fragment checking
        vec![
            // (000) ldh [12]                  ; Load EtherType
            BpfInstruction { op: 0x28, jt: 0, jf: 0, k: 0x0000000c },
            // (001) jeq #0x86dd, 0, 8         ; If IPv6, continue; else check IPv4
            BpfInstruction { op: 0x15, jt: 0, jf: 8, k: 0x000086dd },
            // (002) ldb [20]                  ; Load IPv6 next header
            BpfInstruction { op: 0x30, jt: 0, jf: 0, k: 0x00000014 },
            // (003) jeq #132, 2, 0            ; Check SCTP
            BpfInstruction { op: 0x15, jt: 2, jf: 0, k: 0x00000084 },
            // (004) jeq #6, 1, 0              ; Check TCP
            BpfInstruction { op: 0x15, jt: 1, jf: 0, k: 0x00000006 },
            // (005) jeq #17, 0, 17            ; Check UDP
            BpfInstruction { op: 0x15, jt: 0, jf: 17, k: 0x00000011 },
            // (006) ldh [54]                  ; Load IPv6 src port
            BpfInstruction { op: 0x28, jt: 0, jf: 0, k: 0x00000036 },
            // (007) jeq #port, 14, 0          ; If port matches, goto reject
            BpfInstruction { op: 0x15, jt: 14, jf: 0, k: port_k },
            // (008) ldh [56]                  ; Load IPv6 dst port
            BpfInstruction { op: 0x28, jt: 0, jf: 0, k: 0x00000038 },
            // (009) jeq #port, 12, 13         ; If port matches, goto reject; else accept
            BpfInstruction { op: 0x15, jt: 12, jf: 13, k: port_k },
            // (010) jeq #0x0800, 0, 12        ; Check IPv4
            BpfInstruction { op: 0x15, jt: 0, jf: 12, k: 0x00000800 },
            // (011) ldb [23]                  ; Load IPv4 protocol
            BpfInstruction { op: 0x30, jt: 0, jf: 0, k: 0x00000017 },
            // (012) jeq #132, 2, 0            ; Check SCTP
            BpfInstruction { op: 0x15, jt: 2, jf: 0, k: 0x00000084 },
            // (013) jeq #6, 1, 0              ; Check TCP
            BpfInstruction { op: 0x15, jt: 1, jf: 0, k: 0x00000006 },
            // (014) jeq #17, 0, 8             ; Check UDP
            BpfInstruction { op: 0x15, jt: 0, jf: 8, k: 0x00000011 },
            // (015) ldh [20]                  ; Load frag offset field
            BpfInstruction { op: 0x28, jt: 0, jf: 0, k: 0x00000014 },
            // (016) jset #0x1fff, 6, 0        ; Check if fragmented
            BpfInstruction { op: 0x45, jt: 6, jf: 0, k: 0x00001fff },
            // (017) ldxb 4*([14]&0xf)         ; Load IP header length
            BpfInstruction { op: 0xb1, jt: 0, jf: 0, k: 0x0000000e },
            // (018) ldh [x+14]                ; Load src port
            BpfInstruction { op: 0x48, jt: 0, jf: 0, k: 0x0000000e },
            // (019) jeq #port, 2, 0           ; If port matches, goto reject
            BpfInstruction { op: 0x15, jt: 2, jf: 0, k: port_k },
            // (020) ldh [x+16]                ; Load dst port
            BpfInstruction { op: 0x48, jt: 0, jf: 0, k: 0x00000010 },
            // (021) jeq #port, 0, 1           ; If port matches, goto reject; else accept
            BpfInstruction { op: 0x15, jt: 0, jf: 1, k: port_k },
            // (022) ret #0                    ; Reject packet
            BpfInstruction { op: 0x06, jt: 0, jf: 0, k: 0x00000000 },
            // (023) ret #262144               ; Accept packet
            BpfInstruction { op: 0x06, jt: 0, jf: 0, k: 0x00040000 },
        ]
    }

    /// Build BPF filter for raw IP packets (RAW/DLT_RAW).
    ///
    /// Used for tunnel interfaces (kubespan, wireguard, tun, etc.)
    /// where packets start directly with IP header (no Ethernet frame).
    /// Generated from: tcpdump -dd -y RAW 'not port 50000'
    fn build_port_exclusion_filter_raw(port: u16) -> Vec<crate::proto::machine::BpfInstruction> {
        use crate::proto::machine::BpfInstruction;

        let port_k = port as u32;

        // BPF bytecode from: tcpdump -dd -y RAW 'not port <port>'
        // Handles IPv4, IPv6, TCP, UDP, SCTP, and fragment checking
        vec![
            // (000) ldb [0]                   ; Load IP version byte
            BpfInstruction { op: 0x30, jt: 0, jf: 0, k: 0x00000000 },
            // (001) and #0xf0                 ; Mask for IP version
            BpfInstruction { op: 0x54, jt: 0, jf: 0, k: 0x000000f0 },
            // (002) jeq #0x60, 0, 8           ; If IPv6, continue; else check IPv4
            BpfInstruction { op: 0x15, jt: 0, jf: 8, k: 0x00000060 },
            // (003) ldb [6]                   ; Load IPv6 next header
            BpfInstruction { op: 0x30, jt: 0, jf: 0, k: 0x00000006 },
            // (004) jeq #132, 2, 0            ; Check SCTP
            BpfInstruction { op: 0x15, jt: 2, jf: 0, k: 0x00000084 },
            // (005) jeq #6, 1, 0              ; Check TCP
            BpfInstruction { op: 0x15, jt: 1, jf: 0, k: 0x00000006 },
            // (006) jeq #17, 0, 19            ; Check UDP
            BpfInstruction { op: 0x15, jt: 0, jf: 19, k: 0x00000011 },
            // (007) ldh [40]                  ; Load IPv6 src port
            BpfInstruction { op: 0x28, jt: 0, jf: 0, k: 0x00000028 },
            // (008) jeq #port, 16, 0          ; If port matches, goto reject
            BpfInstruction { op: 0x15, jt: 16, jf: 0, k: port_k },
            // (009) ldh [42]                  ; Load IPv6 dst port
            BpfInstruction { op: 0x28, jt: 0, jf: 0, k: 0x0000002a },
            // (010) jeq #port, 14, 15         ; If port matches, goto reject; else accept
            BpfInstruction { op: 0x15, jt: 14, jf: 15, k: port_k },
            // (011) ldb [0]                   ; Load IP version byte again
            BpfInstruction { op: 0x30, jt: 0, jf: 0, k: 0x00000000 },
            // (012) and #0xf0                 ; Mask for IP version
            BpfInstruction { op: 0x54, jt: 0, jf: 0, k: 0x000000f0 },
            // (013) jeq #0x40, 0, 12          ; Check IPv4
            BpfInstruction { op: 0x15, jt: 0, jf: 12, k: 0x00000040 },
            // (014) ldb [9]                   ; Load IPv4 protocol
            BpfInstruction { op: 0x30, jt: 0, jf: 0, k: 0x00000009 },
            // (015) jeq #132, 2, 0            ; Check SCTP
            BpfInstruction { op: 0x15, jt: 2, jf: 0, k: 0x00000084 },
            // (016) jeq #6, 1, 0              ; Check TCP
            BpfInstruction { op: 0x15, jt: 1, jf: 0, k: 0x00000006 },
            // (017) jeq #17, 0, 8             ; Check UDP
            BpfInstruction { op: 0x15, jt: 0, jf: 8, k: 0x00000011 },
            // (018) ldh [6]                   ; Load frag offset field
            BpfInstruction { op: 0x28, jt: 0, jf: 0, k: 0x00000006 },
            // (019) jset #0x1fff, 6, 0        ; Check if fragmented
            BpfInstruction { op: 0x45, jt: 6, jf: 0, k: 0x00001fff },
            // (020) ldxb 4*([0]&0xf)          ; Load IP header length
            BpfInstruction { op: 0xb1, jt: 0, jf: 0, k: 0x00000000 },
            // (021) ldh [x+0]                 ; Load src port
            BpfInstruction { op: 0x48, jt: 0, jf: 0, k: 0x00000000 },
            // (022) jeq #port, 2, 0           ; If port matches, goto reject
            BpfInstruction { op: 0x15, jt: 2, jf: 0, k: port_k },
            // (023) ldh [x+2]                 ; Load dst port
            BpfInstruction { op: 0x48, jt: 0, jf: 0, k: 0x00000002 },
            // (024) jeq #port, 0, 1           ; If port matches, goto reject; else accept
            BpfInstruction { op: 0x15, jt: 0, jf: 1, k: port_k },
            // (025) ret #0                    ; Reject packet
            BpfInstruction { op: 0x06, jt: 0, jf: 0, k: 0x00000000 },
            // (026) ret #262144               ; Accept packet
            BpfInstruction { op: 0x06, jt: 0, jf: 0, k: 0x00040000 },
        ]
    }

    /// Internal packet capture with explicit BPF filter
    async fn packet_capture_with_filter(
        &self,
        interface: &str,
        promiscuous: bool,
        snap_len: u32,
        bpf_filter: Vec<crate::proto::machine::BpfInstruction>,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>, TalosError> {
        use crate::proto::machine::PacketCaptureRequest;

        let mut client = self.machine_client();

        let request = self.with_nodes(Request::new(PacketCaptureRequest {
            interface: interface.to_string(),
            promiscuous,
            snap_len: if snap_len == 0 { 65535 } else { snap_len },
            bpf_filter,
        }));

        let response = client.packet_capture(request).await?;
        let mut stream = response.into_inner();

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        // Spawn a task to read from the stream and send to channel
        tokio::spawn(async move {
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(data) => {
                        if tx.send(data.bytes).is_err() {
                            // Receiver dropped, stop streaming
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Packet capture stream error: {}", e);
                        break;
                    }
                }
            }
        });

        Ok(rx)
    }

    /// Reboot the node
    ///
    /// # Arguments
    /// * `mode` - Reboot mode (default, powercycle)
    pub async fn reboot(&self, mode: RebootMode) -> Result<RebootResult, TalosError> {
        use crate::proto::machine::{RebootRequest, reboot_request::Mode};

        let proto_mode = match mode {
            RebootMode::Default => Mode::Default,
            RebootMode::Powercycle => Mode::Powercycle,
        };

        let mut client = self.machine_client();
        let request = self.with_nodes(Request::new(RebootRequest {
            mode: proto_mode as i32,
        }));

        let response = client.reboot(request).await?;
        let inner = response.into_inner();

        // Get the first message (we're typically rebooting one node at a time)
        let msg = inner.messages.into_iter().next();

        Ok(RebootResult {
            node: msg.as_ref()
                .and_then(|m| m.metadata.as_ref())
                .map(|m| m.hostname.clone())
                .unwrap_or_else(|| self.nodes.first().cloned().unwrap_or_default()),
            success: msg.is_some(),
        })
    }
}

/// Reboot mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RebootMode {
    /// Default reboot
    #[default]
    Default,
    /// Power cycle (hard reboot)
    Powercycle,
}

/// Result of a reboot operation
#[derive(Debug, Clone)]
pub struct RebootResult {
    /// Node that was rebooted
    pub node: String,
    /// Whether the reboot was initiated successfully
    pub success: bool,
}

// ==================== Configuration Types ====================

/// Mode for applying configuration changes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyMode {
    /// Reboot immediately after applying
    Reboot,
    /// Auto-detect if reboot is needed
    Auto,
    /// Apply without rebooting (may not apply all changes)
    NoReboot,
    /// Stage for next reboot
    Staged,
}

/// Result of applying configuration
#[derive(Debug, Clone)]
pub struct ApplyConfigResult {
    /// Node that was configured
    pub node: String,
    /// Details about the apply mode result
    pub mode_result: String,
    /// Any warnings from the apply
    pub warnings: Vec<String>,
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
    /// Platform name (e.g., "container", "metal", "aws", "gcp")
    pub platform: String,
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

/// Result of a service restart operation
#[derive(Debug, Clone)]
pub struct ServiceRestartResult {
    pub node: String,
    pub response: String,
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
    /// Process ID owning this connection (if available)
    pub process_pid: Option<u32>,
    /// Process name owning this connection (if available)
    pub process_name: Option<String>,
    /// Network namespace (for container connections)
    pub netns: Option<String>,
}

impl ConnectionInfo {
    fn from_proto(record: crate::proto::machine::ConnectRecord) -> Self {
        // Extract process info if available
        let (process_pid, process_name) = record.process
            .map(|p| (Some(p.pid), Some(p.name)))
            .unwrap_or((None, None));

        // Extract network namespace if non-empty
        let netns = if record.netns.is_empty() {
            None
        } else {
            Some(record.netns)
        };

        Self {
            protocol: record.l4proto,
            local_ip: record.localip,
            local_port: record.localport,
            remote_ip: record.remoteip,
            remote_port: record.remoteport,
            state: ConnectionState::from_proto(record.state),
            rx_queue: record.rxqueue,
            tx_queue: record.txqueue,
            process_pid,
            process_name,
            netns,
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

// ==================== Time Types ====================

/// Time synchronization status for a node
#[derive(Debug, Clone)]
pub struct NodeTimeInfo {
    /// Node hostname
    pub node: String,
    /// NTP server being used
    pub server: String,
    /// Local time
    pub local_time: Option<std::time::SystemTime>,
    /// Remote NTP time
    pub remote_time: Option<std::time::SystemTime>,
    /// Time offset from NTP server (in seconds, positive means local is ahead)
    pub offset_seconds: f64,
    /// Whether time is considered synced (offset within tolerance)
    pub synced: bool,
}

impl NodeTimeInfo {
    /// Get a human-readable offset string
    pub fn offset_human(&self) -> String {
        let offset_ms = (self.offset_seconds * 1000.0).abs();
        if offset_ms < 1.0 {
            format!("{:.3} ms", offset_ms)
        } else if offset_ms < 1000.0 {
            format!("{:.1} ms", offset_ms)
        } else {
            format!("{:.2} s", self.offset_seconds.abs())
        }
    }

    /// Get sync status as a string
    pub fn sync_status(&self) -> &'static str {
        if self.synced {
            "synced"
        } else {
            "not synced"
        }
    }
}
