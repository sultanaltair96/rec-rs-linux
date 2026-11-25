//! The PipeWire consumer of a portal screen-cast stream.
//!
//! `portal.rs` hands over a node id and a file descriptor on the portal's
//! private PipeWire remote; this module turns that into [`VideoFrame`]s.
//!
//! # Threading
//!
//! PipeWire delivers buffers on its own real-time thread, and the contract says
//! a source must not do work there. So the process callback copies the buffer
//! once (it is recycled as soon as the callback returns) and drops it into a
//! small bounded queue; a worker thread crops the region, draws the cursor and
//! the click rings, and calls the session's sink. When the queue is full the new
//! frame is dropped: the newest frame is what a viewer needs, and the writer
//! repeats the last one on its own grid anyway.
//!
//! # Negotiation
//!
//! We ask for packed 32-bit RGB (`BGRx`, then `BGRA`) and never for dmabuf: a
//! dmabuf frame would need the GPU importer phase 1C adds. A compositor that
//! offers only dmabuf is therefore an error that says so, not a black picture.
//! A resolution change is followed (the format callback updates the size), which
//! is what a window resize looks like on a portal stream.
//!
//! `spa_meta_region` (a compositor's "only this part changed" hint) is ignored:
//! every buffer is copied whole and `--region` is cropped out of it here, so a
//! damage hint would only double-crop. `spa_meta_header.pts` is used as the
//! capture stamp when the compositor sends one, otherwise the frame is stamped
//! on arrival — the same `CLOCK_MONOTONIC`, off by the transport delay.
//!
//! Adapted from rec-rs src/screen.rs (MIT OR Apache-2.0), which gets its frames
//! from ScreenCaptureKit instead of a PipeWire node.

use std::cell::RefCell;
use std::io::Cursor;
use std::os::fd::OwnedFd;
use std::rc::Rc;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use pipewire as pw;
use pw::proxy::ProxyT;
use pw::spa;
use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pw::spa::param::format_utils;
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::spa::pod::{Object, Pod, Property, Value};
use pw::spa::utils::{Id, SpaTypes};

use crate::media_types::{FrameSink, PixelFormat, VideoFrame, VideoSource};

use super::CaptureOptions;
use super::cursor::{CursorImage, draw_clicks};
use super::format::{self, Rect, pts_secs};

/// How many captured frames may wait for the processing thread before new ones
/// are dropped.
const QUEUE: usize = 3;

/// How long the first probe waits for the compositor to offer a format.
///
/// Short on purpose: a node that offers none is a node whose stream never got
/// its params through (measured under load: our stream reaches `Paused` and
/// receives no `param` at all, on every connection, until the whole portal and
/// PipeWire session is replaced). The portal session is what has to be replaced
/// first, so waiting here only delays that.
pub const PROBE_WAIT: Duration = Duration::from_secs(2);

/// How long the probe of the *second* session waits: a fresh handshake deserves
/// more patience before a recording is refused.
pub const PROBE_RETRY_WAIT: Duration = Duration::from_secs(5);

/// Longest cursor bitmap edge we accept. A cursor is 24-64 px.
const MAX_CURSOR: usize = 256;

/// Largest metadata area we will read a cursor bitmap out of.
const MAX_META: usize = 1 << 22;

/// PipeWire's `spa_meta_cursor`: where the pointer is, and optionally its
/// bitmap, sent when the portal session asked for `CursorMode::Metadata`.
///
/// Declared here rather than taken from the bindings so the field order (and
/// therefore the layout the C side writes) is visible next to the code that
/// reads it: `spa_meta_cursor` in spa/buffer/meta.h.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct SpaMetaCursor {
    id: u32,
    flags: u32,
    position_x: i32,
    position_y: i32,
    hotspot_x: i32,
    hotspot_y: i32,
    bitmap_offset: u32,
    reserved: u32,
}

impl spa::buffer::meta::Metadata for SpaMetaCursor {
    const META_TYPE: u32 = spa::sys::SPA_META_Cursor;
}

/// `spa_meta_cursor.flags`: the compositor has hidden the pointer right now.
const CURSOR_FLAG_HIDDEN: u32 = 1;

/// `spa_meta_bitmap`, which sits at `spa_meta_cursor.bitmap_offset`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct SpaMetaBitmap {
    format: u32,
    stride: i32,
    width: u32,
    height: u32,
    offset: u32,
}

/// One captured buffer on its way to the processing thread.
struct RawFrame {
    pts: f64,
    width: usize,
    height: usize,
    format: PixelFormat,
    stride: usize,
    data: Vec<u8>,
    /// The pointer's bitmap and its position in stream pixels.
    cursor: Option<(Arc<CursorImage>, f64, f64)>,
}

/// What the capture thread shares with its callbacks.
struct StreamData {
    format: VideoInfoRaw,
    target: Option<Rc<RefCell<TargetState>>>,
    was_streaming: bool,
    tx: SyncSender<RawFrame>,
    dropped: u64,
    /// The last cursor bitmap, reused while the compositor's cursor is
    /// unchanged (it re-sends one bitmap per frame otherwise).
    cursor: Option<(u32, Arc<CursorImage>)>,
    error: Arc<Mutex<Option<String>>>,
    /// Warned already that the compositor sends positions but no bitmap.
    warned_no_bitmap: bool,
    /// A buffer has been seen (debug log once).
    saw_buffer: bool,
    /// The probe leaves the loop as soon as a format arrived.
    quit_on_format: Option<pw::main_loop::MainLoopRc>,
    /// Leaves the loop when the stream fails, so a dead capture does not sit in
    /// `run()` until someone stops it.
    quit_on_error: Option<pw::main_loop::MainLoopRc>,
    /// Tells the probe's watchdog that the answer arrived (tests reuse it).
    probe_done: Option<std::sync::mpsc::Sender<()>>,
}

/// A running screen-cast stream.
pub struct Stream {
    node_id: u32,
    fd: Option<OwnedFd>,
    crop: Option<Rect>,
    opts: CaptureOptions,
    /// Asks the capture thread's PipeWire loop to leave `run()`.
    ///
    /// A channel rather than a `pw_main_loop *`: the callback runs *on the loop
    /// thread*, so the loop cannot be freed while it is being asked to quit. A
    /// stored pointer could: the loop thread frees its `MainLoopRc` when `run()`
    /// returns, and quitting a freed loop is undefined behaviour.
    quit: Option<pw::channel::Sender<()>>,
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
    description: String,
}

impl Stream {
    /// A stream for `node_id` on the portal's remote. `crop` is the `--region`
    /// rectangle in stream pixels.
    pub fn new(
        node_id: u32,
        fd: OwnedFd,
        crop: Option<Rect>,
        opts: CaptureOptions,
        description: String,
    ) -> Stream {
        Stream {
            node_id,
            fd: Some(fd),
            crop,
            opts,
            quit: None,
            stop: Arc::new(AtomicBool::new(false)),
            error: Arc::new(Mutex::new(None)),
            thread: None,
            description,
        }
    }

    /// Fatal capture failure, available before `stop()` joins the source.
    pub fn fatal_error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }

    pub(super) fn error_handle(&self) -> Arc<Mutex<Option<String>>> {
        self.error.clone()
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    fn begin(&mut self, sink: FrameSink) -> Result<()> {
        if self.thread.is_some() {
            bail!("starting a capture that is already running");
        }
        let fd = self
            .fd
            .take()
            .ok_or_else(|| anyhow!("the portal's PipeWire remote was already consumed"))?;
        self.stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = sync_channel::<RawFrame>(QUEUE);
        let (quit_tx, quit_rx) = pw::channel::channel::<()>();
        self.quit = Some(quit_tx);
        let stop = self.stop.clone();
        let error = self.error.clone();
        let error2 = error.clone();
        let node_id = self.node_id;
        let opts = self.opts;
        let crop = self.crop;
        // The processing thread owns the sink. It exits when the capture thread
        // drops the sending half, which happens after the loop returns.
        let worker = std::thread::Builder::new()
            .name("rec-screen".into())
            .spawn(move || process_frames(rx, sink, crop, opts))
            .context("spawning the screen frame thread")?;
        self.thread = Some(std::thread::spawn(move || {
            let r = run(node_id, fd, tx, quit_rx, &stop, &error);
            let _ = worker.join();
            if let Err(e) = r {
                let mut slot = error2.lock().unwrap();
                if slot.is_none() {
                    // Say it now: the session only learns about `stop()` at the
                    // very end.
                    eprintln!("rec: error: screen capture stopped: {e:#}");
                    *slot = Some(format!("{e:#}"));
                }
            }
            #[cfg(test)]
            LOOPS_EXITED.fetch_add(1, Ordering::SeqCst);
        }));
        Ok(())
    }

    fn halt(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(quit) = self.quit.take() {
            // A `send` nobody reads is harmless: the channel owns its pipe, so
            // this is a plain write, never a pointer into a freed loop. That is
            // what makes stopping a capture that already died safe.
            let _ = quit.send(());
        }
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

impl VideoSource for Stream {
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
        self.thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
            && self.fatal_error().is_none()
    }
}

/// The capture thread: connect to the portal's remote and pump buffers.
fn run(
    node_id: u32,
    fd: OwnedFd,
    tx: SyncSender<RawFrame>,
    quit: pw::channel::Receiver<()>,
    stop: &Arc<AtomicBool>,
    error: &Arc<Mutex<Option<String>>>,
) -> Result<()> {
    pw::init();
    let main = pw::main_loop::MainLoopRc::new(None).context("creating the PipeWire main loop")?;
    // The session's stop request leaves the loop through the loop's own event
    // source, so `halt()` never touches a loop it does not own. A stop that
    // arrived before this point is already queued and fires on the first
    // iteration; `stop` is checked too, for the case where the channel is not
    // the reason.
    let _quit = quit.attach(main.loop_(), {
        let main = main.clone();
        move |_| main.quit()
    });
    // Test hook: leave `run()` with the loop created and immediately freed, so
    // a test can stop a capture whose loop is already gone without waiting for
    // a compositor to drop a stream at the right moment. `cfg(test)` removes it
    // from every real build.
    #[cfg(test)]
    if EXIT_BEFORE_RUN.swap(false, Ordering::SeqCst) {
        return Ok(());
    }
    if stop.load(Ordering::SeqCst) {
        main.quit();
    }
    let context =
        pw::context::ContextRc::new(&main, None).context("creating a PipeWire context")?;
    let core = context
        .connect_fd_rc(fd, None)
        .map_err(|e| anyhow!("connecting to the portal's PipeWire remote: {e}"))?;
    let target = resolve_target(&core, &main, node_id, error.clone());
    if stop.load(Ordering::SeqCst) {
        return Ok(());
    }
    let (serial, target_watch) = target?;
    let stream = new_stream(&core, &serial)?;
    let data = StreamData {
        format: VideoInfoRaw::new(),
        tx,
        dropped: 0,
        cursor: None,
        error: error.clone(),
        warned_no_bitmap: false,
        saw_buffer: false,
        quit_on_format: None,
        target: Some(target_watch.state.clone()),
        was_streaming: false,
        quit_on_error: Some(main.clone()),
        probe_done: None,
    };
    let listener = stream
        .add_local_listener_with_user_data(data)
        .param_changed(on_param_changed)
        .process(on_process)
        .state_changed(|_, data, old, new| {
            if super::debug() {
                eprintln!("rec: debug: screen stream {old:?} -> {new:?}");
            }
            on_state(data, old, new);
        })
        .register()
        .context("registering the stream listener")?;
    connect_stream(&stream, node_id)?;
    if super::debug() {
        eprintln!("rec: debug: screen stream connected to node {node_id}");
    }
    main.run();
    if super::debug() {
        eprintln!("rec: debug: screen stream loop left run()");
    }
    drop(listener);
    let _ = stream.disconnect();
    Ok(())
}

fn new_stream(core: &pw::core::CoreRc, serial: &str) -> Result<pw::stream::StreamRc> {
    let props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Screen",
        "target.object" => serial,
        "node.dont-reconnect" => "true",
    };
    pw::stream::StreamRc::new(core.clone(), "rec-rs screen", props)
        .map_err(|e| anyhow!("creating a PipeWire stream: {e}"))
}

/// Offer the packed formats and connect to the portal's node.
fn connect_stream(stream: &pw::stream::Stream, node_id: u32) -> Result<()> {
    let pods = format_pods();
    let mut params: Vec<&Pod> = pods.iter().filter_map(|p| Pod::from_bytes(p)).collect();
    stream
        .connect(
            spa::utils::Direction::Input,
            None, // target.object is the stable object.serial, never a recycled id.
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::DONT_RECONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| anyhow!("connecting to the screen-cast node {node_id}: {e}"))
}

/// Keep the restricted portal registry alive for the entire connection. A
/// numeric global id is only used to find the portal's node; routing uses its
/// serial so a recycled id can never select a different producer.
struct TargetWatch {
    // Listeners must be dropped before the proxies they observe.
    _proxy_listener: pw::proxy::ProxyListener,
    _node: pw::node::Node,
    _listener: pw::registry::Listener,
    _registry: pw::registry::RegistryRc,
    _core_listener: pw::core::Listener,
    state: Rc<RefCell<TargetState>>,
}

#[derive(Default)]
struct TargetState {
    node_id: u32,
    serial: Option<String>,
    present: bool,
}

impl TargetState {
    fn remove(&mut self, id: u32) -> bool {
        // global_remove carries only an id. Preserve the serial of the exact
        // global we resolved, and never adopt a later global with a reused id.
        if id != self.node_id || self.serial.is_none() {
            return false;
        }
        self.present = false;
        true
    }

    fn removed_message(&self, event: &str) -> String {
        format!(
            "PipeWire screen-cast node {} (serial {}) was removed ({event})",
            self.node_id,
            self.serial.as_deref().unwrap_or("unknown")
        )
    }
}

fn target_removed(
    state: &Rc<RefCell<TargetState>>,
    id: u32,
    event: &str,
    error: &Arc<Mutex<Option<String>>>,
    main: &pw::main_loop::MainLoopRc,
) {
    let mut target = state.borrow_mut();
    let matched = target.remove(id);
    if super::debug() {
        eprintln!(
            "rec: debug: screen target {event}: id {id}, target {} serial {:?}, matched {matched}",
            target.node_id, target.serial
        );
    }
    if matched {
        set_error(error, target.removed_message(event));
        main.quit();
    }
}

fn resolve_target(
    core: &pw::core::CoreRc,
    main: &pw::main_loop::MainLoopRc,
    node_id: u32,
    error: Arc<Mutex<Option<String>>>,
) -> Result<(String, TargetWatch)> {
    let registry = core
        .get_registry_rc()
        .context("opening the portal PipeWire registry")?;
    let state = Rc::new(RefCell::new(TargetState {
        node_id,
        ..Default::default()
    }));
    let target_global = Rc::new(RefCell::new(None));
    let listener = registry
        .add_listener_local()
        .global({
            let state = state.clone();
            let target_global = target_global.clone();
            move |global| {
                let mut target = state.borrow_mut();
                if global.id == node_id
                    && global.type_ == pw::types::ObjectType::Node
                    && target.serial.is_none()
                {
                    target.serial = global
                        .props
                        .and_then(|props| props.get("object.serial"))
                        .filter(|s| s.parse::<u64>().is_ok())
                        .map(str::to_owned);
                    target.present = target.serial.is_some();
                    if target.present {
                        *target_global.borrow_mut() = Some(global.to_owned());
                    }
                    if super::debug() {
                        eprintln!(
                            "rec: debug: screen target registry.global: id {node_id}, serial {:?}",
                            target.serial
                        );
                    }
                }
            }
        })
        .global_remove({
            let main = main.clone();
            let error = error.clone();
            let state = state.clone();
            move |id| target_removed(&state, id, "registry.global_remove", &error, &main)
        })
        .register();
    let seq = core
        .sync(0)
        .context("synchronising the portal PipeWire registry")?;
    let core_listener = core
        .add_listener_local()
        .done({
            let main = main.clone();
            move |id, done| {
                if id == pw::core::PW_ID_CORE && done == seq {
                    main.quit();
                }
            }
        })
        .error({
            let main = main.clone();
            let error = error.clone();
            move |id, seq, res, message| {
                if super::debug() {
                    eprintln!(
                        "rec: debug: screen core.error: id {id}, seq {seq}, res {res}: {message}"
                    );
                }
                set_error(
                    &error,
                    format!("PipeWire stopped or its remote failed: {message}"),
                );
                main.quit();
            }
        })
        .register();
    let timer = main.loop_().add_timer({
        let main = main.clone();
        move |_| main.quit()
    });
    timer.update_timer(Some(PROBE_RETRY_WAIT), None);
    main.run();
    if let Some(message) = error.lock().unwrap().as_ref() {
        bail!("{message}");
    }
    let serial = state.borrow().serial.clone().ok_or_else(|| {
        anyhow!("PipeWire screen-cast node {node_id} has no object.serial in the portal registry")
    })?;
    let node: pw::node::Node = registry
        .bind(
            target_global
                .borrow()
                .as_ref()
                .expect("resolved target global"),
        )
        .context("binding the screen-cast target node")?;
    let proxy_listener = node
        .upcast_ref()
        .add_listener_local()
        .removed({
            let state = state.clone();
            let main = main.clone();
            let error = error.clone();
            move || target_removed(&state, node_id, "target proxy.removed", &error, &main)
        })
        .register();
    Ok((
        serial,
        TargetWatch {
            _proxy_listener: proxy_listener,
            _node: node,
            _registry: registry,
            _listener: listener,
            _core_listener: core_listener,
            state,
        },
    ))
}

fn on_state(data: &mut StreamData, old: pw::stream::StreamState, new: pw::stream::StreamState) {
    data.was_streaming |= matches!(old, pw::stream::StreamState::Streaming)
        || matches!(new, pw::stream::StreamState::Streaming);
    let message = match new {
        // Paused is normal for a live, idle producer. Only independent target
        // absence after streaming makes it terminal; never use a frame timeout.
        pw::stream::StreamState::Paused if data.was_streaming => {
            let Some(target) = &data.target else { return };
            let target = target.borrow();
            if target.present {
                return;
            }
            target.removed_message("stream Paused after Streaming; target absent")
        }
        pw::stream::StreamState::Error(e) => format!("PipeWire stream error: {e}"),
        pw::stream::StreamState::Unconnected
            if !matches!(old, pw::stream::StreamState::Unconnected) =>
        {
            "PipeWire screen-cast stream disconnected".to_owned()
        }
        _ => return,
    };
    set_error(&data.error, message);
    if let Some(done) = data.probe_done.take() {
        let _ = done.send(());
    }
    if let Some(main) = &data.quit_on_error {
        main.quit();
    }
}

fn set_error(slot: &Arc<Mutex<Option<String>>>, msg: String) {
    let mut e = slot.lock().unwrap();
    if e.is_none() {
        *e = Some(msg);
    }
}

fn on_param_changed(
    _stream: &pw::stream::Stream,
    data: &mut StreamData,
    id: u32,
    param: Option<&Pod>,
) {
    if !read_format(data, id, param) {
        return;
    }
    let size = data.format.size();
    if super::debug() {
        eprintln!(
            "rec: debug: screen stream format {:?} {}x{}",
            data.format.format(),
            size.width,
            size.height
        );
    }
    if let Some(main) = data.quit_on_format.take() {
        main.quit();
    }
}

/// Parse a `Format` parameter into `data.format`. True when it is a usable
/// raw-video format with a real size.
fn read_format(data: &mut StreamData, id: u32, param: Option<&Pod>) -> bool {
    if id != spa::param::ParamType::Format.as_raw() {
        return false;
    }
    let Some(param) = param else {
        // A NULL format clears the negotiated one.
        return false;
    };
    let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
        return false;
    };
    if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
        set_error(
            &data.error,
            format!(
                "the screen-cast stream offers {media_type:?}/{media_subtype:?}, not raw video"
            ),
        );
        return false;
    }
    if let Err(e) = data.format.parse(param) {
        set_error(
            &data.error,
            format!("cannot parse the screen-cast video format: {e}"),
        );
        return false;
    }
    let size = data.format.size();
    size.width > 0 && size.height > 0
}

fn on_process(stream: &pw::stream::Stream, data: &mut StreamData) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let size = data.format.size();
    let (width, height) = (size.width as usize, size.height as usize);
    if width == 0 || height == 0 {
        return;
    }
    let Some(format) = pixel_format(data.format.format()) else {
        set_error(
            &data.error,
            format!(
                "the screen-cast stream negotiates {:?}, which this build cannot consume; rec \
                 needs a packed XRGB/BGRX buffer from the compositor",
                data.format.format()
            ),
        );
        return;
    };
    // The compositor's own capture time when it sends one (wlr and mutter both
    // set a header pts); otherwise the buffer is here now, which is the same
    // clock.
    let now = crate::media_types::now();
    let pts = sane_pts(
        buffer
            .find_meta::<spa::buffer::meta::MetaHeader>()
            .and_then(|h| pts_secs(h.pts())),
        now,
    );
    let cursor = read_cursor(&buffer, data);
    let datas = buffer.datas_mut();
    if datas.is_empty() {
        return;
    }
    let d = &mut datas[0];
    let (offset, stride) = {
        let chunk = d.chunk();
        // `chunk.size()` may be short when only the damaged region was
        // filled, so copy the whole plane the format describes.
        (chunk.offset() as usize, chunk.stride())
    };
    let Some(bytes) = d.data() else {
        set_error(
            &data.error,
            format!(
                "the screen-cast stream sent a buffer without CPU memory (a DMA-BUF), and {}",
                crate::gpu::dmabuf::refusal(crate::gpu::dmabuf::probe_support())
            ),
        );
        return;
    };
    // A stride of 0 means "tightly packed". A negative one is a bottom-up
    // buffer: no screencast backend produces one, and misreading it would put
    // the picture together upside down, so say so.
    let row = width * 4;
    if stride < 0 {
        set_error(
            &data.error,
            format!("the screen-cast stream sent a bottom-up buffer (stride {stride})"),
        );
        return;
    }
    let stride = if stride > 0 { stride as usize } else { row };
    let plane = stride * height;
    if stride < row || plane > bytes.len().saturating_sub(offset) {
        set_error(
            &data.error,
            format!(
                "the screen-cast buffer is too small for {width}x{height} at stride {stride} \
                 ({} bytes)",
                bytes.len()
            ),
        );
        return;
    }
    let mut copy = vec![0u8; plane];
    copy.copy_from_slice(&bytes[offset..offset + plane]);
    if super::debug() && !data.saw_buffer {
        data.saw_buffer = true;
        eprintln!(
            "rec: debug: first screen buffer {width}x{height} stride {stride} plane {plane} \
             (chunk offset {offset}, pts {pts:.3})"
        );
    }
    let raw = RawFrame {
        pts,
        width,
        height,
        format,
        stride,
        data: copy,
        cursor,
    };
    match data.tx.try_send(raw) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => data.dropped += 1,
        Err(TrySendError::Disconnected(_)) => {}
    }
}

/// The cursor bitmap and position from `spa_meta_cursor`, with the bitmap
/// cached by id: a compositor re-sends the same bitmap with every frame.
fn read_cursor(
    buffer: &pw::buffer::Buffer<'_>,
    data: &mut StreamData,
) -> Option<(Arc<CursorImage>, f64, f64)> {
    let meta = buffer.find_meta::<SpaMetaCursor>()?;
    if meta.flags & CURSOR_FLAG_HIDDEN != 0 {
        return None;
    }
    let position = (meta.position_x as f64, meta.position_y as f64);
    if let Some((id, image)) = &data.cursor
        && *id == meta.id
    {
        return Some((image.clone(), position.0, position.1));
    }
    let Some(image) = cursor_bitmap(meta) else {
        if !data.warned_no_bitmap {
            data.warned_no_bitmap = true;
            super::warn_once(
                "the compositor sends the pointer's position but no cursor bitmap, so the cursor \
                 is not in the recording; without --no-cursor it usually draws the cursor itself",
            );
        }
        return None;
    };
    let image = Arc::new(image);
    data.cursor = Some((meta.id, image.clone()));
    Some((image, position.0, position.1))
}

/// Copy the bitmap out of the cursor meta, which is only valid for the
/// duration of the process callback.
fn cursor_bitmap(meta: &SpaMetaCursor) -> Option<CursorImage> {
    if meta.bitmap_offset == 0 {
        return None;
    }
    // The metadata lives in the mapped buffer; `bitmap_offset` is relative to
    // the metadata's start and the bitmap's own `offset` likewise.
    let base = meta as *const SpaMetaCursor as *const u8;
    if meta.bitmap_offset as usize + std::mem::size_of::<SpaMetaBitmap>() > MAX_META {
        return None;
    }
    let bitmap = unsafe { &*(base.add(meta.bitmap_offset as usize) as *const SpaMetaBitmap) };
    let (w, h, stride) = (bitmap.width as usize, bitmap.height as usize, bitmap.stride);
    // The bitmap lives inside a buffer the compositor sized: bound every field
    // before reading past the cursor struct.
    if w == 0
        || h == 0
        || w > MAX_CURSOR
        || h > MAX_CURSOR
        || stride < (w * 4) as i32
        || stride as usize > w * 4 + 4096
        || bitmap.offset as usize + stride as usize * h > MAX_META
    {
        return None;
    }
    // BGRA is what wlr and mutter send; anything else falls back to no cursor
    // rather than a scrambled one.
    let format = VideoFormat::from_raw(bitmap.format);
    if format != VideoFormat::BGRA && format != VideoFormat::BGRx {
        return None;
    }
    let stride = stride as usize;
    let need = stride * h;
    let pixels =
        unsafe { std::slice::from_raw_parts(base.add(bitmap.offset as usize), need).to_vec() };
    Some(CursorImage {
        width: w,
        height: h,
        stride,
        pixels,
        hotspot_x: meta.hotspot_x,
        hotspot_y: meta.hotspot_y,
    })
}

/// The timestamp a frame gets: the compositor's, when it is on our clock.
///
/// A producer that stamps buffers with another epoch (a compositor bug, or a
/// graph clock that is not `CLOCK_MONOTONIC`) would otherwise shift the whole
/// video timeline against the audio, which the writer can only answer by
/// dropping audio. A stamp more than a few seconds away from arrival is not a
/// capture time we can use; the arrival time is.
fn sane_pts(header: Option<f64>, now: f64) -> f64 {
    match header {
        Some(pts) if (pts - now).abs() < 5.0 => pts,
        Some(pts) => {
            super::warn_once(&format!(
                "the screen-cast stream stamps frames {:.1} s away from the host clock, so \
                 arrival time is used instead (A/V sync would be wrong otherwise)",
                pts - now
            ));
            now
        }
        None => now,
    }
}

/// The negotiated SPA format in the contract's terms.
fn pixel_format(f: VideoFormat) -> Option<PixelFormat> {
    if f == VideoFormat::BGRx || f == VideoFormat::BGRA {
        Some(PixelFormat::Bgra)
    } else if f == VideoFormat::RGBx || f == VideoFormat::RGBA {
        Some(PixelFormat::Rgba)
    } else {
        None
    }
}

/// The EnumFormat parameters we offer: packed 32-bit RGB, any size.
///
/// Size and frame rate are left out, which is SPA's "whatever the producer
/// has": the compositor's monitor size and its frame rate.
fn format_pods() -> Vec<Vec<u8>> {
    [VideoFormat::BGRx, VideoFormat::BGRA]
        .into_iter()
        .filter_map(|format| {
            let properties = vec![
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
            ];
            let object = Object {
                type_: SpaTypes::ObjectParamFormat.as_raw(),
                id: spa::param::ParamType::EnumFormat.as_raw(),
                properties,
            };
            let values = spa::pod::serialize::PodSerializer::serialize(
                Cursor::new(Vec::new()),
                &Value::Object(object),
            )
            .ok()?
            .0
            .into_inner();
            Some(values)
        })
        .collect()
}

/// Read the size the compositor will produce, without keeping the stream.
///
/// This is what makes `resolve()` return the *pixel* size on Wayland, where the
/// portal's own `size` field is in the compositor's (logical) coordinate space
/// and can be half the pixels on a HiDPI desktop. The caller passes a dedicated
/// portal remote, which closes when the probe ends; capture uses another.
pub fn probe_size(fd: OwnedFd, node_id: u32, wait: Duration) -> Result<(usize, usize)> {
    probe_once(fd, node_id, wait)
}

/// One probe connection: connect, wait for a format, disconnect.
fn probe_once(fd: OwnedFd, node_id: u32, wait: Duration) -> Result<(usize, usize)> {
    pw::init();
    let main = pw::main_loop::MainLoopRc::new(None).context("creating the PipeWire main loop")?;
    let context =
        pw::context::ContextRc::new(&main, None).context("creating a PipeWire context")?;
    let core = context
        .connect_fd_rc(fd, None)
        .map_err(|e| anyhow!("connecting to the portal's PipeWire remote: {e}"))?;
    let error = Arc::new(Mutex::new(None::<String>));
    let (serial, target_watch) = resolve_target(&core, &main, node_id, error.clone())?;
    let stream = new_stream(&core, &serial)?;
    if super::debug() {
        eprintln!("rec: debug: probe connecting to node {node_id} (wait {wait:?})");
    }
    let size = Arc::new(Mutex::new(None::<(usize, usize)>));
    let (tx, _rx) = sync_channel(1);
    // The watchdog leaves the loop through the loop's own event source, the
    // same way `Stream::halt` does: no thread ever dereferences the loop.
    let (quit_tx, quit_rx) = pw::channel::channel::<()>();
    let _quit = quit_rx.attach(main.loop_(), {
        let main = main.clone();
        move |_| main.quit()
    });
    let (probe_done, done_rx) = std::sync::mpsc::channel::<()>();
    let data = StreamData {
        format: VideoInfoRaw::new(),
        tx,
        dropped: 0,
        cursor: None,
        error: error.clone(),
        warned_no_bitmap: false,
        saw_buffer: false,
        quit_on_format: Some(main.clone()),
        target: Some(target_watch.state.clone()),
        was_streaming: false,
        quit_on_error: Some(main.clone()),
        probe_done: Some(probe_done),
    };
    let size2 = size.clone();
    let debug_state = super::debug();
    let listener = stream
        .add_local_listener_with_user_data(data)
        .param_changed(move |_, data, id, param| {
            if debug_state {
                eprintln!(
                    "rec: debug: probe param id {id} (value {})",
                    if param.is_some() { "present" } else { "NULL" }
                );
            }
            if !read_format(data, id, param) {
                return;
            }
            let s = data.format.size();
            *size2.lock().unwrap() = Some((s.width as usize, s.height as usize));
            // Tell the watchdog the answer is in, then leave the loop.
            if let Some(done) = data.probe_done.take() {
                let _ = done.send(());
            }
            if let Some(main) = data.quit_on_format.take() {
                main.quit();
            }
        })
        .state_changed(move |_, data, old, new| {
            if debug_state {
                eprintln!("rec: debug: probe stream {old:?} -> {new:?} (target node {node_id})");
            }
            on_state(data, old, new);
        })
        .register()
        .context("registering the probe listener")?;
    connect_stream(&stream, node_id)?;
    // A compositor that never offers a format must not hang the recorder
    // before it even starts.
    let watcher = std::thread::spawn(move || {
        // The loop's callbacks signal completion; on the timeout the watchdog
        // wakes the loop through the channel instead. A message that nobody
        // reads (the loop already left `run()`) is a no-op on a pipe the
        // channel owns.
        if done_rx.recv_timeout(wait).is_err() {
            let _ = quit_tx.send(());
        }
    });
    main.run();
    drop(_quit);
    drop(listener);
    let _ = stream.disconnect();
    drop(stream);
    drop(context);
    drop(main);
    let _ = watcher.join();
    if let Some(e) = error.lock().unwrap().take() {
        bail!("the portal's PipeWire stream failed: {e}");
    }
    if let Some(s) = *size.lock().unwrap()
        && s.0 >= 2
        && s.1 >= 2
    {
        return Ok(s);
    }
    bail!("the portal's PipeWire stream did not offer a video format within {wait:?}")
}

/// The processing thread: crop, draw the cursor and clicks, deliver.
fn process_frames(
    rx: std::sync::mpsc::Receiver<RawFrame>,
    sink: FrameSink,
    crop: Option<Rect>,
    opts: CaptureOptions,
) {
    let mut clicks: Vec<super::cursor::Click> = Vec::new();
    let debug = super::debug();
    for (delivered, raw) in rx.into_iter().enumerate() {
        let mut data = raw.data;
        let mut width = raw.width;
        let mut height = raw.height;
        let mut stride = raw.stride;
        if let Some(rect) = crop {
            data = format::crop_bytes(&data, raw.stride, rect);
            width = rect.width;
            height = rect.height;
            stride = width * 4;
        }
        let mut frame = super::cursor::Frame {
            data: &mut data,
            stride,
            width,
            height,
            format: raw.format,
        };
        if let Some((image, x, y)) = &raw.cursor {
            // The metadata position is in stream pixels; a crop moves the
            // origin.
            let (ox, oy) = crop.map(|r| (r.x as f64, r.y as f64)).unwrap_or((0.0, 0.0));
            image.draw(&mut frame, *x - ox, *y - oy);
        }
        if opts.clicks && !clicks.is_empty() {
            clicks.retain(|c| raw.pts - c.at < super::cursor::CLICK_LIFE);
            draw_clicks(&mut frame, &clicks, raw.pts);
        }
        if debug && delivered == 0 {
            eprintln!("rec: debug: first screen frame delivered ({width}x{height})");
        }
        sink(VideoFrame {
            pts: raw.pts,
            width,
            height,
            format: raw.format,
            planes: vec![crate::media_types::Plane::new(data, stride)],
        });
    }
}

/// How many capture loops have left `run()`, so a test can stop a stream
/// strictly *after* its loop was freed (the ordering that used to be unsafe).
#[cfg(test)]
static LOOPS_EXITED: AtomicUsize = AtomicUsize::new(0);

/// Makes the next `run()` return as soon as its loop exists (see the tests).
#[cfg(test)]
static EXIT_BEFORE_RUN: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
fn loops_exited() -> usize {
    LOOPS_EXITED.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    // The dead-loop hook and exit counter are global. Keep tests that create
    // capture threads from consuming one another's hook or exit notification.
    static STREAM_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn disconnect_and_error_are_fatal_but_initial_unconnected_is_not() {
        use pw::stream::StreamState::{Error, Paused, Streaming, Unconnected};
        let (tx, _rx) = sync_channel(1);
        let (done, completion) = std::sync::mpsc::channel();
        let mut data = StreamData {
            format: VideoInfoRaw::new(),
            tx,
            dropped: 0,
            cursor: None,
            error: Arc::new(Mutex::new(None)),
            warned_no_bitmap: false,
            saw_buffer: false,
            quit_on_format: None,
            target: None,
            was_streaming: false,
            quit_on_error: None,
            probe_done: Some(done),
        };
        on_state(&mut data, Unconnected, Unconnected);
        on_state(&mut data, Streaming, Paused);
        assert!(data.error.lock().unwrap().is_none());
        let target = Rc::new(RefCell::new(TargetState {
            node_id: 42,
            serial: Some("1234".into()),
            present: true,
        }));
        data.target = Some(target.clone());
        on_state(&mut data, Streaming, Paused);
        assert!(
            data.error.lock().unwrap().is_none(),
            "live Paused is not loss"
        );
        target.borrow_mut().remove(42);
        data.was_streaming = false;
        on_state(&mut data, Unconnected, Paused);
        assert!(
            data.error.lock().unwrap().is_none(),
            "initial Paused is not loss"
        );
        on_state(&mut data, Streaming, Paused);
        let loss = data.error.lock().unwrap().take().unwrap();
        assert!(loss.contains("serial 1234"));
        assert!(loss.contains("target absent"));
        data.probe_done = None; // The fatal target loss already completed the probe.
        completion.recv_timeout(Duration::from_millis(100)).unwrap();
        let (done, completion) = std::sync::mpsc::channel();
        data.probe_done = Some(done);
        on_state(&mut data, Paused, Unconnected);
        completion.recv_timeout(Duration::from_millis(100)).unwrap();
        let first = data.error.lock().unwrap().clone().unwrap();
        assert!(first.contains("disconnected"));
        on_state(&mut data, Unconnected, Error("late error".into()));
        assert_eq!(data.error.lock().unwrap().as_deref(), Some(first.as_str()));
        *data.error.lock().unwrap() = None;
        on_state(&mut data, Streaming, Error("producer failed".into()));
        assert!(
            data.error
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .contains("producer failed")
        );
    }

    #[test]
    fn registry_and_proxy_loss_callbacks_publish_the_exact_target() {
        pw::init();
        let main = pw::main_loop::MainLoopRc::new(None).unwrap();
        let state = Rc::new(RefCell::new(TargetState {
            node_id: 42,
            serial: Some("1234".into()),
            present: true,
        }));
        let error = Arc::new(Mutex::new(None));
        target_removed(&state, 43, "registry.global_remove", &error, &main);
        assert!(error.lock().unwrap().is_none());
        for event in ["registry.global_remove", "target proxy.removed"] {
            target_removed(&state, 42, event, &error, &main);
            let message = error.lock().unwrap().take().unwrap();
            assert!(message.contains("42 (serial 1234)"));
            assert!(message.contains(event));
            assert!(!state.borrow().present);
        }
    }

    #[test]
    fn target_removal_matches_only_the_resolved_global() {
        let mut target = TargetState {
            node_id: 42,
            ..Default::default()
        };
        assert!(
            !target.remove(42),
            "an unresolved global is not our producer"
        );
        target.serial = Some("1234".into());
        target.present = true;
        assert!(!target.remove(43), "unrelated global removal is harmless");
        assert!(target.present);
        assert!(target.remove(42));
        assert!(!target.present);
        assert_eq!(
            target.serial.as_deref(),
            Some("1234"),
            "retain identity after removal"
        );
        let message = target.removed_message("registry.global_remove");
        assert!(message.contains("42 (serial 1234)"));
        assert!(message.contains("registry.global_remove"));
    }

    #[test]
    fn a_frame_timestamp_must_be_on_our_clock() {
        let now = 1000.0;
        // The compositor's own capture time: used as is.
        assert_eq!(sane_pts(Some(999.98), now), 999.98);
        // Absent: arrival time.
        assert_eq!(sane_pts(None, now), now);
        // Wildly off (an epoch, a broken graph clock): arrival time.
        assert_eq!(sane_pts(Some(1.7e9), now), now);
        assert_eq!(sane_pts(Some(0.0), now), now);
        // The boundary is generous enough for a slow frame.
        assert_eq!(sane_pts(Some(now - 4.9), now), now - 4.9);
        assert_eq!(sane_pts(Some(now + 4.9), now), now + 4.9);
    }

    #[test]
    fn only_packed_formats_are_accepted() {
        assert_eq!(pixel_format(VideoFormat::BGRx), Some(PixelFormat::Bgra));
        assert_eq!(pixel_format(VideoFormat::BGRA), Some(PixelFormat::Bgra));
        assert_eq!(pixel_format(VideoFormat::RGBx), Some(PixelFormat::Rgba));
        assert_eq!(pixel_format(VideoFormat::RGBA), Some(PixelFormat::Rgba));
        // Planar formats (which a compositor may offer instead) are refused by
        // name, not misread as packed pixels.
        assert_eq!(pixel_format(VideoFormat::NV12), None);
        assert_eq!(pixel_format(VideoFormat::I420), None);
    }

    #[test]
    fn the_format_list_offers_packed_rgb_only() {
        let pods = format_pods();
        assert_eq!(pods.len(), 2, "BGRx and BGRA");
        for pod in &pods {
            assert!(Pod::from_bytes(pod).is_some());
        }
    }

    /// Stop a capture whose PipeWire loop is already gone.
    ///
    /// The hook makes the capture thread leave `run()` the instant its loop
    /// exists, so the loop is created and freed before `halt` is called: the
    /// ordering that used to call `pw_main_loop_quit` on freed memory. This
    /// needs no portal, so it runs on every `cargo test`.
    #[test]
    fn stopping_a_capture_whose_loop_already_exited_is_safe() {
        let _guard = STREAM_TEST_LOCK.lock().unwrap();
        // A regular file is not a PipeWire socket; with the hook returning
        // first, it is never even used as one.
        let fd: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        let sink: FrameSink = Arc::new(|_| {});
        for i in 0..50 {
            let before = loops_exited();
            let mut stream = Stream::new(
                1,
                fd.try_clone().unwrap(),
                None,
                CaptureOptions::default(),
                format!("dead-loop-{i}"),
            );
            EXIT_BEFORE_RUN.store(true, Ordering::SeqCst);
            stream.begin(sink.clone()).unwrap();
            // Wait until the thread has left `run()` *and dropped the loop*, so
            // the ordering is fixed rather than raced.
            let deadline = Instant::now() + std::time::Duration::from_secs(5);
            while loops_exited() == before && Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert!(
                loops_exited() > before,
                "the capture loop never left run() (cycle {i})"
            );
            // Stopping a capture that is already gone is a no-op: not a hang,
            // and not a quit through a pointer into freed memory.
            stream
                .halt()
                .unwrap_or_else(|e| panic!("stop after the loop exited failed: {e:#}"));
        }
    }

    /// A live portal session, or `None` when the tests run without one (the
    /// normal case: this needs `scripts/screen/headless.sh`).
    fn live_portal() -> Option<super::super::portal::Prepared> {
        std::env::var_os("DBUS_SESSION_BUS_ADDRESS")?;
        let result = super::super::portal::prepare(
            super::super::Want::Monitor,
            CaptureOptions::default(),
            std::time::Duration::from_secs(20),
        );
        if std::env::var_os("REC_HEADLESS_SESSION").is_some() {
            Some(result.expect("headless portal must be available"))
        } else {
            result.ok()
        }
    }

    #[test]
    fn closing_a_session_while_streaming_is_fatal() {
        let _guard = STREAM_TEST_LOCK.lock().unwrap();
        let Some(prepared) = live_portal() else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let mut stream = Stream::new(
            prepared.node_id,
            prepared.dup_fd().unwrap(),
            None,
            CaptureOptions::default(),
            "session-close".into(),
        );
        stream
            .begin(Arc::new(move |_| {
                let _ = tx.send(());
            }))
            .unwrap();
        rx.recv_timeout(Duration::from_secs(10))
            .expect("capture must deliver a real frame");
        assert!(stream.is_running());
        assert!(stream.fatal_error().is_none());
        prepared.close();
        let deadline = Instant::now() + Duration::from_secs(5);
        while stream.thread.as_ref().is_some_and(|t| !t.is_finished()) && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            stream.thread.as_ref().unwrap().is_finished(),
            "capture must terminate without stop"
        );
        assert!(!stream.is_running());
        let error = stream
            .fatal_error()
            .expect("session close must expose a fatal error");
        assert!(error.contains("PipeWire"), "{error}");
        assert!(stream.halt().is_err(), "stop must retain the failure");
    }

    #[test]
    fn probing_a_closed_session_fails_without_retargeting() {
        let Some(prepared) = live_portal() else {
            return;
        };
        let fd = prepared.dup_fd().unwrap();
        let node_id = prepared.node_id;
        prepared.close();
        let started = Instant::now();
        let error = probe_size(fd, node_id, Duration::from_secs(2)).unwrap_err();
        assert!(format!("{error:#}").contains("PipeWire"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(6));
    }

    #[test]
    fn stream_properties_pin_the_serial_and_disable_reconnection() {
        pw::init();
        let main = pw::main_loop::MainLoopRc::new(None).unwrap();
        let context = pw::context::ContextRc::new(&main, None).unwrap();
        let Some(prepared) = live_portal() else {
            return;
        };
        let core = context
            .connect_fd_rc(prepared.dup_fd().unwrap(), None)
            .unwrap();
        let error = Arc::new(Mutex::new(None));
        let (serial, _watch) = resolve_target(&core, &main, prepared.node_id, error).unwrap();
        assert!(serial.parse::<u64>().is_ok());
        let stream = new_stream(&core, &serial).unwrap();
        assert_eq!(
            stream.properties().get("target.object"),
            Some(serial.as_str())
        );
        assert_eq!(stream.properties().get("node.dont-reconnect"), Some("true"));
    }

    /// 200 start/stop cycles against real screen-cast nodes, half of them with
    /// streams that die on their own before `stop` runs.
    ///
    /// The second half is the reported bug: the loop thread leaves `run()` on a
    /// stream error and frees its `MainLoopRc`, and only then is the capture
    /// stopped. A quit through a stored pointer would run on freed memory; the
    /// channel makes it a write into a pipe nobody reads. The test waits for the
    /// loop to be gone first, so the order is fixed, not hoped for.
    ///
    /// A closed portal session is what produces the error deterministically:
    /// its node disappears, PipeWire answers "no target node available" within
    /// milliseconds, and the stream's error callback leaves the loop.
    #[test]
    fn two_hundred_start_stop_cycles_survive_dying_streams() {
        let _guard = STREAM_TEST_LOCK.lock().unwrap();
        let Some(prepared) = live_portal() else {
            return;
        };
        let sink: FrameSink = Arc::new(|_| {});
        let mut cycles = 0;
        // 1) A working node: the loop runs until `halt` asks it to leave.
        for i in 0..100 {
            let fd = prepared.dup_fd().unwrap();
            let mut stream = Stream::new(
                prepared.node_id,
                fd,
                None,
                CaptureOptions::default(),
                format!("stress-live-{i}"),
            );
            stream.begin(sink.clone()).unwrap();
            // Vary how long the loop has been running when the stop arrives.
            match i % 4 {
                1 => std::thread::sleep(std::time::Duration::from_millis(2)),
                2 => std::thread::sleep(std::time::Duration::from_millis(5)),
                _ => {}
            }
            let started = Instant::now();
            stream
                .halt()
                .unwrap_or_else(|e| panic!("a healthy stream's stop failed at cycle {i}: {e:#}"));
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "halt {i} hung for {:?}",
                started.elapsed()
            );
            cycles += 1;
        }
        // 2) Close the portal session: its node goes away, so every stream on
        // it errors out, the loop leaves on its own, and `halt` runs after the
        // loop is gone - the ordering that used to be a use-after-free.
        let dead_node = prepared.node_id;
        let dead_fd = prepared.dup_fd().unwrap();
        prepared.close();
        for i in 0..100 {
            let fd = dead_fd.try_clone().unwrap();
            let mut stream = Stream::new(
                dead_node,
                fd,
                None,
                CaptureOptions::default(),
                format!("stress-dead-{i}"),
            );
            let target = loops_exited() + 1;
            stream.begin(sink.clone()).unwrap();
            let deadline = Instant::now() + std::time::Duration::from_secs(10);
            while loops_exited() < target && Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert!(
                loops_exited() >= target,
                "the dead stream's loop never left run() (cycle {i})"
            );
            let started = Instant::now();
            // The dead node's error is reported (not swallowed), and stopping
            // returns instead of hanging or touching the freed loop.
            let err = stream
                .halt()
                .expect_err("a stream on a closed session reports its error");
            assert!(err.to_string().contains("PipeWire"), "cycle {i}: {err:#}");
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "halt {i} hung for {:?}",
                started.elapsed()
            );
            cycles += 1;
        }
        assert_eq!(cycles, 200, "both halves run their 100 cycles");
        eprintln!(
            "screen capture stress: {cycles} start/stop cycles, {} loops exited, no crash",
            loops_exited()
        );
    }
}
