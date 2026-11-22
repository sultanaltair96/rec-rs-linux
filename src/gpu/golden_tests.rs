//! Golden-image tests: each scene and overlay shape rendered at 1080p from a
//! synthetic screen (a test card) and a synthetic camera (a gradient with a
//! marker), compared against committed PNGs under `tests/golden/`.
//!
//! The comparison has to pass on Mesa's software Vulkan adapter *and* on a real
//! GPU, so it allows a small tolerance and a small mean error: the two agree to
//! within a code or two on the same shaders, and a stricter test would fail on
//! whichever hardware the goldens were not made on.
//!
//! Regenerate them (only when the picture is *meant* to change) with
//!
//! ```sh
//! scripts/dev.sh env REC_GPU_UPDATE_GOLDEN=1 cargo test goldens
//! ```
//!
//! and look at the diff before committing.

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;

use crate::compose::{Corner, PipSpec, Place, Scene, Shape};

use super::testkit::{self, Image, cam, screen};
use super::{Options, OutputFormat};

/// The largest per-channel difference a golden accepts.
const MAX_DIFF: u8 = 8;
/// The largest mean per-channel difference a golden accepts.
const MAX_MEAN: f64 = 0.75;

type Frames = Box<
    dyn Fn() -> (
        Option<crate::media_types::VideoFrame>,
        Option<crate::media_types::VideoFrame>,
    ),
>;

fn at(name: &str) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/golden");
    path.push(format!("{name}.png"));
    path
}

const HD: (usize, usize) = (1920, 1080);

fn goldens() -> Vec<(&'static str, Options, Frames)> {
    let opts = |scene, pip: PipSpec| Options {
        width: HD.0,
        height: HD.1,
        scene,
        pip,
        output: OutputFormat::Bgra,
        corner_fill: false,
        pipeline_depth: 1,
    };
    let card = || Some(screen(HD.0, HD.1));
    let camera = || Some(cam(1280, 720));
    let default = PipSpec::default();
    vec![
        (
            "screen",
            opts(Scene::Screen, default.clone()),
            Box::new(move || (card(), None)) as Frames,
        ),
        (
            "screen-4k-to-1080",
            opts(Scene::Screen, default.clone()),
            Box::new(move || (Some(screen(3840, 2160)), None)) as Frames,
        ),
        (
            "screen-cam-rounded",
            opts(Scene::ScreenCam, default.clone()),
            Box::new(move || (card(), camera())) as Frames,
        ),
        (
            "screen-cam-circle",
            opts(
                Scene::ScreenCam,
                PipSpec {
                    shape: Shape::Circle,
                    place: Place::Corner(Corner::TopLeft),
                    ..default.clone()
                },
            ),
            Box::new(move || (card(), camera())) as Frames,
        ),
        (
            "screen-cam-square-border",
            opts(
                Scene::ScreenCam,
                PipSpec {
                    shape: Shape::Square,
                    roundness: 0.15,
                    border: 6.0,
                    border_color: [255, 200, 60],
                    place: Place::At([0.5, 0.5]),
                    ..default.clone()
                },
            ),
            Box::new(move || (card(), camera())) as Frames,
        ),
        (
            "screen-cam-pill",
            opts(
                Scene::ScreenCam,
                PipSpec {
                    shape: Shape::Pill,
                    place: Place::Corner(Corner::BottomLeft),
                    ..default.clone()
                },
            ),
            Box::new(move || (card(), camera())) as Frames,
        ),
        (
            "screen-cam-squircle-shadow",
            opts(
                Scene::ScreenCam,
                PipSpec {
                    shape: Shape::Squircle,
                    roundness: 0.5,
                    shadow: true,
                    border: 3.0,
                    border_color: [255, 255, 255],
                    place: Place::Corner(Corner::BottomRight),
                    ..default.clone()
                },
            ),
            Box::new(move || (card(), camera())) as Frames,
        ),
        (
            "cam",
            opts(Scene::Cam, default),
            Box::new(move || (None, camera())) as Frames,
        ),
    ]
}

fn write_png(path: &PathBuf, image: &Image) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = File::create(path)?;
    let mut encoder = png::Encoder::new(
        BufWriter::new(file),
        image.width as u32,
        image.height as u32,
    );
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut bytes = Vec::with_capacity(image.rgb.len() * 3);
    for px in &image.rgb {
        bytes.extend_from_slice(px);
    }
    writer
        .write_image_data(&bytes)
        .map_err(|e| std::io::Error::other(e.to_string()))
}

fn read_png(path: &PathBuf) -> Result<Image, String> {
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| format!("{}: no buffer size", path.display()))?;
    let mut buf = vec![0; size];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if info.color_type != png::ColorType::Rgb || info.bit_depth != png::BitDepth::Eight {
        return Err(format!(
            "{}: expected 8-bit RGB, found {:?}/{:?}",
            path.display(),
            info.color_type,
            info.bit_depth
        ));
    }
    let rgb = buf[..size]
        .chunks_exact(3)
        .map(|c| [c[0], c[1], c[2]])
        .collect();
    Ok(Image {
        width: info.width as usize,
        height: info.height as usize,
        rgb,
    })
}

#[test]
fn goldens_match_the_committed_images() {
    let update = std::env::var("REC_GPU_UPDATE_GOLDEN").as_deref() == Ok("1");
    let Some(adapter) = testkit::gpu() else {
        return;
    };
    eprintln!("goldens rendered on: {adapter}");
    let mut failures: Vec<String> = Vec::new();
    for (name, opts, make) in goldens() {
        let Some(mut r) = testkit::build(&opts) else {
            return;
        };
        let (screen, cam) = make();
        let out = r
            .render_now(screen.as_ref(), cam.as_ref(), 0.0)
            .expect("render the golden case");
        let got = Image::bgra(&out);
        let path = at(name);
        if update {
            write_png(&path, &got).expect("write the golden");
            eprintln!("wrote {} ({}x{})", path.display(), got.width, got.height);
            continue;
        }
        let want = match read_png(&path) {
            Ok(image) => image,
            Err(e) => {
                failures.push(format!("{name}: {e}"));
                continue;
            }
        };
        if (want.width, want.height) != (got.width, got.height) {
            failures.push(format!(
                "{name}: the golden is {}x{}, the render is {}x{}",
                want.width, want.height, got.width, got.height
            ));
            continue;
        }
        let (max, mean) = got.diff(&want);
        if max > MAX_DIFF || mean > MAX_MEAN {
            failures.push(format!(
                "{name}: worst channel differs by {max} (allowed {MAX_DIFF}), mean {mean:.3} (allowed {MAX_MEAN})"
            ));
        } else {
            eprintln!("{name}: ok (max {max}, mean {mean:.3})");
        }
    }
    assert!(
        failures.is_empty(),
        "golden images differ:\n  {}\n\nIf the change is intended, regenerate them with\n  \
         scripts/dev.sh env REC_GPU_UPDATE_GOLDEN=1 cargo test goldens\nand look at the diff.",
        failures.join("\n  ")
    );
}

/// A missing golden is a failure, not a silent pass.
#[test]
fn every_golden_exists() {
    if std::env::var("REC_GPU_UPDATE_GOLDEN").as_deref() == Ok("1") {
        return;
    }
    let missing: Vec<&str> = goldens()
        .into_iter()
        .filter(|(name, _, _)| !at(name).exists())
        .map(|(name, _, _)| name)
        .collect();
    assert!(
        missing.is_empty(),
        "missing golden images: {missing:?} (run `scripts/dev.sh env REC_GPU_UPDATE_GOLDEN=1 cargo test goldens`)"
    );
}
