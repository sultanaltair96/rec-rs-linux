//! Pixel and maths tests for the compositor: the ported upstream render test,
//! every shape against the geometry, borders, shadows, mirroring, placement,
//! the camera scene, the Lanczos scaler against a CPU reference, the BT.709
//! output and the pipelined readback.
//!
//! They need a wgpu adapter, which on a machine without a GPU is Mesa's
//! software Vulkan device; if there is no adapter at all they print why and
//! return instead of failing.

use crate::compose::{self, Corner, PipSpec, Place, Scene, Shape};
use crate::media_types::{PixelFormat, Plane, VideoFrame};

use super::testkit::{self, Image, overlay, screen, solid};
use super::{Options, OutputFormat, Renderer};

#[test]
fn camera_upload_is_reused_only_for_the_same_plane_buffers() {
    let mut input = super::Input::default();
    let first = solid(8, 8, [255, 0, 0]);
    let same = first.clone();
    let next = solid(8, 8, [255, 0, 0]);
    assert!(input.camera_needs_upload(&first));
    assert!(!input.camera_needs_upload(&same));
    assert!(input.camera_needs_upload(&next));
    let mut changed_stride = next.clone();
    changed_stride.planes[0].stride += 4;
    assert!(input.camera_needs_upload(&changed_stride));
}

/// Redrawing a cached camera must preserve its pixels, not just its upload key.
#[test]
fn unchanged_camera_upload_draws_twice_without_flicker() {
    let pip = PipSpec {
        size: 0.5,
        shape: Shape::Square,
        roundness: 0.0,
        border: 0.0,
        shadow: false,
        place: Place::At([0.5, 0.5]),
        ..Default::default()
    };
    let Some(mut r) = renderer(128, 96, pip, Scene::ScreenCam) else {
        return;
    };
    let black = solid(128, 96, [0, 0, 0]);
    for camera in [
        solid(64, 48, [0, 255, 0]),
        testkit::solid_nv12(64, 48, [0, 255, 0]),
    ] {
        let first = Image::bgra(
            &r.render_now(Some(&black), Some(&camera), 1.0)
                .expect("first draw"),
        );
        let [red, green, blue] = first.at(64, 48);
        assert!(green > 250 && red < 6 && blue < 6, "camera is visible");
        assert_eq!(first.at(0, 0), [0, 0, 0], "screen outside camera");

        // A clone retains the plane Arcs, so the second render skips upload.
        let unchanged = camera.clone();
        let second = Image::bgra(
            &r.render_now(Some(&black), Some(&unchanged), 2.0)
                .expect("cached camera draw"),
        );
        assert_eq!(
            second.rgb, first.rgb,
            "{:?} camera flickered",
            camera.format
        );
    }
}

/// A renderer in `width`x`height` with this overlay, or a skip.
fn renderer(width: usize, height: usize, pip: PipSpec, scene: Scene) -> Option<Renderer> {
    testkit::renderer(width, height, pip, scene)
}

/// The ported upstream render test (src/compose.rs,
/// `renders_overlay_with_border_into_the_frame`): a black screen, a red camera,
/// a square overlay at 25% height with a 12 px border, then a live move.
#[test]
fn renders_overlay_with_border_into_the_frame() {
    let pip = PipSpec {
        size: 0.25,
        place: Place::At([0.5, 0.25]),
        shape: Shape::Square,
        roundness: 0.3,
        border: 12.0,
        border_color: [255, 255, 255],
        ..Default::default()
    };
    let Some(mut r) = renderer(640, 360, pip.clone(), Scene::ScreenCam) else {
        return;
    };
    let screen = solid(640, 360, [0, 0, 0]);
    let cam = solid(320, 180, [255, 0, 0]);
    let out = r
        .render_now(Some(&screen), Some(&cam), 1.0)
        .expect("render");
    assert_eq!(out.format, PixelFormat::Bgra);
    assert_eq!((out.width, out.height), (640, 360));
    let img = Image::bgra(&out);

    // The geometry the renderer used is the geometry the CLI promises.
    let rect = overlay(&pip, 640, 360, r.cam_aspect());
    assert_eq!(rect, [240.0, 10.0, 160.0, 160.0]);

    assert_eq!(img.at(320, 90), [255, 0, 0], "camera at the overlay centre");
    // 12 px of border at 1080p is 4 px at 360p: just inside the top edge.
    assert_eq!(img.at(320, 11), [255, 255, 255], "border at the top edge");
    assert_eq!(
        img.at(241, 11),
        [0, 0, 0],
        "rounded corner shows the screen"
    );
    assert_eq!(img.at(320, 200), [0, 0, 0], "screen below the overlay");

    // Moving the overlay is live.
    r.set_pip(PipSpec {
        place: Place::Corner(Corner::BottomLeft),
        ..pip
    });
    let out = r
        .render_now(Some(&screen), Some(&cam), 2.0)
        .expect("render");
    assert_eq!(out.pts, 2.0);
    let img = Image::bgra(&out);
    assert_eq!(
        img.at(320, 90),
        [0, 0, 0],
        "the old position is screen again"
    );
    assert_eq!(img.at(93, 267), [255, 0, 0], "the new position");
}

/// Every shape covers exactly the pixels `compose::coverage` says it does,
/// including the anti-aliased edge.
#[test]
fn every_shape_matches_the_mask_geometry() {
    let pip = PipSpec {
        size: 0.4,
        place: Place::At([0.5, 0.5]),
        ..Default::default()
    };
    let Some(mut r) = renderer(640, 400, pip, Scene::ScreenCam) else {
        return;
    };
    let black = solid(640, 400, [0, 0, 0]);
    let white = solid(320, 200, [255, 255, 255]);
    for shape in [
        Shape::Rounded,
        Shape::Circle,
        Shape::Square,
        Shape::Pill,
        Shape::Squircle,
        Shape::Rect,
    ] {
        let pip = PipSpec {
            shape,
            ..r.pip().clone()
        };
        r.set_pip(pip.clone());
        let out = r
            .render_now(Some(&black), Some(&white), 0.0)
            .expect("render");
        let img = Image::bgra(&out);
        let [x0, y0, ow, oh] = overlay(&pip, 640, 400, r.cam_aspect());
        let (ow, oh) = (ow, oh);
        // Probe a grid over the overlay plus a margin, and compare against the
        // coverage the CPU geometry computes.
        let mut anti_aliased = 0;
        let (ix, iy, iw, ih) = (x0 as usize, y0 as usize, ow as usize, oh as usize);
        for y in iy.saturating_sub(2)..(iy + ih + 2).min(400) {
            for x in ix.saturating_sub(2)..(ix + iw + 2).min(640) {
                let cov = if x >= ix && x < ix + iw && y >= iy && y < iy + ih {
                    compose::coverage(
                        shape,
                        ow,
                        oh,
                        pip.roundness,
                        x as f64 + 0.5 - x0,
                        y as f64 + 0.5 - y0,
                    )
                } else {
                    0.0
                };
                if cov > 0.01 && cov < 0.99 {
                    anti_aliased += 1;
                }
                let want = (cov * 255.0).round() as u8;
                let got = img.at(x, y);
                for (c, v) in got.iter().enumerate() {
                    assert!(
                        v.abs_diff(want) <= 2,
                        "{shape:?} at ({x},{y}): channel {c} is {v}, coverage {cov:.3} says {want}"
                    );
                }
            }
        }
        // A round shape has a soft edge; a rectangle does not.
        if shape != Shape::Rect {
            assert!(
                anti_aliased > 0,
                "{shape:?}: no anti-aliased pixels in the probe"
            );
        }
    }
}

/// The border is the width asked for (px at 1080p) and the colour asked for,
/// and the camera fills the rest.
#[test]
fn the_border_has_the_width_and_colour_of_the_spec() {
    let pip = PipSpec {
        shape: Shape::Square,
        roundness: 0.0,
        border: 8.0,
        border_color: [255, 120, 0],
        place: Place::Corner(Corner::TopLeft),
        ..Default::default()
    };
    let Some(mut r) = renderer(1920, 1080, pip.clone(), Scene::ScreenCam) else {
        return;
    };
    let screen = solid(1920, 1080, [0, 0, 0]);
    let cam = solid(426, 240, [0, 0, 255]);
    let out = r
        .render_now(Some(&screen), Some(&cam), 0.0)
        .expect("render");
    let img = Image::bgra(&out);
    let [x0, y0, ow, oh] = overlay(&pip, 1920, 1080, r.cam_aspect());
    assert_eq!([x0, y0, ow, oh], [38.0, 38.0, 422.0, 422.0]);
    let (x0, y0, ow, oh) = (x0 as usize, y0 as usize, ow as usize, oh as usize);
    let mid = y0 + oh / 2;
    for (edge, from, to) in [("left", x0, x0 + 8), ("right", x0 + ow - 8, x0 + ow)] {
        for x in from..to {
            assert_eq!(img.at(x, mid), [255, 120, 0], "{edge} border at x={x}");
        }
    }
    // Inside the ring the camera shows; outside the shape it is the screen.
    assert_eq!(img.at(x0 + 9, mid), [0, 0, 255], "camera inside the ring");
    assert_eq!(
        img.at(x0 + ow / 2, y0 + 9),
        [0, 0, 255],
        "camera at the top"
    );
    assert_eq!(img.at(x0 + 2, mid + oh / 2), [0, 0, 0], "screen below");
    // The border scales with the output height: 8 px at 1080p is 5 px at 720p.
    let small = PipSpec { border: 8.0, ..pip };
    let Some(mut r720) = renderer(1280, 720, small.clone(), Scene::ScreenCam) else {
        return;
    };
    let out = r720
        .render_now(
            Some(&solid(1280, 720, [0, 0, 0])),
            Some(&solid(426, 240, [0, 0, 255])),
            0.0,
        )
        .expect("render");
    let img = Image::bgra(&out);
    let [x0, y0, ow, _] = overlay(&small, 1280, 720, r720.cam_aspect());
    let (x0, y0, ow) = (x0 as usize, y0 as usize, ow as usize);
    let mid = y0 + 40;
    assert_eq!(
        img.at(x0 + 4, mid),
        [255, 120, 0],
        "5 px border, last pixel"
    );
    assert_eq!(img.at(x0 + 5, mid), [0, 0, 255], "camera after the border");
    assert_eq!(
        img.at(x0 + ow - 1, mid),
        [255, 120, 0],
        "border on the right"
    );
}

/// The drop shadow darkens the screen next to the overlay, most where it is
/// closest, and not at all beyond its spread.
#[test]
fn the_drop_shadow_falls_off_outside_the_overlay() {
    let pip = PipSpec {
        shape: Shape::Square,
        roundness: 0.0,
        shadow: true,
        border: 0.0,
        place: Place::At([0.5, 0.5]),
        size: 0.3,
        ..Default::default()
    };
    let Some(mut r) = renderer(1000, 800, pip.clone(), Scene::ScreenCam) else {
        return;
    };
    let white = solid(1000, 800, [255, 255, 255]);
    let black = solid(300, 300, [0, 0, 0]);
    let out = r
        .render_now(Some(&white), Some(&black), 0.0)
        .expect("render");
    let img = Image::bgra(&out);
    let [x0, y0, ow, oh] = overlay(&pip, 1000, 800, r.cam_aspect());
    let x = x0 as usize + (ow as usize) / 2;
    let bottom = (y0 as usize) + (oh as usize);
    // `at(0)` is the last row inside the overlay (the camera, black); the rest
    // are rows of screen below it, darkened by the shadow.
    let at = |dy: usize| img.at(x, bottom - 1 + dy)[0];
    assert_eq!(at(0), 0, "the last row inside the overlay is the camera");
    let near = at(5);
    let mid = at(15);
    let far = at(45);
    assert!(near < 250, "4 px below: {near}");
    assert!(mid > near, "14 px below is lighter: {mid} vs {near}");
    assert_eq!(far, 255, "45 px below, past the spread: {far}");
    assert!(near > 20, "the shadow is a shadow, not a black box: {near}");
}

/// Mirroring flips the camera in the overlay and in the camera scene.
#[test]
fn mirroring_flips_the_camera() {
    let pip = PipSpec {
        shape: Shape::Rounded,
        place: Place::At([0.5, 0.5]),
        size: 0.4,
        mirror: true,
        ..Default::default()
    };
    let Some(mut r) = renderer(1280, 720, pip.clone(), Scene::ScreenCam) else {
        return;
    };
    let black = solid(1280, 720, [0, 0, 0]);
    // Left third red, right third blue, so a flip is unmistakable.
    let mut halves = solid(640, 360, [220, 20, 20]);
    {
        let stride = halves.planes[0].stride;
        let data = Arc::make_mut(&mut halves.planes[0].data);
        for y in 0..360 {
            for x in 320..640 {
                let at = y * stride + x * 4;
                data[at] = 220;
                data[at + 1] = 20;
                data[at + 2] = 20;
            }
        }
    }
    let mirrored = r
        .render_now(Some(&black), Some(&halves), 0.0)
        .expect("render");
    let img = Image::bgra(&mirrored);
    let [x0, y0, ow, oh] = overlay(&pip, 1280, 720, r.cam_aspect());
    let left = ((x0 + ow * 0.2) as usize, (y0 + oh / 2.0) as usize);
    let right = ((x0 + ow * 0.8) as usize, (y0 + oh / 2.0) as usize);
    assert!(
        img.at(left.0, left.1)[2] > 150,
        "mirrored: blue on the left"
    );
    assert!(
        img.at(right.0, right.1)[0] > 150,
        "mirrored: red on the right"
    );

    r.set_pip(PipSpec {
        mirror: false,
        ..pip.clone()
    });
    let plain = r
        .render_now(Some(&black), Some(&halves), 0.0)
        .expect("render");
    let img = Image::bgra(&plain);
    assert!(
        img.at(left.0, left.1)[0] > 150,
        "not mirrored: red on the left"
    );
    assert!(
        img.at(right.0, right.1)[2] > 150,
        "not mirrored: blue on the right"
    );

    // The camera scene mirrors the same way.
    r.set_scene(Scene::Cam);
    let out = r.render_now(None, Some(&halves), 0.0).expect("render");
    let img = Image::bgra(&out);
    assert!(img.at(100, 360)[0] > 150, "camera scene, red on the left");
    assert!(
        img.at(1180, 360)[2] > 150,
        "camera scene, blue on the right"
    );
}

/// The overlay lands where `compose::overlay_rect` puts it: every corner
/// preset (with its margin) and free positions.
#[test]
fn placement_follows_the_geometry_for_corners_and_free_positions() {
    let base = PipSpec {
        shape: Shape::Square,
        roundness: 0.0,
        place: Place::Corner(Corner::BottomRight),
        ..Default::default()
    };
    let Some(mut r) = renderer(1920, 1080, base.clone(), Scene::ScreenCam) else {
        return;
    };
    let black = solid(1920, 1080, [0, 0, 0]);
    let white = solid(426, 240, [255, 255, 255]);
    let cases = [
        Place::Corner(Corner::BottomRight),
        Place::Corner(Corner::BottomLeft),
        Place::Corner(Corner::TopRight),
        Place::Corner(Corner::TopLeft),
        Place::At([0.5, 0.5]),
        Place::At([0.25, 0.75]),
        // Dragged past the edge: kept inside the frame.
        Place::At([1.3, -0.4]),
    ];
    for place in cases {
        let pip = PipSpec {
            place,
            ..base.clone()
        };
        r.set_pip(pip.clone());
        let out = r
            .render_now(Some(&black), Some(&white), 0.0)
            .expect("render");
        let img = Image::bgra(&out);
        let [x0, y0, ow, oh] = overlay(&pip, 1920, 1080, r.cam_aspect());
        let (x0, y0, ow, oh) = (x0 as usize, y0 as usize, ow as usize, oh as usize);
        // Two pixels inside each edge is the camera; two pixels outside is the
        // screen.
        let cx = x0 + ow / 2;
        let cy = y0 + oh / 2;
        for (x, y, what) in [
            (cx, y0 + 2, "top"),
            (cx, y0 + oh - 3, "bottom"),
            (x0 + 2, cy, "left"),
            (x0 + ow - 3, cy, "right"),
            (cx, cy, "centre"),
        ] {
            assert_eq!(img.at(x, y), [255, 255, 255], "{place:?}: {what} inside");
        }
        if y0 >= 4 {
            assert_eq!(img.at(cx, y0 - 3), [0, 0, 0], "{place:?}: above");
        }
        if y0 + oh + 3 < 1080 {
            assert_eq!(img.at(cx, y0 + oh + 2), [0, 0, 0], "{place:?}: below");
        }
        if x0 >= 4 {
            assert_eq!(img.at(x0 - 3, cy), [0, 0, 0], "{place:?}: left of it");
        }
        if x0 + ow + 3 < 1920 {
            assert_eq!(img.at(x0 + ow + 2, cy), [0, 0, 0], "{place:?}: right of it");
        }
    }
}

/// The camera scene covers the whole frame: a 4:3 camera into a 16:9 output is
/// cropped top and bottom, never letterboxed.
#[test]
fn the_camera_scene_covers_the_frame() {
    let pip = PipSpec::default();
    let Some(mut r) = renderer(1920, 1080, pip, Scene::Cam) else {
        return;
    };
    // Three horizontal bands: green, red, blue.
    let (cw, ch) = (1280, 960);
    let mut frame = solid(cw, ch, [20, 200, 20]);
    {
        let stride = frame.planes[0].stride;
        let data = Arc::make_mut(&mut frame.planes[0].data);
        for y in 0..ch {
            // BGRA: green, blue, red from the top.
            let colour: [u8; 4] = if y < ch / 4 {
                [20, 200, 20, 255]
            } else if y < ch * 3 / 4 {
                [200, 20, 20, 255]
            } else {
                [20, 20, 200, 255]
            };
            for x in 0..cw {
                let at = y * stride + x * 4;
                data[at..at + 4].copy_from_slice(&colour);
            }
        }
    }
    let out = r.render_now(None, Some(&frame), 0.0).expect("render");
    let img = Image::bgra(&out);
    // Cover: scale 1.5, so the visible source rows are 120..840 of 960. The
    // green band (0..240) is partly cropped, the blue band (240..720) fills
    // the middle, the red band (720..960) shows at the bottom.
    assert_eq!(img.at(960, 0), [20, 200, 20], "green at the top");
    assert_eq!(img.at(960, 540), [20, 20, 200], "blue in the middle");
    assert_eq!(img.at(960, 1079), [200, 20, 20], "red at the bottom");
    // No bars anywhere: the frame is fully covered.
    for y in [0, 1, 539, 1078, 1079] {
        for x in [0, 1, 959, 1918, 1919] {
            let p = img.at(x, y);
            assert!(p != [0, 0, 0], "bar at ({x},{y})");
        }
    }
}

/// The output is BT.709 limited range: the codes a known RGB becomes are the
/// ones every HD player and encoder expects.
#[test]
fn the_output_is_bt709_limited_range() {
    let cases: [([u8; 3], (u8, u8, u8)); 6] = [
        ([255, 255, 255], (235, 128, 128)),
        ([0, 0, 0], (16, 128, 128)),
        ([255, 0, 0], (63, 102, 240)),
        ([0, 255, 0], (173, 42, 26)),
        ([0, 0, 255], (32, 240, 118)),
        ([128, 128, 128], (126, 128, 128)),
    ];
    for format in [OutputFormat::Nv12, OutputFormat::I420] {
        for (rgb, want) in cases {
            let Some(mut r) = testkit::build(&Options {
                width: 32,
                height: 18,
                scene: Scene::Screen,
                output: format,
                pipeline_depth: 1,
                ..Default::default()
            }) else {
                return;
            };
            let out = r
                .render_now(Some(&solid(32, 18, rgb)), None, 0.0)
                .expect("render");
            assert_eq!(out.format, format.pixel_format());
            assert!(out.is_well_formed(), "{out:?}");
            let y = out.planes[0].data[0];
            let (u, v) = match format {
                OutputFormat::Nv12 => (out.planes[1].data[0], out.planes[1].data[1]),
                _ => (out.planes[1].data[0], out.planes[2].data[0]),
            };
            assert!(
                y.abs_diff(want.0) <= 1 && u.abs_diff(want.1) <= 1 && v.abs_diff(want.2) <= 1,
                "{format:?} {rgb:?}: got ({y},{u},{v}), want {want:?}"
            );
        }
    }
}

/// The Lanczos scale matches an independent CPU implementation of the same
/// filter to within 2/255.
#[test]
fn lanczos_matches_a_cpu_reference() {
    for (sw, sh, dw, dh) in [(60, 36, 40, 24), (64, 36, 32, 18), (30, 18, 20, 12)] {
        let source = screen(sw, sh);
        let Some(mut r) = testkit::build(&Options {
            width: dw,
            height: dh,
            scene: Scene::Screen,
            output: OutputFormat::Bgra,
            pipeline_depth: 1,
            ..Default::default()
        }) else {
            return;
        };
        let out = r.render_now(Some(&source), None, 0.0).expect("render");
        let got = Image::bgra(&out);
        let want = lanczos_reference(&source, dw, dh);
        got.assert_close(&want, 2, &format!("lanczos {sw}x{sh} -> {dw}x{dh}"));
    }
}

/// Downscaling has to antialias: a 1-px pattern far above the destination's
/// Nyquist frequency must average out to its own mean, with no beat pattern.
///
/// 5120x2880 scaled to 1920x1080 is 2.67:1, the largest reduction the CLI asks
/// for (a 5K display at 1080p). An interpolating kernel — fixed taps at the
/// source's spacing — folds the pattern back as moiré and fails this by two
/// orders of magnitude; with the filter stretched by the reduction factor only
/// the outermost columns move, and only because the edge pixel is replicated.
/// ffmpeg's own Lanczos behaves the same way on this pattern (swscale reaches
/// 22.6/255 at the last column and 1.6/255 inside, zscale 29.7 and 1.7), so the
/// strict bound is on the interior and the edge gets a loose one.
#[test]
fn downscaling_a_one_pixel_pattern_leaves_only_its_mean() {
    let Some(mut r) = renderer(1920, 1080, PipSpec::default(), Scene::Screen) else {
        return;
    };
    let mut failures = Vec::new();
    for (name, vertical) in [("stripes", true), ("checkerboard", false)] {
        let src = testkit::stripes(5120, 2880, vertical);
        let out = r.render_now(Some(&src), None, 0.0).expect("render");
        let img = Image::bgra(&out);
        let mean = img.mean(1);
        // The filter's support is 3 * 2.67 = 8 source pixels, about 3 output
        // pixels; 8 pixels of border is comfortably outside it.
        let (interior, ix, iy) = img.max_deviation(mean, 8);
        let (edge, ex, ey) = img.max_deviation(mean, 0);
        println!(
            "{name} 5120x2880 -> 1920x1080: mean {mean:.1}, interior {interior:.1} at ({ix},{iy}), \
             edge {edge:.1} at ({ex},{ey}), centre {:?}",
            img.at(960, 540)
        );
        if (mean - 127.5).abs() > 4.0 {
            failures.push(format!(
                "{name}: the mean should survive the scale, got {mean:.1}"
            ));
        }
        if interior > 8.0 {
            failures.push(format!(
                "{name}: a {interior:.1}/255 beat pattern at ({ix},{iy}) inside the frame, \
                 around a mean of {mean:.1}"
            ));
        }
        if edge > 32.0 {
            failures.push(format!(
                "{name}: {edge:.1}/255 at the frame's edge ({ex},{ey}); the references stay \
                 under 30"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The scaler against ffmpeg's Lanczos on the same RGB input, at the two
/// reductions this app actually uses: a 4K and a 5K screen into 1080p.
///
/// `#[ignore]`d because it needs `ffmpeg` on the PATH (the dev image has it);
/// `scripts/selftest.sh compositor` runs it. 40 dB is the bar: two Lanczos3
/// implementations differ at edges by a code or two (edge mode, rounding), and
/// nothing more.
#[test]
#[ignore = "needs ffmpeg on the PATH"]
fn the_scaler_matches_ffmpeg_lanczos() {
    for (sw, sh) in [(3840usize, 2160usize), (5120, 2880)] {
        let source = screen(sw, sh);
        let Some(mut r) = renderer(1920, 1080, PipSpec::default(), Scene::Screen) else {
            return;
        };
        let mine = Image::bgra(&r.render_now(Some(&source), None, 0.0).expect("render"));
        let Some(swscale) = ffmpeg_scale(
            &source,
            "scale=1920:1080:flags=lanczos+accurate_rnd+full_chroma_int",
        ) else {
            eprintln!("SKIP: ffmpeg is not on the PATH");
            return;
        };
        let psnr = mine.psnr(&swscale);
        let zscale = ffmpeg_scale(&source, "zscale=w=1920:h=1080:f=lanczos")
            .map(|theirs| mine.psnr(&theirs));
        println!("{sw}x{sh} -> 1920x1080: PSNR {psnr:.2} dB against swscale's Lanczos");
        if let Some(z) = zscale {
            println!("{sw}x{sh} -> 1920x1080: PSNR {z:.2} dB against zimg's Lanczos (zscale)");
        }
        assert!(
            psnr >= 40.0,
            "{sw}x{sh} -> 1920x1080 differs from ffmpeg's Lanczos by {psnr:.2} dB"
        );
    }
}

/// `source` through one ffmpeg video filter, as RGB.
fn ffmpeg_scale(source: &VideoFrame, filter: &str) -> Option<Image> {
    let stride = source.planes[0].stride;
    let data = &source.planes[0].data;
    let mut rgb = Vec::with_capacity(source.width * source.height * 3);
    for y in 0..source.height {
        for x in 0..source.width {
            let at = y * stride + x * 4;
            rgb.extend_from_slice(&[data[at + 2], data[at + 1], data[at]]);
        }
    }
    let path = std::env::temp_dir().join(format!(
        "rec-rs-ffmpeg-{}-{}x{}.rgb",
        std::process::id(),
        source.width,
        source.height
    ));
    std::fs::write(&path, &rgb).ok()?;
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-f", "rawvideo", "-pix_fmt", "rgb24", "-s"])
        .arg(format!("{}x{}", source.width, source.height))
        .arg("-i")
        .arg(&path)
        .args(["-vf", filter, "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .output()
        .ok()?;
    let _ = std::fs::remove_file(&path);
    if !out.status.success() {
        eprintln!("ffmpeg {filter}: {}", String::from_utf8_lossy(&out.stderr));
        return None;
    }
    let wanted = 1920 * 1080 * 3;
    if out.stdout.len() != wanted {
        eprintln!(
            "ffmpeg {filter}: {} bytes, wanted {wanted}",
            out.stdout.len()
        );
        return None;
    }
    Some(Image {
        width: 1920,
        height: 1080,
        rgb: out
            .stdout
            .chunks_exact(3)
            .map(|c| [c[0], c[1], c[2]])
            .collect(),
    })
}

/// A screen that is already the output size is a copy: no scaling pass touches
/// the pixels.
#[test]
fn a_screen_at_the_output_size_is_bit_exact() {
    let Some(mut r) = renderer(64, 36, PipSpec::default(), Scene::Screen) else {
        return;
    };
    let source = screen(64, 36);
    let out = r.render_now(Some(&source), None, 0.0).expect("render");
    let want = Image::bgra(&source);
    let got = Image::bgra(&out);
    got.assert_close(&want, 0, "1:1 screen");
}

/// A picture smaller than the output is never enlarged: it is centred at 1:1
/// with video black around it.
#[test]
fn a_small_screen_is_not_upscaled() {
    let Some(mut r) = renderer(64, 40, PipSpec::default(), Scene::Screen) else {
        return;
    };
    let source = screen(32, 18);
    let out = r.render_now(Some(&source), None, 0.0).expect("render");
    let img = Image::bgra(&out);
    // 64x40 output, 32x18 picture: centred at (16, 11).
    let want = Image::bgra(&source);
    for y in 0..18 {
        for x in 0..32 {
            let (got, want) = (img.at(16 + x, 11 + y), want.at(x, y));
            for c in 0..3 {
                assert!(
                    got[c].abs_diff(want[c]) <= 1,
                    "at ({x},{y}): {got:?} vs {want:?}"
                );
            }
        }
    }
    assert_eq!(img.at(0, 20), [0, 0, 0], "black bar on the left");
    assert_eq!(img.at(63, 20), [0, 0, 0], "black bar on the right");
    assert_eq!(img.at(32, 0), [0, 0, 0], "black bar on top");
}

/// Every input pixel format the contract allows goes through the same
/// pipeline and comes out as the colour it went in as.
#[test]
fn every_input_format_is_uploaded_correctly() {
    // A saturated colour and a mid grey, so a swapped channel or a wrong matrix
    // shows up.
    for rgb in [[255, 0, 0], [0, 200, 255], [128, 128, 128]] {
        let Some(mut r) = renderer(32, 18, PipSpec::default(), Scene::Screen) else {
            return;
        };
        let bgra = Image::bgra(
            &r.render_now(Some(&solid(32, 18, rgb)), None, 0.0)
                .expect("render"),
        );
        for (name, frame) in [
            ("rgba", testkit::solid_rgba(32, 18, rgb)),
            ("nv12", testkit::solid_nv12(32, 18, rgb)),
            ("i420", testkit::solid_i420(32, 18, rgb)),
        ] {
            let got = Image::bgra(&r.render_now(Some(&frame), None, 0.0).expect("render"));
            let (a, b) = (got.at(16, 9), bgra.at(16, 9));
            for c in 0..3 {
                // The YUV paths round through BT.709 and back: allow 3 codes.
                assert!(
                    a[c].abs_diff(b[c]) <= 3,
                    "{name} {rgb:?}: {a:?} vs BGRA {b:?}"
                );
            }
        }
    }
}

/// A camera frame in NV12 goes through the same upload path as a packed one.
#[test]
fn a_yuv_camera_is_uploaded_correctly() {
    let pip = PipSpec {
        shape: Shape::Square,
        roundness: 0.0,
        place: Place::At([0.5, 0.5]),
        ..Default::default()
    };
    let Some(mut r) = renderer(320, 180, pip.clone(), Scene::ScreenCam) else {
        return;
    };
    let black = solid(320, 180, [0, 0, 0]);
    let cam = testkit::solid_nv12(160, 120, [0, 255, 0]);
    let out = r.render_now(Some(&black), Some(&cam), 0.0).expect("render");
    let img = Image::bgra(&out);
    let [x, y, ow, oh] = overlay(&pip, 320, 180, r.cam_aspect());
    let (x, y) = ((x + ow / 2.0) as usize, (y + oh / 2.0) as usize);
    let [r_, g, b] = img.at(x, y);
    assert!(
        g > 250 && r_ < 6 && b < 6,
        "NV12 green camera: {r_},{g},{b}"
    );
}

/// A window resized mid-recording is re-fitted, not stretched.
#[test]
fn a_screen_that_changes_size_is_refitted() {
    let Some(mut r) = renderer(64, 40, PipSpec::default(), Scene::Screen) else {
        return;
    };
    let first = screen(64, 40);
    let out = r.render_now(Some(&first), None, 0.0).expect("render");
    assert_eq!(Image::bgra(&out).at(5, 5), Image::bgra(&first).at(5, 5));
    // A 32x18 window into the same output: fitted, centred, bilinear-free (1:1).
    let second = screen(32, 18);
    let out = r.render_now(Some(&second), None, 0.033).expect("render");
    let img = Image::bgra(&out);
    let want = Image::bgra(&second);
    assert_eq!(
        img.at(16 + 5, 11 + 5),
        want.at(5, 5),
        "the new size lands centred"
    );
    assert_eq!(img.at(0, 0), [0, 0, 0], "with black around it");
}

/// An NV12 screen frame goes through the same pipeline as a BGRA one.
#[test]
fn yuv_screen_input_is_converted_before_scaling() {
    let Some(mut r) = renderer(32, 18, PipSpec::default(), Scene::Screen) else {
        return;
    };
    let out = r
        .render_now(Some(&testkit::solid_nv12(32, 18, [255, 0, 0])), None, 0.0)
        .expect("render");
    let img = Image::bgra(&out);
    // BT.709 round trip of pure red: within a code or two of it.
    let [r_, g, b] = img.at(16, 9);
    assert!(
        r_ > 250 && g < 6 && b < 6,
        "NV12 red came back as {r_},{g},{b}"
    );
}

/// A captured window's transparent rounded corners take the window's own edge
/// colour when the corner fill is on, and stay black when it is off.
#[test]
fn the_corner_fill_replaces_transparent_window_corners() {
    let (w, h) = (200, 120);
    let edge = [40, 120, 200];
    // A window: one colour, transparent rounded corners, and a darker stripe
    // inside the inset band that must survive the fill untouched.
    let stripe = [10, 200, 20];
    let mut frame = solid(w, h, edge);
    {
        let stride = frame.planes[0].stride;
        let data = Arc::make_mut(&mut frame.planes[0].data);
        let radius = 20.0;
        for y in 0..24 {
            for x in 0..24 {
                let (dx, dy) = (radius - x as f32 - 0.5, radius - y as f32 - 0.5);
                if dx > 0.0 && dy > 0.0 && (dx * dx + dy * dy).sqrt() > radius {
                    let at = y * stride + x * 4;
                    data[at..at + 4].copy_from_slice(&[0, 0, 0, 0]);
                }
            }
        }
        for y in 0..h {
            for x in 5..20 {
                let at = y * stride + x * 4;
                data[at..at + 4].copy_from_slice(&[stripe[2], stripe[1], stripe[0], 255]);
            }
        }
    }
    // `corner_inset(200, 120)` is 30: the fill samples the frame's edges past
    // the largest window radius, so the whole corner takes one colour.
    assert_eq!(compose::corner_inset(w as f64, h as f64), 30.0);
    let Some(mut r) = renderer(w, h, PipSpec::default(), Scene::Screen) else {
        return;
    };
    let off = Image::bgra(&r.render_now(Some(&frame), None, 0.0).expect("render"));
    assert_eq!(
        off.at(1, 1),
        [0, 0, 0],
        "transparent corner without the fill"
    );
    assert_eq!(off.at(8, 60), stripe, "the stripe without the fill");
    assert_eq!(off.at(100, 60), edge, "the window itself is untouched");

    r.set_corner_fill(true);
    let on = Image::bgra(&r.render_now(Some(&frame), None, 0.0).expect("render"));
    assert_eq!(
        on.at(1, 1),
        edge,
        "the transparent corner takes the edge colour"
    );
    assert_eq!(
        on.at(8, 60),
        stripe,
        "opaque content inside the band is untouched"
    );
    assert_eq!(on.at(40, 60), edge, "the window itself is still untouched");
}

/// Scenes and camera changes apply from the next frame.
#[test]
fn scenes_and_camera_changes_apply_from_the_next_frame() {
    let Some(mut r) = renderer(64, 36, PipSpec::default(), Scene::Screen) else {
        return;
    };
    let screen = solid(64, 36, [10, 20, 30]);
    let cam = solid(320, 180, [200, 100, 50]);
    let out = r
        .render_now(Some(&screen), Some(&cam), 0.0)
        .expect("render");
    let img = Image::bgra(&out);
    assert_eq!(img.at(32, 18), [10, 20, 30], "screen only");
    assert_eq!(
        r.cam_aspect(),
        320.0 / 180.0,
        "the camera's aspect is tracked"
    );

    r.set_scene(Scene::Cam);
    let out = r
        .render_now(Some(&screen), Some(&cam), 0.0)
        .expect("render");
    let img = Image::bgra(&out);
    assert_eq!(img.at(32, 18), [200, 100, 50], "the camera fills the frame");

    // A portrait camera changes the overlay's shape on the next frame.
    r.set_scene(Scene::ScreenCam);
    let portrait = solid(90, 160, [0, 0, 0]);
    let _ = r
        .render_now(Some(&screen), Some(&portrait), 0.0)
        .expect("render");
    assert_eq!(r.cam_aspect(), 90.0 / 160.0);
    let out = r
        .render_now(Some(&screen), Some(&portrait), 0.0)
        .expect("render");
    let img = Image::bgra(&out);
    let rect = overlay(r.pip(), 64, 36, r.cam_aspect());
    assert!(
        rect[3] > rect[2],
        "a portrait camera makes a tall overlay: {rect:?}"
    );
    assert_eq!(
        img.at(
            (rect[0] + rect[2] / 2.0) as usize,
            (rect[1] + rect[3] / 2.0) as usize
        ),
        [0, 0, 0]
    );
}

/// With no frames at all the compositor still produces an output frame: black.
#[test]
fn an_empty_frame_is_black() {
    for scene in Scene::ALL {
        let Some(mut r) = renderer(32, 18, PipSpec::default(), scene) else {
            return;
        };
        let out = r.render_now(None, None, 0.0).expect("render");
        let img = Image::bgra(&out);
        assert_eq!(img.at(16, 9), [0, 0, 0], "{scene:?}");
    }
}

/// The readback is pipelined: one frame is in flight, each call returns the
/// previous one with its own timestamp, and `flush` returns the last.
#[test]
fn the_readback_is_pipelined() {
    let Some(mut r) = testkit::build(&Options {
        width: 16,
        height: 16,
        scene: Scene::Screen,
        output: OutputFormat::Bgra,
        pipeline_depth: 2,
        ..Default::default()
    }) else {
        return;
    };
    let frame = solid(16, 16, [1, 2, 3]);
    // The first frame fills the pipeline; nothing comes back yet.
    assert!(
        r.render(Some(&frame), None, 10.0)
            .expect("render")
            .is_none()
    );
    assert_eq!(r.in_flight(), 1);
    // From then on, each call returns the frame before it.
    let got = r
        .render(Some(&frame), None, 11.0)
        .expect("render")
        .expect("a frame");
    assert_eq!(got.pts, 10.0);
    assert_eq!(r.in_flight(), 1);
    let got = r
        .render(Some(&frame), None, 12.0)
        .expect("render")
        .expect("a frame");
    assert_eq!(got.pts, 11.0);
    // And the last one is only in `flush`.
    let rest = r.flush().expect("flush");
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].pts, 12.0);
    assert_eq!(r.in_flight(), 0);
    // A synchronous renderer always answers immediately.
    let Some(mut sync) = testkit::build(&Options {
        width: 16,
        height: 16,
        scene: Scene::Screen,
        output: OutputFormat::Bgra,
        pipeline_depth: 1,
        ..Default::default()
    }) else {
        return;
    };
    let got = sync.render(Some(&frame), None, 7.0).expect("render");
    assert_eq!(got.map(|f| f.pts), Some(7.0));
}

/// The default constructor is the writer's: NV12 output at the size asked for,
/// with the depth-2 pipeline.
#[test]
fn the_default_construction_is_the_writers() {
    let _ = testkit::gpu();
    let Some(r) = testkit::build(&Options::default()) else {
        return;
    };
    assert_eq!(r.size(), (1920, 1080));
    assert_eq!(r.output(), OutputFormat::Nv12);
    assert_eq!(r.scene(), Scene::ScreenCam);
    assert_eq!(r.options().pipeline_depth, 2);
    assert_eq!(r.texture_format(), wgpu::TextureFormat::Bgra8Unorm);

    // `Renderer::new` keeps the seam's signature and the same defaults.
    let info = r.adapter_info().clone();
    let (device, queue) = (r.device().clone(), r.queue().clone());
    drop(r);
    let Some(pip) = Some(PipSpec::default()) else {
        return;
    };
    let r = Renderer::with_device(
        device,
        queue,
        info,
        &Options {
            pip,
            ..Default::default()
        },
    )
    .expect("a renderer on the shared device");
    assert_eq!(r.output(), OutputFormat::Nv12);
}

/// The preview gets the composited frame as a texture at the output size.
#[test]
fn the_composited_frame_is_exposed_for_the_preview() {
    let Some(mut r) = renderer(64, 36, PipSpec::default(), Scene::Screen) else {
        return;
    };
    assert!(r.texture().is_none(), "nothing rendered yet");
    let frame = solid(64, 36, [1, 2, 3]);
    let _ = r.render_now(Some(&frame), None, 0.0).expect("render");
    let texture = r.texture().expect("the composited frame");
    assert_eq!(texture.format(), wgpu::TextureFormat::Bgra8Unorm);
    assert_eq!(texture.width(), 64);
    assert_eq!(texture.height(), 36);
}

/// Exercise the real compositor and the app's inbox across repeated frames,
/// including redraws between the writer's two callbacks. CPU pts deliberately
/// differ: readback pipelining means timestamp equality is not a pairing key.
#[test]
fn preview_publishes_one_path_per_compositor_emission() {
    use crate::app::{TestPreviewFrame as Frame, TestPreviewTap as Tap};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let Some(mut r) = renderer(64, 36, PipSpec::default(), Scene::Screen) else {
        return;
    };
    let wakes = Arc::new(AtomicUsize::new(0));
    let count = wakes.clone();
    let (mut tap, cpu) = Tap::new(Arc::new(move || {
        count.fetch_add(1, Ordering::SeqCst);
    }));
    let gpu = tap.gpu_sink();
    tap.set_wanted(true);
    let input = solid(64, 36, [21, 45, 67]);
    for n in 1..=20 {
        let pts = n as f64;
        let frame = r.render_now(Some(&input), None, pts).expect("render");
        let mut delayed = frame.clone();
        delayed.pts -= 0.5;
        // Run callbacks on a producer thread; force the UI to drain during
        // the gap, not merely after both callbacks have completed.
        let wakes_before = wakes.load(Ordering::SeqCst);
        std::thread::scope(|scope| {
            let (sent, recv) = std::sync::mpsc::channel();
            let (resume, wait) = std::sync::mpsc::channel();
            let tex = r.preview_frame(pts).expect("composited texture");
            let gpu = &gpu;
            let cpu = &cpu;
            scope.spawn(move || {
                gpu(tex);
                sent.send(()).unwrap();
                wait.recv().unwrap();
                cpu(delayed);
            });
            recv.recv().unwrap();
            assert!(tap.take().is_none(), "a half-pair must not be visible");
            assert_eq!(wakes.load(Ordering::SeqCst), wakes_before);
            resume.send(()).unwrap();
        });
        let Some(Frame::Gpu(tex)) = tap.take() else {
            panic!("expected compositor texture, emission {n}");
        };
        assert_eq!(tex.pts, pts);
        assert_eq!((tex.width, tex.height), (64, 36));
        for _ in 0..3 {
            assert!(tap.take().is_none(), "no leftover CPU frame on repaint");
        }
    }
    assert!(wakes.load(Ordering::SeqCst) > 0);

    // No GPU callback means genuine fallback, even after using the GPU.
    cpu(input.clone());
    assert!(matches!(tap.take(), Some(Frame::Cpu(_))));
    assert!(tap.take().is_none());

    // Hide/show between callbacks must not discard pairing information.
    gpu(r.preview_frame(21.0).unwrap());
    tap.set_wanted(false);
    tap.set_wanted(true);
    cpu(input.clone());
    assert!(matches!(tap.take(), Some(Frame::Gpu(_))));
    tap.set_wanted(false);
    gpu(r.preview_frame(22.0).unwrap());
    cpu(input.clone());
    tap.set_wanted(true);
    cpu(input.clone());
    assert!(
        matches!(tap.take(), Some(Frame::Cpu(_))),
        "hidden pair drained"
    );

    // A slow UI sees only the newest committed emission, not a queue.
    gpu(r.preview_frame(23.0).unwrap());
    cpu(input.clone());
    gpu(r.preview_frame(24.0).unwrap());
    cpu(input);
    let Some(Frame::Gpu(tex)) = tap.take() else {
        panic!("newest texture");
    };
    assert_eq!(tex.pts, 24.0);
    assert!(tap.take().is_none());
}

/// The compositor can move to the writer thread: the writer owns it there.
#[test]
fn the_renderer_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Renderer>();
    assert_send::<Options>();
}

/// A frame that is not well formed is refused with a message naming it.
#[test]
fn a_malformed_frame_is_an_error() {
    let Some(mut r) = renderer(16, 16, PipSpec::default(), Scene::Screen) else {
        return;
    };
    let broken = VideoFrame {
        pts: 0.0,
        width: 16,
        height: 16,
        format: PixelFormat::I420,
        planes: vec![Plane::new(vec![0u8; 16], 16)],
    };
    let err = r
        .render_now(Some(&broken), None, 0.0)
        .expect_err("must fail");
    assert!(err.to_string().contains("screen frame"), "{err}");
}

// -- the CPU reference -----------------------------------------------------

/// The Lanczos window of half-width 3, written here from the definition rather
/// than shared with the shader, so the comparison is a real cross-check.
fn lanczos3(x: f64) -> f64 {
    let a = x.abs();
    if a < 1e-6 {
        return 1.0;
    }
    if a >= 3.0 {
        return 0.0;
    }
    let pix = std::f64::consts::PI * a;
    (3.0 * pix.sin() * (pix / 3.0).sin()) / (pix * pix)
}

/// One axis of the separable resampler at a destination coordinate.
///
/// Written from the definition rather than shared with the shader: a Lanczos3
/// reconstruction kernel of half-width 3, **stretched by the reduction factor**
/// (a kernel at the source's own spacing would interpolate and alias), sampled
/// at the source taps inside its support and normalised so a flat field stays
/// flat. At 1:1 it is the textbook six taps.
fn filter(
    dst_pixels: usize,
    scale: f64,
    offset: f64,
    src_len: usize,
    mut sample: impl FnMut(usize) -> [f32; 3],
) -> Vec<[f32; 3]> {
    let stretch = (1.0 / scale).max(1.0);
    let support = 3.0 * stretch;
    (0..dst_pixels)
        .map(|i| {
            let pos = (i as f64 + 0.5 - offset) / scale - 0.5;
            // Taps inside the window: floor(pos - 3s) + 1 .. floor(pos + 3s).
            let lo = (pos - support).floor() as i64 + 1;
            let hi = (pos + support).floor() as i64;
            let mut acc = [0.0f64; 3];
            let mut wsum = 0.0;
            for t in lo..=hi {
                let w = lanczos3((pos - t as f64) / stretch);
                if w != 0.0 {
                    let u = (t as f64).clamp(0.0, src_len as f64 - 1.0) as usize;
                    let c = sample(u);
                    for (k, v) in acc.iter_mut().enumerate() {
                        *v += c[k] as f64 * w;
                    }
                    wsum += w;
                }
            }
            let inv = 1.0 / wsum.max(1e-6);
            [
                (acc[0] * inv) as f32,
                (acc[1] * inv) as f32,
                (acc[2] * inv) as f32,
            ]
        })
        .collect()
}

/// The whole two-pass scale, in f32, the way the shaders do it.
fn lanczos_reference(src: &VideoFrame, dw: usize, dh: usize) -> Image {
    let (sw, sh) = (src.width, src.height);
    let stride = src.planes[0].stride;
    let data = &src.planes[0].data;
    let pixel = |x: usize, y: usize| -> [f32; 3] {
        let at = y * stride + x * 4;
        [
            data[at + 2] as f32 / 255.0,
            data[at + 1] as f32 / 255.0,
            data[at] as f32 / 255.0,
        ]
    };
    let (sx, sy, tx, ty) = compose::fill_no_upscale(sw as f64, sh as f64, dw as f64, dh as f64);
    // Horizontal: each row of the source becomes a row of `dw` pixels.
    let mut mid = vec![[0f32; 3]; dw * sh];
    for y in 0..sh {
        let row = filter(dw, sx, tx, sw, |x| pixel(x, y));
        mid[y * dw..(y + 1) * dw].copy_from_slice(&row);
    }
    // Vertical: each column of the intermediate becomes `dh` pixels.
    let mut out = vec![[0f32; 3]; dw * dh];
    for x in 0..dw {
        let column = filter(dh, sy, ty, sh, |y| mid[y * dw + x]);
        for (y, value) in column.iter().enumerate() {
            out[y * dw + x] = *value;
        }
    }
    // The bars stay video black.
    for y in 0..dh {
        for x in 0..dw {
            let (cx, cy) = (x as f64 + 0.5, y as f64 + 0.5);
            if cx < tx || cx >= tx + sw as f64 * sx || cy < ty || cy >= ty + sh as f64 * sy {
                out[y * dw + x] = [0.0, 0.0, 0.0];
            }
        }
    }
    Image {
        width: dw,
        height: dh,
        rgb: out
            .iter()
            .map(|c| {
                [
                    (c[0].clamp(0.0, 1.0) * 255.0).round() as u8,
                    (c[1].clamp(0.0, 1.0) * 255.0).round() as u8,
                    (c[2].clamp(0.0, 1.0) * 255.0).round() as u8,
                ]
            })
            .collect(),
    }
}

use std::sync::Arc;

#[cfg(test)]
mod reference_tests {
    use super::*;

    #[test]
    fn the_reference_filter_keeps_a_flat_field_flat() {
        let flat = [[0.5f32, 0.25, 1.0]; 16];
        let column = filter(8, 0.5, 0.0, 16, |i| flat[i]);
        for c in column {
            for (v, want) in c.iter().zip([0.5, 0.25, 1.0]) {
                assert!((v - want).abs() < 1e-6, "{c:?}");
            }
        }
    }

    #[test]
    fn the_reference_filter_stretches_with_the_reduction() {
        // A single bright column, halved: the stretched kernel spreads it over
        // a window twice as wide as an interpolating one would, and far away
        // the picture is untouched.
        let mut src = [[0.0f32; 3]; 16];
        src[8] = [1.0, 1.0, 1.0];
        let column = filter(8, 0.5, 0.0, 16, |i| src[i]);
        let lit = column.iter().filter(|c| c[0].abs() > 1e-3).count();
        assert!(
            lit >= 3,
            "a stretched kernel touches several outputs: {column:?}"
        );
        assert!(
            column[4][0] > 0.2,
            "the bright column lands near output 4: {column:?}"
        );
        assert!(column.iter().all(|c| c[0].abs() < 0.8), "{column:?}");
        assert_eq!(column[0][0], 0.0, "far away it is black");
        // At 1:1 the kernel is the six-tap interpolator: one output is lit.
        let same = filter(16, 1.0, 0.0, 16, |i| src[i]);
        assert!(same[8][0] > 0.99, "{same:?}");
    }
}
