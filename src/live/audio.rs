//! AAC and Opus encoding of the live audio mix (48 kHz stereo f32) with
//! GStreamer, and the AAC decoding the replay writer needs.
//!
//! appsrc (the post-fader mix, host-clock pts) → audioconvert → audioresample
//! → `fdkaacenc`/`avenc_aac` (raw AAC + AudioSpecificConfig for RTMP and the
//! replay buffer) or `opusenc` (20 ms packets for WHIP) → appsink. One packet
//! per buffer, so the FLV audio tags and RTP payloads are exactly the buffer.
//!
//! Adapted from rec-rs src/live/audio.rs (MIT OR Apache-2.0): AudioToolbox's
//! `AudioConverter` is replaced by the GStreamer pipelines above; the packet
//! geometry (1024 vs 960 frames) and the priming handling keep upstream's
//! shapes.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};

use crate::media_types::AudioChunk;
pub use crate::media_types::AudioCodec;

/// Sample rate of the live encode, like upstream's `RATE`.
pub const RATE: u32 = 48_000;
/// Frames of one AAC-LC packet.
pub const AAC_FRAME: usize = 1024;
/// Frames of one 20 ms Opus packet.
pub const OPUS_FRAME: usize = 960;

/// AudioSpecificConfig for AAC-LC at 48 kHz stereo (FLV/RTMP sequence header
/// and the replay MP4's `esds` fallback).
pub const AAC_ASC: [u8; 2] = [0x11, 0x90];

/// One encoded audio packet. `pts` is in host-clock seconds.
#[derive(Clone, Debug)]
pub struct AudioPacket {
    pub codec: AudioCodec,
    pub pts: f64,
    /// Frames in the packet (the codec's fixed geometry: 1024 for AAC, 960 for
    /// a 20 ms Opus packet).
    #[allow(dead_code)]
    pub frames: u32,
    pub data: Arc<[u8]>,
    /// The encoder's codec configuration (AAC AudioSpecificConfig), empty for
    /// Opus.
    pub cookie: Vec<u8>,
}

pub type OnPacket = Arc<dyn Fn(AudioPacket) + Send + Sync>;

impl AudioCodec {
    /// Frames in one packet.
    pub fn frames_per_packet(self) -> usize {
        match self {
            AudioCodec::Aac => AAC_FRAME,
            AudioCodec::Opus => OPUS_FRAME,
        }
    }
}

/// AAC encoder elements to try, most capable first (the writer's list).
pub const AAC_CANDIDATES: &[&str] = &["fdkaacenc", "avenc_aac", "voaacenc", "faac"];

/// Samples an AAC encoder holds back before its first frame (its priming).
///
/// Measured for each element in `dev/ELEMENTS.md`: fdkaacenc 2048, avenc_aac
/// 1024. The replay writer drops that many frames and re-stamps the rest, as
/// the recording writer does, so a clip's audio starts where its video does.
pub fn primitive_samples(element: &str) -> u64 {
    match element {
        "fdkaacenc" => 2048,
        "avenc_aac" | "faac" => 1024,
        "voaacenc" => 0,
        _ => 1024,
    }
}

/// How far a decoder's output lags the stream position its buffers are labelled
/// with, in samples (measured on this image: the same click through
/// `fdkaacenc` and `avenc_aac` decoded again, dev/ELEMENTS.md's method).
///
/// An AAC encoder's priming frames mean packet `n` holds input samples
/// `[n*1024 - priming, …)`; that is what [`AudioPacket::pts`] corrects for, so a
/// packet's label is the time of the audio it carries. A decoder then emits its
/// own warm-up before the content lines up again, and the replay writer's
/// sample-accurate cut has to know it:
///
/// | decoder | offset (samples) |
/// |---|---|
/// | `avdec_aac` (gst-libav, the default) | +400 |
/// | `fdkaacdec` | +720 |
/// | `faad` | 0 (untested, no element in the image) |
///
/// Measured on clips written by `rec-rs record --replay` and checked with
/// `scripts/synccheck.py` — the decoder's warm-up depends on the timestamps it
/// is fed, which is why this is a measured constant next to the encoder
/// priming table above rather than a formula.
pub fn decoder_offset(element: &str) -> i64 {
    match element {
        "avdec_aac" => 400,
        "fdkaacdec" => 720,
        _ => 0,
    }
}

fn set(el: &gst::Element, name: &str, value: impl ToString) {
    if el.find_property(name).is_some() {
        el.set_property_from_str(name, &value.to_string());
    }
}

/// Learn the codec configuration buffer a caps field carries.
pub(crate) fn cookie_from_caps(caps: Option<&gst::CapsRef>) -> Vec<u8> {
    caps.and_then(|c| c.structure(0))
        .and_then(|s| s.get::<gst::Buffer>("codec_data").ok())
        .and_then(|b| b.map_readable().ok().map(|m| m.as_slice().to_vec()))
        .unwrap_or_default()
}

/// A running live audio encoder (one packet per buffer from appsink).
pub struct AudioEncoder {
    pipeline: gst::Pipeline,
    bus: gst::Bus,
    appsrc: gstreamer_app::AppSrc,
    /// The codec this encoder produces (the packet geometry the tests assert
    /// on; each output subscribes to one of them).
    #[allow(dead_code)]
    pub codec: AudioCodec,
    /// The element that was picked, for logs.
    pub encoder: String,
    /// Input samples pushed since `base`, in frames: packets are counted from
    /// it, so a gap in the timeline does not shift the ones that follow.
    frames: u64,
    dropped: u64,
}

impl AudioEncoder {
    /// `bitrate` in bits per second.
    pub fn new(codec: AudioCodec, bitrate: u32, on_packet: OnPacket) -> Result<AudioEncoder> {
        let (encoder, encoder_name) = match codec {
            AudioCodec::Aac => {
                let mut made = None;
                // REC_LIVE_AAC pins the element (A/B measurements and hosts
                // without one of them); the default is the candidates' order.
                let only = std::env::var("REC_LIVE_AAC").ok();
                for name in AAC_CANDIDATES {
                    if let Some(o) = &only
                        && o != name
                    {
                        continue;
                    }
                    if gst::ElementFactory::find(name).is_none() {
                        continue;
                    }
                    let el = gst::ElementFactory::make(name)
                        .build()
                        .map_err(|e| anyhow!("{name}: {e}"))?;
                    match *name {
                        "fdkaacenc" => {
                            set(&el, "bitrate", bitrate);
                            set(&el, "rate-control", "cbr");
                        }
                        "avenc_aac" => {
                            set(&el, "bitrate", bitrate);
                        }
                        _ => {
                            set(&el, "bitrate", bitrate);
                        }
                    }
                    made = Some((el, name.to_string()));
                    break;
                }
                made.ok_or_else(|| {
                    anyhow!(
                        "no AAC encoder is installed: tried {} (fdkaacenc comes with \
                         fdk-aac-free, avenc_aac with gst-libav)",
                        AAC_CANDIDATES.join(", ")
                    )
                })?
            }
            AudioCodec::Opus => {
                let el = gst::ElementFactory::make("opusenc")
                    .build()
                    .map_err(|e| anyhow!("no Opus encoder (opusenc, gst-plugins-base): {e}"))?;
                // 20 ms frames, no padding: what WHIP's Opus expects.
                set(&el, "bitrate", bitrate);
                set(&el, "frame-size", 20);
                set(&el, "inband-fec", false);
                set(&el, "audio-type", "generic");
                (el, "opusenc".to_string())
            }
        };
        let pipeline = gst::Pipeline::new();
        let caps = gst::Caps::builder("audio/x-raw")
            .field("format", "F32LE")
            .field("layout", "interleaved")
            .field("rate", RATE as i32)
            .field("channels", 2i32)
            .build();
        let appsrc = gstreamer_app::AppSrc::builder()
            .name("live-audio")
            .format(gst::Format::Time)
            .caps(&caps)
            .is_live(false)
            .do_timestamp(false)
            .block(false)
            // Room for a second or two of audio: an encoder that falls behind
            // for a moment must not punch a hole in the stream (the tap in
            // front of it is the queue that drops under real pressure).
            .max_bytes(4 << 20)
            .property("max-buffers", 256u64)
            .build();
        let convert = gst::ElementFactory::make("audioconvert")
            .build()
            .map_err(|e| anyhow!("audioconvert: {e}"))?;
        let resample = gst::ElementFactory::make("audioresample")
            .build()
            .map_err(|e| anyhow!("audioresample: {e}"))?;
        let parse = gst::ElementFactory::make("aacparse").build().ok();
        let out_sink = gstreamer_app::AppSink::builder()
            .name("live-audio-out")
            .sync(false)
            .async_(false)
            .max_buffers(0)
            .callbacks(
                gstreamer_app::AppSinkCallbacks::builder()
                    .new_sample({
                        let on_packet = on_packet.clone();
                        let priming = if codec == AudioCodec::Aac {
                            primitive_samples(&encoder_name) as f64 / RATE as f64
                        } else {
                            // Opus carries its pre-skip in the stream header;
                            // the RTP/FLV time is the capture time.
                            0.0
                        };
                        move |sink| {
                            if let Ok(sample) = sink.pull_sample() {
                                on_audio_sample(&sample, codec, priming, &on_packet);
                            }
                            Ok(gst::FlowSuccess::Ok)
                        }
                    })
                    .build(),
            )
            .build();
        let mut elements: Vec<gst::Element> =
            vec![appsrc.clone().upcast(), convert, resample, encoder];
        // aacparse normalises the framing (raw, one packet per buffer) and puts
        // the AudioSpecificConfig on the caps.
        if let (AudioCodec::Aac, Some(parse)) = (codec, parse) {
            elements.push(parse);
        }
        elements.push(out_sink.upcast());
        pipeline.add_many(&elements)?;
        gst::Element::link_many(&elements)?;
        let bus = pipeline.bus().ok_or_else(|| anyhow!("pipeline bus"))?;
        pipeline.set_state(gst::State::Playing)?;
        if let (Err(e), _, _) = pipeline.state(gst::ClockTime::from_seconds(2)) {
            let _ = pipeline.set_state(gst::State::Null);
            bail!("the live audio pipeline could not start: {e:?}");
        }
        Ok(AudioEncoder {
            pipeline,
            bus,
            appsrc,
            codec,
            encoder: encoder_name,
            frames: 0,
            dropped: 0,
        })
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Feed the post-fader mix starting at host time `t`. A chunk that is not
    /// 48 kHz stereo is converted by the pipeline; a gap in `t` is passed
    /// through as a gap (the caller fills short holes with silence).
    pub fn push(&mut self, t: f64, chunk: &AudioChunk) -> bool {
        if chunk.rate == 0 || chunk.channels == 0 {
            return false;
        }
        let interleaved = interleaved(chunk);
        if interleaved.is_empty() {
            return false;
        }
        let mut buf = gst::Buffer::from_slice(interleaved);
        {
            // Host-clock timestamps, like the video: a packet carries the time
            // of its first sample.
            let buf = buf.get_mut().unwrap();
            buf.set_pts(crate::live::video::host_time(t));
        }
        self.frames += chunk.frames() as u64;
        match self.appsrc.push_buffer(buf) {
            Ok(_) => true,
            Err(_) => {
                self.dropped += 1;
                false
            }
        }
    }

    /// Push `frames` of silence starting at host time `t` (a gap bridged, or a
    /// session with no audio source at all).
    pub fn push_silence(&mut self, t: f64, frames: usize) -> bool {
        if frames == 0 {
            return true;
        }
        let chunk = AudioChunk::stereo(t, RATE, vec![0.0; frames * 2]);
        self.push(t, &chunk)
    }

    /// Take the first pipeline error, if one arrived.
    pub fn error(&self) -> Option<String> {
        while let Some(msg) = self.bus.pop() {
            use gst::MessageView;
            if let MessageView::Error(err) = msg.view() {
                return Some(format!(
                    "live audio encoder ({}): {}",
                    self.encoder,
                    err.error()
                ));
            }
        }
        None
    }
}

impl Drop for AudioEncoder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Interleaved f32 samples of a chunk, converted to stereo if it is mono.
fn interleaved(chunk: &AudioChunk) -> Vec<u8> {
    let mut out = Vec::with_capacity(chunk.samples.len() * 2 * 4);
    match chunk.channels {
        2 => {
            for s in &chunk.samples {
                out.extend_from_slice(&s.to_le_bytes());
            }
        }
        1 => {
            for s in &chunk.samples {
                out.extend_from_slice(&s.to_le_bytes());
                out.extend_from_slice(&s.to_le_bytes());
            }
        }
        _ => return vec![],
    }
    out
}

/// Turn one appsink sample into a packet whose label is the **time of the audio
/// it carries**: the AAC encoder's priming frames are taken out of the
/// timestamp, exactly as upstream subtracts `leading`, so a player that decodes
/// the packet and places it at its label gets it in step with the picture.
fn on_audio_sample(
    sample: &gst::Sample,
    codec: AudioCodec,
    priming_secs: f64,
    on_packet: &OnPacket,
) {
    let Some(buffer) = sample.buffer() else {
        return;
    };
    let Ok(map) = buffer.map_readable() else {
        return;
    };
    let data: Arc<[u8]> = Arc::from(map.as_slice());
    let pts = buffer.pts().map(|t| t.nseconds()).unwrap_or(0) as f64 / 1e9 - priming_secs;
    on_packet(AudioPacket {
        codec,
        pts,
        frames: codec.frames_per_packet() as u32,
        data,
        cookie: cookie_from_caps(sample.caps()),
    });
}

/// One decoded PCM buffer, with the time of its first sample.
#[derive(Clone, Debug)]
pub struct DecodedAudio {
    pub pts: f64,
    pub samples: Vec<[f32; 2]>,
}

/// Decodes raw AAC packets (as the live encoder emits them) back to 48 kHz
/// stereo f32, for the replay writer: the clip's audio is re-encoded from the
/// decoded mix so every player agrees on its sync, like upstream.
pub struct AacDecoder {
    pipeline: gst::Pipeline,
    bus: gst::Bus,
    appsrc: gstreamer_app::AppSrc,
    appsink: gstreamer_app::AppSink,
    /// The decoder element, and its output offset in samples.
    pub decoder: String,
    pub offset: i64,
}

impl AacDecoder {
    /// `cookie` is the AudioSpecificConfig from the encoder's caps.
    pub fn new(cookie: &[u8]) -> Result<AacDecoder> {
        if cookie.is_empty() {
            bail!("the AAC stream has no codec configuration (AudioSpecificConfig)");
        }
        let pipeline = gst::Pipeline::new();
        let caps = gst::Caps::builder("audio/mpeg")
            .field("mpegversion", 4i32)
            .field("stream-format", "raw")
            .field("framed", true)
            .field("rate", RATE as i32)
            .field("channels", 2i32)
            .field("codec_data", gst::Buffer::from_slice(cookie.to_vec()))
            .build();
        let appsrc = gstreamer_app::AppSrc::builder()
            .name("replay-aac")
            .format(gst::Format::Time)
            .caps(&caps)
            .is_live(false)
            .do_timestamp(false)
            .block(false)
            .build();
        let parse = gst::ElementFactory::make("aacparse")
            .build()
            .map_err(|e| anyhow!("aacparse: {e}"))?;
        // avdec_aac first: it is part of gst-libav (always installed) and its
        // output offset is the smallest of the two.
        let (decoder, name) = ["avdec_aac", "fdkaacdec", "faad"]
            .iter()
            .find(|n| gst::ElementFactory::find(n).is_some())
            .map(|n| (gst::ElementFactory::make(n).build(), n.to_string()))
            .ok_or_else(|| anyhow!("no AAC decoder is installed (avdec_aac or fdkaacdec)"))?;
        let decoder = decoder.map_err(|e| anyhow!("{name}: {e}"))?;
        let offset = decoder_offset(&name);
        let convert = gst::ElementFactory::make("audioconvert")
            .build()
            .map_err(|e| anyhow!("audioconvert: {e}"))?;
        let resample = gst::ElementFactory::make("audioresample")
            .build()
            .map_err(|e| anyhow!("audioresample: {e}"))?;
        let out_caps = gst::ElementFactory::make("capsfilter")
            .build()
            .map_err(|e| anyhow!("capsfilter: {e}"))?;
        out_caps.set_property(
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .field("layout", "interleaved")
                .field("rate", RATE as i32)
                .field("channels", 2i32)
                .build(),
        );
        let appsink = gstreamer_app::AppSink::builder()
            .name("replay-aac-out")
            .sync(false)
            .async_(false)
            .build();
        let elements: Vec<gst::Element> = vec![
            appsrc.clone().upcast(),
            parse,
            decoder,
            convert,
            resample,
            out_caps,
            appsink.clone().upcast(),
        ];
        pipeline.add_many(&elements)?;
        gst::Element::link_many(&elements)?;
        let bus = pipeline.bus().ok_or_else(|| anyhow!("pipeline bus"))?;
        pipeline.set_state(gst::State::Playing)?;
        if let (Err(e), _, _) = pipeline.state(gst::ClockTime::from_seconds(2)) {
            let _ = pipeline.set_state(gst::State::Null);
            bail!("the AAC decoder could not start: {e:?}");
        }
        Ok(AacDecoder {
            pipeline,
            bus,
            appsrc,
            appsink,
            decoder: name,
            offset,
        })
    }

    /// Push one packet at host time `t` (the decoder outputs it later, pulled
    /// by [`decode_all`](AacDecoder::decode_all)).
    pub fn push(&mut self, t: f64, data: &[u8]) -> Result<()> {
        let mut buf = gst::Buffer::from_slice(data.to_vec());
        buf.get_mut()
            .unwrap()
            .set_pts(crate::live::video::host_time(t));
        self.appsrc
            .push_buffer(buf)
            .map_err(|_| anyhow!("the AAC decoder refused a packet"))?;
        Ok(())
    }

    /// Decode every pushed packet (push first, then call this once); returns
    /// the PCM buffers in order with their host times.
    pub fn decode_all(&mut self) -> Result<Vec<DecodedAudio>> {
        self.appsrc
            .end_of_stream()
            .map_err(|_| anyhow!("the AAC decoder could not be drained"))?;
        let mut out = vec![];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Some(sample) = self
                .appsink
                .try_pull_sample(gst::ClockTime::from_mseconds(500))
            {
                push_decoded(&mut out, &sample);
                continue;
            }
            // No sample: EOS, an error, or just a slow pipeline.
            if let Some(msg) = self.bus.pop() {
                use gst::MessageView;
                match msg.view() {
                    MessageView::Eos(_) => break,
                    MessageView::Error(err) => bail!("AAC decoding failed: {}", err.error()),
                    _ => {}
                }
            }
            if std::time::Instant::now() > deadline {
                bail!("timed out decoding the replay buffer's audio");
            }
        }
        Ok(out)
    }
}

fn push_decoded(out: &mut Vec<DecodedAudio>, sample: &gst::Sample) {
    let Some(buffer) = sample.buffer() else {
        return;
    };
    let Ok(map) = buffer.map_readable() else {
        return;
    };
    let raw = map.as_slice();
    if raw.len() < 8 {
        return;
    }
    let mut samples = Vec::with_capacity(raw.len() / 8);
    for frame in raw.chunks_exact(8) {
        let l = f32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
        let r = f32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]);
        samples.push([l, r]);
    }
    let pts = buffer.pts().map(|t| t.nseconds()).unwrap_or(0) as f64 / 1e9;
    out.push(DecodedAudio { pts, samples });
}

impl Drop for AacDecoder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn init() {
        let _ = gst::init();
    }

    fn tone(n: usize) -> Vec<[f32; 2]> {
        (0..n)
            .map(|i| {
                let v = (i as f32 * 0.05).sin() * 0.3;
                [v, v]
            })
            .collect()
    }

    fn collect(
        codec: AudioCodec,
        bitrate: u32,
        t0: f64,
        chunks: &[(f64, usize)],
    ) -> Vec<AudioPacket> {
        let _ = t0;
        let got: Arc<Mutex<Vec<AudioPacket>>> = Arc::default();
        let g = got.clone();
        let mut enc =
            AudioEncoder::new(codec, bitrate, Arc::new(move |p| g.lock().unwrap().push(p)))
                .unwrap();
        for (t, n) in chunks {
            let samples: Vec<f32> = tone(*n).into_iter().flatten().collect();
            let chunk = AudioChunk::stereo(*t, RATE, samples);
            assert!(enc.push(*t, &chunk), "chunk at {t} accepted");
        }
        drain(&got, enc.codec.frames_per_packet() as f64)
    }

    /// Wait until the encoder has emitted everything it had (the count stops
    /// growing), then return the packets.
    fn drain(got: &Arc<Mutex<Vec<AudioPacket>>>, _frame: f64) -> Vec<AudioPacket> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut last = 0;
        let mut stable_since = std::time::Instant::now();
        while std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
            let n = got.lock().unwrap().len();
            if n != last {
                last = n;
                stable_since = std::time::Instant::now();
            } else if n > 0 && stable_since.elapsed() > std::time::Duration::from_millis(300) {
                break;
            }
        }
        std::mem::take(&mut *got.lock().unwrap())
    }

    #[test]
    fn aac_packets_with_timestamps() {
        init();
        let pk = collect(
            AudioCodec::Aac,
            160_000,
            100.0,
            &[(100.0, 48_000), (101.0, 48_000)],
        );
        assert!(pk.len() >= 90, "about 93 packets for 2 s, got {}", pk.len());
        assert!(
            pk.iter().all(|p| !p.data.is_empty() && p.data.len() < 2048),
            "raw AAC frames"
        );
        assert!(
            !pk[0].cookie.is_empty(),
            "AudioSpecificConfig from the caps"
        );
        // Consecutive packets are 1024 frames apart.
        let d = pk[10].pts - pk[9].pts;
        assert!((d - 1024.0 / 48000.0).abs() < 1e-6, "delta {d}");
        // Timestamps are anchored to the input clock (the first packet starts
        // at the chunk's time, less the encoder's priming at most).
        let first = pk[0].pts;
        assert!(
            (first - 100.0).abs() < 0.06,
            "first packet at {first}, input at 100.0"
        );
    }

    #[test]
    fn opus_packets() {
        init();
        let pk = collect(AudioCodec::Opus, 128_000, 5.0, &[(5.0, 48_000)]);
        assert!(pk.len() >= 45, "{} packets", pk.len());
        assert!(pk.iter().all(|p| p.frames == 960 && !p.data.is_empty()));
        let d = pk[10].pts - pk[9].pts;
        assert!((d - 960.0 / 48000.0).abs() < 1e-6, "delta {d}");
    }

    #[test]
    fn gap_reanchors_timeline() {
        init();
        let pk = collect(
            AudioCodec::Aac,
            128_000,
            10.0,
            &[(10.0, 4_800), (20.0, 48_000)],
        );
        // Half a packet in the first chunk, so the packets of the second chunk
        // follow its own time (10 s later), not the first chunk's.
        let last_first = pk
            .iter()
            .find(|p| p.pts > 19.0)
            .map(|p| p.pts)
            .expect("packets after the gap exist");
        assert!(
            (last_first - 20.0).abs() < 0.06,
            "after the gap, packets follow the new time: {last_first}"
        );
    }

    /// Encode two clicks, decode the packets again, and find where each click
    /// lands: the audio survives the round trip with a constant delay (the
    /// encoder's priming plus the decoder's own warm-up), never drift.
    ///
    /// The absolute delay is a property of the codec pair, not of the clip: the
    /// replay writer drops the re-encoder's priming frames and re-stamps the
    /// rest, so a saved clip's picture and sound start together — that is what
    /// `scripts/live/` measures on the written file with `synccheck.py`.
    #[test]
    fn round_trip_preserves_timing() {
        init();
        let clicks = [9_000usize, 30_000usize];
        let got: Arc<Mutex<Vec<AudioPacket>>> = Arc::default();
        let g = got.clone();
        let mut enc = AudioEncoder::new(
            AudioCodec::Aac,
            160_000,
            Arc::new(move |p| g.lock().unwrap().push(p)),
        )
        .unwrap();
        let input: Vec<f32> = (0..48_000)
            .flat_map(|i| {
                let on = clicks.iter().any(|c| (*c..*c + 48).contains(&i));
                let v = if on { 0.9 } else { 0.0 };
                [v, v]
            })
            .collect();
        let chunk = AudioChunk::stereo(0.0, RATE, input);
        enc.push(0.0, &chunk);
        let packets = drain(&got, 1024.0);
        let mut dec = AacDecoder::new(&packets[0].cookie).unwrap();
        for p in &packets {
            dec.push(p.pts, &p.data).unwrap();
        }
        let decoded = dec.decode_all().unwrap();
        // Where each click appears, in the decoded stream's own samples.
        let mut onsets = vec![];
        let mut index = 0usize;
        for buf in &decoded {
            if let Some(i) = buf.samples.iter().position(|s| s[0].abs() > 0.3) {
                onsets.push(index + i);
            }
            index += buf.samples.len();
        }
        assert_eq!(
            onsets.len(),
            clicks.len(),
            "both clicks decoded: {onsets:?}"
        );
        let delay = onsets[0] as i64 - clicks[0] as i64;
        let second = onsets[1] as i64 - clicks[1] as i64;
        assert_eq!(
            delay, second,
            "a constant delay, no drift: {delay} vs {second}"
        );
        // The delay is the AAC priming (a few 1024-sample frames); a clip's
        // writer removes it.
        assert!(
            (0..4 * 1024).contains(&delay),
            "the round-trip delay is the codec's priming: {delay} samples"
        );
    }

    #[test]
    fn mono_and_stereo_chunks_interleave() {
        let mono = AudioChunk::stereo(0.0, RATE, vec![0.5, 0.25]);
        let mono = AudioChunk {
            channels: 1,
            ..mono
        };
        let bytes = interleaved(&mono);
        assert_eq!(bytes.len(), 4 * 4);
        // The mono sample is duplicated into both channels.
        let l = f32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let r = f32::from_le_bytes(bytes[4..8].try_into().unwrap());
        assert_eq!((l, r), (0.5, 0.5));
    }
}
