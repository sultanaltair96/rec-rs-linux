// Helpers shared by the compositor's passes: the Lanczos window, the BT.709
// limited-range matrix in both directions, and the 4:2:0 chroma average.
//
// Adapted from rec-rs src/scale.rs and src/compose.rs (MIT OR Apache-2.0),
// with MPS/core-video calls replaced by the maths they implement.

const LAN_A: f32 = 3.0;
const PI: f32 = 3.141592653589793;
// BT.709 luma weights (Kr, Kb), the ones the encoders and every player assume
// for anything HD. Kg follows.
const KR: f32 = 0.2126;
const KB: f32 = 0.0722;
const KG: f32 = 0.7152;

// The Lanczos window of half-width 3 (a = 3), the filter Metal Performance
// Shaders uses for MPSImageLanczosScale. `x` is in source pixels from the tap.
fn lanczos3(x: f32) -> f32 {
    let ax = abs(x);
    if (ax < 1e-6) {
        return 1.0;
    }
    if (ax >= LAN_A) {
        return 0.0;
    }
    let px = PI * ax;
    return (LAN_A * sin(px) * sin(px / LAN_A)) / (px * px);
}

// YUV (BT.709 primaries, limited range, 8-bit codes as 0..1) to RGB (full
// range, still gamma encoded: no transfer-function change, like a video
// scaler that never leaves the encoded domain).
fn yuv709_limited_to_rgb(y: f32, cb: f32, cr: f32) -> vec3<f32> {
    let yl = (y * 255.0 - 16.0) / 219.0;
    let cbl = (cb * 255.0 - 128.0) / 224.0;
    let crl = (cr * 255.0 - 128.0) / 224.0;
    let r = yl + 2.0 * (1.0 - KR) * crl;
    let b = yl + 2.0 * (1.0 - KB) * cbl;
    let g = yl - (2.0 * KB * (1.0 - KB) / KG) * cbl - (2.0 * KR * (1.0 - KR) / KG) * crl;
    return clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0));
}

// RGB (gamma encoded, full range) to YUV (BT.709, limited range), the codes
// scaled to 0..1: (Y 16..235, Cb/Cr 16..240)/255.
fn rgb_to_yuv709_limited(rgb: vec3<f32>) -> vec3<f32> {
    let y = KR * rgb.r + (1.0 - KR - KB) * rgb.g + KB * rgb.b;
    let cb = (rgb.b - y) / (2.0 * (1.0 - KB));
    let cr = (rgb.r - y) / (2.0 * (1.0 - KR));
    return vec3<f32>((16.0 + 219.0 * y) / 255.0, (128.0 + 224.0 * cb) / 255.0, (128.0 + 224.0 * cr) / 255.0);
}

// One 4:2:0 chroma sample for the 2x2 block whose top-left pixel is `gid`:
// the average of the full-resolution chroma of the four pixels, which is the
// right low-pass before subsampling (and what a 4:4:4-to-4:2:0 conversion in
// swscale/zscale does).
fn block_chroma(src: texture_2d<f32>, size: vec2<u32>, gid: vec2<u32>) -> vec2<f32> {
    var acc = vec2<f32>(0.0, 0.0);
    var n = 0.0;
    for (var dy = 0u; dy < 2u; dy = dy + 1u) {
        for (var dx = 0u; dx < 2u; dx = dx + 1u) {
            let x = gid.x + dx;
            let y = gid.y + dy;
            if (x < size.x && y < size.y) {
                let c = rgb_to_yuv709_limited(textureLoad(src, vec2<i32>(i32(x), i32(y)), 0).rgb);
                acc = acc + vec2<f32>(c.y, c.z);
                n = n + 1.0;
            }
        }
    }
    return acc / max(n, 1.0);
}
