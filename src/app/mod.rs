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

pub(crate) mod control;
mod install;
mod linux_desktop;
mod lock;
mod preview;
#[cfg(test)]
pub(crate) use preview::{Frame as TestPreviewFrame, Tap as TestPreviewTap};
mod preview_gpu;
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
use winit::raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
};
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
/// A camera or microphone was plugged in or removed (1B's watchers, which run
/// on their own threads): reload in the tick (G9).
static DEVICES_CHANGED: AtomicBool = AtomicBool::new(false);
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

/// The actual winit backend, not the session environment (XWayland can have
/// WAYLAND_DISPLAY set too). Initialized before any app windows are opened.
static WAYLAND: AtomicBool = AtomicBool::new(false);
pub(crate) fn wayland() -> bool {
    WAYLAND.load(Ordering::Relaxed)
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
    /// The newest composited frame, drawn by `preview_gpu` (G10): the GPU
    /// converts and scales it, so the app never touches its pixels.
    pub preview_frame: Option<Arc<crate::media_types::VideoFrame>>,
    /// The composited frame as a GPU texture (G10): set instead of
    /// `preview_frame` when the compositor shares this window's device, so the
    /// preview draws it with no readback and no upload.
    pub preview_tex: Option<Arc<crate::gpu::PreviewFrame>>,
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
            preview_frame: None,
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

/// The X11 window id behind a winit handle, if it has one (Wayland windows have
/// no X11 id: their capture goes through the portal).
pub(crate) fn x11_window_id(handle: &RawWindowHandle) -> Option<u32> {
    match handle {
        RawWindowHandle::Xlib(h) => Some(h.window as u32),
        RawWindowHandle::Xcb(h) => Some(h.window.get()),
        _ => None,
    }
}

/// Whether the capture backend can paint Rec's own windows out by itself
/// (`capture::screen::set_excluded_windows`, X11). Elsewhere (the portal) the
/// only exclusion is hiding them.
pub(crate) fn exclusion_is_done_by_capture(
    backend: Result<crate::capture::screen::Backend>,
) -> bool {
    matches!(backend, Ok(crate::capture::screen::Backend::X11))
}

/// What closing (or Ctrl+W) does on this session (1E): keep running in the
/// tray, or quit when nothing would show the window again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HideAction {
    /// Wayland cannot hide a window (`winit::Window::set_visible` is a no-op
    /// there): destroy it and recreate it on show.
    DestroyWindow,
    /// X11: hide it in place.
    SetInvisible,
    /// No tray host: the window is the only way back, so closing quits.
    Quit,
}

pub(crate) fn hide_action(on_wayland: bool, tray_available: bool) -> HideAction {
    if !tray_available {
        HideAction::Quit
    } else if on_wayland {
        HideAction::DestroyWindow
    } else {
        HideAction::SetInvisible
    }
}

/// Whether a camera scene's camera has not delivered a frame: the preview
/// would be black with no explanation (1B's portal path can fail after the
/// session started, and a camera that never opens looks the same).
pub(crate) fn camera_not_delivering(live: bool, has_aspect: bool, elapsed: Duration) -> bool {
    live && !has_aspect && elapsed >= Duration::from_secs(5)
}

/// Whether closing should explain itself: once, only when the app keeps
/// running (a quit needs no explanation).
pub(crate) fn should_notify_hidden(already_notified: bool, action: HideAction) -> bool {
    !already_notified && matches!(action, HideAction::DestroyWindow | HideAction::SetInvisible)
}

/// Whether the main window must be hidden for a recording (G3): a display or
/// region capture that cannot exclude Rec's own window would film the preview;
/// a single-window capture cannot contain it, the user can turn the behaviour
/// off, and on X11 the capture paints our windows out (no hide needed).
pub(crate) fn hides_window_for_recording(
    settings: &Settings,
    main_visible: bool,
    capture_excludes_ours: bool,
) -> bool {
    settings.hide_window_when_recording
        && settings.window.is_none()
        && main_visible
        && !capture_excludes_ours
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

struct CaptureHide {
    windows: Vec<WindowId>,
    sync: Option<WaylandSync>,
    deadline: Instant,
}

impl CaptureHide {
    fn destroyed(&mut self, id: WindowId) {
        self.windows.retain(|w| *w != id);
    }

    fn ready_to_sync(&self, now: Instant) -> Result<bool> {
        anyhow::ensure!(
            now < self.deadline,
            "timed out waiting for Wayland window destruction/sync"
        );
        Ok(self.windows.is_empty())
    }
}

/// A nonblocking wl_display.sync on winit's libwayland connection. A private
/// event queue avoids dispatching winit callbacks reentrantly. Winit reads the
/// socket; we dispatch only our callback. No second Wayland connection is used.
///
/// This acknowledges processing of destruction requests, NOT presentation of a
/// new desktop frame, nor draining frames already queued by a portal backend.
struct WaylandSync {
    _display: winit::event_loop::OwnedDisplayHandle,
    library: *mut libc::c_void,
    display: *mut libc::c_void,
    queue: *mut libc::c_void,
    callback: *mut libc::c_void,
    done: Box<bool>,
    dispatch: unsafe extern "C" fn(*mut libc::c_void, *mut libc::c_void) -> libc::c_int,
    flush: unsafe extern "C" fn(*mut libc::c_void) -> libc::c_int,
    destroy_proxy: unsafe extern "C" fn(*mut libc::c_void),
    destroy_queue: unsafe extern "C" fn(*mut libc::c_void),
}

impl WaylandSync {
    fn new(el: &ActiveEventLoop) -> Result<Self> {
        let owned = el.owned_display_handle();
        let RawDisplayHandle::Wayland(display) = owned.display_handle()?.as_raw() else {
            anyhow::bail!("winit is not using Wayland");
        };
        // SAFETY: winit 0.30 uses wayland-backend/client_system; its raw display
        // is a libwayland wl_display. The owned handle keeps it alive. All calls
        // and callback dispatch happen on the event-loop thread. Function ABIs
        // below are from wayland-client-core.h (sync is display opcode 0).
        unsafe {
            let library = libc::dlopen(c"libwayland-client.so.0".as_ptr(), libc::RTLD_NOW);
            anyhow::ensure!(!library.is_null(), "could not load libwayland-client");
            macro_rules! symbol {
                ($name:literal) => {{
                    let p = libc::dlsym(library, $name.as_ptr());
                    if p.is_null() {
                        libc::dlclose(library);
                        anyhow::bail!("missing libwayland symbol");
                    }
                    p
                }};
                ($name:literal, $ty:ty) => {{ std::mem::transmute::<*mut libc::c_void, $ty>(symbol!($name)) }};
            }
            type Ptr = *mut libc::c_void;
            let create_queue =
                symbol!(c"wl_display_create_queue", unsafe extern "C" fn(Ptr) -> Ptr);
            let create_wrapper =
                symbol!(c"wl_proxy_create_wrapper", unsafe extern "C" fn(Ptr) -> Ptr);
            let destroy_wrapper = symbol!(c"wl_proxy_wrapper_destroy", unsafe extern "C" fn(Ptr));
            let set_queue = symbol!(c"wl_proxy_set_queue", unsafe extern "C" fn(Ptr, Ptr));
            let marshal = symbol!(
                c"wl_proxy_marshal_constructor",
                unsafe extern "C" fn(Ptr, u32, Ptr, ...) -> Ptr
            );
            let add_listener = symbol!(
                c"wl_proxy_add_listener",
                unsafe extern "C" fn(Ptr, *const unsafe extern "C" fn(), Ptr) -> libc::c_int
            );
            let interface = symbol!(c"wl_callback_interface");
            let mut sync = Self {
                _display: owned,
                library,
                display: display.display.as_ptr(),
                queue: std::ptr::null_mut(),
                callback: std::ptr::null_mut(),
                done: Box::new(false),
                dispatch: symbol!(
                    c"wl_display_dispatch_queue_pending",
                    unsafe extern "C" fn(Ptr, Ptr) -> libc::c_int
                ),
                flush: symbol!(
                    c"wl_display_flush",
                    unsafe extern "C" fn(Ptr) -> libc::c_int
                ),
                destroy_proxy: symbol!(c"wl_proxy_destroy", unsafe extern "C" fn(Ptr)),
                destroy_queue: symbol!(c"wl_event_queue_destroy", unsafe extern "C" fn(Ptr)),
            };
            sync.queue = create_queue(sync.display);
            anyhow::ensure!(!sync.queue.is_null(), "could not create Wayland sync queue");
            let wrapper = create_wrapper(sync.display);
            anyhow::ensure!(!wrapper.is_null(), "could not wrap Wayland display");
            set_queue(wrapper, sync.queue);
            sync.callback = marshal(wrapper, 0, interface, std::ptr::null_mut::<libc::c_void>());
            destroy_wrapper(wrapper);
            anyhow::ensure!(
                !sync.callback.is_null(),
                "could not create Wayland sync callback"
            );
            unsafe extern "C" fn done(data: Ptr, _: Ptr, _: u32) {
                // SAFETY: data points to the stable Box until proxy destruction.
                unsafe {
                    *data.cast::<bool>() = true;
                }
            }
            static LISTENER: [unsafe extern "C" fn(Ptr, Ptr, u32); 1] = [done];
            anyhow::ensure!(
                add_listener(
                    sync.callback,
                    LISTENER.as_ptr().cast(),
                    (&mut *sync.done as *mut bool).cast()
                ) == 0,
                "could not attach Wayland sync listener"
            );
            Ok(sync)
        }
    }

    fn poll(&mut self) -> Result<bool> {
        // SAFETY: see new(); these objects remain live until Drop.
        unsafe {
            let rc = (self.flush)(self.display);
            anyhow::ensure!(
                rc >= 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EAGAIN),
                "could not flush Wayland sync"
            );
            anyhow::ensure!(
                (self.dispatch)(self.display, self.queue) >= 0,
                "Wayland sync dispatch failed"
            );
        }
        Ok(*self.done)
    }
}

impl Drop for WaylandSync {
    fn drop(&mut self) {
        // SAFETY: only our callback/queue are destroyed, not winit's display.
        // Destroying the proxy also makes a late callback harmless on timeout.
        unsafe {
            if !self.callback.is_null() {
                (self.destroy_proxy)(self.callback);
            }
            if !self.queue.is_null() {
                (self.destroy_queue)(self.queue);
            }
            libc::dlclose(self.library);
        }
    }
}

struct App {
    gpu: Option<Gpu>,
    main: Option<Win>,
    settings_win: Option<Win>,
    main_visible: bool,
    /// Quit is in progress: dispatch teardown completions only, never touch
    /// windows or start another capture session.
    quitting: bool,
    /// The main window was hidden for the running recording (G3): a capture
    /// that cannot exclude Rec's window would film it. Restored on stop.
    hidden_for_recording: bool,
    /// Wait for winit's deferred destruction, then a same-connection wl_display.sync.
    capture_hide: Option<CaptureHide>,
    /// The screen is locked (G8): nothing is drawn and no preview frames are
    /// converted until it is unlocked.
    screen_locked: bool,
    /// The main window is covered or minimized (winit's occlusion event, which
    /// Wayland often does not send).
    main_occluded: bool,
    /// The main window's last logical size and (X11 only) position, to reopen
    /// where it was after a Wayland destroy.
    main_size: (f64, f64),
    main_pos: Option<(i32, i32)>,
    /// The "no camera frames" warning was shown for the running session.
    camera_warned: bool,
    /// The lock watcher (G8), if a bus answered.
    lock: Option<lock::LockWatcher>,
    /// The device lists as last seen, to ignore a watch event that changes
    /// nothing (G9).
    device_fingerprint: Vec<String>,
    /// 1B's hotplug watchers (G9); they stop when dropped.
    _camera_watch: Option<crate::capture::camera::DeviceWatch>,
    _audio_watch: Option<crate::capture::audio::DeviceWatch>,
    st: State,
    tray: Option<tray::Handle>,
    /// The tray starts on a thread: registering the status item talks to D-Bus,
    /// and a slow (or missing) watcher must not hold up the first frame.
    tray_rx: Option<Receiver<Option<tray::Handle>>>,
    tray_watcher: Option<tray::Watcher>,
    tap: preview::Tap,
    tap_sink: FrameSink,
    gpu_preview_sink: crate::gpu::PreviewSink,
    last_tick: Instant,
    last_meter: Instant,
    control_rx: Receiver<Control>,
    _listener: Option<control::Listener>,
    /// The recorder socket, held only while a recording runs, so a CLI
    /// `rec-rs record` can start while this app is idle (and refuses while it
    /// records).
    recorder_server: Option<control::Server>,
    recorder_rx: Receiver<Control>,
    start_recording: bool,
    test_pattern: bool,
    finished: Arc<Mutex<Option<Finished>>>,
    /// Preview teardown is not recording finalization: it must not publish a
    /// file, remove the recorder pidfile/socket, or restore hidden windows.
    preview_cleanup: Option<std::thread::JoinHandle<()>>,
    pending_recording: bool,
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
        let gpu_preview_sink = tap.gpu_sink();
        let lock = lock::LockWatcher::start(Arc::new(wake));
        // G9: 1B's watchers call back on their own thread; wake the loop and
        // reload in the tick. An unavailable backend simply has no watch.
        let changed = || {
            DEVICES_CHANGED.store(true, Ordering::SeqCst);
            wake();
        };
        let camera_watch = crate::capture::camera::watch_devices(changed).ok();
        let audio_watch = crate::capture::audio::watch_devices(changed).ok();
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
            capture_hide: None,
            screen_locked: false,
            main_occluded: false,
            main_size: (1040.0, 660.0),
            main_pos: None,
            camera_warned: false,
            device_fingerprint,
            st,
            tray: None,
            tray_rx: None,
            tray_watcher: None,
            lock,
            _camera_watch: camera_watch,
            _audio_watch: audio_watch,
            tap,
            tap_sink,
            gpu_preview_sink,
            last_tick: Instant::now(),
            last_meter: Instant::now(),
            control_rx,
            _listener: listener,
            recorder_server: None,
            recorder_rx: std::sync::mpsc::channel().1,
            start_recording,
            test_pattern,
            finished: Arc::new(Mutex::new(None)),
            preview_cleanup: None,
            pending_recording: false,
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
        let mut renderer =
            egui_wgpu::Renderer::new(&g.device, format, egui_wgpu::RendererOptions::default());
        // G10: the preview's pipelines and textures live with the window.
        renderer
            .callback_resources
            .insert(preview_gpu::PreviewPainter::new(&g.device, format));
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
        if self.exit_if_requested(el) {
            return;
        }
        if self.main.is_none() {
            // REC_MAIN_SIZE=WxH or full overrides the remembered size (layout
            // checks and screenshots). A window destroyed on hide reopens where
            // it was; Wayland lets the compositor place it, an X11 position is
            // honoured.
            let size = std::env::var("REC_MAIN_SIZE")
                .ok()
                .and_then(|v| {
                    v.split_once('x')
                        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                })
                .unwrap_or(self.main_size);
            let w = self.open_window(el, "Rec", size, (860.0, 560.0));
            if let Some((x, y)) = self.main_pos {
                w.window
                    .set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
            }
            if std::env::var("REC_MAIN_SIZE").as_deref() == Ok("full") {
                w.window
                    .set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
            }
            self.main = Some(w);
            if std::env::var_os("REC_DEBUG").is_some() {
                eprintln!("rec: debug: window created ({:?})", self.main_size);
            }
        }
        if let Some(w) = &self.main {
            if !wayland() {
                w.window.set_visible(true);
            }
            if !background() {
                w.window.focus_window();
            }
            w.window.request_redraw();
        }
        self.main_visible = true;
        self.update_tap_wanted();
        self.sync_preview();
    }

    /// Hide the main window. On Wayland `set_visible(false)` is a no-op, so the
    /// window (surface, egui state, preview textures) is destroyed and recreated
    /// by [`show_main`](App::show_main); X11 hides it in place.
    fn hide_main(&mut self) {
        if let Some(w) = &self.main {
            self.main_size = {
                let s = w
                    .window
                    .inner_size()
                    .to_logical::<f64>(w.window.scale_factor());
                (s.width, s.height)
            };
            self.main_pos = w.window.outer_position().ok().map(|p| (p.x, p.y));
            if !wayland() {
                w.window.set_visible(false);
                self.main_size = (
                    w.window.inner_size().width as f64,
                    w.window.inner_size().height as f64,
                );
            }
        }
        if wayland() {
            // Drop the window and everything that belongs to it; the app state
            // (settings, sessions, tray) stays.
            self.main = None;
            if std::env::var_os("REC_DEBUG").is_some() {
                eprintln!("rec: debug: window destroyed (Wayland cannot hide it)");
            }
        }
        self.main_visible = false;
        // Nothing is drawn while the window is hidden: the tap drops frames.
        self.update_tap_wanted();
        self.st.preview_frame = None;
        self.sync_preview();
    }

    /// The close button / Ctrl+W: hide to the tray, or quit when no tray host
    /// exists (otherwise the app would be unreachable). Wayland destroys the
    /// window; the first close explains itself once.
    fn close_or_hide(&mut self, el: &ActiveEventLoop) {
        let action = hide_action(wayland(), self.tray_available());
        if action == HideAction::Quit {
            if std::env::var_os("REC_DEBUG").is_some() {
                eprintln!("rec: debug: no tray host; closing quits");
            }
            self.handle(el, Cmd::Quit);
            return;
        }
        let notify = should_notify_hidden(self.st.settings.notifies_when_hidden, action);
        self.hide_main();
        if notify {
            self.st.settings.notifies_when_hidden = true;
            let _ = self.st.settings.save();
            linux_desktop::notify(
                "Rec",
                "Rec is still running in the background; quit it from the tray.",
            );
        }
    }

    /// Only a confirmed, still-online registration is a way back to the app.
    /// A pending, failed or disconnected startup channel is never a tray host.
    fn tray_available(&self) -> bool {
        self.tray.as_ref().is_some_and(tray::Handle::available)
    }

    fn show_settings(&mut self, el: &ActiveEventLoop) {
        if self.exit_if_requested(el) {
            return;
        }
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
        let mut o = session::options(
            &settings,
            output,
            Some(self.st.live.clone()),
            Some(self.tap_sink.clone()),
            pattern,
        )?;
        // G10: composite on this window's device so the preview can draw the
        // composited texture directly. The writer falls back to a device of its
        // own (and to CPU preview frames) if this one cannot take it.
        if let Some(g) = &self.gpu {
            o.gpu = Some(crate::gpu::SharedGpu {
                device: Arc::new(g.device.clone()),
                queue: Arc::new(g.queue.clone()),
                info: g.adapter.get_info(),
            });
            o.preview_gpu = Some(self.gpu_preview_sink.clone());
        }
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
        self.check_preview_failure();
        // A taken session still owns its capture until the worker returns.
        if self.quitting || self.preview_cleanup.is_some() || self.pending_recording {
            return;
        }
        // The replay buffer and streams keep a session running while the
        // window is closed.
        let want = (self.main_visible || self.live_wanted())
            && self.st.recording.is_none()
            && !self.st.finishing
            && self.capture_hide.is_none();
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
                self.st.preview = Some(s);
                self.finish_preview_async();
            }
            // Do not open another capture until the old worker releases it.
            return;
        }
        if want && self.st.preview.is_none() && self.st.error.is_none() {
            self.exclude_own_windows();
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

    fn toggle_recording(&mut self, el: &ActiveEventLoop) {
        if self.preview_cleanup.is_some() || self.pending_recording {
            self.pending_recording = !self.pending_recording;
            return;
        }
        if self.st.finishing || self.capture_hide.is_some() {
            return;
        }
        if self.st.recording.is_some() {
            self.stop_recording();
        } else {
            self.start_recording(el);
        }
    }

    fn start_recording(&mut self, el: &ActiveEventLoop) {
        self.check_preview_failure();
        if self.preview_cleanup.is_some() {
            self.pending_recording = true;
            return;
        }
        // One capture session at a time: the recording feeds the preview and
        // takes over its camera, already exposed for the room.
        let mut cam = self.st.spare_cam.take();
        if let Some(mut s) = self.st.preview.take() {
            cam = cam.or_else(|| s.take_camera());
            self.st.spare_cam = cam;
            self.st.preview = Some(s);
            self.finish_preview_async();
            self.pending_recording = true;
            return;
        }
        self.st.preview_settings = None;
        self.st.error = None;
        // G3: the window must be off the screen before the first frame is
        // captured, so hide it here, not after the session started.
        if hides_window_for_recording(
            &self.st.settings,
            self.main_visible,
            Self::capture_excludes_ours(),
        ) {
            if wayland() {
                self.capture_hide = Some(CaptureHide {
                    windows: [self.main.as_ref(), self.settings_win.as_ref()]
                        .into_iter()
                        .flatten()
                        .map(|w| w.window.id())
                        .collect(),
                    sync: None,
                    deadline: Instant::now() + Duration::from_secs(5),
                });
            }
            self.hide_window_for_recording();
        }
        self.st.spare_cam = cam;
        if self.capture_hide.is_none() {
            self.finish_start_recording(el);
        }
    }

    fn finish_start_recording(&mut self, el: &ActiveEventLoop) {
        let cam = self.st.spare_cam.take();
        let file = self.st.settings.new_output_file();
        // The capture reads the exclusion list when it opens, so the recording
        // (which replaces the preview's capture) gets the current windows.
        self.exclude_own_windows();
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
                self.start_recorder_socket();
                record::PAUSE_REQ.store(false, Ordering::SeqCst);
                record::RESUME_REQ.store(false, Ordering::SeqCst);
            }
            Err(e) => {
                eprintln!("rec: could not start recording: {e:#}");
                self.st.error = Some(format!("Could not start recording: {e:#}"));
                self.restore_after_recording(el);
                self.sync_preview();
            }
        }
    }

    /// Rec's own top-level X11 window ids, for
    /// `capture::screen::set_excluded_windows` (G3): the capture paints their
    /// visible parts out of every frame on X11. Empty on Wayland, where the ids
    /// do not exist and the portal cannot exclude windows either way.
    fn own_window_ids(&self) -> Vec<u32> {
        let mut ids = Vec::new();
        for w in [self.main.as_ref(), self.settings_win.as_ref()]
            .into_iter()
            .flatten()
        {
            if let Ok(handle) = w.window.window_handle()
                && let Some(id) = x11_window_id(&handle.as_raw())
            {
                ids.push(id);
            }
        }
        ids
    }

    /// Hand our window ids to the capture that is about to start (G3). The
    /// capture reads the list once, when it opens.
    fn exclude_own_windows(&self) {
        let ids = self.own_window_ids();
        if std::env::var_os("REC_DEBUG").is_some() {
            eprintln!("rec: debug: excluding own windows {ids:?}");
        }
        crate::capture::screen::set_excluded_windows(ids);
    }

    /// Hold the recorder socket while a recording runs: `rec-rs
    /// stop|pause|resume|save-replay` find it there (in a Flatpak too, where the
    /// pidfile's pid means nothing), and a second recorder refuses on
    /// `AddrInUse`. An idle app holds only its control socket.
    fn start_recorder_socket(&mut self) {
        if self.recorder_server.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let handler: control::Handler = std::sync::Arc::new(move |c| tx.send(c).is_ok());
        match control::serve_recorder(handler) {
            Ok(server) => {
                self.recorder_server = Some(server);
                self.recorder_rx = rx;
            }
            Err(e) => {
                eprintln!("rec: warning: could not open the recorder socket: {e}");
            }
        }
    }

    /// Release the recorder socket (the recording ended): the name is free for
    /// the next recorder.
    fn stop_recorder_socket(&mut self) {
        self.recorder_server = None;
    }

    /// Whether the capture excludes our windows itself (X11) rather than the app
    /// having to hide them (portal).
    fn capture_excludes_ours() -> bool {
        exclusion_is_done_by_capture(crate::capture::screen::backend())
    }

    /// After a device was plugged in or removed (G9): refresh the lists, and
    /// restart the preview when the saved mic now resolves to another device
    /// (upstream's `devices_changed`).
    fn devices_changed(&mut self) {
        let cameras = crate::capture::camera::cameras().unwrap_or_default();
        let mics = crate::capture::audio::microphones().unwrap_or_default();
        let fingerprint = device_fingerprint(&cameras, &mics);
        if fingerprint == self.device_fingerprint {
            return;
        }
        self.device_fingerprint = fingerprint;
        load_devices(&mut self.st);
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

    /// Hide the main window *before* the capture starts, and say so (G3).
    ///
    /// Wayland destruction is deferred by winit. `poll_capture_hide` waits for
    /// Destroyed events and then syncs the *same* display connection before
    /// opening capture. Portal setup on a different connection is not a fence.
    fn hide_window_for_recording(&mut self) {
        if !self.main_visible {
            return;
        }
        if std::env::var_os("REC_DEBUG").is_some() {
            eprintln!(
                "rec: debug: hiding own window for the recording (the portal cannot exclude it)"
            );
        }
        self.hide_main();
        // The settings window is Rec's own window too: it would be filmed as
        // well (the portal cannot exclude either), so it closes here.
        self.settings_win = None;
        self.hidden_for_recording = true;
        let message = "Rec's window is hidden while recording, because a screen capture would \
                       film it. Open it from the tray.";
        self.st.notice = Some((message.into(), Instant::now()));
        // The window that would show the notice is the one being hidden.
        linux_desktop::notify("Rec", message);
        // On X11 this is not reached: the capture paints our windows out
        // (`set_excluded_windows`); hiding is the portal's only exclusion.
    }

    fn poll_capture_hide(&mut self, el: &ActiveEventLoop) {
        let Some(hide) = &mut self.capture_hide else {
            return;
        };
        let result = (|| -> Result<bool> {
            if !hide.ready_to_sync(Instant::now())? {
                return Ok(false);
            }
            if hide.sync.is_none() {
                // winit sends Destroyed only AFTER dropping WindowState, which
                // queues xdg_toplevel/xdg_surface/wl_surface destruction.
                hide.sync = Some(WaylandSync::new(el)?);
            }
            hide.sync.as_mut().unwrap().poll()
        })();
        match result {
            Ok(false) => (),
            Ok(true) => {
                self.capture_hide = None;
                self.finish_start_recording(el);
            }
            Err(e) => {
                self.capture_hide = None;
                self.st.error = Some(format!("Could not hide window before recording: {e:#}"));
                self.restore_after_recording(el);
            }
        }
    }

    /// Bring the window back after a recording that hid it (G3): a window
    /// destroyed on hide is recreated here.
    fn restore_after_recording(&mut self, el: &ActiveEventLoop) {
        if self.hidden_for_recording {
            self.hidden_for_recording = false;
            self.show_main(el);
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

    fn check_preview_failure(&mut self) {
        if let Some(s) = self.st.preview.as_ref()
            && s.failed()
        {
            let err = s.error().unwrap_or_default();
            self.st.error = Some(format!("Preview stopped: {err}"));
            if std::env::var_os("REC_DEBUG").is_some() {
                eprintln!("rec: debug: preview failed; cleaning up asynchronously: {err}");
            }
            self.finish_preview_async();
        }
    }

    /// Move preview teardown off winit: even healthy capture can block joining
    /// source threads or waiting on GPU work. The worker owns the entire session
    /// (no borrowed app/window state), so dropping its handle safely detaches it.
    fn finish_preview_async(&mut self) {
        let Some(s) = self.st.preview.take() else {
            return;
        };
        debug_assert!(self.preview_cleanup.is_none());
        self.st.preview_settings = None;
        self.preview_cleanup = Some(std::thread::spawn(move || {
            let _ = s.finish();
            wake();
        }));
    }

    fn poll_preview_cleanup(&mut self) {
        if self
            .preview_cleanup
            .as_ref()
            .is_some_and(|w| w.is_finished())
        {
            let _ = self.preview_cleanup.take().unwrap().join();
            if std::env::var_os("REC_DEBUG").is_some() {
                eprintln!("rec: debug: preview cleanup complete");
            }
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
        self.pending_recording = false;
        self.capture_hide = None;
        self.stop_recording();
        self.finish_preview_async();
        // Idle preview has no recording to finalize. Never let stuck capture/GPU
        // teardown hold Quit hostage; the owning worker can outlive the app.
        self.preview_cleanup.take();
        self.poll_quit(el);
    }

    /// Keep dispatching the desktop connection while a recording finalizes;
    /// idle preview teardown is detached, not a prerequisite for exiting.
    /// Quit stays latched and no late command may start a session or window.
    fn poll_quit(&mut self, el: &ActiveEventLoop) {
        self.poll_preview_cleanup();
        let done = self.finished.lock().unwrap().take();
        if let Some((file, result)) = done {
            self.st.finishing = false;
            self.stop_recorder_socket();
            println!("{}", file.display());
            if let Err(e) = result {
                eprintln!("rec: could not finish the recording: {e}");
            }
        }
        if self.st.finishing {
            el.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(20),
            ));
            return;
        }
        session::stop_all(&self.st.live);
        self.tray = None;
        // Before the loop ends: see `close_windows`.
        self.close_windows();
        if std::env::var_os("REC_DEBUG").is_some() {
            eprintln!("rec: debug: quit: exiting the event loop");
        }
        el.exit();
    }

    fn handle(&mut self, el: &ActiveEventLoop, c: Cmd) {
        if self.exit_if_requested(el) {
            return;
        }
        // Reopening a window during the fence would invalidate the exclusion.
        // Treat an explicit show request as cancellation of the pending start.
        if matches!(c, Cmd::ShowWindow | Cmd::ShowSettings) && self.capture_hide.is_some() {
            self.capture_hide = None;
            self.restore_after_recording(el);
        }
        match c {
            Cmd::ToggleRecording => self.toggle_recording(el),
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
            Cmd::HideWindow => self.close_or_hide(el),
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

    /// Check before touching any windows, not just on the periodic tick:
    /// a queued redraw can precede that tick during desktop/session teardown.
    /// Keep SIG_QUIT latched, including while a recording finishes; subsequent
    /// commands must never recreate a window after quit has closed it.
    fn exit_if_requested(&mut self, el: &ActiveEventLoop) -> bool {
        if self.quitting {
            self.poll_quit(el);
            return true;
        }
        if SIG_QUIT.load(Ordering::SeqCst) {
            self.quit(el);
            return true;
        }
        false
    }

    /// Periodic work: commands, signals, finished recordings, failures, tray,
    /// meters.
    fn tick(&mut self, el: &ActiveEventLoop) {
        if self.exit_if_requested(el) {
            return;
        }
        self.poll_preview_cleanup();
        // Commands in this tick must not synchronously retire a dead preview.
        self.check_preview_failure();
        self.poll_capture_hide(el);
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
            if self.exit_if_requested(el) {
                return;
            }
        }
        let done = self.finished.lock().unwrap().take();
        if let Some((file, r)) = done {
            self.st.finishing = false;
            self.stop_recorder_socket();
            self.restore_after_recording(el);
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
        self.check_preview_failure();
        if self.st.refresh_devices {
            self.st.refresh_devices = false;
            load_devices(&mut self.st);
        }
        // G9: a device was plugged in or removed.
        if DEVICES_CHANGED.swap(false, Ordering::SeqCst) {
            self.devices_changed();
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
        // A camera scene whose camera never delivers: say so once instead of
        // showing a black preview (1B's portal path can fail late).
        if self.st.settings.scene.uses_cam()
            && !self.camera_warned
            && let Some(sess) = self.st.recording.as_ref().or(self.st.preview.as_ref())
            && camera_not_delivering(true, sess.cam_aspect().is_some(), sess.started.elapsed())
        {
            self.camera_warned = true;
            self.st.error = Some(
                "The camera is not delivering frames; check the camera (and the camera portal in Flatpak)."
                    .into(),
            );
        }
        if self.st.preview.is_none() && self.st.recording.is_none() {
            self.camera_warned = false;
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
        if let Some(watcher) = &mut self.tray_watcher {
            tray::poll_restart(
                &mut self.tray,
                &mut self.tray_rx,
                watcher,
                &view,
                Arc::new(push),
            );
        }
        if let Some(t) = &self.tray {
            if t.available() {
                tray::update(t, view);
            } else {
                // Retain the invalid handle so poll_restart can replace it
                // after a watcher returns and registration is confirmed.
                // A hidden app must not become unreachable when its host dies.
                // Don't undo the deliberate capture hide (that would film Rec),
                // or the explicit headless/background modes.
                if !self.main_visible
                    && !self.hidden_for_recording
                    && !background()
                    && std::env::var_os("REC_GUI_HIDDEN").is_none()
                {
                    self.show_main(el);
                }
            }
        }
        while let Ok(c) = self.recorder_rx.try_recv() {
            let cmd = match c {
                Control::Stop if self.st.recording.is_some() => Cmd::ToggleRecording,
                Control::Pause => Cmd::Pause,
                Control::Resume => Cmd::Resume,
                Control::SaveReplay => Cmd::SaveReplay,
                // App commands on the recorder socket: not ours to run here.
                _ => continue,
            };
            self.handle(el, cmd);
        }
        while let Ok(c) = self.control_rx.try_recv() {
            if self.exit_if_requested(el) {
                return;
            }
            let cmd = match c {
                Control::Show => Cmd::ShowWindow,
                Control::Settings => Cmd::ShowSettings,
                Control::StartRecording => {
                    if self.preview_cleanup.is_some() || self.pending_recording {
                        self.pending_recording = true;
                        continue;
                    }
                    if self.st.recording.is_none() {
                        Cmd::ToggleRecording
                    } else {
                        Cmd::ShowWindow
                    }
                }
                Control::ToggleRecording => Cmd::ToggleRecording,
                Control::Hide => Cmd::HideWindow,
                Control::Stop => {
                    self.pending_recording = false;
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
        // Process Stop/Quit already queued before fulfilling a deferred Start.
        if !self.exit_if_requested(el)
            && self.preview_cleanup.is_none()
            && std::mem::take(&mut self.pending_recording)
        {
            self.start_recording(el);
        }
    }

    fn redraw(&mut self, which: WindowId) {
        if self.quitting || SIG_QUIT.load(Ordering::SeqCst) || self.screen_locked {
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
            // The compositor's own texture first (G10: no readback, no
            // upload); the CPU frame is the fallback for a device that could
            // not be shared, and takes over again if the compositor is dropped.
            match self.tap.take() {
                Some(preview::Frame::Gpu(tex)) => {
                    let (pw, ph) = preview::target_size(
                        tex.width as usize,
                        tex.height as usize,
                        self.st.preview_target,
                        self.st.settings.preview_low_power,
                    );
                    self.st.preview_aspect = tex.width as f32 / tex.height.max(1) as f32;
                    self.st.preview_px = [pw as u32, ph as u32];
                    self.st.preview_frame = None;
                    self.st.frames_shown += 1;
                    if self.st.frames_shown == 1 && std::env::var_os("REC_DEBUG").is_some() {
                        // "preview frame" is what the self-test (and the audit)
                        // greps for; the texture path says where it came from.
                        eprintln!(
                            "rec: debug: preview frame {}x{} at {:.3}s (the compositor's texture)",
                            tex.width, tex.height, tex.pts
                        );
                    }
                    self.st.preview_tex = Some(Arc::new(tex));
                }
                Some(preview::Frame::Cpu(frame)) => {
                    // Drawn from the GPU in `ui::preview`; the size the UI shows it
                    // at is the low-power target (half resolution), the texture
                    // stays the frame's (the GPU scales without a CPU copy).
                    let (pw, ph) = preview::target_size(
                        frame.width,
                        frame.height,
                        self.st.preview_target,
                        self.st.settings.preview_low_power,
                    );
                    self.st.preview_aspect = frame.width as f32 / frame.height as f32;
                    self.st.preview_px = [pw as u32, ph as u32];
                    let pts = frame.pts;
                    let (fw, fh) = (frame.width, frame.height);
                    self.st.preview_frame = Some(Arc::new(frame));
                    self.st.preview_tex = None;
                    self.st.frames_shown += 1;
                    if self.st.frames_shown == 1 && std::env::var_os("REC_DEBUG").is_some() {
                        eprintln!("rec: debug: preview frame {fw}x{fh} at {pts:.3}s");
                    }
                }
                None => {}
            }
        }
        let win = if is_main {
            self.main.as_mut()
        } else {
            self.settings_win.as_mut()
        };
        let Some(win) = win else { return };
        // The signal may have arrived while preparing the preview. In winit's
        // X11 backend take_egui_input queries GetGeometry with an unwrap; don't
        // make that query on a window the desktop may already be tearing down.
        if SIG_QUIT.load(Ordering::SeqCst) {
            return;
        }
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
        if self.exit_if_requested(el) {
            return;
        }
        use winit::platform::wayland::ActiveEventLoopExtWayland;
        WAYLAND.store(el.is_wayland(), Ordering::Relaxed);
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
        // The async NameOwnerChanged listener also handles a shell/extension
        // that first appears after startup. No D-Bus work on the event loop.
        self.tray_watcher = tray::Watcher::new();
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
        if self.exit_if_requested(el) {
            return;
        }
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
        if self.exit_if_requested(el) {
            return;
        }
        if matches!(event, WindowEvent::Destroyed) {
            if let Some(hide) = &mut self.capture_hide {
                hide.destroyed(id);
            }
            // A desktop can destroy an X11 window independently of us. Never
            // forward this event (or later queued events) to egui for that XID.
            if self.main.as_ref().is_some_and(|w| w.window.id() == id) {
                self.main = None;
                self.main_visible = false;
                self.update_tap_wanted();
            }
            if self
                .settings_win
                .as_ref()
                .is_some_and(|w| w.window.id() == id)
            {
                self.settings_win = None;
            }
            return;
        }
        let is_main = self.main.as_ref().is_some_and(|w| w.window.id() == id);
        let win = if is_main {
            self.main.as_mut()
        } else {
            self.settings_win.as_mut()
        };
        let Some(win) = win.filter(|w| w.window.id() == id) else {
            return;
        };
        let resp = win.egui.on_window_event(&win.window, &event);
        match event {
            WindowEvent::CloseRequested => {
                if is_main {
                    // Close hides to the tray (quitting when nothing hosts it);
                    // Wayland destroys the window instead of hiding it.
                    self.close_or_hide(el);
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
        if self.exit_if_requested(el) {
            return;
        }
        if self.last_tick.elapsed() >= Duration::from_millis(100) {
            self.last_tick = Instant::now();
            self.tick(el);
        }
        if self.exit_if_requested(el) {
            return;
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
        self.capture_hide = None;
        self.st.preview_frame = None;
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
            // ashpd caches its connection process-wide. If this query wins
            // the first-connection race, later camera/screen portal calls must
            // still have an executor after the accent-fetch thread returns.
            static RT: std::sync::OnceLock<Option<tokio::runtime::Runtime>> =
                std::sync::OnceLock::new();
            let rt = RT.get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                    .ok()
            });
            let color = rt.as_ref().and_then(|rt| {
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

    /// Run alone under Xvfb. A pending SIGTERM must beat a queued redraw even
    /// if the desktop has already destroyed the XID. Also exercise quit while
    /// the asynchronous finalizer owns the recording, and commands after Quit.
    #[test]
    #[ignore = "requires X11, software Vulkan and an isolated session"]
    fn sigterm_preempts_destroyed_x11_redraw_and_waits_for_finalizer() {
        use winit::platform::x11::EventLoopBuilderExtX11;
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::ConnectionExt;

        struct Check {
            app: App,
            checked: bool,
        }
        impl ApplicationHandler for Check {
            fn resumed(&mut self, el: &ActiveEventLoop) {
                self.app.resumed(el);
                let win = self.app.main.as_ref().expect("main window");
                let id = win.window.id();
                let RawWindowHandle::Xlib(handle) = win.window.window_handle().unwrap().as_raw()
                else {
                    panic!("expected Xlib window");
                };
                let (conn, _) = x11rb::connect(None).unwrap();
                conn.destroy_window(handle.window as u32)
                    .unwrap()
                    .check()
                    .unwrap();
                conn.flush().unwrap();

                // Model a Stop already in progress. Quit must wait for this
                // worker, not just for st.recording (which is already None).
                self.app.st.finishing = true;
                let finished = self.app.finished.clone();
                let (finish_recording, wait_recording) = std::sync::mpsc::channel();
                let worker = std::thread::spawn(move || {
                    wait_recording.recv().unwrap();
                    *finished.lock().unwrap() =
                        Some((PathBuf::new(), Err("test finalizer".into())));
                });
                // A failed preview may still be releasing its capture. Start
                // is deferred; repeated explicit Start is idempotent, and quit
                // cancels that intent without blocking this event callback.
                let (release, wait) = std::sync::mpsc::channel();
                let preview = self.app.st.preview.take();
                let (preview_done, preview_completion) = std::sync::mpsc::channel();
                self.app.preview_cleanup = Some(std::thread::spawn(move || {
                    wait.recv().unwrap();
                    if let Some(s) = preview {
                        let _ = s.finish();
                    }
                    let _ = preview_done.send(());
                }));
                self.app.start_recording(el);
                self.app.start_recording(el);
                assert!(self.app.pending_recording);
                self.app.toggle_recording(el);
                assert!(!self.app.pending_recording);
                self.app.toggle_recording(el);
                assert!(self.app.pending_recording);
                self.app.st.error = None; // A dismissed banner cannot restart yet.
                self.app.sync_preview();
                assert!(self.app.st.preview.is_none());
                on_sigterm(libc::SIGTERM);
                self.app.window_event(el, id, WindowEvent::RedrawRequested);
                assert!(self.app.quitting);
                assert!(!self.app.pending_recording);
                assert!(self.app.preview_cleanup.is_none()); // detached, not joined
                assert!(self.app.st.finishing);
                assert!(!el.exiting()); // recording finalization is still mandatory
                let quit_started = Instant::now();
                finish_recording.send(()).unwrap();
                worker.join().unwrap();
                self.app.poll_quit(el);
                assert!(!self.app.st.finishing);
                assert!(el.exiting());
                assert!(quit_started.elapsed() < Duration::from_secs(1));
                // Keep the preview worker blocked beyond the old 2s timeout.
                // Exit has already been requested, without releasing that worker.
                assert_eq!(
                    preview_completion.recv_timeout(Duration::from_millis(2100)),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                );
                release.send(()).unwrap();
                preview_completion
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
                // Neither a command later in the batch nor late events may
                // recreate windows or enter egui after finalizing.
                self.app.handle(el, Cmd::ShowWindow);
                self.app.handle(el, Cmd::ShowSettings);
                self.app.user_event(el, ());
                self.app.about_to_wait(el);
                assert!(self.app.main.is_none());
                assert!(self.app.settings_win.is_none());
                assert!(SIG_QUIT.load(Ordering::SeqCst));
                self.checked = true;
            }
            fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
        }
        SIG_QUIT.store(false, Ordering::SeqCst);
        let (_tx, rx) = std::sync::mpsc::channel();
        let mut check = Check {
            app: App::new(false, false, None, true, rx, None),
            checked: false,
        };
        let el = EventLoop::<()>::with_user_event()
            .with_x11()
            .with_any_thread(true)
            .build()
            .unwrap();
        el.run_app(&mut check).unwrap();
        assert!(check.checked);
        SIG_QUIT.store(false, Ordering::SeqCst);
    }

    /// Run under headless sway with WAYLAND_DISPLAY set. Exercises the real
    /// winit deferred drop and the libwayland callback ABI, not presentation.
    #[test]
    #[ignore = "requires a running Wayland compositor"]
    fn capture_fence_on_winit_connection() {
        use winit::platform::wayland::EventLoopBuilderExtWayland;
        struct Check {
            hide: Option<CaptureHide>,
            acknowledged: bool,
        }
        impl ApplicationHandler for Check {
            fn resumed(&mut self, el: &ActiveEventLoop) {
                let window = el.create_window(Window::default_attributes()).unwrap();
                self.hide = Some(CaptureHide {
                    windows: vec![window.id()],
                    sync: None,
                    deadline: Instant::now() + Duration::from_secs(5),
                });
                drop(window);
            }
            fn window_event(&mut self, _: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
                if matches!(event, WindowEvent::Destroyed) {
                    self.hide.as_mut().unwrap().destroyed(id);
                }
            }
            fn about_to_wait(&mut self, el: &ActiveEventLoop) {
                let hide = self.hide.as_mut().unwrap();
                if hide
                    .ready_to_sync(Instant::now())
                    .expect("destruction/sync timed out")
                {
                    if hide.sync.is_none() {
                        hide.sync = Some(WaylandSync::new(el).unwrap());
                    }
                    if hide.sync.as_mut().unwrap().poll().unwrap() {
                        self.acknowledged = true;
                        self.hide = None;
                        el.exit();
                        return;
                    }
                }
                el.set_control_flow(ControlFlow::WaitUntil(
                    Instant::now() + Duration::from_millis(10),
                ));
            }
        }
        let el = EventLoop::builder()
            .with_wayland()
            .with_any_thread(true)
            .build()
            .unwrap();
        let mut check = Check {
            hide: None,
            acknowledged: false,
        };
        el.run_app(&mut check).unwrap();
        assert!(check.acknowledged);
    }

    #[test]
    fn capture_fence_waits_for_both_destroyed_windows() {
        let now = Instant::now();
        let main = WindowId::from(1);
        let settings = WindowId::from(2);
        let mut hide = CaptureHide {
            windows: vec![main, settings],
            sync: None,
            deadline: now + Duration::from_secs(5),
        };
        assert!(!hide.ready_to_sync(now).unwrap());
        hide.destroyed(WindowId::from(3));
        hide.destroyed(main);
        assert!(!hide.ready_to_sync(now).unwrap());
        hide.destroyed(main); // duplicate events cannot advance the fence
        assert!(!hide.ready_to_sync(now).unwrap());
        hide.destroyed(settings);
        assert!(hide.ready_to_sync(now).unwrap());
        assert!(hide.sync.is_none()); // destruction alone is NOT an acknowledgement
    }

    #[test]
    fn capture_fence_times_out_even_after_destruction() {
        let now = Instant::now();
        let mut hide = CaptureHide {
            windows: vec![WindowId::from(1)],
            sync: None,
            deadline: now,
        };
        assert!(hide.ready_to_sync(now).is_err());
        hide.destroyed(WindowId::from(1));
        assert!(hide.ready_to_sync(now).is_err());
    }

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
    fn the_window_is_hidden_only_where_the_capture_cannot_exclude_it() {
        // G3: on the portal a display or region capture would film Rec's own
        // window (hide it); on X11 the capture paints it out (do not hide). A
        // single-window capture cannot contain it, and the setting can turn the
        // hide off.
        use crate::capture::screen::Backend;
        let mut settings = Settings::default();
        assert!(settings.hide_window_when_recording, "on by default");
        assert!(hides_window_for_recording(&settings, true, false), "portal");
        assert!(
            !hides_window_for_recording(&settings, true, true),
            "X11 excludes our windows itself"
        );
        assert!(
            !hides_window_for_recording(&settings, false, false),
            "already hidden"
        );
        settings.window = Some(42);
        assert!(
            !hides_window_for_recording(&settings, true, false),
            "a window capture cannot contain our window"
        );
        settings.window = None;
        settings.hide_window_when_recording = false;
        assert!(!hides_window_for_recording(&settings, true, false));

        // The backend decides which path is taken.
        assert!(exclusion_is_done_by_capture(Ok(Backend::X11)));
        assert!(!exclusion_is_done_by_capture(Ok(Backend::Portal)));
        assert!(!exclusion_is_done_by_capture(Err(anyhow::anyhow!(
            "no session"
        ))));
    }

    #[test]
    fn own_window_ids_come_from_x11_handles_only() {
        // G3: the ids the app hands to `set_excluded_windows` are the X11 ones;
        // a Wayland window has none (its capture cannot exclude anything).
        use winit::raw_window_handle::{WaylandWindowHandle, XcbWindowHandle, XlibWindowHandle};
        let xlib = RawWindowHandle::Xlib(XlibWindowHandle::new(0x2a));
        assert_eq!(x11_window_id(&xlib), Some(0x2a));
        let xcb = RawWindowHandle::Xcb(XcbWindowHandle::new(
            std::num::NonZeroU32::new(0x2a).unwrap(),
        ));
        assert_eq!(x11_window_id(&xcb), Some(0x2a));
        let wayland =
            RawWindowHandle::Wayland(WaylandWindowHandle::new(std::ptr::NonNull::dangling()));
        assert_eq!(x11_window_id(&wayland), None);
    }

    #[test]
    fn closing_hides_where_it_can_and_quits_without_a_tray() {
        // Wayland cannot hide a window (winit: set_visible is unsupported), so
        // closing destroys it and the app keeps running; X11 hides in place; and
        // with no tray host closing must quit, or nothing could show the window
        // again.
        assert_eq!(hide_action(true, true), HideAction::DestroyWindow);
        assert_eq!(hide_action(false, true), HideAction::SetInvisible);
        assert_eq!(hide_action(true, false), HideAction::Quit);
        assert_eq!(hide_action(false, false), HideAction::Quit);
    }

    #[test]
    fn the_first_close_explains_itself_once() {
        // A quit needs no explanation; the first hide does, later ones don't.
        assert!(should_notify_hidden(false, HideAction::DestroyWindow));
        assert!(should_notify_hidden(false, HideAction::SetInvisible));
        assert!(!should_notify_hidden(true, HideAction::DestroyWindow));
        assert!(!should_notify_hidden(true, HideAction::SetInvisible));
        assert!(!should_notify_hidden(false, HideAction::Quit));
        // The flag is remembered in settings.json.
        let s = Settings {
            notifies_when_hidden: true,
            ..Default::default()
        };
        let back = crate::settings::Settings::from_json(&serde_json::to_string(&s).unwrap())
            .expect("valid json");
        assert!(back.notifies_when_hidden);
        assert!(!Settings::default().notifies_when_hidden);
    }

    #[test]
    fn a_camera_scene_without_frames_says_so() {
        // A camera that never delivers would otherwise leave a black preview
        // (1B's portal path can fail after the session started).
        assert!(camera_not_delivering(true, false, Duration::from_secs(6)));
        assert!(!camera_not_delivering(true, false, Duration::from_secs(4)));
        assert!(!camera_not_delivering(true, true, Duration::from_secs(30)));
        assert!(!camera_not_delivering(
            false,
            false,
            Duration::from_secs(30)
        ));
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
