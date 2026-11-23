# Packaging

Rec ships as a **Flatpak** (`io.github.sultanaltair96.Rec`) and an **AppImage**
(`Rec-<version>-x86_64.AppImage`). Both carry the whole recorder — the app
entry point (no subcommand) and every CLI command — and integrate with the
desktop through a `.desktop` file with the upstream Dock-menu actions, AppStream
metainfo and hicolor icons.

Nothing here builds on the host: there is no Rust toolchain and no flatpak on a
plain machine, so every build and every check runs in a container. Docker is the
only requirement.

| | Flatpak | AppImage |
|---|---|---|
| Artifact | `$REC_PKG_OUT/rec.flatpak` | `$REC_PKG_OUT/Rec-<version>-x86_64.AppImage` + `.zsync` |
| Build | `packaging/flatpak/build-flatpak.sh` | `packaging/appimage/build-appimage.sh` |
| Test | `packaging/flatpak/test-flatpak.sh` | `packaging/appimage/test-appimage.sh` |
| Base | `org.gnome.Platform` 49 (freedesktop 25.08) + rust-stable, llvm21 SDK extensions | `ubuntu:24.04` container, glibc 2.39 floor |
| GStreamer | the runtime's 1.26 + `codecs-extra` | bundled 1.24 (base/good/bad/ugly/libav) |
| Size | 3.5 MB bundle (runtime shared) | 98 MB AppImage (GStreamer inside) |

`REC_PKG_OUT` defaults to `${XDG_CACHE_HOME:-$HOME/.cache}/rec-rs-linux-packaging`.
Nothing is written inside the repository.

```
packaging/
├── desktop/                     shared by both packages
│   ├── io.github.sultanaltair96.Rec.desktop
│   ├── io.github.sultanaltair96.Rec.metainfo.xml
│   └── icons/hicolor/{16..512}x{16..512}/apps/io.github.sultanaltair96.Rec.png
├── flatpak/
│   ├── io.github.sultanaltair96.Rec.yml
│   ├── cargo-sources.json       generated, offline crates from Cargo.lock
│   ├── patches/x265-cmake4.patch
│   ├── build-flatpak.sh, test-flatpak.sh, test-inside.sh
├── appimage/
│   ├── AppRun                   GStreamer environment for the bundle
│   ├── build-appimage.sh, build-inside.sh
│   ├── test-appimage.sh, test-inside.sh
└── update-sources.sh            regenerate cargo-sources.json from Cargo.lock
```

## Flatpak

### Runtime and extensions

`org.gnome.Platform` **49** is the newest stable GNOME runtime on Flathub at the
time of writing, on the freedesktop 25.08 base (GStreamer 1.26.11). The manifest
adds:

* `org.freedesktop.Sdk.Extension.rust-stable` 25.08 — the Rust toolchain for the
  offline cargo build.
* `org.freedesktop.Sdk.Extension.llvm21` — `libclang` for `bindgen`
  (`pipewire-sys`, `v4l2-sys-mit`). The base SDK has no clang. The manifest sets
  `LIBCLANG_PATH` and `BINDGEN_EXTRA_CLANG_ARGS` (the extension's clang does not
  find its own builtin headers on the default search path).

### Codecs: the decision and the evidence

The runtime already has the whole recording pipeline except two encoders:
`appsrc`/`appsink`, `videoconvert`/`videoscale`, the muxers, `pipewiresrc`,
VA-API, and `gst-libav` with **avenc_aac** (AAC-LC), **avenc_prores_ks** (ProRes
422) and the decoders `probe` needs. It ships no `x264enc` and no HEVC encoder.

`gst-inspect-1.0` run inside the built sandbox (from `test-flatpak.sh`, full log
in the phase report) gives exactly:

```
present x264enc x265enc avenc_aac avenc_prores_ks opusenc pipewiresrc appsrc appsink
        videoconvert videoscale mp4mux qtmux qtdemux h264parse h265parse
        avdec_h264 avdec_h265
ABSENT  openh264enc fdkaacenc
```

**H.264 → `org.freedesktop.Platform.codecs-extra`.** The runtime itself declares
the extension point (`flatpak info --show-metadata org.gnome.Platform//49`):

```
[Extension org.freedesktop.Platform.codecs-extra]
directory = lib/x86_64-linux-gnu/codecs-extra
version = 25.08-extra
add-ld-path = lib
autodelete = true
```

so *declaring nothing in the manifest* mounts the Flathub-signed extension for
the app and puts its plugin directory on `GST_PLUGIN_SYSTEM_PATH`. It contains
`libgstx264.so` (gst-plugins-ugly's `x264enc`) plus libx264, libx265 and a full
ffmpeg for HEIF. This is the Flathub-accepted way to get patent-encumbered
codecs; nothing is built into the app for H.264.

**HEVC → built into the app.** No Flathub runtime or extension has an HEVC
*encoder*: `codecs-extra` ships libx265 only for HEIF decoding, and its ffmpeg
has no `libx265` encoder (`avenc_libx265` is absent — checked in the sandbox).
The manifest therefore builds:

* **x265 4.1** (`-DENABLE_CLI=OFF`, shared library, assembly on; the tarball's
  two `cmake_policy(... OLD)` lines and `cmake_minimum_required(2.8.8)` are
  patched in `patches/x265-cmake4.patch` because CMake 4 in the SDK rejects
  them), and
* **gst-plugins-bad 1.26.11**'s `x265enc` only (`-Dauto_features=disabled
  -Dgpl=enabled -Dx265=enabled`), at the *same* 1.26.11 version as the runtime's
  GStreamer so the plugin loads into its core without an ABI mismatch.

x265 is GPL-2.0-or-later; it is a separate module, its licence is installed
under `/app/share/licenses/io.github.sultanaltair96.Rec/x265`, and the app itself
stays MIT OR Apache-2.0 (mere aggregation).

**AAC** comes from the runtime's `avenc_aac` (fdk-aac is non-free and not in the
runtime); the writer already knows its priming (`REC_AAC_PRIMING`, see
`src/media/writer.rs`). ProRes 422 is `avenc_prores_ks`, Opus is `opusenc`.

Ubuntu and the dev container use `x264enc`/`x265enc` from RPM Fusion or
gst-plugins-ugly/bad; the Flatpak ends up with the same two elements, so the
writer's candidate order picks the same encoders everywhere.

### Sandbox permissions (`finish-args`), each one justified

| Permission | Why |
|---|---|
| `--socket=wayland`, `--socket=fallback-x11` | winit/wgpu window on Wayland, or X11 on an X11 session |
| `--share=ipc` | `wl_shm`/MIT-SHM for the window's buffers |
| `--device=dri` | a render node for wgpu's Vulkan/GL compositor; without one the recorder composites on the CPU instead |
| `--socket=pulseaudio` | system audio (default sink monitor) and microphone through PipeWire's PulseAudio compatibility |
| `--filesystem=xdg-run/pipewire-0` | the PipeWire socket: screen frames from the ScreenCast portal, and the audio nodes phase 1B opens directly |
| `--share=network` | RTMP/WHIP streaming and update checks |
| `--filesystem=xdg-videos:create` | the default recording folder (`$XDG_VIDEOS_DIR`, else `~/Videos`) |
| `--talk-name=org.freedesktop.secrets` | stream keys in the Secret Service |
| `--talk-name=org.kde.StatusNotifierWatcher` | the tray icon (StatusNotifierItem) |

Deliberately *not* granted: `--device=all`, the home directory, any other
filesystem, and the session bus.

**Cameras.** The camera sources use the Camera portal when running inside a
sandbox (`capture::camera::portal`), so no device access is requested.
**Trade-off:** the direct V4L2 path (`--cam /dev/video0` by name or number) sees
no `/dev/video*` inside the sandbox. A user who needs it can widen the sandbox
for this app only:

```sh
flatpak override --user --device=all io.github.sultanaltair96.Rec
```

That is the documented trade-off: the default is the safe, portal-only camera;
direct V4L2 is opt-in per user. The Flatpak does not grant it for everyone.

**Files.** Settings live in the sandbox's XDG config dir
(`~/.var/app/io.github.sultanaltair96.Rec/config/rec-rs/settings.json`), the
portal restore token in its state dir, and recordings in `~/Videos`. Flatpak can
only map `xdg-videos` when the host has XDG user directories configured
(`~/.config/user-dirs.dirs`), which every desktop environment writes; the test
container creates one to look like a desktop. If it is missing, Flatpak silently
drops the mapping and `~/Videos` inside the sandbox is not the host folder — the
test creates the file for exactly that reason.

### Build and test

```sh
packaging/flatpak/build-flatpak.sh     # Flathub builder image ghcr.io/flathub-infra/flatpak-github-actions:gnome-49
packaging/flatpak/test-flatpak.sh      # installs the bundle in a clean Fedora container, runs it as a user
```

`build-flatpak.sh` runs `flatpak-builder` privileged (bubblewrap inside Docker)
with an 8 GB memory ceiling and 8 jobs, in the official Flathub builder image
pinned by tag `gnome-49`; the named volume `rec-rs-flatpak` keeps runtimes and
the build cache between runs. The result is a single-file bundle.

`test-flatpak.sh` installs that bundle system-wide in a **plain `fedora:43`
container with only `flatpak`** (not the builder image), as a normal user, and
checks 9 things: H.264 record to `~/Videos` through the CLI, the file appearing
on the host side of the Videos mount, `probe` inside the sandbox, the default
output folder, HEVC, ProRes, the no-subcommand entry point, and the installed
desktop/metainfo/icon files. Result: **9/9 pass**, encoder names printed by the
recorder (`x264enc`, `x265enc`, `avenc_prores_ks`) and `probe` reporting
`video h264 1920x1080 @ 60.000 fps` / HEVC.

Bundle size: **3.5 MB** (`rec.flatpak`) — the runtime is shared. The manifest
also produces an `io.github.sultanaltair96.Rec.Debug` ref (22 MB of debug info
from `[profile.release] debug = 1`), which stays in the repository and is not
part of the bundle.

### Flathub submission notes

* The app source is a local `dir` source so `build-flatpak.sh` works on a
  checkout. For a Flathub submission, replace it with the release tag:

  ```yaml
  - type: git
    url: https://github.com/sultanaltair96/rec-rs-linux
    tag: v0.1.0
    commit: <sha>
  ```

* `packaging/update-sources.sh` must be run and the result committed whenever
  `Cargo.lock` changes (every later phase adds crates).
* The metainfo's screenshots still point at upstream's macOS captures; replace
  them with Linux screenshots (and remote URLs) once phase 1E renders the
  window. The `TODO(packaging)` comment in the file says so.
* The app entry point currently prints *"the graphical app is not implemented
  yet (phase 1E)"* and exits 1. The package is complete for the CLI and becomes
  a windowed app when 1E lands; nothing in the packaging needs to change for
  that.

## AppImage

### Base and glibc floor

Built in `ubuntu:24.04`, so the binary needs glibc ≥ 2.39: **Ubuntu 24.04+,
Debian 13+, Fedora 40+**. A 22.04 base would be nicer (glibc 2.35) but is
impossible: pipewire-rs 0.10 (`libspa` 0.10.1) uses
`spa_video_info_raw.flags`, `spa_meta_first` and `spa_meta_region_is_valid`,
which Ubuntu 22.04's PipeWire 0.3.48 does not have — the build fails there.
24.04's PipeWire 1.0 is the oldest base that compiles it, and building against
newer headers while linking an older host library is exactly the ABI mismatch
the PipeWire exclusion below exists to avoid.

### Bundled GStreamer

`build-inside.sh` resolves each element the recorder and the probe can ask for to
its plugin file with `gst-inspect-1.0` (so a renamed or split plugin package
cannot silently drop an element), copies those plugins, and lets linuxdeploy
resolve their library dependencies:

* encoders: `x264enc`, `x265enc`, `openh264enc` (if present), `avenc_aac`,
  `avenc_prores_ks`, `opusenc`, `vorbisenc`, `voaacenc`, VA-API (`vah264enc`,
  `vah265enc`, `vaapih264enc`, `vaapih265enc`) when the plugin exists;
* containers/muxing: `mp4mux`, `qtmux`, `qtdemux`, `matroskamux`;
* parsing/decoding for `probe`: `h264parse`, `h265parse`, `aacparse`,
  `avdec_h264`, `avdec_h265`, `avdec_aac`, `avdec_prores`;
* plumbing: `appsrc`/`appsink`, `videoconvert`/`videoscale`, `audioconvert`,
  `audioresample`, `volume`, `level`, `queue`, `capsfilter`, `typefindfunctions`,
  `uridecodebin`, `decodebin`, `parsebin`, `multiqueue`, `fakesink`, `filesink`,
  `filesrc`, `videotestsrc`, `tee`, `valve`.

The bundle ends up with **28 plugins** (their list is printed by
`test-appimage.sh`). A VA-API plugin without a GPU, or without a driver in the
host's `/usr/lib/*/dri`, simply registers no elements; the writer then picks
`x264enc`/`x265enc` as on the Flatpak.

### What deliberately comes from the host

| Library | Why not bundled |
|---|---|
| glibc, libm, libpthread, libdl | A bundled glibc does not work; linuxdeploy's excludelist keeps them out |
| libGL, libEGL, libvulkan, Mesa/GPU drivers | must match the user's GPU and driver |
| the VA-API driver (iHD/radeonsi/...) | it must match the GPU; `libva` itself *is* bundled, because `libavcodec` links it directly and a host without it would lose AAC, ProRes and the HEVC decoder |
| libdbus-1 | must match the running session bus |
| **libpipewire-0.3 / libspa-0.2** | the client ABI has to match the *daemon* it talks to; a bundled client against the host's PipeWire is the classic AppImage failure. Every desktop ships it. If it is missing, the recorder fails to start; screen/audio capture would not work without a PipeWire daemon anyway |
| the Vulkan ICDs and the GL drivers | `libvulkan.so.1` (the loader) is bundled because ffmpeg's libavcodec links it; the ICD JSON files and the drivers behind it are the host's |

`AppRun` narrows `GST_PLUGIN_SYSTEM_PATH_1_0` to the bundled plugins only, so a
host GStreamer can never be mixed into the bundled core, and points
`GST_REGISTRY_1_0` at `~/.cache/rec-rs-appimage/` so the host's registry never
accumulates paths inside a temporary AppImage mount.

### Updates

`appimagetool` embeds

```
gh-releases-zsync|sultanaltair96|rec-rs-linux|latest|Rec-*x86_64.AppImage.zsync
```

(`--appimage-updateinformation` prints it back) and writes
`Rec-<version>-x86_64.AppImage.zsync` next to the AppImage. Both are attached to
each GitHub Release, so AppImageUpdate and any zsync-aware launcher can update
in place. The app's own "Check for Updates" menu item belongs to phase 1E; the
feed it will read exists from the first release.

### Build and test

```sh
packaging/appimage/build-appimage.sh   # ubuntu:24.04 container, writes AppImage + zsync
packaging/appimage/test-appimage.sh    # Fedora 43 and Debian 13, no GStreamer, ordinary user
```

The build container has the Ubuntu GStreamer packages whose plugins are bundled,
`linuxdeploy` 1-alpha-20251107-1 and `appimagetool` 1.9.1 (both pinned by release
*and* sha256 in `build-inside.sh`), and rustup. The named volume
`rec-rs-appimage-cache` caches the toolchain and the target directory.

The test runs the AppImage on **Fedora 43 and Debian 13 containers with no
GStreamer installed** (the only host package added is the PipeWire client
library, see the table above). It records H.264, HEVC and ProRes with the test
pattern, probes each file with the bundled GStreamer, checks that no bundled
plugin failed to load, extracts the bundle to list its plugins and the libraries
that still come from the host, and prints the embedded update information.
Result on both distributions: **7/7 checks pass**, `x264enc`, `x265enc` and
`avenc_prores_ks` chosen, `probe` reporting `video h264 1920x1080 @ 60.000 fps`
and HEVC, 28 bundled plugins, and the only host libraries resolved are
`libpipewire-0.3.so.0` and glibc (`libc`, `libm`, `ld-linux`).

Sizes: AppImage **93.6 MiB** (98,126,328 bytes), `.zsync` 328 KiB. Most of it
is the ffmpeg stack that `gst-libav` needs (libavcodec, libx265, libavfilter,
libplacebo, libaom, ...). That is the price of AAC and ProRes in a single file;
the Flatpak gets the same codecs from the runtime for 3.5 MB.

AppImages normally mount themselves with FUSE; containers have no FUSE, so both
scripts use the runtime's own `APPIMAGE_EXTRACT_AND_RUN=1`, which extracts the
same image and runs the same `AppRun`.

## Desktop integration

* **Desktop entry** `io.github.sultanaltair96.Rec.desktop`: `Exec=rec-rs gui`,
  `StartupWMClass` = the app id, categories `AudioVideo;Recorder;`, and the two
  upstream Dock-menu equivalents as actions:
  `Start Recording` → `rec-rs gui --start-recording`, `Settings` →
  `rec-rs gui --settings`.
* **AppStream metainfo**: description, feature list, homepage/bugtracker/VCS
  links, OARS 1.1 content rating, licence `MIT OR Apache-2.0`, an 0.1.0 release,
  `provides` for both binaries and screenshots. The screenshots are upstream's
  macOS captures at the pinned commit with a `TODO(packaging)` until Linux
  screenshots exist.
* **Icons**: hicolor PNGs at 16, 24, 32, 48, 64, 128, 256 and 512 px, converted
  from upstream's `support/icon.png` (1024×1024, MIT OR Apache-2.0, same
  repository) with Lanczos resampling. There is no SVG: upstream ships a PNG and
  an `.icns`, and tracing a scalable icon would not be faithful.
* **Provenance**: the icon and the screenshot subject are upstream's, licensed
  MIT OR Apache-2.0 (see the repository's `LICENSE-MIT`/`LICENSE-APACHE`); the
  metadata files here are new (metainfo itself CC0-1.0, as AppStream expects).

Validation (Fedora container, `appstream` and `desktop-file-utils`):

```sh
desktop-file-validate packaging/desktop/io.github.sultanaltair96.Rec.desktop   # no output: valid
appstreamcli validate --no-net packaging/desktop/io.github.sultanaltair96.Rec.metainfo.xml
# ✔ Validation was successful: pedantic: 1
```

The single pedantic note is `cid-contains-uppercase-letter` for
`io.github.sultanaltair96.Rec`: the app id is prescribed by the project and
matches the GitHub account (`sultanaltair96`), so the note is expected.

## Release workflow

`.github/workflows/release.yml` runs only on a `v*` tag (never on branches or
pull requests). It has three jobs:

1. **flatpak** — `packaging/flatpak/build-flatpak.sh`, then `test-flatpak.sh`
   (so a broken bundle cannot be released), uploads `rec.flatpak`;
2. **appimage** — `packaging/appimage/build-appimage.sh`, then
   `test-appimage.sh`, uploads the AppImage and its `.zsync`;
3. **release** — checks that the tag equals the version in `Cargo.toml`,
   downloads both artifacts and creates the GitHub Release with
   `rec.flatpak`, `Rec-<version>-x86_64.AppImage` and the `.zsync` attached.

Actions are pinned by commit SHA (with the version in a comment), and the build
steps call the same scripts that were run locally, so CI cannot drift from a
verified build. The workflow was validated with `actionlint` and **never
triggered**: no tag was created.

## Maintenance

* **Cargo.lock changed** → `packaging/update-sources.sh`, commit
  `packaging/flatpak/cargo-sources.json` with the change.
* **New runtime** → bump `runtime-version`/`sdk` in the manifest, bump
  `org.freedesktop.Sdk.Extension.rust-stable` and `llvm21` to the same freedesktop
  branch, bump `gst-plugins-bad`'s version to the runtime's GStreamer version
  (`gst-inspect-1.0 --version` inside the runtime), and regenerate
  `cargo-sources.json` only if `Cargo.lock` changed meanwhile.
* **New linuxdeploy/appimagetool** → change the release URL and its sha256
  together in `packaging/appimage/build-inside.sh`.
* **`packaging/README.md` and `PARITY.md`** carry the measured evidence; update
  them when a number changes.

## Needs a real desktop

Everything below needs a display, a portal host, audio hardware or a camera,
which the build host (WSL, no display, no GPU passthrough, no webcam) cannot
provide. The exact checks:

1. **Flatpak GUI** — `flatpak install --user rec.flatpak && flatpak run
   io.github.sultanaltair96.Rec` opens the app window (phase 1E); before 1E it
   prints the phase message, which is what the container test checks.
2. **Flatpak screen capture** — inside the Flatpak, `flatpak run
   --command=rec-rs io.github.sultanaltair96.Rec permissions --request` shows
   the desktop's own ScreenCast picker once, then `record -t 5` produces a
   playable file; `flatpak permissions` shows no device escalation.
3. **Flatpak system audio and microphone** — with `--socket=pulseaudio` and the
   PipeWire socket, `record -t 5` writes both stems; the log's audio offsets are
   ≤ 1 ms.
4. **Flatpak camera** — `flatpak run --command=rec-rs ... devices` lists the
   Camera portal's camera without `--device=all`; `--cam` records a webcam
   overlay.
5. **Flatpak tray and hotkeys** — the StatusNotifierItem appears (KDE, or GNOME
   with the AppIndicator extension); the GlobalShortcuts portal prompt appears
   once for Ctrl+Shift+R/S.
6. **Flatpak VA-API** — on a GPU host `/dev/dri` makes the runtime's VA-API
   encoder available; `REC_DEBUG=1` shows which encoder was chosen.
7. **Flatpak update** — publish a newer build to the same remote,
   `flatpak update` picks it up.
8. **AppImage on a desktop** — `chmod +x Rec-*.AppImage && ./Rec-*.AppImage`
   opens the window; `record -t 5` captures the real screen through the portal
   with the *host's* PipeWire (this is the box the libpipewire exclusion is
   about).
9. **AppImage update** — upload a newer AppImage + `.zsync` to a GitHub Release
   and run `AppImageUpdate Rec-*.AppImage` (or wait for the app's 1E menu item).
10. **AppImage VA-API** — the bundled plugin plus the host's libva/driver should
    give `vah264enc` on a GPU; `REC_DEBUG=1` says which encoder ran.
