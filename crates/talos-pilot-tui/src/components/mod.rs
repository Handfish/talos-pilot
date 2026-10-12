//! Component system for talos-pilot TUI
//!
//! Based on the ratatui Component template pattern.

pub mod cluster;
pub mod diagnostics;
pub mod etcd;
pub mod home;
pub mod insecure;
pub mod lifecycle;
pub mod logs;
pub mod multi_logs;
pub mod network;
pub mod node_operations;
pub mod processes;
pub mod rolling_operations;
pub mod security;
pub mod storage;
pub mod wizard;
pub mod workloads;

pub use cluster::ClusterComponent;
pub use diagnostics::DiagnosticsComponent;
pub use etcd::EtcdComponent;
pub use home::HomeComponent;
pub use insecure::InsecureComponent;
pub use lifecycle::LifecycleComponent;
pub use logs::LogsComponent;
pub use multi_logs::MultiLogsComponent;
pub use network::NetworkStatsComponent;
pub use node_operations::NodeOperationsComponent;
pub use processes::ProcessesComponent;
pub use rolling_operations::RollingOperationsComponent;
pub use security::SecurityComponent;
pub use storage::StorageComponent;
pub use wizard::WizardComponent;
pub use workloads::WorkloadHealthComponent;

use crate::action::Action;
use color_eyre::Result;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{Frame, layout::Rect};

/// Trait for UI components
///
/// Components are modular, reusable UI elements that can handle events,
/// update their state, and render themselves.
pub trait Component {
    /// Initialize the component with the given area
    fn init(&mut self, _area: Rect) -> Result<()> {
        Ok(())
    }

    /// Handle key events and optionally produce actions
    fn handle_key_event(&mut self, _key: KeyEvent) -> Result<Option<Action>> {
        Ok(None)
    }

    /// Handle mouse events and optionally produce actions
    fn handle_mouse_event(&mut self, _mouse: MouseEvent) -> Result<Option<Action>> {
        Ok(None)
    }

    /// Update the component state based on an action
    fn update(&mut self, action: Action) -> Result<Option<Action>>;

    /// Render the component to the frame
    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()>;
}

/// A component whose refresh can run on a clone, off the event loop.
///
/// The app clones the component, runs `refresh()` on the clone in a spawned
/// task, and hands the finished clone back via [`adopt_refresh`] so the UI
/// keeps drawing and handling keys while slow API calls are in flight.
///
/// [`adopt_refresh`]: BackgroundRefresh::adopt_refresh
pub trait BackgroundRefresh: Clone + Send + 'static {
    /// Take the data `refreshed` loaded while keeping the UI state (selection,
    /// dialogs, scroll) the user may have changed during the refresh.
    fn adopt_refresh(&mut self, refreshed: Self);
}
