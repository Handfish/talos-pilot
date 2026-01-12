//! talos-pilot-core: Core business logic for talos-pilot
//!
//! This crate contains domain types, shared utilities, and business logic
//! for the talos-pilot TUI. It is intentionally kept independent of any
//! TUI framework to enable:
//!
//! - Unit testing without UI dependencies
//! - Reuse in CLI tools or other consumers
//! - Clear separation between business logic and presentation
//!
//! # Modules
//!
//! - [`types`] - Core domain types (Cluster, Node, Service, etc.)
//! - [`indicators`] - Health and status indicators for consistent UI representation
//! - [`formatting`] - Utilities for formatting bytes, durations, percentages, etc.
//! - [`selection`] - Generic selection logic for list-based UI components
//! - [`async_state`] - Async component state management (loading, error, refresh)
//! - [`errors`] - Error formatting utilities for user-friendly messages
//! - [`network`] - Network analysis utilities (port mapping, connection classification)

pub mod async_state;
pub mod errors;
pub mod formatting;
pub mod indicators;
pub mod network;
pub mod selection;
pub mod types;

// Re-export commonly used items at crate root
pub use async_state::*;
pub use errors::*;
pub use formatting::*;
pub use indicators::*;
pub use selection::*;
pub use types::*;

// Network is not re-exported at root to avoid name conflicts
// Use talos_pilot_core::network::* explicitly
