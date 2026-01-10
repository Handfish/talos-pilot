//! Flannel CNI diagnostics
//!
//! Flannel-specific checks:
//! - br_netfilter kernel module (required for iptables-based networking)
//! - subnet.env file existence (primary source of truth for CNI health)
//! - Flannel pod health
//!
//! Philosophy: Check actual state, not logs. Logs can contain stale errors
//! from before issues were fixed. The subnet.env file is the definitive
//! indicator of whether Flannel has successfully initialized.

use crate::components::diagnostics::types::{
    DiagnosticCheck, DiagnosticContext, DiagnosticFix, FixAction,
};
use talos_rs::TalosClient;

/// Run Flannel-specific diagnostic checks
pub async fn run_flannel_checks(
    client: &TalosClient,
    ctx: &DiagnosticContext,
) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    // Check br_netfilter kernel module (Flannel requires this)
    let br_netfilter_check = check_br_netfilter(client, ctx).await;
    let br_netfilter_ok = br_netfilter_check.status == crate::components::diagnostics::types::CheckStatus::Pass;

    checks.push(br_netfilter_check);

    // If we have K8s API info, check Flannel pod health
    if let Some(ref cni_info) = ctx.cni_info {
        checks.push(check_flannel_pods(cni_info));
    }

    // Check CNI health from kubelet logs
    let cni_check = check_flannel_cni(client, ctx, br_netfilter_ok).await;
    checks.push(cni_check);

    checks
}

/// Check Flannel pod health from K8s API
fn check_flannel_pods(cni_info: &crate::components::diagnostics::types::CniInfo) -> DiagnosticCheck {
    if cni_info.pods.is_empty() {
        return DiagnosticCheck::warn(
            "flannel_pods",
            "Flannel Pods",
            "No pods found",
        ).with_details("Could not find Flannel pods in kube-system namespace.");
    }

    let healthy = cni_info.are_pods_healthy();
    let summary = cni_info.pod_health_summary();

    if healthy {
        DiagnosticCheck::pass("flannel_pods", "Flannel Pods", &summary)
    } else {
        // Find unhealthy pods
        let unhealthy: Vec<_> = cni_info.pods.iter()
            .filter(|p| p.phase != "Running" || !p.ready)
            .collect();

        let details = unhealthy.iter()
            .map(|p| format!("  {} - {} (ready: {})", p.name, p.phase, p.ready))
            .collect::<Vec<_>>()
            .join("\n");

        DiagnosticCheck::fail(
            "flannel_pods",
            "Flannel Pods",
            &summary,
            None,
        ).with_details(&format!("Unhealthy pods:\n{}", details))
    }
}

/// Check if br_netfilter kernel module is loaded
/// This is required for Flannel's iptables-based networking
async fn check_br_netfilter(
    client: &TalosClient,
    ctx: &DiagnosticContext,
) -> DiagnosticCheck {
    // Check by reading /proc/sys/net/bridge/bridge-nf-call-iptables
    // If it exists and contains "1", br_netfilter is loaded
    let br_netfilter_loaded = match client.is_br_netfilter_loaded().await {
        Ok(loaded) => loaded,
        Err(_) => false,
    };

    if br_netfilter_loaded {
        DiagnosticCheck::pass("br_netfilter", "br_netfilter", "Loaded")
    } else {
        // Provide platform-specific fix
        let fix = if ctx.is_container {
            // Generate combined command with modprobe and docker restart
            let container_name = if ctx.hostname.is_empty() {
                "<container-name>".to_string()
            } else {
                ctx.hostname.clone()
            };
            let combined_command = format!(
                "sudo modprobe br_netfilter && \\\ndocker restart {}",
                container_name
            );
            DiagnosticFix {
                description: "Load br_netfilter on Docker host".to_string(),
                action: FixAction::HostCommand {
                    command: combined_command,
                    description: "Load br_netfilter on Docker host".to_string(),
                },
            }
        } else {
            DiagnosticFix {
                description: "Add br_netfilter kernel module".to_string(),
                action: FixAction::AddKernelModule("br_netfilter".to_string()),
            }
        };

        let details = if ctx.is_container {
            "The br_netfilter kernel module must be loaded on your Docker host machine.\n\n\
             Run this command on your host (not in the container):\n  \
             sudo modprobe br_netfilter\n\n\
             Then restart the Talos container."
        } else {
            "The br_netfilter kernel module is required for Flannel networking. \
             Without it, pod networking will not function properly."
        };

        DiagnosticCheck::fail(
            "br_netfilter",
            "br_netfilter",
            if ctx.is_container { "Missing (load on host)" } else { "Missing" },
            Some(fix),
        )
        .with_details(details)
    }
}

/// Check Flannel CNI health by verifying subnet.env file exists
///
/// This is the primary source of truth for CNI health. The subnet.env file
/// is created by flanneld when it successfully initializes. If this file
/// exists, Flannel is working - regardless of what old error logs might say.
async fn check_flannel_cni(
    client: &TalosClient,
    ctx: &DiagnosticContext,
    br_netfilter_ok: bool,
) -> DiagnosticCheck {
    // Primary check: Does /run/flannel/subnet.env exist?
    // This file is the definitive indicator that Flannel has initialized.
    let subnet_env_exists = match client.read_file("/run/flannel/subnet.env").await {
        Ok(content) => {
            // File exists - verify it has valid content
            content.contains("FLANNEL_NETWORK=") && content.contains("FLANNEL_SUBNET=")
        }
        Err(_) => false,
    };

    if subnet_env_exists {
        // Flannel is working - subnet.env exists with valid content
        DiagnosticCheck::pass("cni", "CNI (Flannel)", "OK")
    } else {
        // subnet.env missing - Flannel hasn't initialized
        // Determine the likely cause and appropriate fix
        let (fix, message, details) = if !br_netfilter_ok {
            // br_netfilter is the root cause
            let fix = if ctx.is_container {
                let container_name = if ctx.hostname.is_empty() {
                    "<container-name>".to_string()
                } else {
                    ctx.hostname.clone()
                };
                let combined_command = format!(
                    "sudo modprobe br_netfilter && \\\ndocker restart {}",
                    container_name
                );
                Some(DiagnosticFix {
                    description: "Load br_netfilter on Docker host".to_string(),
                    action: FixAction::HostCommand {
                        command: combined_command,
                        description: "Load br_netfilter on Docker host".to_string(),
                    },
                })
            } else {
                Some(DiagnosticFix {
                    description: "Add br_netfilter kernel module".to_string(),
                    action: FixAction::AddKernelModule("br_netfilter".to_string()),
                })
            };
            (
                fix,
                "br_netfilter missing",
                "CNI plugin cannot initialize because br_netfilter kernel module is not loaded.\n\
                 The /run/flannel/subnet.env file is missing.",
            )
        } else if ctx.is_container {
            // br_netfilter is OK but subnet.env missing - Flannel pod may not be running
            let container_name = if ctx.hostname.is_empty() {
                "<container-name>".to_string()
            } else {
                ctx.hostname.clone()
            };
            (
                Some(DiagnosticFix {
                    description: "Check Flannel pod status".to_string(),
                    action: FixAction::HostCommand {
                        command: format!("kubectl get pods -n kube-flannel && docker restart {}", container_name),
                        description: "Check Flannel pods and restart container if needed".to_string(),
                    },
                }),
                "Flannel not initialized",
                "The /run/flannel/subnet.env file is missing. Flannel has not initialized.\n\
                 Check if Flannel pods are running in kube-flannel namespace.",
            )
        } else {
            (
                None,
                "Flannel not initialized",
                "The /run/flannel/subnet.env file is missing. Flannel has not initialized.\n\
                 Check Flannel pod logs: kubectl logs -n kube-flannel -l app=flannel",
            )
        };

        DiagnosticCheck::fail("cni", "CNI (Flannel)", message, fix).with_details(details)
    }
}
