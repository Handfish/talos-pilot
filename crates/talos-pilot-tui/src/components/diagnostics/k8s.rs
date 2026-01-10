//! Kubernetes client helper for diagnostics
//!
//! Creates a K8s client from Talos-provided kubeconfig.

use k8s_openapi::api::core::v1::Pod;
use kube::{
    api::{Api, ListParams},
    Client, Config,
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

            cni_info.pods.push(CniPodInfo {
                name,
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
