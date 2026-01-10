//! Flannel CNI diagnostics
//!
//! Flannel-specific checks:
//! - br_netfilter kernel module (required for iptables-based networking)
//! - subnet.env file existence
//! - Flannel pod health

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

    // Check CNI health from kubelet logs
    let cni_check = check_flannel_cni(client, ctx, br_netfilter_ok).await;

    checks.push(br_netfilter_check);
    checks.push(cni_check);

    checks
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
            DiagnosticFix {
                description: "Load br_netfilter on Docker host".to_string(),
                action: FixAction::HostCommand {
                    command: "sudo modprobe br_netfilter".to_string(),
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

/// Check Flannel CNI health from kubelet logs
async fn check_flannel_cni(
    client: &TalosClient,
    ctx: &DiagnosticContext,
    br_netfilter_ok: bool,
) -> DiagnosticCheck {
    match client.logs("kubelet", 100).await {
        Ok(logs) => {
            let log_lines: Vec<&str> = logs.lines().collect();
            let recent_logs = if log_lines.len() > 20 {
                log_lines[log_lines.len() - 20..].join("\n")
            } else {
                logs.clone()
            };

            // Check for Flannel-specific failures
            let has_subnet_error = recent_logs.contains("subnet.env: no such file");
            let has_cni_failure = recent_logs.contains("failed to setup network for sandbox");

            // Check for successes
            let has_success = recent_logs.contains("successfully setup network")
                || logs.contains("ADD command succeeded");

            let cni_failed = (has_subnet_error || has_cni_failure) && !has_success;

            if cni_failed {
                // Determine fix based on br_netfilter status
                let (fix, message, details) = if !br_netfilter_ok {
                    // br_netfilter is the root cause
                    let fix = if ctx.is_container {
                        Some(DiagnosticFix {
                            description: "Load br_netfilter on Docker host".to_string(),
                            action: FixAction::HostCommand {
                                command: "sudo modprobe br_netfilter".to_string(),
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
                        "CNI plugin failed because br_netfilter kernel module is not loaded.",
                    )
                } else if ctx.is_container {
                    // In Docker, might need to restart container
                    (
                        Some(DiagnosticFix {
                            description: "Restart Talos container".to_string(),
                            action: FixAction::HostCommand {
                                command: "docker restart <container-name>".to_string(),
                                description: "Restart Talos container".to_string(),
                            },
                        }),
                        "Restart container",
                        "CNI plugin failed. If you recently loaded br_netfilter, restart the Talos container.",
                    )
                } else {
                    (
                        None,
                        "Network setup failed",
                        "CNI plugin failed to set up pod networking. Check Flannel pod logs.",
                    )
                };

                DiagnosticCheck::fail("cni", "CNI (Flannel)", message, fix).with_details(details)
            } else {
                DiagnosticCheck::pass("cni", "CNI (Flannel)", "OK")
            }
        }
        Err(_) => DiagnosticCheck::unknown("cni", "CNI (Flannel)"),
    }
}
