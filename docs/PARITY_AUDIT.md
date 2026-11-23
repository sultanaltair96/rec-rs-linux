# Independent parity audit (phase 3A)

Reviewer's read-only audit of this branch (head `b2e7006`) against the pinned upstream
`reference/rec-rs` (`93dd494`, "v0.1.2 + site analytics"). I did not write product code and did not
change anything except this file.

The inventory below was built **from upstream**, not from `PARITY.md`: `reference/rec-rs/README.md`
end to end (main-window list, settings list, option table, output-format guarantees, OBS comparison
table, known gaps), `PERFORMANCE.md`, and then `reference/rec-rs/src/{main,settings,record,writer,
screen,camera,compose,synth,probe,tcc,hotkey}.rs` and `reference/rec-rs/src/app/{ui,menubar,mod}.rs`
for flags, controls and settings fields the README does not list. Each item was then chased into the
port and, where cheap, run.

## How this was checked

Everything ran through `scripts/dev.sh` (Fedora 43 container, release build, `CARGO_BUILD_JOBS=2`,
`REC_TEST_DIR=/home/dev/rec-tests/audit`). The host is shared with other worktrees, so timings and
CPU-bound measurements are pessimistic; they are reported as measured and flagged where load matters.

| What | Command | Result |
|---|---|---|
| Unit + UI + golden tests | `cargo test --release -- --test-threads=2` | `286 passed; 0 failed; 6 ignored` (45.5 s) |
| Core writer/codec self-tests | `scripts/selftest.sh pattern-1080-h264 pattern-mp4 tracks sync pause-resume gain crash` | `5 passed, 1 failed` on the first run; `gain` re-ran alone and passed (see G15) |
| Codec self-tests | `scripts/selftest.sh pattern-prores pattern-hevc pattern-4k-hevc` | `3 passed, 0 failed` |
| Crash / bitrate / gaps / stop | `scripts/selftest.sh h264-bitrate audio-gaps timeout-stop defrag` | `4 passed, 0 failed` |
| Portal (Wayland) capture | `scripts/selftest.sh pattern-screen screen-region screen-token` | `3 passed, 0 failed` |
| App | `scripts/selftest.sh gui camera camera-lag` | `3 passed, 0 failed` (165 + 78 frames, preview 704x396, second launch handed over, pause/resume/stop, SIGTERM finalised) |
| Real audio | `scripts/selftest.sh pattern-mic pattern-system-audio` | `2 passed, 0 failed` (48 kHz and 44.1 kHz sinks, mic by default/name, missing device) |
| Output latency | `scripts/audio/integration.sh devices` under `scripts/audio/harness.sh` | `PASS devices (Output latency … 5.3 ms)` |
| X11 capture | `scripts/screen/xvfb.sh` + manual runs | `devices` lists `[1] id screen`; `--region 0,0,640,480` → 640x480/60 frames; `--window gstreamer` → 400x300; `-d 0`/`-d 9` → upstream's message, exit 1 |
| CLI behaviour | `devices`, `permissions`, `permissions --request`, `probe`, `stop`, `pause`, `save-replay`, `stream-key`, error paths | see the rows and the gap list below |
| Preview cost | `cargo test --release app::preview::tests::preview_cost -- --ignored --nocapture` | `6.61 ms` (1080p) and `8.15 ms` (4K) per 704x396 preview frame |
| Scaler quality | `cargo test --release gpu::tests::the_scaler_matches_ffmpeg_lanczos -- --ignored --nocapture` | PSNR 50.27 dB / 58.10 dB (4K→1080p), 51.36 / 56.95 (5K→1080p) — reproduces `PARITY.md` |

Not run here (and therefore not counted as verified): any capture or app case on a **real**
compositor (GNOME/KDE Wayland), a real V4L2 camera, a real audio output device, the tray on a real
panel, the portal hotkey dialog, MediaMTX streaming, Flatpak/AppImage builds, and any external player
(QuickTime, Final Cut, Premiere, Resolve).

Classification vocabulary: **verified** (I ran it or read a test that asserts it), **claimed-unverified**
(`PARITY.md` says done, no real test or I could not confirm), **missing** (no implementation),
**deviates** (implemented differently from upstream in a way a user notices), **n-a-ok** (justified
Linux difference), **needs-real-desktop** (headless cannot prove it), **pending-merge** (phase 1D/1F
is on a branch, not in this tree — `bb/1d-…`, `bb/1f-…`).

## Command line: `record` and its options

| Feature | Upstream source | Port source | Evidence I checked | Classification | Note |
|---|---|---|---|---|---|
| `record` records screen + system audio + mic + webcam into one file | `README.md:223-236`, `main.rs:29,120` | `src/cli.rs:26`, `src/record.rs:651` | Ran in four configurations: test pattern, portal/Wayland (`pattern-screen`), X11 (`xvfb`), pattern + synthetic camera + real PipeWire mic (4 tracks, 120 frames for `-t 2`) | verified | The canonical README example (`--display 1 --mic default --system-audio --cam --out file.mp4 --duration 2`) ran with `--test-pattern` + `REC_SYNTH_CAM=1`: 120 frames, 4 tracks, mp4 |
| `-o/--out/--alias output`, `.mov` default | `README.md:243`, `main.rs:104,357` | `src/cli.rs:29`, `src/cli.rs:277` | `REC_OUTPUT_DIR=…` + `record --test-pattern -t 1` → `rec-2026-11-23-013704.mov`; stdout prints the path | verified | Path differs by design: `$XDG_VIDEOS_DIR`/`~/Videos` instead of `~/Movies` (documented, `README.md:243` upstream vs port README) |
| `-d/--display N` (1 = main) | `README.md:244`, `main.rs:107,455-458` | `src/cli.rs:32`, `src/capture/screen/mod.rs:280,287` | X11: `devices` → `[1] id screen`; `-d 0` and `-d 9` → `display 0 not found: displays are numbered 1..=1 (see rec-rs devices)`, exit 1. Wayland: `-d 2` prints the picker note | deviates | Upstream enforces the display; a portal has no "monitor 2" parameter, so on Wayland the picker decides (`PARITY.md` says so). X11 path matches upstream |
| `--window W` (id or app/title substring, largest match wins) | `README.md:245`, `screen.rs:99` | `src/cli.rs:40`, `src/capture/screen/x11.rs`, `selector.rs` | X11: `--window gstreamer` → 400x300, 60 frames; `--window 2097154` → same window by id; `--window NoSuchApp` → `no on-screen window matches "NoSuchApp" (see rec-rs devices)`, exit 1. Portal: `--window` only a hint, wlr refuses windows with a clear message | deviates | Justified on Wayland (portal owns the choice, xdg-desktop-portal-wlr offers monitors only). X11 matches upstream |
| `--region X,Y,W,H` (points, top-left) | `README.md:246`, `main.rs:111` | `src/cli.rs:36`, `format.rs`, `screen/mod.rs` | X11 `--region 0,0,640,480` → 640x480/60 frames; portal selftest `screen-region` → 640x480 native pixels out of 1280x720; `--region 1,2,3` → `--region must be X,Y,WIDTH,HEIGHT (points)` | verified | Conflicting `--region`/`--window` rejected (clap), like upstream |
| `-r/--resolution` 720p/1080p/1440p/4k/native/WxH, source aspect, never upscaled | `README.md:247`, `main.rs:120,286-320` | `src/cli.rs:46,200` | Unit `tests::sizes` (portrait, 16:10, odd, never up). Ran 720p (1280x720), 1080p (1920x1080), `native` on the 4K pattern (3840x2160), `-r 4k` | verified | Same maths, ported line for line including the portrait rule |
| `-f/--fps` 24/25/30/50/60/120, divisors of 60000 | `README.md:248`, `main.rs:127,368` | `src/cli.rs:53,281` | `--fps 7` → `--fps must divide 60000 (e.g. 24, 25, 30, 50, 60, 120)`; 30 and 60 ran; PTS grid 0.016667 | verified | Same validation and message |
| `--codec` auto (H.264 ≤ 1080p, HEVC above), h264, hevc, prores | `README.md:249`, `main.rs:130,346` | `src/cli.rs:56,331` | `pattern-1080-h264` (h264/High), `pattern-hevc` (hevc/Main), `pattern-prores` (prores/Standard), `pattern-4k-hevc` (`-r 4k` chose HEVC); `--codec prores -o x.mp4` → `ProRes needs a .mov output` | verified | Same automatic rule and same ProRes/.mov guard |
| `--bitrate` Mb/s, else bits-per-pixel heuristic | `README.md:250`, `main.rs:133,349-362` | `src/cli.rs:59,338` | Selftest `h264-bitrate`: `--bitrate 8` → 8.10 Mb/s at profile High; unit `test_pattern_prepares_without_a_display` asserts 4K ≥ 30 Mb/s | verified | bpp 0.16/0.10/0, clamp 1–400 Mb/s: identical |
| `--system-audio`/`--no-system-audio` (on by default, last wins) | `README.md:251`, `main.rs:136-140` | `src/cli.rs:62-67` | Unit `system_audio_flags_override_each_other`; `pattern-system-audio` records 48 kHz and 44.1 kHz sinks at −5.9/−6.0 dBFS with 1.000 s spacing | verified | See G4 for the no-sink case, which upstream cannot reach |
| `--mic DEVICE`/`--no-mic` (default mic, id or name substring) | `README.md:252`, `main.rs:143-151`, `camera.rs:75` | `src/cli.rs:69-77`, `src/capture/audio/mod.rs:100` | `pattern-mic` passed: default mic, `--mic 'Rec-Test'`, and the missing-device case (explicit fails, default warns and continues). `--mic definitely-not-a-device` → `the microphone you asked for could not be opened: …`, exit 1 | verified | `--mic` **takes an optional value** here; upstream requires `DEVICE` — see G6 |
| `-w/--cam/--webcam [DEVICE]`, scene turns on with a camera | `README.md:253`, `main.rs:153` | `src/cli.rs:79` | Pattern + `REC_SYNTH_CAM=1` with `--cam`; `--scene cam --cam` → camera-only file; six overlay shapes recorded | verified | Camera source and overlay verified with `REC_SYNTH_CAM`; a real `/dev/video*`/portal camera is a desktop check (no camera in the container) |
| `--scene screen/screen-cam/cam`, default screen-cam with a camera | `README.md:254`, `main.rs:158,389` | `src/cli.rs:84` | Unit `scene_option`; ran `screen`, `screen-cam`, `cam`; `--scene cam` implies the webcam | verified | Same enum and rule |
| `--pip-size/-corner/-shape/-roundness/-border/-border-color/-shadow/-pos/-no-mirror` | `README.md:255-258`, `main.rs:161-186` | `src/cli.rs:87-112`, `src/compose.rs` | Unit `overlay_options`, `compose::tests::{snapping, corners_and_free_positions}`; recorded all six shapes (`circle, rounded, square, pill, squircle, rect`); golden `screen-cam-*`; UI drag test | verified | Clamps identical (size 0.05–0.9, roundness 0–1, border 0–64, pos 0–1); `--pip-border-color zz0000` gives the same `anyhow` parse error as upstream |
| `--audio-tracks both/mix/stems` (one track with one source) | `README.md:258`, `writer.rs` track layout | `src/cli.rs:126`, `src/media/writer.rs` | `both` → 4 tracks (video, mix enabled, 2 disabled stems, alt-group 1); `mix` → 2 tracks; `stems` → 2 enabled stems, no group; `stems` without a mic → video + system only | verified | Bitrates and channels match upstream (256/256/192, 2/2/1 ch) |
| `--system-gain`, `--mic-gain` (−60 silence, +24 cap, every track) | `README.md:259`, `mixer.rs:233-252` | `src/cli.rs:177-183`, `src/mixer.rs:194-220` | Selftest `gain` (`--ref=-6 --expect=-12/-18`) passed; `--system-gain=-100/100` accepted (clamped); unit `volume_gain` | verified | One flaky `gain` failure under heavy load — see G15 |
| `--system-audio-offset-ms` (default = output latency), `--mic-offset-ms` (0) | `README.md:260`, `outlatency.rs`, `record.rs:234` | `src/cli.rs:130-140`, `src/record.rs` (`set_audio_offsets`) | `devices` under the audio harness: `Output latency …: 5.3 ms`; writer applies the offset (`set_audio_offsets`, unit `node_latency_property_is_a_fraction_or_frames`); `sync` selftest 0.0 ms with the default | verified | The value (5.3 ms) and its application are verified; audible alignment on a real sink/BT device is a desktop check. Overrides work; the default is read once at start, as upstream |
| `--no-cursor`, `--clicks` | `README.md:261`, `screen.rs`/`writer.rs` | `src/cli.rs:114-119`, `src/capture/screen/{portal,cursor}.rs`, `x11.rs` | Portal: cursor mode chosen from `AvailableCursorModes` (`permissions` shows `cursor hidden/embedded`); X11: XFixes compositing + QueryPointer click ring, live tests `a_click_draws_a_ring_at_the_pointer`, `cursor_is_composited_at_its_hotspot`; `--clicks` on Wayland warns and still records the cursor | verified | Verified on X11; on Wayland portals carry no button state (a justified gap, warned about), so the Wayland click highlight is a desktop check |
| `-t/--duration` exact in output frames, pauses don't count | `README.md:262,238-239`, `writer.rs:812-940` | `src/cli.rs:120`, `src/media/writer.rs:1052` | 720p `-t 1` → exactly 60 frames, movie duration 1.000 s; 1080p `-t 2` → 120 frames/2.000 s in the canonical run; `pause-resume` → 300 frames with a 2 s pause; `-t 0` → `--duration must be positive` | verified | On a starved CPU the slot is dropped (counted) and the timeline still ends exactly on time — same policy as upstream (`video_drops`, `out_slot` advances) |
| `--mic-backend` | `README.md:263`, `main.rs:254` (clap enum `sck`/`avcapture`) | `src/cli.rs:185`, `src/record.rs:839` | Unit `mic_backend_maps_upstream_values`: `sck`/`avcapture` → pipewire with a message, `pipewire`/`pulse` accepted, `jack` → `unknown --mic-backend "jack": use pipewire or pulse` | n-a-ok | Different values necessarily; the port takes a string and validates in `prepare`, so an invalid value prints a message instead of clap's enum list |
| `--test-pattern [--noise]` (flash + beep) | `README.md:264`, `synth.rs` | `src/cli.rs:190-193`, `src/synth.rs` | Every recording above; `sync` measures beeps at 0.0 ms; `--noise` used by `pattern-4k-hevc`/`pattern-4k-long`; unit `pattern_timeline`, `beep_starts_on_each_second` | verified | Help omits upstream's "implies --mic-backend avcapture" (meaningless here) |
| `--quiet` | `main.rs:197` | `src/cli.rs:123` | Used throughout; `-q` suppresses the start line and status, still prints the path | verified | Same |
| hidden `--keep-aspect`, `--mic-name` compatibility flags | `main.rs:117,147` | `src/cli.rs:49,73` | Parse; `--mic-name` used to re-find a renamed device (`mic_matching` unit) | verified | Same as upstream (kept for old settings files) |
| `--help` texts for every flag | `README.md:240-273`, `main.rs:101-263` | `src/cli.rs` | Ran `record --help` and compared flag by flag | deviates | Flag names/aliases/defaults all match. Text drops upstream's limits: `--keyframe` (0.5–4), `--stream-bitrate` preset numbers, `--live-resolution` "keeps its shape, never above the capture" — see G11 |
| `--replay SECS` (15–300) | `README.md:265`, `live/replay.rs` | `src/cli.rs:142`, `src/live/mod.rs:25` | `--replay 5` → `--replay must be 15-300 seconds`; `--replay 30` → `the replay buffer and streaming are not implemented yet (phase 1D)` | pending-merge | Range validation is in place and tested now |
| `--stream DEST` (repeatable, youtube/twitch/kick/rtmp(s)/WHIP) | `README.md:266`, `live/{rtmp,whip}.rs` | `src/cli.rs:147`, `src/live/mod.rs:139-260` | Unit `services_expand_and_are_recognized`; a run with `--stream youtube` fails with the phase 1D message | pending-merge | Presets, URLs and clamps are ported and tested |
| `--no-record` (replay/stream only) | `README.md:267` | `src/cli.rs:150,371` | `--no-record` alone → `--no-record needs --replay or --stream` | pending-merge | The rule is enforced; nothing can succeed until 1D lands |
| `--live-resolution`, `--stream-bitrate`, `--stream-codec`, `--keyframe`, `--vbr`, `--audio-bitrate` | `README.md:268-273`, `live/mod.rs` | `src/cli.rs:153-175`, `src/live/mod.rs:218` | Unit `settings::tests::live_encode_fits_every_enabled_destination`, `live::tests::{platform_presets, shared_preset_fits_every_destination}` (part of the 286 passing) | pending-merge | Parsing, clamping (keyframe 0.5–4, bitrate 500–80000, audio 64–320) and the shared preset are ported |

## Command line: the other commands

| Feature | Upstream source | Port source | Evidence I checked | Classification | Note |
|---|---|---|---|---|---|
| `devices` lists displays, windows, cameras, mics (+ latency) | `README.md:277`, `main.rs:606-660` | `src/main.rs:152-212` | Ran bare (all unavailable, exit 0), on Xvfb (display + window), in the audio harness (mic + `Output latency … 5.3 ms`), on headless sway (clear "picker chooses" text) | deviates | Output matches upstream's layout except: display lines gain a `primary` marker, and the latency line says `unavailable (…)` when nothing reports one (upstream prints nothing). See G11 |
| `probe FILE` (playable, tracks, codecs, decode every sample) | `README.md:278-279`, `probe.rs:16` | `src/main.rs:299`, `src/media/probe.rs:64` | Ran on finalised, fragmented, missing, random and truncated files; exit 1 with `no moov box` for garbage; every selftest decodes with `ffmpeg -v error -f null` too | deviates | **Duration is wrong on fragmented files** and the track lines are a different format from upstream — G2 |
| `permissions [--request]` (screen/mic/camera, prompts) | `README.md:104-118,280`, `tcc.rs:183,257` | `src/main.rs:214-241`, `src/capture/screen/mod.rs:539-599` | Ran bare (no portal → `unavailable - …; PipeWire NOT running`), in the harness (`not granted yet - ScreenCast v5; monitors; cursor hidden/embedded; PipeWire running`), and `--request` (grant printed, token 0600, `selftest screen-token`) | deviates | Linux equivalent of TCC, fine. But `--request` does **not** ask again when a grant is stored (upstream clears the decision first) — G5 |
| `stop` (finalizes, prints the file) | `README.md:281,213`, `record.rs:652` | `src/record.rs:814` | Selftest `timeout-stop` (120 frames after `rec-rs stop`) and `gui`; with nothing running: `no recording in progress`, exit 1 | verified | Byte-for-byte the same logic as upstream, pidfile path differs |
| `pause`/`resume` (SIGUSR1/SIGUSR2) | `README.md:211-212`, `record.rs:642` | `src/record.rs:804`, `src/main.rs:272-274` | Selftest `pause-resume` (300 frames, sync held) and `gui` (CLI pause/resume reaches the app) | verified | Same signals |
| `save-replay` (SIGINFO → MP4) | `README.md:214,164`, `record.rs:556-565` | `src/main.rs:276-284`, `src/record.rs:74` | With nothing running: `no recording in progress`. With a running recording it prints the phase 1D message | pending-merge | Uses `SIGRTMIN+1` (Linux has no SIGINFO), as planned |
| `stream-key DEST [--delete]` (OS key store, echo off) | `README.md:283-285`, `main.rs:318-352` | `src/main.rs:285-296` | Errors immediately with `stream keys in the Secret Service are not implemented yet (phase 1D); set REC_STREAM_KEY …` | pending-merge | The command exists and does not silently ignore the request |
| `gui [--start-recording] [--settings] [--window ID]` | `README.md:122`, `main.rs:38-53` | `src/main.rs:47-66`, `src/app/mod.rs:1554` | `gui --start-recording` in `selftest gui`; `--window` wired into `settings.window`; `--settings` pushes `ShowSettings` | verified | `--start-recording` exercised by `selftest gui`; `--settings` and `--window` are desktop checks. `--test-pattern` is added (hidden) for the headless self-test |
| hidden `play FILE` (reference player) | `main.rs:69-75`, `play.rs` | `src/main.rs:303-308` | `play` errors with `the reference player is macOS-only and is not ported; use ffplay FILE or mpv FILE` | n-a-ok | Deliberate, not silent |

## Main window and app behaviour

| Feature | Upstream source | Port source | Evidence I checked | Classification | Note |
|---|---|---|---|---|---|
| Live preview of exactly what is recorded, composited, ≤ 30/s | `README.md:128-132`, `ui.rs:143`, `app/mod.rs:685` | `src/app/ui.rs:259`, `src/app/preview.rs`, `src/app/mod.rs:670` | `selftest gui` logs `preview frame 704x396`; unit `app::preview::tests::*` (newest-only, BGRA byte-exact, NV12/I420 BT.709, low power); UI test `the_composited_preview_moves_the_overlay_after_set_pip` (reads the overlay position out of real preview frames) | verified | The recording feeds the preview through `Options::preview` (`Session::options`), so it keeps running during a recording |
| Preview drawn 1:1 at the area's pixel size | `README.md:128-131`, `ui.rs:224-238` | `src/app/ui.rs:276-330`, `src/app/preview.rs` | Unit `the_preview_target_is_the_area_in_pixels`, `frames_are_shown_byte_for_byte_without_a_second_scale`; snapshot `tests/snapshots/main-window.png` | verified | — |
| Preview low-power (half resolution, 15 fps) | `README.md:131`, `ui.rs:944` | `src/app/ui.rs:1471`, `src/app/preview.rs:439`, `src/app/mod.rs:1218` | UI test clicks the checkbox and asserts `preview_low_power: true` in `settings.json`; unit asserts the target halves | verified | `PARITY.md` still marks this `todo` (wrong — see G12) |
| Nothing drawn while the window is hidden | `README.md:131-132`, `app/mod.rs:685` | `src/app/mod.rs:915,1219`, `preview.rs` (Tap) | Unit `only_the_newest_frame_is_kept_and_none_while_hidden`; `hide_main` sets `tap.set_wanted(false)` | verified | — |
| Nothing drawn while the screen is locked | `README.md:131` | — | No logind/`LockedHint` code anywhere (a grep for logind, LockedHint and login1 in `src/` finds nothing) | claimed-unverified | Only winit's `Occluded` event can stop the redraw; upstream *polls* the window's occlusion each pass (`macos.rs:18-24`, `app/mod.rs:1164-1173`) because the event is unreliable. Wayland generally sends no occlusion state at all, so a locked screen may keep the preview converting frames — G8 |
| Own windows excluded from capture | `README.md:129`, `screen.rs` (SCK filter) | — | a grep for "exclude"/"own window" in `src/` finds only comments; `start_recording` hides nothing and adds no hint | missing | On X11 the app window (with its live preview) is in the picture, and the port only documents that. Its own phase 1A note says "The app (1E) must hide its own windows while recording" — not done. G3 |
| Scene: Screen (display or window), Lanczos scaled | `README.md:133-134` | `src/app/ui.rs:911`, `src/gpu/mod.rs` | Goldens `screen`, `screen-4k-to-1080`; unit `a_screen_at_the_output_size_is_bit_exact`, `lanczos_matches_a_cpu_reference`; my X11/portal recordings | verified | — |
| Scene: Screen + Cam, drag anywhere, corner snap | `README.md:135-141`, `ui.rs:309` | `src/app/ui.rs:334-478`, `src/compose.rs:215,239` | Unit `compose::tests::{snapping, corners_and_free_positions}`; UI test `dragging_the_overlay_snaps_to_a_corner`; goldens `screen-cam-*` | verified | Ghost outline while dragging is an extra, harmless |
| Overlay shapes/roundness/size/border/shadow/mirror, in sidebar and settings | `README.md:136-141`, `ui.rs:309-387` | `src/app/ui.rs:479-620`, `src/app/widgets.rs` | Six shapes recorded; unit `every_shape_matches_the_mask_geometry`, `renders_overlay_with_border_into_the_frame`; goldens `screen-cam-circle/rounded/pill/square-border/squircle-shadow`; snapshot | verified | Same five shapes plus `rect` |
| Overlay changes live while recording, baked in | `README.md:139-141` | `src/record.rs:606`, `src/media/writer.rs:550` | Unit `scenes_and_camera_changes_apply_from_the_next_frame`; UI test `the_composited_preview_moves_the_overlay_after_set_pip`; `set_pip` wired from every drag/slider change | verified | — |
| Scene: Camera full frame, cover-fitted, mirrored | `README.md:142` | `src/gpu/mod.rs`, `src/app/ui.rs:911` | Unit `the_camera_scene_covers_the_frame`, `mirroring_flips_the_camera`; golden `cam`; ran `--scene cam --cam` | verified | — |
| Switch scenes live while recording, only if started with a camera | `README.md:144-145` | `src/app/mod.rs:369`, `src/app/ui.rs:911-950` | Unit `scenes_and_camera_changes_apply_from_the_next_frame`; `can_switch_live`; the picker disables scenes the session cannot draw and shows a notice when refused | verified | — |
| Sources: display, entire display or a window, camera, mic, system audio | `README.md:148-153`, `ui.rs:579` | `src/app/ui.rs:911-1063`, `src/app/mod.rs:381` | `load_devices` fills displays/windows/cameras/mics; `selftest gui`, `devices` on Xvfb and in the audio harness | verified | Lists are filled from the real backends (X11, PipeWire, V4L2); a real camera/mic inside the app is a desktop check. Refresh is manual (↻); no hotplug watch — G9 |
| Record/Stop button, timer | `README.md:154`, `ui.rs:388-448` | `src/app/ui.rs:622-720` | `selftest gui` records twice through the app; unit `state_shows_the_recording_clock_and_pause` | verified | `PARITY.md` marks this `todo` (wrong — G12) |
| Pause/Resume button | `README.md:154` | `src/app/ui.rs:655-670` | `selftest gui` pauses/resumes through the CLI and the button handler is the same `Cmd::TogglePause` | verified | `PARITY.md` marks this `todo` (wrong — G12) |
| Audio mixer: meter with peak hold above fader, mute, dB, after the fader | `README.md:155-160`, `ui.rs:450-533` | `src/app/ui.rs:730-910`, `src/app/mod.rs:143-200` | Units `meters_decay_and_hold_peaks`, `a_muted_channel_greys_its_meter`; UI tests `fader_follows_drag_click_scroll_and_double_click`, `the_mute_button_toggles`, `the_level_can_be_typed`; writer levels are post-fader (`levels[i] = peak * meter_gain`) | verified | `PARITY.md` marks mixer/faders `todo` (wrong — G12) |
| Fader −∞..+24 dB, 0 dB marked, drag/click/scroll/double-click/type | `README.md:156-158` | `src/app/widgets.rs:626-800`, `src/app/ui.rs:1060-1130` | Ported upstream tests keep their assertions (`fader_scale`, `fader_follows_the_pointer`, `fader_scrolls_in_fine_steps`, `disabled_fader_ignores_input`, `mute_button_toggles`, `levels_as_text_and_typed` incl. `-∞`/`off`) | verified | — |
| Level changes apply live to every track | `README.md:158-159` | `src/app/mod.rs:356`, `src/record.rs:584`, `writer.rs:726` | Unit `fader_glides_and_passes_unity_through`; `apply_volumes` on every change; `set_volumes` reaches the writer | verified | The recording/live-file path is verified; the replay/stream targets are phase 1D (pending-merge) |
| Reveal Last / Open Folder | `README.md:163`, `macos.rs` | `src/app/linux_desktop.rs`, `src/app/ui.rs:207-240,720` | Unit `file_uris_are_escaped`, `reveal_asks_file_manager1_for_the_file`; code falls back to `xdg-open` on the folder | verified | The FileManager1/xdg-open logic is unit-tested; seeing the file selected in Nautilus/Dolphin/Thunar is a desktop check. `PARITY.md` marks it `todo` (wrong — G12) |
| Replay buffer card (15–300 s, memory use, Save Replay) | `README.md:164-166`, `ui.rs:676` | `src/app/ui.rs:1065-1116` | UI test `main_window_shows_every_card`; the card is present and shows the phase 1D hint; `replay_stats` is a stub | pending-merge | — |
| Stream card (YouTube/Twitch/Kick switches, Add Server, Go Live, keys) | `README.md:168-181`, `ui.rs:707` | `src/app/ui.rs:1120-1300` | UI tests `go_live_needs_a_stream_key`, `main_window_shows_every_card`; key save/delete route to the phase 1D stub with a message | pending-merge | — |
| Per-destination live status and LIVE n/m badge | `README.md:176-179`, `ui.rs:829` | `src/app/ui.rs:65-100,1300` | Code present (`streams`, `status`, `LIVE {live}/{}`); statuses come from the 1D stub | pending-merge | — |
| One encode for every destination, within all limits | `README.md:180-182`, `settings.rs:315-340` | `src/settings.rs:329`, `src/live/mod.rs:218` | Unit `live_encode_fits_every_enabled_destination` (YouTube 4K60 → 2160p HEVC 35 Mb/s; +Twitch → 1080p 6 Mb/s; +Kick → 8 Mb/s) | pending-merge | Maths verified; no encode yet |
| Replay/stream run with or without a recording, survive window close, gaps bridged | `README.md:182-184` | `src/app/mod.rs:708-758,827` | `live_wanted`, `sync_preview` keep a session while the window is hidden; nothing to encode yet | pending-merge | Lifecycle model is ported |
| Settings window: folder, container, resolution, fps, codec, bitrate, cursor/clicks, audio devices/tracks, camera + overlay, live encode | `README.md:185-204`, `ui.rs:851-1060` | `src/app/ui.rs:1331-1610` | UI test `the_settings_window_round_trips_to_settings_json`; snapshot `settings-window.png`; section-by-section comparison with upstream (`OUTPUT`, `VIDEO`, `AUDIO`, `LIVE ENCODE`, `CAMERA`) — the sections, controls, labels and defaults are upstream's | verified | The port adds a "CAMERA OVERLAY" section and a "Settings are locked while recording" note; both exist upstream too (`ui.rs:866,871`) |
| Live encode section locked while streaming | `README.md:204`, `ui.rs:977` | `src/app/ui.rs:1520` | UI test `the_live_encode_section_is_locked_while_streaming` | pending-merge | The gate is in place |
| Settings persist in a per-user JSON file | `README.md:206`, `settings.rs:171` | `src/settings.rs:179` | Unit `settings_live_in_the_xdg_config_dir`, UI round-trip test | verified | Path is `$XDG_CONFIG_HOME/rec-rs/settings.json` instead of Application Support (documented) |
| App menu (Settings ⌘,, Hide ⌘H, Quit ⌘Q, Window ▸ Minimize/Close/Show, About) | `menubar.rs:214-255` | `src/app/ui.rs:207-257` | UI test `the_app_menu_has_updates_unless_flatpak`; menus are the toolbar ⋯ plus Settings… | deviates | No About, no Hide (Ctrl+W hides to the tray instead of ⌘H), Minimize/Close come from the native decorations. Window.Show is the tray's Show Rec Window |
| Dock icon and Dock menu | `dock.rs` | — | — | n-a-ok | No Dock on Linux; the tray and the app menu carry the same commands |
| Close the window: keep running, stop the preview | `README.md:207-210`, `app/mod.rs:526` | `src/app/mod.rs:643,1411` | `CloseRequested` → `hide_main`; preview unit test (`only_the_newest_frame_is_kept_and_none_while_hidden`); the code keeps the recording running because nothing stops it. `selftest gui` does not close the window, so the "keeps recording" half is a code read, not a run | verified | The close-to-tray path is code + unit verified; tray visibility while recording is a desktop check |
| Quit from menu/tray/SIGTERM/logout finalises first | `README.md:210`, `app/mod.rs:797` | `src/app/mod.rs:967`, `src/app/mod.rs:1506` | `selftest gui`: SIGTERM mid-recording leaves a probed file and the app exits 0 | verified | — |
| Menu bar item: red dot, elapsed time, LIVE, full menu | `README.md:205-206`, `menubar.rs:100-212` | `src/app/tray.rs:63-140` | Units `menu_matches_upstream_in_order`, `titles_match_upstream`, `icon_shows_a_red_dot_while_capturing` | needs-real-desktop | A StatusNotifierItem needs a panel; in a bare session `start` returns None and the app continues |
| Global hotkeys ⇧⌘R / ⇧⌘S (⌘ → Ctrl) | `README.md:216-221`, `hotkey.rs:105` | `src/hotkey.rs:19-120`, `src/app/mod.rs:714` | Units on triggers, backend choice, keycodes and lock masks; `Ctrl+Shift+S` registered only while the replay buffer is on; `Ctrl+Shift+R` fallback to XGrabKey | needs-real-desktop | The portal dialog and X11 grabs cannot be exercised headlessly |
| Hotkey conflict shown in the UI | `README.md:220` | `src/app/ui.rs:141`, `src/app/tray.rs:65,110` | Unit `menu_matches_upstream_in_order` (label row), `registration_results_are_queued_once`; the window shows `Ctrl+Shift+R unavailable` | verified | The wiring and the label are unit tested; a real grabbed combination is a desktop check |
| Single instance: a second launch shows the window | Launch Services, `app/mod.rs:498` | `src/app/control.rs`, `src/main.rs:1606` | `selftest gui` second launch handed over; units `the_second_launch_talks_to_the_first`, `a_stale_socket_is_replaced` | verified | Unix socket in `$XDG_RUNTIME_DIR`, else `/tmp/rec-rs-$UID` |
| Check for Updates… | `README.md:96-101`, Sparkle appcast | `src/app/updater.rs`, `src/app/ui.rs:212` | Units on release parsing, numeric compare, offline; UI test hides it in Flatpak; no network check in the self-test | deviates | Opens the GitHub release page instead of replacing the bundle (documented); a Flatpak is updated by Flatpak, an AppImage/distro build by the user |
| Add the AppImage to the desktop menu | `README.md:99-101`, `install.rs` | `src/app/install.rs`, `src/app/mod.rs:950` | Ported upstream tests (`installs_the_desktop_entry_and_icon`, `replaces_an_older_entry`, `compares_real_paths`, `never_leaves_the_destination_missing`, `staging_is_apart_from_other_copies`) | verified | Ported upstream tests; appearing in Nautilus/Dolphin/Thunar is a desktop check |
| AppImage/Flatpak self-update | `README.md:96-101` | — | — | pending-merge | Phase 1F (`bb/1f-…`), not in this tree: no `packaging/` directory |
| Re-launch with responsibility disclaimed / "Re-confirm screen recording" prompt | `README.md:113-117,421`, `tcc.rs` | — | — | n-a-ok | No TCC; the portal owns attribution and the restore token avoids re-prompts |

## Output format guarantees

| Feature | Upstream source | Port source | Evidence I checked | Classification | Note |
|---|---|---|---|---|---|
| Video: H.264 High / HEVC Main (`hvc1`) / ProRes 422 | `README.md:325-327`, `writer.rs` | `src/media/writer.rs`, `src/media/mp4.rs` | `pattern-1080-h264` (h264/High), `pattern-hevc` (hevc/Main), `pattern-prores` (prores/Standard), all BT.709 and fully decodable | verified | Hardware encoder selection needs `/dev/dri` (absent here), so only the software encoders are proven on this host |
| BT.709 limited range tagging | `README.md:327` | `gpu/yuv.rs`, `writer.rs` caps | ffprobe on every codec: `bt709/bt709/bt709`, `color_range=tv`; unit `the_output_is_bt709_limited_range`, `yuv::tests::known_bars` | verified | — |
| Keyframe every 2 s | `README.md:327` | `writer.rs` (`key-int-max`/`gop-size`) | `pattern-1080-h264` max gap 1.000 s (x264 scene cut on the flash), all gaps < 2.2 s | verified | Same maximum-gap semantics |
| Constant frame rate on an exact 1/fps grid | `README.md:335-337` | `writer.rs:1052-1140` | ffprobe PTS 0.000/0.016667/0.033333…; `-t 1` → 60 samples, duration 1.000 s; `pause-resume` → 300 | verified | Drops advance `out_slot` so the grid and the duration stay exact |
| Track 2: mix AAC-LC 48 kHz stereo 256 kb/s, enabled | `README.md:328` | `writer.rs` track layout, `mp4.rs` finalise | `probe` on `tracks`: `aac 48000 Hz, 2 ch … enabled alt-group 1`, `elst media_time=2048`; ffprobe `2:256` | verified | — |
| Track 3/4: system stereo 256, mic mono 192, disabled alternatives | `README.md:329-330` | same | `probe` `tracks`: index 2/3 `disabled`, `2 ch`/`1 ch`, 256/192 kb/s measured | verified | — |
| Track grouping: alternate group with the mix enabled | `README.md:332-334` | `src/media/mp4.rs` (`tkhd` alternate_group, flags) | Unit `finalize_builds_a_regular_movie_and_keeps_the_media_bytes`; `probe tracks`: `alt-group 1`, mix enabled, stems disabled; `audio-tracks stems`: no group, both enabled | verified | qtmux cannot write this; the finalise does |
| One audio source → a single audio track | `README.md:332` | `writer.rs` layout | `stems` with `--no-mic` → video + one system track | verified | One observed run produced a video-only file — G15 |
| Single clock, audio never stretched | `README.md:338-340` | `media_types.rs`, `writer.rs` | `sync` selftest: mean/worst 0.0 ms; `mixer` resampler units | verified | — |
| Flash/beep sync 0.0 ms in the file, including across pauses | `README.md:338-340`, `PERFORMANCE.md:107-130` | `writer.rs`, `mp4.rs` edit list | `sync` (range [+0.0,+0.0]), `pause-resume`, `audio-gaps` | verified | — |
| Levels: fader and mute before everything, ≤ 20 ms ramp, 0 dB untouched | `README.md:341-344` | `mixer.rs` (`Volume`, ramp), `writer.rs` | `gain` (system −12, mic −18 dB exact); unit `fader_glides_and_passes_unity_through` | verified | — |
| System audio latency shift (output device latency) | `README.md:345-348` | `capture/audio/mod.rs:83`, `record.rs` | `devices` under the audio harness prints 5.3 ms; units `collector_combines_the_latency_parts`, `node_latency_property_is_a_fraction_or_frames`; applied in `set_audio_offsets` | verified | The value and the shift are verified; audible alignment on a real device is a desktop check |
| Webcam lag adaptation (emission waits for the slowest source) | `README.md:349-351` | `writer.rs` (latency peak, decay 50 ms/s, 0.05–0.3 s) | `camera-lag` selftest: 80 ms late synthetic camera → 120/120 frames, no drift; unit `camera_latency_widens_the_lag_and_decays` | verified | Proven with an 80 ms late synthetic camera; a real 70–120 ms camera is a desktop check |
| Crash-safe: fragments every 2 s, regular movie on stop | `README.md:352-354`, `PERFORMANCE.md:101-106` | `writer.rs` (`fragment-duration=2000`), `mp4.rs` finalise | `crash` (SIGKILL at 4.5 s → 238 frames, fragmented, decodes), `defrag` (same 120 samples and frame hashes; no moof/mvex), `finalize-crash` | verified | `probe` misreports the duration of the crashed file — G2 |
| Edge to edge, source aspect, no bars, never cropped | `README.md:355-356` | `compose.rs:122`, `cli.rs:200` | Unit `fill_is_edge_to_edge_for_the_same_aspect`, `tests::sizes` for 16:10/portrait/odd sizes; goldens | verified | — |
| Window transparent corners filled with the edge colour | `README.md:356-357` | `compose.rs:386`, `gpu/mod.rs` (`corner_fill`) | Unit `the_corner_fill_replaces_transparent_window_corners`; `corner_fill` enabled when recording a window | verified | Unit-proven fill; a real rounded window is a desktop check |
| Camera: app passes the preview camera into the recording and back; cold CLI camera settles 2 s | `README.md:358-360`, `record.rs:201,375` | `src/record.rs:408-432` | CLI `camera` selftest passed (2 s settle then 120 frames, first frame the camera's red); there is **no** `take_camera`/`start_with_camera` in the port | deviates | The CLI half matches; the app half is missing — G1 |
| Audio quality: 48 kHz capture, windowed-sinc resampler, limiter < −1 dBFS | `README.md:361-365` | `mixer.rs` | Units `resamples_seamlessly`, `resampler_removes_content_above_nyquist`, `limiting`; `pattern-system-audio` 44.1 kHz → 48 kHz, spacing 1.000 s | verified | — |
| Pause cuts whole slots, audio at the exact sample, no drift | `README.md:366-368` | `media/pauses.rs`, `writer.rs` | Units `output_slots_stay_contiguous_across_pauses`, `keep_cuts_captures_at_pause_boundaries`; `pause-resume` | verified | — |
| AAC priming skipped via an edit list | `PERFORMANCE` note, `writer.rs` (AVAssetWriter equivalent) | `src/media/mp4.rs` | `probe` shows `elst media_time=2048` on every audio track; `sync` first beep at 0.0 ms; `REC_AAC_PRIMING` override | verified | Upstream's AVFoundation writes it; GStreamer's muxers cannot, so the finalise does |
| `probe` reports the file like QuickTime/AVFoundation | `probe.rs:16-104` | `src/media/probe.rs:64-190` | Fragmented crash file: `probe` says `duration: 1.984s`, ffprobe and the samples say 4.000 s / 3.950 s. Finalised 2 s file: `probe` 2.049 s vs ffprobe 2.0067 s. Track lines use `track 0 : video h264 1920x1080 @ 56.500 fps [mp4 #1 enabled alt-group 0 113 samples]` | deviates | G2: wrong duration and a different line format (no `vide avc1 enabled=… start/dur/kb/s`, no nominal fps) |

## OBS comparison rows rec-rs claims

| Feature | Upstream source | Port source | Evidence I checked | Classification | Note |
|---|---|---|---|---|---|
| Display capture | `README.md:371`, `screen.rs:52` | `src/capture/screen/portal.rs`, `pipewire.rs`, `x11.rs` | `pattern-screen` (1280x720/60, 180 frames, 180 distinct), `screen-x11` (1280x800, motion) | verified | CPU frames, not dmabuf (see below) |
| Window capture | `README.md:372` | `x11.rs`, `portal.rs` | `screen-window` + manual `--window gstreamer`/by id → 400x300 | deviates | Verified on X11; on Wayland only where the portal offers window sources (wlr refuses, with a message) |
| Region / crop | `README.md:373` | `format.rs`, `screen/mod.rs` | `screen-region` + manual X11 region | verified | CLI-only, as upstream documents |
| Application capture (all windows of an app) | `README.md:374` | — | — | n-a-ok | Upstream gap too |
| Scenes: three fixed, live switch, cut only | `README.md:375` | `compose.rs`, `gpu/mod.rs`, `app/ui.rs` | Units + goldens + `can_switch_live` | verified | Same limitation, as documented |
| Multiple displays/sources at once | `README.md:376` | — | — | n-a-ok | Upstream gap too |
| System audio | `README.md:377` | `capture/audio/pipewire.rs` | `pattern-system-audio` (48k + 44.1k) | verified | — |
| Microphone | `README.md:378` | `capture/audio/pipewire.rs` | `pattern-mic` (default, by name, missing) | verified | Real/Bluetooth devices need a desktop |
| Webcam overlay with shapes/border/shadow/mirror, live while recording | `README.md:379` | `capture/camera/*`, `gpu/*`, `record.rs` | Synthetic camera recordings + goldens + units; a real V4L2 camera is not available here | verified | Synthetic camera + goldens; a real V4L2 camera is a desktop check |
| Separate audio tracks (up to 4) | `README.md:380` | `writer.rs` | `tracks` (4), `audio-tracks` | verified | — |
| Per-source gain and mute, live | `README.md:381` | `mixer.rs`, `app/ui.rs`, `writer.rs` | `gain`, `fader_glides_and_passes_unity_through`, mixer UI tests | verified | — |
| Audio filters (noise suppression, compressor) | `README.md:382` | — | — | n-a-ok | Upstream gap too |
| Hardware H.264/HEVC | `README.md:383` | `writer.rs` (VA candidates) | No `/dev/dri` in the container; `dev/ELEMENTS.md` documents the fallback | needs-real-desktop | The candidate selection and fallback are in code; only software is proven here |
| ProRes 422 | `README.md:384` | `writer.rs`, `mp4.rs` | `pattern-prores`: prores/Standard, bt709, .mov only | verified | — |
| 10-bit / HDR | `README.md:387` | — | — | n-a-ok | Upstream gap too (8-bit SDR) |
| Constant frame rate | `README.md:388` | `writer.rs` | ffprobe PTS grid | verified | — |
| Crash-safe files | `README.md:389` | `writer.rs`, `mp4.rs` | `crash`, `defrag`, `finalize-crash` | verified | — |
| Pause/resume, sample-accurate, app and CLI | `README.md:390` | `record.rs`, `writer.rs`, `app/mod.rs` | `pause-resume`, `selftest gui` | verified | — |
| Replay buffer 15–300 s | `README.md:391` | `live/mod.rs` | Range validation + stub | pending-merge | — |
| Streaming RTMP/WHIP, multi-destination, HEVC, reconnect | `README.md:392` | `live/mod.rs` | Preset/URL units; no outputs | pending-merge | — |
| Screen scaling: GPU Lanczos | `README.md:393`, `PERFORMANCE.md:165-193` | `gpu/mod.rs` | `the_scaler_matches_ffmpeg_lanczos`: 50.27/58.10 dB (4K→1080p), 51.36/56.95 (5K→1080p); `downscaling_a_one_pixel_pattern_leaves_only_its_mean`; golden `screen-4k-to-1080` | verified | Quality reproduced against swscale/zimg; 77 ms/frame 4K→1080p on lavapipe, so throughput on a real GPU is a desktop check |
| Live preview | `README.md:394` | `app/preview.rs` | See the preview rows | verified | 8.15 ms CPU per 4K preview frame; upstream's shipped path costs 0.25–0.39 ms and its old CPU-readback path 1.3–1.9 ms — G10 |
| Level meters (peak with hold, after the fader) | `README.md:395` | `app/ui.rs`, `writer.rs` | Units + UI tests | verified | — |
| Global hotkey (fixed) | `README.md:396` | `hotkey.rs` | Units; needs a desktop | needs-real-desktop | — |
| Menu bar control | `README.md:397` | `app/tray.rs` | Units; needs a panel | needs-real-desktop | — |
| Headless CLI | `README.md:398` | `src/main.rs` | All the CLI runs above | verified | — |
| Footprint ~22 MB app, ~6% CPU at 4K60 screen-only | `README.md:399`, `PERFORMANCE.md:24-80` | — | No packaging in this tree; no 4K60 screen-only CPU measurement possible without a display | pending-merge | Phase 1F; the CPU claim also needs a real GPU/display |
| Encoder input: IOSurface straight to VideoToolbox (zero copy) | `README.md:400`, `PERFORMANCE.md:78` | `capture/screen/*` | Frames are CPU `Vec<u8>`/`Arc<[u8]>`; dmabuf import is not present | missing | Documented as phase 1C/2 work; on Wayland this is a real bandwidth cost |

## Known gaps upstream documents

| Gap | Upstream source | Port source | Evidence I checked | Classification | Note |
|---|---|---|---|---|---|
| Three fixed scenes, no layout editor/transitions; live switch needs a camera start | `README.md:404-405` | `compose.rs`, `app/ui.rs` | Same limitation; picker disables impossible scenes | verified | Parity |
| No application or multi-display capture; region is CLI-only | `README.md:406-407` | same | Same | n-a-ok | Parity |
| No audio filters, hotkeys not configurable | `README.md:408` | `mixer.rs`, `hotkey.rs` | Same | n-a-ok | Parity |
| Streaming: no SRT/multitrack, one encode, live ≤ recording size, settings need a restart | `README.md:409-414` | `live/mod.rs`, `app/ui.rs` | Model encoded in the preset maths; no outputs | pending-merge | Parity once 1D lands |
| HEVC to YouTube tested only against a local server | `README.md:412-413` | — | — | pending-merge | 1D |
| 8-bit SDR only | `README.md:415` | `writer.rs` | Same | n-a-ok | Parity |
| Webcam overlay burned in, no separate camera track | `README.md:416` | `gpu/*`, `writer.rs` | Same (compositor before the encoder) | verified | Parity |
| Output latency read once at start | `README.md:417-418` | `capture/audio/mod.rs:83` | Read once in `start_sources`; a device switch needs a restart | verified | Parity |
| A non-48 kHz mic was resampled linearly per buffer upstream | `README.md:419-420` | `mixer.rs` | The port uses the windowed-sinc resampler for every path | verified | Better than upstream, documented |
| Re-confirmation prompt for screen recording | `README.md:421` | — | — | n-a-ok | Portal token instead |
| Region capture is CLI-only (no UI) | `README.md:407` | `app/ui.rs` | No region control in the app | n-a-ok | Parity (upstream has none either); `PARITY.md` lists it as `todo` for 1E, which would *exceed* upstream |

## PERFORMANCE.md claims

| Claim | Upstream source | Port source | Evidence I checked | Classification | Note |
|---|---|---|---|---|---|
| Under overload frames are dropped at a 0.5 s backlog instead of growing memory | `PERFORMANCE.md:82-100` | `writer.rs:174` (`MAX_BACKLOG_SECS = 0.5`), bounded appsrc/queues | Constant matches; unit `a_silent_or_far_future_source_cannot_grow_memory`; my 4K run dropped slots (20 samples for 1 s) with an exact 1.000 s timeline | verified | Same policy |
| Crash safety (fragments, regular movie after a normal stop) | `PERFORMANCE.md:101-106` | `writer.rs`, `mp4.rs` | `crash`, `defrag` | verified | — |
| A/V sync measured with flash+beep, output-latency shift | `PERFORMANCE.md:107-130` | `record.rs`, `writer.rs` | `sync`, `audio-gaps` 0.0 ms; latency value from `devices` | verified | Sync is 0.0 ms on the synthetic source; a real sink/Bluetooth device is a desktop check |
| Picture quality: native capture + Lanczos per plane (5K→4K VMAF 100) | `PERFORMANCE.md:165-193` | `gpu/mod.rs` | Scaler PSNR reproduced; VMAF not computed on Linux, and 5K capture needs a real display | needs-real-desktop | The scaler's PSNR was reproduced, but the *claim* (VMAF 100 at 5K→4K, native 5K capture) needs a real 5K display and a GPU; no VMAF is computed on Linux |
| App preview costs 0.25–0.39 ms per frame (1.3–1.9 ms before the IOSurface change) | `PERFORMANCE.md:242-247` | `app/preview.rs` | `preview_cost`: 6.61 ms (1080p) and 8.15 ms (4K) per 704x396 frame | deviates | The port is on the CPU-readback path upstream replaced: ~4–6× the 1.3–1.9 ms figure and ~20–30× the shipped 0.25–0.39 ms. The shared-device texture is phase 2 — G10 |
| Live encode adds no measurable CPU | `PERFORMANCE.md:138-163` | — | — | pending-merge | 1D |
| ~6% of one core at 4K60 screen-only, ~34% with the overlay | `PERFORMANCE.md:24-80` | — | Needs a real display + GPU | needs-real-desktop | — |

## Gaps to fix, in priority order

Every gap below has: what is wrong, how to reproduce it, what upstream does, and the files that
likely need to change.

### G1. The app does not hand its camera from the preview to the recording (and back)

- **What:** `start_recording` closes the preview session and starts a new one; `Session::start`
  opens a *cold* camera and, with a camera scene, the writer holds the file for the 2 s settle. Every
  recording started from an open preview therefore re-opens the camera, waits 2 s, and the first
  frames are dark — and stopping a recording throws the camera away instead of returning it to the
  preview. There is no `take_camera`/`start_with_camera`/`spare_cam` anywhere in `src/`.
- **Reproduce:** with a camera scene, open the app, wait for the preview to show the camera, press
  record, and watch the start: ~2 s of dark frames before the file starts, and the camera LED
  restarts each time. The synthetic equivalent shows the same wait
  (`REC_SYNTH_CAM=1` and a camera scene in the app).
- **Upstream:** `record.rs:201,375` + `app/mod.rs:738-777` hand the running `Camera` over, "so a
  recording starts with the exposure already settled" (`README.md:358-360`).
- **Likely files:** `src/record.rs` (add `take_camera` + `start_with_camera`), `src/app/mod.rs`
  (`start_recording`, `stop_recording`, `sync_preview`, a `spare_cam` field), `src/app/ui_tests.rs`
  (a test that the handover happens), `PARITY.md`.

### G2. `probe` reports the wrong duration for fragmented files and a foreign line format

- **What:** on a crash-left fragmented movie `probe` prints half the real duration; on a finalised
  file it is ~43 ms long (AAC priming). The track lines also differ from upstream's: no media type
  (`vide`/`soun`), no codec fourcc (`avc1`/`mp4a`), no `start=`/`dur=`/`kb/s`, and `@ … fps` is the
  *effective* rate (samples/seconds), so a file with dropped slots reads `56.500 fps` where upstream
  prints the nominal `60.000`. Anything that parses `rec-rs probe` output (the README presents it as
  QuickTime-equivalent verification) breaks or mis-reads.
- **Reproduce:**
  ```sh
  REC_TEST_DIR=/tmp/audit scripts/dev.sh scripts/selftest.sh crash
  scripts/dev.sh '$CARGO_TARGET_DIR/release/rec-rs probe /tmp/audit/selftest-*/crash.mov'
  scripts/dev.sh 'ffprobe -v error -show_entries format=duration -of csv=p=0 /tmp/audit/selftest-*/crash.mov'
  ```
  → `probe` prints `duration : 1.984s`, ffprobe prints `4.000000`, and probe's own decode line says
  `pts 0.000..3.950s`. On a finalised 2 s file: `probe` 2.049 s vs ffprobe 2.006667.
- **Upstream:** `probe.rs:16-104` prints `AVURLAsset.duration` (2.000/4.000) and per track
  `vide avc1 enabled=true start=0.000s dur=4.000s 19000.0 kb/s 1920x1080 @ 60.000 fps`.
- **Likely files:** `src/media/probe.rs` (`structure`/`run`: take the duration from the demuxer's
  seekable range or from the decoded samples' last PTS; print upstream's fields, keep `enabled`/
  `alt-group`/`elst` as additions), `src/media/probe.rs` tests, `PARITY.md` (its "same format" claim).

### G3. Rec's own window is captured (X11), and nothing hides it or warns

- **What:** upstream filters Rec's own windows out of the capture, so the preview can stay open
  while recording. The port cannot exclude them through a portal (justified on Wayland) but does not
  do it on X11 either, and does not hide the main window or tell the user when a recording starts.
  The result on X11 is a video of the desktop with Rec's live preview of that same desktop inside it.
- **Reproduce:** on an X11 session: `scripts/dev.sh '$CARGO_TARGET_DIR/release/rec-rs record -t 5 -o /tmp/own.mp4'`
  with the app window visible
  → the file contains the Rec window and its preview. (The port's own phase 1A note already says
  "The app (1E) must hide its own windows while recording".)
- **Upstream:** `README.md:128-129`, ScreenCaptureKit's content filter excludes own windows.
- **Likely files:** `src/app/mod.rs` (hide the main window on recording start on X11, with a notice;
  restore on stop), `src/capture/screen/x11.rs` (subtract the app's own windows from `GetImage`, the
  better fix), `src/capture/screen/portal.rs` (say clearly when the window cannot be excluded),
  `PARITY.md` (the row is `n-a`, but the 1A notes require the app work).

### G4. Default system audio aborts a recording when no default sink exists

- **What:** with `--system-audio` on (the default) and no default PipeWire sink, `record` fails:
  `rec: error: … no default output device for system audio …`. An unavailable *default microphone*
  only warns, following the project rule that "audio never blocks a session"; system audio has no
  such grace. On a Linux box with PipeWire but no sink (headless, HDMI unplugged, a bare X11
  session) the recorder simply cannot record unless the user knows to pass `--no-system-audio`.
- **Reproduce:**
  ```sh
  scripts/dev.sh scripts/screen/headless.sh '$CARGO_TARGET_DIR/release/rec-rs record -t 3 -o /tmp/x.mp4 --no-mic'
  ```
  → `no default output device for system audio …`, no file. Same in a bare container (the screen
  harness starts PipeWire but no sink). Adding `--no-system-audio` succeeds.
- **Upstream:** system audio comes from ScreenCaptureKit and is always available, so this state
  cannot occur; the analogous rule for the mic is warn-and-continue (`main.rs:377-387`).
- **Likely files:** `src/record.rs` (`start_sources`: warn and continue when the *default* system
  source cannot be opened, fail only for an explicit request), `src/capture/audio/mod.rs`
  (`open_system` error text), the no-frames/no-audio warnings, `PARITY.md`.

### G5. `permissions --request` reuses the saved grant instead of asking again

- **What:** upstream documents "Requesting again … first clears any earlier Screen Recording
  decision so that macOS asks again" (`README.md:117-118`). On Linux `--request` calls
  `portal::prepare`, which passes the stored restore token, so the portal reuses it and the dialog
  never appears; the command returns in ~0.1 s and prints `granted`. The hint to delete the token is
  buried in the status text.
- **Reproduce:**
  ```sh
  scripts/dev.sh scripts/screen/headless.sh '$CARGO_TARGET_DIR/release/rec-rs permissions --request'
  ```
  twice → both finish in ~0.1 s (measured 0.115 s and 0.092 s), no dialog, `granted` both times.
- **Upstream:** `tcc.rs:183-230` forgets the decision before prompting.
- **Likely files:** `src/capture/screen/mod.rs:550` (`request_permission`: remove/ignore the token
  before `prepare`, and say so), `src/capture/screen/portal.rs:142` (a "force a dialog" mode),
  `docs`/`PARITY.md`.

### G6. `--mic` takes an optional value; upstream requires `DEVICE`

- **What:** `rec-rs record --mic` parses here and means "default mic"; upstream's clap definition
  (`main.rs:143`) has no `num_args`, so it errors with `--mic <DEVICE>` required. A script written
  for upstream that relies on that error (or a user following `--help DEVICE`) gets different
  behaviour. The help text describes the optional form (`--mic [<DEVICE>]`).
- **Reproduce:** `scripts/dev.sh '$CARGO_TARGET_DIR/release/rec-rs record --mic --test-pattern -t 1 -q -o /tmp/x.mov'`
  → records a file; upstream's clap errors instead.
- **Upstream:** `main.rs:141-143` (`--mic DEVICE`, default `default`).
- **Likely files:** `src/cli.rs:69` (`num_args`/`default_missing_value`), `src/cli.rs:498` (the test
  that asserts the optional form), `README`/`PARITY.md`.

### G7. The "no screen frames" warning lost upstream's permission hint

- **What:** upstream warns `no screen frames after 3s (is Screen Recording permission granted?)`;
  the port warns `no screen frames after 3s` with no hint. On Linux the actionable hint is different
  (portal picker, wrong monitor) but there is none at all, at the most common failure moment.
- **Reproduce:** `scripts/dev.sh '$CARGO_TARGET_DIR/release/rec-rs record -t 4 -o /tmp/x.mov --no-mic --no-system-audio'`
  with no display: stderr has the plain warning, without any pointer at the permission.
- **Upstream:** `record.rs:581`.
- **Likely files:** `src/record.rs` (the warning text), plus a line naming `rec-rs permissions`.

### G8. Screen lock does not stop the preview

- **What:** upstream draws nothing while the screen is locked and measured the effect
  ("about 100% of a core to 2% with the screen locked", `PERFORMANCE.md:248-250`). The port stops on
  `Occluded` and on `hide_main` only; there is no `org.freedesktop.login1` `LockedHint` handling, so
  on a compositor that keeps sending frames during lock the app keeps converting preview frames
  (8.15 ms each at 4K) for nothing.
- **Reproduce:** lock a GNOME/KDE session while the app is open and watch CPU/preview frames
  (`REC_DEBUG=1`); headless cannot.
- **Upstream:** `macos.rs:18-24` (`is_occluded`: covered, minimized, another Space *or the screen
  locked*) polled from `app/mod.rs:1164-1173`, `README.md:131`, `PERFORMANCE.md:248-250`.
- **Likely files:** `src/app/mod.rs` (subscribe to `login1.Session.LockedHint`, fold it into
  `Tap::set_wanted`), `src/app/preview.rs` test.

### G9. Device hotplug is not watched

- **What:** upstream reloads the device lists on AVCapture connect/disconnect and restarts the
  preview if the saved mic now resolves differently (`app/mod.rs:587-600`). The port reloads only
  when the ↻ button is pressed, so a plugged-in mic or camera does not appear until the user
  refreshes, and the preview keeps the old device.
- **Reproduce:** open the app, plug in a USB mic, look at the mic list — unchanged until ↻.
- **Upstream:** `app/mod.rs:587` (`devices_changed`), called from the AVCapture notification.
- **Likely files:** `src/app/mod.rs` (`load_devices`, a udev/PipeWire registry watch or a periodic
  poll that only updates when the set changed), `src/capture/audio/*`, `src/capture/camera/*`.

### G10. Preview costs ~4× upstream's CPU per frame

- **What:** measured 6.61 ms (1080p source) and 8.15 ms (4K source) per 704x396 preview frame; at
  30 previews/s that is ~20–25 % of one core. Upstream measured 1.3–1.9 ms for the CPU-readback path
  it replaced and **0.25–0.39 ms** for the shipped IOSurface path (`PERFORMANCE.md:242-247`), so the
  port is ~4–6× its predecessor and ~20–30× the shipped figure. `PARITY.md` documents the cause (a
  CPU copy per frame until 1C's shared-device `Renderer::texture()` lands) but the cost is real
  today.
- **Reproduce:** `scripts/dev.sh cargo test --release app::preview::tests::preview_cost -- --ignored --nocapture`.
- **Upstream:** Metal texture, no readback.
- **Likely files:** `src/app/preview.rs`, `src/gpu/mod.rs` (`Renderer::texture`), `src/app/mod.rs`
  (`redraw`).

### G11. Small user-visible differences in `--help`, `devices` and `--version`

- **What:** (a) `--help` drops upstream's limits: `--keyframe` "(0.5–4)", the `--stream-bitrate`
  preset numbers, `--live-resolution` "keeps its shape, never above the capture"; (b) `devices` adds
  a `primary` marker to display lines and prints an `Output latency: unavailable (…)` line where
  upstream prints nothing — both change the line format a script may parse; (c) `--version` reports
  `0.1.0` while the pinned upstream is `0.1.2`.
- **Reproduce:** `scripts/dev.sh '$CARGO_TARGET_DIR/release/rec-rs record --help'` (the
  `--keyframe` range is missing) and `… rec-rs devices` in a session with no PipeWire sink (the extra
  latency line).
- **Upstream:** `main.rs:226-243` (help), `main.rs:606-660` (devices), `Cargo.toml` (`0.1.2`).
- **Likely files:** `src/cli.rs` (doc comments), `src/main.rs:152`, `Cargo.toml` (or state the
  version policy).

### G12. `PARITY.md` understates finished work

- **What:** rows marked `todo` with `—` evidence for features that are implemented *and* tested:
  Record/Stop + timer, Pause/Resume, the audio mixer (meter/peak hold/fader/mute/dB), the fader
  range and interactions, Reveal Last/Open Folder, "Cursor and click settings" (1A), "Audio device
  and track layout settings" (1B/1E), "Preview low-power mode" (1E). A reader (or the next worker)
  will think these are unbuilt.
- **Reproduce:** compare each row against `src/app/ui.rs:622-910`, `src/app/ui.rs:1471`,
  `src/app/linux_desktop.rs` and the tests named in this audit.
- **Fix:** update those rows to `done` with the evidence named here. No code change.

### G13. `PARITY.md` overstates three things

- **What:** (a) "probe … track/codec summary in the same format upstream prints" — it is not (G2);
  (b) the "Camera settles 2 s before the file starts" row is `done`, but the app half of upstream's
  guarantee (camera handover) is missing (G1); (c) "Own windows … `n-a`" contradicts the port's own
  1A note that 1E must hide the app's window (G3), and the app neither hides nor warns.
- **Fix:** correct the three rows and link the gaps above.

### G14. Packaging (phase 1F) is not in this branch

- **What:** `packaging/` does not exist, so the Flatpak/AppImage, the desktop file, the metainfo,
  the icons and the release workflow are unproven here, as are the OBS "small footprint" and
  "Check for Updates replaces the bundle" rows. `Check for Updates…` opens the release page.
- **Reproduce:** `ls packaging` → not found; `git branch -a | grep 1f` → `bb/1f-…` exists.
- **Files:** `packaging/**`, `.github/workflows/release.yml` (1F branch); `PARITY.md` should say
  "pending merge" for those rows rather than `todo`.

### G15. Audio flakiness at the edges (observed once)

- **What:** (a) the first `gain` self-test run failed with the mic stem's last beep 3.4 dB hot
  (`beep 1.98s -20.6 dBFS -14.6 dB`) while the host was compiling three other worktrees; the same
  case passed on a re-run (`-23.9 dBFS -17.9 dB`). A 3.4 dB error is not a scheduling wobble a
  level test should produce — it looks like a duplicated or overlapping mic buffer, worth
  reproducing under load with `REC_DEBUG=1`. (b) One `record --test-pattern --scene cam -t 1`
  immediately after another session in the same PipeWire graph produced a **video-only** file with
  no warning; three repeats of the same command on an idle graph produced 4 tracks every time.
  Audio tracks are created lazily when the first buffer arrives, so a slow source plus a short
  `-t` can drop them silently.
- **Reproduce:** `scripts/dev.sh scripts/selftest.sh gain` while the host is busy (my first run failed with the
  numbers above; a re-run passed); for (b), two short recordings back to back in one
  `scripts/audio/harness.sh`.
- **Upstream:** AVAssetWriter adds the audio inputs at session start, so the tracks exist even for a
  very short clip; a busy `AVAssetWriterInput` drops frames but never a track.
- **Likely files:** `src/mixer.rs` (mic buffer de-duplication), `src/media/writer.rs` (create the
  tracks up front, or at least warn when audio was requested and no buffer arrived before the
  finish), `scripts/selftest.sh`.

## Summary

Classification counts over the 143 inventory rows above (each row is one feature or behaviour):

| Classification | Count |
|---|---|
| verified | 89 |
| pending-merge | 19 |
| n-a-ok | 13 |
| deviates | 12 |
| needs-real-desktop | 7 |
| missing | 2 |
| claimed-unverified | 1 |
| **Total** | **143** |

Notes on the counts:

- The `deviates` rows are not all defects: several are justified Linux differences (Wayland display/
  window selection, `--mic-backend` values, `permissions` text, `Check for Updates` opening a page).
  The user-visible defects among them are G1, G2, G4, G5, G6, G7 and G11; G15 is a separate
  flakiness finding on rows that otherwise classify as `verified`.
- `verified` includes features proven by a meaningful test I read *and* by the passing `cargo test`
  run; where I ran the feature myself the evidence column says so.
- `pending-merge` covers the phase 1D rows (replay, streaming, stream keys, live encode) and the
  phase 1F rows (packaging, footprint, self-update). The 1D branch exists
  (`bb/1d-live-encode-replay-rtmp-whip-stream-keys-thr_4a6uh2kdfg`) and is not in this tree, so none
  of it is counted against this branch.
- `needs-real-desktop` is used only where headless cannot prove the behaviour at all (a real camera,
  a real sink's latency/audible sync, the tray on a panel, the portal hotkey dialog, hardware
  encoders, 5K capture, external players).
- The single `claimed-unverified` row is the screen-lock preview behaviour (G8); the two `missing`
  rows are own-window exclusion (G3) and dmabuf/zero-copy encoder input (the camera handover is
  counted under `deviates`, G1, because the CLI half works). Everything else that `PARITY.md` claims
  `done` is either verified here or is listed as a deviation with its evidence.

### The five things I would fix first

1. **G1** camera handover — it is the one upstream guarantee in the README that the app visibly
   breaks on every recording (2 s dark start, camera restart).
2. **G2** `probe` on fragmented files — the crash-safety feature's own verifier misreports the file,
   and the line format differs from upstream in a way scripts will notice.
3. **G3** own-window exclusion on X11 (or auto-hide + a notice) — otherwise the app records itself
   recording.
4. **G4** system audio should not block a session when the *default* sink is missing.
5. **G12/G13** correct `PARITY.md` in both directions, so the next phase plans against the truth.

---

# Re-audit (2026-11-23, phase 3B)

Read-only re-check of the same 143-item inventory against `main` at `bacdd62` ("1F final: 0.1.2
packages, ProRes encoder check, second-instance control in the Flatpak"), with 1D and 1F merged.
I changed `PARITY.md`, this file and the new `docs/DESKTOP_CHECKLIST.md`; no product code.

Everything ran through `scripts/dev.sh` in the Fedora 43 dev container with
`REC_TEST_DIR=/home/dev/rec-tests/audit2-*`. The host was shared with other worktrees (two other
dev containers building at the same time), which matters for the two load-sensitive failures below.
The 4 GB cgroup limit killed a plain `cargo build --release` (`rustc` SIGKILL on `ash`), so every
build ran with `CARGO_BUILD_JOBS=2` inside the container; see G20d.

## What ran

| What | Command | Result |
|---|---|---|
| Unit + UI + golden tests | `cargo test --release -- --test-threads=2` | **339 passed; 0 failed; 10 ignored** (349 run, 56 s) — 3A had 286/0/6 |
| Writer, codec, sync, crash | `scripts/selftest.sh pattern-1080-h264 pattern-prores pattern-hevc tracks audio-tracks audio-silent sync pause-resume gain crash finalize-crash defrag h264-bitrate` | 12 passed, 1 failed: `defrag` (115/120 frames under load; passed alone with 120/120 and identical hashes) — G20a |
| Screen, app, no-sink | `scripts/selftest.sh pattern-screen screen-region screen-x11 screen-window screen-token screen-exclude no-sink app-cli-control camera gui` | 8 passed, 2 failed: `app-cli-control` (52 frames, its threshold is 60) and `screen-token`; both passed alone (`app-cli-control` 79 frames), and `pattern-screen screen-region screen-token` passed 3/3 in a batch — G20b |
| Live | `scripts/selftest.sh replay stream-rtmp stream-whip live-e2e` | **4 passed, 0 failed** |
| HEVC over enhanced RTMP | `scripts/live/streams.sh --hevc` | **4 passed, 0 failed** (`stream-hevc`, `hevc-sync +0.1 ms`, `hevc-main10-note`, `stream-hevc-fallback`) |
| Mic under CPU pressure (G15a) | `scripts/selftest.sh mic-stress` | **PASS**: 20 runs under 8 burners, every beep within 1 dB, stems within 5 ms, no clicks |
| Output latency | `dbus-run-session -- sh scripts/audio/integration.sh devices` | PASS: `Output latency (system audio is shifted by this): 5.3 ms` |
| Device watch (G9) | `cargo test --release the_device_watch_reports_a_new_sink -- --ignored` under a PipeWire session | PASS (0.40 s); the `integration.sh watch` wrapper was OOM-killed in debug — G20c |
| Stream keys | `dbus-run-session` + unlocked gnome-keyring: `stream-key`, `--delete`, `secret-tool`, `REC_KEYRING_TEST=1 cargo test live::keyring` | keys stored under `rec-rs stream key`, read back by `secret-tool`, per-destination delete, `secret_service_round_trip` passed against the real service |
| Preview cost (G10) | `cargo test --release app::preview::tests::preview_cost -- --ignored` | 1080p NV12 **0.78 ms**, 4K NV12 **1.57 ms** (BGRA 2.29/6.76) per preview frame |
| CLI, formats, controls | `--version`, `record --help`, `devices` (plain, Xvfb, audio harness), `probe` on crash/clean files, `--mic`, `--no-record`, `--replay` 5/300/301, `save-replay` with/without a buffer, `stop`/`pause`/`resume`/`save-replay` with nothing running, `stream-key` without a keyring, status lines with `--stream`/`--replay` | see the rows below |
| Packaging (G14) | read `packaging/README.md`, `packaging/**`, `.github/workflows/release.yml` | present and complete; **not re-run** (the Flatpak/AppImage builds and their Docker tests take hours), so the package rows rest on the 1F report |

Not run, and therefore not counted as verified: any real compositor (GNOME/KDE Wayland), a real
V4L2/portal camera, the tray on a panel, the hotkey portal dialog, screen lock, VA-API on a GPU,
the real streaming services, an actual `flatpak update` / zsync update, and external players
(QuickTime, Final Cut, Premiere, Resolve, VLC). All of it is now in `docs/DESKTOP_CHECKLIST.md`.

## Classification counts

| Classification | 3A | 3B | Moved |
|---|---|---|---|
| verified | 89 | **108** | +19 |
| deviates | 12 | **8** | −4 |
| needs-real-desktop | 7 | **12** | +5 |
| n-a-ok | 13 | **14** | +1 |
| missing | 2 | **1** | −1 |
| pending-merge | 19 | **0** | −19 |
| claimed-unverified | 1 | **0** | −1 |
| **Total** | **143** | **143** | |

Where the moves came from: the 19 pending-merge rows split into 14 verified (replay, streams, live
flags, save-replay, stream keys, the two app cards, per-destination status, one encode, the settings
lock, the OBS replay/stream rows, HEVC-to-YouTube), 4 needs-real-desktop (the window-close clause,
self-update, footprint, live-encode CPU) and 1 n-a-ok (the documented streaming limitations). The
4 deviates that became verified are `devices`, `probe` ×2 and the camera hand-over. The missing
row that became verified is own-window exclusion; the claimed-unverified row (screen lock) is
implemented and unit-tested but only a real logind/screensaver session can prove it, so it is
needs-real-desktop. dmabuf/zero-copy encoder input is still missing. The remaining 8 deviates are
all justified Linux differences or a measured performance gap (see below).

## G1–G15 verdicts

| Gap | Verdict | Evidence | What is left |
|---|---|---|---|
| G1 camera hand-over | **fixed** | `app::ui_tests::the_camera_is_handed_over_without_reopening_it` (preview opens once, the recording reuses it and starts <1.5 s, the preview gets it back; a cold start waits ≥1.8 s); `Session::take_camera`/`start_with_camera`; selftest `camera` (cold CLI: 2 s settle, 102 frames in 5.4 s) | none for the hand-over; a real camera's LED/exposure is a desktop check |
| G2 `probe` duration + format | **fixed** | crash-left fragmented file: `probe` `duration : 4.000s` vs ffprobe `4.000000` (was 1.984); clean 1/3/5 s files match ffprobe exactly; track line `vide avc1 enabled=true start=0.000s dur=2.000s 17546.4 kb/s 1920x1080 @ 60.000 fps  [alt-group 0 samples=115]` (nominal fps, upstream's fields, additions bracketed) | the overall duration is the first video track's span, not the longest track's (G18) |
| G3 own window in the picture | **fixed** | selftest `screen-exclude`: X11 paints the excluded window out (1 distinct frame of 120) and logs `leaving 1 of rec's own windows out`; the portal path logs `a screen-cast portal cannot leave rec's own windows out`; unit `excluded_windows_are_painted_out_of_the_frame`, `the_window_is_hidden_only_where_the_capture_cannot_exclude_it`, `own_window_ids_come_from_x11_handles_only` | the portal hide + notification + tray reopen on a real compositor (checklist) |
| G4 no default sink blocks a session | **fixed** | selftest `no-sink`: exit 0, `rec: warning: no default output device for system audio (...); recording without system audio`, a written file whose `probe` shows one video track (no empty stem) | none |
| G5 `permissions --request` reuses a grant | **fixed** | selftest `screen-token` passes: the second request's stderr says `rec: forgetting the saved screen-capture grant so the system asks again`, the chooser log shows the picker ran again, a corrupt token is ignored and the recording still happens | the dialog itself on GNOME/KDE (checklist) |
| G6 `--mic` optional value | **fixed** | `record --mic` → `error: a value is required for '--mic <DEVICE>' but none was supplied`, exit 2; `--help` shows `--mic <DEVICE>` | none |
| G7 no-frames warning lacked the hint | **fixed** | `NO_FRAMES_WARNING = "rec: warning: no screen frames after 3s (was the screen-sharing request approved? see rec-rs permissions)"`; unit `record::tests::the_no_frames_warning_points_at_the_permission` | none |
| G8 screen lock does not stop the preview | **fixed (logic), needs a desktop** | new `app/lock.rs`: logind `Session.LockedHint` on the system bus plus `org.freedesktop.ScreenSaver.ActiveChanged` on the session bus, folded into `preview::Tap::set_wanted`; units `either_source_can_lock_it`, `preview_frames_stop_when_locked_covered_or_hidden` | lock a real GNOME/KDE session and watch `REC_DEBUG=1` frames/CPU (checklist) |
| G9 no hotplug watch | **fixed** | `capture::audio::watch_devices` (PipeWire registry) + `capture::camera::watch_devices` (inotify), started by the app; unit `the_device_watch_sees_video_nodes_appear_and_disappear`; ignored `the_device_watch_reports_a_new_sink` passes in release under the harness; `app::tests::hotplug_notices_a_changed_mic_and_device_list` | a real USB plug inside the app (checklist) |
| G10 preview costs ~4× upstream | **partially fixed** | shader conversion + plane upload: 1080p NV12 6.6 → **0.78 ms**, 4K NV12 8.15 → **1.57 ms** (BGRA 2.29/6.76); `preview_gpu::tests::the_shader_matches_the_cpu_reference`, `every_format_uploads` | still 3–4× upstream's shipped 0.25–0.39 ms (a hardware texture from the compositor, phase 2); BGRA frames cost 3–4× the NV12 ones |
| G11 help/devices/version | **fixed** | `--version` → `rec-rs 0.1.2`; Xvfb `devices` → `  [1] id screen  1280x800 pixels (1280x800 points)` (no `primary`), no latency line with no sink, `Output latency (system audio is shifted by this): 5.3 ms` with one (upstream's exact text); `--help` names the keyframe range, the bitrate presets and the live-size rule | the help is not upstream's text verbatim (G17) |
| G12 PARITY understated finished work | **fixed in this commit** | the rows for Record/Stop, Pause/Resume, the mixer, the fader, Reveal, cursor settings, audio/camera settings, low-power preview and `gui` are `done` with the tests that prove them | — |
| G13 PARITY overstated | **fixed in this commit** | the probe row is now true; the camera-settle row is true; the own-window row is true, but its two cited tests had been renamed (`the_window_is_hidden_only_where_the_capture_cannot_exclude_it`, `own_window_ids_come_from_x11_handles_only`) and its `selftest gui` claim was unsupported (`gui` does not assert the hide/restore) — both corrected; the defrag "identical framemd5" claim now says what a dropped slot does | — |
| G14 packaging not in the tree | **fixed (per the 1F report)** | `packaging/flatpak`, `packaging/appimage`, `packaging/desktop`, `packaging/test-mediamtx.sh` and `.github/workflows/release.yml` are in `main`; `packaging/README.md` reports `test-flatpak.sh` 23/23 and `test-appimage.sh` 14/14 (Debian 13) / 13/13 + 1 skip (Fedora 43), with two-way streaming, replay and cross-sandbox control | not re-run here (hours, and they need the Flatpak toolchain); no tag has ever triggered the release workflow |
| G15 audio flakiness at the edges | **fixed** | `mic-stress`: 20 runs under 8 burners, every beep within 1 dB, stems within 5 ms, no clicks; unit `a_late_first_chunk_with_irregular_sizes_still_fills_the_track`; the track set follows the opened sources (`audio-silent`: 4 tracks with a dead mic; `no-sink`: no empty system stem) | the original 3.4 dB observation is not reproducible with the fix in place |

## New and remaining gaps

### G16. Argument errors are masked by a capture error when no display is reachable

- **What:** `cli::prepare` calls `screen::resolve` (which needs a display or portal) before it
  validates `--no-record`, the ProRes/`.mp4` pair and the rest. On a headless box
  `rec-rs record --no-record` prints `the desktop portal is not reachable over D-Bus ...` instead of
  `--no-record needs --replay or --stream` (with a display, the right message appears). Upstream
  validates every flag before it resolves the target.
- **Reproduce:** `scripts/dev.sh '$CARGO_TARGET_DIR/release/rec-rs record --no-record'` (no
  `DISPLAY`/`WAYLAND_DISPLAY`) → portal error. Same for `record --codec prores -o x.mp4`.
- **Upstream:** `main.rs:492` (the `--no-record` check) and the ProRes guard run before
  `screen::resolve` at the end of `prepare`.
- **Files:** `src/cli.rs` (`prepare`: move `screen::resolve` after the flag checks; the size needs
  `native` only after them), `src/cli.rs` tests.

### G17. The `--stream` help no longer names `rec-rs stream-key`

- **What:** upstream's help for `--stream` says "The key comes from $REC_STREAM_KEY or the Keychain
  (`rec-rs stream-key URL`)". The port says only "The key comes from $REC_STREAM_KEY", although
  `rec-rs stream-key` stores it in the Secret Service and the recorder reads it from there. The
  `--keyframe` (0.5–4) and `--live-resolution`/`--stream-bitrate` texts are also more verbose than
  upstream's. All are text-only; the flags, values, defaults and clamps match.
- **Reproduce:** `rec-rs record --help | sed -n '/--stream /,/--no-record/p'`.
- **Upstream:** `main.rs:226-243`.
- **Files:** `src/cli.rs` (doc comments).

### G18. `probe`'s overall duration is the video track's span, not the asset's

- **What:** `probe` prints `start + dur` of the *first video track* as the file duration. When the
  audio track runs past the video by a partial buffer (a 2 s recording with five dropped slots:
  video 2.000 s, audio 2.021 s) `probe` says 2.000 s while ffprobe and AVFoundation report 2.021 s.
  Clean files where both tracks end together match exactly (1.000/3.000/4.000/5.000 s all checked),
  and the fragmented file's 4.000 s is right, so this is a ≤ one-buffer difference.
- **Reproduce:** on the `defrag` output (a dropped-slot run): `rec-rs probe defrag.mov` →
  `duration : 2.000s`; `ffprobe -show_entries format=duration` → `2.021313`.
- **Upstream:** `probe.rs:29` prints `AVURLAsset.duration`, the asset's (max-track) duration.
- **Files:** `src/media/probe.rs` (`probe`: take the max of the track spans, keeping the audio edit
  list subtraction), a unit test with an audio track longer than the video.

### G19. Small CLI text differences (additions, not regressions)

- **What:** (a) `save-replay` against a recording without `--replay` prints `rec: save-replay: this
  recording has no replay buffer (start it with --replay SECS)`; upstream is silent. (b)
  `pause`/`resume`/`save-replay` with nothing running say `no recording in progress (<pidfile> not
  found)`; upstream says just `no recording in progress` (`stop` matches). (c) the final summary
  omits upstream's `mix N` buffer count and adds `gaps filled`. (d) a non-quiet recording prints an
  extra `rec: encoder <name>` line.
- **Reproduce:** the commands above; the summary line of any `-t 1` recording.
- **Upstream:** `record.rs:552-575` and `record.rs:642-660`.
- **Files:** `src/record.rs`, `src/main.rs`. Decide per item: (a) and (d) are useful; (b) and (c)
  could match upstream exactly.

### G20. Evidence that is load-sensitive or was blocked by the container's limits

- **What:** four test-harness issues, all reproducible on a loaded 4 GB host; the product behaviour
  behind each is correct.
  - **(a) `defrag`** requires exactly 120 frames *and* identical ffmpeg framemd5. With a dropped
    slot the finalise writes the true 2-tick sample duration at the hole, which is the correct
    behaviour, so framemd5 differs while every decoded pixel is identical (115/115 frames, same
    pixel hashes; re-ran alone: 120/120 and identical). The test's 120 and its hash claim are not
    load-tolerant.
  - **(b) `app-cli-control`** requires ≥ 60 frames; under load it got 52 (all the behaviours it
    exists to prove passed: `refused`, `alive`, `stop rc=0`, `already running`, `app exit=0`). It
    passed alone with 79. One 10-case batch also saw `screen-token` fail; it passed alone and in a
    3-case batch.
  - **(c) `scripts/audio/integration.sh watch|stress`** run `cargo test` in debug without
    `CARGO_BUILD_JOBS`; on this 4 GB container the debug build was OOM-killed (`x11rb-protocol`,
    `ash`, `vello_cpu`: `signal: 9`). The release test they wrap passes in 0.40 s.
  - **(d) a plain `cargo build --release`** is OOM-killed by the 4 GB cgroup limit at the image's
    default job count (`rustc` SIGKILL on `ash`); `CARGO_BUILD_JOBS=2` inside the container builds in
    3m31s. `AGENTS.md` says `CARGO_BUILD_JOBS=4`, but `scripts/dev.sh` only forwards `REC_*`, so the
    value has to be set inside the command.
- **Reproduce:** the commands in the table above on a loaded host; `scripts/dev.sh sh -c 'cargo
  build --release'` (default jobs) for (d).
- **Upstream:** n/a — this is this repo's test harness and container; upstream's cases run on a
  developer's Mac and do not share these thresholds or a 4 GB cgroup.
- **Files:** `scripts/selftest.sh` (`defrag`, `app-cli-control`), `scripts/audio/integration.sh`
  (`watch`, `stress`; add `CARGO_BUILD_JOBS`), `scripts/dev.sh` (forward `CARGO_BUILD_JOBS` if the
  documented default is meant to apply inside the container).

## What still prevents a "feature parity" claim

Apart from the desktop checklist (one sitting, ~30–45 min), the honest blockers are:

1. **Hardware encoding and HEVC Main10 are unproven.** The VA-API candidates and the software
   fallback are in the writer and selected at run time, but no host with `/dev/dri` has run them;
   the image's x265 is 8-bit, so the HEVC proof is Main, not Main10 (the live encode says so).
2. **No zero-copy encoder input.** Frames still reach the compositor and the encoder from CPU memory;
   upstream goes IOSurface → VideoToolbox. The preview is now 0.78/1.57 ms per NV12 frame (3–4×
   upstream's shipped 0.25–0.39 ms), and screen capture copies every frame. This is a performance,
   not a feature, difference, but it is the largest remaining gap to upstream's design.
3. **A real camera has never been recorded.** V4L2 and the Camera portal are unit-tested and the
   synthetic camera proves the wiring, but no `/dev/video*` device and no Flatpak camera portal run
   exists in the container.
4. **Streaming is proved against MediaMTX, not the services.** YouTube/Twitch/Kick URLs and presets
   are unit-tested and the enhanced-RTMP HEVC negotiation is tested, but no live service account was
   used (upstream's own testing has the same limit for HEVC).
5. **The packages were not re-run.** The 1F report's 23/23 and 14/14 are taken as read; no tag
   triggered the release workflow, and `flatpak update` / a zsync update have never run.
6. **Small output differences** (G16–G19): error precedence without a display, the `--stream` help's
   missing `stream-key`, `probe`'s overall duration, and four text additions. None is a missing
   feature; all are cheap to align or document.
