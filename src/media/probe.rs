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

use crate::media::mp4;

/// What one demuxed stream is.
#[derive(Clone, Debug)]
struct Stream {
    /// "video" or "audio".
    media: &'static str,
    /// Codec name, e.g. "h264", "hevc", "prores", "aac".
    codec: String,
    width: usize,
    height: usize,
    /// The demuxer's frame rate, when its caps carry one.
    fps: Option<f64>,
}

/// The four-character code upstream's `probe` prints for a codec.
fn fourcc(codec: &str) -> &'static str {
    match codec {
        "h264" => "avc1",
        "hevc" => "hvc1",
        "prores" => "apcn",
        "aac" => "mp4a",
        "opus" => "Opus",
        "vp9" => "vp09",
        "av1" => "av01",
        _ => "????",
    }
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

/// A track's presentation span: `(start, duration)` in seconds, with the
/// edit list's skipped priming subtracted from the duration.
fn track_span(t: &mp4::TrackInfo) -> (f64, f64) {
    let ts = t.timescale.max(1) as f64;
    let priming = t
        .edits
        .iter()
        .find(|e| e.media_time > 0)
        .map(|e| e.media_time as f64 / ts)
        .unwrap_or(0.0);
    (t.first_pts, (t.duration as f64 / ts - priming).max(0.0))
}

/// The asset's duration: the end of its longest track, like
/// `AVURLAsset.duration` upstream (a dropped-slot file's audio can run one
/// buffer past the video, and ffprobe reports that longer figure too).
fn movie_duration(tracks: &[mp4::TrackInfo]) -> f64 {
    tracks
        .iter()
        .map(|t| {
            let (start, dur) = track_span(t);
            start + dur
        })
        .fold(0.0, f64::max)
}

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
    if std::env::var_os("REC_DEBUG").is_some() {
        eprintln!("rec: debug: reading mp4 boxes");
    }
    let movie =
        mp4::read(&abs).with_context(|| format!("reading the boxes of {}", abs.display()))?;
    if std::env::var_os("REC_DEBUG").is_some() {
        eprintln!("rec: debug: mp4 boxes ok; structure");
    }
    let (duration, streams) = structure(&abs)?;
    if std::env::var_os("REC_DEBUG").is_some() {
        eprintln!("rec: debug: structure ok; decode");
    }
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
    // The duration comes from the real samples, not the muxer's declared
    // movie duration: a crash-left fragmented file is as long as its fragments
    // (4.000 s, not 1.984 s), and a finalised file must not count the AAC
    // priming that its edit list skips (2.000/2.006 s, not 2.049 s).
    // The demuxer's duration is the last resort; the mp4 tables are the truth.
    let fallback = if movie.movie_timescale > 0 && movie.duration > 0 {
        movie.duration as f64 / movie.movie_timescale as f64
    } else {
        duration.map(|d| d.seconds_f64()).unwrap_or(0.0)
    };
    // The asset's duration is its longest track's end (AVURLAsset.duration),
    // not the first video track's: a dropped slot can leave audio longer.
    let media_secs = match movie_duration(&movie.tracks) {
        d if d > 0.0 => d,
        _ => fallback,
    };
    let playable = !streams.is_empty();
    println!("playable  : {playable}");
    println!("duration  : {media_secs:.3}s");
    // The MP4 track layout (alternate group, enabled flag, edit list) is not
    // something qtdemux exposes as caps, so it comes from the boxes themselves.
    println!(
        "fragmented: {}",
        if movie.fragmented { "yes" } else { "no" }
    );
    for (i, s) in streams.iter().enumerate() {
        let t = movie.tracks.get(i);
        let (start, dur) = t.map(track_span).unwrap_or((0.0, fallback));
        let kbps = t
            .filter(|_| dur > 0.0)
            .map(|t| t.bytes as f64 * 8.0 / dur / 1000.0)
            .unwrap_or(0.0);
        let media = if s.media == "video" { "vide" } else { "soun" };
        let mut line = format!(
            "track {i}  : {media} {} enabled={} start={start:.3}s dur={dur:.3}s {kbps:.1} kb/s",
            fourcc(&s.codec),
            t.is_none_or(|t| t.enabled)
        );
        if s.media == "video" {
            let fps = t
                .and_then(|t| t.nominal_fps)
                .or(s.fps)
                .or_else(|| decoded.iter().filter(|d| d.video).find_map(|d| d.fps()))
                .unwrap_or(0.0);
            line += &format!(" {}x{} @ {fps:.3} fps", s.width, s.height);
        }
        if let Some(t) = t {
            line += &format!("  [alt-group {} samples={}", t.alternate_group, t.samples);
            for e in &t.edits {
                if e.media_time >= 0 {
                    line += &format!(" elst media_time={}", e.media_time);
                } else {
                    line += &format!(" elst empty={}", e.segment_duration);
                }
            }
            line += "]";
        }
        println!("{line}");
    }
    if movie.tracks.len() != streams.len() {
        // The demuxer and the box parser disagree (an unusual file): show the
        // box view too rather than guessing which is right.
        for (i, t) in movie.tracks.iter().enumerate() {
            println!(
                "mp4 {i}    : {} #{} {} alt-group {} {} samples, {} edits",
                t.handler.label(),
                t.track_id,
                if t.enabled { "enabled" } else { "disabled" },
                t.alternate_group,
                t.samples,
                t.edits.len()
            );
        }
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
        let debug = std::env::var_os("REC_DEBUG").is_some();
        if let Some(caps) = pad.current_caps().or_else(|| Some(pad.query_caps(None)))
            && let Some(s) = describe(&caps)
        {
            if debug {
                eprintln!("rec: debug: structure pad {} {s:?}", pad.name());
            }
            found2.lock().unwrap().push(s);
        }
        // Consume the stream so the pipeline reaches EOS. The sinks are added
        // while the pipeline runs: `sync=false` keeps them off the pipeline
        // clock and `async=false` keeps them out of preroll, so a late pad
        // cannot leave the whole pipeline waiting for one it will never get.
        if let Ok(sink) = gst::ElementFactory::make("fakesink").build()
            && let Ok(sink_pad) = sink.static_pad("sink").context("fakesink sink pad")
            && pipe.add(&sink).is_ok()
            && pad.link(&sink_pad).is_ok()
        {
            sink.set_property("sync", false);
            sink.set_property("async", false);
            if debug {
                eprintln!("rec: debug: structure linked {}", pad.name());
            }
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
    let (width, height, fps) = if media == "video" {
        let w = s.get::<i32>("width").unwrap_or(0).max(0) as usize;
        let h = s.get::<i32>("height").unwrap_or(0).max(0) as usize;
        let fps = s
            .get::<gst::Fraction>("framerate")
            .ok()
            .map(|f| f.numer() as f64 / f.denom().max(1) as f64)
            .filter(|f| *f > 0.0);
        (w, h, fps)
    } else {
        (0, 0, None)
    };
    Some(Stream {
        media,
        codec: codec.to_string(),
        width,
        height,
        fps,
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
        sink.set_property("async", false);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::mp4::{Edit, Handler};

    fn track(handler: Handler, timescale: u32, duration: u64, edits: Vec<Edit>) -> mp4::TrackInfo {
        mp4::TrackInfo {
            track_id: 1,
            handler,
            timescale,
            duration,
            enabled: true,
            alternate_group: 0,
            edits,
            samples: 1,
            bytes: 1,
            first_pts: 0.0,
            nominal_fps: None,
        }
    }

    #[test]
    fn the_asset_duration_is_the_longest_track() {
        // A 2 s video with dropped slots leaves the audio one buffer longer
        // (2.0213 s); AVFoundation and ffprobe report 2.021, not 2.000.
        let video = track(Handler::Video, 3000, 6000, vec![]);
        let audio = track(
            Handler::Audio,
            48_000,
            103_000,
            vec![Edit {
                segment_duration: 102_000,
                media_time: 1024,
            }],
        );
        let d = movie_duration(&[video, audio]);
        let expected = (103_000.0 - 1024.0) / 48_000.0;
        assert!((d - expected).abs() < 1e-9, "{d} != {expected}");
        assert!(d > 2.0);
    }

    #[test]
    fn the_edit_list_priming_is_not_counted() {
        let audio = track(
            Handler::Audio,
            48_000,
            96_000,
            vec![Edit {
                segment_duration: 94_000,
                media_time: 2048,
            }],
        );
        let d = movie_duration(&[audio]);
        assert!((d - (96_000.0 - 2048.0) / 48_000.0).abs() < 1e-9, "{d}");
    }
}
