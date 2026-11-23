//! Replay buffer: the last N seconds of encoded video and audio, kept in
//! memory from a keyframe on, saved to MP4 on demand without stopping.
//!
//! The packets are the shared live encode's, so saving costs no encode and no
//! interruption: the ring is snapshotted under its lock, and a background
//! thread writes the clip. The video is passed through (AVCC/HVCC with the
//! encoder's configuration record); the audio is decoded from the buffered AAC
//! and encoded again, with the new encoder's priming frames dropped, so
//! `ffmpeg`, `mpv` and QuickTime agree on the clip's sync — the same thing
//! upstream does with AVAssetWriter, and what the recording writer does for
//! its AAC track.
//!
//! Adapted from rec-rs src/live/replay.rs (MIT OR Apache-2.0): AVAssetWriter is
//! replaced by a GStreamer mp4mux pipeline, and the ring is kept verbatim.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use super::Formats;
use super::audio::{self, AAC_FRAME, AacDecoder};
use crate::media_types::EncodedPacket;

/// What the ring needs to know about a packet.
pub trait Timed {
    fn pts(&self) -> f64;
    fn key(&self) -> bool {
        true
    }
    fn size(&self) -> usize;
}

impl Timed for EncodedPacket {
    fn pts(&self) -> f64 {
        self.pts
    }
    fn key(&self) -> bool {
        self.keyframe
    }
    fn size(&self) -> usize {
        self.data.len()
    }
}

/// Ring of the last `secs` seconds, always starting at a video keyframe.
pub struct Ring<V, A> {
    pub secs: f64,
    video: VecDeque<V>,
    audio: VecDeque<A>,
    bytes: usize,
}

impl<V: Timed + Clone, A: Timed + Clone> Ring<V, A> {
    pub fn new(secs: f64) -> Self {
        Ring {
            secs,
            video: VecDeque::new(),
            audio: VecDeque::new(),
            bytes: 0,
        }
    }

    pub fn push_video(&mut self, v: V) {
        // Wait for a keyframe to start.
        if self.video.is_empty() && !v.key() {
            return;
        }
        self.bytes += v.size();
        self.video.push_back(v);
        self.trim();
    }

    pub fn push_audio(&mut self, a: A) {
        // Keep a little audio before the first keyframe: the saved clip feeds
        // it to the decoder as warm-up before the clip's first sample.
        if self
            .video
            .front()
            .is_none_or(|v| a.pts() < v.pts() - AUDIO_LEAD)
        {
            return;
        }
        self.bytes += a.size();
        self.audio.push_back(a);
    }

    /// Drop whole GOPs that are older than the window (keeping the keyframe
    /// that starts it, so the saved clip always decodes).
    fn trim(&mut self) {
        let Some(latest) = self.video.back().map(|v| v.pts()) else {
            return;
        };
        let horizon = latest - self.secs;
        // Newest keyframe at or before the horizon.
        let cut = self
            .video
            .iter()
            .enumerate()
            .filter(|(_, v)| v.key() && v.pts() <= horizon)
            .map(|(i, _)| i)
            .last();
        if let Some(cut) = cut.filter(|&c| c > 0) {
            for v in self.video.drain(..cut) {
                self.bytes -= v.size();
            }
            let start = self.video[0].pts() - AUDIO_LEAD;
            while self.audio.front().is_some_and(|a| a.pts() < start) {
                let a = self.audio.pop_front().unwrap();
                self.bytes -= a.size();
            }
        }
    }

    /// Bytes held (encoded payloads), and packets held.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Seconds of video held.
    pub fn held(&self) -> f64 {
        match (self.video.front(), self.video.back()) {
            (Some(a), Some(b)) => b.pts() - a.pts(),
            _ => 0.0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.video.is_empty()
    }

    /// The last `secs` (or everything held), starting at a keyframe.
    pub fn snapshot(&self) -> (Vec<V>, Vec<A>) {
        let latest = self.video.back().map_or(0.0, |v| v.pts());
        let horizon = latest - self.secs;
        let start = self
            .video
            .iter()
            .enumerate()
            .filter(|(_, v)| v.key() && v.pts() <= horizon.max(self.video[0].pts()))
            .map(|(i, _)| i)
            .last()
            .unwrap_or(0);
        let v: Vec<V> = self.video.iter().skip(start).cloned().collect();
        let t0 = v.first().map_or(0.0, |v| v.pts());
        let a = self
            .audio
            .iter()
            .filter(|a| a.pts() >= t0 - AUDIO_LEAD)
            .cloned()
            .collect();
        (v, a)
    }

    pub fn clear(&mut self) {
        self.video.clear();
        self.audio.clear();
        self.bytes = 0;
    }
}

pub type Replay = Ring<EncodedPacket, EncodedPacket>;

/// Audio kept before the clip start (more than any AAC priming delay).
const AUDIO_LEAD: f64 = 0.2;
/// The replay's audio is re-encoded at this bitrate.
const REPLAY_AUDIO_BITRATE: u32 = 192_000;

/// Write a snapshot to an MP4 (video passed through, audio re-encoded;
/// blocking). `formats` is the shared encode's configuration for the packets
/// in the snapshot.
pub fn write_mp4(
    path: &Path,
    video_packets: &[EncodedPacket],
    audio_packets: &[EncodedPacket],
    formats: &Formats,
) -> Result<PathBuf> {
    let Some(first) = video_packets.first() else {
        bail!("the replay buffer is empty");
    };
    if formats.record.is_empty() {
        bail!("the replay buffer has no video configuration yet");
    }
    let t0 = first.pts;
    let abs = std::path::absolute(path)?;
    if let Some(dir) = abs.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    // The clip is as long as its video: audio that arrives after the last
    // frame (the tap can be a little ahead) is cut, as upstream does.
    let last_v = video_packets.iter().map(|p| p.pts).fold(t0, f64::max) - t0;
    let pcm = decode_audio(audio_packets, formats, t0, last_v + 0.05)?;
    mux_mp4(&abs, video_packets, t0, &pcm, formats)
        .with_context(|| format!("writing the replay to {}", abs.display()))?;
    Ok(abs)
}

/// Decoded PCM of the clip, starting exactly at `t0`.
fn decode_audio(
    packets: &[EncodedPacket],
    formats: &Formats,
    t0: f64,
    max_secs: f64,
) -> Result<Vec<[f32; 2]>> {
    // The decoder's own warm-up shifts where its output starts (see
    // `audio::decoder_offset`); the cut is sample-accurate because of it.
    if packets.is_empty() || formats.aac_cookie.is_empty() {
        return Ok(vec![]);
    }
    let rate = audio::RATE as f64;
    let mut dec = AacDecoder::new(&formats.aac_cookie)?;
    for p in packets {
        dec.push(p.pts, &p.data)?;
    }
    let decoded = dec.decode_all()?;
    if std::env::var_os("REC_DEBUG").is_some() {
        eprintln!(
            "rec: debug: replay audio: {} packets, decoder {} (offset {} samples)",
            packets.len(),
            dec.decoder,
            dec.offset
        );
    }
    // Never longer than the picture: the last PCM the muxer gets ends with it.
    let limit = (max_secs.max(0.0) * rate).round() as usize;
    let mut out: Vec<[f32; 2]> = Vec::with_capacity(limit.min(1 << 20));
    for buf in decoded {
        // Sample-accurate cut: the clip's audio starts at t0, and the packets
        // before it are only the decoder's warm-up (which is why they are in
        // the ring at all).
        let skip = (((t0 - buf.pts) * rate).round().max(0.0) as i64 + dec.offset).max(0) as usize;
        for sample in buf.samples.iter().skip(skip) {
            if out.len() >= limit {
                break;
            }
            out.push(*sample);
        }
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

/// Feed the clip through an mp4mux pipeline.
fn mux_mp4(
    path: &Path,
    video_packets: &[EncodedPacket],
    t0: f64,
    pcm: &[[f32; 2]],
    formats: &Formats,
) -> Result<()> {
    let hevc = formats.hevc;
    let pipeline = gst::Pipeline::new();
    let video_caps = gst::Caps::builder(if hevc { "video/x-h265" } else { "video/x-h264" })
        .field("stream-format", if hevc { "hvc1" } else { "avc" })
        .field("alignment", "au")
        .field(
            "codec_data",
            gst::Buffer::from_slice(formats.record.clone()),
        )
        .build();
    let vsrc = gstreamer_app::AppSrc::builder()
        .name("replay-video")
        .format(gst::Format::Time)
        .caps(&video_caps)
        .is_live(false)
        .do_timestamp(false)
        .build();
    let parser = gst::ElementFactory::make(if hevc { "h265parse" } else { "h264parse" })
        .build()
        .context("the bitstream parser is missing (gst-plugins-bad)")?;
    let vqueue = gst::ElementFactory::make("queue").build()?;
    let mux = gst::ElementFactory::make("mp4mux").build()?;
    let sink = gst::ElementFactory::make("filesink").build()?;
    sink.set_property("location", path.to_string_lossy().to_string());
    sink.set_property("sync", false);

    // Everything goes into the pipeline before any pad is linked: a muxer pad
    // can only link to a branch in the same bin.
    let video_chain: Vec<gst::Element> = vec![vsrc.clone().upcast(), parser, vqueue.clone()];
    let mut audio_chain: Vec<gst::Element> = vec![];
    let mut asrc = None;
    let mut priming: Option<(gst::Element, u64)> = None;
    if !pcm.is_empty() {
        let caps = gst::Caps::builder("audio/x-raw")
            .field("format", "F32LE")
            .field("layout", "interleaved")
            .field("rate", audio::RATE as i32)
            .field("channels", 2i32)
            .build();
        let src = gstreamer_app::AppSrc::builder()
            .name("replay-audio")
            .format(gst::Format::Time)
            .caps(&caps)
            .is_live(false)
            .do_timestamp(false)
            .build();
        // fdkaacenc takes 16-bit samples; the converter is what lets the mux
        // pipeline take our f32 mix without a second copy in this code.
        let convert = gst::ElementFactory::make("audioconvert")
            .build()
            .map_err(|e| anyhow!("audioconvert: {e}"))?;
        let enc = gst::ElementFactory::make("fdkaacenc")
            .build()
            .or_else(|_| gst::ElementFactory::make("avenc_aac").build())
            .context("no AAC encoder for the replay's audio")?;
        if enc.find_property("bitrate").is_some() {
            enc.set_property_from_str("bitrate", &REPLAY_AUDIO_BITRATE.to_string());
        }
        // The encoder's priming, plus one frame: a decoder needs one frame of
        // history to reconstruct the first frame it plays, so the file carries
        // that much extra priming instead of starting on a cold decoder.
        // Measured on the written clip with `scripts/synccheck.py` (10 s of the
        // test pattern, 720p30): priming alone +27.0 ms, priming + 1 frame
        // +5.7 ms, +2 frames -15.6 ms.
        let drop_frames = audio::primitive_samples(enc.name().as_str()) / AAC_FRAME as u64 + 1;
        let aparse = gst::ElementFactory::make("aacparse").build()?;
        let q = gst::ElementFactory::make("queue").build()?;
        priming = Some((aparse.clone(), drop_frames));
        audio_chain = vec![src.clone().upcast(), convert, enc, aparse, q];
        asrc = Some(src);
    }
    let mut all = video_chain.clone();
    all.extend(audio_chain.iter().cloned());
    all.push(mux.clone());
    all.push(sink.clone());
    pipeline.add_many(&all)?;
    gst::Element::link_many(&video_chain)?;
    let video_pad = mux
        .request_pad_simple("video_0")
        .context("the muxer has no video_0 pad")?;
    vqueue
        .static_pad("src")
        .context("video queue src pad")?
        .link(&video_pad)?;
    if !audio_chain.is_empty() {
        gst::Element::link_many(&audio_chain)?;
        let audio_pad = mux
            .request_pad_simple("audio_0")
            .context("the muxer has no audio_0 pad")?;
        audio_chain
            .last()
            .and_then(|q| q.static_pad("src"))
            .context("audio queue src pad")?
            .link(&audio_pad)?;
    }
    gst::Element::link(&mux, &sink)?;

    // Drop the AAC encoder's priming frames and move the rest back by the time
    // they covered, so the clip's audio starts at 0 with its picture (the
    // recording writer does the same; GStreamer's mp4mux cannot write the edit
    // list that AVFoundation writes upstream).
    if let Some((aparse, drop_frames)) = priming {
        use std::sync::atomic::{AtomicU64, Ordering};
        let seen = AtomicU64::new(0);
        let shift = AtomicU64::new(u64::MAX);
        if let Some(pad) = aparse.static_pad("src") {
            pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
                if seen.load(Ordering::Relaxed) < drop_frames {
                    seen.fetch_add(1, Ordering::Relaxed);
                    return gst::PadProbeReturn::Drop;
                }
                if let Some(buf) = info.buffer_mut() {
                    let pts = buf.pts().map(|p| p.nseconds()).unwrap_or(0);
                    let s = match shift.compare_exchange(
                        u64::MAX,
                        pts,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => pts,
                        Err(v) => v,
                    };
                    if let Some(b) = buf.get_mut() {
                        b.set_pts(gst::ClockTime::from_nseconds(pts.saturating_sub(s)));
                        if let Some(dts) = b.dts() {
                            b.set_dts(gst::ClockTime::from_nseconds(
                                dts.nseconds().saturating_sub(s),
                            ));
                        }
                    }
                }
                gst::PadProbeReturn::Ok
            });
        }
    }

    let bus = pipeline.bus().context("pipeline bus")?;
    pipeline.set_state(gst::State::Playing)?;
    let ns = |t: f64| gst::ClockTime::from_nseconds((t * 1e9).max(0.0) as u64);
    // Video first: it is all in memory, and the muxer interleaves by PTS.
    for p in video_packets {
        let mut buf = gst::Buffer::from_slice(p.data.to_vec());
        {
            let b = buf.get_mut().unwrap();
            b.set_pts(ns(p.pts - t0));
            b.set_dts(ns(p.dts - t0));
            if !p.keyframe {
                b.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        vsrc.push_buffer(buf)
            .map_err(|e| anyhow!("the replay muxer refused a video packet: {e}"))?;
    }
    vsrc.end_of_stream().ok();
    if let Some(src) = &asrc {
        // 20 ms of PCM per buffer, timed from the clip's start.
        let chunk = (audio::RATE as usize / 50).max(1);
        for (i, block) in pcm.chunks(chunk).enumerate() {
            let t = i as f64 * chunk as f64 / audio::RATE as f64;
            let mut bytes = Vec::with_capacity(block.len() * 8);
            for s in block {
                bytes.extend_from_slice(&s[0].to_le_bytes());
                bytes.extend_from_slice(&s[1].to_le_bytes());
            }
            let mut buf = gst::Buffer::from_slice(bytes);
            buf.get_mut().unwrap().set_pts(ns(t));
            src.push_buffer(buf)
                .map_err(|e| anyhow!("the replay muxer refused audio: {e}"))?;
        }
        src.end_of_stream().ok();
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        use gst::MessageView;
        let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(1),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) else {
            if std::time::Instant::now() > deadline {
                let _ = pipeline.set_state(gst::State::Null);
                bail!("timed out writing the replay");
            }
            continue;
        };
        match msg.view() {
            MessageView::Eos(_) => break,
            MessageView::Error(err) => {
                let _ = pipeline.set_state(gst::State::Null);
                bail!(
                    "{} ({})",
                    err.error(),
                    err.debug().unwrap_or_else(|| "no detail".into())
                );
            }
            _ => {}
        }
    }
    let _ = pipeline.set_state(gst::State::Null);
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size < 1000 {
        bail!("the replay file is empty");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_types::{PacketKind, VideoCodec};
    use std::sync::Arc;

    #[derive(Clone, Debug, PartialEq)]
    struct P(f64, bool, usize);
    impl Timed for P {
        fn pts(&self) -> f64 {
            self.0
        }
        fn key(&self) -> bool {
            self.1
        }
        fn size(&self) -> usize {
            self.2
        }
    }

    /// 30 fps video with a keyframe every second, audio every 21 ms.
    fn fill(r: &mut Ring<P, P>, secs: f64) {
        let mut a = 0.0;
        for i in 0..(secs * 30.0) as usize {
            let t = i as f64 / 30.0;
            r.push_video(P(t, i % 30 == 0, 1000));
            while a <= t {
                r.push_audio(P(a, true, 100));
                a += 1024.0 / 48000.0;
            }
        }
    }

    #[test]
    fn keeps_the_window_from_a_keyframe() {
        let mut r = Ring::new(10.0);
        fill(&mut r, 60.0);
        let (v, a) = r.snapshot();
        assert!(v[0].1, "starts at a keyframe");
        let span = v.last().unwrap().0 - v[0].0;
        assert!((10.0..11.0).contains(&span), "10-11 s of video, got {span}");
        // Audio starts up to AUDIO_LEAD early (decoder warm-up for the clip).
        assert!(
            a[0].0 < v[0].0 && v[0].0 - a[0].0 <= AUDIO_LEAD + 0.03,
            "audio lead-in: {} vs {}",
            a[0].0,
            v[0].0
        );
        // Memory stays bounded (about 11 s held, not 60).
        assert!(r.held() < 11.1);
        assert!(r.bytes() < 12 * (30 * 1000 + 47 * 100));
    }

    #[test]
    fn waits_for_a_keyframe_and_handles_short_buffers() {
        let mut r: Ring<P, P> = Ring::new(30.0);
        r.push_video(P(0.0, false, 10));
        r.push_audio(P(0.0, true, 10));
        assert_eq!(r.bytes(), 0);
        fill(&mut r, 3.0);
        let (v, _) = r.snapshot();
        assert_eq!(v.len(), 90, "less than the window: everything");
        r.clear();
        assert_eq!((r.bytes(), r.held()), (0, 0.0));
    }

    fn packet(pts: f64, key: bool) -> EncodedPacket {
        EncodedPacket {
            pts,
            dts: pts,
            keyframe: key,
            data: Arc::from(vec![0u8; 64]),
            kind: PacketKind::Video(VideoCodec::H264),
        }
    }

    /// A one-GOP clip whose configuration record says nothing still refuses to
    /// write a broken file rather than inventing one.
    #[test]
    fn an_empty_snapshot_is_a_clear_error() {
        let dir = std::env::temp_dir().join(format!("rec-replay-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let err = write_mp4(&dir.join("x.mp4"), &[], &[], &Formats::default()).unwrap_err();
        assert_eq!(err.to_string(), "the replay buffer is empty");
        let err = write_mp4(
            &dir.join("x.mp4"),
            &[packet(0.0, true)],
            &[],
            &Formats::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("no video configuration"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
