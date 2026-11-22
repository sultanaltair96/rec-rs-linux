//! Cursor and click highlighting on CPU frames.
//!
//! Upstream asks ScreenCaptureKit for `showsCursor` and `showMouseClicks` and
//! macOS draws both. On Linux the two halves come from different places:
//!
//! * **The cursor** is usually the compositor's job: the portal's `Embedded`
//!   cursor mode paints it into the stream with the desktop's own theme. When a
//!   desktop offers only `Metadata` (the pointer's position and bitmap sent
//!   beside the pixels) the bitmap is composited here, and on X11 the cursor is
//!   never part of a window's pixels at all, so XFixes' cursor image is drawn
//!   on top — up to 60 times a second, cheaply, by `CursorImage::draw`.
//! * **Clicks** are visible only where button events are: X11, through
//!   `QueryPointer`'s button bits. No portal reports mouse buttons, so a
//!   Wayland recording gets the cursor but no rings (and says so).
//!
//! Everything works on packed BGRA/RGBA rows so it can be unit tested without a
//! compositor: the ring maths is where an off-by-one shows up as a lopsided
//! circle, and the blend is where a premultiplied cursor shows up as a dark
//! halo.
//!
//! Adapted from rec-rs src/screen.rs (MIT OR Apache-2.0), which gets the same
//! behaviour from `SCStreamConfiguration.setShowMouseClicks`.

use crate::media_types::PixelFormat;

/// How long a click stays visible. macOS's highlight is a quick pulse; a third
/// of a second reads at 60 fps without lingering.
pub const CLICK_LIFE: f64 = 0.35;

/// Ring radius at the start and the end of its life, in points at 1080p.
const RING_FROM: f64 = 9.0;
const RING_TO: f64 = 26.0;
/// Ring thickness at 1080p, in points.
const RING_WIDTH: f64 = 3.0;
/// Alpha at birth; it fades to zero quadratically.
const RING_ALPHA: f64 = 0.85;

/// One click to highlight: where and when (contract seconds).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Click {
    pub x: f64,
    pub y: f64,
    pub at: f64,
}

impl Click {
    pub fn new(x: f64, y: f64, at: f64) -> Click {
        Click { x, y, at }
    }
}

/// A packed frame to draw into: one plane, its row stride and its format.
///
/// The capture code has these pieces separately (a buffer, a stride, a size and
/// a format), and passing them around as four arguments is how a stride or a
/// format silently stops matching the buffer. One borrow ties them together.
pub struct Frame<'a> {
    pub data: &'a mut [u8],
    pub stride: usize,
    pub width: usize,
    pub height: usize,
    pub format: PixelFormat,
}

/// How a click looks at `age` seconds: `(radius, alpha)` for a frame `width`
/// pixels wide, or `None` once it has expired.
///
/// Exposed for the tests and for the (phase 1E) preview, which draws the same
/// ring over the preview image.
pub fn ring_at(age: f64, width: usize) -> Option<(f64, f64)> {
    if !(0.0..CLICK_LIFE).contains(&age) {
        return None;
    }
    // Everything scales with the frame so a 4K recording gets a 4K-sized ring.
    let k = (width.max(320) as f64 / 1920.0).clamp(0.35, 4.0);
    let p = age / CLICK_LIFE;
    // Ease-out expansion, quadratic fade: the pulse is visible at once and
    // then gets out of the way.
    let grow = 1.0 - (1.0 - p) * (1.0 - p);
    let radius = (RING_FROM + (RING_TO - RING_FROM) * grow) * k;
    let alpha = RING_ALPHA * (1.0 - p) * (1.0 - p);
    Some((radius, alpha))
}

/// Draw the rings of every click that is still alive at `now`.
///
/// Clicks outside the frame are ignored, so a ring never bleeds across an edge.
pub fn draw_clicks(frame: &mut Frame<'_>, clicks: &[Click], now: f64) {
    let width = frame.width;
    for click in clicks {
        let Some((radius, alpha)) = ring_at(now - click.at, width) else {
            continue;
        };
        if alpha <= 0.004 {
            continue;
        }
        let k = (width.max(320) as f64 / 1920.0).clamp(0.35, 4.0);
        // A dark outer ring under a white inner one, so the highlight is
        // visible on white and on black content alike (and on video, where a
        // plain white ring disappears).
        draw_ring(
            frame,
            click.x,
            click.y,
            radius + RING_WIDTH * 0.55 * k,
            RING_WIDTH * k + 1.0,
            alpha * 0.45,
            [0, 0, 0],
        );
        draw_ring(
            frame,
            click.x,
            click.y,
            radius,
            RING_WIDTH * k,
            alpha,
            [255, 255, 255],
        );
    }
}

/// Draw one anti-aliased circle outline.
///
/// `radius` is the ring's centre line, `thickness` its width. Coverage falls
/// off over one pixel at both edges, which is what makes a 3 px ring look round
/// rather than blocky.
pub fn draw_ring(
    frame: &mut Frame<'_>,
    cx: f64,
    cy: f64,
    radius: f64,
    thickness: f64,
    alpha: f64,
    rgb: [u8; 3],
) {
    let Some(bpp) = frame.format.bytes_per_pixel() else {
        return;
    };
    let (width, height, stride) = (frame.width, frame.height, frame.stride);
    let format = frame.format;
    if radius <= 0.0 || thickness <= 0.0 || alpha <= 0.0 {
        return;
    }
    let half = thickness * 0.5;
    let reach = radius + half + 1.0;
    let x0 = (cx - reach).floor().max(0.0) as usize;
    let y0 = (cy - reach).floor().max(0.0) as usize;
    let x1 = ((cx + reach).ceil() as usize).min(width.saturating_sub(1));
    let y1 = ((cy + reach).ceil() as usize).min(height.saturating_sub(1));
    if x0 > x1 || y0 > y1 {
        return;
    }
    let channels = match format {
        // BGRA in memory is B, G, R, A; the caller passes RGB.
        PixelFormat::Bgra => [rgb[2], rgb[1], rgb[0], 0],
        PixelFormat::Rgba => [rgb[0], rgb[1], rgb[2], 0],
        _ => return,
    };
    let data = &mut *frame.data;
    for y in y0..=y1 {
        let dy = y as f64 + 0.5 - cy;
        for x in x0..=x1 {
            let dx = x as f64 + 0.5 - cx;
            let d = (dx * dx + dy * dy).sqrt();
            let edge = (d - radius).abs() - half;
            let coverage = (1.0 - edge.max(0.0)).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }
            let a = (alpha * coverage).clamp(0.0, 1.0);
            let at = y * stride + x * bpp;
            if at + bpp > data.len() {
                continue;
            }
            for c in 0..3 {
                let src = channels[c] as f64;
                let dst = data[at + c] as f64;
                data[at + c] = (src * a + dst * (1.0 - a)).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
}

/// A cursor bitmap from PipeWire's `spa_meta_cursor`, or (on X11) from XFixes.
///
/// `pixels` is BGRA with premultiplied alpha, `stride` bytes per row — what both
/// producers hand over.
#[derive(Clone, Debug)]
pub struct CursorImage {
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub pixels: Vec<u8>,
    /// Where the bitmap's top-left goes relative to the pointer position.
    pub hotspot_x: i32,
    pub hotspot_y: i32,
}

impl CursorImage {
    /// Composite the cursor at pointer position (`x`, `y`), clipped to the
    /// frame. Nothing is drawn when the pointer is outside the frame.
    pub fn draw(&self, frame: &mut Frame<'_>, x: f64, y: f64) {
        let Some(bpp) = frame.format.bytes_per_pixel() else {
            return;
        };
        let (width, height, stride) = (frame.width, frame.height, frame.stride);
        let format = frame.format;
        let data = &mut *frame.data;
        let left = x.round() as i64 - self.hotspot_x as i64;
        let top = y.round() as i64 - self.hotspot_y as i64;
        // The whole bitmap outside the frame: nothing to do.
        if left + self.width as i64 <= 0
            || top + self.height as i64 <= 0
            || left >= width as i64
            || top >= height as i64
        {
            return;
        }
        let src_is_bgra = matches!(format, PixelFormat::Bgra);
        for row in 0..self.height {
            let fy = top + row as i64;
            if fy < 0 || fy >= height as i64 {
                continue;
            }
            for col in 0..self.width {
                let fx = left + col as i64;
                if fx < 0 || fx >= width as i64 {
                    continue;
                }
                let s = row * self.stride + col * 4;
                if s + 4 > self.pixels.len() {
                    continue;
                }
                let d = fy as usize * stride + fx as usize * bpp;
                if d + 4 > data.len() {
                    continue;
                }
                // Premultiplied BGRA source over the destination.
                let a = self.pixels[s + 3] as u32;
                let inv = 255 - a;
                let mix = |src: u8, dst: u8| -> u8 {
                    ((src as u32 * 255 + dst as u32 * inv + 127) / 255).min(255) as u8
                };
                if src_is_bgra {
                    data[d] = mix(self.pixels[s], data[d]);
                    data[d + 1] = mix(self.pixels[s + 1], data[d + 1]);
                    data[d + 2] = mix(self.pixels[s + 2], data[d + 2]);
                } else {
                    data[d] = mix(self.pixels[s + 2], data[d]);
                    data[d + 1] = mix(self.pixels[s + 1], data[d + 1]);
                    data[d + 2] = mix(self.pixels[s], data[d + 2]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: usize, height: usize, fill: u8) -> Vec<u8> {
        vec![fill; width * height * 4]
    }

    fn pixel(data: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
        let at = (y * width + x) * 4;
        [data[at], data[at + 1], data[at + 2], data[at + 3]]
    }

    #[test]
    fn a_ring_is_a_ring_not_a_disc() {
        // A 1920-wide frame is the scale the ring sizes are given at, so
        // k = 1 and the first ring is 9 px across.
        let w = 1920;
        let h = 96;
        let mut data = frame(w, h, 0);
        let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
        let mut view = Frame {
            data: &mut data,
            stride: 4 * w,
            width: w,
            height: h,
            format: PixelFormat::Bgra,
        };
        draw_clicks(&mut view, &[Click::new(cx, cy, 5.0)], 5.0);
        // The middle is untouched: clicks are highlighted with a circle.
        assert_eq!(pixel(&data, w, w / 2, h / 2), [0, 0, 0, 0]);
        let (radius, _) = ring_at(0.0, w).unwrap();
        let r = radius.round() as usize;
        // The ring is lit in all four directions at its radius.
        for (x, y) in [(w / 2 + r, h / 2), (w / 2 - r, h / 2), (w / 2, h / 2 + r)] {
            let p = pixel(&data, w, x, y);
            assert!(p[0] > 100 && p[1] > 100, "{x},{y}: {p:?}");
        }
        // Well outside the ring nothing changed.
        assert_eq!(pixel(&data, w, 10, 10), [0, 0, 0, 0]);
    }

    #[test]
    fn a_ring_grows_fades_and_expires() {
        let (r0, a0) = ring_at(0.0, 1920).unwrap();
        let (r1, a1) = ring_at(CLICK_LIFE * 0.5, 1920).unwrap();
        let (r2, a2) = ring_at(CLICK_LIFE * 0.99, 1920).unwrap();
        assert!(r0 < r1 && r1 < r2, "{r0} {r1} {r2}");
        assert!(a0 > a1 && a1 > a2, "{a0} {a1} {a2}");
        assert_eq!(ring_at(CLICK_LIFE, 1920), None);
        assert_eq!(ring_at(-0.1, 1920), None);
        // A 4K frame gets a bigger ring, a small one a smaller ring, clamped.
        assert!(ring_at(0.0, 3840).unwrap().0 > r0);
        assert!(ring_at(0.0, 320).unwrap().0 < r0);
        assert_eq!(ring_at(0.0, 80), ring_at(0.0, 320));
    }

    #[test]
    fn clicks_fade_out_and_leave_the_frame_alone_when_stale() {
        let w = 200;
        let mut data = frame(w, w, 0);
        let click = Click::new(w as f64 / 2.0, w as f64 / 2.0, 10.0);
        // Before it happened and long after: no change at all.
        {
            let mut view = Frame {
                data: &mut data,
                stride: 4 * w,
                width: w,
                height: w,
                format: PixelFormat::Bgra,
            };
            draw_clicks(&mut view, &[click], 9.0);
            draw_clicks(&mut view, &[click], 10.0 + CLICK_LIFE + 1.0);
        }
        assert!(data.iter().all(|&b| b == 0));
        // While alive the ring lights the middle band of the frame.
        {
            let mut view = Frame {
                data: &mut data,
                stride: 4 * w,
                width: w,
                height: w,
                format: PixelFormat::Bgra,
            };
            draw_clicks(&mut view, &[click], 10.05);
        }
        assert!(data.iter().any(|&b| b > 40), "nothing drawn");
        // And a click outside the frame is ignored instead of wrapping.
        let mut outside = frame(64, 64, 0);
        {
            let mut view = Frame {
                data: &mut outside,
                stride: 4 * 64,
                width: 64,
                height: 64,
                format: PixelFormat::Bgra,
            };
            draw_clicks(&mut view, &[Click::new(-500.0, -500.0, 1.0)], 1.0);
        }
        assert!(outside.iter().all(|&b| b == 0));
    }

    #[test]
    fn the_cursor_is_composited_at_its_hotspot() {
        // A 2x2 opaque white cursor with a (0,0) hotspot.
        let mut pixels = Vec::new();
        for _ in 0..4 {
            pixels.extend_from_slice(&[255, 255, 255, 255]);
        }
        let cursor = CursorImage {
            width: 2,
            height: 2,
            stride: 8,
            pixels,
            hotspot_x: 0,
            hotspot_y: 0,
        };
        let mut data = frame(4, 4, 0);
        cursor.draw(
            &mut Frame {
                data: &mut data,
                stride: 16,
                width: 4,
                height: 4,
                format: PixelFormat::Bgra,
            },
            1.0,
            2.0,
        );
        assert_eq!(pixel(&data, 4, 1, 2), [255, 255, 255, 0]);
        assert_eq!(pixel(&data, 4, 2, 3), [255, 255, 255, 0]);
        assert_eq!(pixel(&data, 4, 0, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn a_half_transparent_cursor_blends_and_clips() {
        // Premultiplied 50% white over black: half grey.
        let cursor = CursorImage {
            width: 1,
            height: 1,
            stride: 4,
            pixels: vec![128, 128, 128, 128],
            hotspot_x: 0,
            hotspot_y: 0,
        };
        let mut data = frame(2, 2, 0);
        cursor.draw(
            &mut Frame {
                data: &mut data,
                stride: 8,
                width: 2,
                height: 2,
                format: PixelFormat::Bgra,
            },
            0.0,
            0.0,
        );
        let p = pixel(&data, 2, 0, 0);
        assert!((126..=130).contains(&p[0]), "{p:?}");
        // The hotspot decides where the bitmap's corner goes: a cursor whose
        // hotspot is its bottom-right corner is drawn up and to the left, so a
        // pointer at 0,0 puts only that corner pixel on screen.
        let mut data = frame(4, 4, 0);
        let hot = CursorImage {
            width: 2,
            height: 2,
            stride: 8,
            pixels: vec![128; 16],
            hotspot_x: 2,
            hotspot_y: 2,
        };
        hot.draw(
            &mut Frame {
                data: &mut data,
                stride: 16,
                width: 4,
                height: 4,
                format: PixelFormat::Bgra,
            },
            1.0,
            1.0,
        );
        // The bitmap's bottom-right pixel lands at 0,0; every other pixel of it
        // is off screen, so the rest of the frame is untouched.
        let p = pixel(&data, 4, 0, 0);
        assert!((126..=130).contains(&p[0]), "{p:?}");
        assert_eq!(pixel(&data, 4, 1, 0), [0, 0, 0, 0]);
        assert_eq!(pixel(&data, 4, 0, 1), [0, 0, 0, 0]);
    }

    #[test]
    fn rgba_frames_get_rgba_rings() {
        let w = 120;
        let mut data = frame(w, w, 0);
        draw_ring(
            &mut Frame {
                data: &mut data,
                stride: 4 * w,
                width: w,
                height: w,
                format: PixelFormat::Rgba,
            },
            w as f64 / 2.0,
            w as f64 / 2.0,
            20.0,
            4.0,
            1.0,
            [10, 20, 30],
        );
        // Red is the first byte of an RGBA pixel.
        let p = pixel(&data, w, w / 2 + 20, w / 2);
        assert_eq!(p[0], 10);
        assert_eq!(p[1], 20);
        assert_eq!(p[2], 30);
        // Planar formats are left alone rather than misinterpreted.
        let mut i420 = vec![0u8; w * w * 3];
        draw_ring(
            &mut Frame {
                data: &mut i420,
                stride: w,
                width: w,
                height: w,
                format: PixelFormat::I420,
            },
            60.0,
            60.0,
            20.0,
            4.0,
            1.0,
            [255, 255, 255],
        );
        assert!(i420.iter().all(|&b| b == 0));
    }
}
