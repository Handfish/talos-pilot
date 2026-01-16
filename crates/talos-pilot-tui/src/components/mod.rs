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
