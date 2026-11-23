//! The app: a main window (live preview, scene and sources, record/pause, the
//! timer, level meters, replay and stream cards), a settings window, a tray
//! item and global hotkeys, in egui on wgpu with Inter (the paper-rs look).
//!
//! `rec-rs` with no subcommand starts it; `rec-rs gui` is the same command.
//! Ctrl+Shift+R toggles recording from anywhere, Ctrl+Shift+S saves the replay
//! buffer while it is on, Ctrl+, opens settings, Ctrl+W hides the window to the
//! tray and Ctrl+Q quits (finalizing a running recording first).
//!
//! One capture session exists at a time: a preview session while the window is
//! visible, replaced by the recording when one starts, and kept alive while the
//! replay buffer or a stream needs it even with the window closed. The live
//! encode (`crate::live`) is attached to every session, so replay and streams
//! survive starting and stopping a recording.
//!
//! Adapted from rec-rs src/app/mod.rs (MIT OR Apache-2.0): the windowing,
//! state and lifecycle are ported; the macOS specifics (borderless window,
//! traffic lights, NSMenuItem, Carbon, Sparkle) are replaced by native window
//! decorations, a StatusNotifierItem tray, the GlobalShortcuts portal/X11
//! grabs and the GitHub Releases check.

mod control;
mod install;
mod linux_desktop;
mod lock;
mod preview;
mod session;
mod theme;
mod tray;
mod ui;
mod updater;
mod widgets;

#[cfg(test)]
mod ui_tests;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use crate::compose::{PipSpec, Scene};
use crate::live::Live;
use crate::media::Stats;
use crate::media_types::FrameSink;
use crate::mixer::Volume;
use crate::record::{self, Session};
use crate::settings::Settings;

use self::control::{Claim, Control};
use self::session::StreamStatus;

/// Things that happen to the app from menus, the tray, hotkeys, signals and
/// buttons; all handled on the main thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmd {
    ToggleRecording,
    TogglePause,
    /// Start/pause/resume from the control socket (`rec-rs gui`'s second
    /// launch and scripts), which mean exactly one of the three.
    Pause,
    Resume,
    ShowWindow,
    /// Close the main window to the tray (Ctrl+W).
    HideWindow,
    ShowSettings,
    CloseSettings,
    RevealLast,
    OpenFolder,
    SaveReplay,
    ToggleStream,
    CheckForUpdates,
    /// Add this AppImage to the desktop's menu (self-integration).
    Integrate,
    Quit,
}

static QUEUE: Mutex<Vec<Cmd>> = Mutex::new(Vec::new());
static PROXY: OnceLock<Mutex<EventLoopProxy<()>>> = OnceLock::new();

/// Wake the event loop (from any thread).
fn wake() {
    if let Some(p) = PROXY.get() {
        let _ = p.lock().unwrap().send_event(());
    }
}

/// Queue a command from anywhere (tray, hotkeys, the control socket).
pub fn push(c: Cmd) {
    QUEUE.lock().unwrap().push(c);
    wake();
}

/// `REC_GUI_HIDDEN`: start without opening the window (a login item, and the
/// headless self-test).
fn background() -> bool {
    std::env::var_os("REC_GUI_BACKGROUND").is_some()
}

/// SIGINT (`rec-rs stop`) stops the recording; SIGTERM/SIGHUP (logout, Quit
/// from the tray) quit after saving it. SIGUSR1/2 (`pause`/`resume`) and
/// SIGRTMIN+1 (`save-replay`) are `record.rs`'s flags, installed below.
static SIG_STOP: AtomicBool = AtomicBool::new(false);
static SIG_QUIT: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_: libc::c_int) {
    SIG_STOP.store(true, Ordering::SeqCst);
}

extern "C" fn on_sigterm(_: libc::c_int) {
    SIG_QUIT.store(true, Ordering::SeqCst);
}

/// One OS window rendered with egui.
struct Win {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    egui: egui_winit::State,
    ctx: egui::Context,
    renderer: egui_wgpu::Renderer,
    repaint_at: Option<Instant>,
    /// Fully covered or minimized: nothing would be presented, so don't draw.
    occluded: bool,
}

struct Gpu {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

/// Level meter state for one source.
#[derive(Default, Clone, Copy)]
pub struct Meter {
    /// Displayed level in dBFS (decays smoothly).
    pub db: f32,
    pub peak_db: f32,
    pub peak_at: Option<Instant>,
}

impl Meter {
    fn feed(&mut self, linear: f32, dt: f32) {
        let db = if linear <= 1e-5 {
            -90.0
        } else {
            20.0 * linear.log10()
        };
        self.db = db.max(self.db - 24.0 * dt).max(-90.0);
        if db >= self.peak_db
            || self
                .peak_at
                .is_none_or(|t| t.elapsed() > Duration::from_millis(1500))
        {
            self.peak_db = db;
            self.peak_at = Some(Instant::now());
        }
    }
}

#[derive(Clone)]
pub struct Device {
    pub id: String,
    pub name: String,
}

/// A running stream to one destination.
pub struct Stream {
    /// Output ID in the live encode.
    pub id: u64,
    pub url: String,
    pub status: Option<StreamStatus>,
}

/// Everything the UI shows and edits.
pub struct State {
    pub settings: Settings,
    pub recording: Option<Session>,
    pub preview: Option<Session>,
    /// A running camera between sessions (preview -> recording -> preview),
    /// handed over so the device never restarts dark (phase 1E, upstream's
    /// `spare_cam`): its exposure stays settled and the LED stays on.
    spare_cam: Option<crate::record::Camera>,
    /// Settings the running preview session was started with.
    preview_settings: Option<Settings>,
    pub finishing: bool,
    pub last_file: Option<PathBuf>,
    pub error: Option<String>,
    pub meters: [Meter; 2],
    pub displays: Vec<String>,
    pub windows: Vec<(u32, String)>,
    pub cameras: Vec<Device>,
    pub mics: Vec<Device>,
    pub hotkey_ok: bool,
    /// The preview frame as an egui texture.
    pub preview_tex: Option<egui::TextureId>,
    /// Its size in pixels.
    pub preview_px: [u32; 2],
    pub preview_aspect: f32,
    /// The preview area in pixels, as the UI laid it out last frame.
    pub preview_target: (u32, u32),
    /// Preview frames shown (for the self-test).
    pub frames_shown: u64,
    /// UI requests handled after the frame.
    pub cmds: Vec<Cmd>,
    pub refresh_devices: bool,
    /// Overlay last sent to the running session.
    pip_applied: Option<PipSpec>,
    /// Audio levels last sent to the running session.
    volumes_applied: Option<[Volume; 2]>,
    /// Camera aspect ratio from the running session (for drag hit-testing).
    pub cam_aspect: f64,
    /// The shared live encode (replay buffer, streams).
    pub live: Arc<Live>,
    /// Replay buffer (seconds held, bytes) when on.
    pub replay_stats: Option<(f64, usize)>,
    /// Ctrl+Shift+S is registered (only held while the replay buffer is on).
    pub replay_hotkey: Option<bool>,
    /// One per destination enabled at Go Live; empty when not streaming.
    pub streams: Vec<Stream>,
    /// Stream keys being typed, by destination URL.
    pub key_inputs: HashMap<String, String>,
    /// Whether a key is stored, by destination URL.
    key_saved: HashMap<String, bool>,
    /// A short confirmation (replay saved) and when it was shown.
    pub notice: Option<(String, Instant)>,
    /// Flatpak updates the app itself, so there is no "Check for Updates…".
    pub updates: bool,
    /// The mic the running session opened (None = system default) and the
    /// device fallback warning last shown, so it isn't repeated.
    mic_in_use: Option<Option<String>>,
    mic_warned: Option<String>,
}

impl State {
    /// A UI state with nothing running (the UI tests build one directly).
    pub fn new(settings: Settings) -> State {
        State {
            settings,
            recording: None,
            preview: None,
            spare_cam: None,
            preview_settings: None,
            finishing: false,
            last_file: None,
            error: None,
            meters: Default::default(),
            displays: vec![],
            windows: vec![],
            cameras: vec![],
            mics: vec![],
            hotkey_ok: true,
            preview_tex: None,
            preview_px: [0, 0],
            preview_aspect: 16.0 / 9.0,
            preview_target: (0, 0),
            frames_shown: 0,
            cmds: vec![],
            refresh_devices: false,
            pip_applied: None,
            volumes_applied: None,
            cam_aspect: 16.0 / 9.0,
            live: Arc::new(Live::new()),
            replay_stats: None,
            replay_hotkey: None,
            streams: vec![],
            key_inputs: Default::default(),
            key_saved: Default::default(),
            notice: None,
            updates: install::updates_self(),
            mic_in_use: None,
            mic_warned: None,
        }
    }

    /// Whether a key is stored for this destination (looked up once per URL).
    pub fn has_stream_key(&mut self, url: &str) -> bool {
        if url.is_empty() {
            return false;
        }
        if let Some(v) = self.key_saved.get(url) {
            return *v;
        }
        let v = session::has_stream_key(url);
        self.key_saved.insert(url.to_string(), v);
        v
    }

    pub fn save_stream_key(&mut self, url: &str) {
        let key = self.key_inputs.remove(url).unwrap_or_default();
        let key = key.trim();
        if url.is_empty() || key.is_empty() {
            return;
        }
        match session::save_stream_key(url, key) {
            Ok(()) => {
                self.key_saved.insert(url.to_string(), true);
            }
            Err(e) => self.error = Some(format!("Could not save the stream key: {e}")),
        }
    }

    /// Forget the cached key lookups (the UI tests change the environment).
    #[cfg(test)]
    pub fn key_saved_clear_for_tests(&mut self) {
        self.key_saved.clear();
    }

    pub fn delete_stream_key(&mut self, url: &str) {
        self.key_saved.insert(url.to_string(), false);
    }

    pub fn streaming(&self) -> bool {
        !self.streams.is_empty()
    }

    pub fn is_paused(&self) -> bool {
        self.recording.as_ref().is_some_and(|s| s.is_paused())
    }

    /// Recorded time (excluding pauses) of the running recording.
    pub fn clock(&self) -> String {
        self.recording
            .as_ref()
            .map(|s| crate::util::clock(s.recorded_secs()))
            .unwrap_or_default()
    }

    /// Send the overlay settings to the running session if they changed:
    /// moving or restyling the camera is live in the preview and the file.
    pub fn apply_pip(&mut self) {
        let Some(sess) = self.recording.as_ref().or(self.preview.as_ref()) else {
            self.pip_applied = None;
            return;
        };
        if let Some(a) = sess.cam_aspect() {
            self.cam_aspect = a;
        }
        let spec = self.settings.pip_spec();
        if self.pip_applied.as_ref() == Some(&spec) {
            return;
        }
        self.pip_applied = Some(spec.clone());
        // Baked into the next emitted frame, which is also the next preview
        // frame, so a drag moves the overlay while the button is down. Sent
        // even in the screen-only scene: the writer keeps it for the moment the
        // camera scene is switched on.
        sess.set_pip(spec);
    }

    /// Send the fader levels to the running session if they changed: the
    /// preview's meters, the file and the live encode follow at once.
    pub fn apply_volumes(&mut self) {
        let Some(sess) = self.recording.as_ref().or(self.preview.as_ref()) else {
            self.volumes_applied = None;
            return;
        };
        let v = self.settings.volumes();
        if self.volumes_applied != Some(v) {
            sess.set_volumes(v);
            self.volumes_applied = Some(v);
        }
    }

    /// Whether the running session can switch to `scene` live.
    pub fn can_switch_live(&self, scene: Scene) -> bool {
        let cam_running = self
            .recording
            .as_ref()
            .is_some_and(|s| s.camera_name().is_some());
        !scene.uses_cam() || cam_running
    }
}

/// A finalized recording: (file, outcome).
type Finished = (PathBuf, std::result::Result<Stats, String>);

/// What the hotplug poll watches: the microphone and camera lists (ids and
/// names). Display and window lists talk to the portal and are refreshed by
/// the ↻ button instead (G9).
pub(crate) fn device_fingerprint(
    cameras: &[crate::media_types::CameraInfo],
    mics: &[crate::media_types::MicInfo],
) -> Vec<String> {
    let mut v: Vec<String> = Vec::with_capacity(cameras.len() + mics.len());
    v.extend(cameras.iter().map(|c| format!("cam:{}:{}", c.id, c.name)));
    v.extend(mics.iter().map(|m| format!("mic:{}:{}", m.id, m.name)));
    v
}

/// Whether the main window must be hidden for a recording (G3): a display or
/// region capture cannot exclude Rec's own window, so it would film the
/// preview; a single-window capture cannot contain it, and the user can turn
/// the behaviour off.
pub(crate) fn hides_window_for_recording(settings: &Settings, main_visible: bool) -> bool {
    settings.hide_window_when_recording && settings.window.is_none() && main_visible
}

/// Whether the mic a running preview opened is no longer the one the saved
/// selection resolves to (G9; upstream's `devices_changed` restarts the preview
/// for this). `in_use` is the running session's selection: `Some(None)` = the
/// default mic, `None` = no preview/mic was running, which also means a
/// restart when the settings now want one.
pub(crate) fn mic_needs_restart(in_use: Option<&Option<String>>, want: Option<&str>) -> bool {
    let current = in_use.and_then(|i| i.as_deref()).map(str::to_string);
    let want = want.map(str::to_string);
    current != want
}

fn load_devices(st: &mut State) {
    st.displays = crate::capture::screen::displays()
        .map(|ds| {
            ds.iter()
                .map(|d| format!("Display {} — {}×{}", d.index, d.pixels.0, d.pixels.1))
                .collect()
        })
        .unwrap_or_default();
    st.windows = crate::capture::screen::windows()
        .map(|ws| {
            ws.into_iter()
                .filter(|w| !w.title.is_empty())
                .map(|w| (w.id as u32, format!("{} — {}", w.app, w.title)))
                .collect()
        })
        .unwrap_or_default();
    st.cameras = crate::capture::camera::cameras()
        .unwrap_or_default()
        .into_iter()
        .map(|d| Device {
            id: d.id,
            name: d.name,
        })
        .collect();
    st.mics = crate::capture::audio::microphones()
        .unwrap_or_default()
        .into_iter()
        .map(|d| Device {
            id: d.id,
            name: d.name,
        })
        .collect();
}

struct App {
    gpu: Option<Gpu>,
    main: Option<Win>,
    settings_win: Option<Win>,
    main_visible: bool,
    /// Quit is in progress: the windows are gone and the loop is ending, so no
    /// event may touch them.
    quitting: bool,
    /// The main window was hidden for the running recording (G3): a capture
    /// that cannot exclude Rec's window would film it. Restored on stop.
    hidden_for_recording: bool,
    /// The screen is locked (G8): nothing is drawn and no preview frames are
    /// converted until it is unlocked.
    screen_locked: bool,
    /// The main window is covered or minimized (winit's occlusion event, which
    /// Wayland often does not send).
    main_occluded: bool,
    /// The lock watcher (G8), if a bus answered.
    lock: Option<lock::LockWatcher>,
    /// When the device lists were last refreshed by the hotplug poll (G9).
    last_device_poll: Instant,
    /// The device lists as last seen, to spot a plug/unplug (G9).
    device_fingerprint: Vec<String>,
    st: State,
    tray: Option<ksni::blocking::Handle<tray::Tray>>,
    /// The tray starts on a thread: registering the status item talks to D-Bus,
    /// and a slow (or missing) watcher must not hold up the first frame.
    tray_rx: Option<Receiver<Option<ksni::blocking::Handle<tray::Tray>>>>,
    tap: preview::Tap,
    tap_sink: FrameSink,
    preview_texture: Option<egui::TextureHandle>,
    last_tick: Instant,
    last_meter: Instant,
    control_rx: Receiver<Control>,
    _listener: Option<control::Listener>,
    start_recording: bool,
    test_pattern: bool,
    finished: Arc<Mutex<Option<Finished>>>,
    update_tx: Sender<std::result::Result<updater::Found, String>>,
    update_rx: Receiver<std::result::Result<updater::Found, String>>,
    accent_rx: Receiver<Option<egui::Color32>>,
    accent_done: bool,
}

impl App {
    fn new(
        start_recording: bool,
        settings: bool,
        window: Option<u32>,
        test_pattern: bool,
        control_rx: Receiver<Control>,
        listener: Option<control::Listener>,
    ) -> App {
        let mut st = State::new(Settings::load());
        st.settings.window = window;
        theme::set_accent(theme::accent_from_env());
        load_devices(&mut st);
        let device_fingerprint = device_fingerprint(
            &crate::capture::camera::cameras().unwrap_or_default(),
            &crate::capture::audio::microphones().unwrap_or_default(),
        );
        st.hotkey_ok = crate::hotkey::backend() != crate::hotkey::Backend::None;
        let (update_tx, update_rx) = std::sync::mpsc::channel();
        let (accent_tx, accent_rx) = std::sync::mpsc::channel();
        spawn_accent_fetch(accent_tx);
        let (tap, tap_sink) = preview::Tap::new(Arc::new(wake));
        let lock = lock::LockWatcher::start(Arc::new(wake));
        if settings {
            push(Cmd::ShowSettings);
        }
        App {
            gpu: None,
            main: None,
            settings_win: None,
            main_visible: true,
            quitting: false,
            hidden_for_recording: false,
            screen_locked: false,
            main_occluded: false,
            last_device_poll: Instant::now(),
            device_fingerprint,
            st,
            tray: None,
            tray_rx: None,
            lock,
            tap,
            tap_sink,
            preview_texture: None,
            last_tick: Instant::now(),
            last_meter: Instant::now(),
            control_rx,
            _listener: listener,
            start_recording,
            test_pattern,
            finished: Arc::new(Mutex::new(None)),
            update_tx,
            update_rx,
            accent_rx,
            accent_done: false,
        }
    }

    fn gpu(&mut self, surface: &wgpu::Surface<'static>) -> &Gpu {
        if self.gpu.is_none() {
            let instance = wgpu::Instance::new(
                wgpu::InstanceDescriptor::new_without_display_handle_from_env(),
            );
            let adapter = pollster::block_on(instance.request_adapter(
                &wgpu::RequestAdapterOptions {
                    compatible_surface: Some(surface),
                    ..Default::default()
                },
            ))
            .expect(
                "no compatible GPU adapter (install Mesa's Vulkan driver or run with a display)",
            );
            let (device, queue) =
                pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                    label: Some("rec-rs"),
                    required_limits:
                        wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
                    ..Default::default()
                }))
                .expect("create the wgpu device");
            self.gpu = Some(Gpu {
                instance,
                adapter,
                device,
                queue,
            });
        }
        self.gpu.as_ref().unwrap()
    }

    fn open_window(
        &mut self,
        el: &ActiveEventLoop,
        title: &str,
        size: (f64, f64),
        min: (f64, f64),
    ) -> Win {
        let attrs = Window::default_attributes()
            .with_title(title)
            .with_inner_size(winit::dpi::LogicalSize::new(size.0, size.1))
            .with_min_inner_size(winit::dpi::LogicalSize::new(min.0, min.1));
        let window = Arc::new(el.create_window(attrs).expect("create window"));
        // A throwaway instance creates the first surface; the shared one after
        // that (surfaces must come from the adapter's instance).
        let surface = match &self.gpu {
            Some(g) => g.instance.create_surface(window.clone()).expect("surface"),
            None => {
                let instance = wgpu::Instance::new(
                    wgpu::InstanceDescriptor::new_without_display_handle_from_env(),
                );
                let s = instance.create_surface(window.clone()).expect("surface");
                let adapter =
                    pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                        compatible_surface: Some(&s),
                        ..Default::default()
                    }))
                    .expect("no compatible GPU adapter");
                let (device, queue) =
                    pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                        label: Some("rec-rs"),
                        required_limits:
                            wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
                        ..Default::default()
                    }))
                    .expect("create the wgpu device");
                self.gpu = Some(Gpu {
                    instance,
                    adapter,
                    device,
                    queue,
                });
                s
            }
        };
        let g = self.gpu(&surface);
        let caps = surface.get_capabilities(&g.adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let px = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: px.width.max(1),
            height: px.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            color_space: Default::default(),
        };
        surface.configure(&g.device, &config);
        let renderer =
            egui_wgpu::Renderer::new(&g.device, format, egui_wgpu::RendererOptions::default());
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let egui = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            window.theme(),
            Some(g.device.limits().max_texture_dimension_2d as usize),
        );
        window.request_redraw();
        Win {
            window,
            surface,
            config,
            egui,
            ctx,
            renderer,
            repaint_at: None,
            occluded: false,
        }
    }

    fn show_main(&mut self, el: &ActiveEventLoop) {
        if self.main.is_none() {
            // REC_MAIN_SIZE=WxH or full: open at this size or fullscreen
            // (layout checks and screenshots).
            let size = std::env::var("REC_MAIN_SIZE")
                .ok()
                .and_then(|v| {
                    v.split_once('x')
                        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                })
                .unwrap_or((1040.0, 660.0));
            let w = self.open_window(el, "Rec", size, (860.0, 560.0));
            if std::env::var("REC_MAIN_SIZE").as_deref() == Ok("full") {
                w.window
                    .set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
            }
            self.main = Some(w);
        }
        if let Some(w) = &self.main {
            w.window.set_visible(true);
            if !background() {
                w.window.focus_window();
            }
            w.window.request_redraw();
        }
        self.main_visible = true;
        self.update_tap_wanted();
        self.sync_preview();
    }

    fn hide_main(&mut self) {
        if let Some(w) = &self.main {
            w.window.set_visible(false);
        }
        self.main_visible = false;
        // Nothing is drawn while the window is hidden: the tap drops frames.
        self.update_tap_wanted();
        self.st.preview_tex = None;
        self.preview_texture = None;
        self.sync_preview();
    }

    fn show_settings(&mut self, el: &ActiveEventLoop) {
        if self.settings_win.is_none() {
            let w = self.open_window(el, "Rec Settings", (520.0, 820.0), (460.0, 480.0));
            self.settings_win = Some(w);
        }
        if let Some(w) = &self.settings_win {
            if !background() {
                w.window.focus_window();
            }
            w.window.request_redraw();
        }
    }

    /// The capture session's options, from the settings (through the CLI, so
    /// the app and `rec-rs record` cannot drift apart).
    fn options(&mut self, output: Option<&std::path::Path>) -> Result<record::Options> {
        let mut settings = self.st.settings.clone();
        // A display that was unplugged falls back to the main one.
        let count = crate::capture::screen::displays()
            .map(|d| d.len())
            .unwrap_or(0);
        if count > 0 && settings.window.is_none() && settings.display > count {
            let warning = format!(
                "Display {} not connected; using display 1",
                settings.display
            );
            settings.display = 1;
            eprintln!("rec: warning: {warning}");
            self.st.notice = Some((warning, Instant::now()));
        }
        // REC_TEST_PATTERN: the CLI's synthetic source, for self-tests that
        // need no capture permission.
        let pattern = self.test_pattern || std::env::var_os("REC_TEST_PATTERN").is_some();
        let o = session::options(
            &settings,
            output,
            Some(self.st.live.clone()),
            Some(self.tap_sink.clone()),
            pattern,
        )?;
        // Fallbacks never block: say so once.
        let warning = o.mic_warning.clone();
        if warning != self.st.mic_warned {
            if let Some(w) = &warning {
                self.st.notice = Some((w.clone(), Instant::now()));
            }
            self.st.mic_warned = warning;
        }
        self.st.mic_in_use = o.mic.clone();
        Ok(o)
    }

    /// Whether the live encode needs a capture session (replay or stream).
    fn live_wanted(&self) -> bool {
        self.st.settings.replay_enabled || self.st.streaming()
    }

    /// Apply the replay and live-encode settings, and hold Ctrl+Shift+S while
    /// the replay buffer is on (it is "Save As" in most apps otherwise).
    fn sync_live(&mut self) {
        let cfg = self.st.settings.live_config();
        if self.st.live.config() != Some(cfg.clone()) {
            self.st.live.set_config(cfg);
        }
        let want = self
            .st
            .settings
            .replay_enabled
            .then(|| self.st.settings.replay_secs.clamp(15, 300) as f64);
        let have = session::replay_stats(&self.st.live).is_some();
        if want.is_some() || have {
            self.st.live.set_replay(want);
        }
        self.st.replay_stats = session::replay_stats(&self.st.live);
        match (want.is_some(), self.st.replay_hotkey.is_some()) {
            (true, false) => {
                let ok = crate::hotkey::register(crate::hotkey::Shortcut::Replay, || {
                    push(Cmd::SaveReplay)
                });
                if let Err(ref e) = ok {
                    eprintln!(
                        "rec: could not register {} ({e}); use the Save Replay button",
                        crate::hotkey::Shortcut::Replay.label()
                    );
                }
                self.st.replay_hotkey = Some(ok.is_ok());
            }
            (false, true) => {
                crate::hotkey::unregister(crate::hotkey::Shortcut::Replay);
                self.st.replay_hotkey = None;
            }
            _ => {}
        }
        if self.st.streaming() {
            let statuses = session::statuses(&self.st.live);
            for s in &mut self.st.streams {
                s.status = statuses
                    .iter()
                    .find(|(i, _)| *i == s.id)
                    .map(|(_, st)| st.clone());
            }
        }
    }

    fn save_replay(&mut self) {
        if self.st.replay_stats.is_none() {
            return;
        }
        let path = self
            .st
            .settings
            .output_dir
            .join(crate::util::timestamped("replay", "mp4"));
        let r = std::fs::create_dir_all(&self.st.settings.output_dir)
            .map_err(anyhow::Error::from)
            .and_then(|_| session::save_replay(&self.st.live, path).map_err(anyhow::Error::msg));
        match r {
            Ok(()) => self.st.notice = Some(("Saving the replay…".into(), Instant::now())),
            Err(e) => self.st.error = Some(format!("Replay: {e:#}")),
        }
    }

    /// Go live on every enabled destination, or stop them all. Starting is all
    /// or nothing: one that can't start (no key, bad URL) stops the rest, so a
    /// stream never runs on only some of them by accident.
    fn toggle_stream(&mut self) {
        if self.st.streaming() {
            for s in std::mem::take(&mut self.st.streams) {
                let _ = session::remove_output(&self.st.live, s.id);
            }
            self.sync_preview();
            return;
        }
        let urls = self.st.settings.stream_urls();
        if urls.is_empty() {
            self.st.error = Some("Could not go live: turn on a destination in Stream first".into());
            return;
        }
        if let Err(e) = session::begin_live(
            &self.st.live,
            self.st.settings.system_audio,
            self.st.settings.mic_enabled,
        ) {
            self.st.error = Some(format!("Could not go live: {e}"));
            return;
        }
        let key = std::env::var("REC_STREAM_KEY").unwrap_or_default();
        let mut added = vec![];
        for url in &urls {
            match session::add_output(&self.st.live, url, &key) {
                Ok(id) => added.push(Stream {
                    id,
                    url: url.clone(),
                    status: None,
                }),
                Err(e) => {
                    for a in added {
                        let _ = session::remove_output(&self.st.live, a.id);
                    }
                    self.st.error = Some(e);
                    return;
                }
            }
        }
        self.st.streams = added;
        self.st.error = None;
        // The live encode needs the capture to keep running.
        self.sync_preview();
    }

    /// Run a preview session while the window is visible and nothing is
    /// recording; restart it when the settings change.
    fn sync_preview(&mut self) {
        // The replay buffer and streams keep a session running while the
        // window is closed.
        let want = (self.main_visible || self.live_wanted())
            && self.st.recording.is_none()
            && !self.st.finishing;
        // Overlay changes are applied live (`apply_pip`), not by restarting.
        let stale = self.st.preview_settings.as_ref().map(Settings::capture_key)
            != Some(self.st.settings.capture_key());
        if self.st.preview.is_some() && (!want || stale) {
            if let Some(mut s) = self.st.preview.take() {
                // Keep the camera for the next session (a recording, or the
                // restarted preview): re-opening it would restart its
                // exposure and its LED.
                if want {
                    self.st.spare_cam = s.take_camera();
                }
                let _ = s.finish();
            }
            self.st.preview_settings = None;
        }
        if want && self.st.preview.is_none() && self.st.error.is_none() {
            let cam = self.st.spare_cam.take();
            let r = self
                .options(None)
                .and_then(|o| Session::start_with_camera(&o, cam));
            match r {
                Ok(s) => {
                    self.st.pip_applied = Some(self.st.settings.pip_spec());
                    self.st.volumes_applied = Some(self.st.settings.volumes());
                    if let Some(a) = s.cam_aspect() {
                        self.st.cam_aspect = a;
                    }
                    self.st.preview = Some(s);
                }
                Err(e) => {
                    self.st.error = Some(session::preview_error(&e));
                }
            }
            self.st.preview_settings = Some(self.st.settings.clone());
        }
    }

    fn toggle_recording(&mut self) {
        if self.st.finishing {
            return;
        }
        if self.st.recording.is_some() {
            self.stop_recording();
        } else {
            self.start_recording();
        }
    }

    fn start_recording(&mut self) {
        // One capture session at a time: the recording feeds the preview and
        // takes over its camera, already exposed for the room.
        let mut cam = self.st.spare_cam.take();
        if let Some(mut s) = self.st.preview.take() {
            cam = cam.or_else(|| s.take_camera());
            let _ = s.finish();
        }
        self.st.preview_settings = None;
        self.st.error = None;
        let file = self.st.settings.new_output_file();
        let r = std::fs::create_dir_all(&self.st.settings.output_dir)
            .map_err(anyhow::Error::from)
            .and_then(|_| self.options(Some(&file)))
            .and_then(|o| Session::start_with_camera(&o, cam));
        match r {
            Ok(sess) => {
                let _ = std::fs::write(
                    record::pidfile_path(),
                    format!("{}\n{}\n", std::process::id(), file.display()),
                );
                self.st.pip_applied = Some(self.st.settings.pip_spec());
                self.st.volumes_applied = Some(self.st.settings.volumes());
                if let Some(a) = sess.cam_aspect() {
                    self.st.cam_aspect = a;
                }
                self.st.recording = Some(sess);
                record::PAUSE_REQ.store(false, Ordering::SeqCst);
                record::RESUME_REQ.store(false, Ordering::SeqCst);
                // G3: a display (or region) capture includes Rec's own window
                // unless it is hidden; a single-window capture cannot.
                if hides_window_for_recording(&self.st.settings, self.main_visible) {
                    self.hide_window_for_recording();
                }
            }
            Err(e) => {
                eprintln!("rec: could not start recording: {e:#}");
                self.st.error = Some(format!("Could not start recording: {e:#}"));
                self.restore_after_recording();
                self.sync_preview();
            }
        }
    }

    /// Hide the main window for the running recording, and say so (G3).
    fn hide_window_for_recording(&mut self) {
        self.hide_main();
        self.hidden_for_recording = true;
        let message = "Rec's window is hidden while recording, because a screen capture would \
                       film it. Open it from the tray.";
        self.st.notice = Some((message.into(), Instant::now()));
        // The window that would show the notice is the one being hidden.
        linux_desktop::notify("Rec", message);
        // The X11 backend can subtract our windows from its GetImage instead
        // (phase 1A adds `capture::screen::set_excluded_windows(Vec<u32>)` with
        // the ids from `window.raw_window_handle()`); until that lands, hiding
        // is the only exclusion on both backends.
    }

    /// Bring the window back after a recording that hid it (G3).
    fn restore_after_recording(&mut self) {
        if self.hidden_for_recording {
            self.hidden_for_recording = false;
            if let Some(w) = &self.main {
                w.window.set_visible(true);
            }
            self.main_visible = true;
            self.update_tap_wanted();
        }
    }

    fn stop_recording(&mut self) {
        let Some(mut sess) = self.st.recording.take() else {
            return;
        };
        // The camera goes back to the preview once the file is finished.
        self.st.spare_cam = sess.take_camera();
        let file = sess.output.clone().unwrap_or_default();
        self.st.finishing = true;
        let done = self.finished.clone();
        let tap = &mut self.tap;
        tap.set_wanted(self.main_visible);
        // Finalizing can take a moment; keep the UI responsive.
        std::thread::spawn(move || {
            let r = sess.finish().map_err(|e| format!("{e:#}"));
            *done.lock().unwrap() = Some((file, r));
            let _ = std::fs::remove_file(record::pidfile_path());
            wake();
        });
    }

    fn toggle_pause(&mut self) {
        if let Some(s) = &self.st.recording {
            if s.is_paused() {
                s.resume();
            } else {
                s.pause();
            }
        }
    }

    fn check_for_updates(&mut self) {
        if !self.st.updates {
            self.st.notice = Some((
                "Flatpak updates Rec; use your software centre".into(),
                Instant::now(),
            ));
            return;
        }
        self.st.notice = Some(("Checking for updates…".into(), Instant::now()));
        let tx = self.update_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(updater::check_now().map_err(|e| format!("{e:#}")));
        });
    }

    fn integrate_appimage(&mut self) {
        let Some(appimage) = install::appimage_path() else {
            self.st.error = Some("Add to Menu works when Rec runs as an AppImage".into());
            return;
        };
        let i = install::integration(&appimage, "Rec");
        let r = install::check_appimage(&appimage)
            .and_then(|()| install::remove_stale(&i, &appimage).map(|_| ()))
            .and_then(|()| install::integrate(&i));
        match r {
            Ok(()) => {
                self.st.notice = Some(("Rec is in your app menu".into(), Instant::now()));
            }
            Err(e) => self.st.error = Some(format!("Could not add Rec to the menu: {e:#}")),
        }
    }

    fn quit(&mut self, el: &ActiveEventLoop) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        let debug = std::env::var_os("REC_DEBUG").is_some();
        if debug {
            eprintln!("rec: debug: quit: finalizing");
        }
        if let Some(s) = self.st.recording.take() {
            let file = s.output.clone();
            let r = s.finish();
            let _ = std::fs::remove_file(record::pidfile_path());
            if let Some(f) = &file {
                println!("{}", f.display());
            }
            if let Err(e) = r {
                eprintln!("rec: could not finish the recording: {e:#}");
            }
        }
        // A recording that was already finalizing in the worker thread.
        while self.st.finishing && self.finished.lock().unwrap().is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
        if let Some(s) = self.st.preview.take() {
            let _ = s.finish();
        }
        session::stop_all(&self.st.live);
        self.tray = None;
        // Before the loop ends: see `close_windows`.
        self.close_windows();
        if debug {
            eprintln!("rec: debug: quit: exiting the event loop");
        }
        el.exit();
    }

    fn handle(&mut self, el: &ActiveEventLoop, c: Cmd) {
        match c {
            Cmd::ToggleRecording => self.toggle_recording(),
            Cmd::TogglePause => self.toggle_pause(),
            Cmd::Pause => {
                if let Some(s) = &self.st.recording {
                    s.pause();
                }
            }
            Cmd::Resume => {
                if let Some(s) = &self.st.recording {
                    s.resume();
                }
            }
            Cmd::ShowWindow => self.show_main(el),
            Cmd::HideWindow => self.hide_main(),
            Cmd::ShowSettings => self.show_settings(el),
            Cmd::CloseSettings => self.settings_win = None,
            Cmd::RevealLast => {
                if let Some(f) = &self.st.last_file {
                    linux_desktop::reveal(f);
                }
            }
            Cmd::OpenFolder => linux_desktop::open_folder(&self.st.settings.output_dir),
            Cmd::SaveReplay => self.save_replay(),
            Cmd::ToggleStream => self.toggle_stream(),
            Cmd::CheckForUpdates => self.check_for_updates(),
            Cmd::Integrate => self.integrate_appimage(),
            Cmd::Quit => self.quit(el),
        }
    }

    /// Periodic work: commands, signals, finished recordings, failures, tray,
    /// meters.
    fn tick(&mut self, el: &ActiveEventLoop) {
        if self.quitting {
            return;
        }
        if SIG_QUIT.swap(false, Ordering::SeqCst) {
            self.quit(el);
            return;
        }
        if SIG_STOP.swap(false, Ordering::SeqCst) && self.st.recording.is_some() {
            self.stop_recording();
        }
        if self.st.recording.as_ref().is_some_and(|s| s.has_frames()) {
            if record::PAUSE_REQ.swap(false, Ordering::SeqCst) {
                self.st.recording.as_ref().unwrap().pause();
            }
            if record::RESUME_REQ.swap(false, Ordering::SeqCst) {
                self.st.recording.as_ref().unwrap().resume();
            }
        }
        // `rec-rs save-replay` (SIGRTMIN+1).
        if record::REPLAY_REQ.swap(false, Ordering::SeqCst) {
            push(Cmd::SaveReplay);
        }
        self.sync_live();
        let cmds: Vec<Cmd> = std::mem::take(&mut *QUEUE.lock().unwrap());
        let ui_cmds = std::mem::take(&mut self.st.cmds);
        for c in cmds.into_iter().chain(ui_cmds) {
            self.handle(el, c);
        }
        let done = self.finished.lock().unwrap().take();
        if let Some((file, r)) = done {
            self.st.finishing = false;
            self.restore_after_recording();
            match r {
                Ok(_) => {
                    self.st.last_file = Some(file.clone());
                    self.update_tap_wanted();
                    self.st.notice = Some((
                        format!(
                            "Saved {}",
                            file.file_name().unwrap_or_default().to_string_lossy()
                        ),
                        Instant::now(),
                    ));
                }
                Err(e) => self.st.error = Some(format!("Recording failed: {e}")),
            }
        }
        let failed = self.st.recording.as_ref().is_some_and(|s| s.failed());
        if failed {
            let err = self
                .st
                .recording
                .as_ref()
                .and_then(|s| s.error())
                .unwrap_or_default();
            self.stop_recording();
            self.st.error = Some(format!("Recording stopped: {err}"));
        }
        if let Some(s) = self.st.preview.as_ref()
            && s.failed()
        {
            let err = s.error().unwrap_or_default();
            if let Some(s) = self.st.preview.take() {
                let _ = s.finish();
            }
            self.st.error = Some(format!("Preview stopped: {err}"));
        }
        if self.st.refresh_devices {
            self.st.refresh_devices = false;
            load_devices(&mut self.st);
        }
        // G9: device hotplug. 1B has no change notification yet, so poll the
        // two cheap lists (V4L2 nodes, PipeWire nodes) and only do the rest
        // when they actually changed.
        if self.last_device_poll.elapsed() >= Duration::from_secs(2) {
            self.last_device_poll = Instant::now();
            let cameras = crate::capture::camera::cameras().unwrap_or_default();
            let mics = crate::capture::audio::microphones().unwrap_or_default();
            let fingerprint = device_fingerprint(&cameras, &mics);
            if fingerprint != self.device_fingerprint {
                self.device_fingerprint = fingerprint;
                load_devices(&mut self.st);
                // A preview keeps the mic it opened: restart it when the saved
                // selection resolves to another device now (upstream's
                // `devices_changed`).
                if self.st.recording.is_none()
                    && self.st.preview.is_some()
                    && self.st.settings.mic_enabled
                    && let Some(sel) = self.st.settings.mic.as_deref()
                    && let Ok(pick) =
                        crate::capture::audio::pick_mic(sel, self.st.settings.mic_name.as_deref())
                    && mic_needs_restart(self.st.mic_in_use.as_ref(), pick.id.as_deref())
                {
                    self.st.preview_settings = None;
                }
            }
        }
        for e in crate::hotkey::poll() {
            match e {
                crate::hotkey::Event::Registered(_) => {
                    self.st.hotkey_ok = true;
                }
                crate::hotkey::Event::Unavailable(_, why) => {
                    eprintln!("rec: global hotkey unavailable: {why}");
                    self.st.hotkey_ok = false;
                }
            }
        }
        while let Ok(r) = self.update_rx.try_recv() {
            match r {
                Ok(updater::Found::Newer(rel)) => {
                    self.st.notice = Some((
                        updater::message(&updater::Found::Newer(rel.clone())),
                        Instant::now(),
                    ));
                    linux_desktop::open_url(&rel.url);
                }
                Ok(found) => self.st.notice = Some((updater::message(&found), Instant::now())),
                Err(e) => self.st.error = Some(format!("Could not check for updates: {e}")),
            }
        }
        if !self.accent_done
            && let Ok(c) = self.accent_rx.try_recv()
        {
            self.accent_done = true;
            if let Some(c) = c {
                theme::set_accent(c);
                for w in [self.main.as_ref(), self.settings_win.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    theme::apply(&w.ctx);
                }
            }
        }
        // G8: the screen lock stops the preview (the recording keeps going).
        let locked = self.lock.as_ref().is_some_and(|l| l.locked());
        if locked != self.screen_locked {
            self.screen_locked = locked;
            self.update_tap_wanted();
            if !locked {
                for w in [self.main.as_mut(), self.settings_win.as_mut()]
                    .into_iter()
                    .flatten()
                {
                    w.window.request_redraw();
                }
            }
        }
        // Meters.
        let dt = self.last_meter.elapsed().as_secs_f32();
        self.last_meter = Instant::now();
        let sess = self.st.recording.as_ref().or(self.st.preview.as_ref());
        let lv = sess.map(|s| s.poll().1).unwrap_or([0.0; 2]);
        for (m, l) in self.st.meters.iter_mut().zip(lv) {
            m.feed(l, dt);
        }
        self.sync_preview();
        self.st.apply_pip();
        self.st.apply_volumes();
        // Tray.
        let clock = self.st.clock();
        let view = tray::View {
            recording: self.st.recording.is_some(),
            paused: self.st.is_paused(),
            finishing: self.st.finishing,
            clock,
            has_last: self.st.last_file.is_some(),
            replay: self.st.replay_stats.is_some(),
            live: self.st.streaming(),
            updates: self.st.updates,
            hotkey_ok: self.st.hotkey_ok,
        };
        if let Some(rx) = &self.tray_rx
            && let Ok(handle) = rx.try_recv()
        {
            self.tray_rx = None;
            self.tray = handle;
        }
        if let Some(t) = &self.tray {
            tray::update(t, view);
        }
        while let Ok(c) = self.control_rx.try_recv() {
            let cmd = match c {
                Control::Show => Cmd::ShowWindow,
                Control::Settings => Cmd::ShowSettings,
                Control::StartRecording => {
                    if self.st.recording.is_none() {
                        Cmd::ToggleRecording
                    } else {
                        Cmd::ShowWindow
                    }
                }
                Control::ToggleRecording => Cmd::ToggleRecording,
                Control::Stop => {
                    if self.st.recording.is_some() {
                        Cmd::ToggleRecording
                    } else {
                        // Nothing to stop: leave the app as it is.
                        continue;
                    }
                }
                Control::Pause => Cmd::Pause,
                Control::Resume => Cmd::Resume,
                Control::SaveReplay => Cmd::SaveReplay,
                Control::Quit => Cmd::Quit,
            };
            self.handle(el, cmd);
        }
    }

    fn redraw(&mut self, which: WindowId) {
        if self.quitting || self.screen_locked {
            return;
        }
        let is_main = self.main.as_ref().is_some_and(|w| w.window.id() == which);
        if self.gpu.is_none() {
            return;
        }
        // The preview texture follows the settings' low-power mode.
        self.tap.set_low_power(self.st.settings.preview_low_power);
        if is_main && self.main_visible && self.tap.wanted() {
            self.tap.set_target(
                self.st.preview_target.0.max(2),
                self.st.preview_target.1.max(2),
            );
            if let Some(frame) = self.tap.take()
                && let Some((img, px)) = self.tap.image(&frame)
                && px[0] > 0
                && px[1] > 0
            {
                self.st.preview_aspect = px[0] as f32 / px[1] as f32;
                self.st.preview_px = px;
                let options = egui::TextureOptions::LINEAR;
                match &mut self.preview_texture {
                    Some(h) => h.set(img, options),
                    None => {
                        let ctx = &self.main.as_ref().unwrap().ctx;
                        self.preview_texture = Some(ctx.load_texture("preview", img, options));
                    }
                }
                self.st.preview_tex = self.preview_texture.as_ref().map(|h| h.id());
                self.st.frames_shown += 1;
                if self.st.frames_shown == 1 && std::env::var_os("REC_DEBUG").is_some() {
                    eprintln!("rec: debug: preview frame {}x{}", px[0], px[1]);
                }
            }
        }
        let win = if is_main {
            self.main.as_mut()
        } else {
            self.settings_win.as_mut()
        };
        let Some(win) = win else { return };
        let input = win.egui.take_egui_input(&win.window);
        let st = &mut self.st;
        let out = win.ctx.run_ui(input, |ui| {
            if is_main {
                ui::main_window(ui, st);
            } else {
                ui::settings_window(ui, st);
            }
        });
        win.egui
            .handle_platform_output(&win.window, out.platform_output);
        let delay = out
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map_or(Duration::MAX, |v| v.repaint_delay);
        win.repaint_at = if delay < Duration::from_secs(60) {
            Some(Instant::now() + delay)
        } else {
            None
        };
        let ppp = out.pixels_per_point;
        let prims = win.ctx.tessellate(out.shapes, ppp);
        let gpu = self.gpu.as_ref().unwrap();
        // Upload every frame, even when the surface frame is skipped, or a
        // later partial update targets a texture that was never allocated.
        for (id, deltas) in &out.textures_delta.set {
            for d in deltas {
                win.renderer.update_texture(&gpu.device, &gpu.queue, *id, d);
            }
        }
        let frame = match win.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            _ => {
                win.surface.configure(&gpu.device, &win.config);
                for id in &out.textures_delta.free {
                    win.renderer.free_texture(id);
                }
                return;
            }
        };
        let view = frame.texture.create_view(&Default::default());
        let sd = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [win.config.width, win.config.height],
            pixels_per_point: ppp,
        };
        let mut enc = gpu.device.create_command_encoder(&Default::default());
        let extra = win
            .renderer
            .update_buffers(&gpu.device, &gpu.queue, &mut enc, &prims, &sd);
        {
            let bg = theme::palette(win.ctx.theme() == egui::Theme::Dark).window;
            let [r, g, b, _] = bg.to_normalized_gamma_f32();
            let mut pass = enc
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("egui"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: r as f64,
                                g: g as f64,
                                b: b as f64,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            win.renderer.render(&mut pass, &prims, &sd);
        }
        gpu.queue.submit(extra.into_iter().chain([enc.finish()]));
        win.window.pre_present_notify();
        gpu.queue.present(frame);
        for id in &out.textures_delta.free {
            win.renderer.free_texture(id);
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.main.is_some() {
            return;
        }
        theme::set_accent(theme::accent_from_env());
        let ok = crate::hotkey::register(crate::hotkey::Shortcut::Record, || {
            push(Cmd::ToggleRecording)
        });
        if let Err(e) = ok {
            eprintln!(
                "rec: could not register {} ({e}); use the tray or the window instead",
                crate::hotkey::Shortcut::Record.label()
            );
            self.st.hotkey_ok = false;
        }
        let view = tray::View {
            has_last: false,
            replay: self.st.replay_stats.is_some(),
            updates: self.st.updates,
            hotkey_ok: self.st.hotkey_ok,
            ..Default::default()
        };
        // Never block the event loop on the status notifier.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("rec-tray-start".into())
            .spawn(move || {
                let _ = tx.send(tray::start(view, Arc::new(push)));
            })
            .ok();
        self.tray_rx = Some(rx);
        if std::env::var_os("REC_GUI_HIDDEN").is_some() {
            self.main_visible = false;
            self.tap.set_wanted(false);
        } else {
            self.show_main(el);
        }
        if self.start_recording {
            push(Cmd::ToggleRecording);
        }
        if install::appimage_path().is_some()
            && install::should_offer_integration(
                &install::appimage_path().unwrap(),
                &crate::util::home_dir().join(".local/share"),
            )
        {
            // No nagging: a notice with the menu item that does it.
            self.st.notice = Some((
                "Add Rec to your app menu from the ⋯ menu".into(),
                Instant::now(),
            ));
        }
        eprintln!("rec: app running (Ctrl+Shift+R toggles recording, Ctrl+Q quits)");
    }

    fn user_event(&mut self, el: &ActiveEventLoop, _: ()) {
        if self.quitting {
            return;
        }
        self.tick(el);
        if let Some(w) = &self.main
            && self.main_visible
            && !w.occluded
            && !self.screen_locked
        {
            w.window.request_redraw();
        }
        if let Some(w) = &self.settings_win {
            w.window.request_redraw();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let is_main = self.main.as_ref().is_some_and(|w| w.window.id() == id);
        let win = if is_main {
            self.main.as_mut()
        } else {
            self.settings_win.as_mut()
        };
        let Some(win) = win else { return };
        let resp = win.egui.on_window_event(&win.window, &event);
        match event {
            WindowEvent::CloseRequested => {
                if is_main {
                    self.hide_main();
                } else {
                    self.settings_win = None;
                }
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                let s = win.window.inner_size();
                win.config.width = s.width.max(1);
                win.config.height = s.height.max(1);
                if let Some(g) = &self.gpu {
                    win.surface.configure(&g.device, &win.config);
                }
                win.window.request_redraw();
            }
            WindowEvent::Occluded(o) => {
                win.occluded = o;
                if !o {
                    win.window.request_redraw();
                }
                if is_main {
                    self.main_occluded = o;
                    self.update_tap_wanted();
                }
            }
            WindowEvent::ThemeChanged(_) => {
                theme::apply(&win.ctx);
                win.window.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                self.redraw(id);
                if !self.st.cmds.is_empty() {
                    self.tick(el);
                }
            }
            _ => {
                if resp.repaint {
                    win.window.request_redraw();
                }
            }
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if self.quitting {
            el.exit();
            return;
        }
        if self.last_tick.elapsed() >= Duration::from_millis(100) {
            self.last_tick = Instant::now();
            self.tick(el);
        }
        // Animate while capturing (preview, meters, timer); otherwise idle
        // with a slow tick for signals and the control socket.
        let live = self.st.recording.is_some()
            || self.st.preview.is_some()
            || self.st.finishing
            || self.st.streaming();
        let now = Instant::now();
        let mut next = now + Duration::from_millis(if live { 100 } else { 250 });
        for w in [self.main.as_mut(), self.settings_win.as_mut()]
            .into_iter()
            .flatten()
        {
            let visible = w.window.is_visible().unwrap_or(true) && !w.occluded;
            if live && visible && !self.screen_locked {
                // Timer, meters: ~15 fps; preview frames wake us on arrival.
                w.window.request_redraw();
            }
            if let Some(t) = w.repaint_at.filter(|_| !w.occluded) {
                if t <= now {
                    w.repaint_at = None;
                    w.window.request_redraw();
                } else {
                    next = next.min(t);
                }
            }
        }
        el.set_control_flow(ControlFlow::WaitUntil(next));
    }
}

impl App {
    /// The preview wants frames while the window is visible, not covered and
    /// the screen is not locked (G8; upstream draws nothing while locked).
    fn update_tap_wanted(&mut self) {
        self.tap.set_wanted(lock::preview_wanted(
            self.main_visible,
            self.main_occluded,
            self.screen_locked,
        ));
    }

    /// Destroy the windows (their surfaces, renderers and egui contexts) while
    /// the event loop is still running: a Wayland surface must not outlive it.
    fn close_windows(&mut self) {
        self.preview_texture = None;
        self.st.preview_tex = None;
        self.st.preview_px = [0, 0];
        self.settings_win = None;
        self.main = None;
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // Idempotent: `quit` already closed them; this covers a panic path, and
        // keeps the windows (which were created from the device) ahead of the
        // GPU instance in the drop order.
        self.close_windows();
    }
}

/// Ask the desktop's settings portal for its accent colour, once, on a thread:
/// a session without the portal simply keeps Rec's blue.
fn spawn_accent_fetch(tx: Sender<Option<egui::Color32>>) {
    std::thread::Builder::new()
        .name("rec-accent".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let color = rt.ok().and_then(|rt| {
                rt.block_on(async {
                    let s = ashpd::desktop::settings::Settings::new().await.ok()?;
                    let c = s.accent_color().await.ok()?;
                    let to = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
                    Some(egui::Color32::from_rgb(
                        to(c.red()),
                        to(c.green()),
                        to(c.blue()),
                    ))
                })
            });
            let _ = tx.send(color);
        })
        .ok();
}

/// Start the app. `start_recording` records immediately (a login item or a
/// script), `settings` opens the settings window, `window` captures one window
/// (id from `rec-rs devices`) and `test_pattern` swaps the screen for the
/// synthetic source (the headless self-test).
pub fn run(
    start_recording: bool,
    settings: bool,
    window: Option<u32>,
    test_pattern: bool,
) -> Result<()> {
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as *const () as usize);
        libc::signal(libc::SIGTERM, on_sigterm as *const () as usize);
        libc::signal(libc::SIGHUP, on_sigterm as *const () as usize);
    }
    record::install_control_signals();
    // One Rec per session: a second launch tells the running one what it
    // wanted and exits.
    let launch = if start_recording {
        Control::StartRecording
    } else if settings {
        Control::Settings
    } else {
        Control::Show
    };
    let (listener, control_rx) = match control::claim(launch) {
        Claim::AlreadyRunning => {
            println!("rec: Rec is already running; asked it to show its window");
            return Ok(());
        }
        Claim::Listening { listener, rx } => (Some(listener), rx),
        Claim::Unavailable(rx) => (None, rx),
    };
    let event_loop = EventLoop::<()>::with_user_event().build()?;
    let _ = PROXY.set(Mutex::new(event_loop.create_proxy()));
    let mut app = App::new(
        start_recording,
        settings,
        window,
        test_pattern,
        control_rx,
        listener,
    );
    event_loop.run_app(&mut app)?;
    if std::env::var_os("REC_DEBUG").is_some() {
        eprintln!("rec: debug: event loop finished");
    }
    let _ = std::fs::remove_file(record::pidfile_path());
    drop(app);
    if std::env::var_os("REC_DEBUG").is_some() {
        eprintln!("rec: debug: app dropped");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preview_banner_names_the_phase() {
        // A backend that isn't built says which phase owes it, so the banner is
        // actionable and never just "error".
        let e = anyhow::anyhow!(
            "screen capture is not implemented yet (phase 1A); the portal is missing"
        );
        let msg = session::preview_error(&e);
        assert!(msg.starts_with("Preview unavailable: "), "{msg}");
        assert!(msg.contains("phase 1A"), "{msg}");
        let e = anyhow::anyhow!("no display found");
        assert_eq!(
            session::preview_error(&e),
            "Preview unavailable: no display found"
        );
    }

    #[test]
    fn state_shows_the_recording_clock_and_pause() {
        let st = State::new(Settings::default());
        assert_eq!(st.clock(), "");
        assert!(!st.is_paused());
        assert!(!st.streaming());
        assert!(st.updates == install::updates_self());
    }

    #[test]
    fn stream_key_cache_asks_the_backend_once() {
        let mut st = State::new(Settings::default());
        // An empty URL never has a key and never asks.
        assert!(!st.has_stream_key(""));
        // A key that isn't stored is remembered as "no", also after the
        // environment changes: the answer is per URL, not a polling loop.
        unsafe { std::env::remove_var("REC_STREAM_KEY") };
        let url = "rtmps://live.twitch.tv:443/app";
        assert!(!st.has_stream_key(url));
        st.key_saved.insert(url.to_string(), true);
        assert!(st.has_stream_key(url));
        st.delete_stream_key(url);
        assert!(!st.has_stream_key(url));
    }

    #[test]
    fn the_window_is_hidden_only_for_a_display_recording() {
        // G3: a display or region capture would film Rec's own window; a
        // single-window capture cannot, and the setting can turn it off.
        let mut settings = Settings::default();
        assert!(settings.hide_window_when_recording, "on by default");
        assert!(hides_window_for_recording(&settings, true));
        assert!(
            !hides_window_for_recording(&settings, false),
            "already hidden"
        );
        settings.window = Some(42);
        assert!(
            !hides_window_for_recording(&settings, true),
            "a window capture cannot contain our window"
        );
        settings.window = None;
        settings.hide_window_when_recording = false;
        assert!(!hides_window_for_recording(&settings, true));
    }

    #[test]
    fn hotplug_notices_a_changed_mic_and_device_list() {
        // G9: a plugged/unplugged device changes the fingerprint, and a preview
        // whose mic now resolves to another device restarts.
        use crate::media_types::{CameraInfo, MicInfo};
        let mic = |id: &str| MicInfo {
            id: id.into(),
            name: format!("Mic {id}"),
            rate: Some(48_000),
        };
        let cam = |id: &str| CameraInfo {
            id: id.into(),
            name: format!("Cam {id}"),
        };
        let a = device_fingerprint(&[cam("c1")], &[mic("m1")]);
        assert_eq!(a, ["cam:c1:Cam c1", "mic:m1:Mic m1"]);
        assert_eq!(a, device_fingerprint(&[cam("c1")], &[mic("m1")]), "stable");
        assert_ne!(a, device_fingerprint(&[cam("c1"), cam("c2")], &[mic("m1")]));
        assert_ne!(a, device_fingerprint(&[cam("c1")], &[mic("m1"), mic("m2")]));

        assert!(!mic_needs_restart(Some(&Some("m1".into())), Some("m1")));
        assert!(mic_needs_restart(Some(&Some("m1".into())), Some("m2")));
        assert!(mic_needs_restart(Some(&Some("m1".into())), None));
        assert!(
            mic_needs_restart(None, Some("m1")),
            "the running session has no mic but the settings want one"
        );
        assert!(!mic_needs_restart(None, None), "nothing to change");
    }

    #[test]
    fn meters_decay_and_hold_peaks() {
        let mut m = Meter::default();
        m.feed(1.0, 0.1);
        assert!((m.db - 0.0).abs() < 1e-3, "{}", m.db);
        assert!((m.peak_db - 0.0).abs() < 1e-3);
        // Silence decays at 24 dB/s instead of dropping to -90 at once.
        m.feed(0.0, 0.1);
        assert!((m.db + 2.4).abs() < 1e-3, "{}", m.db);
        assert!((m.peak_db - 0.0).abs() < 1e-3, "the peak holds");
        // After the hold time a louder peak replaces it; a quieter one doesn't
        // until then.
        m.peak_at = Some(Instant::now() - Duration::from_secs(2));
        m.feed(0.1, 0.1);
        assert!((m.peak_db + 20.0).abs() < 0.2, "{}", m.peak_db);
    }
}
