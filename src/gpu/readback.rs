//! Staging buffers: the plane layout `copy_texture_to_buffer` needs, and the
//! CPU frames built from it.
//!
//! `copy_texture_to_buffer` requires every row pitch to be a multiple of
//! [`COPY_ALIGN`] (256 bytes), so the staging buffer is padded and each plane
//! is copied out row by row into a tightly packed [`VideoFrame`] plane. 1920
//! pixels of BGRA is already a multiple of 256; 1080p NV12's Y plane is not
//! (1920 is, but the 960-byte chroma rows are not), so the compaction is not
//! optional.

use crate::media_types::{PixelFormat, Plane, VideoFrame};

/// `wgpu::COPY_BYTES_PER_ROW_ALIGNMENT`: every copied row starts on this
/// boundary.
pub const COPY_ALIGN: usize = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize;

/// One plane in the staging buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaneLayout {
    /// Pixels per row.
    pub width: usize,
    /// Rows copied.
    pub height: usize,
    /// Padded bytes per row (a multiple of [`COPY_ALIGN`]).
    pub pitch: usize,
    /// Bytes per row once compacted into the frame.
    pub row_bytes: usize,
    /// Where the plane starts in the staging buffer.
    pub offset: usize,
}

/// Where each plane of an output frame goes in the staging buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub width: usize,
    pub height: usize,
    pub format: PixelFormat,
    pub planes: Vec<PlaneLayout>,
    /// Bytes the staging buffer needs.
    pub len: usize,
}

/// `n` rounded up to the next multiple of `align`.
fn align(n: usize, align: usize) -> usize {
    n.div_ceil(align) * align
}

impl Layout {
    pub fn new(width: usize, height: usize, format: PixelFormat) -> Layout {
        let (w, h) = (width, height);
        let mut planes = Vec::new();
        let mut offset = 0;
        // `texels` is the texture's width in texels, `block` the bytes of one
        // texel: one for a luma plane, four for BGRA, two for interleaved
        // chroma.
        let mut push = |texels: usize, height: usize, block: usize| {
            let row_bytes = texels * block;
            let pitch = align(row_bytes, COPY_ALIGN);
            planes.push(PlaneLayout {
                width: texels,
                height,
                pitch,
                row_bytes,
                offset,
            });
            offset += pitch * height;
        };
        match format {
            PixelFormat::Bgra | PixelFormat::Rgba => push(w, h, 4),
            PixelFormat::Nv12 => {
                push(w, h, 1);
                push(w.div_ceil(2), h.div_ceil(2), 2);
            }
            PixelFormat::I420 => {
                push(w, h, 1);
                push(w.div_ceil(2), h.div_ceil(2), 1);
                push(w.div_ceil(2), h.div_ceil(2), 1);
            }
        }
        Layout {
            width,
            height,
            format,
            planes,
            len: offset,
        }
    }

    /// Copy the staging buffer's rows into a frame with tight planes.
    pub fn frame_from(&self, data: &[u8], pts: f64) -> VideoFrame {
        let mut planes = Vec::with_capacity(self.planes.len());
        for p in &self.planes {
            let mut bytes = vec![0u8; p.row_bytes * p.height];
            for row in 0..p.height {
                let from = p.offset + row * p.pitch;
                let to = row * p.row_bytes;
                if let Some(src) = data.get(from..from + p.row_bytes) {
                    bytes[to..to + p.row_bytes].copy_from_slice(src);
                }
            }
            planes.push(Plane::new(bytes, p.row_bytes));
        }
        VideoFrame {
            pts,
            width: self.width,
            height: self.height,
            format: self.format,
            planes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pitches_are_256_aligned_and_offsets_follow() {
        // 1920x1080 BGRA: 7680 bytes a row, already aligned.
        let l = Layout::new(1920, 1080, PixelFormat::Bgra);
        assert_eq!(l.planes[0].pitch, 7680);
        assert_eq!(l.len, 7680 * 1080);
        // 1080p NV12: 1920 bytes a row is not a multiple of 256 (1920 / 256 =
        // 7.5), so the Y rows are padded to 2048 while the chroma rows (960
        // pairs, 1920 bytes) are padded to 2048 too.
        let l = Layout::new(1920, 1080, PixelFormat::Nv12);
        assert_eq!(l.planes[0].pitch, 2048);
        assert_eq!(l.planes[0].row_bytes, 1920);
        assert_eq!(l.planes[1].pitch, 2048);
        assert_eq!(l.planes[1].width, 960);
        assert_eq!(l.planes[1].row_bytes, 1920);
        assert_eq!(l.planes[1].offset, 2048 * 1080);
        assert_eq!(l.len, 2048 * 1080 + 2048 * 540);
        // A size no plane of which is aligned: every pitch is padded up.
        let l = Layout::new(1918, 1080, PixelFormat::I420);
        assert_eq!(l.planes[0].pitch, 2048);
        assert_eq!(l.planes[0].row_bytes, 1918);
        assert_eq!(l.planes[1].pitch, 1024);
        assert_eq!(l.planes[1].row_bytes, 959);
        assert_eq!(l.planes[1].offset, 2048 * 1080);
        assert_eq!(l.planes[2].offset, 2048 * 1080 + 1024 * 540);
        for p in &l.planes {
            assert_eq!(p.pitch % COPY_ALIGN, 0, "{p:?}");
            assert_eq!(p.offset % COPY_ALIGN, 0, "{p:?}");
        }
    }

    #[test]
    fn rows_are_compacted() {
        let l = Layout::new(4, 2, PixelFormat::Bgra);
        assert_eq!(l.planes[0].pitch, COPY_ALIGN);
        let mut data = vec![0u8; l.len];
        for y in 0..2 {
            for x in 0..16 {
                data[y * COPY_ALIGN + x] = (y * 16 + x) as u8;
            }
        }
        let frame = l.frame_from(&data, 1.5);
        assert_eq!(frame.pts, 1.5);
        assert!(frame.is_well_formed());
        assert_eq!(frame.planes[0].stride, 16);
        assert_eq!(&frame.planes[0].data[..16], &data[..16]);
        assert_eq!(
            &frame.planes[0].data[16..],
            &data[COPY_ALIGN..COPY_ALIGN + 16]
        );
    }

    #[test]
    fn odd_sizes_round_chroma_up() {
        let l = Layout::new(3, 3, PixelFormat::I420);
        assert_eq!(l.planes[1].width, 2);
        assert_eq!(l.planes[1].height, 2);
        let frame = l.frame_from(&vec![7u8; l.len], 0.0);
        assert!(frame.is_well_formed(), "{frame:?}");
        assert_eq!(frame.planes[1].data.len(), 4);
    }
}
