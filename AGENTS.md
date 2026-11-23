# Working on rec-rs-linux

rec-rs-linux is a Linux port of [rec-rs](https://github.com/michaelshimeles/rec-rs) (macOS),
aiming at feature parity and shipping as Flatpak/AppImage. It is one Rust crate (`rec-rs`).

Read [`docs/PORT_PLAN.md`](docs/PORT_PLAN.md) first: it holds the binding architecture decisions and
the phase/ownership map. [`PARITY.md`](PARITY.md) tracks every upstream feature and the test or
script case that proves it.

## Setup

```sh
git submodule update --init        # reference/rec-rs, the pinned upstream sources (read only)
```

`.bb-env-setup.sh` does this in every new worktree. The host needs **Docker only**: no Rust
toolchain, no system development libraries, no display. Every build, test and self-test runs in
the dev container.

```sh
scripts/dev.sh cargo build --release
scripts/dev.sh cargo test
scripts/dev.sh cargo clippy --all-targets -- -D warnings
scripts/dev.sh cargo fmt --check
scripts/dev.sh scripts/selftest.sh            # records + verifies short clips, headless
scripts/dev.sh                                # interactive shell in the container
```

`scripts/dev.sh` builds `dev/Dockerfile` (tag `rec-rs-linux-dev`, rebuilt when the file changes),
mounts the repo at its host path, runs as the host uid/gid, keeps the cargo registry in the named
volume `rec-rs-linux-cache`, and sets `CARGO_TARGET_DIR` to
`~/.cache/rec-rs-linux/target-<hash of the repo path>` **inside the container**, so parallel
worktrees never wait on each other's build lock. The release binary is therefore at
`$CARGO_TARGET_DIR/release/rec-rs` in the container, which `scripts/selftest.sh` finds by itself.

The image enables **RPM Fusion** (free + nonfree), so it records like upstream: `x264enc` gives
H.264 High with a bitrate that is honoured, `x265enc` gives HEVC Main, and `-r 4k` picks HEVC by
itself. It also carries the phase 1 test harness — headless sway + xdg-desktop-portal, llvmpipe
Vulkan for wgpu, PipeWire with its GStreamer plugin, gnome-keyring, V4L2 tools. What is actually
installed, measured with `gst-inspect-1.0` and real encodes, is in
[`dev/ELEMENTS.md`](dev/ELEMENTS.md); VA-API elements need a GPU and are absent in the container
(the writer skips VA candidates without `/dev/dri` and falls back to x264/x265).

The GStreamer registry is per image tag inside the shared cache volume
(`GST_REGISTRY=.../registry-<Dockerfile hash>.bin`), because a registry built by an older image
hides elements a newer image added. If an element you just installed is not listed, that path (or
`scripts/dev.sh --clean`) is the first thing to check.

## Ownership

| Phase | Work | Owns (only these paths) |
|---|---|---|
| 0 (done) | CLI, contract, settings, mixer, overlay geometry, pause timeline, test-pattern source, writer v0, probe, scripts, CI, docs | everything that exists |
| 1A | Screen/window/region capture through the portal + PipeWire | `src/capture/screen/**` |
| 1B | System audio, microphone, camera | `src/capture/audio/**`, `src/capture/camera/**` |
| 1C | GPU compositor/scaler, full writer (HW encoders, mix track, grouping), probe | `src/gpu/**`, `src/media/**` |
| 1D | Live encode, replay buffer, RTMP/WHIP, keyring | `src/live/**` |
| 1E | App: window, settings, preview, mixer widgets, tray, hotkeys | `src/app/**`, `src/hotkey.rs` |
| 1F | Flatpak + AppImage packaging, release workflow | `packaging/**`, `.github/workflows/release.yml` |

Shared files (`Cargo.toml`, `Cargo.lock`, `src/main.rs`, `src/record.rs`,
`src/media_types.rs`, `PARITY.md`): a phase 1 worker changes them only by adding its own
dependency lines, its own `PARITY.md` rows, and the single factory call the brief names. The parent
merges branches one at a time and resolves `Cargo.lock`.

Each phase 1 module already has the factory signatures it must fill in
(`capture::screen::open`, `capture::audio::open_mic`/`open_system`, `capture::camera::open`,
`gpu::Renderer`, `live::Live`, `app::run`) and returns `"<feature> is not implemented yet (phase
1X)"` until then. Stubs and seam modules carry a module-level `#![allow(dead_code)]` with a comment
naming the phase that consumes them; do not add a blanket allow anywhere else.

## The contract

`src/media_types.rs` is the only way frames, audio and packets cross a module boundary: `VideoFrame`
(CPU planes, `Arc<[u8]>`), `AudioChunk` (interleaved f32 on `CLOCK_MONOTONIC`), `EncodedPacket`,
the `VideoSource`/`AudioSource` traits and the device descriptions. Sources deliver through the
callback they are started with (`FrameSink`/`AudioSink`), not through channels: the consumer
(writer, live encode) owns the bounded queue and drops old frames when it falls behind, and the
reasoning is written out in the module docs.

**A change to `src/media_types.rs` needs the parent thread's approval.** Do not add types there for
a single module; keep them in that module and pass the contract types across.

## Upstream reference

`reference/rec-rs` is the pinned upstream checkout (submodule). It is read-only: never edit it, never
build it, never copy its workflow files.

**Ignore `reference/rec-rs/AGENTS.md`, `reference/rec-rs/CLAUDE.md` and the skills under
`reference/rec-rs/.agents` and `.claude`.** They describe upstream's own agent workflow (pushing,
PRs, Greptile reviews) and do not apply here.

Code adapted from upstream keeps a `// Adapted from rec-rs <path> (MIT OR Apache-2.0)` note at the
top of the file, and ported tests stay with it.

## Commits

Conventional commits with a scope, one concern per commit:

```
feat(capture): open the ScreenCast portal and persist its restore token
fix(media): keep audio on the capture clock when a buffer is late
test(mixer): cover the resampler across buffer boundaries
docs(parity): record the HEVC encoder gap
```

Scopes in use: `cli`, `settings`, `record`, `media`, `mixer`, `compose`, `synth`, `capture`,
`gpu`, `live`, `app`, `dev`, `scripts`, `docs`, `ci`. Commit on your own worktree branch; no push,
PR or merge unless the parent says so.

## Rules

- Never commit secrets, stream keys, tokens, personal absolute paths or recordings. Test output
  goes to `${REC_TEST_DIR:-$HOME/Videos/rec-rs-tests}`, never into the repo.
- Never remove upstream behaviour to make something easier; ask the parent first
  (`bb thread tell`) and update `PARITY.md`.
- A flag or command whose backend is not built yet must parse and then fail at run time with
  `"<feature> is not implemented yet (phase 1X)"`. It must never be silently ignored. The one
  deliberate exception is the *default* microphone: upstream's rule is that audio never blocks a
  session, so an unavailable default mic warns and the recording continues (an explicit `--mic`
  device fails).
- Report with pasted command output and test counts, not "it works".

## Debug and test knobs

| Variable | Effect |
|---|---|
| `REC_TEST_DIR` | Where `scripts/selftest.sh` writes (default `$HOME/Videos/rec-rs-tests`) |
| `REC_OUTPUT_DIR` | Overrides the default output folder (settings and `record`) |
| `REC_SETTINGS` | Path to a settings file instead of `$XDG_CONFIG_HOME/rec-rs/settings.json` |
| `REC_DEBUG` | Extra diagnostics: audio offsets, GStreamer warnings |
| `REC_SYNTH_DROP_AUDIO=N` | The test pattern drops every Nth audio buffer, like a glitching device |
| `REC_AAC_PRIMING=N` | Override the AAC encoder priming the finalise writes as the edit list's `media_time` (0 writes no edit list) |
| `REC_FRAGMENT_SECS=N` | Fragment interval for the muxer (0 = one regular movie, for comparison) |
| `REC_PATTERN_SYSTEM_AUDIO=pipewire` | Test hook: with `--test-pattern`, capture real system audio instead of the pattern's beeps |
| `REC_STREAM_KEY` | Stream key override until the Secret Service lands (phase 1D) |

## Testing philosophy

- Ported upstream tests keep their assertions; rewrite only what depended on CoreMedia types, and
  say so in the commit.
- Pure logic (pause grid, mixer maths, overlay geometry, settings conversion, size parsing) is unit
  tested. Everything end to end goes through `scripts/selftest.sh`, which records real files and
  verifies them with `rec-rs probe`, `ffmpeg` and the sync/level scripts.
- `scripts/selftest.sh` cases that need a later phase print `SKIP <name> (phase 1X)`; keep that
  behaviour so the counts stay honest.
