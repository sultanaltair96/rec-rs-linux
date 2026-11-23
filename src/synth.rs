//! Synthetic "screen" and "system audio": a generated test pattern on the host
//! clock, for self-tests and encoder stress tests without a desktop.
//!
//! The frame flashes solid white once a second and the "system audio" beeps at
//! the same instant, so [`scripts/synccheck.py`] can measure the A/V offset of
//! a real recording, and [`scripts/levelcheck.py`] can measure the faders
//! (the beep is generated at −6 dBFS).
//!
//! `REC_SYNTH_DROP_AUDIO=N` drops every Nth audio buffer, like a glitching
//! device: the writer must fill the gap with silence so later audio does not
//! shift.
//!
//! Adapted from rec-rs src/synth.rs (MIT OR Apache-2.0), with CVPixelBuffer
//! replaced by [`VideoFrame`] and the CoreMedia sample buffer by
//! [`AudioChunk`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Result, bail};

use crate::media_types::{
    self, AudioChunk, AudioKind, AudioSink, AudioSource, FrameSink, PixelFormat, VideoFrame,
    VideoSource,
};

/// Length of the beep that starts with every white flash.
pub const BEEP_SECS: f64 = 0.04;
/// The beep's frequency.
pub const BEEP_HZ: f64 = 1000.0;
/// The beep's amplitude: 0.5 is −6 dBFS, the reference `levelcheck.py` uses.
pub const BEEP_AMPLITUDE: f32 = 0.5;
/// Frames per audio chunk (10 ms at 48 kHz), like a PipeWire device.
pub const AUDIO_CHUNK: usize = 480;

/// Samples `[start, start+n)` of the synthetic "system audio": a 1 kHz beep at
/// the start of every second (when the video flashes), silence otherwise.
/// Interleaved stereo.
pub fn beep_samples(start: i64, n: usize) -> Vec<f32> {
    let rate = crate::mixer::RATE as i64;
    let len = (BEEP_SECS * rate as f64) as i64;
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n as i64 {
        let s = start + i;
        let in_sec = s.rem_euclid(rate);
        let v = if in_sec < len {
            (BEEP_AMPLITUDE as f64
                * (2.0 * std::f64::consts::PI * BEEP_HZ * in_sec as f64 / rate as f64).sin())
                as f32
        } else {
            0.0
        };
        out.push(v);
        out.push(v);
    }
    out
}

/// xorshift64, so noise blocks are reproducible between runs.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// The test pattern's configuration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pattern {
    pub width: usize,
    pub height: usize,
    pub fps: u32,
    pub format: PixelFormat,
    /// Add random noise blocks to stress the encoder.
    pub noise: bool,
}

impl Default for Pattern {
    fn default() -> Self {
        Pattern {
            width: 1920,
            height: 1080,
            fps: 60,
            format: PixelFormat::Bgra,
            noise: false,
        }
    }
}

/// Draw frame `n` of the pattern.
///
/// Every `fps` frames (once a second) the frame is solid white so A/V offsets
/// can be measured against an audio click; otherwise a moving white box over a
/// horizontal gradient whose offset changes every frame, so the encoder sees
/// change.
fn draw(pattern: &Pattern, n: u64, rng: &mut Rng) -> VideoFrame {
    let (w, h) = (pattern.width, pattern.height);
    let flash = n % pattern.fps as u64 == 0;
    let box_x = ((n * 12) % w.saturating_sub(200).max(1) as u64) as usize;
    let box_y = h / 3;
    let block = 32usize;
    let mut planes = Vec::new();
    match pattern.format {
        PixelFormat::I420 => {
            // Y: the pattern's luminance; U/V: flat colour bars, white on a
            // flash.
            let mut y = vec![0u8; w * h];
            for row in 0..h {
                for x in 0..w {
                    let in_box = x >= box_x && x < box_x + 200 && row >= box_y && row < box_y + 200;
                    y[row * w + x] = if flash || in_box {
                        235
                    } else {
                        let g = (((x + n as usize * 4) % 256) as u8 / 2).wrapping_add(16);
                        let nz = if pattern.noise
                            && ((x / block) + (row / block) + n as usize) % 3 == 0
                        {
                            (rng.next() & 0xff) as u8
                        } else {
                            0
                        };
                        g ^ nz
                    };
                }
            }
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let mut u = vec![128u8; cw * ch];
            let mut v = vec![128u8; cw * ch];
            if !flash {
                for row in 0..ch {
                    for x in 0..cw {
                        u[row * cw + x] = 100 + (row % 56) as u8;
                        v[row * cw + x] = 150;
                    }
                }
            }
            planes.push(media_types::Plane::new(y, w));
            planes.push(media_types::Plane::new(u, cw));
            planes.push(media_types::Plane::new(v, cw));
        }
        _ => {
            // BGRA (or RGBA, whose channel order is swapped below).
            let mut px = vec![0u8; w * h * 4];
            for row in 0..h {
                for x in 0..w {
                    let at = (row * w + x) * 4;
                    let in_box = x >= box_x && x < box_x + 200 && row >= box_y && row < box_y + 200;
                    let rgba = if flash || in_box {
                        [255, 255, 255, 255]
                    } else {
                        let g = ((x + n as usize * 4) % 256) as u8;
                        let nz = if pattern.noise
                            && ((x / block) + (row / block) + n as usize) % 3 == 0
                        {
                            (rng.next() & 0xff) as u8
                        } else {
                            0
                        };
                        [g ^ nz, ((row * 255 / h.max(1)) as u8) ^ nz, 120 ^ nz, 255]
                    };
                    if pattern.format == PixelFormat::Rgba {
                        px[at..at + 4].copy_from_slice(&rgba);
                    } else {
                        px[at..at + 4].copy_from_slice(&[rgba[2], rgba[1], rgba[0], rgba[3]]);
                    }
                }
            }
            planes.push(media_types::Plane::new(px, w * 4));
        }
    }
    VideoFrame {
        pts: 0.0,
        width: w,
        height: h,
        format: pattern.format,
        planes,
    }
}

/// The origin the video and audio threads share.
///
/// It is created with the current time but reset once, when the first of the
/// two sources starts: a source opened long before it is started (the writer
/// builds its pipeline in between, which takes seconds at 4K) must not stamp
/// its first frame in the past.
struct Origin {
    at: AtomicU64,
    started: AtomicBool,
}

impl Origin {
    fn new() -> Origin {
        Origin {
            at: AtomicU64::new(media_types::now().to_bits()),
            started: AtomicBool::new(false),
        }
    }

    /// The first start resets the origin; every later call reads it, so the
    /// video flashes and the beeps stay on one timeline.
    fn begin(&self) -> f64 {
        if !self.started.swap(true, Ordering::SeqCst) {
            self.at
                .store(media_types::now().to_bits(), Ordering::SeqCst);
        }
        f64::from_bits(self.at.load(Ordering::SeqCst))
    }

    fn get(&self) -> f64 {
        f64::from_bits(self.at.load(Ordering::SeqCst))
    }
}

/// The synthetic video source.
pub struct Synth {
    pattern: Pattern,
    /// Host time of frame 0 and of beep 0: video and audio share it, so flash
    /// k and beep k are stamped exactly `origin + k` seconds.
    origin: Arc<Origin>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    running: bool,
}

impl Synth {
    /// A pattern source whose origin is now.
    pub fn new(pattern: Pattern) -> Synth {
        Synth {
            pattern,
            origin: Arc::new(Origin::new()),
            stop: Arc::new(AtomicBool::new(false)),
            handle: None,
            running: false,
        }
    }

    /// Host time of the first frame (and of the matching beep).
    #[allow(dead_code)] // the tests and phase 1 sources that align to the pattern
    pub fn origin(&self) -> f64 {
        self.origin.get()
    }

    /// The matching synthetic system audio, on the same origin.
    pub fn audio(&self, system: bool) -> SynthAudio {
        SynthAudio {
            origin: self.origin.clone(),
            enabled: system,
            stop: Arc::new(AtomicBool::new(false)),
            handle: None,
            running: false,
        }
    }
}

impl VideoSource for Synth {
    fn describe(&self) -> String {
        format!(
            "test pattern {}x{} @ {} fps {}{}",
            self.pattern.width,
            self.pattern.height,
            self.pattern.fps,
            self.pattern.format.label(),
            if self.pattern.noise { " (noise)" } else { "" }
        )
    }

    fn start(&mut self, sink: FrameSink) -> Result<()> {
        if self.running {
            bail!("the test pattern is already running");
        }
        if self.pattern.width < 16
            || self.pattern.height < 16
            || !(1..=240).contains(&self.pattern.fps)
        {
            bail!(
                "bad test pattern {}x{} @ {} fps",
                self.pattern.width,
                self.pattern.height,
                self.pattern.fps
            );
        }
        self.running = true;
        let pattern = self.pattern;
        let origin = self.origin.begin();
        let stop = self.stop.clone();
        let period = 1.0 / pattern.fps as f64;
        self.handle = Some(std::thread::spawn(move || {
            let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
            let mut n = 0u64;
            while !stop.load(Ordering::SeqCst) {
                let due = origin + n as f64 * period;
                let now = media_types::now();
                if due > now {
                    std::thread::sleep(Duration::from_secs_f64(due - now));
                }
                let mut frame = draw(&pattern, n, &mut rng);
                frame.pts = due;
                sink(frame);
                n += 1;
            }
        }));
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.running = false;
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.running
    }
}

impl Drop for Synth {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// The synthetic system audio: a beep every second, delivered in real time like
/// a device.
pub struct SynthAudio {
    origin: Arc<Origin>,
    /// False when the recording has system audio off (then nothing is
    /// delivered, so the writer sees no audio at all).
    enabled: bool,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    running: bool,
}

impl AudioSource for SynthAudio {
    fn kind(&self) -> AudioKind {
        AudioKind::System
    }

    fn describe(&self) -> String {
        if self.enabled {
            "test pattern beep".into()
        } else {
            "test pattern (no audio)".into()
        }
    }

    fn start(&mut self, sink: AudioSink) -> Result<()> {
        if self.running {
            bail!("the test pattern audio is already running");
        }
        if !self.enabled {
            return Ok(());
        }
        self.running = true;
        let stop = self.stop.clone();
        let origin = self.origin.begin();
        let rate = crate::mixer::RATE;
        // Self-test hook: drop every Nth buffer like a glitching device.
        let drop_every: i64 = std::env::var("REC_SYNTH_DROP_AUDIO")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        self.handle = Some(std::thread::spawn(move || {
            let mut k = 0i64;
            while !stop.load(Ordering::SeqCst) {
                let pts = origin + (k * AUDIO_CHUNK as i64) as f64 / rate;
                // Deliver each chunk once it has "played", like a real device.
                let ready = pts + AUDIO_CHUNK as f64 / rate;
                let now = media_types::now();
                if ready > now {
                    std::thread::sleep(Duration::from_secs_f64(ready - now));
                }
                let dropped = drop_every > 0 && k % drop_every == drop_every - 1;
                if !dropped {
                    let samples = beep_samples(k * AUDIO_CHUNK as i64, AUDIO_CHUNK);
                    sink(
                        AudioKind::System,
                        AudioChunk::stereo(pts, rate as u32, samples),
                    );
                }
                k += 1;
            }
        }));
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.running = false;
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.running
    }
}

impl Drop for SynthAudio {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beep_starts_on_each_second() {
        let s = beep_samples(48000 - 2, 6);
        assert_eq!(&s[..4], &[0.0; 4], "silent before the second");
        assert_eq!(s[4], 0.0, "sine starts at zero phase");
        assert!(s[6] > 0.0);
        let len = (BEEP_SECS * 48000.0) as i64;
        assert_eq!(beep_samples(len, 1), vec![0.0, 0.0]);
        // Exactly one beep per second, and it is −6 dBFS.
        let s = beep_samples(0, 96_000);
        let loud: Vec<usize> = (0..96_000).filter(|i| s[i * 2].abs() > 0.01).collect();
        assert!(
            loud.iter()
                .all(|i| *i < 2_000 || (48_000..50_000).contains(i)),
            "one beep per second"
        );
        assert!((s.iter().fold(0f32, |m, v| m.max(v.abs())) - BEEP_AMPLITUDE).abs() < 0.01);
    }

    #[test]
    fn pattern_timeline() {
        // Frame 0 of every second is solid white; the rest are not. The pixel
        // sampled is above the moving box (which sits at a third of the height).
        let p = Pattern {
            width: 640,
            height: 360,
            fps: 30,
            ..Default::default()
        };
        let mut rng = Rng(1);
        for (n, white) in [(0u64, true), (29, false), (30, true), (60, true)] {
            let f = draw(&p, n, &mut rng);
            let plane = &f.planes[0];
            let at = (20 * plane.stride) + 320 * 4;
            let px = &plane.data[at..at + 4];
            if white {
                assert_eq!(px, [255, 255, 255, 255], "frame {n} flashes");
            } else {
                assert_ne!(px, [255, 255, 255, 255], "frame {n} does not");
            }
        }
    }

    #[test]
    fn pattern_planes_are_well_formed() {
        for format in [PixelFormat::Bgra, PixelFormat::Rgba, PixelFormat::I420] {
            let p = Pattern {
                width: 50,
                height: 30,
                fps: 30,
                format,
                noise: true,
            };
            let f = draw(&p, 7, &mut Rng(2));
            assert!(f.is_well_formed(), "{format:?}");
            assert_eq!(f.planes.len(), format.planes());
            assert_eq!((f.width, f.height), (50, 30));
        }
        // Noise differs between frames (the encoder sees change).
        let p = Pattern {
            width: 96,
            height: 96,
            format: PixelFormat::Bgra,
            noise: true,
            fps: 30,
        };
        let mut rng = Rng(3);
        let a = draw(&p, 1, &mut rng);
        let b = draw(&p, 2, &mut rng);
        assert_ne!(a.planes[0].data, b.planes[0].data);
    }

    /// The source stamps frames on the exact grid and stops cleanly.
    #[test]
    fn source_emits_on_the_grid() {
        let p = Pattern {
            width: 32,
            height: 18,
            fps: 120,
            ..Default::default()
        };
        let mut s = Synth::new(p);
        assert!(!s.is_running());
        let pts = Arc::new(std::sync::Mutex::new(vec![]));
        let pts2 = pts.clone();
        s.start(Arc::new(move |f: VideoFrame| {
            pts2.lock().unwrap().push(f.pts)
        }))
        .unwrap();
        assert!(s.is_running());
        std::thread::sleep(Duration::from_millis(120));
        s.stop().unwrap();
        assert!(!s.is_running());
        let pts = pts.lock().unwrap().clone();
        assert!(pts.len() >= 4, "{} frames in 120 ms at 120 fps", pts.len());
        let origin = s.origin();
        for (i, t) in pts.iter().enumerate() {
            assert!(
                (t - (origin + i as f64 / 120.0)).abs() < 1e-6,
                "frame {i} at {t}"
            );
        }
    }

    /// A source opened long before it starts must not stamp its first frame in
    /// the past: the writer builds its pipeline between opening and starting
    /// the sources (6.4 s at 4K), and a creation-time origin made the file
    /// begin that far behind.
    #[test]
    fn a_late_start_does_not_stamp_the_first_frame_in_the_past() {
        let mut s = Synth::new(Pattern {
            width: 32,
            height: 18,
            fps: 120,
            ..Default::default()
        });
        let created = s.origin();
        let pts = Arc::new(std::sync::Mutex::new(vec![]));
        let pts2 = pts.clone();
        std::thread::sleep(Duration::from_millis(200));
        let started = media_types::now();
        s.start(Arc::new(move |f: VideoFrame| {
            pts2.lock().unwrap().push(f.pts)
        }))
        .unwrap();
        std::thread::sleep(Duration::from_millis(60));
        s.stop().unwrap();
        let pts = pts.lock().unwrap().clone();
        assert!(!pts.is_empty());
        assert!(
            pts[0] > created + 0.1,
            "first frame {:.4} is at the creation time {:.4}",
            pts[0],
            created
        );
        assert!(
            pts[0] + 0.02 >= started,
            "first frame {:.4} predates the start {:.4}",
            pts[0],
            started
        );
    }

    /// The video start sets the shared origin; an audio start that follows
    /// reads it, so flashes and beeps stay aligned.
    #[test]
    fn video_and_audio_share_the_origin_after_a_late_start() {
        let mut s = Synth::new(Pattern {
            width: 32,
            height: 18,
            fps: 120,
            ..Default::default()
        });
        let mut a = s.audio(true);
        std::thread::sleep(Duration::from_millis(100));
        let video = Arc::new(std::sync::Mutex::new(vec![]));
        let video2 = video.clone();
        s.start(Arc::new(move |f: VideoFrame| {
            video2.lock().unwrap().push(f.pts)
        }))
        .unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let audio = Arc::new(std::sync::Mutex::new(vec![]));
        let audio2 = audio.clone();
        a.start(Arc::new(move |_, chunk: AudioChunk| {
            audio2.lock().unwrap().push(chunk.pts)
        }))
        .unwrap();
        std::thread::sleep(Duration::from_millis(60));
        a.stop().unwrap();
        s.stop().unwrap();
        let video = video.lock().unwrap().clone();
        let audio = audio.lock().unwrap().clone();
        assert!(!video.is_empty() && !audio.is_empty());
        assert!(
            (video[0] - audio[0]).abs() < 0.05,
            "video {} and audio {} start apart",
            video[0],
            audio[0]
        );
    }

    /// The audio source delivers 10 ms chunks on the same timeline, in real
    /// time, and honours `enabled`.
    #[test]
    fn audio_chunks_share_the_origin() {
        let s = Synth::new(Pattern {
            width: 32,
            height: 18,
            fps: 60,
            ..Default::default()
        });
        let mut a = s.audio(true);
        let got = Arc::new(std::sync::Mutex::new(vec![]));
        let got2 = got.clone();
        a.start(Arc::new(move |kind, chunk: AudioChunk| {
            assert_eq!(kind, AudioKind::System);
            got2.lock().unwrap().push((chunk.pts, chunk.frames()));
        }))
        .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        a.stop().unwrap();
        let got = got.lock().unwrap().clone();
        assert!(got.len() >= 5, "{} chunks in 100 ms", got.len());
        assert_eq!(got[0].1, AUDIO_CHUNK);
        for (i, (t, _)) in got.iter().enumerate() {
            let want = s.origin() + i as f64 * AUDIO_CHUNK as f64 / 48_000.0;
            assert!((t - want).abs() < 1e-6, "chunk {i} at {t}, want {want}");
        }
        let mut off = s.audio(false);
        off.start(Arc::new(|_, _| unreachable!("audio is off")))
            .unwrap();
        assert!(!off.is_running());
        off.stop().unwrap();
    }
}
