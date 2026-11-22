//! The camera portal: xdg-desktop-portal's `org.freedesktop.portal.Camera`.
//!
//! A Flatpak app cannot open `/dev/video*`; the portal asks the user once, then
//! hands over a PipeWire remote (a file descriptor with only the camera nodes
//! visible). The nodes are video streams that have to be converted and stamped
//! like a V4L2 buffer, so this module reuses the same converters and clock and
//! differs only in where the pixels come from.
//!
//! The sequence is `Camera::new` → `IsCameraPresent` → `AccessCamera` (the
//! one-time dialog) → `OpenPipeWireRemote`, then enumerate one connection for
//! the node list and stream from a fresh one. The portal also serves
//! non-Flatpak apps that have a portal backend; [`in_sandbox`] only decides
//! whether a missing `/dev/video*` should fall back here on its own.
//!
//! The stream asks for BGRx, YUY2, NV12 or I420 (the formats a portal camera
//! normally offers) and delivers BGRA like every other source. Because the
//! portal does not report a size before the stream negotiates one, the source
//! starts with a 1280x720 guess (upstream's session preset) and corrects itself
//! from the first frame.
//!
//! Adapted from rec-rs src/camera.rs (MIT OR Apache-2.0), whose AVCapture
//! session this replaces, and from ashpd's PipeWire camera example.

use std::cell::RefCell;
use std::io::Cursor;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use ashpd::desktop::camera::Camera;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::spa::param::ParamType;
use pw::spa::param::format::{MediaSubtype, MediaType};
use pw::spa::param::format_utils;
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Pod, Value};
use pw::spa::utils::{Direction, Fraction, Rectangle, SpaTypes};

use super::convert::{self, CameraClock, YuvMatrix};
use super::v4l2::FrameSizeCell;
use super::{Backend, CameraSource};
use crate::media_types::{CameraInfo, FrameSink, VideoFrame, now};

/// The portal's runtime. One for the process, like the screen portal's: ashpd
/// caches its D-Bus connection in a static and its tasks live on the runtime
/// that created it.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .thread_name("rec-camera-portal")
            .build()
            .expect("building the camera portal runtime")
    })
}

/// True inside a Flatpak sandbox, where `/dev/video*` is usually not mounted and
/// the camera portal is the only access.
pub(super) fn in_sandbox() -> bool {
    std::env::var_os("FLATPAK_ID").is_some() || std::path::Path::new("/.flatpak-info").exists()
}

/// One camera node of the portal's PipeWire remote.
#[derive(Clone, Debug, PartialEq)]
struct PortalNode {
    node_id: u32,
    name: String,
    description: String,
}

/// What a running portal camera needs to reconnect: the remote's fd (a fresh
/// clone per connection) and the node to stream from.
#[derive(Clone)]
pub(super) struct PortalCamera {
    fd: Arc<OwnedFd>,
    node_id: Option<u32>,
}

fn portal_error(e: ashpd::Error) -> anyhow::Error {
    anyhow!("the camera portal: {e}")
}

fn proxy() -> Result<Camera<'static>> {
    runtime()
        .block_on(async { Camera::new().await })
        .map_err(portal_error)
}

fn present(camera: &Camera<'static>) -> Result<bool> {
    runtime()
        .block_on(async { camera.is_present().await })
        .map_err(portal_error)
}

fn open_remote(camera: &Camera<'static>) -> Result<OwnedFd> {
    runtime()
        .block_on(async { camera.open_pipe_wire_remote().await })
        .map_err(portal_error)
}

/// The cameras the portal exposes. The portal asks for access only on the first
/// session; listing may come back empty before that, in which case the portal
/// itself is offered as a single "portal" camera (a stream without a node id).
pub(super) fn cameras() -> Result<Vec<CameraInfo>> {
    let camera = proxy()?;
    if !present(&camera)? {
        return Ok(vec![]);
    }
    let nodes = match open_remote(&camera).and_then(enumerate_remote) {
        Ok(nodes) if !nodes.is_empty() => nodes,
        _ => vec![PortalNode {
            node_id: 0,
            name: "portal".into(),
            description: "Camera (portal)".into(),
        }],
    };
    Ok(nodes
        .into_iter()
        .map(|n| CameraInfo {
            id: n.name,
            name: n.description,
        })
        .collect())
}

/// One line for `rec-rs permissions` when there is no `/dev/video*`.
pub(super) fn status() -> Option<String> {
    match proxy() {
        Ok(camera) => match present(&camera) {
            Ok(true) => Some("ok (camera portal: a camera is present)".to_string()),
            Ok(false) => Some("unavailable (the camera portal reports no camera)".to_string()),
            Err(e) => Some(format!("unavailable ({e})")),
        },
        // Not a portal session: the caller says "no /dev/video*" instead.
        Err(_) => None,
    }
}

/// A portal camera source, or None when the portal has no camera (or is not
/// there at all) so the caller can report the usual "no camera found".
pub(super) fn open_source(sel: Option<&str>) -> Result<Option<CameraSource>> {
    let Ok(camera) = proxy() else {
        return Ok(None);
    };
    if !present(&camera)? {
        return Ok(None);
    }
    // `request_access` waits for the portal's response (ashpd prepares the
    // Response signal inside the call): the first run shows the desktop's
    // camera dialog, later runs return immediately. A refusal is a real error,
    // not "no camera found".
    runtime()
        .block_on(async { camera.request_access().await })
        .map_err(|e| anyhow!("the camera portal refused access: {e}"))?;
    let fd = open_remote(&camera)?;
    let nodes = enumerate_remote(fd)?;
    let chosen = match sel {
        Some(sel) => pick(&nodes, sel),
        None => nodes.first(),
    };
    let Some(node) = chosen else {
        return Ok(None);
    };
    // A fresh connection for the stream: the enumeration closed its own.
    let fd = open_remote(&camera)?;
    let native = (1280usize, 720usize);
    Ok(Some(CameraSource {
        id: node.name.clone(),
        name: node.description.clone(),
        backend: Backend::Portal(PortalCamera {
            fd: Arc::new(fd),
            node_id: Some(node.node_id),
        }),
        format_label: "portal camera, 720p until it negotiates".to_string(),
        native: Arc::new(Mutex::new(native)),
        sink: None,
        thread: None,
        stop: Arc::new(AtomicBool::new(false)),
        error: Arc::new(Mutex::new(None)),
        running: false,
        started: None,
    }))
}

/// Match a requested camera against the portal's nodes: id/name exactly, then a
/// name substring, then "portal".
fn pick<'a>(nodes: &'a [PortalNode], sel: &str) -> Option<&'a PortalNode> {
    let sel = sel.trim();
    let lower = sel.to_lowercase();
    nodes
        .iter()
        .find(|n| n.name == sel || n.description.to_lowercase() == lower)
        .or_else(|| {
            nodes
                .iter()
                .find(|n| n.description.to_lowercase().contains(&lower))
        })
        .or_else(|| {
            (lower == "portal" || lower == "default")
                .then(|| nodes.first())
                .flatten()
        })
}

/// Enumerate the nodes of a portal PipeWire remote. The fd is consumed: the
/// connection lives for the enumeration and then closes, which is what lets the
/// caller open a second one for the stream.
fn enumerate_remote(fd: OwnedFd) -> Result<Vec<PortalNode>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("camera-portal-list".into())
        .spawn(move || {
            let _ = tx.send(list_remote(fd));
        })
        .context("starting the portal camera enumeration")?;
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(r) => r,
        Err(_) => bail!("the camera portal's PipeWire remote did not answer within 5 s"),
    }
}

fn list_remote(fd: OwnedFd) -> Result<Vec<PortalNode>> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| anyhow!("creating a PipeWire main loop: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|e| anyhow!("creating a PipeWire context: {e}"))?;
    let core = context
        .connect_fd_rc(fd, None)
        .map_err(|e| anyhow!("opening the camera portal's PipeWire remote: {e}"))?;
    let registry = core
        .get_registry_rc()
        .map_err(|e| anyhow!("opening the camera portal's registry: {e}"))?;
    let nodes = Rc::new(RefCell::new(Vec::<PortalNode>::new()));
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let nodes = nodes.clone();
            move |global| {
                let Some(props) = global.props else {
                    return;
                };
                if props.get("media.role") != Some("Camera") {
                    return;
                }
                let name = props
                    .get("node.name")
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("portal-camera-{}", global.id));
                let description = props
                    .get("node.description")
                    .or_else(|| props.get("node.nick"))
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&name)
                    .to_string();
                nodes.borrow_mut().push(PortalNode {
                    node_id: global.id,
                    name,
                    description,
                });
            }
        })
        .register();
    let _core_listener = core
        .add_listener_local()
        .done({
            let mainloop = mainloop.clone();
            let core = core.clone();
            let stage = Rc::new(std::cell::Cell::new(0));
            move |id, _seq| {
                if id != 0 {
                    return;
                }
                match stage.get() {
                    0 => {
                        stage.set(1);
                        let _ = core.sync(2);
                    }
                    _ => mainloop.quit(),
                }
            }
        })
        .register();
    core.sync(1)
        .map_err(|e| anyhow!("synchronising with the portal's PipeWire remote: {e}"))?;
    let timer = mainloop.loop_().add_timer({
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });
    timer.update_timer(Some(Duration::from_secs(3)), None);
    mainloop.run();
    let out = std::mem::take(&mut *nodes.borrow_mut());
    Ok(out)
}

pub(super) fn capture_thread(
    cam: PortalCamera,
    sink: FrameSink,
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    ready: mpsc::Sender<Result<()>>,
    native: FrameSizeCell,
) {
    let result = run_capture(&cam, &sink, &stop, &ready, &native);
    if let Err(e) = result {
        let msg = format!("{e:#}");
        if let Ok(mut slot) = error.lock()
            && slot.is_none()
        {
            *slot = Some(msg.clone());
        }
        let _ = ready.send(Err(anyhow!("{msg}")));
    }
}

struct VideoState {
    info: VideoInfoRaw,
    clock: CameraClock,
    ready_sent: bool,
}

fn run_capture(
    cam: &PortalCamera,
    sink: &FrameSink,
    stop: &Arc<AtomicBool>,
    ready: &mpsc::Sender<Result<()>>,
    native: &FrameSizeCell,
) -> Result<()> {
    let fd = cam
        .fd
        .try_clone()
        .context("duplicating the camera portal's PipeWire fd")?;
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| anyhow!("creating a PipeWire main loop: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|e| anyhow!("creating a PipeWire context: {e}"))?;
    let core = context
        .connect_fd_rc(fd, None)
        .map_err(|e| anyhow!("opening the camera portal's PipeWire remote: {e}"))?;
    let props = properties! {
        *pw::keys::MEDIA_TYPE => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Camera",
        *pw::keys::NODE_NAME => "rec-rs-camera",
    };
    let stream = pw::stream::StreamBox::new(&core, "rec-rs-camera", props)
        .map_err(|e| anyhow!("creating the camera stream: {e}"))?;
    let state = Rc::new(RefCell::new(VideoState {
        info: VideoInfoRaw::new(),
        clock: CameraClock::default(),
        ready_sent: false,
    }));
    let _listener = stream
        .add_local_listener_with_user_data(state.clone())
        .state_changed({
            let mainloop = mainloop.downgrade();
            move |_stream, st, _old, new| match new {
                pw::stream::StreamState::Error(e) => {
                    fail(&mainloop, &format!("the camera portal stream failed: {e}"));
                    let _ = &st;
                }
                pw::stream::StreamState::Unconnected if st.borrow().ready_sent => {
                    fail(&mainloop, "the portal camera was disconnected");
                }
                _ => {}
            }
        })
        .param_changed(|_stream, st, id, param| {
            if id != ParamType::Format.as_raw() {
                return;
            }
            let Some(param) = param else {
                return;
            };
            let Ok((media_type, subtype)) = format_utils::parse_format(param) else {
                return;
            };
            if media_type != MediaType::Video || subtype != MediaSubtype::Raw {
                return;
            }
            let mut info = VideoInfoRaw::new();
            if info.parse(param).is_ok() {
                st.borrow_mut().info = info;
            }
        })
        .process({
            let sink = sink.clone();
            let ready = ready.clone();
            let native = native.clone();
            move |stream, st| on_process(stream, st, &sink, &ready, &native)
        })
        .register()
        .map_err(|e| anyhow!("listening to the camera stream: {e}"))?;

    // Ask for the uncompressed layouts a portal camera normally provides.
    let obj = pw::spa::pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pw::spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            MediaType::Video
        ),
        pw::spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBA,
            VideoFormat::YUY2,
            VideoFormat::NV12,
            VideoFormat::I420
        ),
        pw::spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 1280,
                height: 720
            },
            Rectangle {
                width: 160,
                height: 120
            },
            Rectangle {
                width: 1920,
                height: 1080
            }
        ),
        pw::spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 30, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: 60, denom: 1 }
        ),
    );
    let values = PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(obj))
        .map_err(|e| anyhow!("building the camera format: {e:?}"))?
        .0
        .into_inner();
    let pod =
        Pod::from_bytes(&values).ok_or_else(|| anyhow!("internal: malformed camera format"))?;
    let mut params = [pod];
    stream
        .connect(
            Direction::Input,
            cam.node_id,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| anyhow!("connecting the camera stream: {e}"))?;

    // A tick that notices the session stopping; the stream callbacks are the
    // only other wakeups.
    let tick = mainloop.loop_().add_timer({
        let mainloop = mainloop.clone();
        let stop = stop.clone();
        move |_| {
            if stop.load(Ordering::SeqCst) {
                mainloop.quit();
            }
        }
    });
    tick.update_timer(
        Some(Duration::from_millis(100)),
        Some(Duration::from_millis(100)),
    );

    mainloop.run();
    let st = state.borrow_mut();
    if !st.ready_sent
        && let Err(e) = ready.send(Err(anyhow!(
            "the portal camera stopped before its first frame"
        )))
    {
        let _ = e;
    }
    Ok(())
}

fn fail(mainloop: &pw::main_loop::MainLoopWeak, msg: &str) {
    eprintln!("rec: warning: {msg}");
    if let Some(mainloop) = mainloop.upgrade() {
        mainloop.quit();
    }
}

fn on_process(
    stream: &pw::stream::Stream,
    st: &mut Rc<RefCell<VideoState>>,
    sink: &FrameSink,
    ready: &mpsc::Sender<Result<()>>,
    native: &FrameSizeCell,
) {
    let raw = unsafe { stream.dequeue_raw_buffer() };
    if raw.is_null() {
        return;
    }
    let time_ns = unsafe { (*raw).time };
    let mut planes: Vec<(Vec<u8>, usize)> = Vec::new();
    unsafe {
        let buf = &*raw;
        if !buf.buffer.is_null() {
            let spa_buf = &*buf.buffer;
            if spa_buf.n_datas > 0 && !spa_buf.datas.is_null() {
                let datas = std::slice::from_raw_parts_mut(
                    spa_buf.datas as *mut spa::buffer::Data,
                    spa_buf.n_datas as usize,
                );
                for data in datas {
                    let chunk = data.chunk();
                    let size = chunk.size() as usize;
                    let offset = chunk.offset() as usize;
                    let stride = chunk.stride().unsigned_abs() as usize;
                    if let Some(bytes) = data.data() {
                        let end = (offset + size).min(bytes.len());
                        if end > offset {
                            planes.push((bytes[offset..end].to_vec(), stride));
                        }
                    }
                }
            }
        }
    }
    unsafe { stream.queue_raw_buffer(raw) };

    let info = {
        let s = st.borrow();
        s.info
    };
    let (width, height) = (info.size().width as usize, info.size().height as usize);
    if width == 0 || height == 0 || planes.is_empty() {
        return;
    }
    // No colourspace in the stream's format: HD camera YUV is BT.709, SD is
    // BT.601, and the packed formats carry RGB.
    let matrix = if height >= 720 {
        YuvMatrix::Bt709
    } else {
        YuvMatrix::Bt601
    };
    let converted = match info.format() {
        VideoFormat::BGRx => convert::bgrx_to_bgra(&planes[0].0, width, height, planes[0].1),
        VideoFormat::BGRA => {
            convert::packed_to_bgra(&planes[0].0, width, height, planes[0].1, false)
        }
        VideoFormat::RGBA => {
            convert::packed_to_bgra(&planes[0].0, width, height, planes[0].1, true)
        }
        VideoFormat::YUY2 => {
            convert::yuyv_to_bgra(&planes[0].0, width, height, planes[0].1, matrix)
        }
        VideoFormat::NV12 => {
            if let Some(uv) = planes.get(1) {
                convert::nv12_planes_to_bgra(
                    &planes[0].0,
                    planes[0].1,
                    &uv.0,
                    uv.1,
                    width,
                    height,
                    matrix,
                )
            } else {
                convert::nv12_to_bgra(&planes[0].0, width, height, planes[0].1, matrix)
            }
        }
        VideoFormat::I420 => match (planes.get(1), planes.get(2)) {
            (Some(u), Some(v)) => convert::i420_to_bgra(
                &planes[0].0,
                planes[0].1,
                &u.0,
                u.1,
                &v.0,
                v.1,
                width,
                height,
                matrix,
            ),
            _ => {
                if std::env::var_os("REC_DEBUG").is_some() {
                    eprintln!("rec: debug: I420 camera frame without its chroma planes");
                }
                return;
            }
        },
        other => {
            if std::env::var_os("REC_DEBUG").is_some() {
                eprintln!("rec: debug: camera portal chose unsupported {other:?}");
            }
            return;
        }
    };
    let Ok(data) = converted else {
        return;
    };
    let pts = st
        .borrow_mut()
        .clock
        .stamp_at(time_ns as f64 / 1e9, now(), now());
    let (ready_sent, first) = {
        let mut s = st.borrow_mut();
        let first = !s.ready_sent;
        s.ready_sent = true;
        (s.ready_sent, first)
    };
    if ready_sent && first {
        if let Ok(mut n) = native.lock() {
            *n = (width, height);
        }
        let _ = ready.send(Ok(()));
    }
    sink(VideoFrame::packed(
        pts,
        width,
        height,
        crate::media_types::PixelFormat::Bgra,
        data,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u32, name: &str, description: &str) -> PortalNode {
        PortalNode {
            node_id: id,
            name: name.into(),
            description: description.into(),
        }
    }

    #[test]
    fn portal_camera_matching() {
        let nodes = [
            node(7, "portal-camera-1", "Integrated Camera"),
            node(9, "portal-camera-2", "Logitech BRIO"),
        ];
        assert_eq!(pick(&nodes, "portal-camera-2").unwrap().node_id, 9);
        assert_eq!(pick(&nodes, "integrated").unwrap().node_id, 7);
        assert_eq!(pick(&nodes, "brio").unwrap().node_id, 9);
        assert_eq!(pick(&nodes, "portal").unwrap().node_id, 7);
        assert_eq!(pick(&nodes, "default").unwrap().node_id, 7);
        assert!(pick(&nodes, "no such camera").is_none());
    }
}
