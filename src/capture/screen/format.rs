//! Frame geometry: the logical region a user asked for, the pixel rectangle it
//! maps to, the crop of that rectangle out of a captured buffer, and PipeWire's
//! timestamp unit.
//!
//! Adapted from rec-rs src/screen.rs (MIT OR Apache-2.0): upstream hands a
//! `sourceRect` in points to ScreenCaptureKit, which crops *and* scales on the
//! GPU. A portal stream is a whole monitor (or window) at its native size, so
//! the crop happens here and nothing is scaled: the source delivers native
//! pixels, exactly like upstream's capture does, and the compositor (phase 1C)
//! does the one Lanczos scale to the output size. Scaling in the capture path
//! would cost CPU on every frame and duplicate that worker.
//!
//! Everything here is pure: no capture types, no I/O, so the awkward parts —
//! points to pixels, odd sizes, strides, a stream smaller than the request —
//! are unit tested instead of discovered on someone's desktop.

use super::Region;

/// A rectangle in pixels, from the top-left corner of the captured stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

impl Rect {
    pub fn new(x: usize, y: usize, width: usize, height: usize) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    /// The whole of a `width`x`height` frame.
    pub fn full(width: usize, height: usize) -> Rect {
        Rect::new(0, 0, width, height)
    }

    /// This rectangle clipped to a `width`x`height` frame: at least 2x2 (an
    /// encoder needs one chroma pair) and even in every field, so the crop can
    /// be encoded without a second pass. Returns `None` when the frame has no
    /// pixel of the rectangle left.
    pub fn clipped(&self, width: usize, height: usize) -> Option<Rect> {
        if width < 2 || height < 2 {
            return None;
        }
        // Even origin, so both the crop and any later subsampled conversion
        // keep chroma pairs together.
        let x = even_floor(self.x.min(width - 2));
        let y = even_floor(self.y.min(height - 2));
        let mut w = even_floor(self.width.min(width - x));
        let mut h = even_floor(self.height.min(height - y));
        if w < 2 {
            w = 2.min(width - x);
        }
        if h < 2 {
            h = 2.min(height - y);
        }
        if w < 2 || h < 2 {
            return None;
        }
        Some(Rect::new(x, y, w, h))
    }
}

/// The largest even number at most `x`.
pub fn even_floor(x: usize) -> usize {
    x & !1
}

/// `x` (a fractional coordinate) rounded down to an even number.
fn even_floor_f(x: f64) -> usize {
    even_floor(x.round().max(0.0) as usize)
}

/// Map a display region given in logical points onto the pixels of a captured
/// stream.
///
/// `stream` is the size of the buffer in pixels, `points` the size of the same
/// display in logical units. The two differ on a scaled (HiDPI) desktop, where
/// `--region` still means points, like upstream and like the sizes `devices`
/// prints. A display whose logical size is unknown is passed as equal to its
/// pixel size, which is exactly right at scale 1.
///
/// The result is odd-size safe: every field is even and inside the stream.
pub fn region_to_pixels(
    region: &Region,
    stream: (usize, usize),
    points: (usize, usize),
) -> Option<Rect> {
    let (sw, sh) = stream;
    let sx = sw as f64 / points.0.max(1) as f64;
    let sy = sh as f64 / points.1.max(1) as f64;
    let rect = Rect::new(
        even_floor_f(region.x * sx),
        even_floor_f(region.y * sy),
        even_floor_f(region.width * sx),
        even_floor_f(region.height * sy),
    );
    // A region that starts outside the stream captures nothing; clamping it to
    // the corner would silently record the wrong pixels.
    if rect.x >= sw || rect.y >= sh {
        return None;
    }
    if rect.width < 2 || rect.height < 2 {
        return None;
    }
    rect.clipped(sw, sh)
}

/// Seconds on `CLOCK_MONOTONIC` from a PipeWire timestamp in nanoseconds.
///
/// PipeWire's graph clock is `CLOCK_MONOTONIC` — the contract's clock — so this
/// is a unit conversion and nothing else. A zero or negative stamp means "the
/// producer has no timestamp", which the caller replaces with
/// [`media_types::now`](crate::media_types::now).
pub fn pts_secs(ns: i64) -> Option<f64> {
    (ns > 0).then(|| ns as f64 / 1_000_000_000.0)
}

/// Crop `data` (one packed plane, `src_stride` bytes per row) to `rect`,
/// tightly packed. The rectangle must already be inside the frame; the capture
/// path clips it with [`Rect::clipped`] first.
pub fn crop_bytes(data: &[u8], src_stride: usize, rect: Rect) -> Vec<u8> {
    let mut out = Vec::with_capacity(rect.width * rect.height * 4);
    // One pass over the rows we need: a row copy would only touch padding.
    for row in rect.y..rect.y + rect.height {
        let at = row * src_stride + rect.x * 4;
        out.extend_from_slice(&data[at..at + rect.width * 4]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(x: f64, y: f64, width: f64, height: f64) -> Region {
        Region {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn a_region_scales_with_the_display() {
        // A 1280x720-point display captured at 2560x1440: scale 2.
        assert_eq!(
            region_to_pixels(
                &region(100.0, 50.0, 800.0, 600.0),
                (2560, 1440),
                (1280, 720)
            ),
            Some(Rect::new(200, 100, 1600, 1200))
        );
        // Unscaled display: points are pixels.
        assert_eq!(
            region_to_pixels(&region(10.0, 20.0, 100.0, 50.0), (1920, 1080), (1920, 1080)),
            Some(Rect::new(10, 20, 100, 50))
        );
    }

    #[test]
    fn regions_are_even_and_stay_inside() {
        // Odd coordinates, an odd size and a fractional scale: still even, and
        // still inside the stream.
        let r = region_to_pixels(&region(1.0, 1.0, 3.0, 3.0), (1920, 1080), (1280, 720)).unwrap();
        assert_eq!(r, Rect::new(2, 2, 4, 4));
        assert!(r.x + r.width <= 1920 && r.y + r.height <= 1080);
        // Hanging off the right/bottom edge is clipped, not wrapped.
        let r = region_to_pixels(
            &region(1200.0, 700.0, 200.0, 100.0),
            (1280, 720),
            (1280, 720),
        )
        .unwrap();
        assert!(r.x + r.width <= 1280 && r.y + r.height <= 720, "{r:?}");
        // Larger than the display: the whole display.
        assert_eq!(
            region_to_pixels(&region(0.0, 0.0, 4000.0, 3000.0), (1280, 720), (1280, 720)),
            Some(Rect::new(0, 0, 1280, 720))
        );
        // Degenerate: no area left.
        assert_eq!(
            region_to_pixels(
                &region(5000.0, 5000.0, 100.0, 100.0),
                (1280, 720),
                (1280, 720)
            ),
            None
        );
    }

    #[test]
    fn cropping_copies_only_the_rectangle() {
        // 4x2 BGRA frame, distinct per pixel, stride 4 * 4.
        let mut data = vec![0u8; 4 * 2 * 4];
        for (i, b) in data.iter_mut().enumerate() {
            *b = i as u8;
        }
        let cropped = crop_bytes(&data, 16, Rect::new(2, 0, 2, 2));
        // Tightly packed 2x2, starting at pixel (2,0) and then (2,1).
        assert_eq!(
            &cropped[..],
            &[8, 9, 10, 11, 12, 13, 14, 15, 24, 25, 26, 27, 28, 29, 30, 31]
        );
        // A whole-frame crop is a straight copy.
        assert_eq!(crop_bytes(&data, 16, Rect::full(4, 2)), data);
    }

    #[test]
    fn cropping_honours_row_padding() {
        // 2x2 pixels with a 16-byte stride (2 pixels of padding per row).
        let mut data = vec![0u8; 16 * 2];
        for (i, b) in data.iter_mut().enumerate() {
            *b = i as u8;
        }
        // The second row starts at byte 16, not 8.
        assert_eq!(
            crop_bytes(&data, 16, Rect::new(0, 1, 2, 1)),
            vec![16, 17, 18, 19, 20, 21, 22, 23]
        );
    }

    #[test]
    fn pipewire_timestamps_are_monotonic_seconds() {
        assert_eq!(pts_secs(0), None);
        assert_eq!(pts_secs(-1), None);
        assert_eq!(pts_secs(1_500_000_000), Some(1.5));
        // The contract's clock and PipeWire's agree bit for bit.
        let ns = (crate::media_types::now() * 1e9) as i64;
        let back = pts_secs(ns).unwrap();
        assert!((back - crate::media_types::now()).abs() < 0.05, "{back}");
    }

    #[test]
    fn clipping_keeps_chroma_pairs_together() {
        // An odd-sized request inside an odd-sized frame.
        let r = Rect::new(1, 1, 3, 3).clipped(5, 5).unwrap();
        for v in [r.x, r.y, r.width, r.height] {
            assert_eq!(v % 2, 0, "{r:?}");
        }
        assert!(r.width >= 2 && r.height >= 2);
        // A frame too small to hold a chroma pair has no crop.
        assert_eq!(Rect::new(0, 0, 10, 10).clipped(1, 1), None);
    }

    #[test]
    fn a_region_never_leaves_a_frame_smaller_than_a_chroma_pair() {
        assert_eq!(
            Rect::new(0, 0, 1, 1).clipped(1280, 720),
            Some(Rect::new(0, 0, 2, 2))
        );
    }
}
