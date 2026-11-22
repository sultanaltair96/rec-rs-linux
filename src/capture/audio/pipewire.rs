//! PipeWire implementation of the phase 1B audio sources.
//!
//! Three jobs share one code path:
//!
//! - **discovery** ([`snapshot`]): one short-lived PipeWire main loop enumerates
//!   the graph's `Audio/Source` and `Audio/Sink` nodes with their properties,
//!   reads the session manager's default source/sink (the `default` metadata
//!   object) and, when asked, the default sink's latency. It is bounded by a
//!   timer so `devices` can never hang on a broken session.
//! - **capture**: [`open_capture`] connects a capture stream to a node (or lets
//!   the session manager pick the default) and delivers `f32` interleaved
//!   frames through the contract's [`AudioSink`]. The stream's format asks for
//!   the device's own rate and channel count, so the mixer does the only
//!   resampling in the chain.
//! - **latency**: the default sink's `Latency`/`ProcessLatency` params, falling
//!   back to its `node.latency` property.
//!
//! Timestamps are the PipeWire buffer's graph time (`pw_buffer.time`, in
//! nanoseconds on `CLOCK_MONOTONIC`), not the arrival time: a buffer that was
//! late still carries the instant it was captured, which is what keeps audio in
//! sync with the screen. If a build or a node does not fill that field, the
//! arrival time is used instead.
//!
//! # Threading
//!
//! Each source owns one main loop in a worker thread. `start` waits (bounded)
//! for the stream's format so a device that cannot be opened fails the session
//! instead of recording silence; `stop` signals the loop through a PipeWire
//! channel and joins it. A device that disappears mid-recording puts the loop
//! down and the error is returned from `stop`, which the session prints.
//!
//! Adapted from rec-rs src/camera.rs, src/record.rs and src/outlatency.rs
//! (MIT OR Apache-2.0), with AVCapture/CoreAudio replaced by PipeWire.

// The source's own helpers are exercised by the unit tests and phase 1E.
#![allow(dead_code)]

use std::cell::RefCell;
use std::io::Cursor;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::spa::param::ParamType;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pw::spa::param::format::{MediaSubtype, MediaType};
use pw::spa::param::format_utils;
use pw::spa::pod::deserialize::PodDeserializer;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Pod, Value};
use pw::spa::utils::{Direction, SpaTypes};

use crate::media_types::{AudioChunk, AudioKind, AudioSink, AudioSource, MicInfo, now};

/// How long a source may take to negotiate its format and reach `Paused`.
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `devices` waits for the graph to answer.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(4);
/// Safety net for the discovery loop (it quits itself on the third round trip).
const SNAPSHOT_LOOP_LIMIT: Duration = Duration::from_secs(3);

/// One node of the audio graph, as the registry reports it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Node {
    pub id: u32,
    pub name: String,
    pub description: String,
    /// `Audio/Source`, `Audio/Sink`, or whatever else the node is.
    pub class: String,
    /// `audio.rate`, when the node advertises one.
    pub rate: Option<u32>,
    /// `audio.channels`, when the node advertises it.
    pub channels: Option<u16>,
    /// `node.latency`, e.g. `"1024/48000"`.
    latency_prop: Option<String>,
}

impl Node {
    fn label(&self) -> String {
        if !self.description.is_empty() {
            self.description.clone()
        } else if !self.name.is_empty() {
            self.name.clone()
        } else {
            format!("node {}", self.id)
        }
    }
}

/// What discovery found: the audio nodes, the session defaults and the default
/// sink's latency.
#[derive(Clone, Debug, Default)]
pub(super) struct Snapshot {
    pub nodes: Vec<Node>,
    pub default_source: Option<String>,
    pub default_sink: Option<String>,
    pub sink_latency: Option<f64>,
}

impl Snapshot {
    pub fn sources(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter().filter(|n| n.class == "Audio/Source")
    }

    pub fn sinks(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter().filter(|n| n.class == "Audio/Sink")
    }

    fn find_node(&self, name: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.name == name)
    }
}

/// Enumerate the audio graph. `with_latency` adds the default sink's latency
/// query (two more round trips), which `microphones` does not need.
pub(super) fn snapshot(with_latency: bool) -> Result<Snapshot> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("pw-snapshot".into())
        .spawn(move || {
            let _ = tx.send(snapshot_thread(with_latency));
        })
        .context("starting the PipeWire discovery thread")?;
    match rx.recv_timeout(SNAPSHOT_TIMEOUT) {
        Ok(r) => r,
        Err(_) => bail!(
            "PipeWire did not answer within {} s (is a PipeWire session running?)",
            SNAPSHOT_TIMEOUT.as_secs()
        ),
    }
}

/// What the `param` callback got from the sink: one latency figure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LatencyParam {
    /// `Latency` object, `minNs`: the node's own buffering.
    Physical(i64),
    /// `ProcessLatency` object, `ns`: the driver's processing delay.
    Process(i64),
}

fn snapshot_thread(with_latency: bool) -> Result<Snapshot> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| anyhow!("creating a PipeWire main loop: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|e| anyhow!("creating a PipeWire context: {e}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| anyhow!("PipeWire is not available: {e}"))?;
    let registry = core
        .get_registry_rc()
        .map_err(|e| anyhow!("opening the PipeWire registry: {e}"))?;

    let state = Rc::new(RefCell::new(Collector::default()));
    let stage = Rc::new(std::cell::Cell::new(0i32));

    // Globals: the audio nodes and the "default" metadata object.
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let state = state.clone();
            let registry = registry.clone();
            move |global| on_global(&state, &registry, global)
        })
        .register();

    // The round trips: globals bound, metadata properties delivered, then the
    // sink's latency params (when wanted). The sequence numbers double as the
    // stage counter.
    const SYNC_METADATA: i32 = 2;
    const SYNC_LATENCY: i32 = 3;
    let _core_listener = core
        .add_listener_local()
        .done({
            let state = state.clone();
            let core = core.clone();
            let registry = registry.clone();
            let mainloop = mainloop.clone();
            let stage = stage.clone();
            move |id, _seq| {
                if id != 0 {
                    return; // PW_ID_CORE only
                }
                // `core.sync` returns the server's sequence, which is not the
                // number passed in, so advance by round trip: the syncs are
                // the only async operations this core starts.
                match stage.get() {
                    0 => {
                        // All globals are in; the node and metadata binds made
                        // on the way are answered before the next round trip.
                        stage.set(1);
                        let _ = core.sync(2);
                    }
                    1 => {
                        stage.set(2);
                        if !with_latency {
                            mainloop.quit();
                            return;
                        }
                        let sink = {
                            let st = state.borrow();
                            let name = st.default_sink.as_deref();
                            name.and_then(|name| {
                                st.nodes.iter().find(|n| n.name == name).map(|n| n.id)
                            })
                        };
                        let Some(id) = sink else {
                            mainloop.quit();
                            return;
                        };
                        // Bind the default sink to ask for its latency params.
                        // The global object is reconstructed from the id: only
                        // the type matters for the bind.
                        let global: pw::registry::GlobalObject<&spa::utils::dict::DictRef> =
                            pw::registry::GlobalObject {
                                id,
                                permissions: pw::permissions::PermissionFlags::empty(),
                                type_: pw::types::ObjectType::Node,
                                version: 3,
                                props: None,
                            };
                        if let Ok(node) = registry.bind::<pw::node::Node, _>(&global) {
                            let st = state.clone();
                            let listener = node
                                .add_listener_local()
                                .param(move |_seq, _id, _index, _next, param| {
                                    if let Some(param) = param
                                        && let Some(v) = parse_latency_pod(param)
                                    {
                                        st.borrow_mut().add_latency(v);
                                    }
                                })
                                .register();
                            node.enum_params(0, Some(ParamType::Latency), 0, 2);
                            node.enum_params(1, Some(ParamType::ProcessLatency), 0, 2);
                            let mut st = state.borrow_mut();
                            // Listener before proxy, so the hook is gone first.
                            st.latency_listener = Some(listener);
                            st.latency_node = Some(node);
                        }
                        let _ = core.sync(3);
                    }
                    _ => {
                        stage.set(3);
                        mainloop.quit();
                    }
                }
            }
        })
        .register();

    // The first round trip: globals, and the metadata bound on the way.
    core.sync(1)
        .map_err(|e| anyhow!("synchronising with PipeWire: {e}"))?;

    // Never let a broken or empty session hold the command.
    let timer = mainloop.loop_().add_timer({
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });
    timer.update_timer(Some(SNAPSHOT_LOOP_LIMIT), None);

    mainloop.run();

    if stage.get() < 1 {
        bail!("PipeWire closed the connection during discovery (no round trip completed)");
    }
    let collected = std::mem::take(&mut *state.borrow_mut());
    Ok(collected.finish())
}

fn on_global(
    state: &Rc<RefCell<Collector>>,
    registry: &pw::registry::RegistryRc,
    global: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
) {
    let Some(props) = global.props else {
        return;
    };
    match global.type_ {
        pw::types::ObjectType::Node => {
            let get = |k: &str| props.get(k).unwrap_or("").to_string();
            let class = get("media.class");
            if class != "Audio/Source" && class != "Audio/Sink" {
                return;
            }
            let name = get("node.name");
            if name.is_empty() {
                return;
            }
            let description = {
                let d = get("node.description");
                if d.is_empty() { get("node.nick") } else { d }
            };
            state.borrow_mut().nodes.push(Node {
                id: global.id,
                name,
                description,
                class,
                rate: None,
                channels: None,
                latency_prop: None,
            });
            // The registry global only carries a few properties; the format
            // and latency live in the node's own info, so bind it and read
            // them when the info arrives (before the next round trip ends).
            let Ok(node) = registry.bind::<pw::node::Node, _>(global) else {
                return;
            };
            let st = state.clone();
            let listener = node
                .add_listener_local()
                .info(move |info| {
                    if let Some(props) = info.props() {
                        st.borrow_mut().fill_node(info.id(), props);
                    }
                })
                .register();
            let mut st = state.borrow_mut();
            // Listener before proxy, so the hook is gone first.
            st.bound_nodes.push(BoundNode {
                listener,
                proxy: node,
            });
        }
        pw::types::ObjectType::Metadata => {
            // Several metadata objects can exist ("default", "route", ...);
            // the default nodes are in whichever one emits these keys.
            let Ok(metadata) = registry.bind::<pw::metadata::Metadata, _>(global) else {
                return;
            };
            let st = state.clone();
            let listener = metadata
                .add_listener_local()
                .property(move |_subject, key, _type, value| {
                    if let (Some(key), Some(value)) = (key, value) {
                        st.borrow_mut().set_default(key, value);
                    }
                    0
                })
                .register();
            let mut st = state.borrow_mut();
            st.metadata.push(MetadataConn {
                listener,
                proxy: metadata,
            });
        }
        _ => {}
    }
}

/// A bound node and the listener that fills its info in. The listener is
/// declared before the proxy so it is destroyed first.
struct BoundNode {
    listener: pw::node::NodeListener,
    proxy: pw::node::Node,
}

/// A bound metadata object and its listener, same drop order.
struct MetadataConn {
    listener: pw::metadata::MetadataListener,
    proxy: pw::metadata::Metadata,
}

/// The mutable side of discovery.
#[derive(Default)]
struct Collector {
    nodes: Vec<Node>,
    default_source: Option<String>,
    default_sink: Option<String>,
    latency_min_ns: Option<i64>,
    latency_proc_ns: Option<i64>,
    bound_nodes: Vec<BoundNode>,
    metadata: Vec<MetadataConn>,
    /// The sink's latency-param listener and proxy (a second listener on a
    /// second bind, so the info listener stays simple).
    latency_listener: Option<pw::node::NodeListener>,
    latency_node: Option<pw::node::Node>,
}

impl Collector {
    /// Fold a node's own info properties into its entry: `audio.rate`,
    /// `audio.channels` and `node.latency` are not in the registry global.
    fn fill_node(&mut self, id: u32, props: &spa::utils::dict::DictRef) {
        let Some(node) = self.nodes.iter_mut().find(|n| n.id == id) else {
            return;
        };
        if let Some(v) = props.get("audio.rate").and_then(|s| s.parse().ok()) {
            node.rate = Some(v);
        }
        if let Some(v) = props.get("audio.channels").and_then(|s| s.parse().ok()) {
            node.channels = Some(v);
        }
        if let Some(v) = props.get("node.latency") {
            node.latency_prop = Some(v.to_string());
        }
        if let Some(v) = props.get("node.description")
            && !v.is_empty()
        {
            node.description = v.to_string();
        }
    }

    fn set_default(&mut self, key: &str, value: &str) {
        let Some(name) = default_name(value) else {
            return;
        };
        match key {
            "default.audio.source" => self.default_source = Some(name),
            "default.audio.sink" => self.default_sink = Some(name),
            _ => {}
        }
    }

    fn add_latency(&mut self, v: LatencyParam) {
        match v {
            LatencyParam::Physical(ns) => {
                self.latency_min_ns = Some(self.latency_min_ns.map_or(ns, |p| p.min(ns)))
            }
            LatencyParam::Process(ns) => self.latency_proc_ns = Some(ns),
        }
    }

    fn finish(self) -> Snapshot {
        // The params win; a virtual sink may only carry node.latency.
        let mut sink_latency = match self.latency_min_ns {
            Some(min) => Some(((min + self.latency_proc_ns.unwrap_or(0)).max(0)) as f64 / 1e9),
            None => None,
        };
        if sink_latency.is_none()
            && let Some(name) = self.default_sink.as_deref()
            && let Some(node) = self.nodes.iter().find(|n| n.name == name)
        {
            sink_latency = node
                .latency_prop
                .as_deref()
                .and_then(|s| parse_latency_prop(s, node.rate));
        }
        Snapshot {
            nodes: self.nodes,
            default_source: self.default_source,
            default_sink: self.default_sink,
            sink_latency,
        }
    }
}

/// The name in a metadata value: WirePlumber stores JSON (`{"name":"…"}`),
/// some tools a bare name.
fn default_name(value: &str) -> Option<String> {
    let v = value.trim();
    if v.starts_with('{') {
        let parsed: serde_json::Value = serde_json::from_str(v).ok()?;
        let name = parsed.get("name")?.as_str()?.trim();
        return (!name.is_empty()).then(|| name.to_string());
    }
    let name = v.trim_matches('"').trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// `node.latency`, as a fraction of seconds (`"1024/48000"`) or a frame count
/// (then `rate` is needed).
fn parse_latency_prop(value: &str, rate: Option<u32>) -> Option<f64> {
    let v = value.trim();
    if let Some((num, den)) = v.split_once('/') {
        let num: f64 = num.trim().parse().ok()?;
        let den: f64 = den.trim().parse().ok()?;
        if den <= 0.0 {
            return None;
        }
        return Some((num / den).max(0.0));
    }
    let frames: f64 = v.parse().ok()?;
    let rate = f64::from(rate?);
    if rate <= 0.0 {
        return None;
    }
    Some((frames / rate).max(0.0))
}

/// Read a `Latency` or `ProcessLatency` object's nanoseconds.
fn parse_latency_pod(pod: &Pod) -> Option<LatencyParam> {
    let Ok((_, value)) = PodDeserializer::deserialize_from::<Value>(pod.as_bytes()) else {
        return None;
    };
    let Value::Object(obj) = value else {
        return None;
    };
    if obj.type_ == SpaTypes::ObjectParamLatency.as_raw() {
        let ns = obj
            .properties
            .iter()
            .find_map(|p| (p.key == spa::sys::SPA_PARAM_LATENCY_minNs).then_some(&p.value));
        if let Some(Value::Long(ns)) = ns {
            return Some(LatencyParam::Physical(*ns));
        }
    } else if obj.type_ == SpaTypes::ObjectParamProcessLatency.as_raw() {
        let ns = obj
            .properties
            .iter()
            .find_map(|p| (p.key == spa::sys::SPA_PARAM_PROCESS_LATENCY_ns).then_some(&p.value));
        if let Some(Value::Long(ns)) = ns {
            return Some(LatencyParam::Process(*ns));
        }
    }
    None
}

/// Sample counts that the stream and the sink agreed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Negotiated {
    pub rate: u32,
    pub channels: u16,
}

impl Negotiated {
    fn label(self) -> String {
        format!("{} Hz, {} ch", self.rate, self.channels)
    }
}

/// Turn PipeWire buffer times into chunk timestamps: monotonic, on the host
/// clock, and never before the previous buffer.
#[derive(Debug, Default)]
struct Stamp {
    last: Option<f64>,
}

impl Stamp {
    /// `buffer_time_ns` is `pw_buffer.time` (graph clock, nanoseconds); 0 means
    /// the build did not fill it and the arrival time is used.
    fn stamp(&mut self, buffer_time_ns: u64) -> f64 {
        let t = if buffer_time_ns > 0 {
            buffer_time_ns as f64 / 1e9
        } else {
            now()
        };
        let out = match self.last {
            Some(prev) if t < prev => prev,
            _ => t,
        };
        self.last = Some(out);
        out
    }
}

/// Open one capture stream. `device` is a resolved node name (see
/// `super::pick_from`); `None` means the session's default source (mic) or sink
/// monitor (system audio).
pub(super) fn open_capture(kind: AudioKind, device: Option<&str>) -> Result<PwAudioSource> {
    let snap = snapshot(false).context("listing the PipeWire audio graph")?;
    let node = match (kind, device) {
        (AudioKind::Mic, None) => {
            let name = snap.default_source.clone();
            name.and_then(|n| snap.find_node(&n)).cloned()
        }
        (AudioKind::Mic, Some(sel)) => {
            let mics: Vec<MicInfo> = snap
                .sources()
                .map(|n| MicInfo {
                    id: n.name.clone(),
                    name: n.description.clone(),
                    rate: n.rate,
                })
                .collect();
            let pick = super::pick_from(&mics, sel, None);
            match pick
                .id
                .as_deref()
                .and_then(|id| snap.sources().find(|n| n.name == id))
            {
                Some(n) => Some(n.clone()),
                None => bail!("microphone {sel:?} is not connected (see `rec-rs devices`)"),
            }
        }
        (AudioKind::System, _) => {
            let Some(sink) = snap
                .default_sink
                .as_deref()
                .and_then(|n| snap.sinks().find(|s| s.name == n))
            else {
                bail!(
                    "no default output device for system audio (see `rec-rs devices`; use --no-system-audio to skip it)"
                );
            };
            Some(sink.clone())
        }
    };
    let label = node.as_ref().map(Node::label).unwrap_or_else(|| {
        if kind == AudioKind::Mic {
            "default microphone".into()
        } else {
            "default output".into()
        }
    });
    Ok(PwAudioSource {
        kind,
        description: format!(
            "{} ({})",
            label,
            if kind == AudioKind::System {
                "monitor"
            } else {
                "PipeWire"
            }
        ),
        target: node.as_ref().map(|n| n.id),
        system: kind == AudioKind::System,
        rate: node.as_ref().and_then(|n| n.rate),
        channels: node.as_ref().and_then(|n| n.channels),
        sink: None,
        thread: None,
        control: None,
        error: Arc::new(Mutex::new(None)),
        negotiated: None,
        running: false,
    })
}

#[derive(Clone, Copy, Debug)]
enum Command {
    Quit,
}

/// A stopped or running PipeWire capture stream.
pub(super) struct PwAudioSource {
    kind: AudioKind,
    description: String,
    /// Node id to connect to; None asks the session manager for the default.
    target: Option<u32>,
    system: bool,
    rate: Option<u32>,
    channels: Option<u16>,
    sink: Option<AudioSink>,
    thread: Option<JoinHandle<()>>,
    control: Option<pw::channel::Sender<Command>>,
    error: Arc<Mutex<Option<String>>>,
    negotiated: Option<Negotiated>,
    running: bool,
}

impl AudioSource for PwAudioSource {
    fn kind(&self) -> AudioKind {
        self.kind
    }

    fn describe(&self) -> String {
        match self.negotiated {
            Some(n) => format!("{} ({})", self.description, n.label()),
            None => self.description.clone(),
        }
    }

    fn start(&mut self, sink: AudioSink) -> Result<()> {
        if self.running {
            bail!("the {} source is already running", self.kind.label());
        }
        self.sink = Some(sink.clone());
        self.error = Arc::new(Mutex::new(None));
        let (ready_tx, ready_rx) = mpsc::channel();
        let (ctl_tx, ctl_rx) = pw::channel::channel();
        let cfg = CaptureConfig {
            kind: self.kind,
            target: self.target,
            system: self.system,
            rate: self.rate,
            channels: self.channels,
            sink,
            error: self.error.clone(),
        };
        let name = format!("pw-{}-capture", self.kind.label());
        self.thread = Some(
            std::thread::Builder::new()
                .name(name)
                .spawn(move || capture_thread(cfg, ready_tx, ctl_rx))
                .context("starting the PipeWire capture thread")?,
        );
        self.control = Some(ctl_tx);
        match ready_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok(n)) => {
                self.negotiated = Some(n);
                self.running = true;
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = self.stop();
                bail!("{e}");
            }
            Err(_) => {
                let _ = self.stop();
                bail!(
                    "the {} stream did not start within {} s (is the device busy?)",
                    self.kind.label(),
                    START_TIMEOUT.as_secs()
                )
            }
        }
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(ctl) = self.control.take() {
            let _ = ctl.send(Command::Quit);
        }
        if let Some(t) = self.thread.take()
            && t.join().is_err()
        {
            self.running = false;
            bail!("the {} capture thread panicked", self.kind.label());
        }
        let _was_running = std::mem::replace(&mut self.running, false);
        match self.error.lock().unwrap().take() {
            Some(e) => Err(anyhow!("{e}")),
            None => Ok(()),
        }
    }

    fn is_running(&self) -> bool {
        self.running
    }
}

impl Drop for PwAudioSource {
    fn drop(&mut self) {
        // A source dropped without stop must not leave a PipeWire client and
        // its thread behind.
        let _ = self.stop();
    }
}

struct CaptureConfig {
    kind: AudioKind,
    target: Option<u32>,
    system: bool,
    rate: Option<u32>,
    channels: Option<u16>,
    sink: AudioSink,
    error: Arc<Mutex<Option<String>>>,
}

impl CaptureConfig {
    fn name(&self) -> String {
        format!("rec-rs-{}-capture", self.kind.label())
    }
}

struct StreamState {
    kind: AudioKind,
    sink: AudioSink,
    /// Negotiated format; 0 until `param_changed`.
    rate: u32,
    channels: u16,
    requested_rate: Option<u32>,
    requested_channels: Option<u16>,
    ready: Option<mpsc::Sender<Result<Negotiated, String>>>,
    ready_sent: bool,
    error: Arc<Mutex<Option<String>>>,
    mainloop: pw::main_loop::MainLoopWeak,
    stamp: Stamp,
    /// First device error of this stream, reported from `stop`.
    failure: Option<String>,
}

impl StreamState {
    fn mark_ready(&mut self) {
        if self.ready_sent {
            return;
        }
        // `param_changed` usually filled these in; the node's advertised
        // values (or the graph default) cover a node that stays silent.
        let rate = if self.rate > 0 {
            self.rate
        } else {
            self.requested_rate.unwrap_or(48_000)
        };
        let channels = if self.channels > 0 {
            self.channels
        } else {
            self.requested_channels.unwrap_or(2)
        };
        self.ready_sent = true;
        if let Some(tx) = self.ready.take() {
            let _ = tx.send(Ok(Negotiated { rate, channels }));
        }
    }

    /// Remember the first failure and wake the loop.
    fn fail(&mut self, msg: String) {
        if self.failure.is_none() {
            self.failure = Some(msg.clone());
        }
        if let Ok(mut slot) = self.error.lock()
            && slot.is_none()
        {
            *slot = Some(msg.clone());
        }
        if !self.ready_sent
            && let Some(tx) = self.ready.take()
        {
            let _ = tx.send(Err(msg));
        }
        if let Some(mainloop) = self.mainloop.upgrade() {
            mainloop.quit();
        }
    }
}

fn capture_thread(
    cfg: CaptureConfig,
    ready: mpsc::Sender<Result<Negotiated, String>>,
    control: pw::channel::Receiver<Command>,
) {
    let error = cfg.error.clone();
    let result = run_capture(&cfg, ready, control);
    if let Err(e) = result {
        let msg = format!("{e:#}");
        if let Ok(mut slot) = error.lock()
            && slot.is_none()
        {
            *slot = Some(msg);
        }
    }
}

fn run_capture(
    cfg: &CaptureConfig,
    ready: mpsc::Sender<Result<Negotiated, String>>,
    control: pw::channel::Receiver<Command>,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| anyhow!("creating a PipeWire main loop: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|e| anyhow!("creating a PipeWire context: {e}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| anyhow!("PipeWire is not available: {e}"))?;

    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::NODE_NAME => cfg.name(),
    };
    if cfg.system {
        // Capture the sink's monitor: what is played, before it is heard.
        props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
    }
    if let Some(id) = cfg.target {
        props.insert(*pw::keys::TARGET_OBJECT, id.to_string());
    }
    let stream = pw::stream::StreamBox::new(&core, &cfg.name(), props)
        .map_err(|e| anyhow!("creating the PipeWire stream: {e}"))?;

    let state = Rc::new(RefCell::new(StreamState {
        kind: cfg.kind,
        sink: cfg.sink.clone(),
        rate: 0,
        channels: 0,
        requested_rate: cfg.rate,
        requested_channels: cfg.channels,
        ready: Some(ready),
        ready_sent: false,
        error: cfg.error.clone(),
        mainloop: mainloop.downgrade(),
        stamp: Stamp::default(),
        failure: None,
    }));

    // The stream does not always leave Streaming when its target vanishes (the
    // session manager may leave it paused without a link), so watch the
    // registry: losing the node we are linked to is the device disappearing.
    let capture_registry = core
        .get_registry_rc()
        .map_err(|e| anyhow!("opening the PipeWire registry: {e}"))?;
    let _registry_listener = capture_registry
        .add_listener_local()
        .global_remove({
            let state = state.clone();
            let target = cfg.target;
            let kind = cfg.kind;
            move |id| {
                if Some(id) == target {
                    state
                        .borrow_mut()
                        .fail(format!("the {} device was disconnected", kind.label()));
                }
            }
        })
        .register();

    let _listener = stream
        .add_local_listener_with_user_data(state.clone())
        .state_changed(|_, st, _old, new| on_state(st, new))
        .param_changed(|_, st, id, param| on_param(st, id, param))
        .process(on_process)
        .register()
        .map_err(|e| anyhow!("listening to the PipeWire stream: {e}"))?;

    // Ask for f32 interleaved audio at the device's own rate and channel count
    // when we know them; 0 means "the device's choice" to PipeWire.
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    if let Some(rate) = cfg.rate {
        info.set_rate(rate);
    }
    if let Some(channels) = cfg.channels {
        info.set_channels(u32::from(channels.max(1)));
    }
    let obj = pw::spa::pod::Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let values = PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(obj))
        .map_err(|e| anyhow!("building the audio format: {e:?}"))?
        .0
        .into_inner();
    let pod =
        Pod::from_bytes(&values).ok_or_else(|| anyhow!("internal: malformed audio format"))?;
    let mut params = [pod];

    stream
        .connect(
            Direction::Input,
            cfg.target,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| anyhow!("connecting the audio stream: {e}"))?;

    let _control = control.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });
    mainloop.run();

    let mut st = state.borrow_mut();
    if !st.ready_sent
        && let Some(tx) = st.ready.take()
    {
        let _ = tx.send(Err(format!(
            "the {} stream stopped before it was ready",
            cfg.kind.label()
        )));
    }
    Ok(())
}

fn on_state(st: &mut Rc<RefCell<StreamState>>, new: pw::stream::StreamState) {
    match new {
        pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming => {
            st.borrow_mut().mark_ready();
        }
        pw::stream::StreamState::Error(e) => {
            st.borrow_mut()
                .fail(format!("PipeWire audio stream error: {e}"));
        }
        pw::stream::StreamState::Unconnected => {
            let failed = {
                let s = st.borrow();
                s.ready_sent
            };
            if failed {
                st.borrow_mut()
                    .fail("the audio device was disconnected".to_string());
            }
        }
        other => {
            if std::env::var_os("REC_DEBUG").is_some() {
                eprintln!(
                    "rec: debug: {} stream state {other:?}",
                    st.borrow().kind.label()
                );
            }
        }
    }
}

fn on_param(st: &mut Rc<RefCell<StreamState>>, id: u32, param: Option<&Pod>) {
    if id != ParamType::Format.as_raw() {
        return;
    }
    let Some(param) = param else {
        return;
    };
    let Ok((media_type, subtype)) = format_utils::parse_format(param) else {
        return;
    };
    if media_type != MediaType::Audio || subtype != MediaSubtype::Raw {
        return;
    }
    let mut info = AudioInfoRaw::new();
    if info.parse(param).is_err() {
        return;
    }
    let mut s = st.borrow_mut();
    if info.rate() > 0 {
        s.rate = info.rate();
    }
    if info.channels() > 0 {
        s.channels = info.channels() as u16;
    }
    if std::env::var_os("REC_DEBUG").is_some() {
        eprintln!(
            "rec: debug: {} format {} Hz, {} ch ({:?})",
            s.kind.label(),
            s.rate,
            s.channels,
            info.format()
        );
    }
}

/// Copy one dequeue'd buffer out and hand it to the sink. The buffer is
/// returned to PipeWire before the sink runs, so a slow consumer never keeps
/// the graph from refilling it.
fn on_process(stream: &pw::stream::Stream, st: &mut Rc<RefCell<StreamState>>) {
    let raw = unsafe { stream.dequeue_raw_buffer() };
    if raw.is_null() {
        return;
    }
    // `pw_buffer.time` is the graph cycle in nanoseconds (CLOCK_MONOTONIC).
    let time_ns = unsafe { (*raw).time };
    let mut samples: Vec<f32> = Vec::new();
    unsafe {
        let buf = &*raw;
        if !buf.buffer.is_null() {
            let spa_buf = &*buf.buffer;
            if spa_buf.n_datas > 0 && !spa_buf.datas.is_null() {
                let datas = std::slice::from_raw_parts_mut(
                    spa_buf.datas as *mut spa::buffer::Data,
                    spa_buf.n_datas as usize,
                );
                let data = &mut datas[0];
                let chunk = data.chunk();
                let size = chunk.size() as usize;
                let offset = chunk.offset() as usize;
                if let Some(bytes) = data.data() {
                    let end = (offset + size).min(bytes.len());
                    if end > offset {
                        let flat = &bytes[offset..end];
                        samples.extend(
                            flat.chunks_exact(4)
                                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])),
                        );
                    }
                }
            }
        }
        stream.queue_raw_buffer(raw);
    }
    if samples.is_empty() {
        return;
    }
    let (kind, sink, rate, channels) = {
        let s = st.borrow();
        (s.kind, s.sink.clone(), s.rate, s.channels)
    };
    if rate == 0 || channels == 0 {
        return;
    }
    let ch = channels as usize;
    let frames = samples.len() / ch;
    if frames == 0 {
        return;
    }
    samples.truncate(frames * ch);
    let pts = st.borrow_mut().stamp.stamp(time_ns);
    sink(
        kind,
        AudioChunk {
            pts,
            rate,
            channels,
            samples,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_defaults_are_json_or_plain_names() {
        assert_eq!(
            default_name(r#"{"name":"recnull"}"#).as_deref(),
            Some("recnull")
        );
        assert_eq!(
            default_name(r#"{ "name": "alsa_output.pci-0000", "id": 3 }"#).as_deref(),
            Some("alsa_output.pci-0000")
        );
        assert_eq!(
            default_name("rec_test_mic").as_deref(),
            Some("rec_test_mic")
        );
        assert_eq!(default_name(r#""quoted""#).as_deref(), Some("quoted"));
        assert_eq!(default_name(""), None);
        assert_eq!(default_name("   "), None);
        assert_eq!(default_name("{not json}"), None);
        assert_eq!(default_name(r#"{"id":3}"#), None);
    }

    #[test]
    fn node_latency_property_is_a_fraction_or_frames() {
        assert!((parse_latency_prop("1024/48000", None).unwrap() - 0.021_333_333).abs() < 1e-6);
        assert!((parse_latency_prop("256 / 44100", None).unwrap() - 0.005_804_988).abs() < 1e-6);
        assert_eq!(parse_latency_prop("0/48000", None), Some(0.0));
        assert!((parse_latency_prop("1024", Some(48_000)).unwrap() - 0.021_333_333).abs() < 1e-6);
        assert_eq!(parse_latency_prop("1024", None), None);
        assert_eq!(parse_latency_prop("junk", Some(48_000)), None);
        assert_eq!(parse_latency_prop("1/0", None), None);
        assert_eq!(parse_latency_prop("-5/48000", None), Some(0.0));
    }

    fn latency_pod(value: Value) -> Vec<u8> {
        PodSerializer::serialize(Cursor::new(Vec::new()), &value)
            .unwrap()
            .0
            .into_inner()
    }

    #[test]
    fn latency_params_are_read_from_their_objects() {
        use pw::spa::pod::{Object, Property};
        let pod = latency_pod(Value::Object(Object {
            type_: SpaTypes::ObjectParamLatency.as_raw(),
            id: ParamType::Latency.as_raw(),
            properties: vec![
                Property::new(
                    spa::sys::SPA_PARAM_LATENCY_direction,
                    Value::Id(spa::utils::Id(1)),
                ),
                Property::new(spa::sys::SPA_PARAM_LATENCY_minNs, Value::Long(21_000_000)),
                Property::new(spa::sys::SPA_PARAM_LATENCY_maxNs, Value::Long(42_000_000)),
            ],
        }));
        let pod = Pod::from_bytes(&pod).unwrap();
        assert_eq!(
            parse_latency_pod(pod),
            Some(LatencyParam::Physical(21_000_000))
        );

        let pod = latency_pod(Value::Object(Object {
            type_: SpaTypes::ObjectParamProcessLatency.as_raw(),
            id: ParamType::ProcessLatency.as_raw(),
            properties: vec![Property::new(
                spa::sys::SPA_PARAM_PROCESS_LATENCY_ns,
                Value::Long(1_500_000),
            )],
        }));
        let pod = Pod::from_bytes(&pod).unwrap();
        assert_eq!(
            parse_latency_pod(pod),
            Some(LatencyParam::Process(1_500_000))
        );

        // Something else: ignored.
        let pod = latency_pod(Value::Object(Object {
            type_: SpaTypes::ObjectParamLatency.as_raw(),
            id: ParamType::Latency.as_raw(),
            properties: vec![Property::new(
                spa::sys::SPA_PARAM_LATENCY_minQuantum,
                Value::Float(2.0),
            )],
        }));
        let pod = Pod::from_bytes(&pod).unwrap();
        assert_eq!(parse_latency_pod(pod), None);
    }

    #[test]
    fn collector_combines_the_latency_parts() {
        let sink = Node {
            id: 7,
            name: "recnull".into(),
            description: "Null".into(),
            class: "Audio/Sink".into(),
            rate: Some(48_000),
            channels: Some(2),
            latency_prop: Some("256/48000".into()),
        };
        let c = Collector {
            nodes: vec![sink.clone()],
            default_sink: Some("recnull".into()),
            ..Default::default()
        };
        // node.latency is the fallback.
        assert!((c.finish().sink_latency.unwrap() - 256.0 / 48_000.0).abs() < 1e-9);
        // The params win and add up.
        let mut c = Collector {
            nodes: vec![sink],
            default_sink: Some("recnull".into()),
            ..Default::default()
        };
        c.add_latency(LatencyParam::Physical(10_000_000));
        c.add_latency(LatencyParam::Physical(21_000_000));
        c.add_latency(LatencyParam::Process(1_500_000));
        assert!((c.finish().sink_latency.unwrap() - 0.011_5).abs() < 1e-9);
        // No sink: no latency.
        assert_eq!(Collector::default().finish().sink_latency, None);
    }

    #[test]
    fn buffer_times_become_monotonic_seconds() {
        let mut s = Stamp::default();
        assert!((s.stamp(1_500_000_000) - 1.5).abs() < 1e-9);
        assert!((s.stamp(1_510_000_000) - 1.51).abs() < 1e-9);
        // A driver/build that goes backwards: hold the previous time.
        assert!((s.stamp(1_505_000_000) - 1.51).abs() < 1e-9);
        // 0 means unknown: arrival time, which is on the same clock.
        let before = now();
        let t = s.stamp(0);
        assert!(t >= before, "{t} !>= {before}");
        assert!(t >= 1.51);
    }

    #[test]
    #[ignore = "needs a live PipeWire session (scripts/audio/harness.sh)"]
    fn snapshot_sees_a_live_session() {
        // Requires the harness (`scripts/audio/harness.sh`), so it is ignored
        // in the normal `cargo test` run.
        let snap = match snapshot(true) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("no PipeWire session: {e}");
                return;
            }
        };
        for n in &snap.nodes {
            eprintln!(
                "node {} {:?} {:?} rate {:?} ch {:?} latency {:?}",
                n.id, n.class, n.name, n.rate, n.channels, n.latency_prop
            );
        }
        eprintln!(
            "default source {:?}, default sink {:?}, sink latency {:?}",
            snap.default_source, snap.default_sink, snap.sink_latency
        );
        assert!(
            snap.nodes
                .iter()
                .all(|n| n.class == "Audio/Source" || n.class == "Audio/Sink"),
            "the snapshot only holds audio nodes"
        );
    }
}
