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
use pw::spa::pod::Pod;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::utils::{Direction, SpaTypes};

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

// Everything ashpd/zbus hands out (`Camera`, its `Request`, the proxies) must
// be created, used and **dropped inside one `runtime().block_on`**. zbus's
// executor reaches for the Tokio 1.x reactor when a proxy or request is
// dropped; a Camera returned from block_on and dropped on a plain capture
// thread panics there, and a panic in a destructor while cleaning up aborts the
// process (seen on a real desktop: exit 134 during camera open). Each function
// below therefore finishes its whole portal conversation inside the future and
// lets only plain data (booleans, fds, node descriptions) escape.

/// The cameras the portal exposes. The portal asks for access only on the first
/// session; listing may come back empty before that, in which case the portal
/// itself is offered as a single "portal" camera (a stream without a node id).
pub(super) fn cameras() -> Result<Vec<CameraInfo>> {
    let fd = runtime().block_on(async {
        let camera = Camera::new().await.map_err(portal_error)?;
        if !camera.is_present().await.map_err(portal_error)? {
            return Ok::<Option<OwnedFd>, anyhow::Error>(None);
        }
        let fd = camera.open_pipe_wire_remote().await.map_err(portal_error)?;
        Ok(Some(fd))
        // `camera` (and the proxy inside it) drops here, on the runtime.
    })?;
    let Some(fd) = fd else {
        return Ok(vec![]);
    };
    let nodes = match enumerate_remote(fd) {
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
    runtime().block_on(async {
        match Camera::new().await {
            Ok(camera) => match camera.is_present().await {
                Ok(true) => Some("ok (camera portal: a camera is present)".to_string()),
                Ok(false) => Some("unavailable (the camera portal reports no camera)".to_string()),
                Err(e) => Some(format!("unavailable ({e})")),
            },
            // Not a portal session: the caller says "no /dev/video*" instead.
            Err(_) => None,
        }
        // `camera` drops here, inside the runtime.
    })
}

/// A portal camera source, or None when the portal has no camera (or is not
/// there at all) so the caller can report the usual "no camera found".
pub(super) fn open_source(sel: Option<&str>) -> Result<Option<CameraSource>> {
    // One portal conversation, entirely inside the runtime: the Camera, the
    // AccessCamera Request and their proxies are dropped before the future
    // returns. Two remote fds come out: one to enumerate (consumed by the
    // watcher thread), one kept for the stream.
    let fds = runtime().block_on(async {
        let camera = match Camera::new().await {
            Ok(camera) => camera,
            Err(_) => return Ok::<_, anyhow::Error>(None),
        };
        if !camera.is_present().await.map_err(portal_error)? {
            return Ok(None);
        }
        // `request_access` waits for the portal's response (ashpd prepares the
        // Response signal inside the call): the first run shows the desktop's
        // camera dialog, later runs return immediately. A refusal is a real
        // error, not "no camera found". The returned Request is dropped here.
        camera
            .request_access()
            .await
            .map_err(|e| anyhow!("the camera portal refused access: {e}"))?;
        let list = camera.open_pipe_wire_remote().await.map_err(portal_error)?;
        let stream = camera.open_pipe_wire_remote().await.map_err(portal_error)?;
        Ok(Some((list, stream)))
        // `camera` drops here, inside the runtime.
    })?;
    let Some((list_fd, stream_fd)) = fds else {
        return Ok(None);
    };
    let nodes = enumerate_remote(list_fd)?;
    let chosen = match sel {
        Some(sel) => pick(&nodes, sel),
        None => nodes.first(),
    };
    let Some(node) = chosen else {
        return Ok(None);
    };
    let fd = stream_fd;
    let native = (1280usize, 720usize);
    Ok(Some(CameraSource {
        id: node.name.clone(),
        name: node.description.clone(),
        backend: Backend::Portal(PortalCamera {
            fd: Arc::new(fd),
            node_id: Some(node.node_id),
        }),
        format_label: "portal camera".to_string(),
        native: Arc::new(Mutex::new(native)),
        sink: None,
        thread: None,
        stop: Arc::new(AtomicBool::new(false)),
        error: Arc::new(Mutex::new(None)),
        running: false,
        started: None,
    }))
}

/// A running watch on the portal's PipeWire camera nodes.
pub(super) struct Watch {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watch {
    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

/// Watch the camera nodes of the portal's PipeWire remote.
///
/// Inside a Flatpak `/dev/video*` is not visible, so inotify cannot see a
/// camera being plugged in. The portal's remote is an ordinary PipeWire
/// connection as far as the registry is concerned: a newly created camera node
/// (media.role=Camera) arrives as a `global` event, and unplugging it as
/// `global_remove`. The watch opens its own remote (no AccessCamera here: the
/// app must not show a permission dialog just because it started; the grant is
/// persistent once a recording has used the camera) and coalesces the burst a
/// hotplug produces into one callback.
///
/// Every zbus object is dropped inside the runtime, like the rest of this
/// module.
pub(super) fn watch_devices(cb: impl Fn() + Send + 'static) -> Result<Watch> {
    let fd = runtime().block_on(async {
        let camera = Camera::new().await.map_err(portal_error)?;
        let fd = camera.open_pipe_wire_remote().await.map_err(portal_error)?;
        Ok::<OwnedFd, anyhow::Error>(fd)
        // `camera` drops here, inside the runtime.
    })?;
    let (ready_tx, ready_rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let thread = std::thread::Builder::new()
        .name("camera-portal-watch".into())
        .spawn(move || watch_thread(fd, cb, thread_stop, ready_tx))
        .context("starting the portal camera watch")?;
    match ready_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(())) => Ok(Watch {
            stop,
            thread: Some(thread),
        }),
        Ok(Err(e)) => bail!("{e}"),
        Err(_) => bail!("the portal camera watch did not start within 3 s"),
    }
}

fn watch_thread(
    fd: OwnedFd,
    cb: impl Fn() + Send + 'static,
    stop: Arc<AtomicBool>,
    ready: mpsc::Sender<Result<(), String>>,
) {
    pw::init();
    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(m) => m,
        Err(e) => {
            let _ = ready.send(Err(format!("creating a PipeWire main loop: {e}")));
            return;
        }
    };
    let context = match pw::context::ContextRc::new(&mainloop, None) {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(format!("creating a PipeWire context: {e}")));
            return;
        }
    };
    let core = match context.connect_fd_rc(fd, None) {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(format!(
                "opening the camera portal's PipeWire remote: {e}"
            )));
            return;
        }
    };
    let registry = match core.get_registry_rc() {
        Ok(r) => r,
        Err(e) => {
            let _ = ready.send(Err(format!("opening the camera portal's registry: {e}")));
            return;
        }
    };
    let ids = Rc::new(RefCell::new(std::collections::HashSet::<u32>::new()));
    let dirty = Rc::new(std::cell::Cell::new(false));
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let ids = ids.clone();
            let dirty = dirty.clone();
            move |global| {
                if global.type_ != pw::types::ObjectType::Node {
                    return;
                }
                let Some(props) = global.props else {
                    return;
                };
                // The portal marks its camera nodes with media.role=Camera;
                // media.class Video/Source is the same node's type.
                let camera = props.get("media.role") == Some("Camera")
                    || (props.get("media.class") == Some("Video/Source")
                        && props.get("node.name").is_some_and(|n| n.contains("camera")));
                if camera {
                    ids.borrow_mut().insert(global.id);
                    dirty.set(true);
                }
            }
        })
        .global_remove({
            let ids = ids.clone();
            let dirty = dirty.clone();
            move |id| {
                if ids.borrow_mut().remove(&id) {
                    dirty.set(true);
                }
            }
        })
        .register();
    let timer = mainloop.loop_().add_timer({
        let mainloop = mainloop.clone();
        let stop = stop.clone();
        let dirty = dirty.clone();
        move |_| {
            if stop.load(Ordering::SeqCst) {
                mainloop.quit();
                return;
            }
            if dirty.replace(false) {
                cb();
            }
        }
    });
    timer.update_timer(
        Some(Duration::from_millis(200)),
        Some(Duration::from_millis(200)),
    );
    if ready.send(Ok(())).is_err() {
        return;
    }
    mainloop.run();
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

/// The EnumFormat parameters the camera stream offers: one pod per raw layout
/// the converters know. No size, no frame rate: the camera's own default is
/// what it can actually deliver.
fn format_pods() -> Vec<Vec<u8>> {
    use pw::spa::param::format::FormatProperties;
    use pw::spa::pod::{Object, Property, Value};
    use pw::spa::utils::Id;

    [
        VideoFormat::BGRx,
        VideoFormat::BGRA,
        VideoFormat::RGBA,
        VideoFormat::YUY2,
        VideoFormat::NV12,
        VideoFormat::I420,
    ]
    .into_iter()
    .filter_map(|format| {
        let object = Object {
            type_: SpaTypes::ObjectParamFormat.as_raw(),
            id: ParamType::EnumFormat.as_raw(),
            properties: vec![
                Property::new(
                    FormatProperties::MediaType.as_raw(),
                    Value::Id(Id(MediaType::Video.as_raw())),
                ),
                Property::new(
                    FormatProperties::MediaSubtype.as_raw(),
                    Value::Id(Id(MediaSubtype::Raw.as_raw())),
                ),
                Property::new(
                    FormatProperties::VideoFormat.as_raw(),
                    Value::Id(Id(format.as_raw())),
                ),
            ],
        };
        let values = PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(object))
            .ok()?
            .0
            .into_inner();
        Some(values)
    })
    .collect()
}

/// Buffer requirements, like GStreamer's pipewiresrc sends: without this the
/// v4l2 camera source is handed a client-node port with zero buffers and
/// rejects the link ("error use input buffers: -22").
fn buffers_pod() -> Vec<u8> {
    use pw::spa::pod::{ChoiceValue, Object, Property, Value};
    use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags};

    let range = |default: i32, min: i32, max: i32| {
        Value::Choice(ChoiceValue::Int(Choice::<i32>(
            ChoiceFlags::empty(),
            ChoiceEnum::<i32>::Range { default, min, max },
        )))
    };
    let mem_types = (1 << pw::spa::sys::SPA_DATA_MemFd) | (1 << pw::spa::sys::SPA_DATA_MemPtr);
    let data_type = Value::Choice(ChoiceValue::Int(Choice::<i32>(
        ChoiceFlags::empty(),
        ChoiceEnum::<i32>::Flags {
            default: mem_types,
            flags: vec![mem_types],
        },
    )));
    let object = Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![
            Property::new(pw::spa::sys::SPA_PARAM_BUFFERS_buffers, range(8, 2, 32)),
            Property::new(
                pw::spa::sys::SPA_PARAM_BUFFERS_blocks,
                range(0, 1, i32::MAX),
            ),
            Property::new(pw::spa::sys::SPA_PARAM_BUFFERS_size, range(0, 1, i32::MAX)),
            Property::new(
                pw::spa::sys::SPA_PARAM_BUFFERS_stride,
                range(0, 0, i32::MAX),
            ),
            Property::new(pw::spa::sys::SPA_PARAM_BUFFERS_dataType, data_type),
        ],
    };
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(object))
        .expect("serialising the camera Buffers pod")
        .0
        .into_inner()
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
            move |_stream, st, old, new| {
                if std::env::var_os("REC_DEBUG").is_some() {
                    eprintln!("rec: debug: portal camera stream {old:?} -> {new:?}");
                }
                match new {
                    pw::stream::StreamState::Error(e) => {
                        fail(&mainloop, &format!("the camera portal stream failed: {e}"));
                        let _ = &st;
                    }
                    pw::stream::StreamState::Unconnected if st.borrow().ready_sent => {
                        fail(&mainloop, "the portal camera was disconnected");
                    }
                    _ => {}
                }
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
                if std::env::var_os("REC_DEBUG").is_some() {
                    let size = info.size();
                    let fps = info.framerate();
                    eprintln!(
                        "rec: debug: portal camera format {:?} {}x{} @ {}/{} range {} matrix {}",
                        info.format(),
                        size.width,
                        size.height,
                        fps.num,
                        fps.denom,
                        info.color_range(),
                        info.color_matrix()
                    );
                }
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

    // Offer the layouts one pod at a time, like the screen-cast path: a node
    // advertises one discrete format per parameter, and a single pod carrying
    // a Choice/Range selection makes the client-node negotiate a format whose
    // buffer size never gets resolved (the server then reports "use input
    // buffers: -22" and the camera dies before its first frame). Size and
    // frame rate are left to the camera, exactly like the screen stream.
    let mut pods = format_pods();
    pods.push(buffers_pod());
    let mut params: Vec<&Pod> = pods.iter().filter_map(|p| Pod::from_bytes(p)).collect();
    stream
        .connect(
            Direction::Input,
            cam.node_id,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::ALLOC_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
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
    fn the_stream_offers_raw_formats_and_an_explicit_buffers_param() {
        use pw::spa::pod::Value;
        use pw::spa::pod::deserialize::PodDeserializer;

        let formats = format_pods();
        assert_eq!(formats.len(), 6, "BGRx, BGRA, RGBA, YUY2, NV12, I420");
        for bytes in &formats {
            let (_, value) =
                PodDeserializer::deserialize_from::<Value>(bytes).expect("format pod deserializes");
            let Value::Object(object) = value else {
                panic!("format pod is not an object");
            };
            assert_eq!(object.type_, SpaTypes::ObjectParamFormat.as_raw());
            assert!(
                object.properties.iter().any(|p| {
                    p.key == pw::spa::param::format::FormatProperties::VideoFormat.as_raw()
                }),
                "a format pod without Video:format"
            );
        }
        // A v4l2 source rejects a client-node port with no Buffers param
        // ("error use input buffers: -22"); the camera stream must send one.
        let bytes = buffers_pod();
        let (_, value) =
            PodDeserializer::deserialize_from::<Value>(&bytes).expect("buffers pod deserializes");
        let Value::Object(object) = value else {
            panic!("buffers pod is not an object");
        };
        assert_eq!(object.type_, SpaTypes::ObjectParamBuffers.as_raw());
        assert_eq!(object.id, ParamType::Buffers.as_raw());
        for key in [
            pw::spa::sys::SPA_PARAM_BUFFERS_buffers,
            pw::spa::sys::SPA_PARAM_BUFFERS_blocks,
            pw::spa::sys::SPA_PARAM_BUFFERS_size,
            pw::spa::sys::SPA_PARAM_BUFFERS_stride,
            pw::spa::sys::SPA_PARAM_BUFFERS_dataType,
        ] {
            assert!(
                object.properties.iter().any(|p| p.key == key),
                "missing buffers property {key}"
            );
        }
    }

    #[test]
    #[ignore = "needs a D-Bus session (scripts/audio/integration.sh portal)"]
    fn portal_objects_are_dropped_inside_the_runtime() {
        // A session bus without a Camera backend still constructs the `Camera`
        // zbus proxy and then drops it on the error path. That destructor needs
        // the Tokio reactor: before the fix it ran on the bare test thread and
        // panicked with "there is no reactor running" (an abort in the real
        // Flatpak). Reaching the end of these calls without a panic is the
        // regression test.
        let _ = cameras();
        let _ = status();
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
