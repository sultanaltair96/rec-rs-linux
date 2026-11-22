# rec-rs-linux port plan

This is the coordination document for the port. It names the upstream reference, the architecture decisions every contributor follows, the phases, and who owns which files. When a task brief and this file disagree, the brief wins for that task; tell the parent thread so this file gets updated.

## Upstream reference

- Repository: https://github.com/michaelshimeles/rec-rs (MIT OR Apache-2.0)
- Pinned commit: `93dd4948cdc8919e8b444a5af0141cf7c3628e7c` (v0.1.2 + site analytics, 2026-10-21)
- Available in this repo as the git submodule `reference/rec-rs` (read-only; never edit it, never build it).
- Upstream is ~15k lines of Rust built on Apple frameworks (ScreenCaptureKit, AVFoundation, VideoToolbox, CoreImage/Metal, Keychain, Carbon hotkeys, Sparkle). The portable parts (CLI, settings, pause timeline, mixer maths, overlay geometry, RTMP/FLV/AMF, replay buffer, WHIP framing, egui UI) are ported with their unit tests. The Apple parts are reimplemented on Linux equivalents below.
- Code copied or adapted from upstream keeps its licence. Add `// Adapted from rec-rs <path> (MIT OR Apache-2.0)` at the top of such files.

## Goal

Feature parity with upstream on Linux, shipped as a Flatpak and an AppImage. "Parity" is tracked feature by feature in `PARITY.md` (created in phase 0) with an evidence column. A feature counts only when a test or a recorded manual check proves it.

## Architecture decisions (binding)

| Concern | Upstream (macOS) | Linux port |
|---|---|---|
| Language/UI | Rust 2024, egui 0.36 + egui-wgpu + winit 0.30, Inter font | Same. Keep the paper-rs look; native window decorations are fine. |
| Screen + window capture | ScreenCaptureKit | xdg-desktop-portal ScreenCast (`ashpd`) + PipeWire (`pipewire` crate). Works on Wayland and X11 desktops with a portal. Persist the portal restore token so the picker only shows once. Cursor via portal cursor mode (embedded/metadata). Optional direct X11 path (x11rb) for `devices` enumeration and `--window <substring>` on X11 sessions. |
| System audio | ScreenCaptureKit audio | PipeWire capture of the default sink's monitor. |
| Microphone | ScreenCaptureKit / AVCapture | PipeWire capture node (device by id or name substring). `--mic-backend` accepts `pipewire` (default) and `pulse` if implemented; upstream values map with a clear message. |
| Output latency | CoreAudio | PipeWire/Pulse sink latency, read at record start. |
| Camera | AVCaptureSession | V4L2 (`v4l` crate) first; PipeWire camera portal when running inside Flatpak if V4L2 is not reachable. |
| Composite + scale | CoreImage on Metal, MPS Lanczos | wgpu compute/render shaders: Lanczos scaling, scenes, webcam overlay (circle, rounded, square, pill, squircle; roundness, border, shadow, mirror), window corner fill. Shares the egui wgpu device in the app. |
| Recording encode + mux | AVAssetWriter + VideoToolbox | GStreamer (`gstreamer-rs`) appsrc pipelines. Video: VA-API (`va*`/`vaapi*`) or NVENC when present, else x264/x265 software. ProRes 422 via `avenc_prores_ks`. Audio: AAC-LC 48 kHz. Mux: `qtmux`/`mp4mux` with fragments every 2 s while recording and a regular moov on stop (crash-safe). Track layout: video, mix, system stem, mic stem; alternate group with mix default. |
| Live encode | VideoToolbox + AudioConverter | Separate GStreamer encode to appsink producing H.264/HEVC + AAC/Opus packets for the replay buffer and every stream. |
| RTMP/RTMPS | built-in client, Security.framework TLS | Port the built-in client; TLS via `rustls`. |
| WHIP | str0m with apple-crypto | str0m with a Linux crypto backend (openssl or rust-crypto feature), ureq + rustls. |
| Stream keys | macOS Keychain | Secret Service (`keyring` crate or `secret-service`), keyed per destination URL. `REC_STREAM_KEY` override kept. |
| Global hotkeys | Carbon, ⇧⌘R / ⇧⌘S | XDG GlobalShortcuts portal; X11 XGrabKey fallback. Defaults Ctrl+Shift+R and Ctrl+Shift+S (⌘ maps to Ctrl). If unavailable, the UI says so. |
| Menu bar item / Dock | NSStatusItem, Dock menu | StatusNotifierItem tray (`ksni`) with the same menu; `.desktop` actions. Closing the window keeps the app running in the tray. |
| Control from shell | pidfile + SIGUSR1/SIGUSR2/SIGINFO/SIGTERM | pidfile in `$XDG_RUNTIME_DIR` + SIGUSR1 pause, SIGUSR2 resume, SIGTERM stop-and-finalize. Linux has no SIGINFO: `save-replay` uses a real-time signal (`SIGRTMIN+1`) or a unix control socket. |
| Permissions | TCC | `permissions` reports portal/PipeWire/camera/secret-service availability; `--request` runs the portal dialogs and stores the restore token. |
| Probe | AVFoundation decode | GStreamer decode of every sample (plus track/codec summary in the same format upstream prints). |
| Settings path | ~/Library/Application Support/rec-rs | `$XDG_CONFIG_HOME/rec-rs/settings.json`; recordings default to `$XDG_VIDEOS_DIR` (fallback `~/Videos`). |
| Updates | Sparkle, move-to-Applications | Flatpak: updates through Flatpak. AppImage: embedded zsync update info + "Check for Updates" against GitHub Releases. |
| Packaging | .app/.dmg | Flatpak (GNOME runtime + rust-stable SDK extension) and AppImage (bundling GStreamer plugins). App id `io.github.sultanaltair96.Rec`. |

### Core contract

Frames and audio cross module boundaries only through types in `src/media_types.rs` (created in phase 0, owned by the parent). Timestamps are seconds on `CLOCK_MONOTONIC` as `f64`, matching upstream's single host clock. Every source module exposes factory functions that `record.rs` calls; sources never call each other. A change to the contract needs the parent thread's approval.

## Development environment

The build host has no Rust toolchain or system dev libraries, but Docker works. All builds and tests run in the dev container defined in `dev/Dockerfile` via `scripts/dev.sh <cmd>` (phase 0). Headless integration tests use PipeWire + WirePlumber inside the container, GStreamer test sources, and MediaMTX for streaming. Real desktop capture is verified later on a Wayland desktop.

## Phases and ownership

| Phase | Work | Owns (only these paths) |
|---|---|---|
| 0 Foundation | Submodule, dev container, Cargo project, contract types, full CLI surface, settings, pause timeline, mixer, overlay geometry, test-pattern source, minimal GStreamer writer, ported unit tests, Linux self-test and sync/level scripts, CI, AGENTS.md, PARITY.md | everything (single writer) |
| 1A Screen capture | portal + PipeWire display/window/region capture, cursor and clicks, restore token, devices listing | `src/capture/screen/**` |
| 1B Audio + camera | PipeWire mic and system audio, device listing, latency, offsets; V4L2/portal camera, settle | `src/capture/audio/**`, `src/capture/camera/**` |
| 1C GPU + writer | wgpu compositor and scaler, scenes and overlay, full writer (HW/SW encoders, ProRes, track layout, fragments, CFR grid, pause), probe | `src/gpu/**`, `src/media/**` |
| 1D Live | live encode, replay buffer, RTMP/RTMPS, WHIP, keyring, stream-key, multi-destination, reconnect | `src/live/**` |
| 1E App | egui main + settings windows, preview, mixer widgets, tray, hotkeys, control, single instance | `src/app/**`, `src/hotkey.rs` |
| 1F Packaging | Flatpak manifest, AppImage build, desktop file, metainfo, icons, release workflow | `packaging/**`, `.github/workflows/release.yml` |
| 2 Integration | wire everything through `record.rs`, cross-module fixes, self-test on the real pipeline | coordinated by the parent |
| 3 Parity audit | read-only review of PARITY.md against upstream, real-desktop test pass | reviewer, no writes |

Shared files (`Cargo.toml`, `Cargo.lock`, `src/main.rs`, `src/record.rs`, `src/media_types.rs`, `PARITY.md`) are changed by phase 1 workers only by adding their own dependency lines, their own `PARITY.md` rows, and a single factory hook where the brief says so. The parent merges branches one at a time and resolves `Cargo.lock`.

## Rules for every worker

- Commit on your own worktree branch. No push, PR or merge unless the parent says so.
- Never commit secrets, stream keys, tokens, personal paths or recordings. Test output goes outside the repo.
- Never remove upstream behaviour to make something easier; ask the parent with `bb thread tell` first.
- Report with pasted command output and test counts, not "it works".
