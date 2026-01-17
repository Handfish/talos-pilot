//! Kubernetes client helper for diagnostics
//!
//! Creates a K8s client from Talos-provided kubeconfig.

use k8s_openapi::api::core::v1::{Node, Pod};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use k8s_openapi::serde_json::json;
use kube::{
    Client, Config,
    api::{Api, EvictParams, ListParams, Patch, PatchParams},
};
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

/// Source of the kubeconfig used to create the K8s client
#[derive(Debug, Clone)]
pub enum KubeconfigSource {
    /// From KUBECONFIG environment variable or default path (~/.kube/config)
    Environment,
    /// Fetched from a specific Talos control plane node
    TalosNode(String),
    /// Source unknown or unavailable
    Unavailable(String),
}

/// Create a Kubernetes client from Talos-provided kubeconfig
///
/// If `kubeconfig_client` is provided, it will be used to fetch the kubeconfig.
/// This is useful when diagnosing worker nodes that don't have the kubeconfig endpoint.
pub async fn create_k8s_client(talos_client: &TalosClient) -> Result<Client, K8sError> {
    let (client, _source) = create_k8s_client_with_source(talos_client, None, None).await?;
    Ok(client)
}

/// Create a Kubernetes client and return the source of the kubeconfig
///
/// This function tries sources in this order:
/// 1. KUBECONFIG environment variable (via Config::infer())
/// 2. Fetching kubeconfig from a specific control plane node (if cp_node_ip provided)
/// 3. Fetching kubeconfig from Talos API via VIP (fallback, may fail with multiple nodes)
///
/// # Arguments
/// * `talos_client` - The main Talos client (connected to VIP or endpoint)
/// * `cp_node_ip` - Optional control plane node IP to target for kubeconfig fetch
/// * `kubeconfig_client` - Optional pre-configured client targeting a control plane node
///
/// # Returns
/// A tuple of (Client, KubeconfigSource) on success
pub async fn create_k8s_client_with_source(
    talos_client: &TalosClient,
    cp_node_ip: Option<&str>,
    kubeconfig_client: Option<&TalosClient>,
) -> Result<(Client, KubeconfigSource), K8sError> {
    // Try KUBECONFIG environment variable first (via Config::infer())
    // This respects standard K8s tooling conventions
    if let Ok(config) = Config::infer().await
        && let Ok(client) = Client::try_from(config)
    {
        tracing::debug!("Using kubeconfig from environment (KUBECONFIG or default path)");
        return Ok((client, KubeconfigSource::Environment));
    }

    tracing::debug!("KUBECONFIG not available, falling back to Talos API");

    // If a specific control plane node IP is provided, target that node
    if let Some(node_ip) = cp_node_ip {
        tracing::debug!("Targeting control plane node {} for kubeconfig", node_ip);
        let node_client = talos_client.with_node(node_ip);
        match fetch_kubeconfig_from_client(&node_client).await {
            Ok(client) => {
                return Ok((client, KubeconfigSource::TalosNode(node_ip.to_string())));
            }
            Err(e) => {
                tracing::warn!("Failed to fetch kubeconfig from node {}: {}", node_ip, e);
                // Fall through to try other methods
            }
        }
    }

    // Try the provided kubeconfig_client if available
    if let Some(kc_client) = kubeconfig_client {
        tracing::debug!("Trying provided kubeconfig client");
        match fetch_kubeconfig_from_client(kc_client).await {
            Ok(client) => {
                return Ok((
                    client,
                    KubeconfigSource::TalosNode("control-plane".to_string()),
                ));
            }
            Err(e) => {
                tracing::warn!("Failed to fetch kubeconfig from provided client: {}", e);
            }
        }
    }

    // Last resort: try the main client (may fail with multiple nodes configured)
    tracing::debug!("Trying main Talos client for kubeconfig (may fail with multiple nodes)");
    match fetch_kubeconfig_from_client(talos_client).await {
        Ok(client) => Ok((client, KubeconfigSource::TalosNode("vip".to_string()))),
        Err(e) => Err(e),
    }
}

/// Fetch kubeconfig from a specific Talos client and create a K8s client
async fn fetch_kubeconfig_from_client(client: &TalosClient) -> Result<Client, K8sError> {
    // Get kubeconfig from Talos
    let kubeconfig_yaml = client
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

/// Create a Kubernetes client, optionally using a different client to fetch kubeconfig
///
/// This allows fetching kubeconfig from a control plane node while diagnosing a worker node.
/// Legacy function - prefer create_k8s_client_with_source for new code.
pub async fn create_k8s_client_with_kubeconfig_source(
    talos_client: &TalosClient,
    kubeconfig_client: Option<&TalosClient>,
) -> Result<Client, K8sError> {
    let (client, _source) =
        create_k8s_client_with_source(talos_client, None, kubeconfig_client).await?;
    Ok(client)
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
        let is_cni_pod =
            if name_lower.starts_with("kube-flannel") || name_lower.starts_with("flannel") {
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
                .map(|containers| containers.iter().map(|c| c.restart_count).sum())
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

    info.pods
        .iter()
        .all(|pod| pod.phase == "Running" && pod.ready)
}

/// Get summary of CNI pod health
pub fn cni_pod_health_summary(info: &CniInfo) -> String {
    if info.pods.is_empty() {
        return "No CNI pods found".to_string();
    }

    let total = info.pods.len();
    let healthy = info
        .pods
        .iter()
        .filter(|p| p.phase == "Running" && p.ready)
        .count();
    let total_restarts: i32 = info.pods.iter().map(|p| p.restart_count).sum();

    if healthy == total && total_restarts == 0 {
        format!("{}/{} pods healthy", healthy, total)
    } else if healthy == total {
        format!(
            "{}/{} pods healthy ({} restarts)",
            healthy, total, total_restarts
        )
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
        if self.crashing.is_empty() && self.image_pull_errors.is_empty() && self.pending.is_empty()
        {
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
                        "ImagePullBackOff" | "ErrImagePull" => {
                            info.image_pull_errors.push(pod_info)
                        }
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
    /// Pods that were force-deleted
    pub force_deleted_pods: Vec<String>,
    /// Error message if failed
    pub error: Option<String>,
}

/// Options for drain and reboot operations
#[derive(Debug, Clone)]
pub struct DrainOptions {
    /// Timeout per pod for PDB retry (seconds)
    pub per_pod_timeout_secs: u64,
    /// Grace period for pod termination (None = use pod's default)
    pub grace_period_secs: Option<i64>,
    /// Force delete pods that can't be evicted (unmanaged pods)
    pub force_delete_unmanaged: bool,
    /// Ignore DaemonSet pods
    pub ignore_daemonsets: bool,
    /// Delete pods with emptyDir volumes
    pub delete_emptydir_data: bool,
    /// Wait for node to become Ready after reboot
    pub wait_for_node_ready: bool,
    /// Timeout for waiting for node to become Ready (seconds)
    pub post_reboot_timeout_secs: u64,
    /// Uncordon node after it becomes Ready
    pub uncordon_after_reboot: bool,
}

impl Default for DrainOptions {
    fn default() -> Self {
        Self {
            per_pod_timeout_secs: 30,
            grace_period_secs: None, // Use pod's default
            force_delete_unmanaged: false,
            ignore_daemonsets: true,
            delete_emptydir_data: true,
            wait_for_node_ready: true,     // Safe default for production
            post_reboot_timeout_secs: 300, // 5 minutes
            uncordon_after_reboot: true,   // Auto-uncordon when healthy
        }
    }
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
        .patch(node_name, &PatchParams::default(), &Patch::Merge(&patch))
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
        .patch(node_name, &PatchParams::default(), &Patch::Merge(&patch))
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

/// Information about a pod to evict
#[derive(Debug, Clone)]
struct PodToEvict {
    namespace: String,
    name: String,
    /// Whether this pod is managed by a controller (ReplicaSet, Deployment, etc.)
    is_managed: bool,
}

/// Drain a node (evict all pods)
///
/// This will:
/// 1. Get all pods on the node
/// 2. Filter out DaemonSet pods and mirror pods
/// 3. Evict each pod, respecting PDBs
pub async fn drain_node(
    client: &Client,
    node_name: &str,
    options: &DrainOptions,
) -> Result<DrainResult, K8sError> {
    drain_node_with_progress(client, node_name, options, None).await
}

/// Drain a node with progress callback
pub async fn drain_node_with_progress(
    client: &Client,
    node_name: &str,
    options: &DrainOptions,
    progress_callback: Option<DrainProgressCallback>,
) -> Result<DrainResult, K8sError> {
    use kube::api::DeleteParams;

    let pods: Api<Pod> = Api::all(client.clone());

    // List pods on this node
    let list_params = ListParams::default().fields(&format!("spec.nodeName={}", node_name));

    let pod_list = pods
        .list(&list_params)
        .await
        .map_err(|e| K8sError::ApiError(format!("Failed to list pods: {}", e)))?;

    let mut pods_to_evict = Vec::new();
    let mut _skipped_daemonset = 0;

    for pod in pod_list.items {
        let pod_name = pod.metadata.name.clone().unwrap_or_default();
        let namespace = pod.metadata.namespace.clone().unwrap_or_default();

        // Skip mirror pods (created by kubelet, not managed by API server)
        if let Some(annotations) = &pod.metadata.annotations
            && annotations.contains_key("kubernetes.io/config.mirror")
        {
            continue;
        }

        // Check if pod is owned by a controller (managed)
        let owner_refs = pod.metadata.owner_references.as_ref();
        let is_daemonset_pod =
            owner_refs.is_some_and(|refs| refs.iter().any(|r| r.kind == "DaemonSet"));
        let is_managed = owner_refs.is_some_and(|refs| {
            refs.iter().any(|r| {
                matches!(
                    r.kind.as_str(),
                    "ReplicaSet" | "Deployment" | "StatefulSet" | "Job" | "DaemonSet"
                )
            })
        });

        if is_daemonset_pod && options.ignore_daemonsets {
            _skipped_daemonset += 1;
            continue;
        }
        // DaemonSet pods can't be evicted without ignoring them

        // Check for local storage (emptyDir)
        let has_emptydir = pod.spec.as_ref().is_some_and(|spec| {
            spec.volumes
                .as_ref()
                .is_some_and(|volumes| volumes.iter().any(|v| v.empty_dir.is_some()))
        });

        if has_emptydir && !options.delete_emptydir_data {
            // Skip pods with emptyDir unless explicitly allowed
            continue;
        }

        pods_to_evict.push(PodToEvict {
            namespace,
            name: pod_name,
            is_managed,
        });
    }

    let total_pods = pods_to_evict.len();
    let mut evicted = 0;
    let mut failed_pods = Vec::new();
    let mut force_deleted_pods = Vec::new();

    // Report initial count
    if let Some(ref cb) = progress_callback {
        cb(&format!("Found {} pods to evict", total_pods));
    }

    // Calculate max attempts based on configurable timeout
    // Each attempt waits 2 seconds, so max_attempts = timeout / 2
    let max_attempts = (options.per_pod_timeout_secs / 2).max(1) as usize;

    // Eviction uses the pod's configured grace period by default.
    // Custom grace_period_secs is only applied to force-delete operations.
    let evict_params = EvictParams::default();

    // Evict pods one at a time, respecting PDBs
    // PDBs may temporarily block eviction, so we retry with backoff
    for (idx, pod_info) in pods_to_evict.into_iter().enumerate() {
        let namespace = &pod_info.namespace;
        let pod_name = &pod_info.name;
        let ns_pods: Api<Pod> = Api::namespaced(client.clone(), namespace);

        // Report which pod we're evicting
        if let Some(ref cb) = progress_callback {
            cb(&format!(
                "Evicting {}/{} ({}/{})",
                namespace,
                pod_name,
                idx + 1,
                total_pods
            ));
        }

        let mut attempts = 0;
        let mut success = false;

        while attempts < max_attempts && !success {
            match ns_pods.evict(pod_name, &evict_params).await {
                Ok(_) => {
                    evicted += 1;
                    tracing::info!("Evicted pod {}/{}", namespace, pod_name);
                    success = true;

                    // Report success
                    if let Some(ref cb) = progress_callback {
                        cb(&format!(
                            "Evicted {}/{} ({}/{})",
                            namespace, pod_name, evicted, total_pods
                        ));
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
                            cb(&format!(
                                "Pod gone {}/{} ({}/{})",
                                namespace, pod_name, evicted, total_pods
                            ));
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
                            cb(&format!(
                                "Waiting for PDB: {}/{} (retry {}/{})",
                                namespace, pod_name, attempts, max_attempts
                            ));
                        }
                        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                        continue;
                    }

                    // Other error - check if we can force delete
                    tracing::warn!("Failed to evict {}/{}: {}", namespace, pod_name, e);

                    // Try force delete if enabled and pod is unmanaged
                    if options.force_delete_unmanaged && !pod_info.is_managed {
                        if let Some(ref cb) = progress_callback {
                            cb(&format!(
                                "Force deleting unmanaged pod {}/{}",
                                namespace, pod_name
                            ));
                        }

                        let delete_params = if let Some(grace) = options.grace_period_secs {
                            DeleteParams::default().grace_period(grace as u32)
                        } else {
                            DeleteParams::default().grace_period(0)
                        };

                        match ns_pods.delete(pod_name, &delete_params).await {
                            Ok(_) => {
                                evicted += 1;
                                force_deleted_pods.push(format!("{}/{}", namespace, pod_name));
                                tracing::info!(
                                    "Force deleted unmanaged pod {}/{}",
                                    namespace,
                                    pod_name
                                );
                                success = true;
                                if let Some(ref cb) = progress_callback {
                                    cb(&format!(
                                        "Force deleted {}/{} ({}/{})",
                                        namespace, pod_name, evicted, total_pods
                                    ));
                                }
                            }
                            Err(del_err) => {
                                tracing::warn!(
                                    "Force delete failed for {}/{}: {}",
                                    namespace,
                                    pod_name,
                                    del_err
                                );
                                failed_pods.push(format!("{}/{}", namespace, pod_name));
                                if let Some(ref cb) = progress_callback {
                                    cb(&format!("Failed: {}/{}", namespace, pod_name));
                                }
                            }
                        }
                    } else {
                        failed_pods.push(format!("{}/{}", namespace, pod_name));
                        if let Some(ref cb) = progress_callback {
                            cb(&format!("Failed: {}/{}", namespace, pod_name));
                        }
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

            // Try force delete on timeout if enabled and pod is unmanaged
            if options.force_delete_unmanaged && !pod_info.is_managed {
                if let Some(ref cb) = progress_callback {
                    cb(&format!(
                        "Force deleting unmanaged pod {}/{} after timeout",
                        namespace, pod_name
                    ));
                }

                let delete_params = if let Some(grace) = options.grace_period_secs {
                    DeleteParams::default().grace_period(grace as u32)
                } else {
                    DeleteParams::default().grace_period(0)
                };

                match ns_pods.delete(pod_name, &delete_params).await {
                    Ok(_) => {
                        evicted += 1;
                        force_deleted_pods.push(format!("{}/{}", namespace, pod_name));
                        tracing::info!(
                            "Force deleted unmanaged pod {}/{} after timeout",
                            namespace,
                            pod_name
                        );
                        if let Some(ref cb) = progress_callback {
                            cb(&format!(
                                "Force deleted {}/{} ({}/{})",
                                namespace, pod_name, evicted, total_pods
                            ));
                        }
                    }
                    Err(del_err) => {
                        tracing::warn!(
                            "Force delete failed for {}/{}: {}",
                            namespace,
                            pod_name,
                            del_err
                        );
                        failed_pods.push(format!("{}/{}", namespace, pod_name));
                        if let Some(ref cb) = progress_callback {
                            cb(&format!("Timeout: {}/{}", namespace, pod_name));
                        }
                    }
                }
            } else {
                failed_pods.push(format!("{}/{}", namespace, pod_name));
                if let Some(ref cb) = progress_callback {
                    cb(&format!("Timeout: {}/{}", namespace, pod_name));
                }
            }
        }
    }

    Ok(DrainResult {
        node: node_name.to_string(),
        success: failed_pods.is_empty(),
        pods_evicted: evicted,
        failed_pods,
        force_deleted_pods,
        error: None,
    })
}

// ==================== Post-Operation Verification ====================

/// Result of waiting for a node to become ready
#[derive(Debug, Clone)]
pub struct NodeReadyResult {
    /// Whether the node became ready
    pub success: bool,
    /// Time taken to become ready (seconds)
    pub time_taken_secs: u64,
    /// Error message if failed
    pub error: Option<String>,
}

/// Node condition status from Kubernetes
#[derive(Debug, Clone, PartialEq)]
pub enum NodeConditionStatus {
    Ready,
    NotReady,
    Unknown,
}

/// Get the current ready status of a node
pub async fn get_node_ready_status(
    client: &Client,
    node_name: &str,
) -> Result<NodeConditionStatus, K8sError> {
    let nodes: Api<Node> = Api::all(client.clone());

    let node = nodes
        .get(node_name)
        .await
        .map_err(|e| K8sError::ApiError(format!("Failed to get node: {}", e)))?;

    let conditions = node.status.as_ref().and_then(|s| s.conditions.as_ref());

    if let Some(conditions) = conditions {
        for condition in conditions {
            if condition.type_ == "Ready" {
                return match condition.status.as_str() {
                    "True" => Ok(NodeConditionStatus::Ready),
                    "False" => Ok(NodeConditionStatus::NotReady),
                    _ => Ok(NodeConditionStatus::Unknown),
                };
            }
        }
    }

    Ok(NodeConditionStatus::Unknown)
}

/// Check if a node exists and is reachable in the cluster
pub async fn node_exists(client: &Client, node_name: &str) -> bool {
    let nodes: Api<Node> = Api::all(client.clone());
    nodes.get(node_name).await.is_ok()
}

/// Progress callback for node readiness waiting
pub type NodeReadyProgressCallback = Box<dyn Fn(&str) + Send + Sync>;

/// Wait for a node to become Ready after reboot
///
/// This function:
/// 1. Optionally waits for the node to disappear (confirm reboot started)
/// 2. Waits for the node to reappear
/// 3. Waits for the node condition to become Ready
pub async fn wait_for_node_ready(
    client: &Client,
    node_name: &str,
    timeout_secs: u64,
    wait_for_disconnect: bool,
    progress_callback: Option<NodeReadyProgressCallback>,
) -> Result<NodeReadyResult, K8sError> {
    let start = std::time::Instant::now();
    let poll_interval = tokio::time::Duration::from_secs(5);

    // Phase 1: Wait for node to disconnect (if requested)
    if wait_for_disconnect {
        if let Some(ref cb) = progress_callback {
            cb("Waiting for node to begin rebooting...");
        }

        // Wait up to 60 seconds for the node to disconnect
        let disconnect_timeout = 60;
        let disconnect_start = std::time::Instant::now();
        let mut disconnected = false;

        while disconnect_start.elapsed().as_secs() < disconnect_timeout {
            match get_node_ready_status(client, node_name).await {
                Ok(NodeConditionStatus::NotReady) | Ok(NodeConditionStatus::Unknown) => {
                    disconnected = true;
                    if let Some(ref cb) = progress_callback {
                        cb("Node is rebooting...");
                    }
                    break;
                }
                Err(_) => {
                    // API error might mean node is disconnecting
                    disconnected = true;
                    if let Some(ref cb) = progress_callback {
                        cb("Node is rebooting...");
                    }
                    break;
                }
                Ok(NodeConditionStatus::Ready) => {
                    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                }
            }
        }

        if !disconnected {
            // Node never went down - might be a problem, but continue waiting
            if let Some(ref cb) = progress_callback {
                cb("Warning: Node didn't appear to disconnect, continuing to wait...");
            }
        }
    }

    // Phase 2: Wait for node to become Ready
    if let Some(ref cb) = progress_callback {
        cb("Waiting for node to come back online...");
    }

    loop {
        let elapsed = start.elapsed().as_secs();

        if elapsed >= timeout_secs {
            return Ok(NodeReadyResult {
                success: false,
                time_taken_secs: elapsed,
                error: Some(format!(
                    "Timed out waiting for node after {}s",
                    timeout_secs
                )),
            });
        }

        // Check node status
        match get_node_ready_status(client, node_name).await {
            Ok(NodeConditionStatus::Ready) => {
                if let Some(ref cb) = progress_callback {
                    cb(&format!("Node is Ready (took {}s)", elapsed));
                }
                return Ok(NodeReadyResult {
                    success: true,
                    time_taken_secs: elapsed,
                    error: None,
                });
            }
            Ok(status) => {
                let remaining = timeout_secs - elapsed;
                if let Some(ref cb) = progress_callback {
                    let status_str = match status {
                        NodeConditionStatus::NotReady => "NotReady",
                        NodeConditionStatus::Unknown => "Unknown",
                        _ => "Unknown",
                    };
                    cb(&format!(
                        "Node status: {} ({}s remaining)",
                        status_str, remaining
                    ));
                }
            }
            Err(_) => {
                let remaining = timeout_secs - elapsed;
                if let Some(ref cb) = progress_callback {
                    cb(&format!(
                        "Waiting for node to rejoin cluster ({}s remaining)",
                        remaining
                    ));
                }
            }
        }

        tokio::time::sleep(poll_interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;
    use tokio::sync::Mutex;

    static ENV_MUTEX: Mutex<()> = Mutex::const_new(());

    /// Test that KUBECONFIG environment variable is respected.
    ///
    /// This test creates a valid kubeconfig file pointing to a non-existent cluster.
    /// If KUBECONFIG is respected, Config::infer() will find it and try to use it,
    /// resulting in a connection error (not a kubeconfig fetch error from Talos).
    ///
    /// Note: This test manipulates environment variables and must be run serially.
    #[tokio::test]
    async fn test_kubeconfig_env_is_tried_first() {
        let _guard = ENV_MUTEX.lock().await;

        // Create a valid kubeconfig file pointing to a non-existent cluster
        let kubeconfig_content = r#"
apiVersion: v1
kind: Config
clusters:
- cluster:
    server: https://127.0.0.1:64321
    insecure-skip-tls-verify: true
  name: test-cluster
contexts:
- context:
    cluster: test-cluster
    user: test-user
  name: test-context
current-context: test-context
users:
- name: test-user
  user:
    token: test-token
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(kubeconfig_content.as_bytes()).unwrap();
        let kubeconfig_path = temp_file.path().to_string_lossy().to_string();

        // Set KUBECONFIG to our temp file
        let old_kubeconfig = std::env::var("KUBECONFIG").ok();
        unsafe {
            std::env::set_var("KUBECONFIG", &kubeconfig_path);
        }

        // Try to infer config - this should succeed in finding the kubeconfig
        // (it will fail to connect, but that's expected)
        let config_result = Config::infer().await;

        // Restore original KUBECONFIG
        unsafe {
            if let Some(old_value) = old_kubeconfig {
                std::env::set_var("KUBECONFIG", old_value);
            } else {
                std::env::remove_var("KUBECONFIG");
            }
        }

        // The config should have been loaded (even though connection would fail)
        assert!(
            config_result.is_ok(),
            "Config::infer() should find our KUBECONFIG file"
        );

        // Verify it points to our test cluster
        let config = config_result.unwrap();
        assert_eq!(
            config.cluster_url.to_string(),
            "https://127.0.0.1:64321/",
            "Should use the cluster URL from our kubeconfig"
        );
    }

    /// Test that when KUBECONFIG points to a non-existent file, it falls back gracefully.
    ///
    /// Note: This test can't fully verify the Talos fallback without a real Talos client,
    /// but it verifies that the function handles missing KUBECONFIG correctly.
    #[tokio::test]
    async fn test_kubeconfig_fallback_on_missing_file() {
        let _guard = ENV_MUTEX.lock().await;

        // Set KUBECONFIG to a non-existent file
        let old_kubeconfig = std::env::var("KUBECONFIG").ok();
        unsafe {
            std::env::set_var("KUBECONFIG", "/non/existent/path/kubeconfig");
        }

        // Try to infer config - this should fail
        let config_result = Config::infer().await;

        // Restore original KUBECONFIG
        unsafe {
            if let Some(old_value) = old_kubeconfig {
                std::env::set_var("KUBECONFIG", old_value);
            } else {
                std::env::remove_var("KUBECONFIG");
            }
        }

        // Config::infer() should fail when kubeconfig doesn't exist
        // This triggers the fallback to Talos API in create_k8s_client_with_kubeconfig_source
        assert!(
            config_result.is_err(),
            "Config::infer() should fail with non-existent kubeconfig"
        );
    }
}
