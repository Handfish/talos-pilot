//! Application state and main loop

use crate::action::Action;
use crate::components::{Component, HomeComponent};
use crate::tui::{self, Tui};
use color_eyre::Result;
use crossterm::event::{self, Event, KeyEventKind};
use std::time::Duration;

/// Main application state
pub struct App {
    /// Whether the application should quit
    should_quit: bool,
    /// The active component
    home: HomeComponent,
    /// Tick rate for animations (ms)
    tick_rate: Duration,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            should_quit: false,
            home: HomeComponent::new(),
            tick_rate: Duration::from_millis(250),
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
        loop {
            // Draw
            terminal.draw(|frame| {
                let area = frame.area();
                let _ = self.home.draw(frame, area);
            })?;

            // Handle events
            if event::poll(self.tick_rate)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if let Some(action) = self.home.handle_key_event(key)? {
                            self.handle_action(action)?;
                        }
                    }
                    Event::Resize(w, h) => {
                        self.handle_action(Action::Resize(w, h))?;
                    }
                    _ => {}
                }
            } else {
                // Tick for animations
                self.handle_action(Action::Tick)?;
            }

            // Check if we should quit
            if self.should_quit {
                break;
            }
        }

        Ok(())
    }

    /// Handle an action
    fn handle_action(&mut self, action: Action) -> Result<()> {
        match action {
            Action::Quit => {
                self.should_quit = true;
            }
            Action::Tick => {
                // Update animations, etc.
                if let Some(next_action) = self.home.update(Action::Tick)? {
                    self.handle_action(next_action)?;
                }
            }
            Action::Resize(_w, _h) => {
                // Terminal will automatically resize on next draw
            }
            Action::Refresh => {
                // TODO: Trigger data refresh
                tracing::info!("Refresh requested");
            }
            _ => {
                // Forward to component
                if let Some(next_action) = self.home.update(action)? {
                    self.handle_action(next_action)?;
                }
            }
        }
        Ok(())
    }
}
