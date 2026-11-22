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
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::{PersistMode, Session};

use super::{Want, pipewire, token};

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

/// What the portal exposes, for `rec-rs permissions`.
#[derive(Clone, Debug)]
pub struct Availability {
    pub version: u32,
    pub monitors: bool,
    pub windows: bool,
    pub cursor_modes: Vec<&'static str>,
}

/// Ask the portal daemon what it can do, without showing a dialog.
pub fn available() -> Result<Availability> {
    let screencast = runtime()
        .block_on(async { Screencast::new().await })
        .map_err(portal_error)?;
    // `Screencast` derefs to the zbus proxy, whose cached `version` property is
    // the portal interface version (`AvailableCursorModes` needs >= 2).
    let version = runtime()
        .block_on(async { screencast.get_property::<u32>("version").await })
        .unwrap_or(0);
    let sources = runtime()
        .block_on(async { screencast.available_source_types().await })
        .map_err(portal_error)?;
    let modes = runtime()
        .block_on(async { screencast.available_cursor_modes().await })
        .map_err(portal_error)?;
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
}

/// A session the portal has already started, with nothing connected to it yet.
///
/// `resolve()` needs its size before the writer is built, and `open()` needs the
/// same session afterwards, so this type is what travel between them.
pub struct Prepared {
    /// Kept alive for the session's lifetime: dropping the proxy ends it.
    _screencast: Screencast<'static>,
    session: Session<'static, Screencast<'static>>,
    pub node_id: u32,
    pub fd: OwnedFd,
    /// The pixel size of the stream (probed over PipeWire).
    pub pixels: (usize, usize),
    /// The source's size in the compositor's coordinates, when the desktop
    /// reports one. Equal to `pixels` at scale 1.
    pub points: (usize, usize),
    pub want: Want,
    pub cursor_mode: CursorMode,
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
        let _ = runtime().block_on(async { self.session.close().await });
    }
}

/// Run the ScreenCast handshake, showing the picker only when there is no
/// usable restore token.
///
/// `timeout` bounds the dialog: a picker that is never answered must fail the
/// recording with a message, not hang it.
pub fn prepare(want: Want, cursor: CursorMode, timeout: Duration) -> Result<Prepared> {
    let types = match want {
        Want::Monitor => SourceType::Monitor,
        Want::Window => SourceType::Window,
    };
    let mut saved = token::load();
    let result = runtime().block_on(async {
        let screencast = Screencast::new().await.map_err(portal_error)?;
        let session = screencast.create_session().await.map_err(portal_error)?;
        // A stale token is retried without: the portal then asks again, which
        // is recoverable, while failing the recording is not.
        let mut select = screencast
            .select_sources(
                &session,
                cursor,
                ashpd::enumflags2::BitFlags::from(types),
                false,
                saved.as_deref(),
                PersistMode::ExplicitlyRevoked,
            )
            .await;
        if select.is_err() && saved.is_some() {
            saved = None;
            let _ = token::clear();
            eprintln!(
                "rec: the saved screen-capture grant is no longer valid; the system picker will \
                 ask again"
            );
            select = screencast
                .select_sources(
                    &session,
                    cursor,
                    ashpd::enumflags2::BitFlags::from(types),
                    false,
                    None,
                    PersistMode::ExplicitlyRevoked,
                )
                .await;
        }
        select.map_err(portal_error)?;
        let request = tokio::time::timeout(timeout, screencast.start(&session, None))
            .await
            .map_err(|_| {
                anyhow!(
                    "the screen-capture dialog did not answer within {} s; run `rec-rs \
                     permissions --request` or check that the desktop portal is running",
                    timeout.as_secs()
                )
            })?
            .map_err(portal_error)?;
        let streams = request.response().map_err(portal_error)?;
        Ok::<_, anyhow::Error>((screencast, session, streams))
    });
    let (screencast, session, streams) = result?;
    if let Some(t) = streams.restore_token()
        && let Err(e) = token::store(t)
    {
        eprintln!("rec: warning: could not save the screen-capture grant: {e}");
    }
    let Some(stream) = streams.streams().first().cloned() else {
        let _ = runtime().block_on(async { session.close().await });
        bail!("the portal started a screen-cast session without a stream");
    };
    let node_id = stream.pipe_wire_node_id();
    let fd = runtime()
        .block_on(async { screencast.open_pipe_wire_remote(&session).await })
        .map_err(portal_error)
        .context("opening the portal's PipeWire remote")?;
    let reported = stream
        .size()
        .map(|(w, h)| (w.max(2) as usize, h.max(2) as usize))
        .unwrap_or((1920, 1080));
    // The compositor's coordinates, in pixels: what PipeWire will negotiate.
    let pixels = match stream_size(&fd, node_id) {
        Ok(size) => size,
        Err(e) => {
            if super::debug() {
                eprintln!("rec: debug: could not probe the stream size ({e}); using the portal's");
            }
            reported
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
        _screencast: screencast,
        session,
        node_id,
        fd,
        pixels,
        points,
        want: if source == "window" {
            Want::Window
        } else {
            Want::Monitor
        },
        cursor_mode: cursor,
        picked,
    })
}

/// The pixel size the compositor will send.
fn stream_size(fd: &OwnedFd, node_id: u32) -> Result<(usize, usize)> {
    let dup = fd
        .try_clone()
        .context("duplicating the PipeWire remote fd")?;
    pipewire::probe_size(dup, node_id)
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
    fn sizes_read_naturally() {
        assert_eq!(size_line((2560, 1440)), "2560x1440 pixels");
    }
}
