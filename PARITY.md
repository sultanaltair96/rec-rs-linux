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
| `record` command | Records screen + system audio + mic + webcam | Same, through PipeWire/V4L2 sources and the GStreamer writer | 0 (CLI), 1A/1B (sources) | partial | unit `tests::goal_command_line_parses`; selftest `pattern-1080-h264` (test-pattern), `pattern-mic` and `pattern-system-audio` (real PipeWire audio); the camera overlay is wired by 1G |
| `-o/--out/--output` | Output file, `.mov` or `.mp4`, default `~/Movies/rec-<ts>.mov` | Same flags; default `$XDG_VIDEOS_DIR`/`~/Videos/rec-<ts>.mov` | 0 | done | unit `tests::default_output_is_under_the_videos_dir`, `util::tests::timestamps_are_safe_file_names` |
| `-d/--display N` | Display number from `devices`, default 1 | 1-based among RandR monitors on X11 (1 = primary, even when the server names none). On Wayland the portal's picker decides and a note says so; the picked display is named in the log when XWayland's monitor list identifies it | 1A | partial | unit `x11::tests::a_monitor_and_a_window_are_enumerated`; selftest `screen-x11` (`devices` lists `[1] id screen`); why not exact on Wayland: see the phase 1A notes |
| `--window W` | Window id or app/title substring | X11: EWMH `_NET_CLIENT_LIST` + `_NET_WM_NAME`/`WM_CLASS`, id or case-insensitive app/title substring, largest match wins (upstream's rule), captured with `GetImage`. Wayland: the portal's window source where the desktop offers one (xdg-desktop-portal-wlr offers none and says so); substring matching is a hint for the picker | 1A | partial | unit `selector::tests::*`, `x11::tests::a_window_is_captured_with_its_own_pixels`; selftest `screen-window` (400x300 client surface, 117 distinct frames) |
| `--region X,Y,W,H` | Crop a display region (points) | Same, in logical units: mapped to stream pixels with the display's scale, cropped on the CPU frame, even and inside the stream | 1A | done | unit `format::tests::a_region_scales_with_the_display`, `regions_are_even_and_stay_inside`; selftest `screen-region` (640x480 native pixels out of a 1280x720 stream) |
| `-r/--resolution` | 720p/1080p/1440p/4k/native/WxH, source aspect, never upscaled | Identical maths (ported) | 0 | done | unit `tests::sizes` (incl. portrait, odd sizes, 16:10 examples) |
| `-f/--fps` | 24/25/30/50/60/120 (divisors of 60000) | Same validation and grid | 0 | done | unit `tests::bad_arguments_are_rejected`; selftest CFR check |
| `--codec` | auto (H.264 ≤1080p, HEVC above), h264, hevc, prores | Same selection and same automatic rule | 0 | done | selftest `pattern-prores`, `pattern-hevc`, `pattern-4k-hevc` (`-r 4k` picks HEVC by itself) |
| `--bitrate` | Mb/s, else bits/pixel heuristic | Same heuristic (ported); x264enc/x265enc honour it | 0 | done | unit `tests::test_pattern_prepares_without_a_display` (4K ≥ 30 Mb/s); selftest `h264-bitrate`: 8 Mb/s requested → 8.10 Mb/s at profile High |
| `--system-audio` / `--no-system-audio` | On by default, flags override each other | Same | 0 | done | unit `tests::system_audio_flags_override_each_other`; selftest `gain` (system track) |
| `--mic DEVICE` / `--no-mic` | Default mic, id or name substring | PipeWire `Audio/Source` node: the session default, an exact node name, a saved/argument display name (exact then substring), or the same USB device on another port. The default mic warns and continues; an explicit missing device fails | 1B | done | selftest `pattern-mic` (default mic, `--mic 'Rec-Test'`, missing device); unit `capture::audio::tests::mic_matching`; script `scripts/audio/integration.sh` |
| `-w/--cam/--webcam [DEVICE]` | Webcam overlay, default or device | V4L2 (`/dev/video*`), or the xdg-desktop-portal Camera inside Flatpak; id/number/name substring, `default` = first camera | 1B | partial | source: unit `camera::v4l2::tests::device_matching`, `camera::format::tests::*`, `camera::convert::tests::*`; real hardware and the overlay wiring are listed under "Needs a real desktop" (source done, 1G wires it) |
| `--scene` | screen, screen-cam, cam | Same enum and default rule | 0 | done | unit `tests::scene_option` |
| `--pip-size/-corner/-shape/-roundness/-border/-border-color/-shadow/-pos/-no-mirror` | Overlay geometry and style | Same parsing, clamping and geometry | 0 | done | unit `tests::overlay_options`, `compose::tests::corners_and_free_positions`, `snapping`, `shape_coverage` |
| `--audio-tracks both/mix/stems` | Mix + stems, mix only, stems only | Same values and track layout; one track when only one source is present | 1G | done | selftest `tracks` (4 tracks: video, mix, system, mic), `audio-tracks` (mix: 2 tracks; stems: video + 2 enabled stems) |
| `--system-gain`, `--mic-gain` | dB per source, −60 = silence, +24 max, in every track | Same fader maths applied in the writer | 0 | done | selftest `gain` (`--ref=-6 --expect=-12`); unit `mixer::tests::volume_gain`, `fader_glides_and_passes_unity_through` |
| `--system-audio-offset-ms`, `--mic-offset-ms` | Sync shifts; system default = output latency | Same; the default sink's latency is read once at start from PipeWire and passed to the writer as the system offset | 0 (shift), 1B (latency) | done | unit `capture::audio::pipewire::tests::collector_combines_the_latency_parts`, `node_latency_property_is_a_fraction_or_frames`; integration `devices` prints 5.3 ms for a 256/48000 sink; unit (offsets applied in `push_audio`); selftest `sync` |
| `--no-cursor`, `--clicks` | Hide cursor / highlight clicks | Portal cursor modes: embedded (drawn by the compositor), hidden, metadata (we composite the bitmap). X11: the cursor is composited from XFixes and clicks are the button bits of `QueryPointer`, drawn as a ring that grows and fades | 1A | partial | unit `cursor::tests::a_ring_is_a_ring_not_a_disc`, `ring_grows_fades_and_expires`, `cursor_is_composited_at_its_hotspot`; `x11::tests::a_click_draws_a_ring_at_the_pointer` (real X server, XTEST click). Wayland: no button events through a portal, so `--clicks` warns and records the cursor only |
| `-t/--duration` | Exact, counted in output frames; pauses don't count | Identical: grid slots + `reached_limit` | 0 | done | selftest `sync` (300 frames for `-t 5`), selftest `pause-resume` (300 frames with a 2 s pause) |
| `--mic-backend` | `sck` or `avcapture` (macOS) | `pipewire` (default) or `pulse`; `sck`/`avcapture` map with a message | 0 | done | unit `record::tests::mic_backend_maps_upstream_values` |
| `--test-pattern [--noise]` | Flash + beep source for self-tests | Same, CPU frames, BGRA/I420 | 0 | done | unit `synth::tests::pattern_timeline`, `beep_starts_on_each_second`, `source_emits_on_the_grid`; selftest `pattern-1080-h264` |
| `--replay SECS` | 15–300 s replay buffer | Packet ring of the live encode, trimmed a GOP at a time, memory reported, saved by `save-replay` | 1D | done | selftest `replay` (20 s run, 15 s window, clip 16.0 s, keyframe start, +5.7 ms A/V); unit `live::replay::tests::keeps_the_window_from_a_keyframe`, `live::tests::replay_saves_a_playable_mp4` |
| `--stream DEST` (repeatable) | RTMP(S)/WHIP, platform shortcuts | Same URLs and presets, built-in clients; one encode feeds all of them | 1D | done | unit `live::tests::services_expand_and_are_recognized`, `shared_preset_fits_every_destination`; selftest `stream-rtmp` (2 RTMP + 1 RTMPS from one encode) |
| `--no-record` | Replay/stream only | Same rule (needs replay or stream); the writer runs without a file and still taps the live encode | 1D | done | unit `tests::bad_arguments_are_rejected`; selftest `live-e2e` (`--no-record --stream` streamed with no file written) |
| `--live-resolution/--stream-bitrate/--stream-codec/--keyframe/--vbr/--audio-bitrate` | Live encode controls | Same parsing, presets and clamping, applied by the live encoder (videoscale, key-int-max, CBR or ABR) | 0 (parse/config), 1D (encode) | done | unit `settings::tests::live_encode_fits_every_enabled_destination`, `live::tests::encode_size_fits_the_box`; selftest `stream-rtmp` (720p30 from one encode) |
| `devices` command | Lists displays, windows, cameras, mics + output latency | Same layout, line for line (no extra columns: `  [1] id HEADLESS-1  1280x720 pixels (1280x720 points)`). Displays and windows come from RandR/EWMH (also under XWayland); a Wayland session without X11 prints that the system picker chooses instead of an empty list. Cameras are V4L2 capture nodes (path + card name), microphones PipeWire sources (node name + description). The output-latency line is printed only when a sink reports a latency, like upstream (G11) | 0 (shape), 1A/1B | partial | selftest `screen-x11` for displays; integration `devices` matches `Rec-Test-Mic [rec_test_mic]` and `Output latency ... 5.3 ms`, and `no-pipewire` asserts no latency line when nothing reports one |
| `probe FILE` command | AVFoundation: playable, tracks, codecs, full sample decode, non-zero exit on error | GStreamer: qtdemux structure + `uridecodebin` decode of every sample | 0 | done | selftest uses it on every case (`.probe` files); `ffmpeg -v error -f null` cross-check |
| `permissions [--request]` | TCC status and prompts | ScreenCast version, source types, cursor modes, whether a grant is stored, whether PipeWire runs. `--request` forgets the stored grant first (`tcc.rs:183-230` clears the decision so the system asks again), then runs the portal dialog and stores the new restore token (0600, `$XDG_STATE_HOME/rec-rs/portal-token`). The microphone line reports PipeWire and the number of inputs; the camera line reports `/dev/video*` or the portal | 1A/1B | partial | unit `portal::tests::cursor_modes_fall_back_to_what_the_portal_offers`; selftest `screen-token` (two requests: the harness chooser log proves the second picker really ran, and stderr says `forgetting the saved screen-capture grant`); integration `no-pipewire`; container report shows the mic/camera lines |
| `stop` / `pause` / `resume` | pidfile + SIGUSR1/SIGUSR2/SIGTERM | **Socket first**: `rec-rs-control.sock` in the shared `$XDG_RUNTIME_DIR` (0600) is answered by the app *and* a CLI `rec-rs record`; this is what reaches the recorder from a second **Flatpak instance**, whose PID namespace makes the pidfile's pid meaningless (the audit of the packaging found the pid hitting the wrong process). The pidfile + SIGUSR1/SIGUSR2/SIGINT stay as the fallback when no socket listens; a second recorder refuses to start while one is live | 0 (pidfile/signals), 1E (socket) | done | unit `app::control::tests::*` (protocol round-trip, handled/unknown replies, stale socket, 0600, second listener refused); selftest `pause-resume` and `timeout-stop` run with a **foreign pid** in the pidfile (socket path), `timeout-stop-signal` proves the SIGINT fallback, `timeout-stop` proves a second recorder refuses |
| `save-replay` | SIGINFO writes the buffer to MP4 | Socket first (`save-replay` command, so it works from another Flatpak instance), then `SIGRTMIN+1` (no SIGINFO on Linux) as the fallback; the clip is written on a background thread | 0 (signal), 1E (socket), 1D (buffer) | done | selftest `replay` (`rec-rs save-replay` next to a running encode wrote and verified the clip), `pause-resume`/`timeout-stop` (socket with a foreign pid), `timeout-stop-signal` (signal fallback); unit `record::tests::save_replay_signal_is_a_real_time_signal`, `app::control::tests::a_recorder_answers_only_the_commands_it_handles` |
| `stream-key DEST [--delete]` | Keychain | Secret Service (the `keyring` crate), keyed per destination, read with echo off; `REC_STREAM_KEY` overrides for one run and nothing ever goes to a file | 1D | done | unit `live::keyring::tests::round_trip`, `secret_service_round_trip` (real service when the session has one); manual check below for a desktop keyring |
| `gui [--start-recording] [--settings] [--window ID]` | The app | egui/wgpu app | 1E | todo | command exists and fails with phase 1E |

## Main window and app behaviour

| Feature | Upstream behaviour | Linux approach | Phase/owner | Status | Evidence |
|---|---|---|---|---|---|
| Live preview of what will be recorded | Rendered at preview size, composited | `Options::preview` is the writer's **composited** frame (≤30/s), uploaded as its planes (BGRA/RGBA, NV12 or I420) and converted, scaled and drawn by `preview.wgsl` in egui's wgpu pass: the CPU only copies the planes into mapped staging memory. One hardware-texture upload would remove even that (1C's `Renderer::texture()`, phase 2) | 1E | done | unit `app::preview::tests::*` (size, BGRA, resample, NV12/BT.709, newest-only); selftest `gui` (screen + synthetic camera) logs `preview frame 704x396`; `app::ui_tests::the_composited_preview_moves_the_overlay_after_set_pip` (the camera is bottom-right, `set_pip` puts it top-left in the next preview frame); `app::ui_tests::dragging_the_overlay_snaps_to_a_corner`; `preview_cost` (release): 1.0–1.2 ms per 1080p NV12 frame and 2.0–2.2 ms per 4K NV12 frame, against 6.6/8.15 ms with the old CPU converter; `preview_gpu::tests::the_shader_matches_the_cpu_reference` (the shader's BT.709 output equals the CPU reference per channel), `every_format_uploads`; snapshot `tests/snapshots/main-window.png` |
| Own windows excluded from capture | ScreenCaptureKit filter | X11: the app hands its own top-level window ids to `capture::screen::set_excluded_windows` before every capture, and their visible parts are painted out of each frame (clipped by whatever is stacked above, filled with the desktop colour around them). A portal stream cannot exclude anything: the capture logs a note and the app hides its own window while a display/region recording runs (setting `hide_window_when_recording`, on by default) with a desktop notification, restoring it on stop; a single-window capture cannot contain it | 1A (X11 exclusion), 1E (portal hide/restore) | done | unit `x11::tests::excluded_windows_are_painted_out_of_the_frame`, `subtracting_a_rectangle_covers_the_possible_shapes`, `excluded_windows_round_trip` (1A); unit `app::tests::the_window_is_hidden_only_for_a_display_recording`, `app::ui_tests::the_window_ids_are_handed_to_the_x11_exclusion` (1E); selftest `gui` (portal path: hides, records, restores) |
| Preview low-power mode (half res, 15 fps) | Setting | Same setting, honoured by the preview | 1E | todo | — |
| Scene: Screen | Full display or window | Same, scaled to the output with Lanczos | 1C (composite), 1A (capture) | partial | unit `gpu::tests::a_screen_at_the_output_size_is_bit_exact`, `a_small_screen_is_not_upscaled`, `lanczos_matches_a_cpu_reference`; golden `screen`, `screen-4k-to-1080` |
| Scene: Screen + Cam, drag anywhere, corner snap | Overlay placement | Same geometry, drawn by wgpu; the preview drag uses `compose::overlay_rect`/`snap_corner` and the session seam | 0 (geometry), 1C (GPU), 1E (drag) | done | unit `compose::tests::snapping`, `corners_and_free_positions`; `gpu::tests::renders_overlay_with_border_into_the_frame`, `placement_follows_the_geometry_for_corners_and_free_positions`, `mirroring_flips_the_camera`; goldens `screen-cam-rounded`, `screen-cam-pill`; drag: unit `app::ui_tests::main_window_shows_every_card` |
| Overlay shapes (circle, rounded, square, pill, squircle), roundness, size, border, shadow, mirror | CoreImage | Same masks and shadows computed on the CPU, drawn by wgpu; every control is in the sidebar and the settings window | 0 (masks), 1C (GPU), 1E (controls) | done | unit `gpu::tests::every_shape_matches_the_mask_geometry`, `renders_overlay_with_border_into_the_frame`; goldens `screen-cam-rounded`, `screen-cam-circle`, `screen-cam-square-border`, `screen-cam-pill`, `screen-cam-squircle-shadow`; snapshot `main-window.png` (overlay card) |
| Overlay changes live while recording, baked in, no extra encode | Yes | The compositor restyles from the next frame; the app sends the spec on every drag step and slider move (`Session::set_pip`), and the drag snaps on release | 1C (compositor), 1G (session), 1E (drag) | done | `gpu::tests::scenes_and_camera_changes_apply_from_the_next_frame`; `app::ui_tests::the_composited_preview_moves_the_overlay_after_set_pip`, `dragging_the_overlay_snaps_to_a_corner` |
| Scene: Camera full frame | AVCapture | V4L2 into a cover-fitted, mirrored full frame | 1B (source), 1C (composite) | partial | unit `gpu::tests::the_camera_scene_covers_the_frame`, `mirroring_flips_the_camera`; golden `cam` |
| Switch scenes live if recording started with the camera | Yes | The compositor switches from the next frame; the app offers camera scenes while recording only when the session has a camera, sends `set_scene` and shows a notice when the writer refuses | 1C/1E/1G | done | unit `gpu::tests::scenes_and_camera_changes_apply_from_the_next_frame`, `app::ui_tests::main_window_shows_every_card`; `State::can_switch_live` |
| Sources: display, window, camera, mic, system audio | Yes | PipeWire display/window, V4L2 or portal camera, PipeWire audio; the sidebar lists them and passes the choice through `settings`. Hotplug: 1B's `watch_devices` (PipeWire registry / `/dev/video*` inotify) wake the event loop, which reloads the lists and restarts the preview when the saved mic resolves to another device (upstream's `devices_changed`) | 1A/1B (sources), 1E (UI, hotplug) | done (sources), done (hotplug watch), a real plug on the manual list | selftest `pattern-screen`, `pattern-mic`, `pattern-system-audio`; unit `app::tests::hotplug_notices_a_changed_mic_and_device_list`, `capture::*::tests::watch_*`, `app::ui_tests::main_window_shows_every_card` |
| Record/Stop button and timer | Yes | App | 1E | todo | — |
| Pause/Resume button | Yes | App (the session API exists) | 1E | todo | session API: selftest `pause-resume` |
| Audio mixer: meter with peak hold, fader, mute, dB | Yes, after the fader | Same widgets on the same session API | 1E | todo | levels in the status line; unit `mixer::tests::volume_gain` |
| Fader range −∞..+24 dB, drag/click/scroll/double-click/type | Yes | Same | 1E | todo | unit `mixer::tests::volume_gain` (range and silence) |
| Level changes apply live to every track (and replay/streams) | Yes, ≤20 ms ramp | Writer follows live (`set_volumes`); ≤20 ms envelope | 0 (ramp), 1E (widget) | partial | unit `fader_glides_and_passes_unity_through` |
| Reveal last / open folder | Yes | `xdg-open` on the output folder | 1E | todo | — |
| Replay buffer 15–300 s with memory use, Save Replay | Yes | Same ring and save, memory reported in the status line | 1D | done | selftest `replay`; unit `live::tests::replay_saves_a_playable_mp4` (held 15–17 s, H.264+AAC, both tracks 14–17 s) |
| Stream to YouTube/Twitch/Kick + custom servers, Go Live all-at-once, all-or-nothing | Yes | Same destinations and presets, built-in RTMP/RTMPS/WHIP; a destination that cannot start stops the ones before it | 1D | done | unit `live::tests::shared_preset_fits_every_destination`, `live::tests::outputs_start_all_or_nothing`; selftest `stream-rtmp` (four destinations from one encode). The three services' RTMPS URLs and presets are unit-tested; streaming to the real services was not done (see the known gaps) |
| Per-destination live status (duration, bitrate, drops, reconnect) and LIVE 2/3 | Yes | `live::OutputStatus` per destination; the CLI status line shows state, bitrate and drops, and the app's stream cards read the same list through `app::session::statuses` (id, live, state, error, duration, bitrate, drops, reconnects) | 1D | done (CLI and app seam) | selftest `stream-rtmp` (`4/4 destinations live at once`), `stream-whip`, `live-e2e`; unit `app::session::tests::the_seam_drives_the_live_encode` |
| One encode for every destination, within all limits | Yes | Same `shared_preset` logic; one GStreamer encode fans out to the ring and every output | 0 (choice), 1D (encode) | done | unit `shared_preset_fits_every_destination`, `live::tests::short_gaps_are_filled`; selftest `stream-rtmp` (two RTMP paths, RTMPS and WHIP from one encode) |
| Replay/stream run with or without a recording, survive recording start/stop, gaps bridged | Yes | Same design: the live encode owns its queues and encoder and knows nothing of the writer; short holes get a repeated frame and silence (up to 3 s), longer ones restart the timeline | 1D | done | selftest `live-e2e`: streams and the replay buffer keep running across a pause/resume of the recording, `--no-record` streams with no file, and the clips are saved while recording; unit `live::tests::short_gaps_are_filled`, `silence_stands_in_for_missing_audio` |
| Settings window: folder, container | Yes | Same, persisted as JSON; folder picker via `rfd` on the XDG portal | 0 (model), 1E (window) | done | unit `app::ui_tests::the_settings_window_round_trips_to_settings_json` |
| Resolution, frame rate, codec, bitrate settings | Yes | Same settings → record args | 0 (model), 1E (window) | done | unit `settings::tests::record_args_parse_with_the_cli`, snapshot `settings-window.png` |
| Cursor and click settings | Yes | Portal cursor modes | 1A | todo | the settings UI exists (snapshot `settings-window.png`) |
| Audio device and track layout settings | Yes | PipeWire devices; `microphones()` (default mic first) feeds the sidebar's list | 1B (devices), 1E (UI) | partial | unit `audio_levels_do_not_restart_capture`, `capture::audio::tests::mic_matching`; snapshot `settings-window.png`; the mixer list is exercised by `app::ui_tests::main_window_shows_every_card` |
| Camera device + overlay settings | Yes | V4L2/portal camera selection + the same overlay model | 1B (device), 1C (overlay), 1E (UI) | partial | unit `overlay_settings_round_trip_through_the_cli`, `camera::v4l2::tests::device_matching`; snapshot `settings-window.png`; real hardware under the manual list |
| Live encode settings (size, bitrate, CBR, keyframe, audio bitrate), locked while streaming | Yes | Same model: `LiveConfig` (re)builds the encoders with a keyframe and restarts the replay ring | 1D (model), 1E (UI lock) | partial | unit `settings::tests::live_encode_fits_every_enabled_destination`, `live::tests::encode_size_fits_the_box`; the app's lock is 1E |
| Settings persist in Application Support | `~/Library/Application Support/rec-rs/settings.json` | `$XDG_CONFIG_HOME/rec-rs/settings.json` | 0 | done | unit `settings::tests::settings_live_in_the_xdg_config_dir`, `app::ui_tests::the_settings_window_round_trips_to_settings_json` |
| App menu (Settings ⌘, Hide ⌘H, Quit ⌘Q, Check for Updates, Window menu) | Yes, macOS menu bar | The toolbar's ⋯ menu plus Ctrl+, / Ctrl+W / Ctrl+Q; native window decorations | 1E | done | unit `app::ui_tests::the_app_menu_has_updates_unless_flatpak`; snapshots `main-window.png`, `settings-window.png` |
| Dock icon and Dock menu | NSStatusItem/Dock | n-a: Linux has no Dock; the tray and the app menu carry the same commands | — | n-a | — |
| Closing the window keeps the app running and stops the preview | Yes | Tray-resident app: Ctrl+W or the close button hides the window and drops preview frames | 1E | done | unit `app::preview::tests::only_the_newest_frame_is_kept_and_none_while_hidden`; real-desktop check below |
| Quitting (Dock/menu/logout) finalizes a running recording | Yes | Same: Ctrl+Q, the tray's Quit, SIGTERM and a logout's SIGTERM finalize first | 1E | done | selftest `gui` (SIGTERM mid-recording leaves a probed file and the app exits 0) |
| Menu bar item with red dot, elapsed time, full menu | NSStatusItem | StatusNotifierItem (`ksni`) with the same menu, an ARGB icon and the title/tooltip | 1E | partial | unit `app::tray::tests::menu_matches_upstream_in_order`, `titles_match_upstream`, `icon_shows_a_red_dot_while_capturing`; visibility per desktop in the manual list below |
| Global hotkeys ⇧⌘R / ⇧⌘S | Carbon | XDG GlobalShortcuts portal, Ctrl+Shift+R/S defaults, X11 `XGrabKey` fallback, ⇧⌘S only while the replay buffer is on | 1E | partial | unit `hotkey::tests::*` (triggers, backend choice, keycodes, lock masks); the portal/X11 paths need a real desktop (below) |
| Hotkey conflict reported in the UI | Yes | Same: a refused grab, a declined portal dialog or a missing portal shows "Ctrl+Shift+R unavailable" in the window and the tray | 1E | done | unit `hotkey::tests::registration_results_are_queued_once`, `app::tray::tests::menu_matches_upstream_in_order`; selftest `gui` log shows the unavailable path |
| Quit or SIGTERM mid-recording finalizes the file | Yes | SIGINT/SIGTERM/SIGHUP stop and finalize | 0 (CLI), 1E (app) | done | selftest `gui`, `pause-resume`; crash test in the phase report |
| Single instance (a second launch shows the window) | Launch Services | Unix socket in `$XDG_RUNTIME_DIR` (else `/tmp/rec-rs-$UID`); the second launch says what it wants and exits | 1E | done | unit `app::control::tests::the_second_launch_talks_to_the_first`, `a_stale_socket_is_replaced`; selftest `gui` (second launch handed over) |
| Dock icon, app menu (Settings ⌘,, Hide ⌘H, Quit ⌘Q, Window menu) | Yes | Native window menu + `.desktop` actions | 1E/1F | todo | — |
| Re-launch with responsibility disclaimed (macOS permission prompt attribution) | Yes | n-a: Linux has no TCC; the portal attributes the dialog itself | — | n-a | — |
| "Re-confirm screen recording" prompt | macOS-specific | n-a: the portal's restore token avoids repeated prompts | — | n-a | — |
| "Check for Updates…" | Sparkle | GitHub Releases for `sultanaltair96/rec-rs-linux`; opens the release page when newer; hidden in Flatpak (Flatpak updates the app) | 1E | done | unit `app::updater::tests::*` (parse, numeric compare, offline), `app::ui_tests::the_app_menu_has_updates_unless_flatpak`; selftest has no network check |
| Add the AppImage to the desktop menu | Move to Applications | Writes `~/.local/share/applications/rec-rs.desktop` and a scalable icon, atomically | 1E | done | unit `app::install::tests::*` (entry text, quoting, replace, atomic, stale entry) |
| AppImage/Flatpak self-update | Sparkle replaces the bundle | AppImage: the release check; Flatpak: the software centre | 1E | partial | unit `app::install::tests::spots_flatpak_and_appimage`, `updates_are_flatpak_managed` |

### The 19 upstream app tests on Linux

Upstream's `src/app` carries 19 tests. What happened to each (the pure-logic ones keep their
assertions; the rest say why not):

| Upstream test | Linux |
|---|---|
| widgets: `fader_scale` | ported unchanged |
| widgets: `fader_follows_the_pointer` | ported unchanged (same synthetic pointer events) |
| widgets: `fader_scrolls_in_fine_steps` | ported unchanged |
| widgets: `disabled_fader_ignores_input` | ported unchanged |
| widgets: `mute_button_toggles` | ported unchanged (the button also gets an accessible label now) |
| preview: `preview_size_follows_the_area_in_pixels` | ported unchanged; low power halves it |
| preview: `preview_texture_is_not_srgb` | n-a: there is no Metal texture format to check; the Linux equivalent is `app::preview::tests::frames_are_shown_byte_for_byte_without_a_second_scale` (egui textures are gamma-encoded, values are not converted) |
| preview: `preview_pixels` (`#[ignore]`, CoreImage/Metal harness) | n-a: replaced by `Tap::image` unit tests (BGRA byte-exact, bilinear resample, NV12/I420 → BT.709 RGB) |
| mod: `missing_permissions_banner_points_at_its_button` | n-a: Linux has no TCC and no Request Permissions button; replaced by `app::tests::the_preview_banner_names_the_phase` |
| ui: `levels_as_text_and_typed` | ported unchanged, plus the `-∞`/`off` cases |
| install: `installs_a_copy_without_quarantine` | n-a: no quarantine flag; replaced by `installs_the_desktop_entry_and_icon` |
| install: `replaces_an_older_copy` | ported as `replaces_an_older_entry` (the new AppImage's `Exec` replaces the old) |
| install: `spots_read_only_and_translocated_apps` | n-a: App Translocation is macOS-only; replaced by `spots_flatpak_and_appimage` |
| install: `plans_by_where_the_download_really_is` | n-a: no move to Applications; replaced by `updates_are_flatpak_managed` |
| install: `compares_real_paths` | ported as `compares_real_paths` (a symlinked destination folder) |
| install: `never_leaves_the_destination_missing` | ported as `never_leaves_the_destination_missing` (the desktop entry is replaced by an atomic rename) |
| install: `stages_apart_from_other_copies` | ported as `staging_is_apart_from_other_copies` |
| install: `reopens_a_new_instance_with_the_same_options` | n-a: the app never moves itself, so it never reopens; the single-instance handover is covered by `app::control::tests::the_second_launch_talks_to_the_first` and `desktop_entry_starts_this_appimage` |
| install: `original_of_a_translocated_app` (`#[ignore]`, real macOS bundle) | n-a: no translocated bundle exists on Linux |

New app tests beyond those: `app::theme` (3), `app::preview` (6), `app::control` (4), `app::tray`
(3), `app::install` (10), `app::updater` (3), `app::linux_desktop` (2), `app::session` (4),
`app::mod` (4), `app::ui` (3), `hotkey` (5), and the 11 `egui_kittest` UI tests. Those 11 create a wgpu device each; they hold a test-wide
mutex because concurrent device creation on Mesa's lavapipe crashed the test binary every few runs
(the whole suite still takes about 3 s).

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
| Screen lock stops the preview | GNOME, KDE | Locking the session stops preview frames (`REC_DEBUG=1` logs no new frames) and the app's CPU drops; unlocking resumes. The recording, if any, keeps running |
| Hotplug | Any desktop | Plugging a USB mic or camera makes it appear in the sidebar within ~2 s without ↻; a preview using the saved mic restarts on the new device |
| Own window while recording | X11 and Wayland | Starting a display recording hides Rec's window (a notification says so, the tray reopens it) and the file does not contain it; stopping restores it |
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
- Different: no camera overlay is visible in the main snapshot (the test host has no camera); the
  overlay card and shape picker are there.
- Settings: same sections and macOS-style pop-up buttons, sliders and switch; ours adds the
  "Levels" hint row and the live-encode "up to WxH codec at N Mb/s" line. The snapshot is the light
  fallback (headless), while a desktop's appearance selects light or dark.

## Output format guarantees

| Feature | Upstream behaviour | Linux approach | Phase/owner | Status | Evidence |
|---|---|---|---|---|---|
| Video track: H.264 High, HEVC Main (`hvc1`), ProRes 422 | VideoToolbox | GStreamer: x264enc (H.264 High), x265enc (HEVC Main), `avenc_prores_ks` (ProRes 422); VA-API on a GPU host | 0 (software), 1C (hardware) | done | selftest `pattern-1080-h264`, `pattern-hevc`, `pattern-4k-hevc`, `pattern-prores`, `h264-bitrate`; `ffprobe` reports h264/High and hevc/Main |
| BT.709 tagging | Yes | The compositor writes BT.709 limited range NV12/I420 (Y 16..235, Cb/Cr 16..240); for CPU frames the encoder capsfilter pins primaries/transfer/matrix BT.709 and limited range | 1C (conversion), 1G (caps) | done | unit `gpu::tests::the_output_is_bt709_limited_range`, `gpu::yuv::tests::known_bars`; selftest `pattern-1080-h264`, `pattern-hevc`, `pattern-4k-hevc`, `pattern-prores`: ffprobe reports `bt709/bt709/bt709` and `color_range=tv` for all three codecs |
| Keyframe every 2 s | Yes | `key-int-max`/`gop-size` = 2 fps | 0 | done | writer sets it per encoder; crash test decodes |
| Constant frame rate on an exact 1/fps grid | Yes, measured 16.667 ms | Yes: slot grid, PTS checked | 0 | done | `ffprobe` PTS 0.000/0.016667/0.033333/0.050000 (phase report) |
| Mix track: AAC-LC 48 kHz stereo 256 kb/s, default track | Yes (track 2) | AAC-LC 48 kHz stereo 256 kb/s from the mix bus, the enabled track of the audio alternate group | 1G | done | selftest `tracks`: ffprobe `aac, 48000 Hz, 2 ch, 255999 b/s` at index 1, probe `enabled alt-group 1` |
| System stem: AAC 48 kHz stereo 256 kb/s | Yes (track 3) | AAC-LC 48 kHz stereo 256 kb/s, its own encoder | 0 (track), 1G (faders/mix) | done | selftest `tracks`: index 2, disabled alternative in group 1, system beep at −12 dB with `--system-gain=-12` |
| Mic stem: AAC 48 kHz mono 192 kb/s | Yes (track 4) | AAC-LC 48 kHz mono 192 kb/s (the device's own sample per frame) | 1B (capture), 1G (track) | done | selftest `tracks`: index 3, `1 ch`, `192000 b/s`, disabled alternative; the synthetic 1.5 kHz beep is the proof it carries the mic and not the system source |
| Track grouping: alternate group with the mix as default | Yes | The finalise writes `tkhd` alternate_group and enabled flags into the moov (qtmux cannot) | 1G | done | unit `mp4::tests::finalize_builds_a_regular_movie_and_keeps_the_media_bytes`; selftest `tracks`: probe reports `alt-group 1`, mix `enabled`, both stems `disabled`; `audio-tracks stems` has no group and all tracks enabled |
| Single clock, audio never stretched | Yes | Same: CLOCK_MONOTONIC everywhere, resampling only for device rates | 0 | done | selftest `sync` (offsets 0.0 ms) |
| Flash/beep sync 0.0 ms in the file, including across pauses | Yes | 0.0 ms on every beep, the first included (the AAC priming is skipped by an edit list) | 1G | done | selftest `sync` (`--max-ms 1`), `pause-resume`, `audio-gaps`: mean +0.0 ms, range [+0.0, +0.0] |
| Levels: fader and mute before everything, ≤20 ms ramp, 0 dB untouched | Yes | Same ramp and ordering in the writer | 0 | done | selftest `gain`; unit `fader_glides_and_passes_unity_through` |
| System audio latency shift (output device latency) | CoreAudio | PipeWire default sink's `Latency`/`ProcessLatency`, else `node.latency`, read once at start | 1B | done | unit `collector_combines_the_latency_parts`, `node_latency_property_is_a_fraction_or_frames`; integration `devices` prints 5.3 ms; writer applies the offset |
| Webcam lag adaptation (emission waits for the slowest source) | Yes | Arrival-latency peak per source, decaying 50 ms/s, sets the emission lag (0.05–0.3 s) | 1G (adaptation), 1B (camera) | done | unit `media::writer::tests::camera_latency_widens_the_lag_and_decays`; selftest `camera-lag` (synthetic camera 80 ms late: 120/120 frames, no drift); needs a real desktop for a real 70–120 ms camera |
| Crash-safe: fragments every 2 s, regular movie on stop | AVAssetWriter fragments while recording, then a regular moov | `fragment-duration=2000` while recording; `media::mp4::finalize` appends a regular moov that indexes the samples where they lie and flips the fragments to `free` (no media bytes move) | 1G | done | selftest `crash` (SIGKILL at 4.5 s: 238 frames, fragmented, decodes), `finalize-crash` (kill between the append and the flip: all 120 frames; mid-flip: playable), `defrag` (same sample count and identical ffmpeg framemd5 as the kept fragmented copy; final file has no `moof`/`mvex`) |
| Edge to edge: frame has the source's aspect, no bars, never cropped | Yes | Same `fill`/`same_aspect` maths | 0 | done | unit `compose::tests::fill_is_edge_to_edge_for_the_same_aspect` |
| Window transparent corners filled with the window's edge colour | CoreImage clamp | `compose::corner_inset` + a clamp in the upload shader, compositing the window over the clamped edge | 0 (geometry), 1C (GPU) | partial | unit `compose::tests::fill_is_edge_to_edge_for_the_same_aspect` (`corner_inset`), `gpu::tests::the_corner_fill_replaces_transparent_window_corners` |
| Camera settles 2 s before the file starts | Yes | `wait_for_camera` holds the first screen frame until the camera's `settle_remaining()` has elapsed (gives up after 3 s more); the app hands its running camera from the preview to the recording and back (`Session::take_camera`/`start_with_camera`, upstream's `spare_cam`), so a recording from an open preview never waits and never restarts the device | 1B (settle API), 1G (gate), 1E (hand-over) | done | unit `media::writer::tests::the_camera_gate_holds_the_start_until_the_camera_settles`, `…gives_up_on_a_camera_that_never_delivers`; selftest `camera` (cold CLI: wall 4.2 s); `app::ui_tests::the_camera_is_handed_over_without_reopening_it` (preview opens once, the recording reuses it and starts in <1.5 s, the preview gets it back; a cold start waits ≥1.8 s and opens its own) |
| Audio quality: 48 kHz capture, windowed-sinc resampler continuous across buffers | Yes | Same resampler maths (ported) | 0 | done | unit `mixer::tests::resamples_seamlessly`, `resampler_removes_content_above_nyquist`, `small_jitter_snaps_to_continuous` |
| Mix limiter keeps the mix under −1 dBFS, no clipping | Yes | Same soft-knee limiter on the mix bus; the stems are not limited (upstream's float path) | 0 (maths), 1G (mix track) | done | unit `mixer::tests::limiting`; selftest `tracks`/`gain` measure the mix and both stems |
| Pause cuts whole frame slots, audio at the exact sample, no gap, no drift | Yes | Same `Pauses` timeline in the writer | 0 | done | unit `media::pauses::tests::output_slots_stay_contiguous_across_pauses`, `keep_cuts_captures_at_pause_boundaries`; selftest `pause-resume` (300 frames, sync holds) |
| `--duration` exact in output frames, pauses don't count | Yes | Same | 0 | done | selftest `pause-resume`, `sync` |

**AAC priming note.** AVFoundation writes the MP4 edit list that makes decoders skip an AAC
encoder's priming samples; GStreamer's `qtmux`/`mp4mux` 1.26 cannot. The finalise in
`src/media/mp4.rs` writes it instead: each audio track gets an `edts`/`elst` whose `media_time` is
the encoder's priming (fdkaacenc 2048 samples, avenc_aac 1024, scaled to the track's timescale), and
`REC_AAC_PRIMING` overrides it. Readers that honour edit lists (ffmpeg, QuickTime) start at the
first real sample, so the first beep in a file is at 0.0 ms too. `rec-rs probe` reports the edit
list per track; GStreamer's own decode shows the unskipped priming, which is why the probe's decode
line starts at 0.043 s for an audio track.

## Streaming

| Feature | Upstream behaviour | Linux approach | Phase/owner | Status | Evidence |
|---|---|---|---|---|---|
| One shared live encode for replay + every stream | VideoToolbox/AudioConverter | GStreamer encode to appsink, fanned out by the hub | 1D | done | unit `live::tests::short_gaps_are_filled`, `live::tests::replay_saves_a_playable_mp4`; selftest `stream-rtmp`, `stream-whip` |
| H.264 High / HEVC Main10, no B-frames, 2 s keyframes, CBR or ABR | Yes | H.264 High (x264enc, `tune=zerolatency`, `bframes=0`) and HEVC **Main** — this image's x265 is 8-bit, so Main10 needs a 10-bit encoder (VA-API on a GPU host); the encode logs which one it got. CBR (`pass=cbr` + VBV) or ABR (`--vbr`); keyframes from `--keyframe` | 1D | partial (Main10) | selftest `stream-rtmp` (`ffprobe` reports h264/High), `stream-hevc` (MediaMTX's recording is hevc/Main with the A/V offset measured); unit `live::video::tests::encodes_keyframes_on_schedule`, `parameter_sets_and_records`; preset maths unit `live::tests::platform_presets` |
| Enhanced RTMP HEVC (`hvc1`), fallback to H.264 | Yes | `fourCcList` offer, `hvc1` SequenceStart and CodedFramesX; a server that lists its codecs without `hvc1`, or keeps failing HEVC, switches the shared encode to H.264 with a note | 1D | done | selftest `stream-hevc` (HEVC recorded by MediaMTX; fallback run records H.264 with the note); unit `live::rtmp::tests::hevc_negotiation`, `flv_bodies` |
| Live size scaled from the recording size | Lanczos | Live pipeline scales with GStreamer `videoscale method=lanczos` to the encode size (never upscaled, even dimensions); the recording's own scaler is the GPU one (1C), so the two are independent but both Lanczos | 1C/1D | done | unit `live::tests::encode_size_fits_the_box`; selftest `stream-rtmp` streams 720p30 out of the same encode |
| Replay buffer trims a GOP at a time, re-encodes audio, syncs in QuickTime and ffmpeg | Yes | Same: video passed through (AVCC/HVCC), audio decoded and re-encoded with the new encoder's priming + one frame dropped and the rest re-stamped | 1D | done | selftest `replay` (+5.7 ms measured by `synccheck.py` on the saved clip; first packet has the keyframe flag); unit `live::replay::tests::keeps_the_window_from_a_keyframe` |
| RTMP/RTMPS client with TLS, drop a GOP when behind, reconnect with backoff | Built-in, Security.framework | Same client ported to rustls (ring provider, webpki roots); 3 s queue that drops video a GOP at a time; 1 s → 30 s backoff. `REC_INSECURE_TLS` (the self-test's switch) is honoured only for a loopback host, so a stray export cannot send a key to an unverified server | 1D | done | selftest `stream-rtmp` (RTMP + RTMPS, reconnect after a server restart: 3 reconnects, live at the end; A/V +0.2 ms); unit `live::rtmp::tests::{amf_round_trip,urls,chunking_round_trips_through_the_reader,flv_bodies}`, `live::tests::insecure_tls_is_loopback_only`; `scripts/live/streams.sh --insecure` (ignored with a warning and the certificate refused for a non-loopback address, still accepted on localhost) |
| WHIP (str0m, ICE/DTLS/SRTP, H.264+Opus, DELETE on stop, 5 s silence → reconnect) | Yes | Same: str0m on its pure-Rust `rust-crypto` backend, SDP POST over ureq+rustls with the key as a Bearer token, packets sent at capture time, an A/V-aligned start, DELETE on stop, reconnect after 5 s of silence. One deliberate difference: every packet's RTP time is its capture time — upstream nudges the first keyframe's RTP onto the first audio packet's, which measured ±20 ms swings against MediaMTX and moved the recording by the nudge | 1D | done | selftest `stream-whip` (MediaMTX records H.264 + Opus; every mean of five consecutive sessions within ±15 ms — +3.4/−3.3/+3.4/+3.3/+3.4 ms in the last set, the worst mean over repeated sets −13.8 ms with one 20 ms step from MediaMTX's WebRTC receiver on a saturated box, where our sender logged no drops and RTMP in the same session stayed at −0.3 ms; it was +1.7/−36.5/−36.5/−36.5/+1.7 ms before the anchor fix) and `live-e2e` (WHIP from a real `record` run: +0.0 ms, with RTMP −8.6 ms and the file +0.0 ms in the same session); unit `live::whip::tests::{urls,session_starts_aligned_and_paced,starts_without_audio,waits_for_audio_next_to_the_keyframe}` |
| Platform bitrate/size presets (YouTube/Twitch/Kick), lowest across destinations | Yes | Identical maths (ported), and the encode honours the chosen size/bitrate | 0 (maths), 1D (encode) | done | unit `live::tests::platform_presets`, `shared_preset_fits_every_destination`; `live-selftest` prints the preset it picked (`5.0 Mb/s` for a custom server at 720p30) |
| Stream keys in the OS key store, `REC_STREAM_KEY` override | Keychain | Secret Service (`keyring` crate, `sync-secret-service`), keyed per destination URL, read with echo off; `REC_STREAM_KEY` for one run; a clear error when no Secret Service is running, never a plaintext file | 1D | done | unit `live::keyring::tests::round_trip` (in-memory adapter), `secret_service_round_trip` (real service when the session has one; `REC_KEYRING_TEST=1` requires it); manual check below |
| Tested against MediaMTX (RTMP, RTMPS, WHIP) | Yes | MediaMTX is downloaded at test time into a cache outside the repo; RTMP, RTMPS (self-signed certificate generated at test time) and WHIP run at once from one encode, and it records every path for the checks | 1D | done | selftest `stream-rtmp` (2 RTMP + 1 RTMPS + WHIP, all four recordings probe and are +0.2/−3.1 ms), `stream-whip`; `scripts/live/mediamtx.sh` |

## OBS comparison rows rec-rs claims

| Feature | Upstream behaviour | Linux approach | Phase/owner | Status | Evidence |
|---|---|---|---|---|---|
| Display capture | ScreenCaptureKit, zero copy | Portal + PipeWire (packed BGRx/BGRA on the CPU) on Wayland, `GetImage` on X11. dmabuf/zero-copy needs phase 1C's GPU importer | 1A | done | selftest `pattern-screen`: 1280x720 @ 60/1, 180 frames, 180 distinct, `probe` + full ffmpeg decode |
| Window capture | Yes | X11 captures the client's own drawable; Wayland uses the portal's window source when the desktop offers one (GNOME/KDE yes, wlr no) | 1A | partial | selftest `screen-window`; live test `a_window_is_captured_with_its_own_pixels` |
| Region / crop | `--region` | `--region` | 1A | done | selftest `screen-region`; unit `format::tests::*` |
| Application capture | No (known gap) | No | — | n-a | matches upstream |
| Cursor shown (default) | Yes, embedded by ScreenCaptureKit | Embedded by the compositor (portal) or composited from XFixes (X11) | 1A | done | live test `a_window_is_captured_with_its_own_pixels` (XFixes path); cursor modes chosen from the portal's `AvailableCursorModes` |
| Click highlight (`--clicks`) | Yes | Ring drawn into the CPU frame; X11 only, because desktop portals do not report mouse buttons | 1A | partial | live test `a_click_draws_a_ring_at_the_pointer` (XTEST); `--clicks` warns on Wayland |
| One-time screen permission | TCC grant | ScreenCast restore token in `$XDG_STATE_HOME/rec-rs/portal-token`, replayed through `SelectSources` | 1A | done | selftest `screen-token`; unit `token::tests::*` |
| Scenes | Three fixed scenes, switchable live, cut only | Same three scenes, composited on the GPU; the picker enforces the camera rule while recording | 1C/1E | partial | scene model: unit `tests::scene_option`; rendering: `gpu::tests::scenes_and_camera_changes_apply_from_the_next_frame`; goldens `screen`, `screen-cam-*`, `cam`; unit `app::ui_tests::main_window_shows_every_card` |
| Multiple displays/sources at once | No | No (a portal session may hand over several streams; the first is used) | — | n-a | matches upstream |
| System audio | Yes | PipeWire default sink monitor | 1B | done | selftest `pattern-system-audio` (48 kHz and 44.1 kHz: 5 beeps at −6 dBFS, spacing 1.000 s); integration `system-48k`/`system-44k` |
| Microphone | Yes | PipeWire `Audio/Source` capture at the device's own rate | 1B | done | selftest `pattern-mic`; integration `mic-default`, `mic-by-name`, `mic-unplug` |
| Webcam overlay with shapes/border/shadow/mirror, live while recording | Yes | V4L2/portal source (1B) + wgpu compositor (1C); the recorder wires them (1G) | 1B/1C/1G | partial | unit `gpu::tests::every_shape_matches_the_mask_geometry`, `renders_overlay_with_border_into_the_frame`; camera source units; goldens `screen-cam-*`; real camera under "Needs a real desktop" |
| Separate audio tracks | Up to 4 | Mix + 2 stems | 1G | done | selftest `tracks` (4 tracks with real AAC 256/256/192 kb/s), `audio-tracks` |
| Per-source gain and mute, live | Yes | Same ramp/fader | 0 (maths), 1E (widgets) | done | selftest `gain`; unit `fader_glides_and_passes_unity_through`, `app::ui_tests::the_level_can_be_typed`, `a_muted_channel_greys_its_meter` |
| Audio filters (noise suppression, compressor) | No (known gap) | No | — | n-a | matches upstream |
| Hardware H.264/HEVC | VideoToolbox | VA-API on a GPU host; the container has no GPU, so it records with x264enc/x265enc (the writer skips VA without /dev/dri and trial-encodes it otherwise) | 1C | todo | dev/ELEMENTS.md |
| ProRes | 422 | `avenc_prores_ks`, profile pinned to 422 `standard` | 0 (element), 1G (profile) | done | selftest `pattern-prores`: ffprobe `prores/Standard`, BT.709, `.mov` only |
| 10-bit / HDR | No | No | — | n-a | matches upstream |
| Constant frame rate | Exact grid | Exact grid | 0 | done | selftest + ffprobe PTS |
| Crash-safe files | Fragments every 2 s | Fragments every 2 s, finalised in place | 1G | done | selftest `crash`, `finalize-crash`, `defrag` |
| Pause / resume, sample-accurate, app and CLI | Yes | Yes; the app's button and `rec-rs pause`/`resume` hit the same session | 0 (CLI), 1E (app) | done | selftest `pause-resume`, `gui` |
| Replay buffer 15–300 s | Yes | Done: 15–300 s in memory, memory reported, `save-replay` writes an MP4 | 1D | done | selftest `replay` |
| Streaming (RTMP/RTMPS, WHIP, multi-destination, HEVC, reconnect) | Yes | Done: built-in RTMP/RTMPS and WHIP, one encode for every destination, enhanced-RTMP HEVC with H.264 fallback, reconnect with backoff | 1D | done | selftest `stream-rtmp`, `stream-whip`, `stream-hevc` (not a case of its own; run by `scripts/live/streams.sh`) |
| Bounded memory when the encoder cannot keep up | AVAssetWriterInput is asked `isReadyForMoreMediaData`; a busy encoder drops the frame | appsrc and every queue are bounded (2 video frames, 8 audio buffers); a full appsrc means the grid slot is dropped and counted, never buffered | 1G | done | unit `media::writer::tests::a_silent_or_far_future_source_cannot_grow_memory` (30 s at 1080p60 with a silent and a 1000 s-ahead source: RSS growth < 300 MB), `a_broken_audio_clock_cannot_stretch_the_file`, `mixer::tests::a_far_future_chunk_cannot_grow_the_mix_buffer`; selftest `pattern-4k-long` (20 s 4K60 noise: peak RSS 1.1 GB, 14.8 fps, 904 frames dropped on this CPU-only host) |
| Screen scaling: GPU Lanczos | Metal MPS, which antialiases a downscale | wgpu Lanczos (a = 3), separable, per plane in the encoded domain, **stretched by the reduction factor** so a downscale antialiases, edge clamp, never upscaling | 1C | done | unit `gpu::tests::lanczos_matches_a_cpu_reference` (≤2/255 against an independent CPU implementation), `downscaling_a_one_pixel_pattern_leaves_only_its_mean` (a 1-px 5K pattern leaves 0.7/255 ripple inside the frame, against 121.7 for an unstretched kernel; ffmpeg's own swscale/zimg reach 22.6/29.7 at the replicated edge against our 22.7), `the_scaler_matches_ffmpeg_lanczos` (PSNR 50.3 dB 4K→1080p, 51.4 dB 5K→1080p against swscale's Lanczos; 58.1/57.0 dB against zimg), `a_small_screen_is_not_upscaled`; golden `screen-4k-to-1080`; measured 77 ms/frame 4K→1080p and 116 ms 5K→1080p on lavapipe (`tests/golden/README.md`) |
| Live preview | Yes | The composited frame uploaded as planes and converted/scaled in a shader (G10); 1C's `Renderer::texture()` could remove the last copy in phase 2 | 1E | done | unit `app::preview::tests::*`, `app::preview_gpu::tests::*`; `preview_cost` 1.0–1.2 ms (1080p) / 2.0–2.2 ms (4K) vs 6.6/8.15 before; selftest `gui` (`preview frame 1920x1080`) |
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
| Streaming: no SRT/multitrack, same encode for all, live ≤ recording size, changing settings needs a restart | Yes | Same limitations (parity, not a gap to fix) | 1D | n-a |
| HEVC to YouTube tested only against a local server | Yes | Same testing honestly reported: enhanced-RTMP HEVC is verified against MediaMTX and by the unit-tested `fourCcList` negotiation, not against YouTube itself | 1D | done (as upstream) |
| 8-bit SDR only | Yes | Same | — | n-a |
| Webcam overlay burned into the video | Yes | Same: the compositor runs before the encoder | 1C | partial | the compositor is done and pixel-tested; the writer calls it (1G) |
| Output latency read once at start | Yes | Same (PipeWire sink params at start; a device switch needs a restart, like upstream) | 1B | done | unit `collector_combines_the_latency_parts`; integration `devices` |
| Mix track resamples a non-48 kHz mic linearly per buffer upstream | Yes | Linux: the mix uses the same windowed-sinc resampler as everything else (better) | 1C | todo |
| Re-confirmation prompt for screen recording | macOS-only | n-a: the portal's restore token avoids it | — | n-a |

## Phase 0 summary

Done (proved by tests or self-test cases): the CLI surface, settings and the settings-to-arguments
path, pause timeline and pause/resume end to end, the frame-rate grid, faders/gain ramps and the
resampler and limiter maths, overlay geometry/masks/shadows, the test-pattern source, the writer's
track layout (video, mix, system stem, mic stem, alternate group with the mix enabled), the AAC
priming edit list, BT.709 tags on every codec, the fragment-to-regular-movie finalise with both of
its crash windows, the camera gate and lag adaptation, the native-size scaler, bounded memory when
the encoder falls behind, `probe` (tracks, alternate group, enabled flags, edit lists, full
decode), the platform preset maths, the pidfile/signal control, and the sync/level/self-test
scripts.

Partial (a named part is missing, listed per row): hardware encoding (VA-API is selected and
trial-encoded when `/dev/dri` exists; the container has no GPU, so x264/x265 are what the self-tests
prove), and what can only be checked on a real desktop (a real V4L2 camera and its 70–120 ms
latency, see the rows above).

Todo (phase 1): the app, replay/streaming, packaging.

## Screen capture (phase 1A)

Two backends, chosen by the session type: xdg-desktop-portal's ScreenCast plus PipeWire on Wayland
(`src/capture/screen/portal.rs`, `pipewire.rs`), and X11 directly on an X11 session (`x11.rs`).
`REC_SCREEN_BACKEND=portal|x11` overrides the choice.

What runs headless in the dev container, on every `scripts/selftest.sh` run:

| Case | Session | Proves |
|---|---|---|
| `pattern-screen` | sway (wlroots, headless 1280x720) + xdg-desktop-portal-wlr + PipeWire | the whole Wayland pipeline: picker → PipeWire node → 1280x720 BGRA frames at a constant 60 fps, all 180 frames different (a terminal scrolling in the capture), `probe` and a full ffmpeg decode clean, and a 0600 restore token stored |
| `screen-region` | same | `--region` crops the shared monitor to 640x480 native pixels (not a scaled 16:9 frame) |
| `screen-token` | same | `permissions --request` forgets the stored grant so the dialog runs again (the harness chooser log proves the second picker ran), stores a 0600 grant; a corrupted token is ignored with a message and the recording still happens |
| `screen-x11` | Xvfb 1280x800 | RandR enumeration (`devices` lists `[1]`), a root-window capture at the native size with motion, no portal involved |
| `screen-window` | Xvfb + a GStreamer window | `--window gstreamer` captures that client's own 400x300 drawable, with motion |
| `screen-exclude` | Xvfb + the portal harness | `REC_EXCLUDE_WINDOWS=<window id>`: on X11 the only animating window is painted out, so the recording is a still frame and the log says `leaving 1 of rec's own windows out`; on the portal path the log says a screen-cast portal cannot exclude windows |
| `cargo test` under `scripts/screen/xvfb.sh` | Xvfb | 12 X11 unit tests, 7 of them against the live server: monitor/window enumeration, a window captured with its own pixels, `--region` capturing the pixels it names (red/blue halves), a click ring from a real XTEST click, excluded windows painted out (with a window stacked on top of one, which must survive), and the region geometry |

### Needs a real desktop (streaming and stream keys)

The container has no Secret Service in a normal `scripts/dev.sh` shell (the unit test
skips and says so), no GPU for VA-API Main10, and no account on a real streaming
service. These are the checks to run on a real machine:

1. **Secret Service.** In a desktop session (GNOME/KDE or any keyring daemon):
   `rec-rs stream-key twitch` prompts with echo off, never prints the key, and
   `secret-tool search service 'rec-rs stream key'` shows it under the normalised
   destination. `REC_STREAM_KEY=... rec-rs record --stream twitch --no-record -t 5`
   uses the env override for that run only. Stop the keyring daemon and run
   `rec-rs stream-key twitch` again: it must say no Secret Service is running and
   must not create any file.
2. **Real services.** Go live to YouTube, Twitch and Kick for 30 s each:
   the service reports a healthy stream, its own player is in sync with a
   hand-clap/beep test, and the CLI status line shows `live` with a bitrate near the
   preset. Then all three at once: one encode, three `live` lines.
3. **HEVC Main10.** On a GPU host (`/dev/dri` passed through, VA-API working), stream
   with `--codec hevc`: `rec-rs` must log the VA-API HEVC encoder and, with a
   10-bit-capable one, Main10; YouTube (enhanced RTMP) must accept it. This is the
   one streaming row that is partial: the dev image's x265 is 8-bit, so it produces
   HEVC Main and says so.
4. **RTMPS against a real certificate.** `--stream rtmps://a.rtmps.youtube.com/live2`
   must verify the certificate normally. `REC_INSECURE_TLS=1` is accepted only for a
   loopback host (it is the self-test's switch); against a real host it must be ignored
   with a warning naming the host, which is what `scripts/live/streams.sh --insecure`
   checks with the container's own non-loopback address.
5. **Replay while recording.** `rec-rs record --replay 60 --stream twitch`: pause and
   resume the recording, run `rec-rs save-replay` twice, and check both clips play in
   QuickTime/mpv and unexpected settings changes are refused while live (the app's
   lock, phase 1E).

### Needs a real desktop

The container has no GPU, no output attached to a real compositor, no pointer hardware and no
desktop portal other than xdg-desktop-portal-wlr. These are the checks to run on a real machine,
in order, on **GNOME Wayland** and **KDE Plasma Wayland** (and once on an X11 session):

1. `rec-rs permissions` → `not granted yet - ScreenCast vX; monitors + windows; cursor ...; PipeWire running`.
   Run `rec-rs permissions --request`: the desktop's own dialog appears once, the line says `granted`, and
   `$XDG_STATE_HOME/rec-rs/portal-token` exists with mode 600.
2. `rec-rs devices` on Wayland: the display list either comes from XWayland's RandR (then the pixel
   sizes are the logical sizes) or says the picker chooses. Window list: same.
3. `rec-rs record -t 5 -o /tmp/display.mp4`: the picker appears (first run only), a monitor is shared,
   the start line names it, `rec-rs probe /tmp/display.mp4` is clean and the file is not black. Record
   again: **no picker** (the token was reused).
4. `--no-cursor`: the pointer is absent from the file, and only from the file.
5. `--clicks`: on GNOME/KDE the run warns that portals send no button state and the cursor is recorded
   without highlights; on an X11 session clicks draw a ring.
6. `--window <app substring>` on GNOME/KDE: the picker offers windows, `--window` is only a hint;
   check that the recorded size is the window's and that resizing the window mid-recording is
   followed (the stream renegotiates; the writer's 1G scaler keeps the output size).
7. `--region 100,100,800,600`: the recorded size is 800x600 (× the desktop scale), and the content is
   that part of the screen.
8. `--display 2` with two monitors: the picker still decides; the log names the monitor that was
   picked. On X11, `--display 2` records the second monitor without a picker.
9. X11 session, `--window firefox`: the window is captured even while partially covered by another
   window (a compositor redirects it; without one, obscured content is undefined and the file shows
   garbage where the covering window is — see the gaps below).
10. Revoke the grant in the desktop's settings (GNOME: Settings → Privacy → Screen), then record
    again: the picker appears with `the saved screen-capture grant was refused ...` and the recording
    still happens.

### Known gaps, with the reason

- **dmabuf/zero copy.** Frames are copied to the CPU (`BGRx`/`BGRA`, or X11's `GetImage`). A
  compositor that offers only dmabuf is an error that names phase 1C; the GPU importer is 1C's work,
  and it is also what removes the per-frame copy.
- **Click highlighting on Wayland.** No portal reports mouse buttons (`spa_meta_cursor` carries a
  position and a bitmap, never a button state). `--clicks` warns and records the cursor only. X11
  highlights clicks.
- **`--display N` on Wayland.** The picker owns the choice; a portal has no "monitor 2" parameter.
  The log names the monitor that was picked when XWayland's monitor list identifies it.
- **Window substring matching on Wayland.** The same: the picker owns it, and
  xdg-desktop-portal-wlr does not offer window sources at all (it says so and suggests `--region`).
- **Own windows in the picture.** `set_excluded_windows` lets the app leave its own windows out. On
  X11 that works: their visible parts (occluded by anything stacked above them) are filled with the
  desktop colour sampled around them. What is really *beneath* a mapped window is not readable — no
  server query returns the root's background pixel and a compositor's targets are not ours — so a
  wallpaper is not restored, where upstream composites what is beneath. A portal client cannot
  exclude windows at all: the capture logs that, and the app (1E) hides its windows while recording.
- **Cursor motion on a still screen.** PipeWire streams deliver frames on damage. With the cursor
  embedded the compositor damages the stream, so the pointer is recorded; in metadata mode a pointer
  that moves over an unchanged screen may not produce a frame, and the cursor in the file lags. X11
  polls the pointer, so it always produces a new frame when the cursor moves.
- **X11 `GetImage`.** One round trip and one copy per frame (no MIT-SHM, no XDamage); a 4K screen at
  60 fps costs ~2 GB/s. Acceptable at 1080p, and the portal path is the default on Wayland anyway.
  An SHM/Damage version is a later optimisation, not a parity gap.
- **Obscured windows on X11 without a compositor.** `GetImage` of a window that another window covers
  returns the server's undefined content for that area (black under Xvfb). With a compositor the
  window is redirected and correct.

## Audio and camera sources (phase 1B)

System audio and the microphone are PipeWire capture streams (`src/capture/audio/`); the camera is
V4L2 (`src/capture/camera/v4l2.rs`), falling back to xdg-desktop-portal's Camera when `/dev/video*`
is not reachable (`portal.rs`). All three stamp on `CLOCK_MONOTONIC` and deliver the contract's
`AudioChunk`/`VideoFrame`; the mixer and compositor stay on the other side.

| Feature | Upstream behaviour | Linux | Status | Evidence |
|---|---|---|---|---|
| System audio = what the speakers play | ScreenCaptureKit audio | `stream.capture.sink=true` on the default sink (WirePlumber's `default.audio.sink`), captured at the sink's own rate | done | selftest `pattern-system-audio` (5 beeps, −5.9 dBFS, 1.000 s apart); integration `system-48k`; `system-44k` captures 44100 Hz and resamples to the file's 48 kHz |
| Microphone selection | AVCapture devices, unique id/name | PipeWire `Audio/Source`: default, exact `node.name`, the same USB device on another port, saved/argument display name (exact, substring), case-insensitive | done | unit `capture::audio::tests::mic_matching`; selftest `pattern-mic`; integration `mic-by-name` |
| Missing microphone | Warn and use default | `pick_mic` returns the default plus upstream's warning; the CLI's explicit `--mic` fails with `is not connected`, the default mic warns and records | done | integration `mic-missing`; unit `mic_matching` (warning text) |
| No default sink (G4) | Cannot happen (ScreenCaptureKit always has audio) | The default system-audio source warns and the session continues (`recording without system audio`), like the default mic; `--no-system-audio` skips it silently. The writer still creates a silent system track because tracks are built before `start_sources` (1G could drop it) | done (warn/continue); silent track noted | selftest `no-sink` (headless screen harness has PipeWire and no sink: exit 0, warning, a written file) |
| Device hotplug watch (G9) | AVCapture connect/disconnect notifications reload the lists | `capture::audio::watch_devices(cb)` (PipeWire registry, 200 ms coalescing) and `capture::camera::watch_devices(cb)` (inotify on `/dev/video*`, 250 ms coalescing). 1E starts both and refreshes its source lists; in Flatpak the portal has no camera-added signal, so 1E polls that list | done (backend) | unit `camera::v4l2::tests::the_device_watch_sees_video_nodes_appear_and_disappear`; integration `watch` (a new sink notifies within 5 s) |
| Native rate and channels | 48 kHz AVCapture | The stream asks for the node's `audio.rate`/`audio.channels`; the mixer resamples other rates with its windowed-sinc resampler | done | integration `system-44k` (log: `system format 44100 Hz`, recorded 1 kHz, spacing 1.000 s); `mixer::tests::resamples_seamlessly` |
| Chunk timestamps and continuity | Host clock from CoreMedia | `pw_buffer.time` (graph cycle, ns) / 1e9, guarded against other clocks; live worst `|pts − arrival|` 1.0–4.9 ms; a continuity detector counts overlapping/duplicated buffers under `REC_DEBUG` (G15) | done | unit `buffer_times_become_monotonic_seconds`, `timestamps_from_another_clock_fall_back_to_now`, `overlapping_chunks_are_detected`; ignored `live_chunk_timestamps_are_on_the_host_clock`, `live_mic_chunks_do_not_overlap`; integration `clock`, `stress` (8 burners: 92 chunks, no overlap) |
| Device disappears mid-recording | Capture error | The registry's `global_remove` of the linked node stops the loop; `stop()` returns `the mic device was disconnected`; the session keeps recording video and finalizes | done | integration `mic-unplug` (121 frames, warning in the log) |
| Capture latency | CoreAudio | Measured end to end: first captured tone **+26.6…+33.4 ms** after `pw-play` was spawned (its start-up included) | measured | ignored `system_audio_capture_latency`; integration `latency` |
| Output latency | CoreAudio device latency | Default sink `Latency` + `ProcessLatency` params, else `node.latency`; printed by `devices` only when a sink reports one (G11) and used as the system-audio shift | done | unit `collector_combines_the_latency_parts`, `node_latency_property_is_a_fraction_or_frames`; integration `devices` (256/48000 → 5.3 ms) and `no-pipewire` (no line) |
| Camera enumeration | AVCapture discovery | `/dev/video*` nodes with `VIDEO_CAPTURE`+`STREAMING` (the metadata node is skipped), path as id, driver card as name, numeric order | done (unit); real device listed below | unit `camera::v4l2::tests::device_matching`, `enumerate_devices_never_panics_without_a_camera` |
| Camera format negotiation | AVCapture 720p preset | Largest frame ≤1920×1080 that reaches 30 fps, even dimensions, 16:9 at equal size, YUYV → NV12 → MJPEG; driver adjustments read back; stepwise ranges sampled, never enumerated exhaustively | done (unit); real device listed below | unit `camera::format::tests::*`; unit `fps_of`, frame interval tests |
| Camera pixels | AVCapture `PIX_BGRA` | YUYV/NV12/I420/BGRx → BGRA with BT.601/709 limited range; MJPEG through `zune-jpeg`; every frame `is_well_formed()` | done | unit `camera::convert::tests::*` (synthetic YUYV/NV12/I420, a 16×16 JPEG fixture, truncated JPEG is an error) |
| Camera timestamps | Host clock | V4L2 `timeval`; epoch-valued (wall clock) timestamps converted by age; timestamps >5 s from the host clock replaced by arrival; non-monotonic output repaired | done | unit `monotonic_driver_timestamps_pass_through`, `wall_clock_driver_timestamps_are_converted`, `unknown_and_non_monotonic_timestamps_are_repaired` |
| Camera size/aspect and settle API | Session preset + 2 s settle | `CameraSource::native_size()`/`aspect()` known from negotiation; `CAMERA_SETTLE_SECS = 2.0`, `settle_remaining()`; 1C/1G consume | done (API); wiring 1G | unit `camera::tests::size_and_aspect_are_known_before_the_first_frame`, `describe_names_the_camera_and_its_format` |
| Camera portal (Flatpak) | n/a | `ashpd` Camera → access request → PipeWire remote → node; BGRx/BGRA/RGBA/YUY2/NV12/I420 → BGRA; 1280×720 until the stream negotiates | needs real desktop | compiles and is exercised only for the no-portal path here; checklist below |
| Dead camera | Capture error | Five empty 500 ms stream timeouts report `camera … stopped delivering frames`; `stop()` returns it; the thread never blocks the session | done (unit logic); real device below | code path in `v4l2::run_capture`; covers 1G's writer error handling |
| `permissions` audio/camera lines | TCC | Mic: PipeWire + number of inputs. Camera: V4L2 nodes or the portal. A missing PipeWire session is reported, never a hang | done | integration `no-pipewire`; container report: `microphone : unavailable (PipeWire is not available …)`, `camera : unavailable (no /dev/video* capture device)` |

### Needs a real desktop (camera and hardware audio)

The dev container has no webcam (and no `v4l2loopback` module it could load), no sound card and no
Flatpak portal host. These are the exact checks to run on a real machine:

1. **UVC camera.** Plug a webcam. `rec-rs devices` must list `/dev/videoN` with the card name, not
   the metadata node (`v4l2-ctl --list-devices` shows which is which). `rec-rs record --test-pattern
   --cam -t 5 -o /tmp/cam.mov` (once 1G wires the camera) must show a live overlay;
   `rec-rs probe /tmp/cam.mov` decodes every frame. `--webcam <name substring>` and
   `--webcam /dev/videoN` must select the same device.
2. **Camera settle.** Start a recording immediately after plugging the camera: the first two seconds
   are not dark (auto-exposure converges), and the writer's camera gate starts the file once
   `settle_remaining()` is 0.
3. **Format choice.** `v4l2-ctl -d /dev/videoN --list-formats-ext`; the start log's camera line must
   name 1080p30 MJPEG when YUYV only reaches 5 fps, and YUYV/NV12 when it reaches 30 fps. An
   unplugged camera mid-recording must finalize the file and log the capture error.
4. **Camera portal.** Build the Flatpak without `/dev/video*` access, run
   `flatpak run io.github.sultanaltair96.Rec devices`: the portal camera is listed; the first
   recording shows the desktop's camera dialog; denying it gives a clear error and no hang; granting
   it records BGRA frames.
5. **Real sink latency.** With a real output device, `rec-rs devices` must print a non-zero latency
   (compare `pactl list sinks`); record a screen flash plus a `pw-play` beep and measure it with
   `scripts/synccheck.py` (≤20 ms with the automatic offset; a Bluetooth sink is the interesting
   case).
6. **Bluetooth microphone.** `--mic <headset>` must select it, and `--mic-offset-ms N` must move the
   mic track against the video flash the way upstream's mic offset does.

### Known gaps (phase 1B)

- **Default sink changes mid-recording are not followed.** System audio is linked to the sink that
  was the default when the source opened (a target-pinned PipeWire stream is not moved by
  WirePlumber), so switching outputs needs a new session. Upstream reads the output latency once at
  start too; the app (1E) or the writer's restart path (1G) can reopen the source later.
- **The camera is not wired into `record` yet.** `Session::start_sources` is 1G's: it will call
  `camera::open_source`, feed the compositor and pass `settle_remaining()` to the writer's camera
  gate. Until then `record --webcam` still says phase 1B.
- **The camera portal path is untested at run time.** It compiles and its matching logic is unit
  tested, but this host has no Flatpak portal camera; the manual checklist above is the proof.
- **MJPEG decoding is on the CPU** (zune-jpeg). Fine for a 720p/1080p webcam on one core; a 4K MJPEG
  camera would want the GPU path, which is not claimed.

## Phase 1E summary

Done (proved by tests or the self-test): the main window and settings window on egui 0.36/wgpu 30
with the paper-rs look; the live preview of the **composited** frame (`Options::preview`, ≤30/s,
one-pass CPU upload, low-power mode, nothing buffered while hidden); the camera overlay drag with
corner snapping, applied live through `Session::set_pip` and visible in the next preview frame; live
scene switching; the mixer widgets (fader scale, drag/click/scroll/double click/typed dB, mute
greying the meter); the settings window and its `settings.json` round trip; the app menu and its
shortcuts; the tray model (menu, title, tooltip, ARGB icon); the single-instance socket; the
pidfile/signal controls against the running app; quitting that finalizes a recording; the composited
preview selftest case; "Check for Updates" against GitHub Releases; AppImage self-integration; and
the headless self-test case that runs the real app with a camera.

Partial (a named part is missing, listed per row): the replay buffer and streaming are 1D (the cards
and their gating are here); "Reveal Last" selects the file only where FileManager1 exists; the tray
and the global hotkeys are complete in code but their visibility and the portal dialog need a real
desktop (manual list above).

Known gaps: SNI hosts disagree about title text (the tooltip carries the state everywhere); the
X11 hotkey fallback cannot be exercised under a Wayland-only session and vice versa; the update
check opens the release page rather than replacing the binary (Flatpak and AppImage packaging own
that, phase 1F); a visible Rec window is part of an X11 capture (the portal path cannot exclude it;
Ctrl+W hides it while the recording continues). No audio filters and no configurable hotkeys,
matching upstream.
