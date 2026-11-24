//! The xdg-desktop-portal ScreenCast session: the picker, the restore token and
//! the PipeWire remote.
//!
//! This is the Linux stand-in for ScreenCaptureKit's `SCShareableContent`: the
//! compositor decides what a client may see, and the portal asks the user once
//! (then remembers, through a restore token). Two consequences shape the code
//! here:
//!
//! * **The picker is the only monitor selector.** `--display N` can only be
//!   honoured where the desktop exposes its monitors (X11), so on Wayland we
//!   run the handshake, learn what the user picked, and say so.
//! * **The pixel size is not the portal's size.** The portal reports the source
//!   in the compositor's coordinate space, which on a HiDPI desktop is logical
//!   units. `resolve()` needs pixels, so the session is probed once over
//!   PipeWire ([`super::pipewire::probe_size`]) and the real stream connects
//!   afterwards.
//!
//! A cancelled dialog, a missing portal, a revoked token and a picker that never
//! answers all end in a clear error with a non-zero exit — never in a hang.
//!
//! Adapted from rec-rs src/tcc.rs (MIT OR Apache-2.0), whose one-time Screen
//! Recording grant this replaces.

use std::os::fd::OwnedFd;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use ashpd::Error as PortalError;
use ashpd::desktop::ResponseError;
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType, Streams};
use ashpd::desktop::{PersistMode, Session};
use ashpd::enumflags2::BitFlags;
use futures_util::{
    StreamExt,
    future::{Either, select},
};

use super::{CaptureOptions, Want, pipewire, token};

/// The async runtime every portal call is driven on.
///
/// One runtime for the whole process, on purpose: `ashpd` caches its D-Bus
/// connection in a static, and that connection's tasks live on the runtime that
/// created it. A runtime per session would leave the second session talking
/// over a dead connection.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .thread_name("rec-portal")
            .build()
            .expect("building the portal runtime")
    })
}

/// How many `Prepared`s have been dropped inside the runtime, and whether the
/// drop saw a reactor (tests: see `portal_objects_are_dropped_inside_the_runtime`).
#[cfg(test)]
static DROPS_ON_RUNTIME: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static DROP_SAW_REACTOR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Run `fut` on the portal runtime from wherever the caller is.
///
/// Only the destructors of ashpd/zbus objects need this (see [`Prepared`]):
/// they reach for the Tokio reactor when the proxy is dropped, and a panic in a
/// destructor while unwinding aborts the process. If the caller is already
/// inside a runtime — the runtime's own worker, or a future that drops a
/// `Prepared` — `block_on` would panic ("Cannot start a runtime from within a
/// runtime"), so the work is spawned there instead.
fn runtime_scope<F>(fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let rt = runtime();
    match tokio::runtime::Handle::try_current() {
        Ok(_) => {
            rt.spawn(fut);
        }
        Err(_) => rt.block_on(fut),
    }
}

/// What the portal exposes, for `rec-rs permissions`.
#[derive(Clone, Debug)]
pub struct Availability {
    pub version: u32,
    pub monitors: bool,
    pub windows: bool,
    pub cursor_modes: Vec<&'static str>,
}

/// Ask the portal daemon what it can do, without showing a dialog.
///
/// The whole conversation (and the proxy's drop) happens inside one runtime
/// block: a zbus proxy dropped on a plain thread panics in its destructor with
/// "there is no reactor running".
pub fn available() -> Result<Availability> {
    runtime().block_on(async {
        let screencast = Screencast::new().await.map_err(portal_error)?;
        // `Screencast` derefs to the zbus proxy, whose cached `version` property
        // is the portal interface version (`AvailableCursorModes` needs >= 2).
        let version = screencast.get_property::<u32>("version").await.unwrap_or(0);
        let sources = screencast
            .available_source_types()
            .await
            .map_err(portal_error)?;
        // `AvailableCursorModes` needs portal v2; a v1 portal can still cast, it
        // just cannot tell us which modes it has, so assume the two v1 ones.
        let modes = screencast
            .available_cursor_modes()
            .await
            .unwrap_or(CursorMode::Hidden | CursorMode::Embedded);
        let mut cursor_modes = vec![];
        for (mode, name) in [
            (CursorMode::Hidden, "hidden"),
            (CursorMode::Embedded, "embedded"),
            (CursorMode::Metadata, "metadata"),
        ] {
            if modes.contains(mode) {
                cursor_modes.push(name);
            }
        }
        Ok(Availability {
            version,
            monitors: sources.contains(SourceType::Monitor),
            windows: sources.contains(SourceType::Window),
            cursor_modes,
        })
        // `screencast` drops here, inside the runtime.
    })
}

/// A session the portal has already started, with nothing connected to it yet.
///
/// `resolve()` needs its size before the writer is built, and `open()` needs the
/// same session afterwards, so this type is what travel between them.
pub struct Prepared {
    /// Kept alive for the session's lifetime: dropping the proxy ends it.
    ///
    /// Its owner returns both objects to the runtime for cleanup, including
    /// when preparation unwinds before this struct is constructed.
    _portal: PortalOwner,
    pub node_id: u32,
    pub fd: OwnedFd,
    /// The pixel size of the stream (probed over PipeWire).
    pub pixels: (usize, usize),
    /// The source's size in the compositor's coordinates, when the desktop
    /// reports one. Equal to `pixels` at scale 1.
    pub points: (usize, usize),
    pub want: Want,
    /// Why the cursor looks different from what `--no-cursor`/`--clicks` asked
    /// for, when the desktop could not do it.
    pub cursor_note: Option<String>,
    /// The stream offered a video format when probed. `false` means the node is
    /// not capturing, and only another session can fix it.
    pub probed: bool,
    /// One line about what the picker chose.
    pub picked: String,
}

impl Prepared {
    /// Another descriptor for the same PipeWire remote (the probe uses its own).
    pub fn dup_fd(&self) -> Result<OwnedFd> {
        self.fd
            .try_clone()
            .context("duplicating the PipeWire remote fd")
    }

    /// Close the portal session, which also stops the compositor capturing.
    pub fn close(self) {
        // Dropping is closing: see `Drop`.
        drop(self);
    }
}

/// Owns the portal objects as soon as they leave the handshake, including
/// during probing and unwinding before `Prepared` has been constructed.
struct PortalOwner {
    screencast: Option<Screencast<'static>>,
    session: Option<Session<'static, Screencast<'static>>>,
}

impl Drop for PortalOwner {
    fn drop(&mut self) {
        let session = self.session.take();
        let screencast = self.screencast.take();
        if session.is_none() && screencast.is_none() {
            return;
        }
        // The portal conversation ends here, on the runtime: close the session
        // (which stops the compositor capturing) and then drop both objects
        // where their destructors can reach the Tokio reactor.
        runtime_scope(async move {
            #[cfg(test)]
            {
                // Inside this future we are on the portal runtime by
                // construction; recording it lets the test prove the drop did
                // not happen on the caller's bare thread.
                DROP_SAW_REACTOR.store(
                    tokio::runtime::Handle::try_current().is_ok(),
                    std::sync::atomic::Ordering::SeqCst,
                );
                DROPS_ON_RUNTIME.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            if let Some(session) = session {
                let _ = session.close().await;
                drop(session);
            }
            drop(screencast);
        });
    }
}

/// Run the ScreenCast handshake, showing the picker only when there is no
/// usable restore token.
///
/// `timeout` bounds the dialog: a picker that is never answered must fail the
/// recording with a message, not hang it.
/// Run the ScreenCast handshake, and once more if the stream never negotiated.
///
/// A compositor can export the node before its own capture stream has a format
/// (`xdg-desktop-portal-wlr` does this under load). No consumer of that node
/// ever sees a format — a second connection, or six seconds of patience, does
/// not help — so the only recovery is a fresh portal session, which the restore
/// token makes silent. Recordings then start instead of failing with "no video
/// frames".
pub fn prepare(want: Want, opts: CaptureOptions, timeout: Duration) -> Result<Prepared> {
    prepare_with(want, opts, Some(timeout), &super::PortalCancel::default())
}

pub(super) fn prepare_with(
    want: Want,
    opts: CaptureOptions,
    portal_wait: Option<Duration>,
    cancel: &super::PortalCancel,
) -> Result<Prepared> {
    cancel.check()?;
    let prepared = prepare_session(want, opts, portal_wait, cancel, pipewire::PROBE_WAIT)?;
    cancel.check()?;
    if prepared.probed {
        return Ok(prepared);
    }
    eprintln!(
        "rec: the screen-cast stream offered no video format; asking the portal for a fresh session"
    );
    prepared.close();
    let fresh = prepare_session(want, opts, portal_wait, cancel, pipewire::PROBE_RETRY_WAIT)?;
    cancel.check()?;
    if !fresh.probed {
        fresh.close();
        // Two sessions, and not one `param` ever reached the stream: the
        // backend's nodes exist but this client is not shown their formats
        // (measured: the stream reaches `Paused` and stays silent, and every
        // node of the session behaves the same). Recording would write a file
        // with no video, three seconds later and with a vaguer message.
        bail!(
            "the screen-cast stream offered no video format on two sessions, so a recording \
             would have no video. The desktop's screen-cast backend (xdg-desktop-portal-wlr, \
             xdg-desktop-portal-gnome or xdg-desktop-portal-kde) is not delivering a stream: \
             check that PipeWire and the portal are running, and try `rec-rs permissions`"
        );
    }
    Ok(fresh)
}

/// Test hook: how many of the next sessions should look like a stream that
/// never negotiated a format (see `a_session_without_a_format_is_replaced`).
#[cfg(test)]
static FORCE_UNPROBED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static PROBE_SOCKET_INODE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn prepare_session(
    want: Want,
    opts: CaptureOptions,
    portal_wait: Option<Duration>,
    cancel: &super::PortalCancel,
    probe_wait: Duration,
) -> Result<Prepared> {
    #[cfg(test)]
    let forced_unprobed = FORCE_UNPROBED
        .try_update(
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
            |n| n.checked_sub(1),
        )
        .is_ok();
    let mut saved = token::load();
    let result = runtime().block_on(async {
        let screencast = Screencast::new().await.map_err(portal_error)?;
        // Ask what this desktop can actually do before asking it to do it: a
        // session created with an unsupported source type or cursor mode is
        // rejected outright, and the error says only "Unavailable cursor mode
        // 4", which helps nobody.
        let types = screencast
            .available_source_types()
            .await
            .map_err(portal_error)?;
        let mode_wanted = match want {
            Want::Monitor => SourceType::Monitor,
            Want::Window => SourceType::Window,
        };
        if !types.contains(mode_wanted) {
            let offer = match (
                types.contains(SourceType::Monitor),
                types.contains(SourceType::Window),
            ) {
                (true, false) => "monitors only",
                (false, true) => "windows only",
                _ => "no monitors and no windows",
            };
            return Err(anyhow!(
                "this desktop's screen-cast portal offers {offer}, so {} cannot be captured. \
                 Record the display (`--region` for part of it) instead",
                if want == Want::Window {
                    "a single window"
                } else {
                    "a display"
                }
            ));
        }
        let available = screencast
            .available_cursor_modes()
            .await
            .unwrap_or(CursorMode::Hidden | CursorMode::Embedded);
        // The chosen mode goes straight into the request; only a fallback is
        // worth telling the user about later.
        let (cursor, cursor_note) = choose_cursor(available, opts);
        let types = BitFlags::from(mode_wanted);
        let session = screencast.create_session().await.map_err(portal_error)?;
        // A stale token is retried without: the portal then asks again, which
        // is recoverable, while failing the recording is not.
        let mut select = screencast
            .select_sources(
                &session,
                cursor,
                types,
                false,
                saved.as_deref(),
                PersistMode::ExplicitlyRevoked,
            )
            .await;
        if let Err(e) = &select
            && saved.is_some()
        {
            // A revoked or expired grant, or a portal that never honoured it.
            // Ask again rather than fail the recording.
            saved = None;
            let _ = token::clear();
            eprintln!(
                "rec: the saved screen-capture grant was refused ({e}); the system picker will \
                 ask again"
            );
            select = screencast
                .select_sources(
                    &session,
                    cursor,
                    types,
                    false,
                    None,
                    PersistMode::ExplicitlyRevoked,
                )
                .await;
        }
        select.map_err(portal_error)?;
        let streams = match start_cancellable(&screencast, &session, portal_wait, cancel).await {
            Ok(streams) => streams,
            Err(error) => {
                let _ = session.close().await;
                return Err(error);
            }
        };
        let Some(stream) = streams.streams().first().cloned() else {
            // Drop the session here, on the runtime, before the error escapes.
            let _ = session.close().await;
            return Err(anyhow!(
                "the portal started a screen-cast session without a stream"
            ));
        };
        // The PipeWire remote belongs to the same conversation: opening it in a
        // second `block_on` used to leave the session and its proxy alive
        // across the boundary, to be dropped on a plain thread.
        let fd = screencast
            .open_pipe_wire_remote(&session)
            .await
            .map_err(portal_error)
            .context("opening the portal's PipeWire remote")?;
        // A dup shares the underlying socket, not just the portal session.
        // The probe stops its loop on Format, before teardown is flushed; a
        // retained duplicate keeps its half-negotiated server-side link alive,
        // blocking other graph links (including the camera). Close a separate
        // portal connection completely before starting any capture streams.
        let probe_fd = screencast
            .open_pipe_wire_remote(&session)
            .await
            .map_err(portal_error)
            .context("opening the portal's PipeWire probe remote")?;
        let owner = PortalOwner {
            screencast: Some(screencast),
            session: Some(session),
        };
        Ok::<_, anyhow::Error>((owner, streams, stream, fd, probe_fd, cursor_note))
    });
    // The owner guards the session and proxy immediately upon leaving the
    // runtime, including if probing or constructing Prepared unwinds.
    let (owner, streams, stream, fd, probe_fd, cursor_note) = result?;
    #[cfg(test)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(format!("/proc/self/fd/{}", probe_fd.as_raw_fd()))?;
        PROBE_SOCKET_INODE.store(metadata.ino(), std::sync::atomic::Ordering::SeqCst);
    }
    if let Some(t) = streams.restore_token()
        && let Err(e) = token::store(t)
    {
        eprintln!("rec: warning: could not save the screen-capture grant: {e}");
    }
    let node_id = stream.pipe_wire_node_id();
    let reported = stream
        .size()
        .map(|(w, h)| (w.max(2) as usize, h.max(2) as usize))
        .unwrap_or((1920, 1080));
    // The compositor's coordinates, in pixels: what PipeWire will negotiate.
    #[cfg(test)]
    let probed_by_hook = probed_hook(forced_unprobed);
    #[cfg(not(test))]
    let probed_by_hook = true;
    let (pixels, probed) = match pipewire::probe_size(probe_fd, node_id, probe_wait) {
        Ok(size) => (size, true),
        Err(e) => {
            if super::debug() {
                eprintln!("rec: debug: could not probe the stream size ({e}); using the portal's");
            }
            (reported, false)
        }
    };
    let (points, matched) = match_points(stream.position(), reported, pixels);
    let source = match stream.source_type() {
        Some(SourceType::Window) => "window",
        _ => "monitor",
    };
    let picked = match matched {
        Some(i) => format!(
            "the {} the system picker chose ({}, display {i} of the desktop)",
            if source == "window" {
                "window"
            } else {
                "monitor"
            },
            size_line(pixels)
        ),
        None => format!(
            "the {} the system picker chose ({})",
            if source == "window" {
                "window"
            } else {
                "monitor"
            },
            size_line(pixels)
        ),
    };
    Ok(Prepared {
        _portal: owner,
        node_id,
        fd,
        pixels,
        points,
        want: if source == "window" {
            Want::Window
        } else {
            Want::Monitor
        },
        cursor_note,
        probed: probed && probed_by_hook,
        picked,
    })
}

/// Own the pending Request even if its response future is dropped or unwinds.
/// Cleanup is always driven by the process-lifetime portal runtime, never by a
/// caller's (possibly shutting-down) runtime. A broken portal cannot hang drop.
struct PendingRequest(Option<zbus::Proxy<'static>>);

impl PendingRequest {
    async fn close(&mut self) {
        if let Some(request) = self.0.take() {
            close_request(request).await;
        }
    }
}

async fn close_request(request: zbus::Proxy<'static>) {
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        let _: zbus::Result<()> = request.call("Close", &()).await;
    })
    .await;
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        if let Some(request) = self.0.take() {
            runtime_scope(close_request(request));
        }
    }
}

/// ashpd 0.12's `start` hides the Request until Response arrives. Use the
/// documented wire protocol here so cancellation can close the *pending*
/// Request, not merely drop its response future and leave the picker open.
async fn start_cancellable(
    screencast: &Screencast<'_>,
    session: &Session<'_, Screencast<'_>>,
    wait: Option<Duration>,
    cancel: &super::PortalCancel,
) -> Result<Streams> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    cancel.check()?;
    let token = format!("rec_start_{}", NEXT.fetch_add(1, Ordering::Relaxed));
    let connection = screencast.connection();
    let sender = connection
        .unique_name()
        .context("portal connection has no unique name")?
        .as_str()
        .trim_start_matches(':')
        .replace('.', "_");
    let path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
    let request = zbus::Proxy::new_owned(
        connection.clone(),
        "org.freedesktop.portal.Desktop",
        path,
        "org.freedesktop.portal.Request",
    )
    .await?;
    let mut pending_request = PendingRequest(Some(request.clone()));
    let mut responses = request.receive_signal("Response").await?;
    let mut options = std::collections::HashMap::new();
    options.insert("handle_token", zbus::zvariant::Value::from(token.as_str()));
    let response = async {
        let _: zbus::zvariant::OwnedObjectPath =
            screencast.call("Start", &(session, "", options)).await?;
        let message = responses
            .next()
            .await
            .context("screen-capture portal closed without a response")?;
        let (code, streams): (u32, Streams) = message.body().deserialize()?;
        match code {
            0 => Ok(streams),
            1 => Err(portal_error(PortalError::Response(
                ResponseError::Cancelled,
            ))),
            _ => Err(portal_error(PortalError::Response(ResponseError::Other))),
        }
    };
    let interrupted = async {
        let cancelled = cancel.cancelled();
        let deadline = async {
            match wait {
                Some(duration) => tokio::time::sleep(duration).await,
                None => std::future::pending::<()>().await,
            }
        };
        match select(Box::pin(cancelled), Box::pin(deadline)).await {
            Either::Left(_) => anyhow!("screen capture startup cancelled"),
            Either::Right(_) => anyhow!(
                "the screen-capture dialog did not answer within {} s; run `rec-rs \
                 permissions --request` or check that the desktop portal is running",
                wait.unwrap().as_secs()
            ),
        }
    };
    match select(Box::pin(response), Box::pin(interrupted)).await {
        Either::Left((result, _)) => {
            if result.is_ok() {
                // Response completed the Request; only the Session stays live.
                pending_request.0.take();
            } else {
                pending_request.close().await;
            }
            result
        }
        Either::Right((error, pending)) => {
            // Drop the response future and all proxies here, on the runtime.
            drop(pending);
            pending_request.close().await;
            Err(error)
        }
    }
}

/// Pick the cursor mode the desktop can do, and explain anything it cannot.
///
/// `Embedded` is what upstream's "cursor shown" means: the compositor paints
/// the pointer into the stream, with its own theme and no sampling error.
/// `Metadata` carries the pointer's position and its bitmap separately, which is
/// the only way to *move* the cursor in the picture ourselves — but no portal
/// sends mouse button state, so it cannot make `--clicks` work either. A mode
/// the portal does not support fails the whole session, so ask first.
fn choose_cursor(
    available: BitFlags<CursorMode>,
    opts: CaptureOptions,
) -> (CursorMode, Option<String>) {
    let can = |m: CursorMode| available.contains(m);
    if !opts.cursor {
        if can(CursorMode::Hidden) {
            return (CursorMode::Hidden, None);
        }
        return (
            shown_mode(&available),
            Some(
                "this desktop's portal cannot hide the cursor (it offers no hidden cursor mode), \
                 so it is recorded"
                    .to_string(),
            ),
        );
    }
    let note = opts.clicks.then(|| {
        "desktop portals do not report mouse buttons, so --clicks cannot highlight clicks in a          Wayland recording; the cursor itself is recorded (an X11 session highlights clicks)"
            .to_string()
    });
    (shown_mode(&available), note)
}

/// The best way to have the pointer visible: the compositor draws it, else we
/// composite its bitmap from the metadata, else it cannot be shown.
fn shown_mode(available: &BitFlags<CursorMode>) -> CursorMode {
    if available.contains(CursorMode::Embedded) {
        CursorMode::Embedded
    } else if available.contains(CursorMode::Metadata) {
        CursorMode::Metadata
    } else {
        CursorMode::Hidden
    }
}

/// The stream's logical size, and the display number it matches when the
/// desktop exposes its monitors (through XWayland's RandR, usually).
///
/// `position` is in the compositor's coordinate space, which is the same space
/// RandR reports under XWayland, so a match by position is a match by monitor.
fn match_points(
    position: Option<(i32, i32)>,
    reported: (usize, usize),
    pixels: (usize, usize),
) -> ((usize, usize), Option<usize>) {
    let Ok(x11) = super::x11::Server::connect() else {
        return (pixels, None);
    };
    let monitors = x11.monitors();
    if monitors.is_empty() {
        return (pixels, None);
    }
    if let Some(pos) = position
        && let Some((d, _)) = monitors
            .iter()
            .find(|(_, r)| r.x as i32 == pos.0 && r.y as i32 == pos.1)
    {
        // The monitor's RandR size is its size in the compositor's space; the
        // pixel size is what PipeWire said. A HiDPI monitor is the case where
        // the two differ, and exactly what `--region` needs.
        return (d.points, Some(d.index));
    }
    // No position (portal v1): the reported size is the logical one, unless it
    // is the pixel size, in which case treat it as unscaled.
    let points = if reported == pixels { pixels } else { reported };
    let matched = monitors
        .iter()
        .find(|(d, _)| d.points == points || d.pixels == points)
        .map(|(d, _)| d.index);
    (points, matched)
}

fn size_line((w, h): (usize, usize)) -> String {
    format!("{w}x{h} pixels")
}

/// A portal error as a message worth reading.
pub fn portal_error(e: PortalError) -> anyhow::Error {
    match e {
        PortalError::Response(ResponseError::Cancelled) => {
            anyhow!("screen capture was cancelled in the system dialog (nothing was recorded)")
        }
        PortalError::Response(ResponseError::Other) => anyhow!(
            "the portal refused the screen-cast request. On a Wayland desktop the compositor's \
             portal backend (xdg-desktop-portal-wlr, xdg-desktop-portal-gnome or \
             xdg-desktop-portal-kde) must be installed and running"
        ),
        PortalError::PortalNotFound(name) => anyhow!(
            "no screen-cast portal is available ({name} is missing): install \
             xdg-desktop-portal and a backend for your desktop"
        ),
        PortalError::Zbus(e) => anyhow!(
            "the desktop portal is not reachable over D-Bus ({e}); is xdg-desktop-portal running \
             in this session?"
        ),
        other => anyhow!("the desktop portal failed: {other}"),
    }
}

/// Test hook: a forced session reports "no format".
#[cfg(test)]
fn probed_hook(forced: bool) -> bool {
    !forced
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cancelled_dialog_says_so() {
        let e = portal_error(PortalError::Response(ResponseError::Cancelled)).to_string();
        assert!(e.contains("cancelled"), "{e}");
        assert!(e.contains("nothing was recorded"), "{e}");
        let e = portal_error(PortalError::Response(ResponseError::Other)).to_string();
        assert!(e.contains("portal backend"), "{e}");
    }

    #[test]
    fn without_a_session_bus_the_error_names_the_portal() {
        // The test environment has no session bus; the message must name the
        // daemon rather than a D-Bus error code.
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() {
            return;
        }
        let err = available().unwrap_err().to_string();
        assert!(err.contains("xdg-desktop-portal"), "{err}");
    }

    #[test]
    fn cursor_modes_fall_back_to_what_the_portal_offers() {
        let both: BitFlags<CursorMode> =
            CursorMode::Hidden | CursorMode::Embedded | CursorMode::Metadata;
        let wlr: BitFlags<CursorMode> = CursorMode::Hidden | CursorMode::Embedded;
        let shown = CaptureOptions {
            cursor: true,
            clicks: false,
        };
        // "Cursor shown" is the compositor's own cursor, not metadata.
        assert_eq!(choose_cursor(both, shown), (CursorMode::Embedded, None));
        assert_eq!(choose_cursor(wlr, shown), (CursorMode::Embedded, None));
        // --clicks cannot work over a portal (no button events), and says so
        // rather than silently doing nothing.
        let clicks = CaptureOptions {
            cursor: true,
            clicks: true,
        };
        let (mode, note) = choose_cursor(both, clicks);
        assert_eq!(mode, CursorMode::Embedded);
        assert!(note.unwrap().contains("--clicks"));
        // Hiding needs the hidden mode.
        assert_eq!(
            choose_cursor(
                wlr,
                CaptureOptions {
                    cursor: false,
                    clicks: false
                }
            ),
            (CursorMode::Hidden, None)
        );
        // A portal that only sends metadata: we composite the bitmap
        // ourselves, and cannot hide it.
        let only_meta: BitFlags<CursorMode> = CursorMode::Metadata.into();
        assert_eq!(
            choose_cursor(only_meta, shown),
            (CursorMode::Metadata, None)
        );
        let (mode, note) = choose_cursor(
            only_meta,
            CaptureOptions {
                cursor: false,
                clicks: false,
            },
        );
        assert_eq!(mode, CursorMode::Metadata);
        assert!(note.unwrap().contains("cannot hide"));
        // Nothing available at all: hidden, with no complaint.
        assert_eq!(
            choose_cursor(BitFlags::empty(), shown).0,
            CursorMode::Hidden
        );
    }

    // The portal tests share the forced-probe hook and drop observations.
    static PORTAL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn app_preparation_owns_its_session_and_opens_without_a_second_picker() {
        let _lock = PORTAL_TEST_LOCK.lock().unwrap();
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            return;
        }
        let cancel = super::super::PortalCancel::default();
        let prepared = super::super::prepare_for_app(
            &super::super::ScreenRequest::default(),
            &CaptureOptions::default(),
            None,
            &cancel,
        )
        .expect("app preparation");
        assert!(prepared.native.0 >= 2 && prepared.native.1 >= 2);
        let _ = prepared.target.describe();
        // The session is owned by this result, not CLI's global pending slot.
        assert!(super::super::PENDING.lock().unwrap().is_none());
        let (mut source, error) = prepared.open().expect("open prepared session");
        assert!(error.is_some());
        assert!(!source.is_running());
        source.stop().unwrap();
    }

    #[test]
    fn scoped_app_resolve_open_handoff_and_failed_options_cleanup() {
        let _lock = PORTAL_TEST_LOCK.lock().unwrap();
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            return; // scripts/screen/headless.sh, with REC_SCREEN_BACKEND=portal
        }
        let cancel = super::super::PortalCancel::default();
        let req = super::super::ScreenRequest::default();
        let mut source = super::super::with_app_portal(&cancel, || {
            let (_, native) = super::super::resolve(&req)?;
            assert!(native.0 >= 2 && native.1 >= 2);
            assert!(super::super::PENDING.lock().unwrap().is_none());
            let before = DROPS_ON_RUNTIME.load(std::sync::atomic::Ordering::SeqCst);
            let (source, _) = super::super::open_with_status(&req, &CaptureOptions::default())?;
            assert!(
                super::super::APP_PORTAL
                    .with(|slot| { slot.borrow().as_ref().unwrap().pending.is_none() })
            );
            assert_eq!(
                DROPS_ON_RUNTIME.load(std::sync::atomic::Ordering::SeqCst),
                before,
                "open must consume the prepared session, not replace it"
            );
            Ok(source)
        })
        .unwrap();
        source.stop().unwrap();
        let before = DROPS_ON_RUNTIME.load(std::sync::atomic::Ordering::SeqCst);
        let failed = super::super::with_app_portal::<()>(&cancel, || {
            super::super::resolve(&req)?;
            bail!("options failed after resolve")
        });
        assert_eq!(
            failed.unwrap_err().to_string(),
            "options failed after resolve"
        );
        assert!(
            DROPS_ON_RUNTIME.load(std::sync::atomic::Ordering::SeqCst) > before,
            "scope must close unused resolved session"
        );
        assert!(super::super::PENDING.lock().unwrap().is_none());
    }

    #[test]
    fn probe_and_capture_use_distinct_portal_connections() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;

        let _lock = PORTAL_TEST_LOCK.lock().unwrap();
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            return; // run inside scripts/screen/headless.sh
        }
        let prepared = prepare(
            Want::Monitor,
            CaptureOptions::default(),
            Duration::from_secs(20),
        )
        .expect("a portal session");
        assert!(prepared.probed, "the probe must negotiate a format");
        let capture_inode = std::fs::metadata(format!("/proc/self/fd/{}", prepared.fd.as_raw_fd()))
            .unwrap()
            .ino();
        let probe_inode = PROBE_SOCKET_INODE.load(std::sync::atomic::Ordering::SeqCst);
        assert_ne!(probe_inode, 0);
        assert_ne!(
            probe_inode, capture_inode,
            "dup shares a socket and leaves the probe link alive"
        );
        prepared.close();
    }

    /// A portal session whose stream never negotiated a format is replaced,
    /// not used: no consumer of such a node ever gets a format, so the session
    /// (not the stream) is what has to go. The `FORCE_UNPROBED` hook makes the
    /// first session look like that without needing a stalled compositor.
    #[test]
    fn a_session_without_a_format_is_replaced() {
        let _lock = PORTAL_TEST_LOCK.lock().unwrap();
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            return; // needs the headless harness (scripts/screen/headless.sh)
        }
        FORCE_UNPROBED.store(1, std::sync::atomic::Ordering::SeqCst);
        let prepared = prepare(
            Want::Monitor,
            CaptureOptions::default(),
            Duration::from_secs(20),
        )
        .expect("a portal session");
        assert!(
            prepared.probed,
            "the second session must have negotiated a format"
        );
        prepared.close();
        // The hook is spent: the next handshake is normal.
        assert_eq!(FORCE_UNPROBED.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// Every ashpd/zbus object must be created, used **and dropped** on the
    /// portal runtime. A zbus proxy dropped on a plain thread panics in its
    /// destructor with "there is no reactor running", and a panic in a
    /// destructor while unwinding aborts the process — a real desktop crash
    /// (exit 134) in the camera portal, and latent here until `available()` and
    /// `Prepared` were fixed.
    ///
    /// Both calls run on the bare test thread and drop whatever they created:
    /// `available()` its `Screencast`, and the `Prepared` below is dropped on a
    /// second thread that never had a runtime at all.
    #[test]
    fn portal_objects_are_dropped_inside_the_runtime() {
        let _lock = PORTAL_TEST_LOCK.lock().unwrap();
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            return; // needs the headless harness (scripts/screen/headless.sh)
        }
        let availability = available().expect("the portal reports its capabilities");
        assert!(availability.version > 0, "{availability:?}");
        let prepared = prepare(
            Want::Monitor,
            CaptureOptions::default(),
            Duration::from_secs(20),
        )
        .expect("a portal session");
        let before = DROPS_ON_RUNTIME.load(std::sync::atomic::Ordering::SeqCst);
        DROP_SAW_REACTOR.store(false, std::sync::atomic::Ordering::SeqCst);
        let dropped = std::thread::spawn(move || drop(prepared));
        if let Err(e) = dropped.join() {
            let message = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| (*s).to_string()))
                .unwrap_or_else(|| "no message".to_string());
            panic!("dropping a Prepared off the runtime panicked: {message}");
        }
        // The destructor's work ran, and it ran inside the runtime: without
        // that, the proxy's drop spawns a match-rule removal on an executor
        // that is not there.
        assert_eq!(
            DROPS_ON_RUNTIME.load(std::sync::atomic::Ordering::SeqCst),
            before + 1,
            "the session was not dropped on the runtime"
        );
        assert!(
            DROP_SAW_REACTOR.load(std::sync::atomic::Ordering::SeqCst),
            "the drop saw no Tokio reactor"
        );
    }

    /// Two sessions without a format are a refusal, not a silent recording.
    /// Before this check the second session was used anyway and the recording
    /// failed three seconds later with "no video frames were captured".
    #[test]
    fn two_sessions_without_a_format_are_refused() {
        let _lock = PORTAL_TEST_LOCK.lock().unwrap();
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            return; // needs the headless harness (scripts/screen/headless.sh)
        }
        FORCE_UNPROBED.store(2, std::sync::atomic::Ordering::SeqCst);
        let before = DROPS_ON_RUNTIME.load(std::sync::atomic::Ordering::SeqCst);
        let err = match prepare(
            Want::Monitor,
            CaptureOptions::default(),
            Duration::from_secs(20),
        ) {
            Ok(_) => panic!("a stream that never negotiates must be refused"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("two sessions"), "{err}");
        assert!(err.contains("no video"), "{err}");
        // Both sessions were closed and dropped on the runtime, not left to the
        // caller's thread (the recovery path returns nothing, so its objects
        // must be cleaned up inside the block).
        assert!(
            DROPS_ON_RUNTIME.load(std::sync::atomic::Ordering::SeqCst) >= before + 2,
            "the refused sessions were not dropped on the runtime"
        );
        assert!(DROP_SAW_REACTOR.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// A dialog that never answers is an error path inside the same runtime
    /// block: the session and the proxy must be dropped there, and the call
    /// must not panic or hang.
    #[test]
    fn a_timed_out_picker_is_an_error_without_a_crash() {
        let _lock = PORTAL_TEST_LOCK.lock().unwrap();
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            return; // needs the headless harness (scripts/screen/headless.sh)
        }
        let err = match prepare(Want::Monitor, CaptureOptions::default(), Duration::ZERO) {
            Ok(prepared) => {
                prepared.close();
                panic!("a zero timeout must not produce a session");
            }
            Err(e) => e.to_string(),
        };
        assert!(err.contains("did not answer"), "{err}");
    }

    /// Run against scripts/screen/fake-pending-portal.py on a private bus.
    /// The fake never answers Start: None must wait for our explicit cancel,
    /// while Some must retain the CLI timeout diagnostic.
    #[test]
    fn pending_picker_waits_until_cancelled_and_bounded_wait_still_times_out() {
        if std::env::var_os("REC_TEST_PENDING_PORTAL").is_none() {
            return;
        }
        let cancel = super::super::PortalCancel::default();
        let worker_cancel = cancel.clone();
        let started = std::time::Instant::now();
        let worker = std::thread::spawn(move || {
            super::super::with_app_portal(&worker_cancel, || {
                super::super::resolve(&super::super::ScreenRequest::default())
            })
            .expect_err("cancelled startup")
        });
        std::thread::sleep(Duration::from_millis(350));
        assert!(
            !worker.is_finished(),
            "unbounded picker returned before cancellation"
        );
        cancel.cancel();
        let error = worker.join().expect("no reactor/drop panic");
        assert_eq!(error.to_string(), "screen capture startup cancelled");
        assert!(started.elapsed() < Duration::from_secs(5));
        let error = prepare(
            Want::Monitor,
            CaptureOptions::default(),
            Duration::from_millis(50),
        )
        .err()
        .expect("bounded startup times out");
        assert!(
            error
                .to_string()
                .starts_with("the screen-capture dialog did not answer within 0 s;")
        );
    }

    /// Abort a pending response future: its Request guard must still close on
    /// the long-lived runtime, even when dropped by a non-runtime thread.
    #[test]
    fn pending_request_drop_is_safe_off_runtime() {
        if std::env::var_os("REC_TEST_PENDING_PORTAL").is_none() {
            return;
        }
        let guard = runtime().block_on(async {
            let screencast = Screencast::new().await.unwrap();
            let session = screencast.create_session().await.unwrap();
            let mut options = std::collections::HashMap::new();
            options.insert("handle_token", zbus::zvariant::Value::from("rec_drop_test"));
            let path: zbus::zvariant::OwnedObjectPath = screencast
                .call("Start", &(&session, "", options))
                .await
                .unwrap();
            let proxy = zbus::Proxy::new_owned(
                screencast.connection().clone(),
                "org.freedesktop.portal.Desktop",
                path,
                "org.freedesktop.portal.Request",
            )
            .await
            .unwrap();
            PendingRequest(Some(proxy))
        });
        std::thread::spawn(move || drop(guard)).join().unwrap();
    }

    #[test]
    fn cancellation_is_sticky_and_wakes_existing_and_late_waiters() {
        let cancel = super::super::PortalCancel::default();
        let clone = cancel.clone();
        assert!(clone.check().is_ok());
        runtime().block_on(async {
            let waiting = clone.cancelled();
            let trigger = async {
                cancel.cancel();
            };
            futures_util::future::join(waiting, trigger).await;
            tokio::time::timeout(Duration::from_millis(50), clone.cancelled())
                .await
                .unwrap();
        });
        assert_eq!(
            clone.check().unwrap_err().to_string(),
            "screen capture startup cancelled"
        );
        assert!(prepare_with(Want::Monitor, CaptureOptions::default(), None, &clone).is_err());
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(size_line((2560, 1440)), "2560x1440 pixels");
    }
}
