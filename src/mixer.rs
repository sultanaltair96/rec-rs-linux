//! Real-time audio mixer: system audio + microphone -> one stereo track.
//!
//! Each source's chunks are placed on a common sample timeline derived from
//! their host-clock timestamps, so the mix stays in sync with video. Per-source
//! continuity snapping avoids clicks from sub-millisecond timestamp jitter.
//! Sources whose rate is not 48 kHz go through a streaming windowed-sinc
//! resampler, and the mix bus runs through a limiter that keeps it under
//! −1 dBFS instead of clipping.
//!
//! This is the portable half of upstream's mixer: it works on plain
//! `[f32; 2]` frames and [`AudioChunk`]s instead of CoreMedia sample buffers,
//! so it can be unit-tested without a device and reused by the live encode.
//!
//! Adapted from rec-rs src/mixer.rs (MIT OR Apache-2.0).

// The faders, volumes and resampler are used by the writer; the mix bus itself
// is wired into the writer's mix track by phase 1C, and its tests run here.
#![allow(dead_code)]

use std::collections::VecDeque;

use crate::media_types::{AudioChunk, AudioKind};

/// The mix bus sample rate. Every track in the file is 48 kHz, like upstream.
pub const RATE: f64 = 48_000.0;
/// Emit even if a source is silent/stalled for longer than this.
const MAX_WAIT: i64 = 24000; // 0.5 s
/// Treat timestamp jumps up to this many samples as continuous.
const SNAP: i64 = 96; // 2 ms
/// Samples the mix bus may hold for one source. Four times the 0.5 s emission
/// lag, so real jitter is never dropped, while a source whose clock jumps
/// (a device bug, wall-clock timestamps) cannot make the buffer grow without
/// bound. A chunk beyond it is dropped and counted in [`Mixer::overruns`]; the
/// source resyncs on its next sane buffer.
const MAX_BUFFER: i64 = 2 * RATE as i64;

/// Index of a source in the per-source arrays.
fn idx(k: AudioKind) -> usize {
    k.index()
}

/// Stereo frames of a chunk: mono is duplicated to both channels, anything
/// wider than stereo keeps its first two channels (a recorder writes stereo).
pub fn stereo_frames(chunk: &AudioChunk) -> Vec<[f32; 2]> {
    let ch = chunk.channels.max(1) as usize;
    if ch == 1 {
        return chunk.samples.iter().map(|s| [*s, *s]).collect();
    }
    chunk
        .samples
        .chunks_exact(ch)
        .map(|c| [c[0], c[1]])
        .collect()
}

/// An [`AudioChunk`] from stereo frames.
pub fn chunk_from_frames(pts: f64, frames: &[[f32; 2]]) -> AudioChunk {
    let mut samples = Vec::with_capacity(frames.len() * 2);
    for f in frames {
        samples.extend_from_slice(f);
    }
    AudioChunk::stereo(pts, RATE as u32, samples)
}

pub struct Mixer {
    /// Session start in samples on the host clock.
    t0: i64,
    base: i64,
    buf: VecDeque<[f32; 2]>,
    /// Newest sample index (relative to `t0`) each source has reached.
    horizon: [Option<i64>; 2],
    active: [bool; 2],
    /// Samples that would have clipped (the limiter held them back).
    pub clipped: u64,
    /// Chunks dropped because their timestamp was more than `MAX_BUFFER`
    /// ahead of the mix (a clock jump, not a real gap).
    pub overruns: u64,
    resamplers: [Option<Resampler>; 2],
    limiter: Limiter,
}

impl Mixer {
    /// A mix bus whose timeline starts at host time `t0_secs`, with the sources
    /// that will feed it.
    pub fn new(t0_secs: f64, system: bool, mic: bool) -> Self {
        Mixer {
            t0: (t0_secs * RATE).round() as i64,
            base: 0,
            buf: VecDeque::new(),
            horizon: [None, None],
            active: [system, mic],
            clipped: 0,
            overruns: 0,
            resamplers: [None, None],
            limiter: Limiter { gain: 1.0 },
        }
    }

    /// Add interleaved chunk audio, resampling to 48 kHz if needed.
    pub fn push_chunk(&mut self, kind: AudioKind, chunk: &AudioChunk) {
        self.push(
            kind,
            chunk.pts,
            chunk.rate.max(1) as f64,
            stereo_frames(chunk),
        );
    }

    /// Add decoded stereo frames that start at `pts` (seconds on the output
    /// timeline, i.e. already shifted for sync offsets and pauses).
    pub fn push(&mut self, kind: AudioKind, pts: f64, rate: f64, frames: Vec<[f32; 2]>) {
        let i = idx(kind);
        let (pts, frames) = if (rate - RATE).abs() < 0.5 || rate <= 0.0 {
            self.resamplers[i] = None;
            (pts, frames)
        } else {
            self.resamplers[i]
                .get_or_insert_with(|| Resampler::new(rate, pts))
                .push(pts, rate, frames)
        };
        if frames.is_empty() {
            return;
        }
        let mut start = (pts * RATE).round() as i64 - self.t0;
        if let Some(h) = self.horizon[i]
            && (start - h).abs() <= SNAP
        {
            start = h;
        }
        let end = start + frames.len() as i64;
        let need = (end - self.base).max(0);
        if need > MAX_BUFFER {
            // A timestamp far ahead of the mix (a device clock jump, a source
            // that switched to wall-clock time) would make the buffer grow
            // without bound. Drop the chunk; memory stays flat and the source
            // resyncs on its next sane buffer.
            self.overruns += 1;
            return;
        }
        self.horizon[i] = Some(self.horizon[i].map_or(end, |h| h.max(end)));
        let need = need as usize;
        if self.buf.len() < need {
            self.buf.resize(need, [0.0, 0.0]);
        }
        for (n, fr) in frames.iter().enumerate() {
            let pos = start + n as i64 - self.base;
            if pos < 0 {
                continue; // already emitted
            }
            let slot = &mut self.buf[pos as usize];
            slot[0] += fr[0];
            slot[1] += fr[1];
        }
    }

    /// Mixed frames ready with their start time in host seconds.
    ///
    /// Without `flush`, this is everything all active sources have reached;
    /// with it, everything any source has reached (the end of the session).
    pub fn pull_frames(&mut self, flush: bool) -> Option<(f64, Vec<[f32; 2]>)> {
        let hs: Vec<i64> = (0..2)
            .filter(|&i| self.active[i])
            .map(|i| self.horizon[i].unwrap_or(0))
            .collect();
        let max_h = hs.iter().copied().max().unwrap_or(0);
        let until = if flush {
            max_h
        } else {
            hs.iter().copied().min().unwrap_or(0).max(max_h - MAX_WAIT)
        };
        let n = (until - self.base).min(self.buf.len() as i64);
        if n <= 0 {
            return None;
        }
        let t = (self.t0 + self.base) as f64 / RATE;
        self.base += n;
        let mut frames: Vec<[f32; 2]> = self.buf.drain(..n as usize).collect();
        for fr in &mut frames {
            self.clipped += fr.iter().filter(|s| s.abs() > 1.0).count() as u64;
            *fr = self.limiter.process(*fr);
        }
        Some((t, frames))
    }

    /// The same as [`pull_frames`](Mixer::pull_frames), as an [`AudioChunk`].
    pub fn pull_chunk(&mut self, flush: bool) -> Option<AudioChunk> {
        let (t, frames) = self.pull_frames(flush)?;
        Some(chunk_from_frames(t, &frames))
    }
}

/// Fader travel in dB. The bottom is silence (shown as -inf), the top a
/// boost for quiet sources.
pub const MIN_DB: f32 = -60.0;
pub const MAX_DB: f32 = 24.0;

/// A source's level as set on its fader: gain in dB, and mute.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Volume {
    pub db: f32,
    pub muted: bool,
}

impl Volume {
    pub const UNITY: Volume = Volume {
        db: 0.0,
        muted: false,
    };

    /// Linear gain of the fader alone: silence at or below MIN_DB.
    pub fn fader_gain(self) -> f32 {
        if self.db > MIN_DB {
            10f32.powf(self.db.min(MAX_DB) / 20.0)
        } else {
            0.0
        }
    }

    /// Linear gain applied to the source's audio.
    pub fn gain(self) -> f32 {
        if self.muted { 0.0 } else { self.fader_gain() }
    }
}

/// How long a gain change takes to complete.
const RAMP_SECS: f32 = 0.02;

/// Applies a source's gain. A new gain is reached over [`RAMP_SECS`] instead
/// of at once, so moving a fader or muting never clicks.
pub struct Fader {
    gain: f32,
    target: f32,
    /// Size of the change in progress, which sets its pace for every buffer it
    /// spans.
    span: f32,
}

impl Fader {
    pub fn new(gain: f32) -> Self {
        Fader {
            gain,
            target: gain,
            span: 0.0,
        }
    }

    /// The gain currently in effect.
    pub fn current(&self) -> f32 {
        self.gain
    }

    pub fn set(&mut self, gain: f32) {
        if gain != self.target {
            self.target = gain;
            self.span = (gain - self.gain).abs();
        }
    }

    /// The gain for each of the next `n` frames at `rate`, or None while it is
    /// unity (the audio passes through untouched).
    pub fn envelope(&mut self, n: usize, rate: f64) -> Option<Vec<f32>> {
        if self.gain == 1.0 && self.target == 1.0 {
            return None;
        }
        // A floor so that a tiny change still lands.
        let step = (self.span / (RAMP_SECS * rate.max(1.0) as f32)).max(1e-5);
        Some(
            (0..n)
                .map(|_| {
                    let d = self.target - self.gain;
                    self.gain = if d.abs() <= step {
                        self.target
                    } else {
                        self.gain + step.copysign(d)
                    };
                    self.gain
                })
                .collect(),
        )
    }
}

/// Taps on each side of a resampled output sample.
const HALF_TAPS: usize = 24;

/// Streaming sinc resampler to 48 kHz for one source (e.g. a 44.1 kHz USB mic
/// or a 16/24 kHz Bluetooth headset). Kaiser-windowed sinc, low-passed below
/// the lower Nyquist rate, with its filter history and fractional position
/// carried across buffers, so buffer boundaries are seamless.
///
/// The mix bus and each written audio track use one, so a device at another
/// rate is converted once per destination and never drifts.
pub struct Resampler {
    from: f64,
    /// Input samples not yet fully used, starting at absolute input index
    /// `first`.
    hist: Vec<[f32; 2]>,
    first: i64,
    /// Absolute input position of the next output sample.
    pos: f64,
    /// Host time (seconds) of input sample 0.
    t0: f64,
    /// Absolute input index the next buffer is expected to start at.
    next_in: i64,
}

/// Zeroth-order modified Bessel function of the first kind, for the Kaiser
/// window (a 32-term series is plenty for beta ≤ 8).
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0, 1.0);
    for k in 1..32 {
        term *= (x / (2.0 * k as f64)).powi(2);
        sum += term;
    }
    sum
}

impl Resampler {
    /// A resampler reading `from` Hz, whose input sample 0 is at host time
    /// `t0` seconds.
    pub fn new(from: f64, t0: f64) -> Self {
        Resampler {
            from,
            hist: vec![],
            first: 0,
            pos: 0.0,
            t0,
            next_in: 0,
        }
    }

    /// Resample a buffer starting at host time `pts`. Returns the output frames
    /// and the host time of the first one. A buffer that does not continue the
    /// stream (gap, overlap, rate change) restarts it.
    pub fn push(&mut self, pts: f64, rate: f64, src: Vec<[f32; 2]>) -> (f64, Vec<[f32; 2]>) {
        let expected = self.t0 + self.next_in as f64 / self.from;
        if rate != self.from || (pts - expected).abs() > SNAP as f64 / RATE {
            *self = Resampler::new(rate, pts);
        }
        self.next_in += src.len() as i64;
        self.hist.extend(src);
        let step = self.from / RATE;
        // Cutoff (cycles per input sample): below both Nyquist rates.
        let fc = 0.5 * (RATE / self.from).min(1.0) * 0.94;
        let beta = 8.0;
        let norm = bessel_i0(beta);
        let mut out = vec![];
        let t_out = self.t0 + self.pos / self.from;
        let h = HALF_TAPS as f64 / (2.0 * fc).min(1.0);
        let reach = h.ceil() as i64;
        // Taps before the stream's start read as silence.
        while (self.pos.floor() as i64) + reach < self.first + self.hist.len() as i64 {
            let c = self.pos.floor() as i64;
            let (mut l, mut r, mut wsum) = (0.0f64, 0.0f64, 0.0f64);
            for j in (c - reach + 1)..=(c + reach) {
                let x = j as f64 - self.pos;
                if x.abs() >= h {
                    continue;
                }
                let arg = 2.0 * fc * x;
                let sinc = if arg.abs() < 1e-12 {
                    1.0
                } else {
                    (std::f64::consts::PI * arg).sin() / (std::f64::consts::PI * arg)
                };
                let w = bessel_i0(beta * (1.0 - (x / h).powi(2)).max(0.0).sqrt()) / norm;
                let k = 2.0 * fc * sinc * w;
                wsum += k;
                if j >= self.first {
                    let s = self.hist[(j - self.first) as usize];
                    l += k * s[0] as f64;
                    r += k * s[1] as f64;
                }
            }
            // Unity gain at DC whatever the fractional phase.
            let g = if wsum > 0.0 { 1.0 / wsum } else { 0.0 };
            out.push([(l * g) as f32, (r * g) as f32]);
            self.pos += step;
        }
        let keep_from = (self.pos.floor() as i64 - reach).max(self.first);
        let drop = (keep_from - self.first) as usize;
        self.hist.drain(..drop.min(self.hist.len()));
        self.first += drop as i64;
        (t_out, out)
    }
}

/// Mix bus limiter: keeps summed sources under the ceiling with smooth gain
/// changes (fast attack, slow release) and a soft knee for the rare peak the
/// envelope misses, instead of clipping them flat.
struct Limiter {
    gain: f32,
}

/// -1 dBFS: headroom for the AAC encoder, whose output overshoots its input
/// peaks slightly.
const CEILING: f32 = 0.891;
const ATTACK: f32 = 0.35; // per sample, ~0.06 ms
const RELEASE: f32 = 1.0 / (0.15 * RATE as f32); // full recovery in ~150 ms

impl Limiter {
    fn process(&mut self, fr: [f32; 2]) -> [f32; 2] {
        let peak = fr[0].abs().max(fr[1].abs());
        let want = if peak > CEILING { CEILING / peak } else { 1.0 };
        self.gain = if want < self.gain {
            self.gain + (want - self.gain) * ATTACK
        } else {
            (self.gain + RELEASE).min(want)
        };
        fr.map(|s| soft_clip(s * self.gain))
    }
}

/// Identity below the knee, then a smooth curve that never passes 1.0.
fn soft_clip(s: f32) -> f32 {
    const KNEE: f32 = 0.9;
    let a = s.abs();
    if a <= KNEE {
        return s;
    }
    let room = 1.0 - KNEE;
    (KNEE + room * ((a - KNEE) / room).tanh()).copysign(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pull_all(m: &mut Mixer, flush: bool) -> (Option<f64>, Vec<[f32; 2]>) {
        let mut first = None;
        let mut out = vec![];
        while let Some(b) = m.pull_frames(flush) {
            let (pts, frames) = b;
            first.get_or_insert(pts);
            out.extend(frames);
        }
        (first, out)
    }

    #[test]
    fn sums_sources_on_a_shared_timeline() {
        let mut m = Mixer::new(10.0, true, true);
        m.push(AudioKind::System, 10.0, RATE, vec![[0.25, 0.25]; 480]);
        // Nothing until the mic reaches the same point (it may still arrive).
        assert!(m.pull_frames(false).is_none());
        // Mic starts 5 ms later, mono duplicated to both channels.
        m.push(AudioKind::Mic, 10.005, RATE, vec![[0.5, 0.5]; 480]);
        let (pts, out) = pull_all(&mut m, false);
        assert_eq!(pts, Some(10.0));
        assert_eq!(out.len(), 480, "only up to the slower source's horizon");
        assert_eq!(out[0], [0.25, 0.25]);
        assert_eq!(out[240], [0.75, 0.75], "overlap is summed");
        let (pts, rest) = pull_all(&mut m, true);
        assert_eq!(pts, Some(10.01));
        assert_eq!(rest.len(), 240);
        assert_eq!(rest[0], [0.5, 0.5]);
    }

    #[test]
    fn mono_chunks_are_duplicated_and_rates_are_resampled() {
        let mut m = Mixer::new(0.0, false, true);
        // 44.1 kHz mono in, 48 kHz stereo out: same duration, both channels.
        let mono = AudioChunk {
            pts: 0.0,
            rate: 44_100,
            channels: 1,
            samples: vec![0.3; 44_100],
        };
        m.push_chunk(AudioKind::Mic, &mono);
        let (pts, out) = pull_all(&mut m, true);
        assert_eq!(pts, Some(0.0));
        assert!((out.len() as i64 - 48_000).abs() <= 100, "{}", out.len());
        // Away from the filter's start and end ramp, a constant stays constant.
        assert!(
            out[2400..out.len() - 2400]
                .iter()
                .all(|f| (f[0] - f[1]).abs() < 1e-9 && (f[0] - 0.3).abs() < 0.01)
        );
    }

    #[test]
    fn limiting() {
        // Under the ceiling: untouched.
        let mut m = Mixer::new(0.0, false, true);
        m.push(AudioKind::Mic, 0.0, RATE, vec![[0.4, -0.6]; 100]);
        let (_, out) = pull_all(&mut m, true);
        assert_eq!(out[50], [0.4, -0.6]);
        assert_eq!(m.clipped, 0);
        // Too loud (a source boosted on its fader): held under full scale (both
        // channels by the same gain, so the stereo image keeps), never
        // flattened at 1.0.
        let mut m = Mixer::new(0.0, false, true);
        m.push(AudioKind::Mic, 0.0, RATE, vec![[0.8, -1.2]; 4800]);
        let (_, out) = pull_all(&mut m, true);
        assert!(out.iter().all(|f| f[0].abs() < 1.0 && f[1].abs() < 1.0));
        let last = out[4799];
        assert!((last[1].abs() - CEILING).abs() < 0.01, "{last:?}");
        assert!((last[0] / last[1] + 0.8 / 1.2).abs() < 0.01);
        assert_eq!(m.clipped, 4800);
    }

    #[test]
    fn volume_gain() {
        let v = |db, muted| Volume { db, muted };
        assert_eq!(Volume::UNITY.gain(), 1.0);
        assert!((v(-6.0, false).gain() - 0.501).abs() < 0.001);
        assert!((v(12.0, false).gain() - 3.981).abs() < 0.001);
        assert_eq!(
            v(MAX_DB + 20.0, false).gain(),
            v(MAX_DB, false).gain(),
            "boost is capped"
        );
        assert_eq!(
            v(MIN_DB, false).gain(),
            0.0,
            "the bottom of the fader is silence"
        );
        assert_eq!(v(f32::NEG_INFINITY, false).gain(), 0.0);
        assert_eq!(v(f32::NAN, false).gain(), 0.0);
        assert_eq!(v(0.0, true).gain(), 0.0);
        assert_eq!(
            v(-6.0, true).fader_gain(),
            v(-6.0, false).gain(),
            "the meter still sees a muted source"
        );
    }

    #[test]
    fn fader_glides_and_passes_unity_through() {
        let mut f = Fader::new(1.0);
        assert!(f.envelope(480, RATE).is_none(), "unity: untouched");
        // Mute: down to silence within 20 ms, never a jump.
        f.set(0.0);
        let e = f.envelope(1920, RATE).unwrap();
        assert!(
            e.windows(2).all(|w| w[1] <= w[0] && w[0] - w[1] < 0.0011),
            "smooth"
        );
        assert!(e[0] > 0.99 && e[950] > 0.0);
        assert!(e[961..].iter().all(|g| *g == 0.0));
        // A boost glides as fast, whatever its size, and settles exactly.
        let boost = Volume {
            db: 24.0,
            muted: false,
        }
        .gain();
        f.set(boost);
        let e = f.envelope(1920, RATE).unwrap();
        assert!(e[950] < boost && e[961..].iter().all(|g| *g == boost));
        // Muting a boosted source in device-sized buffers still takes 20 ms.
        let mut g = vec![];
        f.set(0.0);
        for _ in 0..4 {
            g.extend(f.envelope(480, RATE).unwrap());
        }
        assert!(
            g[950] > 0.0 && g[961..].iter().all(|g| *g == 0.0),
            "{} {}",
            g[959],
            g[961]
        );
        f.set(boost);
        f.envelope(960, RATE);
        // Back to unity: once there, the audio passes through again.
        f.set(1.0);
        assert_eq!(*f.envelope(960, 16000.0).unwrap().last().unwrap(), 1.0);
        assert!(f.envelope(480, RATE).is_none());
    }

    /// A sine through the resampler in uneven buffers: the right length and
    /// pitch, seamless across buffers (error against the ideal 48 kHz sine).
    fn resampled_sine_error(from: f64, freq: f64, chunk: usize) -> (usize, f64) {
        let mut m = Mixer::new(0.0, false, true);
        let n = from as usize; // 1 s
        let sine = |t: f64| (2.0 * std::f64::consts::PI * freq * t).sin() as f32 * 0.5;
        let mut i = 0;
        while i < n {
            let len = chunk + (i / chunk) % 7; // uneven buffer sizes
            let end = (i + len).min(n);
            let frames: Vec<[f32; 2]> = (i..end)
                .map(|k| {
                    let v = sine(k as f64 / from);
                    [v, v]
                })
                .collect();
            m.push(AudioKind::Mic, i as f64 / from, from, frames);
            i = end;
        }
        let (pts, out) = pull_all(&mut m, true);
        let t0 = pts.unwrap();
        // Compare away from the stream's start and end (filter ramp).
        let err = (2400..out.len() - 2400)
            .map(|k| (out[k][0] - sine(t0 + k as f64 / RATE)).abs() as f64)
            .fold(0.0, f64::max);
        (out.len(), err)
    }

    #[test]
    fn resamples_seamlessly() {
        for (from, chunk) in [
            (44100.0, 441),
            (16000.0, 160),
            (24000.0, 512),
            (96000.0, 1024),
        ] {
            let (len, err) = resampled_sine_error(from, 1000.0, chunk);
            // The last ~1.6 ms wait in the filter for input that never came.
            assert!((len as i64 - 48000).abs() <= 100, "{from}: {len} samples");
            assert!(err < 0.002, "{from}: max error {err}");
        }
    }

    #[test]
    fn resampler_removes_content_above_nyquist() {
        // 20 kHz at 96 kHz cannot pass to 48 kHz unaliased, but is under
        // 24 kHz; 30 kHz must be filtered out rather than fold to 18 kHz.
        let mut m = Mixer::new(0.0, false, true);
        let frames: Vec<[f32; 2]> = (0..96000)
            .map(|k| {
                let v =
                    (2.0 * std::f64::consts::PI * 30000.0 * k as f64 / 96000.0).sin() as f32 * 0.5;
                [v, v]
            })
            .collect();
        m.push(AudioKind::Mic, 0.0, 96000.0, frames);
        let (_, out) = pull_all(&mut m, true);
        let peak = out[2400..out.len() - 2400]
            .iter()
            .fold(0f32, |a, f| a.max(f[0].abs()));
        assert!(peak < 0.005, "aliased residue {peak}");
    }

    #[test]
    fn small_jitter_snaps_to_continuous() {
        let mut m = Mixer::new(0.0, true, false);
        m.push(AudioKind::System, 0.0, RATE, vec![[0.1, 0.1]; 480]);
        // 1 ms late: within SNAP, so no gap of silence is inserted.
        m.push(AudioKind::System, 0.011, RATE, vec![[0.1, 0.1]; 480]);
        let (_, out) = pull_all(&mut m, true);
        assert_eq!(out.len(), 960);
        assert!(out.iter().all(|f| f[0] == 0.1));
    }

    /// A fader applied to a chunk in the source's own layout (phase 0 only
    /// handles f32 chunks; device layouts are converted once at the source).
    #[test]
    fn scales_in_the_chunks_own_layout() {
        let env = [0.5, 0.25, 2.0, 0.0];
        // Stereo: one gain per frame, both channels.
        let c = AudioChunk::stereo(
            7.0,
            48_000,
            vec![0.2, -0.4, 0.2, -0.4, 0.6, -0.6, 0.2, -0.4],
        );
        let out = c.scaled(&env);
        assert_eq!(
            (out.pts, out.rate, out.frames()),
            (7.0, 48_000, 4),
            "same timing"
        );
        assert_eq!(
            stereo_frames(&out),
            vec![[0.1, -0.2], [0.05, -0.1], [1.0, -1.0], [0.0, 0.0]],
            "saturates instead of wrapping"
        );
        // Mono: still one gain per frame (the third sample is 0.6, times 2.0).
        let mono = AudioChunk { channels: 1, ..c };
        let mono = mono.scaled(&env);
        assert_eq!((mono.frames(), mono.samples[2]), (8, 0.4f32));
    }
    #[test]
    fn a_far_future_chunk_cannot_grow_the_mix_buffer() {
        // A source whose clock jumps (wall-clock timestamps, a device bug) must
        // not make the mix allocate the samples of the jump. The chunk is
        // dropped and counted; a sane buffer afterwards still mixes.
        let mut m = Mixer::new(0.0, true, true);
        m.push(AudioKind::System, 0.0, RATE, vec![[0.1, 0.1]; 480]);
        m.push(AudioKind::Mic, 1000.0, RATE, vec![[0.1, 0.1]; 480]);
        assert_eq!(m.overruns, 1, "the far-future chunk is dropped");
        assert!(
            m.buf.len() <= MAX_BUFFER as usize,
            "mix buffer {} samples",
            m.buf.len()
        );
        // The mic's next sane buffer still lands on the shared timeline.
        m.push(AudioKind::Mic, 0.0, RATE, vec![[0.2, 0.2]; 480]);
        let (_, frames) = m.pull_frames(true).expect("mixed audio");
        assert!((frames[100][0] - 0.3).abs() < 1e-6, "{}", frames[100][0]);
    }

    #[test]
    fn the_mix_buffer_stays_bounded_when_one_source_never_delivers() {
        // Only the system source feeds the bus for ten minutes of audio; the
        // mic stays silent. The bus emits MAX_WAIT behind the newest source and
        // never buffers more than that.
        let mut m = Mixer::new(0.0, true, true);
        let mut pulled = 0usize;
        for k in 0..(600 * 100) {
            let t = k as f64 * 0.01;
            m.push(AudioKind::System, t, RATE, vec![[0.05, 0.05]; 480]);
            while let Some((_, frames)) = m.pull_frames(false) {
                pulled += frames.len();
            }
        }
        assert!(pulled > 0);
        assert!(
            m.buf.len() <= MAX_BUFFER as usize,
            "mix buffer {} samples",
            m.buf.len()
        );
        assert_eq!(m.overruns, 0, "a silent source is not an overrun");
    }

}
