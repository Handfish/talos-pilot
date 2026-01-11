//! UI extensions for talos-pilot-core types
//!
//! Provides ratatui-specific extensions for core types, bridging the gap
//! between the UI-agnostic core library and the TUI presentation layer.

use ratatui::style::Color;
use talos_pilot_core::{ConnectionState, HealthIndicator, QuorumState, SafetyStatus};

/// Extension trait for HealthIndicator to provide ratatui colors
pub trait HealthIndicatorExt {
    /// Get the ratatui Color for this indicator
    fn color(&self) -> Color;

    /// Get symbol and color together (common pattern)
    fn symbol_and_color(&self) -> (&'static str, Color);
}

impl HealthIndicatorExt for HealthIndicator {
    fn color(&self) -> Color {
        match self {
            HealthIndicator::Healthy => Color::Green,
            HealthIndicator::Warning => Color::Yellow,
            HealthIndicator::Error => Color::Red,
            HealthIndicator::Pending => Color::Cyan,
            HealthIndicator::Info => Color::Blue,
            HealthIndicator::Unknown => Color::DarkGray,
        }
    }

    fn symbol_and_color(&self) -> (&'static str, Color) {
        (self.symbol(), self.color())
    }
}

/// Extension trait for QuorumState to provide ratatui colors
pub trait QuorumStateExt {
    /// Get indicator symbol and color
    fn indicator_with_color(&self) -> (&'static str, Color);

    /// Get display text and color
    fn display_with_color(&self) -> (&'static str, Color);
}

impl QuorumStateExt for QuorumState {
    fn indicator_with_color(&self) -> (&'static str, Color) {
        use talos_pilot_core::HasHealth;
        let health = self.health();
        (health.symbol(), health.color())
    }

    fn display_with_color(&self) -> (&'static str, Color) {
        let (text, _) = self.display();
        let color = match self {
            QuorumState::Healthy => Color::Green,
            QuorumState::Degraded { .. } => Color::Yellow,
            QuorumState::NoQuorum { .. } => Color::Red,
            QuorumState::Unknown => Color::DarkGray,
        };
        (text, color)
    }
}

/// Extension trait for SafetyStatus to provide ratatui colors
pub trait SafetyStatusExt {
    /// Get indicator symbol and color
    fn indicator_with_color(&self) -> (&'static str, Color);
}

impl SafetyStatusExt for SafetyStatus {
    fn indicator_with_color(&self) -> (&'static str, Color) {
        use talos_pilot_core::HasHealth;
        let health = self.health();
        (health.symbol(), health.color())
    }
}

/// Extension trait for ConnectionState to provide ratatui colors
pub trait ConnectionStateExt {
    /// Get the ratatui Color for this state
    fn color(&self) -> Color;

    /// Get symbol and color together
    fn symbol_and_color(&self) -> (&'static str, Color);
}

impl ConnectionStateExt for ConnectionState {
    fn color(&self) -> Color {
        match self {
            ConnectionState::Connected => Color::Green,
            ConnectionState::Partial => Color::Yellow,
            ConnectionState::Disconnected => Color::Red,
            ConnectionState::Unknown => Color::DarkGray,
        }
    }

    fn symbol_and_color(&self) -> (&'static str, Color) {
        (self.symbol(), self.color())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_indicator_colors() {
        assert_eq!(HealthIndicator::Healthy.color(), Color::Green);
        assert_eq!(HealthIndicator::Warning.color(), Color::Yellow);
        assert_eq!(HealthIndicator::Error.color(), Color::Red);
        assert_eq!(HealthIndicator::Unknown.color(), Color::DarkGray);
    }

    #[test]
    fn test_health_indicator_symbol_and_color() {
        let (symbol, color) = HealthIndicator::Healthy.symbol_and_color();
        assert_eq!(symbol, "●");
        assert_eq!(color, Color::Green);
    }

    #[test]
    fn test_quorum_state_colors() {
        let (_, color) = QuorumState::Healthy.display_with_color();
        assert_eq!(color, Color::Green);

        let (_, color) = QuorumState::NoQuorum { healthy: 1, total: 3 }.display_with_color();
        assert_eq!(color, Color::Red);
    }
}
