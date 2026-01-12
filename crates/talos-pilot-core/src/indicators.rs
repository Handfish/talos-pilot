//! Health and status indicators for consistent UI representation
//!
//! This module provides unified types for representing health states,
//! eliminating duplication across TUI components.

use serde::{Deserialize, Serialize};

/// Universal health/status indicator
///
/// Represents the health state of any entity (node, service, check, etc.)
/// with consistent visual representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum HealthIndicator {
    /// Fully healthy/operational
    Healthy,
    /// Degraded but functional
    Warning,
    /// Failed or critical error
    Error,
    /// Pending or transitioning state
    Pending,
    /// Informational (neutral)
    Info,
    /// State cannot be determined
    #[default]
    Unknown,
}

impl HealthIndicator {
    /// Unicode symbol for this status
    ///
    /// Returns a single character that visually represents the state.
    pub fn symbol(&self) -> &'static str {
        match self {
            HealthIndicator::Healthy => "●",
            HealthIndicator::Warning => "◐",
            HealthIndicator::Error => "✗",
            HealthIndicator::Pending => "○",
            HealthIndicator::Info => "○",
            HealthIndicator::Unknown => "?",
        }
    }

    /// Human-readable label for this status
    pub fn label(&self) -> &'static str {
        match self {
            HealthIndicator::Healthy => "Healthy",
            HealthIndicator::Warning => "Warning",
            HealthIndicator::Error => "Error",
            HealthIndicator::Pending => "Pending",
            HealthIndicator::Info => "Info",
            HealthIndicator::Unknown => "Unknown",
        }
    }

    /// Check if this represents a healthy state
    pub fn is_healthy(&self) -> bool {
        matches!(self, HealthIndicator::Healthy)
    }

    /// Check if this represents an error state
    pub fn is_error(&self) -> bool {
        matches!(self, HealthIndicator::Error)
    }

    /// Check if this represents a warning or worse
    pub fn needs_attention(&self) -> bool {
        matches!(
            self,
            HealthIndicator::Warning | HealthIndicator::Error | HealthIndicator::Unknown
        )
    }

    /// Get severity level (for sorting/prioritization)
    ///
    /// Higher numbers = more severe
    pub fn severity(&self) -> u8 {
        match self {
            HealthIndicator::Healthy => 0,
            HealthIndicator::Info => 1,
            HealthIndicator::Pending => 2,
            HealthIndicator::Unknown => 3,
            HealthIndicator::Warning => 4,
            HealthIndicator::Error => 5,
        }
    }

    /// Compare severity with another indicator
    pub fn more_severe_than(&self, other: &HealthIndicator) -> bool {
        self.severity() > other.severity()
    }

    /// Return the more severe of two indicators
    pub fn worst(self, other: HealthIndicator) -> HealthIndicator {
        if self.severity() >= other.severity() {
            self
        } else {
            other
        }
    }
}

impl std::fmt::Display for HealthIndicator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label())
    }
}

/// Trait for types that can report their health status
///
/// Implement this trait to provide consistent health reporting
/// across different entity types.
pub trait HasHealth {
    /// Return the current health indicator
    fn health(&self) -> HealthIndicator;

    /// Check if the entity is in a healthy state
    fn is_healthy(&self) -> bool {
        self.health().is_healthy()
    }

    /// Check if the entity needs attention
    fn needs_attention(&self) -> bool {
        self.health().needs_attention()
    }
}

/// Connection state indicator
///
/// Represents the connection state of a node or service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum ConnectionState {
    /// Fully connected and responsive
    Connected,
    /// Partially connected or some issues
    Partial,
    /// Disconnected or unreachable
    Disconnected,
    /// Connection state unknown
    #[default]
    Unknown,
}

impl ConnectionState {
    /// Unicode symbol for this state
    pub fn symbol(&self) -> &'static str {
        match self {
            ConnectionState::Connected => "●",
            ConnectionState::Partial => "◐",
            ConnectionState::Disconnected => "○",
            ConnectionState::Unknown => "?",
        }
    }

    /// Human-readable label
    pub fn label(&self) -> &'static str {
        match self {
            ConnectionState::Connected => "Connected",
            ConnectionState::Partial => "Partial",
            ConnectionState::Disconnected => "Disconnected",
            ConnectionState::Unknown => "Unknown",
        }
    }

    /// Check if connected
    pub fn is_connected(&self) -> bool {
        matches!(self, ConnectionState::Connected)
    }
}

impl From<ConnectionState> for HealthIndicator {
    fn from(state: ConnectionState) -> Self {
        match state {
            ConnectionState::Connected => HealthIndicator::Healthy,
            ConnectionState::Partial => HealthIndicator::Warning,
            ConnectionState::Disconnected => HealthIndicator::Error,
            ConnectionState::Unknown => HealthIndicator::Unknown,
        }
    }
}

/// Quorum state for clustered services (etcd, etc.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuorumState {
    /// Cluster has full quorum - all members healthy
    Healthy,
    /// Cluster is degraded but has quorum
    Degraded { healthy: usize, total: usize },
    /// Cluster has lost quorum - critical
    NoQuorum { healthy: usize, total: usize },
    /// Unknown state (loading or error)
    Unknown,
}

impl QuorumState {
    /// Calculate quorum state from member counts
    pub fn from_counts(healthy: usize, total: usize) -> Self {
        if total == 0 {
            return QuorumState::Unknown;
        }

        let required = (total / 2) + 1;

        if healthy == total {
            QuorumState::Healthy
        } else if healthy >= required {
            QuorumState::Degraded { healthy, total }
        } else {
            QuorumState::NoQuorum { healthy, total }
        }
    }

    /// Check if quorum is maintained
    pub fn has_quorum(&self) -> bool {
        matches!(self, QuorumState::Healthy | QuorumState::Degraded { .. })
    }

    /// Get display text and color hint for the quorum state
    pub fn display(&self) -> (&'static str, &'static str) {
        match self {
            QuorumState::Healthy => ("HEALTHY", "green"),
            QuorumState::Degraded { .. } => ("DEGRADED", "yellow"),
            QuorumState::NoQuorum { .. } => ("NO QUORUM", "red"),
            QuorumState::Unknown => ("UNKNOWN", "gray"),
        }
    }

    /// Get member count as "healthy/total" string
    pub fn member_count_display(&self) -> String {
        match self {
            QuorumState::Healthy => "?/?".to_string(), // Caller should use actual counts
            QuorumState::Degraded { healthy, total } => format!("{}/{}", healthy, total),
            QuorumState::NoQuorum { healthy, total } => format!("{}/{}", healthy, total),
            QuorumState::Unknown => "?/?".to_string(),
        }
    }
}

impl HasHealth for QuorumState {
    fn health(&self) -> HealthIndicator {
        match self {
            QuorumState::Healthy => HealthIndicator::Healthy,
            QuorumState::Degraded { .. } => HealthIndicator::Warning,
            QuorumState::NoQuorum { .. } => HealthIndicator::Error,
            QuorumState::Unknown => HealthIndicator::Unknown,
        }
    }
}

impl Default for QuorumState {
    fn default() -> Self {
        QuorumState::Unknown
    }
}

/// Safety status for operations that may have risks
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SafetyStatus {
    /// Operation is safe to proceed
    #[default]
    Safe,
    /// Operation has warnings but can proceed
    Warning(String),
    /// Operation is unsafe and should not proceed
    Unsafe(String),
    /// Safety status is unknown (still loading)
    Unknown,
}

impl SafetyStatus {
    /// Check if safe to proceed
    pub fn is_safe(&self) -> bool {
        matches!(self, SafetyStatus::Safe)
    }

    /// Check if unknown (still loading)
    pub fn is_unknown(&self) -> bool {
        matches!(self, SafetyStatus::Unknown)
    }

    /// Get the reason if unsafe or warning
    pub fn reason(&self) -> Option<&str> {
        match self {
            SafetyStatus::Safe | SafetyStatus::Unknown => None,
            SafetyStatus::Warning(reason) | SafetyStatus::Unsafe(reason) => Some(reason),
        }
    }
}

impl HasHealth for SafetyStatus {
    fn health(&self) -> HealthIndicator {
        match self {
            SafetyStatus::Safe => HealthIndicator::Healthy,
            SafetyStatus::Warning(_) => HealthIndicator::Warning,
            SafetyStatus::Unsafe(_) => HealthIndicator::Error,
            SafetyStatus::Unknown => HealthIndicator::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_indicator_severity() {
        assert!(HealthIndicator::Error.more_severe_than(&HealthIndicator::Warning));
        assert!(HealthIndicator::Warning.more_severe_than(&HealthIndicator::Healthy));
        assert!(!HealthIndicator::Healthy.more_severe_than(&HealthIndicator::Warning));
    }

    #[test]
    fn test_health_indicator_worst() {
        assert_eq!(
            HealthIndicator::Healthy.worst(HealthIndicator::Error),
            HealthIndicator::Error
        );
        assert_eq!(
            HealthIndicator::Error.worst(HealthIndicator::Healthy),
            HealthIndicator::Error
        );
    }

    #[test]
    fn test_quorum_state_from_counts() {
        // Empty cluster
        assert!(matches!(
            QuorumState::from_counts(0, 0),
            QuorumState::Unknown
        ));

        // 3-node cluster
        assert!(matches!(
            QuorumState::from_counts(3, 3),
            QuorumState::Healthy
        ));
        assert!(matches!(
            QuorumState::from_counts(2, 3),
            QuorumState::Degraded { .. }
        ));
        assert!(matches!(
            QuorumState::from_counts(1, 3),
            QuorumState::NoQuorum { .. }
        ));

        // 5-node cluster
        assert!(matches!(
            QuorumState::from_counts(5, 5),
            QuorumState::Healthy
        ));
        assert!(matches!(
            QuorumState::from_counts(3, 5),
            QuorumState::Degraded { .. }
        ));
        assert!(matches!(
            QuorumState::from_counts(2, 5),
            QuorumState::NoQuorum { .. }
        ));
    }

    #[test]
    fn test_has_health_trait() {
        let quorum = QuorumState::Healthy;
        assert!(quorum.is_healthy());
        assert!(!quorum.needs_attention());

        let degraded = QuorumState::Degraded {
            healthy: 2,
            total: 3,
        };
        assert!(!degraded.is_healthy());
        assert!(degraded.needs_attention());
    }

    #[test]
    fn test_safety_status() {
        let safe = SafetyStatus::Safe;
        assert!(safe.is_safe());
        assert!(safe.reason().is_none());

        let unsafe_op = SafetyStatus::Unsafe("Would lose quorum".to_string());
        assert!(!unsafe_op.is_safe());
        assert_eq!(unsafe_op.reason(), Some("Would lose quorum"));
    }
}
