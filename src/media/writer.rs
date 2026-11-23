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
//! # Track layout (upstream's, from the README)
//!
//! 1. video
//! 2. the system+mic mix (AAC-LC 48 kHz stereo 256 kb/s, the default track)
//! 3. the system stem (AAC 48 kHz stereo 256 kb/s)
//! 4. the mic stem (AAC 48 kHz mono 192 kb/s)
//!
//! With `--audio-tracks mix` only the mix is written; with `stems` only the two
//! stems; a session with a single source writes that source's stem alone. When
//! the mix and the stems coexist the audio tracks share an alternate group with
//! the mix enabled, so a player gets the mix and an editor picks a stem.
//!
//! Faders are applied once per captured buffer, before the mix, the stem and
//! the live encode see it, exactly like upstream; the mix bus limiter keeps the
//! mix under −1 dBFS. At 0 dB a stem carries the device's samples unchanged.
//!
//! # AAC priming
//!
//! Every AAC encoder delays its output by its look-ahead (fdkaacenc 2048
//! samples, avenc_aac 1024). AVFoundation writes an edit list that makes
//! players skip it; [`super::mp4`] writes the same `elst` here, so the first
//! beep in a file is at 0 ms instead of 42.7 ms late.
//!
//! # Crash safety and the final file
//!
//! While recording, the muxer writes a fragment every two seconds, so a crash
//! leaves a playable file up to the last fragment. On a normal stop
//! [`super::mp4::finalize`] appends a regular `moov` that indexes the samples
//! where they lie and flips the fragment boxes to `free` — upstream's
//! `ftyp ... mdat moov`, with no media bytes moved. If the finalise fails the
//! fragmented file is kept and a warning says why.
//!
//! # Camera timing
//!
//! Webcams deliver frames 70–120 ms after capture. Emission runs behind real
//! time by the slowest source's arrival latency (at least [`LAG_SECS`], at most
//! 300 ms), so a late frame still lands in its own slot; a cold camera holds
//! the start of the file until its exposure has settled (upstream's 2 s gate).
//!
//! Adapted from rec-rs src/writer.rs (MIT OR Apache-2.0), with AVAssetWriter
//! replaced by GStreamer appsrc pipelines and the finalise done in
//! [`super::mp4`].

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::AppSrc;
use serde::{Deserialize, Serialize};

use crate::compose::{PipSpec, Scene};
use crate::gpu::Renderer;
use crate::live;
use crate::media::mp4;
use crate::media::pauses::Pauses;
use crate::media_types::{AudioChunk, AudioKind, FrameSink, PixelFormat, VideoFrame};
use crate::mixer::{self, Fader, Mixer, Resampler, Volume};

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
    /// Slots retried because the encoder was busy (upstream's backlog rule).
    pub video_retries: u64,
    pub audio_sys_buffers: u64,
    pub audio_mic_buffers: u64,
    pub audio_mix_buffers: u64,
    /// Audio buffers or parts that could not be written.
    pub audio_drops: u64,
    /// Audio gaps (late start, dropped device buffers) filled with silence.
    pub audio_gaps_filled: u64,
    /// Audio chunks dropped because their timestamp jumped more than
    /// `MAX_GAP_SECS`/`MAX_BUFFER` (a broken clock, not a real gap).
    pub audio_resyncs: u64,
    pub audio_overruns: u64,
    /// Samples the mix would have clipped (the limiter held them back).
    pub audio_clipped_samples: u64,
    /// Grid slots skipped while paused or past `--duration`.
    pub paused_slots: u64,
    /// Arrival latency (host now − capture pts), max and sum, in seconds.
    pub screen_latency_max: f64,
    pub screen_latency_sum: f64,
    pub cam_latency_max: f64,
    pub cam_latency_sum: f64,
    /// What the finalise cost, and how big the movie was.
    pub finalize_millis: u128,
    pub fragments: usize,
}

/// How far behind real time emission runs, so late-arriving frames still land
/// in their own slot.
pub const LAG_SECS: f64 = 0.05;

/// The most emission may run behind real time, whatever the sources report.
pub const MAX_LAG_SECS: f64 = 0.3;

/// Frames buffered per source before the oldest are dropped. A screen source's
/// own queue depth is about 8: hold fewer, which covers the emission lag at
/// 60 fps up to about 115 ms of continuous motion.
const QUEUE_MAX: usize = 7;

/// Frames the GStreamer video appsrc and its queue may hold. Two frames cover
/// one encoder frame plus one in flight; anything more is memory a slow
/// software encoder (a 4K BGRA frame is 33 MB) must not spend.
const VIDEO_QUEUE_FRAMES: u64 = 2;

/// Bytes an audio appsrc may hold: about 0.7 s of 10 ms stereo f32 buffers.
/// Large enough that a busy muxer rides out a stall without dropping a chunk
/// in the middle of a beep (a dropped buffer ends the previous one abruptly,
/// which is a quiet click), still a hard memory bound per track.
const AUDIO_QUEUE_BYTES: u64 = 256 * 1024;

/// Buffers an audio appsrc/queue may hold.
const AUDIO_QUEUE_BUFFERS: u32 = 32;

/// How far behind its time a slot may fall while the encoder is busy before it
/// is dropped instead of retried (upstream's `MAX_BACKLOG_SECS`).
pub const MAX_BACKLOG_SECS: f64 = 0.5;

/// Audio timestamp jitter tolerated before a track is padded or trimmed.
pub const SNAP_SECS: f64 = 0.0015;

/// Fade-in applied to real audio that follows filler silence, so the join is
/// not a step (a click) whatever phase the signal resumes at. Short enough to
/// leave a beep's onset and peak intact (the parent's ±5 ms / ±1 dB).
const FILLER_FADE_SECS: f64 = 0.002;

/// The longest audio gap the writer fills with silence. A longer jump is a
/// source's clock bug or a device that connected late: the track resyncs
/// instead of allocating (and writing) the silence for it.
pub const MAX_GAP_SECS: f64 = 5.0;

/// The furthest a track may resync to (a microphone that connected a minute
/// after the session). Beyond it the chunk is dropped outright, so a broken
/// clock cannot make a ten-minute file out of a two-second recording.
pub const MAX_RESYNC_SECS: f64 = 60.0;

/// How far a track that has produced audio may lag the video timeline before
/// the writer fills the gap with silence (a device that died mid-session).
/// Without it the muxer waits for the silent stream and stalls every other
/// stream with it.
pub const STARVE_SECS: f64 = 0.5;

/// The most silence written to one track per pacer pass.
const SILENCE_CHUNK_SECS: f64 = 1.0;

/// Start without the camera if it delivers nothing for this long after the
/// settle time.
const CAMERA_TIMEOUT_SECS: f64 = 3.0;

/// How long finalising the file may take (muxing the trailer, the defragment).
const FINISH_TIMEOUT: Duration = Duration::from_secs(120);

/// Video PTS of output slot `slot` in nanoseconds (every allowed fps divides a
/// second, so the grid alternates 16666666/16666667 ns and never drifts).
fn slot_ns(slot: i64, fps: u32) -> u64 {
    ((slot as i128 * 1_000_000_000i128) / fps as i128).max(0) as u64
}

fn secs_ns(t: f64) -> u64 {
    (t * 1_000_000_000.0).round().max(0.0) as u64
}

/// Which track of the file an audio stream goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Mix,
    System,
    Mic,
}

/// One audio track being written.
struct AudioTrack {
    target: Target,
    appsrc: AppSrc,
    channels: u8,
    /// Output-timeline position (seconds) where this track continues.
    next: Option<f64>,
    /// The last thing written to this track was filler silence (a source that
    /// had not delivered yet, or a gap). Real audio that would overlap it is
    /// dropped whole rather than spliced mid-chunk, and the next real write
    /// after it is faded in, so a late burst never clicks.
    after_filler: bool,
}

/// One audio source's stream state: resampling to 48 kHz and its fader.
struct SourceAudio {
    rate: Option<u32>,
    resampler: Option<Resampler>,
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
    /// Track layout to apply when the file is finalised.
    layout: mp4::Layout,
    /// Bytes the video appsrc may hold (2 frames): the drop policy matches
    /// upstream's `isReadyForMoreMediaData`.
    video_max_bytes: u64,
}

/// The camera gate: hold the start of the file until the camera has delivered
/// a frame and had time to settle its exposure (upstream's `wait_for_camera`).
struct CamGate {
    settle: f64,
    /// The newest screen frame held back meanwhile.
    pending: Option<VideoFrame>,
    /// Capture time the camera is ready (first camera frame + settle).
    ready_at: Option<f64>,
    give_up: f64,
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
    cam_q: VecDeque<(f64, Arc<VideoFrame>)>,
    cam_last: Option<Arc<VideoFrame>>,
    /// Audio that arrived before the first video frame set the clock.
    preroll: VecDeque<(AudioKind, AudioChunk)>,
    /// Seconds added to [system, mic] audio timestamps for sync (output device
    /// latency, `--system-audio-offset-ms`).
    audio_offset: [f64; 2],
    /// Sources the session will feed.
    audio_wanted: [bool; 2],
    /// Per-source resampler and fader.
    sources: [SourceAudio; 2],
    /// The mix bus for the file's mix track (output timeline).
    mixer: Option<Mixer>,
    /// The mix bus for the live encode (real time, keeps running while paused).
    live_mixer: Option<Mixer>,
    /// Host time of the first frame handed to the live encode. Its timeline
    /// starts there; audio from before it is pre-roll the RTMP/WHIP servers
    /// discard as "received too late" (a real race: 3 packets, one run in two
    /// under load), so the tap does not send it.
    live_t0: Option<f64>,
    /// Wall-clock time each source last delivered a chunk. Filler silence is
    /// only for a source that is *gone* (nothing for STARVE_SECS), never for
    /// one that is alive but behind in media time (a backlog burst): filling
    /// in the middle of a burst pushed the track late and spliced it.
    source_seen: [f64; 2],
    /// The shared live encode (replay buffer, streams; phase 1D).
    live: Option<Arc<live::Live>>,
    /// Where every emitted frame is offered (the app's preview; phase 1E).
    preview: Option<FrameSink>,
    /// Last preview time on the real grid (upstream throttles it to 30 fps).
    last_preview: f64,
    /// GPU compositor, when phase 1C's renderer is available.
    renderer: Option<Renderer>,
    /// The last composited frame, reused if a pipelined renderer has none ready
    /// yet (the frame grid must not lose a slot).
    composed_last: Option<Arc<VideoFrame>>,
    /// Moving average of the compositor's cost per frame, and how many frames
    /// in a row exceeded the frame budget on a software device.
    compose_ema: f64,
    slow_frames: u32,
    /// The session has a camera, even before its first frame arrives.
    cam_expected: bool,
    /// Size of the newest camera frame, for `cam_aspect` before the first
    /// emitted slot picks it.
    cam_size: Option<(usize, usize)>,
    scene: Scene,
    pip: PipSpec,
    /// Hold the start until the camera is ready (see [`CamGate`]).
    cam_gate: Option<CamGate>,
    /// Decaying peak arrival latency [screen, camera] and when it was set.
    latency_peak: [f64; 2],
    latency_at: [f64; 2],
    /// Caps currently announced to the video appsrc.
    caps_key: Option<(usize, usize, PixelFormat)>,
    /// A CPU-fallback scene note is printed once.
    warned_overlay: bool,
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
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        path: Option<&Path>,
        spec: &VideoSpec,
        system: bool,
        mic: bool,
        layout: AudioLayout,
        volumes: [Volume; 2],
        scene: Scene,
        pip: PipSpec,
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
            Some(p) => Some(open_sink(p, spec, system, mic, layout)?),
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
            cam_q: VecDeque::new(),
            cam_last: None,
            preroll: VecDeque::new(),
            audio_offset: [0.0; 2],
            audio_wanted: [system, mic],
            sources: std::array::from_fn(|i| SourceAudio {
                rate: None,
                resampler: None,
                fader: Fader::new(volumes[i].gain()),
                meter_gain: volumes[i].fader_gain(),
            }),
            mixer: None,
            live_mixer: None,
            source_seen: [0.0; 2],
            live_t0: None,
            live: None,
            preview: None,
            last_preview: 0.0,
            renderer: None,
            composed_last: None,
            compose_ema: 0.0,
            slow_frames: 0,
            cam_expected: false,
            cam_size: None,
            scene,
            pip,
            cam_gate: None,
            latency_peak: [0.0; 2],
            latency_at: [0.0; 2],
            caps_key: None,
            warned_overlay: false,
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

    /// Change a source's level: everything captured from now on follows, in the
    /// mix, both stems and the live encode.
    pub fn set_volumes(&mut self, volumes: [Volume; 2]) {
        for (i, v) in volumes.into_iter().enumerate() {
            self.sources[i].fader.set(v.gain());
            self.sources[i].meter_gain = v.fader_gain();
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

    /// How far behind real time emission runs: the slowest source's recent
    /// arrival latency plus a margin, never less than [`LAG_SECS`].
    pub fn lag(&self) -> f64 {
        let worst = self.latency_peak[0].max(self.latency_peak[1]);
        (worst + 0.025).clamp(LAG_SECS, MAX_LAG_SECS)
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

    /// The writer's live tap: every emitted frame and every post-fader audio
    /// buffer is offered to the shared live encode (replay buffer, streams).
    /// Non-blocking, and it keeps running while a recording is paused and with
    /// `--no-record`.
    pub fn set_live(&mut self, live: Arc<live::Live>) {
        self.live = Some(live);
    }

    /// Tell the writer the session has a camera, so a camera scene can be
    /// selected before the first frame arrives.
    pub fn set_camera_expected(&mut self, yes: bool) {
        self.cam_expected = yes;
    }

    /// Offer every emitted (composited) frame to `sink`, at most 30 times a
    /// second. This is the app's preview; it shows what is recorded, overlay
    /// included.
    pub fn set_preview(&mut self, sink: FrameSink) {
        self.preview = Some(sink);
    }

    /// Make phase 1C's compositor available. Called by the session when a
    /// camera is used; until 1C lands the writer falls back to the CPU path.
    pub fn set_renderer(&mut self, renderer: Option<Renderer>) {
        self.renderer = renderer;
        if let Some(r) = self.renderer.as_mut() {
            r.set_scene(self.scene);
            r.set_pip(self.pip.clone());
        }
    }

    /// Switch scenes live. `Screen` always works; `Cam` needs camera frames;
    /// `ScreenCam` needs the compositor (the overlay is drawn on the GPU).
    pub fn set_scene(&mut self, scene: Scene) -> bool {
        let ok = match scene {
            Scene::Screen => true,
            Scene::Cam => self.renderer.is_some() || self.cam_expected || self.cam_last.is_some(),
            Scene::ScreenCam => self.renderer.is_some(),
        };
        if ok {
            self.scene = scene;
            if let Some(r) = self.renderer.as_mut() {
                r.set_scene(scene);
            }
        }
        ok
    }

    /// Change the camera overlay live (no-op without a compositor).
    pub fn set_pip(&mut self, pip: PipSpec) {
        self.pip = pip.clone();
        if let Some(r) = self.renderer.as_mut() {
            r.set_pip(pip);
        }
    }

    /// Aspect ratio of the camera, once frames arrive.
    pub fn cam_aspect(&self) -> Option<f64> {
        if let Some(r) = self.renderer.as_ref() {
            return Some(r.cam_aspect());
        }
        self.cam_size
            .or_else(|| self.cam_last.as_ref().map(|f| (f.width, f.height)))
            .map(|(w, h)| w as f64 / h.max(1) as f64)
    }

    /// Start only once the camera has delivered a frame and then run for
    /// `settle` seconds (its exposure settling). Gives up after a few seconds
    /// without camera frames rather than never starting (upstream's gate).
    pub fn wait_for_camera(&mut self, settle: f64) {
        let now = crate::media_types::now();
        self.cam_gate = Some(CamGate {
            settle,
            pending: None,
            ready_at: None,
            give_up: now + settle + CAMERA_TIMEOUT_SECS,
        });
    }

    /// Track arrival latency as a peak that decays by 50 ms per second, so one
    /// stall (a busy thread, a device starting) does not keep the output
    /// delayed for the rest of the session.
    fn note_latency(&mut self, i: usize, lat: f64, at: f64) {
        let p = &mut self.latency_peak[i];
        let decayed = (*p - 0.05 * (at - self.latency_at[i]).max(0.0)).max(0.0);
        *p = decayed.max(lat);
        self.latency_at[i] = at;
    }

    /// Take a captured screen frame. Called from the capture thread.
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
        let lat = crate::media_types::now() - frame.pts;
        self.stats.screen_latency_max = self.stats.screen_latency_max.max(lat);
        self.stats.screen_latency_sum += lat;
        self.note_latency(0, lat, frame.pts);
        // The camera gate: hold the first screen frame until the camera is
        // ready (a still screen sends no more frames).
        if self.t0.is_none()
            && let Some(gate) = self.cam_gate.as_mut()
        {
            let now = frame.pts;
            if gate.ready_at.is_none_or(|r| now < r) && now < gate.give_up {
                gate.pending = Some(frame);
                return;
            }
            self.cam_gate = None;
        }
        self.push_screen_frame(frame, true);
    }

    /// Take a captured camera frame. Called from the capture thread.
    pub fn push_cam(&mut self, frame: VideoFrame) {
        self.stats.video_frames_in += 1;
        if !frame.is_well_formed() {
            self.stats.video_drops += 1;
            if self.failed.is_none() {
                self.failed = Some("malformed camera frame".into());
            }
            return;
        }
        let lat = crate::media_types::now() - frame.pts;
        self.stats.cam_latency_max = self.stats.cam_latency_max.max(lat);
        self.stats.cam_latency_sum += lat;
        self.note_latency(1, lat, frame.pts);
        self.cam_expected = true;
        self.cam_size = Some((frame.width, frame.height));
        // The camera gate releases as soon as the camera has settled: the
        // held screen frame starts the file at this camera frame's time.
        if self.t0.is_none()
            && let Some(gate) = self.cam_gate.as_mut()
        {
            let ready = *gate.ready_at.get_or_insert(frame.pts + gate.settle);
            if frame.pts >= ready.min(gate.give_up)
                && let Some(mut screen) = gate.pending.take()
            {
                // The waiting screen frame starts the file now: it is what is
                // on screen at the moment the camera is ready (upstream passes
                // the camera's time for it).
                screen.pts = frame.pts;
                self.cam_gate = None;
                self.push_screen_frame(screen, false);
            }
        }
        // A camera frame can start the clock too: with no gate and no screen
        // frame yet (a camera-only session, or a still screen), the file
        // starts here rather than never.
        if !self.started() && self.cam_gate.is_none() {
            self.start_clock(frame.pts);
        }
        self.cam_q.push_back((frame.pts, Arc::new(frame)));
        let due = self.due_time();
        Self::cap(&mut self.cam_q, &mut self.cam_last, QUEUE_MAX, due);
    }

    /// Queue a screen frame (already counted) and start the clock if needed.
    fn push_screen_frame(&mut self, frame: VideoFrame, counted: bool) {
        if !self.started() {
            self.start_clock(frame.pts);
        }
        let pts = frame.pts;
        let _ = counted;
        self.screen_q.push_back((pts, Arc::new(frame)));
        let due = self.due_time();
        Self::cap(&mut self.screen_q, &mut self.screen_last, QUEUE_MAX, due);
    }

    /// The first frame set the session clock: start the pipeline, the mix bus
    /// and the audio that arrived before it.
    fn start_clock(&mut self, t0: f64) {
        self.t0 = Some(t0);
        if let Some(sink) = self.sink.as_ref() {
            let _ = sink.pipeline.set_state(gst::State::Playing);
        }
        let started = crate::media_types::now();
        for i in 0..2 {
            if self.audio_wanted[i] {
                self.source_seen[i] = started;
            }
        }
        if self.audio_wanted.iter().any(|w| *w) {
            if self
                .sink
                .as_ref()
                .is_some_and(|s| s.tracks.iter().any(|t| t.target == Target::Mix))
            {
                self.mixer = Some(Mixer::new(0.0, self.audio_wanted[0], self.audio_wanted[1]));
            }
            if self.live.is_some() {
                // The live tap is fed absolute host-clock times, so its mix
                // bus must start at the session clock: a bus starting at zero
                // would place every buffer hours in (and try to allocate the
                // samples for it).
                self.live_mixer = Some(Mixer::new(t0, self.audio_wanted[0], self.audio_wanted[1]));
            }
        }
        for (kind, chunk) in std::mem::take(&mut self.preroll) {
            self.push_audio(kind, &chunk);
        }
    }

    /// Take audio from a source. Called from the capture thread.
    pub fn push_audio(&mut self, kind: AudioKind, chunk: &AudioChunk) {
        let i = kind.index();
        if !self.audio_wanted[i] || chunk.frames() == 0 {
            return;
        }
        self.source_seen[i] = crate::media_types::now();
        let Some(t0) = self.t0 else {
            // Keep it until the clock starts; the part after t0 is used.
            self.preroll.push_back((kind, chunk.clone()));
            if self.preroll.len() > 400 {
                self.preroll.pop_front();
            }
            return;
        };
        // When the sound was heard (see `set_audio_offsets`).
        let real = chunk.pts + self.audio_offset[i];
        // The meter reads the raw device level after the fader gain but before
        // mute, like upstream.
        self.levels[i] = self.levels[i].max(chunk.peak() * self.sources[i].meter_gain);

        // One canonical 48 kHz stereo stream, resampled once.
        let src = &mut self.sources[i];
        let input = mixer::stereo_frames(chunk);
        let (pts48, mut frames) = if (chunk.rate as f64 - mixer::RATE).abs() < 0.5 {
            src.resampler = None;
            (real, input)
        } else {
            if src.rate != Some(chunk.rate) {
                src.rate = Some(chunk.rate);
                src.resampler = Some(Resampler::new(chunk.rate as f64, real));
            }
            src.resampler
                .as_mut()
                .unwrap()
                .push(real, chunk.rate as f64, input)
        };
        // The fader is applied once, and the same faded samples feed the mix,
        // the stem and the live encode. `Fader` returns None at unity, so a
        // stem at 0 dB keeps the device's samples byte for byte.
        if let Some(env) = src.fader.envelope(frames.len(), mixer::RATE) {
            apply_gain(&mut frames, &env);
        }

        // The live encode keeps running while the file is paused: it gets the
        // whole buffer on the host clock.
        if self.live.is_some() {
            if self.live_mixer.is_none() {
                self.live_mixer = Some(Mixer::new(t0, self.audio_wanted[0], self.audio_wanted[1]));
            }
            if let Some(m) = self.live_mixer.as_mut() {
                m.push(kind, real, mixer::RATE, frames.clone());
            }
            self.drain_live(false);
        }

        // The file: cut the buffer at pause boundaries (sample-accurate: the
        // tracks are laid end to end, so a dropped or overlapping piece would
        // shift everything after it).
        let rate = mixer::RATE;
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
        let part = frames[i0..i1].to_vec();

        // The mix bus for the file's mix track.
        if self.mixer.is_some() {
            if let Some(m) = self.mixer.as_mut() {
                m.push(kind, out_s, rate, part.clone());
            }
            self.drain_mix(false);
        }

        // The source's own stem.
        let target = match kind {
            AudioKind::System => Target::System,
            AudioKind::Mic => Target::Mic,
        };
        if self.write_track(target, out_s, &part) {
            match kind {
                AudioKind::System => self.stats.audio_sys_buffers += 1,
                AudioKind::Mic => self.stats.audio_mic_buffers += 1,
            }
        } else {
            self.stats.audio_drops += 1;
        }
    }

    /// Pull finished mix frames into the mix track and the live encode.
    fn drain_mix(&mut self, flush: bool) {
        let mut chunks: Vec<(f64, Vec<[f32; 2]>)> = vec![];
        let mut clipped = 0;
        let mut overruns = 0;
        if let Some(m) = self.mixer.as_mut() {
            while let Some((t, frames)) = m.pull_frames(flush) {
                chunks.push((t, frames));
            }
            clipped = m.clipped;
            overruns = m.overruns;
        }
        for (t, frames) in chunks {
            if self.write_track(Target::Mix, t, &frames) {
                self.stats.audio_mix_buffers += 1;
            } else {
                self.stats.audio_drops += 1;
            }
        }
        if self.mixer.is_some() {
            self.stats.audio_clipped_samples = clipped;
            self.note_overruns(overruns);
        }
    }

    /// Count the chunks both mix buses refused (a timestamp jump), minus the
    /// ones already counted for the other bus.
    fn note_overruns(&mut self, file: u64) {
        let live = self.live_mixer.as_ref().map_or(0, |m| m.overruns);
        self.stats.audio_overruns = file + live;
    }

    /// Offer the live mix to the shared encode.
    fn drain_live(&mut self, flush: bool) {
        let Some(live) = self.live.clone() else {
            return;
        };
        let mut chunks = vec![];
        if let Some(m) = self.live_mixer.as_mut() {
            while let Some(chunk) = m.pull_chunk(flush) {
                chunks.push(chunk);
            }
        }
        for chunk in chunks {
            live.push_audio(&chunk);
        }
        let file = self.mixer.as_ref().map_or(0, |m| m.overruns);
        self.note_overruns(file);
    }

    /// Keep every audio track moving on the file's timeline.
    ///
    /// A track whose source never delivered, or stopped delivering, must not
    /// stall the muxer: qtmux waits for every stream to reach a fragment
    /// before writing it, so one silent track blocks video too (measured: a
    /// requested-but-silent microphone stopped a 320x180 test at 11 frames).
    /// A never-started track gets silence from the start; a track that has
    /// data gets it only once it is STARVE_SECS behind. Real audio that arrives
    /// later overlaps the silence and `write_track` trims the overlap, so the
    /// source's own samples replace it exactly.
    fn top_up_audio(&mut self, out_time: f64) {
        let Some(sink) = self.sink.as_mut() else {
            return;
        };
        let limit = self.limit.map(|l| l as f64 / self.spec.fps as f64);
        let out_time = limit.map_or(out_time, |l| out_time.min(l));
        let mut filled = 0u64;
        let mut failed = false;
        let wall = crate::media_types::now();
        for track in &mut sink.tracks {
            let source_alive = match track.target {
                Target::System => wall - self.source_seen[0] < STARVE_SECS,
                Target::Mic => wall - self.source_seen[1] < STARVE_SECS,
                Target::Mix => {
                    (0..2).any(|i| self.audio_wanted[i] && wall - self.source_seen[i] < STARVE_SECS)
                }
            };
            if source_alive {
                // The source is delivering, just behind in media time (a
                // backlog burst). Its own samples must fill this range; filler
                // here would push the track late and splice the burst.
                continue;
            }
            let start = track.next.unwrap_or(0.0);
            // Only a track that is genuinely overdue gets filler: a source
            // that is merely late (under load, a burst of past-pts chunks) must
            // get its own samples into the file first, or the filler would
            // push the whole track late (measured: 11-60 ms under load) and
            // splicing at the filler edge clicked.
            if out_time - start < STARVE_SECS {
                continue;
            }
            let mut at = start;
            while at + SNAP_SECS < out_time {
                let secs = (out_time - at).min(SILENCE_CHUNK_SECS);
                let n = (secs * mixer::RATE).round() as usize;
                if n == 0 {
                    break;
                }
                if !push_track_samples(track, at, &vec![[0.0, 0.0]; n], track.channels as usize) {
                    failed = true;
                    break;
                }
                at += n as f64 / mixer::RATE;
                filled += 1;
            }
            track.next = Some(at);
            track.after_filler = true;
        }
        self.stats.audio_gaps_filled += filled;
        if filled > 0 {
            self.check_bus();
        }
        if failed {
            self.stats.audio_drops += 1;
        }
    }

    /// Write 48 kHz stereo frames to the track that holds `target`, keeping it
    /// on the output timeline (a gap becomes silence, an overlap is trimmed).
    /// Returns true when a buffer was written; the caller counts a track that
    /// never received one (qtmux only writes a trak once it has a sample).
    fn write_track(&mut self, target: Target, pts: f64, frames: &[[f32; 2]]) -> bool {
        if frames.is_empty() {
            return false;
        }
        let limit_secs = self.limit.map(|l| l as f64 / self.spec.fps as f64);
        let Some(sink) = self.sink.as_mut() else {
            return false;
        };
        let Some(track) = sink.tracks.iter_mut().find(|t| t.target == target) else {
            return false;
        };
        let rate = mixer::RATE;
        // The track continues from the last buffer written: fill a gap (late
        // start, dropped device buffer) with silence and trim an overlap, both
        // sample-accurate. Error stays under SNAP_SECS.
        let mut start = track.next.unwrap_or(0.0);
        let gap = pts - start;
        let mut pad = 0usize;
        let mut skip = 0usize;
        if gap > MAX_GAP_SECS {
            // A timestamp jump (a device clock, not a real gap): filling the
            // gap with silence would allocate it. A device that started late
            // (up to MAX_RESYNC_SECS) begins its track here; anything further
            // is a broken clock and the chunk is dropped, so a 1000 s jump
            // cannot stretch a 2 s recording.
            self.stats.audio_resyncs += 1;
            if gap > MAX_RESYNC_SECS {
                return false;
            }
            start = pts;
            track.next = Some(pts);
        } else if gap > SNAP_SECS {
            pad = (gap * rate).round() as usize;
        } else if gap < -SNAP_SECS {
            skip = (-gap * rate).round() as usize;
            if skip >= frames.len() {
                return false;
            }
            if track.after_filler {
                // The range this chunk covers was already written as filler
                // (the source was overdue and burst afterwards). Dropping the
                // whole chunk keeps an intact signal from the next one instead
                // of splicing this one mid-beep, which clicked and measured
                // 2-4 dB hot.
                return false;
            }
        }
        let body = &frames[skip..];
        // `--duration` cuts every track at the same output time, so the file
        // ends where the video grid does.
        let body = if let Some(limit) = limit_secs {
            if start >= limit {
                return false;
            }
            pad = pad.min(((limit - start) * rate).floor().max(0.0) as usize);
            let body_start = start + pad as f64 / rate;
            if body_start >= limit {
                return false;
            }
            let room = ((limit - body_start) * rate).floor().max(0.0) as usize;
            &body[..body.len().min(room)]
        } else {
            body
        };
        if body.is_empty() && pad == 0 {
            return false;
        }
        let body_start = start + pad as f64 / rate;
        let mut ok = true;
        if pad > 0 {
            ok &= push_track_samples(track, start, &vec![[0.0, 0.0]; pad], 2);
            self.stats.audio_gaps_filled += 1;
        }
        // Fade in when the signal resumes after filler, so silence-to-audio at
        // an arbitrary phase is not a step.
        let after_filler = track.after_filler || pad > 0;
        let faded;
        let body = if after_filler && !body.is_empty() {
            let fade = ((FILLER_FADE_SECS * rate) as usize).min(body.len()).max(1);
            let mut f = body.to_vec();
            for (i, s) in f.iter_mut().enumerate().take(fade) {
                let g = i as f32 / fade as f32;
                s[0] *= g;
                s[1] *= g;
            }
            faded = f;
            &faded[..]
        } else {
            body
        };
        if !body.is_empty() {
            ok &= push_track_samples(track, body_start, body, track.channels as usize);
        }
        if ok {
            track.next = Some(body_start + body.len() as f64 / rate);
            if !body.is_empty() {
                track.after_filler = false;
            }
        } else {
            self.check_bus();
        }
        ok
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

    /// True while the video queue has room (the GStreamer stand-in for
    /// AVAssetWriterInput's `isReadyForMoreMediaData`).
    fn video_ready(&self) -> bool {
        let Some(sink) = self.sink.as_ref() else {
            return true;
        };
        let level = sink
            .video_src
            .property_value("current-level-buffers")
            .get::<u64>()
            .or_else(|_| {
                sink.video_src
                    .property_value("current-level-buffers")
                    .get::<u32>()
                    .map(u64::from)
            })
            .unwrap_or(0);
        let bytes = sink
            .video_src
            .property_value("current-level-bytes")
            .get::<u64>()
            .unwrap_or(0);
        level < VIDEO_QUEUE_FRAMES && bytes < sink.video_max_bytes
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
            // Encoder busy (a GPU spike, a slow software encode): retry this
            // slot on the next pacer pass unless it is already too far behind.
            if !self.video_ready() && slot_t > until - MAX_BACKLOG_SECS {
                self.stats.video_retries += 1;
                break;
            }
            let paused = self.pauses.offset_at(slot as f64).is_none();
            let past_limit = self.limit.is_some_and(|l| self.out_slot >= l);
            self.next_slot += 1;
            let before = self.screen_q.len();
            Self::pick(&mut self.screen_q, &mut self.screen_last, slot_t + half);
            let fresh = self.screen_q.len() != before;
            Self::pick(&mut self.cam_q, &mut self.cam_last, slot_t + half);
            let write = !paused && !past_limit;
            if !write {
                self.stats.paused_slots += 1;
            }
            // The preview and the live encode get every frame, paused or not.
            let want_preview = self.preview.is_some() && slot_t - self.last_preview >= 1.0 / 30.0;
            let want_live = self.live.as_ref().is_some_and(|l| l.active());
            if !write && !want_preview && !want_live {
                continue;
            }
            let Some(frame) = self.compose(slot_t) else {
                continue;
            };
            if write {
                let out_slot = self.out_slot;
                let refused = !self.video_ready();
                self.out_slot += 1;
                if !refused && self.push_video_buffer(out_slot, &frame) {
                    self.stats.video_frames_out += 1;
                    if !fresh {
                        self.stats.video_dups += 1;
                    }
                } else {
                    self.stats.video_drops += 1;
                    self.check_bus();
                }
            }
            if want_live && let Some(l) = self.live.clone() {
                // The live encode places frames on the real grid, so a paused
                // recording does not stall it.
                if self.live_t0.is_none() {
                    self.live_t0 = Some(slot_t);
                }
                let mut stamped = frame.as_ref().clone();
                stamped.pts = slot_t;
                l.push_video(&stamped);
            }
            if want_preview && let Some(p) = self.preview.clone() {
                self.last_preview = slot_t;
                p(frame.as_ref().clone());
            }
        }
        self.top_up_audio(self.out_slot as f64 / self.spec.fps as f64);
    }

    /// The output frame for a slot: the compositor's result when phase 1C is
    /// available, else the current CPU path (the screen frame; the camera
    /// frame for a camera-only scene).
    fn compose(&mut self, slot_t: f64) -> Option<Arc<VideoFrame>> {
        let screen = self.screen_last.clone();
        let cam = self.cam_last.clone();
        if self.renderer.is_some() {
            let started = std::time::Instant::now();
            let rendered =
                self.renderer
                    .as_mut()
                    .unwrap()
                    .render(screen.as_deref(), cam.as_deref(), slot_t);
            let took = started.elapsed().as_secs_f64();
            match rendered {
                Ok(Some(frame)) => {
                    self.note_compose_cost(took);
                    let frame = Arc::new(frame);
                    self.composed_last = Some(frame.clone());
                    return Some(frame);
                }
                // Only with a pipelined readback (the writer uses depth 1):
                // the previous composited frame keeps the grid exact.
                Ok(None) => return self.composed_last.clone(),
                Err(e) => {
                    // A compositor failure must not stop the recording: fall
                    // back to the CPU path and say so once.
                    if !self.warned_overlay {
                        self.warned_overlay = true;
                        eprintln!(
                            "rec: warning: compositing failed ({e}); recording without the camera overlay"
                        );
                    }
                    self.renderer = None;
                }
            }
        }
        match self.scene {
            Scene::Screen => screen,
            Scene::Cam => cam.or(screen),
            Scene::ScreenCam => {
                if !self.warned_overlay && cam.is_some() {
                    self.warned_overlay = true;
                    eprintln!(
                        "rec: warning: the camera overlay needs the GPU compositor, which is \
                         unavailable on this host; recording the screen alone"
                    );
                }
                screen.or(cam)
            }
        }
    }

    /// Track the compositor's cost. On a software device (lavapipe) a 4K/5K
    /// screen can take 45-50 ms a frame, far past the frame budget; the writer
    /// then falls back to the CPU path (videoscale + no overlay) instead of
    /// dropping every other slot. A real GPU never trips this.
    fn note_compose_cost(&mut self, took: f64) {
        self.compose_ema = if self.compose_ema == 0.0 {
            took
        } else {
            self.compose_ema * 0.9 + took * 0.1
        };
        let budget = 0.8 / self.spec.fps as f64;
        let software = self.renderer.as_ref().is_some_and(|r| r.is_software());
        if software && self.compose_ema > budget {
            self.slow_frames += 1;
            if self.slow_frames == 30 {
                let adapter = self
                    .renderer
                    .as_ref()
                    .map(|r| r.describe_adapter())
                    .unwrap_or_default();
                eprintln!(
                    "rec: warning: the software compositor ({adapter}) needs {:.1} ms per frame, \
                     over the {:.1} ms budget at {} fps; using the CPU scaler without the overlay \
                     so frames are not dropped",
                    self.compose_ema * 1000.0,
                    budget * 1000.0,
                    self.spec.fps
                );
                self.renderer = None;
            }
        } else {
            self.slow_frames = 0;
        }
    }

    /// Announce the frame's size and format to the appsrc when they change.
    ///
    /// The pipeline pins the *output* size after the scaler, so a source that
    /// delivers its native size (a 1440p display into a 1080p recording) is
    /// accepted and scaled with Lanczos.
    fn ensure_video_caps(&mut self, frame: &VideoFrame) {
        let Some(sink) = self.sink.as_ref() else {
            return;
        };
        let key = (frame.width, frame.height, frame.format);
        if self.caps_key == Some(key) {
            return;
        }
        let format = match frame.format {
            PixelFormat::Bgra => "BGRA",
            PixelFormat::Rgba => "RGBA",
            PixelFormat::Nv12 => "NV12",
            PixelFormat::I420 => "I420",
        };
        let caps = gst::Caps::builder("video/x-raw")
            .field("format", format)
            .field("width", frame.width as i32)
            .field("height", frame.height as i32)
            .field("framerate", gst::Fraction::new(self.spec.fps as i32, 1))
            .build();
        sink.video_src.set_caps(Some(&caps));
        self.caps_key = Some(key);
    }

    fn push_video_buffer(&mut self, out_slot: i64, frame: &VideoFrame) -> bool {
        if self.sink.is_none() {
            // Preview-only session: counted as written, nothing is encoded.
            return true;
        }
        self.ensure_video_caps(frame);
        let bytes = tight_bytes(frame);
        let Some(bytes) = bytes else { return false };
        let mut buffer = gst::Buffer::from_mut_slice(bytes);
        {
            let b = buffer.get_mut().unwrap();
            let ns = slot_ns(out_slot, self.spec.fps);
            b.set_pts(gst::ClockTime::from_nseconds(ns));
            b.set_duration(gst::ClockTime::from_nseconds(
                slot_ns(out_slot + 1, self.spec.fps) - ns,
            ));
        }
        let sink = self.sink.as_ref().unwrap();
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
        let failed = self.failed.clone();
        if self.t0.is_none() {
            self.abort();
            bail!("no video frames were captured, nothing written");
        }
        self.emit_until(end);
        // Frames still in flight on the compositor's GPU pipeline (depth 1
        // keeps none, but the API must be drained).
        if let Some(r) = self.renderer.as_mut() {
            let _ = r.flush();
        }
        // The last pieces of audio, then the end of the streams.
        self.drain_mix(true);
        self.drain_live(true);
        let Some(mut sink) = self.sink.take() else {
            // Preview-only session.
            return Ok(self.stats.clone());
        };
        // A track that never got audio would make an empty track: qtmux only
        // writes a trak once it has a sample, so a short clip with a slow
        // source lost it silently. Give every requested track a moment of
        // silence and say so; the layout is then intact whatever the device did.
        for t in &mut sink.tracks {
            let written = match t.target {
                Target::Mix => self.stats.audio_mix_buffers,
                Target::System => self.stats.audio_sys_buffers,
                Target::Mic => self.stats.audio_mic_buffers,
            };
            if written == 0 {
                let what = match t.target {
                    Target::Mix => "mix",
                    Target::System => "system audio",
                    Target::Mic => "microphone",
                };
                eprintln!(
                    "rec: warning: no {what} arrived; writing a silent {what} track so the layout is intact"
                );
                let _ = push_track_samples(t, 0.0, &[[0.0, 0.0]; 480], t.channels as usize);
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
        if let Err(e) = &result {
            // Keep the file: a fragmented recording is playable up to the last
            // fragment even when the muxer or an encoder failed.
            eprintln!("rec: warning: the movie could not be finalized: {e}");
            eprintln!("rec: warning: {} is still playable", sink.path.display());
            if let Some(e) = failed {
                bail!("writer failed: {e}");
            }
            return Ok(self.stats.clone());
        }
        // The last step: a regular movie instead of fragments, plus the track
        // layout (alternate group, enabled flags, AAC priming edit lists).
        let crash = match std::env::var("REC_DEFRAG_CRASH").as_deref() {
            Ok("before-flip") => mp4::CrashAt::BeforeFlip,
            Ok("mid-flip") => mp4::CrashAt::MidFlip,
            _ => mp4::CrashAt::None,
        };
        let keep = std::env::var_os("REC_DEFRAG_KEEP").map(PathBuf::from);
        if std::env::var("REC_DEFRAG_OFF").is_ok_and(|v| v != "0") {
            return Ok(self.stats.clone());
        }
        match mp4::finalize(&sink.path, &sink.layout, crash, keep.as_deref()) {
            Ok(report) => {
                self.stats.finalize_millis = report.millis;
                self.stats.fragments = report.fragments;
                if std::env::var_os("REC_DEBUG").is_some() {
                    eprintln!(
                        "rec: debug: finalise {} ({} fragments, {} samples, {} tracks, {} B in, moov {} B) in {} ms",
                        if report.defragmented {
                            "defragmented"
                        } else {
                            "patched"
                        },
                        report.fragments,
                        report.samples,
                        report.tracks,
                        report.input_bytes,
                        report.moov_bytes,
                        report.millis
                    );
                }
            }
            Err(e) => {
                // Never delete or truncate: the fragmented file still plays.
                eprintln!(
                    "rec: warning: could not turn the fragmented file into a regular movie ({e})"
                );
                eprintln!(
                    "rec: warning: {} is complete up to its last fragment and still plays",
                    sink.path.display()
                );
            }
        }
        if let Some(e) = failed {
            bail!("writer failed: {e}");
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

/// Multiply a fader envelope into stereo frames.
///
/// No clamping: upstream's float path lets a boosted stem exceed full scale
/// (the encoder and the player decide what to do), and the mix bus has its own
/// limiter. Clamping here would hard-clip a +dB stem that upstream leaves
/// alone.
fn apply_gain(frames: &mut [[f32; 2]], env: &[f32]) {
    for (f, g) in frames.iter_mut().zip(env) {
        f[0] *= g;
        f[1] *= g;
    }
}

/// The frame's planes as one tightly packed buffer, in the plane order the
/// format defines (what GStreamer expects without a `GstVideoMeta`).
fn tight_bytes(frame: &VideoFrame) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(frame.byte_len());
    for (i, plane) in frame.planes.iter().enumerate() {
        let rows = frame.format.plane_rows(i, frame.height);
        let stride = frame.format.min_stride(i, frame.width);
        if plane.stride == stride {
            let len = stride * rows;
            if plane.data.len() < len {
                return None;
            }
            out.extend_from_slice(&plane.data[..len]);
        } else {
            if plane.stride == 0 || plane.data.len() < plane.stride * rows {
                return None;
            }
            for row in 0..rows {
                let at = row * plane.stride;
                out.extend_from_slice(&plane.data[at..at + stride]);
            }
        }
    }
    Some(out)
}

/// Push 48 kHz frames to one track, interleaved with `channels` channels.
/// A mono track keeps the first channel only (the microphone's own sample).
fn push_track_samples(
    track: &mut AudioTrack,
    pts: f64,
    frames: &[[f32; 2]],
    channels: usize,
) -> bool {
    let mut bytes = Vec::with_capacity(frames.len() * 4 * channels);
    for f in frames {
        bytes.extend_from_slice(&f[0].to_le_bytes());
        if channels > 1 {
            bytes.extend_from_slice(&f[1].to_le_bytes());
        }
    }
    let mut buffer = gst::Buffer::from_mut_slice(bytes);
    {
        let b = buffer.get_mut().unwrap();
        b.set_pts(gst::ClockTime::from_nseconds(secs_ns(pts)));
        b.set_duration(gst::ClockTime::from_nseconds(secs_ns(
            frames.len() as f64 / mixer::RATE,
        )));
    }
    // A full queue (the muxer stalled behind a broken stream) is a dropped
    // buffer, not a blocked capture thread; the caller counts it.
    let queued = track
        .appsrc
        .property_value("current-level-bytes")
        .get::<u64>()
        .unwrap_or(0);
    if queued >= AUDIO_QUEUE_BYTES {
        return false;
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
            "avenc_libx264",
            "avenc_h264",
        ],
        Codec::Hevc => &[
            "vah265enc",
            "vaapih265enc",
            "x265enc",
            "avenc_libx265",
            "avenc_hevc",
        ],
        // ffmpeg has two ProRes encoders: the "Apple" one (prores_aw, also
        // registered as avenc_prores) is several times faster than Kostya's
        // (prores_ks) and is what a CPU can record in real time; ks stays as
        // the fallback for a build without it.
        Codec::Prores => &["avenc_prores_aw", "avenc_prores", "avenc_prores_ks"],
    }
}

/// AAC encoder elements to try, most capable first.
///
/// `avenc_aac` is preferred over `fdkaacenc`: measured on a 40 ms 1 kHz beep
/// train, fdkaacenc reproduces the first beep exactly but encodes the later
/// ones 1.2-1.5 dB hot (a pure tone through it standalone is exact, so this is
/// its behaviour on the gated stream a recording produces), and a recorder
/// with faders must not change levels. ffmpeg's encoder is level-accurate and
/// its 1024-sample priming is what the edit list skips.
pub const AUDIO_ENCODER_CANDIDATES: &[&str] = &["avenc_aac", "fdkaacenc", "voaacenc", "faac"];

/// Samples an AAC encoder holds back before it emits its first frame (its
/// priming, the MDCT look-ahead). The file keeps those frames and the finalise
/// writes an edit list whose `media_time` is this many samples, so players
/// start at the first real sample instead of 42.7 ms late (what AVFoundation
/// does upstream).
///
/// Measured for each element in dev/ELEMENTS.md's image (10 ms click at the
/// start, decoded again and its onset read back): fdkaacenc 2048, avenc_aac
/// 1024. `REC_AAC_PRIMING` overrides it (0 disables the edit list).
pub fn aac_priming(element: &str) -> u64 {
    match element {
        "fdkaacenc" => 2048,
        "avenc_aac" | "faac" => 1024,
        "voaacenc" => 0,
        _ => 1024,
    }
}

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

/// The audio tracks to write, in file order, per `--audio-tracks` and the
/// sources the session has (upstream's rule).
fn audio_plan(system: bool, mic: bool, layout: AudioLayout) -> Vec<(Target, u32, u64)> {
    let both = system && mic;
    let want_mix = both && layout != AudioLayout::Stems;
    let want_stems = !both || layout != AudioLayout::Mix;
    let mut plan = vec![];
    if want_mix {
        plan.push((Target::Mix, 2, 256_000));
    }
    if system && want_stems {
        plan.push((Target::System, 2, 256_000));
    }
    if mic && want_stems {
        plan.push((Target::Mic, 1, 192_000));
    }
    plan
}

/// Build the first installed encoder from `candidates` and set its properties.
fn make_encoder(
    candidates: &[&str],
    bitrate_bps: u64,
    fps: Option<u32>,
    label: &str,
    probe_format: Option<&str>,
    wide: bool,
) -> Result<(gst::Element, String)> {
    for name in candidates {
        // VA-API needs a render node: without /dev/dri the plugin registers no
        // elements at all (dev/ELEMENTS.md), and with a node but a broken driver
        // the trial encode below fails and the next candidate is used.
        if (name.starts_with("vah") || name.starts_with("vaapi"))
            && !std::path::Path::new("/dev/dri").exists()
        {
            continue;
        }
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
                    // Real-time screen recording: a 4K frame gets the faster
                    // preset, 1080p the better-looking one.
                    let preset = if wide { "superfast" } else { "veryfast" };
                    el.set_property_from_str("speed-preset", preset);
                }
            }
            "x265enc" => {
                // x265's zerolatency tune also disables B-frames and the
                // lookahead, which keeps the frame grid simple and the encoder
                // real-time (a CPU cannot do 4K60 any slower); ultrafast is
                // what a software screen recorder needs.
                set(&el, "tune", "zerolatency");
                set(&el, "speed-preset", "ultrafast");
                // ABR alone lets x265 run away on very noisy frames (83 Mb/s
                // for a requested 8 Mb/s was measured); a half-second VBV pins
                // it to the budget (8.7-9.7 Mb/s, dev/ELEMENTS.md).
                let kbps = (bitrate_bps / 1000).max(1);
                set(
                    &el,
                    "option-string",
                    format!(
                        "vbv-maxrate={kbps}:vbv-bufsize={}:rc-lookahead=0:bframes=0",
                        (kbps / 2).max(1)
                    ),
                );
            }
            "vah264enc" | "vah265enc" if el.find_property("rate-control").is_some() => {
                el.set_property_from_str("rate-control", "cbr");
            }
            // ffmpeg's ProRes encoders default to 422 HQ; upstream writes
            // plain 422 ("standard"), and editors expect that. ProRes is
            // intra-only and a 1080p60 frame needs all the parallelism it can
            // get: prores_ks goes from 3.5 to 15 fps with slice threads.
            "avenc_prores_ks" | "avenc_prores_aw" | "avenc_prores" => {
                set(&el, "profile", "standard");
                // ffmpeg's ProRes encoders only use slice threading when a
                // slice count is set; with threads=auto alone the encoder is
                // 3.5 fps at 1080p60, with slices it is far above real time.
                if el.find_property("threads").is_some() {
                    set(&el, "threads", "auto");
                }
                if el.find_property("slices").is_some() {
                    set(&el, "slices", 8);
                }
            }
            _ => {}
        }
        if let Some(format) = probe_format
            && (name.starts_with("vah") || name.starts_with("vaapi"))
            && !trial_encode(&el, format)
        {
            continue;
        }
        if std::env::var_os("REC_DEBUG").is_some() {
            // x265 prints its banner (rate control, bitrate) at info level.
            if *name == "x265enc" {
                set(&el, "log-level", 4);
            }
            let mut dump = vec![];
            for prop in [
                "bitrate",
                "tune",
                "speed-preset",
                "key-int-max",
                "gop-size",
                "qp",
            ] {
                if el.find_property(prop).is_some() {
                    dump.push(format!("{prop}={:?}", el.property_value(prop)));
                }
            }
            eprintln!("rec: debug: encoder {name}: {}", dump.join(" "));
        }
        return Ok((el, name.to_string()));
    }
    bail!(
        "no {label} encoder is installed: tried {}. Install one (x264enc comes with RPM Fusion's \
         gst-plugins-ugly, x265enc with gst-plugins-bad-freeworld; openh264enc is the free-repository \
         fallback for H.264 and there is no HEVC encoder without RPM Fusion)",
        candidates.join(", ")
    )
}

/// Encode a couple of test frames through a candidate element, to prove it can
/// actually run. A VA-API encoder whose driver cannot open a display fails here
/// instead of failing the recording.
///
/// Returns false on any error or after 3 seconds.
fn trial_encode(el: &gst::Element, format: &str) -> bool {
    let caps = gst::Caps::builder("video/x-raw")
        .field("format", format)
        .field("width", 320i32)
        .field("height", 240i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .build();
    let pipeline = gst::Pipeline::new();
    let Ok(src) = gst::ElementFactory::make("videotestsrc").build() else {
        return false;
    };
    set(&src, "num-buffers", 3);
    let Ok(convert) = gst::ElementFactory::make("videoconvert").build() else {
        return false;
    };
    let Ok(filter) = gst::ElementFactory::make("capsfilter").build() else {
        return false;
    };
    filter.set_property("caps", &caps);
    let Ok(sink) = gst::ElementFactory::make("fakesink").build() else {
        return false;
    };
    set(&sink, "sync", "false");
    if pipeline
        .add_many([&src, &convert, &filter, el, &sink])
        .is_err()
    {
        return false;
    }
    if gst::Element::link_many([&src, &convert, &filter, el, &sink]).is_err() {
        return false;
    }
    if pipeline.set_state(gst::State::Playing).is_err() {
        let _ = pipeline.set_state(gst::State::Null);
        return false;
    }
    let Some(bus) = pipeline.bus() else {
        let _ = pipeline.set_state(gst::State::Null);
        return false;
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut ok = false;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_nseconds(left.as_nanos() as u64),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) else {
            continue;
        };
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(_) => {
                ok = true;
                break;
            }
            MessageView::Error(err) => {
                if std::env::var_os("REC_DEBUG").is_some() {
                    eprintln!("rec: debug: {} cannot encode: {}", el.name(), err.error());
                }
                break;
            }
            _ => {}
        }
    }
    let _ = pipeline.set_state(gst::State::Null);
    if !ok {
        // Take the element out of the pipeline it was tested in, so it can be
        // added to the real one.
        let _ = el.set_state(gst::State::Null);
    }
    ok
}

/// Build the pipeline and the muxed output file.
fn open_sink(
    path: &Path,
    spec: &VideoSpec,
    system: bool,
    mic: bool,
    layout: AudioLayout,
) -> Result<Sink> {
    let mp4 = matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("mp4") | Some("m4v")
    );
    if spec.codec == Codec::Prores && mp4 {
        bail!("ProRes needs a .mov output");
    }
    let pipeline = gst::Pipeline::new();
    // The pixel layout handed to the encoder decides the H.264/HEVC profile:
    // x264enc with no constraint negotiates 4:4:4 and encodes "High 4:4:4
    // Predictive" instead of upstream's 4:2:0 High, and ProRes 422 needs 4:2:2
    // 10-bit. An explicit capsfilter is what pins it, and it carries the BT.709
    // tags upstream writes (primaries, transfer, matrix, limited range).
    let enc_format = match spec.codec {
        Codec::H264 | Codec::Hevc => "I420",
        Codec::Prores => "I422_10LE",
    };
    let (venc, vename) = make_encoder(
        video_encoder_candidates(spec.codec),
        spec.bitrate,
        Some(spec.fps),
        "video",
        Some(enc_format),
        spec.width * spec.height > 1920 * 1080,
    )?;

    // The appsrc announces the source's own size (a display's native 1440p for
    // a 1080p recording, the camera's size); the capsfilter pins the output.
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
    // Two frames, and never block the pacer: a full queue means the encoder is
    // behind, and the writer drops that grid slot (counted) instead of letting
    // raw 4K frames pile up (a 4 GB process was measured with an unbounded
    // appsrc behind a CPU x265).
    let frame_bytes = (spec.width * spec.height * 4) as u64;
    let video_max_bytes = VIDEO_QUEUE_FRAMES * frame_bytes;
    set(video_src.upcast_ref(), "max-buffers", VIDEO_QUEUE_FRAMES);
    set(video_src.upcast_ref(), "max-bytes", video_max_bytes);
    set(video_src.upcast_ref(), "block", false);
    set(video_src.upcast_ref(), "leaky-type", "downstream");
    let videoconvert = gst::ElementFactory::make("videoconvert").build()?;
    let videoscale = gst::ElementFactory::make("videoscale").build()?;
    set(&videoscale, "method", "lanczos");
    let scale_caps = gst::ElementFactory::make("capsfilter").build()?;
    scale_caps.set_property(
        "caps",
        gst::Caps::builder("video/x-raw")
            .field("width", spec.width as i32)
            .field("height", spec.height as i32)
            .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
            .build(),
    );
    let convert2 = gst::ElementFactory::make("videoconvert").build()?;
    let enc_capsfilter = gst::ElementFactory::make("capsfilter").build()?;
    enc_capsfilter.set_property(
        "caps",
        gst::Caps::builder("video/x-raw")
            .field("format", enc_format)
            .field("width", spec.width as i32)
            .field("height", spec.height as i32)
            .field("framerate", gst::Fraction::new(spec.fps as i32, 1))
            .field("colorimetry", "bt709")
            .build(),
    );
    let vqueue = gst::ElementFactory::make("queue").build()?;
    // Bounded but not leaky: the writer retries a slot while the encoder is
    // merely busy (upstream's `MAX_BACKLOG_SECS`) and drops only when it is
    // more than half a second behind. A leaky queue here silently lost 20% of
    // the frames in a non-fragmented muxer run.
    set(&vqueue, "max-size-buffers", VIDEO_QUEUE_FRAMES);
    set(&vqueue, "max-size-bytes", video_max_bytes);
    set(&vqueue, "max-size-time", 0);
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
    // leaves a playable file up to the last fragment. The fragments are turned
    // into a regular movie at a normal stop (`media::mp4::finalize`).
    // REC_FRAGMENT_SECS overrides it (0 = one regular movie) for comparison and
    // debugging.
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

    let mut chain: Vec<gst::Element> = vec![
        video_src.clone().upcast(),
        videoconvert,
        videoscale,
        scale_caps,
        convert2,
        enc_capsfilter,
        venc,
    ];
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
    let plan = audio_plan(system, mic, layout);
    let mut tracks = vec![];
    let mut audio_names = vec![];
    let mut priming = vec![];
    if !plan.is_empty() {
        for (n, (target, channels, bitrate)) in plan.iter().enumerate() {
            let audio_caps = gst::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .field("layout", "interleaved")
                .field("rate", mixer::RATE as i32)
                .field("channels", *channels as i32)
                .build();
            // One encoder per track: a shared one would interleave the streams.
            let (aenc, aname) =
                make_encoder(AUDIO_ENCODER_CANDIDATES, *bitrate, None, "AAC", None, false)?;
            audio_names.push(aname.clone());
            let priming_override = std::env::var("REC_AAC_PRIMING")
                .ok()
                .and_then(|v| v.parse::<u64>().ok());
            priming.push(priming_override.unwrap_or_else(|| aac_priming(&aname)));
            let src = gstreamer_app::AppSrc::builder()
                .name(format!("audio-{}", n))
                .format(gst::Format::Time)
                .caps(&audio_caps)
                .is_live(false)
                .do_timestamp(false)
                .max_bytes(1 << 20)
                .build();
            set(src.upcast_ref(), "max-buffers", AUDIO_QUEUE_BUFFERS);
            set(src.upcast_ref(), "max-bytes", AUDIO_QUEUE_BYTES);
            set(src.upcast_ref(), "block", false);
            set(src.upcast_ref(), "leaky-type", "downstream");
            let convert = gst::ElementFactory::make("audioconvert").build()?;
            let resample = gst::ElementFactory::make("audioresample").build()?;
            let aparse = gst::ElementFactory::make("aacparse").build()?;
            let q = gst::ElementFactory::make("queue").build()?;
            set(&q, "max-size-buffers", AUDIO_QUEUE_BUFFERS);
            set(&q, "max-size-bytes", AUDIO_QUEUE_BYTES);
            set(&q, "max-size-time", 0);
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
            q.static_pad("src")
                .context("audio queue src pad")?
                .link(&pad)?;
            tracks.push(AudioTrack {
                target: *target,
                appsrc: src,
                channels: *channels as u8,
                next: None,
                after_filler: false,
            });
        }
    }
    // The default track is the mix when it is there; with stems only, every
    // track stays enabled.
    let has_mix = plan.iter().any(|(t, _, _)| *t == Target::Mix);
    let grouping = has_mix && plan.len() > 1;
    let mp4_layout = mp4::Layout {
        audio_alternate_group: grouping.then_some(1),
        default_audio: grouping.then_some(0),
        audio_priming: priming,
    };
    let encoder = if tracks.is_empty() {
        vename
    } else {
        format!("{vename} + {}", audio_names.join(", "))
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
        layout: mp4_layout,
        video_max_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_cap_never_shows_future_frames() {
        // Overflowing the frame queue while emission runs behind must never
        // make a frame from the future the current one (shown early).
        let mut q: VecDeque<(f64, Arc<VideoFrame>)> = Default::default();
        let mut last: Option<Arc<VideoFrame>> = None;
        let frame = |i: i32| {
            Arc::new(VideoFrame::packed(
                i as f64,
                4,
                2,
                PixelFormat::Bgra,
                vec![i as u8; 32],
            ))
        };
        for i in 0..12 {
            q.push_back((i as f64, frame(i)));
            // Emission is at frame 2: nothing newer than 2.5 may become current.
            Writer::cap(&mut q, &mut last, 4, 2.5);
        }
        assert_eq!(
            last.as_ref().unwrap().pts,
            2.0,
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
        // The fast ffmpeg ProRes encoder first, Kostya's as the fallback.
        assert_eq!(
            video_encoder_candidates(Codec::Prores)[0],
            "avenc_prores_aw"
        );
        assert!(video_encoder_candidates(Codec::Prores).contains(&"avenc_prores_ks"));
        assert_eq!(AUDIO_ENCODER_CANDIDATES[0], "avenc_aac");
    }

    #[test]
    fn audio_plan_matches_upstreams_track_layout() {
        // Both sources, both layouts: mix first, then system, then mic.
        let both = audio_plan(true, true, AudioLayout::Both);
        assert_eq!(
            both,
            vec![
                (Target::Mix, 2, 256_000),
                (Target::System, 2, 256_000),
                (Target::Mic, 1, 192_000)
            ]
        );
        // Mix only.
        assert_eq!(
            audio_plan(true, true, AudioLayout::Mix),
            vec![(Target::Mix, 2, 256_000)]
        );
        // Stems only: no group, both enabled.
        assert_eq!(
            audio_plan(true, true, AudioLayout::Stems),
            vec![(Target::System, 2, 256_000), (Target::Mic, 1, 192_000)]
        );
        // One source: that source's stem alone, whatever the layout says.
        assert_eq!(
            audio_plan(true, false, AudioLayout::Both),
            vec![(Target::System, 2, 256_000)]
        );
        assert_eq!(
            audio_plan(false, true, AudioLayout::Mix),
            vec![(Target::Mic, 1, 192_000)]
        );
        assert!(audio_plan(false, false, AudioLayout::Both).is_empty());
    }

    #[test]
    fn fader_gain_is_applied_without_clipping() {
        // 0 dB is a pass-through: the fader returns no envelope, so the stem
        // keeps the device's samples byte for byte.
        let mut fader = Fader::new(1.0);
        assert_eq!(fader.envelope(480, mixer::RATE), None);
        // +6 dB multiplies without a clamp (upstream's float path); the mix
        // limiter is what keeps the mix under −1 dBFS, not the fader.
        let mut fader = Fader::new(1.0);
        fader.set(10f32.powf(6.0 / 20.0));
        // The ramp is 20 ms = 960 frames at 48 kHz.
        let env = fader.envelope(960, mixer::RATE).unwrap();
        let mut frames = vec![[0.6f32, -0.6f32]; 960];
        apply_gain(&mut frames, &env);
        assert!(
            (frames[959][0] - 0.6 * 10f32.powf(0.3)).abs() < 1e-3,
            "{}",
            frames[959][0]
        );
        assert!((frames[959][1] + 0.6 * 10f32.powf(0.3)).abs() < 1e-3);
        // At unity the samples are untouched, bit for bit.
        let mut frames = vec![[0.123_456_79f32, -0.5f32]; 16];
        apply_gain(&mut frames, &[1.0; 16]);
        assert_eq!(frames[0], [0.123_456_79, -0.5]);
    }

    #[test]
    fn a_muted_source_is_silent_but_still_metered() {
        let v = Volume {
            db: -6.0,
            muted: true,
        };
        assert_eq!(v.gain(), 0.0);
        assert!(
            (v.fader_gain() - 0.501_187).abs() < 1e-5,
            "meters read before mute"
        );
    }

    #[test]
    fn camera_latency_widens_the_lag_and_decays() {
        let mut w = test_writer();
        assert_eq!(w.lag(), LAG_SECS, "no measured latency: the minimum");
        // A webcam 90 ms behind: emission waits for it plus the margin.
        w.note_latency(1, 0.09, 0.0);
        assert!((w.lag() - 0.115).abs() < 1e-9, "{}", w.lag());
        // A minute later the peak has decayed away.
        w.note_latency(1, 0.0, 60.0);
        assert_eq!(w.lag(), LAG_SECS);
        // It never waits longer than MAX_LAG_SECS, even for a stalled device.
        w.note_latency(0, 5.0, 61.0);
        assert_eq!(w.lag(), MAX_LAG_SECS);
    }

    #[test]
    fn the_camera_gate_holds_the_start_until_the_camera_settles() {
        let mut w = test_writer();
        // A cold camera settling for 2 s, giving up 3 s after that.
        w.cam_gate = Some(CamGate {
            settle: 2.0,
            pending: None,
            ready_at: None,
            give_up: 10.0,
        });
        // Screen frames before the camera arrives are held (the newest wins).
        w.push_video(frame(1.0));
        w.push_video(frame(1.1));
        assert!(!w.started(), "the file has not started yet");
        assert!(w.cam_gate.is_some());
        // The first camera frame sets the ready time; frames before it do not
        // start the file.
        w.push_cam(frame(1.2));
        w.push_video(frame(1.3));
        assert!(!w.started(), "the camera is still settling");
        // A camera frame at the settle time starts the file at that instant.
        w.push_cam(frame(3.2));
        assert!(w.started());
        let t0 = w.t0.unwrap();
        assert!((t0 - 3.2).abs() < 1e-9, "{t0}");
        assert!(w.cam_gate.is_none());
    }

    #[test]
    fn the_camera_gate_gives_up_on_a_camera_that_never_delivers() {
        let mut w = test_writer();
        w.cam_gate = Some(CamGate {
            settle: 2.0,
            pending: None,
            ready_at: None,
            give_up: 5.0,
        });
        w.push_video(frame(1.0));
        assert!(!w.started());
        // A screen frame after the deadline starts the file without a camera.
        w.push_video(frame(5.5));
        assert!(w.started());
        assert!(w.cam_gate.is_none());
    }

    #[test]
    fn scenes_that_need_the_overlay_report_when_they_cannot_be_drawn() {
        let mut w = test_writer();
        // Screen always works; without a compositor or a camera, neither
        // camera scene can be shown.
        assert!(w.set_scene(Scene::Screen));
        assert!(!w.set_scene(Scene::Cam));
        assert!(!w.set_scene(Scene::ScreenCam));
        // Once camera frames arrive, the full-frame camera scene works on the
        // CPU path (the overlay still needs the compositor).
        w.push_cam(frame(1.0));
        assert!(w.set_scene(Scene::Cam));
        assert_eq!(w.scene, Scene::Cam);
        assert!((w.cam_aspect().unwrap() - 2.0).abs() < 1e-9, "4x2 frame");
        assert!(!w.set_scene(Scene::ScreenCam));
        assert_eq!(w.scene, Scene::Cam);
    }

    #[test]
    fn a_camera_only_scene_emits_camera_frames() {
        let mut w = test_writer();
        w.set_camera_expected(true);
        assert!(w.set_scene(Scene::Cam));
        w.push_cam(frame(1.0));
        assert!(w.started(), "the camera frame started the clock");
        w.emit_until(1.0);
        // No sink (preview-only), so emission counts slots: the run works and
        // the camera frame became the current one.
        assert!(w.cam_last.is_some());
        assert_eq!(w.stats.video_frames_out, 1);
    }

    #[test]
    fn pauses_keep_audio_cut_at_sample_boundaries() {
        // The writer's audio cut uses the pause timeline; a buffer straddling
        // a pause keeps only the part outside it (a unit-level check of the
        // maths `push_audio` uses, with a 60 fps grid).
        let mut p = Pauses::default();
        p.pause(60);
        p.resume(120);
        // A buffer spanning slots 59.5..60.5 keeps the part before the pause.
        let (a, b) = (59.5, 60.5);
        let (x, y, _) = p.keep(a, b).unwrap();
        assert_eq!((x, y), (59.5, 60.0));
    }

    #[test]
    fn tight_bytes_pads_and_truncates_strides() {
        // A BGRA frame whose rows carry padding: the encoder gets tight rows.
        let padded = VideoFrame {
            pts: 0.0,
            width: 4,
            height: 2,
            format: PixelFormat::Bgra,
            planes: vec![crate::media_types::Plane::new(
                (0..24u8).collect::<Vec<u8>>(),
                12, // 4 pixels = 16 bytes used, 12 stride is too small
            )],
        };
        // A too-small stride is not "well formed", but the writer must reject
        // it rather than read out of bounds.
        assert!(!padded.is_well_formed());
        let padded = VideoFrame {
            pts: 0.0,
            width: 4,
            height: 2,
            format: PixelFormat::Bgra,
            planes: vec![crate::media_types::Plane::new(
                (0..32u8).collect::<Vec<u8>>(),
                16,
            )],
        };
        assert_eq!(
            tight_bytes(&padded).unwrap(),
            (0..32u8).collect::<Vec<u8>>()
        );
        // I420: three planes, concatenated.
        let i420 = VideoFrame {
            pts: 0.0,
            width: 4,
            height: 4,
            format: PixelFormat::I420,
            planes: vec![
                crate::media_types::Plane::new(vec![1u8; 16], 4),
                crate::media_types::Plane::new(vec![2u8; 4], 2),
                crate::media_types::Plane::new(vec![3u8; 4], 2),
            ],
        };
        let bytes = tight_bytes(&i420).unwrap();
        assert_eq!(bytes.len(), 24);
        assert_eq!(&bytes[..16], &[1u8; 16]);
        assert_eq!(&bytes[16..20], &[2u8; 4]);
        assert_eq!(&bytes[20..], &[3u8; 4]);
    }

    fn frame(pts: f64) -> VideoFrame {
        VideoFrame::packed(pts, 4, 2, PixelFormat::Bgra, vec![0; 32])
    }

    fn test_writer() -> Writer {
        Writer::new(
            None,
            &VideoSpec {
                width: 320,
                height: 180,
                fps: 60,
                codec: Codec::H264,
                bitrate: 1_000_000,
            },
            true,
            true,
            AudioLayout::Both,
            [Volume::UNITY; 2],
            Scene::Screen,
            PipSpec::default(),
        )
        .unwrap()
    }
    /// Resident set size in kB, from /proc (Linux only).
    fn vm_rss_kb() -> u64 {
        let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
            return 0;
        };
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                return rest
                    .trim()
                    .trim_end_matches("kB")
                    .trim()
                    .parse()
                    .unwrap_or(0);
            }
        }
        0
    }

    fn frame_at(pts: f64, width: usize, height: usize) -> VideoFrame {
        VideoFrame::packed(
            pts,
            width,
            height,
            PixelFormat::Bgra,
            vec![0u8; width * height * 4],
        )
    }

    /// A recording must never buffer without bound: a source that is silent
    /// for the whole session, and a source whose timestamps jump far into the
    /// future, both have to leave resident memory flat. Before the bounds were
    /// added a timestamp jump made the mix allocate a sample per second of the
    /// jump (a 10 GB process was measured).
    #[test]
    fn a_silent_or_far_future_source_cannot_grow_memory() {
        let t0 = crate::media_types::now();
        let mut w = Writer::new(
            None,
            &VideoSpec {
                width: 1920,
                height: 1080,
                fps: 60,
                codec: Codec::H264,
                bitrate: 8_000_000,
            },
            true,
            true,
            AudioLayout::Both,
            [Volume::UNITY; 2],
            Scene::Screen,
            PipSpec::default(),
        )
        .unwrap();
        // The live tap is the other mix bus: it must be bounded too.
        w.set_live(Arc::new(crate::live::Live::new()));
        let before = vm_rss_kb();
        let mut audio = 0usize;
        // Thirty seconds of 1080p60, fed as fast as the CPU can.
        for i in 0..(30 * 60) {
            let t = t0 + i as f64 / 60.0;
            w.push_video(frame_at(t, 1920, 1080));
            // System audio every 10 ms, on time.
            while t0 + audio as f64 * 0.01 <= t {
                let at = t0 + audio as f64 * 0.01;
                w.push_audio(
                    AudioKind::System,
                    &AudioChunk::stereo(at, 48_000, vec![0.001; 960]),
                );
                audio += 1;
            }
            // The mic never delivers for the first half; then it delivers with
            // a clock 1000 s ahead (the pathological case).
            if i > 15 * 60 {
                let at = t0 + 1000.0 + i as f64 * 0.01;
                w.push_audio(
                    AudioKind::Mic,
                    &AudioChunk::stereo(at, 48_000, vec![0.001; 960]),
                );
            }
            w.emit_until(t - w.lag());
        }
        let after = vm_rss_kb();
        let growth = after.saturating_sub(before);
        // The 30 s of 1080p frames are 8 MB each, so an unbounded queue would
        // be tens of GB; the bound is a few queued frames plus audio.
        assert!(
            growth < 300_000,
            "resident memory grew by {growth} kB ({before} -> {after} kB) over 30 s of video"
        );
        assert!(
            w.stats.audio_resyncs + w.stats.audio_overruns > 0,
            "the far-future audio was counted as a clock jump"
        );
        assert!(w.stats.video_frames_out > 0, "video was emitted");
    }

    /// A broken microphone clock must not stretch the file either: a chunk
    /// 1000 s ahead is dropped, not written 1000 s into the audio track. The
    /// file stays as long as the video and the memory stays flat.
    #[test]
    fn a_broken_audio_clock_cannot_stretch_the_file() {
        let _ = gst::init();
        let dir = std::env::temp_dir().join(format!("rec-rs-clock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clock.mov");
        let _ = std::fs::remove_file(&path);
        let spec = VideoSpec {
            width: 320,
            height: 180,
            fps: 30,
            codec: Codec::H264,
            bitrate: 1_000_000,
        };
        let mut w = Writer::new(
            Some(&path),
            &spec,
            true,
            true,
            AudioLayout::Both,
            [Volume::UNITY; 2],
            Scene::Screen,
            PipSpec::default(),
        )
        .unwrap();
        w.limit_secs(Some(2.0));
        let before = vm_rss_kb();
        let t0 = crate::media_types::now();
        let end = t0 + 2.0;
        let mut audio = 0usize;
        for i in 0..60 {
            let t = t0 + i as f64 / 30.0;
            // Let the encoder catch up, then feed this slot.
            let deadline = std::time::Instant::now() + Duration::from_millis(500);
            while !w.video_ready() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }
            w.push_video(frame_at(t, 320, 180));
            while t0 + audio as f64 * 0.01 <= t {
                let at = t0 + audio as f64 * 0.01;
                w.push_audio(
                    AudioKind::System,
                    &AudioChunk::stereo(at, 48_000, vec![0.05; 960]),
                );
                audio += 1;
            }
            // The mic's clock is 1000 s ahead, every buffer.
            w.push_audio(
                AudioKind::Mic,
                &AudioChunk::stereo(t0 + 1000.0 + i as f64 * 0.01, 48_000, vec![0.05; 960]),
            );
            w.emit_until(t);
        }
        // Give the encoder time to take the last slots.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while w.stats.video_frames_out < 60 && std::time::Instant::now() < deadline {
            // Always call emit_until: even when the video queue is full it
            // tops up the audio tracks with silence, which is what unblocks a
            // muxer waiting for a stream that never delivered.
            w.emit_until(end);
            std::thread::sleep(Duration::from_millis(2));
        }
        let stats = w.finish(end).unwrap();
        let after = vm_rss_kb();
        assert!(stats.audio_resyncs > 0, "the jump was counted");
        assert!(stats.audio_overruns > 0, "the mix bus refused the jump");
        // A source that never delivers makes the muxer wait for it; the
        // writer's answer is to drop grid slots (counted, memory flat), not to
        // buffer. The session avoids creating such a track at all when a
        // device fails to open (record.rs).
        assert!(stats.video_frames_out > 0, "video was written");
        // A black-hole source (opened, but never delivers) still makes qtmux
        // wait for it until the fragment boundary; the writer's contract is
        // that it drops grid slots (counted) and keeps memory flat, not that
        // every slot flows. `record.rs` does not create a track at all when a
        // device fails to open.
        assert!(stats.video_frames_out > 0, "video was written");
        assert!(
            stats.video_frames_out + stats.video_drops > 0,
            "slots are written or counted as dropped"
        );
        let info = crate::media::mp4::read(&path).expect("the file reads back");
        let secs = info.duration as f64 / info.movie_timescale.max(1) as f64;
        assert!(
            secs < 5.0,
            "the file is {secs:.3}s, not stretched to the jump"
        );
        assert!(
            after.saturating_sub(before) < 300_000,
            "memory grew {} kB",
            after.saturating_sub(before)
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
    /// A real PipeWire microphone starts after its stream reaches Streaming
    /// and delivers 1024/2048-frame buffers, so its first chunk is late and
    /// its sizes are not 10 ms. Such a source must still fill its track (the
    /// 1B `pattern-mic` regression was a finalise bug, but this keeps the
    /// writer side honest too).
    #[test]
    fn a_late_first_chunk_with_irregular_sizes_still_fills_the_track() {
        let _ = gst::init();
        let dir = std::env::temp_dir().join(format!("rec-rs-late-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("late.mov");
        let _ = std::fs::remove_file(&path);
        let spec = VideoSpec {
            width: 320,
            height: 180,
            fps: 30,
            codec: Codec::H264,
            bitrate: 1_000_000,
        };
        let mut w = Writer::new(
            Some(&path),
            &spec,
            false,
            true,
            AudioLayout::Stems,
            [Volume::UNITY; 2],
            Scene::Screen,
            PipSpec::default(),
        )
        .unwrap();
        w.limit_secs(Some(1.0));
        let t0 = crate::media_types::now();
        // Video on time; the microphone's first buffer only at 0.3 s, then
        // 1024- and 2048-frame buffers (21.3 and 42.7 ms).
        let mut next_audio = t0 + 0.3;
        let mut sizes = [1024usize, 2048].into_iter().cycle();
        let mut audio_count = 0;
        for i in 0..30 {
            let t = t0 + i as f64 / 30.0;
            w.push_video(frame_at(t, 320, 180));
            while next_audio <= t {
                let n = sizes.next().unwrap();
                let samples: Vec<f32> = vec![0.05; n * 2];
                w.push_audio(
                    AudioKind::Mic,
                    &AudioChunk::stereo(next_audio, 48_000, samples),
                );
                next_audio += n as f64 / 48_000.0;
                audio_count += 1;
            }
            w.emit_until(t);
        }
        let stats = w.finish(t0 + 1.0).unwrap();
        assert!(audio_count > 0);
        assert!(stats.audio_mic_buffers > 0, "mic buffers were written");
        assert_eq!(stats.audio_drops, 0, "no mic buffer was dropped");
        let info = crate::media::mp4::read(&path).expect("the file reads back");
        let mic = info
            .tracks
            .iter()
            .find(|t| t.handler == crate::media::mp4::Handler::Audio)
            .expect("a microphone track exists");
        assert!(mic.samples > 0, "the mic track has samples");
        assert!(
            mic.duration as f64 / mic.timescale.max(1) as f64 > 0.5,
            "the mic track covers most of the second: {} samples",
            mic.samples
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
