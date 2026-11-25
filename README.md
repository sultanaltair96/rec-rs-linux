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

## Diagnostics

```sh
rec-rs report                      # timestamped archive in Videos; prints path
rec-rs report /tmp/rec-support.tar  # chosen path (--output also supported)
```

Creates a new **0600**, uncompressed tar; refuses to overwrite files or symlinks.
Review `report.json` before sharing. Nothing is uploaded. Every CLI export prints:
“The report includes device names (e.g. Bluetooth headset names); review before sharing.”
The destination is printed after export completes.

`report` (alias `diagnostics`) asks the running GUI to write the archive, including
its memory-only log ring. Relative destinations are resolved in the CLI's working
directory. If no GUI is listening, it exports local diagnostics and explicitly warns
that the running app's log ring is unavailable. Other control/export errors are
reported, not silently retried locally. No previous-crash history is available.

Includes app version, OS/architecture, normalized session type, Flatpak presence,
GStreamer version, allowlisted encoder availability, sanitized numeric/boolean
persisted settings, typed lifecycle events, filtered device names, and up to five
numeric ffprobe summaries of recent recordings (not the recordings or filenames).
Encoder presence does not guarantee working hardware; `collection_status` describes
collection limits and unavailable sections.

Stderr is teed unchanged to the terminal and asynchronously captured in a bounded,
process-local ring of the last 5000 filtered lines, including native library warnings.
Credential/URL-bearing lines are redacted before retention; oversized lines and
queue overload are dropped and counted. Filtering is **best effort**: personal paths,
device names and arbitrary private text may remain. No environment dump or recorded
media is included. Unknown session values become `unknown`. **Review before sharing.**

App integration: call `diagnostics::record(Event::...)` for safe lifecycle events,
`diagnostics::snapshot()` for a bounded view, and `diagnostics::write_report(path)`
on a worker thread after the user chooses a destination. Do not add arbitrary text
or errors to the event vocabulary. A failed write may leave a partial private archive;
remove it and retry with a new path. Tar is supported; ZIP is not currently offered.

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
