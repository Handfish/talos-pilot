//! Terminal UI wrapper
//!
//! Handles terminal setup, restoration, and the main render loop.

use color_eyre::Result;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::{self, Stdout};

/// Terminal wrapper type
pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Initialize the terminal for TUI mode
pub fn init() -> Result<Tui> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

/// Restore the terminal to its original state
pub fn restore() -> Result<()> {
    terminal::disable_raw_mode()?;
    crossterm::execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    Ok(())
}

/// Install panic hook to restore terminal on panic
pub fn install_panic_hook() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = restore();
        original_hook(panic_info);
    }));
}
