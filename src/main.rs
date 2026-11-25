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
pub mod diagnostics;
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
    /// Export a privacy-filtered diagnostics archive (never includes recordings).
    #[command(name = "report", visible_alias = "diagnostics")]
    Diagnostics(cli::DiagnosticsArgs),
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
    /// The live encode's harness: drive the live path with the test pattern
    /// (streams, replay buffer) without a recording. Used by `scripts/live/`.
    #[command(hide = true, name = "live-selftest")]
    LiveSelftest(live::harness::HarnessArgs),
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
    diagnostics::record(diagnostics::Event::RecordingStarted);
    let stats = record::run(opts)?;
    diagnostics::record(diagnostics::Event::RecordingStopped);
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
            "rec: saved {} ({} video frames, {} duplicated, {} dropped; audio buffers sys {} mic {} mix {}, {} dropped, {} clipped samples)",
            out.display(),
            stats.video_frames_out,
            stats.video_dups,
            stats.video_drops,
            stats.audio_sys_buffers,
            stats.audio_mic_buffers,
            stats.audio_mix_buffers,
            stats.audio_drops,
            stats.audio_clipped_samples
        );
        if std::env::var_os("REC_DEBUG").is_some() {
            eprintln!(
                "rec: debug: audio {} gaps filled, {} resyncs, {} overruns; video {} in, {} retries, latency max {:.3}s; finalise {} ms over {} fragments",
                stats.audio_gaps_filled,
                stats.audio_resyncs,
                stats.audio_overruns,
                stats.video_frames_in,
                stats.video_retries,
                stats.screen_latency_max,
                stats.finalize_millis,
                stats.fragments
            );
        }
    }
    println!("{}", out.display());
    Ok(())
}

/// `rec-rs stream-key DEST`: store (or delete) a stream key in the Secret
/// Service. The key is read from the terminal with echo off and is never
/// printed, logged or written to a file.
fn stream_key(dest: &str, delete: bool) -> Result<()> {
    let url = live::destination_url(dest);
    if delete {
        live::keyring::delete(&url)?;
        println!("removed the stream key for {url}");
        return Ok(());
    }
    let tty = unsafe { libc::isatty(0) } == 1;
    let mut old: libc::termios = unsafe { std::mem::zeroed() };
    if tty {
        eprint!("stream key for {url}: ");
        unsafe {
            libc::tcgetattr(0, &mut old);
            let mut t = old;
            t.c_lflag &= !libc::ECHO;
            libc::tcsetattr(0, libc::TCSANOW, &t);
        }
    }
    let mut key = String::new();
    let r = std::io::stdin().read_line(&mut key);
    if tty {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &old) };
        eprintln!();
    }
    r?;
    let key = key.trim();
    if key.is_empty() {
        bail!("no key given");
    }
    live::keyring::set(&url, key)?;
    println!("saved the stream key for {url} in the Secret Service");
    Ok(())
}

/// One display line of `rec-rs devices`, in upstream's format
/// (`reference/rec-rs/src/main.rs:606-660`): index, id, pixels, points.
///
/// Deliberately no extra markers: a script parses these lines, and upstream
/// prints nothing after the points. A Linux-only fact worth saying (the display
/// is the picker's choice on Wayland) goes on its own indented line below.
fn display_line(d: &media_types::DisplayInfo) -> String {
    format!(
        "  [{}] id {}  {}x{} pixels ({}x{} points)",
        d.index, d.id, d.pixels.0, d.pixels.1, d.points.0, d.points.1
    )
}

fn devices() -> Result<()> {
    println!("Displays:");
    match capture::screen::displays() {
        Ok(ds) => {
            for d in &ds {
                println!("{}", display_line(d));
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
    // Upstream prints this line only when the output device reports a
    // latency; a session with no sink prints nothing here at all.
    if let Some(l) = capture::audio::output_latency() {
        println!(
            "Output latency (system audio is shifted by this): {:.1} ms",
            l * 1000.0
        );
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
    println!("microphone       : {}", capture::audio::permission_status());
    println!(
        "camera           : {}",
        capture::camera::permission_status()
    );
    Ok(())
}

fn main() {
    // BEGIN diagnostics stderr tee (process-local, fail-closed retention).
    // Install before library threads. fd 2 remains blocking; an independent
    // reader drains it even when the original terminal stops accepting output.
    if let Err(e) = diagnostics::install_stderr_tee() {
        eprintln!("rec: warning: stderr diagnostics unavailable: {e}");
    }
    // END diagnostics stderr tee.
    diagnostics::record(diagnostics::Event::AppStarted);
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
        Cmd::Diagnostics(args) => {
            let path = args
                .path
                .or(args.output)
                .unwrap_or_else(|| util::videos_dir().join(util::timestamped("rec-report", "tar")));
            // Resolve relative destinations here, not in the GUI's working directory.
            let path = std::path::absolute(path)?;
            eprintln!(
                "The report includes device names (e.g. Bluetooth headset names); review before sharing."
            );
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Only an absent GUI permits fallback. A failed/lost completion reply
            // must not trigger a second export or hide the original error.
            if !app::control::request_report(&path)? {
                eprintln!(
                    "rec: no running GUI; report contains local diagnostics only, without a running app's log ring."
                );
                diagnostics::write_report(&path)?;
            }
            println!("{}", path.display());
            Ok(())
        }
        Cmd::Devices => devices(),
        // Control over the shared runtime-directory socket, so these work from
        // a second Flatpak instance (its PID namespace hides the recorder's
        // pid); the pidfile signal stays as the fallback (record.rs).
        Cmd::Stop => record::stop_running(),
        Cmd::Pause => record::pause_running(),
        Cmd::Resume => record::resume_running(),
        Cmd::SaveReplay => record::save_replay_running(),
        Cmd::StreamKey { dest, delete } => stream_key(&dest, delete),
        Cmd::LiveSelftest(a) => live::harness::run(a),
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
    fn diagnostics_command_parses() {
        let parsed =
            Cli::try_parse_from(["rec-rs", "diagnostics", "--output", "/tmp/report.tar"]).unwrap();
        assert!(
            matches!(parsed.cmd, Some(Cmd::Diagnostics(cli::DiagnosticsArgs { output: Some(output), .. })) if output == std::path::Path::new("/tmp/report.tar"))
        );
        assert!(Cli::try_parse_from(["rec-rs", "report"]).is_ok());
        assert!(Cli::try_parse_from(["rec-rs", "report", "/tmp/report.tar"]).is_ok());
    }

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
    fn devices_lines_match_upstreams_format() {
        // Upstream prints exactly this (no primary marker, no extra columns).
        let d = media_types::DisplayInfo {
            index: 1,
            id: "HDMI-1".into(),
            name: "HDMI-1".into(),
            pixels: (2560, 1440),
            points: (1280, 720),
            primary: true,
        };
        assert_eq!(
            display_line(&d),
            "  [1] id HDMI-1  2560x1440 pixels (1280x720 points)"
        );
        assert!(!display_line(&d).contains("primary"));
        // A second display numbers itself, and a non-primary one looks the same:
        // the marker used to distinguish them and must not come back.
        let second = media_types::DisplayInfo {
            index: 2,
            primary: false,
            ..d
        };
        assert_eq!(
            display_line(&second),
            "  [2] id HDMI-1  2560x1440 pixels (1280x720 points)"
        );
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
