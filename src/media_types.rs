//! The contract between capture sources, the composer, the writer and the live
//! path.
//!
//! Everything that crosses a module boundary is defined here: frames, audio,
//! packets, the source traits and the device descriptions `devices` prints.
//! This file changes only with the parent thread's approval, so five phase 1
//! workers can build against it in parallel without touching each other.
//!
//! # Clock
//!
//! Every timestamp is **seconds on `CLOCK_MONOTONIC`** (`f64`), the one host
//! clock every source is stamped on. PipeWire, GStreamer, V4L2 buffers and the
//! synthetic sources all use that clock (directly or converted once at the
//! source), exactly like upstream: video and audio from different devices are
//! therefore comparable sample by sample, and the writer never resynchronises
//! by drift, only by the timestamps it is given.
//!
//! `now()` is that clock. It is not wall-clock time: it cannot be compared with
//! `SystemTime`, and it must not be persisted.
//!
//! # Delivery: callbacks, not channels
//!
//! A source delivers through the callback it is started with:
//! [`FrameSink`] for video, [`AudioSink`] for audio. The callback runs on the
//! capture thread and must return quickly; it may be called from any thread
//! and from several threads at once.
//!
//! A callback rather than a channel, because a recorder is a real-time sink:
//!
//! - When the encoder falls behind, the right answer is to *drop* the oldest
//!   frame and keep the newest, not to buffer. The writer (and the live encode
//!   in phase 1D) already holds a small bounded queue and applies that policy;
//!   a channel would only add a second, unbounded queue in front of it and grow
//!   memory (a 4K frame is 33 MB) whenever the consumer stalls.
//! - It keeps backpressure explicit. A source that blocks on a full queue
//!   stops capturing, which is a capture bug that shows up as latency; a
//!   callback that pushes into a bounded queue and drops is what screen and
//!   audio capture actually want.
//! - It carries no lifetime puzzle for the sink. The sink is an `Arc` the
//!   source clones; the writer holds the other end. No channel must be kept
//!   alive, and stopping a source cannot leave a receiver hanging on `recv`.
//!
//! The cost is that a misbehaving sink blocks a capture thread; the writer
//! keeps its work per call to a queue push and, for audio, at most one buffer
//! of silence.
//!
//! # Frames on the CPU
//!
//! [`VideoFrame`] is CPU memory in a [`PixelFormat`] every consumer can parse.
//! A later GPU path (phase 1C) adds a dmabuf variant to [`PixelFormat`]; the
//! enum is `#[non_exhaustive]` so that does not break out-of-crate callers,
//! and sources keep offering CPU frames until the compositor can take the GPU
//! ones.
//!
//! Adapted from rec-rs src/writer.rs, src/synth.rs and src/screen.rs
//! (MIT OR Apache-2.0), with the CoreMedia types replaced.

// This module is the API surface of phases 1A-1D by design: phase 0 uses the
// video, audio and source parts, and the packets, device descriptions and
// helpers land with the workers that need them.
#![allow(dead_code)]

use std::sync::Arc;

use anyhow::Result;

/// Seconds on `CLOCK_MONOTONIC`, the single host clock of the whole program.
///
/// PipeWire timestamps are on this clock too, so a PipeWire buffer's timestamp
/// can be used without conversion.
pub fn now() -> f64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // CLOCK_MONOTONIC never fails for a valid pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1_000_000_000.0
}

/// How a [`VideoFrame`]'s planes are laid out.
///
/// The four formats the pipeline needs today. `#[non_exhaustive]`: a `Dmabuf`
/// variant (planes that carry a file descriptor instead of bytes) joins them
/// in phase 1C, and callers outside this crate keep compiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PixelFormat {
    /// 32-bit BGRA, one plane. What the compositor and the software encoder
    /// path consume.
    Bgra,
    /// 32-bit RGBA, one plane.
    Rgba,
    /// 8-bit NV12 (a full-size Y plane, then interleaved UV at half size).
    Nv12,
    /// 8-bit I420 (Y, then U, then V, each chroma plane at half size).
    I420,
}

impl PixelFormat {
    /// Bytes per pixel for packed formats, `None` for planar 4:2:0 ones.
    pub fn bytes_per_pixel(self) -> Option<usize> {
        match self {
            PixelFormat::Bgra | PixelFormat::Rgba => Some(4),
            PixelFormat::Nv12 | PixelFormat::I420 => None,
        }
    }

    /// The plane lengths for a `width`x`height` frame with tightly packed
    /// rows. Strides may be larger; this is what a source must at least fill.
    pub fn plane_sizes(self, width: usize, height: usize) -> Vec<usize> {
        let (w, h) = (width, height);
        match self {
            PixelFormat::Bgra | PixelFormat::Rgba => vec![w * h * 4],
            PixelFormat::Nv12 => vec![w * h, w * (h.div_ceil(2))],
            PixelFormat::I420 => vec![
                w * h,
                w.div_ceil(2) * h.div_ceil(2),
                w.div_ceil(2) * h.div_ceil(2),
            ],
        }
    }

    /// Number of planes a frame in this format has.
    pub fn planes(self) -> usize {
        match self {
            PixelFormat::Bgra | PixelFormat::Rgba => 1,
            PixelFormat::Nv12 => 2,
            PixelFormat::I420 => 3,
        }
    }

    /// Rows in plane `index` for a frame `height` pixels tall (chroma planes
    /// are half height, rounded up).
    pub fn plane_rows(self, index: usize, height: usize) -> usize {
        match self {
            PixelFormat::Bgra | PixelFormat::Rgba | PixelFormat::Nv12 => {
                if index == 0 {
                    height
                } else {
                    height.div_ceil(2)
                }
            }
            PixelFormat::I420 => {
                if index == 0 {
                    height
                } else {
                    height.div_ceil(2)
                }
            }
        }
    }

    /// The smallest stride plane `index` can have for `width` pixels.
    pub fn min_stride(self, index: usize, width: usize) -> usize {
        match self {
            PixelFormat::Bgra | PixelFormat::Rgba => width * 4,
            PixelFormat::Nv12 => width,
            PixelFormat::I420 => {
                if index == 0 {
                    width
                } else {
                    width.div_ceil(2)
                }
            }
        }
    }

    /// One line of text for a status line or a log.
    pub fn label(self) -> &'static str {
        match self {
            PixelFormat::Bgra => "BGRA",
            PixelFormat::Rgba => "RGBA",
            PixelFormat::Nv12 => "NV12",
            PixelFormat::I420 => "I420",
        }
    }
}

/// One plane of a [`VideoFrame`]: `stride` bytes per row, `height` rows of
/// pixels. The data is immutable and shared (`Arc`), so handing a frame to the
/// writer and the preview costs nothing.
#[derive(Clone, Debug)]
pub struct Plane {
    /// Row-major pixel bytes, at least `stride * rows`.
    pub data: Arc<[u8]>,
    /// Bytes per row, including any padding the device adds.
    pub stride: usize,
}

impl Plane {
    /// A plane of `height` rows of `stride` bytes.
    pub fn new(data: impl Into<Arc<[u8]>>, stride: usize) -> Self {
        Plane {
            data: data.into(),
            stride,
        }
    }

    /// Rows this plane can hold without running past its data.
    pub fn rows(&self) -> usize {
        self.data.len().checked_div(self.stride).unwrap_or(0)
    }
}

/// A CPU video frame on the host clock.
///
/// `pts` is the instant the pixels were captured (or, for a synthetic source,
/// the instant they represent): [`now`] seconds. The writer places frames on
/// its own constant-frame-rate grid from these timestamps.
#[derive(Clone, Debug)]
pub struct VideoFrame {
    /// Capture time, seconds on [`now`]'s clock.
    pub pts: f64,
    pub width: usize,
    pub height: usize,
    pub format: PixelFormat,
    /// Planes in the format's order.
    pub planes: Vec<Plane>,
}

impl VideoFrame {
    /// A one-plane packed frame.
    pub fn packed(
        pts: f64,
        width: usize,
        height: usize,
        format: PixelFormat,
        data: Vec<u8>,
    ) -> Self {
        let stride = width * format.bytes_per_pixel().unwrap_or(4);
        VideoFrame {
            pts,
            width,
            height,
            format,
            planes: vec![Plane::new(data, stride)],
        }
    }

    /// True if the plane layout matches `format` and the frame's size: every
    /// source validates its output in debug builds, and the writer refuses
    /// frames that would be read out of bounds.
    pub fn is_well_formed(&self) -> bool {
        if self.planes.len() != self.format.plane_sizes(self.width, self.height).len() {
            return false;
        }
        self.planes.iter().enumerate().all(|(i, p)| {
            let rows = self.format.plane_rows(i, self.height);
            let min_stride = self.format.min_stride(i, self.width);
            p.stride >= min_stride && p.stride > 0 && p.data.len() >= p.stride * rows
        })
    }

    /// Number of bytes in the frame, for stats and buffers.
    pub fn byte_len(&self) -> usize {
        self.planes.iter().map(|p| p.data.len()).sum()
    }
}

/// Whether a source is the system output (what is played) or the microphone.
///
/// Same distinction as upstream: the two are mixed together and can also be
/// written as separate tracks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AudioKind {
    System,
    Mic,
}

impl AudioKind {
    /// Index in per-source arrays (`[system, mic]`), as upstream used.
    pub fn index(self) -> usize {
        match self {
            AudioKind::System => 0,
            AudioKind::Mic => 1,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AudioKind::System => "system",
            AudioKind::Mic => "mic",
        }
    }
}

/// Interleaved stereo f32 audio on the host clock.
///
/// `samples` holds `frames * channels` values, channel-interleaved, in the
/// range −1..1 (a muted or silent chunk is all zeros). A device at a rate other
/// than 48 kHz keeps its own `rate` and is resampled by the mixer, so a source
/// never has to convert.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioChunk {
    /// Capture time of the first sample, seconds on [`now`]'s clock.
    pub pts: f64,
    /// Sample rate of `samples`.
    pub rate: u32,
    /// Channels interleaved in `samples` (1 or 2 in practice).
    pub channels: u16,
    /// Interleaved samples.
    pub samples: Vec<f32>,
}

impl AudioChunk {
    /// A stereo chunk of `samples.len() / 2` frames.
    pub fn stereo(pts: f64, rate: u32, samples: Vec<f32>) -> Self {
        AudioChunk {
            pts,
            rate,
            channels: 2,
            samples,
        }
    }

    /// Frames (samples per channel) in the chunk.
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels as usize
        }
    }

    /// Duration in seconds.
    pub fn secs(&self) -> f64 {
        if self.rate == 0 {
            0.0
        } else {
            self.frames() as f64 / self.rate as f64
        }
    }

    /// Peak absolute sample, for level meters.
    pub fn peak(&self) -> f32 {
        self.samples.iter().fold(0f32, |m, s| m.max(s.abs()))
    }

    /// Copy of this chunk with `gain[k]` multiplied into every sample of frame
    /// `k` (saturating at full scale), used for faders.
    pub fn scaled(&self, env: &[f32]) -> AudioChunk {
        let ch = self.channels.max(1) as usize;
        let mut out = self.clone();
        for (k, g) in env.iter().enumerate() {
            for c in 0..ch {
                if let Some(s) = out.samples.get_mut(k * ch + c) {
                    *s = (*s * *g).clamp(-1.0, 1.0);
                }
            }
        }
        out
    }
}

/// A video codec of the live encode (phase 1D).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
}

/// An audio codec of the live encode (phase 1D): AAC for RTMP, Opus for WHIP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioCodec {
    Aac,
    Opus,
}

/// What an [`EncodedPacket`] carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketKind {
    Video(VideoCodec),
    Audio(AudioCodec),
}

impl PacketKind {
    pub fn is_video(self) -> bool {
        matches!(self, PacketKind::Video(_))
    }
}

/// One encoded packet for the live path (replay buffer, RTMP, WHIP; phase 1D).
#[derive(Clone, Debug)]
pub struct EncodedPacket {
    /// Presentation time, seconds on [`now`]'s clock.
    pub pts: f64,
    /// Decode time (equal to `pts` without B-frames, which is what the live
    /// encode produces).
    pub dts: f64,
    pub keyframe: bool,
    /// The bitstream (AVC/HEVC access unit, ADTS-less AAC frame, Opus packet).
    pub data: Arc<[u8]>,
    pub kind: PacketKind,
}

/// Where a video source delivers frames. Called from capture threads; must not
/// block (see the module docs).
pub type FrameSink = Arc<dyn Fn(VideoFrame) + Send + Sync>;

/// Where an audio source delivers chunks. Same rules as [`FrameSink`].
pub type AudioSink = Arc<dyn Fn(AudioKind, AudioChunk) + Send + Sync>;

/// A running video source: screen, window, camera or the test pattern.
///
/// The factory in the source's module (`capture::screen::open`,
/// `capture::camera::open`, `synth::Synth::new`) returns the source stopped;
/// [`start`](VideoSource::start) begins delivery and [`stop`](VideoSource::stop)
/// ends it. Starting a source that is already running is an error, and so is
/// stopping one that is not — the session uses `is_running` to decide.
pub trait VideoSource: Send {
    /// One line for the status output, e.g. "display 1 (2560x1440)".
    fn describe(&self) -> String;
    /// Begin delivering frames to `sink`.
    fn start(&mut self, sink: FrameSink) -> Result<()>;
    /// Stop delivering. Idempotent; returns the first error from the device.
    fn stop(&mut self) -> Result<()>;
    /// True between a successful `start` and `stop`.
    fn is_running(&self) -> bool;
}

/// A running audio source: system output monitor or microphone.
pub trait AudioSource: Send {
    fn kind(&self) -> AudioKind;
    fn describe(&self) -> String;
    fn start(&mut self, sink: AudioSink) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    fn is_running(&self) -> bool;
}

/// A display, as `rec-rs devices` lists it and `--display N` selects it.
#[derive(Clone, Debug, PartialEq)]
pub struct DisplayInfo {
    /// 1-based number on the command line (1 = main).
    pub index: usize,
    /// Stable identifier from the desktop (PipeWire node id, X11 output name).
    pub id: String,
    pub name: String,
    /// Native pixel size.
    pub pixels: (usize, usize),
    /// Size in logical units (points), which `--region` uses.
    pub points: (usize, usize),
    pub primary: bool,
}

/// A window `--window` can select.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowInfo {
    pub id: u64,
    /// Application name, as the desktop reports it.
    pub app: String,
    pub title: String,
    /// Size in logical units.
    pub size: (usize, usize),
}

/// A camera `--webcam` can select.
#[derive(Clone, Debug, PartialEq)]
pub struct CameraInfo {
    /// Unique id (V4L2 path or portal node id).
    pub id: String,
    pub name: String,
}

/// A microphone `--mic` can select.
#[derive(Clone, Debug, PartialEq)]
pub struct MicInfo {
    /// Unique id (PipeWire node id or name).
    pub id: String,
    pub name: String,
    /// Native sample rate in Hz, when the device reports one.
    pub rate: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_is_monotonic_seconds() {
        let a = now();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = now();
        assert!(b > a, "{b} !> {a}");
        assert!(b - a < 1.0, "2 ms took {} s", b - a);
        // Not wall-clock: the host has been up for less than 100 days here,
        // but the point is that it is not an epoch timestamp.
        assert!(a < 4_000_000_000.0);
    }

    #[test]
    fn plane_layouts() {
        assert_eq!(PixelFormat::Bgra.plane_sizes(4, 2), vec![32]);
        assert_eq!(PixelFormat::I420.plane_sizes(4, 4), vec![16, 4, 4]);
        assert_eq!(PixelFormat::Nv12.plane_sizes(4, 4), vec![16, 8]);
        // Odd sizes round the chroma planes up, never down.
        assert_eq!(PixelFormat::I420.plane_sizes(3, 3), vec![9, 4, 4]);
        assert_eq!(PixelFormat::I420.planes(), 3);
        assert_eq!(PixelFormat::Nv12.planes(), 2);
    }

    #[test]
    fn well_formed_frames() {
        let f = VideoFrame::packed(1.0, 4, 2, PixelFormat::Bgra, vec![0; 32]);
        assert!(f.is_well_formed());
        assert_eq!(f.planes[0].stride, 16);
        assert_eq!(f.planes[0].rows(), 2);
        let short = VideoFrame::packed(1.0, 4, 2, PixelFormat::Bgra, vec![0; 31]);
        assert!(!short.is_well_formed(), "truncated frames are rejected");
        let wrong = VideoFrame {
            format: PixelFormat::I420,
            ..f.clone()
        };
        assert!(!wrong.is_well_formed(), "one plane is not I420");
        assert_eq!(f.byte_len(), 32);
    }

    #[test]
    fn audio_chunk_geometry() {
        let a = AudioChunk::stereo(2.0, 48_000, vec![0.5; 960]);
        assert_eq!(a.frames(), 480);
        assert!((a.secs() - 0.01).abs() < 1e-12);
        assert_eq!(a.peak(), 0.5);
        let mono = AudioChunk {
            channels: 1,
            ..a.clone()
        };
        assert_eq!(mono.frames(), 960);
        // One gain per frame, applied to every channel of that frame.
        let scaled = a.scaled(&[0.5, 0.0]);
        assert_eq!(&scaled.samples[..4], &[0.25, 0.25, 0.0, 0.0]);
        // Saturates instead of wrapping.
        let loud = AudioChunk::stereo(0.0, 48_000, vec![0.8, -0.8]).scaled(&[2.0]);
        assert_eq!(loud.samples, vec![1.0, -1.0]);
    }
}
