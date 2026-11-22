//! Verify a recording with GStreamer: playability, tracks, and a full decode of
//! every sample.
//!
//! The output keeps upstream's shape (`rec-rs probe FILE`): the file, whether
//! it plays, its duration, one line per track with codec and format, and a
//! decode line per track. A decode error, or a track that cannot be decoded at
//! all, exits non-zero.
//!
//! Adapted from rec-rs src/probe.rs (MIT OR Apache-2.0), with AVFoundation
//! replaced by GStreamer.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use gstreamer as gst;
use gstreamer::prelude::*;

/// What one demuxed stream is.
#[derive(Clone, Debug)]
struct Stream {
    /// "video" or "audio".
    media: &'static str,
    /// Codec name, e.g. "h264", "hevc", "prores", "aac".
    codec: String,
    /// `1920x1080 @ 60.000 fps` or `48000 Hz, 2 ch`.
    format: String,
}

/// Counters for one decoded pad.
#[derive(Clone, Debug, Default)]
struct Decoded {
    buffers: u64,
    frames: u64,
    first: Option<f64>,
    last: Option<f64>,
    /// The frame before `last`, to derive the frame rate when the demuxer's
    /// caps carry none.
    previous: Option<f64>,
    video: bool,
}

impl Decoded {
    /// The frame rate the decoded timestamps show, or None with fewer than two
    /// frames.
    fn fps(&self) -> Option<f64> {
        let (first, last, previous) = (self.first?, self.last?, self.previous?);
        let spacing = last - previous;
        if spacing <= 0.0 || self.frames < 2 {
            return None;
        }
        Some(self.frames as f64 / ((last - first) + spacing))
    }
}

/// How long a probe may take: a 4K60 recording decodes slower than real time
/// but not by more than this.
const TIMEOUT: Duration = Duration::from_secs(1800);

/// Probe a recording: structure, then a full decode.
pub fn probe(file: &Path) -> Result<()> {
    gst::init().context("GStreamer could not be initialised")?;
    let abs = std::path::absolute(file).with_context(|| format!("resolving {}", file.display()))?;
    if !abs.exists() {
        bail!("{} does not exist", abs.display());
    }
    let uri = gst::glib::filename_to_uri(&abs, None)
        .map_err(|e| anyhow!("{}: {e}", abs.display()))?
        .to_string();
    println!("file      : {}", abs.display());
    let (duration, mut streams) = structure(&abs)?;
    let decoded = decode(&uri)?;
    if std::env::var_os("REC_DEBUG").is_some() {
        for (i, d) in decoded.iter().enumerate() {
            eprintln!(
                "rec: debug: stream {i}: video={} buffers={} frames={} fps={:?}",
                d.video,
                d.buffers,
                d.frames,
                d.fps()
            );
        }
    }
    // qtdemux's caps do not always carry the frame rate; the decoded timestamps
    // show it (a constant frame rate is a guarantee of the writer).
    for s in &mut streams {
        if s.media == "video"
            && !s.format.contains("fps")
            && let Some(fps) = decoded.iter().filter(|d| d.video).find_map(|d| d.fps())
        {
            s.format = format!("{} @ {fps:.3} fps", s.format);
        }
    }
    let playable = !streams.is_empty();
    println!("playable  : {playable}");
    println!(
        "duration  : {:.3}s",
        duration.map(|d| d.seconds_f64()).unwrap_or(0.0)
    );
    for (i, s) in streams.iter().enumerate() {
        println!("track {i}  : {} {} {}", s.media, s.codec, s.format);
    }
    let mut ok = playable;
    // Decode lines follow the tracks: the nth video stream is the nth decoded
    // video pad, and so on for audio.
    let mut used = [0usize; 2];
    for s in &streams {
        let kind = usize::from(s.media == "audio");
        let candidates: Vec<&Decoded> = decoded.iter().filter(|d| d.video == (kind == 0)).collect();
        let d = candidates.get(used[kind]);
        used[kind] += 1;
        match d {
            Some(d) if d.buffers > 0 => println!(
                "          decode: ok ({} buffers, {} frames, pts {:.3}..{:.3}s)",
                d.buffers,
                d.frames,
                d.first.unwrap_or(0.0),
                d.last.unwrap_or(0.0)
            ),
            _ => {
                println!("          decode: FAILED (nothing decoded)");
                ok = false;
            }
        }
    }
    if !ok {
        bail!("probe found problems");
    }
    Ok(())
}

/// Read the container: every stream's caps and the duration.
///
/// The demuxer is used on its own (its pads carry the encoded caps, which name
/// the codec); decoding is a separate pass so this one never waits on an
/// encoder-heavy file.
fn structure(path: &Path) -> Result<(Option<gst::ClockTime>, Vec<Stream>)> {
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("filesrc").build()?;
    src.set_property("location", path.to_string_lossy().to_string());
    let demux = gst::ElementFactory::make("qtdemux").build()?;
    pipeline.add_many([&src, &demux])?;
    gst::Element::link(&src, &demux)?;
    let found: Arc<Mutex<Vec<Stream>>> = Arc::new(Mutex::new(vec![]));
    let found2 = found.clone();
    let pipe = pipeline.clone();
    demux.connect_pad_added(move |_demux, pad| {
        if let Some(caps) = pad.current_caps().or_else(|| Some(pad.query_caps(None)))
            && let Some(s) = describe(&caps)
        {
            found2.lock().unwrap().push(s);
        }
        // Consume the stream so the pipeline prerolls and runs to EOS.
        if let Ok(sink) = gst::ElementFactory::make("fakesink").build()
            && let Ok(sink_pad) = sink.static_pad("sink").context("fakesink sink pad")
            && pipe.add(&sink).is_ok()
            && pad.link(&sink_pad).is_ok()
        {
            let _ = sink.sync_state_with_parent();
        }
    });
    let duration = run(&pipeline)?;
    let streams = found.lock().unwrap().clone();
    Ok((duration, streams))
}

/// A one-line description of a demuxed stream's caps.
fn describe(caps: &gst::Caps) -> Option<Stream> {
    let s = caps.structure(0)?;
    let name = s.name().as_str();
    let (media, codec) = match name {
        "video/x-h264" => ("video", "h264"),
        "video/x-h265" => ("video", "hevc"),
        "video/x-prores" => ("video", "prores"),
        "video/x-vp9" => ("video", "vp9"),
        "video/x-av1" => ("video", "av1"),
        "audio/mpeg" => ("audio", "aac"),
        "audio/x-opus" => ("audio", "opus"),
        "audio/x-vorbis" => ("audio", "vorbis"),
        "audio/x-flac" => ("audio", "flac"),
        _ if name.starts_with("video/") => ("video", "other"),
        _ if name.starts_with("audio/") => ("audio", "other"),
        _ => return None,
    };
    let format = if media == "video" {
        let w = s.get::<i32>("width").unwrap_or(0);
        let h = s.get::<i32>("height").unwrap_or(0);
        let fps = s
            .get::<gst::Fraction>("framerate")
            .ok()
            .map(|f| f.numer() as f64 / f.denom().max(1) as f64);
        match fps {
            Some(f) if f > 0.0 => format!("{w}x{h} @ {f:.3} fps"),
            _ => format!("{w}x{h}"),
        }
    } else {
        let rate = s.get::<i32>("rate").unwrap_or(0);
        let ch = s.get::<i32>("channels").unwrap_or(0);
        format!("{rate} Hz, {ch} ch")
    };
    Some(Stream {
        media,
        codec: codec.to_string(),
        format,
    })
}

/// Decode every sample off every stream and count what came out.
fn decode(uri: &str) -> Result<Vec<Decoded>> {
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("uridecodebin")
        .property("uri", uri)
        .build()?;
    pipeline.add(&src)?;
    let counts: Arc<Mutex<Vec<Decoded>>> = Arc::new(Mutex::new(vec![]));
    let counts2 = counts.clone();
    let pipe = pipeline.clone();
    src.connect_pad_added(move |_bin, pad| {
        let caps = pad.current_caps().or_else(|| Some(pad.query_caps(None)));
        let video = caps.as_ref().is_some_and(|c| {
            c.structure(0)
                .is_some_and(|s| s.name().starts_with("video/"))
        });
        let idx = {
            let mut c = counts2.lock().unwrap();
            c.push(Decoded {
                video,
                ..Default::default()
            });
            c.len() - 1
        };
        let Ok(sink) = gst::ElementFactory::make("fakesink").build() else {
            return;
        };
        sink.set_property("sync", false);
        let Ok(queue) = gst::ElementFactory::make("queue").build() else {
            return;
        };
        let _ = pipe.add_many([&queue, &sink]);
        let _ = gst::Element::link_many([&queue, &sink]);
        let Some(queue_sink) = queue.static_pad("sink") else {
            return;
        };
        if pad.link(&queue_sink).is_err() {
            return;
        }
        let _ = sink.sync_state_with_parent();
        let _ = queue.sync_state_with_parent();
        let counts3 = counts2.clone();
        if let Some(sink_pad) = sink.static_pad("sink") {
            sink_pad.add_probe(gst::PadProbeType::BUFFER, move |p, info| {
                if let Some(buffer) = info.buffer() {
                    let bpf = raw_audio_bpf(p);
                    let is_video = p
                        .current_caps()
                        .and_then(|c| c.structure(0).map(|s| s.name().starts_with("video/")))
                        .unwrap_or(false);
                    let mut c = counts3.lock().unwrap();
                    let d = &mut c[idx];
                    d.video = is_video;
                    d.buffers += 1;
                    d.frames += match bpf {
                        Some(bpf) if bpf > 0 => buffer.size() as u64 / bpf,
                        _ => 1,
                    };
                    if let Some(pts) = buffer.pts() {
                        let t = pts.seconds_f64();
                        d.first.get_or_insert(t);
                        d.previous = d.last;
                        d.last = Some(t);
                    }
                }
                gst::PadProbeReturn::Ok
            });
        }
    });
    run(&pipeline)?;
    let out = counts.lock().unwrap().clone();
    Ok(out)
}

/// Bytes per frame from a raw-audio pad's current caps: the `bpf` field when
/// the decoder sets it, else channels times the sample size of `format`.
fn raw_audio_bpf(pad: &gst::Pad) -> Option<u64> {
    let caps = pad.current_caps()?;
    let s = caps.structure(0)?;
    if !s.name().starts_with("audio/") {
        return None;
    }
    if let Ok(bpf) = s.get::<i32>("bpf")
        && bpf > 0
    {
        return Some(bpf as u64);
    }
    let channels = s.get::<i32>("channels").unwrap_or(2).max(1) as u64;
    let format = s.get::<&str>("format").unwrap_or("F32LE");
    let bytes = match format.trim_end_matches(['L', 'B', 'E']) {
        "F32" | "S32" | "F64" | "S24_32" => {
            if format.starts_with("F64") {
                8
            } else {
                4
            }
        }
        "S24" => 3,
        "S16" => 2,
        "U8" | "S8" => 1,
        _ => 4,
    };
    Some(channels * bytes)
}

/// Run a pipeline to EOS, returning the duration it reported, and fail on an
/// error or a timeout.
fn run(pipeline: &gst::Pipeline) -> Result<Option<gst::ClockTime>> {
    pipeline.set_state(gst::State::Playing)?;
    let bus = pipeline.bus().context("pipeline bus")?;
    let deadline = Instant::now() + TIMEOUT;
    let mut duration: Option<gst::ClockTime> = None;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            let _ = pipeline.set_state(gst::State::Null);
            bail!("timed out decoding");
        }
        let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_nseconds(left.as_nanos() as u64),
            &[
                gst::MessageType::Eos,
                gst::MessageType::Error,
                gst::MessageType::AsyncDone,
                gst::MessageType::StateChanged,
            ],
        ) else {
            continue;
        };
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(_) => {
                if duration.is_none() {
                    duration = pipeline.query_duration();
                }
                break;
            }
            MessageView::AsyncDone(_) | MessageView::StateChanged(_) => {
                if duration.is_none() {
                    duration = pipeline.query_duration();
                }
            }
            MessageView::Error(err) => {
                let debug = err.debug().map(|d| format!(" ({d})")).unwrap_or_default();
                let _ = pipeline.set_state(gst::State::Null);
                bail!(
                    "decode error in {}: {}{debug}",
                    err.src().map(|s| s.name().to_string()).unwrap_or_default(),
                    err.error()
                );
            }
            _ => {}
        }
    }
    let _ = pipeline.set_state(gst::State::Null);
    Ok(duration)
}
