// Composite the canvas, the webcam overlay and the camera scene into the
// output frame.
//
// A fullscreen triangle: every output pixel is written, so the pass both
// copies the scaled screen and draws the overlay on top of it.
//
// The overlay layers are the ones upstream's CoreImage graph stacks
// (src/compose.rs, Compositor::render), bottom to top: the shadow, the border
// ring, then the camera clipped to the inner mask. All three come from one
// cached decor texture (shadow in .r, outer mask in .g, inner mask in .b) that
// is rebuilt only when the size or the style changes, so dragging the overlay
// is a change of `b3` and nothing else.

struct Params {
    // canvas.x, canvas.y, cam.x, cam.y
    b0: vec4<f32>,
    // The camera's target rectangle in canvas pixels: x, y, width, height.
    b1: vec4<f32>,
    // Cover fit of the camera into that rectangle: scale, offset x, offset y,
    // decor padding.
    b2: vec4<f32>,
    // Top-left of the decor texture in canvas pixels: x, y, width, height.
    b3: vec4<f32>,
    // Border colour (r, g, b), mode: 0 blit, 1 overlay, 2 camera full frame.
    b4: vec4<f32>,
    // mirror (1 = flip the camera horizontally), unused
    b5: vec4<f32>,
};

@group(0) @binding(0) var canvas_tex: texture_2d<f32>;
@group(0) @binding(1) var cam_tex: texture_2d<f32>;
@group(0) @binding(2) var cam_smp: sampler;
@group(0) @binding(3) var decor_tex: texture_2d<f32>;
@group(0) @binding(4) var<uniform> p: Params;

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

// The camera pixel under `centre` (canvas pixel coordinates, pixel centres),
// cover-fitted into the target rectangle p.b1 and optionally mirrored.
fn cam_at(centre: vec2<f32>) -> vec3<f32> {
    let rect = p.b1;
    let s = max(p.b2.x, 1e-6);
    let local = centre - rect.xy;
    var sx = (local.x - p.b2.y) / s;
    if (p.b5.x > 0.5) {
        sx = (rect.z - p.b2.y - local.x) / s;
    }
    let sy = (local.y - p.b2.z) / s;
    let uv = vec2<f32>(sx / max(p.b0.z, 1.0), sy / max(p.b0.w, 1.0));
    return textureSampleLevel(cam_tex, cam_smp, uv, 0.0).rgb;
}

@fragment
fn fs(in: VSOut) -> @location(0) vec4<f32> {
    let at = in.clip.xy;
    let ix = vec2<i32>(i32(floor(at.x)), i32(floor(at.y)));
    var rgb = textureLoad(canvas_tex, ix, 0).rgb;
    let mode = u32(p.b4.w + 0.5);
    if (mode == 2u) {
        rgb = cam_at(at);
    } else if (mode == 1u) {
        let local = at - p.b3.xy;
        if (local.x >= 0.0 && local.y >= 0.0 && local.x < p.b3.z && local.y < p.b3.w) {
            let decor = textureLoad(decor_tex, vec2<i32>(i32(local.x), i32(local.y)), 0);
            // Soft drop shadow: toward black by its alpha.
            rgb = mix(rgb, vec3<f32>(0.0, 0.0, 0.0), decor.r);
            // Border ring.
            rgb = mix(rgb, p.b4.rgb, decor.g);
            // The camera, clipped to the inner mask.
            if (decor.b > 0.0) {
                let inside = at - p.b3.xy - vec2<f32>(p.b2.w, p.b2.w);
                rgb = mix(rgb, cam_at(p.b1.xy + inside), decor.b);
            }
        }
    }
    return vec4<f32>(rgb, 1.0);
}
