//! Addon detection and diagnostics
//!
//! This module handles detecting and checking common Kubernetes addons:
//! - cert-manager
//! - external-secrets
//! - And more...
//!
//! Addon detection uses the Kubernetes API to check for CRDs, namespaces, and pods.

mod cert_manager;

use super::types::{DiagnosticCheck, DiagnosticContext};
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::{
    Client,
    api::{Api, ListParams},
};
use talos_pilot_core::constants::{
    ARGOCD_CRDS, CERT_MANAGER_CRDS, EXTERNAL_SECRETS_CRDS, FLUX_CRDS, KYVERNO_CRDS,
};

/// Detected addons in the cluster
#[derive(Debug, Clone, Default)]
pub struct DetectedAddons {
    pub cert_manager: bool,
    pub external_secrets: bool,
    pub kyverno: bool,
    pub ingress_nginx: bool,
    pub traefik: bool,
    pub prometheus: bool,
    pub argocd: bool,
    pub flux: bool,
}

impl DetectedAddons {
    /// Check if any addons are detected
    pub fn any_detected(&self) -> bool {
        self.cert_manager
            || self.external_secrets
            || self.kyverno
            || self.ingress_nginx
            || self.traefik
            || self.prometheus
            || self.argocd
            || self.flux
    }

    /// Get list of detected addon names
    pub fn detected_names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.cert_manager {
            names.push("cert-manager");
        }
        if self.external_secrets {
            names.push("external-secrets");
        }
        if self.kyverno {
            names.push("kyverno");
        }
        if self.ingress_nginx {
            names.push("ingress-nginx");
        }
        if self.traefik {
            names.push("traefik");
        }
        if self.prometheus {
            names.push("prometheus");
        }
        if self.argocd {
            names.push("argocd");
        }
        if self.flux {
            names.push("flux");
        }
        names
    }
}

/// Detect which addons are installed by checking CRDs and pods
pub async fn detect_addons(client: &Client) -> DetectedAddons {
    let mut addons = DetectedAddons::default();

    // Try to detect via CRDs first (most reliable)
    if let Ok(crds) = detect_addons_via_crds(client).await {
        addons = crds;
    }

    // Supplement with pod-based detection for addons without distinctive CRDs
    if let Ok(pod_addons) = detect_addons_via_pods(client).await {
        // Merge - pod detection can find ingress-nginx, traefik, prometheus
        if pod_addons.ingress_nginx {
            addons.ingress_nginx = true;
        }
        if pod_addons.traefik {
            addons.traefik = true;
        }
        if pod_addons.prometheus {
            addons.prometheus = true;
        }
    }

    tracing::info!("Detected addons: {:?}", addons.detected_names());
    addons
}

/// Detect addons by checking for their CRDs
async fn detect_addons_via_crds(client: &Client) -> Result<DetectedAddons, kube::Error> {
    let crds: Api<CustomResourceDefinition> = Api::all(client.clone());
    let crd_list = crds.list(&ListParams::default()).await?;

    let mut addons = DetectedAddons::default();

    let crd_names: Vec<String> = crd_list
        .items
        .iter()
        .filter_map(|crd| crd.metadata.name.clone())
        .collect();

    // Check for cert-manager
    if CERT_MANAGER_CRDS
        .iter()
        .any(|crd| crd_names.iter().any(|n| n == *crd))
    {
        addons.cert_manager = true;
    }

    // Check for external-secrets
    if EXTERNAL_SECRETS_CRDS
        .iter()
        .any(|crd| crd_names.iter().any(|n| n == *crd))
    {
        addons.external_secrets = true;
    }

    // Check for kyverno
    if KYVERNO_CRDS
        .iter()
        .any(|crd| crd_names.iter().any(|n| n == *crd))
    {
        addons.kyverno = true;
    }

    // Check for argocd
    if ARGOCD_CRDS
        .iter()
        .any(|crd| crd_names.iter().any(|n| n == *crd))
    {
        addons.argocd = true;
    }

    // Check for flux
    if FLUX_CRDS
        .iter()
        .any(|crd| crd_names.iter().any(|n| n == *crd))
    {
        addons.flux = true;
    }

    Ok(addons)
}

/// Detect addons by checking for pods (for addons without distinctive CRDs)
async fn detect_addons_via_pods(client: &Client) -> Result<DetectedAddons, kube::Error> {
    let mut addons = DetectedAddons::default();

    // Check common addon namespaces
    let namespaces = ["ingress-nginx", "traefik", "monitoring", "prometheus"];

    for ns in namespaces {
        let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
        if let Ok(pod_list) = pods.list(&ListParams::default().limit(1)).await
            && !pod_list.items.is_empty()
        {
            match ns {
                "ingress-nginx" => addons.ingress_nginx = true,
                "traefik" => addons.traefik = true,
                "monitoring" | "prometheus" => addons.prometheus = true,
                _ => {}
            }
        }
    }

    // Also check kube-system for ingress controllers
    let kube_system_pods: Api<Pod> = Api::namespaced(client.clone(), "kube-system");
    if let Ok(pod_list) = kube_system_pods.list(&ListParams::default()).await {
        for pod in pod_list.items {
            let name = pod.metadata.name.unwrap_or_default().to_lowercase();
            if name.contains("ingress-nginx") {
                addons.ingress_nginx = true;
            }
            if name.contains("traefik") {
                addons.traefik = true;
            }
        }
    }

    Ok(addons)
}

/// Run addon-specific diagnostic checks
pub async fn run_addon_checks(
    client: Option<&Client>,
    addons: &DetectedAddons,
    _ctx: &DiagnosticContext,
) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    let Some(k8s_client) = client else {
        // No K8s client - can't check addon health
        return checks;
    };

    // cert-manager checks
    if addons.cert_manager {
        checks.extend(cert_manager::run_checks(k8s_client).await);
    }

    // TODO: Add checks for other addons as needed
    // if addons.external_secrets {
    //     checks.extend(external_secrets::run_checks(k8s_client).await);
    // }

    checks
}
