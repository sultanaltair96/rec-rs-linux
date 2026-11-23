//! System audio and microphone capture (phase 1B).
//!
//! Both sources are PipeWire nodes: system audio is the default sink's monitor
//! (what the speakers play, captured before it is heard), a microphone is an
//! `Audio/Source` node chosen by name or by the id `rec-rs devices` prints. The
//! sources deliver [`AudioChunk`](crate::media_types::AudioChunk)s at the
//! device's own rate and channel count; the mixer resamples to the file's
//! 48 kHz. Every chunk is stamped with the PipeWire buffer's graph time, which
//! is `CLOCK_MONOTONIC`, the host clock the whole recorder uses.
//!
//! The device matching rules (exact unique id, the USB port suffix, saved
//! display name, then the selector as a name substring) are upstream's, so a
//! saved `--mic` keeps working when a device is replugged on another port.
//!
//! Adapted from rec-rs src/camera.rs (microphone discovery and `pick_mic`),
//! src/record.rs and src/outlatency.rs (MIT OR Apache-2.0), with AVCapture and
//! CoreAudio replaced by PipeWire.

// The factory surface is called by `record.rs`, `devices` and the app; the
// device list and permission helpers are also used by phase 1E.
#![allow(dead_code)]

use anyhow::{Result, bail};

use crate::media_types::{AudioSource, MicInfo};

mod pipewire;

/// The phase that implements this module.
pub const PHASE: &str = "1B";

/// Which microphone to open.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioRequest {
    /// Unique id (`node.name`) or name substring; None = the system default.
    pub device: Option<String>,
    /// The device's display name, used to find it again when its id changed.
    pub name: Option<String>,
}

/// A saved or requested mic resolved against the connected devices.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MicPick {
    /// Node name to open; None = the system default mic.
    pub id: Option<String>,
    /// Display name of the resolved device.
    pub name: Option<String>,
    /// Why the requested mic wasn't used (it is not connected).
    pub warning: Option<String>,
}

/// The microphones `--mic` and the mixer list.
///
/// `id` is the PipeWire node name (stable across restarts, unlike the numeric
/// node id) and `name` the node's human description. The default source comes
/// first.
pub fn microphones() -> Result<Vec<MicInfo>> {
    let snap = pipewire::snapshot(false)?;
    let default = snap.default_source.clone();
    let mut out: Vec<MicInfo> = snap
        .sources()
        .map(|n| MicInfo {
            id: n.name.clone(),
            name: n.description.clone(),
            rate: n.rate,
        })
        .collect();
    // The desktop's default source first, like AVCapture lists the preferred
    // device first upstream; `pick_mic` matches by id/name either way.
    if let Some(d) = default {
        out.sort_by_key(|m| m.id != d);
    }
    Ok(out)
}

/// The output device's latency in seconds: system audio is captured when it is
/// mixed and heard one latency later, so it is shifted back by this.
///
/// None when there is no default sink or it reports no latency (a virtual sink
/// with no `node.latency`). PipeWire's own latency, i.e. the graph and device
/// buffer that `pw-top` shows, is the closest equivalent of CoreAudio's device
/// latency; the user can override it with `--system-audio-offset-ms`.
pub fn output_latency() -> Option<f64> {
    pipewire::snapshot(true).ok().and_then(|s| s.sink_latency)
}

/// Start capturing the system output (the default sink's monitor).
pub fn open_system() -> Result<Box<dyn AudioSource>> {
    pipewire::open_capture(crate::media_types::AudioKind::System, None)
        .map(|s| Box::new(s) as Box<dyn AudioSource>)
}

/// Start capturing a microphone.
///
/// An explicitly requested device that is not connected is an error (the
/// caller decides whether that ends the session); opening the *default* mic is
/// the only path that may fall back, and `record.rs` turns its failure into a
/// warning. [`pick_mic`] is the resilient variant upstream uses for a saved
/// device: it returns the default plus a warning instead of failing.
pub fn open_mic(req: &AudioRequest) -> Result<Box<dyn AudioSource>> {
    let sel = req
        .device
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "default");
    let pick = match sel {
        None => MicPick::default(),
        Some(sel) => pick_mic(sel, req.name.as_deref())?,
    };
    if sel.is_some() && pick.id.is_none() {
        let label = req
            .name
            .as_deref()
            .filter(|n| !n.is_empty())
            .or(sel)
            .unwrap_or("");
        bail!("Microphone \u{201c}{label}\u{201d} is not connected (see `rec-rs devices`)");
    }
    pipewire::open_capture(crate::media_types::AudioKind::Mic, pick.id.as_deref())
        .map(|s| Box::new(s) as Box<dyn AudioSource>)
}

/// Resolve a `--mic` selection to a device, warning when the requested one is
/// not connected and the default is used instead (upstream's rule: audio never
/// blocks a session). The app shows the warning; the CLI's explicit `--mic`
/// path fails instead (see [`open_mic`]).
pub fn pick_mic(sel: &str, name: Option<&str>) -> Result<MicPick> {
    let mics = microphones()?;
    Ok(pick_from(&mics, sel, name))
}

/// The matching rules of [`pick_mic`], without a PipeWire session.
///
/// Upstream order: exact unique ID, the same USB device on another port
/// (unique IDs end in `:index`), the saved display name (exact, then
/// substring), then the selector as a name (exact, then substring). Matching
/// is case-insensitive from the saved name on, like upstream.
fn pick_from(mics: &[MicInfo], sel: &str, name: Option<&str>) -> MicPick {
    if sel.trim().is_empty() || sel.trim() == "default" {
        return MicPick::default();
    }
    let sel = sel.trim();
    if let Some(m) = mics.iter().find(|m| m.id == sel) {
        return pick_of(m);
    }
    // USB unique IDs end in a port/instance index (`…:SERIAL:4`) that can
    // change between plugs; match on the rest.
    if let Some(stem) = id_stem(sel)
        && let Some(m) = mics
            .iter()
            .find(|m| id_stem(&m.id).as_deref() == Some(stem.as_str()))
    {
        return pick_of(m);
    }
    for n in name
        .into_iter()
        .chain([sel])
        .filter(|n| !n.trim().is_empty())
    {
        let lower = n.to_lowercase();
        if let Some(m) = mics.iter().find(|m| m.name.to_lowercase() == lower) {
            return pick_of(m);
        }
        if let Some(m) = mics.iter().find(|m| m.name.to_lowercase().contains(&lower)) {
            return pick_of(m);
        }
    }
    let label = name.filter(|n| !n.is_empty()).unwrap_or(sel);
    MicPick {
        id: None,
        name: None,
        warning: Some(format!(
            "Microphone \u{201c}{label}\u{201d} not connected; using the default mic"
        )),
    }
}

fn pick_of(m: &MicInfo) -> MicPick {
    MicPick {
        id: Some(m.id.clone()),
        name: Some(m.name.clone()),
        warning: None,
    }
}

/// An id without a trailing `:digits` port/instance suffix.
fn id_stem(id: &str) -> Option<String> {
    id.rsplit_once(':')
        .filter(|(_, n)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        .map(|(s, _)| s.to_string())
}

/// One line for `rec-rs permissions`.
pub fn permission_status() -> String {
    match microphones() {
        Ok(m) if m.is_empty() => "unavailable (PipeWire has no audio inputs)".to_string(),
        Ok(m) => format!(
            "ok (PipeWire: {} input{})",
            m.len(),
            if m.len() == 1 { "" } else { "s" }
        ),
        Err(e) => format!("unavailable ({e})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mic(id: &str, name: &str) -> MicInfo {
        MicInfo {
            id: id.into(),
            name: name.into(),
            rate: None,
        }
    }

    /// Upstream `camera.rs::tests::mic_matching`, with PipeWire node names.
    #[test]
    fn mic_matching() {
        let mics = [
            mic(
                "alsa_input.pci-0000_00_1f.3.analog-stereo",
                "Built-in Audio Analog Stereo",
            ),
            mic(
                "alsa_input.usb-DJI_DJIPocket3_123456789ABCDEF-02.analog-stereo:2",
                "OsmoPocket3",
            ),
        ];
        let dji =
            Some("alsa_input.usb-DJI_DJIPocket3_123456789ABCDEF-02.analog-stereo:2".to_string());
        // Exact node name.
        assert_eq!(pick_from(&mics, &mics[1].id, None).id, dji);
        // Same device with another port/instance suffix.
        assert_eq!(
            pick_from(
                &mics,
                "alsa_input.usb-DJI_DJIPocket3_123456789ABCDEF-02.analog-stereo:4",
                None
            )
            .id,
            dji
        );
        // By saved name.
        assert_eq!(pick_from(&mics, "gone-id", Some("OsmoPocket3")).id, dji);
        // By name substring (CLI).
        assert_eq!(
            pick_from(&mics, "built-in", None).id.as_deref(),
            Some("alsa_input.pci-0000_00_1f.3.analog-stereo")
        );
        // Not connected: the default is used, with upstream's warning.
        let p = pick_from(&mics[..1], "alsa_input.usb-x", Some("OsmoPocket3"));
        assert!(p.id.is_none());
        assert_eq!(
            p.warning.as_deref(),
            Some("Microphone \u{201c}OsmoPocket3\u{201d} not connected; using the default mic")
        );
        // "default"/empty select the default without a warning.
        for sel in ["default", " ", ""] {
            let p = pick_from(&mics, sel, None);
            assert_eq!(p, MicPick::default(), "{sel:?}");
        }
    }

    #[test]
    fn usb_stem_only_strips_numeric_suffixes() {
        assert_eq!(id_stem("device:SERIAL:4").as_deref(), Some("device:SERIAL"));
        assert_eq!(id_stem("device:SERIAL:port"), None);
        assert_eq!(id_stem("plain"), None);
    }
}
