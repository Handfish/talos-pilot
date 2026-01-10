//! CNI detection and diagnostics
//!
//! This module handles:
//! 1. Auto-detecting which CNI is installed (Flannel, Cilium, Calico)
//! 2. Running CNI-specific diagnostic checks
//! 3. Providing CNI-specific fixes

mod flannel;

pub use flannel::run_flannel_checks;

use super::types::{CniType, DiagnosticCheck, DiagnosticContext};
use talos_rs::TalosClient;

/// Detect which CNI is installed in the cluster
///
/// For now, we detect based on kubelet logs and known patterns.
/// In Phase 2, we'll use the Kubernetes API to check pods in kube-system.
pub async fn detect_cni(client: &TalosClient) -> CniType {
    // Try to detect CNI from kubelet logs
    match client.logs("kubelet", 200).await {
        Ok(logs) => {
            // Check for Flannel indicators
            if logs.contains("flannel") || logs.contains("subnet.env") {
                return CniType::Flannel;
            }

            // Check for Cilium indicators
            if logs.contains("cilium") {
                return CniType::Cilium;
            }

            // Check for Calico indicators
            if logs.contains("calico") || logs.contains("felix") {
                return CniType::Calico;
            }

            CniType::Unknown
        }
        Err(_) => CniType::Unknown,
    }
}

/// Run CNI-specific diagnostic checks based on detected CNI type
pub async fn run_cni_checks(
    client: &TalosClient,
    ctx: &DiagnosticContext,
) -> Vec<DiagnosticCheck> {
    match ctx.cni_type {
        CniType::Flannel => flannel::run_flannel_checks(client, ctx).await,
        CniType::Cilium => run_cilium_checks(client, ctx).await,
        CniType::Calico => run_calico_checks(client, ctx).await,
        CniType::Unknown | CniType::None => run_generic_cni_checks(client, ctx).await,
    }
}

/// Generic CNI checks when we don't know the CNI type
async fn run_generic_cni_checks(
    client: &TalosClient,
    _ctx: &DiagnosticContext,
) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    // Just check if CNI is working at a basic level
    let (cni_ok, error) = super::core::check_cni_health(client).await;

    if cni_ok {
        checks.push(DiagnosticCheck::pass("cni", "CNI", "OK"));
    } else {
        checks.push(
            DiagnosticCheck::fail("cni", "CNI", "Network setup failed", None)
                .with_details(&error.unwrap_or_else(|| "Unknown error".to_string())),
        );
    }

    checks
}

/// Cilium-specific checks (stub for Phase 2)
async fn run_cilium_checks(
    _client: &TalosClient,
    _ctx: &DiagnosticContext,
) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    // TODO: Implement Cilium-specific checks in Phase 2
    // - Cilium agent health
    // - BPF filesystem mounted
    // - Cilium connectivity test
    // - No br_netfilter requirement (eBPF handles it)

    checks.push(DiagnosticCheck::pass(
        "cni",
        "CNI (Cilium)",
        "Detected (checks coming soon)",
    ));

    checks
}

/// Calico-specific checks (stub for Phase 2)
async fn run_calico_checks(
    _client: &TalosClient,
    _ctx: &DiagnosticContext,
) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    // TODO: Implement Calico-specific checks in Phase 2
    // - Felix health
    // - BGP peers (if using BGP mode)
    // - br_netfilter (depends on datapath mode)

    checks.push(DiagnosticCheck::pass(
        "cni",
        "CNI (Calico)",
        "Detected (checks coming soon)",
    ));

    checks
}
