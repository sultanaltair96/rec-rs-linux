//! The tray item: Rec's replacement for upstream's macOS menu bar item.
//!
//! A StatusNotifierItem with the same menu, a red dot while recording (plus the
//! elapsed time in its title and tooltip) and "LIVE" while streaming without a
//! recording. SNI hosts differ in what they show: KDE and the GNOME AppIndicator
//! extension draw the icon and the title next to it, while some hosts show only
//! the icon — the tooltip carries the same text everywhere, so the state is
//! never invisible.
//!
//! The menu model, the title/tooltip mapping and the icon are pure functions
//! (unit tested); only [`start`] talks to D-Bus, and it returns `None` when
//! there is no StatusNotifierWatcher (a bare session), where the window and the
//! hotkeys still work.
//!
//! Adapted from rec-rs src/app/menubar.rs (MIT OR Apache-2.0), NSStatusItem
//! replaced by ksni's StatusNotifierItem.

use std::sync::Arc;

use ksni::menu::{MenuItem, StandardItem};
use ksni::{Icon, ToolTip, Tray as KsniTray};

use super::Cmd;

/// What the tray shows, in one value (the app rebuilds it every update).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct View {
    pub recording: bool,
    pub paused: bool,
    pub finishing: bool,
    /// Recorded time, `mm:ss` (from `record::clock`), empty when idle.
    pub clock: String,
    /// There is a last recording to reveal.
    pub has_last: bool,
    /// The replay buffer is on.
    pub replay: bool,
    /// A stream is running.
    pub live: bool,
    /// Flatpak updates the app itself: no "Check for Updates…".
    pub updates: bool,
    /// The global hotkey was registered (the menu says so when it wasn't).
    pub hotkey_ok: bool,
}

/// One row of the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Action {
        label: String,
        enabled: bool,
        cmd: Cmd,
    },
    /// A non-clickable line (the hotkey hint).
    Label(String),
    Separator,
}

/// The menu, in upstream's order.
pub fn menu(v: &View) -> Vec<Entry> {
    let action = |label: &str, enabled: bool, cmd: Cmd| Entry::Action {
        label: label.to_string(),
        enabled,
        cmd,
    };
    let mut m = vec![
        action(
            if v.finishing {
                "Saving…"
            } else if v.recording {
                "Stop Recording"
            } else {
                "Start Recording"
            },
            !v.finishing,
            Cmd::ToggleRecording,
        ),
        action(
            if v.paused { "Resume" } else { "Pause" },
            v.recording,
            Cmd::TogglePause,
        ),
        action("Save Replay", v.replay, Cmd::SaveReplay),
        action(
            if v.live { "Stop Streaming" } else { "Go Live" },
            true,
            Cmd::ToggleStream,
        ),
        Entry::Separator,
        action("Show Rec Window", true, Cmd::ShowWindow),
        action("Settings…", true, Cmd::ShowSettings),
        Entry::Separator,
        action("Reveal Last Recording", v.has_last, Cmd::RevealLast),
        action("Open Recordings Folder", true, Cmd::OpenFolder),
    ];
    if v.updates {
        m.push(action("Check for Updates…", true, Cmd::CheckForUpdates));
    }
    m.push(Entry::Separator);
    m.push(Entry::Label(if v.hotkey_ok {
        "Global hotkey: Ctrl+Shift+R".into()
    } else {
        "Global hotkey unavailable".into()
    }));
    m.push(action("Quit Rec", true, Cmd::Quit));
    m
}

/// The status item's title: the elapsed time while recording (with a red dot
/// drawn in the icon), " LIVE" while streaming, " Saving…" while finalizing.
pub fn title(v: &View) -> String {
    if v.finishing {
        " Saving…".into()
    } else if v.recording {
        format!(" {}", v.clock)
    } else if v.live {
        " LIVE".into()
    } else {
        String::new()
    }
}

/// The tooltip: the same state in words, for hosts that show no title.
pub fn tooltip(v: &View) -> String {
    if v.finishing {
        "Rec — saving the recording".into()
    } else if v.paused {
        format!("Rec — paused at {}", v.clock)
    } else if v.recording {
        format!("Rec — recording {}", v.clock)
    } else if v.live {
        "Rec — live".into()
    } else {
        "Rec".into()
    }
}

/// Whether the icon is the "live" (red) variant.
pub fn icon_alert(v: &View) -> bool {
    v.recording || v.live
}

/// The icon: a 64×64 ARGB32 (network byte order) rounded square with a red dot
/// while recording or live, Rec's grey otherwise. Drawn here so the tray needs
/// no icon theme.
pub fn icon_argb(v: &View, size: u32) -> Vec<u8> {
    let n = size as f32;
    let c = n / 2.0;
    let radius = n * 0.46;
    let dot = n * 0.30;
    // Recording: an opaque dark disc with a red dot; idle: a grey dot only,
    // which reads as a template icon on light and dark panels.
    let (fill, dot_color) = if icon_alert(v) {
        ([0x2bu8, 0x2b, 0x2d, 0xff], [0xffu8, 0x3b, 0x30, 0xff])
    } else {
        ([0, 0, 0, 0], [0xc8u8, 0xc8, 0xcc, 0xff])
    };
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let in_square = (px - c).abs() <= radius && (py - c).abs() <= radius;
            let in_dot = ((px - c).powi(2) + (py - c).powi(2)).sqrt() <= dot;
            // ARGB, network byte order: A, R, G, B.
            let px = if in_dot {
                [dot_color[3], dot_color[0], dot_color[1], dot_color[2]]
            } else if in_square {
                [fill[3], fill[0], fill[1], fill[2]]
            } else {
                [0, 0, 0, 0]
            };
            data.extend_from_slice(&px);
        }
    }
    data
}

fn icon(v: &View) -> Vec<Icon> {
    [22u32, 48]
        .into_iter()
        .map(|s| Icon {
            width: s as i32,
            height: s as i32,
            data: icon_argb(v, s),
        })
        .collect()
}

/// The ksni side: holds the current view and forwards clicks to the app.
pub struct Tray {
    pub view: View,
    send: Arc<dyn Fn(Cmd) + Send + Sync>,
}

impl KsniTray for Tray {
    fn id(&self) -> String {
        "rec-rs".into()
    }

    fn title(&self) -> String {
        title(&self.view)
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        icon(&self.view)
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            icon_name: String::new(),
            icon_pixmap: icon(&self.view),
            title: "Rec".into(),
            description: tooltip(&self.view),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        (self.send)(Cmd::ShowWindow);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        menu(&self.view)
            .into_iter()
            .map(|e| match e {
                Entry::Separator => MenuItem::Separator,
                Entry::Label(label) => MenuItem::Standard(StandardItem {
                    label,
                    enabled: false,
                    ..Default::default()
                }),
                Entry::Action {
                    label,
                    enabled,
                    cmd,
                } => {
                    let send = self.send.clone();
                    MenuItem::Standard(StandardItem {
                        label,
                        enabled,
                        activate: Box::new(move |_| send(cmd)),
                        ..Default::default()
                    })
                }
            })
            .collect()
    }
}

/// Start the tray. `None` (with a warning) when the session has no
/// StatusNotifierWatcher, so the app keeps working without one.
pub fn start(
    view: View,
    send: Arc<dyn Fn(Cmd) + Send + Sync>,
) -> Option<ksni::blocking::Handle<Tray>> {
    use ksni::blocking::TrayMethods;
    // Flatpak: owning the well-known name is not allowed inside the sandbox.
    let flatpak = super::install::is_flatpak();
    let tray = Tray { view, send };
    match tray.disable_dbus_name(flatpak).spawn() {
        Ok(h) => Some(h),
        Err(e) => {
            eprintln!("rec: warning: no tray item ({e}); close the window to keep Rec running hidden");
            None
        }
    }
}

/// Update the item's state (title, icon, menu).
pub fn update(handle: &ksni::blocking::Handle<Tray>, view: View) {
    handle.update(move |t| {
        if t.view != view {
            t.view = view;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle() -> View {
        View {
            updates: true,
            hotkey_ok: true,
            ..Default::default()
        }
    }

    fn labels(v: &View) -> Vec<String> {
        menu(v)
            .into_iter()
            .filter_map(|e| match e {
                Entry::Action { label, .. } | Entry::Label(label) => Some(label),
                Entry::Separator => None,
            })
            .collect()
    }

    #[test]
    fn menu_matches_upstream_in_order() {
        let v = idle();
        assert_eq!(
            labels(&v),
            [
                "Start Recording",
                "Pause",
                "Save Replay",
                "Go Live",
                "Show Rec Window",
                "Settings…",
                "Reveal Last Recording",
                "Open Recordings Folder",
                "Check for Updates…",
                "Global hotkey: Ctrl+Shift+R",
                "Quit Rec",
            ]
        );
        // Separators: after Go Live, after Settings, before the hotkey line.
        let seps = menu(&v)
            .iter()
            .filter(|e| **e == Entry::Separator)
            .count();
        assert_eq!(seps, 3);
        // Recording flips the first two rows and enables Pause.
        let v = View {
            recording: true,
            clock: "01:02".into(),
            ..idle()
        };
        let m = menu(&v);
        assert_eq!(
            m[0],
            Entry::Action {
                label: "Stop Recording".into(),
                enabled: true,
                cmd: Cmd::ToggleRecording
            }
        );
        assert_eq!(
            m[1],
            Entry::Action {
                label: "Pause".into(),
                enabled: true,
                cmd: Cmd::TogglePause
            }
        );
        // Paused: Resume. Finishing: Saving… and no second start.
        let v = View {
            recording: true,
            paused: true,
            ..idle()
        };
        assert_eq!(labels(&v)[1], "Resume");
        let v = View {
            finishing: true,
            ..idle()
        };
        assert_eq!(labels(&v)[0], "Saving…");
        assert!(!matches!(menu(&v)[0], Entry::Action { enabled: true, .. }));
        // Replay off: Save Replay is disabled; live: Stop Streaming.
        let v = View {
            live: true,
            ..idle()
        };
        assert_eq!(labels(&v)[3], "Stop Streaming");
        assert!(!matches!(menu(&idle())[2], Entry::Action { enabled: true, .. }));
        // No last recording: nothing to reveal (index 8; 0-3 the actions,
        // 4 separator, 5/6 window/settings, 7 separator, 8 reveal).
        assert!(!matches!(
            menu(&idle())[8],
            Entry::Action { enabled: true, .. }
        ));
        // Flatpak: no update item, and the hotkey line says when it failed.
        let v = View {
            updates: false,
            hotkey_ok: false,
            ..Default::default()
        };
        assert!(!labels(&v).iter().any(|l| l.contains("Check for Updates")));
        assert!(labels(&v).contains(&"Global hotkey unavailable".to_string()));
    }

    #[test]
    fn titles_match_upstream() {
        assert_eq!(title(&idle()), "");
        assert_eq!(tooltip(&idle()), "Rec");
        let v = View {
            recording: true,
            clock: "02:03".into(),
            ..idle()
        };
        assert_eq!(title(&v), " 02:03");
        assert_eq!(tooltip(&v), "Rec — recording 02:03");
        let v = View {
            recording: true,
            paused: true,
            clock: "02:03".into(),
            ..idle()
        };
        assert_eq!(title(&v), " 02:03");
        assert_eq!(tooltip(&v), "Rec — paused at 02:03");
        let v = View {
            live: true,
            ..idle()
        };
        assert_eq!(title(&v), " LIVE");
        assert_eq!(tooltip(&v), "Rec — live");
        let v = View {
            finishing: true,
            ..idle()
        };
        assert_eq!(title(&v), " Saving…");
        assert_eq!(tooltip(&v), "Rec — saving the recording");
    }

    #[test]
    fn icon_shows_a_red_dot_while_capturing() {
        let idle_icon = icon_argb(&idle(), 8);
        assert_eq!(idle_icon.len(), 8 * 8 * 4, "ARGB, 4 bytes per pixel");
        // The centre is the dot; the corner is transparent.
        let centre = 4 * (4 * 8 + 4);
        let corner = 0;
        assert_eq!(&idle_icon[corner..corner + 4], &[0, 0, 0, 0]);
        let v = View {
            recording: true,
            ..idle()
        };
        let rec = icon_argb(&v, 8);
        assert_eq!(&rec[centre..centre + 4], &[0xff, 0xff, 0x3b, 0x30]);
        // Idle is grey, and the square behind both is opaque Rec grey.
        assert_ne!(&idle_icon[centre..centre + 4], &rec[centre..centre + 4]);
        let edge = 4 * (4 * 8 + 1);
        assert_eq!(&rec[edge..edge + 4], &[0xff, 0x2b, 0x2b, 0x2d]);
        // Live without a recording also shows the alert icon.
        let v = View {
            live: true,
            ..idle()
        };
        assert!(icon_alert(&v));
        assert!(!icon_alert(&idle()));
    }
}
