// Separable Lanczos (a = 3) resampling, one axis per dispatch.
//
// The screen is scaled in the encoded RGB domain, plane for plane, exactly
// where upstream's MPSImageLanczosScale runs over the Y and CbCr planes
// (src/scale.rs): no linearisation, so text keeps its contrast, and the taps
// past an edge repeat the edge pixel (MPSImageEdgeMode::Clamp) rather than
// pulling the border toward zero.
//
// # It is a resampler, not an interpolator
//
// Reducing a picture means removing frequencies, so the reconstruction filter
// is stretched by the reduction factor: with `s = source pixels per
// destination pixel`, the window covers `3·s` source pixels either side and a
// tap at distance `t` from the sample position weighs `lanczos3(t / s)`.
// Six fixed taps at the source's own spacing — plain interpolation — would fold
// everything above the destination's Nyquist frequency back into the picture as
// moiré: a 1-px checkerboard on a 5K screen downscaled to 1080p would come out
// as a beat pattern instead of uniform grey.
//
// At 1:1 (s = 1) the window is the textbook six taps and nothing changes, and a
// bigger source is never enlarged at all (the renderer clamps the scale), so
// the stretch only ever widens the filter.
//
// The destination is the canvas: the picture is written where `compose::fill`
// puts it and everything else stays video black, the way MPS's clip rect
// leaves the cleared bars alone.

/// The widest stretch the loop is allowed to take: 10.6:1. The largest a
/// recording can ask for is 8K to 1080p (4:1) or 5K to 720p (3.6:1); anything
/// beyond is clamped rather than looping over hundreds of taps.
const MAX_STRETCH: f32 = 10.6;
/// Lanczos' half-width in lobes.
const LOBES: f32 = 3.0;

struct Params {
    // src.x, src.y: source plane size; dst.x, dst.y: destination size
    b0: vec4<f32>,
    // scale: source pixels per destination pixel, offset: picture origin in
    // destination pixels, extent: picture size in destination pixels,
    // axis: 0 filters x, 1 filters y
    b1: vec4<f32>,
    b2: vec4<f32>,
    b3: vec4<f32>,
};

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var<uniform> p: Params;
@group(0) @binding(2) var dst_tex: texture_storage_2d<rgba16float, write>;

@compute @workgroup_size(8, 8)
fn scale(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dst = vec2<u32>(u32(p.b0.z), u32(p.b0.w));
    if (gid.x >= dst.x || gid.y >= dst.y) {
        return;
    }
    let vertical = p.b1.w != 0.0;
    let out_at = vec2<i32>(i32(gid.x), i32(gid.y));
    // The picture's centre coordinate along the filtered axis.
    let centre = select(f32(gid.x) + 0.5, f32(gid.y) + 0.5, vertical);
    if (centre < p.b1.y || centre >= p.b1.y + p.b1.z) {
        textureStore(dst_tex, out_at, vec4<f32>(0.0, 0.0, 0.0, 1.0));
        return;
    }
    let src_len = select(p.b0.x, p.b0.y, vertical);
    let scale = p.b1.x;
    // Continuous source coordinate of this destination pixel's centre.
    let pos = (centre - p.b1.y) / scale - 0.5;
    // The filter's stretch, and its support in source pixels.
    let stretch = clamp(1.0 / scale, 1.0, MAX_STRETCH);
    let support = LOBES * stretch;
    // The taps inside the window. At 1:1 this is floor(pos - 3) + 1 ..
    // floor(pos + 3), the six taps of a Lanczos3 interpolator.
    let lo = i32(floor(pos - support)) + 1;
    let hi = i32(floor(pos + support));
    var acc = vec3<f32>(0.0, 0.0, 0.0);
    var wsum = 0.0;
    for (var t = lo; t <= hi; t = t + 1) {
        let w = lanczos3((pos - f32(t)) / stretch);
        if (w != 0.0) {
            let u = clamp(f32(t), 0.0, src_len - 1.0);
            let at = select(
                vec2<i32>(i32(u), i32(gid.y)),
                vec2<i32>(i32(gid.x), i32(u)),
                vertical,
            );
            acc = acc + textureLoad(src_tex, at, 0).rgb * w;
            wsum = wsum + w;
        }
    }
    // Normalising keeps a flat field flat, whatever the taps clamp to.
    let rgb = acc / max(wsum, 1e-6);
    textureStore(dst_tex, out_at, vec4<f32>(rgb, 1.0));
}
