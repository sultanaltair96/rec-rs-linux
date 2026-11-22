//! Capture sources: screen, system audio, microphone and camera.
//!
//! Each source is a module of its own, owned by one phase 1 worker:
//!
//! | Module | Phase | What it captures |
//! |---|---|---|
//! | [`screen`] | 1A | displays, windows and regions through the ScreenCast portal + PipeWire |
//! | [`audio`] | 1B | the default sink's monitor (system audio) and microphones through PipeWire |
//! | [`camera`] | 1B | webcams through V4L2, or the camera portal inside Flatpak |
//!
//! Sources never call each other: the factories here return a source, the
//! session in `record.rs` starts it with a sink, and everything they produce
//! crosses in [`crate::media_types`]. Phase 0 ships the factory signatures and
//! device descriptions (so `devices`, `--display`, `--window`, `--mic` and
//! `--webcam` already parse and report) and a clear error for the capture
//! itself.

pub mod audio;
pub mod camera;
pub mod screen;

/// The error a phase 1 stub returns.
pub(crate) fn not_implemented(what: &str, phase: &str) -> anyhow::Error {
    anyhow::anyhow!("{what} is not implemented yet (phase {phase})")
}
