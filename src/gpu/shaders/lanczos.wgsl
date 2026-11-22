// Separable Lanczos (a = 3) scaling, one axis per dispatch.
//
// The screen is scaled in the encoded RGB domain, plane for plane, exactly
// where upstream's MPSImageLanczosScale runs over the Y and CbCr planes
// (src/scale.rs): no linearisation, so text keeps its contrast, and the taps
// past an edge repeat the edge pixel (MPSImageEdgeMode::Clamp) rather than
// pulling the border toward zero.
//
// The destination is the canvas: the picture is written where `compose::fill`
// puts it and everything else stays video black, the way MPS's clip rect
// leaves the cleared bars alone.

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
    // Continuous source coordinate of this destination pixel's centre.
    let pos = (centre - p.b1.y) / p.b1.x - 0.5;
    let base = floor(pos);
    var acc = vec3<f32>(0.0, 0.0, 0.0);
    var wsum = 0.0;
    for (var j: i32 = -2; j < 4; j = j + 1) {
        let w = lanczos3(pos - (base + f32(j)));
        if (w != 0.0) {
            let t = clamp(base + f32(j), 0.0, src_len - 1.0);
            let at = select(
                vec2<i32>(i32(t), i32(gid.y)),
                vec2<i32>(i32(gid.x), i32(t)),
                vertical,
            );
            acc = acc + textureLoad(src_tex, at, 0).rgb * w;
            wsum = wsum + w;
        }
    }
    let rgb = acc / max(wsum, 1e-6);
    textureStore(dst_tex, out_at, vec4<f32>(rgb, 1.0));
}
