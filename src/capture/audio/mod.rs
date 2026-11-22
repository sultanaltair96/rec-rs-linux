//! System audio and microphone capture (phase 1B).
//!
//! The real implementation captures the default sink's monitor for system
//! audio and a PipeWire capture node for the microphone, by node id or name
//! substring, and reads the sink's latency so system audio can be shifted back
//! into sync (`--system-audio-offset-ms` defaults to it).
//!
//! Interfaces below are what phase 1B fills in.

// The factory signatures are called by `record.rs` as each source lands; until
// then some of them have no caller.
#![allow(dead_code)]

use anyhow::Result;

use crate::media_types::{AudioSource, MicInfo};

use super::not_implemented;

/// The phase that implements this module.
pub const PHASE: &str = "1B";

/// Which microphone to open.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioRequest {
    /// Unique id or name substring; None = the system default.
    pub device: Option<String>,
    /// The device's display name, used to find it again when its id changed.
    pub name: Option<String>,
}

/// Start capturing the system output (the default sink's monitor).
///
/// The returned source is stopped; the session calls [`AudioSource::start`]
/// with its audio sink.
pub fn open_system() -> Result<Box<dyn AudioSource>> {
    Err(not_implemented("system audio capture", PHASE))
}

/// Start capturing a microphone.
pub fn open_mic(req: &AudioRequest) -> Result<Box<dyn AudioSource>> {
    let _ = req;
    Err(not_implemented("microphone capture", PHASE))
}

/// The microphones `--mic` and the mixer list.
pub fn microphones() -> Result<Vec<MicInfo>> {
    Err(not_implemented("listing microphones", PHASE))
}

/// The output device's latency in seconds: system audio is captured when it is
/// mixed and heard one latency later, so it is shifted back by this.
///
/// None when no sink reports one (a headless session, or before phase 1B).
pub fn output_latency() -> Option<f64> {
    None
}

/// Resolve a `--mic` selection to a device, warning when the requested one is
/// not connected and the default is used instead.
pub fn pick_mic(sel: &str, name: Option<&str>) -> Result<MicInfo> {
    let _ = (sel, name);
    Err(not_implemented("microphone capture", PHASE))
}
