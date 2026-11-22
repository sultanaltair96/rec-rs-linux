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
const CAMERA_SETTLE_SECS: f64 = 2.0;
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

/// A running capture session.
pub struct Session {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    pacer: Option<std::thread::JoinHandle<()>>,
    video: Option<Box<dyn VideoSource>>,
    camera: Option<Box<dyn VideoSource>>,
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

impl Session {
    /// Start capturing and writing. `o.write` false = preview-only.
    pub fn start(o: &Options) -> Result<Session> {
        let (w, h) = o.size;
        let spec = VideoSpec {
            width: w,
            height: h,
            fps: o.fps,
            codec: o.codec,
            bitrate: o.bitrate,
        };
        let path = o.write.then_some(o.output.as_path());
        let mut writer = Writer::new(
            path,
            &spec,
            o.system_audio,
            o.mic.is_some(),
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
            let _ = l.begin_session(o.system_audio, o.mic.is_some());
            writer.set_live(l.clone());
        }
        // Phase 1C's compositor draws the scenes; until it lands the writer
        // falls back to the CPU path (and says what it cannot draw).
        if o.webcam.is_some() {
            match gpu::Renderer::new(w, h, o.pip.clone()) {
                Ok(r) => writer.set_renderer(Some(r)),
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
            video: None,
            camera: None,
            camera_desc: String::new(),
            audio: vec![],
            output: o.write.then(|| o.output.clone()),
            started: Instant::now(),
            description: String::new(),
            encoder,
        };
        match sess.start_sources(o) {
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
        sess.pacer = Some(std::thread::spawn(move || {
            let period = Duration::from_secs_f64(1.0 / fps as f64 / 2.0);
            while !pacer_stop.load(Ordering::SeqCst) {
                let now = media_types::now();
                let mut failure = None;
                pacer_shared.with(|w| {
                    w.emit_until(now - w.lag());
                    failure = w.failure().map(str::to_string);
                });
                if let Some(f) = failure {
                    pacer_shared.fail(format!("writer failed: {f}"));
                }
                std::thread::sleep(period);
            }
        }));
        sess.started = Instant::now();
        Ok(sess)
    }

    fn start_sources(&mut self, o: &Options) -> Result<()> {
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
            let started = std::time::Instant::now();
            let req = CameraRequest {
                device: sel.clone(),
            };
            let mut cam = match testsrc::camera_from_env() {
                Some(c) => c,
                None => camera_capture::open(&req)?,
            };
            self.camera_desc = cam.describe();
            cam.start(camsink)?;
            // A preview just shows the dark frames; only a file waits.
            if o.write && o.scene.uses_cam() {
                let settle = (CAMERA_SETTLE_SECS - started.elapsed().as_secs_f64()).max(0.0);
                self.shared
                    .with(|w| w.wait_for_camera(settle));
            }
            self.camera = Some(cam);
        }
        match o.test_pattern {
            Some(noise) => {
                // REC_SYNTH_NATIVE=WxH makes the pattern larger than the
                // output, to exercise the scaler (a native-size display into a
                // smaller recording).
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
                self.description = synth.describe();
                // Both halves of the pattern share one origin, so flash and
                // beep stay aligned.
                if o.system_audio {
                    self.audio.push(Box::new(synth.audio(true)));
                }
                self.video = Some(Box::new(synth));
            }
            None => {
                let req = o.screen.clone().unwrap_or_default();
                let src = screen::open(&req)?;
                self.description = src.describe();
                self.video = Some(src);
                if o.system_audio {
                    self.audio.push(audio_capture::open_system()?);
                }
            }
        }
        if let Some(sel) = &o.mic {
            let req = AudioRequest {
                device: sel.clone(),
                name: o.mic_name.clone(),
            };
            match testsrc::mic_from_env() {
                // A synthetic microphone stands in for the device in tests.
                Some(src) => self.audio.push(src),
                None => match audio_capture::open_mic(&req) {
                    Ok(src) => self.audio.push(src),
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
        if let Some(mut c) = self.camera.take()
            && let Err(e) = c.stop()
        {
            eprintln!("rec: warning: stopping the camera: {e}");
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
    if o.live.is_some() {
        bail!(
            "the replay buffer and streaming are not implemented yet (phase {})",
            live::PHASE
        );
    }
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
    let sess = match Session::start(&o) {
        Ok(s) => s,
        Err(e) => {
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
            sess.camera_name().unwrap_or_else(|| "off".into()),
        );
        if o.write && !o.quiet {
            eprintln!("rec: encoder {}", sess.encoder);
        }
        eprintln!("rec: press Ctrl-C to stop");
    }

    let mut last_status = Instant::now();
    let mut warned_no_frames = false;
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
            eprint!(
                "\r\x1b[2Krec: save-replay: the replay buffer is not implemented yet (phase {})\n",
                live::PHASE
            );
        }
        let elapsed = sess.started.elapsed().as_secs_f64();
        if o.duration.is_some() && (sess.reached_limit() || (!sess.has_frames() && elapsed > 10.0))
        {
            break;
        }
        if !warned_no_frames && elapsed > 3.0 && !sess.has_frames() {
            warned_no_frames = true;
            eprintln!("rec: warning: no screen frames after 3s");
        }
        if !o.quiet && last_status.elapsed() >= Duration::from_secs(1) {
            last_status = Instant::now();
            let (st, lv) = sess.poll();
            eprint!(
                "\r\x1b[2K  {}{}  video {} fr ({} dup, {} drop)  sys {} dB  mic {} dB  ",
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
            );
        }
    }
    if !o.quiet {
        eprintln!("\nrec: stopping...");
    }
    let result = sess.finish();
    let _ = std::fs::remove_file(&pidfile);
    result
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
    let text = std::fs::read_to_string(pidfile_path()).map_err(|_| {
        anyhow::anyhow!(
            "no recording in progress ({} not found)",
            pidfile_path().display()
        )
    })?;
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
        bail!("no recording in progress (pid {pid} is not running)");
    }
    Ok(())
}

/// Ask a running `rec-rs record` to stop (like Ctrl-C) and wait for it to
/// finish, then print the file.
pub fn stop_running() -> Result<()> {
    let path = pidfile_path();
    let (pid, file) = running().map_err(|_| anyhow::anyhow!("no recording in progress"))?;
    if unsafe { libc::kill(pid, 0) } != 0 {
        let _ = std::fs::remove_file(&path);
        bail!("no recording in progress (stale pidfile removed)");
    }
    unsafe { libc::kill(pid, libc::SIGINT) };
    let deadline = Instant::now() + Duration::from_secs(90);
    // Done when the recorder removes its pidfile (file finalized) or exits.
    while unsafe { libc::kill(pid, 0) } == 0 && path.exists() {
        if Instant::now() > deadline {
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
}
