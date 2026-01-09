//! Clipboard utilities for cross-platform clipboard support
//!
//! On Linux, clipboard contents don't persist after the Clipboard object is dropped.
//! This module provides a helper that keeps the clipboard alive in a background thread.

use std::thread;
use std::time::Duration;

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
                if let Err(e) = clipboard.set_text(&text) {
                    tracing::warn!("Failed to copy to clipboard: {}", e);
                    return;
                }
                // On Linux, keep clipboard alive for clipboard managers to grab contents
                #[cfg(target_os = "linux")]
                {
                    thread::sleep(Duration::from_secs(2));
                }
            }
            Err(e) => {
                tracing::warn!("Failed to access clipboard: {}", e);
            }
        }
    });

    Ok(())
}
