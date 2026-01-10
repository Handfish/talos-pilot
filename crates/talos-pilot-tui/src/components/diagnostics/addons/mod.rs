//! Addon detection and diagnostics
//!
//! This module handles detecting and checking common Kubernetes addons:
//! - cert-manager
//! - external-secrets
//! - kyverno
//! - etc.
//!
//! Addon detection requires Kubernetes API access (Phase 3).

use super::types::DiagnosticCheck;

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

/// Detect which addons are installed (stub for Phase 3)
///
/// In Phase 3, this will query the Kubernetes API for:
/// - CRDs (certificates.cert-manager.io, etc.)
/// - Namespaces (cert-manager, external-secrets, etc.)
/// - Pods in kube-system
pub async fn detect_addons() -> DetectedAddons {
    // TODO: Implement in Phase 3 with kube crate
    DetectedAddons::default()
}

/// Run addon-specific checks (stub for Phase 3)
pub async fn run_addon_checks(_addons: &DetectedAddons) -> Vec<DiagnosticCheck> {
    // TODO: Implement in Phase 3
    Vec::new()
}
