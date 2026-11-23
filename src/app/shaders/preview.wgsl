// The preview frame straight from the compositor, converted and scaled on the
// GPU (G10). One pass: sample the frame's planes (BGRA/RGBA, NV12 or I420),
// BT.709 limited range to RGB, bilinear scaling by the sampler. The CPU only
// copies the planes into textures.
//
// The BT.709 constants match `preview::yuv_to_rgb` (the CPU reference the unit
// tests check); keep the two in step.

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// A full-viewport triangle: egui sets the viewport to the preview's rect, so
// this fills exactly the picture.
@vertex
fn vs(@builtin(vertex_index) i: u32) -> VOut {
    var p = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    let xy = p[i];
    var o: VOut;
    o.pos = vec4<f32>(xy, 0.0, 1.0);
    // The texture's first row is the top of the picture.
    o.uv = vec2<f32>((xy.x + 1.0) * 0.5, (1.0 - xy.y) * 0.5);
    return o;
}

@group(0) @binding(0) var plane0: texture_2d<f32>;
@group(0) @binding(1) var plane1: texture_2d<f32>;
@group(0) @binding(2) var plane2: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;

// The composited texture's own bind group: binding 0 is the texture, binding 1
// its sampler (a pipeline layout takes one group; the two preview paths are two
// entry points and two layouts, not two shaders).
@group(0) @binding(0) var frame_tex: texture_2d<f32>;
@group(0) @binding(1) var frame_samp: sampler;

fn yuv_to_rgb(y: f32, u: f32, v: f32) -> vec3<f32> {
    let yy = (y * 255.0 - 16.0) * (255.0 / 219.0) / 255.0;
    let uu = u - 128.0 / 255.0;
    let vv = v - 128.0 / 255.0;
    let r = yy + 1.5748 * vv;
    let g = yy - 0.1873 * uu - 0.4681 * vv;
    let b = yy + 1.8556 * uu;
    return clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0));
}

// The compositor's own output (G10): already gamma-encoded BGRA at the output
// size, drawn 1:1 — no conversion, no upload. Its texture is a `Bgra8Unorm`
// texture, so sampling already gives RGB in the right order (unlike the upload
// path, whose bytes are copied verbatim into an `Rgba8Unorm` texture and need
// the swap). The sampler is linear, so drawing into a smaller preview rect is
// the only scaling there is.
@fragment
fn fs_texture(in: VOut) -> @location(0) vec4<f32> {
    let c = textureSample(frame_tex, frame_samp, in.uv);
    return vec4<f32>(c.rgb, 1.0);
}

// Packed BGRA: bytes are B,G,R,A, so swap red and blue.
@fragment
fn fs_bgra(in: VOut) -> @location(0) vec4<f32> {
    let c = textureSample(plane0, samp, in.uv);
    return vec4<f32>(c.b, c.g, c.r, 1.0);
}

@fragment
fn fs_rgba(in: VOut) -> @location(0) vec4<f32> {
    let c = textureSample(plane0, samp, in.uv);
    return vec4<f32>(c.r, c.g, c.b, 1.0);
}

// NV12: a full-size luma plane and an interleaved half-size UV plane.
@fragment
fn fs_nv12(in: VOut) -> @location(0) vec4<f32> {
    let y = textureSample(plane0, samp, in.uv).r;
    let uv = textureSample(plane1, samp, in.uv).rg;
    return vec4<f32>(yuv_to_rgb(y, uv.r, uv.g), 1.0);
}

// I420: luma plus separate half-size U and V planes.
@fragment
fn fs_i420(in: VOut) -> @location(0) vec4<f32> {
    let y = textureSample(plane0, samp, in.uv).r;
    let u = textureSample(plane1, samp, in.uv).r;
    let v = textureSample(plane2, samp, in.uv).r;
    return vec4<f32>(yuv_to_rgb(y, u, v), 1.0);
}
