//! Recording session orchestration: sources -> writer, the pacing thread, the
//! signals `pause`/`resume`/`stop` use, the pidfile and the live status line.
//!
//! One clock rules everything: a source stamps a frame or an audio chunk with
//! [`media_types::now`], the writer turns those timestamps into the constant
//! frame-rate grid, and the pacing thread emits grid slots a few tens of
//! milliseconds behind real time so a late frame still lands in its own slot.
//!
//! Adapted from rec-rs src/record.rs (MIT OR Apache-2.0), with ScreenCaptureKit
//! and AVFoundation replaced by the capture factories and the GStreamer writer.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::capture::audio::AudioRequest;
use crate::capture::camera::CameraRequest;
use crate::capture::screen;
use crate::capture::{audio as audio_capture, camera as camera_capture};
use crate::compose::{PipSpec, Scene};
use crate::gpu;
use crate::live;
use crate::media::{AudioLayout, Codec, Stats, VideoSpec, Writer, testsrc};
use crate::media_types::{self, AudioSink, FrameSink, VideoSource};

/// How long a freshly started camera needs before its exposure is close to
/// settled (upstream measured a MacBook camera: 94% of the final brightness
/// after 1.5 s, 98% after 2 s).
use crate::mixer::Volume;
use crate::synth::{Pattern, Synth};
use crate::util::{clock, db, runtime_dir};

/// What the sources, the pacer and the session share.
pub struct Shared {
    pub writer: Mutex<Option<Writer>>,
    /// Set when a source or the writer failed.
    pub error: Mutex<Option<String>>,
    /// Set when the session must end.
    pub stop: Arc<AtomicBool>,
}

impl Shared {
    /// Run `f` on the writer, if the session still has one.
    pub fn with(&self, f: impl FnOnce(&mut Writer)) {
        if let Some(w) = self.writer.lock().unwrap().as_mut() {
            f(w);
        }
    }

    /// Remember the first failure and ask the session to stop.
    pub fn fail(&self, msg: String) {
        let mut e = self.error.lock().unwrap();
        if e.is_none() {
            *e = Some(msg);
        }
        drop(e);
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Keep the source's diagnosis: transport failure is not evidence of display loss.
fn screen_capture_failure(error: &str) -> String {
    format!("screen capture stopped: {error}")
}

/// The warning shown when a started screen source delivers nothing for 3 s.
/// Upstream points at the Screen Recording grant; on Linux the actionable
/// equivalent is the portal's screen-sharing request.
const NO_FRAMES_WARNING: &str = "rec: warning: no screen frames after 3s (was the \
screen-sharing request approved? see `rec-rs permissions`)";

/// Set by SIGINT/SIGTERM/SIGHUP.
static STOP: AtomicBool = AtomicBool::new(false);
/// Pause/resume requests (SIGUSR1/SIGUSR2 from `rec-rs pause`/`resume`).
pub static PAUSE_REQ: AtomicBool = AtomicBool::new(false);
pub static RESUME_REQ: AtomicBool = AtomicBool::new(false);
/// Save-replay request (`SIGRTMIN+1` from `rec-rs save-replay`).
pub static REPLAY_REQ: AtomicBool = AtomicBool::new(false);

/// The signal `rec-rs save-replay` sends. Linux has no SIGINFO, so a real-time
/// signal stands in for it; the replay buffer itself is phase 1D.
pub fn save_replay_signal() -> libc::c_int {
    // SIGRTMIN is a function in glibc, and the first usable real-time signal
    // after the C library's own is MIN+1.
    libc::SIGRTMIN() + 1
}

extern "C" fn on_pause_signal(sig: libc::c_int) {
    if sig == libc::SIGUSR1 {
        PAUSE_REQ.store(true, Ordering::SeqCst);
    } else {
        RESUME_REQ.store(true, Ordering::SeqCst);
    }
}

extern "C" fn on_replay_signal(_: libc::c_int) {
    REPLAY_REQ.store(true, Ordering::SeqCst);
}

/// Install the pause/resume/save-replay handlers. Safe to call more than once.
pub fn install_control_signals() {
    unsafe {
        libc::signal(libc::SIGUSR1, on_pause_signal as *const () as usize);
        libc::signal(libc::SIGUSR2, on_pause_signal as *const () as usize);
        libc::signal(save_replay_signal(), on_replay_signal as *const () as usize);
    }
}

static SIGNALS: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
    // A second Ctrl-C while finalizing aborts immediately.
    if SIGNALS.fetch_add(1, Ordering::SeqCst) >= 1 {
        unsafe { libc::_exit(130) };
    }
}

/// The microphone capture backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicBackend {
    /// PipeWire capture node (the default on Linux).
    Pipewire,
    /// PulseAudio compatibility capture, if a build has it.
    Pulse,
}

// The app (phase 1E) and phase 1B's capture module read the labels and the
// capture-only options; the phase 0 CLI writes them and stops there.
impl MicBackend {
    /// Parse `--mic-backend`. Upstream's macOS values (`sck`, `avcapture`) map
    /// to PipeWire with a message instead of being silently accepted, so an old
    /// command line keeps working and says what happened.
    pub fn parse(s: &str) -> Result<MicBackend> {
        match s.trim().to_ascii_lowercase().as_str() {
            "pipewire" => Ok(MicBackend::Pipewire),
            "pulse" | "pulseaudio" => Ok(MicBackend::Pulse),
            "sck" | "avcapture" => {
                eprintln!(
                    "rec: warning: --mic-backend {s} is a macOS backend (ScreenCaptureKit/AVCapture); \
                     using pipewire on Linux"
                );
                Ok(MicBackend::Pipewire)
            }
            other => bail!("unknown --mic-backend {other:?}: use pipewire or pulse"),
        }
    }

    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            MicBackend::Pipewire => "pipewire",
            MicBackend::Pulse => "pulse",
        }
    }
}

/// Everything a recording session needs, resolved from the command line (or the
/// app's settings).
///
/// The overlay, cursor and camera fields are the phase 1A-1C surface: `prepare`
/// fills them from the flags (so parsing and validation are already exercised
/// by the tests), and the workers that consume them read them from here instead
/// of adding options of their own.
#[allow(dead_code)]
pub struct Options {
    pub output: PathBuf,
    /// Output size.
    pub size: (usize, usize),
    /// The capture target's native pixel size.
    pub native: (usize, usize),
    pub fps: u32,
    pub codec: Codec,
    /// Video bits per second.
    pub bitrate: u64,
    pub system_audio: bool,
    /// None = no mic, Some(None) = the default mic, Some(Some(id)) = that mic.
    pub mic: Option<Option<String>>,
    /// True when `--mic`/`--mic-name` was given explicitly (an unavailable
    /// backend then fails instead of warning).
    pub mic_explicit: bool,
    /// The mic's display name, used to find it again when its id changed.
    pub mic_name: Option<String>,
    /// Set when the requested mic isn't connected and the default is used.
    pub mic_warning: Option<String>,
    pub webcam: Option<Option<String>>,
    pub pip: PipSpec,
    pub cursor: bool,
    pub clicks: bool,
    pub duration: Option<f64>,
    pub quiet: bool,
    /// Replace the screen with a generated pattern (Some(noise)).
    pub test_pattern: Option<bool>,
    pub audio_layout: AudioLayout,
    /// [system, mic] fader levels at the start.
    pub volumes: [Volume; 2],
    pub mic_backend: MicBackend,
    /// What to capture (None with `--test-pattern`).
    pub screen: Option<screen::ScreenRequest>,
    /// Initial scene (only Screen without a webcam).
    pub scene: Scene,
    /// System audio offset in ms; None = the output device's latency.
    pub system_offset_ms: Option<f64>,
    /// Microphone offset in ms.
    pub mic_offset_ms: f64,
    /// Write the recording file (false: replay buffer / streams only).
    pub write: bool,
    /// Shared live encode for the replay buffer and streams (phase 1D).
    pub live: Option<Arc<live::Live>>,
    /// The app offers every emitted (composited) frame here, at most 30 times
    /// a second (phase 1E's preview). Non-blocking.
    pub preview: Option<FrameSink>,
    /// The app's own wgpu device, when it has one: the compositor draws on it
    /// so the preview can read the composited texture instead of a readback
    /// (G10). Falls back to a device of its own when this is `None` or fails.
    pub gpu: Option<gpu::SharedGpu>,
    /// Where the composited texture goes, when the compositor shares the app's
    /// device. Ignored otherwise (the texture would belong to another device).
    pub preview_gpu: Option<gpu::PreviewSink>,
    /// Stream destinations (URL, key) to start with the session.
    pub streams: Vec<(String, String)>,
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("output", &self.output)
            .field("size", &self.size)
            .field("fps", &self.fps)
            .field("codec", &self.codec)
            .field("bitrate", &self.bitrate)
            .field("system_audio", &self.system_audio)
            .field("mic", &self.mic)
            .field("webcam", &self.webcam)
            .field("duration", &self.duration)
            .field("test_pattern", &self.test_pattern)
            .field("write", &self.write)
            .finish_non_exhaustive()
    }
}

/// How many cameras this process opened (tests prove a hand-over opens none).
#[cfg(test)]
pub(crate) static CAMERA_OPENS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// The number of cameras opened so far (tests only).
#[cfg(test)]
pub(crate) fn camera_opens() -> usize {
    CAMERA_OPENS.load(Ordering::SeqCst)
}

/// A running camera, handed from one session to the next (phase 1E).
///
/// The device stays open across a preview → recording → preview hand-over, so
/// a recording starts with the exposure already settled (upstream's
/// `take_camera`/`start_with_camera`) and the camera LED never restarts.
/// Frames go to whatever session last adopted it; between sessions they go
/// nowhere, so a finished writer never sees them.
pub struct Camera {
    source: Box<dyn VideoSource>,
    /// The session the frames go to, or None while being handed over.
    target: Arc<Mutex<Option<FrameSink>>>,
    desc: String,
}

impl Camera {
    /// Start `source` with a forwarding sink, so the next session can adopt it
    /// without stopping the device.
    fn start(source: Box<dyn VideoSource>, sink: FrameSink) -> Result<Camera> {
        let target: Arc<Mutex<Option<FrameSink>>> = Arc::new(Mutex::new(Some(sink)));
        let forward = target.clone();
        let forwarder: FrameSink = Arc::new(move |frame| {
            let to = forward.lock().ok().and_then(|t| t.clone());
            if let Some(to) = to {
                to(frame);
            }
        });
        let desc = source.describe();
        let mut cam = Camera {
            source,
            target,
            desc,
        };
        cam.source.start(forwarder)?;
        Ok(cam)
    }

    /// Point the running camera's frames at `sink` (a new session adopting it).
    fn adopt(&self, sink: FrameSink) {
        if let Ok(mut t) = self.target.lock() {
            *t = Some(sink);
        }
    }

    /// Frames go nowhere until another session adopts the camera.
    fn detach(&self) {
        if let Ok(mut t) = self.target.lock() {
            *t = None;
        }
    }

    /// The device name, for the app and the start line.
    pub fn describe(&self) -> &str {
        &self.desc
    }

    /// Stop the camera; the device's LED goes off.
    pub fn stop(&mut self) {
        if let Err(e) = self.source.stop() {
            eprintln!("rec: warning: stopping the camera: {e}");
        }
        self.detach();
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        // An idle spare camera must not keep the device (and its LED) on.
        let _ = self.source.stop();
        self.detach();
    }
}

/// A running capture session.
pub struct Session {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    pacer: Option<std::thread::JoinHandle<()>>,
    disk_monitor: Option<std::thread::JoinHandle<()>>,
    disk_failed: Arc<AtomicBool>,
    video: Option<Box<dyn VideoSource>>,
    camera: Option<Camera>,
    /// What the camera reported (its device name), for the start log and the app.
    camera_desc: String,
    audio: Vec<Box<dyn crate::media_types::AudioSource>>,
    /// The file being written, if any. The app shows it (phase 1E).
    #[allow(dead_code)]
    pub output: Option<PathBuf>,
    pub started: Instant,
    pub description: String,
    /// The video/audio encoders the pipeline chose, for the start log.
    pub encoder: String,
}

/// Open the capture sources a session will use: the screen or the test
/// pattern, and the system audio and microphone that are actually available.
///
/// Called before the writer is built so the file's track set follows these
/// sources: a source that failed to open gets no track (and warns, because
/// audio never blocks a session), while a source that opened but never
/// delivers keeps its track.
struct OpenedSources {
    video: Box<dyn VideoSource>,
    screen_error: Option<screen::ScreenError>,
    audio: Vec<Box<dyn crate::media_types::AudioSource>>,
    description: String,
}

fn open_sources(
    o: &Options,
    mic: Option<Box<dyn crate::media_types::AudioSource>>,
) -> Result<OpenedSources> {
    let mut audio: Vec<Box<dyn crate::media_types::AudioSource>> = vec![];
    let video: Box<dyn VideoSource>;
    let mut screen_error = None;
    let description;
    match o.test_pattern {
        Some(noise) => {
            // REC_SYNTH_NATIVE=WxH makes the pattern larger than the output,
            // to exercise the scaler (a native-size display into a smaller
            // recording).
            let (pw, ph) = std::env::var("REC_SYNTH_NATIVE")
                .ok()
                .and_then(|v| {
                    let (w, h) = v.split_once('x')?;
                    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
                })
                .unwrap_or((o.size.0, o.size.1));
            let synth = Synth::new(Pattern {
                width: pw,
                height: ph,
                fps: o.fps,
                format: media_types::PixelFormat::Bgra,
                noise,
            });
            description = synth.describe();
            // Test hook (scripts/audio/integration.sh): take system audio from
            // PipeWire instead of the pattern's own beeps.
            let pattern_audio = match std::env::var("REC_PATTERN_SYSTEM_AUDIO").as_deref() {
                Err(_) | Ok("") => false,
                Ok("pipewire") => true,
                Ok(other) => {
                    bail!("REC_PATTERN_SYSTEM_AUDIO={other}: only `pipewire` is supported")
                }
            };
            // Both halves of the pattern share one origin, so flash and beep
            // stay aligned.
            if o.system_audio {
                if pattern_audio {
                    match audio_capture::open_system() {
                        Ok(src) => audio.push(src),
                        Err(e) => {
                            eprintln!("rec: warning: {e}; recording without system audio")
                        }
                    }
                } else {
                    audio.push(Box::new(synth.audio(true)));
                }
            }
            video = Box::new(synth);
        }
        None => {
            let req = o.screen.clone().unwrap_or_default();
            let (src, error) = screen::open_with_status(
                &req,
                &screen::CaptureOptions {
                    cursor: o.cursor,
                    clicks: o.clicks,
                },
            )?;
            description = src.describe();
            screen_error = error;
            video = src;
            if o.system_audio {
                match audio_capture::open_system() {
                    Ok(src) => audio.push(src),
                    Err(e) => eprintln!("rec: warning: {e}; recording without system audio"),
                }
            }
        }
    }
    if let Some(mic) = mic {
        audio.push(mic);
    }
    Ok(OpenedSources {
        video,
        screen_error,
        audio,
        description,
    })
}

impl Session {
    /// Start capturing and writing. `o.write` false = preview-only.
    pub fn start(o: &Options) -> Result<Session> {
        Self::start_with_camera(o, None)
    }

    /// `start`, taking over a running camera (from [`take_camera`](Session::take_camera)):
    /// the device stays open, so a recording starts with its exposure already
    /// settled instead of waiting the 2 s cold-camera gate. A camera the
    /// options no longer want is stopped.
    pub fn start_with_camera(o: &Options, cam: Option<Camera>) -> Result<Session> {
        // Disk admission precedes devices and file creation. Live-only sessions
        // neither require a destination nor touch its filesystem.
        let interval = crate::media::disk::check_interval()?;
        let disk = if o.write {
            Some(crate::media::disk::preflight(&o.output, o.bitrate)?)
        } else {
            None
        };
        let (w, h) = o.size;
        let spec = VideoSpec {
            width: w,
            height: h,
            fps: o.fps,
            codec: o.codec,
            bitrate: o.bitrate,
        };
        let path = o.write.then_some(o.output.as_path());
        // The microphone is opened before the writer: its track is only
        // created when the device is really there. A track whose source never
        // delivers makes the muxer wait for it and stalls every other stream
        // (measured: a requested but silent mic stopped 4K video).
        let mut mic_source: Option<Box<dyn crate::media_types::AudioSource>> = None;
        if let Some(sel) = &o.mic {
            let req = AudioRequest {
                device: sel.clone(),
                name: o.mic_name.clone(),
            };
            match testsrc::mic_from_env() {
                // A synthetic microphone stands in for the device in tests.
                Some(src) => mic_source = Some(src),
                None => match audio_capture::open_mic(&req) {
                    Ok(src) => mic_source = Some(src),
                    // An explicit device request fails; the default mic only
                    // warns, because audio never blocks a session (upstream's
                    // rule).
                    Err(e) if o.mic_explicit => {
                        return Err(e.context("the microphone you asked for could not be opened"));
                    }
                    Err(e) => eprintln!("rec: warning: {e}; recording without the microphone"),
                },
            }
        }
        // Open the capture sources before the writer: the file's audio tracks
        // follow the sources that really opened. A default sink that is gone
        // (1B's G4 fix warns and records) must not leave a silent system track
        // in the file; a source that opened but stays silent keeps its track.
        let opened = open_sources(o, mic_source.take())?;
        let system_ok = opened
            .audio
            .iter()
            .any(|a| a.kind() == media_types::AudioKind::System);
        let has_mic = opened
            .audio
            .iter()
            .any(|a| a.kind() == media_types::AudioKind::Mic);
        let mut writer = Writer::new(
            path,
            &spec,
            system_ok,
            has_mic,
            o.audio_layout,
            o.volumes,
            o.scene,
            o.pip.clone(),
        )?;
        writer.limit_secs(o.duration);
        // A camera scene may be chosen before the camera's first frame.
        writer.set_camera_expected(o.webcam.is_some());
        // The app's preview sees exactly what is written, overlay included.
        if let Some(p) = &o.preview {
            writer.set_preview(p.clone());
        }
        // One shared live encode for the replay buffer and the streams.
        if let Some(l) = &o.live {
            let _ = l.begin_session(o.system_audio, has_mic);
            writer.set_live(l.clone());
        }
        // Phase 1C's compositor draws the scenes; until it lands the writer
        // falls back to the CPU path (and says what it cannot draw).
        if o.webcam.is_some() {
            // The recording needs one composited frame per grid slot, so the
            // writer builds the renderer with a synchronous readback
            // (`pipeline_depth: 1`); the app's preview uses its own texture.
            let opts = gpu::Options {
                width: w,
                height: h,
                scene: o.scene,
                pip: o.pip.clone(),
                output: gpu::OutputFormat::Nv12,
                // A window capture's rounded transparent corners are filled
                // with the window's edge colour before encoding.
                corner_fill: o.test_pattern.is_none()
                    && o.screen.as_ref().is_some_and(|s| s.window.is_some()),
                pipeline_depth: 1,
            };
            // On the app's own device when it handed one over, so the preview
            // can draw the composited texture with no readback; a device of its
            // own otherwise (the CLI), and the same device if the shared one
            // cannot take the compositor.
            let shared = o.gpu.as_ref().map(|g| {
                gpu::Renderer::with_device(g.device.clone(), g.queue.clone(), g.info.clone(), &opts)
            });
            let built = match shared {
                Some(Ok(r)) => Some(Ok(r)),
                Some(Err(e)) => {
                    if std::env::var_os("REC_DEBUG").is_some() {
                        eprintln!(
                            "rec: debug: the app's GPU device cannot composite ({e:#}); using one of its own"
                        );
                    }
                    Some(gpu::Renderer::with_options(&opts))
                }
                None => Some(gpu::Renderer::with_options(&opts)),
            };
            match built.expect("always built") {
                Ok(r) => {
                    if !o.quiet {
                        eprintln!("rec: compositor {}", r.describe_adapter());
                    }
                    // Only a renderer on the app's device may publish textures.
                    if o.gpu.is_some()
                        && let Some(sink) = o.preview_gpu.clone()
                        && r.shares_app_device()
                    {
                        writer.set_preview_gpu(sink);
                    }
                    writer.set_renderer(Some(r));
                }
                Err(e) => {
                    if std::env::var_os("REC_DEBUG").is_some() {
                        eprintln!("rec: debug: no GPU compositor: {e}");
                    }
                }
            }
        }
        // System audio is captured when mixed and heard one output latency
        // later; the synthetic source needs no shift.
        let sys_ms = match (o.system_offset_ms, o.test_pattern) {
            (Some(ms), _) => ms,
            (None, Some(_)) => 0.0,
            (None, None) => audio_capture::output_latency().unwrap_or(0.0) * 1000.0,
        };
        writer.set_audio_offsets([sys_ms / 1000.0, o.mic_offset_ms / 1000.0]);
        let encoder = writer.encoder_name().to_string();
        if std::env::var_os("REC_DEBUG").is_some() {
            eprintln!(
                "rec: debug: audio offsets: system {sys_ms:+.1} ms, mic {:+.1} ms",
                o.mic_offset_ms
            );
        }
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Shared {
            writer: Mutex::new(Some(writer)),
            error: Mutex::new(None),
            stop: stop.clone(),
        });
        let mut sess = Session {
            shared: shared.clone(),
            stop,
            pacer: None,
            disk_monitor: None,
            disk_failed: Arc::new(AtomicBool::new(false)),
            video: Some(opened.video),
            camera: None,
            camera_desc: String::new(),
            audio: opened.audio,
            output: o.write.then(|| o.output.clone()),
            started: Instant::now(),
            description: opened.description,
            encoder,
        };
        match sess.start_sources(o, cam) {
            Ok(()) => {}
            Err(e) => {
                sess.stop_sources();
                if let Some(w) = shared.writer.lock().unwrap().take() {
                    let _ = w.finish(0.0);
                }
                if o.write {
                    let _ = std::fs::remove_file(&o.output);
                }
                return Err(e);
            }
        }
        // Pacer: emit constant-frame-rate video a little behind real time.
        let pacer_shared = shared.clone();
        let pacer_stop = sess.stop.clone();
        let fps = o.fps;
        // Upstream's ScreenCaptureKit didStopWithError calls shared.fail
        // (reference/rec-rs/src/screen.rs:165-170), stopping the session while
        // finish still finalizes the file (reference/rec-rs/src/record.rs:459-479).
        let screen_error = opened.screen_error;
        sess.pacer = Some(std::thread::spawn(move || {
            let period = Duration::from_secs_f64(1.0 / fps as f64 / 2.0);
            while !pacer_stop.load(Ordering::SeqCst) {
                if let Some(error) = screen_error
                    .as_ref()
                    .and_then(|e| e.lock().unwrap().clone())
                {
                    pacer_shared.fail(screen_capture_failure(&error));
                    break;
                }
                let now = media_types::now();
                let mut failure = None;
                pacer_shared.with(|w| {
                    w.emit_until(now - w.lag(), 4);
                    failure = w.failure().map(str::to_string);
                });
                if let Some(f) = failure {
                    pacer_shared.fail(format!("writer failed: {f}"));
                }
                std::thread::sleep(period);
            }
        }));
        if let Some((directory, low_water)) = disk {
            let shared = shared.clone();
            let disk_failed = sess.disk_failed.clone();
            // Never statvfs under the writer lock or on the frame pacing thread.
            sess.disk_monitor = Some(std::thread::spawn(move || {
                while !shared.stop.load(Ordering::SeqCst) {
                    if let Err(error) = crate::media::disk::check(&directory, low_water) {
                        disk_failed.store(true, Ordering::SeqCst);
                        shared.fail(error.to_string());
                        break;
                    }
                    std::thread::park_timeout(interval);
                }
            }));
        }
        sess.started = Instant::now();
        Ok(sess)
    }

    fn start_sources(&mut self, o: &Options, handed: Option<Camera>) -> Result<()> {
        let vsink: FrameSink = {
            let shared = self.shared.clone();
            Arc::new(move |f| shared.with(|w| w.push_video(f)))
        };
        let camsink: FrameSink = {
            let shared = self.shared.clone();
            Arc::new(move |f| shared.with(|w| w.push_cam(f)))
        };
        let asink: AudioSink = {
            let shared = self.shared.clone();
            Arc::new(move |kind, chunk| shared.with(|w| w.push_audio(kind, &chunk)))
        };
        // The camera starts first: a cold one holds the file's start until its
        // exposure has settled (upstream's 2 s gate), so the first frames are
        // not dark, and its frames feed the scene from the first slot.
        if let Some(sel) = &o.webcam {
            // A camera handed over from the preview is already running (and
            // already warm): adopt it instead of opening the device again.
            let (cam, settle) = match handed {
                Some(cam) => {
                    cam.adopt(camsink.clone());
                    (cam, 0.0)
                }
                None => {
                    let req = CameraRequest {
                        device: sel.clone(),
                    };
                    // The camera source knows how much of upstream's settle
                    // time is left (a device that was already running keeps
                    // its exposure).
                    let (src, settle): (Box<dyn VideoSource>, f64) =
                        match testsrc::camera_from_env() {
                            Some(c) => (c, camera_capture::CAMERA_SETTLE_SECS),
                            None => {
                                let src = camera_capture::open_source(&req)?;
                                let settle = src.settle_remaining();
                                (Box::new(src), settle)
                            }
                        };
                    #[cfg(test)]
                    CAMERA_OPENS.fetch_add(1, Ordering::SeqCst);
                    (Camera::start(src, camsink.clone())?, settle)
                }
            };
            self.camera_desc = cam.describe().to_string();
            // A preview just shows the dark frames; only a file waits, and a
            // warm camera has nothing left to settle.
            if o.write && o.scene.uses_cam() {
                self.shared.with(|w| w.wait_for_camera(settle));
            }
            self.camera = Some(cam);
        } else if let Some(mut cam) = handed {
            // The scene no longer wants the camera: close it rather than leak
            // the device (its LED would stay on).
            cam.stop();
        }
        if let Some(v) = &mut self.video {
            v.start(vsink)?;
        }
        for a in &mut self.audio {
            a.start(asink.clone())?;
        }
        Ok(())
    }

    fn stop_sources(&mut self) {
        if let Some(mut v) = self.video.take()
            && let Err(e) = v.stop()
        {
            eprintln!("rec: warning: stopping capture: {e}");
        }
        if let Some(mut c) = self.camera.take() {
            c.stop();
        }
        for mut a in self.audio.drain(..) {
            if let Err(e) = a.stop() {
                eprintln!("rec: warning: stopping audio: {e}");
            }
        }
    }

    /// True once a source or the writer failed (see [`error`](Session::error)).
    pub fn failed(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// What failed, for the app's banner (phase 1E).
    #[allow(dead_code)]
    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    /// True once the first frame arrived (the timeline started).
    pub fn has_frames(&self) -> bool {
        let mut ok = false;
        self.shared.with(|w| ok = w.started());
        ok
    }

    pub fn pause(&self) -> bool {
        let mut ok = false;
        let now = media_types::now();
        self.shared.with(|w| ok = w.pause(now));
        ok
    }

    pub fn resume(&self) -> bool {
        let mut ok = false;
        let now = media_types::now();
        self.shared.with(|w| ok = w.resume(now));
        ok
    }

    pub fn is_paused(&self) -> bool {
        let mut p = false;
        self.shared.with(|w| p = w.is_paused());
        p
    }

    /// Recorded (output) seconds so far, excluding pauses.
    pub fn recorded_secs(&self) -> f64 {
        let mut t = 0.0;
        self.shared.with(|w| t = w.output_secs());
        t
    }

    /// True once `--duration` is reached, counted in output frames.
    pub fn reached_limit(&self) -> bool {
        let mut done = false;
        self.shared.with(|w| done = w.reached_limit());
        done
    }

    /// Move the audio faders [system, mic] live. The app's mixer widgets call
    /// this (phase 1E); the CLI sets the levels once, at start.
    #[allow(dead_code)]
    pub fn set_volumes(&self, volumes: [Volume; 2]) {
        self.shared.with(|w| w.set_volumes(volumes));
    }

    /// Take the camera out of this session, still running, for the next one
    /// ([`start_with_camera`](Session::start_with_camera)): the preview hands it
    /// to the recording and the recording hands it back. Its frames go nowhere
    /// until then.
    pub fn take_camera(&mut self) -> Option<Camera> {
        let cam = self.camera.take()?;
        cam.detach();
        self.camera_desc.clear();
        Some(cam)
    }

    /// The camera in use, for the status line and the app (phase 1E).
    #[allow(dead_code)]
    pub fn camera_name(&self) -> Option<&str> {
        (!self.camera_desc.is_empty()).then_some(self.camera_desc.as_str())
    }

    /// Switch scenes live. Scenes that need the overlay require the GPU
    /// compositor (phase 1C); the writer returns false when it cannot draw it.
    #[allow(dead_code)]
    pub fn set_scene(&self, scene: Scene) -> bool {
        let mut ok = false;
        self.shared.with(|w| ok = w.set_scene(scene));
        ok
    }

    /// Move or restyle the camera overlay live; it is baked into the file from
    /// the next emitted frame on.
    #[allow(dead_code)]
    pub fn set_pip(&self, pip: PipSpec) {
        self.shared.with(|w| w.set_pip(pip));
    }

    /// The camera's aspect ratio, once its frames arrive.
    #[allow(dead_code)]
    pub fn cam_aspect(&self) -> Option<f64> {
        let mut a = None;
        self.shared.with(|w| a = w.cam_aspect());
        a
    }

    /// (stats, peak levels [system, mic] since the last call).
    pub fn poll(&self) -> (Stats, [f32; 2]) {
        let mut st = Stats::default();
        let mut lv = [0f32; 2];
        self.shared.with(|w| {
            st = w.stats.clone();
            lv = w.take_levels();
        });
        (st, lv)
    }

    /// Stop capture and finalize the file (if any). Blocks until written.
    pub fn finish(mut self) -> Result<Stats> {
        let end = media_types::now();
        self.stop.store(true, Ordering::SeqCst);
        if let Some(monitor) = self.disk_monitor.take() {
            monitor.thread().unpark();
            let _ = monitor.join();
        }
        if let Some(p) = self.pacer.take() {
            let _ = p.join();
        }
        self.stop_sources();
        let writer = self.shared.writer.lock().unwrap().take();
        let result = match writer {
            Some(w) => w.finish(end),
            None => bail!("internal: writer missing"),
        };
        if let Some(e) = self.shared.error.lock().unwrap().take() {
            eprintln!("rec: error during recording: {e}");
            if self.disk_failed.load(Ordering::SeqCst) {
                result?;
                bail!("{e}");
            }
        }
        result
    }
}

/// CLI recording: run until Ctrl-C/SIGTERM, `rec stop`, `--duration`, or an
/// error.
pub fn run(o: Options) -> Result<Stats> {
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as usize);
        libc::signal(libc::SIGTERM, on_signal as *const () as usize);
        libc::signal(libc::SIGHUP, on_signal as *const () as usize);
    }
    install_control_signals();
    // Control over the shared socket first (it crosses PID namespaces, as a
    // Flatpak's do): a second launch's `stop`/`pause`/`resume`/`save-replay`
    // reaches this recorder, and a live listener makes a second recorder refuse
    // to start. The signals stay as the fallback for a bare shell.
    let _control = crate::app::control::serve_recorder(std::sync::Arc::new(|cmd| {
        use crate::app::control::Control;
        match cmd {
            Control::Stop => {
                STOP.store(true, Ordering::SeqCst);
                true
            }
            Control::Pause => {
                PAUSE_REQ.store(true, Ordering::SeqCst);
                true
            }
            Control::Resume => {
                RESUME_REQ.store(true, Ordering::SeqCst);
                true
            }
            Control::SaveReplay => {
                REPLAY_REQ.store(true, Ordering::SeqCst);
                true
            }
            _ => false,
        }
    }))
    .map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            anyhow::anyhow!("another recorder is running; stop it with `rec-rs stop`")
        } else {
            anyhow::anyhow!("could not open the control socket: {e}")
        }
    })?;
    let mic_desc = match &o.mic {
        None => "off".to_string(),
        Some(None) => "default".to_string(),
        Some(Some(id)) => id.clone(),
    };
    let pidfile = pidfile_path();
    if let Some(dir) = pidfile.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(
        &pidfile,
        format!("{}\n{}\n", std::process::id(), o.output.display()),
    );
    // --- live session (phase 1D) ------------------------------------------
    // One encode for the replay buffer and every stream, independent of the
    // writer and its pauses. Going live is all or nothing: a destination that
    // cannot start stops the ones before it and the session never starts.
    let mut live_streams: Vec<(u64, String)> = vec![];
    if let Some(l) = &o.live {
        l.begin_session(o.system_audio, o.mic.is_some())?;
        let urls: Vec<String> = o.streams.iter().map(|(u, _)| u.clone()).collect();
        let keys: Vec<String> = o.streams.iter().map(|(_, k)| k.clone()).collect();
        let started = l.add_outputs(&urls, |url| {
            let key = urls
                .iter()
                .position(|u| u == url)
                .map(|i| keys[i].clone())
                .unwrap_or_default();
            live::start_output(url, &key)
        });
        match started {
            Ok(added) => live_streams = added,
            Err(e) => {
                let _ = std::fs::remove_file(&pidfile);
                return Err(e);
            }
        }
    }
    let sess = match Session::start(&o) {
        Ok(s) => s,
        Err(e) => {
            if let Some(l) = &o.live {
                for (id, _) in live_streams.drain(..) {
                    l.remove_output(id);
                }
            }
            let _ = std::fs::remove_file(&pidfile);
            return Err(e);
        }
    };
    let (w, h) = o.size;
    if !o.quiet {
        eprintln!(
            "rec: {} {} -> {} ({}x{} @ {}fps {:?}{}, system audio {}, mic {}, webcam {})",
            if o.write { "recording" } else { "capturing" },
            sess.description,
            if o.write {
                o.output.display().to_string()
            } else {
                "no file".into()
            },
            w,
            h,
            o.fps,
            o.codec,
            if o.codec == Codec::Prores {
                String::new()
            } else {
                format!(" {} Mb/s", o.bitrate / 1_000_000)
            },
            if o.system_audio { "on" } else { "off" },
            mic_desc,
            sess.camera_name().unwrap_or("off"),
        );
        if o.write && std::env::var_os("REC_DEBUG").is_some() {
            eprintln!("rec: debug: encoder {}", sess.encoder);
        }
        eprintln!("rec: press Ctrl-C to stop");
    }

    let mut last_status = Instant::now();
    let mut warned_no_frames = false;
    let mut saves: Vec<std::thread::JoinHandle<Result<PathBuf>>> = vec![];
    while !STOP.load(Ordering::SeqCst) && !sess.failed() {
        std::thread::sleep(Duration::from_millis(20));
        // Requests wait for the first frame (the timeline starts there).
        if sess.has_frames() && PAUSE_REQ.swap(false, Ordering::SeqCst) && sess.pause() && !o.quiet
        {
            eprint!("\r\x1b[2Krec: paused (`rec-rs resume` to continue)\n");
        }
        if sess.has_frames()
            && RESUME_REQ.swap(false, Ordering::SeqCst)
            && sess.resume()
            && !o.quiet
        {
            eprint!("\r\x1b[2Krec: resumed\n");
        }
        if REPLAY_REQ.swap(false, Ordering::SeqCst) {
            let dir = o
                .output
                .parent()
                .map(PathBuf::from)
                .unwrap_or_else(crate::util::videos_dir);
            match save_replay_request(o.live.as_deref(), &dir) {
                Ok(h) => saves.push(h),
                Err(e) => eprint!("\r\x1b[2Krec: replay: {e:#}\n"),
            }
        }
        for h in std::mem::take(&mut saves) {
            if h.is_finished() {
                match h.join() {
                    Ok(Ok(p)) => eprint!("\r\x1b[2Krec: replay saved {}\n", p.display()),
                    Ok(Err(e)) => eprint!("\r\x1b[2Krec: replay: {e:#}\n"),
                    Err(_) => eprint!("\r\x1b[2Krec: replay: the writer panicked\n"),
                }
            } else {
                saves.push(h);
            }
        }
        let elapsed = sess.started.elapsed().as_secs_f64();
        if o.duration.is_some() && (sess.reached_limit() || (!sess.has_frames() && elapsed > 10.0))
        {
            break;
        }
        if !warned_no_frames && elapsed > 3.0 && !sess.has_frames() {
            warned_no_frames = true;
            eprintln!("{NO_FRAMES_WARNING}");
        }
        if !o.quiet && last_status.elapsed() >= Duration::from_secs(1) {
            last_status = Instant::now();
            let (st, lv) = sess.poll();
            let mut live_status = String::new();
            if let Some(l) = &o.live {
                if let Some((secs, bytes)) = l.replay_stats() {
                    live_status += &format!("  replay {secs:.0}s {:.1} MB", bytes as f64 / 1e6);
                }
                for (_, s) in l.statuses() {
                    live_status += &format!(
                        "  {} {} {:.1} Mb/s drop {}",
                        s.name,
                        s.state,
                        s.bitrate / 1e6,
                        s.dropped_frames
                    );
                }
            }
            eprint!(
                "\r\x1b[2K  {}{}  video {} fr ({} dup, {} drop)  sys {} dB  mic {} dB{}  ",
                clock(sess.recorded_secs()),
                if sess.is_paused() { " PAUSED" } else { "" },
                st.video_frames_out,
                st.video_dups,
                st.video_drops,
                if o.system_audio {
                    db(lv[0])
                } else {
                    "   off".into()
                },
                if o.mic.is_some() {
                    db(lv[1])
                } else {
                    "   off".into()
                },
                live_status,
            );
        }
    }
    if !o.quiet {
        eprintln!("\nrec: stopping...");
    }
    let result = sess.finish();
    if let Some(l) = &o.live {
        for (id, _) in live_streams {
            l.remove_output(id);
        }
    }
    for h in saves {
        match h.join() {
            Ok(Ok(p)) => eprintln!("rec: replay saved {}", p.display()),
            Ok(Err(e)) => eprintln!("rec: replay: {e:#}"),
            Err(_) => {}
        }
    }
    let _ = std::fs::remove_file(&pidfile);
    result
}

/// `rec-rs save-replay`: start writing the buffer, or say why there is nothing
/// to write.
///
/// A recording without `--replay` has no buffer, and the message is upstream's
/// (its `Live::save_replay` says `the replay buffer is off`), printed the same
/// way: `rec: replay: the replay buffer is off`.
fn save_replay_request(
    live: Option<&live::Live>,
    dir: &std::path::Path,
) -> Result<std::thread::JoinHandle<Result<PathBuf>>> {
    let live = live.ok_or_else(|| anyhow::anyhow!("the replay buffer is off"))?;
    live.save_replay_async(dir.join(crate::util::timestamped("replay", "mp4")))
}

/// The pidfile of the running recorder.
///
/// `$XDG_RUNTIME_DIR/rec-rs.pid`, else `/tmp/rec-rs-$UID/rec-rs.pid` (created
/// 0700). It holds the pid, then the output path, one per line.
pub fn pidfile_path() -> PathBuf {
    runtime_dir().join("rec-rs.pid")
}

/// A running recorder's pid and output file.
fn running() -> Result<(i32, String)> {
    // Upstream's text, word for word: it does not name the pidfile, and neither
    // do we (`record.rs:643` upstream).
    let text = std::fs::read_to_string(pidfile_path())
        .map_err(|_| anyhow::anyhow!("no recording in progress"))?;
    let mut lines = text.lines();
    let pid: i32 = lines
        .next()
        .unwrap_or("")
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("corrupt pidfile"))?;
    Ok((pid, lines.next().unwrap_or("").to_string()))
}

/// Send `sig` to the running recorder (SIGUSR1 pause, SIGUSR2 resume,
/// SIGRTMIN+1 save replay).
pub fn signal_running(sig: libc::c_int) -> Result<()> {
    let (pid, _) = running()?;
    if unsafe { libc::kill(pid, sig) } != 0 {
        // Upstream: "no recording in progress (pid {pid} not running)".
        bail!("no recording in progress (pid {pid} not running)");
    }
    Ok(())
}

/// Send a control command to the running recorder: over the socket first (it
/// crosses PID namespaces, so a second `flatpak run` reaches the recorder),
/// then, when nothing listens, the pidfile signal as before.
fn control(cmd: crate::app::control::Control, sig: libc::c_int) -> Result<()> {
    match crate::app::control::send_to_recorder(cmd) {
        Ok(true) => Ok(()),
        Ok(false) => bail!("the running recorder does not handle `{}`", cmd.name()),
        Err(_) => {
            // No socket, or a stale one from a crash: drop it and use the
            // pidfile signal, which still works outside a namespace.
            crate::app::control::remove_stale_recorder_socket();
            signal_running(sig)
        }
    }
}

/// Pause the running recording (socket, else SIGUSR1).
pub fn pause_running() -> Result<()> {
    control(crate::app::control::Control::Pause, libc::SIGUSR1)
}

/// Resume the running recording (socket, else SIGUSR2).
pub fn resume_running() -> Result<()> {
    control(crate::app::control::Control::Resume, libc::SIGUSR2)
}

/// Save the running recorder's replay buffer (socket, else SIGRTMIN+1).
pub fn save_replay_running() -> Result<()> {
    control(
        crate::app::control::Control::SaveReplay,
        save_replay_signal(),
    )
}

/// Ask a running recorder (the app or `rec-rs record`) to stop, wait for it to
/// finish, then print the file.
///
/// The socket carries the command when one listens (a Flatpak instance's pid is
/// meaningless in another instance); the signal is the fallback. Either way the
/// wait is on the pidfile, which the recorder removes once the file is
/// finalized, so a pid from another namespace never matters.
pub fn stop_running() -> Result<()> {
    let path = pidfile_path();
    let (pid, file) = running().map_err(|_| anyhow::anyhow!("no recording in progress"))?;
    let pid_live = unsafe { libc::kill(pid, 0) } == 0;
    match crate::app::control::send_to_recorder(crate::app::control::Control::Stop) {
        Ok(true) => {}
        Ok(false) => bail!("the running recorder does not handle `stop`"),
        Err(_) => {
            crate::app::control::remove_stale_recorder_socket();
            if !pid_live {
                let _ = std::fs::remove_file(&path);
                bail!("no recording in progress (stale pidfile removed)");
            }
            unsafe { libc::kill(pid, libc::SIGINT) };
        }
    }
    let deadline = Instant::now() + Duration::from_secs(90);
    // Done when the recorder removes its pidfile (file finalized).
    while path.exists() {
        if Instant::now() > deadline {
            // Upstream's text (it names the pid; the file is what we wait on).
            bail!("recorder (pid {pid}) did not exit within 90s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("{file}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_no_frames_warning_points_at_the_permission() {
        // Upstream's "(is Screen Recording permission granted?)"; Linux's
        // actionable equivalent is the portal request and `rec-rs permissions`.
        assert!(
            NO_FRAMES_WARNING.contains("permissions"),
            "{NO_FRAMES_WARNING}"
        );
        assert!(
            NO_FRAMES_WARNING.contains("screen-sharing request"),
            "{NO_FRAMES_WARNING}"
        );
        assert!(NO_FRAMES_WARNING.starts_with("rec: warning: no screen frames after 3s"));
    }

    #[test]
    fn screen_failure_preserves_transport_vs_display_diagnosis() {
        let remote = screen_capture_failure("PipeWire stopped or its remote failed: disconnected");
        assert_eq!(
            remote,
            "screen capture stopped: PipeWire stopped or its remote failed: disconnected"
        );
        assert!(!remote.contains("display disconnected"));
        let display = "the shared screen may have gone away (display disconnected or sharing ended): target removed";
        assert_eq!(
            screen_capture_failure(display),
            format!("screen capture stopped: {display}")
        );
    }

    #[test]
    fn mic_backend_maps_upstream_values() {
        assert_eq!(MicBackend::parse("pipewire").unwrap(), MicBackend::Pipewire);
        assert_eq!(MicBackend::parse("Pulse").unwrap(), MicBackend::Pulse);
        // Upstream's macOS backends keep working, with a message on stderr.
        assert_eq!(MicBackend::parse("sck").unwrap(), MicBackend::Pipewire);
        assert_eq!(
            MicBackend::parse("avcapture").unwrap(),
            MicBackend::Pipewire
        );
        assert!(MicBackend::parse("jack").is_err());
    }

    #[test]
    fn pidfile_lives_in_the_runtime_dir() {
        let p = pidfile_path();
        assert!(p.ends_with("rec-rs.pid"), "{}", p.display());
        let dir = p.parent().unwrap();
        assert!(dir.is_dir() || dir.starts_with("/tmp"), "{}", dir.display());
        // The fallback directory is private to the user.
        if dir.to_string_lossy().contains("rec-rs-") {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    /// Without a replay buffer there is nothing to save, and the wording is
    /// upstream's ("the replay buffer is off"), not one of our own.
    #[test]
    fn saving_a_replay_without_a_buffer_says_so() {
        let dir = std::env::temp_dir();
        let e = save_replay_request(None, &dir).unwrap_err();
        assert_eq!(format!("{e:#}"), "the replay buffer is off");
        // A live encode whose buffer is off says the same thing.
        let live = live::Live::new();
        let e = save_replay_request(Some(&live), &dir).unwrap_err();
        assert_eq!(format!("{e:#}"), "the replay buffer is off");
    }

    #[test]
    fn save_replay_signal_is_a_real_time_signal() {
        let sig = save_replay_signal();
        assert!(sig >= libc::SIGRTMIN());
        assert!(sig <= libc::SIGRTMAX());
        assert_ne!(sig, libc::SIGUSR1);
        assert_ne!(sig, libc::SIGUSR2);
    }

    #[test]
    fn no_recorder_means_a_clear_error() {
        // No pidfile in a fresh runtime dir: the commands must say so.
        let err = signal_running(libc::SIGUSR1).unwrap_err().to_string();
        assert!(err.contains("no recording in progress"), "{err}");
    }

    /// G19: upstream's control-error text, word for word, and without the
    /// pidfile path it does not print (`reference/rec-rs/src/record.rs:643-660`).
    #[test]
    fn control_errors_match_upstream() {
        let dir = std::env::temp_dir().join(format!("rec-ctl-text-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = std::env::var_os("XDG_RUNTIME_DIR");
        // SAFETY: this test is about the pidfile path and runs in the test
        // binary; no other test reads the recorder's pidfile at the same time
        // for its result (the others only check the fallback's mode).
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &dir) };
        // No pidfile at all.
        assert_eq!(
            signal_running(libc::SIGUSR1).unwrap_err().to_string(),
            "no recording in progress"
        );
        assert_eq!(
            stop_running().unwrap_err().to_string(),
            "no recording in progress"
        );
        // A pidfile whose process is gone (a stale pid from another namespace
        // looks the same to us).
        std::fs::write(pidfile_path(), "999999\n/tmp/x.mov\n").unwrap();
        assert_eq!(
            signal_running(libc::SIGUSR1).unwrap_err().to_string(),
            "no recording in progress (pid 999999 not running)"
        );
        assert_eq!(
            stop_running().unwrap_err().to_string(),
            "no recording in progress (stale pidfile removed)"
        );
        match old {
            Some(v) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", v) },
            None => unsafe { std::env::remove_var("XDG_RUNTIME_DIR") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
