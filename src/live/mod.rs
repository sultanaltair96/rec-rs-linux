//! Live outputs: one shared H.264/HEVC + AAC/Opus encode of the composited
//! output, fanned out to the replay buffer and any streams (RTMP, WHIP).
//!
//! The capture session feeds every output frame and every post-fader audio
//! buffer in (on the host clock, independent of recording pauses), through
//! [`Live::push_video`] and [`Live::push_audio`]. Nothing is encoded while no
//! live output is active.
//!
//! # Threads and queues
//!
//! The taps are non-blocking: they push into a small bounded queue that drops
//! its oldest frame (a stream must see the newest picture, not a backlog) and
//! return. One worker thread drains the queues into the GStreamer encoders and
//! fills short holes (a repeated frame, silence), so swapping a capture session
//! leaves no gap in a stream. Each encoder's appsink callback hands the packets
//! to the hub, which keeps the replay ring and feeds every destination; a
//! destination owns its socket and its own queue, so a slow or reconnecting
//! server never stalls the encoder or the capture.
//!
//! # What is here
//!
//! The *option* surface the CLI, the settings file and the app share, with its
//! tests: destination URLs, platform recognition and the shared encode's
//! bitrate/size presets (pure functions, ported from upstream, so they work
//! before any output exists). Then the encode itself ([`video`], [`audio`]),
//! the replay buffer ([`replay`]), the two built-in clients ([`rtmp`],
//! [`whip`]) and stream keys in the Secret Service ([`keyring`]).
//!
//! Adapted from rec-rs src/live/mod.rs (MIT OR Apache-2.0).

pub mod audio;
pub mod harness;
pub mod keyring;
pub mod replay;
pub mod rtmp;
pub mod video;
pub mod whip;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::media_types::{
    AudioChunk, AudioCodec, EncodedPacket, PacketKind, VideoCodec, VideoFrame,
};

/// The services with a built-in ingest URL, in the order the app lists them.
pub const SERVICES: [(&str, &str, &str); 3] = [
    (
        "youtube",
        "YouTube",
        "rtmps://a.rtmps.youtube.com:443/live2",
    ),
    ("twitch", "Twitch", "rtmps://live.twitch.tv:443/app"),
    (
        "kick",
        "Kick",
        "rtmps://fa723fc1b171.global-contribute.live-video.net:443/app",
    ),
];

/// Settings of the shared live encode.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveConfig {
    /// Encode size (frames of another size are scaled).
    pub width: usize,
    pub height: usize,
    /// Video bits per second.
    pub bitrate: u64,
    pub keyframe_secs: f64,
    pub cbr: bool,
    /// AAC bits per second (Opus for WHIP uses the same).
    pub audio_bitrate: u32,
    /// Prefer HEVC (used when every stream accepts it, else H.264).
    pub hevc: bool,
}

impl Default for LiveConfig {
    fn default() -> Self {
        LiveConfig {
            width: 1920,
            height: 1080,
            bitrate: 6_000_000,
            keyframe_secs: 2.0,
            cbr: true,
            audio_bitrate: 160_000,
            hevc: false,
        }
    }
}

/// The codec configuration the outputs need (sequence headers, SDP payloads).
#[derive(Clone, Debug, PartialEq)]
pub struct Formats {
    /// The current video is HEVC (else H.264).
    pub hevc: bool,
    /// Parameter set NAL units (SPS/PPS, or VPS/SPS/PPS for HEVC).
    pub param_sets: Vec<Vec<u8>>,
    /// Decoder configuration record (`avcC` / `hvcC`).
    pub record: Vec<u8>,
    /// AAC AudioSpecificConfig.
    pub aac_cookie: Vec<u8>,
    /// Encoded picture size, for RTMP's metadata.
    pub width: usize,
    pub height: usize,
    /// Audio sample rate and channels of the AAC stream.
    pub audio_rate: u32,
    pub audio_channels: u16,
}

impl Default for Formats {
    fn default() -> Self {
        Formats {
            hevc: false,
            param_sets: vec![],
            record: vec![],
            aac_cookie: vec![],
            width: 0,
            height: 0,
            audio_rate: audio::RATE,
            audio_channels: 2,
        }
    }
}

/// A stream destination fed with the shared encode's packets.
pub trait Output: Send {
    fn packet(&mut self, p: &EncodedPacket, f: &Formats);
    fn status(&self) -> OutputStatus;
    /// Wants Opus audio (WebRTC) instead of AAC.
    fn wants_opus(&self) -> bool {
        false
    }
    /// Can take HEVC (otherwise the shared encode uses H.264).
    fn accepts_hevc(&self) -> bool {
        false
    }
    /// Asked for a keyframe (just connected or reconnected).
    fn take_keyframe_request(&mut self) -> bool {
        false
    }
    fn stop(&mut self);
}

/// One destination's state, for the app and the CLI's status line.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OutputStatus {
    pub name: String,
    pub state: String,
    pub live: bool,
    /// Measured send rate over the last seconds, bits per second.
    pub bitrate: f64,
    pub dropped_frames: u64,
    pub reconnects: u64,
    /// Seconds since the stream started.
    pub duration: f64,
    pub error: Option<String>,
}

/// The hub every encoded packet goes through: the replay ring and every
/// destination.
#[derive(Default)]
struct Hub {
    formats: Formats,
    replay: Option<replay::Replay>,
    outputs: Vec<(u64, Box<dyn Output>)>,
}

impl Hub {
    fn dispatch(&mut self, p: EncodedPacket) {
        match &p {
            EncodedPacket {
                kind: PacketKind::Video(_),
                ..
            } => {
                if let Some(r) = &mut self.replay {
                    r.push_video(p.clone());
                }
            }
            EncodedPacket {
                kind: PacketKind::Audio(AudioCodec::Aac),
                ..
            } => {
                if let Some(r) = &mut self.replay {
                    r.push_audio(p.clone());
                }
            }
            _ => {}
        }
        for (_, o) in &mut self.outputs {
            o.packet(&p, &self.formats);
        }
    }
}

/// The encode's audio sources for the session, and whether silence stands in
/// for them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct SessionState {
    /// [system, mic] delivered by the current session.
    sources: [bool; 2],
}

/// Encoded frame/audio counters, for the app and the harness.
#[derive(Default)]
struct Stats {
    video_packets: AtomicU64,
    audio_packets: AtomicU64,
    bytes: AtomicU64,
    dropped: AtomicU64,
}

/// A short gap (a capture session being swapped) is filled; a longer one
/// restarts the timeline. Same rule and value as upstream.
const MAX_FILL: f64 = 3.0;

/// Frames kept in the tap: enough for a jittery encode, never a backlog.
const MAX_TAP_FRAMES: usize = 4;
/// Bytes kept in the tap (`4` frames of 4K BGRA is ~130 MB; this caps it).
const MAX_TAP_BYTES: usize = 64 << 20;
/// Audio frames (per channel) kept in the tap: two seconds of 48 kHz.
const MAX_TAP_AUDIO: usize = audio::RATE as usize * 2;

/// The writer's tap: bounded queues that never block the capture thread.
struct Tap {
    queues: Mutex<Queues>,
    cv: Condvar,
}

#[derive(Default)]
struct Queues {
    video: VecDeque<VideoFrame>,
    audio: VecDeque<AudioChunk>,
    video_bytes: usize,
    audio_frames: usize,
    dropped_video: u64,
    dropped_audio: u64,
    closed: bool,
}

impl Default for Tap {
    fn default() -> Self {
        Tap {
            queues: Mutex::new(Queues::default()),
            cv: Condvar::new(),
        }
    }
}

impl Tap {
    fn push_video(&self, frame: VideoFrame) {
        let mut q = self.queues.lock().unwrap();
        if q.closed {
            return;
        }
        q.video_bytes += frame.byte_len();
        q.video.push_back(frame);
        while q.video.len() > MAX_TAP_FRAMES || (q.video_bytes > MAX_TAP_BYTES && q.video.len() > 1)
        {
            // Drop the oldest frame: a live viewer wants the newest picture.
            if let Some(old) = q.video.pop_front() {
                q.video_bytes -= old.byte_len();
                q.dropped_video += 1;
            }
        }
        self.cv.notify_one();
    }

    fn push_audio(&self, chunk: AudioChunk) {
        let mut q = self.queues.lock().unwrap();
        if q.closed {
            return;
        }
        q.audio_frames += chunk.frames();
        q.audio.push_back(chunk);
        while q.audio.len() > 1 && q.audio_frames > MAX_TAP_AUDIO {
            if let Some(old) = q.audio.pop_front() {
                q.audio_frames -= old.frames();
                q.dropped_audio += 1;
            }
        }
        self.cv.notify_one();
    }

    /// Wait up to `timeout` for work and take everything queued.
    fn take(&self, timeout: Duration) -> (Vec<VideoFrame>, Vec<AudioChunk>) {
        let mut q = self.queues.lock().unwrap();
        if q.video.is_empty() && q.audio.is_empty() && !q.closed {
            q = self.cv.wait_timeout(q, timeout).unwrap().0;
        }
        let video = q.video.drain(..).collect();
        let audio = q.audio.drain(..).collect();
        q.video_bytes = 0;
        q.audio_frames = 0;
        (video, audio)
    }

    fn wake(&self) {
        self.cv.notify_all();
    }

    fn close(&self) {
        self.queues.lock().unwrap().closed = true;
        self.cv.notify_all();
    }

    fn dropped(&self) -> (u64, u64) {
        let q = self.queues.lock().unwrap();
        (q.dropped_video, q.dropped_audio)
    }
}

/// Everything the worker thread and the public handles share.
struct Shared {
    cfg: Mutex<LiveConfig>,
    /// Bumped by `set_config`, so the worker rebuilds the encoders.
    cfg_gen: AtomicU64,
    hub: Arc<Mutex<Hub>>,
    tap: Tap,
    session: Mutex<SessionState>,
    stats: Arc<Stats>,
    /// The last encoder failure, for the app's banner.
    error: Mutex<Option<String>>,
}

/// The live encode and its outputs.
pub struct Live {
    shared: Arc<Shared>,
    worker: Mutex<Option<Worker>>,
    active: AtomicBool,
    next_id: Mutex<u64>,
}

struct Worker {
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

impl Default for Live {
    fn default() -> Self {
        Live {
            shared: Arc::new(Shared {
                cfg: Mutex::new(LiveConfig::default()),
                cfg_gen: AtomicU64::new(0),
                hub: Arc::new(Mutex::new(Hub::default())),
                tap: Tap::default(),
                session: Mutex::new(SessionState::default()),
                stats: Arc::new(Stats::default()),
                error: Mutex::new(None),
            }),
            worker: Mutex::new(None),
            active: AtomicBool::new(false),
            next_id: Mutex::new(0),
        }
    }
}

impl Live {
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure the shared encode. A change restarts the encoders with a
    /// keyframe; the replay buffer starts over (a clip holds one set of
    /// parameter sets), exactly as upstream does.
    pub fn set_config(&self, config: LiveConfig) {
        {
            let mut cfg = self.shared.cfg.lock().unwrap();
            if *cfg == config {
                return;
            }
            *cfg = config;
        }
        self.shared.cfg_gen.fetch_add(1, Ordering::SeqCst);
        {
            // Lock order: configuration, then the hub.
            let mut hub = self.shared.hub.lock().unwrap();
            hub.formats = Formats::default();
            if let Some(r) = &mut hub.replay {
                r.clear();
            }
        }
        self.shared.tap.wake();
    }

    /// The current configuration, if one was set.
    pub fn config(&self) -> Option<LiveConfig> {
        Some(self.shared.cfg.lock().unwrap().clone())
    }

    /// Keep the last `secs` seconds in memory (None turns the buffer off).
    pub fn set_replay(&self, secs: Option<f64>) {
        {
            let mut hub = self.shared.hub.lock().unwrap();
            match (secs, hub.replay.as_mut()) {
                (Some(s), Some(r)) => r.secs = s,
                (Some(s), None) => hub.replay = Some(replay::Replay::new(s)),
                (None, _) => hub.replay = None,
            }
        }
        self.update_active();
    }

    /// The replay window, if one was set (the app shows it and its settings
    /// start from it).
    #[allow(dead_code)]
    pub fn replay_secs(&self) -> Option<f64> {
        self.shared
            .hub
            .lock()
            .unwrap()
            .replay
            .as_ref()
            .map(|r| r.secs)
    }

    /// (seconds held, bytes held), if the replay buffer is on — what the app
    /// shows.
    pub fn replay_stats(&self) -> Option<(f64, usize)> {
        self.shared
            .hub
            .lock()
            .unwrap()
            .replay
            .as_ref()
            .map(|r| (r.held(), r.bytes()))
    }

    /// True while the encode is running (something consumes it).
    pub fn active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    /// Something failed in the encode (the app shows it).
    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    /// (video packets, audio packets, bytes encoded, dropped input frames).
    pub fn encode_stats(&self) -> (u64, u64, u64, u64) {
        let s = &self.shared.stats;
        let (dv, da) = self.shared.tap.dropped();
        (
            s.video_packets.load(Ordering::Relaxed),
            s.audio_packets.load(Ordering::Relaxed),
            s.bytes.load(Ordering::Relaxed),
            s.dropped.load(Ordering::Relaxed) + dv + da,
        )
    }

    /// Start the encode for a session with these audio sources (already through
    /// their faders). Silence stands in when there are none, so every service
    /// gets an audio track.
    pub fn begin_session(&self, system: bool, mic: bool) -> Result<()> {
        *self.shared.session.lock().unwrap() = SessionState {
            sources: [system, mic],
        };
        self.shared.tap.wake();
        Ok(())
    }

    /// The writer's video tap: called after every emitted constant-frame-rate
    /// frame with the composited output frame (host-clock pts), also while a
    /// recording is paused and with `--no-record`. Must not block. Agreed API
    /// between the writer (1G) and the live encode (1D).
    pub fn push_video(&self, frame: &VideoFrame) {
        if !self.active() {
            return;
        }
        self.shared.tap.push_video(frame.clone());
    }

    /// The writer's audio tap: the post-fader mix, 48 kHz stereo, host-clock
    /// pts. Same rules as [`push_video`](Live::push_video).
    pub fn push_audio(&self, chunk: &AudioChunk) {
        if !self.active() {
            return;
        }
        self.shared.tap.push_audio(chunk.clone());
    }

    /// Write the replay buffer to `path` (an MP4) and wait for it.
    pub fn save_replay(&self, path: PathBuf) -> Result<()> {
        let handle = self.save_replay_async(path)?;
        handle
            .join()
            .map_err(|_| anyhow::anyhow!("the replay writer panicked"))??;
        Ok(())
    }

    /// Start writing the replay buffer to `path` on a background thread; the
    /// buffer keeps running. Returns the thread (join it for the final path).
    pub fn save_replay_async(
        &self,
        path: PathBuf,
    ) -> Result<std::thread::JoinHandle<Result<PathBuf>>> {
        let (video, audio, formats) = {
            let hub = self.shared.hub.lock().unwrap();
            let r = hub
                .replay
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("the replay buffer is off"))?;
            if r.is_empty() {
                bail!("the replay buffer is still empty");
            }
            let (v, a) = r.snapshot();
            (v, a, hub.formats.clone())
        };
        if video.is_empty() {
            bail!("the replay buffer is still empty");
        }
        Ok(std::thread::spawn(move || {
            replay::write_mp4(&path, &video, &audio, &formats)
        }))
    }

    // --- Streams --------------------------------------------------------

    /// Add a running output; returns its id.
    pub fn add_output(&self, o: Box<dyn Output>) -> u64 {
        let id = {
            let mut n = self.next_id.lock().unwrap();
            *n += 1;
            *n
        };
        self.shared.hub.lock().unwrap().outputs.push((id, o));
        self.update_active();
        // A new viewer needs a keyframe and the codec headers.
        self.shared.tap.wake();
        id
    }

    /// Start and add an output for every URL, all or nothing: when one cannot
    /// start, the ones added before it are removed again and the error names
    /// it. Returns each output's id with its URL.
    pub fn add_outputs(
        &self,
        urls: &[String],
        start: impl Fn(&str) -> Result<Box<dyn Output>>,
    ) -> Result<Vec<(u64, String)>> {
        let mut added = vec![];
        for url in urls {
            match start(url) {
                Ok(o) => added.push((self.add_output(o), url.clone())),
                Err(e) => {
                    for (id, _) in added {
                        self.remove_output(id);
                    }
                    return Err(
                        e.context(format!("Could not go live on {}", destination_name(url)))
                    );
                }
            }
        }
        Ok(added)
    }

    pub fn remove_output(&self, id: u64) {
        let removed = {
            let mut hub = self.shared.hub.lock().unwrap();
            let i = hub.outputs.iter().position(|(i, _)| *i == id);
            i.map(|i| hub.outputs.remove(i))
        };
        self.update_active();
        if let Some((_, mut o)) = removed {
            o.stop();
        }
    }

    /// Every destination's state, in the order it was added.
    pub fn statuses(&self) -> Vec<(u64, OutputStatus)> {
        self.shared
            .hub
            .lock()
            .unwrap()
            .outputs
            .iter()
            .map(|(i, o)| (*i, o.status()))
            .collect()
    }

    /// Stop and remove every destination (the encode stops when nothing is
    /// left).
    pub fn stop_all(&self) {
        let outs = std::mem::take(&mut self.shared.hub.lock().unwrap().outputs);
        for (_, mut o) in outs {
            o.stop();
        }
        self.update_active();
    }

    /// Start or stop the encode thread as what consumes it changes.
    fn update_active(&self) {
        let on = {
            let hub = self.shared.hub.lock().unwrap();
            hub.replay.is_some() || !hub.outputs.is_empty()
        };
        self.active.store(on, Ordering::Relaxed);
        let mut worker = self.worker.lock().unwrap();
        if on && worker.is_none() {
            let shared = self.shared.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            let handle = std::thread::Builder::new()
                .name("live-encode".into())
                .spawn({
                    let shared = shared.clone();
                    move || worker_loop(shared, flag)
                })
                .expect("spawning the live encode thread");
            *worker = Some(Worker { stop, handle });
        } else if !on {
            if let Some(w) = worker.take() {
                w.stop.store(true, Ordering::SeqCst);
                self.shared.tap.wake();
                let _ = w.handle.join();
                self.shared.tap.close();
            }
        }
        if on {
            // Re-open the tap if it was closed by a stop.
            let mut q = self.shared.tap.queues.lock().unwrap();
            q.closed = false;
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Some(w) = self.worker.lock().unwrap().take() {
            w.stop.store(true, Ordering::SeqCst);
            self.shared.tap.wake();
            let _ = w.handle.join();
        }
        let outs = std::mem::take(&mut self.shared.hub.lock().unwrap().outputs);
        for (_, mut o) in outs {
            o.stop();
        }
    }
}

/// Encode size: the frame fitted into the configured size, never larger than
/// the frame (no upscaling), even dimensions.
pub fn enc_size(cfg: &LiveConfig, width: usize, height: usize) -> (usize, usize) {
    let (fw, fh) = (width as f64, height as f64);
    if fw <= 0.0 || fh <= 0.0 {
        return (16, 16);
    }
    let s = (cfg.width as f64 / fw).min(cfg.height as f64 / fh).min(1.0);
    (
        ((fw * s).round() as usize).max(16) & !1,
        ((fh * s).round() as usize).max(16) & !1,
    )
}

/// The running encoders and what they were built for.
struct Encoders {
    video: video::VideoEncoder,
    aac: audio::AudioEncoder,
    opus: Option<audio::AudioEncoder>,
    hevc: bool,
    wants_opus: bool,
    fps: u32,
    size: (usize, usize),
    generation: u64,
}

/// What a fresh set of encoders is built for.
struct EncoderSpec<'a> {
    cfg: &'a LiveConfig,
    hevc: bool,
    wants_opus: bool,
    size: (usize, usize),
    fps: u32,
    generation: u64,
}

impl Encoders {
    fn new(spec: EncoderSpec<'_>, hub: &Arc<Mutex<Hub>>, stats: &Arc<Stats>) -> Result<Encoders> {
        let EncoderSpec {
            cfg,
            hevc,
            wants_opus,
            size,
            fps,
            generation,
        } = spec;
        let (video_hub, audio_hub, opus_hub) = (hub.clone(), hub.clone(), hub.clone());
        let video_stats = stats.clone();
        let aac_stats = stats.clone();
        let opus_stats = stats.clone();
        let video = video::VideoEncoder::new(
            video::VideoConfig {
                width: size.0,
                height: size.1,
                fps,
                bitrate: cfg.bitrate,
                keyframe_secs: cfg.keyframe_secs,
                cbr: cfg.cbr,
                hevc,
            },
            Arc::new(move |p| {
                video_stats.video_packets.fetch_add(1, Ordering::Relaxed);
                video_stats
                    .bytes
                    .fetch_add(p.data.len() as u64, Ordering::Relaxed);
                let mut hub = video_hub.lock().unwrap();
                hub.formats.hevc = p.hevc;
                hub.formats.width = p.width;
                hub.formats.height = p.height;
                if p.key && !p.record.is_empty() {
                    hub.formats.param_sets = p.param_sets.clone();
                    hub.formats.record = p.record.clone();
                }
                let packet = EncodedPacket {
                    pts: p.pts,
                    dts: p.pts,
                    keyframe: p.key,
                    data: p.data.clone(),
                    kind: PacketKind::Video(if p.hevc {
                        VideoCodec::Hevc
                    } else {
                        VideoCodec::H264
                    }),
                };
                hub.dispatch(packet);
            }),
        )?;
        let aac = audio::AudioEncoder::new(AudioCodec::Aac, cfg.audio_bitrate, {
            let hub = audio_hub.clone();
            let stats = aac_stats.clone();
            Arc::new(move |p| dispatch_audio(&hub, &stats, p, false))
        })?;
        let opus = if wants_opus {
            Some(audio::AudioEncoder::new(
                AudioCodec::Opus,
                cfg.audio_bitrate.min(256_000),
                Arc::new(move |p| dispatch_audio(&opus_hub, &opus_stats, p, true)),
            )?)
        } else {
            None
        };
        Ok(Encoders {
            video,
            aac,
            opus,
            hevc,
            wants_opus,
            fps,
            size,
            generation,
        })
    }

    fn replace_audio(
        &mut self,
        cfg: &LiveConfig,
        wants_opus: bool,
        hub: &Arc<Mutex<Hub>>,
        stats: &Arc<Stats>,
    ) -> Result<()> {
        let (h, s) = (hub.clone(), stats.clone());
        self.aac = audio::AudioEncoder::new(
            AudioCodec::Aac,
            cfg.audio_bitrate,
            Arc::new(move |p| dispatch_audio(&h, &s, p, false)),
        )?;
        self.opus = if wants_opus {
            let (h, s) = (hub.clone(), stats.clone());
            Some(audio::AudioEncoder::new(
                AudioCodec::Opus,
                cfg.audio_bitrate.min(256_000),
                Arc::new(move |p| dispatch_audio(&h, &s, p, true)),
            )?)
        } else {
            None
        };
        self.wants_opus = wants_opus;
        Ok(())
    }

    fn errors(&self) -> Vec<String> {
        let mut out = vec![];
        if let Some(e) = self.video.error() {
            out.push(e);
        }
        if let Some(e) = self.aac.error() {
            out.push(e);
        }
        if let Some(e) = self.opus.as_ref().and_then(|o| o.error()) {
            out.push(e);
        }
        out
    }

    fn dropped(&self) -> u64 {
        self.video.dropped() + self.aac.dropped() + self.opus.as_ref().map_or(0, |o| o.dropped())
    }
}

fn dispatch_audio(hub: &Arc<Mutex<Hub>>, stats: &Arc<Stats>, p: audio::AudioPacket, opus: bool) {
    stats.audio_packets.fetch_add(1, Ordering::Relaxed);
    stats
        .bytes
        .fetch_add(p.data.len() as u64, Ordering::Relaxed);
    let mut hub = hub.lock().unwrap();
    if p.codec == AudioCodec::Aac && !p.cookie.is_empty() {
        hub.formats.aac_cookie = p.cookie.clone();
    }
    let packet = EncodedPacket {
        pts: p.pts,
        dts: p.pts,
        keyframe: true,
        data: p.data.clone(),
        kind: PacketKind::Audio(if opus {
            AudioCodec::Opus
        } else {
            AudioCodec::Aac
        }),
    };
    hub.dispatch(packet);
}

/// The frame rate of the tap, measured from the frames themselves.
///
/// The tap hands over one frame per emitted constant-frame-rate slot, so the
/// interval between frames is the capture's frame rate; the encode needs it for
/// its `key-int-max` and its caps. Measured (median of the last 31 intervals)
/// rather than configured, because the tap carries no frame rate and the writer
/// may be re-timed.
#[derive(Default)]
struct FrameRate {
    last: Option<f64>,
    intervals: VecDeque<f64>,
}

impl FrameRate {
    fn observe(&mut self, pts: f64) {
        if let Some(last) = self.last {
            let dt = pts - last;
            if dt > 1e-4 && dt < 1.0 {
                self.intervals.push_back(dt);
                if self.intervals.len() > 31 {
                    self.intervals.pop_front();
                }
            }
        }
        self.last = Some(pts);
    }

    /// Rounded frames per second (60 until there is a measurement).
    fn fps(&self) -> u32 {
        if self.intervals.is_empty() {
            return 60;
        }
        let mut v: Vec<f64> = self.intervals.iter().copied().collect();
        v.sort_by(f64::total_cmp);
        let median = v[v.len() / 2];
        (1.0 / median).round().clamp(1.0, 240.0) as u32
    }
}

/// The encode thread: drain the tap, keep the encoders right, fill short gaps.
fn worker_loop(shared: Arc<Shared>, stop: Arc<AtomicBool>) {
    let mut enc: Option<Encoders> = None;
    let mut last_video: Option<(f64, VideoFrame)> = None;
    let mut audio_end: Option<f64> = None;
    let mut notes: Vec<&'static str> = vec![];
    let mut retry_after = Instant::now();
    let mut audio_sources = [false, false];
    let mut rate = FrameRate::default();

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let (frames, chunks) = shared.tap.take(Duration::from_millis(20));
        let cfg = shared.cfg.lock().unwrap().clone();
        let generation = shared.cfg_gen.load(Ordering::Relaxed);
        let session = *shared.session.lock().unwrap();
        if session.sources != audio_sources {
            audio_sources = session.sources;
            audio_end = None;
        }
        let (wants_opus, accepts_hevc, key_request) = {
            let mut hub = shared.hub.lock().unwrap();
            let wants_opus = hub.outputs.iter().any(|(_, o)| o.wants_opus());
            let accepts = hub.outputs.iter().all(|(_, o)| o.accepts_hevc());
            let key = hub
                .outputs
                .iter_mut()
                .fold(false, |k, (_, o)| o.take_keyframe_request() | k);
            (wants_opus, accepts, key)
        };

        if frames.is_empty() && chunks.is_empty() {
            if let Some(e) = &mut enc {
                if key_request {
                    e.video.force_keyframe();
                }
                for error in e.errors() {
                    report(&shared, error);
                }
            }
            continue;
        }

        for frame in &frames {
            rate.observe(frame.pts);
        }
        let fps = rate.fps();

        // Which codec can every destination take (and is an encoder there)?
        let hevc = cfg.hevc && accepts_hevc;
        if cfg.hevc && !hevc && !notes.contains(&"hevc-not-accepted") {
            notes.push("hevc-not-accepted");
            if !video::has_encoder(true) {
                eprintln!("rec: live encode: no HEVC encoder is installed; using H.264");
            } else {
                eprintln!("rec: live encode: a destination does not take HEVC; using H.264");
            }
        }
        if hevc && !video::main10_capable() && !notes.contains(&"main10") {
            notes.push("main10");
            eprintln!("rec: live encode: HEVC Main (an 8-bit encoder; Main10 needs a 10-bit one)");
        }

        // The first frame of a run fixes the encode size.
        let first = frames.first();
        let size = first
            .map(|f| enc_size(&cfg, f.width, f.height))
            .or_else(|| enc.as_ref().map(|e| e.size))
            .unwrap_or(enc_size(&cfg, cfg.width, cfg.height));
        let rebuild_video = match &enc {
            None => true,
            Some(e) => {
                e.hevc != hevc || e.fps != fps || e.generation != generation || e.size != size
            }
        };
        if rebuild_video && first.is_some() && Instant::now() >= retry_after {
            match Encoders::new(
                EncoderSpec {
                    cfg: &cfg,
                    hevc,
                    wants_opus,
                    size,
                    fps,
                    generation,
                },
                &shared.hub,
                &shared.stats,
            ) {
                Ok(mut fresh) => {
                    // Keep the mix's position: the new encoder starts at the
                    // current audio time, with a keyframe.
                    fresh.video.force_keyframe();
                    enc = Some(fresh);
                    // A clip holds one set of parameter sets, and the outputs
                    // need the new stream's configuration.
                    {
                        let mut hub = shared.hub.lock().unwrap();
                        hub.formats = Formats::default();
                        if let Some(r) = hub.replay.as_mut() {
                            r.clear();
                        }
                    }
                    if std::env::var_os("REC_DEBUG").is_some()
                        && let Some(e) = &enc
                    {
                        eprintln!(
                            "rec: debug: live encoders {} {}x{} -> {} + {}",
                            if hevc { "HEVC" } else { "H.264" },
                            size.0,
                            size.1,
                            e.video.encoder,
                            e.aac.encoder
                        );
                    }
                }
                Err(e) => {
                    report(&shared, format!("{e:#}"));
                    retry_after = Instant::now() + Duration::from_secs(2);
                    enc = None;
                    continue;
                }
            }
        }
        let mut audio_failed = false;
        if let Some(e) = &mut enc {
            if e.wants_opus != wants_opus && !rebuild_video {
                let hub = shared.hub.clone();
                if let Err(err) = e.replace_audio(&cfg, wants_opus, &hub, &shared.stats) {
                    report(&shared, format!("{err:#}"));
                    audio_failed = true;
                }
            }
            if key_request {
                e.video.force_keyframe();
            }
        }
        if audio_failed {
            enc = None;
        }

        let Some(e) = &mut enc else {
            // Dropped while the encoder is unavailable.
            let n = shared.tap.dropped().0 + shared.tap.dropped().1;
            shared.stats.dropped.store(n, Ordering::Relaxed);
            continue;
        };
        let step = 1.0 / fps.max(1) as f64;

        // A short hole (sessions swapped, or the tap dropping under pressure):
        // repeat the last frame on the frame grid up to this one, as upstream
        // does. `prev` is the last frame that actually went in, so a hole
        // anywhere — inside a batch or between batches — is bridged.
        let mut prev = last_video.clone();
        for frame in &frames {
            if let Some((lt, lf)) = &prev {
                let dt = frame.pts - lt;
                if dt > 1.5 * step && dt < MAX_FILL {
                    let mut ft = lt + step;
                    while ft < frame.pts - 0.5 * step {
                        e.video.encode(lf, ft);
                        ft += step;
                    }
                }
            }
            if e.video.encode(frame, frame.pts) {
                prev = Some((frame.pts, frame.clone()));
            }
            // Without any audio source, silence keeps the timeline continuous
            // so players and services that require audio accept the stream.
            if audio_sources == [false, false] {
                let from = audio_end.unwrap_or(frame.pts);
                let n = ((frame.pts - from) * audio::RATE as f64).round().max(0.0) as usize;
                if n > 0 || audio_end.is_none() {
                    e.aac.push_silence(from, n);
                    if let Some(o) = e.opus.as_mut() {
                        o.push_silence(from, n);
                    }
                    audio_end = Some(from + n as f64 / audio::RATE as f64);
                }
            }
        }
        if let Some(last) = prev {
            last_video = Some(last);
        }

        for chunk in &chunks {
            // A short hole between buffers is silence, so later audio does not
            // shift; a long one restarts the timeline.
            if let Some(end) = audio_end {
                let gap = chunk.pts - end;
                if gap > 0.005 && gap < MAX_FILL {
                    let n = (gap * audio::RATE as f64).round().max(0.0) as usize;
                    e.aac.push_silence(end, n);
                    if let Some(o) = e.opus.as_mut() {
                        o.push_silence(end, n);
                    }
                }
            }
            // Only advance the timeline when the buffer was taken: a refused
            // one leaves a hole that the next chunk's silence fill closes, so
            // the stream stays continuous even when an encoder stalls.
            let aac_ok = e.aac.push(chunk.pts, chunk);
            let opus_ok = match e.opus.as_mut() {
                Some(o) => o.push(chunk.pts, chunk),
                None => true,
            };
            if aac_ok && opus_ok {
                audio_end = Some(chunk.pts + chunk.secs());
            }
        }

        // Packets and encoded bytes are counted by the appsink callbacks; this
        // is what the tap had to drop.
        shared.stats.dropped.store(
            e.dropped() + shared.tap.dropped().0 + shared.tap.dropped().1,
            Ordering::Relaxed,
        );
        for error in e.errors() {
            report(&shared, error);
        }
    }
    // Let the pipelines finalize (they are stopped by their Drop).
}

/// Remember the first failure and say it once.
fn report(shared: &Arc<Shared>, error: String) {
    let mut slot = shared.error.lock().unwrap();
    if slot.as_deref() == Some(error.as_str()) {
        return;
    }
    eprintln!("rec: live encode: {error}");
    *slot = Some(error);
    drop(slot);
}

/// Redact a secret (stream key) from text meant for logs and the UI.
pub fn redact(text: &str, secret: &str) -> String {
    if secret.len() < 4 {
        text.to_string()
    } else {
        text.replace(secret, "••••")
    }
}

/// Stream URL for a destination: a service id (`youtube`, `twitch`, `kick`)
/// expands to its RTMPS ingest URL; anything else is used as given.
pub fn destination_url(dest: &str) -> String {
    let dest = dest.trim();
    SERVICES
        .iter()
        .find(|(id, ..)| dest.eq_ignore_ascii_case(id))
        .map_or_else(|| dest.to_string(), |(.., url)| url.to_string())
}

/// What to call a destination in the UI: its service's name, else the server's
/// host.
pub fn destination_name(url: &str) -> String {
    match SERVICES.iter().find(|(.., u)| *u == url) {
        Some((_, name, _)) => name.to_string(),
        None => host(url),
    }
}

/// A streaming platform, recognized from the ingest URL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    YouTube,
    Twitch,
    Kick,
    Other,
}

fn host(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split(['/', ':'])
        .next()
        .unwrap_or("")
        .to_lowercase()
}

pub fn platform(url: &str) -> Platform {
    let h = host(url);
    if h.ends_with("youtube.com") {
        Platform::YouTube
    } else if h == host(&destination_url("kick")) {
        // Before Twitch: both ingest on *.contribute.live-video.net.
        Platform::Kick
    } else if h.ends_with("twitch.tv") || h.ends_with("contribute.live-video.net") {
        Platform::Twitch
    } else {
        Platform::Other
    }
}

/// Encode limits and the recommended video bitrate (kb/s) for a platform at an
/// encode height and frame rate:
/// - YouTube (its live encoder settings): H.264 2160p60 45 Mb/s, 2160p30 35,
///   1440p 24/16, 1080p 12/10, 720p 7.5/5; HEVC (enhanced RTMP) about a quarter
///   less (2160p 35/25 Mb/s).
/// - Twitch: H.264 only, at most 1080p60 and 6 Mb/s (its ingest limits).
/// - Kick: H.264 only, at most 1080p60 and 8 Mb/s (its encoder requirements).
pub fn preset(p: Platform, height: usize, fps: u32, hevc: bool) -> (u32, usize, bool) {
    // [H.264 60 fps, H.264 30 fps, HEVC 60 fps, HEVC 30 fps]
    let row = match height {
        h if h > 1440 => [45_000, 35_000, 35_000, 25_000],
        h if h > 1080 => [24_000, 16_000, 18_000, 12_000],
        h if h > 720 => [12_000, 10_000, 9_000, 7_500],
        _ => [7_500, 5_000, 5_500, 3_500],
    };
    let i = usize::from(fps <= 30);
    match p {
        Platform::Twitch => (row[i].min(6_000), height.min(1080), false),
        Platform::Kick => (row[i].min(8_000), height.min(1080), false),
        _ if hevc => (row[2 + i], height, true),
        _ => (row[i], height, false),
    }
}

/// The one encode every destination accepts, as (kb/s, height, HEVC): the
/// lowest bitrate and height of their presets, and HEVC only if all of them
/// take it. `codec` is "auto" (HEVC when every destination is YouTube), "h264"
/// or "hevc". With no destinations (replay buffer only) it is the generic
/// preset.
pub fn shared_preset(
    platforms: &[Platform],
    height: usize,
    fps: u32,
    codec: &str,
) -> (u32, usize, bool) {
    let hevc = match codec {
        "hevc" => true,
        "h264" => false,
        _ => !platforms.is_empty() && platforms.iter().all(|p| *p == Platform::YouTube),
    };
    platforms
        .iter()
        .map(|p| preset(*p, height, fps, hevc))
        .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1), a.2 && b.2))
        .unwrap_or_else(|| preset(Platform::Other, height, fps, hevc))
}

/// (URL, key) for a destination: the key comes from `$REC_STREAM_KEY`, else the
/// Secret Service. WHIP endpoints may have no key.
pub fn resolve_destination(dest: &str) -> Result<(String, String)> {
    let url = destination_url(dest);
    if !(url.starts_with("rtmp://")
        || url.starts_with("rtmps://")
        || url.starts_with("http://")
        || url.starts_with("https://"))
    {
        bail!("unsupported stream URL {url:?} (use rtmp://, rtmps:// or a WHIP http(s):// URL)");
    }
    let key = std::env::var("REC_STREAM_KEY")
        .ok()
        .filter(|k| !k.is_empty())
        .or_else(|| keyring::get(dest));
    match key {
        Some(k) => Ok((url, k)),
        None if url.starts_with("http") => Ok((url, String::new())),
        None if !keyring::available() => bail!(
            "no stream key for {url}: no Secret Service is running (start gnome-keyring-daemon), \
             so run `rec-rs stream-key {dest}` in a session that has one, or set REC_STREAM_KEY"
        ),
        None => {
            bail!("no stream key for {url}: run `rec-rs stream-key {dest}` or set REC_STREAM_KEY")
        }
    }
}

/// Start an output for `url` (rtmp://, rtmps:// or http(s):// WHIP).
pub fn start_output(url: &str, key: &str) -> Result<Box<dyn Output>> {
    if url.starts_with("rtmp://") || url.starts_with("rtmps://") {
        Ok(Box::new(rtmp::RtmpOutput::start(url, key)?))
    } else if url.starts_with("http://") || url.starts_with("https://") {
        Ok(Box::new(whip::WhipOutput::start(url, key)?))
    } else {
        bail!("unsupported stream URL {url:?} (use rtmp://, rtmps:// or a WHIP http(s):// URL)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_types::PixelFormat;

    #[test]
    fn platform_presets() {
        assert_eq!(
            platform("rtmps://a.rtmps.youtube.com:443/live2"),
            Platform::YouTube
        );
        assert_eq!(platform("rtmps://live.twitch.tv:443/app"), Platform::Twitch);
        assert_eq!(platform("rtmp://localhost/live"), Platform::Other);
        // YouTube 4K: 30-50 Mb/s at 60 fps, 20-40 at 30, HEVC lower.
        assert_eq!(
            preset(Platform::YouTube, 2160, 60, false),
            (45_000, 2160, false)
        );
        assert_eq!(
            preset(Platform::YouTube, 2160, 30, true),
            (25_000, 2160, true)
        );
        assert_eq!(preset(Platform::YouTube, 1080, 60, false).0, 12_000);
        // Twitch: H.264, at most 1080p and 6 Mb/s.
        assert_eq!(
            preset(Platform::Twitch, 2160, 60, true),
            (6_000, 1080, false)
        );
        // Kick: H.264, at most 1080p and 8 Mb/s.
        assert_eq!(preset(Platform::Kick, 2160, 60, true), (8_000, 1080, false));
    }

    #[test]
    fn services_expand_and_are_recognized() {
        for (id, name, url) in SERVICES {
            assert_eq!(destination_url(id), url);
            assert_eq!(destination_url(&format!(" {} ", id.to_uppercase())), url);
            assert_eq!(destination_name(url), name);
        }
        assert_eq!(platform(&destination_url("kick")), Platform::Kick);
        // Twitch's own ingests on the same IVS domain stay Twitch.
        assert_eq!(
            platform("rtmps://ingest.global-contribute.live-video.net/app"),
            Platform::Twitch
        );
        assert_eq!(
            destination_url("rtmp://127.0.0.1/live"),
            "rtmp://127.0.0.1/live"
        );
        assert_eq!(destination_name("rtmp://127.0.0.1:1935/live"), "127.0.0.1");
    }

    /// One encode feeds every destination, so it has to fit all of them.
    #[test]
    fn shared_preset_fits_every_destination() {
        use Platform::*;
        // Replay buffer only: YouTube's numbers, H.264.
        assert_eq!(shared_preset(&[], 1080, 60, "auto"), (12_000, 1080, false));
        // YouTube alone gets HEVC on auto, H.264 when asked.
        assert_eq!(
            shared_preset(&[YouTube], 2160, 60, "auto"),
            (35_000, 2160, true)
        );
        assert_eq!(
            shared_preset(&[YouTube], 2160, 60, "h264"),
            (45_000, 2160, false)
        );
        // With Twitch and Kick: 1080p H.264 at Twitch's 6 Mb/s.
        assert_eq!(
            shared_preset(&[YouTube, Twitch, Kick], 2160, 60, "auto"),
            (6_000, 1080, false)
        );
        assert_eq!(
            shared_preset(&[YouTube, Kick], 1440, 60, "hevc"),
            (8_000, 1080, false)
        );
        // A custom server with YouTube: auto stays H.264.
        assert_eq!(
            shared_preset(&[YouTube, Other], 1080, 30, "auto"),
            (10_000, 1080, false)
        );
    }

    /// The live size is a box: the frame is fitted inside it, never upscaled,
    /// even dimensions.
    #[test]
    fn encode_size_fits_the_box() {
        let cfg = LiveConfig {
            width: 1920,
            height: 1080,
            ..Default::default()
        };
        assert_eq!(enc_size(&cfg, 3840, 2160), (1920, 1080));
        assert_eq!(enc_size(&cfg, 1280, 720), (1280, 720), "no upscaling");
        // A 4:3 frame inside 1080p stays 4:3.
        assert_eq!(enc_size(&cfg, 2048, 1536), (1440, 1080));
        assert_eq!(enc_size(&cfg, 3441, 1441), (1920, 804));
        let odd = LiveConfig {
            width: 1280,
            height: 722,
            ..Default::default()
        };
        let (w, h) = enc_size(&odd, 1280, 721);
        assert!(w % 2 == 0 && h % 2 == 0);
    }

    #[test]
    fn secrets_are_redacted() {
        assert_eq!(
            redact("publish sk_live_123 failed", "sk_live_123"),
            "publish •••• failed"
        );
        // A too-short secret would redact half the log.
        assert_eq!(redact("a and b", "a"), "a and b");
    }

    // --- feeding the encode ----------------------------------------------

    fn init() {
        let _ = gstreamer::init();
    }

    /// Feed `secs` of 30 fps frames and audio through the live encode. Every
    /// whole second the frame is white and the audio beeps, like the test
    /// pattern.
    fn feed(live: &Live, secs: f64, t0: f64) {
        feed_with(live, secs, t0, 30, 1600);
    }

    /// `chunk`: audio frames per call (audio arrives in pieces unrelated to
    /// video frames).
    fn feed_with(live: &Live, secs: f64, t0: f64, fps: usize, chunk: usize) {
        let n = (secs * fps as f64) as usize;
        let mut audio_done = 0usize;
        for i in 0..n {
            let t = t0 + i as f64 / fps as f64;
            let flash = i % fps == 0;
            let luma = if flash { 0xEB } else { 0x14 };
            live.push_video(&test_frame(t, luma));
            let until = ((i + 1) as f64 / fps as f64 * 48_000.0) as usize;
            while audio_done + chunk <= until {
                let samples: Vec<f32> = (audio_done..audio_done + chunk)
                    .flat_map(|k| {
                        let in_sec = k % 48_000;
                        let v = if in_sec < 480 {
                            0.5 * (in_sec as f32 * 0.13).sin()
                        } else {
                            0.0
                        };
                        [v, v]
                    })
                    .collect();
                live.push_audio(&AudioChunk::stereo(
                    t0 + audio_done as f64 / 48_000.0,
                    48_000,
                    samples,
                ));
                audio_done += chunk;
            }
            // 200 fps of feeding: far faster than real time, far slower than
            // the tap's cap, so no frame is dropped.
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// A 320x180 I420 frame (`VideoFrame::packed` is for packed formats).
    fn test_frame(pts: f64, luma: u8) -> VideoFrame {
        let (w, h) = (320usize, 180usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        VideoFrame {
            pts,
            width: w,
            height: h,
            format: PixelFormat::I420,
            planes: vec![
                crate::media_types::Plane::new(vec![luma; w * h], w),
                crate::media_types::Plane::new(vec![128; cw * ch], cw),
                crate::media_types::Plane::new(vec![128; cw * ch], cw),
            ],
        }
    }

    /// Records every packet's (is video, pts).
    struct Collect(Arc<Mutex<Vec<(bool, f64)>>>);

    impl Output for Collect {
        fn packet(&mut self, p: &EncodedPacket, _f: &Formats) {
            self.0.lock().unwrap().push((p.kind.is_video(), p.pts));
        }
        fn status(&self) -> OutputStatus {
            OutputStatus::default()
        }
        fn stop(&mut self) {}
    }

    /// Named by its URL; notes when it is stopped.
    struct Probe(String, Arc<Mutex<Vec<String>>>);

    impl Output for Probe {
        fn packet(&mut self, _p: &EncodedPacket, _f: &Formats) {}
        fn status(&self) -> OutputStatus {
            OutputStatus {
                name: self.0.clone(),
                ..Default::default()
            }
        }
        fn stop(&mut self) {
            self.1.lock().unwrap().push(self.0.clone());
        }
    }

    /// The tap is the capture thread's hand-off: it must never grow with a slow
    /// encoder, and it must keep the newest frames (a stream wants the current
    /// picture, not a backlog).
    #[test]
    fn the_tap_keeps_the_newest_frames_only() {
        let tap = Tap::default();
        for i in 0..10 {
            tap.push_video(test_frame(i as f64 / 30.0, 0x40));
        }
        let (video, _) = tap.take(Duration::from_millis(1));
        assert_eq!(video.len(), MAX_TAP_FRAMES, "the tap is bounded");
        assert!(
            (video.last().unwrap().pts - 9.0 / 30.0).abs() < 1e-9,
            "the newest frame survived"
        );
        assert!(video[0].pts > 0.0, "the oldest were dropped");
        assert_eq!(tap.dropped().0, (10 - MAX_TAP_FRAMES) as u64);

        // Two seconds of 10 ms audio is the other bound.
        for i in 0..300 {
            let samples = vec![0.0; 480 * 2];
            tap.push_audio(AudioChunk::stereo(i as f64 * 0.01, 48_000, samples));
        }
        let (_, audio) = tap.take(Duration::from_millis(1));
        let held: usize = audio.iter().map(|c| c.frames()).sum();
        assert!(held <= MAX_TAP_AUDIO, "{held} audio frames held");
        assert!(
            (audio.last().unwrap().pts - 2.99).abs() < 1e-6,
            "the newest buffer survived"
        );
        assert!(tap.dropped().1 > 0, "old audio buffers were dropped");
    }

    /// Nothing is encoded while no live output is active.
    #[test]
    fn nothing_is_queued_without_an_output() {
        let live = Live::new();
        assert!(!live.active());
        live.push_video(&test_frame(1.0, 0x40));
        live.push_audio(&AudioChunk::stereo(1.0, 48_000, vec![0.0; 960]));
        let (video, audio) = live.shared.tap.take(Duration::from_millis(1));
        assert!(video.is_empty() && audio.is_empty());
        live.set_replay(Some(15.0));
        assert!(live.active());
        live.push_video(&test_frame(2.0, 0x40));
        let (video, _) = live.shared.tap.take(Duration::from_millis(1));
        assert_eq!(video.len(), 1, "an active encode takes the tap");
        live.set_replay(None);
        assert!(!live.active());
    }

    /// Go Live is all or nothing: when a destination cannot start, the ones
    /// before it are stopped and removed, and the ones after it never start.
    #[test]
    fn outputs_start_all_or_nothing() {
        let live = Live::new();
        let stopped = Arc::new(Mutex::new(vec![]));
        let tried = Mutex::new(vec![]);
        let start = |url: &str| -> Result<Box<dyn Output>> {
            tried.lock().unwrap().push(url.to_string());
            if platform(url) == Platform::YouTube {
                anyhow::bail!("no stream key");
            }
            Ok(Box::new(Probe(url.to_string(), stopped.clone())))
        };
        let urls = [
            "rtmp://a/live".to_string(),
            destination_url("youtube"),
            "rtmp://c/live".to_string(),
        ];
        let e = live.add_outputs(&urls, start).unwrap_err();
        assert_eq!(
            format!("{e:#}"),
            "Could not go live on YouTube: no stream key"
        );
        assert_eq!(
            *tried.lock().unwrap(),
            ["rtmp://a/live", "rtmps://a.rtmps.youtube.com:443/live2"]
        );
        assert_eq!(*stopped.lock().unwrap(), ["rtmp://a/live"]);
        assert!(live.statuses().is_empty() && !live.active());

        let urls = ["rtmp://a/live".to_string(), "https://b/whip".to_string()];
        let added = live.add_outputs(&urls, start).unwrap();
        assert_eq!(
            added.iter().map(|(_, u)| u.as_str()).collect::<Vec<_>>(),
            urls
        );
        assert_eq!(
            live.statuses()
                .into_iter()
                .map(|(_, s)| s.name)
                .collect::<Vec<_>>(),
            urls
        );
        assert!(live.active() && stopped.lock().unwrap().len() == 1);
        live.stop_all();
        assert!(!live.active());
    }

    /// Swapping capture sessions leaves a hole of a few hundred ms; the stream
    /// must stay continuous across it (a repeated frame, silence), whatever
    /// else the box does under load.
    #[test]
    fn short_gaps_are_filled() {
        init();
        let live = Live::new();
        live.set_config(LiveConfig {
            width: 320,
            height: 180,
            bitrate: 1_000_000,
            keyframe_secs: 1.0,
            ..Default::default()
        });
        let got = Arc::new(Mutex::new(vec![]));
        live.add_output(Box::new(Collect(got.clone())));
        live.begin_session(true, false).unwrap();
        feed(&live, 2.0, 7000.0);
        std::thread::sleep(std::time::Duration::from_millis(100));
        // The second session starts 250 ms after the first one ended.
        live.begin_session(true, false).unwrap();
        feed(&live, 2.0, 7002.25);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            let n = got.lock().unwrap().iter().filter(|p| p.0).count();
            if n >= 110 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let got = got.lock().unwrap();
        let mut video: Vec<f64> = got.iter().filter(|p| p.0).map(|p| p.1).collect();
        video.sort_by(f64::total_cmp);
        assert!(video.len() > 110, "{} frames", video.len());
        // The seam: across the 250 ms hole the first session left, the picture
        // must keep its frame grid (a repeated frame) and the audio must run
        // on (silence), so nothing in the window is further apart than one
        // frame or one buffer.
        let window = |pts: &[f64]| -> Vec<f64> {
            pts.iter()
                .copied()
                .filter(|t| (7001.9..7002.6).contains(t))
                .collect()
        };
        let seam = window(&video);
        assert!(seam.len() >= 5, "frames across the seam: {seam:?}");
        let worst = seam.windows(2).map(|w| w[1] - w[0]).fold(0.0, f64::max);
        assert!(
            worst <= 1.5 / 30.0 + 1e-3,
            "video gap {worst:.4} s across the seam: {seam:?}"
        );
        let mut audio: Vec<f64> = got.iter().filter(|p| !p.0).map(|p| p.1).collect();
        audio.sort_by(f64::total_cmp);
        assert!(audio.len() > 100, "{} audio packets", audio.len());
        let seam = window(&audio);
        assert!(seam.len() >= 5, "audio packets across the seam: {seam:?}");
        let worst = seam.windows(2).map(|w| w[1] - w[0]).fold(0.0, f64::max);
        assert!(
            worst <= 0.04,
            "audio gap {worst:.4} s across the seam: {seam:?}"
        );
        drop(got);
        live.stop_all();
    }

    /// Without any audio source, the encode still carries a silent track so
    /// services that require audio accept the stream.
    #[test]
    fn silence_stands_in_for_missing_audio() {
        init();
        let live = Live::new();
        live.set_config(LiveConfig {
            width: 320,
            height: 180,
            bitrate: 1_000_000,
            keyframe_secs: 1.0,
            ..Default::default()
        });
        let got = Arc::new(Mutex::new(vec![]));
        live.add_output(Box::new(Collect(got.clone())));
        live.begin_session(false, false).unwrap();
        feed_with(&live, 1.0, 100.0, 30, 16_000);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if got.lock().unwrap().iter().filter(|p| !p.0).count() >= 20 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let packets = got.lock().unwrap().clone();
        let audio = packets.iter().filter(|p| !p.0).count();
        assert!(audio >= 20, "{audio} silent audio packets");
        drop(packets);
        live.stop_all();
    }

    #[test]
    #[ignore]
    fn sync_probe_60fps_10ms_chunks() {
        init();
        let live = Live::new();
        live.set_config(LiveConfig {
            width: 640,
            height: 360,
            bitrate: 1_000_000,
            keyframe_secs: 1.0,
            ..Default::default()
        });
        live.begin_session(true, false).unwrap();
        live.set_replay(Some(15.0));
        feed_with(&live, 12.0, 6000.0, 60, 480);
        let dir = std::env::temp_dir();
        let path = dir.join("rec-replay-60.mp4");
        live.save_replay(path.clone()).unwrap();
        println!("saved {}", path.display());
        live.set_replay(None);
    }

    #[test]
    fn replay_saves_a_playable_mp4() {
        init();
        let live = Live::new();
        live.set_config(LiveConfig {
            width: 640,
            height: 360,
            bitrate: 1_000_000,
            keyframe_secs: 1.0,
            ..Default::default()
        });
        live.begin_session(true, false).unwrap();
        live.set_replay(Some(15.0));
        feed(&live, 20.0, 5000.0);
        // Let the encoder finish the last packets.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            if live.replay_stats().is_some_and(|(s, _)| s >= 15.2) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let (held, bytes) = live.replay_stats().unwrap();
        assert!((14.8..17.5).contains(&held), "held {held}");
        assert!(bytes > 100_000, "{} bytes held", bytes);
        let dir = std::env::temp_dir().join(format!("rec-replay-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("replay.mp4");
        live.save_replay(path.clone()).unwrap();
        let out = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_name,duration",
                "-of",
                "csv=p=0",
            ])
            .arg(&path)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        if std::env::var_os("REC_KEEP_REPLAY_TEST").is_some() {
            std::fs::copy(&path, "/tmp/rec_replay_test.mp4").unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(text.contains("h264"), "{text}");
        assert!(text.contains("aac"), "{text}");
        let durs: Vec<f64> = text
            .lines()
            .filter_map(|l| l.split(',').nth(1)?.parse().ok())
            .collect();
        assert!(
            durs.iter().all(|d| (14.0..17.5).contains(d)),
            "durations {durs:?}"
        );
        live.set_replay(None);
    }
}
