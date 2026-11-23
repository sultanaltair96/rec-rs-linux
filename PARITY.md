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
| `--audio-tracks both/mix/stems` | Mix + stems, mix only, stems only | Same values; phase 0 writes stems only (mix is 1C) | 0 (parse), 1C (mix) | partial | parses: unit `settings::tests::record_args_parse_with_the_cli`; writer warns when a mix is requested |
| `--system-gain`, `--mic-gain` | dB per source, −60 = silence, +24 max, in every track | Same fader maths applied in the writer | 0 | done | selftest `gain` (`--ref=-6 --expect=-12`); unit `mixer::tests::volume_gain`, `fader_glides_and_passes_unity_through` |
| `--system-audio-offset-ms`, `--mic-offset-ms` | Sync shifts; system default = output latency | Same; the default sink's latency is read once at start from PipeWire and passed to the writer as the system offset | 0 (shift), 1B (latency) | done | unit `capture::audio::pipewire::tests::collector_combines_the_latency_parts`, `node_latency_property_is_a_fraction_or_frames`; integration `devices` prints 5.3 ms for a 256/48000 sink; unit (offsets applied in `push_audio`); selftest `sync` |
| `--no-cursor`, `--clicks` | Hide cursor / highlight clicks | Portal cursor modes: embedded (drawn by the compositor), hidden, metadata (we composite the bitmap). X11: the cursor is composited from XFixes and clicks are the button bits of `QueryPointer`, drawn as a ring that grows and fades | 1A | partial | unit `cursor::tests::a_ring_is_a_ring_not_a_disc`, `ring_grows_fades_and_expires`, `cursor_is_composited_at_its_hotspot`; `x11::tests::a_click_draws_a_ring_at_the_pointer` (real X server, XTEST click). Wayland: no button events through a portal, so `--clicks` warns and records the cursor only |
| `-t/--duration` | Exact, counted in output frames; pauses don't count | Identical: grid slots + `reached_limit` | 0 | done | selftest `sync` (300 frames for `-t 5`), selftest `pause-resume` (300 frames with a 2 s pause) |
| `--mic-backend` | `sck` or `avcapture` (macOS) | `pipewire` (default) or `pulse`; `sck`/`avcapture` map with a message | 0 | done | unit `record::tests::mic_backend_maps_upstream_values` |
| `--test-pattern [--noise]` | Flash + beep source for self-tests | Same, CPU frames, BGRA/I420 | 0 | done | unit `synth::tests::pattern_timeline`, `beep_starts_on_each_second`, `source_emits_on_the_grid`; selftest `pattern-1080-h264` |
| `--replay SECS` | 15–300 s replay buffer | Planned on the live encode | 1D | todo | range validated in `prepare` (unit) and the run fails with phase 1D |
| `--stream DEST` (repeatable) | RTMP(S)/WHIP, platform shortcuts | Same URLs and presets, built-in clients | 1D | todo | unit `live::tests::services_expand_and_are_recognized`, `shared_preset_fits_every_destination` |
| `--no-record` | Replay/stream only | Same rule (needs replay or stream) | 1D | todo | unit `tests::bad_arguments_are_rejected` |
| `--live-resolution/--stream-bitrate/--stream-codec/--keyframe/--vbr/--audio-bitrate` | Live encode controls | Same parsing, presets and clamping | 0 (parse/config), 1D (encode) | partial | unit `settings::tests::live_encode_fits_every_enabled_destination` |
| `devices` command | Lists displays, windows, cameras, mics + output latency | Same layout. Displays and windows come from RandR/EWMH (also under XWayland); a Wayland session without X11 prints that the system picker chooses instead of an empty list. Cameras are V4L2 capture nodes (path + card name), microphones PipeWire sources (node name + description), and the default sink's latency is printed | 0 (shape), 1A/1B | partial | selftest `screen-x11` for displays; integration `devices` matches `Rec-Test-Mic [rec_test_mic]` and `Output latency ... 5.3 ms`; script `scripts/audio/integration.sh` |
| `probe FILE` command | AVFoundation: playable, tracks, codecs, full sample decode, non-zero exit on error | GStreamer: qtdemux structure + `uridecodebin` decode of every sample | 0 | done | selftest uses it on every case (`.probe` files); `ffmpeg -v error -f null` cross-check |
| `permissions [--request]` | TCC status and prompts | ScreenCast version, source types, cursor modes, whether a grant is stored, whether PipeWire runs. `--request` runs the portal dialog once and stores the restore token (0600, `$XDG_STATE_HOME/rec-rs/portal-token`). The microphone line reports PipeWire and the number of inputs; the camera line reports `/dev/video*` or the portal | 1A/1B | partial | unit `portal::tests::cursor_modes_fall_back_to_what_the_portal_offers`; selftest `screen-token`; integration `no-pipewire` (no session: `devices` says unavailable, exit 0); container report shows the mic/camera lines |
| `stop` / `pause` / `resume` | pidfile + SIGUSR1/SIGUSR2/SIGTERM | pidfile in `$XDG_RUNTIME_DIR` (else `/tmp/rec-rs-$UID`), SIGINT to stop | 0 | done | unit `record::tests::pidfile_lives_in_the_runtime_dir`, `no_recorder_means_a_clear_error`; selftest `pause-resume` runs `pause`/`resume`; `stop` demo in the phase report |
| `save-replay` | SIGINFO writes the buffer to MP4 | `SIGRTMIN+1` (no SIGINFO on Linux); buffer is 1D | 0 (signal), 1D (buffer) | partial | unit `record::tests::save_replay_signal_is_a_real_time_signal` |
| `stream-key DEST [--delete]` | Keychain | Secret Service (`secret-service` crate), `REC_STREAM_KEY` meanwhile | 1D | todo | the command fails with phase 1D and names the env override |
| `gui [--start-recording] [--settings] [--window ID]` | The app | egui/wgpu app | 1E | todo | command exists and fails with phase 1E |

## Main window and app behaviour

| Feature | Upstream behaviour | Linux approach | Phase/owner | Status | Evidence |
|---|---|---|---|---|---|
| Live preview of what will be recorded | Rendered at preview size, composited | Same preview from the composited frames | 1E (with 1C) | todo | — |
| Own windows excluded from capture | ScreenCaptureKit filter | Not possible through the portal: the compositor owns the stream and a portal client cannot name windows to exclude. On X11 the recorder is an ordinary client, so its own windows are captured like any other | 1A | n-a | documented in the phase 1A notes; the app (1E) should hide its own preview windows while recording, as upstream does by excluding them |
| Preview low-power mode (half res, 15 fps) | Setting | Same setting, honoured by the preview | 1E | todo | — |
| Scene: Screen | Full display or window | Same, scaled to the output with Lanczos | 1C (composite), 1A (capture) | partial | unit `gpu::tests::a_screen_at_the_output_size_is_bit_exact`, `a_small_screen_is_not_upscaled`, `lanczos_matches_a_cpu_reference`; golden `screen`, `screen-4k-to-1080` |
| Scene: Screen + Cam, drag anywhere, corner snap | Overlay placement | Same geometry, drawn by wgpu | 0 (geometry), 1C (GPU) | done | unit `compose::tests::snapping`, `corners_and_free_positions`; `gpu::tests::renders_overlay_with_border_into_the_frame`, `placement_follows_the_geometry_for_corners_and_free_positions`, `mirroring_flips_the_camera`; goldens `screen-cam-rounded`, `screen-cam-pill` |
| Overlay shapes (circle, rounded, square, pill, squircle), roundness, size, border, shadow, mirror | CoreImage | Same masks and shadows computed on the CPU (u8 coverage), drawn by wgpu with the same blending order | 0 (masks), 1C (GPU) | done | unit `compose::tests::shape_coverage`, `pill_has_round_ends`, `shadow_spreads_beyond_the_shape`; `gpu::tests::every_shape_matches_the_mask_geometry`, `the_border_has_the_width_and_colour_of_the_spec`, `the_drop_shadow_falls_off_outside_the_overlay`, `mirroring_flips_the_camera`; goldens `screen-cam-circle`, `screen-cam-square-border`, `screen-cam-squircle-shadow` |
| Overlay changes live while recording, baked in, no extra encode | Yes | Yes: the compositor runs before the encoder, and the mask/border/shadow texture is cached per size and style so moving it only changes a transform | 1C | done | unit `gpu::tests::renders_overlay_with_border_into_the_frame` (moves it between frames), `gpu::decor::tests::the_key_ignores_the_position_but_not_the_style` |
| Scene: Camera full frame | AVCapture | V4L2 into a cover-fitted, mirrored full frame | 1B (source), 1C (composite) | partial | unit `gpu::tests::the_camera_scene_covers_the_frame`, `mirroring_flips_the_camera`; golden `cam` |
| Switch scenes live if recording started with the camera | Yes | The compositor switches from the next frame; the app/CLI rule is 1E/1G | 1C/1E | partial | unit `gpu::tests::scenes_and_camera_changes_apply_from_the_next_frame` |
| Sources: display, window, camera, mic, system audio | Yes | PipeWire display/window, V4L2 or portal camera, PipeWire audio | 1A/1B | done | selftest `pattern-screen`, `pattern-mic`, `pattern-system-audio`; unit camera/audio suites; the recorder-side camera wiring is 1G |
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
| Audio device and track layout settings | Yes | PipeWire devices; `microphones()` (with the default first) feeds the mixer list | 1B (devices), 1E (UI) | partial | unit `audio_levels_do_not_restart_capture`, `capture::audio::tests::mic_matching`; integration `devices` |
| Camera device + overlay settings | Yes | V4L2 device selection + the same overlay model | 1B (device), 1C (overlay), 1E (UI) | partial | unit `overlay_settings_round_trip_through_the_cli`, `camera::v4l2::tests::device_matching`; real hardware under "Needs a real desktop" |
| Live encode settings (size, bitrate, CBR, keyframe, audio bitrate), locked while streaming | Yes | Same model | 1D | todo | unit `settings::tests::live_encode_fits_every_enabled_destination` |
| Settings persist in Application Support | `~/Library/Application Support/rec-rs/settings.json` | `$XDG_CONFIG_HOME/rec-rs/settings.json` | 0 | done | unit `settings::tests::settings_live_in_the_xdg_config_dir` |
| Dock icon, app menu (Settings ⌘,, Hide ⌘H, Quit ⌘Q, Window menu) | Yes | Native window menu (1E) + `.desktop` actions `Start Recording` / `Settings` (1F); both packages install the same desktop file | 1E/1F | partial | `desktop-file-validate` clean; actions shipped in `packaging/desktop/io.github.sultanaltair96.Rec.desktop` and listed inside the built Flatpak (`test-flatpak.sh`) |
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
| BT.709 tagging | Yes | The compositor writes BT.709 limited range NV12/I420 (Y 16..235, Cb/Cr 16..240, chroma averaged over each 2x2 block); the encoder declares the tags | 1C (conversion), 1G (caps) | partial | unit `gpu::tests::the_output_is_bt709_limited_range` (white 235, black 16, red 63/102/240); `gpu::yuv::tests::known_bars`, cross-checked against ffmpeg's swscale |
| Keyframe every 2 s | Yes | `key-int-max`/`gop-size` = 2 fps | 0 | done | writer sets it per encoder; crash test decodes |
| Constant frame rate on an exact 1/fps grid | Yes, measured 16.667 ms | Yes: slot grid, PTS checked | 0 | done | `ffprobe` PTS 0.000/0.016667/0.033333/0.050000 (phase report) |
| Mix track: AAC-LC 48 kHz stereo 256 kb/s, default track | Yes (track 2) | Planned: AAC-LC 48 k stereo 256 kb/s | 1C | todo | — |
| System stem: AAC 48 kHz stereo 256 kb/s | Yes (track 3) | AAC-LC 48 k stereo 256 kb/s, written | 0 | done | selftest tracks count (2: video + system); `ffprobe` audio aac 48000 stereo |
| Mic stem: AAC 48 kHz mono 192 kb/s | Yes (track 4) | Planned with the mic track (mono) | 1B/1C | todo | — |
| Track grouping: alternate group with the mix as default | Yes | Muxer track grouping (`mp4mux` input groups) | 1C | todo | — |
| Single clock, audio never stretched | Yes | Same: CLOCK_MONOTONIC everywhere, resampling only for device rates | 0 | done | selftest `sync` (offsets 0.0 ms) |
| Flash/beep sync 0.0 ms in the file, including across pauses | Yes | 0.0 ms on every beep but the first, +9.9 ms on the first (see note) | 0 | partial | selftest `sync`, `pause-resume`, `audio-gaps` |
| Levels: fader and mute before everything, ≤20 ms ramp, 0 dB untouched | Yes | Same ramp and ordering in the writer | 0 | done | selftest `gain`; unit `fader_glides_and_passes_unity_through` |
| System audio latency shift (output device latency) | CoreAudio | PipeWire default sink's `Latency`/`ProcessLatency`, else `node.latency`, read once at start | 1B | done | unit `collector_combines_the_latency_parts`, `node_latency_property_is_a_fraction_or_frames`; integration `devices` prints 5.3 ms; writer applies the offset |
| Webcam lag adaptation (emission waits for the slowest source) | Yes | The camera delivers host-clock frames from the V4L2 buffer time; the writer's adaptive lag is 1G's | 0 (lag), 1B (camera), 1G (adaptation) | partial | unit `camera::convert::tests::*` (timestamp clock); writer `lag()`; selftest frame grid |
| Crash-safe: fragments every 2 s, regular movie on stop | AVAssetWriter fragments | `mp4mux`/`qtmux` `fragment-duration=2000`; SIGKILL leaves a playable file | 0 | done | crash test: SIGKILL at 4.5 s → 4.0 s file that `ffmpeg -f null` decodes |
| Edge to edge: frame has the source's aspect, no bars, never cropped | Yes | Same `fill`/`same_aspect` maths | 0 | done | unit `compose::tests::fill_is_edge_to_edge_for_the_same_aspect` |
| Window transparent corners filled with the window's edge colour | CoreImage clamp | `compose::corner_inset` + a clamp in the upload shader, compositing the window over the clamped edge | 0 (geometry), 1C (GPU) | partial | unit `compose::tests::fill_is_edge_to_edge_for_the_same_aspect` (`corner_inset`), `gpu::tests::the_corner_fill_replaces_transparent_window_corners` |
| Camera settles 2 s before the file starts | Yes | `CAMERA_SETTLE_SECS = 2.0` and `CameraSource::settle_remaining()` are the 1B API; the writer's `wait_for_camera` gate is wired by 1G | 1B/1G | partial | unit `camera::tests::size_and_aspect_are_known_before_the_first_frame` (2 s constant); 1G wires the gate |
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
| Live size scaled from the recording size | Lanczos | wgpu Lanczos, shared with the recording | 1C/1D | partial | scaler done: unit `gpu::tests::lanczos_matches_a_cpu_reference`; the live encode that will reuse it is 1D |
| Replay buffer trims a GOP at a time, re-encodes audio, syncs in QuickTime and ffmpeg | Yes | Same on the packet buffer | 1D | todo | — |
| RTMP/RTMPS client with TLS, drop a GOP when behind, reconnect with backoff | Built-in, Security.framework | Built-in port with rustls | 1D | todo | — |
| WHIP (str0m, ICE/DTLS/SRTP, H.264+Opus, DELETE on stop, 5 s silence → reconnect) | Yes | str0m with a Linux crypto backend | 1D | todo | — |
| Platform bitrate/size presets (YouTube/Twitch/Kick), lowest across destinations | Yes | Identical maths (ported) | 0 | done | unit `live::tests::platform_presets`, `shared_preset_fits_every_destination` |
| Stream keys in the OS key store, `REC_STREAM_KEY` override | Keychain | Secret Service, env override | 1D | todo | `resolve_destination` accepts `REC_STREAM_KEY` today |
| Tested against MediaMTX (RTMP, RTMPS, WHIP) | Yes | MediaMTX in the dev container | 1D | todo | — |

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
| Scenes | Three fixed scenes, switchable live, cut only | Same three scenes, composited on the GPU | 1C/1E | partial | scene model: unit `tests::scene_option`; rendering: `gpu::tests::scenes_and_camera_changes_apply_from_the_next_frame`; goldens `screen`, `screen-cam-*`, `cam` |
| Multiple displays/sources at once | No | No (a portal session may hand over several streams; the first is used) | — | n-a | matches upstream |
| System audio | Yes | PipeWire default sink monitor | 1B | done | selftest `pattern-system-audio` (48 kHz and 44.1 kHz: 5 beeps at −6 dBFS, spacing 1.000 s); integration `system-48k`/`system-44k` |
| Microphone | Yes | PipeWire `Audio/Source` capture at the device's own rate | 1B | done | selftest `pattern-mic`; integration `mic-default`, `mic-by-name`, `mic-unplug` |
| Webcam overlay with shapes/border/shadow/mirror, live while recording | Yes | V4L2/portal source (1B) + wgpu compositor (1C); the recorder wires them (1G) | 1B/1C/1G | partial | unit `gpu::tests::every_shape_matches_the_mask_geometry`, `renders_overlay_with_border_into_the_frame`; camera source units; goldens `screen-cam-*`; real camera under "Needs a real desktop" |
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
| Screen scaling: GPU Lanczos | Metal MPS, which antialiases a downscale | wgpu Lanczos (a = 3), separable, per plane in the encoded domain, **stretched by the reduction factor** so a downscale antialiases, edge clamp, never upscaling | 1C | done | unit `gpu::tests::lanczos_matches_a_cpu_reference` (≤2/255 against an independent CPU implementation), `downscaling_a_one_pixel_pattern_leaves_only_its_mean` (a 1-px 5K pattern leaves 0.7/255 ripple inside the frame, against 121.7 for an unstretched kernel; ffmpeg's own swscale/zimg reach 22.6/29.7 at the replicated edge against our 22.7), `the_scaler_matches_ffmpeg_lanczos` (PSNR 50.3 dB 4K→1080p, 51.4 dB 5K→1080p against swscale's Lanczos; 58.1/57.0 dB against zimg), `a_small_screen_is_not_upscaled`; golden `screen-4k-to-1080`; measured 77 ms/frame 4K→1080p and 116 ms 5K→1080p on lavapipe (`tests/golden/README.md`) |
| Live preview | Yes | wgpu preview | 1E | todo | — |
| Level meters with peak hold, after the fader | Yes | Same | 1E | todo | unit `mixer::tests::volume_gain` (meter gain) |
| Global hotkey | ⇧⌘R / ⇧⌘S | Portal/X11, Ctrl+Shift+R/S | 1E | todo | — |
| Menu bar control | StatusItem | StatusNotifierItem | 1E | todo | — |
| Headless CLI | Yes | Yes | 0 | done | this phase is CLI-first; selftest runs headless |
| Small footprint | ~22 MB app | Flatpak bundle 3.5 MB (GStreamer/PipeWire shared from the runtime); AppImage 98 MB (it bundles GStreamer and its ffmpeg stack) | 1F | done | `packaging/flatpak/build-flatpak.sh` and `packaging/appimage/build-appimage.sh` print the sizes |
| Encoder input: IOSurface straight to VideoToolbox | Zero copy | dmabuf → VA-API where available, else CPU frames | 1C | todo | — |

## Packaging, installation and updates (phase 1F)

| Feature | Upstream behaviour | Linux approach | Phase/owner | Status | Evidence |
|---|---|---|---|---|---|
| Flatpak package | `.app`/`.dmg` | `io.github.sultanaltair96.Rec` on `org.gnome.Platform` 49; H.264 through the runtime's `codecs-extra` extension (`x264enc`), HEVC through an x265 4.1 + gst-plugins-bad 1.26.11 `x265enc` module built by the manifest, AAC/ProRes/Opus from the runtime; single-file `rec.flatpak` | 1F | done | `packaging/flatpak/build-flatpak.sh`; `packaging/flatpak/test-flatpak.sh` 9/9 (encoder lines `x264enc`, `x265enc`, `avenc_prores_ks`; `probe` decodes H.264 and HEVC inside the sandbox) |
| AppImage package | — | Ubuntu 24.04 build (glibc 2.39 floor), bundled GStreamer 1.24 plugins collected element by element, x264/x265/AAC/ProRes/Opus, zsync update information | 1F | done | `packaging/appimage/build-appimage.sh`; `test-appimage.sh` on `fedora:43` and `debian:13` with no GStreamer installed |
| App starts as the GUI and also exposes the CLI | `.app` with a bare binary inside | Both packages run `rec-rs`: no subcommand starts the app (1E), and `record`/`probe`/`devices`/... work headless. `flatpak run <id> ...` and `flatpak run --command=rec-rs <id> ...` both work | 1F | partial | Flatpak: record + probe through `flatpak run` (test-flatpak.sh); the window itself is phase 1E (the run prints the phase message, checked) |
| Install into the applications menu | Drag `Rec.app` to /Applications | Desktop file, metainfo and hicolor icons (16–512 px) are installed by the Flatpak; the AppImage carries the same files at the image root and is integrated by AppImageLauncher/appimaged or the user's launcher | 1F | done | `desktop-file-validate` clean, `appstreamcli validate --no-net` successful (one expected pedantic note: the app id has a capital letter); metadata listed inside the built Flatpak |
| Move to Applications (app translocation fix) | Moves itself into /Applications and clears quarantine | n-a: Flatpak installs into its own store and an AppImage runs from wherever the user puts it; neither runs from a read-only translocated path | 1F | n-a | — |
| Updates | Sparkle appcast, daily check, Check for Updates… | Flatpak: `flatpak update` from the remote. AppImage: embedded `gh-releases-zsync|sultanaltair96|rec-rs-linux|latest|Rec-*x86_64.AppImage.zsync` plus the `.zsync` published next to it | 1F | partial | zsync written by `build-appimage.sh`, string returned by `--appimage-updateinformation` (printed by `test-appimage.sh`); the in-app menu item is phase 1E |
| Release packaging | `release-macos.sh` (universal build, sign, notarize, dmg, appcast) | `.github/workflows/release.yml` on `v*` tags only: builds *and tests* both packages in the same containers as local builds, checks the tag against `Cargo.toml`, then creates the GitHub Release with `rec.flatpak`, `Rec-<version>-x86_64.AppImage` and the `.zsync` | 1F | done | `actionlint` clean; both build+test script pairs run locally with the same commands and the workflow calls exactly them (never triggered: no tag) |
| Icons | `AppIcon.icns` | hicolor PNGs at 16/24/32/48/64/128/256/512 converted from upstream's `support/icon.png` (MIT OR Apache-2.0); no SVG because there is no vector source | 1F | done | icon files, installed by both packages; provenance in `packaging/README.md` |
| AppStream metadata | — | `io.github.sultanaltair96.Rec.metainfo.xml`: description, features, OARS 1.1, `MIT OR Apache-2.0`, 0.1.0 release, binaries, screenshots | 1F | done | `appstreamcli validate --no-net`; screenshots are upstream's macOS captures until 1E (TODO in the file) |
| Offline, reproducible package build | n/a (signed binaries) | Flatpak crates vendored from `Cargo.lock` (`cargo-sources.json`), regenerated by `packaging/update-sources.sh`; linuxdeploy and appimagetool pinned by release and sha256; both builds run in pinned containers | 1F | done | `packaging/update-sources.sh` regenerates 619 entries from the current `Cargo.lock`; the Flatpak builds with `CARGO_NET_OFFLINE=true` |
| Camera inside the Flatpak | n/a | Camera portal (`capture::camera::portal`), so `--device=all` is not granted; direct V4L2 is an opt-in `flatpak override --device=all` trade-off documented in `packaging/README.md` | 1B/1F | partial | compiles and is tested without a portal on this host; the Flatpak portal path needs a real desktop (checklist in `packaging/README.md`) |

## Known gaps upstream documents

| Gap | Upstream | Linux | Phase | Status |
|---|---|---|---|---|
| Scenes are three fixed ones, no layout editor, no transitions, live switch needs a camera start | Yes | Same | 1C/1E | partial |
| No application capture or multi-display capture | Yes | Same | — | n-a |
| Region capture is CLI-only | Yes | Same (the app may add it later) | 1E | todo |
| No audio filters, hotkeys not configurable | Yes | Same | — | n-a |
| Streaming: no SRT/multitrack, same encode for all, live ≤ recording size, changing settings needs a restart | Yes | Same limitations (parity, not a gap to fix) | 1D | todo |
| HEVC to YouTube tested only against a local server | Yes | Same testing honestly reported | 1D | todo |
| 8-bit SDR only | Yes | Same | — | n-a |
| Webcam overlay burned into the video | Yes | Same: the compositor runs before the encoder | 1C | partial | the compositor is done and pixel-tested; the writer calls it (1G) |
| Output latency read once at start | Yes | Same (PipeWire sink params at start; a device switch needs a restart, like upstream) | 1B | done | unit `collector_combines_the_latency_parts`; integration `devices` |
| Mix track resamples a non-48 kHz mic linearly per buffer upstream | Yes | Linux: the mix uses the same windowed-sinc resampler as everything else (better) | 1C | todo |
| Re-confirmation prompt for screen recording | macOS-only | n-a: the portal's restore token avoids it | — | n-a |

## Phase 0 summary

Done (proved by tests or self-test cases): the CLI surface, settings and the settings-to-arguments
path, pause timeline and pause/resume end to end, the frame-rate grid, faders/gain ramps and the
resampler and limiter maths, overlay geometry/masks/shadows, the test-pattern source, the H.264 and
ProRes writer with an AAC system track and crash-safe fragments, `probe`, the platform preset maths,
the pidfile/signal control, and the sync/level/self-test scripts.

Partial (a named part is missing, listed per row): mix and mic tracks, BT.709 colour tagging,
webcam lag adaptation, hardware encoding.

Known items owned by phase 1C (not fixed here): the first ~10 ms of audio in a file fades in
because the AAC priming frames are dropped instead of trimmed with an MP4 edit list (+9.9 ms on the
first beep, 0.0 ms after), and the self-test's sync threshold is 20 ms rather than upstream's 1 ms
for that reason.

Todo (phase 1): all capture, the GPU compositor, the app, replay/streaming, packaging.

## Screen capture (phase 1A)

Two backends, chosen by the session type: xdg-desktop-portal's ScreenCast plus PipeWire on Wayland
(`src/capture/screen/portal.rs`, `pipewire.rs`), and X11 directly on an X11 session (`x11.rs`).
`REC_SCREEN_BACKEND=portal|x11` overrides the choice.

What runs headless in the dev container, on every `scripts/selftest.sh` run:

| Case | Session | Proves |
|---|---|---|
| `pattern-screen` | sway (wlroots, headless 1280x720) + xdg-desktop-portal-wlr + PipeWire | the whole Wayland pipeline: picker → PipeWire node → 1280x720 BGRA frames at a constant 60 fps, all 180 frames different (a terminal scrolling in the capture), `probe` and a full ffmpeg decode clean, and a 0600 restore token stored |
| `screen-region` | same | `--region` crops the shared monitor to 640x480 native pixels (not a scaled 16:9 frame) |
| `screen-token` | same | `permissions --request` runs the dialog and stores the grant; a corrupted token is ignored with a message and the recording still happens |
| `screen-x11` | Xvfb 1280x800 | RandR enumeration (`devices` lists `[1]`), a root-window capture at the native size with motion, no portal involved |
| `screen-window` | Xvfb + a GStreamer window | `--window gstreamer` captures that client's own 400x300 drawable, with motion |
| `cargo test` under `scripts/screen/xvfb.sh` | Xvfb | 8 X11 unit tests, 5 of them against the live server: monitor/window enumeration, a window captured with its own pixels, `--region` capturing the pixels it names (red/blue halves), a click ring from a real XTEST click, and the region geometry |

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
- **Own windows in the picture.** A portal client cannot exclude windows from a compositor's stream.
  The app (1E) must hide its own windows while recording, as upstream's preview-window exclusion
  did automatically.
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
| Native rate and channels | 48 kHz AVCapture | The stream asks for the node's `audio.rate`/`audio.channels`; the mixer resamples other rates with its windowed-sinc resampler | done | integration `system-44k` (log: `system format 44100 Hz`, recorded 1 kHz, spacing 1.000 s); `mixer::tests::resamples_seamlessly` |
| Chunk timestamps | Host clock from CoreMedia | `pw_buffer.time` (graph cycle, ns) / 1e9, guarded against other clocks; live worst `|pts − arrival|` 1.0–3.0 ms over 281 chunks | done | unit `buffer_times_become_monotonic_seconds`, `timestamps_from_another_clock_fall_back_to_now`; ignored `live_chunk_timestamps_are_on_the_host_clock`; integration `clock` |
| Device disappears mid-recording | Capture error | The registry's `global_remove` of the linked node stops the loop; `stop()` returns `the mic device was disconnected`; the session keeps recording video and finalizes | done | integration `mic-unplug` (121 frames, warning in the log) |
| Capture latency | CoreAudio | Measured end to end: first captured tone **+26.6…+33.4 ms** after `pw-play` was spawned (its start-up included) | measured | ignored `system_audio_capture_latency`; integration `latency` |
| Output latency | CoreAudio device latency | Default sink `Latency` + `ProcessLatency` params, else `node.latency`; printed by `devices` and used as the system-audio shift | done | unit `collector_combines_the_latency_parts`, `node_latency_property_is_a_fraction_or_frames`; integration `devices` (256/48000 → 5.3 ms) |
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
