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

use std::io::Cursor;
use std::os::fd::OwnedFd;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use pipewire as pw;
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

/// How long a probe waits for the compositor to offer a format.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

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
    tx: SyncSender<RawFrame>,
    dropped: u64,
    /// The last cursor bitmap, reused while the compositor's cursor is
    /// unchanged (it re-sends one bitmap per frame otherwise).
    cursor: Option<(u32, Arc<CursorImage>)>,
    error: Arc<Mutex<Option<String>>>,
    /// Warned already that the compositor sends positions but no bitmap.
    warned_no_bitmap: bool,
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
        self.thread.is_some()
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
    let stream = new_stream(&core)?;
    let data = StreamData {
        format: VideoInfoRaw::new(),
        tx,
        dropped: 0,
        cursor: None,
        error: error.clone(),
        warned_no_bitmap: false,
        quit_on_format: None,
        quit_on_error: Some(main.clone()),
        probe_done: None,
    };
    let listener = stream
        .add_local_listener_with_user_data(data)
        .param_changed(on_param_changed)
        .process(on_process)
        .state_changed(|_, data, _, new| {
            if let pw::stream::StreamState::Error(e) = new {
                set_error(&data.error, format!("PipeWire stream error: {e}"));
                // A dead stream is not worth waiting in `run()` for.
                if let Some(main) = &data.quit_on_error {
                    main.quit();
                }
            }
        })
        .register()
        .context("registering the stream listener")?;
    connect_stream(&stream, node_id)?;
    main.run();
    drop(listener);
    let _ = stream.disconnect();
    Ok(())
}

fn new_stream(core: &pw::core::CoreRc) -> Result<pw::stream::StreamRc> {
    let props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Screen",
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
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| anyhow!("connecting to the screen-cast node {node_id}: {e}"))
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
                "the screen-cast stream negotiates {:?}, which this build cannot consume; \
                 a compositor that only offers dmabuf needs the GPU importer (phase 1C)",
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
            "the screen-cast stream sent a buffer without CPU memory (dmabuf); \
             the GPU importer is phase 1C"
                .to_string(),
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
/// and can be half the pixels on a HiDPI desktop. The caller passes a duplicate
/// of the portal's fd, so the same remote is used again for the real capture.
pub fn probe_size(fd: OwnedFd, node_id: u32) -> Result<(usize, usize)> {
    pw::init();
    let main = pw::main_loop::MainLoopRc::new(None).context("creating the PipeWire main loop")?;
    let context =
        pw::context::ContextRc::new(&main, None).context("creating a PipeWire context")?;
    let core = context
        .connect_fd_rc(fd, None)
        .map_err(|e| anyhow!("connecting to the portal's PipeWire remote: {e}"))?;
    let stream = new_stream(&core)?;
    let size = Arc::new(Mutex::new(None::<(usize, usize)>));
    let error = Arc::new(Mutex::new(None::<String>));
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
        quit_on_format: Some(main.clone()),
        quit_on_error: Some(main.clone()),
        probe_done: Some(probe_done),
    };
    let size2 = size.clone();
    let error2 = error.clone();
    let listener = stream
        .add_local_listener_with_user_data(data)
        .param_changed(move |_, data, id, param| {
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
        .state_changed(move |_, data, _, new| {
            if let pw::stream::StreamState::Error(e) = new {
                set_error(&error2, format!("PipeWire stream error: {e}"));
                // The watchdog must not keep waiting for a format that will
                // never come.
                if let Some(done) = data.probe_done.take() {
                    let _ = done.send(());
                }
                if let Some(main) = &data.quit_on_error {
                    main.quit();
                }
            }
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
        if done_rx.recv_timeout(PROBE_TIMEOUT).is_err() {
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
    if let Some(s) = *size.lock().unwrap()
        && s.0 >= 2
        && s.1 >= 2
    {
        return Ok(s);
    }
    match error.lock().unwrap().take() {
        Some(e) => bail!("the portal's PipeWire stream failed: {e}"),
        None => bail!(
            "the portal's PipeWire stream did not offer a video format within {PROBE_TIMEOUT:?}"
        ),
    }
}

/// The processing thread: crop, draw the cursor and clicks, deliver.
fn process_frames(
    rx: std::sync::mpsc::Receiver<RawFrame>,
    sink: FrameSink,
    crop: Option<Rect>,
    opts: CaptureOptions,
) {
    let mut clicks: Vec<super::cursor::Click> = Vec::new();
    for raw in rx {
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
        super::super::portal::prepare(
            super::super::Want::Monitor,
            CaptureOptions::default(),
            std::time::Duration::from_secs(20),
        )
        .ok()
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
            stream.begin(sink.clone()).unwrap();
            let target = loops_exited() + 1;
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
