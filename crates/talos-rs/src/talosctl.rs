//! Talosctl command execution
//!
//! Provides functions to execute talosctl commands and parse their output.
//! This is necessary because the COSI State API is not exposed externally
//! through apid - talosctl connects directly to machined via Unix socket.

use crate::error::TalosError;
use std::process::Command;

/// Execute a talosctl command and return stdout (blocking)
fn exec_talosctl(args: &[&str]) -> Result<String, TalosError> {
    let output = Command::new("talosctl")
        .args(args)
        .output()
        .map_err(TalosError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TalosError::Connection(format!(
            "talosctl failed: {}",
            stderr.trim()
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Execute a talosctl command asynchronously and return stdout
async fn exec_talosctl_async(args: &[&str]) -> Result<String, TalosError> {
    let output = tokio::process::Command::new("talosctl")
        .args(args)
        .output()
        .await
        .map_err(TalosError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TalosError::Connection(format!(
            "talosctl failed: {}",
            stderr.trim()
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Volume encryption status from VolumeStatus resource
#[derive(Debug, Clone)]
pub struct VolumeStatus {
    /// Volume ID (e.g., "STATE", "EPHEMERAL")
    pub id: String,
    /// Encryption provider type
    pub encryption_provider: Option<String>,
    /// Volume phase
    pub phase: String,
    /// Pretty size
    pub size: String,
    /// Filesystem type
    pub filesystem: Option<String>,
    /// Mount location
    pub mount_location: Option<String>,
}

/// Disk information from Disks.block.talos.dev resource
#[derive(Debug, Clone)]
pub struct DiskInfo {
    /// Disk ID (e.g., "sda", "nvme0n1")
    pub id: String,
    /// Device path (e.g., "/dev/sda")
    pub dev_path: String,
    /// Size in bytes
    pub size: u64,
    /// Human-readable size (e.g., "500 GB")
    pub size_pretty: String,
    /// Disk model
    pub model: Option<String>,
    /// Disk serial number
    pub serial: Option<String>,
    /// Transport type (e.g., "sata", "nvme", "virtio", "usb")
    pub transport: Option<String>,
    /// Whether the disk is rotational (HDD) or not (SSD/NVMe)
    pub rotational: bool,
    /// Whether the disk is read-only
    pub readonly: bool,
    /// Whether the disk is a CD-ROM
    pub cdrom: bool,
    /// World Wide ID
    pub wwid: Option<String>,
    /// Bus path
    pub bus_path: Option<String>,
}

/// Machine config info from MachineConfig resource
#[derive(Debug, Clone)]
pub struct MachineConfigInfo {
    /// Config version (resource version, acts as hash)
    pub version: String,
    /// Machine type
    pub machine_type: Option<String>,
}

/// KubeSpan peer status from KubeSpanPeerStatus resource
#[derive(Debug, Clone)]
pub struct KubeSpanPeerStatus {
    /// Peer ID (usually the node name or public key)
    pub id: String,
    /// Peer label/hostname
    pub label: String,
    /// Endpoint address (IP:port)
    pub endpoint: Option<String>,
    /// Peer state (e.g., "up", "down", "unknown")
    pub state: String,
    /// Round-trip time in milliseconds
    pub rtt_ms: Option<f64>,
    /// Last handshake time
    pub last_handshake: Option<String>,
    /// Received bytes
    pub rx_bytes: u64,
    /// Transmitted bytes
    pub tx_bytes: u64,
}

/// Discovery member from Members resource
#[derive(Debug, Clone)]
pub struct DiscoveryMember {
    /// Member ID (node ID)
    pub id: String,
    /// Member addresses
    pub addresses: Vec<String>,
    /// Hostname
    pub hostname: String,
    /// Machine type (controlplane, worker)
    pub machine_type: String,
    /// Operating system
    pub operating_system: String,
}

/// Address status from AddressStatus resource (for VIP detection)
#[derive(Debug, Clone)]
pub struct AddressStatus {
    /// Address ID (interface name)
    pub id: String,
    /// Link name
    pub link_name: String,
    /// Address with CIDR
    pub address: String,
    /// Address family (inet, inet6)
    pub family: String,
    /// Address scope
    pub scope: String,
    /// Flags (e.g., contains "vip" for shared VIPs)
    pub flags: Vec<String>,
}

/// Get volume status for a node
///
/// Executes: talosctl get volumestatus --nodes <node> -o yaml
pub fn get_volume_status(node: &str) -> Result<Vec<VolumeStatus>, TalosError> {
    let output = exec_talosctl(&["get", "volumestatus", "--nodes", node, "-o", "yaml"])?;
    parse_volume_status_yaml(&output)
}

/// Get volume status for a specific node using context authentication (async, non-blocking)
///
/// Executes: talosctl --context <context> [--talosconfig <path>] -n <node> get volumestatus -o yaml
pub async fn get_volume_status_for_node(
    context: &str,
    node_ip: &str,
    config_path: Option<&str>,
) -> Result<Vec<VolumeStatus>, TalosError> {
    let mut args = vec!["--context", context];

    // Add talosconfig path if provided
    let config_path_string;
    if let Some(path) = config_path {
        config_path_string = path.to_string();
        args.push("--talosconfig");
        args.push(&config_path_string);
    }

    args.extend_from_slice(&["-n", node_ip, "get", "volumestatus", "-o", "yaml"]);

    let output = exec_talosctl_async(&args).await?;
    parse_volume_status_yaml(&output)
}

/// Get disk information for a node
///
/// Executes: talosctl get disks --nodes <node> -o yaml
pub fn get_disks(node: &str) -> Result<Vec<DiskInfo>, TalosError> {
    let output = exec_talosctl(&["get", "disks", "--nodes", node, "-o", "yaml"])?;
    parse_disks_yaml(&output)
}

/// Get disk information for a specific node using context authentication (async, non-blocking)
///
/// Executes: talosctl --context <context> [--talosconfig <path>] -n <node> get disks -o yaml
pub async fn get_disks_for_node(
    context: &str,
    node_ip: &str,
    config_path: Option<&str>,
) -> Result<Vec<DiskInfo>, TalosError> {
    let mut args = vec!["--context", context];

    // Add talosconfig path if provided
    let config_path_string;
    if let Some(path) = config_path {
        config_path_string = path.to_string();
        args.push("--talosconfig");
        args.push(&config_path_string);
    }

    args.extend_from_slice(&["-n", node_ip, "get", "disks", "-o", "yaml"]);

    let output = exec_talosctl_async(&args).await?;
    parse_disks_yaml(&output)
}

/// Get disk information for a context (async, non-blocking)
///
/// Executes: talosctl --context <context> -n <node> get disks -o yaml
pub async fn get_disks_for_context(
    context: &str,
    config_path: Option<&str>,
) -> Result<Vec<DiskInfo>, TalosError> {
    // Load config to get an endpoint IP to use as the node target
    let config = match config_path {
        Some(path) => {
            let path_buf = std::path::PathBuf::from(path);
            crate::TalosConfig::load_from(&path_buf)?
        }
        None => crate::TalosConfig::load_default()?,
    };
    let ctx = config
        .contexts
        .get(context)
        .ok_or_else(|| TalosError::ContextNotFound(context.to_string()))?;

    // Get the first endpoint and extract the IP (remove port if present)
    let node_ip = ctx
        .endpoints
        .first()
        .ok_or_else(|| TalosError::NoEndpoints(context.to_string()))?
        .split(':')
        .next()
        .unwrap_or("")
        .to_string();

    if node_ip.is_empty() {
        return Err(TalosError::NoEndpoints(context.to_string()));
    }

    let output = exec_talosctl_async(&[
        "--context",
        context,
        "-n",
        &node_ip,
        "get",
        "disks",
        "-o",
        "yaml",
    ])
    .await?;
    parse_disks_yaml(&output)
}

// ============================================================================
// Insecure Mode Functions
// ============================================================================
// These functions connect to Talos nodes without TLS client certificates.
// Used for maintenance mode nodes that haven't been bootstrapped yet.

/// Get disks from a node in insecure mode (no TLS client auth)
///
/// Executes: talosctl get disks --insecure -n <endpoint> -o yaml
///
/// This is useful for pre-bootstrap scenarios where you want to see
/// what disks are available before writing the machine configuration.
pub async fn get_disks_insecure(endpoint: &str) -> Result<Vec<DiskInfo>, TalosError> {
    let output =
        exec_talosctl_async(&["get", "disks", "--insecure", "-n", endpoint, "-o", "yaml"]).await?;
    parse_disks_yaml(&output)
}

/// Get volume status from a node in insecure mode (no TLS client auth)
///
/// Executes: talosctl get volumestatus --insecure -n <endpoint> -o yaml
pub async fn get_volume_status_insecure(endpoint: &str) -> Result<Vec<VolumeStatus>, TalosError> {
    let output = exec_talosctl_async(&[
        "get",
        "volumestatus",
        "--insecure",
        "-n",
        endpoint,
        "-o",
        "yaml",
    ])
    .await?;
    parse_volume_status_yaml(&output)
}

/// Version info returned from insecure mode
#[derive(Debug, Clone)]
pub struct InsecureVersionInfo {
    /// Talos version tag (e.g., "v1.12.1")
    pub tag: String,
    /// Whether the node is in maintenance mode
    pub maintenance_mode: bool,
}

/// Get version info from a node in insecure mode (no TLS client auth)
///
/// Executes: talosctl version --insecure -n <endpoint>
///
/// Note: In maintenance mode, the full version API is not available,
/// so this may return limited info or fail gracefully.
pub async fn get_version_insecure(endpoint: &str) -> Result<InsecureVersionInfo, TalosError> {
    let output = exec_talosctl_async(&["version", "--insecure", "-n", endpoint]).await;

    match output {
        Ok(text) => {
            // Parse version output - look for "Tag:" line
            let tag = text
                .lines()
                .find(|l| l.trim().starts_with("Tag:"))
                .and_then(|l| l.split(':').nth(1))
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "unknown".to_string());

            Ok(InsecureVersionInfo {
                tag,
                maintenance_mode: false,
            })
        }
        Err(e) => {
            // Check if it's a "not implemented in maintenance mode" error
            let err_str = e.to_string();
            if err_str.contains("maintenance mode") || err_str.contains("not implemented") {
                Ok(InsecureVersionInfo {
                    tag: "unknown".to_string(),
                    maintenance_mode: true,
                })
            } else {
                Err(e)
            }
        }
    }
}

/// Check if a node is reachable in insecure mode
///
/// Returns true if we can connect to the maintenance API
pub async fn check_insecure_connection(endpoint: &str) -> bool {
    // Try to get disks - this works in maintenance mode
    get_disks_insecure(endpoint).await.is_ok()
}

/// Result of generating Talos configuration
#[derive(Debug, Clone)]
pub struct GenConfigResult {
    /// Path to generated controlplane.yaml
    pub controlplane_path: String,
    /// Path to generated worker.yaml
    pub worker_path: String,
    /// Path to generated talosconfig
    pub talosconfig_path: String,
    /// Output directory
    pub output_dir: String,
}

/// Generate Talos machine configuration
///
/// Executes: talosctl gen config <cluster-name> <endpoint> --output-dir <dir> [--force]
///
/// This generates controlplane.yaml, worker.yaml, and talosconfig in the output directory.
pub async fn gen_config(
    cluster_name: &str,
    kubernetes_endpoint: &str,
    output_dir: &str,
    additional_sans: Option<&[&str]>,
    force: bool,
) -> Result<GenConfigResult, TalosError> {
    let mut args = vec!["gen", "config", cluster_name, kubernetes_endpoint];

    args.push("--output-dir");
    args.push(output_dir);

    // Add additional SANs if provided
    let sans_joined: String;
    if let Some(sans) = additional_sans
        && !sans.is_empty()
    {
        sans_joined = sans.join(",");
        args.push("--additional-sans");
        args.push(&sans_joined);
    }

    if force {
        args.push("--force");
    }

    exec_talosctl_async(&args).await?;

    Ok(GenConfigResult {
        controlplane_path: format!("{}/controlplane.yaml", output_dir),
        worker_path: format!("{}/worker.yaml", output_dir),
        talosconfig_path: format!("{}/talosconfig", output_dir),
        output_dir: output_dir.to_string(),
    })
}

/// Result of applying configuration in insecure mode
#[derive(Debug, Clone)]
pub struct InsecureApplyResult {
    /// Whether the apply was successful
    pub success: bool,
    /// Output message
    pub message: String,
}

/// Apply configuration to a node in insecure mode
///
/// Executes: talosctl apply-config --insecure -n <endpoint> -f <config_path>
///
/// This applies a machine configuration to a node in maintenance mode.
/// The node will install Talos and reboot.
pub async fn apply_config_insecure(
    endpoint: &str,
    config_path: &str,
) -> Result<InsecureApplyResult, TalosError> {
    let output = exec_talosctl_async(&[
        "apply-config",
        "--insecure",
        "-n",
        endpoint,
        "-f",
        config_path,
    ])
    .await;

    match output {
        Ok(msg) => Ok(InsecureApplyResult {
            success: true,
            message: if msg.trim().is_empty() {
                "Configuration applied successfully. Node will install and reboot.".to_string()
            } else {
                msg
            },
        }),
        Err(e) => Ok(InsecureApplyResult {
            success: false,
            message: format!("Failed to apply config: {}", e),
        }),
    }
}

/// Reboot a node in insecure mode
///
/// Executes: talosctl reboot --insecure -n <endpoint>
pub async fn reboot_insecure(endpoint: &str) -> Result<String, TalosError> {
    exec_talosctl_async(&["reboot", "--insecure", "-n", endpoint]).await
}

/// Shutdown a node in insecure mode
///
/// Executes: talosctl shutdown --insecure -n <endpoint>
pub async fn shutdown_insecure(endpoint: &str) -> Result<String, TalosError> {
    exec_talosctl_async(&["shutdown", "--insecure", "-n", endpoint]).await
}

/// Get machine config info for a node
///
/// Executes: talosctl get machineconfig --nodes <node> -o yaml
pub fn get_machine_config(node: &str) -> Result<MachineConfigInfo, TalosError> {
    let output = exec_talosctl(&["get", "machineconfig", "--nodes", node, "-o", "yaml"])?;
    parse_machine_config_yaml(&output)
}

/// Get KubeSpan peer status for a node
///
/// Executes: talosctl get kubespanpeerstatus --nodes <node> -o yaml
pub fn get_kubespan_peers(node: &str) -> Result<Vec<KubeSpanPeerStatus>, TalosError> {
    let output = exec_talosctl(&["get", "kubespanpeerstatus", "--nodes", node, "-o", "yaml"])?;
    parse_kubespan_peers_yaml(&output)
}

/// Get discovery members for a node
///
/// Executes: talosctl get members --nodes <node> -o yaml
pub fn get_discovery_members(node: &str) -> Result<Vec<DiscoveryMember>, TalosError> {
    let output = exec_talosctl(&["get", "members", "--nodes", node, "-o", "yaml"])?;
    parse_discovery_members_yaml(&output)
}

/// Get discovery members for a context (async, non-blocking)
///
/// Executes: talosctl --context <context> -n <node> get members -o yaml
///
/// This version uses the context name to get the correct certificates and endpoint,
/// and uses tokio async process to avoid blocking the runtime.
/// It extracts a node IP from the context's endpoints to target the query.
///
/// If `config_path` is provided, loads config from that path instead of the default.
pub async fn get_discovery_members_for_context(
    context: &str,
    config_path: Option<&str>,
) -> Result<Vec<DiscoveryMember>, TalosError> {
    // Load config to get an endpoint IP to use as the node target
    // talosctl requires -n flag if nodes: is not set in the config
    let config = match config_path {
        Some(path) => {
            let path_buf = std::path::PathBuf::from(path);
            crate::TalosConfig::load_from(&path_buf)?
        }
        None => crate::TalosConfig::load_default()?,
    };
    let ctx = config
        .contexts
        .get(context)
        .ok_or_else(|| TalosError::ContextNotFound(context.to_string()))?;

    // Get the first endpoint and extract the IP (remove port if present)
    let node_ip = ctx
        .endpoints
        .first()
        .ok_or_else(|| TalosError::NoEndpoints(context.to_string()))?
        .split(':')
        .next()
        .unwrap_or("")
        .to_string();

    if node_ip.is_empty() {
        return Err(TalosError::NoEndpoints(context.to_string()));
    }

    let output = exec_talosctl_async(&[
        "--context",
        context,
        "-n",
        &node_ip,
        "get",
        "members",
        "-o",
        "yaml",
    ])
    .await?;
    parse_discovery_members_yaml(&output)
}

/// Get discovery members for a specific node IP using context certificates (async).
///
/// This allows querying a specific control plane node directly instead of going through the VIP.
async fn get_discovery_members_for_node_async(
    context: &str,
    node_ip: &str,
) -> Result<Vec<DiscoveryMember>, TalosError> {
    let output = exec_talosctl_async(&[
        "--context",
        context,
        "-n",
        node_ip,
        "get",
        "members",
        "-o",
        "yaml",
    ])
    .await?;
    parse_discovery_members_yaml(&output)
}

/// Get discovery members with automatic retry and fallback to specific nodes.
///
/// First tries the VIP endpoint (via context), then falls back to querying
/// individual control plane nodes directly if the VIP fails.
///
/// This handles transient "no request forwarding" errors that occur when
/// the VIP routes to a node that can't forward the request.
pub async fn get_discovery_members_with_retry(
    context: &str,
    config_path: Option<&str>,
    fallback_node_ips: &[String],
) -> Result<Vec<DiscoveryMember>, TalosError> {
    // First, try the VIP-based approach with retries
    // VIP_MAX_RETRIES=2 means 3 total attempts (initial + 2 retries)
    const VIP_MAX_RETRIES: u32 = 2;
    const BASE_DELAY_MS: u64 = 100;

    let mut last_error = None;

    for attempt in 0..=VIP_MAX_RETRIES {
        match get_discovery_members_for_context(context, config_path).await {
            Ok(members) => return Ok(members),
            Err(e) => {
                last_error = Some(e);
                if attempt < VIP_MAX_RETRIES {
                    let delay_ms = BASE_DELAY_MS * (1 << attempt);
                    tracing::debug!(
                        "Discovery fetch via VIP attempt {} failed, retrying in {}ms",
                        attempt + 1,
                        delay_ms
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
            }
        }
    }

    // VIP failed, try fallback nodes directly
    // Shuffle to distribute load and avoid repeatedly hitting a problematic node first
    if !fallback_node_ips.is_empty() {
        tracing::debug!(
            "VIP-based discovery failed, trying {} fallback nodes directly",
            fallback_node_ips.len()
        );

        let mut shuffled_ips: Vec<&String> = fallback_node_ips.iter().collect();
        fastrand::shuffle(&mut shuffled_ips);

        for node_ip in shuffled_ips {
            match get_discovery_members_for_node_async(context, node_ip).await {
                Ok(members) => {
                    tracing::debug!(
                        "Successfully fetched discovery members from fallback node {}",
                        node_ip
                    );
                    return Ok(members);
                }
                Err(e) => {
                    tracing::debug!("Fallback node {} failed: {}", node_ip, e);
                    last_error = Some(e);
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| TalosError::NoEndpoints(context.to_string())))
}

/// Get address status for a node (for VIP detection)
///
/// Executes: talosctl get addressstatus --nodes <node> -o yaml
pub fn get_address_status(node: &str) -> Result<Vec<AddressStatus>, TalosError> {
    let output = exec_talosctl(&["get", "addressstatus", "--nodes", node, "-o", "yaml"])?;
    parse_address_status_yaml(&output)
}

/// Check if KubeSpan is enabled for a node
///
/// Executes: talosctl get kubespanconfig --nodes <node> -o yaml
/// Returns true only if the command succeeds AND shows enabled: true
///
/// Note: We check kubespanconfig instead of kubespanidentity because
/// kubespanconfig exists on all nodes where KubeSpan is configured,
/// while kubespanidentity may be empty on single-node clusters.
pub fn is_kubespan_enabled(node: &str) -> bool {
    match exec_talosctl(&["get", "kubespanconfig", "--nodes", node, "-o", "yaml"]) {
        Ok(output) => {
            // Check if output contains KubeSpanConfig with enabled: true
            let trimmed = output.trim();
            !trimmed.is_empty()
                && trimmed.contains("KubeSpanConfig")
                && trimmed.contains("enabled: true")
        }
        Err(_) => false,
    }
}

/// Parse volume status YAML output from talosctl
fn parse_volume_status_yaml(yaml_str: &str) -> Result<Vec<VolumeStatus>, TalosError> {
    let mut volumes = Vec::new();

    // Split by YAML document separator and parse each
    for doc_str in yaml_str.split("\n---") {
        let doc_str = doc_str.trim();
        if doc_str.is_empty() {
            continue;
        }

        let doc: serde_yaml::Value = match serde_yaml::from_str(doc_str) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Get metadata.id
        let id = doc
            .get("metadata")
            .and_then(|m| m.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Skip if no id
        if id.is_empty() {
            continue;
        }

        // Get spec fields
        let spec = doc.get("spec");

        let encryption_provider = spec
            .and_then(|s| s.get("encryptionProvider"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let phase = spec
            .and_then(|s| s.get("phase"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        // Try prettySize first, fall back to showing volume type if not available
        let size = spec
            .and_then(|s| s.get("prettySize"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                // If no size, show the volume type (directory, partition, etc.)
                spec.and_then(|s| s.get("type"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_default();

        // Try filesystem first, fall back to volume type (directory, partition, symlink, etc.)
        let filesystem = spec
            .and_then(|s| s.get("filesystem"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                spec.and_then(|s| s.get("type"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            });

        // Try mountLocation first, then spec.mountSpec.targetPath, then use the id if it's a path
        let mount_location = spec
            .and_then(|s| s.get("mountLocation"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                spec.and_then(|s| s.get("mountSpec"))
                    .and_then(|m| m.get("targetPath"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .or_else(|| {
                // If id starts with /, it's likely a mount path
                if id.starts_with('/') {
                    Some(id.clone())
                } else {
                    None
                }
            });

        volumes.push(VolumeStatus {
            id,
            encryption_provider,
            phase,
            size,
            filesystem,
            mount_location,
        });
    }

    Ok(volumes)
}

/// Parse disks YAML output from talosctl
fn parse_disks_yaml(yaml_str: &str) -> Result<Vec<DiskInfo>, TalosError> {
    let mut disks = Vec::new();

    // Split by YAML document separator and parse each
    for doc_str in yaml_str.split("\n---") {
        let doc_str = doc_str.trim();
        if doc_str.is_empty() {
            continue;
        }

        let doc: serde_yaml::Value = match serde_yaml::from_str(doc_str) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Get metadata.id (e.g., "sda", "nvme0n1")
        let id = doc
            .get("metadata")
            .and_then(|m| m.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Skip if no id
        if id.is_empty() {
            continue;
        }

        // Get spec fields
        let spec = doc.get("spec");

        let dev_path = spec
            .and_then(|s| s.get("dev_path"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let size = spec
            .and_then(|s| s.get("size"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let size_pretty = spec
            .and_then(|s| s.get("human_size").or_else(|| s.get("pretty_size")))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let model = spec
            .and_then(|s| s.get("model"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        let serial = spec
            .and_then(|s| s.get("serial"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        let transport = spec
            .and_then(|s| s.get("transport"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        let rotational = spec
            .and_then(|s| s.get("rotational"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let readonly = spec
            .and_then(|s| s.get("readonly"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let cdrom = spec
            .and_then(|s| s.get("cdrom"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let wwid = spec
            .and_then(|s| s.get("wwid"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        let bus_path = spec
            .and_then(|s| s.get("bus_path"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        disks.push(DiskInfo {
            id,
            dev_path,
            size,
            size_pretty,
            model,
            serial,
            transport,
            rotational,
            readonly,
            cdrom,
            wwid,
            bus_path,
        });
    }

    Ok(disks)
}

/// Parse machine config YAML output from talosctl
fn parse_machine_config_yaml(yaml_str: &str) -> Result<MachineConfigInfo, TalosError> {
    let doc: serde_yaml::Value = serde_yaml::from_str(yaml_str)
        .map_err(|e| TalosError::Connection(format!("Failed to parse YAML: {}", e)))?;

    let version = doc
        .get("metadata")
        .and_then(|m| m.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let machine_type = doc
        .get("spec")
        .and_then(|s| s.get("machine"))
        .and_then(|m| m.get("type"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Ok(MachineConfigInfo {
        version,
        machine_type,
    })
}

/// Parse KubeSpan peer status YAML output from talosctl
fn parse_kubespan_peers_yaml(yaml_str: &str) -> Result<Vec<KubeSpanPeerStatus>, TalosError> {
    let mut peers = Vec::new();

    for doc_str in yaml_str.split("\n---") {
        let doc_str = doc_str.trim();
        if doc_str.is_empty() {
            continue;
        }

        let doc: serde_yaml::Value = match serde_yaml::from_str(doc_str) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = doc
            .get("metadata")
            .and_then(|m| m.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if id.is_empty() {
            continue;
        }

        let spec = doc.get("spec");

        let label = spec
            .and_then(|s| s.get("label"))
            .and_then(|v| v.as_str())
            .unwrap_or(&id)
            .to_string();

        let endpoint = spec
            .and_then(|s| s.get("endpoint"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let state = spec
            .and_then(|s| s.get("state"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        // RTT might be in nanoseconds or have a duration format
        let rtt_ms = spec
            .and_then(|s| s.get("lastUsedEndpoint"))
            .and_then(|e| e.get("rtt"))
            .and_then(|v| {
                // Could be a number or a string like "2.5ms"
                if let Some(n) = v.as_f64() {
                    Some(n / 1_000_000.0) // nanoseconds to ms
                } else if let Some(s) = v.as_str() {
                    parse_duration_to_ms(s)
                } else {
                    None
                }
            });

        let last_handshake = spec
            .and_then(|s| s.get("lastHandshakeTime"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let rx_bytes = spec
            .and_then(|s| s.get("receiveBytes"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let tx_bytes = spec
            .and_then(|s| s.get("transmitBytes"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        peers.push(KubeSpanPeerStatus {
            id,
            label,
            endpoint,
            state,
            rtt_ms,
            last_handshake,
            rx_bytes,
            tx_bytes,
        });
    }

    Ok(peers)
}

/// Parse discovery members YAML output from talosctl
fn parse_discovery_members_yaml(yaml_str: &str) -> Result<Vec<DiscoveryMember>, TalosError> {
    let mut members = Vec::new();

    for doc_str in yaml_str.split("\n---") {
        let doc_str = doc_str.trim();
        if doc_str.is_empty() {
            continue;
        }

        let doc: serde_yaml::Value = match serde_yaml::from_str(doc_str) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = doc
            .get("metadata")
            .and_then(|m| m.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if id.is_empty() {
            continue;
        }

        let spec = doc.get("spec");

        let addresses = spec
            .and_then(|s| s.get("addresses"))
            .and_then(|v| v.as_sequence())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let hostname = spec
            .and_then(|s| s.get("hostname"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let machine_type = spec
            .and_then(|s| s.get("machineType"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let operating_system = spec
            .and_then(|s| s.get("operatingSystem"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        members.push(DiscoveryMember {
            id,
            addresses,
            hostname,
            machine_type,
            operating_system,
        });
    }

    Ok(members)
}

/// Parse address status YAML output from talosctl
fn parse_address_status_yaml(yaml_str: &str) -> Result<Vec<AddressStatus>, TalosError> {
    let mut addresses = Vec::new();

    for doc_str in yaml_str.split("\n---") {
        let doc_str = doc_str.trim();
        if doc_str.is_empty() {
            continue;
        }

        let doc: serde_yaml::Value = match serde_yaml::from_str(doc_str) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = doc
            .get("metadata")
            .and_then(|m| m.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if id.is_empty() {
            continue;
        }

        let spec = doc.get("spec");

        let link_name = spec
            .and_then(|s| s.get("linkName"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let address = spec
            .and_then(|s| s.get("address"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let family = spec
            .and_then(|s| s.get("family"))
            .and_then(|v| v.as_str())
            .unwrap_or("inet")
            .to_string();

        let scope = spec
            .and_then(|s| s.get("scope"))
            .and_then(|v| v.as_str())
            .unwrap_or("global")
            .to_string();

        let flags = spec
            .and_then(|s| s.get("flags"))
            .and_then(|v| v.as_sequence())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        addresses.push(AddressStatus {
            id,
            link_name,
            address,
            family,
            scope,
            flags,
        });
    }

    Ok(addresses)
}

/// Parse a duration string like "2.5ms" or "1s" to milliseconds
fn parse_duration_to_ms(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.ends_with("ms") {
        s.trim_end_matches("ms").parse::<f64>().ok()
    } else if s.ends_with("µs") || s.ends_with("us") {
        s.trim_end_matches("µs")
            .trim_end_matches("us")
            .parse::<f64>()
            .ok()
            .map(|v| v / 1000.0)
    } else if s.ends_with("ns") {
        s.trim_end_matches("ns")
            .parse::<f64>()
            .ok()
            .map(|v| v / 1_000_000.0)
    } else if s.ends_with('s') {
        s.trim_end_matches('s')
            .parse::<f64>()
            .ok()
            .map(|v| v * 1000.0)
    } else {
        s.parse::<f64>().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_volume_status() {
        let yaml = r#"
node: 10.5.0.2
metadata:
    namespace: runtime
    type: VolumeStatuses.block.talos.dev
    id: STATE
    version: "1"
    phase: running
spec:
    phase: ready
    location: /dev/sda6
    encryptionProvider: luks2
    filesystem: xfs
    mountLocation: /system/state
    prettySize: 100 MiB
---
node: 10.5.0.2
metadata:
    namespace: runtime
    type: VolumeStatuses.block.talos.dev
    id: EPHEMERAL
    version: "1"
    phase: running
spec:
    phase: ready
    location: /dev/sda5
    filesystem: xfs
    mountLocation: /var
    prettySize: 10 GiB
"#;

        let volumes = parse_volume_status_yaml(yaml).unwrap();
        assert_eq!(volumes.len(), 2);
        assert_eq!(volumes[0].id, "STATE");
        assert_eq!(volumes[0].encryption_provider, Some("luks2".to_string()));
        assert_eq!(volumes[1].id, "EPHEMERAL");
        assert_eq!(volumes[1].encryption_provider, None);
    }

    #[test]
    fn test_parse_machine_config() {
        let yaml = r#"
node: 10.5.0.2
metadata:
    namespace: config
    type: MachineConfigs.config.talos.dev
    id: v1alpha1
    version: "5"
spec:
    machine:
        type: controlplane
"#;

        let config = parse_machine_config_yaml(yaml).unwrap();
        assert_eq!(config.version, "5");
        assert_eq!(config.machine_type, Some("controlplane".to_string()));
    }

    #[test]
    fn test_parse_disks() {
        let yaml = r#"
node: 172.20.0.5
metadata:
    namespace: runtime
    type: Disks.block.talos.dev
    id: sda
    version: 1
    owner: block.DisksController
    phase: running
spec:
    dev_path: /dev/sda
    size: 10485760000
    human_size: 10 GB
    io_size: 512
    sector_size: 512
    readonly: false
    cdrom: false
    model: QEMU HARDDISK
    modalias: scsi:t-0x00
    bus_path: /pci0000:00/0000:00:07.0/virtio4/host1/target1:0:0/1:0:0:0
    sub_system: /sys/class/block
    transport: virtio
    rotational: true
---
node: 172.20.0.5
metadata:
    namespace: runtime
    type: Disks.block.talos.dev
    id: nvme0n1
    version: 1
    owner: block.DisksController
    phase: running
spec:
    dev_path: /dev/nvme0n1
    size: 256060514304
    human_size: 256 GB
    io_size: 4096
    sector_size: 512
    readonly: false
    cdrom: false
    model: Samsung SSD 970 EVO Plus
    serial: S4EVNG0N123456
    wwid: nvme.144d-5334455...
    bus_path: /pci0000:00/0000:00:1d.0/0000:3d:00.0/nvme/nvme0/nvme0n1
    sub_system: /sys/class/block
    transport: nvme
    rotational: false
"#;

        let disks = parse_disks_yaml(yaml).unwrap();
        assert_eq!(disks.len(), 2);

        // First disk - virtio HDD
        assert_eq!(disks[0].id, "sda");
        assert_eq!(disks[0].dev_path, "/dev/sda");
        assert_eq!(disks[0].size, 10485760000);
        assert_eq!(disks[0].size_pretty, "10 GB");
        assert_eq!(disks[0].model, Some("QEMU HARDDISK".to_string()));
        assert_eq!(disks[0].transport, Some("virtio".to_string()));
        assert!(disks[0].rotational);
        assert!(!disks[0].readonly);

        // Second disk - NVMe SSD
        assert_eq!(disks[1].id, "nvme0n1");
        assert_eq!(disks[1].dev_path, "/dev/nvme0n1");
        assert_eq!(disks[1].size_pretty, "256 GB");
        assert_eq!(disks[1].model, Some("Samsung SSD 970 EVO Plus".to_string()));
        assert_eq!(disks[1].serial, Some("S4EVNG0N123456".to_string()));
        assert_eq!(disks[1].transport, Some("nvme".to_string()));
        assert!(!disks[1].rotational);
    }

    #[test]
    fn test_parse_discovery_members() {
        let yaml = r#"
node: 192.168.9.11
metadata:
    namespace: cluster
    type: Members.cluster.talos.dev
    id: 3xKYjp
    version: 1
    owner: cluster.DiscoveryService
    phase: running
spec:
    nodeId: 3xKYjp
    addresses:
        - 192.168.9.11
    hostname: cp-1
    machineType: controlplane
    operatingSystem: Talos (v1.9.2)
---
node: 192.168.9.11
metadata:
    namespace: cluster
    type: Members.cluster.talos.dev
    id: 4yLZkq
    version: 1
    owner: cluster.DiscoveryService
    phase: running
spec:
    nodeId: 4yLZkq
    addresses:
        - 192.168.9.21
        - 10.244.0.1
    hostname: worker-1
    machineType: worker
    operatingSystem: Talos (v1.9.2)
"#;

        let members = parse_discovery_members_yaml(yaml).unwrap();
        assert_eq!(members.len(), 2);

        // Control plane node
        assert_eq!(members[0].id, "3xKYjp");
        assert_eq!(members[0].hostname, "cp-1");
        assert_eq!(members[0].machine_type, "controlplane");
        assert_eq!(members[0].addresses, vec!["192.168.9.11"]);
        assert!(members[0].operating_system.contains("Talos"));

        // Worker node with multiple addresses
        assert_eq!(members[1].id, "4yLZkq");
        assert_eq!(members[1].hostname, "worker-1");
        assert_eq!(members[1].machine_type, "worker");
        assert_eq!(members[1].addresses.len(), 2);
        assert!(members[1].addresses.contains(&"192.168.9.21".to_string()));
    }

    #[test]
    fn test_parse_discovery_members_empty() {
        let yaml = "";
        let members = parse_discovery_members_yaml(yaml).unwrap();
        assert!(members.is_empty());
    }

    #[test]
    fn test_parse_discovery_members_invalid_yaml() {
        // Should skip invalid documents and not panic
        let yaml = r#"
not valid yaml: [
---
node: 192.168.9.11
metadata:
    namespace: cluster
    type: Members.cluster.talos.dev
    id: valid
spec:
    hostname: valid-host
    machineType: controlplane
"#;
        let members = parse_discovery_members_yaml(yaml).unwrap();
        // Should have parsed the valid document
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].id, "valid");
    }

    #[test]
    fn test_shuffle_fallback_ips_does_not_panic() {
        // Test that shuffle works correctly on various inputs
        let empty: Vec<String> = vec![];
        let mut shuffled: Vec<&String> = empty.iter().collect();
        fastrand::shuffle(&mut shuffled);
        assert!(shuffled.is_empty());

        let single = ["192.168.1.1".to_string()];
        let mut shuffled: Vec<&String> = single.iter().collect();
        fastrand::shuffle(&mut shuffled);
        assert_eq!(shuffled.len(), 1);

        let multiple = vec![
            "192.168.1.1".to_string(),
            "192.168.1.2".to_string(),
            "192.168.1.3".to_string(),
        ];
        let mut shuffled: Vec<&String> = multiple.iter().collect();
        fastrand::shuffle(&mut shuffled);
        assert_eq!(shuffled.len(), 3);
        // All original elements should still be present
        for ip in &multiple {
            assert!(shuffled.contains(&ip));
        }
    }
}
