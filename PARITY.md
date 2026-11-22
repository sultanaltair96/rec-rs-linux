# Parity with upstream rec-rs

Every feature the upstream macOS app has, one row each, with the Linux approach, the phase that
owns it, its status, and the test or self-test case that proves it. Walked end to end through
upstream's README (pinned at `93dd494`), its option table, its output-format guarantees, its
streaming section, its OBS comparison table and its known-gaps list.

Status: **done** (works and is proved), **partial** (a documented part is missing), **todo** (not
built yet; the phase column says who), **n-a** (does not apply on Linux, with the reason).

Evidence names: `unit` tests are in the crate (`cargo test`), `selftest:` cases are in
`scripts/selftest.sh`, `script:` are `scripts/synccheck.py` / `scripts/levelcheck.py`.

## Command line and controls

| Feature | Upstream behaviour | Linux approach | Phase/owner | Status | Evidence |
|---|---|---|---|---|---|
| `record` command | Records screen + system audio + mic + webcam | Same, through PipeWire/V4L2 sources and the GStreamer writer | 0 (CLI), 1A/1B (sources) | partial | unit `tests::goal_command_line_parses`; selftest `pattern-1080-h264` (test-pattern source) |
| `-o/--out/--output` | Output file, `.mov` or `.mp4`, default `~/Movies/rec-<ts>.mov` | Same flags; default `$XDG_VIDEOS_DIR`/`~/Videos/rec-<ts>.mov` | 0 | done | unit `tests::default_output_is_under_the_videos_dir`, `util::tests::timestamps_are_safe_file_names` |
| `-d/--display N` | Display number from `devices`, default 1 | Same, resolved by the ScreenCast portal | 1A | todo | parses: unit `tests::goal_command_line_parses` |
| `--window W` | Window id or app/title substring | Same via portal window selection, X11 fallback | 1A | todo | parses: unit `tests::goal_command_line_parses` |
| `--region X,Y,W,H` | Crop a display region (points) | Same, in logical units | 1A | todo | parses + conflicts with `--window`: unit `tests::bad_arguments_are_rejected` |
| `-r/--resolution` | 720p/1080p/1440p/4k/native/WxH, source aspect, never upscaled | Identical maths (ported) | 0 | done | unit `tests::sizes` (incl. portrait, odd sizes, 16:10 examples) |
| `-f/--fps` | 24/25/30/50/60/120 (divisors of 60000) | Same validation and grid | 0 | done | unit `tests::bad_arguments_are_rejected`; selftest CFR check |
| `--codec` | auto (H.264 ≤1080p, HEVC above), h264, hevc, prores | Same selection and same automatic rule | 0 | done | selftest `pattern-prores`, `pattern-hevc`, `pattern-4k-hevc` (`-r 4k` picks HEVC by itself) |
| `--bitrate` | Mb/s, else bits/pixel heuristic | Same heuristic (ported); x264enc/x265enc honour it | 0 | done | unit `tests::test_pattern_prepares_without_a_display` (4K ≥ 30 Mb/s); selftest `h264-bitrate`: 8 Mb/s requested → 8.10 Mb/s at profile High |
| `--system-audio` / `--no-system-audio` | On by default, flags override each other | Same | 0 | done | unit `tests::system_audio_flags_override_each_other`; selftest `gain` (system track) |
| `--mic DEVICE` / `--no-mic` | Default mic, id or name substring | PipeWire node by id/name | 1B | todo | parses: unit `tests::goal_command_line_parses`; unavailable mic warns and continues |
| `-w/--cam/--webcam [DEVICE]` | Webcam overlay, default or device | V4L2 (or camera portal in Flatpak) | 1B | todo | parses: unit `tests::goal_command_line_parses` |
| `--scene` | screen, screen-cam, cam | Same enum and default rule | 0 | done | unit `tests::scene_option` |
| `--pip-size/-corner/-shape/-roundness/-border/-border-color/-shadow/-pos/-no-mirror` | Overlay geometry and style | Same parsing, clamping and geometry | 0 | done | unit `tests::overlay_options`, `compose::tests::corners_and_free_positions`, `snapping`, `shape_coverage` |
| `--audio-tracks both/mix/stems` | Mix + stems, mix only, stems only | Same values; phase 0 writes stems only (mix is 1C) | 0 (parse), 1C (mix) | partial | parses: unit `settings::tests::record_args_parse_with_the_cli`; writer warns when a mix is requested |
| `--system-gain`, `--mic-gain` | dB per source, −60 = silence, +24 max, in every track | Same fader maths applied in the writer | 0 | done | selftest `gain` (`--ref=-6 --expect=-12`); unit `mixer::tests::volume_gain`, `fader_glides_and_passes_unity_through` |
| `--system-audio-offset-ms`, `--mic-offset-ms` | Sync shifts; system default = output latency | Same; latency read from PipeWire in 1B | 0 (shift), 1B (latency) | partial | unit (offsets applied in `push_audio`); selftest `sync` |
| `--no-cursor`, `--clicks` | Hide cursor / highlight clicks | Portal cursor modes (embedded/metadata) | 1A | todo | parse |
| `-t/--duration` | Exact, counted in output frames; pauses don't count | Identical: grid slots + `reached_limit` | 0 | done | selftest `sync` (300 frames for `-t 5`), selftest `pause-resume` (300 frames with a 2 s pause) |
| `--mic-backend` | `sck` or `avcapture` (macOS) | `pipewire` (default) or `pulse`; `sck`/`avcapture` map with a message | 0 | done | unit `record::tests::mic_backend_maps_upstream_values` |
| `--test-pattern [--noise]` | Flash + beep source for self-tests | Same, CPU frames, BGRA/I420 | 0 | done | unit `synth::tests::pattern_timeline`, `beep_starts_on_each_second`, `source_emits_on_the_grid`; selftest `pattern-1080-h264` |
| `--replay SECS` | 15–300 s replay buffer | Planned on the live encode | 1D | todo | range validated in `prepare` (unit) and the run fails with phase 1D |
| `--stream DEST` (repeatable) | RTMP(S)/WHIP, platform shortcuts | Same URLs and presets, built-in clients | 1D | todo | unit `live::tests::services_expand_and_are_recognized`, `shared_preset_fits_every_destination` |
| `--no-record` | Replay/stream only | Same rule (needs replay or stream) | 1D | todo | unit `tests::bad_arguments_are_rejected` |
| `--live-resolution/--stream-bitrate/--stream-codec/--keyframe/--vbr/--audio-bitrate` | Live encode controls | Same parsing, presets and clamping | 0 (parse/config), 1D (encode) | partial | unit `settings::tests::live_encode_fits_every_enabled_destination` |
| `devices` command | Lists displays, windows, cameras, mics + output latency | Same layout; each list reports its phase until the source lands | 0 (shape), 1A/1B | partial | `rec-rs devices` output in the phase report |
| `probe FILE` command | AVFoundation: playable, tracks, codecs, full sample decode, non-zero exit on error | GStreamer: qtdemux structure + `uridecodebin` decode of every sample | 0 | done | selftest uses it on every case (`.probe` files); `ffmpeg -v error -f null` cross-check |
| `permissions [--request]` | TCC status and prompts | Portal/PipeWire/camera/secret-service availability | 1A | todo | prints "unknown (not implemented yet, phase 1A/1B)" |
| `stop` / `pause` / `resume` | pidfile + SIGUSR1/SIGUSR2/SIGTERM | pidfile in `$XDG_RUNTIME_DIR` (else `/tmp/rec-rs-$UID`), SIGINT to stop | 0 | done | unit `record::tests::pidfile_lives_in_the_runtime_dir`, `no_recorder_means_a_clear_error`; selftest `pause-resume` runs `pause`/`resume`; `stop` demo in the phase report |
| `save-replay` | SIGINFO writes the buffer to MP4 | `SIGRTMIN+1` (no SIGINFO on Linux); buffer is 1D | 0 (signal), 1D (buffer) | partial | unit `record::tests::save_replay_signal_is_a_real_time_signal` |
| `stream-key DEST [--delete]` | Keychain | Secret Service (`secret-service` crate), `REC_STREAM_KEY` meanwhile | 1D | todo | the command fails with phase 1D and names the env override |
| `gui [--start-recording] [--settings] [--window ID]` | The app | egui 0.36 on wgpu 30, winit 0.30; a hidden `--test-pattern` for self-tests | 1E | done | unit `app::ui_tests::*` (11 cases); selftest `gui`: the real window records under headless sway and shows preview frames, a second launch hands over, `pause`/`resume`/`stop` reach it, SIGTERM finalizes |

## Main window and app behaviour

| Feature | Upstream behaviour | Linux approach | Phase/owner | Status | Evidence |
|---|---|---|---|---|---|
| Live preview of what will be recorded | Rendered at preview size, composited | The session's newest frame (BGRA/RGBA/NV12/I420) converted on the CPU to an egui texture at the preview's pixel size; the compositor hands over a GPU frame in 1C (same `record::Options::preview` tap) | 1E (1C for the composited frame) | partial | unit `app::preview::tests::*` (size, BGRA, resample, NV12/BT.709, newest-only); selftest `gui` logs `preview frame 704x396` from the real window; snapshot `tests/snapshots/main-window.png` |
| Own windows excluded from capture | ScreenCaptureKit filter | Portal picks the window; own windows are excluded the same way | 1A | todo | — |
| Preview low-power mode (half res, 15 fps) | Setting | Same: half resolution and a 15 fps wake rate | 1E | done | unit `app::preview::tests::the_preview_target_is_the_area_in_pixels`; setting round-trip in `app::ui_tests::the_settings_window_round_trips_to_settings_json` |
| Scene: Screen | Full display or window | Same | 1A | todo | picker: unit `app::ui_tests::main_window_shows_every_card` |
| Scene: Screen + Cam, drag anywhere, corner snap | Overlay placement | Same geometry, wgpu rendering; the preview drag uses `compose::overlay_rect`/`snap_corner` | 0 (geometry), 1E (drag), 1C (GPU) | partial | unit `compose::tests::snapping`, `app::ui_tests::main_window_shows_every_card` |
| Overlay shapes (circle, rounded, square, pill, squircle), roundness, size, border, shadow, mirror | CoreImage | Same masks and shadows computed on the CPU, drawn by wgpu; all controls in the sidebar and settings | 0 (masks), 1E (controls), 1C (GPU) | partial | unit `shape_coverage`, `pill_has_round_ends`, `border_inset_mask`, snapshot `main-window.png` (overlay card) |
| Overlay changes live while recording, baked in, no extra encode | Yes | Sent to the running session (`set_pip`); the compositor applies them in 1C | 1C | todo | the seam and its phase note: unit `app::session::tests::session_ext_names_the_compositor` |
| Scene: Camera full frame | AVCapture | V4L2 | 1B (source), 1C (composite) | todo | — |
| Switch scenes live if recording started with the camera | Yes | Same rule: only camera scenes are selectable while recording without a camera | 1C/1E | partial | unit `app::ui_tests::main_window_shows_every_card`; `State::can_switch_live` |
| Sources: display, window, camera, mic, system audio | Yes | PipeWire display/window, V4L2, PipeWire audio | 1A/1B | todo | the sidebar and its combos: unit `app::ui_tests::main_window_shows_every_card` |
| Record/Stop button and timer | Yes | App | 1E | done | unit `app::tests::state_shows_the_recording_clock_and_pause`; selftest `gui` (two recordings through the app) |
| Pause/Resume button | Yes | App (the session API exists) | 1E | done | selftest `gui` drives `rec-rs pause`/`resume` against the running app; unit `app::ui_tests::main_window_shows_every_card` |
| Audio mixer: meter with peak hold, fader, mute, dB | Yes, after the fader | Same widgets on the same session API | 1E | done | unit `app::tests::meters_decay_and_hold_peaks`, `app::ui_tests::a_muted_channel_greys_its_meter`, `the_mute_button_toggles` |
| Fader range −∞..+24 dB, drag/click/scroll/double-click/type | Yes | Same | 1E | done | unit `app::widgets::tests::fader_*`, `app::ui_tests::fader_follows_drag_click_scroll_and_double_click`, `the_level_can_be_typed` |
| Level changes apply live to every track (and replay/streams) | Yes, ≤20 ms ramp | Writer follows live (`set_volumes`); ≤20 ms envelope | 0 (ramp), 1E (widget) | partial | unit `fader_glides_and_passes_unity_through`; the app sends on every fader move (`State::apply_volumes`) |
| Reveal last / open folder | Yes | `xdg-open`; FileManager1 `ShowItems` selects the file when the file manager offers it | 1E | partial | unit `app::linux_desktop::tests::file_uris_are_escaped`, `reveal_asks_file_manager1_for_the_file`; selecting the file needs a real desktop (below) |
| Replay buffer 15–300 s with memory use, Save Replay | Yes | Planned on the live encode; the card shows the length, memory, Save Replay and names phase 1D until then | 1D (buffer), 1E (card) | partial | unit `app::ui_tests::main_window_shows_every_card`; `app::session::tests::live_adapter_says_which_phase_owes_the_feature` |
| Stream to YouTube/Twitch/Kick + custom servers, Go Live all-at-once, all-or-nothing | Yes | Same destinations and presets, built-in RTMP/WHIP; the card blocks Go Live without a key | 1D (clients), 1E (card) | partial | unit `app::ui_tests::go_live_needs_a_stream_key`; `app::session::tests::stream_keys_come_from_the_environment_for_now` |
| Per-destination live status (duration, bitrate, drops, reconnect) and LIVE 2/3 | Yes | Same from the output status | 1D | todo | the card renders the status shape: `ui::stream_status`; LIVE badge in `app::ui_tests::main_window_shows_every_card` |
| One encode for every destination, within all limits | Yes | Same `shared_preset` logic | 0 (choice), 1D (encode) | partial | unit `shared_preset_fits_every_destination`, `settings::tests::live_encode_fits_every_enabled_destination` |
| Replay/stream run with or without a recording, survive recording start/stop, gaps bridged | Yes | Same design: the live encode is attached to every capture session and keeps the capture alive with the window closed | 1D | todo | the lifecycle is in place: `App::live_wanted`, `sync_preview`, `toggle_stream` |
| Settings window: folder, container | Yes | Same, persisted as JSON; folder picker via `rfd` on the XDG portal | 0 (model), 1E (window) | done | unit `app::ui_tests::the_settings_window_round_trips_to_settings_json` |
| Resolution, frame rate, codec, bitrate settings | Yes | Same settings → record args | 0 (model), 1E (window) | done | unit `settings::tests::record_args_parse_with_the_cli`, snapshot `settings-window.png` |
| Cursor and click settings | Yes | Portal cursor modes | 1A | todo | the settings UI exists (snapshot `settings-window.png`) |
| Audio device and track layout settings | Yes | PipeWire devices | 1B (devices), 1E (UI) | partial | unit `audio_levels_do_not_restart_capture`, snapshot `settings-window.png` |
| Camera device + overlay settings | Yes | V4L2 + same overlay model | 1B/1E | partial | unit `overlay_settings_round_trip_through_the_cli`, snapshot `settings-window.png` |
| Live encode settings (size, bitrate, CBR, keyframe, audio bitrate), locked while streaming | Yes | Same model, locked while a stream runs | 1D (model), 1E (UI) | partial | unit `app::ui_tests::the_live_encode_section_is_locked_while_streaming`, `settings::tests::live_encode_fits_every_enabled_destination` |
| Settings persist in Application Support | `~/Library/Application Support/rec-rs/settings.json` | `$XDG_CONFIG_HOME/rec-rs/settings.json` | 0 | done | unit `settings::tests::settings_live_in_the_xdg_config_dir`, `app::ui_tests::the_settings_window_round_trips_to_settings_json` |
| App menu (Settings ⌘, Hide ⌘H, Quit ⌘Q, Check for Updates, Window menu) | Yes, macOS menu bar | The toolbar's ⋯ menu plus Ctrl+, / Ctrl+W / Ctrl+Q; native window decorations | 1E | done | unit `app::ui_tests::the_app_menu_has_updates_unless_flatpak`; snapshots `main-window.png`, `settings-window.png` |
| Dock icon and Dock menu | NSStatusItem/Dock | n-a: Linux has no Dock; the tray and the app menu carry the same commands | — | n-a | — |
| Closing the window keeps the app running and stops the preview | Yes | Tray-resident app: Ctrl+W or the close button hides the window and drops preview frames | 1E | done | unit `app::preview::tests::only_the_newest_frame_is_kept_and_none_while_hidden`; real-desktop check below |
| Quitting (Dock/menu/logout) finalizes a running recording | Yes | Same: Ctrl+Q, the tray's Quit, SIGTERM and a logout's SIGTERM finalize first | 1E | done | selftest `gui` (SIGTERM mid-recording leaves a probed file) |
| Menu bar item with red dot, elapsed time, full menu | NSStatusItem | StatusNotifierItem (`ksni`) with the same menu, an ARGB icon and the title/tooltip | 1E | partial | unit `app::tray::tests::menu_matches_upstream_in_order`, `titles_match_upstream`, `icon_shows_a_red_dot_while_capturing`; visibility per desktop in the manual list below |
| Global hotkeys ⇧⌘R / ⇧⌘S | Carbon | XDG GlobalShortcuts portal, Ctrl+Shift+R/S defaults, X11 `XGrabKey` fallback, ⇧⌘S only while the replay buffer is on | 1E | partial | unit `hotkey::tests::*` (triggers, backend choice, keycodes, lock masks); the portal/X11 paths need a real desktop (below) |
| Hotkey conflict reported in the UI | Yes | Same: a refused grab, a declined portal dialog or a missing portal shows "Ctrl+Shift+R unavailable" in the window and the tray | 1E | done | unit `hotkey::tests::registration_results_are_queued_once`, `app::tray::tests::menu_matches_upstream_in_order`; selftest `gui` log shows the unavailable path |
| Quit or SIGTERM mid-recording finalizes the file | Yes | SIGINT/SIGTERM/SIGHUP stop and finalize | 0 (CLI), 1E (app) | done | selftest `gui`, `pause-resume`; crash test in the phase report |
| Single instance (a second launch shows the window) | Launch Services | Unix socket in `$XDG_RUNTIME_DIR` (else `/tmp/rec-rs-$UID`); the second launch says what it wants and exits | 1E | done | unit `app::control::tests::the_second_launch_talks_to_the_first`, `a_stale_socket_is_replaced`; selftest `gui` (second launch handed over) |
| Re-launch with responsibility disclaimed (macOS permission prompt attribution) | Yes | n-a: Linux has no TCC; the portal attributes the dialog itself | — | n-a | — |
| "Re-confirm screen recording" prompt | macOS-specific | n-a: the portal's restore token avoids repeated prompts | — | n-a | — |
| "Check for Updates…" | Sparkle | GitHub Releases for `sultanaltair96/rec-rs-linux`; opens the release page when newer; hidden in Flatpak (Flatpak updates the app) | 1E | done | unit `app::updater::tests::*` (parse, numeric compare, offline), `app::ui_tests::the_app_menu_has_updates_unless_flatpak`; selftest has no network check |
| Add the AppImage to the desktop menu | Move to Applications | Writes `~/.local/share/applications/rec-rs.desktop` and a scalable icon, atomically | 1E | done | unit `app::install::tests::*` (entry text, quoting, replace, atomic, stale entry) |
| AppImage/Flatpak self-update | Sparkle replaces the bundle | AppImage: the release check; Flatpak: the software centre | 1E | partial | unit `app::install::tests::spots_flatpak_and_appimage`, `updates_are_flatpak_managed` |

### Needs a real desktop (manual checks, phase 1E)

Everything in the app table above is exercised headlessly: `cargo test` (unit and `egui_kittest`
UI tests, wgpu on Mesa's lavapipe) and `scripts/selftest.sh gui`, which starts the real window on
headless sway, records twice, hands a second launch over, pauses/resumes through the CLI, stops
once with `rec-rs stop` and once with SIGTERM, and probes both files. What needs a panel, a portal
and a file manager:

| Check | Desktop | Expected |
|---|---|---|
| Tray item appears and shows state | GNOME with the AppIndicator/KStatusNotifierItem extension, KDE Plasma | A grey dot when idle, a red dot plus `mm:ss` while recording, the time frozen when paused, " LIVE" while streaming without a recording; the tooltip repeats it in words (some hosts show no title) |
| Tray menu acts | Any SNI host | Start/Stop, Pause/Resume, Save Replay (only while the buffer runs), Go Live/Stop Streaming, Show Rec Window, Settings, Reveal Last Recording (only with a recording), Open Recordings Folder, Check for Updates… (absent in Flatpak), Quit |
| Click the tray item | Any SNI host | Shows (or re-shows) the main window; the preview resumes |
| Close the window while recording | Any desktop | Recording continues; the tray stays; Show Rec Window brings the window back without a gap in the file |
| Global hotkeys, Wayland | KDE Plasma, GNOME 45+ (GlobalShortcuts portal) | The first Ctrl+Shift+R shows the desktop's shortcut dialog; accepting toggles recording. Ctrl+Shift+S is bound only while the replay buffer is on, and released when it is switched off |
| Global hotkeys, X11 | Any X11 session | `XGrabKey`: Ctrl+Shift+R toggles, also with CapsLock or NumLock on; a combination another app owns shows "Ctrl+Shift+R unavailable" in the window and the tray |
| Reveal Last | Nautilus, Dolphin, Thunar | Opens the folder with the file selected (FileManager1 `ShowItems`); without a FileManager1 implementation it opens the folder |
| Folder picker | Any portal desktop | Settings › Choose… opens the desktop's folder dialog and the chosen folder persists |
| Accent colour | Any desktop with the settings portal (KDE, GNOME) | Rec uses the desktop's accent for faders, selection and the switch; Rec blue when the portal does not answer |
| Preview quality and sync | Real display, 1080p60 | The preview shows the same frame as the recording, stays smooth while recording, and is drawn pixel-aligned (crisp text at 2×) |
| Logout mid-recording | GNOME/KDE session | Logging out finalizes the file first (SIGTERM → the quit path), and the file plays |
| Flatpak build | Flatpak sandbox | The tray works without owning a D-Bus name; "Check for Updates…" is absent and the software centre updates the app |

### Window snapshots and the upstream screenshots

`tests/snapshots/main-window.png` (1040×660) and `settings-window.png` (520×820) are rendered by
`egui_kittest` with wgpu on lavapipe; `UPDATE_SNAPSHOTS=1 cargo test` rewrites them and the tests
fail on any pixel difference beyond the tolerance. Compared by layout against
`reference/rec-rs/docs/screenshots/main.png` and `settings.png` (the porting agent could not view
images, so this is a description of the diff from the code and sizes):

- Same: the 40 pt toolbar strip with "Rec" on the left, the 304 pt inspector sidebar on the right
  made of letter-spaced icon-headed sections and rounded cards, the 84 pt bottom strip with the
  round record button, Pause, the monospace timer, two mixer rows (label, mute, meter over fader,
  dB field), Open Folder/Reveal Last on the right, the centred preview with a 2 px red border while
  recording, and Inter Regular/Medium/SemiBold.
- Different: upstream's window is borderless with a hidden title and inset traffic lights; ours
  keeps native decorations, so the 76 pt traffic-light inset is gone and the title bar is the
  desktop's.
- Different: the top-right carries the ⋯ app menu (Settings, Check for Updates, Reveal, Open
  Folder, Quit) because Linux has no menu bar, plus the "Ctrl+Shift+R unavailable" note when the
  hotkey failed; upstream uses Dock/menu-bar items.
- Different: no camera overlay is visible in the main snapshot (the test host has no camera and
  the compositor is phase 1C); the overlay card and shape picker are there.
- Settings: same sections and macOS-style pop-up buttons, sliders and switch; ours adds the
  "Levels" hint row and the live-encode "up to WxH codec at N Mb/s" line. The snapshot is the light
  fallback (headless), while a desktop's appearance selects light or dark.

## Output format guarantees

| Feature | Upstream behaviour | Linux approach | Phase/owner | Status | Evidence |
|---|---|---|---|---|---|
| Video track: H.264 High, HEVC Main (`hvc1`), ProRes 422 | VideoToolbox | GStreamer: x264enc (H.264 High), x265enc (HEVC Main), `avenc_prores_ks` (ProRes 422); VA-API on a GPU host | 0 (software), 1C (hardware) | done | selftest `pattern-1080-h264`, `pattern-hevc`, `pattern-4k-hevc`, `pattern-prores`, `h264-bitrate`; `ffprobe` reports h264/High and hevc/Main |
| BT.709 tagging | Yes | `videoconvert` + encoder default (BT.709 for HD) | 1C (explicit tags) | todo | — |
| Keyframe every 2 s | Yes | `key-int-max`/`gop-size` = 2 fps | 0 | done | writer sets it per encoder; crash test decodes |
| Constant frame rate on an exact 1/fps grid | Yes, measured 16.667 ms | Yes: slot grid, PTS checked | 0 | done | `ffprobe` PTS 0.000/0.016667/0.033333/0.050000 (phase report) |
| Mix track: AAC-LC 48 kHz stereo 256 kb/s, default track | Yes (track 2) | Planned: AAC-LC 48 k stereo 256 kb/s | 1C | todo | — |
| System stem: AAC 48 kHz stereo 256 kb/s | Yes (track 3) | AAC-LC 48 k stereo 256 kb/s, written | 0 | done | selftest tracks count (2: video + system); `ffprobe` audio aac 48000 stereo |
| Mic stem: AAC 48 kHz mono 192 kb/s | Yes (track 4) | Planned with the mic track (mono) | 1B/1C | todo | — |
| Track grouping: alternate group with the mix as default | Yes | Muxer track grouping (`mp4mux` input groups) | 1C | todo | — |
| Single clock, audio never stretched | Yes | Same: CLOCK_MONOTONIC everywhere, resampling only for device rates | 0 | done | selftest `sync` (offsets 0.0 ms) |
| Flash/beep sync 0.0 ms in the file, including across pauses | Yes | 0.0 ms on every beep but the first, +9.9 ms on the first (see note) | 0 | partial | selftest `sync`, `pause-resume`, `audio-gaps` |
| Levels: fader and mute before everything, ≤20 ms ramp, 0 dB untouched | Yes | Same ramp and ordering in the writer | 0 | done | selftest `gain`; unit `fader_glides_and_passes_unity_through` |
| System audio latency shift (output device latency) | CoreAudio | PipeWire sink latency | 1B | todo | offset plumbing exists (`set_audio_offsets`) |
| Webcam lag adaptation (emission waits for the slowest source) | Yes | Constant 50 ms lag in phase 0; adapts when the camera lands | 0 (lag), 1B (camera) | partial | writer `lag()`; frames on the grid (selftest) |
| Crash-safe: fragments every 2 s, regular movie on stop | AVAssetWriter fragments | `mp4mux`/`qtmux` `fragment-duration=2000`; SIGKILL leaves a playable file | 0 | done | crash test: SIGKILL at 4.5 s → 4.0 s file that `ffmpeg -f null` decodes |
| Edge to edge: frame has the source's aspect, no bars, never cropped | Yes | Same `fill`/`same_aspect` maths | 0 | done | unit `compose::tests::fill_is_edge_to_edge_for_the_same_aspect` |
| Window transparent corners filled with the window's edge colour | CoreImage clamp | `compose::corner_inset` + a GPU clamp filter | 0 (geometry), 1C (GPU) | partial | unit `compose::tests::fill_is_edge_to_edge_for_the_same_aspect` (`corner_inset`) |
| Camera settles 2 s before the file starts | Yes | Same 2 s gate | 1B | todo | — |
| Audio quality: 48 kHz capture, windowed-sinc resampler continuous across buffers | Yes | Same resampler maths (ported) | 0 | done | unit `mixer::tests::resamples_seamlessly`, `resampler_removes_content_above_nyquist`, `small_jitter_snaps_to_continuous` |
| Mix limiter keeps the mix under −1 dBFS, no clipping | Yes | Same soft-knee limiter | 0 (maths), 1C (mix track) | partial | unit `mixer::tests::limiting` |
| Pause cuts whole frame slots, audio at the exact sample, no gap, no drift | Yes | Same `Pauses` timeline in the writer | 0 | done | unit `media::pauses::tests::output_slots_stay_contiguous_across_pauses`, `keep_cuts_captures_at_pause_boundaries`; selftest `pause-resume` (300 frames, sync holds) |
| `--duration` exact in output frames, pauses don't count | Yes | Same | 0 | done | selftest `pause-resume`, `sync` |

**AAC priming note.** AVFoundation writes the MP4 edit list that makes decoders skip an AAC
encoder's priming samples; GStreamer's `qtmux`/`mp4mux` 1.26 cannot. The writer therefore drops the
priming frames (`fdkaacenc` 2048 samples, `avenc_aac` 1024) and re-stamps the rest, which removes
the 42.7 ms offset every beep otherwise has. The first kept frame lacks decoder history, so the very
first ~10 ms of audio in a file fades in: measured 0.0 ms on beeps 2..N and +9.9 ms on beep 1.
Phase 1C can improve this once the writer owns the audio path (an `elst` writer or a GStreamer
version that does it).

## Streaming

| Feature | Upstream behaviour | Linux approach | Phase/owner | Status | Evidence |
|---|---|---|---|---|---|
| One shared live encode for replay + every stream | VideoToolbox/AudioConverter | GStreamer encode to appsink, fanned out | 1D | todo | — |
| H.264 High / HEVC Main10, no B-frames, 2 s keyframes, CBR or ABR | Yes | Same via the live encoder | 1D | todo | preset maths: unit `live::tests::platform_presets` |
| Enhanced RTMP HEVC (`hvc1`), fallback to H.264 | Yes | Same negotiation | 1D | todo | — |
| Live size scaled from the recording size | Lanczos | wgpu Lanczos | 1C/1D | todo | — |
| Replay buffer trims a GOP at a time, re-encodes audio, syncs in QuickTime and ffmpeg | Yes | Same on the packet buffer | 1D | todo | — |
| RTMP/RTMPS client with TLS, drop a GOP when behind, reconnect with backoff | Built-in, Security.framework | Built-in port with rustls | 1D | todo | — |
| WHIP (str0m, ICE/DTLS/SRTP, H.264+Opus, DELETE on stop, 5 s silence → reconnect) | Yes | str0m with a Linux crypto backend | 1D | todo | — |
| Platform bitrate/size presets (YouTube/Twitch/Kick), lowest across destinations | Yes | Identical maths (ported) | 0 | done | unit `live::tests::platform_presets`, `shared_preset_fits_every_destination` |
| Stream keys in the OS key store, `REC_STREAM_KEY` override | Keychain | Secret Service, env override | 1D | todo | `resolve_destination` accepts `REC_STREAM_KEY` today |
| Tested against MediaMTX (RTMP, RTMPS, WHIP) | Yes | MediaMTX in the dev container | 1D | todo | — |

## OBS comparison rows rec-rs claims

| Feature | Upstream behaviour | Linux approach | Phase/owner | Status | Evidence |
|---|---|---|---|---|---|
| Display capture | ScreenCaptureKit, zero copy | Portal + PipeWire, dmabuf where available | 1A | todo | — |
| Window capture | Yes | Portal window + X11 fallback | 1A | todo | — |
| Region / crop | `--region` | `--region` | 1A | todo | parses today |
| Application capture | No (known gap) | No | — | n-a | matches upstream |
| Scenes | Three fixed scenes, switchable live, cut only | Same three scenes; the picker enforces the camera rule while recording | 1C/1E | partial | unit `tests::scene_option`, `app::ui_tests::main_window_shows_every_card` |
| Multiple displays/sources at once | No | No | — | n-a | matches upstream |
| System audio | Yes | PipeWire sink monitor | 1B | todo | — |
| Microphone | Yes | PipeWire capture | 1B | todo | — |
| Webcam overlay with shapes/border/shadow/mirror, live while recording | Yes | Same, wgpu | 1C | todo | geometry done: unit `compose::tests::corners_and_free_positions` |
| Separate audio tracks | Up to 4 | Mix + 2 stems | 0 (stems), 1C (mix) | partial | selftest: 2 tracks (video + system stem) |
| Per-source gain and mute, live | Yes | Same ramp/fader | 0 (maths), 1E (widgets) | done | selftest `gain`; unit `fader_glides_and_passes_unity_through`, `app::ui_tests::the_level_can_be_typed`, `a_muted_channel_greys_its_meter` |
| Audio filters (noise suppression, compressor) | No (known gap) | No | — | n-a | matches upstream |
| Hardware H.264/HEVC | VideoToolbox | VA-API on a GPU host; the container has no GPU, so it records with x264enc/x265enc (the writer skips VA without /dev/dri and trial-encodes it otherwise) | 1C | todo | dev/ELEMENTS.md |
| ProRes | 422 | `avenc_prores_ks` 422 | 0 | done | selftest `pattern-prores` |
| 10-bit / HDR | No | No | — | n-a | matches upstream |
| Constant frame rate | Exact grid | Exact grid | 0 | done | selftest + ffprobe PTS |
| Crash-safe files | Fragments every 2 s | Fragments every 2 s | 0 | done | crash test decodes |
| Pause / resume, sample-accurate, app and CLI | Yes | Yes; the app's button and `rec-rs pause`/`resume` hit the same session | 0 (CLI), 1E (app) | done | selftest `pause-resume`, `gui` |
| Replay buffer 15–300 s | Yes | Planned | 1D | todo | — |
| Streaming (RTMP/RTMPS, WHIP, multi-destination, HEVC, reconnect) | Yes | Planned | 1D | todo | preset maths done |
| Screen scaling: GPU Lanczos | Metal MPS | wgpu compute Lanczos | 1C | todo | — |
| Live preview | Yes | CPU texture upload now, the compositor's frame in 1C | 1E | done | unit `app::preview::tests::*`; snapshot `main-window.png` |
| Level meters with peak hold, after the fader | Yes | Same | 1E | done | unit `app::tests::meters_decay_and_hold_peaks`, `app::ui_tests::a_muted_channel_greys_its_meter` |
| Global hotkey | ⇧⌘R / ⇧⌘S | Portal/X11, Ctrl+Shift+R/S | 1E | partial | unit `hotkey::tests::*`; needs a real desktop (below) |
| Menu bar control | StatusItem | StatusNotifierItem | 1E | partial | unit `app::tray::tests::*`; needs a real desktop (below) |
| Headless CLI | Yes | Yes | 0 | done | this phase is CLI-first; selftest runs headless |
| Small footprint | ~22 MB app | Rust + system GStreamer/PipeWire (Flatpak runtime) | 1F | todo | — |
| Encoder input: IOSurface straight to VideoToolbox | Zero copy | dmabuf → VA-API where available, else CPU frames | 1C | todo | — |

## Known gaps upstream documents

| Gap | Upstream | Linux | Phase | Status |
|---|---|---|---|---|
| Scenes are three fixed ones, no layout editor, no transitions, live switch needs a camera start | Yes | Same; the picker disables camera scenes the running recording cannot switch to | 1C/1E | partial |
| No application capture or multi-display capture | Yes | Same | — | n-a |
| Region capture is CLI-only | Yes | Same (the app may add it later) | 1E | todo |
| No audio filters, hotkeys not configurable | Yes | Same | — | n-a |
| Streaming: no SRT/multitrack, same encode for all, live ≤ recording size, changing settings needs a restart | Yes | Same limitations (parity, not a gap to fix) | 1D | todo |
| HEVC to YouTube tested only against a local server | Yes | Same testing honestly reported | 1D | todo |
| 8-bit SDR only | Yes | Same | — | n-a |
| Webcam overlay burned into the video | Yes | Same | 1C | todo |
| Output latency read once at start | Yes | Same | 1B | todo |
| Mix track resamples a non-48 kHz mic linearly per buffer upstream | Yes | Linux: the mix uses the same windowed-sinc resampler as everything else (better) | 1C | todo |
| Re-confirmation prompt for screen recording | macOS-only | n-a: the portal's restore token avoids it | — | n-a |

## Phase 0 summary

Done (proved by tests or self-test cases): the CLI surface, settings and the settings-to-arguments
path, pause timeline and pause/resume end to end, the frame-rate grid, faders/gain ramps and the
resampler and limiter maths, overlay geometry/masks/shadows, the test-pattern source, the H.264 and
ProRes writer with an AAC system track and crash-safe fragments, `probe`, the platform preset maths,
the pidfile/signal control, and the sync/level/self-test scripts.

Partial (a named part is missing, listed per row): mix and mic tracks, BT.709 colour tagging,
webcam lag adaptation, GPU compositing, system-audio latency, hardware encoding.

Known items owned by phase 1C (not fixed here): the first ~10 ms of audio in a file fades in
because the AAC priming frames are dropped instead of trimmed with an MP4 edit list (+9.9 ms on the
first beep, 0.0 ms after), and the self-test's sync threshold is 20 ms rather than upstream's 1 ms
for that reason.

Todo (phase 1): all capture, the GPU compositor, the app, replay/streaming, packaging.

## Phase 1E summary

Done (proved by tests or the self-test): the main window and settings window on egui 0.36/wgpu 30
with the paper-rs look, the live preview path (the session's frames as an egui texture, low-power
mode, nothing buffered while hidden), the mixer widgets (fader scale, drag/click/scroll/double
click/typed dB, mute greying the meter), the settings window and its `settings.json` round trip,
the app menu and its shortcuts, the tray model (menu, title, tooltip, ARGB icon), the single-instance
socket, the pidfile/signal controls against the running app, quitting that finalizes a recording,
"Check for Updates" against GitHub Releases, AppImage self-integration, and the headless self-test
case that runs the real app.

Partial (a named part is missing, listed per row): the preview shows the captured frame rather than
the composited one until 1C lands; overlay restyling, scene switching at run time and the camera
need 1B/1C; the replay buffer and streaming are 1D (the cards and their gating are here); "Reveal
Last" selects the file only where FileManager1 exists; the tray and the global hotkeys are complete
in code but their visibility and the portal dialog need a real desktop (manual list above).

Known gaps: SNI hosts disagree about title text (the tooltip carries the state everywhere); the
X11 fallback cannot be exercised under a Wayland-only session and vice versa; the update check
opens the release page rather than replacing the binary (Flatpak and AppImage packaging own that,
phase 1F). No audio filters and no configurable hotkeys, matching upstream.
