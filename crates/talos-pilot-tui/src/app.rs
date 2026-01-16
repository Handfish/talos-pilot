//! Application state and main loop

use crate::action::Action;
use crate::components::rolling_operations::RollingNodeInfo;
use crate::components::wizard::{WizardComponent, WizardState};
use crate::components::{
    ClusterComponent, Component, DiagnosticsComponent, EtcdComponent, LifecycleComponent,
    MultiLogsComponent, NetworkStatsComponent, NodeOperationsComponent, ProcessesComponent,
    RollingOperationsComponent, SecurityComponent, StorageComponent, WorkloadHealthComponent,
};
use crate::tui::{self, Tui};
use color_eyre::Result;
use crossterm::event::{self, Event, KeyEventKind};
use std::time::Duration;
use tokio::sync::mpsc;

/// Current view in the application
#[derive(Debug, Clone, PartialEq)]
enum View {
    Cluster,
    MultiLogs,
    Etcd,
    Processes,
    Network,
    Diagnostics,
    Security,
    Lifecycle,
    Workloads,
    Storage,
    NodeOperations,
    RollingOperations,
}

/// Main application state
pub struct App {
    /// Whether the application should quit
    should_quit: bool,
    /// Current view
    view: View,
    /// Cluster component
    cluster: ClusterComponent,
    /// Multi-service logs component (created when viewing logs)
    multi_logs: Option<MultiLogsComponent>,
    /// Etcd status component (created when viewing etcd)
    etcd: Option<EtcdComponent>,
    /// Processes component (created when viewing processes)
    processes: Option<ProcessesComponent>,
    /// Network stats component (created when viewing network)
    network: Option<NetworkStatsComponent>,
    /// Diagnostics component (created when viewing diagnostics)
    diagnostics: Option<DiagnosticsComponent>,
    /// Security component (created when viewing certificates)
    security: Option<SecurityComponent>,
    /// Lifecycle component (created when viewing versions)
    lifecycle: Option<LifecycleComponent>,
    /// Workload health component (created when viewing workloads)
    workloads: Option<WorkloadHealthComponent>,
    /// Storage component (created when viewing disks/volumes)
    storage: Option<StorageComponent>,
    /// Node operations component (overlay for node operations)
    node_operations: Option<NodeOperationsComponent>,
    /// Rolling operations component (overlay for multi-node operations)
    rolling_operations: Option<RollingOperationsComponent>,
    /// Number of log lines to fetch per service
    tail_lines: i32,
    /// Tick rate for animations (ms)
    tick_rate: Duration,
    /// Channel for async action results
    action_rx: mpsc::UnboundedReceiver<AsyncResult>,
    #[allow(dead_code)] // Will be used for background log streaming
    action_tx: mpsc::UnboundedSender<AsyncResult>,
    /// Custom config file path (from --config flag)
    config_path: Option<String>,
    /// Whether running in insecure mode (no TLS)
    insecure: bool,
    /// Endpoint for insecure mode
    insecure_endpoint: Option<String>,
}

/// Results from async operations
#[derive(Debug)]
#[allow(dead_code)]
enum AsyncResult {
    Connected,
    Refreshed,
    LogsLoaded(String),
    Error(String),
}

impl Default for App {
    fn default() -> Self {
        Self::new(None, None, 500, false, None)
    }
}

impl App {
    pub fn new(
        config_path: Option<String>,
        context: Option<String>,
        tail_lines: i32,
        insecure: bool,
        insecure_endpoint: Option<String>,
    ) -> Self {
        let (action_tx, action_rx) = mpsc::unbounded_channel();
        Self {
            should_quit: false,
            view: View::Cluster,
            cluster: ClusterComponent::new(config_path.clone(), context),
            multi_logs: None,
            etcd: None,
            processes: None,
            network: None,
            diagnostics: None,
            security: None,
            lifecycle: None,
            workloads: None,
            storage: None,
            node_operations: None,
            rolling_operations: None,
            tail_lines,
            tick_rate: Duration::from_millis(100),
            action_rx,
            action_tx,
            config_path,
            insecure,
            insecure_endpoint,
        }
    }

    /// Run the application
    pub async fn run(&mut self) -> Result<()> {
        // Install panic hook
        tui::install_panic_hook();

        // Initialize terminal
        let mut terminal = tui::init()?;

        // Main loop - choose based on mode
        let result = if self.insecure {
            self.insecure_loop(&mut terminal).await
        } else {
            self.main_loop(&mut terminal).await
        };

        // Restore terminal
        tui::restore()?;

        result
    }

    /// Insecure mode event loop - Bootstrap Wizard
    async fn insecure_loop(&mut self, terminal: &mut Tui) -> Result<()> {
        let endpoint = self
            .insecure_endpoint
            .clone()
            .expect("Insecure mode requires endpoint");

        let mut wizard = WizardComponent::new(endpoint);

        // Connect on startup
        wizard.connect().await?;

        // Polling interval for wait states
        let poll_interval = Duration::from_secs(5);
        let mut last_poll = std::time::Instant::now();

        // Spinner animation interval
        let spinner_interval = Duration::from_millis(100);
        let mut last_spinner = std::time::Instant::now();

        loop {
            // Advance spinner for animations
            if last_spinner.elapsed() >= spinner_interval {
                last_spinner = std::time::Instant::now();
                wizard.data_mut().advance_spinner();
            }

            // Draw
            terminal.draw(|frame| {
                let _ = wizard.draw(frame, frame.area());
            })?;

            // Handle events with timeout
            if event::poll(self.tick_rate)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if let Some(action) = wizard.handle_key_event(key)? {
                            match action {
                                Action::Quit => {
                                    self.should_quit = true;
                                }
                                Action::WizardGenConfig => {
                                    self.wizard_generate_config(&mut wizard).await;
                                }
                                Action::WizardApplyConfig => {
                                    self.wizard_apply_config(&mut wizard).await;
                                }
                                Action::WizardBootstrap => {
                                    self.wizard_bootstrap(&mut wizard).await;
                                }
                                Action::WizardRetry => {
                                    wizard.connect().await?;
                                }
                                Action::WizardComplete(context) => {
                                    // Exit wizard - print instructions
                                    self.should_quit = true;
                                    if let Some(ctx) = context {
                                        tracing::info!("Wizard complete. Context: {}", ctx);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Event::Resize(_, _) => {
                        // Terminal will automatically resize on next draw
                    }
                    _ => {}
                }
            }

            // Polling for wait states
            if last_poll.elapsed() >= poll_interval {
                last_poll = std::time::Instant::now();
                self.wizard_poll(&mut wizard).await;
            }

            if self.should_quit {
                break;
            }
        }

        Ok(())
    }

    /// Generate config in wizard
    async fn wizard_generate_config(&self, wizard: &mut WizardComponent) {
        use talos_rs::gen_config;

        // Extract data we need before any mutations
        let cluster_name = wizard.data().cluster_name.clone();
        let k8s_endpoint = wizard.data().k8s_endpoint.clone();
        let output_dir = wizard.data().output_dir.clone();
        let endpoint = wizard.data().endpoint.clone();
        // TODO: Use disk with gen_config_with_disk when --install-disk flag support is added
        let _disk = wizard
            .data()
            .selected_disk
            .as_ref()
            .map(|d| d.dev_path.clone());

        // Build additional SANs
        let sans: Vec<&str> = vec![&endpoint, "127.0.0.1"];

        // Generate config
        // Note: For now we use gen_config without disk selection
        // TODO: Add gen_config_with_disk that uses --install-disk flag
        match gen_config(&cluster_name, &k8s_endpoint, &output_dir, Some(&sans), true).await {
            Ok(result) => {
                // Merge talosconfig and set endpoint/node
                let merge_success = self
                    .wizard_merge_config(&result.talosconfig_path, &cluster_name, &endpoint)
                    .await;

                if merge_success {
                    wizard.data_mut().config_result = Some(result);
                    wizard.data_mut().context_name = Some(cluster_name);
                    wizard.transition(WizardState::ConfigReady);
                } else {
                    wizard.set_error("Failed to merge talosconfig".to_string());
                }
            }
            Err(e) => {
                wizard.set_error(format!("Failed to generate config: {}", e));
            }
        }
    }

    /// Merge talosconfig into user's config
    async fn wizard_merge_config(
        &self,
        config_path: &str,
        context_name: &str,
        endpoint: &str,
    ) -> bool {
        use tokio::process::Command;

        // Run: talosctl config merge <path>
        let merge_output = Command::new("talosctl")
            .args(["config", "merge", config_path])
            .output()
            .await;

        if let Err(e) = &merge_output {
            tracing::warn!("Failed to run talosctl config merge: {}", e);
            return false;
        }

        if let Ok(out) = &merge_output {
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                tracing::warn!("Failed to merge config: {}", stderr);
                return false;
            }
        }

        // Set the endpoint on the context
        let endpoint_output = Command::new("talosctl")
            .args(["--context", context_name, "config", "endpoint", endpoint])
            .output()
            .await;

        if let Err(e) = &endpoint_output {
            tracing::warn!("Failed to set endpoint: {}", e);
            return false;
        }

        // Set the node on the context
        let node_output = Command::new("talosctl")
            .args(["--context", context_name, "config", "node", endpoint])
            .output()
            .await;

        if let Err(e) = &node_output {
            tracing::warn!("Failed to set node: {}", e);
            return false;
        }

        true
    }

    /// Apply config in wizard
    async fn wizard_apply_config(&self, wizard: &mut WizardComponent) {
        use std::time::Instant;
        use talos_rs::apply_config_insecure;

        wizard.transition(WizardState::Applying);

        // Extract values we need before mutating
        let endpoint = wizard.data().endpoint.clone();
        let cluster_name = wizard.data().cluster_name.clone();
        let config_path = {
            let data = wizard.data();
            data.config_result.as_ref().map(|r| match data.node_type {
                crate::components::wizard::NodeType::Controlplane => r.controlplane_path.clone(),
                crate::components::wizard::NodeType::Worker => r.worker_path.clone(),
            })
        };

        if let Some(path) = config_path {
            match apply_config_insecure(&endpoint, &path).await {
                Ok(result) => {
                    if result.success {
                        // Start waiting for reboot
                        wizard.data_mut().wait_started = Some(Instant::now());
                        wizard.data_mut().context_name = Some(cluster_name);
                        wizard.transition(WizardState::WaitingReboot);
                    } else {
                        wizard.set_error(result.message);
                    }
                }
                Err(e) => {
                    wizard.set_error(format!("Failed to apply config: {}", e));
                }
            }
        } else {
            wizard.set_error("No config generated".to_string());
        }
    }

    /// Bootstrap cluster in wizard
    async fn wizard_bootstrap(&self, wizard: &mut WizardComponent) {
        use std::time::Instant;
        use tokio::process::Command;

        wizard.transition(WizardState::Bootstrapping);

        let context = wizard.data().context_name.clone();

        if let Some(ctx) = context {
            let output = Command::new("talosctl")
                .args(["--context", &ctx, "bootstrap"])
                .output()
                .await;

            match output {
                Ok(out) if out.status.success() => {
                    wizard.data_mut().wait_started = Some(Instant::now());
                    wizard.transition(WizardState::WaitingHealthy);
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    wizard.set_error(format!("Bootstrap failed: {}", stderr));
                }
                Err(e) => {
                    wizard.set_error(format!("Failed to run bootstrap: {}", e));
                }
            }
        } else {
            wizard.set_error("No context available for bootstrap".to_string());
        }
    }

    /// Poll for state changes in wait states
    async fn wizard_poll(&self, wizard: &mut WizardComponent) {
        use tokio::process::Command;

        match wizard.state() {
            WizardState::WaitingReboot => {
                // Increment poll attempts
                wizard.data_mut().poll_attempts += 1;

                // Check if node is back online (with TLS)
                if let Some(ctx) = wizard.data().context_name.clone() {
                    let output = Command::new("talosctl")
                        .args(["--context", &ctx, "version"])
                        .output()
                        .await;

                    match output {
                        Ok(out) if out.status.success() => {
                            wizard.data_mut().last_poll_error = None;
                            wizard.transition(WizardState::ReadyToBootstrap);
                        }
                        Ok(out) => {
                            // Command ran but failed - capture error
                            let stderr = String::from_utf8_lossy(&out.stderr);
                            wizard.data_mut().last_poll_error =
                                Some(stderr.lines().next().unwrap_or("Unknown error").to_string());
                        }
                        Err(e) => {
                            wizard.data_mut().last_poll_error = Some(e.to_string());
                        }
                    }
                }

                // Check for timeout (5 minutes)
                if let Some(started) = wizard.data().wait_started
                    && started.elapsed().as_secs() > 300
                {
                    wizard.set_error("Timeout waiting for node to reboot".to_string());
                }
            }
            WizardState::WaitingHealthy => {
                // Increment poll attempts
                wizard.data_mut().poll_attempts += 1;

                // Check if cluster is healthy
                if let Some(ctx) = wizard.data().context_name.clone() {
                    // Check etcd health
                    let output = Command::new("talosctl")
                        .args(["--context", &ctx, "etcd", "status"])
                        .output()
                        .await;

                    match output {
                        Ok(out) if out.status.success() => {
                            wizard.data_mut().last_poll_error = None;
                            wizard.transition(WizardState::Complete);
                        }
                        Ok(out) => {
                            let stderr = String::from_utf8_lossy(&out.stderr);
                            wizard.data_mut().last_poll_error =
                                Some(stderr.lines().next().unwrap_or("Unknown error").to_string());
                        }
                        Err(e) => {
                            wizard.data_mut().last_poll_error = Some(e.to_string());
                        }
                    }
                }

                // Check for timeout (5 minutes)
                if let Some(started) = wizard.data().wait_started
                    && started.elapsed().as_secs() > 300
                {
                    wizard.set_error("Timeout waiting for cluster to become healthy".to_string());
                }
            }
            _ => {}
        }
    }

    /// Main event loop
    async fn main_loop(&mut self, terminal: &mut Tui) -> Result<()> {
        // Connect on startup
        self.cluster.connect().await?;

        loop {
            // Draw current view
            terminal.draw(|frame| {
                let area = frame.area();
                match self.view {
                    View::Cluster => {
                        let _ = self.cluster.draw(frame, area);
                    }
                    View::MultiLogs => {
                        if let Some(multi_logs) = &mut self.multi_logs {
                            let _ = multi_logs.draw(frame, area);
                        }
                    }
                    View::Etcd => {
                        if let Some(etcd) = &mut self.etcd {
                            let _ = etcd.draw(frame, area);
                        }
                    }
                    View::Processes => {
                        if let Some(processes) = &mut self.processes {
                            let _ = processes.draw(frame, area);
                        }
                    }
                    View::Network => {
                        if let Some(network) = &mut self.network {
                            let _ = network.draw(frame, area);
                        }
                    }
                    View::Diagnostics => {
                        if let Some(diagnostics) = &mut self.diagnostics {
                            let _ = diagnostics.draw(frame, area);
                        }
                    }
                    View::Security => {
                        if let Some(security) = &mut self.security {
                            let _ = security.draw(frame, area);
                        }
                    }
                    View::Lifecycle => {
                        if let Some(lifecycle) = &mut self.lifecycle {
                            let _ = lifecycle.draw(frame, area);
                        }
                    }
                    View::Workloads => {
                        if let Some(workloads) = &mut self.workloads {
                            let _ = workloads.draw(frame, area);
                        }
                    }
                    View::Storage => {
                        if let Some(storage) = &mut self.storage {
                            let _ = storage.draw(frame, area);
                        }
                    }
                    View::NodeOperations => {
                        // Draw cluster in background, then overlay
                        let _ = self.cluster.draw(frame, area);
                        if let Some(node_ops) = &mut self.node_operations {
                            let _ = node_ops.draw(frame, area);
                        }
                    }
                    View::RollingOperations => {
                        // Draw cluster in background, then overlay
                        let _ = self.cluster.draw(frame, area);
                        if let Some(rolling_ops) = &mut self.rolling_operations {
                            let _ = rolling_ops.draw(frame, area);
                        }
                    }
                }
            })?;

            // Handle events with timeout
            if event::poll(self.tick_rate)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        let action = match self.view {
                            View::Cluster => self.cluster.handle_key_event(key)?,
                            View::MultiLogs => {
                                if let Some(multi_logs) = &mut self.multi_logs {
                                    multi_logs.handle_key_event(key)?
                                } else {
                                    None
                                }
                            }
                            View::Etcd => {
                                if let Some(etcd) = &mut self.etcd {
                                    etcd.handle_key_event(key)?
                                } else {
                                    None
                                }
                            }
                            View::Processes => {
                                if let Some(processes) = &mut self.processes {
                                    processes.handle_key_event(key)?
                                } else {
                                    None
                                }
                            }
                            View::Network => {
                                if let Some(network) = &mut self.network {
                                    network.handle_key_event(key)?
                                } else {
                                    None
                                }
                            }
                            View::Diagnostics => {
                                if let Some(diagnostics) = &mut self.diagnostics {
                                    diagnostics.handle_key_event(key)?
                                } else {
                                    None
                                }
                            }
                            View::Security => {
                                if let Some(security) = &mut self.security {
                                    security.handle_key_event(key)?
                                } else {
                                    None
                                }
                            }
                            View::Lifecycle => {
                                if let Some(lifecycle) = &mut self.lifecycle {
                                    lifecycle.handle_key_event(key)?
                                } else {
                                    None
                                }
                            }
                            View::Workloads => {
                                if let Some(workloads) = &mut self.workloads {
                                    workloads.handle_key_event(key)?
                                } else {
                                    None
                                }
                            }
                            View::Storage => {
                                if let Some(storage) = &mut self.storage {
                                    storage.handle_key_event(key)?
                                } else {
                                    None
                                }
                            }
                            View::NodeOperations => {
                                if let Some(node_ops) = &mut self.node_operations {
                                    node_ops.handle_key_event(key)?
                                } else {
                                    None
                                }
                            }
                            View::RollingOperations => {
                                if let Some(rolling_ops) = &mut self.rolling_operations {
                                    rolling_ops.handle_key_event(key)?
                                } else {
                                    None
                                }
                            }
                        };
                        if let Some(action) = action {
                            self.handle_action(action).await?;
                        }
                    }
                    Event::Resize(w, h) => {
                        self.handle_action(Action::Resize(w, h)).await?;
                    }
                    _ => {}
                }
            } else {
                // Tick for animations
                self.handle_action(Action::Tick).await?;
            }

            // Check async results (non-blocking)
            while let Ok(result) = self.action_rx.try_recv() {
                match result {
                    AsyncResult::Connected => {
                        tracing::info!("Connected to Talos cluster");
                    }
                    AsyncResult::Refreshed => {
                        tracing::info!("Data refreshed");
                    }
                    AsyncResult::LogsLoaded(_content) => {
                        // Legacy - multi_logs uses set_logs directly
                    }
                    AsyncResult::Error(e) => {
                        tracing::error!("Async error: {}", e);
                        if let Some(multi_logs) = &mut self.multi_logs {
                            multi_logs.set_error(e);
                        }
                    }
                }
            }

            // Check if we should quit
            if self.should_quit {
                break;
            }
        }

        Ok(())
    }

    /// Handle an action
    async fn handle_action(&mut self, action: Action) -> Result<()> {
        match action {
            Action::Quit => {
                self.should_quit = true;
            }
            Action::Back => {
                match self.view {
                    View::MultiLogs => {
                        // Stop streaming if active
                        if let Some(multi_logs) = &mut self.multi_logs {
                            multi_logs.stop_streaming();
                        }
                        self.multi_logs = None;
                    }
                    View::Etcd => {
                        self.etcd = None;
                    }
                    View::Processes => {
                        self.processes = None;
                    }
                    View::Network => {
                        self.network = None;
                    }
                    View::Diagnostics => {
                        self.diagnostics = None;
                    }
                    View::Security => {
                        self.security = None;
                    }
                    View::Lifecycle => {
                        self.lifecycle = None;
                    }
                    View::Workloads => {
                        self.workloads = None;
                    }
                    View::Storage => {
                        self.storage = None;
                    }
                    View::NodeOperations => {
                        self.node_operations = None;
                    }
                    View::RollingOperations => {
                        self.rolling_operations = None;
                    }
                    View::Cluster => {}
                }
                // Return to cluster view
                self.view = View::Cluster;
            }
            Action::Tick => {
                // Update animations, etc.
                match self.view {
                    View::Cluster => {
                        if let Some(next_action) = self.cluster.update(Action::Tick)? {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                        // Auto-refresh selected node stats every 5 seconds
                        if self.cluster.should_auto_refresh() {
                            let _ = self.cluster.refresh_selected_node().await;
                        }
                    }
                    View::MultiLogs => {
                        if let Some(multi_logs) = &mut self.multi_logs
                            && let Some(next_action) = multi_logs.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Etcd => {
                        if let Some(etcd) = &mut self.etcd
                            && let Some(next_action) = etcd.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Processes => {
                        if let Some(processes) = &mut self.processes
                            && let Some(next_action) = processes.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Network => {
                        if let Some(network) = &mut self.network
                            && let Some(next_action) = network.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Diagnostics => {
                        if let Some(diagnostics) = &mut self.diagnostics
                            && let Some(next_action) = diagnostics.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Security => {
                        if let Some(security) = &mut self.security
                            && let Some(next_action) = security.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Lifecycle => {
                        if let Some(lifecycle) = &mut self.lifecycle
                            && let Some(next_action) = lifecycle.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Workloads => {
                        if let Some(workloads) = &mut self.workloads
                            && let Some(next_action) = workloads.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Storage => {
                        if let Some(storage) = &mut self.storage
                            && let Some(next_action) = storage.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::NodeOperations => {
                        if let Some(node_ops) = &mut self.node_operations
                            && let Some(next_action) = node_ops.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::RollingOperations => {
                        if let Some(rolling_ops) = &mut self.rolling_operations
                            && let Some(next_action) = rolling_ops.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                }
            }
            Action::Resize(_w, _h) => {
                // Terminal will automatically resize on next draw
            }
            Action::Refresh => {
                tracing::info!("Refresh requested");
                match self.view {
                    View::Cluster => {
                        self.cluster.refresh().await?;
                    }
                    View::Etcd => {
                        if let Some(etcd) = &mut self.etcd
                            && let Err(e) = etcd.refresh().await
                        {
                            etcd.set_error(e.to_string());
                        }
                    }
                    View::Processes => {
                        if let Some(processes) = &mut self.processes
                            && let Err(e) = processes.refresh().await
                        {
                            processes.set_error(e.to_string());
                        }
                    }
                    View::Network => {
                        if let Some(network) = &mut self.network {
                            // Check for pending service restart first
                            if network.has_pending_restart() {
                                let _ = network.perform_pending_restart().await;
                            }
                            // Check for file viewer content fetch
                            if network.file_viewer_needs_fetch() {
                                network.fetch_file_content().await;
                            }
                            // Check for packet capture start
                            if network.needs_capture_start() {
                                network.start_capture_async().await;
                            }
                            if let Err(e) = network.refresh().await {
                                network.set_error(e.to_string());
                            }
                        }
                    }
                    View::MultiLogs => {
                        // Multi-logs handles its own streaming refresh
                    }
                    View::Diagnostics => {
                        if let Some(diagnostics) = &mut self.diagnostics
                            && let Err(e) = diagnostics.refresh().await
                        {
                            diagnostics.set_error(e.to_string());
                        }
                    }
                    View::Security => {
                        if let Some(security) = &mut self.security
                            && let Err(e) = security.refresh().await
                        {
                            security.set_error(e.to_string());
                        }
                    }
                    View::Lifecycle => {
                        if let Some(lifecycle) = &mut self.lifecycle
                            && let Err(e) = lifecycle.refresh().await
                        {
                            lifecycle.set_error(e.to_string());
                        }
                    }
                    View::Workloads => {
                        if let Some(workloads) = &mut self.workloads
                            && let Err(e) = workloads.refresh().await
                        {
                            workloads.set_error(e.to_string());
                        }
                    }
                    View::Storage => {
                        if let Some(storage) = &mut self.storage
                            && let Err(e) = storage.refresh().await
                        {
                            storage.set_error(e.to_string());
                        }
                    }
                    View::NodeOperations => {
                        if let Some(node_ops) = &mut self.node_operations
                            && let Err(e) = node_ops.refresh().await
                        {
                            node_ops.set_error(e.to_string());
                        }
                    }
                    View::RollingOperations => {
                        // Rolling operations doesn't have a refresh method
                    }
                }
            }
            Action::ShowMultiLogs(node_ip, node_role, active_services, all_services) => {
                // Switch to multi-service logs view
                tracing::info!("Viewing multi-service logs for node: {}", node_ip);

                // Create multi-logs component with all services, marking active ones
                let mut multi_logs = MultiLogsComponent::new(
                    node_ip,
                    node_role,
                    active_services.clone(),
                    all_services,
                );

                // Fetch logs from active services and set up client for streaming
                if let Some(client) = self.cluster.client() {
                    // Set the client for streaming capability
                    multi_logs.set_client(client.clone(), self.tail_lines);

                    let service_refs: Vec<&str> =
                        active_services.iter().map(|s| s.as_str()).collect();
                    match client.logs_multi(&service_refs, self.tail_lines).await {
                        Ok(logs) => {
                            multi_logs.set_logs(logs);
                            // Auto-start streaming for live updates
                            multi_logs.start_streaming();
                        }
                        Err(e) => {
                            multi_logs.set_error(e.to_string());
                        }
                    }
                }

                self.multi_logs = Some(multi_logs);
                self.view = View::MultiLogs;
            }
            Action::ShowNodeDetails(_, _) => {
                // Legacy - no longer used, we use ShowMultiLogs now
            }
            Action::ShowDiagnostics(hostname, address, role, cp_endpoint) => {
                // Switch to diagnostics view for a node
                tracing::info!(
                    "ShowDiagnostics: hostname='{}', address='{}', role='{}', cp_endpoint={:?}",
                    hostname,
                    address,
                    role,
                    cp_endpoint
                );

                // Create diagnostics component
                let mut diagnostics = DiagnosticsComponent::new(
                    hostname,
                    address.clone(),
                    role,
                    self.config_path.clone(),
                );

                // Set the control plane endpoint for worker nodes to fetch kubeconfig
                diagnostics.set_controlplane_endpoint(cp_endpoint);

                // Set the client and refresh data
                if let Some(client) = self.cluster.client() {
                    // Create a client configured for this specific node
                    let node_client = client.with_node(&address);
                    diagnostics.set_client(node_client);
                    if let Err(e) = diagnostics.refresh().await {
                        tracing::error!("Diagnostics refresh error: {:?}", e);
                        diagnostics.set_error(e.to_string());
                    }
                }

                self.diagnostics = Some(diagnostics);
                self.view = View::Diagnostics;
            }
            Action::ApplyDiagnosticFix => {
                // Apply a diagnostic fix (from confirmation dialog)
                if let Some(diagnostics) = &mut self.diagnostics {
                    if let Err(e) = diagnostics.apply_pending_fix().await {
                        diagnostics.set_error(e.to_string());
                    }
                    // Refresh after applying fix
                    if let Err(e) = diagnostics.refresh().await {
                        diagnostics.set_error(e.to_string());
                    }
                }
            }
            Action::ShowEtcd => {
                // Switch to etcd status view
                tracing::info!("Viewing etcd cluster status");

                // Create etcd component
                let mut etcd = EtcdComponent::new();

                // Set the client and refresh data
                if let Some(client) = self.cluster.client() {
                    etcd.set_client(client.clone());
                    if let Err(e) = etcd.refresh().await {
                        etcd.set_error(e.to_string());
                    }
                }

                self.etcd = Some(etcd);
                self.view = View::Etcd;
            }
            Action::ShowProcesses(hostname, address) => {
                // Switch to processes view for a node
                tracing::info!(
                    "ShowProcesses: hostname='{}', address='{}'",
                    hostname,
                    address
                );

                // Create processes component
                let mut processes = ProcessesComponent::new(hostname, address.clone());

                // Set the client and refresh data
                if let Some(client) = self.cluster.client() {
                    // Create a client configured for this specific node
                    let node_client = client.with_node(&address);
                    tracing::info!("Created node client for address: '{}'", address);
                    processes.set_client(node_client);
                    if let Err(e) = processes.refresh().await {
                        tracing::error!("Process refresh error: {:?}", e);
                        processes.set_error(e.to_string());
                    }
                }

                self.processes = Some(processes);
                self.view = View::Processes;
            }
            Action::ShowNetwork(hostname, address) => {
                // Switch to network stats view for a node
                tracing::info!(
                    "ShowNetwork: hostname='{}', address='{}'",
                    hostname,
                    address
                );

                // Create network component
                let mut network = NetworkStatsComponent::new(hostname, address.clone());

                // Set the client and refresh data
                if let Some(client) = self.cluster.client() {
                    // Create a client configured for this specific node
                    let node_client = client.with_node(&address);
                    tracing::info!("Created node client for network: '{}'", address);
                    network.set_client(node_client);
                    if let Err(e) = network.refresh().await {
                        tracing::error!("Network refresh error: {:?}", e);
                        network.set_error(e.to_string());
                    }
                }

                self.network = Some(network);
                self.view = View::Network;
            }
            Action::ShowSecurity => {
                // Switch to security/certificates view
                tracing::info!("Viewing security/certificates");

                // Create security component
                let mut security = SecurityComponent::new(String::new(), self.config_path.clone());

                // Set the client and refresh data
                if let Some(client) = self.cluster.client() {
                    security.set_client(client.clone());
                }

                if let Err(e) = security.refresh().await {
                    tracing::error!("Security refresh error: {:?}", e);
                    security.set_error(e.to_string());
                }

                self.security = Some(security);
                self.view = View::Security;
            }
            Action::ShowLifecycle => {
                // Switch to lifecycle/version view
                tracing::info!("Viewing lifecycle/versions");

                // Create lifecycle component
                let mut lifecycle =
                    LifecycleComponent::new(String::new(), self.config_path.clone());

                // Set the client and refresh data
                if let Some(client) = self.cluster.client() {
                    lifecycle.set_client(client.clone());
                }

                if let Err(e) = lifecycle.refresh().await {
                    tracing::error!("Lifecycle refresh error: {:?}", e);
                    lifecycle.set_error(e.to_string());
                }

                self.lifecycle = Some(lifecycle);
                self.view = View::Lifecycle;
            }
            Action::ShowWorkloads => {
                // Switch to workload health view
                tracing::info!("Viewing workload health");

                // Create workloads component
                let mut workloads = WorkloadHealthComponent::new();

                // Create K8s client from Talos client
                if let Some(talos_client) = self.cluster.client() {
                    match crate::components::diagnostics::k8s::create_k8s_client(talos_client).await
                    {
                        Ok(k8s_client) => {
                            workloads.set_k8s_client(k8s_client);
                        }
                        Err(e) => {
                            tracing::error!("Failed to create K8s client: {:?}", e);
                            workloads.set_error(format!("Failed to connect to Kubernetes: {}", e));
                        }
                    }
                }

                if let Err(e) = workloads.refresh().await {
                    tracing::error!("Workloads refresh error: {:?}", e);
                    workloads.set_error(e.to_string());
                }

                self.workloads = Some(workloads);
                self.view = View::Workloads;
            }
            Action::ShowStorage(hostname, address) => {
                // Switch to storage view for a node
                tracing::info!(
                    "ShowStorage: hostname='{}', address='{}'",
                    hostname,
                    address
                );

                // Get context and config from cluster component
                let context = self.cluster.current_context_name().map(|s| s.to_string());
                let config_path = self.cluster.config_path().map(|s| s.to_string());

                // Create storage component with context for authentication
                let mut storage =
                    StorageComponent::new(hostname, address.clone(), context, config_path);

                // Set the client and refresh data
                if let Some(client) = self.cluster.client() {
                    // Create a client configured for this specific node
                    let node_client = client.with_node(&address);
                    storage.set_client(node_client);
                    if let Err(e) = storage.refresh().await {
                        tracing::error!("Storage refresh error: {:?}", e);
                        storage.set_error(e.to_string());
                    }
                }

                self.storage = Some(storage);
                self.view = View::Storage;
            }
            Action::ShowNodeOperations(hostname, address, is_controlplane) => {
                // Show node operations overlay
                tracing::info!("Viewing node operations for: {} ({})", hostname, address);

                // Create node operations component
                let mut node_ops = NodeOperationsComponent::new(hostname, address, is_controlplane);

                // Set the Talos client
                if let Some(talos_client) = self.cluster.client() {
                    node_ops.set_client(talos_client.clone());
                }

                // Refresh to load safety checks
                if let Err(e) = node_ops.refresh().await {
                    tracing::error!("Node operations refresh error: {:?}", e);
                    node_ops.set_error(e.to_string());
                }

                self.node_operations = Some(node_ops);
                self.view = View::NodeOperations;
            }
            Action::ShowRollingOperations(nodes) => {
                // Show rolling operations overlay
                tracing::info!("Viewing rolling operations for {} nodes", nodes.len());

                // Create rolling operations component
                let mut rolling_ops = RollingOperationsComponent::new();

                // Convert node tuples to RollingNodeInfo
                let node_infos: Vec<RollingNodeInfo> = nodes
                    .into_iter()
                    .map(|(hostname, address, is_controlplane)| RollingNodeInfo {
                        hostname,
                        address,
                        is_controlplane,
                        selection_order: None,
                    })
                    .collect();

                rolling_ops.set_nodes(node_infos);

                // Set clients
                if let Some(talos_client) = self.cluster.client() {
                    rolling_ops.set_talos_client(talos_client.clone());

                    // Try to get K8s client
                    if let Ok(k8s) =
                        crate::components::diagnostics::k8s::create_k8s_client(talos_client).await
                    {
                        rolling_ops.set_k8s_client(k8s);
                    }
                }

                self.rolling_operations = Some(rolling_ops);
                self.view = View::RollingOperations;
            }
            _ => {
                // Forward to current component
                match self.view {
                    View::Cluster => {
                        if let Some(next_action) = self.cluster.update(action)? {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::MultiLogs => {
                        if let Some(multi_logs) = &mut self.multi_logs
                            && let Some(next_action) = multi_logs.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Etcd => {
                        if let Some(etcd) = &mut self.etcd
                            && let Some(next_action) = etcd.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Processes => {
                        if let Some(processes) = &mut self.processes
                            && let Some(next_action) = processes.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Network => {
                        if let Some(network) = &mut self.network
                            && let Some(next_action) = network.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Diagnostics => {
                        if let Some(diagnostics) = &mut self.diagnostics
                            && let Some(next_action) = diagnostics.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Security => {
                        if let Some(security) = &mut self.security
                            && let Some(next_action) = security.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Lifecycle => {
                        if let Some(lifecycle) = &mut self.lifecycle
                            && let Some(next_action) = lifecycle.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Workloads => {
                        if let Some(workloads) = &mut self.workloads
                            && let Some(next_action) = workloads.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Storage => {
                        if let Some(storage) = &mut self.storage
                            && let Some(next_action) = storage.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::NodeOperations => {
                        if let Some(node_ops) = &mut self.node_operations
                            && let Some(next_action) = node_ops.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::RollingOperations => {
                        if let Some(rolling_ops) = &mut self.rolling_operations
                            && let Some(next_action) = rolling_ops.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
