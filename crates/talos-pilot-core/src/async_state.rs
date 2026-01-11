//! Async component state management
//!
//! Provides shared state abstractions for async-loading TUI components,
//! eliminating duplicated loading/error/refresh patterns.

use std::time::{Duration, Instant};

/// Shared state for async-loading components
///
/// This struct consolidates the common pattern of loading state, error handling,
/// and refresh timing found across many TUI components.
///
/// # Examples
///
/// ```
/// use talos_pilot_core::async_state::AsyncState;
/// use std::time::Duration;
///
/// let mut state: AsyncState<Vec<String>> = AsyncState::new();
///
/// // Start loading
/// state.start_loading();
/// assert!(state.is_loading());
///
/// // Set data on success
/// state.set_data(vec!["item1".to_string()]);
/// assert!(!state.is_loading());
/// assert!(state.data().is_some());
///
/// // Check if refresh is needed
/// let needs_refresh = state.should_refresh(Duration::from_secs(5));
/// ```
#[derive(Debug, Clone)]
pub struct AsyncState<T> {
    /// The loaded data (None if not yet loaded or cleared)
    data: Option<T>,
    /// Whether data is currently being loaded
    loading: bool,
    /// Error message if the last load failed
    error: Option<String>,
    /// When the data was last successfully refreshed
    last_refresh: Option<Instant>,
    /// Number of consecutive failures (for retry logic)
    retry_count: u32,
}

impl<T> Default for AsyncState<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> AsyncState<T> {
    /// Create a new async state in initial loading state
    pub fn new() -> Self {
        Self {
            data: None,
            loading: true,
            error: None,
            last_refresh: None,
            retry_count: 0,
        }
    }

    /// Create a new async state that is not loading
    pub fn idle() -> Self {
        Self {
            data: None,
            loading: false,
            error: None,
            last_refresh: None,
            retry_count: 0,
        }
    }

    /// Check if currently loading
    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Check if there's an error
    pub fn has_error(&self) -> bool {
        self.error.is_some()
    }

    /// Get the error message if any
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Get a reference to the data
    pub fn data(&self) -> Option<&T> {
        self.data.as_ref()
    }

    /// Get a mutable reference to the data
    pub fn data_mut(&mut self) -> Option<&mut T> {
        self.data.as_mut()
    }

    /// Take ownership of the data
    pub fn take_data(&mut self) -> Option<T> {
        self.data.take()
    }

    /// Get the last refresh time
    pub fn last_refresh(&self) -> Option<Instant> {
        self.last_refresh
    }

    /// Get the retry count
    pub fn retry_count(&self) -> u32 {
        self.retry_count
    }

    /// Start a loading operation
    ///
    /// Sets loading to true but preserves existing data for display.
    pub fn start_loading(&mut self) {
        self.loading = true;
        // Don't clear error yet - it might be useful to show during loading
    }

    /// Set the data after a successful load
    ///
    /// Clears error, resets retry count, and updates last_refresh.
    pub fn set_data(&mut self, data: T) {
        self.data = Some(data);
        self.loading = false;
        self.error = None;
        self.retry_count = 0;
        self.last_refresh = Some(Instant::now());
    }

    /// Set an error after a failed load
    ///
    /// Increments retry count and clears loading state.
    /// Preserves existing data for continued display.
    pub fn set_error(&mut self, error: impl ToString) {
        self.error = Some(error.to_string());
        self.loading = false;
        self.retry_count += 1;
    }

    /// Set an error with context about retry count
    pub fn set_error_with_retry(&mut self, error: impl ToString) {
        self.retry_count += 1;
        self.error = Some(format!("{} (retry {})", error.to_string(), self.retry_count));
        self.loading = false;
    }

    /// Clear the error
    pub fn clear_error(&mut self) {
        self.error = None;
    }

    /// Clear all state
    pub fn clear(&mut self) {
        self.data = None;
        self.loading = false;
        self.error = None;
        self.last_refresh = None;
        self.retry_count = 0;
    }

    /// Check if a refresh is needed based on the interval
    ///
    /// Returns true if:
    /// - Data has never been loaded, or
    /// - The interval has elapsed since last refresh
    ///
    /// Returns false if currently loading.
    pub fn should_refresh(&self, interval: Duration) -> bool {
        if self.loading {
            return false;
        }

        match self.last_refresh {
            None => true,
            Some(last) => last.elapsed() >= interval,
        }
    }

    /// Check if auto-refresh should trigger
    ///
    /// Convenience method combining should_refresh with auto_refresh flag.
    pub fn should_auto_refresh(&self, auto_refresh_enabled: bool, interval: Duration) -> bool {
        auto_refresh_enabled && self.should_refresh(interval)
    }

    /// Get elapsed time since last refresh
    pub fn elapsed_since_refresh(&self) -> Option<Duration> {
        self.last_refresh.map(|t| t.elapsed())
    }

    /// Check if data is stale (older than given duration)
    pub fn is_stale(&self, max_age: Duration) -> bool {
        match self.last_refresh {
            None => true,
            Some(last) => last.elapsed() > max_age,
        }
    }

    /// Map the data to a different type
    pub fn map<U, F: FnOnce(T) -> U>(self, f: F) -> AsyncState<U> {
        AsyncState {
            data: self.data.map(f),
            loading: self.loading,
            error: self.error,
            last_refresh: self.last_refresh,
            retry_count: self.retry_count,
        }
    }

    /// Check if we have data (regardless of loading/error state)
    pub fn has_data(&self) -> bool {
        self.data.is_some()
    }
}

/// Extension trait for working with AsyncState in TUI components
pub trait AsyncStateExt<T> {
    /// Get display status text
    fn status_text(&self) -> &'static str;

    /// Check if we should show loading indicator
    fn show_loading(&self) -> bool;

    /// Check if we should show error
    fn show_error(&self) -> bool;

    /// Check if we should show data
    fn show_data(&self) -> bool;
}

impl<T> AsyncStateExt<T> for AsyncState<T> {
    fn status_text(&self) -> &'static str {
        if self.loading && self.data.is_none() {
            "Loading..."
        } else if self.loading {
            "Refreshing..."
        } else if self.error.is_some() {
            "Error"
        } else {
            "Ready"
        }
    }

    fn show_loading(&self) -> bool {
        self.loading && self.data.is_none()
    }

    fn show_error(&self) -> bool {
        self.error.is_some() && self.data.is_none()
    }

    fn show_data(&self) -> bool {
        self.data.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_state() {
        let state: AsyncState<String> = AsyncState::new();
        assert!(state.is_loading());
        assert!(!state.has_error());
        assert!(state.data().is_none());
    }

    #[test]
    fn test_set_data() {
        let mut state: AsyncState<i32> = AsyncState::new();
        state.set_data(42);

        assert!(!state.is_loading());
        assert!(!state.has_error());
        assert_eq!(state.data(), Some(&42));
        assert!(state.last_refresh().is_some());
        assert_eq!(state.retry_count(), 0);
    }

    #[test]
    fn test_set_error() {
        let mut state: AsyncState<i32> = AsyncState::new();
        state.set_error("Connection failed");

        assert!(!state.is_loading());
        assert!(state.has_error());
        assert_eq!(state.error(), Some("Connection failed"));
        assert_eq!(state.retry_count(), 1);

        // Second error increments retry count
        state.start_loading();
        state.set_error("Still failing");
        assert_eq!(state.retry_count(), 2);
    }

    #[test]
    fn test_should_refresh() {
        let mut state: AsyncState<i32> = AsyncState::new();

        // Should refresh when never loaded
        assert!(!state.should_refresh(Duration::from_secs(1))); // But loading, so no

        state.set_data(1);
        // Just loaded, shouldn't refresh yet
        assert!(!state.should_refresh(Duration::from_secs(100)));

        // With zero duration, should refresh immediately
        assert!(state.should_refresh(Duration::ZERO));
    }

    #[test]
    fn test_preserves_data_on_error() {
        let mut state: AsyncState<i32> = AsyncState::new();
        state.set_data(42);

        state.start_loading();
        state.set_error("Refresh failed");

        // Data should still be available
        assert_eq!(state.data(), Some(&42));
        assert!(state.has_error());
    }

    #[test]
    fn test_status_text() {
        let mut state: AsyncState<i32> = AsyncState::new();
        assert_eq!(state.status_text(), "Loading...");

        state.set_data(42);
        assert_eq!(state.status_text(), "Ready");

        state.start_loading();
        assert_eq!(state.status_text(), "Refreshing...");

        state.set_error("Failed");
        assert_eq!(state.status_text(), "Error");
    }

    #[test]
    fn test_auto_refresh() {
        let mut state: AsyncState<i32> = AsyncState::idle();

        // Never loaded, should refresh if enabled
        assert!(state.should_auto_refresh(true, Duration::from_secs(5)));
        assert!(!state.should_auto_refresh(false, Duration::from_secs(5)));

        state.set_data(42);
        // Just loaded, shouldn't auto-refresh
        assert!(!state.should_auto_refresh(true, Duration::from_secs(100)));
    }
}
