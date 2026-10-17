//! Cilium CNI diagnostics
//!
//! Cilium-specific checks:
//! - Per-node Cilium agent status (DaemonSet pods)
//! - Cilium Operator deployment status
//! - Hubble Relay deployment status (if enabled)
//! - Cilium eBPF mode detection (kube-proxy replacement)
//! - Cilium + KubeSpan compatibility warning
//!
//! Philosophy: Check actual K8s API state, not logs. The pod status
//! and readiness conditions are the definitive indicators of health.

use crate::components::diagnostics::types::{
    CniInfo, CniPodInfo, DiagnosticCheck, DiagnosticContext,
};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Pod;
use kube::{
    Client,
    api::{Api, ListParams},
};
use talos_rs::is_kubespan_enabled_for_node;

/// Run Cilium-specific diagnostic checks
pub async fn run_cilium_checks(
    ctx: &DiagnosticContext,
    k8s_client: Option<&Client>,
) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    // Check per-node Cilium agent health if we have K8s API info
    if let Some(ref cni_info) = ctx.cni_info {
        checks.push(check_cilium_agents_per_node(cni_info));
    }

    // Check Cilium Operator and Hubble if we have K8s client
    if let Some(client) = k8s_client {
        // Check Cilium Operator
        checks.push(check_cilium_operator(client).await);

        // Check Hubble Relay (only if it exists)
        if let Some(hubble_check) = check_hubble_relay(client).await {
            checks.push(hubble_check);
        }

        // Check Cilium eBPF + KubeSpan compatibility
        if let Some(compat_check) = check_kubespan_compatibility(ctx, client).await {
            checks.push(compat_check);
        }
    }

    // Add overall CNI status check
    let overall_status = determine_overall_status(&checks, ctx);
    checks.push(overall_status);

    checks
}

/// Check per-node Cilium agent status
///
/// Shows the health of Cilium agent pods on each node.
/// This is critical for understanding which nodes have networking issues.
fn check_cilium_agents_per_node(cni_info: &CniInfo) -> DiagnosticCheck {
    // Filter to only cilium-agent pods (not operator, hubble, etc.)
    let agent_pods: Vec<&CniPodInfo> = cni_info
        .pods
        .iter()
        .filter(|p| {
            let name = p.name.to_lowercase();
            // Match cilium agent pods, exclude operator/hubble
            name.starts_with("cilium-")
                && !name.contains("operator")
                && !name.contains("hubble")
                && !name.contains("envoy")
        })
        .collect();

    if agent_pods.is_empty() {
        return DiagnosticCheck::warn("cilium_agents", "Cilium Agents", "No agent pods found")
            .with_details(
                "Could not find Cilium agent pods in kube-system namespace.\n\
                       Expected pods matching 'cilium-*' (DaemonSet).",
            );
    }

    let total = agent_pods.len();
    let healthy: Vec<_> = agent_pods
        .iter()
        .filter(|p| p.phase == "Running" && p.ready)
        .collect();
    let unhealthy: Vec<_> = agent_pods
        .iter()
        .filter(|p| p.phase != "Running" || !p.ready)
        .collect();

    let total_restarts: i32 = agent_pods.iter().map(|p| p.restart_count).sum();

    if unhealthy.is_empty() {
        // All healthy
        let message = if total_restarts > 0 {
            format!(
                "{}/{} agents ready ({} restarts)",
                healthy.len(),
                total,
                total_restarts
            )
        } else {
            format!("{}/{} agents ready", healthy.len(), total)
        };

        let details = format_agent_details(&agent_pods);
        DiagnosticCheck::pass("cilium_agents", "Cilium Agents", &message).with_details(&details)
    } else {
        // Some unhealthy
        let message = format!("{}/{} agents ready", healthy.len(), total);
        let details = format_agent_details_with_issues(&agent_pods, &unhealthy);

        DiagnosticCheck::fail("cilium_agents", "Cilium Agents", &message, None)
            .with_details(&details)
    }
}

/// Format per-node agent details for healthy clusters
fn format_agent_details(pods: &[&CniPodInfo]) -> String {
    let mut lines = vec!["Per-node status:".to_string()];

    for pod in pods {
        let node = pod.node_name.as_deref().unwrap_or("unknown");
        let status = if pod.phase == "Running" && pod.ready {
            "Ready"
        } else {
            &pod.phase
        };
        let restarts = if pod.restart_count > 0 {
            format!(" ({} restarts)", pod.restart_count)
        } else {
            String::new()
        };
        lines.push(format!("  {} on {}{}", status, node, restarts));
    }

    lines.join("\n")
}

/// Format per-node agent details highlighting issues
fn format_agent_details_with_issues(
    all_pods: &[&CniPodInfo],
    unhealthy: &[&&CniPodInfo],
) -> String {
    let mut lines = vec!["Per-node status:".to_string()];

    // Show unhealthy first
    lines.push("Unhealthy:".to_string());
    for pod in unhealthy {
        let node = pod.node_name.as_deref().unwrap_or("unknown");
        let status = format!("{} (ready: {})", pod.phase, pod.ready);
        lines.push(format!("  {} on {} - {}", pod.name, node, status));
    }

    // Then healthy
    let healthy_count = all_pods.len() - unhealthy.len();
    if healthy_count > 0 {
        lines.push(format!("\nHealthy: {} nodes", healthy_count));
    }

    lines.push("\nTip: Check agent logs with:".to_string());
    lines.push("  kubectl logs -n kube-system -l k8s-app=cilium".to_string());

    lines.join("\n")
}

/// Check Cilium Operator deployment status
///
/// Note: Cilium operator defaults to 2 replicas for HA, but on single-node
/// clusters only 1 can run (due to pod anti-affinity). We consider this OK
/// if at least 1 replica is available and functional.
async fn check_cilium_operator(client: &Client) -> DiagnosticCheck {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), "kube-system");

    // Try cilium-operator first, then cilium-operator-generic
    let operator_names = ["cilium-operator", "cilium-operator-generic"];

    for name in operator_names {
        match deployments.get(name).await {
            Ok(deployment) => {
                let status = deployment.status.as_ref();
                let replicas = status.and_then(|s| s.replicas).unwrap_or(0);
                let ready = status.and_then(|s| s.ready_replicas).unwrap_or(0);
                let available = status.and_then(|s| s.available_replicas).unwrap_or(0);

                // Check for pending pods to detect single-node scheduling constraints
                let unavailable = status.and_then(|s| s.unavailable_replicas).unwrap_or(0);

                if ready > 0 && ready == replicas {
                    // All replicas ready - perfect
                    return DiagnosticCheck::pass(
                        "cilium_operator",
                        "Cilium Operator",
                        &format!("{}/{} ready", ready, replicas),
                    );
                } else if available >= 1 && unavailable > 0 {
                    // At least one replica is available and functional.
                    // The unavailable ones are likely pending due to pod anti-affinity
                    // on a cluster with fewer nodes than desired replicas - this is OK.
                    return DiagnosticCheck::pass(
                        "cilium_operator",
                        "Cilium Operator",
                        &format!("{}/{} ready (HA limited)", ready, replicas),
                    ).with_details(
                        &format!("{} replica(s) pending - likely due to pod anti-affinity.\n\
                                 This is normal on clusters with fewer nodes than operator replicas.\n\
                                 The operator is functional with {} available replica(s).",
                                unavailable, available)
                    );
                } else if available > 0 {
                    // Available but something else going on
                    return DiagnosticCheck::warn(
                        "cilium_operator",
                        "Cilium Operator",
                        &format!("{}/{} ready ({} available)", ready, replicas, available),
                    );
                } else {
                    return DiagnosticCheck::fail(
                        "cilium_operator",
                        "Cilium Operator",
                        &format!("{}/{} ready", ready, replicas),
                        None,
                    )
                    .with_details(
                        "Cilium Operator is not ready. This can affect:\n\
                                  - IP address allocation\n\
                                  - CiliumNetworkPolicy enforcement\n\
                                  - Cluster mesh operations\n\n\
                                  Check operator logs:\n\
                                  kubectl logs -n kube-system -l name=cilium-operator",
                    );
                }
            }
            Err(_) => continue,
        }
    }

    // No operator found - this might be fine for some Cilium configurations
    DiagnosticCheck::unknown("cilium_operator", "Cilium Operator").with_details(
        "Cilium Operator deployment not found in kube-system.\n\
                       Some Cilium features may require the operator.",
    )
}

/// Check Hubble Relay deployment status (returns None if Hubble not deployed)
async fn check_hubble_relay(client: &Client) -> Option<DiagnosticCheck> {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), "kube-system");

    match deployments.get("hubble-relay").await {
        Ok(deployment) => {
            let status = deployment.status.as_ref();
            let replicas = status.and_then(|s| s.replicas).unwrap_or(0);
            let ready = status.and_then(|s| s.ready_replicas).unwrap_or(0);

            if ready > 0 && ready == replicas {
                Some(DiagnosticCheck::pass(
                    "hubble_relay",
                    "Hubble Relay",
                    &format!("{}/{} ready", ready, replicas),
                ))
            } else {
                Some(
                    DiagnosticCheck::fail(
                        "hubble_relay",
                        "Hubble Relay",
                        &format!("{}/{} ready", ready, replicas),
                        None,
                    )
                    .with_details(
                        "Hubble Relay is not ready. Network flow observability unavailable.\n\n\
                              Check relay logs:\n\
                              kubectl logs -n kube-system -l k8s-app=hubble-relay",
                    ),
                )
            }
        }
        Err(_) => None, // Hubble not deployed - not an error
    }
}

/// Check if Hubble UI is deployed
#[allow(dead_code)]
async fn check_hubble_ui(client: &Client) -> Option<DiagnosticCheck> {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), "kube-system");

    match deployments.get("hubble-ui").await {
        Ok(deployment) => {
            let status = deployment.status.as_ref();
            let replicas = status.and_then(|s| s.replicas).unwrap_or(0);
            let ready = status.and_then(|s| s.ready_replicas).unwrap_or(0);

            if ready > 0 && ready == replicas {
                Some(DiagnosticCheck::pass(
                    "hubble_ui",
                    "Hubble UI",
                    &format!("{}/{} ready", ready, replicas),
                ))
            } else {
                Some(DiagnosticCheck::warn(
                    "hubble_ui",
                    "Hubble UI",
                    &format!("{}/{} ready", ready, replicas),
                ))
            }
        }
        Err(_) => None, // Hubble UI not deployed
    }
}

/// Detect if Cilium is running in eBPF/kube-proxy replacement mode
///
/// This is detected by checking if kube-proxy pods exist.
/// If no kube-proxy pods are running, Cilium is likely in eBPF mode.
pub async fn detect_ebpf_mode(client: &Client) -> bool {
    let pods: Api<Pod> = Api::namespaced(client.clone(), "kube-system");

    let params = ListParams::default().labels("k8s-app=kube-proxy");
    match pods.list(&params).await {
        Ok(list) => {
            // If no kube-proxy pods, assume Cilium eBPF mode
            list.items.is_empty()
        }
        Err(_) => false, // Can't determine, assume not eBPF mode
    }
}

/// Check for Cilium eBPF + KubeSpan compatibility issues
///
/// Cilium in eBPF mode (kube-proxy replacement) combined with KubeSpan
/// can cause asymmetric routing issues because:
/// - KubeSpan routes traffic through WireGuard tunnels between nodes
/// - Cilium eBPF mode manages service routing at the eBPF level
/// - This combination can cause packets to be sent via different paths
///   for outbound vs inbound traffic, breaking connection tracking
///
/// Returns Some(check) if both conditions are detected, None otherwise.
async fn check_kubespan_compatibility(
    ctx: &DiagnosticContext,
    k8s_client: &Client,
) -> Option<DiagnosticCheck> {
    // First check if Cilium is in eBPF mode (no kube-proxy)
    let is_ebpf_mode = detect_ebpf_mode(k8s_client).await;

    if !is_ebpf_mode {
        // Cilium is not in eBPF mode, no compatibility issue
        return None;
    }

    // Check if KubeSpan is enabled
    let kubespan_enabled = if let Some(ref endpoint) = ctx.node_endpoint {
        is_kubespan_enabled_for_node(
            ctx.talos_context.as_deref(),
            endpoint,
            ctx.config_path.as_deref(),
        )
        .await
    } else {
        false
    };

    if kubespan_enabled {
        // Both Cilium eBPF and KubeSpan are enabled - this is problematic
        Some(
            DiagnosticCheck::warn(
                "cilium_kubespan_compat",
                "Cilium + KubeSpan",
                "Compatibility issue detected",
            )
            .with_details(
                "Cilium is running in eBPF mode (kube-proxy replacement) with KubeSpan enabled.\n\n\
            This combination can cause networking issues due to asymmetric routing:\n\
            - KubeSpan routes inter-node traffic through WireGuard tunnels\n\
            - Cilium eBPF manages service routing at the kernel level\n\
            - Packets may take different paths for outbound vs inbound traffic\n\
            - Connection tracking can be broken, causing intermittent failures\n\n\
            Recommendations:\n\
            1. Disable KubeSpan if using Cilium eBPF mode\n\
            2. Or use Cilium in legacy mode (with kube-proxy) if KubeSpan is required\n\n\
            See: https://www.talos.dev/latest/kubernetes-guides/network/kubespan/",
            ),
        )
    } else {
        // KubeSpan not enabled, no issue
        None
    }
}

/// Determine overall Cilium CNI status based on component checks
fn determine_overall_status(
    checks: &[DiagnosticCheck],
    ctx: &DiagnosticContext,
) -> DiagnosticCheck {
    use crate::components::diagnostics::types::CheckStatus;

    // Check if any critical checks failed
    let has_failure = checks.iter().any(|c| c.status == CheckStatus::Fail);
    let has_warning = checks.iter().any(|c| c.status == CheckStatus::Warn);

    if has_failure {
        DiagnosticCheck::fail("cni", "CNI (Cilium)", "Some components unhealthy", None)
    } else if has_warning {
        DiagnosticCheck::warn("cni", "CNI (Cilium)", "Minor issues detected")
    } else if ctx.cni_info.is_some() {
        DiagnosticCheck::pass("cni", "CNI (Cilium)", "OK")
    } else {
        DiagnosticCheck::pass("cni", "CNI (Cilium)", "Detected")
    }
}
