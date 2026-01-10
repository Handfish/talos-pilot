//! Talosctl command execution
//!
//! Provides functions to execute talosctl commands and parse their output.
//! This is necessary because the COSI State API is not exposed externally
//! through apid - talosctl connects directly to machined via Unix socket.

use crate::error::TalosError;
use std::process::Command;

/// Execute a talosctl command and return stdout
fn exec_talosctl(args: &[&str]) -> Result<String, TalosError> {
    let output = Command::new("talosctl")
        .args(args)
        .output()
        .map_err(|e| TalosError::Io(e))?;

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
