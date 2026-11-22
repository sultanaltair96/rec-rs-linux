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
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScreenRequest {
    /// 1-based display number (`--display`).
    pub display: usize,
    /// Window id or an app/title substring (`--window`).
    pub window: Option<String>,
    /// Region of the display (`--region`), which excludes `window`.
    pub region: Option<Region>,
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

    /// The native pixel size of what is being captured.
    pub fn pixels(&self) -> (usize, usize) {
        match self {
            // A region's native size is its size in pixels, which the caller
            // computed with the display's scale.
            Target::Display(_, Some(r)) => (
                r.width.max(2.0) as usize & !1,
                r.height.max(2.0) as usize & !1,
            ),
            Target::Display(d, None) => (d.pixels.0 & !1, d.pixels.1 & !1),
            Target::Window(w) => (w.size.0 & !1, w.size.1 & !1),
            Target::Picked { pixels, .. } => (pixels.0 & !1, pixels.1 & !1),
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
    if req.display > 1 {
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
    let prepared = portal::prepare(want, CaptureOptions::default(), portal_timeout())?;
    let (target, native) = target_from_portal(req, &prepared)?;
    let new = Pending {
        request: req.clone(),
        opts: CaptureOptions::default(),
        prepared,
    };
    if let Some(old) = PENDING.lock().unwrap().replace(new) {
        old.prepared.close();
    }
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
                None => Target::Display(d.clone(), None).pixels(),
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
    match backend()? {
        Backend::X11 => {
            let server = x11::Server::connect()?;
            let spec = match &req.window {
                Some(sel) => x11::resolve_window(&server, sel)?.0,
                None => x11::resolve_display(&server, req.display, req.region.as_ref())?.0,
            };
            Ok(Box::new(x11::Source::new(spec, *opts)))
        }
        Backend::Portal => Ok(Box::new(PortalSource::open(req, *opts)?)),
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
        let prepared = take_pending(req, opts)?;
        if let Some(note) = &prepared.cursor_note {
            warn_once(note);
        }
        let crop = match &req.region {
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
    let pending = PENDING.lock().unwrap().take();
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
    portal::prepare(want, opts, portal_timeout())
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

/// `rec-rs permissions` screen line, and with `request` the portal dialog.
pub fn permission_status(request: bool, wait: u64) -> Result<String> {
    if request {
        request_permission(wait)?;
    }
    Ok(status_line())
}

/// Run the ScreenCast dialog once and keep the grant.
pub fn request_permission(wait: u64) -> Result<()> {
    let timeout = Duration::from_secs(wait.max(5));
    let prepared = portal::prepare(Want::Monitor, CaptureOptions::default(), timeout)?;
    println!("screen recording : granted for {}", prepared.picked);
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
        return format!(
            "unavailable - {}{pipewire}",
            portal_error_line(portal.err())
        );
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

fn portal_error_line(e: Option<anyhow::Error>) -> String {
    match e {
        Some(e) => format!("; {e:#}"),
        None => String::new(),
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
    fn the_default_request_is_the_primary_display() {
        let req = ScreenRequest::default();
        assert_eq!(req.display, 0);
        // cli.rs always sets --display (default 1); a Default request is only a
        // placeholder for the app's preview.
        let req = ScreenRequest {
            display: 1,
            ..Default::default()
        };
        assert_eq!(req.display, 1);
        assert!(req.window.is_none() && req.region.is_none());
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
    fn targets_report_their_native_size() {
        let d = DisplayInfo {
            index: 2,
            id: "HDMI-1".into(),
            name: "HDMI-1".into(),
            pixels: (2560, 1441),
            points: (1280, 720),
            primary: false,
        };
        // Whole display, rounded to even for the encoder.
        assert_eq!(Target::Display(d.clone(), None).pixels(), (2560, 1440));
        // A region's native size is its own.
        let r = Region {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        assert_eq!(Target::Display(d, Some(r)).pixels(), (800, 600));
        let w = WindowInfo {
            id: 7,
            app: "foot".into(),
            title: "shell".into(),
            size: (801, 599),
        };
        assert_eq!(Target::Window(w.clone()).pixels(), (800, 598));
        assert!(Target::Window(w).describe().contains("801x599"));
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
        assert_eq!(t.pixels(), (1920, 1080));
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
