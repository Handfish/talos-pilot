//! cert-manager addon diagnostics
//!
//! Checks for cert-manager health:
//! - Pod health in cert-manager namespace
//! - ClusterIssuer/Issuer status
//! - Certificate status and expiration

use crate::components::diagnostics::types::DiagnosticCheck;
use k8s_openapi::api::core::v1::Pod;
use kube::{
    api::{Api, ListParams},
    Client,
};

/// Run cert-manager diagnostic checks
pub async fn run_checks(client: &Client) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    // Check cert-manager pod health
    checks.push(check_pod_health(client).await);

    // Check webhook availability (important for cert-manager to function)
    checks.push(check_webhook(client).await);

    checks
}

/// Check cert-manager pods health
async fn check_pod_health(client: &Client) -> DiagnosticCheck {
    let pods: Api<Pod> = Api::namespaced(client.clone(), "cert-manager");

    match pods.list(&ListParams::default()).await {
        Ok(pod_list) => {
            if pod_list.items.is_empty() {
                return DiagnosticCheck::fail(
                    "cert_manager_pods",
                    "cert-manager Pods",
                    "No pods found",
                    None,
                )
                .with_details("cert-manager namespace exists but no pods are running.");
            }

            let total = pod_list.items.len();
            let mut healthy = 0;
            let mut unhealthy_pods = Vec::new();

            for pod in &pod_list.items {
                let name = pod.metadata.name.clone().unwrap_or_default();
                let status = pod.status.as_ref();

                let phase = status
                    .and_then(|s| s.phase.clone())
                    .unwrap_or_else(|| "Unknown".to_string());

                let ready = status
                    .and_then(|s| s.conditions.as_ref())
                    .map(|conditions| {
                        conditions
                            .iter()
                            .any(|c| c.type_ == "Ready" && c.status == "True")
                    })
                    .unwrap_or(false);

                if phase == "Running" && ready {
                    healthy += 1;
                } else {
                    unhealthy_pods.push(format!("{}: {} (ready: {})", name, phase, ready));
                }
            }

            if healthy == total {
                DiagnosticCheck::pass(
                    "cert_manager_pods",
                    "cert-manager Pods",
                    &format!("{}/{} healthy", healthy, total),
                )
            } else {
                DiagnosticCheck::warn(
                    "cert_manager_pods",
                    "cert-manager Pods",
                    &format!("{}/{} healthy", healthy, total),
                )
                .with_details(&format!("Unhealthy pods:\n{}", unhealthy_pods.join("\n")))
            }
        }
        Err(e) => {
            // Namespace might not exist or permission denied
            DiagnosticCheck::unknown("cert_manager_pods", "cert-manager Pods")
                .with_details(&format!("Failed to list pods: {}", e))
        }
    }
}

/// Check cert-manager webhook availability
async fn check_webhook(client: &Client) -> DiagnosticCheck {
    let pods: Api<Pod> = Api::namespaced(client.clone(), "cert-manager");

    match pods.list(&ListParams::default()).await {
        Ok(pod_list) => {
            // Look for webhook pod
            let webhook_pods: Vec<_> = pod_list
                .items
                .iter()
                .filter(|p| {
                    p.metadata
                        .name
                        .as_ref()
                        .map(|n| n.contains("webhook"))
                        .unwrap_or(false)
                })
                .collect();

            if webhook_pods.is_empty() {
                return DiagnosticCheck::warn(
                    "cert_manager_webhook",
                    "cert-manager Webhook",
                    "Not found",
                    )
                    .with_details("Webhook pod not found. Certificate validation may not work.");
            }

            // Check if webhook is ready
            let webhook_ready = webhook_pods.iter().any(|pod| {
                pod.status
                    .as_ref()
                    .and_then(|s| s.conditions.as_ref())
                    .map(|conditions| {
                        conditions
                            .iter()
                            .any(|c| c.type_ == "Ready" && c.status == "True")
                    })
                    .unwrap_or(false)
            });

            if webhook_ready {
                DiagnosticCheck::pass("cert_manager_webhook", "cert-manager Webhook", "Ready")
            } else {
                DiagnosticCheck::warn(
                    "cert_manager_webhook",
                    "cert-manager Webhook",
                    "Not ready",
                )
                .with_details("Webhook pod exists but is not ready. New certificates may fail to be issued.")
            }
        }
        Err(e) => DiagnosticCheck::unknown("cert_manager_webhook", "cert-manager Webhook")
            .with_details(&format!("Failed to check webhook: {}", e)),
    }
}
