//! Core diagnostic checks that run on any Talos cluster
//!
//! These checks are CNI-agnostic and addon-agnostic.

use super::pki::{self, CertStatus, CertificateInfo};
use super::types::{DiagnosticCheck, DiagnosticContext, DiagnosticFix, FixAction};
use talos_rs::TalosClient;

/// Run all core system health checks
pub async fn run_system_checks(
    client: &TalosClient,
    ctx: &DiagnosticContext,
) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    // Memory check
    match client.memory().await {
        Ok(mem_list) => {
            if let Some(mem) = mem_list.first()
                && let Some(info) = &mem.meminfo
            {
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
        Err(e) => {
            checks.push(
                DiagnosticCheck::unknown("memory", "Memory").with_details(&format!("Error: {}", e)),
            );
        }
    }

    // CPU load check - threshold scales by CPU count
    match client.load_avg().await {
        Ok(load_list) => {
            if let Some(load) = load_list.first() {
                let msg = format!("{:.2} / {:.2} / {:.2}", load.load1, load.load5, load.load15);
                // Scale threshold by CPU count: warn if load > cpu_count * 1.5
                let threshold = (ctx.cpu_count as f64) * 1.5;
                if load.load1 > threshold {
                    checks.push(
                        DiagnosticCheck::warn("cpu_load", "CPU Load", &msg).with_details(&format!(
                            "Load exceeds threshold ({:.1} for {} CPUs)",
                            threshold, ctx.cpu_count
                        )),
                    );
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
                    let is_healthy = service.health.as_ref().map(|h| h.healthy).unwrap_or(false);

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
        // Target this specific node for etcd status
        let target_node = ctx
            .node_endpoint
            .clone()
            .unwrap_or_else(|| ctx.hostname.clone());
        match client.etcd_status_for_nodes(&[target_node]).await {
            Ok(status_list) => {
                if let Some(status) = status_list.first() {
                    let is_leader = status.is_leader();
                    let msg = if is_leader {
                        "Leader, healthy".to_string()
                    } else {
                        format!("Follower (leader: {:x})", status.leader_id)
                    };
                    checks.push(DiagnosticCheck::pass("etcd", "Etcd", &msg));
                } else {
                    checks.push(DiagnosticCheck::warn("etcd", "Etcd", "No status returned"));
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

    // Pod health check - use K8s API if available, otherwise skip
    // Note: CNI-specific checks are delegated to CNI providers
    if let Some(ref pod_health) = ctx.pod_health {
        if pod_health.has_issues() {
            let summary = pod_health.summary();
            let mut details = String::new();

            if !pod_health.crashing.is_empty() {
                details.push_str("Crashing pods:\n");
                for pod in &pod_health.crashing {
                    details.push_str(&format!(
                        "  {}/{} ({} restarts)\n",
                        pod.namespace, pod.name, pod.restart_count
                    ));
                }
            }
            if !pod_health.image_pull_errors.is_empty() {
                if !details.is_empty() {
                    details.push('\n');
                }
                details.push_str("Image pull errors:\n");
                for pod in &pod_health.image_pull_errors {
                    details.push_str(&format!("  {}/{}\n", pod.namespace, pod.name));
                }
            }

            checks.push(
                DiagnosticCheck::warn("pod_health", "Pod Health", &summary)
                    .with_details(details.trim_end()),
            );
        } else {
            checks.push(DiagnosticCheck::pass(
                "pod_health",
                "Pod Health",
                &format!("{} pods", pod_health.total_pods),
            ));
        }
    } else {
        // K8s API not available - show the actual error if we have it
        let details = if let Some(ref error) = ctx.k8s_error {
            format!(
                "K8s API unavailable - cannot check pod status.\n\n\
                 Error: {}\n\n\
                 Possible causes:\n\
                 - Cluster is still starting up\n\
                 - API server not ready yet\n\
                 - kubeconfig not available from Talos\n\
                 - Network/TLS issues connecting to API server",
                error
            )
        } else {
            "K8s API unavailable - cannot check pod status.\n\n\
             Possible causes:\n\
             - Cluster is still starting up\n\
             - API server not ready yet\n\
             - Try refreshing in a few seconds"
                .to_string()
        };
        checks.push(DiagnosticCheck::unknown("pod_health", "Pod Health").with_details(&details));
    }

    checks
}

/// Check if CNI is working (generic check via file existence)
///
/// This checks for CNI-specific config files rather than parsing logs.
/// The presence of these files indicates the CNI has initialized.
///
/// Returns (is_working, error_details)
pub async fn check_cni_health(client: &TalosClient) -> (bool, Option<String>) {
    // Check for Flannel subnet.env
    if client.read_file("/run/flannel/subnet.env").await.is_ok() {
        return (true, None);
    }

    // Check for Cilium CNI config
    if client
        .read_file("/etc/cni/net.d/05-cilium.conflist")
        .await
        .is_ok()
    {
        return (true, None);
    }

    // Check for Calico CNI config
    if client
        .read_file("/etc/cni/net.d/10-calico.conflist")
        .await
        .is_ok()
    {
        return (true, None);
    }

    // Check for any CNI config in /etc/cni/net.d/
    // If we find any config file, CNI is likely working
    if let Ok(content) = client.read_file("/etc/cni/net.d").await {
        // If the directory exists and has content, some CNI is configured
        if !content.is_empty() {
            return (true, None);
        }
    }

    // No CNI config files found
    (false, Some("No CNI configuration files found".to_string()))
}

/// Run certificate expiry checks
///
/// Checks:
/// - talosconfig client certificate (local file)
/// - talosconfig CA certificate (local file)
/// - kubeconfig client certificate (from API)
pub async fn run_certificate_checks(
    client: &TalosClient,
    _ctx: &DiagnosticContext,
    config_path: Option<&str>,
) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    // Load talosconfig - use custom path if provided
    let config_result = match config_path {
        Some(path) => {
            let path_buf = std::path::PathBuf::from(path);
            talos_rs::TalosConfig::load_from(&path_buf)
        }
        None => talos_rs::TalosConfig::load_default(),
    };
    match config_result {
        Ok(config) => {
            if let Some(context) = config.current_context() {
                // Check client certificate
                match context.client_cert_pem() {
                    Ok(pem_data) => match pki::parse_certificate("talosconfig", &pem_data) {
                        Ok(cert_info) => {
                            checks.push(cert_to_diagnostic_check(
                                "talosconfig_cert",
                                "talosconfig",
                                &cert_info,
                            ));
                        }
                        Err(e) => {
                            checks.push(
                                DiagnosticCheck::unknown("talosconfig_cert", "talosconfig")
                                    .with_details(&format!("Failed to parse certificate: {}", e)),
                            );
                        }
                    },
                    Err(e) => {
                        checks.push(
                            DiagnosticCheck::unknown("talosconfig_cert", "talosconfig")
                                .with_details(&format!("Failed to decode certificate: {}", e)),
                        );
                    }
                }

                // Check CA certificate
                match context.ca_pem() {
                    Ok(pem_data) => match pki::parse_certificate("Talos CA", &pem_data) {
                        Ok(cert_info) => {
                            checks
                                .push(cert_to_diagnostic_check("talos_ca", "Talos CA", &cert_info));
                        }
                        Err(e) => {
                            checks.push(
                                DiagnosticCheck::unknown("talos_ca", "Talos CA")
                                    .with_details(&format!("Failed to parse CA: {}", e)),
                            );
                        }
                    },
                    Err(e) => {
                        checks.push(
                            DiagnosticCheck::unknown("talos_ca", "Talos CA")
                                .with_details(&format!("Failed to decode CA: {}", e)),
                        );
                    }
                }
            }
        }
        Err(e) => {
            checks.push(
                DiagnosticCheck::unknown("talosconfig_cert", "talosconfig")
                    .with_details(&format!("Failed to load talosconfig: {}", e)),
            );
        }
    }

    // Check kubeconfig certificate (from Talos API)
    match client.kubeconfig().await {
        Ok(kubeconfig_yaml) => {
            // Parse kubeconfig YAML to extract client certificate
            if let Ok(kc) = serde_yaml::from_str::<serde_yaml::Value>(&kubeconfig_yaml)
                && let Some(users) = kc.get("users").and_then(|u| u.as_sequence())
            {
                for user in users {
                    if let Some(user_data) = user.get("user") {
                        // Check for client-certificate-data (base64 encoded PEM)
                        if let Some(cert_data) = user_data
                            .get("client-certificate-data")
                            .and_then(|c| c.as_str())
                        {
                            match pki::parse_base64_certificate("kubeconfig", cert_data) {
                                Ok(cert_info) => {
                                    checks.push(cert_to_diagnostic_check(
                                        "kubeconfig_cert",
                                        "kubeconfig",
                                        &cert_info,
                                    ));
                                }
                                Err(e) => {
                                    checks.push(
                                        DiagnosticCheck::unknown("kubeconfig_cert", "kubeconfig")
                                            .with_details(&format!(
                                                "Failed to parse kubeconfig cert: {}",
                                                e
                                            )),
                                    );
                                }
                            }
                            break; // Only check first user
                        }
                    }
                }
            }
        }
        Err(e) => {
            // kubeconfig may not be available yet during cluster bootstrap
            checks.push(
                DiagnosticCheck::unknown("kubeconfig_cert", "kubeconfig")
                    .with_details(&format!("kubeconfig not available: {}", e)),
            );
        }
    }

    checks
}

/// Convert a CertificateInfo to a DiagnosticCheck
fn cert_to_diagnostic_check(id: &str, name: &str, cert: &CertificateInfo) -> DiagnosticCheck {
    let message = if cert.days_remaining <= 0 {
        format!(
            "EXPIRED {} ago",
            cert.time_remaining.replace("expired ", "")
        )
    } else {
        format!("expires in {}", cert.time_remaining)
    };

    let details = format!(
        "Subject: {}\nIssuer: {}\nExpires: {}\nDays remaining: {}",
        cert.subject,
        cert.issuer,
        cert.not_after.format("%Y-%m-%d %H:%M:%S UTC"),
        cert.days_remaining
    );

    let renewal_hint = match name {
        "talosconfig" => {
            Some("To renew, run:\n  talosctl config new --roles=os:admin new-admin.yaml")
        }
        "kubeconfig" => Some(
            "Kubeconfig certificates are managed by Talos.\nRegenerate with: talosctl kubeconfig",
        ),
        _ => None,
    };

    let full_details = if let Some(hint) = renewal_hint {
        format!("{}\n\n{}", details, hint)
    } else {
        details
    };

    match cert.status {
        CertStatus::Valid => DiagnosticCheck::pass(id, name, &message).with_details(&full_details),
        CertStatus::Warning => {
            let mut check = DiagnosticCheck::warn(id, name, &message).with_details(&full_details);

            // Add fix suggestion for talosconfig
            if name == "talosconfig" {
                check = check.with_fix(DiagnosticFix {
                    description: "Renew talosconfig certificate".to_string(),
                    action: FixAction::HostCommand {
                        command: "talosctl config new --roles=os:admin new-admin.yaml".to_string(),
                        description: "Generate new talosconfig".to_string(),
                    },
                });
            }
            check
        }
        CertStatus::Critical | CertStatus::Expired => {
            let mut check =
                DiagnosticCheck::fail(id, name, &message, None).with_details(&full_details);

            // Add fix suggestion for talosconfig
            if name == "talosconfig" {
                check = check.with_fix(DiagnosticFix {
                    description: "Renew talosconfig certificate (URGENT)".to_string(),
                    action: FixAction::HostCommand {
                        command: "talosctl config new --roles=os:admin new-admin.yaml".to_string(),
                        description: "Generate new talosconfig".to_string(),
                    },
                });
            }
            check
        }
    }
}
