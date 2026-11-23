//! V4L2 camera devices: enumeration, resolution and the capture loop.
//!
//! `cameras()` opens every `/dev/video*` node and keeps the ones that can
//! capture and stream video (a UVC camera exposes a second, metadata-only node
//! beside the picture one). `open` negotiates [`super::format`] on the chosen
//! device, returns the source with its native size (so the compositor can lay
//! the overlay out before the first frame), and the source's thread then reads
//! memory-mapped buffers and converts them to BGRA.
//!
//! A capture stream's `next()` blocks in the kernel, so the loop sets a
//! 500 ms timeout and rebuilds the stream when it expires: that keeps `stop`
//! bounded even on a camera that stops delivering, and five empty timeouts in a
//! row are reported as a dead camera. The driver's buffer timestamp becomes the
//! frame's `pts` after [`CameraClock`] rejects other clocks and repairs
//! non-monotonic ones.
//!
//! Adapted from rec-rs src/camera.rs (MIT OR Apache-2.0), where AVCapture did
//! the enumeration and the pixel conversion for us.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use v4l::buffer::Type as BufferType;
use v4l::capability::Flags as CapFlags;
use v4l::format::Colorspace;
use v4l::frameinterval::FrameIntervalEnum;
use v4l::framesize::{FrameSize, FrameSizeEnum};
use v4l::io::traits::CaptureStream;
use v4l::prelude::MmapStream;
use v4l::video::Capture;
use v4l::{Format, FourCC};

use super::convert::{CameraClock, Converter};
use super::format::{self, Candidate, Choice, Encoding};
use crate::media_types::{FrameSink, PixelFormat, VideoFrame};

/// A camera node.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Device {
    pub path: PathBuf,
    pub name: String,
}

impl Device {
    pub fn id(&self) -> String {
        self.path.to_string_lossy().to_string()
    }
}

/// One negotiated V4L2 format.
#[derive(Clone, Copy, Debug)]
pub(super) struct Negotiated {
    pub fourcc: FourCC,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub colorspace: Colorspace,
    pub fps: u32,
}

impl Negotiated {
    pub fn encoding(&self) -> Encoding {
        Encoding::from_fourcc(self.fourcc).expect("negotiate only accepts supported formats")
    }

    pub fn label(&self) -> String {
        format!(
            "{}x{} {} @ {} fps",
            self.width,
            self.height,
            self.encoding().label(),
            self.fps
        )
    }
}

/// Every node that can capture video, in /dev/videoN order.
pub(super) fn devices() -> Vec<Device> {
    let mut nodes: Vec<(usize, Device)> = Vec::new();
    for node in v4l::context::enum_devices() {
        let Some(index) = video_index(node.path()) else {
            continue;
        };
        let Ok(dev) = v4l::Device::with_path(node.path()) else {
            continue;
        };
        let Ok(caps) = dev.query_caps() else {
            continue;
        };
        // A UVC camera's metadata node lacks VIDEO_CAPTURE; some drivers expose
        // capture without STREAMING (read() only), which the mmap loop cannot
        // use.
        if !caps.capabilities.contains(CapFlags::VIDEO_CAPTURE)
            || !caps.capabilities.contains(CapFlags::STREAMING)
        {
            continue;
        }
        let name = if caps.card.trim().is_empty() {
            node.name()
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| node.path().display().to_string())
        } else {
            caps.card.trim().to_string()
        };
        nodes.push((
            index,
            Device {
                path: node.path().to_path_buf(),
                name,
            },
        ));
    }
    nodes.sort_by_key(|(i, _)| *i);
    nodes.into_iter().map(|(_, d)| d).collect()
}

/// The `/dev/videoN` number of a path.
fn video_index(path: &Path) -> Option<usize> {
    path.file_name()?
        .to_str()?
        .strip_prefix("video")?
        .parse()
        .ok()
}

/// Resolve `--webcam` against the connected cameras: exact id, a video number,
/// the saved name (exact then substring), or the first camera for the default.
/// None when the requested one is not connected.
pub(super) fn resolve<'a>(devices: &'a [Device], sel: Option<&str>) -> Option<&'a Device> {
    let sel = sel
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "default");
    let Some(sel) = sel else {
        return devices.first();
    };
    if let Some(d) = devices
        .iter()
        .find(|d| d.path == Path::new(sel) || d.id() == sel)
    {
        return Some(d);
    }
    if let Some(n) = sel
        .strip_prefix("/dev/video")
        .or_else(|| sel.strip_prefix("video"))
        .and_then(|n| n.parse::<usize>().ok())
    {
        return devices.iter().find(|d| video_index(&d.path) == Some(n));
    }
    if let Ok(n) = sel.parse::<usize>()
        && let Some(d) = devices.iter().find(|d| video_index(&d.path) == Some(n))
    {
        return Some(d);
    }
    let lower = sel.to_lowercase();
    if let Some(d) = devices.iter().find(|d| d.name.to_lowercase() == lower) {
        return Some(d);
    }
    devices
        .iter()
        .find(|d| d.name.to_lowercase().contains(&lower))
}

/// Ask the driver what it offers as [`Candidate`]s: one per format and frame
/// size, with the fastest frame rate the size supports.
fn candidates(dev: &v4l::Device) -> Result<Vec<Candidate>> {
    let mut out = Vec::new();
    for desc in dev.enum_formats().context("enumerating camera formats")? {
        // libv4lconvert's emulated formats cost a conversion we do ourselves.
        if desc
            .flags
            .contains(v4l::format::description::Flags::EMULATED)
        {
            continue;
        }
        if Encoding::from_fourcc(desc.fourcc).is_none() {
            continue;
        }
        let Ok(sizes) = dev.enum_framesizes(desc.fourcc) else {
            continue;
        };
        for (width, height) in sizes.iter().flat_map(discrete_sizes) {
            let Some(max_fps) = max_fps(dev, desc.fourcc, width, height) else {
                continue;
            };
            out.push(Candidate {
                fourcc: desc.fourcc,
                width,
                height,
                max_fps,
            });
        }
    }
    Ok(out)
}

/// A discrete frame size list, including a bounded sample of a stepwise range
/// (never all 16 million sizes a continuous range implies).
fn discrete_sizes(size: &FrameSize) -> Vec<(u32, u32)> {
    match &size.size {
        FrameSizeEnum::Discrete(d) => vec![(d.width, d.height)],
        FrameSizeEnum::Stepwise(s) => {
            let mut wanted = vec![
                (1920, 1080),
                (1600, 900),
                (1280, 720),
                (1024, 768),
                (800, 600),
                (640, 480),
                (320, 240),
            ];
            // The largest size the mode allows, clipped to the recorder's range.
            wanted.push((
                s.max_width.min(format::MAX_WIDTH) & !1,
                s.max_height.min(format::MAX_HEIGHT) & !1,
            ));
            let mut out = Vec::new();
            let mut seen: Vec<(u32, u32)> = Vec::new();
            for (w, h) in wanted {
                if w < s.min_width
                    || w > s.max_width
                    || h < s.min_height
                    || h > s.max_height
                    || (s.step_width != 0 && (w - s.min_width) % s.step_width != 0)
                    || (s.step_height != 0 && (h - s.min_height) % s.step_height != 0)
                    || seen.contains(&(w, h))
                {
                    continue;
                }
                seen.push((w, h));
                out.push((w, h));
            }
            out
        }
    }
}

/// The fastest frame rate (frames per second) a format and size supports.
fn max_fps(dev: &v4l::Device, fourcc: FourCC, width: u32, height: u32) -> Option<f64> {
    let intervals = dev.enum_frameintervals(fourcc, width, height).ok()?;
    intervals
        .iter()
        .map(|i| match &i.interval {
            FrameIntervalEnum::Discrete(f) => fps_of(*f),
            // The fastest rate is the shortest interval.
            FrameIntervalEnum::Stepwise(s) => fps_of(s.min),
        })
        .fold(None, |best: Option<f64>, fps| {
            Some(best.map_or(fps, |b| b.max(fps)))
        })
        .filter(|fps| fps.is_finite() && *fps > 0.0)
}

/// Fraction of seconds per frame to frames per second.
fn fps_of(f: v4l::fraction::Fraction) -> f64 {
    if f.numerator == 0 {
        return 0.0;
    }
    f64::from(f.denominator) / f64::from(f.numerator)
}

/// Negotiate the format to capture with. `None` when the camera offers nothing
/// the recorder can use (the error from [`open`] lists what it does offer).
pub(super) fn negotiate(dev: &v4l::Device) -> Result<Option<(Choice, Negotiated)>> {
    let candidates = candidates(dev)?;
    let Some(choice) = format::choose(&candidates) else {
        return Ok(None);
    };
    let negotiated = apply_format(dev, &choice, &candidates)?;
    Ok(Some((choice, negotiated)))
}

/// Set the chosen format and frame rate; the driver may adjust both, so the
/// actual format is read back and used from then on.
fn apply_format(
    dev: &v4l::Device,
    choice: &Choice,
    candidates: &[Candidate],
) -> Result<Negotiated> {
    let fmt = Format::new(choice.width, choice.height, choice.fourcc);
    let actual = dev
        .set_format(&fmt)
        .with_context(|| format!("setting {} on the camera", choice.label()))?;
    if Encoding::from_fourcc(actual.fourcc).is_none() {
        bail!(
            "the camera changed {} to {}, which this build cannot convert",
            choice.fourcc,
            actual.fourcc
        );
    }
    if actual.width == 0 || actual.height == 0 || actual.width % 2 != 0 || actual.height % 2 != 0 {
        bail!(
            "the camera chose an unusable size {}x{}",
            actual.width,
            actual.height
        );
    }
    if actual.width > format::MAX_WIDTH || actual.height > format::MAX_HEIGHT {
        bail!(
            "the camera chose {}x{}, larger than the {}x{} the recorder asked for",
            actual.width,
            actual.height,
            format::MAX_WIDTH,
            format::MAX_HEIGHT
        );
    }
    let mut fps = set_fps(dev, choice.fps)?;
    if fps + 0.5 < format::MIN_FPS {
        // A camera that cannot do 30 at this size: use its fastest supported
        // rate instead of refusing to record.
        let fastest = candidates
            .iter()
            .filter(|c| {
                c.fourcc == actual.fourcc && c.width == actual.width && c.height == actual.height
            })
            .map(|c| c.max_fps)
            .fold(0.0f64, f64::max);
        if fastest >= 1.0 {
            fps = set_fps(dev, fastest.round().clamp(1.0, 240.0) as u32)?;
        }
    }
    Ok(Negotiated {
        fourcc: actual.fourcc,
        width: actual.width,
        height: actual.height,
        stride: actual.stride,
        colorspace: actual.colorspace,
        fps: if fps > 0.0 {
            fps.round() as u32
        } else {
            choice.fps
        },
    })
}

/// Set the frame interval and read back the rate the driver accepted.
fn set_fps(dev: &v4l::Device, fps: u32) -> Result<f64> {
    let params = dev
        .set_params(&v4l::video::capture::Parameters::with_fps(
            fps.clamp(1, 240),
        ))
        .context("setting the camera frame rate")?;
    Ok(fps_of(params.interval))
}

/// Open the device and negotiate its format, without starting the stream.
pub(super) fn probe(path: &Path) -> Result<(Choice, Negotiated)> {
    let dev = v4l::Device::with_path(path)
        .with_context(|| format!("opening camera {}", path.display()))?;
    let candidates = candidates(&dev)?;
    if candidates.is_empty() {
        bail!(
            "camera {} offers no YUYV, NV12 or MJPEG format (see `v4l2-ctl -d {} --list-formats-ext`)",
            path.display(),
            path.display()
        );
    }
    match negotiate(&dev)? {
        Some(v) => Ok(v),
        None => {
            let offered: Vec<String> = candidates
                .iter()
                .filter(|c| {
                    c.width <= format::MAX_WIDTH
                        && c.height <= format::MAX_HEIGHT
                        && c.width % 2 == 0
                        && c.height % 2 == 0
                })
                .map(|c| {
                    format!(
                        "{}x{} {} @ {:.0}",
                        c.width,
                        c.height,
                        c.encoding().label(),
                        c.max_fps
                    )
                })
                .collect();
            bail!(
                "camera {} has no frame up to {}x{} at {} fps (it offers: {})",
                path.display(),
                format::MAX_WIDTH,
                format::MAX_HEIGHT,
                format::MIN_FPS,
                if offered.is_empty() {
                    "nothing".to_string()
                } else {
                    offered.join(", ")
                }
            )
        }
    }
}

/// The size the first frame turned out to be, shared with the source.
pub(super) type FrameSizeCell = Arc<Mutex<(usize, usize)>>;

/// The capture thread: open, stream, convert and deliver until `stop`.
pub(super) fn capture_thread(
    path: PathBuf,
    negotiated: Negotiated,
    sink: FrameSink,
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    ready: mpsc::Sender<Result<()>>,
    size: FrameSizeCell,
) {
    let result = run_capture(&path, &negotiated, &sink, &stop, &ready, &size);
    if let Err(e) = result {
        let msg = format!("{e:#}");
        if let Ok(mut slot) = error.lock()
            && slot.is_none()
        {
            *slot = Some(msg.clone());
        }
        // Ignored once the source reported ready; the error is picked up by
        // `stop`.
        let _ = ready.send(Err(anyhow!("{msg}")));
    }
}

fn run_capture(
    path: &Path,
    negotiated: &Negotiated,
    sink: &FrameSink,
    stop: &AtomicBool,
    ready: &mpsc::Sender<Result<()>>,
    size: &FrameSizeCell,
) -> Result<()> {
    let dev = v4l::Device::with_path(path)
        .with_context(|| format!("opening camera {}", path.display()))?;
    // Re-apply the format `probe` negotiated: the device is closed between the
    // two opens, so nothing else may have changed it in the meantime.
    let fmt = Format::new(negotiated.width, negotiated.height, negotiated.fourcc);
    let actual = dev
        .set_format(&fmt)
        .with_context(|| format!("setting the format on camera {}", path.display()))?;
    let _ = set_fps(&dev, negotiated.fps)?;
    let mut converter = Converter::new(
        actual.fourcc,
        actual.width as usize,
        actual.height as usize,
        actual.stride as usize,
        actual.colorspace,
    )?;
    let mut clock = CameraClock::default();
    let mut stream = new_stream(&dev)?;
    let mut ready_sent = false;
    let mut timeouts = 0u32;
    while !stop.load(Ordering::SeqCst) {
        match stream.next() {
            Ok((data, meta)) => {
                timeouts = 0;
                // MJPEG's valid bytes are bytesused; YUYV/NV12 fill the buffer.
                let used = (meta.bytesused as usize).min(data.len());
                let src = if meta.bytesused > 0 && used > 0 {
                    &data[..used]
                } else {
                    data
                };
                match converter.convert(src) {
                    Ok(frame) => {
                        let pts = clock.stamp(meta.timestamp.sec, meta.timestamp.usec);
                        if !ready_sent {
                            if let Ok(mut s) = size.lock() {
                                *s = (frame.width, frame.height);
                            }
                            ready_sent = true;
                            let _ = ready.send(Ok(()));
                        }
                        let frame = VideoFrame {
                            pts,
                            width: frame.width,
                            height: frame.height,
                            format: PixelFormat::Bgra,
                            planes: vec![crate::media_types::Plane::new(
                                frame.data,
                                frame.width * 4,
                            )],
                        };
                        debug_assert!(frame.is_well_formed());
                        sink(frame);
                    }
                    Err(e) => {
                        // One corrupt MJPEG frame must not end the camera.
                        if std::env::var_os("REC_DEBUG").is_some() {
                            eprintln!("rec: debug: camera {path:?} frame skipped: {e:#}");
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                timeouts += 1;
                if timeouts >= 5 {
                    bail!(
                        "camera {} stopped delivering frames for {:.1} s",
                        path.display(),
                        (timeouts as f64) * 0.5
                    );
                }
                // `next()` leaves the queue mid-way after a timeout, so a fresh
                // stream is the reliable way back.
                drop(stream);
                stream = new_stream(&dev)?;
            }
            Err(e) => {
                bail!("camera {}: {e}", path.display());
            }
        }
    }
    Ok(())
}

fn new_stream(dev: &v4l::Device) -> Result<MmapStream<'_>> {
    let mut stream = MmapStream::with_buffers(dev, BufferType::VideoCapture, 4)
        .context("starting the camera stream")?;
    stream.set_timeout(Duration::from_millis(500));
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(path: &str, name: &str) -> Device {
        Device {
            path: PathBuf::from(path),
            name: name.into(),
        }
    }

    /// Upstream `camera.rs::device` matching, with /dev paths.
    #[test]
    fn device_matching() {
        let cameras = [
            dev("/dev/video0", "Integrated Camera"),
            dev("/dev/video2", "Logitech BRIO Ultra HD"),
        ];
        assert_eq!(resolve(&cameras, None).unwrap().name, "Integrated Camera");
        assert_eq!(
            resolve(&cameras, Some("default")).unwrap().name,
            "Integrated Camera"
        );
        assert_eq!(
            resolve(&cameras, Some("/dev/video2")).unwrap().name,
            "Logitech BRIO Ultra HD"
        );
        assert_eq!(
            resolve(&cameras, Some("video2")).unwrap().name,
            "Logitech BRIO Ultra HD"
        );
        assert_eq!(
            resolve(&cameras, Some("2")).unwrap().name,
            "Logitech BRIO Ultra HD"
        );
        assert_eq!(
            resolve(&cameras, Some("brio")).unwrap().name,
            "Logitech BRIO Ultra HD"
        );
        assert_eq!(
            resolve(&cameras, Some("Logitech BRIO Ultra HD"))
                .unwrap()
                .name,
            "Logitech BRIO Ultra HD"
        );
        assert_eq!(resolve(&cameras, Some("no such camera")), None);
        assert_eq!(resolve(&[], None), None);
        assert_eq!(resolve(&[], Some("anything")), None);
    }

    #[test]
    fn video_indices_sort_numerically() {
        assert_eq!(video_index(Path::new("/dev/video0")), Some(0));
        assert_eq!(video_index(Path::new("/dev/video12")), Some(12));
        assert_eq!(video_index(Path::new("/dev/media0")), None);
        assert_eq!(video_index(Path::new("/dev/video")), None);
    }

    #[test]
    fn frame_intervals_become_frame_rates() {
        assert_eq!(fps_of(v4l::fraction::Fraction::new(1, 30)), 30.0);
        assert_eq!(fps_of(v4l::fraction::Fraction::new(1, 60)), 60.0);
        assert!((fps_of(v4l::fraction::Fraction::new(1001, 30_000)) - 29.97).abs() < 0.01);
        assert_eq!(fps_of(v4l::fraction::Fraction::new(0, 30)), 0.0);
    }

    #[test]
    fn enumerate_devices_never_panics_without_a_camera() {
        // The host has no webcam; this must simply be empty (or list one).
        let devices = devices();
        for d in &devices {
            assert!(d.path.starts_with("/dev/video"), "{}", d.path.display());
            assert!(!d.name.is_empty());
            assert!(video_index(&d.path).is_some());
        }
    }
}
