//! Workload Health component
//!
//! Displays health status of Kubernetes workloads across all namespaces:
//! - Deployments, StatefulSets, DaemonSets
//! - Pods in problematic states (CrashLoopBackOff, ImagePullBackOff, Pending)
//! - Health classification per namespace
//!
//! Philosophy: Show actual K8s API state. Group by namespace with issues first.

use crate::action::Action;
use crate::components::Component;
use crate::ui_ext::HealthIndicatorExt;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent};
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, StatefulSet};
use k8s_openapi::api::core::v1::Pod;
use kube::{
    Client,
    api::{Api, ListParams},
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};
use std::collections::HashMap;
use std::time::Duration;
use talos_pilot_core::constants::HIGH_RESTART_THRESHOLD;
use talos_pilot_core::{AsyncState, HasHealth, HealthIndicator};

/// Health state of a workload or pod
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HealthState {
    /// Critical failure - needs immediate attention
    Failing,
    /// Degraded - partially working
    Degraded,
    /// Pending - waiting for something
    Pending,
    /// Healthy - all good
    Healthy,
}

impl HasHealth for HealthState {
    fn health(&self) -> HealthIndicator {
        match self {
            HealthState::Failing => HealthIndicator::Error,
            HealthState::Degraded => HealthIndicator::Warning,
            HealthState::Pending => HealthIndicator::Pending,
            HealthState::Healthy => HealthIndicator::Healthy,
        }
    }
}

impl HealthState {
    pub fn indicator(&self) -> (&'static str, Color) {
        self.health().symbol_and_color()
    }

    pub fn color(&self) -> Color {
        self.health().color()
    }
}

/// Problematic state reason for pods
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PodIssue {
    CrashLoopBackOff,
    ImagePullBackOff,
    ErrImagePull,
    Pending,
    OOMKilled,
    Error,
    HighRestarts(i32),
    Unknown(String),
}

impl PodIssue {
    pub fn display(&self) -> &str {
        match self {
            PodIssue::CrashLoopBackOff => "CrashLoopBackOff",
            PodIssue::ImagePullBackOff => "ImagePullBackOff",
            PodIssue::ErrImagePull => "ErrImagePull",
            PodIssue::Pending => "Pending",
            PodIssue::OOMKilled => "OOMKilled",
            PodIssue::Error => "Error",
            PodIssue::HighRestarts(_) => "High Restarts",
            PodIssue::Unknown(_) => "Unknown",
        }
    }

    pub fn severity(&self) -> HealthState {
        match self {
            PodIssue::CrashLoopBackOff => HealthState::Failing,
            PodIssue::ImagePullBackOff => HealthState::Failing,
            PodIssue::ErrImagePull => HealthState::Failing,
            PodIssue::OOMKilled => HealthState::Failing,
            PodIssue::Error => HealthState::Failing,
            PodIssue::Pending => HealthState::Pending,
            PodIssue::HighRestarts(_) => HealthState::Degraded,
            PodIssue::Unknown(_) => HealthState::Degraded,
        }
    }
}

/// Information about a workload (Deployment, StatefulSet, or DaemonSet)
#[derive(Debug, Clone)]
pub struct WorkloadInfo {
    pub name: String,
    pub namespace: String,
    pub kind: WorkloadKind,
    pub ready: i32,
    pub desired: i32,
    pub health: HealthState,
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadKind {
    Deployment,
    StatefulSet,
    DaemonSet,
}

impl WorkloadKind {
    pub fn display(&self) -> &'static str {
        match self {
            WorkloadKind::Deployment => "Deploy",
            WorkloadKind::StatefulSet => "StatefulSet",
            WorkloadKind::DaemonSet => "DaemonSet",
        }
    }
}

/// Information about a problematic pod
#[derive(Debug, Clone)]
pub struct PodInfo {
    pub name: String,
    pub namespace: String,
    pub node: Option<String>,
    pub phase: String,
    pub restarts: i32,
    pub issue: Option<PodIssue>,
    pub age: String,
}

/// Summary of a namespace's health
#[derive(Debug, Clone)]
pub struct NamespaceSummary {
    pub name: String,
    pub health: HealthState,
    pub workloads: Vec<WorkloadInfo>,
    pub problem_pods: Vec<PodInfo>,
    pub total_workloads: usize,
    pub healthy_workloads: usize,
}

/// Auto-refresh interval in seconds
const AUTO_REFRESH_INTERVAL_SECS: u64 = 10;

/// Data loaded asynchronously for the workloads component
#[derive(Debug, Clone, Default)]
pub struct WorkloadsData {
    /// Namespace summaries (sorted: issues first)
    pub namespaces: Vec<NamespaceSummary>,

    /// Global summary counts
    pub total_deployments: usize,
    pub total_statefulsets: usize,
    pub total_daemonsets: usize,
    pub total_pods_healthy: usize,
    pub total_pods_degraded: usize,
    pub total_pods_failing: usize,
}

/// Workload Health Component
pub struct WorkloadHealthComponent {
    /// Async state for loaded data
    state: AsyncState<WorkloadsData>,

    /// Kubernetes client
    k8s_client: Option<Client>,

    /// Selected namespace index
    selected_namespace: usize,
    /// Selected item within namespace (for drill-down)
    selected_item: usize,
    /// Whether we're in drill-down mode
    drill_down: bool,

    /// Table state for rendering
    table_state: TableState,

    /// Auto-refresh enabled
    auto_refresh: bool,
}

impl Default for WorkloadHealthComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkloadHealthComponent {
    pub fn new() -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));

        Self {
            state: AsyncState::new(),
            k8s_client: None,
            selected_namespace: 0,
            selected_item: 0,
            drill_down: false,
            table_state,
            auto_refresh: true,
        }
    }

    /// Access loaded data immutably
    fn data(&self) -> Option<&WorkloadsData> {
        self.state.data()
    }

    /// Set the Kubernetes client
    pub fn set_k8s_client(&mut self, client: Client) {
        self.k8s_client = Some(client);
    }

    /// Set an error message
    pub fn set_error(&mut self, error: String) {
        self.state.set_error(error);
    }

    /// Refresh workload data from the cluster
    pub async fn refresh(&mut self) -> Result<()> {
        let Some(client) = &self.k8s_client else {
            self.set_error("No Kubernetes client configured".to_string());
            return Ok(());
        };

        self.state.start_loading();

        // Fetch all workloads in parallel
        let deployments_api: Api<Deployment> = Api::all(client.clone());
        let statefulsets_api: Api<StatefulSet> = Api::all(client.clone());
        let daemonsets_api: Api<DaemonSet> = Api::all(client.clone());
        let pods_api: Api<Pod> = Api::all(client.clone());

        let list_params = ListParams::default();
        let timeout = std::time::Duration::from_secs(15);
        let fetch_result = tokio::time::timeout(timeout, async {
            tokio::join!(
                deployments_api.list(&list_params),
                statefulsets_api.list(&list_params),
                daemonsets_api.list(&list_params),
                pods_api.list(&list_params)
            )
        })
        .await;

        let (deployments_result, statefulsets_result, daemonsets_result, pods_result) =
            match fetch_result {
                Ok(results) => results,
                Err(_) => {
                    self.set_error(format!("Request timed out after {}s", timeout.as_secs()));
                    return Ok(());
                }
            };

        // Process results
        let deployments = deployments_result.map_err(|e| {
            self.set_error(format!("Failed to fetch deployments: {}", e));
            e
        })?;

        let statefulsets = statefulsets_result.map_err(|e| {
            self.set_error(format!("Failed to fetch statefulsets: {}", e));
            e
        })?;

        let daemonsets = daemonsets_result.map_err(|e| {
            self.set_error(format!("Failed to fetch daemonsets: {}", e));
            e
        })?;

        let pods = pods_result.map_err(|e| {
            self.set_error(format!("Failed to fetch pods: {}", e));
            e
        })?;

        // Build namespace map
        let mut ns_map: HashMap<String, NamespaceSummary> = HashMap::new();

        // Track counts locally
        let total_deployments = deployments.items.len();
        let total_statefulsets = statefulsets.items.len();
        let total_daemonsets = daemonsets.items.len();
        let mut total_pods_healthy = 0usize;
        let mut total_pods_degraded = 0usize;
        let mut total_pods_failing = 0usize;

        // Process deployments
        for deploy in deployments.items {
            let ns = deploy
                .metadata
                .namespace
                .clone()
                .unwrap_or_else(|| "default".to_string());
            let name = deploy
                .metadata
                .name
                .clone()
                .unwrap_or_else(|| "unknown".to_string());

            let status = deploy.status.as_ref();
            let desired = status.and_then(|s| s.replicas).unwrap_or(0);
            let ready = status.and_then(|s| s.ready_replicas).unwrap_or(0);
            let available = status.and_then(|s| s.available_replicas).unwrap_or(0);

            let (health, issues) = Self::classify_workload_health(ready, desired, available);

            let workload = WorkloadInfo {
                name,
                namespace: ns.clone(),
                kind: WorkloadKind::Deployment,
                ready,
                desired,
                health,
                issues,
            };

            ns_map
                .entry(ns)
                .or_insert_with(|| NamespaceSummary {
                    name: workload.namespace.clone(),
                    health: HealthState::Healthy,
                    workloads: Vec::new(),
                    problem_pods: Vec::new(),
                    total_workloads: 0,
                    healthy_workloads: 0,
                })
                .workloads
                .push(workload);
        }

        // Process statefulsets
        for sts in statefulsets.items {
            let ns = sts
                .metadata
                .namespace
                .clone()
                .unwrap_or_else(|| "default".to_string());
            let name = sts
                .metadata
                .name
                .clone()
                .unwrap_or_else(|| "unknown".to_string());

            let status = sts.status.as_ref();
            let desired = status.map(|s| s.replicas).unwrap_or(0);
            let ready = status.and_then(|s| s.ready_replicas).unwrap_or(0);

            let (health, issues) = Self::classify_workload_health(ready, desired, ready);

            let workload = WorkloadInfo {
                name,
                namespace: ns.clone(),
                kind: WorkloadKind::StatefulSet,
                ready,
                desired,
                health,
                issues,
            };

            ns_map
                .entry(ns)
                .or_insert_with(|| NamespaceSummary {
                    name: workload.namespace.clone(),
                    health: HealthState::Healthy,
                    workloads: Vec::new(),
                    problem_pods: Vec::new(),
                    total_workloads: 0,
                    healthy_workloads: 0,
                })
                .workloads
                .push(workload);
        }

        // Process daemonsets
        for ds in daemonsets.items {
            let ns = ds
                .metadata
                .namespace
                .clone()
                .unwrap_or_else(|| "default".to_string());
            let name = ds
                .metadata
                .name
                .clone()
                .unwrap_or_else(|| "unknown".to_string());

            let status = ds.status.as_ref();
            let desired = status.map(|s| s.desired_number_scheduled).unwrap_or(0);
            let ready = status.map(|s| s.number_ready).unwrap_or(0);

            let (health, issues) = Self::classify_workload_health(ready, desired, ready);

            let workload = WorkloadInfo {
                name,
                namespace: ns.clone(),
                kind: WorkloadKind::DaemonSet,
                ready,
                desired,
                health,
                issues,
            };

            ns_map
                .entry(ns)
                .or_insert_with(|| NamespaceSummary {
                    name: workload.namespace.clone(),
                    health: HealthState::Healthy,
                    workloads: Vec::new(),
                    problem_pods: Vec::new(),
                    total_workloads: 0,
                    healthy_workloads: 0,
                })
                .workloads
                .push(workload);
        }

        // Process pods - find problematic ones
        for pod in pods.items {
            let ns = pod
                .metadata
                .namespace
                .clone()
                .unwrap_or_else(|| "default".to_string());
            let name = pod
                .metadata
                .name
                .clone()
                .unwrap_or_else(|| "unknown".to_string());

            let status = pod.status.as_ref();
            let phase = status
                .and_then(|s| s.phase.clone())
                .unwrap_or_else(|| "Unknown".to_string());

            let node = pod.spec.as_ref().and_then(|s| s.node_name.clone());

            // Calculate restarts and detect issues
            let (restarts, issue) = Self::analyze_pod(&pod);

            // Calculate age
            let age = pod
                .metadata
                .creation_timestamp
                .as_ref()
                .map(|ts| Self::format_age(&ts.0))
                .unwrap_or_else(|| "?".to_string());

            // Track global counts
            match &issue {
                Some(i) => match i.severity() {
                    HealthState::Failing => total_pods_failing += 1,
                    HealthState::Degraded | HealthState::Pending => total_pods_degraded += 1,
                    HealthState::Healthy => total_pods_healthy += 1,
                },
                None => {
                    if phase == "Running" || phase == "Succeeded" {
                        total_pods_healthy += 1;
                    }
                }
            }

            // Only add to namespace if there's an issue
            if issue.is_some() {
                let pod_info = PodInfo {
                    name,
                    namespace: ns.clone(),
                    node,
                    phase,
                    restarts,
                    issue,
                    age,
                };

                ns_map
                    .entry(ns)
                    .or_insert_with(|| NamespaceSummary {
                        name: pod_info.namespace.clone(),
                        health: HealthState::Healthy,
                        workloads: Vec::new(),
                        problem_pods: Vec::new(),
                        total_workloads: 0,
                        healthy_workloads: 0,
                    })
                    .problem_pods
                    .push(pod_info);
            }
        }

        // Calculate namespace health and counts
        for summary in ns_map.values_mut() {
            summary.total_workloads = summary.workloads.len();
            summary.healthy_workloads = summary
                .workloads
                .iter()
                .filter(|w| w.health == HealthState::Healthy)
                .count();

            // Namespace health is the worst of its workloads/pods
            let worst_workload = summary
                .workloads
                .iter()
                .map(|w| w.health)
                .min()
                .unwrap_or(HealthState::Healthy);

            let worst_pod = summary
                .problem_pods
                .iter()
                .filter_map(|p| p.issue.as_ref())
                .map(|i| i.severity())
                .min()
                .unwrap_or(HealthState::Healthy);

            summary.health = worst_workload.min(worst_pod);
        }

        // Convert to vec and sort (issues first, then alphabetically)
        let mut namespaces: Vec<NamespaceSummary> = ns_map.into_values().collect();
        namespaces.sort_by(|a, b| {
            // Sort by health (worst first), then by name
            a.health.cmp(&b.health).then_with(|| a.name.cmp(&b.name))
        });

        // Build and set data
        let data = WorkloadsData {
            namespaces,
            total_deployments,
            total_statefulsets,
            total_daemonsets,
            total_pods_healthy,
            total_pods_degraded,
            total_pods_failing,
        };

        let ns_count = data.namespaces.len();
        self.state.set_data(data);

        // Reset selection if needed
        if let Some(d) = self.data()
            && !d.namespaces.is_empty()
            && self.selected_namespace >= d.namespaces.len()
        {
            self.selected_namespace = 0;
        }
        self.table_state.select(Some(self.selected_namespace));

        tracing::info!(
            "Loaded {} namespaces, {} deployments, {} statefulsets, {} daemonsets",
            ns_count,
            total_deployments,
            total_statefulsets,
            total_daemonsets
        );

        Ok(())
    }

    /// Classify workload health based on ready/desired counts
    fn classify_workload_health(
        ready: i32,
        desired: i32,
        available: i32,
    ) -> (HealthState, Vec<String>) {
        let mut issues = Vec::new();

        if desired == 0 {
            return (HealthState::Healthy, issues); // Scaled to 0 is intentional
        }

        if ready == desired && available == desired {
            return (HealthState::Healthy, issues);
        }

        if ready == 0 {
            issues.push(format!("No pods ready (0/{})", desired));
            return (HealthState::Failing, issues);
        }

        if ready < desired {
            issues.push(format!("Partial: {}/{} ready", ready, desired));
            return (HealthState::Degraded, issues);
        }

        (HealthState::Healthy, issues)
    }

    /// Analyze a pod for issues
    fn analyze_pod(pod: &Pod) -> (i32, Option<PodIssue>) {
        let status = match &pod.status {
            Some(s) => s,
            None => return (0, None),
        };

        let phase = status.phase.as_deref().unwrap_or("Unknown");

        // Calculate total restarts
        let restarts: i32 = status
            .container_statuses
            .as_ref()
            .map(|cs| cs.iter().map(|c| c.restart_count).sum())
            .unwrap_or(0);

        // Check container statuses for waiting reasons
        if let Some(container_statuses) = &status.container_statuses {
            for cs in container_statuses {
                if let Some(state) = &cs.state {
                    // Check waiting state
                    if let Some(waiting) = &state.waiting {
                        let reason = waiting.reason.as_deref().unwrap_or("");
                        match reason {
                            "CrashLoopBackOff" => {
                                return (restarts, Some(PodIssue::CrashLoopBackOff));
                            }
                            "ImagePullBackOff" => {
                                return (restarts, Some(PodIssue::ImagePullBackOff));
                            }
                            "ErrImagePull" => return (restarts, Some(PodIssue::ErrImagePull)),
                            _ => {}
                        }
                    }

                    // Check terminated state for OOMKilled
                    if let Some(terminated) = &state.terminated {
                        if terminated.reason.as_deref() == Some("OOMKilled") {
                            return (restarts, Some(PodIssue::OOMKilled));
                        }
                        if terminated.reason.as_deref() == Some("Error") {
                            return (restarts, Some(PodIssue::Error));
                        }
                    }
                }
            }
        }

        // Check phase
        if phase == "Pending" {
            return (restarts, Some(PodIssue::Pending));
        }

        if phase == "Failed" {
            return (restarts, Some(PodIssue::Error));
        }

        // Check for high restarts
        if restarts >= HIGH_RESTART_THRESHOLD {
            return (restarts, Some(PodIssue::HighRestarts(restarts)));
        }

        (restarts, None)
    }

    /// Format age from timestamp
    fn format_age(ts: &chrono::DateTime<chrono::Utc>) -> String {
        let now = chrono::Utc::now();
        let duration = now.signed_duration_since(*ts);

        if duration.num_days() > 0 {
            format!("{}d", duration.num_days())
        } else if duration.num_hours() > 0 {
            format!("{}h", duration.num_hours())
        } else if duration.num_minutes() > 0 {
            format!("{}m", duration.num_minutes())
        } else {
            format!("{}s", duration.num_seconds())
        }
    }

    /// Navigate to previous item
    fn select_prev(&mut self) {
        if self.drill_down {
            let total = self
                .data()
                .and_then(|d| d.namespaces.get(self.selected_namespace))
                .map(|ns| ns.workloads.len() + ns.problem_pods.len())
                .unwrap_or(0);
            if total > 0 {
                self.selected_item = self.selected_item.saturating_sub(1);
            }
        } else {
            let ns_count = self.data().map(|d| d.namespaces.len()).unwrap_or(0);
            if ns_count > 0 {
                self.selected_namespace = self.selected_namespace.saturating_sub(1);
                self.table_state.select(Some(self.selected_namespace));
            }
        }
    }

    /// Navigate to next item
    fn select_next(&mut self) {
        if self.drill_down {
            let total = self
                .data()
                .and_then(|d| d.namespaces.get(self.selected_namespace))
                .map(|ns| ns.workloads.len() + ns.problem_pods.len())
                .unwrap_or(0);
            if total > 0 {
                self.selected_item = (self.selected_item + 1).min(total - 1);
            }
        } else {
            let ns_count = self.data().map(|d| d.namespaces.len()).unwrap_or(0);
            if ns_count > 0 {
                self.selected_namespace = (self.selected_namespace + 1).min(ns_count - 1);
                self.table_state.select(Some(self.selected_namespace));
            }
        }
    }

    /// Enter drill-down mode for selected namespace
    fn enter_drill_down(&mut self) {
        let has_namespaces = self
            .data()
            .map(|d| !d.namespaces.is_empty())
            .unwrap_or(false);
        if has_namespaces {
            self.drill_down = true;
            self.selected_item = 0;
        }
    }

    /// Exit drill-down mode
    fn exit_drill_down(&mut self) {
        self.drill_down = false;
        self.selected_item = 0;
    }

    /// Draw the summary header
    fn draw_summary(&self, frame: &mut Frame, area: Rect) {
        let Some(data) = self.data() else {
            return;
        };

        let healthy_color = if data.total_pods_failing == 0 && data.total_pods_degraded == 0 {
            Color::Green
        } else {
            Color::White
        };

        let line = Line::from(vec![
            Span::raw("Summary: "),
            Span::styled(
                format!("{}", data.total_deployments),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Deployments, "),
            Span::styled(
                format!("{}", data.total_statefulsets),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(" StatefulSets, "),
            Span::styled(
                format!("{}", data.total_daemonsets),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(" DaemonSets    "),
            Span::styled(
                format!("● {}", data.total_pods_healthy),
                Style::default().fg(healthy_color),
            ),
            Span::raw(" healthy  "),
            if data.total_pods_degraded > 0 {
                Span::styled(
                    format!("◐ {}", data.total_pods_degraded),
                    Style::default().fg(Color::Yellow),
                )
            } else {
                Span::raw("")
            },
            if data.total_pods_degraded > 0 {
                Span::raw(" degraded  ")
            } else {
                Span::raw("")
            },
            if data.total_pods_failing > 0 {
                Span::styled(
                    format!("✗ {}", data.total_pods_failing),
                    Style::default().fg(Color::Red),
                )
            } else {
                Span::raw("")
            },
            if data.total_pods_failing > 0 {
                Span::raw(" failing")
            } else {
                Span::raw("")
            },
        ]);

        let para = Paragraph::new(line).block(Block::default().borders(Borders::BOTTOM));
        frame.render_widget(para, area);
    }

    /// Collect namespace row data for rendering
    fn collect_namespace_rows(&self) -> Vec<(String, String, String, String, Color)> {
        let Some(data) = self.data() else {
            return Vec::new();
        };
        data.namespaces
            .iter()
            .map(|ns| {
                let (indicator, color) = ns.health.indicator();
                let issues_count = ns.problem_pods.len()
                    + ns.workloads
                        .iter()
                        .filter(|w| w.health != HealthState::Healthy)
                        .count();
                let issue_text = if issues_count > 0 {
                    format!("{} issues", issues_count)
                } else {
                    "healthy".to_string()
                };
                (
                    format!("{} ", indicator),
                    ns.name.clone(),
                    format!("{} workloads", ns.total_workloads),
                    issue_text,
                    color,
                )
            })
            .collect()
    }

    /// Draw the namespace list
    fn draw_namespace_list(&mut self, frame: &mut Frame, area: Rect) {
        let row_data = self.collect_namespace_rows();
        let rows: Vec<Row> = row_data
            .into_iter()
            .map(|(indicator, name, workloads, issue_text, color)| {
                Row::new(vec![
                    Cell::from(indicator).style(Style::default().fg(color)),
                    Cell::from(name),
                    Cell::from(workloads),
                    Cell::from(issue_text).style(Style::default().fg(color)),
                ])
            })
            .collect();

        let widths = [
            Constraint::Length(3),
            Constraint::Percentage(40),
            Constraint::Length(15),
            Constraint::Percentage(30),
        ];

        let header = Row::new(vec![
            Cell::from(""),
            Cell::from("NAMESPACE"),
            Cell::from("WORKLOADS"),
            Cell::from("STATUS"),
        ])
        .style(Style::default().add_modifier(Modifier::DIM))
        .bottom_margin(1);

        let table = Table::new(rows, widths)
            .header(header)
            .row_highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        frame.render_stateful_widget(table, area, &mut self.table_state);
    }

    /// Collect drill-down row data for the selected namespace
    #[allow(clippy::type_complexity)]
    fn collect_drill_down_data(
        &self,
    ) -> Option<(String, Vec<(String, String, String, String, String, Color)>)> {
        let data = self.data()?;
        let ns = data.namespaces.get(self.selected_namespace)?;
        let title = format!(" {} ", ns.name);

        let mut rows = Vec::new();

        // Add workloads
        for workload in &ns.workloads {
            let (indicator, color) = workload.health.indicator();
            let status = format!("{}/{} ready", workload.ready, workload.desired);
            let issue_text = if workload.issues.is_empty() {
                String::new()
            } else {
                workload.issues.join(", ")
            };
            rows.push((
                format!("{} ", indicator),
                workload.name.clone(),
                workload.kind.display().to_string(),
                status,
                issue_text,
                color,
            ));
        }

        // Add problem pods
        for pod in &ns.problem_pods {
            let issue = pod.issue.as_ref();
            let (indicator, color) = issue
                .map(|i| i.severity().indicator())
                .unwrap_or(("?", Color::DarkGray));
            let issue_text = issue.map(|i| i.display()).unwrap_or("Unknown").to_string();
            rows.push((
                format!("{} ", indicator),
                pod.name.clone(),
                "Pod".to_string(),
                format!("{} restarts", pod.restarts),
                issue_text,
                color,
            ));
        }

        Some((title, rows))
    }

    /// Draw the drill-down view for a namespace
    fn draw_drill_down(&mut self, frame: &mut Frame, area: Rect) {
        let Some((title, row_data)) = self.collect_drill_down_data() else {
            return;
        };

        // Build rows from collected data
        let rows: Vec<Row> = row_data
            .into_iter()
            .map(|(indicator, name, kind, status, issue_text, color)| {
                Row::new(vec![
                    Cell::from(indicator).style(Style::default().fg(color)),
                    Cell::from(name),
                    Cell::from(kind),
                    Cell::from(status),
                    Cell::from(issue_text).style(Style::default().fg(color)),
                ])
            })
            .collect();

        let widths = [
            Constraint::Length(3),
            Constraint::Percentage(40),
            Constraint::Length(12),
            Constraint::Length(15),
            Constraint::Percentage(25),
        ];

        let header = Row::new(vec![
            Cell::from(""),
            Cell::from("NAME"),
            Cell::from("KIND"),
            Cell::from("STATUS"),
            Cell::from("ISSUE"),
        ])
        .style(Style::default().add_modifier(Modifier::DIM))
        .bottom_margin(1);

        let mut drill_table_state = TableState::default();
        if self.drill_down {
            drill_table_state.select(Some(self.selected_item));
        }

        let table = Table::new(rows, widths)
            .header(header)
            .block(Block::default().title(title).borders(Borders::ALL))
            .row_highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        frame.render_stateful_widget(table, area, &mut drill_table_state);
    }

    /// Draw the footer with keybindings
    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let auto_status = if self.auto_refresh {
            Span::styled("ON ", Style::default().fg(Color::Green))
        } else {
            Span::styled("OFF", Style::default().fg(Color::DarkGray))
        };

        let nav_keys = if self.drill_down {
            vec![
                Span::styled("[j/k]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" select  "),
                Span::styled("[q/Esc]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" back  "),
            ]
        } else {
            vec![
                Span::styled("[j/k]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" select  "),
                Span::styled("[Enter]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" expand  "),
                Span::styled("[q]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" back  "),
            ]
        };

        let mut spans = nav_keys;
        spans.extend(vec![
            Span::styled("[r]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" refresh  "),
            Span::styled("[a]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" auto:"),
            auto_status,
        ]);

        let line = Line::from(spans);
        let para = Paragraph::new(line);
        frame.render_widget(para, area);
    }
}

impl Component for WorkloadHealthComponent {
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                if self.drill_down {
                    self.exit_drill_down();
                    Ok(None)
                } else {
                    Ok(Some(Action::Back))
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.select_next();
                Ok(None)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.select_prev();
                Ok(None)
            }
            KeyCode::Enter => {
                if !self.drill_down {
                    self.enter_drill_down();
                }
                Ok(None)
            }
            KeyCode::Char('r') => Ok(Some(Action::Refresh)),
            KeyCode::Char('a') => {
                self.auto_refresh = !self.auto_refresh;
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if let Action::Tick = action {
            // Check for auto-refresh
            let interval = Duration::from_secs(AUTO_REFRESH_INTERVAL_SECS);
            if self.state.should_auto_refresh(self.auto_refresh, interval) {
                return Ok(Some(Action::Refresh));
            }
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        // Draw title
        let block = Block::default()
            .title(" Workload Health ")
            .borders(Borders::ALL);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if self.state.is_loading() {
            let loading =
                Paragraph::new("Loading workloads...").style(Style::default().fg(Color::DarkGray));
            frame.render_widget(loading, inner);
            return Ok(());
        }

        if let Some(err) = self.state.error() {
            let error =
                Paragraph::new(format!("Error: {}", err)).style(Style::default().fg(Color::Red));
            frame.render_widget(error, inner);
            return Ok(());
        }

        let has_namespaces = self
            .data()
            .map(|d| !d.namespaces.is_empty())
            .unwrap_or(false);
        if !has_namespaces {
            let empty =
                Paragraph::new("No workloads found").style(Style::default().fg(Color::DarkGray));
            frame.render_widget(empty, inner);
            return Ok(());
        }

        // Layout
        let chunks = Layout::vertical([
            Constraint::Length(2), // Summary
            Constraint::Fill(1),   // Content (list or drill-down)
            Constraint::Length(1), // Footer
        ])
        .split(inner);

        self.draw_summary(frame, chunks[0]);

        if self.drill_down {
            self.draw_drill_down(frame, chunks[1]);
        } else {
            self.draw_namespace_list(frame, chunks[1]);
        }

        self.draw_footer(frame, chunks[2]);

        Ok(())
    }
}
