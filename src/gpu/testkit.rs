//! Test support for the compositor: synthetic screen and camera frames, an
//! image to inspect the readback with, and a renderer that skips cleanly on a
//! machine with no adapter at all.
//!
//! The synthetic frames are deterministic and built for the properties the
//! tests check: the screen is a test card with flat fields, hairline rules, a
//! diagonal and a fine checker patch (Lanczos ringing and text-like detail),
//! and the camera is a gradient with a marker plus coloured thirds (so a flip
//! is unmistakable).

use std::sync::{Arc, OnceLock};

use crate::compose::{self, PipSpec, Scene};
use crate::media_types::{PixelFormat, VideoFrame};

use super::{Options, OutputFormat, Renderer};

/// The device, queue, adapter info and description every test renders with.
type Shared = (
    Arc<wgpu::Device>,
    Arc<wgpu::Queue>,
    wgpu::AdapterInfo,
    String,
);

/// The adapter every test renders on, created once: opening thirty Vulkan
/// devices in one test process is slow and upsets the software driver.
fn shared() -> Option<&'static Shared> {
    static SHARED: OnceLock<Option<Shared>> = OnceLock::new();
    SHARED
        .get_or_init(|| {
            // A throwaway renderer owns the device; keep its device and queue.
            match Renderer::with_options(&Options {
                width: 16,
                height: 16,
                output: OutputFormat::Bgra,
                pipeline_depth: 1,
                ..Default::default()
            }) {
                Ok(r) => {
                    let description = r.describe_adapter();
                    Some((
                        r.device().clone(),
                        r.queue().clone(),
                        r.adapter_info().clone(),
                        description,
                    ))
                }
                Err(e) => {
                    eprintln!("SKIP: no GPU adapter for the compositor tests ({e:#})");
                    None
                }
            }
        })
        .as_ref()
}

/// Whether a GPU is available at all, decided once.
pub fn gpu() -> Option<&'static str> {
    shared().map(|s| s.3.as_str())
}

/// A renderer in a test's size, or `None` (with a printed reason) when there is
/// no adapter. Logs which adapter every test ran on, once.
pub fn renderer(width: usize, height: usize, pip: PipSpec, scene: Scene) -> Option<Renderer> {
    build(&Options {
        width,
        height,
        pip,
        scene,
        output: OutputFormat::Bgra,
        corner_fill: false,
        // Synchronous: every test wants the frame it just asked for.
        pipeline_depth: 1,
    })
}

/// [`Renderer::with_options`] for a test: `None` only when there is no adapter
/// at all, a panic when the compositor cannot be created on one that exists
/// (a broken shader must not turn into a silently skipped test).
pub fn build(options: &Options) -> Option<Renderer> {
    let (device, queue, info, description) = shared()?;
    static SAID: OnceLock<()> = OnceLock::new();
    SAID.get_or_init(|| eprintln!("compositor tests run on: {description}"));
    match Renderer::with_device(device.clone(), queue.clone(), info.clone(), options) {
        Ok(r) => Some(r),
        Err(e) => panic!("the compositor could not be created on {description}: {e:#}"),
    }
}

/// A screen test card: `w`x`h` BGRA, deterministic.
///
/// Four flat quadrants, a white rule every 16 px, a red diagonal, a black
/// crosshair at the centre and a 1-px checkerboard patch: plenty of hard
/// edges and gradients for a scaler to show ringing and chroma errors on.
pub fn screen(w: usize, h: usize) -> VideoFrame {
    let mut px = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let left = x < w / 2;
            let top = y < h / 2;
            let mut rgb = match (left, top) {
                (true, true) => [32, 40, 64],
                (false, true) => [200, 200, 195],
                (true, false) => [24, 36, 28],
                (false, false) => [90, 70, 40],
            };
            // Hairline rules, like UI chrome.
            if x % 16 == 0 || y % 16 == 0 {
                rgb = [128, 128, 128];
            }
            // A diagonal: sub-pixel geometry an interpolating scaler smears.
            if (x as i64 - y as i64 * 2).abs() < 2 {
                rgb = [220, 40, 40];
            }
            // A fine checker: 1-px detail, the worst case for a downscale.
            if (w / 8..w / 8 + 32).contains(&x) && (h / 8..h / 8 + 32).contains(&y) {
                rgb = if (x + y) % 2 == 0 {
                    [0, 0, 0]
                } else {
                    [255, 255, 255]
                };
            }
            // A centre marker.
            if (x as i64 - w as i64 / 2).abs() <= 2 || (y as i64 - h as i64 / 2).abs() <= 2 {
                rgb = [255, 255, 255];
            }
            let at = (y * w + x) * 4;
            px[at] = rgb[2];
            px[at + 1] = rgb[1];
            px[at + 2] = rgb[0];
            px[at + 3] = 255;
        }
    }
    VideoFrame::packed(0.0, w, h, PixelFormat::Bgra, px)
}

/// A camera frame: `w`x`h` BGRA with a horizontal gradient, a bright cross in
/// the middle and distinctly coloured thirds, so a mirror, a crop or a
/// misplaced overlay all show up.
pub fn cam(w: usize, h: usize) -> VideoFrame {
    let mut px = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let t = x as f32 / (w.max(2) - 1) as f32;
            let v = y as f32 / (h.max(2) - 1) as f32;
            // Thirds: red, green, blue (left to right in the source).
            let base = if t < 1.0 / 3.0 {
                [220, 20, 20]
            } else if t < 2.0 / 3.0 {
                [20, 200, 20]
            } else {
                [20, 20, 220]
            };
            // A vertical gradient over it, still inside each third's hue.
            let shade = 0.65 + 0.35 * (1.0 - v);
            let mut rgb = [
                (base[0] as f32 * shade) as u8,
                (base[1] as f32 * shade) as u8,
                (base[2] as f32 * shade) as u8,
            ];
            // A white cross at the centre, off-centre horizontally.
            let cx = w as f32 * 0.35;
            let cy = h as f32 * 0.5;
            if ((x as f32 - cx).abs() <= 1.0 && (y as f32 - cy).abs() <= h as f32 * 0.2)
                || ((y as f32 - cy).abs() <= 1.0 && (x as f32 - cx).abs() <= w as f32 * 0.2)
            {
                rgb = [255, 255, 255];
            }
            let at = (y * w + x) * 4;
            px[at] = rgb[2];
            px[at + 1] = rgb[1];
            px[at + 2] = rgb[0];
            px[at + 3] = 255;
        }
    }
    VideoFrame::packed(0.0, w, h, PixelFormat::Bgra, px)
}

/// A 1-px black and white pattern: the highest frequency a source can carry,
/// and therefore the sharpest test of a downscaler. `vertical` gives stripes,
/// otherwise a checkerboard.
pub fn stripes(w: usize, h: usize, vertical: bool) -> VideoFrame {
    let mut px = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let on = if vertical {
                x % 2 == 0
            } else {
                (x + y) % 2 == 0
            };
            let v = if on { 255 } else { 0 };
            let at = (y * w + x) * 4;
            px[at..at + 4].copy_from_slice(&[v, v, v, 255]);
        }
    }
    VideoFrame::packed(0.0, w, h, PixelFormat::Bgra, px)
}

/// A solid frame in a chosen format, for colour and coverage checks.
pub fn solid(w: usize, h: usize, rgb: [u8; 3]) -> VideoFrame {
    let mut px = vec![0u8; w * h * 4];
    for p in px.chunks_exact_mut(4) {
        p[0] = rgb[2];
        p[1] = rgb[1];
        p[2] = rgb[0];
        p[3] = 255;
    }
    VideoFrame::packed(0.0, w, h, PixelFormat::Bgra, px)
}

/// The same frame as NV12, BT.709 limited range, for the upload path.
pub fn solid_nv12(w: usize, h: usize, rgb: [u8; 3]) -> VideoFrame {
    let (y, u, v) = super::yuv::rgb_to_yuv709_limited(rgb);
    let ch = h.div_ceil(2);
    let mut planes = Vec::new();
    planes.push(crate::media_types::Plane::new(vec![y; w * h], w));
    let mut uv = vec![0u8; w * ch];
    for pair in uv.chunks_exact_mut(2) {
        pair[0] = u;
        pair[1] = v;
    }
    planes.push(crate::media_types::Plane::new(uv, w));
    VideoFrame {
        pts: 0.0,
        width: w,
        height: h,
        format: PixelFormat::Nv12,
        planes,
    }
}

/// The same colour as RGBA, for the unpacked upload path.
pub fn solid_rgba(w: usize, h: usize, rgb: [u8; 3]) -> VideoFrame {
    let mut px = vec![0u8; w * h * 4];
    for p in px.chunks_exact_mut(4) {
        p[0] = rgb[0];
        p[1] = rgb[1];
        p[2] = rgb[2];
        p[3] = 255;
    }
    VideoFrame::packed(0.0, w, h, PixelFormat::Rgba, px)
}

/// The same colour as I420, BT.709 limited range.
pub fn solid_i420(w: usize, h: usize, rgb: [u8; 3]) -> VideoFrame {
    let (y, u, v) = super::yuv::rgb_to_yuv709_limited(rgb);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    VideoFrame {
        pts: 0.0,
        width: w,
        height: h,
        format: PixelFormat::I420,
        planes: vec![
            crate::media_types::Plane::new(vec![y; w * h], w),
            crate::media_types::Plane::new(vec![u; cw * ch], cw),
            crate::media_types::Plane::new(vec![v; cw * ch], cw),
        ],
    }
}

/// A rendered frame, as RGB, for looking at pixels.
pub struct Image {
    pub width: usize,
    pub height: usize,
    /// Row-major RGB.
    pub rgb: Vec<[u8; 3]>,
}

impl Image {
    /// Read a BGRA frame (what the tests ask the renderer for).
    pub fn bgra(frame: &VideoFrame) -> Image {
        assert_eq!(frame.format, PixelFormat::Bgra, "the tests read BGRA");
        let stride = frame.planes[0].stride;
        let data = &frame.planes[0].data;
        let mut rgb = Vec::with_capacity(frame.width * frame.height);
        for y in 0..frame.height {
            for x in 0..frame.width {
                let at = y * stride + x * 4;
                rgb.push([data[at + 2], data[at + 1], data[at]]);
            }
        }
        Image {
            width: frame.width,
            height: frame.height,
            rgb,
        }
    }

    pub fn at(&self, x: usize, y: usize) -> [u8; 3] {
        self.rgb[y * self.width + x]
    }

    /// The largest and mean absolute difference against another image, per
    /// channel.
    pub fn diff(&self, other: &Image) -> (u8, f64) {
        assert_eq!((self.width, self.height), (other.width, other.height));
        let mut max = 0u8;
        let mut sum = 0f64;
        for (a, b) in self.rgb.iter().zip(other.rgb.iter()) {
            for c in 0..3 {
                let d = a[c].abs_diff(b[c]);
                max = max.max(d);
                sum += d as f64;
            }
        }
        (max, sum / (self.rgb.len() * 3) as f64)
    }

    /// The mean of one channel over the whole image.
    pub fn mean(&self, channel: usize) -> f64 {
        let sum: f64 = self.rgb.iter().map(|p| p[channel] as f64).sum();
        sum / self.rgb.len().max(1) as f64
    }

    /// The largest distance any pixel is from one value over all channels,
    /// ignoring a border `margin` pixels wide.
    pub fn max_deviation(&self, want: f64, margin: usize) -> (f64, usize, usize) {
        let mut worst = 0.0;
        let (mut at_x, mut at_y) = (0, 0);
        for y in margin..self.height.saturating_sub(margin) {
            for x in margin..self.width.saturating_sub(margin) {
                for v in self.at(x, y) {
                    let d = (v as f64 - want).abs();
                    if d > worst {
                        worst = d;
                        at_x = x;
                        at_y = y;
                    }
                }
            }
        }
        (worst, at_x, at_y)
    }

    /// Peak signal-to-noise ratio against another image, in dB.
    pub fn psnr(&self, other: &Image) -> f64 {
        assert_eq!((self.width, self.height), (other.width, other.height));
        let mut sq = 0f64;
        for (a, b) in self.rgb.iter().zip(other.rgb.iter()) {
            for c in 0..3 {
                let d = a[c] as f64 - b[c] as f64;
                sq += d * d;
            }
        }
        let mse = sq / (self.rgb.len() * 3).max(1) as f64;
        if mse == 0.0 {
            return f64::INFINITY;
        }
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }

    /// Every pixel within `tol` of another image, reporting the margin so the
    /// numbers show up with `--nocapture`.
    pub fn assert_close(&self, other: &Image, tol: u8, what: &str) {
        let (max, mean) = self.diff(other);
        println!("{what}: worst channel {max}/{tol}, mean {mean:.4}");
        assert!(
            max <= tol,
            "{what}: worst channel differs by {max} (allowed {tol}, mean {mean:.3})"
        );
    }
}

/// The overlay rectangle the renderer should have drawn, from the geometry
/// every test shares.
pub fn overlay(pip: &PipSpec, w: usize, h: usize, cam_aspect: f64) -> [f64; 4] {
    compose::overlay_rect(pip, w as f64, h as f64, cam_aspect)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_synthetic_frames_are_well_formed() {
        let s = screen(64, 36);
        assert!(s.is_well_formed());
        assert_eq!(s.planes[0].stride, 64 * 4);
        let c = cam(32, 24);
        assert!(c.is_well_formed());
        // The camera's thirds really are red, green and blue.
        let image = Image::bgra(&c);
        let left = image.at(2, 3);
        assert!(left[0] > 150 && left[1] < 60 && left[2] < 60, "{left:?}");
        let middle = image.at(16, 3);
        assert!(middle[1] > 120 && middle[0] < 60, "{middle:?}");
        let right = image.at(30, 3);
        assert!(right[2] > 150 && right[0] < 60, "{right:?}");
    }

    #[test]
    fn nv12_matches_the_cpu_matrix() {
        let f = solid_nv12(8, 6, [255, 0, 0]);
        assert!(f.is_well_formed(), "{f:?}");
        let (y, u, v) = super::super::yuv::rgb_to_yuv709_limited([255, 0, 0]);
        assert_eq!(f.planes[0].data[0], y);
        assert_eq!((f.planes[1].data[0], f.planes[1].data[1]), (u, v));
    }
}
