//! Synthetic sources for the writer's self-tests: a second beep for the
//! microphone and a camera.
//!
//! The test pattern in [`crate::synth`] is the "screen" and the "system
//! audio". These two stand in for the devices a headless container does not
//! have, so the writer's track layout, the camera gate, the emission lag and
//! the scene wiring can be recorded and checked end to end:
//!
//! - [`mic`] beeps once a second at 1.5 kHz (the pattern's beep is 1 kHz), so a
//!   mic stem is distinguishable from the system stem and the mix.
//! - [`camera`] delivers solid red frames, so a file recorded with `--scene cam`
//!   is visibly the camera's and not the pattern's.
//!
//! They are only reached through `REC_SYNTH_MIC` and `REC_SYNTH_CAM`, which no
//! normal recording sets.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Result, bail};

use crate::media_types::{
    self, AudioChunk, AudioKind, AudioSink, AudioSource, FrameSink, PixelFormat, VideoFrame,
    VideoSource,
};

/// The synthetic microphone's beep frequency: the pattern's system beep is
/// 1000 Hz, and the two must not be confused in a mix.
pub const MIC_HZ: f64 = 1500.0;
/// The beep length and amplitude, matching the pattern's (−6 dBFS).
pub const BEEP_SECS: f64 = 0.04;
pub const BEEP_AMPLITUDE: f32 = 0.5;
/// Frames per audio chunk: 10 ms at 48 kHz, like a device.
pub const AUDIO_CHUNK: usize = 480;

/// Interleaved stereo samples `[start, start+n)` of a beep once a second.
fn beep_samples(start: i64, n: usize, hz: f64) -> Vec<f32> {
    let rate = crate::mixer::RATE as i64;
    let len = (BEEP_SECS * rate as f64) as i64;
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n as i64 {
        let s = start + i;
        let in_sec = s.rem_euclid(rate);
        let v = if in_sec < len {
            (BEEP_AMPLITUDE as f64
                * (2.0 * std::f64::consts::PI * hz * in_sec as f64 / rate as f64).sin())
                as f32
        } else {
            0.0
        };
        out.push(v);
        out.push(v);
    }
    out
}

/// A synthetic microphone: the pattern's beep at a different pitch.
pub struct MicBeep {
    origin: f64,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    running: bool,
}

impl MicBeep {
    pub fn new() -> Self {
        MicBeep {
            origin: media_types::now(),
            stop: Arc::new(AtomicBool::new(false)),
            handle: None,
            running: false,
        }
    }
}

impl Default for MicBeep {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioSource for MicBeep {
    fn kind(&self) -> AudioKind {
        AudioKind::Mic
    }

    fn describe(&self) -> String {
        format!("synthetic mic beep ({MIC_HZ:.0} Hz)")
    }

    fn start(&mut self, sink: AudioSink) -> Result<()> {
        if self.running {
            bail!("the synthetic mic is already running");
        }
        self.running = true;
        let stop = self.stop.clone();
        let origin = self.origin;
        let rate = crate::mixer::RATE;
        self.handle = Some(std::thread::spawn(move || {
            let mut k = 0i64;
            while !stop.load(Ordering::SeqCst) {
                let pts = origin + (k * AUDIO_CHUNK as i64) as f64 / rate;
                // Deliver each chunk once it has "played", like a device.
                let ready = pts + AUDIO_CHUNK as f64 / rate;
                let now = media_types::now();
                if ready > now {
                    std::thread::sleep(Duration::from_secs_f64(ready - now));
                }
                let samples = beep_samples(k * AUDIO_CHUNK as i64, AUDIO_CHUNK, MIC_HZ);
                sink(
                    AudioKind::Mic,
                    AudioChunk::stereo(pts, rate as u32, samples),
                );
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

impl Drop for MicBeep {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// A synthetic webcam: solid red BGRA frames at a fixed size and rate.
///
/// `REC_SYNTH_CAM_LATENCY_MS` delays each frame after it was captured (its pts
/// stays the capture time), which lets a test make emission wait for the
/// slowest source.
pub struct TestCamera {
    width: usize,
    height: usize,
    fps: u32,
    latency: f64,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    running: bool,
}

impl TestCamera {
    pub fn new(width: usize, height: usize, fps: u32) -> Self {
        let latency = std::env::var("REC_SYNTH_CAM_LATENCY_MS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
            / 1000.0;
        TestCamera {
            width,
            height,
            fps,
            latency,
            stop: Arc::new(AtomicBool::new(false)),
            handle: None,
            running: false,
        }
    }

    fn frame(pts: f64, width: usize, height: usize, n: u64) -> VideoFrame {
        // Solid red with a white bar whose position moves, so consecutive
        // frames differ (a scene switch is visible in a diff).
        let mut data = vec![0u8; width * height * 4];
        let bar = ((n * 8) % width.max(1) as u64) as usize;
        for y in 0..height {
            for x in 0..width {
                let at = (y * width + x) * 4;
                let on_bar = x >= bar && x < bar + 16;
                data[at] = if on_bar { 240 } else { 20 }; // B
                data[at + 1] = if on_bar { 240 } else { 20 }; // G
                data[at + 2] = 220; // R: red everywhere
                data[at + 3] = 255; // A
            }
        }
        VideoFrame::packed(pts, width, height, PixelFormat::Bgra, data)
    }
}

impl VideoSource for TestCamera {
    fn describe(&self) -> String {
        format!(
            "synthetic camera ({}x{} @ {})",
            self.width, self.height, self.fps
        )
    }

    fn start(&mut self, sink: FrameSink) -> Result<()> {
        if self.running {
            bail!("the synthetic camera is already running");
        }
        self.running = true;
        let stop = self.stop.clone();
        let (w, h, fps, latency) = (self.width, self.height, self.fps, self.latency);
        self.handle = Some(std::thread::spawn(move || {
            let period = 1.0 / fps as f64;
            let mut n = 0u64;
            let start = media_types::now() + period;
            while !stop.load(Ordering::SeqCst) {
                let pts = start + n as f64 * period;
                let deliver = pts + latency;
                let now = media_types::now();
                if deliver > now {
                    std::thread::sleep(Duration::from_secs_f64(deliver - now));
                }
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                sink(TestCamera::frame(pts, w, h, n));
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

impl Drop for TestCamera {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// The synthetic microphone, when `REC_SYNTH_MIC` is set (tests only).
pub fn mic_from_env() -> Option<Box<dyn AudioSource>> {
    std::env::var_os("REC_SYNTH_MIC").map(|_| Box::new(MicBeep::new()) as Box<dyn AudioSource>)
}

/// The synthetic camera, when `REC_SYNTH_CAM` is set (tests only).
/// `REC_SYNTH_CAM_SIZE=WxH` changes its size, `REC_SYNTH_CAM_FPS` its rate.
pub fn camera_from_env() -> Option<Box<dyn VideoSource>> {
    std::env::var_os("REC_SYNTH_CAM")?;
    let size = std::env::var("REC_SYNTH_CAM_SIZE")
        .ok()
        .and_then(|v| {
            let (w, h) = v.split_once('x')?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        })
        .unwrap_or((640, 360));
    let fps = std::env::var("REC_SYNTH_CAM_FPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    Some(Box::new(TestCamera::new(size.0, size.1, fps)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mic_beep_is_at_a_different_pitch_from_the_pattern() {
        // One second of the synthetic mic: exactly one beep, at 1500 Hz, and
        // the pattern's helper at 1000 Hz differs from it.
        let s = beep_samples(0, 48_000, MIC_HZ);
        let peak = s.iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!((peak - BEEP_AMPLITUDE).abs() < 0.01, "{peak}");
        let zeros = s
            .chunks(2)
            .map(|f| f[0])
            .collect::<Vec<f32>>()
            .windows(2)
            .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
            .count();
        // 40 ms of a 1500 Hz sine: about 120 zero crossings.
        assert!((115..=125).contains(&zeros), "{zeros}");
        let pattern = crate::synth::beep_samples(0, 48_000);
        let pattern_zeros = pattern
            .chunks(2)
            .map(|f| f[0])
            .collect::<Vec<f32>>()
            .windows(2)
            .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
            .count();
        assert!(pattern_zeros < zeros, "{pattern_zeros} vs {zeros}");
    }

    #[test]
    fn the_test_camera_frames_are_red_and_well_formed() {
        let f = TestCamera::frame(1.0, 64, 36, 3);
        assert!(f.is_well_formed());
        assert_eq!(f.width, 64);
        let p = &f.planes[0];
        // Red dominates every pixel; the moving bar is white.
        for px in p.data.chunks(4).take(64 * 36).step_by(97) {
            assert!(px[2] > 200, "R={} B={}", px[2], px[0]);
            assert!(px[3] == 255);
        }
    }
}
