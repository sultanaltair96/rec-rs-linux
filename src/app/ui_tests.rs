//! Headless UI tests with egui_kittest: the windows render, the mixer follows
//! the pointer and the keyboard, the settings round-trip through
//! `settings.json`, and the stream card refuses to go live without a key.
//!
//! Rendering goes through wgpu with the software adapter (Mesa's lavapipe), so
//! these run in the dev container with no display. The snapshot images under
//! `tests/snapshots/` are the reference: update them with
//! `UPDATE_SNAPSHOTS=1 cargo test`.

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use super::widgets::{self as w};

/// wgpu device creation and teardown on Mesa's lavapipe is not safe to run on
/// several threads at once (the test binary crashed with SIGSEGV every few
/// runs). Every test that builds a harness holds this, so one GPU context
/// exists at a time; the whole suite still takes about a second.
static GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn gpu() -> std::sync::MutexGuard<'static, ()> {
    GPU.lock().unwrap_or_else(|e| e.into_inner())
}
use super::{Cmd, Device, Meter, State, ui};
use crate::settings::{Settings, StreamDest};

fn device(name: &str, id: &str) -> Device {
    Device {
        id: id.into(),
        name: name.into(),
    }
}

/// A state with every card filled in (screen, camera, mic, stream).
fn full_state(scene: crate::compose::Scene) -> State {
    let settings = Settings {
        scene,
        ..Settings::default()
    };
    let mut st = State::new(settings);
    st.displays = vec!["Display 1 — 2560×1440".into()];
    st.windows = vec![(42, "foot — Terminal".into())];
    st.cameras = vec![device("Integrated Camera", "/dev/video0")];
    st.mics = vec![device("USB Microphone", "alsa_input.usb-mic")];
    st
}

/// A harness with the app's look installed. The first pass only installs the
/// fonts (egui applies them at the start of the next pass), the next draws the
/// window; the app itself does this when it creates its context.
fn themed_harness<F>(size: egui::Vec2, mut window: F, st: State) -> Harness<'static, State>
where
    F: FnMut(&mut egui::Ui, &mut State) + 'static,
{
    let first = std::cell::Cell::new(true);
    let mut h = Harness::builder()
        .with_size(size)
        .with_pixels_per_point(1.0)
        .wgpu()
        .build_ui_state(
            move |ui, st: &mut State| {
                if first.replace(false) {
                    crate::app::theme::apply(ui.ctx());
                    return;
                }
                window(ui, st);
            },
            st,
        );
    h.ctx.request_repaint();
    h.run();
    h
}

fn main_harness(st: State) -> Harness<'static, State> {
    themed_harness(egui::vec2(1040.0, 660.0), ui::main_window, st)
}

fn settings_harness(st: State) -> Harness<'static, State> {
    themed_harness(egui::vec2(520.0, 820.0), ui::settings_window, st)
}

#[test]
fn main_window_shows_every_card() {
    let _gpu_guard = gpu();
    // Tall enough that no card is clipped: egui hides clipped widgets from the
    // accessibility tree, and the sidebar's cards would otherwise be below the
    // fold.
    let h = themed_harness(
        egui::vec2(1040.0, 1500.0),
        ui::main_window,
        full_state(crate::compose::Scene::ScreenCam),
    );
    for label in [
        "Rec",
        "Settings…",
        "Screen + Cam",
        "Screen",
        "Camera",
        "SCENE",
        "SCREEN",
        "CAMERA",
        "CAMERA OVERLAY",
        "AUDIO",
        "OUTPUT",
        "REPLAY BUFFER",
        "STREAM",
        "System audio",
        "Microphone",
        "Keep recent footage",
        "Pause",
        "Open Folder",
        "Default microphone",
        "Entire display",
        "Default camera",
        "Default microphone",
        "Mute System",
        "Mute Mic",
    ] {
        assert!(
            h.query_by_label(label).is_some() || h.query_by_label_contains(label).is_some(),
            "the main window is missing {label:?}"
        );
    }
    // The timer starts at zero, the scene picker offers all three scenes and
    // the stream card lists every service.
    assert!(h.query_by_label("00:00").is_some());
    assert!(h.query_by_label("YouTube").is_some());
    assert!(h.query_by_label("Twitch").is_some());
    assert!(h.query_by_label("Kick").is_some());
    assert!(h.query_by_label("Go Live").is_some());
    // The mixer's levels are DragValues, so they can be typed into.
    assert!(
        h.query_all_by_role(egui::accesskit::Role::SpinButton)
            .count()
            >= 2,
        "two fader levels"
    );
}

#[test]
fn error_banner_shows_a_backend_error_and_can_be_dismissed() {
    let _gpu_guard = gpu();
    let mut st = full_state(crate::compose::Scene::Screen);
    st.error =
        Some("Preview unavailable: the live encode is not implemented yet (phase 1D)".into());
    let mut h = main_harness(st);
    assert!(
        h.query_by_label_contains("not implemented yet (phase 1D)")
            .is_some(),
        "the banner names the phase"
    );
    h.get_by_label("Dismiss").click();
    h.run();
    assert_eq!(h.state().error, None);
    assert!(
        h.query_by_label_contains("not implemented yet").is_none(),
        "the banner is gone"
    );
}

/// The fader: drag the knob, click the track, scroll, double-click for 0 dB.
struct FaderState {
    db: f32,
    rect: egui::Rect,
    clicks: u32,
    doubles: u32,
}

fn fader_harness(db: f32) -> Harness<'static, FaderState> {
    let mut h = Harness::builder()
        .with_size(egui::vec2(400.0, 100.0))
        .with_pixels_per_point(1.0)
        .with_step_dt(1.0 / 60.0)
        .with_max_steps(60)
        .build_ui_state(
            |ui, s: &mut FaderState| {
                ui.add_space(20.0);
                let resp = w::fader(ui, &mut s.db, 300.0, true, true);
                s.rect = resp.rect;
                s.clicks += u32::from(resp.clicked());
                s.doubles += u32::from(resp.double_clicked());
            },
            FaderState {
                db,
                rect: egui::Rect::NOTHING,
                clicks: 0,
                doubles: 0,
            },
        );
    h.run();
    h
}

fn x_of(s: &FaderState, db: f32) -> f32 {
    let travel = s.rect.shrink2(egui::vec2(w::KNOB_R + 1.0, 0.0)).x_range();
    egui::lerp(travel, w::fader_pos(db))
}

#[test]
fn fader_follows_drag_click_scroll_and_double_click() {
    let _gpu_guard = gpu();
    let mut h = fader_harness(-10.0);
    let r = h.state().rect;

    // Press on the knob and move right by 0.15 of the travel: -10 dB -> 0 dB.
    let travel = r.shrink2(egui::vec2(w::KNOB_R + 1.0, 0.0)).width();
    let start = egui::pos2(x_of(h.state(), -10.0) + 3.0, r.center().y);
    h.hover_at(start);
    h.run();
    h.event(egui::Event::PointerButton {
        pos: start,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: Default::default(),
    });
    h.run();
    for i in 1..=5 {
        let to = start + egui::vec2(0.15 * travel * i as f32 / 5.0, 0.0);
        h.hover_at(to);
        h.run();
    }
    h.event(egui::Event::PointerButton {
        pos: start + egui::vec2(0.15 * travel, 0.0),
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: Default::default(),
    });
    h.run();
    assert_eq!(h.state().db, 0.0, "the knob follows the drag");

    // Clicking the track elsewhere jumps there: -20 dB is 40% of the travel.
    let at = egui::pos2(x_of(h.state(), -20.0), r.center().y);
    h.hover_at(at);
    h.run();
    h.event(egui::Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: Default::default(),
    });
    h.run();
    h.event(egui::Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: Default::default(),
    });
    h.run();
    assert!(
        (h.state().db + 20.0).abs() <= 0.1,
        "click to jump: {}",
        h.state().db
    );

    // Scroll up 40 points: +1 dB, in fine steps.
    let before = h.state().db;
    h.hover_at(r.center());
    h.run();
    h.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, 40.0),
        phase: egui::TouchPhase::Move,
        modifiers: Default::default(),
    });
    h.run();
    assert!(
        (h.state().db - (before + 1.0)).abs() < 0.05,
        "scroll: {} -> {}",
        before,
        h.state().db
    );

    // A double click on the knob returns to unity, wherever it was. The two
    // clicks have to be close in time, so step once per event instead of
    // running until the UI settles.
    let at = egui::pos2(x_of(h.state(), 12.0), r.center().y);
    h.hover_at(at);
    h.run();
    // Move the harness's clock on, so the click above (well within egui's
    // 0.6 s triple-click window) is not read as a triple click.
    h.input_mut().time = Some(10.0);
    for _ in 0..2 {
        for pressed in [true, false] {
            h.event(egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: Default::default(),
            });
        }
    }
    h.run();
    assert!(
        h.state().doubles > 0,
        "egui saw a double click (clicks={} doubles={})",
        h.state().clicks,
        h.state().doubles
    );
    assert_eq!(h.state().db, 0.0, "double-click is the 0 dB detent");
}

/// One mixer channel, so the level field can be typed into and the mute button
/// clicked.
struct Chan {
    meter: Meter,
    db: f32,
    muted: bool,
}

fn channel_harness(db: f32, muted: bool) -> Harness<'static, Chan> {
    let mut h = Harness::builder()
        .with_size(egui::vec2(420.0, 80.0))
        .with_pixels_per_point(1.0)
        .build_ui_state(
            |ui, s: &mut Chan| {
                ui.add_space(8.0);
                ui::channel(ui, "System", s.meter, true, &mut s.db, &mut s.muted, 360.0);
            },
            Chan {
                meter: Meter {
                    db: -6.0,
                    peak_db: -6.0,
                    peak_at: None,
                },
                db,
                muted,
            },
        );
    h.run();
    h
}

#[test]
fn a_muted_channel_greys_its_meter() {
    let _gpu_guard = gpu();
    // The meter's green zone is unique in the UI, so count its pixels.
    let green = |h: &mut Harness<'_, Chan>| {
        let img = h.render().expect("render");
        img.pixels()
            .filter(|p| p.0[1] > 140 && p.0[0] < 120 && p.0[2] < 140)
            .count()
    };
    let mut h = channel_harness(-6.0, false);
    let loud = green(&mut h);
    assert!(loud > 0, "a live meter is green");
    h.state_mut().muted = true;
    h.run();
    let muted = green(&mut h);
    assert_eq!(muted, 0, "a muted meter is grey (it still moves)");
    // The grey fill is still drawn: the meter didn't vanish.
    let img = h.render().expect("render");
    let grey = img
        .pixels()
        .filter(|p| {
            let [r, g, b, _] = p.0;
            r.abs_diff(g) < 12 && g.abs_diff(b) < 12 && (40..=120).contains(&r)
        })
        .count();
    assert!(grey > 20, "the muted meter is drawn in grey ({grey} px)");
}

#[test]
fn the_level_can_be_typed() {
    let _gpu_guard = gpu();
    let mut h = channel_harness(-6.0, false);
    // The level field is the only text-editable value in the channel.
    h.get_by_role(egui::accesskit::Role::SpinButton).focus();
    h.run();
    h.event(egui::Event::Text("-12".into()));
    h.run();
    h.key_press(egui::Key::Enter);
    h.run();
    assert!(
        (h.state().db + 12.0).abs() < 0.01,
        "typed level: {}",
        h.state().db
    );
    // "off" is silence (-60 dB), as on the fader's bottom.
    h.get_by_role(egui::accesskit::Role::SpinButton).focus();
    h.run();
    h.event(egui::Event::Text("off".into()));
    h.run();
    h.key_press(egui::Key::Enter);
    h.run();
    assert_eq!(h.state().db, crate::mixer::MIN_DB);
}

#[test]
fn the_mute_button_toggles() {
    let _gpu_guard = gpu();
    let mut h = channel_harness(0.0, false);
    // The mute button's accessibility label is its hover text.
    let mute = h.get_by_label_contains("Mute System");
    mute.click();
    h.run();
    assert!(h.state().muted, "the mute button mutes");
}

/// Tests that write a settings file need the environment to themselves.
static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn the_settings_window_round_trips_to_settings_json() {
    let _gpu_guard = gpu();
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("rec-ui-settings-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("settings.json");
    unsafe { std::env::set_var("REC_SETTINGS", &path) };

    let settings = Settings {
        output_dir: dir.join("Videos"),
        resolution: "1440p".into(),
        fps: 30,
        ..Default::default()
    };
    let mut h = settings_harness(State::new(settings));
    assert!(h.query_by_label("Settings").is_some());
    // Low power preview is a checkbox in the VIDEO section.
    let low = h.get_by_label("Low power (half resolution, 15 fps)");
    low.click();
    h.run();
    assert!(h.state().settings.preview_low_power);
    // The settings file was written by the click, and reads back the same.
    let text = std::fs::read_to_string(&path).expect("settings.json written");
    assert!(text.contains("\"preview_low_power\": true"), "{text}");
    assert!(text.contains("\"resolution\": \"1440p\""));
    let back = Settings::from_json(&text).expect("valid json");
    assert_eq!(back, h.state().settings);
    // The folder dialog is not opened by the test; the path itself round-trips.
    assert_eq!(back.output_dir, dir.join("Videos"));

    unsafe { std::env::remove_var("REC_SETTINGS") };
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn go_live_needs_a_stream_key() {
    let _gpu_guard = gpu();
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::remove_var("REC_STREAM_KEY") };
    let settings = Settings {
        stream_dests: vec![
            StreamDest {
                dest: "youtube".into(),
                enabled: true,
            },
            StreamDest {
                dest: "twitch".into(),
                enabled: true,
            },
        ],
        ..Settings::default()
    };
    let mut st = State::new(settings);
    st.key_saved_clear_for_tests();
    let mut h = main_harness(st);
    // The card says every destination, and Go Live on both of them.
    assert!(h.query_by_label("Go Live on 2 Destinations").is_some());
    h.get_by_label("Go Live on 2 Destinations")
        .click_accesskit();
    h.run();
    assert!(
        h.state().cmds.is_empty() && h.state().streams.is_empty(),
        "a missing key blocks going live"
    );
    // A WHIP endpoint needs no key: with only that destination, Go Live works.
    let settings = Settings {
        stream_dests: vec![StreamDest {
            dest: "https://example.com/whip".into(),
            enabled: true,
        }],
        ..Settings::default()
    };
    let mut st = State::new(settings);
    st.key_saved_clear_for_tests();
    let mut h = main_harness(st);
    h.get_by_label("Go Live").click_accesskit();
    h.run();
    assert_eq!(h.state().cmds, [Cmd::ToggleStream]);
}

#[test]
fn the_app_menu_has_updates_unless_flatpak() {
    let _gpu_guard = gpu();
    let mut st = State::new(Settings::default());
    st.updates = true;
    let mut h = main_harness(st);
    h.get_by_label("⋯").click();
    h.run();
    for label in ["Check for Updates…", "Open Recordings Folder", "Quit Rec"] {
        assert!(
            h.query_by_label(label).is_some(),
            "the app menu is missing {label:?}"
        );
    }
    // Flatpak updates the app itself: no item.
    let mut st = State::new(Settings::default());
    st.updates = false;
    let mut h = main_harness(st);
    h.get_by_label("⋯").click();
    h.run();
    assert!(
        h.query_by_label("Check for Updates…").is_none(),
        "Flatpak hides the update item"
    );
    assert!(h.query_by_label("Quit Rec").is_some());
}

#[test]
fn the_live_encode_section_is_locked_while_streaming() {
    let _gpu_guard = gpu();
    let st = full_state(crate::compose::Scene::Screen);
    let mut h = settings_harness(st);
    // Not streaming: the "Platform preset" checkbox toggles.
    h.get_by_label("Platform preset").click();
    h.run();
    assert!(!h.state().settings.stream_bitrate_auto);
    h.state_mut().settings.stream_bitrate_auto = true;

    // Streaming: the whole section is disabled, with a visible reason.
    h.state_mut().streams = vec![super::Stream {
        id: 1,
        url: crate::live::destination_url("youtube"),
        status: None,
    }];
    h.run();
    assert!(
        h.query_by_label("Stop streaming to change the live encode.")
            .is_some(),
        "the reason is shown"
    );
    h.get_by_label("Platform preset").click_accesskit();
    h.run();
    assert!(
        h.state().settings.stream_bitrate_auto,
        "the live encode cannot change while streaming"
    );
}

#[test]
fn snapshots_of_the_main_and_settings_windows() {
    let _gpu_guard = gpu();
    // The reference images live in tests/snapshots; UPDATE_SNAPSHOTS=1 rewrites
    // them (the layout is compared pixel by pixel). Two harnesses in one test
    // have to report through one SnapshotResults.
    let mut results = egui_kittest::SnapshotResults::new();
    let mut h = main_harness(full_state(crate::compose::Scene::ScreenCam));
    h.snapshot("main-window");
    results.extend_harness(&mut h);

    let mut st = full_state(crate::compose::Scene::ScreenCam);
    st.settings.resolution = "1080p".into();
    let mut h = settings_harness(st);
    h.snapshot("settings-window");
    results.extend_harness(&mut h);
    results.into_result().expect("snapshots match");
}
