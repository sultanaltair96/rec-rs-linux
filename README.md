# rec-rs-linux

An independent Linux port of [rec-rs](https://github.com/michaelshimeles/rec-rs), a macOS screen
and camera recorder.

The goal is feature parity with the upstream application, distributed for Linux as a Flatpak and/or
AppImage. **This is a work in progress.** The foundation (command line, settings, mixer maths,
overlay geometry, pause timeline, test-pattern source, a GStreamer H.264/AAC writer and the probe)
is in place; screen, audio, camera and webcam capture, the GPU compositor, the app, streaming and
packaging are the phases after it. [`PARITY.md`](PARITY.md) tracks every upstream feature against
what exists here, with the test or script case that proves it.

This project is not affiliated with the upstream author. Upstream licensing and attribution are
preserved: code adapted from rec-rs keeps a `// Adapted from rec-rs <path> (MIT OR Apache-2.0)`
note at the top of the file.

## Building and testing

The build host needs Docker only: no Rust toolchain, no system development libraries. Everything
runs in the container defined by [`dev/Dockerfile`](dev/Dockerfile), through
[`scripts/dev.sh`](scripts/dev.sh).

```sh
git submodule update --init          # upstream reference sources (read-only)

scripts/dev.sh cargo build --release
scripts/dev.sh cargo test
scripts/dev.sh cargo clippy --all-targets -- -D warnings
scripts/dev.sh cargo fmt --check
scripts/dev.sh scripts/selftest.sh   # records and verifies short clips headless
```

The image is `fedora:43` with Rust stable, GStreamer 1.26, PipeWire and ffmpeg. What encoders it
actually has (and which are missing from Fedora's free repositories) is measured in
[`dev/ELEMENTS.md`](dev/ELEMENTS.md).

Test recordings go to `${REC_TEST_DIR:-$HOME/Videos/rec-rs-tests}`, never into the repository.

## Command line

```sh
rec-rs record --test-pattern -t 3 --no-mic -o /tmp/demo.mp4   # works today
rec-rs probe /tmp/demo.mp4                                    # decode every sample
rec-rs devices                                               # displays, windows, cameras, mics
```

The full upstream option table is implemented and parses; flags whose backend is a later phase
fail at run time with the phase that implements it, never silently. `rec-rs --help` and
`rec-rs record --help` list everything.

A recording can be controlled from any shell, as on macOS:

```sh
rec-rs pause        # SIGUSR1
rec-rs resume       # SIGUSR2
rec-rs stop         # SIGINT: finalizes, prints the file
rec-rs save-replay  # SIGRTMIN+1 (the replay buffer is phase 1D)
```

## Advanced settings

Recording disk safeguards can be tuned with environment variables (positive whole
numbers in decimal MB, 1 MB = 1,000,000 bytes):

- `REC_MIN_FREE_MB`: free space required before recording starts. Default: the
  larger of 2 GB and five minutes at the requested video bitrate.
- `REC_LOW_WATER_MB`: stop and finalize when available space falls below this
  threshold, checked every five seconds outside the writer lock. Default: at
  least 1 GB, and at least four times the 128 MiB movie-index limit plus one
  fragment and five seconds at the maximum bitrate (80 Mb/s video plus three
  320 kb/s audio tracks, or the requested bitrate if higher). The fragment
  allowance follows `REC_FRAGMENT_SECS` (default two seconds; this existing
  muxer override uses milliseconds despite its name).

Explicit values replace these defaults; invalid or overflowing values fail before
recording starts. `REC_LOW_WATER_MB=0` disables the periodic low-space stop;
this is unsafe and intended for fault testing. `REC_DISK_CHECK_MS` overrides the
poll interval (minimum 10 ms). Admission always requires at least the low watermark.
Lowering the reserves risks running out of space during finalization, especially
with variable-bitrate encoders, ProRes or other concurrent filesystem writers.
Only space available to the current user counts. Live-only sessions skip these checks.

## Packaging

Two installable packages are built from this repository by scripts in `packaging/`. Both run the
same binary: with no subcommand it starts the app, and every command (`record`, `probe`, `devices`,
`pause`, ...) behaves exactly as it does from a terminal.

**Flatpak** — `io.github.sultanaltair96.Rec` on `org.gnome.Platform` 49, with H.264 through the
Flathub-signed `codecs-extra` extension and HEVC through the `x265` + `gst-plugins-bad` module the
manifest builds:

```sh
packaging/flatpak/build-flatpak.sh            # in a container; writes $REC_PKG_OUT/rec.flatpak
flatpak install --user "$REC_PKG_OUT/rec.flatpak"
flatpak run io.github.sultanaltair96.Rec record --test-pattern -t 3 -o ~/Videos/x.mp4
flatpak run --command=rec-rs io.github.sultanaltair96.Rec probe ~/Videos/x.mp4
```

**AppImage** — carries its own GStreamer (x264, x265, AAC, Opus and ProRes encoders plus the
muxers and the decoders `probe` uses), built on Ubuntu 22.04 so it starts on every current
distribution:

```sh
packaging/appimage/build-appimage.sh          # in a container; writes $REC_PKG_OUT/Rec-<version>-x86_64.AppImage
"$REC_PKG_OUT/Rec-<version>-x86_64.AppImage" record --test-pattern -t 3 -o /tmp/x.mp4
"$REC_PKG_OUT/Rec-<version>-x86_64.AppImage" probe /tmp/x.mp4
```

Both need Docker on the build host and nothing else: every build and every check runs in a
container. `REC_PKG_OUT` defaults to `~/.cache/rec-rs-linux-packaging`, never the repository. The
codec decision per package, the sandbox permissions, what comes from the host, the desktop
integration files (with their upstream provenance) and the checks that need a real desktop are in
[`packaging/README.md`](packaging/README.md).

Releases are built by [`.github/workflows/release.yml`](.github/workflows/release.yml) when a `v*`
tag is pushed; it attaches `rec.flatpak`, `Rec-<version>-x86_64.AppImage` and the matching
`.zsync` to the GitHub Release.

## Development

See [`AGENTS.md`](AGENTS.md) for the ownership map (which phase owns which directory), the module
contract (`src/media_types.rs`) and the conventions, and [`docs/PORT_PLAN.md`](docs/PORT_PLAN.md)
for the binding architecture decisions.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.

## Attribution

rec-rs-linux is a port of [rec-rs](https://github.com/michaelshimeles/rec-rs) by Michael Shimeles,
licensed MIT OR Apache-2.0. Ported code keeps its upstream copyright:

```
Copyright (c) 2026 Michael Shimeles
Copyright (c) 2026 rec-rs-linux contributors
```
