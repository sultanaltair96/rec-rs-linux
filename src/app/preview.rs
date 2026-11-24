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

#[cfg(test)]
use egui::ColorImage;

#[cfg(test)]
use crate::media_types::PixelFormat;
use crate::media_types::{FrameSink, VideoFrame};

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

/// Exactly one representation of an emitted preview frame.
pub enum Frame {
    Cpu(VideoFrame),
    Gpu(crate::gpu::PreviewFrame),
}

#[derive(Default)]
struct Inbox {
    ready: Option<Frame>,
    pending_gpu: Option<crate::gpu::PreviewFrame>,
}

/// The preview inbox on the UI side. The session calls its sinks serially
/// from the writer thread: optional GPU first, mandatory CPU second. The CPU
/// callback commits one representation, so UI redraw timing cannot alternate
/// between two representations of the same emission.
pub struct Tap {
    slot: Arc<Mutex<Inbox>>,
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
            slot: Arc::new(Mutex::new(Inbox::default())),
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
            {
                let mut s = slot.lock().unwrap();
                // The writer calls the GPU sink (if available), then this CPU
                // sink, serially for each emission. Commit just ONE item here.
                // A redraw between callbacks cannot consume half of a pair.
                let gpu = s.pending_gpu.take();
                if !wanted.load(Ordering::Relaxed) {
                    return;
                }
                // Do not pair by pts: pipelined readback can lag the texture.
                s.ready = Some(match gpu {
                    Some(tex) => Frame::Gpu(tex),
                    None => Frame::Cpu(frame),
                });
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

    /// The writer must call this sink before the CPU sink for the same
    /// emission, on the same producer thread. Only the CPU sink publishes and
    /// wakes. Absence of a GPU callback on the next emission restores CPU
    /// fallback immediately (no sticky GPU mode).
    pub fn gpu_sink(&self) -> crate::gpu::PreviewSink {
        let slot = self.slot.clone();
        Arc::new(move |frame| {
            slot.lock().unwrap().pending_gpu = Some(frame);
        })
    }

    /// Whether frames are wanted (the main window is visible). Turning it off
    /// drops what is buffered, so nothing is drawn while hidden.
    pub fn set_wanted(&mut self, on: bool) {
        let mut s = self.slot.lock().unwrap();
        self.wanted.store(on, Ordering::Relaxed);
        if !on {
            s.ready = None;
            // Keep the pending half-pair until its CPU callback, even if the
            // window hides/shows between callbacks.
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

    #[cfg(test)]
    pub fn target(&self) -> (u32, u32) {
        *self.target.lock().unwrap()
    }

    /// The newest frame, if one arrived since the last call.
    pub fn take(&self) -> Option<Frame> {
        self.slot.lock().unwrap().ready.take()
    }

    /// The new frame as an egui image for the current preview area, with its
    /// size in pixels. `None` when the frame is malformed.
    ///
    /// The app draws frames with `preview_gpu` (G10); this CPU path stays as
    /// the reference the unit tests check, and its YUV maths is what the
    /// shader implements.
    #[cfg(test)]
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
///
/// A packed frame is resampled and swizzled in one pass: a 4K capture at
/// preview size then costs the preview's pixels, not the frame's.
#[cfg(test)]
pub fn image(frame: &VideoFrame, size: (usize, usize)) -> Option<(ColorImage, [u32; 2])> {
    let (dw, dh) = size;
    if dw < 2 || dh < 2 || !frame.is_well_formed() {
        return None;
    }
    let (sw, sh) = (frame.width, frame.height);
    let (dw, dh) = (dw.min(sw.max(2)), dh.min(sh.max(2)));
    let pixels = match frame.format {
        PixelFormat::Bgra | PixelFormat::Rgba => {
            let plane = frame.planes.first()?;
            resample_packed(plane, sw, sh, dw, dh, frame.format == PixelFormat::Bgra)?
        }
        _ => {
            let (ws, hs, rgba) = to_rgba(frame)?;
            let (dw, dh) = (dw.min(ws.max(2)), dh.min(hs.max(2)));
            if (dw, dh) == (ws, hs) {
                rgba.chunks_exact(4)
                    .map(|p| egui::Color32::from_rgb(p[0], p[1], p[2]))
                    .collect()
            } else {
                resample(&rgba, ws, hs, dw, dh)
                    .chunks_exact(4)
                    .map(|p| egui::Color32::from_rgb(p[0], p[1], p[2]))
                    .collect()
            }
        }
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

/// Bilinear resample of a packed BGRA (or RGBA) plane, straight to egui
/// colours: one pass, no intermediate RGBA frame.
#[cfg(test)]
fn resample_packed(
    plane: &crate::media_types::Plane,
    sw: usize,
    sh: usize,
    dw: usize,
    dh: usize,
    bgra: bool,
) -> Option<Vec<egui::Color32>> {
    if sw == 0 || sh == 0 || plane.stride < sw * 4 {
        return None;
    }
    let fx = sw as f64 / dw as f64;
    let fy = sh as f64 / dh as f64;
    let texel = |px: usize, py: usize| -> [f32; 4] {
        let o = py * plane.stride + px * 4;
        let p = &plane.data[o..o + 4];
        [p[0] as f32, p[1] as f32, p[2] as f32, p[3] as f32]
    };
    let mut out = Vec::with_capacity(dw * dh);
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
            let (a, b, c, d) = (texel(x0, y0), texel(x1, y0), texel(x0, y1), texel(x1, y1));
            let mut px = [0f32; 4];
            for ch in 0..4 {
                let top = a[ch] * (1.0 - wx) + b[ch] * wx;
                let bottom = c[ch] * (1.0 - wx) + d[ch] * wx;
                px[ch] = (top * (1.0 - wy) + bottom * wy).round().clamp(0.0, 255.0);
            }
            let (r, bch) = if bgra { (px[2], px[0]) } else { (px[0], px[2]) };
            out.push(egui::Color32::from_rgb(r as u8, px[1] as u8, bch as u8));
        }
    }
    Some(out)
}

/// Any supported frame as tight RGBA at its own size.
#[cfg(test)]
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
                    // NV12: U and V are interleaved, two bytes per pixel pair
                    // (so the byte offset is (x/2)*2, not (x/2)*stride).
                    // I420: one plane each, half width.
                    let (u, v) = match frame.format {
                        PixelFormat::Nv12 => {
                            let at = chroma_row * u_plane.stride + (x / 2) * 2;
                            (u_plane.data[at], u_plane.data[at + 1])
                        }
                        _ => {
                            let cx = (x / 2).min(chroma_w - 1);
                            let u_at = chroma_row * u_plane.stride + cx;
                            let v_at = chroma_row * v_plane.stride + cx;
                            (u_plane.data[u_at], v_plane.data[v_at])
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

/// One BT.709 limited-range YUV sample as RGB (what a screen capture
/// delivers). The preview shader (`shaders/preview.wgsl`) uses the same
/// constants.
#[cfg(test)]
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
#[cfg(test)]
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

    fn nv12_frame(w: usize, h: usize) -> VideoFrame {
        let y = Plane::new(vec![120u8; w * h], w);
        let uv = Plane::new(
            vec![128u8; w.div_ceil(2) * 2 * h.div_ceil(2)],
            w.div_ceil(2) * 2,
        );
        VideoFrame {
            pts: 1.0,
            width: w,
            height: h,
            format: PixelFormat::Nv12,
            planes: vec![y, uv],
        }
    }

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
        // NV12: a black frame with neutral grey chroma is black, and the
        // interleaved U/V bytes land on the right pixel pairs (a stride-width
        // mistake made the old code read past the plane on real frames).
        let y = Plane::new(vec![16u8; 8 * 2], 8);
        // Row 0: U=128,V=128 for pixels 0-1, U=90,V=240 for pixels 2-3,
        // U=240,V=90 for pixels 4-5, U=128,V=128 for pixels 6-7 (1 row per 2
        // luma rows).
        let uv = Plane::new(vec![128, 128, 90, 240, 240, 90, 128, 128], 8);
        let f = VideoFrame {
            pts: 1.0,
            width: 8,
            height: 2,
            format: PixelFormat::Nv12,
            planes: vec![y, uv],
        };
        let (img, _) = image(&f, (8, 2)).expect("image");
        assert_eq!(img.pixels[0], egui::Color32::BLACK, "neutral chroma");
        let red = img.pixels[2];
        assert!(red.r() > 150 && red.g() < 80 && red.b() < 80, "{red:?}");
        let blue = img.pixels[4];
        assert!(blue.b() > 200 && blue.g() < 80 && blue.r() < 80, "{blue:?}");
    }

    #[test]
    fn only_the_newest_frame_is_kept_and_none_while_hidden() {
        let (mut tap, sink) = Tap::new(Arc::new(|| {}));
        sink(bgra_frame(4, 2, [0, 0, 0, 255]));
        assert!(tap.take().is_none(), "nothing wanted yet");
        tap.set_wanted(true);
        sink(bgra_frame(4, 2, [0, 0, 0, 255]));
        sink(bgra_frame(4, 2, [10, 20, 30, 255]));
        let Frame::Cpu(f) = tap.take().expect("a frame") else {
            panic!("expected CPU fallback");
        };
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

    /// What a preview frame costs now (run in release:
    /// `cargo test --release app::preview::tests::preview_cost -- --ignored --nocapture`).
    /// The CPU only copies the frame's planes into the queue; the conversion and
    /// scaling happen in `preview_gpu`'s shader (G10). Before G10 this path cost
    /// 6.61 ms (1080p) and 8.15 ms (4K) per frame.
    #[test]
    #[ignore]
    fn preview_cost() {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let Some(adapter) = pollster::block_on(instance.request_adapter(&Default::default())).ok()
        else {
            eprintln!("SKIP: no wgpu adapter for the preview cost test");
            return;
        };
        let Ok((device, _queue)) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
        else {
            eprintln!("SKIP: no wgpu device for the preview cost test");
            return;
        };
        let mut painter = super::super::preview_gpu::PreviewPainter::new(
            &device,
            wgpu::TextureFormat::Bgra8Unorm,
        );
        let n = 30;
        for (w, h, format) in [
            (1920usize, 1080usize, PixelFormat::Bgra),
            (1920, 1080, PixelFormat::Nv12),
            (3840, 2160, PixelFormat::Bgra),
            (3840, 2160, PixelFormat::Nv12),
        ] {
            let frame = match format {
                PixelFormat::Bgra => bgra_frame(w, h, [10, 20, 30, 255]),
                _ => nv12_frame(w, h),
            };
            // One upload first: the first call allocates the textures.
            let mut enc = device.create_command_encoder(&Default::default());
            painter.upload(&device, &mut enc, &frame);
            let t = std::time::Instant::now();
            for _ in 0..n {
                let mut enc = device.create_command_encoder(&Default::default());
                std::hint::black_box(painter.upload(&device, &mut enc, &frame));
            }
            println!(
                "{w}x{h} {} -> upload: {:.2} ms/frame",
                format.label(),
                t.elapsed().as_secs_f64() * 1000.0 / n as f64
            );
        }
        // The texture path (G10): the compositor's own texture drawn in the
        // preview rect. No readback, no upload — one bind group and one draw.
        // 704x396 is the preview rect the audit measured.
        for (w, h) in [(1920u32, 1080u32), (3840, 2160)] {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("composited frame"),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bgra8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let (cpu, gpu) = super::super::preview_gpu::texture_path_cost(
                &device,
                &_queue,
                &mut painter,
                &texture,
                wgpu::TextureFormat::Bgra8Unorm,
                (704, 396),
                n,
            );
            println!(
                "{w}x{h} texture -> {cpu:.3} ms/frame of CPU, {gpu:.3} ms/frame of GPU throughput"
            );
        }
    }
}
