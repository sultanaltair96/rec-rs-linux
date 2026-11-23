//! The seam between the app and the backends.
//!
//! Everything the app starts is real: `record::Session` owns the capture and the
//! file, the camera and the compositor are wired, and the live encode
//! ([`crate::live::Live`]) owns the replay buffer and the streams. This module
//! is the one place that translates between the UI's model and those APIs, so a
//! control is never silently ignored: `Err` here is what the banner shows.
//!
//! The live encode's shape: `Live::begin_session(system, mic)` once a session
//! exists, `add_output`/`remove_output` for destinations (each returns the id
//! the status list is keyed by), `statuses()` for the stream cards,
//! `replay_stats()` for the memory line, `save_replay_async` behind the Save
//! Replay button and `stop_all` when the app quits.
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

/// Today's abilities: everything the window offers is backed by a real
/// implementation (camera, compositor, live overlay and scene changes, the live
/// encode with its replay buffer and streams).
pub fn caps() -> Caps {
    Caps {
        camera: true,
        pip: true,
        scene_switch: true,
        replay: true,
        stream: true,
    }
}

/// What the UI says when a control has nothing to act on yet.
pub fn phase_note(feature: &str) -> String {
    match feature {
        "replay" => "the replay buffer starts with the capture session".into(),
        "stream" | "keys" => "streaming needs a destination in Stream".into(),
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

/// Replay buffer (seconds held, bytes in memory), when it is on.
pub fn replay_stats(live: &Live) -> Option<(f64, usize)> {
    live.replay_stats()
}

/// Start the live encode for a session: which audio sources this session
/// delivers (already through their faders). Silence stands in when there are
/// none, so every service gets an audio track.
pub fn begin_live(live: &Live, system: bool, mic: bool) -> Result<(), String> {
    live.begin_session(system, mic)
        .map_err(|e| format!("{e:#}"))
}

/// The key for a destination: the one the caller typed, else the Secret Service
/// entry for the URL. WHIP endpoints may run without one; RTMP(S) may not.
fn destination_key(url: &str, key: &str) -> Result<String, String> {
    let key = key.trim();
    if !key.is_empty() {
        return Ok(key.to_string());
    }
    if let Some(stored) = live::keyring::get(url) {
        return Ok(stored);
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Ok(String::new());
    }
    Err(format!(
        "no stream key for {url}: type one in Stream (it goes to the Secret Service) or set REC_STREAM_KEY"
    ))
}

/// Add one destination to the running live encode; its output id is what
/// [`statuses`] and [`remove_output`] use.
pub fn add_output(live: &Live, url: &str, key: &str) -> Result<u64, String> {
    let url = live::destination_url(url);
    let key = destination_key(&url, key)?;
    let output = live::start_output(&url, &key).map_err(|e| {
        format!(
            "Could not go live on {}: {e:#}",
            live::destination_name(&url)
        )
    })?;
    Ok(live.add_output(output))
}

/// Remove a destination by id (the encode keeps running for the others).
pub fn remove_output(live: &Live, id: u64) -> Result<(), String> {
    live.remove_output(id);
    Ok(())
}

/// Every destination's status, by output id.
pub fn statuses(live: &Live) -> Vec<(u64, StreamStatus)> {
    live.statuses()
        .into_iter()
        .map(|(id, s)| {
            (
                id,
                StreamStatus {
                    live: s.live,
                    state: s.state,
                    error: s.error,
                    duration: s.duration,
                    bitrate: s.bitrate,
                    dropped_frames: s.dropped_frames,
                    reconnects: s.reconnects,
                },
            )
        })
        .collect()
}

/// Stop the live encode and every destination (the window closed, or Go Live
/// was switched off).
pub fn stop_all(live: &Live) {
    live.stop_all()
}

/// Write the replay buffer to `path` in the background (the buffer keeps
/// running). An immediate error — no buffer, still empty — is the banner's; the
/// finished path is printed when it lands.
pub fn save_replay(live: &Live, path: std::path::PathBuf) -> Result<(), String> {
    let handle = live.save_replay_async(path).map_err(|e| format!("{e:#}"))?;
    std::thread::spawn(move || match handle.join() {
        Ok(Ok(saved)) => eprintln!("rec: replay saved {}", saved.display()),
        Ok(Err(e)) => eprintln!("rec: replay: {e:#}"),
        Err(_) => eprintln!("rec: replay: the writer panicked"),
    });
    Ok(())
}

/// Whether a stream key is stored for `url`: the Secret Service entry, or the
/// `REC_STREAM_KEY` override the CLI uses too.
pub fn has_stream_key(url: &str) -> bool {
    if url.is_empty() {
        return false;
    }
    let env = std::env::var("REC_STREAM_KEY").ok();
    has_stream_key_from(env.as_deref(), url) || live::keyring::get(url).is_some()
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

/// Store a stream key in the Secret Service, keyed by destination URL (the same
/// place `rec-rs stream-key` writes). Never a file.
pub fn save_stream_key(url: &str, key: &str) -> Result<(), String> {
    if url.is_empty() || key.trim().is_empty() {
        return Err("a destination URL and a key are needed".into());
    }
    live::keyring::set(url, key.trim()).map_err(|e| format!("{e:#}"))
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

/// The banner for a preview that did not start. Keep the whole source error
/// chain: camera negotiation, permission and first-frame failures contain the
/// actionable cause below the device/session context. Do not replace these
/// with a generic "camera is not delivering" warning.
pub fn preview_error(e: &anyhow::Error) -> String {
    format!("Preview unavailable: {e:#}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_banner_preserves_camera_source_errors() {
        let error = anyhow::anyhow!("Permission denied (os error 13)")
            .context("opening /dev/video0")
            .context("camera USB Camera (/dev/video0)");
        assert_eq!(
            preview_error(&error),
            "Preview unavailable: camera USB Camera (/dev/video0): opening /dev/video0: Permission denied (os error 13)"
        );
        for cause in [
            "camera “USB Camera” is not connected (see `rec-rs devices`)",
            "camera USB Camera did not deliver a frame within 5 s",
            "camera portal permission denied",
            "camera capture is not implemented yet (phase 1B)",
        ] {
            assert_eq!(
                preview_error(&anyhow::anyhow!(cause)),
                format!("Preview unavailable: {cause}")
            );
        }
    }

    /// The seam drives the real `Live`: a key from the environment or the store,
    /// destinations added and removed by id, statuses keyed by that id, and the
    /// replay buffer's own report.
    #[test]
    fn the_seam_drives_the_live_encode() {
        let live = Live::new();
        assert_eq!(replay_stats(&live), None);
        assert!(statuses(&live).is_empty());
        // No buffer configured: the Save Replay button has nothing to save.
        let e = save_replay(&live, "/tmp/r.mp4".into()).unwrap_err();
        assert!(e.contains("replay buffer is off"), "{e}");

        begin_live(&live, true, false).expect("a session can start the encode");
        live.set_replay(Some(15.0));
        assert_eq!(replay_stats(&live), Some((0.0, 0)), "on, still empty");

        // A destination without a key fails before any connection is made, and
        // one with an unsupported URL fails with the URL in the message.
        let e = add_output(&live, "rtmps://live.twitch.tv/app", "").unwrap_err();
        assert!(e.contains("no stream key"), "{e}");
        let e = add_output(&live, "ftp://example.com/live", "k").unwrap_err();
        assert!(e.contains("Could not go live on"), "{e}");

        assert!(live.active(), "the replay buffer keeps the encode running");
        stop_all(&live);
        live.set_replay(None);
        assert!(!live.active(), "nothing consumes it: the encode stops");
    }

    #[test]
    fn stream_keys_come_from_the_store_or_the_environment() {
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
        // Storing goes to the Secret Service and never to a file: with one
        // running this round-trips, without one the error names it.
        let url = format!("rtmps://rec-rs-test.invalid/{}", std::process::id());
        match save_stream_key(&url, "sk_test_123") {
            Ok(()) => {
                assert_eq!(live::keyring::get(&url).as_deref(), Some("sk_test_123"));
                assert!(has_stream_key(&url), "the store answers");
                live::keyring::delete(&url).unwrap();
            }
            Err(e) => {
                assert!(
                    e.contains("Secret Service") || e.contains("keyring"),
                    "a missing keyring must be named: {e}"
                );
                assert!(!has_stream_key(&url));
            }
        }
        assert!(save_stream_key("", "k").is_err());
        assert!(save_stream_key("rtmp://x/live", "  ").is_err());
    }

    #[test]
    fn caps_follow_the_backends() {
        // Every feature the window offers is backed by an implementation now.
        let c = caps();
        assert!(c.camera && c.pip && c.scene_switch && c.replay && c.stream);
        // The hints no longer name a phase that owes the feature.
        for feature in ["replay", "stream", "keys"] {
            assert!(
                !phase_note(feature).contains("not implemented"),
                "{}",
                phase_note(feature)
            );
        }
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
