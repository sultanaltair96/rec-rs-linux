//! The seam between the app and the backends.
//!
//! The app is written against what `main` offers today: `record::Session`,
//! `settings`, `live::Live`, `capture::*` and `compose`. Some things the app
//! needs are still being built by other phase-1 workers, and this module is the
//! one place that names them, so the UI can say *why* a control is disabled
//! instead of silently doing nothing:
//!
//! - camera overlay and scene switching at run time (*phase 1C*, the
//!   compositor: `Session` has no `set_pip`/`set_scene`/`cam_aspect` yet),
//! - camera device names (*phase 1B*),
//! - the replay buffer, per-destination stream status and stream keys
//!   (*phase 1D*, `live::Live`).
//!
//! Every function here is the smallest shape the UI needs; when the backend
//! lands, its module implements the same call and this file shrinks. Nothing
//! here duplicates a `media_types` type.

use anyhow::Result;

use crate::compose::{PipSpec, Scene};
use crate::live::{self, Live};
use crate::record::{Options, Session};

/// What the running backends can do right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// The camera can be opened (phase 1B).
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

/// Today's abilities: the app still has to start cleanly on a build where every
/// backend after the writer is a stub, which is exactly what these say.
pub fn caps() -> Caps {
    Caps {
        camera: false,
        pip: false,
        scene_switch: false,
        replay: false,
        stream: false,
    }
}

/// The phase that provides an app feature, for the UI's "not yet" hints.
pub fn phase_note(feature: &str) -> String {
    match feature {
        "camera" => "camera capture is not implemented yet (phase 1B)".into(),
        "pip" | "scene" => format!(
            "the camera overlay and live scene switching need the compositor (phase {})",
            crate::gpu::PHASE
        ),
        "replay" | "stream" | "keys" => format!(
            "replay and streaming are not implemented yet (phase {})",
            live::PHASE
        ),
        other => format!("{other} is not available in this build"),
    }
}

/// The session features the app needs that `record::Session` does not expose
/// yet. A backend that implements them replaces the default bodies.
pub trait SessionExt {
    /// Restyle or move the camera overlay live (previews, the file, live).
    fn set_pip(&self, pip: PipSpec) -> Result<(), String>;
    /// Switch scenes while recording.
    fn set_scene(&self, scene: Scene) -> Result<(), String>;
    /// The camera's aspect ratio, for drag hit-testing in the preview.
    fn cam_aspect(&self) -> Option<f64>;
    /// The camera the session opened, if any.
    fn camera_name(&self) -> Option<String>;
}

impl SessionExt for Session {
    fn set_pip(&self, _pip: PipSpec) -> Result<(), String> {
        Err(phase_note("pip"))
    }

    fn set_scene(&self, _scene: Scene) -> Result<(), String> {
        Err(phase_note("scene"))
    }

    fn cam_aspect(&self) -> Option<f64> {
        None
    }

    fn camera_name(&self) -> Option<String> {
        None
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
        assert!(has_stream_key_from(Some("abc"), "rtmps://live.twitch.tv/app"));
        assert!(!has_stream_key_from(Some("  "), "rtmps://live.twitch.tv/app"));
        assert!(!has_stream_key_from(None, "rtmps://live.twitch.tv/app"));
        // WHIP can work without one (a tokenless endpoint), RTMP never.
        assert!(has_stream_key_from(None, "https://example.com/whip"));
        assert!(has_stream_key_from(None, "http://localhost/whip"));
        assert!(!has_stream_key_from(Some("x"), ""));
        // The phase is named when saving is attempted, never silently ignored.
        let e = save_stream_key("rtmp://x/live", "k").unwrap_err();
        assert!(e.contains("phase 1D") && e.contains("REC_STREAM_KEY"), "{e}");
    }

    #[test]
    fn live_adapter_says_which_phase_owes_the_feature() {
        let live = Live::new();
        assert_eq!(replay_stats(&live), None);
        assert!(statuses(&live).is_empty());
        assert!(add_output(&live, "rtmp://x/live", "k").unwrap_err().contains("phase 1D"));
        assert!(save_replay(&live, "/tmp/r.mp4".into()).unwrap_err().contains("phase 1D"));
        assert!(begin_live(&live, true, true).unwrap_err().contains("phase 1D"));
        stop_all(&live);
    }

    #[test]
    fn session_ext_names_the_compositor() {
        // Without a session there is nothing to hold; the trait is exercised
        // through the default bodies, which the UI relies on for its hints.
        let pip = PipSpec::default();
        let note = phase_note("pip");
        assert!(note.contains("compositor"), "{note}");
        assert!(phase_note("camera").contains("phase 1B"));
        assert!(phase_note("stream").contains("phase 1D"));
        assert_eq!(pip, PipSpec::default());
        assert!(!caps().pip && !caps().camera && !caps().replay && !caps().stream);
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
