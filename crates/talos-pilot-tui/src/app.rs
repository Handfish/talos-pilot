//! Application state and main loop

use crate::action::Action;
use crate::components::{ClusterComponent, Component};
use crate::tui::{self, Tui};
use color_eyre::Result;
use crossterm::event::{self, Event, KeyEventKind};
use std::time::Duration;
use tokio::sync::mpsc;

/// Main application state
pub struct App {
    /// Whether the application should quit
    should_quit: bool,
    /// The active component
    cluster: ClusterComponent,
    /// Tick rate for animations (ms)
    tick_rate: Duration,
    /// Channel for async action results
    action_rx: mpsc::UnboundedReceiver<AsyncResult>,
    action_tx: mpsc::UnboundedSender<AsyncResult>,
}

/// Results from async operations
#[derive(Debug)]
#[allow(dead_code)]
enum AsyncResult {
    Connected,
    Refreshed,
    Error(String),
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        let (action_tx, action_rx) = mpsc::unbounded_channel();
        Self {
            should_quit: false,
            cluster: ClusterComponent::new(),
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

        // Start async connection
        self.start_connect();

        // Main loop
        let result = self.main_loop(&mut terminal).await;

        // Restore terminal
        tui::restore()?;

        result
    }

    /// Start async connection to Talos
    fn start_connect(&self) {
        let tx = self.action_tx.clone();
        tokio::spawn(async move {
            // Install crypto provider
            let _ = rustls::crypto::ring::default_provider().install_default();

            match talos_rs::TalosClient::from_default_config().await {
                Ok(_) => {
                    let _ = tx.send(AsyncResult::Connected);
                }
                Err(e) => {
                    let _ = tx.send(AsyncResult::Error(e.to_string()));
                }
            }
        });
    }

    /// Start async refresh (for background refresh without blocking UI)
    #[allow(dead_code)]
    fn start_refresh(&mut self) {
        let tx = self.action_tx.clone();
        tokio::spawn(async move {
            // Install crypto provider (idempotent)
            let _ = rustls::crypto::ring::default_provider().install_default();

            match talos_rs::TalosClient::from_default_config().await {
                Ok(client) => {
                    // Fetch data - we'll send results back
                    let _ = client.version().await;
                    let _ = tx.send(AsyncResult::Refreshed);
                }
                Err(e) => {
                    let _ = tx.send(AsyncResult::Error(e.to_string()));
                }
            }
        });
    }

    /// Main event loop
    async fn main_loop(&mut self, terminal: &mut Tui) -> Result<()> {
        // Connect on startup
        self.cluster.connect().await?;

        loop {
            // Draw
            terminal.draw(|frame| {
                let area = frame.area();
                let _ = self.cluster.draw(frame, area);
            })?;

            // Handle events with timeout
            if event::poll(self.tick_rate)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if let Some(action) = self.cluster.handle_key_event(key)? {
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
                    AsyncResult::Error(e) => {
                        tracing::error!("Async error: {}", e);
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
            Action::Tick => {
                // Update animations, etc.
                if let Some(next_action) = self.cluster.update(Action::Tick)? {
                    Box::pin(self.handle_action(next_action)).await?;
                }
            }
            Action::Resize(_w, _h) => {
                // Terminal will automatically resize on next draw
            }
            Action::Refresh => {
                tracing::info!("Refresh requested");
                self.cluster.refresh().await?;
            }
            _ => {
                // Forward to component
                if let Some(next_action) = self.cluster.update(action)? {
                    Box::pin(self.handle_action(next_action)).await?;
                }
            }
        }
        Ok(())
    }
}
