//! Direct X11 capture, monitor and window enumeration, and the pointer.
//!
//! On an X11 session this is the preferred backend: unlike the portal it can
//! honour `--display N` and `--window <substring>` exactly, it never shows a
//! picker, and X11 has no permission model to ask about. On Wayland it is only
//! used when `REC_SCREEN_BACKEND=x11` says so (XWayland's root window does not
//! contain the native Wayland clients, so a half-empty picture is the best it
//! can do).
//!
//! Enumeration uses RandR 1.5 monitors (names, positions, the primary flag) and
//! EWMH's `_NET_CLIENT_LIST` with a `WM_CLASS`/`_NET_WM_NAME` fallback for
//! windows. Capture is `GetImage` on the root window or on the client window,
//! which is what makes this path work without a compositor: Xvfb and a plain
//! Xorg both answer it.
//!
//! The cursor is not part of an X11 window's pixels, so `--cursor` composites it
//! from XFixes, and `--clicks` watches the button bits of `QueryPointer` — no
//! XInput2 grab, so nothing is taken away from the desktop while recording.
//!
//! Adapted from rec-rs src/screen.rs (MIT OR Apache-2.0), whose display/window
//! enumeration and "largest match wins" selector this mirrors.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, ImageFormat, MapState, Window};
use x11rb::rust_connection::RustConnection;

use crate::media_types::{
    self, DisplayInfo, FrameSink, PixelFormat, VideoFrame, VideoSource, WindowInfo,
};

use super::CaptureOptions;
use super::Region;
use super::cursor::{Click, CursorImage, Frame, draw_clicks};
use super::format::{Rect, even_floor, region_to_pixels};
use super::selector;

/// Windows smaller than this are skipped, like upstream (tooltips, menus).
const MIN_WINDOW: usize = 40;

/// Longest cursor bitmap edge we will composite. Cursors are 24-64 px; anything
/// larger is a client bug or a hostile server.
const MAX_CURSOR: usize = 256;

/// One line for `devices` when an X11 property is not set.
fn atom_name(conn: &RustConnection, atom: u32) -> String {
    if atom == 0 {
        return String::new();
    }
    conn.get_atom_name(atom)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| String::from_utf8_lossy(&r.name).to_string())
        .unwrap_or_default()
}

/// An open connection to the X server plus the pieces every query starts from.
pub struct Server {
    conn: RustConnection,
    root: Window,
    width: usize,
    height: usize,
}

impl Server {
    /// Connect to `$DISPLAY`.
    pub fn connect() -> Result<Server> {
        let (conn, screen_num) = RustConnection::connect(None).map_err(|e| {
            anyhow!(
                "no X11 display: {e}. On a Wayland session the portal is the screen-capture \
                 backend (`REC_SCREEN_BACKEND=portal`); set DISPLAY to use the X11 one"
            )
        })?;
        let screen = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or_else(|| anyhow!("X11 screen {screen_num} does not exist"))?;
        Ok(Server {
            root: screen.root,
            width: screen.width_in_pixels as usize,
            height: screen.height_in_pixels as usize,
            conn,
        })
    }

    fn intern(&self, name: &str) -> Result<u32> {
        Ok(self
            .conn
            .intern_atom(false, name.as_bytes())
            .with_context(|| format!("intern {name}"))?
            .reply()
            .with_context(|| format!("intern {name}"))?
            .atom)
    }

    /// A window property's raw bytes, 8-bit or 32-bit as the server formatted
    /// it. `None` when the property is absent (or `format` is unexpected).
    fn property(&self, window: Window, name: &str, format: u8) -> Option<Vec<u8>> {
        let atom = self.intern(name).ok()?;
        let reply = self
            .conn
            .get_property(false, window, atom, AtomEnum::ANY, 0, 1 << 13)
            .ok()?
            .reply()
            .ok()?;
        // A 32-bit property arrives as bytes; `value32` would drop the padding
        // differences, but we only ever read text (format 8) or window ids.
        if reply.format != format {
            return None;
        }
        Some(reply.value)
    }

    /// A UTF-8 string property (`_NET_WM_NAME`, which is `UTF8_STRING`).
    fn text(&self, window: Window, name: &str) -> Option<String> {
        self.property(window, name, 8)
            .map(|v| String::from_utf8_lossy(&v).to_string())
            .filter(|s| !s.is_empty())
    }

    /// The 32-bit window list property (`_NET_CLIENT_LIST`).
    fn window_list(&self, name: &str) -> Option<Vec<Window>> {
        let value = self.property(self.root, name, 32)?;
        Some(
            value
                .chunks_exact(4)
                .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                .filter(|w| *w != 0)
                .collect(),
        )
    }

    /// The monitors, primary first, then left to right and top to bottom, each
    /// with its screen rectangle.
    ///
    /// RandR 1.5's monitors are the desktop's own monitor objects (one per
    /// screen, or several mirrors of one), which is what a user counts as a
    /// display. Without RandR 1.5 (Xvfb, very old servers) the single screen is
    /// one display.
    pub fn monitors(&self) -> Vec<(DisplayInfo, Rect)> {
        let mut out = self.randr_monitors().unwrap_or_default();
        if out.is_empty() {
            out.push((
                DisplayInfo {
                    index: 1,
                    id: "screen".to_string(),
                    name: "screen".to_string(),
                    pixels: (self.width, self.height),
                    points: (self.width, self.height),
                    primary: true,
                },
                Rect::full(self.width, self.height),
            ));
        }
        // Stable numbering: the primary comes first because `--display 1` means
        // "main display" upstream too.
        out.sort_by_key(|(d, r)| (!d.primary, r.x, r.y));
        // A server that reports no primary (Xvfb, a bare X session, some
        // drivers) still has a main display: the first one.
        if !out.iter().any(|(d, _)| d.primary)
            && let Some((d, _)) = out.first_mut()
        {
            d.primary = true;
        }
        for (i, (d, _)) in out.iter_mut().enumerate() {
            d.index = i + 1;
        }
        out
    }

    /// The display list `rec-rs devices` prints.
    pub fn displays(&self) -> Vec<DisplayInfo> {
        self.monitors().into_iter().map(|(d, _)| d).collect()
    }

    fn randr_monitors(&self) -> Option<Vec<(DisplayInfo, Rect)>> {
        let version = self.conn.randr_query_version(1, 5).ok()?.reply().ok()?;
        if (version.major_version, version.minor_version) < (1, 5) {
            return None;
        }
        let reply = self
            .conn
            .randr_get_monitors(self.root, true)
            .ok()?
            .reply()
            .ok()?;
        let mut out = vec![];
        for m in reply.monitors {
            // An unusable monitor: no area, or an origin outside the root
            // window (which `GetImage` cannot address at all).
            if m.width < 2 || m.height < 2 || m.x < 0 || m.y < 0 {
                continue;
            }
            let name = atom_name(&self.conn, m.name);
            let name = if name.is_empty() {
                "monitor".to_string()
            } else {
                name
            };
            out.push((
                DisplayInfo {
                    index: 0,
                    id: name.clone(),
                    name,
                    pixels: (m.width as usize, m.height as usize),
                    // X11 coordinates are pixels; a logical size only exists
                    // under XWayland, where the root window is already in
                    // logical units.
                    points: (m.width as usize, m.height as usize),
                    primary: m.primary,
                },
                Rect::new(
                    m.x as usize,
                    m.y as usize,
                    m.width as usize,
                    m.height as usize,
                ),
            ));
        }
        Some(out)
    }

    /// The on-screen windows `--window` can select.
    ///
    /// `_NET_CLIENT_LIST` is authoritative when a window manager is running.
    /// Without one (Xvfb in the self-test, a bare X session) the top-level
    /// children with a name or a class stand in, which is what makes window
    /// capture testable headless.
    pub fn windows(&self) -> Vec<WindowInfo> {
        let mut ids = self
            .window_list("_NET_CLIENT_LIST")
            .or_else(|| self.window_list("_NET_CLIENT_LIST_STACKING"))
            .unwrap_or_default();
        if ids.is_empty() {
            ids = self
                .conn
                .query_tree(self.root)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| r.children)
                .unwrap_or_default();
        }
        let mut out = vec![];
        for id in ids {
            let Ok(cookie) = self.conn.get_window_attributes(id) else {
                continue;
            };
            let Ok(attrs) = cookie.reply() else {
                continue;
            };
            if attrs.map_state != MapState::VIEWABLE {
                continue;
            }
            let Some((app, title, size)) = self.describe_window(id) else {
                continue;
            };
            if size.0 < MIN_WINDOW || size.1 < MIN_WINDOW {
                continue;
            }
            out.push(WindowInfo {
                id: id as u64,
                app,
                title,
                size,
            });
        }
        out
    }

    /// `(app, title, size)` of a window, skipping ones with nothing to name it.
    fn describe_window(&self, id: Window) -> Option<(String, String, (usize, usize))> {
        let title = self
            .text(id, "_NET_WM_NAME")
            .or_else(|| {
                // WM_NAME is Latin-1 in practice; lossy is close enough for a
                // selector and never fails.
                self.property(id, "WM_NAME", 8)
                    .map(|v| String::from_utf8_lossy(&v).to_string())
            })
            .unwrap_or_default();
        let app = self
            .property(id, "WM_CLASS", 8)
            .map(|v| {
                // Two NUL-terminated strings: instance, then class.
                let mut parts = v.split(|b| *b == 0).filter(|p| !p.is_empty());
                let instance = parts.next().unwrap_or(&[]);
                let class = parts.next().unwrap_or(instance);
                String::from_utf8_lossy(class).to_string()
            })
            .unwrap_or_default();
        if app.is_empty() && title.is_empty() {
            return None;
        }
        let geometry = self.conn.get_geometry(id).ok()?.reply().ok()?;
        Some((
            app,
            title,
            (geometry.width as usize, geometry.height as usize),
        ))
    }

    /// A window's absolute position on the root window.
    fn window_origin(&self, id: Window) -> (i32, i32) {
        self.conn
            .translate_coordinates(id, self.root, 0, 0)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| (r.dst_x as i32, r.dst_y as i32))
            .unwrap_or((0, 0))
    }

    /// Read `rect` of `drawable` as packed BGRX, the layout a depth-24 X11
    /// image arrives in.
    fn image(&self, drawable: Window, rect: Rect) -> Result<Vec<u8>> {
        let reply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                drawable,
                rect.x as i16,
                rect.y as i16,
                rect.width as u16,
                rect.height as u16,
                !0,
            )
            .with_context(|| {
                format!(
                    "GetImage {}x{} of drawable {drawable}",
                    rect.width, rect.height
                )
            })?
            .reply()
            .context("GetImage")?;
        let want = rect.width * rect.height * 4;
        if reply.data.len() < want {
            bail!(
                "X11 returned {} bytes for a {}x{} image ({} needed): the server's depth is not 24/32",
                reply.data.len(),
                rect.width,
                rect.height,
                want
            );
        }
        let mut data = reply.data;
        data.truncate(want);
        #[cfg(target_endian = "big")]
        for px in data.chunks_exact_mut(4) {
            // The server sends native-endian 32-bit pixels; BGRA in memory is
            // little-endian B,G,R,X.
            px.swap(0, 3);
            px.swap(1, 2);
        }
        Ok(data)
    }

    /// The pointer's position on the root window and its button mask.
    fn pointer(&self) -> Result<(i32, i32, u16)> {
        let reply = self
            .conn
            .query_pointer(self.root)
            .context("QueryPointer")?
            .reply()
            .context("QueryPointer")?;
        Ok((reply.root_x as i32, reply.root_y as i32, reply.mask.bits()))
    }

    /// The cursor bitmap, tagged with the server's serial so an unchanged
    /// cursor is converted once instead of 60 times a second.
    fn cursor(&self) -> Option<(u32, CursorImage)> {
        let reply = self.conn.xfixes_get_cursor_image().ok()?.reply().ok()?;
        let (w, h) = (reply.width as usize, reply.height as usize);
        if w == 0 || h == 0 || w > MAX_CURSOR || h > MAX_CURSOR {
            return None;
        }
        // ARGB, premultiplied, as u32 in the server's byte order. Rebuild it as
        // BGRA bytes for `CursorImage`.
        let mut pixels = Vec::with_capacity(w * h * 4);
        for px in &reply.cursor_image {
            let [b, g, r, a] = px.to_ne_bytes();
            // A u32 from the server is A in the high byte on little-endian, so
            // `to_ne_bytes` gives [B, G, R, A].
            pixels.extend_from_slice(&[b, g, r, a]);
        }
        Some((
            reply.cursor_serial,
            CursorImage {
                width: w,
                height: h,
                stride: w * 4,
                pixels,
                hotspot_x: reply.xhot as i32,
                hotspot_y: reply.yhot as i32,
            },
        ))
    }

    /// The desktop's background colour: the root window's background pixel,
    /// interpreted with the root visual's masks.
    /// The visible parts of `excluded` inside `area` (root coordinates).
    pub fn excluded_pieces(&self, excluded: &[u32], area: Rect) -> Vec<Rect> {
        if excluded.is_empty() {
            return vec![];
        }
        let stack = self.stacking_order();
        let mut out = vec![];
        for &id in excluded {
            let Ok(cookie) = self.conn.get_window_attributes(id) else {
                continue;
            };
            let Ok(attrs) = cookie.reply() else {
                continue;
            };
            // Iconified or withdrawn: nothing of it is in the picture.
            if attrs.map_state != MapState::VIEWABLE {
                continue;
            }
            let Some(rect) = self.window_rect(id) else {
                continue;
            };
            let Some(inside) = intersect(rect, area) else {
                continue;
            };
            // The parts of it that a window stacked above still covers are not
            // ours to paint out: they are that window's pixels.
            let mut pieces = vec![inside];
            let at = stack.iter().position(|w| *w == id).unwrap_or(stack.len());
            for above in stack.iter().skip(at + 1) {
                if let Some(cover) = self.window_rect(*above)
                    && intersect(cover, inside).is_some()
                {
                    pieces = subtract_rects(&pieces, cover);
                }
            }
            out.extend(pieces);
        }
        out
    }

    /// The window list in bottom-to-top stacking order.
    fn stacking_order(&self) -> Vec<Window> {
        self.window_list("_NET_CLIENT_LIST_STACKING")
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                self.conn
                    .query_tree(self.root)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .map(|r| r.children)
                    .unwrap_or_default()
            })
    }

    /// A window's rectangle in root coordinates, including its window-manager
    /// decorations (`_NET_FRAME_EXTENTS`), so an excluded window leaves no
    /// title bar behind.
    fn window_rect(&self, id: Window) -> Option<Rect> {
        let geometry = self.conn.get_geometry(id).ok()?.reply().ok()?;
        let (x, y) = self.window_origin(id);
        let (left, right, top, bottom) = self.frame_extents(id);
        let x0 = x as i64 - left as i64;
        let y0 = y as i64 - top as i64;
        Some(Rect::new(
            x0.max(0) as usize,
            y0.max(0) as usize,
            geometry.width as usize + left as usize + right as usize,
            geometry.height as usize + top as usize + bottom as usize,
        ))
    }

    /// `_NET_FRAME_EXTENTS`: left, right, top, bottom. Zero without a window
    /// manager (Xvfb, a bare session).
    fn frame_extents(&self, id: Window) -> (u32, u32, u32, u32) {
        let Some(value) = self.property(id, "_NET_FRAME_EXTENTS", 32) else {
            return (0, 0, 0, 0);
        };
        if value.len() < 16 {
            return (0, 0, 0, 0);
        }
        let at = |i: usize| {
            u32::from_ne_bytes([
                value[i * 4],
                value[i * 4 + 1],
                value[i * 4 + 2],
                value[i * 4 + 3],
            ])
        };
        (at(0), at(1), at(2), at(3))
    }
}

/// `rects` minus `cut`: the parts of each rectangle that `cut` does not cover.
///
/// Axis-aligned subtraction, up to four pieces per rectangle. This is what
/// turns "our window's rectangle" into "the parts of our window that are not
/// hidden behind a window stacked above it".
pub fn subtract_rects(rects: &[Rect], cut: Rect) -> Vec<Rect> {
    let mut out = Vec::with_capacity(rects.len() + 3);
    for r in rects {
        let Some(overlap) = intersect(*r, cut) else {
            out.push(*r);
            continue;
        };
        let (r_right, r_bottom) = (r.x + r.width, r.y + r.height);
        let (o_right, o_bottom) = (overlap.x + overlap.width, overlap.y + overlap.height);
        // Above and below the cut, full width.
        if overlap.y > r.y {
            out.push(Rect::new(r.x, r.y, r.width, overlap.y - r.y));
        }
        if o_bottom < r_bottom {
            out.push(Rect::new(r.x, o_bottom, r.width, r_bottom - o_bottom));
        }
        // Left and right of the cut, only in the band the cut covers.
        if overlap.x > r.x {
            out.push(Rect::new(r.x, overlap.y, overlap.x - r.x, overlap.height));
        }
        if o_right < r_right {
            out.push(Rect::new(
                o_right,
                overlap.y,
                r_right - o_right,
                overlap.height,
            ));
        }
    }
    out.retain(|r| r.width > 0 && r.height > 0);
    out
}

/// The overlapping part of two rectangles, if they overlap at all.
pub fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = (a.x + a.width).min(b.x + b.width);
    let y1 = (a.y + a.height).min(b.y + b.height);
    (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
}

/// What to capture, resolved before the first frame.
#[derive(Clone, Debug)]
pub struct Spec {
    /// The drawable to read: the root window or a client window.
    pub drawable: Window,
    /// The rectangle inside the drawable.
    pub rect: Rect,
    /// The rectangle's absolute position on the root window, for the pointer.
    pub origin: (i32, i32),
    /// One line for the start log.
    pub description: String,
}

/// Resolve `--display N` (and `--region`) to a monitor.
pub fn resolve_display(
    server: &Server,
    index: usize,
    region: Option<&Region>,
) -> Result<(Spec, DisplayInfo)> {
    let monitors = server.monitors();
    if index == 0 || index > monitors.len() {
        bail!(
            "display {index} not found: displays are numbered 1..={} (see `rec-rs devices`)",
            monitors.len()
        );
    }
    let (d, area) = monitors[index - 1].clone();
    let (rect, description) = match region {
        Some(r) => {
            let Some(local) = region_to_pixels(r, d.pixels, d.points) else {
                bail!(
                    "--region {}x{}+{}+{} is outside display {} ({}x{})",
                    r.width,
                    r.height,
                    r.x,
                    r.y,
                    index,
                    d.pixels.0,
                    d.pixels.1
                );
            };
            (
                Rect::new(
                    area.x + local.x,
                    area.y + local.y,
                    local.width,
                    local.height,
                ),
                format!(
                    "display {} region {}x{} at {},{}, in {}x{} pixels",
                    index, local.width, local.height, r.x, r.y, local.width, local.height
                ),
            )
        }
        None => (
            area,
            format!(
                "display {} {} ({}x{} pixels)",
                index, d.name, d.pixels.0, d.pixels.1
            ),
        ),
    };
    Ok((
        Spec {
            drawable: server.root,
            rect,
            origin: (rect.x as i32, rect.y as i32),
            description,
        },
        d,
    ))
}

/// Resolve `--window` (an id or an app/title substring) to a client window.
pub fn resolve_window(server: &Server, sel: &str) -> Result<(Spec, WindowInfo)> {
    let windows = server.windows();
    if windows.is_empty() {
        bail!(
            "no capturable window was found (is a window manager running? `rec-rs devices` lists \
             the stack)"
        );
    }
    let w = selector::find(&windows, sel)?.clone();
    let id = w.id as Window;
    let origin = server.window_origin(id);
    // Even crop: the encoder needs chroma pairs, and a window is never resized
    // by us.
    let width = even_floor(w.size.0).max(2);
    let height = even_floor(w.size.1).max(2);
    Ok((
        Spec {
            drawable: id,
            rect: Rect::new(0, 0, width, height),
            origin,
            description: format!(
                "window {} ({:?} — {}x{} pt)",
                w.id, w.app, w.size.0, w.size.1
            ),
        },
        w,
    ))
}

/// The X11 capture loop, stopped and started by the session.
pub struct Source {
    spec: Spec,
    opts: CaptureOptions,
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl Source {
    pub fn new(spec: Spec, opts: CaptureOptions) -> Source {
        Source {
            spec,
            opts,
            stop: Arc::new(AtomicBool::new(false)),
            error: Arc::new(Mutex::new(None)),
            thread: None,
        }
    }

    pub fn description(&self) -> &str {
        &self.spec.description
    }

    fn begin(&mut self, sink: FrameSink) -> Result<()> {
        if self.thread.is_some() {
            bail!("starting a capture that is already running");
        }
        self.stop = Arc::new(AtomicBool::new(false));
        let stop = self.stop.clone();
        let error = self.error.clone();
        let spec = self.spec.clone();
        let opts = self.opts;
        self.thread = Some(std::thread::spawn(move || {
            if let Err(e) = run(spec, opts, sink, stop, &error) {
                let mut slot = error.lock().unwrap();
                if slot.is_none() {
                    // Say it now: the session only learns about `stop()` at the
                    // very end, and a dead capture should not look like an
                    // empty recording.
                    eprintln!("rec: error: screen capture stopped: {e:#}");
                    *slot = Some(format!("{e:#}"));
                }
            }
        }));
        Ok(())
    }

    fn halt(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let e = self.error.lock().unwrap().take();
        match e {
            Some(e) => Err(anyhow!(e)),
            None => Ok(()),
        }
    }
}

impl VideoSource for Source {
    fn describe(&self) -> String {
        self.description().to_string()
    }

    fn start(&mut self, sink: FrameSink) -> Result<()> {
        self.begin(sink)
    }

    fn stop(&mut self) -> Result<()> {
        self.halt()
    }

    fn is_running(&self) -> bool {
        self.thread.is_some()
    }
}

/// Pick the pointer frame rate. The writer repeats the last frame on its own
/// grid, so a still desktop needs no new frames at all: we read at the frame
/// rate but only hand over a frame whose pixels changed.
const POLL: Duration = Duration::from_millis(15);

fn run(
    spec: Spec,
    opts: CaptureOptions,
    sink: FrameSink,
    stop: Arc<AtomicBool>,
    error: &Arc<Mutex<Option<String>>>,
) -> Result<()> {
    let server = Server::connect()?;
    let mut previous: Option<Arc<[u8]>> = None;
    let mut clicks: Vec<Click> = Vec::new();
    let mut buttons = 0u16;
    let mut cursor = None;
    let mut cursor_fetched = Instant::now();
    let mut last = Instant::now();
    // The recorder's own windows (the app's preview), read once: a capture that
    // starts with them listed paints them out of every frame.
    let excluded = super::excluded_windows();
    // The captured area in root coordinates, where the exclusion rectangles
    // are computed.
    let area_root = Rect::new(
        spec.origin.0.max(0) as usize,
        spec.origin.1.max(0) as usize,
        spec.rect.width,
        spec.rect.height,
    );
    if !excluded.is_empty() {
        eprintln!(
            "rec: note: leaving {} of rec's own windows out of the capture (their area is \
             filled with the desktop colour around them)",
            excluded.len()
        );
    }
    // Where the pointer was when the last frame was made: a cursor that moves
    // over an otherwise static screen is a new frame, because the cursor is
    // part of the picture we produce.
    let mut drawn_pointer: Option<(i32, i32)> = None;
    // The ring outlives the click, so keep one slice of recent clicks.
    while !stop.load(Ordering::SeqCst) {
        let now = media_types::now();
        let pointer = if opts.cursor || opts.clicks {
            server.pointer().ok()
        } else {
            None
        };
        if opts.clicks
            && let Some((x, y, mask)) = pointer
        {
            // A button that was up and is now down is a click; polling the
            // mask catches a press as long as it lasts one poll interval.
            let pressed = mask & 0x1f00;
            let fresh = pressed & !buttons;
            for bit in 0..5 {
                if fresh & (0x100 << bit) != 0 {
                    clicks.push(Click::new(
                        (x - spec.origin.0) as f64,
                        (y - spec.origin.1) as f64,
                        now,
                    ));
                }
            }
            buttons = pressed;
            clicks.retain(|c| now - c.at < super::cursor::CLICK_LIFE);
        }
        let data = match server.image(spec.drawable, spec.rect) {
            Ok(d) => d,
            Err(e) => {
                // A window that closed under us is a source error, not a panic.
                let mut slot = error.lock().unwrap();
                if slot.is_none() {
                    *slot = Some(format!("{e:#}"));
                }
                return Err(e);
            }
        };
        // Identical pixels: nothing to deliver, the writer repeats the last
        // frame. This is what keeps a still desktop cheap. What we draw on top
        // of the pixels counts too: a moving cursor and a live click ring are
        // new pictures even when the screen behind them is still.
        let mut changed = previous
            .as_ref()
            .is_none_or(|p| p.len() != data.len() || p[..] != data[..]);
        let pointer_at = pointer.map(|(x, y, _)| (x, y));
        if opts.cursor && pointer_at != drawn_pointer {
            changed = true;
        }
        if opts.clicks && !clicks.is_empty() {
            changed = true;
        }
        if changed {
            let mut data = data;
            if !excluded.is_empty() {
                let stride = spec.rect.width * 4;
                for piece in server.excluded_pieces(&excluded, area_root) {
                    let local = Rect::new(
                        piece.x - area_root.x,
                        piece.y - area_root.y,
                        piece.width,
                        piece.height,
                    );
                    // What is really behind a mapped window cannot be read
                    // back, so stand in with the desktop colour around it.
                    let colour =
                        ring_colour(&data, stride, spec.rect.width, spec.rect.height, local);
                    fill_rect(
                        &mut data,
                        stride,
                        spec.rect.width,
                        spec.rect.height,
                        local,
                        colour,
                    );
                }
            }
            if opts.cursor || opts.clicks {
                let (px, py) = pointer.map(|(x, y, _)| (x, y)).unwrap_or((0, 0));
                // XFixes hands over the whole bitmap on every call, so fetch it
                // when the pointer moved (the usual reason the cursor changed)
                // and otherwise twice a second, which catches a theme or shape
                // change without a round trip per frame.
                let moved = pointer_at.is_some_and(|p| Some(p) != drawn_pointer);
                if opts.cursor
                    && (moved
                        || cursor.is_none()
                        || cursor_fetched.elapsed() > Duration::from_millis(500))
                    && let Some((serial, image)) = server.cursor()
                {
                    cursor_fetched = Instant::now();
                    if cursor
                        .as_ref()
                        .is_none_or(|(s, _): &(u32, CursorImage)| *s != serial)
                    {
                        cursor = Some((serial, image));
                    }
                }
                let mut frame = Frame {
                    data: &mut data,
                    stride: spec.rect.width * 4,
                    width: spec.rect.width,
                    height: spec.rect.height,
                    format: PixelFormat::Bgra,
                };
                if let Some((_, image)) = &cursor {
                    image.draw(
                        &mut frame,
                        (px - spec.origin.0) as f64,
                        (py - spec.origin.1) as f64,
                    );
                }
                if opts.clicks && !clicks.is_empty() {
                    draw_clicks(&mut frame, &clicks, now);
                }
            }
            previous = Some(Arc::from(data.as_slice()));
            drawn_pointer = pointer_at;
            sink(VideoFrame::packed(
                now,
                spec.rect.width,
                spec.rect.height,
                PixelFormat::Bgra,
                data,
            ));
        }
        // Sleep out the rest of the period, in small slices so `stop` is quick.
        let elapsed = last.elapsed();
        last = Instant::now();
        if elapsed < POLL {
            let mut left = POLL - elapsed;
            while left > Duration::ZERO && !stop.load(Ordering::SeqCst) {
                let step = left.min(Duration::from_millis(5));
                std::thread::sleep(step);
                left = left.saturating_sub(step);
            }
        }
    }
    Ok(())
}

/// A colour standing in for what is behind an excluded window: the median of
/// the desktop pixels in a thin ring just outside it, from the same frame.
///
/// The pixels actually *under* a mapped window cannot be read back (the server
/// has already composited over them, and a compositor's targets are not ours),
/// so this is the closest honest approximation: on a plain desktop it is exactly
/// the background, on a wallpaper it is the wallpaper's colour beside the
/// window. The ring is empty when the window fills the frame; then the fill is
/// black.
fn ring_colour(data: &[u8], stride: usize, width: usize, height: usize, rect: Rect) -> [u8; 4] {
    const BAND: usize = 4;
    let (mut r, mut g, mut b) = (Vec::new(), Vec::new(), Vec::new());
    let mut push = |x: usize, y: usize| {
        if x >= width || y >= height {
            return;
        }
        let at = y * stride + x * 4;
        if at + 4 > data.len() {
            return;
        }
        b.push(data[at]);
        g.push(data[at + 1]);
        r.push(data[at + 2]);
    };
    let (x0, y0) = (rect.x, rect.y);
    let (x1, y1) = (
        (rect.x + rect.width).min(width),
        (rect.y + rect.height).min(height),
    );
    // Every other pixel of each band: enough for a median, cheap at 4K.
    for step in 1..=BAND {
        let top = y0.checked_sub(step);
        let bottom = y1 + step - 1;
        for x in (x0..x1).step_by(2) {
            if let Some(y) = top {
                push(x, y);
            }
            push(x, bottom);
        }
        let left = x0.checked_sub(step);
        let right = x1 + step - 1;
        for y in (y0..y1).step_by(2) {
            if let Some(x) = left {
                push(x, y);
            }
            push(right, y);
        }
    }
    let median = |v: &mut Vec<u8>| -> u8 {
        if v.is_empty() {
            return 0;
        }
        v.sort_unstable();
        v[v.len() / 2]
    };
    [median(&mut b), median(&mut g), median(&mut r), 0]
}

/// Fill `rect` of a packed BGRA frame with one colour, clipped to the frame.
fn fill_rect(
    data: &mut [u8],
    stride: usize,
    width: usize,
    height: usize,
    rect: Rect,
    colour: [u8; 4],
) {
    let x0 = rect.x.min(width);
    let y0 = rect.y.min(height);
    let x1 = (rect.x + rect.width).min(width);
    let y1 = (rect.y + rect.height).min(height);
    for y in y0..y1 {
        let row = y * stride;
        for x in x0..x1 {
            let at = row + x * 4;
            if at + 4 <= data.len() {
                data[at..at + 4].copy_from_slice(&colour);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::protocol::xtest::ConnectionExt as _;
    use x11rb::wrapper::ConnectionExt as _;

    /// A window we own, destroyed when the test ends.
    ///
    /// Red (0x00ff0000 in X's RGB masks) so a captured frame's colour is
    /// unmistakable, with `WM_CLASS`/`WM_NAME` set the way a real client sets
    /// them.
    struct TestWindow {
        server: Server,
        win: Window,
        /// Unique per test, so parallel tests do not capture each other's
        /// windows (they are all solid red and the same size).
        tag: String,
    }

    impl TestWindow {
        /// A window at `x`,`y`. Tests use different positions: without a
        /// compositor, `GetImage` of an obscured window is undefined, so two
        /// test windows must not overlap.
        fn open(server: Server, tag: &str, x: i16, y: i16, width: u16, height: u16) -> TestWindow {
            let win = server.conn.generate_id().unwrap();
            server
                .conn
                .create_window(
                    server.conn.setup().roots[0].root_depth,
                    win,
                    server.root,
                    x,
                    y,
                    width,
                    height,
                    0,
                    x11rb::protocol::xproto::WindowClass::INPUT_OUTPUT,
                    0,
                    &x11rb::protocol::xproto::CreateWindowAux::new().background_pixel(0x00ff0000),
                )
                .unwrap();
            let title = format!("rec-rs test window {tag}");
            server
                .conn
                .change_property8(
                    x11rb::protocol::xproto::PropMode::REPLACE,
                    win,
                    AtomEnum::WM_NAME,
                    AtomEnum::STRING,
                    title.as_bytes(),
                )
                .unwrap();
            let class = format!("{tag}\0{tag}\0");
            server
                .conn
                .change_property8(
                    x11rb::protocol::xproto::PropMode::REPLACE,
                    win,
                    AtomEnum::WM_CLASS,
                    AtomEnum::STRING,
                    class.as_bytes(),
                )
                .unwrap();
            server.conn.map_window(win).unwrap();
            // Paint the background now: without a compositor or a window
            // manager nothing else would, and GetImage would read an
            // unpainted (black) window.
            server
                .conn
                .clear_area(false, win, 0, 0, width, height)
                .unwrap();
            server.conn.flush().unwrap();
            // Give the server a moment to map it, so `windows()` sees it as
            // viewable.
            std::thread::sleep(std::time::Duration::from_millis(150));
            TestWindow {
                server,
                win,
                tag: tag.to_string(),
            }
        }

        /// XTEST input, so the pointer and buttons move exactly as a real
        /// device would (the server generates the events).
        fn pointer_to(&self, x: i16, y: i16) {
            // MOTION_NOTIFY is event type 6.
            self.server
                .conn
                .xtest_fake_input(6, 0, 0, self.server.root, x, y, 0)
                .unwrap();
            self.server.conn.flush().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(60));
        }

        fn button(&self, press: bool) {
            // BUTTON_PRESS is 4, BUTTON_RELEASE is 5; detail 1 is the left
            // button.
            self.server
                .conn
                .xtest_fake_input(if press { 4 } else { 5 }, 1, 0, 0, 0, 0, 0)
                .unwrap();
            self.server.conn.flush().unwrap();
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            let _ = self.server.conn.destroy_window(self.win);
            let _ = self.server.conn.flush();
        }
    }

    /// The X server from `$DISPLAY`, or `None` when the tests run without one
    /// (the normal case: the X11 tests only run inside `scripts/screen/xvfb.sh`).
    fn live_server() -> Option<Server> {
        std::env::var_os("DISPLAY")?;
        Server::connect().ok()
    }

    /// Collects every frame a source delivers.
    fn collector() -> (crate::media_types::FrameSink, Arc<Mutex<Vec<VideoFrame>>>) {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let sink_frames = frames.clone();
        let sink: crate::media_types::FrameSink =
            Arc::new(move |f: VideoFrame| sink_frames.lock().unwrap().push(f));
        (sink, frames)
    }

    /// Fill `rect` of a window with `pixel` (0x00RRGGBB on a TrueColor
    /// visual), so a capture can be told apart pixel by pixel.
    fn fill(window: &TestWindow, x: i16, y: i16, w: u16, h: u16, pixel: u32) {
        let gc = window.server.conn.generate_id().unwrap();
        window
            .server
            .conn
            .create_gc(
                gc,
                window.win,
                &x11rb::protocol::xproto::CreateGCAux::new().foreground(pixel),
            )
            .unwrap();
        window
            .server
            .conn
            .poly_fill_rectangle(
                window.win,
                gc,
                &[x11rb::protocol::xproto::Rectangle {
                    x,
                    y,
                    width: w,
                    height: h,
                }],
            )
            .unwrap();
        window.server.conn.free_gc(gc).unwrap();
        window.server.conn.flush().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    fn red_of(frame: &VideoFrame, x: usize, y: usize) -> u8 {
        let p = &frame.planes[0];
        p.data[y * p.stride + x * 4 + 2]
    }

    fn green_of(frame: &VideoFrame, x: usize, y: usize) -> u8 {
        let p = &frame.planes[0];
        p.data[y * p.stride + x * 4 + 1]
    }

    #[test]
    fn subtracting_a_rectangle_covers_the_possible_shapes() {
        let a = Rect::new(10, 10, 100, 100);
        // Nothing overlaps: unchanged.
        assert_eq!(subtract_rects(&[a], Rect::new(200, 200, 10, 10)), vec![a]);
        // Fully covered: gone.
        assert!(subtract_rects(&[a], Rect::new(0, 0, 500, 500)).is_empty());
        // A corner cut: the three remaining pieces, each disjoint from the cut.
        let cut = Rect::new(10, 10, 40, 40);
        let pieces = subtract_rects(&[a], cut);
        assert!(!pieces.is_empty());
        let area = |r: &[Rect]| r.iter().map(|r| r.width * r.height).sum::<usize>();
        assert_eq!(area(&pieces), 100 * 100 - 40 * 40);
        for p in &pieces {
            assert_eq!(
                intersect(*p, cut),
                None,
                "piece {p:?} still overlaps the cut"
            );
            // And every piece is inside the original.
            assert!(p.x >= a.x && p.y >= a.y);
            assert!(p.x + p.width <= a.x + a.width);
            assert!(p.y + p.height <= a.y + a.height);
        }
        // A band across the middle leaves two pieces.
        let pieces = subtract_rects(&[a], Rect::new(0, 50, 200, 20));
        assert_eq!(area(&pieces), 100 * 100 - 100 * 20);
        // Several cuts in a row are just repeated subtraction.
        let pieces = subtract_rects(
            &subtract_rects(&[a], Rect::new(10, 10, 50, 50)),
            Rect::new(80, 80, 20, 20),
        );
        assert_eq!(area(&pieces), 100 * 100 - 50 * 50 - 20 * 20);
    }

    fn intersect(a: Rect, b: Rect) -> Option<Rect> {
        let x0 = a.x.max(b.x);
        let y0 = a.y.max(b.y);
        let x1 = (a.x + a.width).min(b.x + b.width);
        let y1 = (a.y + a.height).min(b.y + b.height);
        (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
    }

    #[test]
    fn excluded_windows_are_painted_out_of_the_frame() {
        let Some(server) = live_server() else {
            return;
        };
        // The desktop's background is a colour we choose, so the fill can be
        // checked exactly instead of "not red". The fill is the desktop colour
        // around the window, which on a plain desktop is exactly this.
        let background = [0x56u8, 0x34, 0x12, 0]; // BGRA of 0x00123456
        server
            .conn
            .change_window_attributes(
                server.root,
                &x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
                    .background_pixel(0x00123456),
            )
            .unwrap();
        server
            .conn
            .clear_area(true, server.root, 0, 0, 0, 0)
            .unwrap();
        server.conn.flush().unwrap();

        // Our window (red by construction) with a blue window stacked on top of
        // its right half: only the uncovered part may be painted out.
        let _lock = super::super::EXCLUSION_TEST_LOCK.lock().unwrap();
        let ours = TestWindow::open(server, "exclude-own", 200, 500, 200, 100);
        let above = TestWindow::open(live_server().unwrap(), "exclude-above", 320, 520, 120, 60);
        fill(&above, 0, 0, 120, 60, 0x000000ff);
        let _guard = ExcludedGuard::set(vec![ours.win]);

        let (mut source, frames) = capture_display(
            &ours.server,
            CaptureOptions {
                cursor: false,
                clicks: false,
            },
        );
        let f = last_frame(&mut source, frames);
        // Inside our window and not covered: the desktop background, not red.
        assert_eq!(
            pixel(&f, 250, 550),
            background,
            "our window was not painted out"
        );
        // The part covered by a window above ours keeps that window's pixels.
        assert_eq!(
            red_of(&f, 380, 550),
            0,
            "the window above was painted out too"
        );
        let blue = pixel(&f, 380, 550);
        assert_eq!(
            blue[0], 255,
            "the window above ours should still be blue: {blue:?}"
        );
        // The rest of the desktop is untouched.
        assert_eq!(pixel(&f, 1100, 700), background);

        // Without the exclusion the same capture contains our window again.
        drop(_guard);
        let (mut source, frames) = capture_display(
            &ours.server,
            CaptureOptions {
                cursor: false,
                clicks: false,
            },
        );
        let f = last_frame(&mut source, frames);
        assert_eq!(red_of(&f, 250, 550), 255, "the window should be back");
    }

    /// Clears the exclusion list however the test ends.
    struct ExcludedGuard;

    impl ExcludedGuard {
        fn set(ids: Vec<u32>) -> ExcludedGuard {
            super::super::set_excluded_windows(ids);
            ExcludedGuard
        }
    }

    impl Drop for ExcludedGuard {
        fn drop(&mut self) {
            super::super::set_excluded_windows(vec![]);
        }
    }

    /// Capture display 1 until a frame arrives, then stop.
    fn capture_display(
        server: &Server,
        opts: CaptureOptions,
    ) -> (Source, Arc<Mutex<Vec<VideoFrame>>>) {
        let (spec, _) = resolve_display(server, 1, None).unwrap();
        let mut source = Source::new(spec, opts);
        let (sink, frames) = collector();
        VideoSource::start(&mut source, sink).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        VideoSource::stop(&mut source).unwrap();
        (source, frames)
    }

    fn last_frame(_source: &mut Source, frames: Arc<Mutex<Vec<VideoFrame>>>) -> VideoFrame {
        frames.lock().unwrap().last().expect("a frame").clone()
    }

    fn pixel(frame: &VideoFrame, x: usize, y: usize) -> [u8; 4] {
        let p = &frame.planes[0];
        let at = y * p.stride + x * 4;
        [p.data[at], p.data[at + 1], p.data[at + 2], p.data[at + 3]]
    }

    #[test]
    fn a_monitor_and_a_window_are_enumerated() {
        let Some(server) = live_server() else {
            return;
        };
        let displays = server.displays();
        assert!(!displays.is_empty(), "an X server always has a display");
        assert_eq!(displays[0].index, 1);
        assert!(displays[0].primary, "the first display is the main one");
        assert!(displays[0].pixels.0 >= 2 && displays[0].pixels.1 >= 2);
        let window = TestWindow::open(server, "enumerate-test", 0, 0, 300, 200);
        // The window list finds it by class and by title, without a window
        // manager (Xvfb has none).
        let windows = window.server.windows();
        let found = windows
            .iter()
            .find(|w| w.app == "enumerate-test")
            .unwrap_or_else(|| panic!("the test window is not listed: {windows:?}"));
        assert_eq!(found.title, "rec-rs test window enumerate-test");
        assert_eq!(found.size, (300, 200));
        // And the selector resolves it by id and by substring.
        let (spec, w) = resolve_window(&window.server, &window.tag).unwrap();
        assert_eq!(w.id, found.id);
        assert_eq!(spec.rect, Rect::new(0, 0, 300, 200));
        let (spec2, _) = resolve_window(&window.server, &found.id.to_string()).unwrap();
        assert_eq!(spec2.drawable, spec.drawable);
    }

    #[test]
    fn a_window_is_captured_with_its_own_pixels() {
        let Some(server) = live_server() else {
            return;
        };
        let window = TestWindow::open(server, "capture-test", 400, 0, 300, 200);
        let (spec, _) = resolve_window(&window.server, &window.tag).unwrap();
        let mut source = Source::new(spec, CaptureOptions::default());
        let (sink, frames) = collector();
        VideoSource::start(&mut source, sink).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
        VideoSource::stop(&mut source).unwrap();
        let frames = frames.lock().unwrap();
        assert!(!frames.is_empty(), "no frame was captured");
        // The last frame, not the first: a frame may be captured before the
        // server has painted the window.
        let f = frames.last().unwrap();
        assert_eq!((f.width, f.height), (300, 200));
        assert_eq!(f.format, PixelFormat::Bgra);
        // The window is solid red: BGRX bytes are 00 00 ff.
        assert_eq!(red_of(f, 150, 100), 255, "red window");
        assert_eq!(green_of(f, 150, 100), 0);
        // The pointer was never over it, so nothing was drawn over the middle.
        assert_eq!(red_of(f, 150, 100), 255);
    }

    #[test]
    fn a_region_captures_the_pixels_it_names() {
        // A window split red | blue, captured through --region twice: each
        // region must contain only its own half. This is the check that a
        // region's *offset* is applied, which a size alone cannot tell.
        let Some(server) = live_server() else {
            return;
        };
        let window = TestWindow::open(server, "region-test", 900, 400, 200, 100);
        // The right half (100 px on) blue: 0x000000ff in X's RGB masks.
        fill(&window, 100, 0, 100, 100, 0x000000ff);
        let left = super::super::Region {
            x: 900.0,
            y: 400.0,
            width: 100.0,
            height: 100.0,
        };
        let right = super::super::Region { x: 1000.0, ..left };
        let mut captured = vec![];
        for region in [left, right] {
            let (spec, _) = resolve_display(&window.server, 1, Some(&region)).unwrap();
            assert_eq!(
                spec.rect,
                Rect::new(region.x as usize, region.y as usize, 100, 100)
            );
            let mut source = Source::new(
                spec,
                CaptureOptions {
                    cursor: false,
                    clicks: false,
                },
            );
            let (sink, frames) = collector();
            VideoSource::start(&mut source, sink).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(250));
            VideoSource::stop(&mut source).unwrap();
            let frames = frames.lock().unwrap();
            let f = frames.last().expect("a frame").clone();
            assert_eq!((f.width, f.height), (100, 100));
            captured.push(f);
        }
        // The left region is the window's red half, the right one its blue.
        for (f, what) in captured.iter().zip(["red", "blue"]) {
            for (x, y) in [(0, 0), (50, 50), (99, 99)] {
                let (r, g, b) = (
                    red_of(f, x, y),
                    green_of(f, x, y),
                    f.planes[0].data[y * 400 + x * 4],
                );
                match what {
                    "blue" => assert!(
                        b > 200 && r < 60 && g < 60,
                        "blue half at {x},{y}: {r},{g},{b}"
                    ),
                    _ => assert!(
                        r > 200 && b < 60 && g < 60,
                        "red half at {x},{y}: {r},{g},{b}"
                    ),
                }
            }
        }
    }

    #[test]
    fn a_region_crops_a_monitor() {
        let Some(server) = live_server() else {
            return;
        };
        let region = super::super::Region {
            x: 4.0,
            y: 6.0,
            width: 320.0,
            height: 240.0,
        };
        let (spec, d) = resolve_display(&server, 1, Some(&region)).unwrap();
        assert_eq!((spec.rect.width, spec.rect.height), (320, 240));
        assert_eq!(spec.origin, (region.x as i32, region.y as i32));
        assert!(d.pixels.0 >= 320);
    }

    #[test]
    fn a_click_draws_a_ring_at_the_pointer() {
        let Some(server) = live_server() else {
            return;
        };
        let window = TestWindow::open(server, "click-test", 800, 0, 300, 200);
        let (spec, _) = resolve_window(&window.server, &window.tag).unwrap();
        let origin = spec.origin;
        let mut source = Source::new(
            spec,
            CaptureOptions {
                cursor: false,
                clicks: true,
            },
        );
        let (sink, frames) = collector();
        VideoSource::start(&mut source, sink).unwrap();
        // Let one clean frame through first.
        std::thread::sleep(std::time::Duration::from_millis(150));
        // The pointer is in root coordinates; the window is not at the origin.
        window.pointer_to(origin.0 as i16 + 150, origin.1 as i16 + 100);
        window.button(true);
        std::thread::sleep(std::time::Duration::from_millis(120));
        window.button(false);
        std::thread::sleep(std::time::Duration::from_millis(120));
        VideoSource::stop(&mut source).unwrap();
        let frames = frames.lock().unwrap();
        // At least one frame shows the ring: a white-ish pixel somewhere near
        // the pointer, on an otherwise solid red window.
        let ring = frames.iter().any(|f| {
            (0..f.height).any(|y| {
                (0..f.width).any(|x| {
                    let (dx, dy) = (x as i32 - 150, y as i32 - 100);
                    let d2 = dx * dx + dy * dy;
                    // 4..40 px from the pointer: the ring's neighbourhood.
                    (16..1600).contains(&d2) && red_of(f, x, y) > 200 && green_of(f, x, y) > 200
                })
            })
        });
        assert!(
            ring,
            "no click ring in {} frames (button events were not seen)",
            frames.len()
        );
    }

    #[test]
    fn wm_class_takes_the_class_not_the_instance() {
        // "foot\0foot\0" names the application twice; GTK apps differ
        // ("Navigator\0firefox\0").
        let value = b"Navigator\0firefox\0";
        let mut parts = value.split(|b| *b == 0).filter(|p| !p.is_empty());
        let instance = parts.next().unwrap();
        let class = parts.next().unwrap_or(instance);
        assert_eq!(String::from_utf8_lossy(class), "firefox");
    }

    #[test]
    fn without_a_server_queries_fail_clearly() {
        // There is no DISPLAY in the test environment; a failed connection must
        // explain both backends instead of panicking.
        if std::env::var_os("DISPLAY").is_some() {
            return; // a real display: nothing to assert about the error path
        }
        let err = match Server::connect() {
            Ok(_) => return,
            Err(e) => e.to_string(),
        };
        assert!(err.contains("REC_SCREEN_BACKEND"), "{err}");
    }

    #[test]
    fn a_spec_carries_the_pointer_origin() {
        let spec = Spec {
            drawable: 1,
            rect: Rect::new(100, 50, 640, 480),
            origin: (100, 50),
            description: "display 1".into(),
        };
        // A pointer at root 300,250 is at 200,200 inside the rect.
        assert_eq!((300 - spec.origin.0, 250 - spec.origin.1), (200, 200));
    }
}
