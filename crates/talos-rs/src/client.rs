//! High-level Talos API client
//!
//! Provides a convenient interface for interacting with Talos clusters.

use crate::auth::create_channel;
use crate::config::{Context, TalosConfig};
use crate::error::TalosError;
use crate::proto::machine::machine_service_client::MachineServiceClient;
use crate::proto::machine::LogsRequest;
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
                meminfo: msg.meminfo.map(|m| MemInfo {
                    mem_total: m.memtotal,
                    mem_free: m.memfree,
                    mem_available: m.memavailable,
                    buffers: m.buffers,
                    cached: m.cached,
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
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(data) => {
                        if let Ok(text) = String::from_utf8(data.bytes) {
                            // Split into lines and send each
                            for line in text.lines() {
                                if !line.trim().is_empty()
                                    && tx.send(line.to_string()).is_err()
                                {
                                    // Receiver dropped, stop streaming
                                    break;
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
