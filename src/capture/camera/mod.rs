//! Webcam capture (phase 1B).
//!
//! A camera is opened through V4L2 (`/dev/video*`) and delivers BGRA
//! [`VideoFrame`]s on the host clock; inside Flatpak, where there is no
//! `/dev/video*`, the xdg-desktop-portal Camera interface hands over a PipeWire
//! node instead ([`portal`]). Both paths run through the same [`CameraSource`],
//! so the session sees one source with one size and one settle time.
//!
//! # What phase 1C/1G need from here
//!
//! - [`open_source`] instead of [`open`]: it returns the concrete source, whose
//!   [`CameraSource::native_size`] and [`CameraSource::aspect`] are known from
//!   format negotiation (before the first frame) so the compositor can lay the
//!   overlay out, and [`CameraSource::settle_remaining`] is how long a
//!   cold-started camera still needs before its exposure has settled.
//! - [`CAMERA_SETTLE_SECS`] is upstream's 2 s: a freshly started camera looks
//!   dark while auto-exposure converges, and the file should not open on those
//!   frames. The writer's `wait_for_camera` gate takes the number, exactly like
//!   upstream's `CAMERA_SETTLE_SECS`.
//! - Frames are already BGRA and well formed; mirroring and shapes are the
//!   compositor's job (see `compose`), not the source's.
//!
//! Device names and ids are what `rec-rs devices` prints: the node path is the
//! id, the driver's card name the display name. `--webcam` accepts either or a
//! name substring, plus `default`/empty for the first camera.
//!
//! Adapted from rec-rs src/camera.rs (MIT OR Apache-2.0), with AVCapture
//! replaced by V4L2 and the camera portal.

// The factory surface is called by `record.rs`, `devices` and the app; the
// settle/aspect helpers are the phase 1C/1G API.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::media_types::{CameraInfo, FrameSink, VideoSource};

mod convert;
mod format;
mod portal;
mod v4l2;

/// The phase that implements this module.
pub const PHASE: &str = "1B";

/// How long a fresh camera needs for auto-exposure and white balance to settle
/// before the file starts (upstream measured 94% of the final brightness after
/// 1.5 s and 98% after 2 s on a MacBook camera).
pub const CAMERA_SETTLE_SECS: f64 = 2.0;

/// How long `start` waits for the first frame.
const START_TIMEOUT: Duration = Duration::from_secs(5);

/// Which camera to open.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CameraRequest {
    /// Node path, camera number, or name substring; None = the default (first)
    /// camera.
    pub device: Option<String>,
}

/// A running camera hotplug watch (phase 1E's camera list refresh).
///
/// Two backends run side by side: inotify on `/dev/video*` for a normal
/// desktop, and the portal's PipeWire camera nodes when the sandbox hides the
/// device nodes. Dropping it stops both.
pub struct DeviceWatch {
    inotify: Option<v4l2::Watch>,
    portal: Option<portal::Watch>,
}

impl DeviceWatch {
    /// Stop watching and join the watcher threads.
    pub fn stop(mut self) {
        if let Some(w) = self.inotify.take() {
            w.stop();
        }
        if let Some(w) = self.portal.take() {
            w.stop();
        }
    }
}

impl std::fmt::Debug for DeviceWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("camera::DeviceWatch")
            .field("inotify", &self.inotify.is_some())
            .field("portal", &self.portal.is_some())
            .finish()
    }
}

/// Call `cb` when a camera appears or disappears: an inotify watch on
/// `/dev/video*` for a normal desktop, and the portal's PipeWire camera nodes
/// when `/dev/video*` is not visible (a Flatpak). Both run when both are
/// available; each coalesces its own burst, and the callback runs on a watcher
/// thread, so it must not block (reload the camera list, wake the UI).
///
/// This is what makes a camera plugged in after the app started appear without
/// a manual refresh: 1E already calls it and reloads on the callback.
///
/// The watch lives until the returned [`DeviceWatch`] is dropped. It is an
/// error only when neither backend can be started. Pair it with
/// [`crate::capture::audio::watch_devices`].
pub fn watch_devices(cb: impl Fn() + Send + Sync + 'static) -> Result<DeviceWatch> {
    let cb = std::sync::Arc::new(cb);
    // A normal desktop: /dev/video* nodes. In a Flatpak this watch exists but
    // never fires for a camera, which is why the portal watch is needed too.
    let inotify = v4l2::watch_dir(std::path::Path::new("/dev"), {
        let cb = cb.clone();
        move || cb()
    })
    .ok();
    // A Flatpak (or a portal-only camera): the nodes of the portal's remote.
    let portal = portal::watch_devices({
        let cb = cb.clone();
        move || cb()
    })
    .ok();
    if inotify.is_none() && portal.is_none() {
        bail!("camera hotplug is unavailable: inotify on /dev and the camera portal both failed");
    }
    Ok(DeviceWatch { inotify, portal })
}

/// The cameras `--webcam` and the app list.
pub fn cameras() -> Result<Vec<CameraInfo>> {
    let mut out: Vec<CameraInfo> = v4l2::devices()
        .into_iter()
        .map(|d| CameraInfo {
            id: d.id(),
            name: d.name,
        })
        .collect();
    // Inside Flatpak `/dev/video*` is usually hidden; the portal is then the
    // only way to see a camera.
    if out.is_empty() && portal::in_sandbox() {
        out = portal::cameras().unwrap_or_default();
    }
    Ok(dedup_cameras(out))
}

/// One entry per camera.
///
/// The portal can expose the same v4l2 node more than once while a previous
/// remote connection is winding down (two connections, one camera), and both
/// entries carry the same node name. Sorting also makes the list stable for
/// the app's sidebar.
fn dedup_cameras(mut cameras: Vec<CameraInfo>) -> Vec<CameraInfo> {
    cameras.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    cameras.dedup_by(|a, b| a.id == b.id);
    cameras
}

/// Start capturing `req`. The returned source is stopped; the session calls
/// [`VideoSource::start`] with its frame sink.
pub fn open(req: &CameraRequest) -> Result<Box<dyn VideoSource>> {
    Ok(Box::new(open_source(req)?) as Box<dyn VideoSource>)
}

/// The concrete source, for the compositor's size/aspect and the writer's
/// settle gate (phase 1C/1G).
pub fn open_source(req: &CameraRequest) -> Result<CameraSource> {
    let devices = v4l2::devices();
    let requested = req
        .device
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "default");
    if let Some(dev) = v4l2::resolve(&devices, requested) {
        let (choice, negotiated) = v4l2::probe(&dev.path)
            .with_context(|| format!("camera {} ({})", dev.name, dev.id()))?;
        let native = (negotiated.width as usize, negotiated.height as usize);
        return Ok(CameraSource {
            id: dev.id(),
            name: dev.name.clone(),
            backend: Backend::V4l2 {
                path: dev.path.clone(),
                negotiated,
            },
            format_label: choice.label(),
            native: Arc::new(Mutex::new(native)),
            sink: None,
            thread: None,
            stop: Arc::new(AtomicBool::new(false)),
            error: Arc::new(Mutex::new(None)),
            running: false,
            started: None,
        });
    }
    if portal::in_sandbox() || requested.is_some() {
        // Flatpak without device nodes, or a portal-only camera selected.
        if let Some(source) = portal::open_source(requested)? {
            return Ok(source);
        }
    }
    match requested {
        Some(sel) => bail!("camera \u{201c}{sel}\u{201d} is not connected (see `rec-rs devices`)"),
        None => {
            bail!("no camera found (see `rec-rs devices`; a Flatpak build needs the camera portal)")
        }
    }
}

/// One line for `rec-rs permissions`.
pub fn permission_status() -> String {
    let devices = v4l2::devices();
    if !devices.is_empty() {
        return format!(
            "ok ({} camera{})",
            devices.len(),
            if devices.len() == 1 { "" } else { "s" }
        );
    }
    match portal::status() {
        Some(s) => s,
        None if portal::in_sandbox() => {
            "unavailable (the camera portal is not reachable; `/dev/video*` is not mounted)"
                .to_string()
        }
        None => "unavailable (no /dev/video* capture device)".to_string(),
    }
}

enum Backend {
    V4l2 {
        path: PathBuf,
        negotiated: v4l2::Negotiated,
    },
    /// A portal camera is a PipeWire node in the portal's own remote.
    Portal(portal::PortalCamera),
}

/// A stopped or running camera.
pub struct CameraSource {
    id: String,
    name: String,
    backend: Backend,
    /// The negotiated format, e.g. "1280x720 YUYV @ 30 fps".
    format_label: String,
    /// What the frames really are, updated when the first one arrives.
    native: v4l2::FrameSizeCell,
    sink: Option<FrameSink>,
    thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    running: bool,
    started: Option<Instant>,
}

impl CameraSource {
    /// The `/dev/video*` path (or portal node id) `--webcam` accepts.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The camera's display name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The frame size the stream delivers (known from negotiation, before the
    /// first frame).
    pub fn native_size(&self) -> (usize, usize) {
        *self.native.lock().unwrap()
    }

    /// The camera's aspect ratio: the compositor fits its overlay to this.
    pub fn aspect(&self) -> f64 {
        let (w, h) = self.native_size();
        if w == 0 || h == 0 {
            16.0 / 9.0
        } else {
            w as f64 / h as f64
        }
    }

    /// The negotiated format, for `describe()` and the start log.
    pub fn format_label(&self) -> &str {
        &self.format_label
    }

    /// When this source started delivering frames (the settle clock's origin).
    pub fn started(&self) -> Option<Instant> {
        self.started
    }

    /// How much of [`CAMERA_SETTLE_SECS`] is left for a cold camera; 0 once it
    /// has settled (or before start, the full time).
    pub fn settle_remaining(&self) -> f64 {
        match self.started {
            Some(t) => (CAMERA_SETTLE_SECS - t.elapsed().as_secs_f64()).max(0.0),
            None => CAMERA_SETTLE_SECS,
        }
    }
}

impl VideoSource for CameraSource {
    fn describe(&self) -> String {
        format!("camera {} ({})", self.name, self.format_label)
    }

    fn start(&mut self, sink: FrameSink) -> Result<()> {
        if self.running {
            bail!("the camera is already running");
        }
        self.sink = Some(sink.clone());
        self.stop = Arc::new(AtomicBool::new(false));
        self.error = Arc::new(Mutex::new(None));
        let (ready_tx, ready_rx) = mpsc::channel();
        let stop = self.stop.clone();
        let error = self.error.clone();
        let native = self.native.clone();
        let name = self.name.clone();
        type Producer = Box<
            dyn FnOnce(
                    FrameSink,
                    Arc<AtomicBool>,
                    Arc<Mutex<Option<String>>>,
                    mpsc::Sender<Result<()>>,
                ) + Send,
        >;
        let (capture, thread_name): (Producer, &str) = match &self.backend {
            Backend::V4l2 { path, negotiated } => {
                let path = path.clone();
                let negotiated = *negotiated;
                (
                    Box::new(move |sink, stop, error, ready| {
                        v4l2::capture_thread(path, negotiated, sink, stop, error, ready, native);
                    }),
                    "camera-v4l2",
                )
            }
            Backend::Portal(cam) => {
                let cam = cam.clone();
                (
                    Box::new(move |sink, stop, error, ready| {
                        portal::capture_thread(cam, sink, stop, error, ready, native);
                    }),
                    "camera-portal",
                )
            }
        };
        self.thread = Some(
            std::thread::Builder::new()
                .name(thread_name.into())
                .spawn(move || capture(sink, stop, error, ready_tx))
                .context("starting the camera thread")?,
        );
        match ready_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok(())) => {
                self.running = true;
                self.started = Some(Instant::now());
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = self.stop();
                bail!("{e}");
            }
            Err(_) => {
                let _ = self.stop();
                bail!(
                    "camera {name} did not deliver a frame within {} s",
                    START_TIMEOUT.as_secs()
                )
            }
        }
    }

    fn stop(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take()
            && t.join().is_err()
        {
            self.running = false;
            bail!("the camera capture thread panicked");
        }
        self.running = false;
        match self.error.lock().unwrap().take() {
            Some(e) => Err(anyhow::anyhow!("{e}")),
            None => Ok(()),
        }
    }

    fn is_running(&self) -> bool {
        self.running
    }
}

impl Drop for CameraSource {
    fn drop(&mut self) {
        // A source dropped without stop must not leave a capture thread behind.
        if self.thread.is_some() {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn faux(native: (usize, usize)) -> CameraSource {
        CameraSource {
            id: "/dev/video0".into(),
            name: "Test Camera".into(),
            backend: Backend::V4l2 {
                path: PathBuf::from("/dev/video0"),
                negotiated: v4l2::Negotiated {
                    fourcc: format::fourcc_yuyv(),
                    width: native.0 as u32,
                    height: native.1 as u32,
                    stride: (native.0 * 2) as u32,
                    colorspace: v4l::format::Colorspace::Rec709,
                    fps: 30,
                },
            },
            format_label: format!("{}x{} YUYV @ 30 fps", native.0, native.1),
            native: Arc::new(Mutex::new(native)),
            sink: None,
            thread: None,
            stop: Arc::new(AtomicBool::new(false)),
            error: Arc::new(Mutex::new(None)),
            running: false,
            started: None,
        }
    }

    #[test]
    fn size_and_aspect_are_known_before_the_first_frame() {
        let cam = faux((1280, 720));
        assert_eq!(cam.native_size(), (1280, 720));
        assert!((cam.aspect() - 16.0 / 9.0).abs() < 1e-9);
        // A 4:3 camera keeps its shape for the overlay.
        let cam = faux((1024, 768));
        assert!((cam.aspect() - 4.0 / 3.0).abs() < 1e-9);
        // Not started yet: the full settle time, upstream's 2 s.
        assert_eq!(cam.settle_remaining(), CAMERA_SETTLE_SECS);
        assert_eq!(CAMERA_SETTLE_SECS, 2.0);
    }

    #[test]
    fn describe_names_the_camera_and_its_format() {
        let cam = faux((640, 480));
        let s = cam.describe();
        assert!(s.contains("Test Camera"), "{s}");
        assert!(s.contains("640x480 YUYV @ 30 fps"), "{s}");
        assert_eq!(cam.id(), "/dev/video0");
    }

    #[test]
    fn listing_cameras_without_one_is_empty_not_an_error() {
        // The CI host has no webcam; a machine with one lists it. Both are Ok.
        let cams = cameras().unwrap();
        for c in cams {
            assert!(!c.id.is_empty());
            assert!(!c.name.is_empty());
        }
    }

    #[test]
    fn duplicate_portal_nodes_collapse_to_one_entry() {
        let cams = vec![
            CameraInfo {
                id: "v4l2_input.pci-0000_04_00.3-usb-0_2_1.2".into(),
                name: "B525 HD Webcam (V4L2)".into(),
            },
            CameraInfo {
                id: "v4l2_input.pci-0000_04_00.3-usb-0_2_1.2".into(),
                name: "B525 HD Webcam (V4L2)".into(),
            },
            CameraInfo {
                id: "/dev/video3".into(),
                name: "Another Camera".into(),
            },
        ];
        let out = dedup_cameras(cams);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "/dev/video3", "sorted by name first");
        assert_eq!(out[1].id, "v4l2_input.pci-0000_04_00.3-usb-0_2_1.2");
    }

    #[test]
    #[ignore = "needs a camera portal with a camera (real desktop; run inside the Flatpak)"]
    fn the_camera_watch_reports_the_portal_camera() {
        // The public watch 1E uses: inotify has nothing to see inside the
        // Flatpak, so this proves the portal's PipeWire node watch fires.
        let (tx, rx) = std::sync::mpsc::channel();
        let watch = watch_devices(move || {
            let _ = tx.send(());
        })
        .expect("starting the camera watch");
        let got = rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok();
        drop(watch);
        assert!(got, "the camera watch did not report the portal camera");
    }

    #[test]
    fn an_unconnected_camera_is_a_clear_error() {
        // Only meaningful without a /dev/video node whose name matches.
        let err = match open_source(&CameraRequest {
            device: Some("definitely-not-a-camera-1b".into()),
        }) {
            Ok(_) => panic!("a camera that does not exist was opened"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("is not connected")
                || err.contains("no camera found")
                || err.contains("portal"),
            "{err}"
        );
    }
}
