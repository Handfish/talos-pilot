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
/// Executes: talosctl --context <context> get members -o yaml
///
/// This version uses the context name to get the correct certificates and endpoint,
/// and uses tokio async process to avoid blocking the runtime.
pub async fn get_discovery_members_for_context(
    context: &str,
) -> Result<Vec<DiscoveryMember>, TalosError> {
    let output =
        exec_talosctl_async(&["--context", context, "get", "members", "-o", "yaml"]).await?;
    parse_discovery_members_yaml(&output)
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

        let size = spec
            .and_then(|s| s.get("prettySize"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let filesystem = spec
            .and_then(|s| s.get("filesystem"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let mount_location = spec
            .and_then(|s| s.get("mountLocation"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

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
}
