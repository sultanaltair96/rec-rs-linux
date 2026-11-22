# GStreamer elements in the dev image

What the dev container actually has, measured with `gst-inspect-1.0` and real
encodes (not assumed), and what it means for the recorder. Regenerate after
changing `dev/Dockerfile`:

```sh
scripts/dev.sh sh -c 'for e in x264enc x265enc openh264enc avenc_prores_ks \
  fdkaacenc avenc_aac vah264enc vah265enc; do \
  gst-inspect-1.0 $e >/dev/null 2>&1 && echo "$e yes" || echo "$e no"; done'
```

## Versions

| | |
|---|---|
| Image | `fedora:43` (tagged by Dockerfile content, see `scripts/dev.sh`) |
| GStreamer | 1.26.11 (`gstreamer1`, `-plugins-base`, `-good`, `-bad-free`, `-bad-free-extras`, `-bad-freeworld`, `-ugly`, `-vaapi`, `-plugin-openh264`, `-plugin-libav`) |
| RPM Fusion | free + nonfree enabled for Fedora 43 |
| ffmpeg | `ffmpeg` 7.1.5 (full, RPM Fusion; replaces `ffmpeg-free`) |
| x265 | 4.1 (8-bit) |
| Rust | rustup stable 1.100.0 |

The image also carries the phase 1 test harness: a headless sway + xdg-desktop-portal
session, llvmpipe Vulkan, PipeWire (with its GStreamer plugin and PulseAudio
compatibility), gnome-keyring and V4L2 tools. That part is at the bottom.

## Video encoders

| Element | Present | Notes |
|---|---|---|
| `vah264enc`, `vah265enc` | **no** (in this container) | `gstreamer1-vaapi` is installed, but GStreamer registers **no** VA factory without a working VA display: with no GPU the plugin loads with 0 features, so `gst-inspect-1.0 vah264enc` says "No such element". There is no software VA-API driver in Fedora. On a GPU host (`/dev/dri` present, passed through by `scripts/dev.sh`) the elements appear. The writer skips VA candidates when `/dev/dri` is missing and trial-encodes them when it is not, so a GPU-less host records with x264enc instead of failing. |
| `x264enc` | **yes** | RPM Fusion `gstreamer1-plugins-ugly`. `bitrate` in **kbit/s** is honoured, `profile` is not a property (it follows the pixel format): with the writer's I420 capsfilter it produces **H.264 High**, upstream's profile. |
| `x265enc` | **yes** | RPM Fusion `gstreamer1-plugins-bad-freeworld`. `bitrate` in **kbit/s**, **HEVC Main**. ABR alone lets it run away on noisy frames (83 Mb/s for a requested 8 Mb/s); the writer sets `option-string=vbv-maxrate=<kbps>:vbv-bufsize=<kbps/2>`, which brings it to 8.7-9.7 Mb/s. |
| `openh264enc` | **yes** | Fedora's free-repository fallback. No `profile` property: it always encodes **Constrained Baseline**, and its `rate-control=bitrate` does not bound the bitrate in this build (255 Mb/s at a requested 2 Mb/s on 1080p60 noise; `enable-frame-skip=true` reaches the target by encoding 2 of 120 frames, which cannot be used for a constant-frame-rate recording). Used only when x264 is absent. |
| `avenc_prores_ks` | **yes** | ProRes 422 (`--codec prores`, `.mov` only), input I422_10LE. |
| `avenc_libx264`, `avenc_libx265`, `avenc_h264`, `avenc_hevc` | **no** | GStreamer's libav plugin registers ffmpeg's *native* encoders only; the libx264/libx265 wrappers are skipped. The writer keeps them as last-resort candidates for builds that do have them. |

Measured bitrate accuracy (1080p60, the test pattern's `--noise`):

| Encoder | Requested | 2 s clip | 5 s clip | Profile |
|---|---|---|---|---|
| `x264enc` | 8 Mb/s | 8.68 Mb/s | 8.09 Mb/s | High |
| `x265enc` (+ VBV) | 8 Mb/s | 9.68 Mb/s | 8.67 Mb/s | Main |
| `x265enc` (no VBV) | 8 Mb/s | 83.0 Mb/s | 83.0 Mb/s | Main |

`-r 4k` picks HEVC automatically (above 1080p, as upstream does) and works out of
the box: 3840x2160 @ 60 fps, HEVC Main, `rec-rs probe` decodes all 120 frames.

## Audio encoders

| Element | Present | Notes |
|---|---|---|
| `fdkaacenc` | **yes** | Fedora ships `fdk-aac-free`; bitrate in bits per second. Preferred. |
| `avenc_aac` | **yes** | ffmpeg's native AAC (full `ffmpeg` now). Bitrate in bits per second. |
| `voaacenc`, `faac` | **no** | Not packaged. |

Both AAC-LC encoders add priming (the MDCT look-ahead) to the start of the
stream, measured with a 10 ms 1 kHz click encoded and decoded again:

| Encoder | First sample of the click in the decoded file | Priming |
|---|---|---|
| `fdkaacenc` | sample 2052 (42.8 ms) | 2048 samples |
| `avenc_aac` | sample 1027 (21.4 ms) | 1024 samples |

`qtmux`/`mp4mux` 1.26 cannot write the MP4 edit list (`elst` with `media_time`)
that tells a decoder to skip those samples — AVFoundation does, which is why
upstream measures 0.0 ms. The writer therefore drops the priming frames and
re-stamps the rest (`media::writer::aac_priming`, `REC_AAC_PRIMING` to
override), which removes the 42.7 ms offset. The first kept frame then lacks
decoder history, so the very first ~10 ms of audio in a file fades in; every
later sample is exact (measured: 0.0 ms on every beep after the first, +9.9 ms
on the first one). Phase 1C owns the proper fix (an edit-list writer).

## Muxers, parsers, decoders

| Element | Present | Used for |
|---|---|---|
| `mp4mux`, `qtmux` | yes | `.mp4`/`.mov`; `fragment-duration=2000` gives crash-safe output (SIGKILL leaves a playable file, verified). |
| `h264parse`, `h265parse`, `aacparse` | yes | Bitstream framing for the muxer. |
| `openh264dec`, `avdec_h264`, `avdec_h265`, `fdkaacdec`, `avdec_aac` | yes | Decoding for `rec-rs probe` (through `uridecodebin`). |
| `videoconvert`, `audioconvert`, `audioresample` | yes | Format conversion into the encoders. |
| `videotestsrc`, `audiotestsrc` | yes | Pipeline experiments. |
| `opusenc` | yes | For WHIP (phase 1D). |
| `pipewiresrc`, `pipewiresink` | yes | From `pipewire-gstreamer`, for phase 1A/1B pipeline checks. |

## Test harness packages (phase 1)

| Need | Packages | Verified with |
|---|---|---|
| Headless desktop | `sway`, `foot`, `xdg-desktop-portal`, `xdg-desktop-portal-wlr`, `dbus-daemon`, `dbus-tools`, `dbus-broker`, `xorg-x11-server-Xvfb` | `sway version 1.11`, `foot version: 1.25.0`, `dbus-run-session -- true` ok, `Xvfb -help` |
| Software GPU for wgpu | `mesa-vulkan-drivers`, `mesa-dri-drivers`, `vulkan-loader`, `vulkan-tools` | `vulkaninfo --summary` → `deviceName = llvmpipe (LLVM 21.1.8, 256 bits)`, `apiVersion = 1.4.328` |
| PipeWire harness | `pipewire`, `wireplumber`, `pipewire-utils`, `pipewire-gstreamer`, `pipewire-pulseaudio`, `pulseaudio-utils` | `gst-inspect-1.0 pipewiresrc` → "PipeWire source", `pactl 17.0` |
| Secret Service | `gnome-keyring`, `libsecret` | `gnome-keyring-daemon: 48.0` |
| Camera | `v4l-utils` | `v4l2-ctl 1.32.0` |

`sway`'s binary ships file capability `cap_sys_nice=ep`; `scripts/dev.sh` adds
`--cap-add SYS_NICE` so executing it in the container does not fail with EPERM.

The GStreamer registry is per image tag
(`GST_REGISTRY=/home/dev/.cache/gstreamer-1.0/registry-<Dockerfile hash>.bin`
inside the shared cache volume). Without that, a registry built by an older image
hides elements a newer one added — a stale registry reported no `avdec_h264`
after `ffmpeg-free` was replaced by the full `ffmpeg`.

## Not in the image, needed later

| Need | Phase | How |
|---|---|---|
| Hardware H.264/HEVC | 1C | VA-API on a real desktop; the container has no GPU, so the elements are unavailable here (see above). |
| HEVC/HDR 10-bit | 1C | x265 Main10 (`x265enc` with 10-bit input) if upstream parity needs it. |
| NVENC | 1C | Not in GStreamer 1.26; not claimed. |
