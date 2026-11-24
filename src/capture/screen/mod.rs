//! Screen and window capture.
//!
//! Two backends, one contract:
//!
//! | Desktop | Backend | Why |
//! |---|---|---|
//! | X11 | [`x11`], direct | honours `--display N`, `--window <substring>` and `--region` exactly, never prompts |
//! | Wayland (and anything else) | [`portal`] + [`pipewire`] | the only way to see the screen: xdg-desktop-portal's ScreenCast, with a persisted grant |
//!
//! `REC_SCREEN_BACKEND=portal|x11` forces one, which the headless self-test uses
//! (and which a user on XWayland may want: the X11 path cannot see native
//! Wayland clients).
//!
//! # The size rule
//!
//! A source delivers **native pixels**: a monitor's whole framebuffer, a
//! window's own size, or the `--region` crop of one. Nothing is scaled here —
//! upstream captures native frames and scales once in the compositor, and phase
//! 1C's renderer does the same on the GPU. `resolve()` therefore has to report
//! the native size before the writer is built, which on Wayland means asking the
//! portal (see [`resolve`]).
//!
//! # Frames
//!
//! Frames arrive on the host clock (`CLOCK_MONOTONIC`) and go out through the
//! session's [`FrameSink`](crate::media_types::FrameSink). Both backends drop
//! frames rather than queue them when the writer falls behind: the newest frame
//! is what matters, and the writer repeats the last one on its own grid.
//!
//! Adapted from rec-rs src/screen.rs (MIT OR Apache-2.0): displays, windows,
//! `--region`, the cursor, click highlighting and the "largest window wins"
//! selector are ported; ScreenCaptureKit is replaced by the portal and X11.

mod cursor;
mod format;
mod pipewire;
mod portal;
mod selector;
mod token;
pub mod x11;

use std::cell::RefCell;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Result, bail};

use crate::media_types::{DisplayInfo, FrameSink, VideoSource, WindowInfo};

/// A rectangle of a display, in the display's logical units (points) from its
/// top-left corner.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Region {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// What `record` asked for on the command line.
#[derive(Clone, Debug, PartialEq)]
pub struct ScreenRequest {
    /// 1-based display number (`--display`).
    pub display: usize,
    /// Window id or an app/title substring (`--window`).
    pub window: Option<String>,
    /// Region of the display (`--region`), which excludes `window`.
    pub region: Option<Region>,
}

impl Default for ScreenRequest {
    /// The primary display, like the command line's `--display 1`.
    fn default() -> ScreenRequest {
        ScreenRequest {
            display: 1,
            window: None,
            region: None,
        }
    }
}

/// How the session wants the capture to look.
///
/// The defaults are upstream's: the cursor is drawn, clicks are not
/// highlighted. `record.rs` fills these from `--no-cursor` / `--clicks`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureOptions {
    pub cursor: bool,
    pub clicks: bool,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        CaptureOptions {
            cursor: true,
            clicks: false,
        }
    }
}

/// Whether the user asked for a display (optionally a region) or a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Want {
    Monitor,
    Window,
}

/// What is being captured, once resolved.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// A whole display, or a region of it.
    Display(DisplayInfo, Option<Region>),
    /// A single window.
    Window(WindowInfo),
    /// A Wayland session: the system picker decided, and there is no
    /// enumerable id to name. `window` says what it picked.
    Picked {
        window: bool,
        pixels: (usize, usize),
    },
}

impl Target {
    /// One line for the start log.
    ///
    /// The session logs `VideoSource::describe` instead; the app's source list
    /// (phase 1E) is what shows this one.
    #[allow(dead_code)]
    pub fn describe(&self) -> String {
        match self {
            Target::Display(d, None) => {
                format!("display {} ({}x{} pixels)", d.index, d.pixels.0, d.pixels.1)
            }
            Target::Display(d, Some(r)) => {
                format!(
                    "display {} region {}x{} at {},{}",
                    d.index, r.width, r.height, r.x, r.y
                )
            }
            Target::Window(w) => {
                format!("window {:?} ({} — {}x{})", w.id, w.app, w.size.0, w.size.1)
            }
            Target::Picked { window, pixels } => format!(
                "the {} the system picker chose ({}x{} pixels)",
                if *window { "window" } else { "monitor" },
                pixels.0,
                pixels.1
            ),
        }
    }
}

/// The displays to list in `rec-rs devices` (1 = main).
///
/// X11 (including XWayland, which every major Wayland desktop runs) exposes the
/// monitor layout through RandR. A pure-Wayland session without XWayland has no
/// way to enumerate monitors outside the portal, so listing them would mean
/// running the picker: `devices` says so instead.
pub fn displays() -> Result<Vec<DisplayInfo>> {
    match session() {
        Session::X11 | Session::Wayland => match x11::Server::connect() {
            Ok(server) => Ok(server.displays()),
            Err(e) if session() == Session::Wayland => Err(anyhow::anyhow!(
                "no monitor list on this Wayland session ({e}). The system picker chooses what \
                 is shared when a recording starts, and `rec-rs permissions` shows what was \
                 granted"
            )),
            Err(e) => Err(e),
        },
        Session::Unknown => Err(anyhow::anyhow!(
            "no display server: set WAYLAND_DISPLAY (portal capture) or DISPLAY (X11 capture)"
        )),
    }
}

/// The windows `--window` can select.
pub fn windows() -> Result<Vec<WindowInfo>> {
    let server = x11::Server::connect().map_err(|e| {
        if session() == Session::Wayland {
            anyhow::anyhow!(
                "Wayland: the system picker chooses the window. Set DISPLAY to list the X11 \
                 (XWayland) windows, or pass --window with a hint for the picker ({e})"
            )
        } else {
            e
        }
    })?;
    let ws = server.windows();
    if ws.is_empty() {
        bail!("no on-screen window was found (nothing is mapped, or no window manager is running)");
    }
    Ok(ws)
}

/// Which windowing system this session runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Session {
    X11,
    Wayland,
    Unknown,
}

pub fn session() -> Session {
    let non_empty = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    if non_empty("WAYLAND_DISPLAY") {
        return Session::Wayland;
    }
    if non_empty("DISPLAY") {
        return Session::X11;
    }
    match std::env::var("XDG_SESSION_TYPE") {
        Ok(t) if t.eq_ignore_ascii_case("wayland") => Session::Wayland,
        Ok(t) if t.eq_ignore_ascii_case("x11") => Session::X11,
        _ => Session::Unknown,
    }
}

/// The capture backend to use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    X11,
    Portal,
}

/// `REC_SCREEN_BACKEND` picks a backend; otherwise X11 sessions capture
/// directly and everything else goes through the portal.
pub fn backend() -> Result<Backend> {
    match std::env::var("REC_SCREEN_BACKEND") {
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "" => Ok(default_backend()),
            "x11" => Ok(Backend::X11),
            "portal" | "pipewire" => Ok(Backend::Portal),
            other => bail!(
                "unknown REC_SCREEN_BACKEND {other:?}: use portal (xdg-desktop-portal + \
                 PipeWire) or x11 (direct)"
            ),
        },
        Err(_) => Ok(default_backend()),
    }
}

fn default_backend() -> Backend {
    match session() {
        Session::X11 => Backend::X11,
        Session::Wayland | Session::Unknown => Backend::Portal,
    }
}

/// How long the portal dialog may stay unanswered (`REC_PORTAL_TIMEOUT`).
fn portal_timeout() -> Duration {
    let secs = std::env::var("REC_PORTAL_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|s| *s > 0.0)
        .unwrap_or(120.0);
    Duration::from_secs_f64(secs)
}

/// Sticky, cloneable cancellation for app capture startup. Cancelling any clone
/// wakes the pending portal picker; it does not stop an already returned source.
#[derive(Clone, Debug)]
pub struct PortalCancel(tokio::sync::watch::Sender<bool>);

impl Default for PortalCancel {
    fn default() -> Self {
        Self(tokio::sync::watch::channel(false).0)
    }
}

impl PortalCancel {
    #[allow(dead_code)] // Phase 1E app startup consumes this cancellation seam.
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }

    fn check(&self) -> Result<()> {
        if *self.0.borrow() {
            bail!("screen capture startup cancelled");
        }
        Ok(())
    }

    async fn cancelled(&self) {
        let mut receiver = self.0.subscribe();
        loop {
            if *receiver.borrow_and_update() {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

thread_local! {
    /// App policy and handoff belong to the synchronous startup thread, not the
    /// process: previews and CLI capture must never steal the app's session.
    static APP_PORTAL: RefCell<Option<AppPortal>> = const { RefCell::new(None) };
}

struct AppPortal {
    cancel: PortalCancel,
    pending: Option<Pending>,
}

/// Run the app's entire synchronous recording startup on one blocking thread.
///
/// ```ignore
/// screen::with_app_portal(&cancel, || {
///     let options = options()?; // includes cli::prepare -> screen::resolve
///     record::Session::start(options, /* ... */)
/// })
/// ```
/// Both options preparation and `Session::start` MUST run inside this closure,
/// on the same dedicated thread (not a Tokio runtime worker). The portal picker
/// has no deadline here; cancelling any clone of `cancel` closes its Request.
/// PipeWire probing remains bounded and cancellation is checked after it.
/// A successful startup value is always returned, even if cancellation raced
/// with startup completion. The caller must observe its cancellation state and
/// explicitly stop/finalize that value if needed: generic `T` may own a running
/// recording that cannot be cleaned up by dropping it.
///
/// The resolve/open handoff is owned by this scope, so a failed startup or panic
/// closes any unused session on the portal runtime. Nested calls restore their
/// caller's policy. Other threads and CLI factories keep their normal timeout
/// and pending-session behaviour. No environment variables are changed.
#[allow(dead_code)] // Phase 1E app startup consumes this scoped seam.
pub fn with_app_portal<T>(cancel: &PortalCancel, startup: impl FnOnce() -> Result<T>) -> Result<T> {
    struct Restore(Option<AppPortal>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let current = APP_PORTAL.with(|slot| slot.replace(self.0.take()));
            // Destructors can enter the portal runtime; never hold a TLS borrow.
            drop(current);
        }
    }
    cancel.check()?;
    let _restore = Restore(APP_PORTAL.with(|slot| {
        slot.replace(Some(AppPortal {
            cancel: cancel.clone(),
            pending: None,
        }))
    }));
    // Once startup succeeds, only the caller knows how to stop/finalize T.
    // Never turn late cancellation into an error that discards that ownership.
    startup()
}

fn app_cancel() -> Option<PortalCancel> {
    APP_PORTAL.with(|slot| slot.borrow().as_ref().map(|app| app.cancel.clone()))
}

fn check_startup() -> Result<()> {
    if let Some(cancel) = app_cancel() {
        cancel.check()?;
    }
    Ok(())
}

fn prepare_startup(want: Want, opts: CaptureOptions) -> Result<portal::Prepared> {
    match app_cancel() {
        Some(cancel) => portal::prepare_with(want, opts, None, &cancel),
        None => portal::prepare(want, opts, portal_timeout()),
    }
}

/// App-owned startup result: no global pending-session cache is involved.
/// Dropping this value closes its portal session safely on the portal runtime.
#[allow(dead_code)] // Phase 1E app startup consumes this owned-session seam.
pub struct PreparedScreen {
    pub target: Target,
    pub native: (usize, usize),
    request: ScreenRequest,
    opts: CaptureOptions,
    portal: Option<portal::Prepared>,
}

#[allow(dead_code)] // Phase 1E app startup consumes this owned-session seam.
impl PreparedScreen {
    /// Consume the exact session that was prepared, without another picker.
    pub fn open(self) -> Result<(Box<dyn VideoSource>, Option<ScreenError>)> {
        if let Some(prepared) = self.portal {
            let source = PortalSource::from_prepared(&self.request, self.opts, prepared)?;
            let error = source.stream.error_handle();
            Ok((Box::new(source), Some(error)))
        } else {
            open_with_status(&self.request, &self.opts)
        }
    }
}

/// Blocking app startup seam; call on a dedicated worker / `spawn_blocking`,
/// never an async runtime worker. `portal_wait: None` allows an unanswered
/// picker to remain open indefinitely. CLI factories retain REC_PORTAL_TIMEOUT
/// (120 s by default). Cancellation is sticky and closes the pending Request.
/// PipeWire probing is bounded but not interruptible; cancellation during it
/// is observed before returning. No shared media contract or CLI flags change.
#[allow(dead_code)] // Phase 1E app startup consumes this blocking worker seam.
pub fn prepare_for_app(
    req: &ScreenRequest,
    opts: &CaptureOptions,
    portal_wait: Option<Duration>,
    cancel: &PortalCancel,
) -> Result<PreparedScreen> {
    cancel.check()?;
    let (target, native, portal) = match backend()? {
        Backend::X11 => {
            let (target, native) = resolve_x11(req)?;
            (target, native, None)
        }
        Backend::Portal => {
            let want = if req.window.is_some() {
                Want::Window
            } else {
                Want::Monitor
            };
            let prepared = portal::prepare_with(want, *opts, portal_wait, cancel)?;
            let (target, native) = target_from_portal(req, &prepared)?;
            (target, native, Some(prepared))
        }
    };
    cancel.check()?;
    Ok(PreparedScreen {
        target,
        native,
        request: req.clone(),
        opts: *opts,
        portal,
    })
}

/// A portal session prepared by [`resolve`], waiting for [`open`].
///
/// On Wayland the pixel size is only known after the portal has started a
/// session, so `resolve` (which needs that size) runs the handshake and hands
/// the started session to the capture that follows. On X11 nothing is cached:
/// RandR knows the sizes and the picker is not involved.
struct Pending {
    request: ScreenRequest,
    opts: CaptureOptions,
    prepared: portal::Prepared,
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);

/// Resolve a request to the target and its native pixel size, which `prepare`
/// needs to compute the output size.
///
/// On Wayland this runs the ScreenCast handshake, because the pixel size of the
/// stream the compositor will send is not knowable any other way. A restore
/// token from an earlier run (or from `permissions --request`) makes that
/// silent; the first run shows the picker, which is also the first run's
/// permission prompt.
pub fn resolve(req: &ScreenRequest) -> Result<(Target, (usize, usize))> {
    check_startup()?;
    match backend()? {
        Backend::X11 => resolve_x11(req),
        Backend::Portal => resolve_portal(req),
    }
}

fn resolve_x11(req: &ScreenRequest) -> Result<(Target, (usize, usize))> {
    let server = x11::Server::connect()?;
    if let Some(sel) = &req.window {
        let (_, w) = x11::resolve_window(&server, sel)?;
        let pixels = (w.size.0 & !1, w.size.1 & !1);
        return Ok((Target::Window(w), pixels));
    }
    let (spec, d) = x11::resolve_display(&server, req.display, req.region.as_ref())?;
    let native = (spec.rect.width, spec.rect.height);
    Ok((Target::Display(d, req.region), native))
}

fn resolve_portal(req: &ScreenRequest) -> Result<(Target, (usize, usize))> {
    let want = if req.window.is_some() {
        Want::Window
    } else {
        Want::Monitor
    };
    if req.window.is_some() {
        eprintln!(
            "rec: note: on Wayland the system picker chooses the window, so --window {:?} is only \
             a hint; `rec-rs devices` lists the windows on an X11 session",
            req.window.as_deref().unwrap_or("")
        );
    }
    // XWayland usually knows the monitor layout, which is enough to reject a
    // display number that cannot exist (upstream's message) even though the
    // picker still makes the choice.
    let known = x11::Server::connect()
        .map(|s| s.monitors())
        .unwrap_or_default();
    if !known.is_empty() && (req.display == 0 || req.display > known.len()) {
        bail!(
            "display {} not found: displays are numbered 1..={} (see `rec-rs devices`)",
            req.display,
            known.len()
        );
    }
    if req.display > 1 && !known.is_empty() {
        eprintln!(
            "rec: note: on Wayland the system picker chooses which monitor is shared, so \
             --display {} is not enforced (display {} is {}x{}, but the picker decides)",
            req.display,
            req.display,
            known[req.display - 1].0.pixels.0,
            known[req.display - 1].0.pixels.1
        );
    } else if req.display > 1 {
        eprintln!(
            "rec: note: on Wayland the system picker chooses which monitor is shared, so \
             --display {} is not enforced",
            req.display
        );
    }
    // `--no-cursor`/`--clicks` are not known here (the CLI resolves the size
    // before it builds the writer), so this session uses upstream's defaults.
    // `open_with` reuses it when the options match and otherwise asks again;
    // the restore token makes that second handshake silent.
    let prepared = prepare_startup(want, CaptureOptions::default())?;
    let (target, native) = target_from_portal(req, &prepared)?;
    let new = Pending {
        request: req.clone(),
        opts: CaptureOptions::default(),
        prepared,
    };
    let old = APP_PORTAL.with(|slot| {
        let mut slot = slot.borrow_mut();
        match slot.as_mut() {
            Some(app) => app.pending.replace(new),
            None => PENDING.lock().unwrap().replace(new),
        }
    });
    drop(old);
    Ok((target, native))
}

/// The target and native size a started portal session implies.
fn target_from_portal(
    req: &ScreenRequest,
    prepared: &portal::Prepared,
) -> Result<(Target, (usize, usize))> {
    match prepared.want {
        Want::Window => {
            // A window has no size of its own to check against: the stream's
            // size is what the compositor sends.
            let pixels = (prepared.pixels.0 & !1, prepared.pixels.1 & !1);
            Ok((
                Target::Picked {
                    window: true,
                    pixels,
                },
                pixels,
            ))
        }
        Want::Monitor => {
            let d = match x11::Server::connect() {
                Ok(server) => server
                    .monitors()
                    .into_iter()
                    .map(|(d, _)| d)
                    .find(|d| d.points == prepared.points || d.pixels == prepared.pixels)
                    .unwrap_or_else(|| portal_display(prepared)),
                Err(_) => portal_display(prepared),
            };
            let native = match &req.region {
                Some(r) => {
                    let Some(rect) = format::region_to_pixels(r, prepared.pixels, prepared.points)
                    else {
                        bail!(
                            "--region {}x{}+{}+{} is outside the shared display ({}x{} pixels)",
                            r.width,
                            r.height,
                            r.x,
                            r.y,
                            prepared.pixels.0,
                            prepared.pixels.1
                        );
                    };
                    (rect.width, rect.height)
                }
                None => (d.pixels.0 & !1, d.pixels.1 & !1),
            };
            Ok((Target::Display(d, req.region), native))
        }
    }
}

/// A `DisplayInfo` for a portal stream no monitor list could name.
fn portal_display(prepared: &portal::Prepared) -> DisplayInfo {
    DisplayInfo {
        index: 1,
        id: "portal".to_string(),
        name: "the shared display".to_string(),
        pixels: prepared.pixels,
        points: prepared.points,
        primary: true,
    }
}

/// Start capturing `req` with the upstream defaults (cursor visible, no click
/// highlight). The returned source is stopped; the session calls
/// [`VideoSource::start`] with its frame sink.
// The app's preview and the tests use this; `record` passes the real options.
#[allow(dead_code)]
pub fn open(req: &ScreenRequest) -> Result<Box<dyn VideoSource>> {
    open_with(req, &CaptureOptions::default())
}

/// Start capturing `req` with `opts`.
pub fn open_with(req: &ScreenRequest, opts: &CaptureOptions) -> Result<Box<dyn VideoSource>> {
    Ok(open_with_status(req, opts)?.0)
}

/// A screen-specific failure handle lets the recording observe fatal portal
/// loss without changing the shared frame/audio contract.
pub type ScreenError = std::sync::Arc<Mutex<Option<String>>>;
pub fn open_with_status(
    req: &ScreenRequest,
    opts: &CaptureOptions,
) -> Result<(Box<dyn VideoSource>, Option<ScreenError>)> {
    check_startup()?;
    match backend()? {
        Backend::X11 => {
            let server = x11::Server::connect()?;
            let spec = match &req.window {
                Some(sel) => x11::resolve_window(&server, sel)?.0,
                None => x11::resolve_display(&server, req.display, req.region.as_ref())?.0,
            };
            Ok((Box::new(x11::Source::new(spec, *opts)), None))
        }
        Backend::Portal => {
            let source = PortalSource::open(req, *opts)?;
            let error = source.stream.error_handle();
            Ok((Box::new(source), Some(error)))
        }
    }
}

/// A Wayland capture: the portal session plus the PipeWire stream.
struct PortalSource {
    /// The portal session; closing it stops the compositor capturing.
    prepared: Option<portal::Prepared>,
    stream: pipewire::Stream,
    description: String,
}

impl PortalSource {
    fn open(req: &ScreenRequest, opts: CaptureOptions) -> Result<PortalSource> {
        Self::from_prepared(req, opts, take_pending(req, opts)?)
    }

    fn from_prepared(
        req: &ScreenRequest,
        opts: CaptureOptions,
        prepared: portal::Prepared,
    ) -> Result<Self> {
        if let Some(note) = &prepared.cursor_note {
            warn_once(note);
        }
        if !excluded_windows().is_empty() {
            // Upstream's content filter leaves its own windows out; a portal
            // client cannot, so say it where a recording's log will show it and
            // let the app hide itself (phase 1E).
            warn_once(
                "a screen-cast portal cannot leave rec's own windows out of the stream, so the \
                 app hides its windows while recording (the X11 backend paints them out instead)",
            );
        }
        // `--region` is a display crop; a window stream is already the window.
        let region = match prepared.want {
            Want::Monitor => req.region.as_ref(),
            Want::Window => None,
        };
        let crop = match region {
            Some(r) => {
                let Some(rect) = format::region_to_pixels(r, prepared.pixels, prepared.points)
                else {
                    bail!(
                        "--region {}x{}+{}+{} is outside the shared display ({}x{} pixels)",
                        r.width,
                        r.height,
                        r.x,
                        r.y,
                        prepared.pixels.0,
                        prepared.pixels.1
                    );
                };
                Some(rect)
            }
            None => None,
        };
        let description = match &crop {
            Some(rect) => format!(
                "{} cropped to {}x{} pixels",
                prepared.picked, rect.width, rect.height
            ),
            None => prepared.picked.clone(),
        };
        let fd = prepared.dup_fd()?;
        let stream = pipewire::Stream::new(prepared.node_id, fd, crop, opts, description.clone());
        Ok(PortalSource {
            prepared: Some(prepared),
            stream,
            description,
        })
    }
}

/// The pending session for `req` and `opts`, or a fresh handshake.
fn take_pending(req: &ScreenRequest, opts: CaptureOptions) -> Result<portal::Prepared> {
    check_startup()?;
    let pending = APP_PORTAL.with(|slot| {
        let mut slot = slot.borrow_mut();
        match slot.as_mut() {
            Some(app) => app.pending.take(),
            None => PENDING.lock().unwrap().take(),
        }
    });
    if let Some(p) = pending {
        if p.request == *req && p.opts == opts {
            return Ok(p.prepared);
        }
        p.prepared.close();
    }
    let want = if req.window.is_some() {
        Want::Window
    } else {
        Want::Monitor
    };
    prepare_startup(want, opts)
}

impl VideoSource for PortalSource {
    fn describe(&self) -> String {
        self.description.clone()
    }

    fn start(&mut self, sink: FrameSink) -> Result<()> {
        VideoSource::start(&mut self.stream, sink)
    }

    fn stop(&mut self) -> Result<()> {
        let r = VideoSource::stop(&mut self.stream);
        if let Some(p) = self.prepared.take() {
            p.close();
        }
        r
    }

    fn is_running(&self) -> bool {
        VideoSource::is_running(&self.stream)
    }
}

/// Windows that belong to the recorder itself, left out of the capture.
///
/// The app (phase 1E) hands over its own top-level window ids when it starts a
/// recording, so a recording does not contain its own live preview — the job
/// upstream gives ScreenCaptureKit's content filter (`README.md:128-129`).
///
/// On X11 the capture paints their visible area out of every frame. A portal
/// stream cannot exclude windows at all (the compositor owns it), so the
/// capture says so and the app has to hide its windows instead.
static EXCLUDED: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Leave these windows (X11 window ids) out of the capture.
///
/// Call it with an empty list to restore the default (capture everything).
/// Only the X11 backend can honour it; see [`excluded_windows`] for why. The
/// app (phase 1E) calls it when a recording starts and clears it when one ends.
#[allow(dead_code)] // the app, phase 1E
pub fn set_excluded_windows(ids: Vec<u32>) {
    *EXCLUDED.lock().unwrap() = ids;
}

/// The windows [`set_excluded_windows`] was given.
///
/// `REC_EXCLUDE_WINDOWS=id,id` overrides it, so a script can prove the
/// exclusion without an app to call the API (the self-test does).
pub(crate) fn excluded_windows() -> Vec<u32> {
    if let Ok(list) = std::env::var("REC_EXCLUDE_WINDOWS") {
        let ids: Vec<u32> = list
            .split(',')
            .filter_map(|s| s.trim().parse::<u32>().ok())
            .collect();
        if !ids.is_empty() {
            return ids;
        }
    }
    EXCLUDED.lock().unwrap().clone()
}

/// Serialises the tests that touch the process-wide exclusion list.
#[cfg(test)]
pub(crate) static EXCLUSION_TEST_LOCK: Mutex<()> = Mutex::new(());

/// `rec-rs permissions` screen line, and with `request` the portal dialog.
pub fn permission_status(request: bool, wait: u64) -> Result<String> {
    if request {
        request_permission(wait)?;
    }
    Ok(status_line())
}

/// Run the ScreenCast dialog once and keep the grant.
///
/// `wait` bounds the dialog. What the picker chose goes to stderr; the status
/// line `permissions` prints is the answer on stdout.
///
/// An existing grant is forgotten first, so the dialog really appears: upstream
/// clears its Screen Recording decision before prompting for the same reason
/// (`tcc.rs:183-230`), because a system that remembers answers silently. The
/// Linux grant is the portal's restore token, so removing it is the whole
/// equivalent of `tccutil reset`.
pub fn request_permission(wait: u64) -> Result<()> {
    let timeout = Duration::from_secs(wait.max(5));
    let saved = token::path();
    if saved.exists() {
        match token::clear() {
            Ok(()) => {
                eprintln!("rec: forgetting the saved screen-capture grant so the system asks again")
            }
            // Keep going: the worst case is the silent answer this flag exists
            // to avoid, and the portal may still prompt for another reason.
            Err(e) => eprintln!(
                "rec: warning: could not forget the saved screen-capture grant ({}): {e}",
                saved.display()
            ),
        }
    }
    let prepared = portal::prepare(Want::Monitor, CaptureOptions::default(), timeout)?;
    eprintln!("rec: screen-capture grant for {}", prepared.picked);
    prepared.close();
    Ok(())
}

fn status_line() -> String {
    let grant = token::load();
    let portal = portal::available();
    let pipewire = if pipewire_running() {
        "PipeWire running"
    } else {
        "PipeWire NOT running"
    };
    let Ok(a) = portal else {
        let reason = match portal.err() {
            Some(e) => format!("{e:#}"),
            None => "the screen-cast portal is unavailable".to_string(),
        };
        return format!("unavailable - {reason}; {pipewire}");
    };
    let mut bits = vec![format!("ScreenCast v{}", a.version)];
    let mut what = vec![];
    if a.monitors {
        what.push("monitors");
    }
    if a.windows {
        what.push("windows");
    }
    if !what.is_empty() {
        bits.push(what.join(" + "));
    }
    if !a.cursor_modes.is_empty() {
        bits.push(format!("cursor {}", a.cursor_modes.join("/")));
    }
    bits.push(pipewire.to_string());
    let what = bits.join("; ");
    match grant {
        Some(_) => format!(
            "granted - {what}; a saved grant means the picker is skipped (remove {} to ask again)",
            token::path().display()
        ),
        None => format!(
            "not granted yet - {what}; the picker appears once, at the first recording \
             (or run `rec-rs permissions --request`)"
        ),
    }
}

/// Is a PipeWire daemon socket there? The portal needs one, and a missing
/// socket is the usual reason a screen-cast stream never starts.
fn pipewire_running() -> bool {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        if std::path::Path::new(&dir).join("pipewire-0").exists() {
            return true;
        }
    }
    // PipeWire also exports a pulse-compatible socket; either proves the
    // daemon is up.
    std::path::Path::new("/run/user")
        .join(unsafe { libc::getuid() }.to_string())
        .join("pipewire-0")
        .exists()
}

/// `REC_DEBUG` diagnostics.
pub(crate) fn debug() -> bool {
    std::env::var_os("REC_DEBUG").is_some()
}

/// Print `message` once per process.
pub(crate) fn warn_once(message: &str) {
    static WARNED: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let mut seen = WARNED.lock().unwrap();
    if seen.iter().any(|m| m == message) {
        return;
    }
    seen.push(message.to_string());
    eprintln!("rec: warning: {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_policy_is_scoped_nested_and_thread_local() {
        let outer = PortalCancel::default();
        with_app_portal(&outer, || {
            assert!(app_cancel().is_some());
            assert!(
                std::thread::spawn(|| app_cancel().is_none())
                    .join()
                    .unwrap()
            );
            let inner = PortalCancel::default();
            with_app_portal(&inner, || {
                inner.cancel();
                assert!(check_startup().is_err());
                check_startup()
            })
            .unwrap_err();
            assert!(check_startup().is_ok(), "outer policy restored");
            Ok(())
        })
        .unwrap();
        assert!(app_cancel().is_none());
    }

    #[test]
    fn successful_startup_retains_ownership_when_cancelled_before_return() {
        // Like record::Session, this resource needs explicit cleanup, not Drop.
        struct Recording(std::rc::Rc<std::cell::Cell<bool>>);
        impl Recording {
            fn stop(self) {
                self.0.set(true);
            }
        }

        let stopped = std::rc::Rc::new(std::cell::Cell::new(false));
        let cancel = PortalCancel::default();
        let worker_cancel = cancel.clone();
        let recording = with_app_portal(&worker_cancel, || {
            let recording = Recording(stopped.clone());
            cancel.cancel(); // Cancellation races with the end of Session::start.
            Ok(recording)
        })
        .expect("successful startup must transfer ownership even after cancellation");

        assert!(app_cancel().is_none(), "startup scope restored");
        assert!(cancel.check().is_err(), "caller can observe cancellation");
        assert!(!stopped.get());
        recording.stop();
        assert!(
            stopped.get(),
            "caller can explicitly finalize the recording"
        );
    }

    #[test]
    fn app_policy_restores_after_error_and_panic() {
        let cancel = PortalCancel::default();
        let error = with_app_portal::<()>(&cancel, || bail!("options failed"));
        assert_eq!(error.unwrap_err().to_string(), "options failed");
        assert!(app_cancel().is_none());
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = with_app_portal::<()>(&cancel, || panic!("startup panic"));
        }));
        assert!(panic.is_err());
        assert!(app_cancel().is_none());
        cancel.cancel();
        assert!(with_app_portal::<()>(&cancel, || panic!("must not run")).is_err());
    }

    #[test]
    fn the_default_request_is_the_primary_display() {
        // The same default as the command line: --display 1.
        let req = ScreenRequest::default();
        assert_eq!(req.display, 1);
        assert!(req.window.is_none() && req.region.is_none());
    }

    #[test]
    fn excluded_windows_round_trip() {
        let _lock = EXCLUSION_TEST_LOCK.lock().unwrap();
        set_excluded_windows(vec![7, 9]);
        assert_eq!(excluded_windows(), vec![7, 9]);
        // The app clears it when a recording ends: back to capturing everything.
        set_excluded_windows(vec![]);
        assert!(excluded_windows().is_empty());
    }

    #[test]
    fn capture_options_default_to_upstreams_cursor_and_no_clicks() {
        let o = CaptureOptions::default();
        assert!(o.cursor, "upstream shows the cursor by default");
        assert!(!o.clicks, "upstream highlights clicks only with --clicks");
        assert_ne!(
            o,
            CaptureOptions {
                cursor: true,
                clicks: true
            }
        );
    }

    #[test]
    fn the_backend_follows_the_knob_and_the_session() {
        // The environment is process-wide, so this test only covers the knob's
        // parsing and the default for the session it finds itself in.
        let backend = backend().unwrap();
        match std::env::var("REC_SCREEN_BACKEND").as_deref() {
            Ok("x11") => assert_eq!(backend, Backend::X11),
            Ok("portal") => assert_eq!(backend, Backend::Portal),
            _ => match session() {
                Session::X11 => assert_eq!(backend, Backend::X11),
                _ => assert_eq!(backend, Backend::Portal),
            },
        }
    }

    #[test]
    fn targets_describe_what_is_captured() {
        let d = DisplayInfo {
            index: 2,
            id: "HDMI-1".into(),
            name: "HDMI-1".into(),
            pixels: (2560, 1440),
            points: (1280, 720),
            primary: false,
        };
        assert_eq!(
            Target::Display(d.clone(), None).describe(),
            "display 2 (2560x1440 pixels)"
        );
        let r = Region {
            x: 10.0,
            y: 20.0,
            width: 800.0,
            height: 600.0,
        };
        assert_eq!(
            Target::Display(d, Some(r)).describe(),
            "display 2 region 800x600 at 10,20"
        );
        let w = WindowInfo {
            id: 7,
            app: "foot".into(),
            title: "shell".into(),
            size: (801, 599),
        };
        assert_eq!(Target::Window(w).describe(), "window 7 (foot — 801x599)");
    }

    #[test]
    fn a_portal_target_says_what_the_picker_chose() {
        let t = Target::Picked {
            window: false,
            pixels: (1920, 1080),
        };
        assert_eq!(
            t.describe(),
            "the monitor the system picker chose (1920x1080 pixels)"
        );
    }

    #[test]
    fn without_a_display_server_listing_explains_itself() {
        // The self-test environment has neither DISPLAY nor WAYLAND_DISPLAY.
        if session() != Session::Unknown {
            return;
        }
        let e = displays().unwrap_err().to_string();
        assert!(
            e.contains("WAYLAND_DISPLAY") || e.contains("Wayland"),
            "{e}"
        );
        let e = windows().unwrap_err().to_string();
        assert!(e.contains("X11") || e.contains("DISPLAY"), "{e}");
    }
}
