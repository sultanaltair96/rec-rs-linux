//! The GPU compositor and scaler (phase 1C).
//!
//! wgpu compute/render shaders draw the scenes (screen, screen + camera
//! overlay, camera full frame), the overlay shapes, borders and shadows whose
//! geometry [`crate::compose`] already computes, and the Lanczos scaler. In the
//! app it shares the egui wgpu device.
//!
//! Phase 0 defines the seam so the writer and the app can be written against
//! it: a [`Renderer`] takes the current scene and the newest captured frames
//! and returns the composited frame.

// This is the phase 1C seam: the writer and the app call it as the compositor
// lands.
#![allow(dead_code)]

use anyhow::Result;

use crate::compose::{PipSpec, Scene};
use crate::media_types::VideoFrame;

use crate::capture::not_implemented;

/// The phase that implements this module.
pub const PHASE: &str = "1C";

/// A wgpu compositor for one output size.
pub struct Renderer {
    width: usize,
    height: usize,
    scene: Scene,
    pip: PipSpec,
    cam_aspect: f64,
}

impl Renderer {
    /// A renderer for `width`x`height` output with this overlay.
    pub fn new(width: usize, height: usize, pip: PipSpec) -> Result<Self> {
        let _ = (width, height, pip);
        Err(not_implemented("GPU compositing", PHASE))
    }

    pub fn set_scene(&mut self, scene: Scene) {
        self.scene = scene;
    }

    pub fn set_pip(&mut self, pip: PipSpec) {
        self.pip = pip;
    }

    /// Aspect ratio of the last camera frame (16:9 until one arrives).
    pub fn cam_aspect(&self) -> f64 {
        self.cam_aspect
    }

    /// Composite the current scene, stamped at `pts`.
    pub fn render(
        &mut self,
        screen: Option<&VideoFrame>,
        cam: Option<&VideoFrame>,
        pts: f64,
    ) -> Result<VideoFrame> {
        let _ = (screen, cam, pts);
        Err(not_implemented("GPU compositing", PHASE))
    }
}
