//! Rolling operations component
//!
//! Allows performing drain/reboot operations across multiple nodes sequentially
//! with health checks between each node.

use crate::action::Action;
use crate::audit::{audit_failure, audit_start, audit_success};
use crate::components::Component;
use crate::components::diagnostics::k8s::{
    DrainOptions, DrainProgressCallback, NodeReadyProgressCallback, cordon_node,
    drain_node_with_progress, uncordon_node, wait_for_node_ready,
};
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent};
use kube::Client;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::sync::{Arc, Mutex};
use talos_rs::TalosClient;
use tokio::task::JoinHandle;

/// Node information for rolling operations
#[derive(Debug, Clone)]
pub struct RollingNodeInfo {
    /// Node hostname
    pub hostname: String,
    /// Node IP address
    pub address: String,
    /// Whether this is a control plane node
    pub is_controlplane: bool,
    /// Selection order (None = not selected, Some(n) = nth node to process)
    pub selection_order: Option<usize>,
}

/// State of the rolling operation
#[derive(Debug, Clone, PartialEq)]
pub enum RollingState {
    /// Selecting nodes
    Selecting,
    /// Confirming the operation
    Confirming(RollingOperationType),
    /// Operation in progress
    InProgress {
        operation: RollingOperationType,
        current_node_idx: usize,
        message: String,
    },
    /// Operation completed
    Completed {
        operation: RollingOperationType,
        success: bool,
        completed_nodes: usize,
        failed_node: Option<String>,
        message: String,
    },
}

/// Type of rolling operation
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RollingOperationType {
    Drain,
    Reboot,
}

impl RollingOperationType {
    fn name(&self) -> &'static str {
        match self {
            RollingOperationType::Drain => "Rolling Drain",
            RollingOperationType::Reboot => "Rolling Reboot",
        }
    }
}

/// Result of a rolling operation
#[derive(Debug, Clone)]
pub struct RollingOperationResult {
    pub success: bool,
    pub completed_nodes: usize,
    pub failed_node: Option<String>,
    pub message: String,
}

/// Shared progress for rolling operations
#[derive(Debug, Clone, Default)]
pub struct RollingProgress {
    pub current_node_idx: usize,
    pub message: String,
}

/// Rolling operations component
pub struct RollingOperationsComponent {
    /// List of nodes
    nodes: Vec<RollingNodeInfo>,
    /// Currently highlighted node (for selection)
    cursor: usize,
    /// Current state
    state: RollingState,
    /// Drain options
    drain_options: DrainOptions,
    /// Delay between nodes (seconds)
    delay_between_nodes_secs: u64,
    /// Stop on failure
    stop_on_failure: bool,
    /// Background task handle
    operation_task: Option<JoinHandle<RollingOperationResult>>,
    /// Shared progress state
    operation_progress: Arc<Mutex<RollingProgress>>,
    /// K8s client
    k8s_client: Option<Client>,
    /// Talos client
    talos_client: Option<TalosClient>,
}

impl Default for RollingOperationsComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl RollingOperationsComponent {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            cursor: 0,
            state: RollingState::Selecting,
            drain_options: DrainOptions::default(),
            delay_between_nodes_secs: 30,
            stop_on_failure: true,
            operation_task: None,
            operation_progress: Arc::new(Mutex::new(RollingProgress::default())),
            k8s_client: None,
            talos_client: None,
        }
    }

    /// Set the list of nodes
    pub fn set_nodes(&mut self, nodes: Vec<RollingNodeInfo>) {
        self.nodes = nodes;
        self.cursor = 0;
    }

    /// Set the K8s client
    pub fn set_k8s_client(&mut self, client: Client) {
        self.k8s_client = Some(client);
    }

    /// Set the Talos client
    pub fn set_talos_client(&mut self, client: TalosClient) {
        self.talos_client = Some(client);
    }

    /// Get selected nodes in selection order
    fn selected_nodes(&self) -> Vec<&RollingNodeInfo> {
        let mut selected: Vec<&RollingNodeInfo> = self
            .nodes
            .iter()
            .filter(|n| n.selection_order.is_some())
            .collect();
        selected.sort_by_key(|n| n.selection_order);
        selected
    }

    /// Get the count of selected nodes
    fn selected_count(&self) -> usize {
        self.nodes
            .iter()
            .filter(|n| n.selection_order.is_some())
            .count()
    }

    /// Toggle selection of the current node
    fn toggle_current(&mut self) {
        // Calculate next order before taking mutable borrow
        let next_order = self.selected_count() + 1;
        let cursor = self.cursor;

        if let Some(node) = self.nodes.get_mut(cursor) {
            if node.selection_order.is_some() {
                // Deselecting - remove order and renumber remaining
                let removed_order = node.selection_order.take();
                if let Some(removed) = removed_order {
                    // Renumber nodes with higher order numbers
                    for n in &mut self.nodes {
                        if let Some(order) = n.selection_order
                            && order > removed
                        {
                            n.selection_order = Some(order - 1);
                        }
                    }
                }
            } else {
                // Selecting - assign next order number
                node.selection_order = Some(next_order);
            }
        }
    }

    /// Start a rolling operation
    fn start_operation(&mut self, operation: RollingOperationType) {
        // Get selected nodes sorted by selection order
        let mut selected: Vec<RollingNodeInfo> = self
            .nodes
            .iter()
            .filter(|n| n.selection_order.is_some())
            .cloned()
            .collect();
        selected.sort_by_key(|n| n.selection_order);

        if selected.is_empty() {
            return;
        }

        // Reset progress
        {
            let mut progress = self.operation_progress.lock().unwrap();
            progress.current_node_idx = 0;
            progress.message = "Starting...".to_string();
        }

        let progress = self.operation_progress.clone();
        let options = self.drain_options.clone();
        let delay = self.delay_between_nodes_secs;
        let stop_on_failure = self.stop_on_failure;
        let k8s_client = self.k8s_client.clone();
        let talos_client = self.talos_client.clone();

        let task = tokio::spawn(async move {
            run_rolling_operation(
                progress,
                selected,
                operation,
                options,
                delay,
                stop_on_failure,
                k8s_client,
                talos_client,
            )
            .await
        });

        self.operation_task = Some(task);
        self.state = RollingState::InProgress {
            operation,
            current_node_idx: 0,
            message: "Starting...".to_string(),
        };
    }

    /// Poll the background operation
    fn poll_operation(&mut self) {
        if let Some(task) = &mut self.operation_task {
            if task.is_finished() {
                let task = self.operation_task.take().unwrap();
                match futures::executor::block_on(task) {
                    Ok(result) => {
                        if let RollingState::InProgress { operation, .. } = &self.state {
                            self.state = RollingState::Completed {
                                operation: *operation,
                                success: result.success,
                                completed_nodes: result.completed_nodes,
                                failed_node: result.failed_node,
                                message: result.message,
                            };
                        }
                    }
                    Err(e) => {
                        if let RollingState::InProgress { operation, .. } = &self.state {
                            self.state = RollingState::Completed {
                                operation: *operation,
                                success: false,
                                completed_nodes: 0,
                                failed_node: None,
                                message: format!("Task error: {}", e),
                            };
                        }
                    }
                }
            } else {
                // Update progress from shared state
                if let RollingState::InProgress { operation, .. } = &self.state {
                    let progress = self.operation_progress.lock().unwrap();
                    self.state = RollingState::InProgress {
                        operation: *operation,
                        current_node_idx: progress.current_node_idx,
                        message: progress.message.clone(),
                    };
                }
            }
        }
    }
}

/// Run the rolling operation across all selected nodes
#[allow(clippy::too_many_arguments)]
async fn run_rolling_operation(
    progress: Arc<Mutex<RollingProgress>>,
    nodes: Vec<RollingNodeInfo>,
    operation: RollingOperationType,
    options: DrainOptions,
    delay_secs: u64,
    stop_on_failure: bool,
    k8s_client: Option<Client>,
    talos_client: Option<TalosClient>,
) -> RollingOperationResult {
    use talos_rs::RebootMode;

    let total = nodes.len();
    let op_name = operation.name();

    audit_start(
        op_name,
        &format!("{} nodes", total),
        "Starting rolling operation",
    );

    let Some(k8s) = k8s_client else {
        audit_failure(op_name, "cluster", "No K8s client available");
        return RollingOperationResult {
            success: false,
            completed_nodes: 0,
            failed_node: None,
            message: "No K8s client available".to_string(),
        };
    };

    let mut completed = 0;

    for (idx, node) in nodes.iter().enumerate() {
        // Update progress
        {
            let mut p = progress.lock().unwrap();
            p.current_node_idx = idx;
            p.message = format!("Processing {}/{}: {}", idx + 1, total, node.hostname);
        }

        // Step 1: Cordon
        {
            let mut p = progress.lock().unwrap();
            p.message = format!("Cordoning {} ({}/{})", node.hostname, idx + 1, total);
        }

        if let Err(e) = cordon_node(&k8s, &node.hostname).await {
            let msg = format!("Failed to cordon {}: {}", node.hostname, e);
            audit_failure(op_name, &node.hostname, &msg);
            if stop_on_failure {
                return RollingOperationResult {
                    success: false,
                    completed_nodes: completed,
                    failed_node: Some(node.hostname.clone()),
                    message: msg,
                };
            }
            continue;
        }

        // Step 2: Drain
        {
            let mut p = progress.lock().unwrap();
            p.message = format!("Draining {} ({}/{})", node.hostname, idx + 1, total);
        }

        let progress_clone = progress.clone();
        let hostname_clone = node.hostname.clone();
        let callback: DrainProgressCallback = Box::new(move |msg: &str| {
            if let Ok(mut p) = progress_clone.lock() {
                p.message = format!("{}: {}", hostname_clone, msg);
            }
        });

        let drain_result =
            drain_node_with_progress(&k8s, &node.hostname, &options, Some(callback)).await;
        match drain_result {
            Ok(result) if !result.success => {
                let _ = uncordon_node(&k8s, &node.hostname).await;
                let msg = format!(
                    "Drain failed for {}: {:?}",
                    node.hostname, result.failed_pods
                );
                audit_failure(op_name, &node.hostname, &msg);
                if stop_on_failure {
                    return RollingOperationResult {
                        success: false,
                        completed_nodes: completed,
                        failed_node: Some(node.hostname.clone()),
                        message: msg,
                    };
                }
                continue;
            }
            Err(e) => {
                let _ = uncordon_node(&k8s, &node.hostname).await;
                let msg = format!("Drain error for {}: {}", node.hostname, e);
                audit_failure(op_name, &node.hostname, &msg);
                if stop_on_failure {
                    return RollingOperationResult {
                        success: false,
                        completed_nodes: completed,
                        failed_node: Some(node.hostname.clone()),
                        message: msg,
                    };
                }
                continue;
            }
            _ => {}
        }

        // Step 3: Reboot (if reboot operation)
        if operation == RollingOperationType::Reboot {
            {
                let mut p = progress.lock().unwrap();
                p.message = format!("Rebooting {} ({}/{})", node.hostname, idx + 1, total);
            }

            if let Some(ref client) = talos_client {
                let node_client = client.with_node(&node.address);
                if let Err(e) = node_client.reboot(RebootMode::Default).await {
                    let _ = uncordon_node(&k8s, &node.hostname).await;
                    let msg = format!("Reboot failed for {}: {}", node.hostname, e);
                    audit_failure(op_name, &node.hostname, &msg);
                    if stop_on_failure {
                        return RollingOperationResult {
                            success: false,
                            completed_nodes: completed,
                            failed_node: Some(node.hostname.clone()),
                            message: msg,
                        };
                    }
                    continue;
                }

                // Wait for node to come back
                if options.wait_for_node_ready {
                    {
                        let mut p = progress.lock().unwrap();
                        p.message = format!(
                            "Waiting for {} to come back ({}/{})",
                            node.hostname,
                            idx + 1,
                            total
                        );
                    }

                    let progress_clone = progress.clone();
                    let hostname_clone = node.hostname.clone();
                    let ready_callback: NodeReadyProgressCallback = Box::new(move |msg: &str| {
                        if let Ok(mut p) = progress_clone.lock() {
                            p.message = format!("{}: {}", hostname_clone, msg);
                        }
                    });

                    let ready_result = wait_for_node_ready(
                        &k8s,
                        &node.hostname,
                        options.post_reboot_timeout_secs,
                        true,
                        Some(ready_callback),
                    )
                    .await;

                    match ready_result {
                        Ok(result) if !result.success => {
                            let msg = format!(
                                "Node {} didn't become Ready: {:?}",
                                node.hostname, result.error
                            );
                            audit_failure(op_name, &node.hostname, &msg);
                            if stop_on_failure {
                                return RollingOperationResult {
                                    success: false,
                                    completed_nodes: completed,
                                    failed_node: Some(node.hostname.clone()),
                                    message: msg,
                                };
                            }
                            continue;
                        }
                        Err(e) => {
                            let msg = format!("Error waiting for {}: {}", node.hostname, e);
                            audit_failure(op_name, &node.hostname, &msg);
                            if stop_on_failure {
                                return RollingOperationResult {
                                    success: false,
                                    completed_nodes: completed,
                                    failed_node: Some(node.hostname.clone()),
                                    message: msg,
                                };
                            }
                            continue;
                        }
                        _ => {}
                    }

                    // Uncordon after successful reboot
                    if options.uncordon_after_reboot {
                        let _ = uncordon_node(&k8s, &node.hostname).await;
                    }
                }
            } else {
                let _ = uncordon_node(&k8s, &node.hostname).await;
                let msg = "No Talos client for reboot".to_string();
                audit_failure(op_name, &node.hostname, &msg);
                if stop_on_failure {
                    return RollingOperationResult {
                        success: false,
                        completed_nodes: completed,
                        failed_node: Some(node.hostname.clone()),
                        message: msg,
                    };
                }
                continue;
            }
        }

        completed += 1;
        audit_success(
            op_name,
            &node.hostname,
            &format!("Node {}/{} completed", completed, total),
        );

        // Delay between nodes (if not the last node)
        if idx < total - 1 && delay_secs > 0 {
            {
                let mut p = progress.lock().unwrap();
                p.message = format!("Waiting {}s before next node...", delay_secs);
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(delay_secs)).await;
        }
    }

    let msg = format!("Completed {}/{} nodes", completed, total);
    audit_success(op_name, "cluster", &msg);

    RollingOperationResult {
        success: completed == total,
        completed_nodes: completed,
        failed_node: None,
        message: msg,
    }
}

impl Component for RollingOperationsComponent {
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match &self.state {
            RollingState::Selecting => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => Ok(Some(Action::Back)),
                KeyCode::Up | KeyCode::Char('k') => {
                    if self.cursor > 0 {
                        self.cursor -= 1;
                    }
                    Ok(None)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if self.cursor < self.nodes.len().saturating_sub(1) {
                        self.cursor += 1;
                    }
                    Ok(None)
                }
                KeyCode::Char(' ') | KeyCode::Enter => {
                    self.toggle_current();
                    Ok(None)
                }
                KeyCode::Char('d') => {
                    if !self.selected_nodes().is_empty() {
                        self.state = RollingState::Confirming(RollingOperationType::Drain);
                    }
                    Ok(None)
                }
                KeyCode::Char('r') => {
                    if !self.selected_nodes().is_empty() {
                        self.state = RollingState::Confirming(RollingOperationType::Reboot);
                    }
                    Ok(None)
                }
                _ => Ok(None),
            },
            RollingState::Confirming(op_type) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.start_operation(*op_type);
                    Ok(None)
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.state = RollingState::Selecting;
                    Ok(None)
                }
                _ => Ok(None),
            },
            RollingState::InProgress { .. } => {
                self.poll_operation();
                Ok(None)
            }
            RollingState::Completed { .. } => Ok(Some(Action::Back)),
        }
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if matches!(action, Action::Tick) && matches!(self.state, RollingState::InProgress { .. }) {
            self.poll_operation();
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        let overlay_width = 60.min(area.width.saturating_sub(4));
        let overlay_height = 20.min(area.height.saturating_sub(4));
        let x = (area.width.saturating_sub(overlay_width)) / 2;
        let y = (area.height.saturating_sub(overlay_height)) / 2;
        let overlay_area = Rect::new(x, y, overlay_width, overlay_height);

        frame.render_widget(Clear, overlay_area);

        match &self.state {
            RollingState::Selecting => {
                self.draw_selection(frame, overlay_area);
            }
            RollingState::Confirming(op_type) => {
                self.draw_confirmation(frame, overlay_area, *op_type);
            }
            RollingState::InProgress {
                operation,
                current_node_idx,
                message,
            } => {
                self.draw_progress(frame, overlay_area, *operation, *current_node_idx, message);
            }
            RollingState::Completed {
                operation,
                success,
                completed_nodes,
                failed_node,
                message,
            } => {
                self.draw_completed(
                    frame,
                    overlay_area,
                    *operation,
                    *success,
                    *completed_nodes,
                    failed_node.as_deref(),
                    message,
                );
            }
        }

        Ok(())
    }
}

impl RollingOperationsComponent {
    fn draw_selection(&self, frame: &mut Frame, area: Rect) {
        let mut lines = Vec::new();
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "  Select nodes (order shown by number):",
            Style::default().fg(Color::Yellow),
        )]));
        lines.push(Line::from(""));

        for (idx, node) in self.nodes.iter().enumerate() {
            // Show order number if selected, empty brackets if not
            let checkbox = match node.selection_order {
                Some(order) => format!("[{}]", order),
                None => "[ ]".to_string(),
            };
            let is_current = idx == self.cursor;
            let is_selected = node.selection_order.is_some();

            let style = if is_current {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if is_selected {
                Style::default().fg(Color::Green)
            } else {
                Style::default()
            };

            let role_style = if node.is_controlplane {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Green)
            };

            let role = if node.is_controlplane { "CP" } else { "W" };
            let prefix = if is_current { "> " } else { "  " };

            lines.push(Line::from(vec![
                Span::raw(prefix),
                Span::styled(checkbox, style),
                Span::raw(" "),
                Span::styled(&node.hostname, style),
                Span::raw(" "),
                Span::styled(format!("[{}]", role), role_style),
            ]));
        }

        let selected_count = self.selected_count();
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("{} nodes selected", selected_count),
                Style::default().fg(Color::Cyan),
            ),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  [Space]", Style::default().fg(Color::Green)),
            Span::raw(" Toggle  "),
            Span::styled("[d]", Style::default().fg(Color::Yellow)),
            Span::raw(" Rolling Drain  "),
            Span::styled("[r]", Style::default().fg(Color::Red)),
            Span::raw(" Rolling Reboot"),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  [q]", Style::default().fg(Color::DarkGray)),
            Span::raw(" Cancel"),
        ]));

        let block = Block::default()
            .title(" Rolling Operations ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));

        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, area);
    }

    fn draw_confirmation(&self, frame: &mut Frame, area: Rect, op_type: RollingOperationType) {
        let selected = self.selected_nodes();
        let mut lines = Vec::new();
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            format!("  {} {} nodes?", op_type.name(), selected.len()),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]));
        lines.push(Line::from(""));

        for node in selected.iter().take(5) {
            lines.push(Line::from(vec![
                Span::raw("    - "),
                Span::styled(&node.hostname, Style::default().fg(Color::Cyan)),
            ]));
        }
        if selected.len() > 5 {
            lines.push(Line::from(vec![Span::styled(
                format!("    ... and {} more", selected.len() - 5),
                Style::default().fg(Color::DarkGray),
            )]));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  [y]", Style::default().fg(Color::Green)),
            Span::raw(" Confirm  "),
            Span::styled("[n]", Style::default().fg(Color::Red)),
            Span::raw(" Cancel"),
        ]));

        let block = Block::default()
            .title(format!(" Confirm {} ", op_type.name()))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow));

        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, area);
    }

    fn draw_progress(
        &self,
        frame: &mut Frame,
        area: Rect,
        operation: RollingOperationType,
        current_idx: usize,
        message: &str,
    ) {
        let total = self.selected_nodes().len();
        let mut lines = Vec::new();
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            format!("  {} in progress...", operation.name()),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw("  Progress: "),
            Span::styled(
                format!("{}/{}", current_idx + 1, total),
                Style::default().fg(Color::Cyan),
            ),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            format!("  {}", message),
            Style::default().fg(Color::White),
        )]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "  Please wait...",
            Style::default().fg(Color::DarkGray),
        )]));

        let block = Block::default()
            .title(format!(" {} ", operation.name()))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow));

        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, area);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_completed(
        &self,
        frame: &mut Frame,
        area: Rect,
        operation: RollingOperationType,
        success: bool,
        completed: usize,
        failed_node: Option<&str>,
        message: &str,
    ) {
        let (status_icon, status_color) = if success {
            ("Success", Color::Green)
        } else {
            ("Failed", Color::Red)
        };

        let mut lines = Vec::new();
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            format!("  {} {}", operation.name(), status_icon),
            Style::default()
                .fg(status_color)
                .add_modifier(Modifier::BOLD),
        )]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw("  Completed: "),
            Span::styled(format!("{}", completed), Style::default().fg(Color::Cyan)),
            Span::raw(" nodes"),
        ]));

        if let Some(failed) = failed_node {
            lines.push(Line::from(vec![
                Span::raw("  Failed at: "),
                Span::styled(failed, Style::default().fg(Color::Red)),
            ]));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::raw(format!("  {}", message))]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "  Press any key to continue...",
            Style::default().fg(Color::DarkGray),
        )]));

        let block = Block::default()
            .title(format!(" {} Complete ", operation.name()))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(status_color));

        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, area);
    }
}
