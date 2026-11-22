// Captured frame (BGRA, RGBA, NV12 or I420) to the compositor's RGBA8 working
// space: gamma encoded, full range, no transfer-function change.
//
// Also fills a captured window's transparent rounded corners: every sample of
// the source is clamped into the inset rectangle, so the corners take the
// colour of the chrome next to them instead of compositing as black. That is
// upstream's CoreImage `imageByCroppingToRect(...).imageByClampingToExtent()`
// (src/compose.rs, CornerFill) done by one clamp on the sample coordinates.

struct Params {
    // size.x, size.y: frame size; inset: corner-fill clamp in pixels (0 off)
    b0: vec4<f32>,
    // chroma.x, chroma.y: chroma plane size (NV12/I420)
    b1: vec4<f32>,
    b2: vec4<f32>,
    b3: vec4<f32>,
};

@group(0) @binding(0) var tex_y: texture_2d<f32>;
@group(0) @binding(1) var tex_cb: texture_2d<f32>;
@group(0) @binding(2) var tex_cr: texture_2d<f32>;
@group(0) @binding(3) var<uniform> p: Params;
@group(0) @binding(4) var out_tex: texture_storage_2d<rgba8unorm, write>;

@compute @workgroup_size(8, 8)
fn from_packed(@builtin(global_invocation_id) gid: vec3<u32>) {
    let size = vec2<i32>(i32(p.b0.x), i32(p.b0.y));
    if (gid.x >= u32(size.x) || gid.y >= u32(size.y)) {
        return;
    }
    let here = vec2<i32>(i32(gid.x), i32(gid.y));
    let src = textureLoad(tex_y, here, 0);
    var rgb = src.rgb;
    if (p.b0.z > 0.0) {
        // Corner fill: the clamped-edge colour shows through where the window
        // is transparent, and the window's own pixels stay exactly as they
        // are (upstream composites the frame *over* the clamped background).
        let back = textureLoad(tex_y, clamped(here, size), 0);
        rgb = mix(back.rgb, src.rgb, src.a);
    }
    textureStore(out_tex, here, vec4<f32>(rgb, 1.0));
}

@compute @workgroup_size(8, 8)
fn from_nv12(@builtin(global_invocation_id) gid: vec3<u32>) {
    let size = vec2<i32>(i32(p.b0.x), i32(p.b0.y));
    if (gid.x >= u32(size.x) || gid.y >= u32(size.y)) {
        return;
    }
    let at = clamped(vec2<i32>(i32(gid.x), i32(gid.y)), size);
    let y = textureLoad(tex_y, at, 0).r;
    let csize = vec2<i32>(i32(p.b1.x), i32(p.b1.y));
    // Interleaved CbCr at half resolution in both directions.
    let cat = vec2<i32>(at.x / 2, at.y / 2);
    let c = textureLoad(tex_cb, clamp(cat, vec2<i32>(0, 0), csize - vec2<i32>(1, 1)), 0);
    let rgb = yuv709_limited_to_rgb(y, c.r, c.g);
    textureStore(out_tex, vec2<i32>(i32(gid.x), i32(gid.y)), vec4<f32>(rgb, 1.0));
}

@compute @workgroup_size(8, 8)
fn from_i420(@builtin(global_invocation_id) gid: vec3<u32>) {
    let size = vec2<i32>(i32(p.b0.x), i32(p.b0.y));
    if (gid.x >= u32(size.x) || gid.y >= u32(size.y)) {
        return;
    }
    let at = clamped(vec2<i32>(i32(gid.x), i32(gid.y)), size);
    let y = textureLoad(tex_y, at, 0).r;
    let csize = vec2<i32>(i32(p.b1.x), i32(p.b1.y));
    let cat = clamp(vec2<i32>(at.x / 2, at.y / 2), vec2<i32>(0, 0), csize - vec2<i32>(1, 1));
    let u = textureLoad(tex_cb, cat, 0).r;
    let v = textureLoad(tex_cr, cat, 0).r;
    let rgb = yuv709_limited_to_rgb(y, u, v);
    textureStore(out_tex, vec2<i32>(i32(gid.x), i32(gid.y)), vec4<f32>(rgb, 1.0));
}

// The corner fill: sample coordinates are clamped into the inset rectangle, so
// anything outside it takes the colour of its edge.
fn clamped(at: vec2<i32>, size: vec2<i32>) -> vec2<i32> {
    let inset = i32(p.b0.z);
    let lo = vec2<i32>(inset, inset);
    let hi = max(lo, size - vec2<i32>(1, 1) - lo);
    return clamp(at, lo, hi);
}
