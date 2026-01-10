//! Core diagnostic checks that run on any Talos cluster
//!
//! These checks are CNI-agnostic and addon-agnostic.

use super::types::{DiagnosticCheck, DiagnosticContext, DiagnosticFix, FixAction};
use talos_rs::TalosClient;

/// Run all core system health checks
pub async fn run_system_checks(
    client: &TalosClient,
    _ctx: &DiagnosticContext,
) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    // Memory check
    match client.memory().await {
        Ok(mem_list) => {
            if let Some(mem) = mem_list.first() {
                if let Some(info) = &mem.meminfo {
                    let usage_pct = info.usage_percent();
                    let used_gb = (info.mem_total - info.mem_available) as f64 / 1_073_741_824.0;
                    let total_gb = info.mem_total as f64 / 1_073_741_824.0;
                    let msg = format!("{:.1} / {:.1} GB ({:.0}%)", used_gb, total_gb, usage_pct);

                    if usage_pct > 90.0 {
                        checks.push(DiagnosticCheck::fail("memory", "Memory", &msg, None));
                    } else if usage_pct > 80.0 {
                        checks.push(DiagnosticCheck::warn("memory", "Memory", &msg));
                    } else {
                        checks.push(DiagnosticCheck::pass("memory", "Memory", &msg));
                    }
                }
            }
        }
        Err(e) => {
            checks.push(
                DiagnosticCheck::unknown("memory", "Memory")
                    .with_details(&format!("Error: {}", e)),
            );
        }
    }

    // CPU load check
    match client.load_avg().await {
        Ok(load_list) => {
            if let Some(load) = load_list.first() {
                let msg = format!("{:.2} / {:.2} / {:.2}", load.load1, load.load5, load.load15);
                // Simple heuristic: load > 4 is concerning (assumes multi-core)
                if load.load1 > 4.0 {
                    checks.push(DiagnosticCheck::warn("cpu_load", "CPU Load", &msg));
                } else {
                    checks.push(DiagnosticCheck::pass("cpu_load", "CPU Load", &msg));
                }
            }
        }
        Err(e) => {
            checks.push(
                DiagnosticCheck::unknown("cpu_load", "CPU Load")
                    .with_details(&format!("Error: {}", e)),
            );
        }
    }

    // TODO: Add disk usage check
    // This would check ephemeral and state partition usage

    checks
}

/// Run Talos service health checks
pub async fn run_service_checks(
    client: &TalosClient,
    _ctx: &DiagnosticContext,
) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    match client.services().await {
        Ok(services_list) => {
            for node_services in services_list {
                for service in node_services.services {
                    let is_healthy = service
                        .health
                        .as_ref()
                        .map(|h| h.healthy)
                        .unwrap_or(false);

                    let status_msg = format!(
                        "{} ({})",
                        service.state,
                        if is_healthy { "healthy" } else { "unhealthy" }
                    );

                    if is_healthy {
                        checks.push(DiagnosticCheck::pass(
                            &format!("service_{}", service.id),
                            &service.id,
                            &status_msg,
                        ));
                    } else {
                        checks.push(DiagnosticCheck::fail(
                            &format!("service_{}", service.id),
                            &service.id,
                            &status_msg,
                            Some(DiagnosticFix {
                                description: format!("Restart {}", service.id),
                                action: FixAction::RestartService(service.id.clone()),
                            }),
                        ));
                    }
                }
            }
        }
        Err(e) => {
            checks.push(
                DiagnosticCheck::unknown("services", "Services")
                    .with_details(&format!("Error: {}", e)),
            );
        }
    }

    checks
}

/// Run core Kubernetes component checks
pub async fn run_kubernetes_checks(
    client: &TalosClient,
    ctx: &DiagnosticContext,
) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    // Etcd check (for control plane nodes)
    if ctx.node_role.contains("controlplane") || ctx.node_role.contains("control") {
        match client.etcd_status().await {
            Ok(status_list) => {
                if let Some(status) = status_list.first() {
                    let is_leader = status.is_leader();
                    let msg = if is_leader {
                        "Leader, healthy".to_string()
                    } else {
                        format!("Follower (leader: {:x})", status.leader_id)
                    };
                    checks.push(DiagnosticCheck::pass("etcd", "Etcd", &msg));
                }
            }
            Err(e) => {
                checks.push(
                    DiagnosticCheck::fail("etcd", "Etcd", "Unreachable", None)
                        .with_details(&format!("Error: {}", e)),
                );
            }
        }
    }

    // Generic pod health check from kubelet logs
    // Note: CNI-specific checks are delegated to CNI providers
    match client.logs("kubelet", 100).await {
        Ok(logs) => {
            let log_lines: Vec<&str> = logs.lines().collect();
            let recent_logs = if log_lines.len() > 20 {
                log_lines[log_lines.len() - 20..].join("\n")
            } else {
                logs.clone()
            };

            // Check for CrashLoopBackOff (generic issue)
            let crashloop = recent_logs.contains("CrashLoopBackOff");

            if crashloop {
                checks.push(DiagnosticCheck::warn(
                    "pods_crashing",
                    "Pod Health",
                    "CrashLoopBackOff detected",
                ));
            } else {
                checks.push(DiagnosticCheck::pass(
                    "pods_crashing",
                    "Pod Health",
                    "No issues detected",
                ));
            }
        }
        Err(_) => {
            checks.push(DiagnosticCheck::unknown("pods_crashing", "Pod Health"));
        }
    }

    checks
}

/// Check if CNI is working (generic check)
/// Returns (is_working, error_details)
pub async fn check_cni_health(client: &TalosClient) -> (bool, Option<String>) {
    match client.logs("kubelet", 100).await {
        Ok(logs) => {
            let log_lines: Vec<&str> = logs.lines().collect();
            let recent_logs = if log_lines.len() > 20 {
                log_lines[log_lines.len() - 20..].join("\n")
            } else {
                logs.clone()
            };

            // Check for CNI failures
            let has_failure = recent_logs.contains("failed to setup network for sandbox")
                || recent_logs.contains("network plugin is not ready");

            // Check for successes
            let has_success = recent_logs.contains("successfully setup network")
                || logs.contains("ADD command succeeded");

            if has_failure && !has_success {
                (false, Some("CNI plugin failed to set up pod networking".to_string()))
            } else {
                (true, None)
            }
        }
        Err(e) => (false, Some(format!("Could not check CNI health: {}", e))),
    }
}
