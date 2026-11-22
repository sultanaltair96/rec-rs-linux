//! Screen and window capture (phase 1A).
//!
//! The real implementation is xdg-desktop-portal's ScreenCast interface
//! (`ashpd`) plus PipeWire, which works on Wayland and X11 desktops that have a
//! portal. It persists the portal restore token so the picker appears once, and
//! can list windows and crop a region. An optional direct X11 path (x11rb)
//! serves `devices` and `--window` on X11 sessions without a portal.
//!
//! Interfaces below are what phase 1A fills in; `record.rs` and `cli.rs`
//! already call them, so the worker touches only this directory.

// The factory signatures are called by `record.rs` as each source lands; until
// then some of them have no caller.
#![allow(dead_code)]

use anyhow::Result;

use crate::media_types::{DisplayInfo, VideoSource, WindowInfo};

use super::not_implemented;

/// The phase that implements this module.
pub const PHASE: &str = "1A";

/// A rectangle of a display, in the display's logical units (points) from its
/// top-left corner.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Region {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// What `record` asked for on the command line.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScreenRequest {
    /// 1-based display number (`--display`).
    pub display: usize,
    /// Window id or an app/title substring (`--window`).
    pub window: Option<String>,
    /// Region of the display (`--region`), which excludes `window`.
    pub region: Option<Region>,
}

/// What is being captured, once resolved.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// A whole display, or a region of it.
    Display(DisplayInfo, Option<Region>),
    /// A single window.
    Window(WindowInfo),
}

impl Target {
    /// One line for the start log.
    pub fn describe(&self) -> String {
        match self {
            Target::Display(d, None) => {
                format!("display {} ({}x{} pixels)", d.index, d.pixels.0, d.pixels.1)
            }
            Target::Display(d, Some(r)) => {
                format!(
                    "display {} region {}x{} at {},{}",
                    d.index, r.width, r.height, r.x, r.y
                )
            }
            Target::Window(w) => {
                format!("window {:?} ({} — {}x{})", w.id, w.app, w.size.0, w.size.1)
            }
        }
    }
}

/// The displays to list in `rec-rs devices` (1 = main).
pub fn displays() -> Result<Vec<DisplayInfo>> {
    Err(not_implemented("listing displays", PHASE))
}

/// The windows `--window` can select.
pub fn windows() -> Result<Vec<WindowInfo>> {
    Err(not_implemented("listing windows", PHASE))
}

/// Resolve a request to the target and its native pixel size, which `prepare`
/// needs to compute the output size.
pub fn resolve(req: &ScreenRequest) -> Result<(Target, (usize, usize))> {
    let what = if req.window.is_some() {
        "window capture"
    } else {
        "screen capture"
    };
    Err(not_implemented(what, PHASE))
}

/// Start capturing `req`. The returned source is stopped; the session calls
/// [`VideoSource::start`] with its frame sink.
pub fn open(req: &ScreenRequest) -> Result<Box<dyn VideoSource>> {
    let what = if req.window.is_some() {
        "window capture"
    } else {
        "screen capture"
    };
    Err(not_implemented(what, PHASE))
}
