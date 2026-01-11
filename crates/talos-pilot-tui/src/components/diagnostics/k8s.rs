//! Kubernetes client helper for diagnostics
//!
//! Creates a K8s client from Talos-provided kubeconfig.

use k8s_openapi::api::core::v1::{Node, Pod};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use kube::{
    api::{Api, EvictParams, ListParams, Patch, PatchParams},
    Client, Config,
};
use k8s_openapi::serde_json::json;
use talos_rs::TalosClient;

/// Error type for K8s operations
#[derive(Debug, thiserror::Error)]
pub enum K8sError {
    #[error("Failed to get kubeconfig from Talos: {0}")]
    KubeconfigFetch(String),
    #[error("Failed to parse kubeconfig: {0}")]
    KubeconfigParse(String),
    #[error("Failed to create K8s client: {0}")]
    ClientCreate(String),
    #[error("K8s API error: {0}")]
    ApiError(String),
}

/// Create a Kubernetes client from Talos-provided kubeconfig
pub async fn create_k8s_client(talos_client: &TalosClient) -> Result<Client, K8sError> {
    // Get kubeconfig from Talos
    let kubeconfig_yaml = talos_client
        .kubeconfig()
        .await
        .map_err(|e| K8sError::KubeconfigFetch(e.to_string()))?;

    // Parse kubeconfig
    let kubeconfig: kube::config::Kubeconfig = serde_yaml::from_str(&kubeconfig_yaml)
        .map_err(|e| K8sError::KubeconfigParse(e.to_string()))?;

    // Create client config from kubeconfig
    let config = Config::from_custom_kubeconfig(kubeconfig, &Default::default())
        .await
        .map_err(|e| K8sError::ClientCreate(e.to_string()))?;

    // Create client
    Client::try_from(config).map_err(|e| K8sError::ClientCreate(e.to_string()))
}

/// Detected CNI information from K8s
#[derive(Debug, Clone, Default)]
pub struct CniInfo {
    /// Detected CNI type
    pub cni_type: super::types::CniType,
    /// CNI pods in kube-system
    pub pods: Vec<CniPodInfo>,
}

/// Information about a CNI pod
#[derive(Debug, Clone)]
pub struct CniPodInfo {
    /// Pod name
    pub name: String,
    /// Node the pod is running on
    pub node_name: Option<String>,
    /// Pod phase (Running, Pending, etc.)
    pub phase: String,
    /// Whether pod is ready
    pub ready: bool,
    /// Number of restarts
    pub restart_count: i32,
}

/// Detect CNI type by checking pods in kube-system namespace
pub async fn detect_cni_from_k8s(client: &Client) -> Result<CniInfo, K8sError> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), "kube-system");

    let pod_list = pods
        .list(&ListParams::default())
        .await
        .map_err(|e| K8sError::ApiError(e.to_string()))?;

    let mut cni_info = CniInfo::default();

    for pod in pod_list.items {
        let name = pod.metadata.name.clone().unwrap_or_default();
        let name_lower = name.to_lowercase();

        // Detect CNI type from pod names
        let is_cni_pod = if name_lower.starts_with("kube-flannel")
            || name_lower.starts_with("flannel")
        {
            cni_info.cni_type = super::types::CniType::Flannel;
            true
        } else if name_lower.starts_with("cilium") {
            cni_info.cni_type = super::types::CniType::Cilium;
            true
        } else if name_lower.starts_with("calico") || name_lower.starts_with("calico-node") {
            cni_info.cni_type = super::types::CniType::Calico;
            true
        } else {
            false
        };

        if is_cni_pod {
            let status = pod.status.as_ref();
            let phase = status
                .and_then(|s| s.phase.clone())
                .unwrap_or_else(|| "Unknown".to_string());

            // Check if pod is ready
            let ready = status
                .and_then(|s| s.conditions.as_ref())
                .map(|conditions| {
                    conditions
                        .iter()
                        .any(|c| c.type_ == "Ready" && c.status == "True")
                })
                .unwrap_or(false);

            // Get restart count from container statuses
            let restart_count = status
                .and_then(|s| s.container_statuses.as_ref())
                .map(|containers| {
                    containers.iter().map(|c| c.restart_count).sum()
                })
                .unwrap_or(0);

            // Get node name from pod spec
            let node_name = pod.spec.as_ref().and_then(|s| s.node_name.clone());

            cni_info.pods.push(CniPodInfo {
                name,
                node_name,
                phase,
                ready,
                restart_count,
            });
        }
    }

    Ok(cni_info)
}

/// Check if all CNI pods are healthy
pub fn are_cni_pods_healthy(info: &CniInfo) -> bool {
    if info.pods.is_empty() {
        return false;
    }

    info.pods.iter().all(|pod| {
        pod.phase == "Running" && pod.ready
    })
}

/// Get summary of CNI pod health
pub fn cni_pod_health_summary(info: &CniInfo) -> String {
    if info.pods.is_empty() {
        return "No CNI pods found".to_string();
    }

    let total = info.pods.len();
    let healthy = info.pods.iter().filter(|p| p.phase == "Running" && p.ready).count();
    let total_restarts: i32 = info.pods.iter().map(|p| p.restart_count).sum();

    if healthy == total && total_restarts == 0 {
        format!("{}/{} pods healthy", healthy, total)
    } else if healthy == total {
        format!("{}/{} pods healthy ({} restarts)", healthy, total, total_restarts)
    } else {
        format!("{}/{} pods healthy", healthy, total)
    }
}

/// Information about an unhealthy pod
#[derive(Debug, Clone)]
pub struct UnhealthyPodInfo {
    /// Pod name
    pub name: String,
    /// Pod namespace
    pub namespace: String,
    /// Container state (e.g., "CrashLoopBackOff", "ImagePullBackOff")
    pub state: String,
    /// Number of restarts
    pub restart_count: i32,
    /// Last termination reason (if any)
    pub last_reason: Option<String>,
}

/// Pod health summary from K8s API
#[derive(Debug, Clone, Default)]
pub struct PodHealthInfo {
    /// Pods in CrashLoopBackOff
    pub crashing: Vec<UnhealthyPodInfo>,
    /// Pods in ImagePullBackOff
    pub image_pull_errors: Vec<UnhealthyPodInfo>,
    /// Pods stuck in Pending
    pub pending: Vec<UnhealthyPodInfo>,
    /// Total pod count
    pub total_pods: usize,
}

impl PodHealthInfo {
    /// Check if there are any unhealthy pods
    pub fn has_issues(&self) -> bool {
        !self.crashing.is_empty() || !self.image_pull_errors.is_empty()
    }

    /// Get summary message
    pub fn summary(&self) -> String {
        if self.crashing.is_empty() && self.image_pull_errors.is_empty() && self.pending.is_empty() {
            "All pods healthy".to_string()
        } else {
            let mut parts = Vec::new();
            if !self.crashing.is_empty() {
                parts.push(format!("{} crashing", self.crashing.len()));
            }
            if !self.image_pull_errors.is_empty() {
                parts.push(format!("{} image errors", self.image_pull_errors.len()));
            }
            if !self.pending.is_empty() {
                parts.push(format!("{} pending", self.pending.len()));
            }
            parts.join(", ")
        }
    }
}

/// Check pod health across all namespaces using K8s API
///
/// This is the authoritative way to check for crashing pods - no log parsing!
pub async fn check_pod_health(client: &Client) -> Result<PodHealthInfo, K8sError> {
    let pods: Api<Pod> = Api::all(client.clone());

    let pod_list = pods
        .list(&ListParams::default())
        .await
        .map_err(|e| K8sError::ApiError(e.to_string()))?;

    let mut info = PodHealthInfo {
        total_pods: pod_list.items.len(),
        ..Default::default()
    };

    for pod in pod_list.items {
        let name = pod.metadata.name.clone().unwrap_or_default();
        let namespace = pod.metadata.namespace.clone().unwrap_or_default();
        let status = pod.status.as_ref();

        // Check container statuses for waiting states
        if let Some(container_statuses) = status.and_then(|s| s.container_statuses.as_ref()) {
            for cs in container_statuses {
                if let Some(waiting) = cs.state.as_ref().and_then(|s| s.waiting.as_ref()) {
                    let reason = waiting.reason.clone().unwrap_or_default();
                    let restart_count = cs.restart_count;

                    // Get last termination reason if available
                    let last_reason = cs
                        .last_state
                        .as_ref()
                        .and_then(|s| s.terminated.as_ref())
                        .and_then(|t| t.reason.clone());

                    let pod_info = UnhealthyPodInfo {
                        name: name.clone(),
                        namespace: namespace.clone(),
                        state: reason.clone(),
                        restart_count,
                        last_reason,
                    };

                    match reason.as_str() {
                        "CrashLoopBackOff" => info.crashing.push(pod_info),
                        "ImagePullBackOff" | "ErrImagePull" => info.image_pull_errors.push(pod_info),
                        _ => {}
                    }
                }
            }
        }

        // Check for stuck Pending pods (no container statuses yet)
        let phase = status.and_then(|s| s.phase.clone()).unwrap_or_default();
        if phase == "Pending" {
            // Check if it's been pending for a while (has conditions but no containers)
            let has_conditions = status
                .and_then(|s| s.conditions.as_ref())
                .map(|c| !c.is_empty())
                .unwrap_or(false);
            let no_containers = status
                .and_then(|s| s.container_statuses.as_ref())
                .map(|c| c.is_empty())
                .unwrap_or(true);

            if has_conditions && no_containers {
                info.pending.push(UnhealthyPodInfo {
                    name,
                    namespace,
                    state: "Pending".to_string(),
                    restart_count: 0,
                    last_reason: None,
                });
            }
        }
    }

    Ok(info)
}

/// Information about a PodDisruptionBudget
#[derive(Debug, Clone)]
pub struct PdbInfo {
    /// PDB name
    pub name: String,
    /// Namespace
    pub namespace: String,
    /// Current number of healthy pods
    pub current_healthy: i32,
    /// Desired number of healthy pods (minAvailable)
    pub desired_healthy: i32,
    /// Number of disruptions allowed
    pub disruptions_allowed: i32,
    /// Expected pods (total matching selector)
    pub expected_pods: i32,
    /// Whether this PDB would block a drain
    pub would_block_drain: bool,
}

/// PDB health summary
#[derive(Debug, Clone, Default)]
pub struct PdbHealthInfo {
    /// All PDBs in the cluster
    pub pdbs: Vec<PdbInfo>,
    /// PDBs that would block drain (disruptions_allowed == 0)
    pub blocking_pdbs: Vec<PdbInfo>,
}

impl PdbHealthInfo {
    /// Check if any PDBs would block a drain
    pub fn has_blocking_pdbs(&self) -> bool {
        !self.blocking_pdbs.is_empty()
    }

    /// Get summary message
    pub fn summary(&self) -> String {
        if self.pdbs.is_empty() {
            "No PDBs configured".to_string()
        } else if self.blocking_pdbs.is_empty() {
            format!("{} PDBs, all allow disruption", self.pdbs.len())
        } else {
            format!(
                "{} PDBs, {} would block drain",
                self.pdbs.len(),
                self.blocking_pdbs.len()
            )
        }
    }
}

/// Check PodDisruptionBudgets across all namespaces
///
/// Identifies PDBs that would block a node drain operation.
pub async fn check_pdb_health(client: &Client) -> Result<PdbHealthInfo, K8sError> {
    let pdbs: Api<PodDisruptionBudget> = Api::all(client.clone());

    let pdb_list = pdbs
        .list(&ListParams::default())
        .await
        .map_err(|e| K8sError::ApiError(e.to_string()))?;

    let mut info = PdbHealthInfo::default();

    for pdb in pdb_list.items {
        let name = pdb.metadata.name.clone().unwrap_or_default();
        let namespace = pdb.metadata.namespace.clone().unwrap_or_default();
        let status = pdb.status.as_ref();

        let current_healthy = status.map(|s| s.current_healthy).unwrap_or(0);
        let desired_healthy = status.map(|s| s.desired_healthy).unwrap_or(0);
        let disruptions_allowed = status.map(|s| s.disruptions_allowed).unwrap_or(0);
        let expected_pods = status.map(|s| s.expected_pods).unwrap_or(0);

        // A PDB blocks drain if disruptions_allowed is 0 and there are expected pods
        let would_block_drain = disruptions_allowed == 0 && expected_pods > 0;

        let pdb_info = PdbInfo {
            name,
            namespace,
            current_healthy,
            desired_healthy,
            disruptions_allowed,
            expected_pods,
            would_block_drain,
        };

        if would_block_drain {
            info.blocking_pdbs.push(pdb_info.clone());
        }
        info.pdbs.push(pdb_info);
    }

    Ok(info)
}

// ==================== Node Operations ====================

/// Result of a cordon operation
#[derive(Debug, Clone)]
pub struct CordonResult {
    /// Node name
    pub node: String,
    /// Whether the operation succeeded
    pub success: bool,
    /// Error message if failed
    pub error: Option<String>,
}

/// Result of a drain operation
#[derive(Debug, Clone)]
pub struct DrainResult {
    /// Node name
    pub node: String,
    /// Whether the operation succeeded
    pub success: bool,
    /// Number of pods evicted
    pub pods_evicted: usize,
    /// Pods that failed to evict
    pub failed_pods: Vec<String>,
    /// Error message if failed
    pub error: Option<String>,
}

/// Cordon a node (mark as unschedulable)
pub async fn cordon_node(client: &Client, node_name: &str) -> Result<CordonResult, K8sError> {
    let nodes: Api<Node> = Api::all(client.clone());

    // Patch the node to set unschedulable = true
    let patch = json!({
        "spec": {
            "unschedulable": true
        }
    });

    match nodes
        .patch(
            node_name,
            &PatchParams::default(),
            &Patch::Merge(&patch),
        )
        .await
    {
        Ok(_) => Ok(CordonResult {
            node: node_name.to_string(),
            success: true,
            error: None,
        }),
        Err(e) => Ok(CordonResult {
            node: node_name.to_string(),
            success: false,
            error: Some(e.to_string()),
        }),
    }
}

/// Uncordon a node (mark as schedulable)
pub async fn uncordon_node(client: &Client, node_name: &str) -> Result<CordonResult, K8sError> {
    let nodes: Api<Node> = Api::all(client.clone());

    // Patch the node to set unschedulable = false
    let patch = json!({
        "spec": {
            "unschedulable": false
        }
    });

    match nodes
        .patch(
            node_name,
            &PatchParams::default(),
            &Patch::Merge(&patch),
        )
        .await
    {
        Ok(_) => Ok(CordonResult {
            node: node_name.to_string(),
            success: true,
            error: None,
        }),
        Err(e) => Ok(CordonResult {
            node: node_name.to_string(),
            success: false,
            error: Some(e.to_string()),
        }),
    }
}

/// Progress callback type for drain operations
pub type DrainProgressCallback = Box<dyn Fn(&str) + Send + Sync>;

/// Drain a node (evict all pods)
///
/// This will:
/// 1. Get all pods on the node
/// 2. Filter out DaemonSet pods and mirror pods
/// 3. Evict each pod, respecting PDBs
pub async fn drain_node(
    client: &Client,
    node_name: &str,
    ignore_daemonsets: bool,
    delete_emptydir_data: bool,
    _timeout_seconds: u64,
) -> Result<DrainResult, K8sError> {
    drain_node_with_progress(client, node_name, ignore_daemonsets, delete_emptydir_data, None).await
}

/// Drain a node with progress callback
pub async fn drain_node_with_progress(
    client: &Client,
    node_name: &str,
    ignore_daemonsets: bool,
    delete_emptydir_data: bool,
    progress_callback: Option<DrainProgressCallback>,
) -> Result<DrainResult, K8sError> {
    let pods: Api<Pod> = Api::all(client.clone());

    // List pods on this node
    let list_params = ListParams::default()
        .fields(&format!("spec.nodeName={}", node_name));

    let pod_list = pods
        .list(&list_params)
        .await
        .map_err(|e| K8sError::ApiError(format!("Failed to list pods: {}", e)))?;

    let mut pods_to_evict = Vec::new();
    let mut skipped_daemonset = 0;

    for pod in pod_list.items {
        let pod_name = pod.metadata.name.clone().unwrap_or_default();
        let namespace = pod.metadata.namespace.clone().unwrap_or_default();

        // Skip mirror pods (created by kubelet, not managed by API server)
        if let Some(annotations) = &pod.metadata.annotations {
            if annotations.contains_key("kubernetes.io/config.mirror") {
                continue;
            }
        }

        // Check if pod is owned by a DaemonSet
        let is_daemonset_pod = pod.metadata.owner_references.as_ref().map_or(false, |refs| {
            refs.iter().any(|r| r.kind == "DaemonSet")
        });

        if is_daemonset_pod {
            if ignore_daemonsets {
                skipped_daemonset += 1;
                continue;
            }
            // DaemonSet pods can't be evicted without ignoring them
        }

        // Check for local storage (emptyDir)
        let has_emptydir = pod.spec.as_ref().map_or(false, |spec| {
            spec.volumes.as_ref().map_or(false, |volumes| {
                volumes.iter().any(|v| v.empty_dir.is_some())
            })
        });

        if has_emptydir && !delete_emptydir_data {
            // Skip pods with emptyDir unless explicitly allowed
            continue;
        }

        pods_to_evict.push((namespace, pod_name));
    }

    let total_pods = pods_to_evict.len();
    let mut evicted = 0;
    let mut failed_pods = Vec::new();

    // Report initial count
    if let Some(ref cb) = progress_callback {
        cb(&format!("Found {} pods to evict", total_pods));
    }

    // Evict pods one at a time, respecting PDBs
    // PDBs may temporarily block eviction, so we retry with backoff
    for (idx, (namespace, pod_name)) in pods_to_evict.into_iter().enumerate() {
        let ns_pods: Api<Pod> = Api::namespaced(client.clone(), &namespace);
        let evict_params = EvictParams::default();

        // Report which pod we're evicting
        if let Some(ref cb) = progress_callback {
            cb(&format!("Evicting {}/{} ({}/{})", namespace, pod_name, idx + 1, total_pods));
        }

        let mut attempts = 0;
        let max_attempts = 5; // 5 attempts * 2 seconds = 10 seconds max per pod
        let mut success = false;

        while attempts < max_attempts && !success {
            match ns_pods.evict(&pod_name, &evict_params).await {
                Ok(_) => {
                    evicted += 1;
                    tracing::info!("Evicted pod {}/{}", namespace, pod_name);
                    success = true;

                    // Report success
                    if let Some(ref cb) = progress_callback {
                        cb(&format!("Evicted {}/{} ({}/{})", namespace, pod_name, evicted, total_pods));
                    }

                    // Wait briefly for pod to start terminating before next eviction
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }
                Err(e) => {
                    let err_str = e.to_string();

                    // Pod already gone - count as success
                    if err_str.contains("404") || err_str.contains("not found") {
                        evicted += 1;
                        success = true;
                        if let Some(ref cb) = progress_callback {
                            cb(&format!("Pod gone {}/{} ({}/{})", namespace, pod_name, evicted, total_pods));
                        }
                        continue;
                    }

                    // PDB blocking eviction - retry after delay
                    // Error codes: 429 (Too Many Requests) or message about disruption budget
                    if err_str.contains("429")
                        || err_str.contains("disruption budget")
                        || err_str.contains("PodDisruptionBudget")
                        || err_str.contains("Cannot evict")
                    {
                        attempts += 1;
                        tracing::debug!(
                            "PDB blocking eviction of {}/{}, attempt {}/{}",
                            namespace,
                            pod_name,
                            attempts,
                            max_attempts
                        );
                        // Report PDB wait
                        if let Some(ref cb) = progress_callback {
                            cb(&format!("Waiting for PDB: {}/{} (retry {}/{})", namespace, pod_name, attempts, max_attempts));
                        }
                        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                        continue;
                    }

                    // Other error - fail immediately
                    tracing::warn!("Failed to evict {}/{}: {}", namespace, pod_name, e);
                    failed_pods.push(format!("{}/{}", namespace, pod_name));
                    if let Some(ref cb) = progress_callback {
                        cb(&format!("Failed: {}/{}", namespace, pod_name));
                    }
                    break;
                }
            }
        }

        if !success && attempts >= max_attempts {
            tracing::warn!(
                "Timed out waiting for PDB to allow eviction of {}/{}",
                namespace,
                pod_name
            );
            failed_pods.push(format!("{}/{}", namespace, pod_name));
            if let Some(ref cb) = progress_callback {
                cb(&format!("Timeout: {}/{}", namespace, pod_name));
            }
        }
    }

    Ok(DrainResult {
        node: node_name.to_string(),
        success: failed_pods.is_empty(),
        pods_evicted: evicted,
        failed_pods,
        error: None,
    })
}
