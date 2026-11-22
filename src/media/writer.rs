//! The recording sink: a GStreamer encode + mux on the host clock.
//!
//! Video is emitted on an exact `1/fps` grid (what editors expect, like OBS),
//! repeating the newest captured frame when the source delivers nothing (a
//! screen source only sends frames when pixels change). Audio is cut and
//! shifted by the [`super::pauses`] timeline, and gaps are filled with silence
//! so the track stays on the capture clock whatever the device does. All
//! timestamps stay on the host clock, which every source shares, so audio is
//! never stretched and stays in sync.
//!
//! The file is a fragmented MP4/MOV while recording — a crash leaves a
//! playable file up to the last fragment — and is finalised on stop.
//!
//! Adapted from rec-rs src/writer.rs (MIT OR Apache-2.0), with AVAssetWriter
//! replaced by GStreamer appsrc pipelines.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::AppSrc;
use serde::{Deserialize, Serialize};

use crate::media::pauses::Pauses;
use crate::media_types::{AudioChunk, AudioKind, PixelFormat, VideoFrame};
use crate::mixer::{self, Fader, Resampler, Volume};

/// The video codec of the recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Codec {
    H264,
    Hevc,
    /// Apple ProRes 422 (intra-frame, large files, smoothest editing).
    Prores,
}

impl Codec {
    /// True for codecs an `.mp4` cannot carry.
    pub fn needs_mov(self) -> bool {
        self == Codec::Prores
    }
}

/// Audio track layout when both system audio and mic are recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AudioLayout {
    /// The system+mic mix (what players use) plus the system and mic stems for
    /// editors.
    Both,
    /// Only the system+mic mix (like OBS's default single track).
    Mix,
    /// Separate system and mic tracks only.
    Stems,
}

#[derive(Clone, Debug)]
pub struct VideoSpec {
    pub width: usize,
    pub height: usize,
    pub fps: u32,
    pub codec: Codec,
    /// Video bits per second.
    pub bitrate: u64,
}

#[derive(Default, Debug, Clone)]
pub struct Stats {
    pub video_frames_in: u64,
    /// Frames written on the grid.
    pub video_frames_out: u64,
    /// Grid slots filled by repeating the newest frame.
    pub video_dups: u64,
    /// Slots the encoder could not take (or a malformed frame).
    pub video_drops: u64,
    pub audio_sys_buffers: u64,
    pub audio_mic_buffers: u64,
    /// Audio buffers or parts that could not be written.
    pub audio_drops: u64,
    /// Audio gaps (late start, dropped device buffers) filled with silence.
    pub audio_gaps_filled: u64,
    /// Samples the mix would have clipped (the limiter held them back).
    pub audio_clipped_samples: u64,
    /// Grid slots skipped while paused or past `--duration`.
    pub paused_slots: u64,
}

/// How far behind real time emission runs, so late-arriving frames still land
/// in their own slot.
pub const LAG_SECS: f64 = 0.05;

/// Frames buffered per source before the oldest are dropped. A screen source's
/// own queue depth is about 8: hold fewer, which covers the emission lag at
/// 60 fps up to about 115 ms of continuous motion.
const QUEUE_MAX: usize = 7;

/// Audio timestamp jitter tolerated before a stem is padded or trimmed.
pub const SNAP_SECS: f64 = 0.0015;

/// How long finalising the file may take (muxing the trailer, flushing).
const FINISH_TIMEOUT: Duration = Duration::from_secs(60);

/// Video PTS of output slot `slot` in nanoseconds (60 fps divides a second
/// exactly, so the grid alternates 16666666/16666667 ns and never drifts).
fn slot_ns(slot: i64, fps: u32) -> u64 {
    ((slot as i128 * 1_000_000_000i128) / fps as i128).max(0) as u64
}

fn secs_ns(t: f64) -> u64 {
    (t * 1_000_000_000.0).round().max(0.0) as u64
}

/// One audio track being written.
struct AudioTrack {
    kind: AudioKind,
    appsrc: AppSrc,
    /// Rate of the device feeding this track; a different one sets up the
    /// resampler.
    source_rate: Option<u32>,
    resampler: Option<Resampler>,
    /// Output-timeline position (seconds) where this track continues.
    next: Option<f64>,
    fader: Fader,
    /// Gain for the level meters without mute (a muted source still shows its
    /// level).
    meter_gain: f32,
}

/// The GStreamer side of the writer, if the session writes a file.
struct Sink {
    pipeline: gst::Pipeline,
    bus: gst::Bus,
    video_src: AppSrc,
    tracks: Vec<AudioTrack>,
    path: PathBuf,
    encoder: String,
}

/// The recording writer: the constant-frame-rate grid, the pause timeline,
/// the audio tracks and the encode pipeline.
pub struct Writer {
    /// None for a preview-only session (no file, nothing encoded).
    sink: Option<Sink>,
    spec: VideoSpec,
    /// Session start on the host clock (the first video frame's pts).
    t0: Option<f64>,
    /// Next slot on the real grid.
    next_slot: i64,
    /// Next slot on the output timeline (real slots minus paused ones).
    out_slot: i64,
    pauses: Pauses,
    /// Stop writing after this many output slots (exact `--duration`).
    limit: Option<i64>,
    /// Newest captured frames, with the time they were captured.
    screen_q: VecDeque<(f64, Arc<VideoFrame>)>,
    screen_last: Option<Arc<VideoFrame>>,
    /// Audio that arrived before the first video frame set the clock.
    preroll: VecDeque<(AudioKind, AudioChunk)>,
    /// Seconds added to [system, mic] audio timestamps for sync (output device
    /// latency, `--system-audio-offset-ms`).
    audio_offset: [f64; 2],
    /// Sources the session will feed.
    audio_wanted: [bool; 2],
    pub stats: Stats,
    failed: Option<String>,
    levels: [f32; 2],
}

impl Writer {
    /// Build the encode pipeline for `spec` and, when `path` is given, the
    /// muxed file. A preview-only session passes None and writes nothing.
    ///
    /// `system` and `mic` say which audio sources the session will feed;
    /// tracks are created for them according to `layout`.
    pub fn new(
        path: Option<&Path>,
        spec: &VideoSpec,
        system: bool,
        mic: bool,
        layout: AudioLayout,
        volumes: [Volume; 2],
    ) -> Result<Self> {
        if spec.width < 16 || spec.height < 16 || !(1..=240).contains(&spec.fps) {
            bail!(
                "bad video size {}x{} @ {} fps",
                spec.width,
                spec.height,
                spec.fps
            );
        }
        if let Some(p) = path
            && p.exists()
        {
            bail!("{} already exists", p.display());
        }
        let sink = match path {
            Some(p) => Some(open_sink(p, spec, system, mic, layout, volumes)?),
            None => None,
        };
        Ok(Writer {
            sink,
            spec: spec.clone(),
            t0: None,
            next_slot: 0,
            out_slot: 0,
            pauses: Pauses::default(),
            limit: None,
            screen_q: VecDeque::new(),
            screen_last: None,
            preroll: VecDeque::new(),
            audio_offset: [0.0; 2],
            audio_wanted: [system, mic],
            stats: Stats::default(),
            failed: None,
            levels: [0.0; 2],
        })
    }

    /// The encoder elements the pipeline chose, for the start log.
    pub fn encoder_name(&self) -> &str {
        self.sink
            .as_ref()
            .map(|s| s.encoder.as_str())
            .unwrap_or("none (no file)")
    }

    /// Write at most `secs` of output (rounded to whole frames).
    pub fn limit_secs(&mut self, secs: Option<f64>) {
        self.limit = secs.map(|d| (d * self.spec.fps as f64).round() as i64);
    }

    /// True once `--duration` is reached: the session can stop.
    pub fn reached_limit(&self) -> bool {
        self.limit.is_some_and(|l| self.out_slot >= l)
    }

    /// Shift [system, mic] audio later by these seconds: the output device's
    /// latency for system audio (captured when mixed, heard later), plus any
    /// user sync offsets.
    pub fn set_audio_offsets(&mut self, offsets: [f64; 2]) {
        self.audio_offset = offsets;
    }

    /// Change a source's level: everything captured from now on follows, in its
    /// own track (and, once phase 1C adds it, the mix and the live encode).
    pub fn set_volumes(&mut self, volumes: [Volume; 2]) {
        if let Some(sink) = self.sink.as_mut() {
            for t in &mut sink.tracks {
                let v = volumes[t.kind.index()];
                t.fader.set(v.gain());
                t.meter_gain = v.fader_gain();
            }
        }
    }

    /// Peak levels [system, mic] since the last call (linear, 0..1), after the
    /// faders but before mute.
    pub fn take_levels(&mut self) -> [f32; 2] {
        std::mem::take(&mut self.levels)
    }

    /// True once the timeline has started (a first video frame arrived).
    pub fn started(&self) -> bool {
        self.t0.is_some()
    }

    /// How far behind real time emission runs. Late webcam frames widen it in
    /// upstream (it adapts to the slowest source); the phase 0 sources are all
    /// on time, so the constant is enough.
    pub fn lag(&self) -> f64 {
        LAG_SECS
    }

    /// The first error the pipeline reported, if any.
    pub fn failure(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    /// Seconds of output written so far (excludes paused time).
    pub fn output_secs(&self) -> f64 {
        self.out_slot as f64 / self.spec.fps as f64
    }

    /// Pause at host time `now`: nothing captured from the next frame slot on
    /// is written until `resume`.
    pub fn pause(&mut self, now: f64) -> bool {
        let Some(t0) = self.t0 else { return false };
        let slot = self.slot_at(now, t0).max(self.next_slot);
        self.pauses.pause(slot)
    }

    pub fn resume(&mut self, now: f64) -> bool {
        let Some(t0) = self.t0 else { return false };
        let slot = self.slot_at(now, t0).max(self.next_slot);
        self.pauses.resume(slot)
    }

    pub fn is_paused(&self) -> bool {
        self.pauses.is_paused()
    }

    fn slot_at(&self, now: f64, t0: f64) -> i64 {
        ((now - t0) * self.spec.fps as f64).ceil().max(0.0) as i64
    }

    /// Take a captured frame. Called from the capture thread.
    pub fn push_video(&mut self, frame: VideoFrame) {
        self.stats.video_frames_in += 1;
        if !frame.is_well_formed() {
            self.stats.video_drops += 1;
            if self.failed.is_none() {
                self.failed = Some(format!(
                    "malformed {}x{} {} frame ({} planes)",
                    frame.width,
                    frame.height,
                    frame.format.label(),
                    frame.planes.len()
                ));
            }
            return;
        }
        if self.t0.is_none() {
            self.t0 = Some(frame.pts);
            if let Some(sink) = self.sink.as_ref() {
                let _ = sink.pipeline.set_state(gst::State::Playing);
            }
            // Audio that arrived before the clock started continues from the
            // session start.
            for (kind, chunk) in std::mem::take(&mut self.preroll) {
                self.push_audio(kind, &chunk);
            }
        }
        let pts = frame.pts;
        self.screen_q.push_back((pts, Arc::new(frame)));
        let due = self.due_time();
        Self::cap(&mut self.screen_q, &mut self.screen_last, QUEUE_MAX, due);
    }

    /// Take audio from a source. Called from the capture thread.
    pub fn push_audio(&mut self, kind: AudioKind, chunk: &AudioChunk) {
        let Some(t0) = self.t0 else {
            // Keep it until the clock starts; the part after t0 is used.
            self.preroll.push_back((kind, chunk.clone()));
            if self.preroll.len() > 400 {
                self.preroll.pop_front();
            }
            return;
        };
        let i = kind.index();
        if !self.audio_wanted[i] || chunk.frames() == 0 {
            return;
        }
        // When the sound was heard (see `set_audio_offsets`).
        let real = chunk.pts + self.audio_offset[i];
        let meter_gain = self
            .sink
            .as_ref()
            .and_then(|s| s.tracks.iter().find(|t| t.kind.index() == i))
            .map(|t| t.meter_gain);
        if let Some(g) = meter_gain {
            self.levels[i] = self.levels[i].max(chunk.peak() * g);
        }
        let Some(sink) = self.sink.as_mut() else {
            return;
        };
        let Some(track) = sink.tracks.iter_mut().find(|t| t.kind.index() == i) else {
            return;
        };
        // The track's audio: 48 kHz stereo frames, faded by this source's fader.
        let input = mixer::stereo_frames(chunk);
        let (pts48, mut frames) = if (chunk.rate as f64 - mixer::RATE).abs() < 0.5 {
            track.resampler = None;
            (real, input)
        } else {
            if track.source_rate != Some(chunk.rate) {
                track.source_rate = Some(chunk.rate);
                track.resampler = Some(Resampler::new(chunk.rate as f64, real));
            }
            track
                .resampler
                .as_mut()
                .unwrap()
                .push(real, chunk.rate as f64, input)
        };
        let rate = mixer::RATE;
        if let Some(env) = track.fader.envelope(frames.len(), rate) {
            for (f, g) in frames.iter_mut().zip(env) {
                f[0] = (f[0] * g).clamp(-1.0, 1.0);
                f[1] = (f[1] * g).clamp(-1.0, 1.0);
            }
        }
        // Cut the chunk at pause boundaries (sample-accurate: the track's
        // buffers are laid end to end, so a dropped or overlapping piece would
        // shift everything after it).
        let n = frames.len();
        let to_slot = |t: f64| (t - t0) * self.spec.fps as f64;
        let (a, b) = (to_slot(pts48), to_slot(pts48 + n as f64 / rate));
        let Some((x, y, off)) = self.pauses.keep(a, b) else {
            return;
        };
        // Nothing before the session start (slot 0).
        let x = x.max(0.0);
        if y <= x {
            return;
        }
        let slot_secs = 1.0 / self.spec.fps as f64;
        let mut i0 = (((x - a) * slot_secs * rate).round() as usize).min(n);
        let mut i1 = (((y - a) * slot_secs * rate).round() as usize).min(n);
        if (x, y) == (a, b) {
            (i0, i1) = (0, n);
        }
        if i1 <= i0 {
            return;
        }
        // The file's timeline starts at the session start (the first frame),
        // so host time becomes output time here.
        let out_s = pts48 - t0 + i0 as f64 / rate - off as f64 / self.spec.fps as f64;
        // The track continues from the last buffer written: fill a gap (late
        // start, dropped device buffer) with silence and trim an overlap, both
        // sample-accurate. Error stays under SNAP_SECS.
        let start = track.next.unwrap_or(0.0);
        let gap = out_s - start;
        let mut pad = 0usize;
        if gap > SNAP_SECS {
            pad = (gap * rate).round() as usize;
        } else if gap < -SNAP_SECS {
            i0 += (-gap * rate).round() as usize;
            if i0 >= i1 {
                return;
            }
        }
        // `--duration` cuts every track at the same output time, so the file
        // ends where the video grid does.
        let limit_secs = self.limit.map(|l| l as f64 / self.spec.fps as f64);
        if let Some(limit) = limit_secs {
            if start >= limit {
                return;
            }
            pad = pad.min(((limit - start) * rate).floor().max(0.0) as usize);
            let body_start = start + pad as f64 / rate;
            if body_start >= limit {
                return;
            }
            let room = ((limit - body_start) * rate).floor().max(0.0) as usize;
            i1 = i1.min(i0 + room);
            if i1 <= i0 {
                return;
            }
        }
        track.next = Some(start + pad as f64 / rate + (i1 - i0) as f64 / rate);
        let body_start = start + pad as f64 / rate;
        if pad > 0 {
            if !push_track_samples(track, start, &vec![[0.0, 0.0]; pad]) {
                self.stats.audio_drops += 1;
                self.check_bus();
                return;
            }
            self.stats.audio_gaps_filled += 1;
        }
        let ok = push_track_samples(track, body_start, &frames[i0.min(n)..i1.min(n)]);
        match kind {
            AudioKind::System => {
                if ok {
                    self.stats.audio_sys_buffers += 1
                } else {
                    self.stats.audio_drops += 1
                }
            }
            AudioKind::Mic => {
                if ok {
                    self.stats.audio_mic_buffers += 1
                } else {
                    self.stats.audio_drops += 1
                }
            }
        }
        if !ok {
            self.check_bus();
        }
    }

    /// Latest capture time the next slot to be emitted may show.
    fn due_time(&self) -> f64 {
        match self.t0 {
            Some(t0) => {
                t0 + self.next_slot as f64 / self.spec.fps as f64 + 0.5 / self.spec.fps as f64
            }
            None => f64::INFINITY,
        }
    }

    /// Bound a frame queue. A dropped frame becomes the current one only if it
    /// is due by the next slot: when emission runs behind, a newer frame must
    /// not be shown under an earlier timestamp (that put content up to several
    /// frames early); it is skipped instead.
    fn cap(
        q: &mut VecDeque<(f64, Arc<VideoFrame>)>,
        last: &mut Option<Arc<VideoFrame>>,
        max: usize,
        due: f64,
    ) {
        while q.len() > max {
            let (pts, frame) = q.pop_front().unwrap();
            if pts <= due || last.is_none() {
                *last = Some(frame);
            }
        }
    }

    /// Pick the newest queued frame captured at or before `t`.
    fn pick(q: &mut VecDeque<(f64, Arc<VideoFrame>)>, last: &mut Option<Arc<VideoFrame>>, t: f64) {
        while let Some((pts, _)) = q.front() {
            if *pts <= t {
                *last = q.pop_front().map(|x| x.1);
            } else {
                break;
            }
        }
    }

    /// Emit every grid slot whose time is at or before `until` (host seconds).
    pub fn emit_until(&mut self, until: f64) {
        let Some(t0) = self.t0 else { return };
        self.check_bus();
        if self.failed.is_some() {
            return;
        }
        let half = 0.5 / self.spec.fps as f64;
        let fps = self.spec.fps as f64;
        loop {
            let slot = self.next_slot;
            let slot_t = t0 + slot as f64 / fps;
            if slot_t > until {
                break;
            }
            let paused = self.pauses.offset_at(slot as f64).is_none();
            let past_limit = self.limit.is_some_and(|l| self.out_slot >= l);
            self.next_slot += 1;
            if paused || past_limit {
                self.stats.paused_slots += 1;
                continue;
            }
            let before = self.screen_q.len();
            Self::pick(&mut self.screen_q, &mut self.screen_last, slot_t + half);
            let fresh = self.screen_q.len() != before;
            let Some(frame) = self.screen_last.clone() else {
                continue;
            };
            let out_slot = self.out_slot;
            self.out_slot += 1;
            if self.push_video_buffer(out_slot, &frame) {
                self.stats.video_frames_out += 1;
                if !fresh {
                    self.stats.video_dups += 1;
                }
            } else {
                self.stats.video_drops += 1;
            }
        }
    }

    fn push_video_buffer(&mut self, out_slot: i64, frame: &VideoFrame) -> bool {
        let Some(sink) = self.sink.as_ref() else {
            // Preview-only session: counted as written, nothing is encoded.
            return true;
        };
        let Some(plane) = frame.planes.first() else {
            return false;
        };
        if !matches!(frame.format, PixelFormat::Bgra | PixelFormat::Rgba) {
            self.failed = Some(format!(
                "the encoder takes BGRA frames, not {}",
                frame.format.label()
            ));
            return false;
        }
        // One copy into the GStreamer buffer: the frame stays owned by the
        // writer for repeats, so its memory cannot be handed over. (A shared
        // allocator that wraps the same bytes is a phase 1C refinement.)
        let bytes = if plane.stride == frame.width * 4 {
            plane.data.to_vec()
        } else {
            let mut tight = Vec::with_capacity(frame.width * frame.height * 4);
            for row in 0..frame.height {
                let at = row * plane.stride;
                tight.extend_from_slice(&plane.data[at..at + frame.width * 4]);
            }
            tight
        };
        let mut buffer = gst::Buffer::from_mut_slice(bytes);
        {
            let b = buffer.get_mut().unwrap();
            let ns = slot_ns(out_slot, self.spec.fps);
            b.set_pts(gst::ClockTime::from_nseconds(ns));
            b.set_duration(gst::ClockTime::from_nseconds(
                slot_ns(out_slot + 1, self.spec.fps) - ns,
            ));
        }
        match sink.video_src.push_buffer(buffer) {
            Ok(_) => true,
            Err(e) => {
                if self.failed.is_none() {
                    self.failed = Some(format!("encoder refused a frame: {e:?}"));
                }
                false
            }
        }
    }

    /// Drain the pipeline bus without blocking and remember the first error.
    fn check_bus(&mut self) {
        let Some(bus) = self.sink.as_ref().map(|s| s.bus.clone()) else {
            return;
        };
        while let Some(msg) = bus.pop() {
            use gst::MessageView;
            match msg.view() {
                MessageView::Error(err) => {
                    if self.failed.is_none() {
                        let src = err.src().map(|s| s.name().to_string()).unwrap_or_default();
                        let debug = err.debug().map(|d| format!(" ({d})")).unwrap_or_default();
                        self.failed = Some(format!("{src}: {}{debug}", err.error()));
                    }
                }
                MessageView::Warning(w) if std::env::var_os("REC_DEBUG").is_some() => {
                    eprintln!(
                        "rec: gst warning: {} ({})",
                        w.error(),
                        w.debug().map(|d| d.to_string()).unwrap_or_default()
                    );
                }
                _ => {}
            }
        }
    }

    /// Flush video up to `end` (host seconds), finalise the file and return the
    /// stats. Blocks until the movie is written.
    pub fn finish(mut self, end: f64) -> Result<Stats> {
        if let Some(e) = self.failed.clone() {
            self.abort();
            bail!("writer failed: {e}");
        }
        if self.t0.is_none() {
            self.abort();
            bail!("no video frames were captured, nothing written");
        }
        self.emit_until(end);
        let Some(sink) = self.sink.take() else {
            // Preview-only session.
            return Ok(self.stats.clone());
        };
        let mut sink = sink;
        // A track that never got audio would make an empty track; give it a
        // moment of silence so the layout is intact.
        for t in &mut sink.tracks {
            let written = match t.kind {
                AudioKind::System => self.stats.audio_sys_buffers,
                AudioKind::Mic => self.stats.audio_mic_buffers,
            };
            if written == 0 {
                let _ = push_track_samples(t, 0.0, &[[0.0, 0.0]; 480]);
            }
        }
        for t in &sink.tracks {
            let _ = t.appsrc.end_of_stream();
        }
        let _ = sink.video_src.end_of_stream();
        let mut result: Result<()> = Ok(());
        let deadline = std::time::Instant::now() + FINISH_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                result = Err(anyhow!("timed out finalizing the movie"));
                break;
            }
            let Some(msg) = sink.bus.timed_pop_filtered(
                gst::ClockTime::from_nseconds(left.as_nanos() as u64),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            ) else {
                continue;
            };
            use gst::MessageView;
            match msg.view() {
                MessageView::Eos(_) => break,
                MessageView::Error(err) => {
                    let debug = err.debug().map(|d| format!(" ({d})")).unwrap_or_default();
                    result = Err(anyhow!(
                        "{}: {}{debug}",
                        err.src().map(|s| s.name().to_string()).unwrap_or_default(),
                        err.error()
                    ));
                    break;
                }
                _ => {}
            }
        }
        let _ = sink.pipeline.set_state(gst::State::Null);
        if let Err(e) = result {
            let _ = std::fs::remove_file(&sink.path);
            return Err(e);
        }
        Ok(self.stats.clone())
    }

    /// Stop the pipeline and remove a partial file (start failure, or an error
    /// before anything usable was written).
    fn abort(&mut self) {
        if let Some(sink) = self.sink.take() {
            let _ = sink.pipeline.set_state(gst::State::Null);
            let _ = std::fs::remove_file(&sink.path);
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if let Some(sink) = self.sink.take() {
            let _ = sink.pipeline.set_state(gst::State::Null);
        }
    }
}

/// Push 48 kHz stereo frames to one track.
fn push_track_samples(track: &mut AudioTrack, pts: f64, frames: &[[f32; 2]]) -> bool {
    let mut bytes = Vec::with_capacity(frames.len() * 8);
    for f in frames {
        bytes.extend_from_slice(&f[0].to_le_bytes());
        bytes.extend_from_slice(&f[1].to_le_bytes());
    }
    let mut buffer = gst::Buffer::from_mut_slice(bytes);
    {
        let b = buffer.get_mut().unwrap();
        b.set_pts(gst::ClockTime::from_nseconds(secs_ns(pts)));
        b.set_duration(gst::ClockTime::from_nseconds(secs_ns(
            frames.len() as f64 / mixer::RATE,
        )));
    }
    track.appsrc.push_buffer(buffer).is_ok()
}

/// Encoder elements to try, most capable first. The one that is actually
/// installed is chosen at run time and logged, because what a distribution
/// offers differs: Fedora's free repositories have no x264enc, no HEVC
/// encoder and no VA-API encoder, so the dev image uses openh264enc.
/// dev/ELEMENTS.md records what the image has.
pub fn video_encoder_candidates(codec: Codec) -> &'static [&'static str] {
    match codec {
        Codec::H264 => &[
            "vah264enc",
            "vaapih264enc",
            "x264enc",
            "openh264enc",
            "avenc_h264",
        ],
        Codec::Hevc => &["vah265enc", "vaapih265enc", "x265enc", "avenc_hevc"],
        Codec::Prores => &["avenc_prores_ks"],
    }
}

/// AAC encoder elements to try, most capable first.
pub const AUDIO_ENCODER_CANDIDATES: &[&str] = &["fdkaacenc", "avenc_aac", "voaacenc", "faac"];

/// Samples an AAC encoder holds back before it emits its first frame (its
/// priming, the MDCT look-ahead). The frames it emits first decode to that many
/// samples of pre-roll, which would push all audio this far behind the picture;
/// the writer drops those frames and re-stamps the rest, so the file's audio
/// starts at 0 like it does with AVFoundation's edit list upstream.
///
/// Measured for each element in dev/ELEMENTS.md's image (10 ms click at the
/// start, decoded again and its onset read back): fdkaacenc 2048, avenc_aac
/// 1024. `REC_AAC_PRIMING` overrides it (0 disables the trim).
pub fn aac_priming(element: &str) -> u64 {
    match element {
        "fdkaacenc" => 2048,
        "avenc_aac" | "faac" => 1024,
        "voaacenc" => 0,
        _ => 1024,
    }
}

/// Samples in one AAC-LC frame, the unit the priming is emitted in.
const AAC_FRAME: u64 = 1024;

/// True when an element named `name` is installed.
pub fn has_element(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}

/// Set a property from text, so the value is converted to the element's own
/// type (plugins disagree: a bitrate is a `guint` in one encoder and a `guint64`
/// in another) and a missing property is skipped instead of panicking.
fn set(el: &gst::Element, name: &str, value: impl ToString) {
    if el.find_property(name).is_some() {
        el.set_property_from_str(name, &value.to_string());
    }
}

/// Build the first installed encoder from `candidates` and set its properties.
fn make_encoder(
    candidates: &[&str],
    bitrate_bps: u64,
    fps: Option<u32>,
    label: &str,
) -> Result<(gst::Element, String)> {
    for name in candidates {
        if !has_element(name) {
            continue;
        }
        let el = gst::ElementFactory::make(name)
            .build()
            .with_context(|| format!("creating {name}"))?;
        // Bitrate: kbit/s for x264enc/x265enc and the VA-API encoders, bits per
        // second for the rest (a long-standing GStreamer inconsistency).
        let kbps = matches!(
            *name,
            "x264enc" | "x265enc" | "vah264enc" | "vah265enc" | "vaapih264enc" | "vaapih265enc"
        );
        let value = if kbps {
            (bitrate_bps / 1000).max(1)
        } else {
            bitrate_bps.max(1000)
        };
        set(&el, "bitrate", value);
        if let Some(fps) = fps {
            // A keyframe every 2 s, like upstream (also the fragment interval).
            for prop in ["key-int-max", "gop-size", "keyframe-period"] {
                if el.find_property(prop).is_some() {
                    set(&el, prop, fps * 2);
                    break;
                }
            }
            set(&el, "bframes", 0);
            set(&el, "bf", 0);
        }
        match *name {
            // Software screen-content tuning; also what keeps openh264enc's
            // rate control on the requested bitrate.
            "openh264enc" => {
                el.set_property_from_str("rate-control", "bitrate");
                if el.find_property("usage-type").is_some() {
                    el.set_property_from_str("usage-type", "screen");
                }
                if el.find_property("complexity").is_some() {
                    el.set_property_from_str("complexity", "medium");
                }
            }
            "x264enc" => {
                if el.find_property("tune").is_some() {
                    el.set_property_from_str("tune", "zerolatency");
                }
                if el.find_property("speed-preset").is_some() {
                    el.set_property_from_str("speed-preset", "veryfast");
                }
            }
            "vah264enc" | "vah265enc" if el.find_property("rate-control").is_some() => {
                el.set_property_from_str("rate-control", "cbr");
            }
            _ => {}
        }
        return Ok((el, name.to_string()));
    }
    bail!(
        "no {label} encoder is installed: tried {}. Install one (x264enc comes with gst-plugins-ugly; Fedora's \
         free repositories only have openh264enc for H.264 and no HEVC encoder at all)",
        candidates.join(", ")
    )
}

/// Build the pipeline and the muxed output file.
fn open_sink(
    path: &Path,
    spec: &VideoSpec,
    system: bool,
    mic: bool,
    layout: AudioLayout,
    volumes: [Volume; 2],
) -> Result<Sink> {
    let mp4 = matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("mp4") | Some("m4v")
    );
    if spec.codec == Codec::Prores && mp4 {
        bail!("ProRes needs a .mov output");
    }
    let pipeline = gst::Pipeline::new();
    let (venc, vename) = make_encoder(
        video_encoder_candidates(spec.codec),
        spec.bitrate,
        Some(spec.fps),
        "video",
    )?;

    let video_caps = gst::Caps::builder("video/x-raw")
        .field("format", "BGRA")
        .field("width", spec.width as i32)
        .field("height", spec.height as i32)
        .field("framerate", gst::Fraction::new(spec.fps as i32, 1))
        .build();
    let video_src = gstreamer_app::AppSrc::builder()
        .name("video")
        .format(gst::Format::Time)
        .caps(&video_caps)
        .is_live(false)
        .do_timestamp(false)
        .build();
    set(video_src.upcast_ref(), "max-buffers", 4);
    let videoconvert = gst::ElementFactory::make("videoconvert").build()?;
    let vqueue = gst::ElementFactory::make("queue").build()?;
    // Bounded, and leaky downstream: a slow encoder loses old frames instead of
    // stalling the capture thread.
    set(&vqueue, "max-size-buffers", 4);
    set(&vqueue, "leaky", "downstream");
    // h264parse/h265parse turn whatever the encoder emits into the format the
    // muxer wants (avc/hvc1 with in-band parameter sets). ProRes is already
    // framed.
    let parser = match spec.codec {
        Codec::H264 => Some(gst::ElementFactory::make("h264parse").build()?),
        Codec::Hevc => Some(gst::ElementFactory::make("h265parse").build()?),
        Codec::Prores => None,
    };
    if let Some(p) = &parser {
        // Repeat parameter sets with every keyframe: a truncated file still
        // decodes.
        set(p, "config-interval", -1);
    }
    let mux = gst::ElementFactory::make(if mp4 { "mp4mux" } else { "qtmux" }).build()?;
    // Crash safety: a fragment every 2 s (one keyframe interval), so a crash
    // leaves a playable file up to the last fragment. REC_FRAGMENT_SECS
    // overrides it (0 = one regular movie) for comparison and debugging.
    let fragment: u64 = std::env::var("REC_FRAGMENT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    if fragment > 0 {
        set(&mux, "fragment-duration", fragment);
    }
    let filesink = gst::ElementFactory::make("filesink").build()?;
    filesink.set_property("location", path.to_string_lossy().to_string());
    filesink.set_property("sync", false);

    let mut chain: Vec<gst::Element> = vec![video_src.clone().upcast(), videoconvert, venc];
    if let Some(p) = parser {
        chain.push(p);
    }
    chain.push(vqueue.clone());
    let mut all = chain.clone();
    all.push(mux.clone());
    all.push(filesink.clone());
    pipeline.add_many(&all)?;
    gst::Element::link_many(&chain)?;
    let video_pad = mux
        .request_pad_simple("video_0")
        .context("the muxer has no video_0 pad")?;
    vqueue
        .static_pad("src")
        .context("video queue src pad")?
        .link(&video_pad)?;
    gst::Element::link(&mux, &filesink)?;

    // --- audio ------------------------------------------------------------
    let both = system && mic;
    let want_mix = both && layout != AudioLayout::Stems;
    let want_stems = !both || layout != AudioLayout::Mix;
    let mut kinds: Vec<AudioKind> = vec![];
    if system && want_stems {
        kinds.push(AudioKind::System);
    }
    if mic && want_stems {
        kinds.push(AudioKind::Mic);
    }
    let mut tracks = vec![];
    let mut audio_name = String::new();
    if !kinds.is_empty() {
        let audio_caps = gst::Caps::builder("audio/x-raw")
            .field("format", "F32LE")
            .field("layout", "interleaved")
            .field("rate", mixer::RATE as i32)
            .field("channels", 2i32)
            .build();
        for (n, kind) in kinds.iter().enumerate() {
            // One encoder per track: a shared one would interleave the streams.
            let (aenc, aname) = make_encoder(AUDIO_ENCODER_CANDIDATES, 256_000, None, "AAC")?;
            audio_name = aname.clone();
            let src = gstreamer_app::AppSrc::builder()
                .name(format!("audio-{}", kind.label()))
                .format(gst::Format::Time)
                .caps(&audio_caps)
                .is_live(false)
                .do_timestamp(false)
                .max_bytes(1 << 20)
                .build();
            set(src.upcast_ref(), "max-buffers", 16);
            let convert = gst::ElementFactory::make("audioconvert").build()?;
            let resample = gst::ElementFactory::make("audioresample").build()?;
            let aparse = gst::ElementFactory::make("aacparse").build()?;
            let q = gst::ElementFactory::make("queue").build()?;
            let chain: Vec<gst::Element> = vec![
                src.clone().upcast(),
                convert,
                resample,
                aenc,
                aparse,
                q.clone(),
            ];
            pipeline.add_many(&chain)?;
            gst::Element::link_many(&chain)?;
            let pad = mux
                .request_pad_simple(&format!("audio_{n}"))
                .with_context(|| format!("the muxer has no audio_{n} pad"))?;
            let qsrc = q.static_pad("src").context("audio queue src pad")?;
            qsrc.link(&pad)?;
            // Drop the encoder's priming frames and move the rest back by the
            // time they covered, so the track starts at 0 in sync with video.
            let priming = std::env::var("REC_AAC_PRIMING")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or_else(|| aac_priming(&aname));
            let drop = priming.div_ceil(AAC_FRAME);
            // A pad probe is a Fn, so its state lives in atomics.
            let seen = AtomicU64::new(0);
            let shift = AtomicU64::new(u64::MAX); // unset
            qsrc.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
                if seen.load(Ordering::Relaxed) < drop {
                    seen.fetch_add(1, Ordering::Relaxed);
                    return gst::PadProbeReturn::Drop;
                }
                if let Some(buf) = info.buffer_mut() {
                    let pts = buf.pts().map(|p| p.nseconds()).unwrap_or(0);
                    // The first kept frame's PTS is the priming exactly.
                    let s = match shift.compare_exchange(
                        u64::MAX,
                        pts,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => pts,
                        Err(v) => v,
                    };
                    if let Some(b) = buf.get_mut() {
                        b.set_pts(gst::ClockTime::from_nseconds(pts.saturating_sub(s)));
                        if let Some(dts) = b.dts() {
                            b.set_dts(gst::ClockTime::from_nseconds(
                                dts.nseconds().saturating_sub(s),
                            ));
                        }
                    }
                    seen.fetch_add(1, Ordering::Relaxed);
                }
                gst::PadProbeReturn::Ok
            });
            tracks.push(AudioTrack {
                kind: *kind,
                appsrc: src,
                source_rate: None,
                resampler: None,
                next: None,
                fader: Fader::new(volumes[kind.index()].gain()),
                meter_gain: volumes[kind.index()].fader_gain(),
            });
        }
    }
    if want_mix && !want_stems {
        bail!("the system+mic mix track is not implemented yet (phase 1C)");
    }
    let encoder = if tracks.is_empty() {
        vename
    } else {
        format!("{vename} + {audio_name}")
    };
    let bus = pipeline.bus().context("pipeline bus")?;
    pipeline.set_state(gst::State::Paused)?;
    Ok(Sink {
        pipeline,
        bus,
        video_src,
        tracks,
        path: path.to_path_buf(),
        encoder,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_cap_never_shows_future_frames() {
        // Overflowing the frame queue while emission runs behind must never
        // make a frame from the future the current one (shown early). The
        // queue cap works on indices here, so the test needs no video buffers.
        fn cap_index(q: &mut VecDeque<(f64, i32)>, last: &mut Option<i32>, max: usize, due: f64) {
            while q.len() > max {
                let (pts, frame) = q.pop_front().unwrap();
                if pts <= due || last.is_none() {
                    *last = Some(frame);
                }
            }
        }
        let mut q: VecDeque<(f64, i32)> = Default::default();
        let mut last: Option<i32> = None;
        for i in 0..12 {
            q.push_back((i as f64, i));
            // Emission is at frame 2: nothing newer than 2.5 may become current.
            cap_index(&mut q, &mut last, 4, 2.5);
        }
        assert_eq!(
            last,
            Some(2),
            "current frame stays due, later ones are skipped"
        );
        assert_eq!(q.len(), 4);
    }

    #[test]
    fn slot_timestamps_are_exact_on_average() {
        // 60 fps: alternating 16666666/16666667 ns, never drifting.
        let ns: Vec<u64> = (0..3600).map(|s| slot_ns(s, 60)).collect();
        assert_eq!(ns[0], 0);
        assert_eq!(ns[60], 1_000_000_000, "one second is one second");
        assert_eq!(ns[3599], 59_983_333_333);
        assert!(ns.windows(2).all(|w| w[1] > w[0]));
        for fps in [24, 25, 30, 50, 60, 120] {
            let step = 1_000_000_000i64 / fps as i64;
            let d: Vec<i64> = (0..fps as i64 * 3)
                .map(|s| (slot_ns(s + 1, fps) - slot_ns(s, fps)) as i64)
                .collect();
            assert!(d.iter().all(|x| (x - step).abs() <= 1), "{fps}: {d:?}");
        }
    }

    #[test]
    fn encoder_candidates_are_ordered_by_preference() {
        assert_eq!(video_encoder_candidates(Codec::H264)[0], "vah264enc");
        assert!(video_encoder_candidates(Codec::H264).contains(&"openh264enc"));
        assert!(video_encoder_candidates(Codec::Hevc).contains(&"x265enc"));
        assert_eq!(video_encoder_candidates(Codec::Prores), ["avenc_prores_ks"]);
        assert_eq!(AUDIO_ENCODER_CANDIDATES[0], "fdkaacenc");
    }
}
