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
            if m.width < 2 || m.height < 2 {
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
                    m.x.max(0) as usize,
                    m.y.max(0) as usize,
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
    let mut last = Instant::now();
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
        // frame. This is what keeps a still desktop cheap.
        let changed = previous
            .as_ref()
            .is_none_or(|p| p.len() != data.len() || p[..] != data[..]);
        if changed {
            let mut data = data;
            if opts.cursor || opts.clicks {
                let (px, py) = pointer.map(|(x, y, _)| (x, y)).unwrap_or((0, 0));
                if opts.cursor
                    && let Some((serial, image)) = server.cursor()
                    && cursor
                        .as_ref()
                        .is_none_or(|(s, _): &(u32, CursorImage)| *s != serial)
                {
                    cursor = Some((serial, image));
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

#[cfg(test)]
mod tests {
    use super::*;

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
