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
