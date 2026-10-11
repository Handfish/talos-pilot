//! Clipboard utilities for cross-platform clipboard support
//!
//! On Linux, clipboard contents don't persist after the Clipboard object is dropped.
//! This module provides a helper that keeps the clipboard alive in a background thread.

use std::thread;

/// Copy text to clipboard, handling Linux quirks
///
/// On Linux, spawns a background thread to keep clipboard contents alive
/// for a few seconds so clipboard managers can grab them.
pub fn copy_to_clipboard(text: String) -> Result<(), String> {
    // Spawn a thread to handle clipboard - this avoids blocking the main thread
    // and keeps the clipboard alive long enough on Linux
    thread::spawn(move || {
        match arboard::Clipboard::new() {
            Ok(mut clipboard) => {
                match clipboard.set_text(&text) {
                    Err(e) => tracing::warn!("Failed to copy to clipboard: {}", e),
                    Ok(()) => {
                        // On Linux, keep clipboard alive for clipboard managers to grab contents
                        #[cfg(target_os = "linux")]
                        thread::sleep(std::time::Duration::from_secs(2));
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to access clipboard: {}", e);
            }
        }
    });

    Ok(())
}
