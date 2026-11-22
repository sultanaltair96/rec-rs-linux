//! Live preview: the newest composited frame, converted to an egui texture.
//!
//! Upstream scales the frame with Metal Performance Shaders and wraps the
//! IOSurface as a wgpu texture. Without a compositor to hand us a GPU frame we
//! do the same work on the CPU: one bilinear resample to the preview area's
//! pixel size, BGRA/RGBA/NV12/I420 to RGBA, then an egui texture upload. This
//! keeps the preview independent of the GPU compositor (phase 1C), which will
//! later hand the app a texture instead (see `record::Options::preview`).
//!
//! The tap never queues: it keeps only the newest frame, and drops frames
//! while the window is hidden, so a slow preview can never slow a recording.
//!
//! Adapted from rec-rs src/app/preview.rs (MIT OR Apache-2.0); `target_size`
//! and its tests are ported unchanged, the Metal path is replaced.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::ColorImage;

use crate::media_types::{FrameSink, PixelFormat, VideoFrame};

/// The pixel size to render a `fw`x`fh` frame at for the preview area: its
/// aspect ratio fitted into the area, never larger than the frame. Low power
/// halves it (and the app also lowers the frame rate).
pub fn target_size(fw: usize, fh: usize, area: (u32, u32), low_power: bool) -> (usize, usize) {
    let (aw, ah) = if area.0 == 0 || area.1 == 0 {
        (960.0, 540.0)
    } else {
        (area.0 as f64, area.1 as f64)
    };
    let mut s = (aw / fw as f64).min(ah / fh as f64).min(1.0);
    if low_power {
        s *= 0.5;
    }
    (
        ((fw as f64 * s).round() as usize).max(2) & !1,
        ((fh as f64 * s).round() as usize).max(2) & !1,
    )
}

/// The preview side of the app: where preview frames go.
///
/// `Tap` lives on the UI side; the [`FrameSink`] returned next to it is the
/// one the capture session calls (on its capture thread, from
/// `record::Options::preview`).
pub struct Tap {
    slot: Arc<Mutex<Option<VideoFrame>>>,
    wanted: Arc<AtomicBool>,
    low_power: Arc<AtomicBool>,
    target: Arc<Mutex<(u32, u32)>>,
    /// When the UI was last woken, so a 60 fps capture doesn't post a redraw
    /// per frame; the UI shows 30 fps (15 in low power).
    last_wake: Instant,
}

impl Default for Tap {
    fn default() -> Self {
        Tap {
            slot: Arc::new(Mutex::new(None)),
            wanted: Arc::new(AtomicBool::new(false)),
            low_power: Arc::new(AtomicBool::new(false)),
            target: Arc::new(Mutex::new((0, 0))),
            last_wake: Instant::now(),
        }
    }
}

impl Tap {
    /// The tap and the sink the session delivers to. `wake` requests a redraw
    /// (it runs on the capture thread and must not block).
    pub fn new(wake: Arc<dyn Fn() + Send + Sync>) -> (Tap, FrameSink) {
        let tap = Tap::default();
        let sink = tap.sink(wake);
        (tap, sink)
    }

    fn sink(&self, wake: Arc<dyn Fn() + Send + Sync>) -> FrameSink {
        let slot = self.slot.clone();
        let wanted = self.wanted.clone();
        let low_power = self.low_power.clone();
        let last_wake = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(1)));
        Arc::new(move |frame: VideoFrame| {
            // Hidden: drop the frame, don't even copy it.
            if !wanted.load(Ordering::Relaxed) {
                return;
            }
            if let Ok(mut s) = slot.lock() {
                // Newest only: the previous frame is dropped, never queued.
                *s = Some(frame);
            }
            let interval = if low_power.load(Ordering::Relaxed) {
                Duration::from_millis(66)
            } else {
                Duration::from_millis(33)
            };
            let mut last = last_wake.lock().unwrap();
            if last.elapsed() >= interval {
                *last = Instant::now();
                drop(last);
                wake();
            }
        })
    }

    /// Whether frames are wanted (the main window is visible). Turning it off
    /// drops what is buffered, so nothing is drawn while hidden.
    pub fn set_wanted(&mut self, on: bool) {
        self.wanted.store(on, Ordering::Relaxed);
        if !on {
            self.slot.lock().unwrap().take();
            self.last_wake = Instant::now();
        }
    }

    /// Whether frames are wanted (the main window is visible and not covered).
    pub fn wanted(&self) -> bool {
        self.wanted.load(Ordering::Relaxed)
    }

    pub fn set_low_power(&self, on: bool) {
        self.low_power.store(on, Ordering::Relaxed);
    }

    /// The preview area's size in pixels (set by the UI each frame).
    pub fn set_target(&self, w: u32, h: u32) {
        *self.target.lock().unwrap() = (w, h);
    }

    pub fn target(&self) -> (u32, u32) {
        *self.target.lock().unwrap()
    }

    /// The newest frame, if one arrived since the last call.
    pub fn take(&self) -> Option<VideoFrame> {
        self.slot.lock().unwrap().take()
    }

    /// The new frame as an egui texture for the current preview area, with its
    /// size in pixels. `None` when the frame is malformed.
    pub fn image(&self, frame: &VideoFrame) -> Option<(ColorImage, [u32; 2])> {
        let (w, h) = target_size(
            frame.width,
            frame.height,
            self.target(),
            self.low_power.load(Ordering::Relaxed),
        );
        image(frame, (w, h))
    }
}

/// One frame at `size` as an egui image (RGBA, pointed at by egui as
/// gamma-encoded values: egui's textures are not sRGB).
pub fn image(frame: &VideoFrame, size: (usize, usize)) -> Option<(ColorImage, [u32; 2])> {
    let (dw, dh) = size;
    if dw < 2 || dh < 2 || !frame.is_well_formed() {
        return None;
    }
    let (sw, sh, rgba) = to_rgba(frame)?;
    let (dw, dh) = (dw.min(sw.max(2)), dh.min(sh.max(2)));
    let pixels = if (dw, dh) == (sw, sh) {
        rgba.chunks_exact(4)
            .map(|p| egui::Color32::from_rgb(p[0], p[1], p[2]))
            .collect()
    } else {
        resample(&rgba, sw, sh, dw, dh)
            .chunks_exact(4)
            .map(|p| egui::Color32::from_rgb(p[0], p[1], p[2]))
            .collect()
    };
    Some((
        ColorImage {
            size: [dw, dh],
            source_size: egui::vec2(dw as f32, dh as f32),
            pixels,
        },
        [dw as u32, dh as u32],
    ))
}

/// Any supported frame as tight RGBA at its own size.
fn to_rgba(frame: &VideoFrame) -> Option<(usize, usize, Vec<u8>)> {
    let (w, h) = (frame.width, frame.height);
    let mut out = Vec::with_capacity(w * h * 4);
    match frame.format {
        PixelFormat::Bgra | PixelFormat::Rgba => {
            let plane = frame.planes.first()?;
            let bgra = frame.format == PixelFormat::Bgra;
            for y in 0..h {
                let row = &plane.data[y * plane.stride..][..w * 4];
                for px in row.chunks_exact(4) {
                    let (r, b) = if bgra { (px[2], px[0]) } else { (px[0], px[2]) };
                    out.extend_from_slice(&[r, px[1], b, 255]);
                }
            }
        }
        PixelFormat::Nv12 | PixelFormat::I420 => {
            let (y_plane, u_plane, v_plane) = match frame.format {
                PixelFormat::Nv12 => {
                    let y = frame.planes.first()?;
                    let uv = frame.planes.get(1)?;
                    (y, uv, uv)
                }
                _ => (
                    frame.planes.first()?,
                    frame.planes.get(1)?,
                    frame.planes.get(2)?,
                ),
            };
            let chroma_w = w.div_ceil(2);
            for y in 0..h {
                let yrow = &y_plane.data[y * y_plane.stride..][..w];
                let chroma_row = y / 2;
                for (x, luma) in yrow.iter().enumerate().take(w) {
                    let cw = chroma_row * u_plane.stride + (x / 2);
                    let (u, v) = match frame.format {
                        PixelFormat::Nv12 => (u_plane.data[cw * 2], u_plane.data[cw * 2 + 1]),
                        _ => {
                            let v_row = chroma_row * v_plane.stride + (x / 2).min(chroma_w - 1);
                            let u_row = chroma_row * u_plane.stride + (x / 2).min(chroma_w - 1);
                            (u_plane.data[u_row], v_plane.data[v_row])
                        }
                    };
                    let (r, g, b) = yuv_to_rgb(*luma, u, v);
                    out.extend_from_slice(&[r, g, b, 255]);
                }
            }
        }
    }
    Some((w, h, out))
}

/// One BT.709 limited-range YUV sample as RGB (what a screen capture delivers).
pub fn yuv_to_rgb(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    let y = (y as f32 - 16.0) * (255.0 / 219.0);
    let u = u as f32 - 128.0;
    let v = v as f32 - 128.0;
    let r = y + 1.5748 * v;
    let g = y - 0.1873 * u - 0.4681 * v;
    let b = y + 1.8556 * u;
    let c = |x: f32| x.clamp(0.0, 255.0).round() as u8;
    (c(r), c(g), c(b))
}

/// Bilinear RGBA resample from `sw`x`sh` to `dw`x`dh` (pixel centres aligned).
fn resample(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(dw * dh * 4);
    let fx = sw as f64 / dw as f64;
    let fy = sh as f64 / dh as f64;
    for y in 0..dh {
        let sy = ((y as f64 + 0.5) * fy - 0.5).max(0.0);
        let y0 = (sy.floor() as usize).min(sh - 1);
        let y1 = (y0 + 1).min(sh - 1);
        let wy = (sy - y0 as f64) as f32;
        for x in 0..dw {
            let sx = ((x as f64 + 0.5) * fx - 0.5).max(0.0);
            let x0 = (sx.floor() as usize).min(sw - 1);
            let x1 = (x0 + 1).min(sw - 1);
            let wx = (sx - x0 as f64) as f32;
            let at = |px: usize, py: usize, c: usize| src[(py * sw + px) * 4 + c] as f32;
            for c in 0..3 {
                let top = at(x0, y0, c) * (1.0 - wx) + at(x1, y0, c) * wx;
                let bottom = at(x0, y1, c) * (1.0 - wx) + at(x1, y1, c) * wx;
                out.push((top * (1.0 - wy) + bottom * wy).round() as u8);
            }
            out.push(255);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_types::{PixelFormat, Plane};

    fn bgra_frame(w: usize, h: usize, px: [u8; 4]) -> VideoFrame {
        let mut data = Vec::new();
        for _ in 0..w * h {
            data.extend_from_slice(&px);
        }
        VideoFrame::packed(1.0, w, h, PixelFormat::Bgra, data)
    }

    #[test]
    fn preview_size_follows_the_area_in_pixels() {
        // A 4K frame in a 1390x782 pt area on a 2x display: 2780x1564 px.
        assert_eq!(target_size(3840, 2160, (2780, 1564), false), (2780, 1564));
        // Never upscaled; aspect kept; low power halves it.
        assert_eq!(target_size(1920, 1080, (2780, 1564), false), (1920, 1080));
        assert_eq!(target_size(3840, 2160, (2000, 2000), false), (2000, 1124));
        assert_eq!(target_size(3840, 2160, (2780, 1564), true), (1390, 782));
        // Unknown area: a sane default, and odd sizes round down to even.
        assert_eq!(target_size(1920, 1080, (0, 0), false), (960, 540));
        assert_eq!(target_size(1001, 1001, (500, 500), false), (500, 500));
    }

    #[test]
    fn frames_are_shown_byte_for_byte_without_a_second_scale() {
        let f = bgra_frame(4, 2, [1, 2, 3, 255]);
        let (img, size) = image(&f, (4, 2)).expect("image");
        assert_eq!(size, [4, 2]);
        // BGRA in, RGBA out.
        assert_eq!(img.pixels[0], egui::Color32::from_rgb(3, 2, 1));
        assert_eq!(img.pixels.len(), 8);
    }

    #[test]
    fn frames_larger_than_the_preview_are_resampled() {
        let f = bgra_frame(8, 8, [0, 0, 255, 255]);
        let (img, size) = image(&f, (4, 4)).expect("image");
        assert_eq!(size, [4, 4]);
        assert_eq!(img.size, [4, 4]);
        // Uniform colour stays uniform whatever the scale.
        assert!(img.pixels.iter().all(|p| *p == egui::Color32::RED));
    }

    #[test]
    fn yuv_planes_convert_as_bt709_limited_range() {
        assert_eq!(yuv_to_rgb(16, 128, 128), (0, 0, 0));
        assert_eq!(yuv_to_rgb(235, 128, 128), (255, 255, 255));
        let (r, g, b) = yuv_to_rgb(80, 90, 240);
        assert!(r > 200 && g < 80 && b < 80, "{r} {g} {b}");
        // NV12: a black frame with neutral grey chroma is black.
        let y = Plane::new(vec![16u8; 4 * 2], 4);
        let uv = Plane::new(vec![128u8; 4], 4);
        let f = VideoFrame {
            pts: 1.0,
            width: 4,
            height: 2,
            format: PixelFormat::Nv12,
            planes: vec![y, uv],
        };
        let (img, _) = image(&f, (4, 2)).expect("image");
        assert!(img.pixels.iter().all(|p| *p == egui::Color32::BLACK));
    }

    #[test]
    fn only_the_newest_frame_is_kept_and_none_while_hidden() {
        let (mut tap, sink) = Tap::new(Arc::new(|| {}));
        sink(bgra_frame(4, 2, [0, 0, 0, 255]));
        assert!(tap.take().is_none(), "nothing wanted yet");
        tap.set_wanted(true);
        sink(bgra_frame(4, 2, [0, 0, 0, 255]));
        sink(bgra_frame(4, 2, [10, 20, 30, 255]));
        let f = tap.take().expect("a frame");
        assert_eq!(f.planes[0].data[0], 10, "the newest frame wins");
        assert!(tap.take().is_none(), "and only once");
        // Hidden: frames are dropped, not buffered.
        tap.set_wanted(false);
        sink(bgra_frame(4, 2, [1, 1, 1, 255]));
        assert!(tap.take().is_none());
        assert!(!tap.wanted());
    }

    #[test]
    fn the_preview_target_is_the_area_in_pixels() {
        let (tap, _sink) = Tap::new(Arc::new(|| {}));
        tap.set_target(2560, 1440);
        assert_eq!(tap.target(), (2560, 1440));
        // Full quality: the frame at the area's pixel size, never upscaled.
        let f = bgra_frame(2560, 1440, [0, 0, 0, 255]);
        let (_, size) = tap.image(&f).expect("image");
        assert_eq!(size, [2560, 1440]);
        // Low power: half resolution, so the UI and the CPU do half the work.
        tap.set_low_power(true);
        let (_, size) = tap.image(&f).expect("image");
        assert_eq!(size, [1280, 720], "low power halves the target");
        // A smaller frame is never upscaled.
        tap.set_low_power(false);
        let small = bgra_frame(640, 360, [0, 0, 0, 255]);
        let (_, size) = tap.image(&small).expect("image");
        assert_eq!(size, [640, 360]);
    }
}
