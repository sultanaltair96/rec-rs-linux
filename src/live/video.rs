//! Real-time H.264 / HEVC encoding with GStreamer for the replay buffer and
//! streaming (the recording keeps its own writer pipeline at its own quality).
//!
//! appsrc (our frames, host-clock pts) → videoconvert → videoscale (Lanczos)
//! → encoder (VA-API, `x264enc`/`x265enc`, `openh264enc` fallback) →
//! `h264parse`/`h265parse` → appsink: one access unit per buffer in AVCC/HVCC
//! form with the decoder configuration record on the caps, which is exactly
//! what RTMP's FLV tags and the replay MP4 take (WHIP converts it to Annex B).
//!
//! Real time (a stream cannot fall behind), no B-frames (the FLV/RTP
//! timestamps are presentation times), a keyframe every `keyframe_secs` and a
//! forced one when a stream starts or reconnects, BT.709 limited-range tags in
//! the VUI, and CBR or average bitrate per `--vbr`. Frames of another size are
//! scaled to the encoder's size here, with the same Lanczos method as the
//! recording; frames are never upscaled.
//!
//! HEVC is Main with an 8-bit encoder (the dev image's x265); Main10 needs a
//! 10-bit encoder (VA-API on a GPU host) and is recorded as a gap in
//! `PARITY.md`.
//!
//! Adapted from rec-rs src/live/video.rs (MIT OR Apache-2.0): the VideoToolbox
//! compression session is replaced by the GStreamer pipeline; the parameter-set
//! and configuration-record parsing keeps upstream's shapes.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app;
use gstreamer_video as gst_video;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};

use crate::media_types::{PixelFormat, VideoFrame};

/// Encoder settings (the shared encode's, see [`LiveConfig`](super::LiveConfig)).
#[derive(Clone, Debug, PartialEq)]
pub struct VideoConfig {
    pub width: usize,
    pub height: usize,
    pub fps: u32,
    /// Bits per second.
    pub bitrate: u64,
    /// Seconds between keyframes (streams need 2 s or less).
    pub keyframe_secs: f64,
    /// Constant bitrate (what streaming services expect).
    pub cbr: bool,
    /// HEVC instead of H.264.
    pub hevc: bool,
}

/// One encoded access unit: AVCC (4-byte length-prefixed NAL units) with the
/// parameter sets and configuration record its caps carry.
#[derive(Clone, Debug)]
pub struct VideoPacket {
    /// Host-clock seconds.
    pub pts: f64,
    pub hevc: bool,
    pub key: bool,
    pub data: Arc<[u8]>,
    /// SPS and PPS (H.264) or VPS, SPS and PPS (HEVC), without start codes.
    pub param_sets: Vec<Vec<u8>>,
    /// Decoder configuration record (`avcC` / `hvcC`).
    pub record: Vec<u8>,
    pub width: usize,
    pub height: usize,
}

pub type OnPacket = Arc<dyn Fn(VideoPacket) + Send + Sync>;

/// Encoder elements to try, most capable first, exactly as the writer's list:
/// a distribution may have none of the RPM Fusion ones, and VA-API needs a
/// render node (without `/dev/dri` GStreamer registers no VA element at all).
pub fn encoder_candidates(hevc: bool) -> &'static [&'static str] {
    if hevc {
        &[
            "vah265enc",
            "vaapih265enc",
            "x265enc",
            "avenc_libx265",
            "avenc_hevc",
        ]
    } else {
        &[
            "vah264enc",
            "vaapih264enc",
            "x264enc",
            "openh264enc",
            "avenc_libx264",
            "avenc_h264",
        ]
    }
}

fn usable(name: &str) -> bool {
    if (name.starts_with("vah") || name.starts_with("vaapi"))
        && !std::path::Path::new("/dev/dri").exists()
    {
        return false;
    }
    gst::ElementFactory::find(name).is_some()
}

/// True when a live HEVC encoder is installed (else the shared encode stays
/// H.264, with a note).
pub fn has_encoder(hevc: bool) -> bool {
    encoder_candidates(hevc).iter().any(|n| usable(n))
}

/// True when the HEVC encoder is a 10-bit-capable one (VA-API), i.e. when a
/// Main10 encode could be produced. The shared encode feeds 8-bit frames
/// today, so this is only what decides the note the caller logs.
pub fn main10_capable() -> bool {
    encoder_candidates(true)
        .iter()
        .any(|n| (n.starts_with("vah") || n.starts_with("vaapi")) && usable(n))
}

/// Set a property from text, so the value is converted to the element's own
/// type and a missing property is skipped instead of panicking.
pub(crate) fn set(el: &gst::Element, name: &str, value: impl ToString) {
    if el.find_property(name).is_some() {
        el.set_property_from_str(name, &value.to_string());
    }
}

/// Build the first installed encoder from `candidates` and configure it for a
/// real-time stream.
pub(crate) fn make_encoder(cfg: &VideoConfig) -> Result<gst::Element> {
    let mut errors = vec![];
    for name in encoder_candidates(cfg.hevc) {
        if !usable(name) {
            continue;
        }
        let el = match gst::ElementFactory::make(name).build() {
            Ok(el) => el,
            Err(e) => {
                errors.push(format!("{name}: {e}"));
                continue;
            }
        };
        // Bitrate: kbit/s for x264enc/x265enc and the VA-API encoders, bits per
        // second for the rest (a long-standing GStreamer inconsistency).
        let kbps = matches!(
            *name,
            "x264enc" | "x265enc" | "vah264enc" | "vah265enc" | "vaapih264enc" | "vaapih265enc"
        );
        let bitrate = if kbps {
            (cfg.bitrate / 1000).max(1)
        } else {
            cfg.bitrate.max(1000)
        };
        set(&el, "bitrate", bitrate);
        for prop in ["key-int-max", "gop-size", "keyframe-period"] {
            if el.find_property(prop).is_some() {
                let frames = (cfg.keyframe_secs * cfg.fps as f64).round().max(1.0) as u64;
                set(&el, prop, frames);
                break;
            }
        }
        // No B-frames, no lookahead, no delayed output: a stream must emit
        // every frame as soon as it is captured.
        for prop in ["bframes", "bf"] {
            set(&el, prop, 0);
        }
        for prop in ["rc-lookahead", "sync-lookahead"] {
            if el.find_property(prop).is_some() {
                set(&el, prop, 0);
            }
        }
        set(&el, "b-adapt", false);
        match *name {
            "x264enc" => {
                set(&el, "tune", "zerolatency");
                set(&el, "speed-preset", "veryfast");
                // CBR: x264's ABR plus a VBV buffer (the element's "cbr" pass).
                // --vbr: plain ABR, which is what upstream's average bitrate is.
                set(&el, "pass", if cfg.cbr { "cbr" } else { "pass1" });
                set(&el, "vbv-buf-capacity", 600);
            }
            "x265enc" => {
                // x265's zerolatency tune disables B-frames as well; a VBV
                // keeps ABR from running away on noisy frames (dev/ELEMENTS.md).
                set(&el, "tune", "zerolatency");
                set(&el, "speed-preset", "veryfast");
                let kbps = (cfg.bitrate / 1000).max(1);
                set(
                    &el,
                    "option-string",
                    format!("vbv-maxrate={kbps}:vbv-bufsize={}", (kbps / 2).max(1)),
                );
            }
            "openh264enc" => {
                set(&el, "rate-control", "bitrate");
                set(&el, "usage-type", "screen");
                set(&el, "complexity", "medium");
                set(&el, "gop-size", (cfg.keyframe_secs * cfg.fps as f64) as u64);
            }
            "vah264enc" | "vah265enc" => {
                set(&el, "rate-control", if cfg.cbr { "cbr" } else { "vbr" });
            }
            _ => {}
        }
        return Ok(el);
    }
    bail!(
        "no live {} encoder is installed: tried {} (x264enc comes with RPM Fusion's \
         gst-plugins-ugly, x265enc with gst-plugins-bad-freeworld)",
        if cfg.hevc { "HEVC" } else { "H.264" },
        if errors.is_empty() {
            encoder_candidates(cfg.hevc).join(", ")
        } else {
            errors.join("; ")
        }
    )
}

/// The GStreamer format name of a frame's pixel layout.
pub(crate) fn input_format(format: PixelFormat) -> &'static str {
    match format {
        PixelFormat::Bgra => "BGRA",
        PixelFormat::Rgba => "RGBA",
        PixelFormat::Nv12 => "NV12",
        PixelFormat::I420 => "I420",
    }
}

/// Copy a [`VideoFrame`] into a GStreamer buffer of `caps`' layout. Rows are
/// copied one by one, so a source stride wider than the frame (a padded device
/// buffer) is packed correctly.
pub(crate) fn frame_buffer(frame: &VideoFrame, caps: &gst::Caps) -> Option<gst::Buffer> {
    let info = gst_video::VideoInfo::from_caps(caps).ok()?;
    let mut buf = gst::Buffer::with_size(info.size()).ok()?;
    {
        let buf = buf.get_mut()?;
        let mut map = buf.map_writable().ok()?;
        let data = map.as_mut_slice();
        data.fill(0);
        for (i, plane) in frame.planes.iter().enumerate() {
            if i >= info.offset().len() || plane.stride == 0 {
                break;
            }
            let rows = frame.format.plane_rows(i, frame.height).min(plane.rows());
            let row_bytes = frame.format.min_stride(i, frame.width);
            let dst_offset = info.offset()[i];
            let dst_stride = info.stride()[i].max(row_bytes as i32) as usize;
            for row in 0..rows {
                let src = row * plane.stride;
                let dst = dst_offset + row * dst_stride;
                if src + row_bytes > plane.data.len() || dst + row_bytes > data.len() {
                    return None;
                }
                data[dst..dst + row_bytes].copy_from_slice(&plane.data[src..src + row_bytes]);
            }
        }
    }
    Some(buf)
}

/// The parameter sets a decoder configuration record carries (`avcC`: SPS,
/// PPS; `hvcC`: VPS, SPS, PPS).
pub fn record_parameter_sets(record: &[u8]) -> Vec<Vec<u8>> {
    let mut out = vec![];
    if record.len() < 7 {
        return out;
    }
    if record[0] != 1 {
        return out;
    }
    // hvcC has 22 bytes of configuration before the arrays; avcC has 5.
    let hevc = record.len() > 22 && record[22] != 0 && record[5] & 0x1f != 1;
    if hevc {
        let mut i = 22usize;
        let Some(&count) = record.get(i) else {
            return out;
        };
        i += 1;
        for _ in 0..count {
            if i + 3 > record.len() {
                break;
            }
            let n = u16::from_be_bytes([record[i + 1], record[i + 2]]) as usize;
            i += 3;
            for _ in 0..n {
                let Some(len) = record
                    .get(i..i + 2)
                    .map(|b| u16::from_be_bytes([b[0], b[1]]) as usize)
                else {
                    return out;
                };
                i += 2;
                match record.get(i..i + len) {
                    Some(nal) => out.push(nal.to_vec()),
                    None => return out,
                }
                i += len;
            }
        }
    } else {
        let mut i = 5usize;
        let Some(&sets) = record.get(i) else {
            return out;
        };
        i += 1;
        for _ in 0..(sets & 0x1f) {
            let Some(len) = record
                .get(i..i + 2)
                .map(|b| u16::from_be_bytes([b[0], b[1]]) as usize)
            else {
                return out;
            };
            i += 2;
            match record.get(i..i + len) {
                Some(nal) => out.push(nal.to_vec()),
                None => return out,
            }
            i += len;
        }
        let Some(&pps) = record.get(i) else {
            return out;
        };
        i += 1;
        for _ in 0..pps {
            let Some(len) = record
                .get(i..i + 2)
                .map(|b| u16::from_be_bytes([b[0], b[1]]) as usize)
            else {
                return out;
            };
            i += 2;
            match record.get(i..i + len) {
                Some(nal) => out.push(nal.to_vec()),
                None => return out,
            }
            i += len;
        }
    }
    out
}

/// AVCC (length-prefixed) to Annex B (start codes), for RTP/WebRTC.
pub fn avcc_to_annexb(avcc: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(avcc.len() + 16);
    let mut i = 0;
    while i + 4 <= avcc.len() {
        let n = u32::from_be_bytes([avcc[i], avcc[i + 1], avcc[i + 2], avcc[i + 3]]) as usize;
        i += 4;
        if i + n > avcc.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&avcc[i..i + n]);
        i += n;
    }
    out
}

/// The NAL unit types that start a new coded picture with an intra refresh
/// (H.264 IDR, HEVC IDR_W_RADL / IDR_N_LP), used when the pipeline does not
/// flag the buffer itself.
fn is_idr(hevc: bool, avcc: &[u8]) -> bool {
    let mut i = 0;
    while i + 4 <= avcc.len() {
        let n = u32::from_be_bytes([avcc[i], avcc[i + 1], avcc[i + 2], avcc[i + 3]]) as usize;
        i += 4;
        if n == 0 || i + n > avcc.len() {
            return false;
        }
        let nal = &avcc[i..i + n];
        if hevc {
            if (nal[0] >> 1) & 0x3f == 19 || (nal[0] >> 1) & 0x3f == 20 {
                return true;
            }
        } else if nal[0] & 0x1f == 5 {
            return true;
        }
        i += n;
    }
    false
}

/// A host-clock time as a GStreamer timestamp (nanoseconds since boot).
pub(crate) fn host_time(t: f64) -> gst::ClockTime {
    gst::ClockTime::from_nseconds((t.max(0.0) * 1e9).round() as u64)
}

/// The constant GStreamer's software H.264/HEVC encoders add to every output
/// timestamp in this build (`x264enc` and `x265enc` both shift by
/// 3,600,000 s; `openh264enc` does not). Rather than assume a value, the shift
/// is measured from the first frame through the pipeline and removed, so a
/// packet always carries its frame's host time whatever the encoder does.
#[derive(Clone, Default)]
struct PtsShift {
    first_input: Arc<std::sync::Mutex<Option<u64>>>,
    offset: Arc<std::sync::Mutex<Option<u64>>>,
    /// The last output timestamp and interval, for a buffer that arrives
    /// without one (an encoder does that for a frame it synthesises).
    last: Arc<std::sync::Mutex<Option<(u64, u64)>>>,
}

impl PtsShift {
    fn remember_input(&self, t: f64) {
        let mut first = self.first_input.lock().unwrap();
        if first.is_none() {
            *first = Some(host_time(t).nseconds());
        }
    }

    /// The host time (seconds) of an output buffer, with the encoder's shift
    /// removed. A buffer without a timestamp takes the next slot after the
    /// previous one.
    fn host_secs(&self, out_ns: Option<u64>) -> f64 {
        let first = *self.first_input.lock().unwrap();
        let mut last = self.last.lock().unwrap();
        let out_ns = match out_ns {
            Some(ns) => {
                if let Some((prev, _)) = *last
                    && ns > prev
                {
                    *last = Some((ns, ns - prev));
                } else if last.is_none() {
                    *last = Some((ns, 1));
                }
                ns
            }
            None => match *last {
                Some((prev, step)) => prev.saturating_add(step),
                None => {
                    let ns = first.unwrap_or(0);
                    *last = Some((ns, 1));
                    ns
                }
            },
        };
        let offset = {
            let mut offset = self.offset.lock().unwrap();
            *offset.get_or_insert_with(|| match first {
                Some(f) => out_ns.wrapping_sub(f),
                None => 0,
            })
        };
        out_ns.wrapping_sub(offset) as f64 / 1e9
    }
}

/// A running live video encoder.
pub struct VideoEncoder {
    pipeline: gst::Pipeline,
    bus: gst::Bus,
    appsrc: gstreamer_app::AppSrc,
    pub cfg: VideoConfig,
    /// The element that was picked (for logs and `probe`-style reporting).
    pub encoder: String,
    /// The input frame layout the appsrc caps currently describe.
    input: Option<(PixelFormat, usize, usize)>,
    /// Those caps, reused for every frame of the same layout.
    input_caps: Option<gst::Caps>,
    /// The encoder's output-timestamp shift (see [`PtsShift`]).
    shift: PtsShift,
    dropped: u64,
    force_key: bool,
}

impl VideoEncoder {
    /// Build the pipeline for output size `size` (`cfg.width`x`cfg.height` as
    /// fitted by the caller) and start it.
    pub fn new(cfg: VideoConfig, on_packet: OnPacket) -> Result<VideoEncoder> {
        let encoder = make_encoder(&cfg)?;
        let encoder_name = encoder.name().to_string();
        let shift = PtsShift::default();
        let pipeline = gst::Pipeline::new();
        let appsrc = gstreamer_app::AppSrc::builder()
            .name("live-video")
            .format(gst::Format::Time)
            .is_live(false)
            .do_timestamp(false)
            .block(false)
            .property("max-buffers", 4u64)
            .build();
        let convert = gst::ElementFactory::make("videoconvert")
            .build()
            .map_err(|e| anyhow!("videoconvert: {e}"))?;
        let scale = gst::ElementFactory::make("videoscale")
            .build()
            .map_err(|e| anyhow!("videoscale: {e}"))?;
        set(&scale, "method", "lanczos");
        let size_filter = gst::ElementFactory::make("capsfilter")
            .build()
            .map_err(|e| anyhow!("capsfilter: {e}"))?;
        size_filter.set_property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("format", "I420")
                .field("width", cfg.width as i32)
                .field("height", cfg.height as i32)
                .field("framerate", gst::Fraction::new(cfg.fps.max(1) as i32, 1))
                // BT.709 limited range, so the VUI says so and players and
                // services do not have to guess (upstream tags the bitstream
                // the same way).
                .field("colorimetry", "bt709")
                .build(),
        );
        let parser = gst::ElementFactory::make(if cfg.hevc { "h265parse" } else { "h264parse" })
            .build()
            .map_err(|e| anyhow!("{}parse: {e}", if cfg.hevc { "h265" } else { "h264" }))?;
        set(&parser, "config-interval", -1);
        let out_filter = gst::ElementFactory::make("capsfilter")
            .build()
            .map_err(|e| anyhow!("capsfilter: {e}"))?;
        out_filter.set_property(
            "caps",
            gst::Caps::builder(if cfg.hevc {
                "video/x-h265"
            } else {
                "video/x-h264"
            })
            .field("stream-format", if cfg.hevc { "hvc1" } else { "avc" })
            .field("alignment", "au")
            .build(),
        );
        let sink = gstreamer_app::AppSink::builder()
            .name("live-video-out")
            .sync(false)
            .async_(false)
            .max_buffers(0)
            .callbacks(
                gstreamer_app::AppSinkCallbacks::builder()
                    .new_sample({
                        let on_packet = on_packet.clone();
                        let hevc = cfg.hevc;
                        let shift = shift.clone();
                        move |sink| {
                            if let Ok(sample) = sink.pull_sample() {
                                on_sample(&sample, hevc, &shift, &on_packet);
                            }
                            Ok(gst::FlowSuccess::Ok)
                        }
                    })
                    .build(),
            )
            .build();
        let elements: Vec<gst::Element> = vec![
            appsrc.clone().upcast(),
            convert,
            scale,
            size_filter,
            encoder.clone(),
            parser,
            out_filter,
            sink.upcast(),
        ];
        pipeline.add_many(&elements)?;
        gst::Element::link_many(&elements)?;
        let bus = pipeline.bus().ok_or_else(|| anyhow!("pipeline bus"))?;
        // Wait for the state change: pushing into a pipeline that is still
        // going to PLAYING fails with FLUSHING and the first frames of a stream
        // would be lost.
        pipeline.set_state(gst::State::Playing)?;
        if let (Err(e), _, _) = pipeline.state(gst::ClockTime::from_seconds(2)) {
            let _ = pipeline.set_state(gst::State::Null);
            bail!("the live video pipeline could not start: {e:?}");
        }
        Ok(VideoEncoder {
            pipeline,
            bus,
            appsrc,
            cfg: cfg.clone(),
            encoder: encoder_name,
            input: None,
            input_caps: None,
            shift,
            dropped: 0,
            force_key: false,
        })
    }

    /// Frames dropped because the encoder could not keep up (or the pipeline
    /// refused them).
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Ask for a keyframe on the next frame (a new stream viewer, a reconnect,
    /// a codec or size change).
    pub fn force_keyframe(&mut self) {
        self.force_key = true;
    }

    /// Encode one frame captured at host time `t`. Frames of another size or
    /// layout than the pipeline was built for are renegotiated in place.
    pub fn encode(&mut self, frame: &VideoFrame, t: f64) -> bool {
        if !frame.is_well_formed() {
            self.dropped += 1;
            return false;
        }
        self.shift.remember_input(t);
        let input = (frame.format, frame.width, frame.height);
        if self.input != Some(input) {
            let caps = gst::Caps::builder("video/x-raw")
                .field("format", input_format(frame.format))
                .field("width", frame.width as i32)
                .field("height", frame.height as i32)
                .field(
                    "framerate",
                    gst::Fraction::new(self.cfg.fps.max(1) as i32, 1),
                )
                .build();
            self.appsrc.set_caps(Some(&caps));
            let bytes = frame.byte_len().max(1) as u64;
            self.appsrc.set_max_bytes(bytes * 4);
            self.input_caps = Some(caps);
            self.input = Some(input);
        }
        let Some(caps) = self.input_caps.as_ref() else {
            self.dropped += 1;
            return false;
        };
        let Some(mut buf) = frame_buffer(frame, caps) else {
            self.dropped += 1;
            return false;
        };
        {
            // Host-clock timestamps throughout: every packet keeps the time of
            // the frame or audio it carries, so the replay ring, RTMP's FLV
            // timestamps and WHIP's RTP times all agree without a shared base.
            let buf = buf.get_mut().unwrap();
            buf.set_pts(host_time(t));
        }
        if self.force_key {
            self.force_key = false;
            let _ = self.appsrc.send_event(
                gst_video::DownstreamForceKeyUnitEvent::builder()
                    .all_headers(true)
                    .build(),
            );
        }
        match self.appsrc.push_buffer(buf) {
            Ok(_) => true,
            Err(e) => {
                if std::env::var_os("REC_DEBUG").is_some() {
                    eprintln!("rec: debug: live video push failed: {e:?}");
                }
                self.dropped += 1;
                false
            }
        }
    }

    /// Take the first pipeline error, if one arrived (a failed encoder).
    pub fn error(&self) -> Option<String> {
        while let Some(msg) = self.bus.pop() {
            use gst::MessageView;
            if let MessageView::Error(err) = msg.view() {
                return Some(format!(
                    "live video encoder ({}): {}",
                    self.encoder,
                    err.error()
                ));
            }
        }
        None
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Turn one appsink sample into a [`VideoPacket`] for the hub.
fn on_sample(sample: &gst::Sample, hevc: bool, shift: &PtsShift, on_packet: &OnPacket) {
    let Some(buffer) = sample.buffer() else {
        return;
    };
    let Ok(map) = buffer.map_readable() else {
        return;
    };
    let data: Arc<[u8]> = Arc::from(map.as_slice());
    let caps = sample.caps();
    let structure = caps.and_then(|c| c.structure(0));
    let record: Vec<u8> = structure
        .and_then(|s| s.get::<gst::Buffer>("codec_data").ok())
        .and_then(|b| b.map_readable().ok().map(|m| m.as_slice().to_vec()))
        .unwrap_or_default();
    let width = structure
        .and_then(|s| s.get::<i32>("width").ok())
        .map(|w| w.max(0) as usize)
        .unwrap_or(0);
    let height = structure
        .and_then(|s| s.get::<i32>("height").ok())
        .map(|h| h.max(0) as usize)
        .unwrap_or(0);
    let pts = shift.host_secs(buffer.pts().map(|t| t.nseconds()));
    // The parser clears DELTA_UNIT on a sync sample; the NAL scan covers a
    // pipeline whose parser does not flag it.
    let key = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) || is_idr(hevc, &data);
    let packet = VideoPacket {
        pts,
        hevc,
        key,
        data,
        param_sets: record_parameter_sets(&record),
        record,
        width,
        height,
    };
    on_packet(packet);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_types::PixelFormat;
    use std::sync::Mutex;

    fn init() {
        let _ = gst::init();
    }

    /// An I420 frame of `w`x`h` (`VideoFrame::packed` is for packed formats;
    /// planar ones need their planes).
    fn i420_frame(pts: f64, w: usize, h: usize, luma: u8) -> VideoFrame {
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        VideoFrame {
            pts,
            width: w,
            height: h,
            format: PixelFormat::I420,
            planes: vec![
                crate::media_types::Plane::new(vec![luma; w * h], w),
                crate::media_types::Plane::new(vec![128; cw * ch], cw),
                crate::media_types::Plane::new(vec![128; cw * ch], cw),
            ],
        }
    }

    #[test]
    fn avcc_conversions() {
        let avcc = [0, 0, 0, 2, 0x65, 0x88, 0, 0, 0, 1, 0x41];
        assert_eq!(
            avcc_to_annexb(&avcc),
            vec![0, 0, 0, 1, 0x65, 0x88, 0, 0, 0, 1, 0x41]
        );
    }

    /// Both codecs give their parameter sets and configuration record (what
    /// RTMP's sequence header and WHIP's keyframes carry).
    #[test]
    fn parameter_sets_and_records() {
        init();
        for hevc in [false, true] {
            if !has_encoder(hevc) {
                eprintln!(
                    "skipping: no live {} encoder installed",
                    if hevc { "HEVC" } else { "H.264" }
                );
                continue;
            }
            let got: Arc<Mutex<Vec<VideoPacket>>> = Arc::default();
            let g = got.clone();
            let cfg = VideoConfig {
                width: 640,
                height: 360,
                fps: 30,
                bitrate: 2_000_000,
                keyframe_secs: 1.0,
                cbr: true,
                hevc,
            };
            let mut enc =
                VideoEncoder::new(cfg, Arc::new(move |p| g.lock().unwrap().push(p))).unwrap();
            let frame = i420_frame(10.0, 640, 360, 0x40);
            for i in 0..5 {
                assert!(enc.encode(&frame, 10.0 + i as f64 / 30.0));
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while got.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let got = got.lock().unwrap();
            let p = got.first().expect("a packet").clone();
            assert!(p.key && p.hevc == hevc);
            assert!(!p.param_sets.is_empty(), "parameter sets from the caps");
            assert_eq!(p.record[0], 1, "configuration version");
            assert_eq!((p.width, p.height), (640, 360));
            if hevc {
                assert_eq!(p.param_sets.len(), 3, "VPS, SPS, PPS");
                assert_eq!(p.param_sets[0][0] >> 1 & 0x3f, 32, "VPS NAL type");
            } else {
                assert_eq!(p.param_sets.len(), 2, "SPS, PPS");
                assert_eq!(p.param_sets[0][0] & 0x1f, 7, "SPS NAL type");
                assert_eq!(p.record[1], 100, "High profile");
            }
        }
    }

    #[test]
    fn encodes_keyframes_on_schedule() {
        init();
        if !has_encoder(false) {
            eprintln!("skipping: no H.264 encoder installed");
            return;
        }
        let got: Arc<Mutex<Vec<(f64, bool, usize)>>> = Arc::default();
        let g = got.clone();
        let cfg = VideoConfig {
            width: 640,
            height: 360,
            fps: 30,
            bitrate: 2_000_000,
            keyframe_secs: 1.0,
            cbr: true,
            hevc: false,
        };
        let mut enc = VideoEncoder::new(
            cfg,
            Arc::new(move |p| g.lock().unwrap().push((p.pts, p.key, p.data.len()))),
        )
        .unwrap();
        // Source frames are 1280x720: the pipeline scales them.
        let frame = i420_frame(1000.0, 1280, 720, 0x40);
        for i in 0..90 {
            assert!(enc.encode(&frame, 1000.0 + i as f64 / 30.0));
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while got.lock().unwrap().len() < 90 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let got = got.lock().unwrap();
        assert!(got.len() >= 90, "{} packets", got.len());
        let keys: Vec<usize> = got
            .iter()
            .enumerate()
            .filter(|(_, p)| p.1)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(keys.first(), Some(&0), "starts with a keyframe");
        assert!(
            keys.len() >= 3,
            "a keyframe at least every second: {keys:?}"
        );
        let first = got[0].0;
        assert!(
            (first - 1000.0).abs() < 0.05,
            "a packet keeps its frame's host time: {first}"
        );
        assert!((got[1].0 - got[0].0 - 1.0 / 30.0).abs() < 1e-3);
    }

    /// A frame with a stride wider than its width is packed row by row.
    #[test]
    fn padded_frames_are_packed() {
        init();
        let caps = gst::Caps::builder("video/x-raw")
            .field("format", "BGRA")
            .field("width", 4i32)
            .field("height", 2i32)
            .build();
        let mut data = Vec::new();
        for _row in 0..2u8 {
            for x in 0..4u8 {
                data.extend_from_slice(&[x, x, x, 0xff]);
            }
            data.extend_from_slice(&[9, 9, 9, 9]); // padding
        }
        let frame = VideoFrame {
            pts: 1.0,
            width: 4,
            height: 2,
            format: PixelFormat::Bgra,
            planes: vec![crate::media_types::Plane::new(data, 20)],
        };
        assert!(frame.is_well_formed());
        let buf = frame_buffer(&frame, &caps).unwrap();
        let map = buf.map_readable().unwrap();
        assert_eq!(map.as_slice().len(), 32);
        assert_eq!(&map.as_slice()[..4], &[0, 0, 0, 0xff]);
        assert_eq!(&map.as_slice()[16..20], &[0, 0, 0, 0xff], "second row");
        assert_eq!(&map.as_slice()[28..32], &[3, 3, 3, 0xff]);
    }

    /// Encoder quality harness, kept from upstream (`scripts/encquality.sh`):
    /// encodes the raw BGRA frames of REC_ENC_SRC with REC_ENC_BITRATE (b/s),
    /// REC_ENC_HEVC=1, REC_ENC_FPS and writes an Annex B stream to REC_ENC_OUT.
    #[test]
    #[ignore]
    fn encode_file() {
        init();
        use std::io::{Read, Write};
        let src = std::env::var("REC_ENC_SRC").expect("REC_ENC_SRC");
        let out = std::env::var("REC_ENC_OUT").expect("REC_ENC_OUT");
        let bitrate: u64 = std::env::var("REC_ENC_BITRATE").unwrap().parse().unwrap();
        let hevc = std::env::var("REC_ENC_HEVC").is_ok_and(|v| v == "1");
        let fps: u32 = std::env::var("REC_ENC_FPS").map_or(60, |v| v.parse().unwrap());
        let (w, h) = (3840usize, 2160usize);
        let mut ff = std::process::Command::new("ffmpeg")
            .args([
                "-v", "error", "-i", &src, "-f", "rawvideo", "-pix_fmt", "bgra", "-",
            ])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut pipe = ff.stdout.take().unwrap();
        let packets = Arc::new(Mutex::new(Vec::<(bool, Vec<u8>, Vec<Vec<u8>>)>::new()));
        let p2 = packets.clone();
        let cfg = VideoConfig {
            width: w,
            height: h,
            fps,
            bitrate,
            keyframe_secs: 2.0,
            cbr: true,
            hevc,
        };
        let mut enc = VideoEncoder::new(
            cfg,
            Arc::new(move |p| {
                p2.lock()
                    .unwrap()
                    .push((p.key, avcc_to_annexb(&p.data), p.param_sets.clone()));
            }),
        )
        .unwrap();
        let mut frame = vec![0u8; w * h * 4];
        let mut n = 0;
        while pipe.read_exact(&mut frame).is_ok() {
            let f = VideoFrame::packed(1000.0, w, h, PixelFormat::Bgra, frame.clone());
            enc.encode(&f, 1000.0 + n as f64 / fps as f64);
            n += 1;
            // Real time: the encoder sees frames at the stream's pace.
            std::thread::sleep(std::time::Duration::from_secs_f64(1.0 / fps as f64));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while packets.lock().unwrap().len() < n && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        drop(enc);
        let _ = ff.wait();
        let mut f = std::fs::File::create(&out).unwrap();
        for (key, data, sets) in packets.lock().unwrap().iter() {
            if *key {
                for ps in sets {
                    f.write_all(&[0, 0, 0, 1]).unwrap();
                    f.write_all(ps).unwrap();
                }
            }
            f.write_all(data).unwrap();
        }
        println!("encoded {n} frames -> {out}");
    }
}
