// The composited frame to the encoder's pixel format, BT.709 limited range.
//
// Two render passes rather than one compute pass: a luma pass at full
// resolution and a chroma pass at half resolution, because an attachment's
// size is fixed by the pass. Render targets also matter for portability —
// `R8Unorm` and `Rg8Unorm` are renderable everywhere, while as *storage*
// textures they are optional (Mesa's lavapipe, the software Vulkan device the
// tests run on, does not offer `R8Unorm` storage images at all).
//
// The conversion itself is the standard non-constant-luminance one applied to
// the gamma-encoded values: Y = 0.2126 R + 0.7152 G + 0.0722 B over 16..235,
// and Cb/Cr over 16..240, averaged over each 2x2 block before subsampling.

struct Params {
    // width, height of the composited frame
    b0: vec4<f32>,
    b1: vec4<f32>,
};

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var<uniform> p: Params;

struct VSOut {
    @builtin(position) clip: vec4<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VSOut {
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    var out: VSOut;
    out.clip = vec4<f32>(x * 2.0 - 1.0, y * 2.0 - 1.0, 0.0, 1.0);
    return out;
}

// Luma of the pixel under this fragment.
@fragment
fn fs_y(in: VSOut) -> @location(0) vec4<f32> {
    let at = vec2<i32>(i32(floor(in.clip.x)), i32(floor(in.clip.y)));
    let yuv = rgb_to_yuv709_limited(textureLoad(src_tex, at, 0).rgb);
    return vec4<f32>(yuv.x, 0.0, 0.0, 1.0);
}

// The top-left pixel of this fragment's 2x2 block in the full-resolution image.
fn block(frag: vec4<f32>) -> vec2<u32> {
    return vec2<u32>(u32(floor(frag.x)) * 2u, u32(floor(frag.y)) * 2u);
}

fn chroma_of(frag: vec4<f32>) -> vec2<f32> {
    let size = vec2<u32>(u32(p.b0.x), u32(p.b0.y));
    return block_chroma(src_tex, size, block(frag));
}

// Interleaved chroma for NV12.
@fragment
fn fs_nv12(in: VSOut) -> @location(0) vec4<f32> {
    let c = chroma_of(in.clip);
    return vec4<f32>(c.x, c.y, 0.0, 1.0);
}

// Separate planes for I420.
struct Chroma {
    @location(0) u: vec4<f32>,
    @location(1) v: vec4<f32>,
};

@fragment
fn fs_i420(in: VSOut) -> Chroma {
    let c = chroma_of(in.clip);
    var out: Chroma;
    out.u = vec4<f32>(c.x, 0.0, 0.0, 1.0);
    out.v = vec4<f32>(c.y, 0.0, 0.0, 1.0);
    return out;
}
