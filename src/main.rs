//! `rec-rs` — a screen, camera and audio recorder for Linux, ported from
//! rec-rs (macOS).
//!
//! The command line is upstream's: `record` (the recorder), `gui` (the app),
//! `devices`, `probe`, `permissions`, and the controls for a running recording
//! (`stop`, `pause`, `resume`, `save-replay`), plus `stream-key`. Phase 0
//! ships the recorder for the test pattern and the file writer; the app, the
//! capture sources, the GPU compositor and the live path are stubs that say
//! which phase implements them.

mod app;
mod capture;
mod cli;
mod compose;
mod gpu;
mod hotkey;
mod live;
mod media;
mod media_types;
mod mixer;
mod record;
mod settings;
mod synth;
mod util;

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use cli::RecordArgs;

#[derive(Parser)]
#[command(
    name = "rec-rs",
    version,
    about = "Screen, camera and audio recorder for Linux (a port of rec-rs)"
)]
struct Cli {
    /// Without a subcommand, rec starts the app (the menu bar item and window).
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

// RecordArgs is large but parsed once; boxing it buys nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Cmd {
    /// Record the screen (plus system audio, mic, and optional webcam overlay).
    Record(RecordArgs),
    /// The app: window with live preview and sources, settings, tray icon and
    /// global hotkeys (phase 1E).
    Gui {
        /// Start recording immediately (e.g. from a Login Item or script).
        #[arg(long)]
        start_recording: bool,
        /// Open the settings window at launch.
        #[arg(long)]
        settings: bool,
        /// Capture this window (ID from `rec-rs devices`) instead of the
        /// display.
        #[arg(long, value_name = "ID")]
        window: Option<u32>,
        /// Record a generated test pattern instead of the screen (self-test;
        /// needs no capture permission).
        #[arg(long, hide = true)]
        test_pattern: bool,
    },
    /// List displays, windows, cameras and microphones.
    Devices,
    /// Stop a running recording (CLI or app) and print its file.
    Stop,
    /// Save the replay buffer of the running recorder (CLI or app) to MP4.
    SaveReplay,
    /// Store a stream key for a destination (reads the key from stdin; never
    /// echoed or logged).
    StreamKey {
        /// Stream URL (rtmp://, rtmps://, https://.../whip) or a service:
        /// youtube, twitch, kick.
        dest: String,
        /// Remove the stored key instead.
        #[arg(long)]
        delete: bool,
    },
    /// Pause the running recording (resume with `rec-rs resume`).
    Pause,
    /// Resume a paused recording.
    Resume,
    /// Show (and optionally request) screen, microphone and camera
    /// permissions.
    Permissions {
        /// Trigger the system permission prompts for anything not yet granted.
        #[arg(long)]
        request: bool,
        /// Seconds to wait for prompt answers when requesting.
        #[arg(long, default_value_t = 60)]
        wait: u64,
    },
    /// Request screen capture permission (run by `permissions --request`).
    #[command(hide = true)]
    RequestScreen,
    /// Check a recording with GStreamer: decode every sample.
    Probe { file: PathBuf },
    /// Play a file in a reference player window (macOS only; not ported).
    #[command(hide = true)]
    Play {
        file: PathBuf,
        #[arg(long, default_value_t = 15.0)]
        secs: f64,
        #[arg(long, default_value = "640x360")]
        size: String,
    },
}

/// Parse a `rec-rs record ...` argument list (used by the app and the settings
/// tests).
pub fn parse_record_args(args: &[&str]) -> Result<RecordArgs> {
    let mut v = vec!["rec-rs", "record"];
    v.extend_from_slice(args);
    match Cli::try_parse_from(v)?.cmd {
        Some(Cmd::Record(a)) => Ok(a),
        _ => bail!("internal: not a record command"),
    }
}

fn record(a: RecordArgs) -> Result<()> {
    let opts = cli::prepare(a)?;
    let out = opts.output.clone();
    let quiet = opts.quiet;
    let wrote = opts.write;
    let stats = record::run(opts)?;
    if !wrote {
        if !quiet {
            eprintln!(
                "rec: stopped ({} video frames, {} dropped)",
                stats.video_frames_out, stats.video_drops
            );
        }
        return Ok(());
    }
    if !quiet {
        eprintln!(
            "rec: saved {} ({} video frames, {} duplicated, {} dropped; audio buffers sys {} mic {}, {} dropped, {} gaps filled, {} clipped samples)",
            out.display(),
            stats.video_frames_out,
            stats.video_dups,
            stats.video_drops,
            stats.audio_sys_buffers,
            stats.audio_mic_buffers,
            stats.audio_drops,
            stats.audio_gaps_filled,
            stats.audio_clipped_samples
        );
    }
    println!("{}", out.display());
    Ok(())
}

fn devices() -> Result<()> {
    println!("Displays:");
    match capture::screen::displays() {
        Ok(ds) => {
            for d in &ds {
                println!(
                    "  [{}] id {}  {}x{} pixels ({}x{} points){}",
                    d.index,
                    d.id,
                    d.pixels.0,
                    d.pixels.1,
                    d.points.0,
                    d.points.1,
                    if d.primary { "  primary" } else { "" }
                );
            }
        }
        Err(e) => println!("  unavailable: {e}"),
    }
    println!("Windows (for --window):");
    match capture::screen::windows() {
        Ok(ws) => {
            for w in ws {
                println!(
                    "  {:>6}  {} — {}  ({}x{} pt)",
                    w.id, w.app, w.title, w.size.0, w.size.1
                );
            }
        }
        Err(e) => println!("  unavailable: {e}"),
    }
    println!("Cameras:");
    match capture::camera::cameras() {
        Ok(cs) => {
            for c in cs {
                println!("  {}  [{}]", c.name, c.id);
            }
        }
        Err(e) => println!("  unavailable: {e}"),
    }
    println!("Microphones:");
    match capture::audio::microphones() {
        Ok(ms) => {
            for m in ms {
                println!("  {}  [{}]", m.name, m.id);
            }
        }
        Err(e) => println!("  unavailable: {e}"),
    }
    match capture::audio::output_latency() {
        Some(l) => println!(
            "Output latency (system audio is shifted by this): {:.1} ms",
            l * 1000.0
        ),
        None => println!("Output latency: unavailable (not implemented yet, phase 1B)"),
    }
    Ok(())
}

fn permissions(request: bool, wait: u64) -> Result<()> {
    // The screen line is phase 1A's (the ScreenCast portal and its restore
    // token); microphone and camera report their phases until 1B fills them in.
    match capture::screen::permission_status(request, wait) {
        Ok(s) => println!("screen recording : {s}"),
        Err(e) => println!("screen recording : unavailable - {e:#}"),
    }
    println!(
        "microphone       : unknown (not implemented yet, phase {})",
        capture::audio::PHASE
    );
    println!(
        "camera           : unknown (not implemented yet, phase {})",
        capture::camera::PHASE
    );
    Ok(())
}

fn main() {
    // Behave like a normal CLI when piped into `head` etc.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    // GStreamer is used by the writer and the probe; initialise it once.
    if let Err(e) = gstreamer::init() {
        eprintln!("rec: error: GStreamer could not be initialised: {e}");
        std::process::exit(1);
    }
    let cli = Cli::parse();
    let r = dispatch(cli.cmd.unwrap_or(Cmd::Gui {
        start_recording: false,
        settings: false,
        window: None,
        test_pattern: false,
    }));
    if let Err(e) = r {
        eprintln!("rec: error: {e:#}");
        std::process::exit(1);
    }
}

/// Run one command, so `?` and `bail!` work (main only reports).
fn dispatch(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Gui {
            start_recording,
            settings,
            window,
            test_pattern,
        } => app::run(start_recording, settings, window, test_pattern),
        Cmd::Record(a) => record(a),
        Cmd::Devices => devices(),
        Cmd::Stop => record::stop_running(),
        Cmd::Pause => record::signal_running(libc::SIGUSR1),
        Cmd::Resume => record::signal_running(libc::SIGUSR2),
        Cmd::SaveReplay => match record::signal_running(record::save_replay_signal()) {
            Ok(()) => Err(anyhow::anyhow!(
                "the replay buffer is not implemented yet (phase {})",
                live::PHASE
            )),
            Err(e) => Err(e),
        },
        Cmd::StreamKey { dest, delete } => {
            let url = live::destination_url(&dest);
            if delete {
                bail!(
                    "stream keys in the Secret Service are not implemented yet (phase {}); \
                     set REC_STREAM_KEY for now",
                    live::PHASE
                )
            }
            bail!(
                "stream keys in the Secret Service are not implemented yet (phase {}); \
                 set REC_STREAM_KEY={url}... in the environment for now",
                live::PHASE
            )
        }
        Cmd::Permissions { request, wait } => permissions(request, wait),
        // macOS re-execs itself with responsibility disclaimed; on Linux the
        // request is a plain portal dialog, so this is the same thing
        // `permissions --request` does.
        Cmd::RequestScreen => capture::screen::request_permission(60),
        Cmd::Probe { file } => media::probe::probe(&file),
        Cmd::Play { file, secs, size } => {
            let _ = (file, secs, size);
            Err(anyhow::anyhow!(
                "the reference player is macOS-only and is not ported; use `ffplay FILE` or `mpv FILE`"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_command_line_parses() {
        let a = parse_record_args(&[
            "--display",
            "1",
            "--mic",
            "default",
            "--system-audio",
            "--cam",
            "--out",
            "file.mp4",
            "--duration",
            "10",
        ])
        .unwrap();
        assert_eq!(a.display, 1);
        assert_eq!(a.mic.as_deref(), Some("default"));
        assert!(a.system_audio && !a.no_system_audio);
        assert_eq!(a.webcam.as_deref(), Some(""));
        assert_eq!(a.output, Some(PathBuf::from("file.mp4")));
        assert_eq!(a.duration, Some(10.0));
    }

    #[test]
    fn system_audio_flags_override_each_other() {
        let a = parse_record_args(&["--system-audio", "--no-system-audio"]).unwrap();
        assert!(a.no_system_audio);
        let a = parse_record_args(&["--no-system-audio", "--system-audio"]).unwrap();
        assert!(!a.no_system_audio);
    }

    #[test]
    fn sizes() {
        // Every size keeps the source's aspect ratio (no bars), even sizes.
        let native = (3456, 2234);
        assert_eq!(cli::parse_size("1080p", native).unwrap(), (1670, 1080));
        assert_eq!(cli::parse_size("4K", native).unwrap(), (3342, 2160));
        assert_eq!(cli::parse_size("native", native).unwrap(), native);
        assert_eq!(cli::parse_size("1280x720", native).unwrap(), (1114, 720));
        assert_eq!(
            cli::parse_size("1080p", (2880, 1800)).unwrap(),
            (1728, 1080)
        );
        assert_eq!(
            cli::parse_size("1080p", (3840, 2160)).unwrap(),
            (1920, 1080)
        );
        // Never upscaled; odd sources round down to even, like the capture.
        assert_eq!(cli::parse_size("4k", (1201, 801)).unwrap(), (1200, 800));
        // Portrait: the preset names the width.
        assert_eq!(
            cli::parse_size("1080p", (2160, 3840)).unwrap(),
            (1080, 1920)
        );
        for (sz, n) in [
            ("1080p", (3456, 2234)),
            ("720p", (4112, 2658)),
            ("4k", (5120, 2880)),
            ("1440p", (3144, 1588)),
        ] {
            let (w, h) = cli::parse_size(sz, n).unwrap();
            assert!(w % 2 == 0 && h % 2 == 0);
            assert!(
                compose::same_aspect(n.0 as f64, n.1 as f64, w as f64, h as f64),
                "{sz} {n:?} -> {w}x{h}"
            );
        }
        assert!(cli::parse_size("huge", native).is_err());
        // The live encode's box: a preset or WxH, rounded to even.
        assert_eq!(cli::live_box("1080p").unwrap(), (1920, 1080));
        assert_eq!(cli::live_box("1280x721").unwrap(), (1280, 722));
        assert!(cli::live_box("big").is_err());
    }

    #[test]
    fn overlay_options() {
        let a = parse_record_args(&[
            "--cam",
            "--pip-pos",
            "0.25,0.75",
            "--pip-shape",
            "squircle",
            "--pip-roundness",
            "0.6",
            "--pip-border",
            "8",
            "--pip-border-color",
            "#ff8800",
            "--pip-shadow",
        ])
        .unwrap();
        assert_eq!(
            cli::parse_pos(a.pip_pos.as_deref().unwrap()).unwrap(),
            [0.25, 0.75]
        );
        assert_eq!(a.pip_shape, compose::Shape::Squircle);
        assert_eq!(cli::parse_hex(&a.pip_border_color).unwrap(), [255, 136, 0]);
        assert!(a.pip_shadow);
        assert!(cli::parse_pos("1.5,0").is_err());
        assert!(cli::parse_hex("fff").is_err());
        for s in ["circle", "rounded", "square", "pill", "squircle", "rect"] {
            assert!(parse_record_args(&["--pip-shape", s]).is_ok(), "{s}");
        }
    }

    #[test]
    fn scene_option() {
        let a = parse_record_args(&["--scene", "cam"]).unwrap();
        assert_eq!(a.scene, Some(compose::Scene::Cam));
        assert!(parse_record_args(&["--scene", "nope"]).is_err());
    }

    #[test]
    fn test_pattern_prepares_without_a_display() {
        let opts = cli::prepare(
            parse_record_args(&["--test-pattern", "-t", "2", "--no-mic", "-o", "/tmp/x.mov"])
                .unwrap(),
        )
        .unwrap();
        assert_eq!(opts.size, (1920, 1080));
        assert_eq!(opts.test_pattern, Some(false));
        assert_eq!(opts.codec, media::Codec::H264);
        assert!(opts.mic.is_none());
        assert_eq!(opts.output, PathBuf::from("/tmp/x.mov"));
        // 4K of a 4K pattern is 4K, HEVC by default, and needs 4k's bitrate.
        let opts = cli::prepare(
            parse_record_args(&["--test-pattern", "-r", "4k", "--no-mic", "-o", "/tmp/x.mov"])
                .unwrap(),
        )
        .unwrap();
        assert_eq!(opts.size, (3840, 2160));
        assert_eq!(opts.codec, media::Codec::Hevc);
        assert!(opts.bitrate >= 30_000_000, "{}", opts.bitrate);
    }

    #[test]
    fn screen_capture_without_a_display_explains_itself() {
        // No display server, no portal: `prepare` must fail with something a
        // person can act on (this test environment has neither).
        if std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some() {
            return;
        }
        let err = cli::prepare(parse_record_args(&["-o", "/tmp/x.mov"]).unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("portal")
                || err.contains("Wayland")
                || err.contains("DISPLAY")
                || err.contains("X11"),
            "{err}"
        );
    }

    #[test]
    fn bad_arguments_are_rejected() {
        assert!(
            parse_record_args(&["--fps", "7"]).is_err()
                || cli::prepare(parse_record_args(&["--fps", "7"]).unwrap()).is_err()
        );
        assert!(parse_record_args(&["--replay", "5", "--test-pattern"]).is_ok());
        let err = cli::prepare(parse_record_args(&["--test-pattern", "--replay", "5"]).unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("--replay must be 15-300"), "{err}");
        let err = cli::prepare(parse_record_args(&["--no-record", "--test-pattern"]).unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("--no-record needs --replay or --stream"),
            "{err}"
        );
        let err = cli::prepare(parse_record_args(&["--test-pattern", "-t", "0"]).unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("--duration must be positive"), "{err}");
        // ProRes cannot go into an .mp4.
        let err = cli::prepare(
            parse_record_args(&["--test-pattern", "--codec", "prores", "-o", "/tmp/x.mp4"])
                .unwrap(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("ProRes needs a .mov output"), "{err}");
    }

    #[test]
    fn mic_backend_flags_map() {
        let a = parse_record_args(&["--test-pattern", "--mic-backend", "pipewire"]).unwrap();
        assert_eq!(a.mic_backend, "pipewire");
        let a = parse_record_args(&["--test-pattern", "--mic-backend", "sck"]).unwrap();
        let opts = cli::prepare(a).unwrap();
        assert_eq!(opts.mic_backend, record::MicBackend::Pipewire);
        let a = parse_record_args(&["--test-pattern", "--mic-backend", "jack"]).unwrap();
        assert!(cli::prepare(a).is_err());
    }

    #[test]
    fn default_output_is_under_the_videos_dir() {
        let p = cli::default_output();
        assert!(p.to_string_lossy().ends_with(".mov"), "{}", p.display());
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            name.starts_with("rec-") && name.len() == "rec-.mov".len() + 17,
            "{name}"
        );
    }
}
