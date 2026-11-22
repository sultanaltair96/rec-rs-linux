//! The overlay's static layers: the camera mask, the border ring and the drop
//! shadow, baked once per size and style into one texture.
//!
//! This is upstream's `Decor` (src/compose.rs): the costly part of drawing an
//! overlay is the per-pixel mask and the three box passes of the shadow, so
//! they are built when the size or the style changes and reused every frame.
//! Moving the overlay only changes where the texture is placed, so dragging it
//! costs nothing.
//!
//! Adapted from rec-rs src/compose.rs (MIT OR Apache-2.0), with the CoreImage
//! images replaced by one RGBA texture and the data tables below.

use crate::compose::{self, PipSpec, Shape};

/// How opaque the drop shadow is where the shape covers it (upstream: 0.45).
const SHADOW_OPACITY: f32 = 0.45;

/// What the decor depends on: everything about the overlay except where it
/// sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecorKey {
    /// Overlay width and height in output pixels.
    pub width: usize,
    pub height: usize,
    pub shape: Shape,
    /// Rounding in bits: the mask is built from it, so it has to be part of
    /// the key without floating-point comparison.
    pub roundness: u64,
    pub border_px: u64,
    pub border_color: [u8; 3],
    pub shadow: bool,
    /// Padding the shadow adds on every side, and how far it is dropped; both
    /// follow the output height.
    pub pad: usize,
}

impl DecorKey {
    pub fn new(pip: &PipSpec, width: f64, height: f64, out_h: f64) -> DecorKey {
        DecorKey {
            width: width.round().max(1.0) as usize,
            height: height.round().max(1.0) as usize,
            shape: pip.shape,
            roundness: pip.roundness.to_bits(),
            border_px: compose::border_px(pip, out_h).to_bits(),
            border_color: pip.border_color,
            shadow: pip.shadow,
            pad: if pip.shadow {
                compose::shadow_pad(out_h) as usize
            } else {
                0
            },
        }
    }
}

/// A built decor: the texture's pixels and their size.
pub struct Decor {
    /// RGBA8, row-major: shadow alpha in `.r`, border (outer) mask in `.g`,
    /// camera (inner) mask in `.b`.
    pub pixels: Vec<u8>,
    pub width: usize,
    pub height: usize,
    /// Padding on every side that the shadow needed.
    pub pad: usize,
}

impl Decor {
    /// Bake the layers for an overlay `w`x`h` in a frame `out_h` tall.
    ///
    /// The texture is the overlay's box plus the shadow's padding on every
    /// side, and `drop` extra rows at the bottom: the shadow sits below the
    /// overlay, and baking that in keeps the composite a single lookup.
    pub fn build(pip: &PipSpec, w: f64, h: f64, out_h: f64) -> Decor {
        let w = w.round().max(1.0) as usize;
        let h = h.round().max(1.0) as usize;
        let bw = compose::border_px(pip, out_h);
        let pad = if pip.shadow {
            compose::shadow_pad(out_h) as usize
        } else {
            0
        };
        let drop = if pip.shadow {
            compose::shadow_drop(out_h) as usize
        } else {
            0
        };
        let outer = compose::mask(pip.shape, w, h, pip.roundness, 0.0);
        let inner = if bw > 0.0 {
            compose::mask(pip.shape, w, h, pip.roundness, bw)
        } else {
            outer.clone()
        };
        // No border width means no border layer at all (upstream only builds
        // one when the width is positive): the camera must not be tinted by
        // the border colour where the shape is semi-transparent.
        let border = if bw > 0.0 {
            outer.clone()
        } else {
            vec![0u8; w * h]
        };
        let dw = w + 2 * pad;
        let dh = h + 2 * pad + drop;
        let mut pixels = vec![0u8; dw * dh * 4];
        for px in pixels.chunks_exact_mut(4) {
            // Opaque alpha: the passes only read the first three channels.
            px[3] = 255;
        }
        if pad > 0 {
            let shadow = compose::shadow(&outer, w, h, pad, SHADOW_OPACITY);
            let sh = w + 2 * pad;
            for y in 0..sh {
                for x in 0..dw {
                    pixels[((y + drop) * dw + x) * 4] = shadow[y * sh + x];
                }
            }
        }
        for y in 0..h {
            for x in 0..w {
                let at = ((y + pad) * dw + x + pad) * 4;
                pixels[at + 1] = border[y * w + x];
                pixels[at + 2] = inner[y * w + x];
            }
        }
        Decor {
            pixels,
            width: dw,
            height: dh,
            pad,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::{Corner, Place};

    fn spec(shape: Shape) -> PipSpec {
        PipSpec {
            shape,
            place: Place::Corner(Corner::BottomRight),
            ..Default::default()
        }
    }

    #[test]
    fn the_key_ignores_the_position_but_not_the_style() {
        let base = spec(Shape::Rounded);
        let key = DecorKey::new(&base, 422.0, 237.0, 1080.0);
        // Moving the overlay rebuilds nothing.
        let moved = PipSpec {
            place: Place::At([0.3, 0.7]),
            ..base.clone()
        };
        assert_eq!(DecorKey::new(&moved, 422.0, 237.0, 1080.0), key);
        // Every style change does.
        let restyled = PipSpec {
            roundness: 0.5,
            ..base.clone()
        };
        assert_ne!(DecorKey::new(&restyled, 422.0, 237.0, 1080.0), key);
        let bordered = PipSpec {
            border: 6.0,
            ..base.clone()
        };
        assert_ne!(DecorKey::new(&bordered, 422.0, 237.0, 1080.0), key);
        let shadow = PipSpec {
            shadow: true,
            ..base.clone()
        };
        assert_ne!(DecorKey::new(&shadow, 422.0, 237.0, 1080.0), key);
        assert_eq!(DecorKey::new(&base, 200.0, 237.0, 1080.0).width, 200);
    }

    #[test]
    fn a_border_ring_is_left_transparent_to_the_camera() {
        // A sharp square, a 4 px border and no shadow: the ring is opaque to
        // the border colour and the inside to the camera.
        let pip = PipSpec {
            shape: Shape::Square,
            roundness: 0.0,
            border: 4.0,
            ..spec(Shape::Square)
        };
        let d = Decor::build(&pip, 100.0, 100.0, 1080.0);
        assert_eq!((d.width, d.height, d.pad), (100, 100, 0));
        let at = |x: usize, y: usize| {
            let i = (y * d.width + x) * 4;
            (d.pixels[i], d.pixels[i + 1], d.pixels[i + 2])
        };
        // The ring runs to the very edge: outer mask set, inner mask clear.
        assert_eq!(at(1, 50), (0, 255, 0));
        assert_eq!(at(50, 1), (0, 255, 0));
        // Inside: the camera shows through.
        assert_eq!(at(50, 50), (0, 255, 255));
        assert_eq!(at(6, 50), (0, 255, 255));
    }

    #[test]
    fn a_round_shape_drops_the_corner_pixels() {
        let pip = spec(Shape::Circle);
        let d = Decor::build(&pip, 100.0, 100.0, 1080.0);
        assert_eq!(d.pad, 0, "no shadow, no padding");
        let corner = &d.pixels[..4];
        assert_eq!(corner[1], 0, "no border layer without a border width");
        assert_eq!(corner[2], 0, "and the camera mask is clear");
        // The middle of the shape is fully covered.
        let mid = (50 * d.width + 50) * 4;
        assert_eq!(d.pixels[mid + 2], 255);
    }

    #[test]
    fn a_shadow_pads_the_box_and_is_dropped_down() {
        let pip = PipSpec {
            shadow: true,
            ..spec(Shape::Square)
        };
        let d = Decor::build(&pip, 100.0, 100.0, 1080.0);
        // 3% of 1080 = 32 px on every side, 6 px of drop at the bottom.
        assert_eq!((d.pad, d.width, d.height), (32, 164, 170));
        let h_plus_pad = 100 + 32;
        // Under the shape, the shadow is at its darkest.
        let under = ((32 + 50) * d.width + 32 + 50) * 4;
        assert!(d.pixels[under] > 100, "shadow {}", d.pixels[under]);
        // Just outside the left edge, there is some falloff but no mask.
        let beside = ((32 + 50) * d.width + 20) * 4;
        assert!(d.pixels[beside] > 0, "shadow {}", d.pixels[beside]);
        assert_eq!(d.pixels[beside + 1], 0);
        // The drop leaves a clear band at the top and fills the bottom: rows
        // below the overlay box hold shadow that only the drop puts there.
        assert_eq!(d.pixels[0], 0);
        let dropped = (h_plus_pad..h_plus_pad + 6).map(|y| d.pixels[(y * d.width + 82) * 4]);
        assert!(
            dropped.clone().any(|v| v > 0),
            "the drop is baked in: {:?}",
            dropped.collect::<Vec<_>>()
        );
    }
}
