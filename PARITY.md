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
| `gui [--start-recording] [--settings] [--window ID]` | The app | egui/wgpu app | 1E | todo | command exists and fails with phase 1E |

## Main window and app behaviour

| Feature | Upstream behaviour | Linux approach | Phase/owner | Status | Evidence |
|---|---|---|---|---|---|
| Live preview of what will be recorded | Rendered at preview size, composited | Same preview from the composited frames | 1E (with 1C) | todo | — |
| Own windows excluded from capture | ScreenCaptureKit filter | Portal picks the window; own windows are excluded the same way | 1A | todo | — |
| Preview low-power mode (half res, 15 fps) | Setting | Same setting, honoured by the preview | 1E | todo | — |
| Scene: Screen | Full display or window | Same | 1A | todo | geometry: unit `compose::tests::fit_letterboxes_and_cover_crops` |
| Scene: Screen + Cam, drag anywhere, corner snap | Overlay placement | Same geometry, wgpu rendering | 0 (geometry), 1C (GPU) | partial | unit `compose::tests::snapping`, `corners_and_free_positions`, `corner_placement_survives_resize` |
| Overlay shapes (circle, rounded, square, pill, squircle), roundness, size, border, shadow, mirror | CoreImage | Same masks and shadows computed on the CPU, drawn by wgpu | 0 (masks), 1C (GPU) | partial | unit `shape_coverage`, `pill_has_round_ends`, `border_inset_mask`, `shadow_spreads_beyond_the_shape`, `border_scales_with_the_output_height` |
| Overlay changes live while recording, baked in, no extra encode | Yes | Yes: the compositor runs before the encoder | 1C | todo | — |
| Scene: Camera full frame | AVCapture | V4L2 | 1B (source), 1C (composite) | todo | — |
| Switch scenes live if recording started with the camera | Yes | Same rule | 1C/1E | todo | — |
| Sources: display, window, camera, mic, system audio | Yes | PipeWire display/window, V4L2, PipeWire audio | 1A/1B | todo | — |
| Record/Stop button and timer | Yes | App | 1E | todo | — |
| Pause/Resume button | Yes | App (the session API exists) | 1E | todo | session API: selftest `pause-resume` |
| Audio mixer: meter with peak hold, fader, mute, dB | Yes, after the fader | Same widgets on the same session API | 1E | todo | levels in the status line; unit `mixer::tests::volume_gain` |
| Fader range −∞..+24 dB, drag/click/scroll/double-click/type | Yes | Same | 1E | todo | unit `mixer::tests::volume_gain` (range and silence) |
| Level changes apply live to every track (and replay/streams) | Yes, ≤20 ms ramp | Writer follows live (`set_volumes`); ≤20 ms envelope | 0 (ramp), 1E (widget) | partial | unit `fader_glides_and_passes_unity_through` |
| Reveal last / open folder | Yes | `xdg-open` on the output folder | 1E | todo | — |
| Replay buffer 15–300 s with memory use, Save Replay | Yes | Planned on the live encode | 1D | todo | — |
| Stream to YouTube/Twitch/Kick + custom servers, Go Live all-at-once, all-or-nothing | Yes | Same destinations and presets, built-in RTMP/WHIP | 1D | todo | unit `live::tests::shared_preset_fits_every_destination` |
| Per-destination live status (duration, bitrate, drops, reconnect) and LIVE 2/3 | Yes | Same from the output status | 1D | todo | — |
| One encode for every destination, within all limits | Yes | Same `shared_preset` logic | 0 (choice), 1D (encode) | partial | unit `shared_preset_fits_every_destination`, `settings::tests::live_encode_fits_every_enabled_destination` |
| Replay/stream run with or without a recording, survive recording start/stop, gaps bridged | Yes | Same design (live encode independent of the session) | 1D | todo | — |
| Settings window: folder, container | Yes | Same, persisted as JSON | 0 (model), 1E (window) | partial | unit `settings::tests::json_roundtrip_and_defaults_for_missing_fields`, `prores_forces_mov` |
| Resolution, frame rate, codec, bitrate settings | Yes | Same settings → record args | 0 | done | unit `settings::tests::record_args_parse_with_the_cli` |
| Cursor and click settings | Yes | Portal cursor modes | 1A | todo | — |
| Audio device and track layout settings | Yes | PipeWire devices | 1B (devices), 1E (UI) | todo | unit `audio_levels_do_not_restart_capture` |
| Camera device + overlay settings | Yes | V4L2 + same overlay model | 1B/1E | todo | unit `overlay_settings_round_trip_through_the_cli` |
| Live encode settings (size, bitrate, CBR, keyframe, audio bitrate), locked while streaming | Yes | Same model | 1D | todo | unit `settings::tests::live_encode_fits_every_enabled_destination` |
| Settings persist in Application Support | `~/Library/Application Support/rec-rs/settings.json` | `$XDG_CONFIG_HOME/rec-rs/settings.json` | 0 | done | unit `settings::tests::settings_live_in_the_xdg_config_dir` |
| Dock icon, app menu (Settings ⌘,, Hide ⌘H, Quit ⌘Q, Window menu) | Yes | Native window menu + `.desktop` actions | 1E/1F | todo | — |
| Closing the window keeps the app running and stops the preview | Yes | Tray-resident app | 1E | todo | — |
| Quitting (Dock/menu/logout) finalizes a running recording | Yes | Same: finalize before exit | 1E | todo | — |
| Menu bar item with red dot, elapsed time, full menu | NSStatusItem | StatusNotifierItem (`ksni`) with the same menu | 1E | todo | — |
| Global hotkeys ⇧⌘R / ⇧⌘S | Carbon | XDG GlobalShortcuts portal, Ctrl+Shift+R/S defaults, X11 fallback | 1E | todo | — |
| Hotkey conflict reported in the UI | Yes | Same: the portal reports the shortcut as taken | 1E | todo | — |
| Quit or SIGTERM mid-recording finalizes the file | Yes | SIGINT/SIGTERM/SIGHUP stop and finalize | 0 | done | selftest `pause-resume` (stop path), `stop` demo, crash test in the phase report |
| Re-launch with responsibility disclaimed (macOS permission prompt attribution) | Yes | n-a: Linux has no TCC; the portal attributes the dialog itself | — | n-a | — |
| "Re-confirm screen recording" prompt | macOS-specific | n-a: the portal's restore token avoids repeated prompts | — | n-a | — |

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
| Scenes | Three fixed scenes, switchable live, cut only | Same three scenes | 1C/1E | todo | scene model: unit `tests::scene_option` |
| Multiple displays/sources at once | No | No | — | n-a | matches upstream |
| System audio | Yes | PipeWire sink monitor | 1B | todo | — |
| Microphone | Yes | PipeWire capture | 1B | todo | — |
| Webcam overlay with shapes/border/shadow/mirror, live while recording | Yes | Same, wgpu | 1C | todo | geometry done: unit `compose::tests::corners_and_free_positions` |
| Separate audio tracks | Up to 4 | Mix + 2 stems | 0 (stems), 1C (mix) | partial | selftest: 2 tracks (video + system stem) |
| Per-source gain and mute, live | Yes | Same ramp/fader | 0 (maths), 1E (widgets) | partial | selftest `gain`; unit `fader_glides_and_passes_unity_through` |
| Audio filters (noise suppression, compressor) | No (known gap) | No | — | n-a | matches upstream |
| Hardware H.264/HEVC | VideoToolbox | VA-API on a GPU host; the container has no GPU, so it records with x264enc/x265enc (the writer skips VA without /dev/dri and trial-encodes it otherwise) | 1C | todo | dev/ELEMENTS.md |
| ProRes | 422 | `avenc_prores_ks` 422 | 0 | done | selftest `pattern-prores` |
| 10-bit / HDR | No | No | — | n-a | matches upstream |
| Constant frame rate | Exact grid | Exact grid | 0 | done | selftest + ffprobe PTS |
| Crash-safe files | Fragments every 2 s | Fragments every 2 s | 0 | done | crash test decodes |
| Pause / resume, sample-accurate, app and CLI | Yes | Yes (CLI done; app 1E) | 0 | done | selftest `pause-resume` |
| Replay buffer 15–300 s | Yes | Planned | 1D | todo | — |
| Streaming (RTMP/RTMPS, WHIP, multi-destination, HEVC, reconnect) | Yes | Planned | 1D | todo | preset maths done |
| Screen scaling: GPU Lanczos | Metal MPS | wgpu compute Lanczos | 1C | todo | — |
| Live preview | Yes | wgpu preview | 1E | todo | — |
| Level meters with peak hold, after the fader | Yes | Same | 1E | todo | unit `mixer::tests::volume_gain` (meter gain) |
| Global hotkey | ⇧⌘R / ⇧⌘S | Portal/X11, Ctrl+Shift+R/S | 1E | todo | — |
| Menu bar control | StatusItem | StatusNotifierItem | 1E | todo | — |
| Headless CLI | Yes | Yes | 0 | done | this phase is CLI-first; selftest runs headless |
| Small footprint | ~22 MB app | Rust + system GStreamer/PipeWire (Flatpak runtime) | 1F | todo | — |
| Encoder input: IOSurface straight to VideoToolbox | Zero copy | dmabuf → VA-API where available, else CPU frames | 1C | todo | — |

## Known gaps upstream documents

| Gap | Upstream | Linux | Phase | Status |
|---|---|---|---|---|
| Scenes are three fixed ones, no layout editor, no transitions, live switch needs a camera start | Yes | Same | 1C/1E | todo |
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
