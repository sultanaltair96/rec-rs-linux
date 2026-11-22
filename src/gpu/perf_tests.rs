//! Performance of the compositor on the adapter it finds, with the readback
//! included.
//!
//! `#[ignore]`d because it is a measurement, not a correctness check:
//!
//! ```sh
//! scripts/dev.sh cargo test gpu::perf_tests -- --ignored --nocapture
//! ```
//!
//! Two numbers per case: the synchronous path (`pipeline_depth: 1`, one frame
//! through the GPU and back at a time) and the pipelined path (depth 2, which
//! overlaps one frame's readback with the next frame's GPU work). On a software
//! adapter both are far below 60 fps; on a GPU the synchronous number is
//! dominated by the readback stall and the pipelined one by the actual work.

use std::time::Instant;

use crate::compose::PipSpec;
use crate::media_types::VideoFrame;

use super::testkit::{self, cam, screen};
use super::{Options, OutputFormat};

/// Frames per case: enough to warm the driver up and still take seconds.
const FRAMES: usize = 24;

struct Case {
    name: &'static str,
    width: usize,
    height: usize,
    screen: (usize, usize),
    cam: Option<(usize, usize)>,
    output: OutputFormat,
}

#[test]
#[ignore = "a measurement, not a check"]
fn performance() {
    let Some(adapter) = testkit::gpu() else {
        return;
    };
    println!("\ncompositor performance on: {adapter}\n");
    println!(
        "{:<44} {:>10} {:>10} {:>8}",
        "case", "sync ms", "piped ms", "frames"
    );
    println!("{}", "-".repeat(76));
    for case in cases() {
        let (sync, piped, frames) = measure(&case);
        println!(
            "{:<44} {:>10.2} {:>10.2} {:>8}",
            case.name, sync, piped, frames
        );
    }
    println!();
    // The copy out of the staging buffers and the uploads are the software
    // adapter's ceiling; report what one readback of a full frame costs on its
    // own so the bottleneck is named.
    for (w, h) in [(1920, 1080), (3840, 2160)] {
        let Some(r) = testkit::build(&Options {
            width: w,
            height: h,
            output: OutputFormat::Bgra,
            pipeline_depth: 1,
            ..Default::default()
        }) else {
            return;
        };
        let frame: VideoFrame = testkit::solid(w, h, [10, 20, 30]);
        let mut r = r;
        let start = Instant::now();
        let _ = r.render_now(Some(&frame), None, 0.0).expect("render");
        println!(
            "one {w}x{h} frame, no scaling, sync: {:.2} ms",
            start.elapsed().as_secs_f64() * 1000.0
        );
    }
    println!();
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "1080p out, 1080p screen",
            width: 1920,
            height: 1080,
            screen: (1920, 1080),
            cam: None,
            output: OutputFormat::Bgra,
        },
        Case {
            name: "1080p out, 4K screen (Lanczos 2:1)",
            width: 1920,
            height: 1080,
            screen: (3840, 2160),
            cam: None,
            output: OutputFormat::Bgra,
        },
        Case {
            name: "1080p out, 4K screen + 720p cam, NV12",
            width: 1920,
            height: 1080,
            screen: (3840, 2160),
            cam: Some((1280, 720)),
            output: OutputFormat::Nv12,
        },
        Case {
            name: "4K out, 4K screen",
            width: 3840,
            height: 2160,
            screen: (3840, 2160),
            cam: None,
            output: OutputFormat::Bgra,
        },
        Case {
            name: "4K out, 4K screen + 720p cam, NV12",
            width: 3840,
            height: 2160,
            screen: (3840, 2160),
            cam: Some((1280, 720)),
            output: OutputFormat::Nv12,
        },
        Case {
            name: "1080p out, 5K screen (Lanczos 2.67:1)",
            width: 1920,
            height: 1080,
            screen: (5120, 2880),
            cam: None,
            output: OutputFormat::Bgra,
        },
    ]
}

/// Milliseconds per frame for one case, synchronous and pipelined, plus how
/// many frames each run actually read back.
fn measure(case: &Case) -> (f64, f64, usize) {
    let src = screen(case.screen.0, case.screen.1);
    let camera = case.cam.map(|(w, h)| cam(w, h));
    let scene = if case.cam.is_some() {
        crate::compose::Scene::ScreenCam
    } else {
        crate::compose::Scene::Screen
    };
    let make = |depth: usize| {
        testkit::build(&Options {
            width: case.width,
            height: case.height,
            scene,
            pip: PipSpec::default(),
            output: case.output,
            corner_fill: false,
            pipeline_depth: depth,
        })
        .expect("a renderer for the measurement")
    };
    // One warm-up frame so the pipelines and textures exist.
    let mut r = make(1);
    let _ = r
        .render_now(Some(&src), camera.as_ref(), 0.0)
        .expect("warm up");

    // Synchronous: submit and wait for every frame.
    let mut sync_frames = 0;
    let start = Instant::now();
    for i in 0..FRAMES {
        let out = r
            .render(Some(&src), camera.as_ref(), i as f64 * 0.033)
            .expect("render");
        sync_frames += usize::from(out.is_some());
    }
    sync_frames += r.flush().expect("flush").len();
    let sync = start.elapsed().as_secs_f64() * 1000.0 / sync_frames.max(1) as f64;

    // Pipelined: one frame in flight, so a readback overlaps the next frame's
    // submission.
    let mut r = make(2);
    let _ = r
        .render_now(Some(&src), camera.as_ref(), 0.0)
        .expect("warm up");
    let mut piped_frames = 0;
    let start = Instant::now();
    for i in 0..FRAMES {
        let out = r
            .render(Some(&src), camera.as_ref(), i as f64 * 0.033)
            .expect("render");
        piped_frames += usize::from(out.is_some());
    }
    piped_frames += r.flush().expect("flush").len();
    let piped = start.elapsed().as_secs_f64() * 1000.0 / piped_frames.max(1) as f64;
    (sync, piped, sync_frames)
}
