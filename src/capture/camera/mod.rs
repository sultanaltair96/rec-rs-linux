//! Webcam capture (phase 1B).
//!
//! The real implementation opens a V4L2 device (`v4l` crate) and converts its
//! frames to the contract's CPU formats, or goes through the camera portal when
//! running inside Flatpak and V4L2 is not reachable. A cold camera is given
//! two seconds to settle its exposure before the file starts, like upstream.
//!
//! Interfaces below are what phase 1B fills in.

// The factory signatures are called by `record.rs` as each source lands; until
// then some of them have no caller.
#![allow(dead_code)]

use anyhow::Result;

use crate::media_types::{CameraInfo, VideoSource};

use super::not_implemented;

/// The phase that implements this module.
pub const PHASE: &str = "1B";

/// Which camera to open.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CameraRequest {
    /// Unique id or name substring; None = the system default.
    pub device: Option<String>,
}

/// Start capturing `req`. The returned source is stopped; the session calls
/// [`VideoSource::start`] with its frame sink.
pub fn open(req: &CameraRequest) -> Result<Box<dyn VideoSource>> {
    let _ = req;
    Err(not_implemented("camera capture", PHASE))
}

/// The cameras `--webcam` and the app list.
pub fn cameras() -> Result<Vec<CameraInfo>> {
    Err(not_implemented("listing cameras", PHASE))
}
