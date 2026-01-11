//! talos-pilot-tui: Terminal UI for talos-pilot
//!
//! This crate provides a Ratatui-based TUI using the Component pattern
//! with tachyonfx effects for smooth animations.

pub mod action;
pub mod app;
pub mod audit;
pub mod clipboard;
pub mod components;
pub mod tui;
pub mod ui_ext;

pub use app::App;
pub use ui_ext::*;
