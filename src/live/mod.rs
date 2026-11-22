//! Live outputs: one shared H.264 + AAC/Opus encode of the composited output,
//! fanned out to the replay buffer and any streams (RTMP, WHIP) — phase 1D.
//!
//! The capture session feeds every output frame and every audio buffer in (on
//! the host clock, independent of recording pauses). Nothing is encoded while
//! no live output is active. The implementation (GStreamer encode to
//! appsink, replay buffer, the built-in RTMP client with rustls, str0m WHIP,
//! Secret Service stream keys) is phase 1D and lands in this directory.
//!
//! What is here already is the *option* surface the CLI, the settings file and
//! the app share, with its tests: destination URLs, platform recognition and
//! the shared encode's bitrate/size presets. Those are pure functions, so the
//! `--stream`, `--live-resolution` and settings tests work before any output
//! exists.
//!
//! Adapted from rec-rs src/live/mod.rs (MIT OR Apache-2.0).

// The option surface is used by `cli.rs` and `settings.rs`; the output methods
// are the phase 1D seam.
#![allow(dead_code)]

use anyhow::{Result, bail};

/// The phase that implements the outputs.
pub const PHASE: &str = "1D";

/// The services with a built-in ingest URL, in the order the app lists them.
pub const SERVICES: [(&str, &str, &str); 3] = [
    (
        "youtube",
        "YouTube",
        "rtmps://a.rtmps.youtube.com:443/live2",
    ),
    ("twitch", "Twitch", "rtmps://live.twitch.tv:443/app"),
    (
        "kick",
        "Kick",
        "rtmps://fa723fc1b171.global-contribute.live-video.net:443/app",
    ),
];

/// Settings of the shared live encode.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveConfig {
    /// Encode size (frames of another size are scaled).
    pub width: usize,
    pub height: usize,
    /// Video bits per second.
    pub bitrate: u64,
    pub keyframe_secs: f64,
    pub cbr: bool,
    /// AAC bits per second (Opus for WHIP uses the same).
    pub audio_bitrate: u32,
    /// Prefer HEVC (used when every stream accepts it, else H.264).
    pub hevc: bool,
}

impl Default for LiveConfig {
    fn default() -> Self {
        LiveConfig {
            width: 1920,
            height: 1080,
            bitrate: 6_000_000,
            keyframe_secs: 2.0,
            cbr: true,
            audio_bitrate: 160_000,
            hevc: false,
        }
    }
}

/// The live encode and its outputs. Phase 1D replaces the stub: the type stays,
/// so `record::Options` and the app keep one shape.
#[derive(Default)]
pub struct Live {
    config: std::sync::Mutex<Option<LiveConfig>>,
    replay: std::sync::Mutex<Option<f64>>,
}

impl Live {
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure the shared encode.
    pub fn set_config(&self, config: LiveConfig) {
        *self.config.lock().unwrap() = Some(config);
    }

    /// The current configuration, if one was set.
    pub fn config(&self) -> Option<LiveConfig> {
        self.config.lock().unwrap().clone()
    }

    /// Keep the last `secs` seconds in memory.
    pub fn set_replay(&self, secs: Option<f64>) {
        *self.replay.lock().unwrap() = secs;
    }

    /// The replay window, if one was set.
    pub fn replay_secs(&self) -> Option<f64> {
        *self.replay.lock().unwrap()
    }

    /// True while the encode is running.
    pub fn active(&self) -> bool {
        false
    }

    /// Start the encode for a session with these audio sources (phase 1D).
    pub fn begin_session(&self, system: bool, mic: bool) -> Result<()> {
        let _ = (system, mic);
        bail!("the live encode is not implemented yet (phase {PHASE})")
    }

    /// Write the replay buffer to `path` (phase 1D).
    pub fn save_replay(&self, path: std::path::PathBuf) -> Result<()> {
        let _ = path;
        bail!("the replay buffer is not implemented yet (phase {PHASE})")
    }
}

/// Stream URL for a destination: a service id (`youtube`, `twitch`, `kick`)
/// expands to its RTMPS ingest URL; anything else is used as given.
pub fn destination_url(dest: &str) -> String {
    let dest = dest.trim();
    SERVICES
        .iter()
        .find(|(id, ..)| dest.eq_ignore_ascii_case(id))
        .map_or_else(|| dest.to_string(), |(.., url)| url.to_string())
}

/// What to call a destination in the UI: its service's name, else the server's
/// host.
pub fn destination_name(url: &str) -> String {
    match SERVICES.iter().find(|(.., u)| *u == url) {
        Some((_, name, _)) => name.to_string(),
        None => host(url),
    }
}

/// A streaming platform, recognized from the ingest URL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    YouTube,
    Twitch,
    Kick,
    Other,
}

fn host(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split(['/', ':'])
        .next()
        .unwrap_or("")
        .to_lowercase()
}

pub fn platform(url: &str) -> Platform {
    let h = host(url);
    if h.ends_with("youtube.com") {
        Platform::YouTube
    } else if h == host(&destination_url("kick")) {
        // Before Twitch: both ingest on *.contribute.live-video.net.
        Platform::Kick
    } else if h.ends_with("twitch.tv") || h.ends_with("contribute.live-video.net") {
        Platform::Twitch
    } else {
        Platform::Other
    }
}

/// Encode limits and the recommended video bitrate (kb/s) for a platform at an
/// encode height and frame rate:
/// - YouTube (its live encoder settings): H.264 2160p60 45 Mb/s, 2160p30 35,
///   1440p 24/16, 1080p 12/10, 720p 7.5/5; HEVC (enhanced RTMP) about a quarter
///   less (2160p 35/25 Mb/s).
/// - Twitch: H.264 only, at most 1080p60 and 6 Mb/s (its ingest limits).
/// - Kick: H.264 only, at most 1080p60 and 8 Mb/s (its encoder requirements).
pub fn preset(p: Platform, height: usize, fps: u32, hevc: bool) -> (u32, usize, bool) {
    // [H.264 60 fps, H.264 30 fps, HEVC 60 fps, HEVC 30 fps]
    let row = match height {
        h if h > 1440 => [45_000, 35_000, 35_000, 25_000],
        h if h > 1080 => [24_000, 16_000, 18_000, 12_000],
        h if h > 720 => [12_000, 10_000, 9_000, 7_500],
        _ => [7_500, 5_000, 5_500, 3_500],
    };
    let i = usize::from(fps <= 30);
    match p {
        Platform::Twitch => (row[i].min(6_000), height.min(1080), false),
        Platform::Kick => (row[i].min(8_000), height.min(1080), false),
        _ if hevc => (row[2 + i], height, true),
        _ => (row[i], height, false),
    }
}

/// The one encode every destination accepts, as (kb/s, height, HEVC): the
/// lowest bitrate and height of their presets, and HEVC only if all of them
/// take it. `codec` is "auto" (HEVC when every destination is YouTube), "h264"
/// or "hevc". With no destinations (replay buffer only) it is the generic
/// preset.
pub fn shared_preset(
    platforms: &[Platform],
    height: usize,
    fps: u32,
    codec: &str,
) -> (u32, usize, bool) {
    let hevc = match codec {
        "hevc" => true,
        "h264" => false,
        _ => !platforms.is_empty() && platforms.iter().all(|p| *p == Platform::YouTube),
    };
    platforms
        .iter()
        .map(|p| preset(*p, height, fps, hevc))
        .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1), a.2 && b.2))
        .unwrap_or_else(|| preset(Platform::Other, height, fps, hevc))
}

/// (URL, key) for a destination: the key comes from `$REC_STREAM_KEY`, else the
/// Secret Service (phase 1D). WHIP endpoints may have no key.
pub fn resolve_destination(dest: &str) -> Result<(String, String)> {
    let url = destination_url(dest);
    if !(url.starts_with("rtmp://")
        || url.starts_with("rtmps://")
        || url.starts_with("http://")
        || url.starts_with("https://"))
    {
        bail!("unsupported stream URL {url:?} (use rtmp://, rtmps:// or a WHIP http(s):// URL)");
    }
    let key = std::env::var("REC_STREAM_KEY")
        .ok()
        .filter(|k| !k.is_empty());
    match key {
        Some(k) => Ok((url, k)),
        None if url.starts_with("http") => Ok((url, String::new())),
        None => bail!(
            "no stream key for {url}: set REC_STREAM_KEY (stream keys in the Secret Service are phase {PHASE})"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_presets() {
        assert_eq!(
            platform("rtmps://a.rtmps.youtube.com:443/live2"),
            Platform::YouTube
        );
        assert_eq!(platform("rtmps://live.twitch.tv:443/app"), Platform::Twitch);
        assert_eq!(platform("rtmp://localhost/live"), Platform::Other);
        // YouTube 4K: 30-50 Mb/s at 60 fps, 20-40 at 30, HEVC lower.
        assert_eq!(
            preset(Platform::YouTube, 2160, 60, false),
            (45_000, 2160, false)
        );
        assert_eq!(
            preset(Platform::YouTube, 2160, 30, true),
            (25_000, 2160, true)
        );
        assert_eq!(preset(Platform::YouTube, 1080, 60, false).0, 12_000);
        // Twitch: H.264, at most 1080p and 6 Mb/s.
        assert_eq!(
            preset(Platform::Twitch, 2160, 60, true),
            (6_000, 1080, false)
        );
        // Kick: H.264, at most 1080p and 8 Mb/s.
        assert_eq!(preset(Platform::Kick, 2160, 60, true), (8_000, 1080, false));
    }

    #[test]
    fn services_expand_and_are_recognized() {
        for (id, name, url) in SERVICES {
            assert_eq!(destination_url(id), url);
            assert_eq!(destination_url(&format!(" {} ", id.to_uppercase())), url);
            assert_eq!(destination_name(url), name);
        }
        assert_eq!(platform(&destination_url("kick")), Platform::Kick);
        // Twitch's own ingests on the same IVS domain stay Twitch.
        assert_eq!(
            platform("rtmps://ingest.global-contribute.live-video.net/app"),
            Platform::Twitch
        );
        assert_eq!(
            destination_url("rtmp://127.0.0.1/live"),
            "rtmp://127.0.0.1/live"
        );
        assert_eq!(destination_name("rtmp://127.0.0.1:1935/live"), "127.0.0.1");
    }

    /// One encode feeds every destination, so it has to fit all of them.
    #[test]
    fn shared_preset_fits_every_destination() {
        use Platform::*;
        // Replay buffer only: YouTube's numbers, H.264.
        assert_eq!(shared_preset(&[], 1080, 60, "auto"), (12_000, 1080, false));
        // YouTube alone gets HEVC on auto, H.264 when asked.
        assert_eq!(
            shared_preset(&[YouTube], 2160, 60, "auto"),
            (35_000, 2160, true)
        );
        assert_eq!(
            shared_preset(&[YouTube], 2160, 60, "h264"),
            (45_000, 2160, false)
        );
        // With Twitch and Kick: 1080p H.264 at Twitch's 6 Mb/s.
        assert_eq!(
            shared_preset(&[YouTube, Twitch, Kick], 2160, 60, "auto"),
            (6_000, 1080, false)
        );
        assert_eq!(
            shared_preset(&[YouTube, Kick], 1440, 60, "hevc"),
            (8_000, 1080, false)
        );
        // A custom server with YouTube: auto stays H.264.
        assert_eq!(
            shared_preset(&[YouTube, Other], 1080, 30, "auto"),
            (10_000, 1080, false)
        );
    }
}
