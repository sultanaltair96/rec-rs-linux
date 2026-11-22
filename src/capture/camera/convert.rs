//! V4L2/portal frame conversion and camera timestamps.
//!
//! The contract wants CPU frames in [`PixelFormat::Bgra`] (what AVCapture gives
//! upstream and the compositor consumes), so each camera encoding is converted
//! here:
//!
//! - YUYV: packed 4:2:2, BT.601 or BT.709 limited range according to the
//!   driver's `colorspace`;
//! - NV12: semi-planar 4:2:0, same matrices;
//! - MJPEG: decoded by `zune-jpeg` to RGB and expanded;
//! - BGRx (the camera portal's usual format): a straight swap.
//!
//! Timestamps are V4L2 buffers' `timeval`s. Drivers report them either on
//! `CLOCK_MONOTONIC` (what the recorder uses) or as wall-clock seconds; the
//! clock below detects a wall-clock value by its magnitude, refuses timestamps
//! from another clock (more than [`CLOCK_SLACK`] away from now), and enforces
//! monotonic output, so the writer never sees a frame from the future or the
//! past.
//!
//! Adapted from rec-rs src/camera.rs (MIT OR Apache-2.0), where AVFoundation
//! did the conversion and the host clock came with the sample buffer.

use anyhow::{Result, bail};
use v4l::format::Colorspace;

use crate::media_types::{PixelFormat, now};

/// How far a driver timestamp may be from our clock before it is treated as
/// another clock entirely (boottime across a suspend, a broken driver).
pub const CLOCK_SLACK: f64 = 5.0;

/// Which YUV matrix a device's pixels use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum YuvMatrix {
    Bt601,
    Bt709,
}

impl YuvMatrix {
    /// The driver reports the colorspace; HD webcams usually say Rec.709 and
    /// older/SD ones SMPTE 170M (BT.601). Unknown/JPEG formats are full range
    /// RGB and never reach the YUV path.
    pub fn for_colorspace(cs: Colorspace) -> YuvMatrix {
        match cs {
            Colorspace::Rec709 | Colorspace::Rec2020 | Colorspace::DCIP3 => YuvMatrix::Bt709,
            _ => YuvMatrix::Bt601,
        }
    }
}

/// One frame's BGRA pixels with their size.
pub struct Converted {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

/// A converter bound to one negotiated format, reusing its output buffer.
pub struct Converter {
    format: PixelFormat,
    encoding: Option<Encoding>,
    width: usize,
    height: usize,
    /// Source row length in bytes (V4L2 `bytesperline`).
    stride: usize,
    matrix: YuvMatrix,
}

use super::format::Encoding;

impl Converter {
    /// `stride` is the driver's `bytesperline` (0 means tightly packed).
    pub fn new(
        fourcc: v4l::FourCC,
        width: usize,
        height: usize,
        stride: usize,
        colorspace: Colorspace,
    ) -> Result<Converter> {
        let Some(encoding) = Encoding::from_fourcc(fourcc) else {
            bail!(
                "{} is not a camera format this build decodes (YUYV, NV12 or MJPEG)",
                fourcc
            );
        };
        let stride = if stride == 0 {
            match encoding {
                Encoding::Yuyv => width * 2,
                Encoding::Nv12 => width,
                Encoding::Mjpeg => 0,
            }
        } else {
            stride
        };
        Ok(Converter {
            format: encoding.pixel_format(),
            encoding: Some(encoding),
            width,
            height,
            stride,
            matrix: YuvMatrix::for_colorspace(colorspace),
        })
    }

    pub fn pixel_format(&self) -> PixelFormat {
        self.format
    }

    /// The format's label for `describe()`, e.g. "MJPEG".
    pub fn label(&self) -> &'static str {
        self.encoding.map_or("BGRA", Encoding::label)
    }

    /// Convert one driver buffer. MJPEG carries its own size; the returned size
    /// is what the frame really is.
    pub fn convert(&mut self, src: &[u8]) -> Result<Converted> {
        match self.encoding.expect("Converter::new checked the format") {
            Encoding::Yuyv => {
                let data = yuyv_to_bgra(src, self.width, self.height, self.stride, self.matrix)?;
                Ok(Converted {
                    width: self.width,
                    height: self.height,
                    data,
                })
            }
            Encoding::Nv12 => {
                let data = nv12_to_bgra(src, self.width, self.height, self.stride, self.matrix)?;
                Ok(Converted {
                    width: self.width,
                    height: self.height,
                    data,
                })
            }
            Encoding::Mjpeg => {
                let (width, height, data) = mjpeg_to_bgra(src)?;
                Ok(Converted {
                    width,
                    height,
                    data,
                })
            }
        }
    }
}

fn yuv_matrix(m: YuvMatrix) -> (f32, f32, f32, f32) {
    // (r from V, g from U, g from V, b from U), after 1.164 * (Y-16).
    match m {
        YuvMatrix::Bt601 => (1.596, 0.392, 0.813, 2.017),
        YuvMatrix::Bt709 => (1.793, 0.213, 0.533, 2.112),
    }
}

#[inline]
fn clamp_u8(v: f32) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}

/// One pixel's BGRA from a limited-range YUV triple, written in place.
#[inline]
fn yuv_pixel(y: u8, u: u8, v: u8, m: YuvMatrix, bgra: &mut [u8]) {
    let (rv, gu, gv, bu) = yuv_matrix(m);
    let y = 1.164 * (f32::from(y) - 16.0);
    let u = f32::from(u) - 128.0;
    let v = f32::from(v) - 128.0;
    bgra[0] = clamp_u8(y + bu * u);
    bgra[1] = clamp_u8(y - gu * u - gv * v);
    bgra[2] = clamp_u8(y + rv * v);
    bgra[3] = 255;
}

/// YUYV (packed 4:2:2: Y0 U Y1 V) to BGRA.
pub fn yuyv_to_bgra(
    src: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    matrix: YuvMatrix,
) -> Result<Vec<u8>> {
    let row = stride.max(width * 2);
    if width == 0 || height == 0 || src.len() < row * height {
        bail!(
            "YUYV frame too small: {} bytes for {width}x{height} at stride {row}",
            src.len()
        );
    }
    let mut out = vec![0u8; width * height * 4];
    for y in 0..height {
        let line = &src[y * row..y * row + width * 2];
        let dst = &mut out[y * width * 4..(y + 1) * width * 4];
        for x in (0..width).step_by(2) {
            let (y0, u, y1, v) = (
                line[x * 2],
                line[x * 2 + 1],
                line[x * 2 + 2],
                line[x * 2 + 3],
            );
            yuv_pixel(y0, u, v, matrix, &mut dst[x * 4..x * 4 + 4]);
            if x + 1 < width {
                yuv_pixel(y1, u, v, matrix, &mut dst[(x + 1) * 4..(x + 1) * 4 + 4]);
            }
        }
    }
    Ok(out)
}

/// NV12 (a Y plane, then interleaved UV at half height) to BGRA.
pub fn nv12_to_bgra(
    src: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    matrix: YuvMatrix,
) -> Result<Vec<u8>> {
    let row = stride.max(width);
    let uv_offset = row * height;
    let uv_row = row;
    let uv_height = height.div_ceil(2);
    if width == 0 || height == 0 || src.len() < uv_offset + uv_row * uv_height {
        bail!(
            "NV12 frame too small: {} bytes for {width}x{height} at stride {row}",
            src.len()
        );
    }
    let mut out = vec![0u8; width * height * 4];
    for y in 0..height {
        let line = &src[y * row..y * row + width];
        let uv = &src[uv_offset + (y / 2) * uv_row..uv_offset + (y / 2) * uv_row + width];
        let dst = &mut out[y * width * 4..(y + 1) * width * 4];
        for x in 0..width {
            let u = uv[(x / 2) * 2];
            let v = uv[(x / 2) * 2 + 1];
            yuv_pixel(line[x], u, v, matrix, &mut dst[x * 4..x * 4 + 4]);
        }
    }
    Ok(out)
}

/// MJPEG: one JPEG per frame, decoded to RGB (or gray) and expanded to BGRA.
pub fn mjpeg_to_bgra(src: &[u8]) -> Result<(usize, usize, Vec<u8>)> {
    let mut decoder = zune_jpeg::JpegDecoder::new(src);
    decoder
        .decode_headers()
        .map_err(|e| anyhow::anyhow!("MJPEG: bad JPEG header: {e:?}"))?;
    let info = decoder
        .info()
        .ok_or_else(|| anyhow::anyhow!("MJPEG: JPEG without a frame header"))?;
    let (width, height, components) = (info.width as usize, info.height as usize, info.components);
    if width == 0 || height == 0 {
        bail!("MJPEG: empty frame {width}x{height}");
    }
    let pixels = decoder
        .decode()
        .map_err(|e| anyhow::anyhow!("MJPEG: JPEG decode failed: {e:?}"))?;
    let mut out = vec![0u8; width * height * 4];
    match components {
        1 => {
            if pixels.len() < width * height {
                bail!("MJPEG: short gray frame");
            }
            for (p, g) in pixels.iter().zip(out.chunks_exact_mut(4)) {
                g[0] = *p;
                g[1] = *p;
                g[2] = *p;
                g[3] = 255;
            }
        }
        3 => {
            if pixels.len() < width * height * 3 {
                bail!("MJPEG: short RGB frame");
            }
            for (p, bgra) in pixels.chunks_exact(3).zip(out.chunks_exact_mut(4)) {
                bgra[0] = p[2];
                bgra[1] = p[1];
                bgra[2] = p[0];
                bgra[3] = 255;
            }
        }
        other => bail!("MJPEG: {other}-component JPEG is not a camera frame"),
    }
    Ok((width, height, out))
}

/// BGRx (B, G, R, unused) to BGRA: the portal's usual video/raw format.
pub fn bgrx_to_bgra(src: &[u8], width: usize, height: usize, stride: usize) -> Result<Vec<u8>> {
    let row = stride.max(width * 4);
    if width == 0 || height == 0 || src.len() < row * height {
        bail!(
            "BGRx frame too small: {} bytes for {width}x{height} at stride {row}",
            src.len()
        );
    }
    let mut out = vec![0u8; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let s = &src[y * row + x * 4..y * row + x * 4 + 4];
            let d = &mut out[(y * width + x) * 4..(y * width + x) * 4 + 4];
            d[0] = s[0];
            d[1] = s[1];
            d[2] = s[2];
            d[3] = 255;
        }
    }
    Ok(out)
}

/// One row of packed pixels to BGRA: used by the camera portal's `BGRA`
/// (already in order) and `RGBA` (red and blue swapped) formats.
pub fn packed_to_bgra(
    src: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    rgba: bool,
) -> Result<Vec<u8>> {
    let row = stride.max(width * 4);
    if width == 0 || height == 0 || src.len() < row * height {
        bail!(
            "packed frame too small: {} bytes for {width}x{height} at stride {row}",
            src.len()
        );
    }
    let mut out = vec![0u8; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let s = &src[y * row + x * 4..y * row + x * 4 + 4];
            let d = &mut out[(y * width + x) * 4..(y * width + x) * 4 + 4];
            if rgba {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
            } else {
                d[0] = s[0];
                d[1] = s[1];
                d[2] = s[2];
            }
            d[3] = 255;
        }
    }
    Ok(out)
}

/// NV12 with separate Y and UV buffers (the portal's planar layout) to BGRA.
#[allow(clippy::too_many_arguments)]
pub fn nv12_planes_to_bgra(
    y: &[u8],
    y_stride: usize,
    uv: &[u8],
    uv_stride: usize,
    width: usize,
    height: usize,
    matrix: YuvMatrix,
) -> Result<Vec<u8>> {
    let cw = width.div_ceil(2);
    let ch = height.div_ceil(2);
    let ys = y_stride.max(width);
    let us = uv_stride.max(width);
    if width == 0 || height == 0 || y.len() < ys * height || uv.len() < us * ch {
        bail!("NV12 frame too small for {width}x{height}");
    }
    let mut out = vec![0u8; width * height * 4];
    for row in 0..height {
        let line = &y[row * ys..row * ys + width];
        let uvl = &uv[(row / 2) * us..(row / 2) * us + cw * 2];
        let dst = &mut out[row * width * 4..(row + 1) * width * 4];
        for x in 0..width {
            yuv_pixel(
                line[x],
                uvl[(x / 2) * 2],
                uvl[(x / 2) * 2 + 1],
                matrix,
                &mut dst[x * 4..x * 4 + 4],
            );
        }
    }
    Ok(out)
}

/// I420 (three planes: Y, then U and V at half size in both directions) to
/// BGRA. The portal may negotiate this instead of NV12.
#[allow(clippy::too_many_arguments)]
pub fn i420_to_bgra(
    y: &[u8],
    y_stride: usize,
    u: &[u8],
    u_stride: usize,
    v: &[u8],
    v_stride: usize,
    width: usize,
    height: usize,
    matrix: YuvMatrix,
) -> Result<Vec<u8>> {
    let cw = width.div_ceil(2);
    let ch = height.div_ceil(2);
    let ys = y_stride.max(width);
    let us = u_stride.max(cw);
    let vs = v_stride.max(cw);
    if width == 0 || height == 0 || y.len() < ys * height || u.len() < us * ch || v.len() < vs * ch
    {
        bail!("I420 frame too small for {width}x{height}");
    }
    let mut out = vec![0u8; width * height * 4];
    for row in 0..height {
        let line = &y[row * ys..row * ys + width];
        let ul = &u[(row / 2) * us..(row / 2) * us + cw];
        let vl = &v[(row / 2) * vs..(row / 2) * vs + cw];
        let dst = &mut out[row * width * 4..(row + 1) * width * 4];
        for x in 0..width {
            yuv_pixel(
                line[x],
                ul[x / 2],
                vl[x / 2],
                matrix,
                &mut dst[x * 4..x * 4 + 4],
            );
        }
    }
    Ok(out)
}

/// A camera timestamp fixer: V4L2 `timeval`s in, host-clock seconds out.
#[derive(Debug, Default)]
pub struct CameraClock {
    last: Option<f64>,
}

impl CameraClock {
    /// `sec`/`usec` as the driver reported them.
    pub fn stamp(&mut self, sec: i64, usec: i64) -> f64 {
        self.stamp_at(sec as f64 + usec as f64 / 1e6, now(), wall_now())
    }

    /// The same with the clocks passed in, for tests.
    pub fn stamp_at(&mut self, ts: f64, now_mono: f64, now_wall: f64) -> f64 {
        let candidate = if !ts.is_finite() || ts <= 0.0 {
            now_mono
        } else if ts >= 1.0e9 {
            // Epoch seconds: a driver on CLOCK_REALTIME. Its age is how long
            // ago the frame was captured.
            let age = (now_wall - ts).max(0.0);
            (now_mono - age).max(0.0)
        } else {
            ts
        };
        // A timestamp far from our clock is another clock (boottime across a
        // suspend, or a driver bug): the arrival time is closer to the truth
        // than a frame minutes out of range.
        let candidate = if (candidate - now_mono).abs() > CLOCK_SLACK {
            now_mono
        } else {
            candidate
        };
        let out = match self.last {
            Some(prev) if candidate <= prev => now_mono.max(prev + 1e-4),
            _ => candidate,
        };
        self.last = Some(out);
        out
    }
}

fn wall_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// YUYV bytes for two RGB pixels sharing one chroma (limited range).
    fn yuyv_pair(a: (u8, u8, u8), b: (u8, u8, u8)) -> [u8; 4] {
        let yuv = |(r, g, b): (u8, u8, u8)| {
            let (r, g, b) = (f32::from(r), f32::from(g), f32::from(b));
            let y = 16.0 + (0.257 * r + 0.504 * g + 0.098 * b);
            let u = 128.0 + (-0.148 * r - 0.291 * g + 0.439 * b);
            let v = 128.0 + (0.439 * r - 0.368 * g - 0.071 * b);
            (
                y.clamp(0.0, 255.0) as u8,
                u.clamp(0.0, 255.0) as u8,
                v.clamp(0.0, 255.0) as u8,
            )
        };
        let (y0, u0, v0) = yuv(a);
        let (y1, u1, v1) = yuv(b);
        // 4:2:2 averages the chroma of the pair.
        [
            y0,
            ((u16::from(u0) + u16::from(u1)) / 2) as u8,
            y1,
            ((u16::from(v0) + u16::from(v1)) / 2) as u8,
        ]
    }

    fn bgra_at(frame: &[u8], width: usize, x: usize, y: usize) -> (u8, u8, u8, u8) {
        let i = (y * width + x) * 4;
        (frame[i], frame[i + 1], frame[i + 2], frame[i + 3])
    }

    #[test]
    fn yuyv_converts_white_black_and_red() {
        // Two pixels: white and black, then a red pair.
        let white = (235u8, 128u8, 128u8);
        let black = (16u8, 128u8, 128u8);
        let src = [
            white.0, white.1, white.0, white.2, // white, white
            black.0, black.1, black.0, black.2, // black, black
        ];
        let out = yuyv_to_bgra(&src, 2, 2, 4, YuvMatrix::Bt601).unwrap();
        let w = bgra_at(&out, 2, 0, 0);
        assert!(
            w.0 >= 250 && w.1 >= 250 && w.2 >= 250 && w.3 == 255,
            "{w:?}"
        );
        let b = bgra_at(&out, 2, 0, 1);
        assert!(b.0 <= 2 && b.1 <= 2 && b.2 <= 2, "{b:?}");

        // A pair of the same colour round-trips within 8-bit YUV precision.
        // (A red pixel next to a green one shares their averaged chroma, so
        // only same-colour pairs can be checked channel by channel.)
        let red = yuyv_pair((220, 30, 30), (220, 30, 30));
        let out = yuyv_to_bgra(&red, 2, 1, 4, YuvMatrix::Bt601).unwrap();
        let p0 = bgra_at(&out, 2, 0, 0);
        let p1 = bgra_at(&out, 2, 1, 0);
        for p in [p0, p1] {
            assert!((i32::from(p.2) - 220).abs() <= 8, "{p:?}"); // R
            assert!((i32::from(p.1) - 30).abs() <= 8, "{p:?}"); // G
            assert!((i32::from(p.0) - 30).abs() <= 8, "{p:?}"); // B
        }
        let green = yuyv_pair((30, 200, 60), (30, 200, 60));
        let out = yuyv_to_bgra(&green, 2, 1, 4, YuvMatrix::Bt601).unwrap();
        let p = bgra_at(&out, 2, 0, 0);
        assert!((i32::from(p.1) - 200).abs() <= 8, "{p:?}");
    }

    #[test]
    fn yuyv_honours_the_stride_and_rejects_short_buffers() {
        // 2x2 with 8 bytes of padding per row: the padding must be skipped.
        let mut src = vec![0u8; 16];
        for y in 0..2 {
            src[y * 8] = 235;
            src[y * 8 + 1] = 128;
            src[y * 8 + 2] = 235;
            src[y * 8 + 3] = 128;
        }
        let out = yuyv_to_bgra(&src, 2, 2, 8, YuvMatrix::Bt601).unwrap();
        assert!(bgra_at(&out, 2, 1, 1).0 > 240);
        assert!(yuyv_to_bgra(&src[..7], 2, 2, 8, YuvMatrix::Bt601).is_err());
    }

    #[test]
    fn nv12_converts_with_shared_chroma() {
        // 2x2: Y = white/black, chroma neutral.
        let mut src = Vec::new();
        src.extend_from_slice(&[235, 16, 235, 16]); // Y plane
        src.extend_from_slice(&[128, 128]); // UV
        let out = nv12_to_bgra(&src, 2, 2, 2, YuvMatrix::Bt601).unwrap();
        assert!(bgra_at(&out, 2, 0, 0).0 > 240);
        assert!(bgra_at(&out, 2, 1, 1).0 < 6);
        // Chroma is shared by the 2x2 block: a strong U shifts both columns.
        let mut src = Vec::new();
        src.extend_from_slice(&[128, 128, 128, 128]);
        src.extend_from_slice(&[200, 50]);
        let out = nv12_to_bgra(&src, 2, 2, 2, YuvMatrix::Bt601).unwrap();
        let a = bgra_at(&out, 2, 0, 0);
        let b = bgra_at(&out, 2, 1, 1);
        assert_eq!(a, b);
        assert!(a.0 > a.2, "blue-ish: {a:?}");
    }

    #[test]
    fn mjpeg_decodes_the_fixture() {
        let jpeg = include_bytes!("testdata/red16.jpg");
        let (w, h, bgra) = mjpeg_to_bgra(jpeg).unwrap();
        assert_eq!((w, h), (16, 16));
        assert_eq!(bgra.len(), 16 * 16 * 4);
        let (b, g, r, a) = bgra_at(&bgra, w, 8, 8);
        assert!(r > 180, "red frame: r={r}");
        assert!(g < 90 && b < 90, "red frame: g={g} b={b}");
        assert_eq!(a, 255);
        // A truncated JPEG is an error, not a panic.
        assert!(mjpeg_to_bgra(&jpeg[..40]).is_err());
    }

    #[test]
    fn bgrx_keeps_the_channels_and_sets_alpha() {
        let src = [1u8, 2, 3, 0, 4, 5, 6, 99];
        let out = bgrx_to_bgra(&src, 2, 1, 8).unwrap();
        assert_eq!(&out, &[1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn converter_reports_its_format() {
        let mut c = Converter::new(v4l::FourCC::new(b"MJPG"), 16, 16, 0, Colorspace::JPEG).unwrap();
        assert_eq!(c.pixel_format(), PixelFormat::Bgra);
        assert_eq!(c.label(), "MJPEG");
        let frame = c.convert(include_bytes!("testdata/red16.jpg")).unwrap();
        assert_eq!((frame.width, frame.height), (16, 16));
        assert_eq!(frame.data.len(), 16 * 16 * 4);
        assert!(Converter::new(v4l::FourCC::new(b"RGB3"), 2, 2, 0, Colorspace::SRGB).is_err());
    }

    #[test]
    fn matrices_follow_the_driver_colorspace() {
        assert_eq!(
            YuvMatrix::for_colorspace(Colorspace::Rec709),
            YuvMatrix::Bt709
        );
        assert_eq!(
            YuvMatrix::for_colorspace(Colorspace::SRGB),
            YuvMatrix::Bt601
        );
        assert_eq!(
            YuvMatrix::for_colorspace(Colorspace::JPEG),
            YuvMatrix::Bt601
        );
        assert_eq!(
            YuvMatrix::for_colorspace(Colorspace::Default),
            YuvMatrix::Bt601
        );
    }

    #[test]
    fn i420_converts_like_nv12() {
        let y = [235u8, 16, 235, 16];
        let u = [128u8];
        let v = [128u8];
        let out = i420_to_bgra(&y, 2, &u, 1, &v, 1, 2, 2, YuvMatrix::Bt601).unwrap();
        assert!(bgra_at(&out, 2, 0, 0).0 > 240);
        assert!(bgra_at(&out, 2, 1, 1).0 < 6);
        assert!(i420_to_bgra(&y, 2, &u, 1, &v, 1, 4, 4, YuvMatrix::Bt601).is_err());
    }

    #[test]
    fn nv12_separate_planes_match_the_contiguous_form() {
        let y = [235u8, 16, 235, 16];
        let uv = [128u8, 128];
        let a = nv12_to_bgra(&[235, 16, 235, 16, 128, 128], 2, 2, 2, YuvMatrix::Bt601).unwrap();
        let b = nv12_planes_to_bgra(&y, 2, &uv, 2, 2, 2, YuvMatrix::Bt601).unwrap();
        assert_eq!(a, b);
        assert!(nv12_planes_to_bgra(&y, 2, &uv, 2, 4, 4, YuvMatrix::Bt601).is_err());
    }

    #[test]
    fn packed_bgra_and_rgba() {
        let bgra = [1u8, 2, 3, 255, 4, 5, 6, 255];
        assert_eq!(packed_to_bgra(&bgra, 2, 1, 8, false).unwrap(), bgra);
        let rgba = [3u8, 2, 1, 9, 6, 5, 4, 0];
        assert_eq!(
            packed_to_bgra(&rgba, 2, 1, 8, true).unwrap(),
            [1u8, 2, 3, 255, 4, 5, 6, 255]
        );
    }

    #[test]
    fn monotonic_driver_timestamps_pass_through() {
        let mut clock = CameraClock::default();
        let now = 1000.0;
        let wall = 1_700_000_000.0;
        assert_eq!(clock.stamp_at(999.9, now, wall), 999.9);
        assert_eq!(clock.stamp_at(999.933, now, wall), 999.933);
    }

    #[test]
    fn wall_clock_driver_timestamps_are_converted() {
        let mut clock = CameraClock::default();
        let now = 1000.0;
        let wall = 1_700_000_000.0;
        let t = clock.stamp_at(wall - 0.05, now, wall);
        assert!((t - 999.95).abs() < 1e-6, "{t}");
    }

    #[test]
    fn unknown_and_non_monotonic_timestamps_are_repaired() {
        let mut clock = CameraClock::default();
        let now = 1000.0;
        let wall = 1_700_000_000.0;
        // Zero: arrival time.
        assert_eq!(clock.stamp_at(0.0, now, wall), now);
        // Backwards: still increasing.
        let a = clock.stamp_at(0.0, 1001.0, wall);
        let b = clock.stamp_at(0.0, 1001.0, wall);
        assert!(b > a, "{b} !> {a}");
        // From another clock (uptime in a different namespace): arrival time.
        assert_eq!(clock.stamp_at(5.0, 1002.0, wall), 1002.0);
        let mut clock = CameraClock::default();
        assert_eq!(clock.stamp_at(500.0, 1000.0, wall), 1000.0);
    }
}
