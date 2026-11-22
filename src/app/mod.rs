//! The app: main window with live preview, settings, tray, hotkeys — phase 1E.
//!
//! Phase 0 gives `rec-rs gui` its exact signature (upstream's
//! `rec-rs gui [--start-recording] [--settings] [--window ID]`) and a clear
//! error, so scripts and the packaging work that reference the command keep
//! their contract. The egui/wgpu implementation lands in this directory.

use anyhow::Result;

/// The phase that implements the app.
pub const PHASE: &str = "1E";

/// Start the app.
///
/// `start_recording` records immediately (a login item or a script),
/// `settings` opens the settings window at launch, and `window` captures one
/// window (id from `rec-rs devices`) instead of the display.
pub fn run(start_recording: bool, settings: bool, window: Option<u32>) -> Result<()> {
    let _ = (start_recording, settings, window);
    Err(anyhow::anyhow!(
        "the graphical app is not implemented yet (phase {PHASE}); use `rec-rs record` (headless) meanwhile"
    ))
}
