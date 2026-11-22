# GStreamer elements in the dev image

What the dev container actually has, measured with `gst-inspect-1.0` (not
assumed), and what it means for the recorder. Regenerate after changing
`dev/Dockerfile`:

```sh
scripts/dev.sh sh -c 'for e in openh264enc avenc_h264 x264enc vah264enc \
  vaapih264enc x265enc vah265enc avenc_prores_ks fdkaacenc avenc_aac; do \
  gst-inspect-1.0 $e >/dev/null 2>&1 && echo "$e yes" || echo "$e no"; done'
```

## Versions

| | |
|---|---|
| Image | `fedora:43` |
| GStreamer | 1.26.11 (`gstreamer1`, `-plugins-base`, `-good`, `-bad-free`, `-bad-free-extras`, `-ugly-free`, `-plugin-openh264`, `-plugin-libav`) |
| ffmpeg | `ffmpeg-free` 7.1.5 (for `ffprobe`/`ffmpeg` in the self-tests) |
| Rust | rustup stable 1.100.0 |

## Video encoders

| Element | Present | Notes |
|---|---|---|
| `vah264enc` (VA-API) | **no** | Fedora's `gst-plugins-bad-free` does not build the VA plugin's encoders, and the container has no GPU anyway. Phase 1C/1F add it for real desktops (RPM Fusion, or the Flatpak runtime). |
| `vaapih264enc` | **no** | Same. |
| `x264enc` | **no** | Needs RPM Fusion's `gstreamer1-plugins-ugly`, which is not in Fedora's free repos. |
| `openh264enc` | **yes** | The only H.264 encoder here. Its `bitrate` property is in **bits per second**, `rate-control=bitrate` must be set for it to honour it, and the keyframe interval is `gop-size` (frames). |
| `avenc_h264` | **no** | `gst-libav` is built from `ffmpeg-free`, which has no libx264. |
| `x265enc` | **no** | RPM Fusion only. |
| `vah265enc` | **no** | As above. |
| `avenc_hevc` | **no** | Needs libx265. |
| `avenc_prores_ks` | **yes** | ProRes 422 (`--codec prores`, `.mov` only) works. |

So **HEVC cannot be recorded in this image at all**. `--codec hevc` and the
automatic "HEVC above 1080p" choice of `prepare` fail at run time with the
encoder list in the error; the self-test skips `pattern-hevc` with that reason.
Phase 1C/1F own the packaging work that brings x264/x265/VA-API in (Flatpak
ships `gst-plugins-ugly`; an AppImage bundles it).

The writer picks an encoder at run time from
`media::writer::video_encoder_candidates` (VA-API → x264 → openh264 →
libav) and logs the one it used, so the same binary gets hardware or x264
encoding on a desktop without a rebuild.

## Audio encoders

| Element | Present | Notes |
|---|---|---|
| `fdkaacenc` | **yes** | Fedora ships `fdk-aac-free`; bitrate in bits per second, `bitrate` property. Preferred. |
| `avenc_aac` | **yes** | ffmpeg's native AAC from `ffmpeg-free`. Bitrate in bits per second. |
| `voaacenc` | **no** | Dropped upstream by GStreamer. |
| `faac` | **no** | Non-free. |

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
on the first one). See PARITY.md's audio row.

## Muxers, parsers, utilities

| Element | Present | Used for |
|---|---|---|
| `mp4mux`, `qtmux` | yes | `.mp4` and `.mov`. `fragment-duration=2000` gives crash-safe fragmented output; a SIGKILL leaves a playable file (verified). |
| `h264parse`, `h265parse`, `aacparse` | yes | Bitstream framing for the muxer. |
| `videoconvert`, `audioconvert`, `audioresample` | yes | Format conversion into the encoders. |
| `openh264dec`, `fdkaacdec`, `avdec_aac` | yes | Decoding for `rec-rs probe` (through `uridecodebin`). |
| `avdec_h264` | **no** | ffmpeg-free has no H.264 decoder; `openh264dec` decodes our H.264. |
| `videotestsrc`, `audiotestsrc` | yes | Available for pipeline experiments (the self-tests use the `--test-pattern` source instead). |
| `pipewire` elements | no | Not needed: phase 1A/1B talk to PipeWire through the `pipewire` crate, not GStreamer. |

## Not in the image, needed later

| Need | Phase | How |
|---|---|---|
| Hardware H.264/HEVC | 1C | VA-API (`vah264enc`) on real desktops; NVENC is not in GStreamer 1.26. |
| x264/x265 software | 1C | RPM Fusion locally, Flatpak runtime in the shipped app. |
| HEVC | 1C | As above; `--codec hevc` says exactly this today. |
| Opus (WHIP) | 1D | `opusenc` is present (gst-plugins-base); the WHIP output uses it. |
