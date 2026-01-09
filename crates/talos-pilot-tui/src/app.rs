//! Application state and main loop

use crate::action::Action;
use crate::components::{ClusterComponent, Component, MultiLogsComponent};
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
    /// Number of log lines to fetch per service
    tail_lines: i32,
    /// Tick rate for animations (ms)
    tick_rate: Duration,
    /// Channel for async action results
    action_rx: mpsc::UnboundedReceiver<AsyncResult>,
    #[allow(dead_code)] // Will be used for background log streaming
    action_tx: mpsc::UnboundedSender<AsyncResult>,
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
        Self::new(None, 500)
    }
}

impl App {
    pub fn new(context: Option<String>, tail_lines: i32) -> Self {
        let (action_tx, action_rx) = mpsc::unbounded_channel();
        Self {
            should_quit: false,
            view: View::Cluster,
            cluster: ClusterComponent::new(context),
            multi_logs: None,
            tail_lines,
            tick_rate: Duration::from_millis(100),
            action_rx,
            action_tx,
        }
    }

    /// Run the application
    pub async fn run(&mut self) -> Result<()> {
        // Install panic hook
        tui::install_panic_hook();

        // Initialize terminal
        let mut terminal = tui::init()?;

        // Main loop
        let result = self.main_loop(&mut terminal).await;

        // Restore terminal
        tui::restore()?;

        result
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
                // Return to cluster view
                self.view = View::Cluster;
                self.multi_logs = None;
            }
            Action::Tick => {
                // Update animations, etc.
                match self.view {
                    View::Cluster => {
                        if let Some(next_action) = self.cluster.update(Action::Tick)? {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::MultiLogs => {
                        if let Some(multi_logs) = &mut self.multi_logs
                            && let Some(next_action) = multi_logs.update(Action::Tick)?
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
                self.cluster.refresh().await?;
            }
            Action::ShowMultiLogs(node_ip, node_role, service_ids) => {
                // Switch to multi-service logs view
                tracing::info!("Viewing multi-service logs for node: {}", node_ip);

                // Create multi-logs component
                let mut multi_logs = MultiLogsComponent::new(
                    node_ip,
                    node_role,
                    service_ids.clone(),
                );

                // Fetch logs from all services in parallel
                if let Some(client) = self.cluster.client() {
                    let service_refs: Vec<&str> = service_ids.iter().map(|s| s.as_str()).collect();
                    match client.logs_multi(&service_refs, self.tail_lines).await {
                        Ok(logs) => {
                            multi_logs.set_logs(logs);
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
                }
            }
        }
        Ok(())
    }
}
