//! The seam between the app and the backends.
//!
//! The camera, the compositor and the writer are real now: `record::Session`
//! offers `set_pip`, `set_scene`, `cam_aspect` and `camera_name`, and
//! `Options::preview` carries the composited frame. What is still a stub is one
//! phase only — the live encode (replay buffer, streams, stream keys, phase
//! 1D) — and this module is the one place that names it, so the UI can say why
//! a control is disabled instead of silently doing nothing.
//!
//! Nothing here duplicates a `media_types` type.

use anyhow::Result;

use crate::live::{self, Live};
use crate::record::Options;

/// What the running backends can do right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// The camera can be opened and composited (1B and 1C).
    pub camera: bool,
    /// The overlay can move/restyle without restarting the session (1C).
    pub pip: bool,
    /// Scenes can be switched while recording (1C).
    pub scene_switch: bool,
    /// The replay buffer exists (1D).
    pub replay: bool,
    /// Streaming exists (1D).
    pub stream: bool,
}

/// Today's abilities: camera, compositor and live overlay/scene changes are
/// in; the live encode (replay buffer, streams, stream keys) is still phase 1D.
pub fn caps() -> Caps {
    Caps {
        camera: true,
        pip: true,
        scene_switch: true,
        replay: false,
        stream: false,
    }
}

/// The phase that provides an app feature, for the UI's "not yet" hints.
pub fn phase_note(feature: &str) -> String {
    match feature {
        "replay" | "stream" | "keys" => format!(
            "replay and streaming are not implemented yet (phase {})",
            live::PHASE
        ),
        other => format!("{other} is not available in this build"),
    }
}

/// One destination's live status, as the stream card shows it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamStatus {
    pub live: bool,
    /// "connecting", "reconnecting", … while not live.
    pub state: String,
    pub error: Option<String>,
    /// Seconds streamed.
    pub duration: f64,
    /// Bits per second.
    pub bitrate: f64,
    pub dropped_frames: u64,
    pub reconnects: u64,
}

/// Replay buffer (seconds held, bytes in memory), when it is on (phase 1D).
pub fn replay_stats(_live: &Live) -> Option<(f64, usize)> {
    None
}

/// Start the live encode for a session. `Err` is what the banner shows.
pub fn begin_live(_live: &Live, system: bool, mic: bool) -> Result<(), String> {
    let _ = (system, mic);
    Err(format!(
        "the live encode is not implemented yet (phase {})",
        live::PHASE
    ))
}

/// Add one destination to the running live encode (phase 1D): its output id
/// and URL.
pub fn add_output(_live: &Live, _url: &str, _key: &str) -> Result<u64, String> {
    Err(format!(
        "streaming is not implemented yet (phase {})",
        live::PHASE
    ))
}

/// Remove a destination by id (phase 1D).
pub fn remove_output(live: &Live, _id: u64) -> Result<(), String> {
    let _ = live;
    Err(format!(
        "streaming is not implemented yet (phase {})",
        live::PHASE
    ))
}

/// Every destination's status, by output id (phase 1D).
pub fn statuses(_live: &Live) -> Vec<(u64, StreamStatus)> {
    vec![]
}

/// Stop the live encode and every destination (phase 1D).
pub fn stop_all(live: &Live) {
    let _ = live;
}

/// Write the replay buffer to `path` in the background (phase 1D).
pub fn save_replay(_live: &Live, _path: std::path::PathBuf) -> Result<(), String> {
    Err(format!(
        "the replay buffer is not implemented yet (phase {})",
        live::PHASE
    ))
}

/// Whether a stream key is stored for `url` (phase 1D: the Secret Service).
/// Until then the `REC_STREAM_KEY` override is the whole story, which is also
/// what the CLI uses.
pub fn has_stream_key(url: &str) -> bool {
    has_stream_key_from(std::env::var("REC_STREAM_KEY").ok().as_deref(), url)
}

/// [`has_stream_key`] with the environment given (tests): WHIP endpoints may
/// run without a key, RTMP(S) may not.
pub fn has_stream_key_from(key: Option<&str>, url: &str) -> bool {
    if url.is_empty() {
        return false;
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return true;
    }
    key.is_some_and(|k| !k.trim().is_empty())
}

/// Store a stream key (phase 1D: the Secret Service). Until then, point at the
/// environment override instead of pretending.
pub fn save_stream_key(_url: &str, _key: &str) -> Result<(), String> {
    Err(format!(
        "stream keys in the Secret Service are not implemented yet (phase {}); set REC_STREAM_KEY",
        live::PHASE
    ))
}

/// Build the session options for these settings, exactly as the CLI would: the
/// app and `rec-rs record` share one code path (`cli::prepare`).
///
/// `output` None = a preview session (no file written). `test_pattern` swaps
/// the screen for the synthetic source (self-tests without a desktop).
pub fn options(
    settings: &crate::settings::Settings,
    output: Option<&std::path::Path>,
    live: Option<std::sync::Arc<Live>>,
    preview: Option<crate::media_types::FrameSink>,
    test_pattern: bool,
) -> Result<Options> {
    let mut args = settings.record_args(output);
    if test_pattern {
        args.push("--test-pattern".into());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut o = crate::cli::prepare(crate::parse_record_args(&refs)?)?;
    // The CLI writes a file unless told otherwise; the app's preview session
    // only captures (`Session::start` reads this flag), and a recording passes
    // its output path.
    o.write = output.is_some();
    o.live = live;
    o.volumes = settings.volumes();
    o.preview = preview;
    Ok(o)
}

/// The banner for a preview that did not start. The CLI's errors already name
/// the phase that owes the feature, so they are shown as they are.
pub fn preview_error(e: &anyhow::Error) -> String {
    let msg = format!("{e:#}");
    if msg.contains("not implemented yet") {
        // The stub modules say "…not implemented yet (phase 1X); use `rec-rs
        // record`…". The app is the window on the same pipeline.
        format!("Preview unavailable: {msg}")
    } else {
        format!("Preview unavailable: {msg}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_keys_come_from_the_environment_for_now() {
        assert!(has_stream_key_from(
            Some("abc"),
            "rtmps://live.twitch.tv/app"
        ));
        assert!(!has_stream_key_from(
            Some("  "),
            "rtmps://live.twitch.tv/app"
        ));
        assert!(!has_stream_key_from(None, "rtmps://live.twitch.tv/app"));
        // WHIP can work without one (a tokenless endpoint), RTMP never.
        assert!(has_stream_key_from(None, "https://example.com/whip"));
        assert!(has_stream_key_from(None, "http://localhost/whip"));
        assert!(!has_stream_key_from(Some("x"), ""));
        // The phase is named when saving is attempted, never silently ignored.
        let e = save_stream_key("rtmp://x/live", "k").unwrap_err();
        assert!(
            e.contains("phase 1D") && e.contains("REC_STREAM_KEY"),
            "{e}"
        );
    }

    #[test]
    fn live_adapter_says_which_phase_owes_the_feature() {
        let live = Live::new();
        assert_eq!(replay_stats(&live), None);
        assert!(statuses(&live).is_empty());
        assert!(
            add_output(&live, "rtmp://x/live", "k")
                .unwrap_err()
                .contains("phase 1D")
        );
        assert!(
            save_replay(&live, "/tmp/r.mp4".into())
                .unwrap_err()
                .contains("phase 1D")
        );
        assert!(
            begin_live(&live, true, true)
                .unwrap_err()
                .contains("phase 1D")
        );
        stop_all(&live);
    }

    #[test]
    fn caps_follow_the_backends() {
        // Camera, compositor and live overlay/scene changes are in (1B/1C);
        // the live encode is still phase 1D, and the UI says so.
        let c = caps();
        assert!(c.camera && c.pip && c.scene_switch);
        assert!(!c.replay && !c.stream);
        assert!(phase_note("replay").contains("phase 1D"));
        assert!(phase_note("stream").contains("phase 1D"));
        assert!(phase_note("keys").contains("phase 1D"));
    }

    #[test]
    fn options_are_the_cli_arguments() {
        let s = crate::settings::Settings::default();
        // A preview session: no output file, and the test pattern needs no
        // display, so this works in the container.
        let o = options(&s, None, None, None, true).expect("preview options");
        assert!(!o.write);
        assert_eq!(o.test_pattern, Some(false));
        assert_eq!(o.size, (1920, 1080));
        assert!(o.preview.is_none());
        assert!(o.live.is_none());
    }
}
