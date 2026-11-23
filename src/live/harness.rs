//! The live encode's own harness: a hidden `live-selftest` command that drives
//! [`Live`](super::Live) with the synthetic test pattern, exactly as the
//! writer's tap does (composited frames and the post-fader mix), so the encode,
//! the replay buffer, RTMP/RTMPS and WHIP can be verified end to end before —
//! and independently of — the writer's wiring in `record.rs`.
//!
//! It is not a user-facing command: `rec-rs live-selftest` only exists for
//! `scripts/live/` and the self-test. Frames come from [`Synth`](crate::synth),
//! so a stream carries the same white flash / 1 kHz beep pattern the sync
//! checks measure. The command writes the recorder's pidfile, so
//! `rec-rs save-replay` works against it like against a recording.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args;

use super::{
    shared_preset, start_output, Live, LiveConfig, OutputStatus, Platform,
};
use crate::media_types::{self, AudioSink, AudioSource, FrameSink, VideoSource};
use crate::synth::{Pattern, Synth};

#[derive(Args)]
pub struct HarnessArgs {
    /// How long to run.
    #[arg(long, default_value_t = 20.0)]
    pub secs: f64,
    /// Live encode box: 720p, 1080p, 1440p, 4k or WxH.
    #[arg(long, default_value = "1080p")]
    pub resolution: String,
    /// Synthetic capture size (the live encode scales it when it is larger).
    #[arg(long, value_name = "WxH")]
    pub capture: Option<String>,
    /// Frames per second.
    #[arg(long, default_value_t = 30)]
    pub fps: u32,
    /// Live video codec: auto, h264 or hevc.
    #[arg(long, default_value = "auto", value_parser = ["auto", "h264", "hevc"])]
    pub codec: String,
    /// Live video bitrate in kb/s (default: the platforms' preset).
    #[arg(long)]
    pub bitrate: Option<u32>,
    /// Live keyframe interval in seconds.
    #[arg(long, default_value_t = 2.0)]
    pub keyframe: f64,
    /// Average bitrate instead of constant.
    #[arg(long)]
    pub vbr: bool,
    /// Live audio bitrate in kb/s.
    #[arg(long, default_value_t = 160)]
    pub audio_bitrate: u32,
    /// Keep the last SECS seconds in memory (15-300).
    #[arg(long, value_name = "SECS")]
    pub replay: Option<f64>,
    /// Save the replay buffer here when the run ends.
    #[arg(long, value_name = "FILE")]
    pub save_replay: Option<PathBuf>,
    /// Stream destination (repeatable): a service or an rtmp(s):// / WHIP URL.
    #[arg(long, value_name = "URL")]
    pub stream: Vec<String>,
    /// Stress the encoder with random noise blocks.
    #[arg(long)]
    pub noise: bool,
    /// Folder for a `save-replay` requested with the signal.
    #[arg(long, value_name = "DIR")]
    pub output_dir: Option<PathBuf>,
    /// Print only the final summary.
    #[arg(long)]
    pub quiet: bool,
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// The pidfile of the recorder, removed on the way out.
struct Pidfile(PathBuf);

impl Drop for Pidfile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn run(a: HarnessArgs) -> Result<()> {
    if a.secs <= 0.0 {
        bail!("--secs must be positive");
    }
    let (capture, live_size) = capture_and_live(&a)?;
    let destinations = a
        .stream
        .iter()
        .map(|d| super::resolve_destination(d))
        .collect::<Result<Vec<_>>>()?;
    let urls: Vec<String> = destinations.iter().map(|(u, _)| u.clone()).collect();
    let platforms: Vec<Platform> = urls.iter().map(|u| super::platform(u)).collect();
    let (preset_kbps, max_h, hevc) =
        shared_preset(&platforms, live_size.1, a.fps.min(60), &a.codec);
    let kbps = a.bitrate.unwrap_or(preset_kbps);
    let live = Arc::new(Live::new());
    live.set_config(LiveConfig {
        width: live_size.0,
        height: live_size.1.min(max_h.max(1) * 16 / 9),
        bitrate: kbps.clamp(500, 80_000) as u64 * 1000,
        keyframe_secs: a.keyframe.clamp(0.5, 4.0),
        cbr: !a.vbr,
        audio_bitrate: a.audio_bitrate.clamp(64, 320) * 1000,
        hevc,
    });
    if let Some(secs) = a.replay {
        if !(15.0..=300.0).contains(&secs) {
            bail!("--replay must be 15-300 seconds");
        }
        live.set_replay(Some(secs));
    }
    live.begin_session(true, false)?;
    let ids = live.add_outputs(&urls, |url| {
        let key = destinations
            .iter()
            .find(|(u, _)| u == url)
            .map(|(_, k)| k.as_str())
            .unwrap_or("");
        start_output(url, key)
    })?;
    if !a.quiet {
        let cfg = live.config().unwrap();
        eprintln!(
            "rec: live-selftest {}x{} -> {}x{} {} {:.1} Mb/s{}",
            capture.0,
            capture.1,
            cfg.width,
            cfg.height,
            if cfg.hevc { "HEVC" } else { "H.264" },
            cfg.bitrate as f64 / 1e6,
            if cfg.cbr { " CBR" } else { " ABR" }
        );
        for (url, _) in &destinations {
            eprintln!("rec: streaming to {}", super::destination_name(url));
        }
    }

    // The signal controls (`rec-rs save-replay`) and a clean shutdown.
    unsafe {
        libc::signal(libc::SIGINT, on_stop as *const () as usize);
        libc::signal(libc::SIGTERM, on_stop as *const () as usize);
        libc::signal(libc::SIGHUP, on_stop as *const () as usize);
    }
    crate::record::install_control_signals();
    let pidfile = crate::record::pidfile_path();
    if let Some(dir) = pidfile.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let replay_dir = a
        .output_dir
        .clone()
        .or_else(|| a.save_replay.as_ref().and_then(|p| p.parent().map(PathBuf::from)))
        .unwrap_or_else(crate::util::videos_dir);
    let _ = std::fs::write(
        &pidfile,
        format!(
            "{}\n{}\n",
            std::process::id(),
            replay_dir
                .join(crate::util::timestamped("replay", "mp4"))
                .display()
        ),
    );
    let _pidfile = Pidfile(pidfile);

    // The synthetic capture: the same pattern (and beep) the writer gets in a
    // test-pattern recording.
    let pattern = Pattern {
        width: capture.0,
        height: capture.1,
        fps: a.fps,
        format: media_types::PixelFormat::Bgra,
        noise: a.noise,
    };
    let mut synth = Synth::new(pattern);
    let video_sink: FrameSink = {
        let live = live.clone();
        Arc::new(move |frame| live.push_video(&frame))
    };
    let audio_sink: AudioSink = {
        let live = live.clone();
        Arc::new(move |_, chunk| live.push_audio(&chunk))
    };
    synth.start(video_sink)?;
    let mut audio = synth.audio(true);
    audio.start(audio_sink)?;

    let started = Instant::now();
    let mut saves: Vec<std::thread::JoinHandle<Result<PathBuf>>> = vec![];
    let mut last_status = Instant::now();
    let mut ever_live = vec![false; ids.len()];
    let mut out = Summary::default();
    while started.elapsed().as_secs_f64() < a.secs && !STOP.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(50));
        if crate::record::REPLAY_REQ.swap(false, Ordering::SeqCst) {
            let path = replay_dir.join(crate::util::timestamped("replay", "mp4"));
            match live.save_replay_async(path) {
                Ok(h) => saves.push(h),
                Err(e) => eprintln!("rec: replay: {e:#}"),
            }
        }
        for h in std::mem::take(&mut saves) {
            if h.is_finished() {
                match h.join() {
                    Ok(Ok(p)) => println!("live: saved {}", p.display()),
                    Ok(Err(e)) => eprintln!("rec: replay: {e:#}"),
                    Err(_) => eprintln!("rec: replay: the writer panicked"),
                }
            } else {
                saves.push(h);
            }
        }
        let statuses = live.statuses();
        for (i, (_, s)) in statuses.iter().enumerate() {
            if s.live {
                ever_live[i] = true;
            }
        }
        if !a.quiet && last_status.elapsed() >= Duration::from_secs(1) {
            last_status = Instant::now();
            let mut line = String::new();
            if let Some((secs, bytes)) = live.replay_stats() {
                line += &format!("  replay {secs:.0}s {:.1} MB", bytes as f64 / 1e6);
            }
            for (_, s) in &statuses {
                line += &format!(
                    "  {} {} {:.1} Mb/s drop {}",
                    s.name,
                    s.state,
                    s.bitrate / 1e6,
                    s.dropped_frames
                );
            }
            eprint!("\r\x1b[2K  {:02.0}s{}  ", started.elapsed().as_secs_f64(), line);
        }
    }
    if !a.quiet {
        eprintln!();
    }
    drop(audio);
    synth.stop().ok();
    // Give the encoders a moment to hand over what is already captured.
    std::thread::sleep(Duration::from_millis(300));

    let statuses = live.statuses();
    let (video_packets, audio_packets, bytes, dropped) = live.encode_stats();
    out.video = video_packets;
    out.audio = audio_packets;
    out.bytes = bytes;
    out.dropped = dropped;
    out.replay = live.replay_stats();
    println!(
        "live: encoded video {} audio {} packets, {:.1} MB, dropped {}",
        out.video,
        out.audio,
        out.bytes as f64 / 1e6,
        out.dropped
    );
    if let Some((secs, held)) = out.replay {
        println!(
            "live: replay {:.1}s held, {:.1} MB",
            secs,
            held as f64 / 1e6
        );
    }
    for (i, (_, s)) in statuses.iter().enumerate() {
        print_status(s, ever_live[i]);
    }
    out.live_everywhere = statuses.iter().enumerate().all(|(i, _)| ever_live[i]);

    let mut failed = None;
    if let Some(path) = &a.save_replay {
        match live.save_replay(path.clone()) {
            Ok(()) => println!("live: saved {}", path.display()),
            Err(e) => {
                eprintln!("rec: replay: {e:#}");
                failed = Some(e);
            }
        }
    }
    for h in saves {
        match h.join() {
            Ok(Ok(p)) => println!("live: saved {}", p.display()),
            Ok(Err(e)) => eprintln!("rec: replay: {e:#}"),
            Err(_) => {}
        }
    }
    live.stop_all();
    let error = live.error();
    if let Some(e) = &error {
        println!("live: encode error {e}");
    }
    if let Some(e) = failed {
        return Err(e);
    }
    if !out.live_everywhere {
        bail!("not every destination went live");
    }
    if error.is_some() {
        bail!("the live encode reported an error");
    }
    Ok(())
}

#[derive(Default)]
struct Summary {
    video: u64,
    audio: u64,
    bytes: u64,
    dropped: u64,
    replay: Option<(f64, usize)>,
    live_everywhere: bool,
}

fn print_status(s: &OutputStatus, ever_live: bool) {
    println!(
        "live: stream {} {} live {:.1}s {:.2} Mb/s drops {} reconnects {} ever-live {} error {}",
        s.name,
        s.state,
        s.duration,
        s.bitrate / 1e6,
        s.dropped_frames,
        s.reconnects,
        if ever_live { "yes" } else { "no" },
        s.error.as_deref().unwrap_or("none"),
    );
}

/// The synthetic capture size and the live encode's box.
fn capture_and_live(a: &HarnessArgs) -> Result<((usize, usize), (usize, usize))> {
    let live = crate::cli::live_box(&a.resolution)?;
    let capture = match &a.capture {
        Some(s) => match crate::cli::live_box(s) {
            Ok(size) => size,
            Err(_) => crate::cli::parse_size(s, (3840, 2160)).context("--capture")?,
        },
        None => live,
    };
    if capture.0 < 64 || capture.1 < 64 {
        bail!("--capture is too small (at least 64x64)");
    }
    Ok((capture, live))
}
