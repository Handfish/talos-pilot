//! Application state and main loop

use crate::action::Action;
use crate::components::{ClusterComponent, Component, LogsComponent};
use crate::tui::{self, Tui};
use color_eyre::Result;
use crossterm::event::{self, Event, KeyEventKind};
use std::time::Duration;
use tokio::sync::mpsc;

/// Current view in the application
#[derive(Debug, Clone, PartialEq)]
enum View {
    Cluster,
    Logs,
}

/// Main application state
pub struct App {
    /// Whether the application should quit
    should_quit: bool,
    /// Current view
    view: View,
    /// Cluster component
    cluster: ClusterComponent,
    /// Logs component (created when viewing logs)
    logs: Option<LogsComponent>,
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
        Self::new(None)
    }
}

impl App {
    pub fn new(context: Option<String>) -> Self {
        let (action_tx, action_rx) = mpsc::unbounded_channel();
        Self {
            should_quit: false,
            view: View::Cluster,
            cluster: ClusterComponent::new(context),
            logs: None,
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
                    View::Logs => {
                        if let Some(logs) = &mut self.logs {
                            let _ = logs.draw(frame, area);
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
                            View::Logs => {
                                if let Some(logs) = &mut self.logs {
                                    logs.handle_key_event(key)?
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
                    AsyncResult::LogsLoaded(content) => {
                        if let Some(logs) = &mut self.logs {
                            logs.set_logs(content);
                        }
                    }
                    AsyncResult::Error(e) => {
                        tracing::error!("Async error: {}", e);
                        if let Some(logs) = &mut self.logs {
                            logs.set_error(e);
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
                self.logs = None;
            }
            Action::Tick => {
                // Update animations, etc.
                match self.view {
                    View::Cluster => {
                        if let Some(next_action) = self.cluster.update(Action::Tick)? {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Logs => {
                        if let Some(logs) = &mut self.logs
                            && let Some(next_action) = logs.update(Action::Tick)?
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
            Action::ShowNodeDetails(_node, service_id) => {
                // Switch to logs view
                tracing::info!("Viewing logs for service: {}", service_id);

                // Create logs component
                let mut logs_component = LogsComponent::new(service_id.clone());

                // Fetch logs asynchronously
                if let Some(client) = self.cluster.client() {
                    match client.logs(&service_id, 100).await {
                        Ok(content) => {
                            logs_component.set_logs(content);
                        }
                        Err(e) => {
                            logs_component.set_error(e.to_string());
                        }
                    }
                }

                self.logs = Some(logs_component);
                self.view = View::Logs;
            }
            _ => {
                // Forward to current component
                match self.view {
                    View::Cluster => {
                        if let Some(next_action) = self.cluster.update(action)? {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Logs => {
                        if let Some(logs) = &mut self.logs
                            && let Some(next_action) = logs.update(action)?
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
