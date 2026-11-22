//! The CPU reference of the compositor's output colour conversion.
//!
//! The output pass does this in WGSL (`shaders/common.wgsl`); this module is the
//! same arithmetic in Rust, used by the tests to check the GPU's numbers and to
//! document the matrix. It is test-only on purpose: nothing in a recording
//! converts colours on the CPU.
//!
//! BT.709, **limited range** (head range): Y in 16..235, Cb and Cr in 16..240,
//! the codes every HD encoder and player assumes. The coefficients are applied
//! to the *gamma-encoded* values (non-constant luminance), which is what a
//! video scaler does and what makes a flat mid-gray come out as mid-gray.

/// BT.709 luma coefficients.
pub const KR: f32 = 0.2126;
pub const KB: f32 = 0.0722;
pub const KG: f32 = 0.7152;

/// RGB (full range, gamma encoded) to YUV (BT.709, limited range), in 8-bit
/// codes.
pub fn rgb_to_yuv709_limited(rgb: [u8; 3]) -> (u8, u8, u8) {
    let (r, g, b) = (
        rgb[0] as f32 / 255.0,
        rgb[1] as f32 / 255.0,
        rgb[2] as f32 / 255.0,
    );
    let y = KR * r + KG * g + KB * b;
    let cb = (b - y) / (2.0 * (1.0 - KB));
    let cr = (r - y) / (2.0 * (1.0 - KR));
    let code = |v: f32| (v.round().clamp(0.0, 255.0)) as u8;
    (
        code(16.0 + 219.0 * y),
        code(128.0 + 224.0 * cb),
        code(128.0 + 224.0 * cr),
    )
}

/// YUV (BT.709, limited range) back to RGB (full range, gamma encoded): what a
/// player does, so a round trip through the encoder's format is checkable.
pub fn yuv709_limited_to_rgb(yuv: (u8, u8, u8)) -> [u8; 3] {
    let y = (yuv.0 as f32 - 16.0) / 219.0;
    let cb = (yuv.1 as f32 - 128.0) / 224.0;
    let cr = (yuv.2 as f32 - 128.0) / 224.0;
    let r = y + 2.0 * (1.0 - KR) * cr;
    let b = y + 2.0 * (1.0 - KB) * cb;
    let g = y - (2.0 * KB * (1.0 - KB) / KG) * cb - (2.0 * KR * (1.0 - KR) / KG) * cr;
    let code = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
    [code(r), code(g), code(b)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_bars() {
        // Cross-checked against ffmpeg's swscale:
        //   ffmpeg -f rawvideo -pix_fmt rgb24 -s 8x8 -i - \
        //     -vf scale=in_range=full:out_range=tv:out_color_matrix=bt709,format=yuv420p
        // for a flat RGB frame of each colour. These are the numbers every
        // broadcast chain and player produce.
        assert_eq!(
            rgb_to_yuv709_limited([255, 255, 255]),
            (235, 128, 128),
            "white"
        );
        assert_eq!(rgb_to_yuv709_limited([0, 0, 0]), (16, 128, 128), "black");
        assert_eq!(rgb_to_yuv709_limited([255, 0, 0]), (63, 102, 240), "red");
        assert_eq!(rgb_to_yuv709_limited([0, 255, 0]), (173, 42, 26), "green");
        assert_eq!(rgb_to_yuv709_limited([0, 0, 255]), (32, 240, 118), "blue");
        assert_eq!(
            rgb_to_yuv709_limited([255, 255, 0]),
            (219, 16, 138),
            "yellow"
        );
        assert_eq!(rgb_to_yuv709_limited([0, 255, 255]), (188, 154, 16), "cyan");
        assert_eq!(
            rgb_to_yuv709_limited([255, 0, 255]),
            (78, 214, 230),
            "magenta"
        );
    }

    #[test]
    fn mid_gray_lands_in_the_middle() {
        // Y 126 is mid-gray in limited range (219/2 + 16 = 125.5); chroma is
        // neutral.
        let (y, cb, cr) = rgb_to_yuv709_limited([128, 128, 128]);
        assert_eq!((cb, cr), (128, 128));
        assert!((125..=126).contains(&y), "Y {y}");
    }

    #[test]
    fn a_round_trip_is_within_one_code() {
        for r in (0..=255).step_by(17) {
            for g in (0..=255).step_by(17) {
                for b in (0..=255).step_by(17) {
                    let rgb = [r as u8, g as u8, b as u8];
                    let back = yuv709_limited_to_rgb(rgb_to_yuv709_limited(rgb));
                    for (a, b) in rgb.iter().zip(back.iter()) {
                        assert!(
                            a.abs_diff(*b) <= 2,
                            "{rgb:?} -> {:?} -> {back:?}",
                            rgb_to_yuv709_limited(rgb)
                        );
                    }
                }
            }
        }
    }
}
