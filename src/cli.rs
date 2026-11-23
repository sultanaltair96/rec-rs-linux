//! The command line: `rec-rs record` arguments and the helpers that turn them
//! into session [`Options`](crate::record::Options).
//!
//! The surface is upstream's, flag for flag (see the README option table), so
//! scripts written for the macOS build keep working. Where Linux differs, the
//! default changes and the help says so: output goes to `$XDG_VIDEOS_DIR`
//! (`~/Videos`), and `--mic-backend` is `pipewire`.
//!
//! Adapted from rec-rs src/main.rs (MIT OR Apache-2.0).

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Args;

use crate::capture::screen::{self, Region, ScreenRequest};
use crate::compose::{self, Corner, PipSpec, Scene, Shape};
use crate::live;
use crate::media::{AudioLayout, Codec};
use crate::mixer::Volume;
use crate::record::{MicBackend, Options};
use crate::util::{even, timestamped, videos_dir};

/// `rec-rs record` arguments.
#[derive(Args)]
pub struct RecordArgs {
    /// Output file (.mov or .mp4). Default: $XDG_VIDEOS_DIR/rec-<timestamp>.mov
    #[arg(short, long, visible_alias = "out")]
    pub output: Option<PathBuf>,
    /// Display number from `rec-rs devices` (1 = main display).
    #[arg(short, long, default_value_t = 1)]
    pub display: usize,
    /// Record only this region of the display: X,Y,WIDTH,HEIGHT in points
    /// from the display's top-left corner.
    #[arg(long, value_name = "X,Y,W,H", conflicts_with = "window")]
    pub region: Option<String>,
    /// Record a single window: window ID or app/title substring (`rec-rs
    /// devices`).
    #[arg(long, value_name = "WINDOW")]
    pub window: Option<String>,
    /// Output size: 720p, 1080p, 1440p, 4k, native, or WIDTHxHEIGHT. The frame
    /// keeps the display's (or window's) aspect ratio and fits inside this
    /// size (1080p of a 16:10 display is 1728x1080), so there are never black
    /// bars; it is never larger than the source.
    #[arg(short, long, default_value = "1080p")]
    pub resolution: String,
    /// Accepted for compatibility: the frame always keeps the source's aspect
    /// ratio now.
    #[arg(long, hide = true)]
    pub keep_aspect: bool,
    /// Frames per second.
    #[arg(short, long, default_value_t = 60)]
    pub fps: u32,
    /// Video codec. Default: h264 up to 1080p, hevc above.
    #[arg(long, value_enum)]
    pub codec: Option<Codec>,
    /// Video bitrate in Mb/s. Default scales with resolution and fps.
    #[arg(long)]
    pub bitrate: Option<f64>,
    /// Record system audio (the default; see --no-system-audio).
    #[arg(long, overrides_with = "no_system_audio")]
    pub system_audio: bool,
    /// Do not record system audio.
    #[arg(long)]
    pub no_system_audio: bool,
    /// Microphone: `default`, a unique ID or a name substring (default: the
    /// system default mic).
    #[arg(long, value_name = "DEVICE")]
    pub mic: Option<String>,
    /// Display name of the --mic device, used to find it again when its unique
    /// ID changed.
    #[arg(long, hide = true, value_name = "NAME")]
    pub mic_name: Option<String>,
    /// Do not record the microphone.
    #[arg(long, conflicts_with = "mic")]
    pub no_mic: bool,
    /// Overlay a webcam (optionally: unique ID or name substring).
    #[arg(short, long, visible_alias = "cam", value_name = "DEVICE", num_args = 0..=1, default_missing_value = "")]
    pub webcam: Option<String>,
    /// Scene: screen, screen-cam (picture-in-picture) or cam (webcam full
    /// frame). Default: screen-cam with a webcam, else screen. A camera scene
    /// turns the webcam on.
    #[arg(long, value_enum)]
    pub scene: Option<Scene>,
    /// Webcam overlay width as a fraction of the frame width.
    #[arg(long, default_value_t = 0.22)]
    pub pip_size: f64,
    /// Snap the webcam overlay to a corner.
    #[arg(long, value_enum, default_value_t = Corner::BottomRight)]
    pub pip_corner: Corner,
    /// Place the overlay's center anywhere: X,Y as fractions of the frame
    /// from the top-left (e.g. 0.5,0.5). Overrides --pip-corner.
    #[arg(long, value_name = "X,Y")]
    pub pip_pos: Option<String>,
    #[arg(long, value_enum, default_value_t = Shape::Rounded)]
    pub pip_shape: Shape,
    /// Corner roundness 0..1 (rounded, square and squircle shapes).
    #[arg(long, default_value_t = 0.2)]
    pub pip_roundness: f64,
    /// Overlay border width in pixels at 1080p (0 = none).
    #[arg(long, default_value_t = 0.0)]
    pub pip_border: f64,
    /// Border color as hex RGB.
    #[arg(long, default_value = "ffffff", value_name = "RRGGBB")]
    pub pip_border_color: String,
    /// Soft drop shadow under the overlay.
    #[arg(long)]
    pub pip_shadow: bool,
    /// Do not mirror the webcam.
    #[arg(long)]
    pub no_mirror: bool,
    /// Hide the mouse cursor.
    #[arg(long)]
    pub no_cursor: bool,
    /// Highlight mouse clicks.
    #[arg(long)]
    pub clicks: bool,
    /// Stop automatically after this many seconds.
    #[arg(short = 't', long)]
    pub duration: Option<f64>,
    /// Do not print live status.
    #[arg(short, long)]
    pub quiet: bool,
    /// Audio tracks when recording both system audio and mic.
    #[arg(long, value_enum, default_value_t = AudioLayout::Both)]
    pub audio_tracks: AudioLayout,
    /// Shift system audio later by this many ms. Default: the output device's
    /// latency (system audio is captured before it is heard).
    #[arg(long, value_name = "MS", allow_hyphen_values = true)]
    pub system_audio_offset_ms: Option<f64>,
    /// Shift the microphone later by this many ms (e.g. a Bluetooth mic).
    #[arg(
        long,
        value_name = "MS",
        default_value_t = 0.0,
        allow_hyphen_values = true
    )]
    pub mic_offset_ms: f64,
    /// Keep the last SECS seconds (15-300) of encoded video and audio in
    /// memory; `rec-rs save-replay` writes them to an MP4.
    #[arg(long, value_name = "SECS")]
    pub replay: Option<f64>,
    /// Stream to an RTMP(S) URL or a WHIP endpoint (http(s)://.../whip).
    /// youtube, twitch and kick are shortcuts. Repeat it to stream to
    /// several at once. The key comes from $REC_STREAM_KEY or the Secret
    /// Service (`rec-rs stream-key URL`).
    #[arg(long, value_name = "URL")]
    pub stream: Vec<String>,
    /// Do not write a recording file (replay buffer / streaming only).
    #[arg(long)]
    pub no_record: bool,
    /// Live encode (replay and streams) size.
    #[arg(long, default_value = "1080p")]
    pub live_resolution: String,
    /// Live video bitrate in kb/s. Default: the platform's recommendation
    /// for the size and frame rate (YouTube 4K60 45 Mb/s H.264 or 35 HEVC,
    /// Twitch at most 6 Mb/s, Kick at most 8), else YouTube's. With several
    /// destinations, the lowest of them.
    #[arg(long)]
    pub stream_bitrate: Option<u32>,
    /// Live video codec: auto (HEVC when every destination is YouTube), h264
    /// or hevc (enhanced RTMP / WHIP H.265; falls back to H.264 for servers
    /// that refuse it).
    #[arg(long, default_value = "auto", value_parser = ["auto", "h264", "hevc"])]
    pub stream_codec: String,
    /// Live keyframe interval in seconds.
    #[arg(long, default_value_t = 2.0)]
    pub keyframe: f64,
    /// Variable bitrate for the live encode (default: constant).
    #[arg(long)]
    pub vbr: bool,
    /// Live audio bitrate in kb/s (AAC; Opus for WHIP).
    #[arg(long, default_value_t = 192)]
    pub audio_bitrate: u32,
    /// System audio level in dB, in every track and the stream (-60 or lower
    /// is silence, at most +24).
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    pub system_gain: f32,
    /// Microphone level in dB, in every track and the stream (-60 or lower is
    /// silence, at most +24).
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    pub mic_gain: f32,
    /// Microphone capture backend: pipewire (default) or pulse. The macOS
    /// values sck and avcapture map to pipewire with a message.
    #[arg(long, default_value = "pipewire", value_name = "BACKEND")]
    pub mic_backend: String,
    /// Record a generated test pattern instead of the screen (self-test; needs
    /// no capture permission). The frame flashes white once a second and the
    /// "system audio" beeps at the same instant, for sync checks.
    #[arg(long)]
    pub test_pattern: bool,
    /// With --test-pattern: add random noise blocks to stress the encoder.
    #[arg(long, requires = "test_pattern")]
    pub noise: bool,
}

/// Output size for `s` (a preset or WxH, the bounding box) and a source of
/// `native` pixels: the source's aspect ratio fitted inside the box, never
/// larger than the source, even dimensions. A frame with the source's shape
/// fills edge to edge: no letterbox or pillarbox bars, nothing cropped.
pub fn parse_size(s: &str, native: (usize, usize)) -> Result<(usize, usize)> {
    let lower = s.to_lowercase();
    // The capture's own size is the native size rounded down to even.
    let (nw, nh) = ((native.0 & !1).max(2) as f64, (native.1 & !1).max(2) as f64);
    let (bw, bh) = match lower.as_str() {
        "720p" => (1280.0, 720.0),
        "1080p" => (1920.0, 1080.0),
        "1440p" => (2560.0, 1440.0),
        "4k" | "2160p" | "uhd" => (3840.0, 2160.0),
        "native" => (nw, nh),
        _ => {
            let Some((w, h)) = lower.split_once('x') else {
                bail!("bad --resolution {s:?}: use 720p/1080p/1440p/4k/native/WxH")
            };
            (
                w.trim().parse::<usize>()? as f64,
                h.trim().parse::<usize>()? as f64,
            )
        }
    };
    // Presets name the frame's height for landscape sources and its width for
    // portrait ones (a 1080p portrait window is 1080 wide).
    let (bw, bh) = if nh > nw
        && matches!(
            lower.as_str(),
            "720p" | "1080p" | "1440p" | "4k" | "2160p" | "uhd"
        ) {
        (bh, bw)
    } else {
        (bw, bh)
    };
    let k = (bw / nw).min(bh / nh).min(1.0);
    Ok((even(nw * k), even(nh * k)))
}

/// The live encode's bounding box for a preset or WxH (the encoder fits the
/// frame inside it, keeping its aspect ratio).
pub fn live_box(s: &str) -> Result<(usize, usize)> {
    Ok(match s.to_lowercase().as_str() {
        "720p" => (1280, 720),
        "1080p" => (1920, 1080),
        "1440p" => (2560, 1440),
        "4k" | "2160p" | "uhd" => (3840, 2160),
        other => {
            let Some((w, h)) = other.split_once('x') else {
                bail!("bad --live-resolution {s:?}: use 720p/1080p/1440p/4k/WxH")
            };
            (
                even(w.trim().parse::<f64>()?),
                even(h.trim().parse::<f64>()?),
            )
        }
    })
}

/// `--pip-pos X,Y`: fractions of the frame from its top-left corner.
pub fn parse_pos(s: &str) -> Result<[f64; 2]> {
    let v: Vec<f64> = s
        .split(',')
        .map(|x| x.trim().parse::<f64>())
        .collect::<Result<_, _>>()?;
    match v[..] {
        [x, y] if (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y) => Ok([x, y]),
        _ => bail!("--pip-pos must be X,Y with both between 0 and 1"),
    }
}

/// `--pip-border-color RRGGBB` (a leading `#` is allowed).
pub fn parse_hex(s: &str) -> Result<[u8; 3]> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 {
        bail!("color must be RRGGBB hex");
    }
    let c = |i: usize| u8::from_str_radix(&s[i..i + 2], 16);
    Ok([c(0)?, c(2)?, c(4)?])
}

/// `--region X,Y,W,H`, in points.
fn parse_region(s: &str) -> Result<Region> {
    let v: Vec<f64> = s
        .split(',')
        .map(|x| x.trim().parse::<f64>())
        .collect::<Result<_, _>>()?;
    if v.len() != 4 || v[2] <= 0.0 || v[3] <= 0.0 {
        bail!("--region must be X,Y,WIDTH,HEIGHT (points)");
    }
    Ok(Region {
        x: v[0],
        y: v[1],
        width: v[2],
        height: v[3],
    })
}

/// The default output: `$XDG_VIDEOS_DIR` (fallback `~/Videos`) with a
/// timestamped name, `.mov` like upstream.
pub fn default_output() -> PathBuf {
    videos_dir().join(timestamped("rec", "mov"))
}

/// Turn record arguments into session options (shared by the CLI and the app).
pub fn prepare(a: RecordArgs) -> Result<Options> {
    if !(1..=240).contains(&a.fps) || 60000 % a.fps != 0 {
        bail!("--fps must divide 60000 (e.g. 24, 25, 30, 50, 60, 120)");
    }
    if let Some(d) = a.duration
        && d <= 0.0
    {
        bail!("--duration must be positive");
    }
    let mic_backend = MicBackend::parse(&a.mic_backend)?;
    // A missing mic falls back to the default; audio never blocks a session.
    // `--mic default` means the system default mic, which warns instead of
    // failing when it is unavailable; a named device is an explicit request.
    let mic_explicit = a.mic.as_deref().is_some_and(|m| m != "default") || a.mic_name.is_some();
    let mic = if a.no_mic {
        None
    } else {
        Some(
            a.mic
                .as_deref()
                .filter(|m| *m != "default")
                .map(str::to_string),
        )
    };
    let mut webcam = a.webcam.map(|s| {
        if s.is_empty() || s == "default" {
            None
        } else {
            Some(s)
        }
    });
    let scene = a.scene.unwrap_or(if webcam.is_some() {
        Scene::ScreenCam
    } else {
        Scene::Screen
    });
    if scene.uses_cam() && webcam.is_none() {
        webcam = Some(None);
    }
    let output = a.output.clone().unwrap_or_else(default_output);
    // Validate every flag before resolving the screen (upstream's order): on a
    // headless or portal-less host a bad argument must still be the error the
    // user sees, not a capture failure.
    if a.no_record && a.replay.is_none() && a.stream.is_empty() {
        bail!("--no-record needs --replay or --stream");
    }
    if let Some(secs) = a.replay
        && !(15.0..=300.0).contains(&secs)
    {
        bail!("--replay must be 15-300 seconds");
    }
    if a.codec == Some(Codec::Prores) && output.extension().is_some_and(|e| e == "mp4") {
        bail!("ProRes needs a .mov output");
    }
    let (request, native) = if a.test_pattern {
        (None, (3840, 2160))
    } else {
        let request = ScreenRequest {
            display: a.display,
            window: a.window.clone(),
            region: a.region.as_deref().map(parse_region).transpose()?,
        };
        // Resolution needs the display's (or window's) native pixel size, which
        // only real capture knows (phase 1A).
        let (_, native) = screen::resolve(&request)?;
        (Some(request), native)
    };
    let (w, h) = parse_size(&a.resolution, native)?;
    let codec = a.codec.unwrap_or(if w * h > 1920 * 1080 {
        Codec::Hevc
    } else {
        Codec::H264
    });
    // Bits per pixel per frame tuned for screen content (sharp text).
    let bpp = match codec {
        Codec::H264 => 0.16,
        Codec::Hevc => 0.10,
        Codec::Prores => 0.0, // fixed quality; bitrate unused
    };
    let bitrate = a
        .bitrate
        .map(|m| (m * 1e6) as u64)
        .unwrap_or_else(|| ((w * h) as f64 * a.fps as f64 * bpp) as u64)
        .clamp(1_000_000, 400_000_000);
    if codec.needs_mov() && output.extension().is_some_and(|e| e == "mp4") {
        bail!("ProRes needs a .mov output");
    }
    let mut live = None;
    if a.replay.is_some() || !a.stream.is_empty() {
        let conf = live::Live::new();
        let (lw, lh) = live_box(&a.live_resolution)?;
        let platforms: Vec<live::Platform> = a
            .stream
            .iter()
            .map(|d| live::platform(&live::destination_url(d)))
            .collect();
        let (preset_kbps, max_h, hevc) =
            live::shared_preset(&platforms, lh.min(h), a.fps.min(60), &a.stream_codec);
        let kbps = a.stream_bitrate.unwrap_or(preset_kbps);
        let (lw, lh) = if lh.min(h) > max_h {
            (max_h * 16 / 9, max_h)
        } else {
            (lw.min(w), lh.min(h))
        };
        conf.set_config(live::LiveConfig {
            width: lw,
            height: lh,
            bitrate: kbps.clamp(500, 80_000) as u64 * 1000,
            keyframe_secs: a.keyframe.clamp(0.5, 4.0),
            cbr: !a.vbr,
            audio_bitrate: a.audio_bitrate.clamp(64, 320) * 1000,
            hevc,
        });
        if !a.quiet {
            eprintln!(
                "rec: live encode {lw}x{lh} {} {:.1} Mb/s, keyframe every {} s",
                if hevc { "HEVC" } else { "H.264" },
                kbps as f64 / 1000.0,
                a.keyframe
            );
        }
        if let Some(secs) = a.replay {
            conf.set_replay(Some(secs));
        }
        live = Some(std::sync::Arc::new(conf));
    }
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let opts = Options {
        output,
        size: (w, h),
        native: (native.0 & !1, native.1 & !1),
        fps: a.fps,
        codec,
        bitrate,
        system_audio: !a.no_system_audio,
        mic,
        mic_explicit,
        mic_name: a.mic_name.clone(),
        mic_warning: None,
        webcam,
        pip: PipSpec {
            size: a.pip_size.clamp(0.05, 0.9),
            place: match &a.pip_pos {
                Some(p) => compose::Place::At(parse_pos(p)?),
                None => compose::Place::Corner(a.pip_corner),
            },
            shape: a.pip_shape,
            roundness: a.pip_roundness.clamp(0.0, 1.0),
            border: a.pip_border.clamp(0.0, 64.0),
            border_color: parse_hex(&a.pip_border_color)?,
            shadow: a.pip_shadow,
            mirror: !a.no_mirror,
        },
        cursor: !a.no_cursor,
        clicks: a.clicks,
        duration: a.duration,
        quiet: a.quiet,
        test_pattern: a.test_pattern.then_some(a.noise),
        audio_layout: a.audio_tracks,
        volumes: [a.system_gain, a.mic_gain].map(|db| Volume { db, muted: false }),
        mic_backend,
        screen: request,
        scene,
        system_offset_ms: a.system_audio_offset_ms,
        mic_offset_ms: a.mic_offset_ms,
        write: !a.no_record,
        preview: None,
        gpu: None,
        preview_gpu: None,
        live,
        streams: a
            .stream
            .iter()
            .map(|d| live::resolve_destination(d))
            .collect::<Result<Vec<_>>>()?,
    };
    Ok(opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ported from rec-rs src/main.rs `goal_command_line_parses` (MIT OR
    /// Apache-2.0): upstream's `--mic` requires a device, and `default` means
    /// the system default microphone.
    #[test]
    fn the_mic_device_is_required() {
        // `--mic` alone is a clap error, as it is upstream (no `num_args`).
        assert!(crate::parse_record_args(&["--test-pattern", "--mic"]).is_err());

        let a = crate::parse_record_args(&["--test-pattern", "--mic", "default"]).unwrap();
        assert_eq!(a.mic.as_deref(), Some("default"));
        let o = prepare(a).unwrap();
        assert_eq!(o.mic, Some(None));
        assert!(!o.mic_explicit);

        let a = crate::parse_record_args(&["--test-pattern", "--mic", "Rec-Test"]).unwrap();
        let o = prepare(a).unwrap();
        assert_eq!(o.mic, Some(Some("Rec-Test".to_string())));
        assert!(o.mic_explicit, "a named device fails when it is missing");

        // `--no-mic` still conflicts with any `--mic`, value or not.
        assert!(crate::parse_record_args(&["--test-pattern", "--no-mic", "--mic"]).is_err());
    }

    /// G16: bad arguments are reported before the screen is resolved, so a
    /// headless box says what is wrong instead of "the portal is not
    /// reachable" (upstream validates every flag first too).
    #[test]
    fn argument_errors_come_before_the_capture_target() {
        let a = crate::parse_record_args(&["--no-record"]).unwrap();
        let err = prepare(a).unwrap_err().to_string();
        assert!(
            err.contains("--no-record needs --replay or --stream"),
            "{err}"
        );

        let a = crate::parse_record_args(&["--codec", "prores", "-o", "x.mp4"]).unwrap();
        let err = prepare(a).unwrap_err().to_string();
        assert!(err.contains("ProRes needs a .mov output"), "{err}");
    }
}
