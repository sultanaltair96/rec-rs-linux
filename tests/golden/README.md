# GPU compositor fixtures

`*.png` are **golden images** for the compositor: each is one frame rendered by
`gpu::Renderer` at 1920x1080 from a synthetic screen (a test card with flat
fields, hairline rules, a diagonal and a 1-px checker patch) and a synthetic
camera (a gradient with a white cross and red/green/blue thirds, so a mirror or
a crop is unmistakable). `src/gpu/golden_tests.rs` asserts every one of them
against a fresh render, with a tolerance loose enough that Mesa's software
Vulkan adapter and a real GPU both pass (worst channel ≤ 8, mean ≤ 0.75).

| File | What it shows |
|---|---|
| `screen.png` | the screen scene at 1:1 (no scaling pass runs) |
| `screen-4k-to-1080.png` | the screen scaled 3840x2160 → 1920x1080 with Lanczos (antialiased: the 1-px checker patch averages to its grey mean, hairline rules to half-strength lines, and the picture is within 50 dB of ffmpeg's own Lanczos) |
| `screen-cam-rounded.png` | the default overlay: rounded, 22%, bottom-right, mirrored |
| `screen-cam-circle.png` | circle, top-left corner preset |
| `screen-cam-square-border.png` | square, 6 px border at 1080p, centred |
| `screen-cam-pill.png` | pill, bottom-left corner preset |
| `screen-cam-squircle-shadow.png` | squircle with roundness 0.5, a 3 px border and the drop shadow |
| `cam.png` | the camera scene: a 1280x720 camera cover-fitted into 1920x1080 |

## Regenerating

Only when the picture is *meant* to change (a style change, a new sampling
rule), and review the diff before committing:

```sh
scripts/dev.sh env REC_GPU_UPDATE_GOLDEN=1 cargo test gpu::golden
```

The images are rendered on whatever adapter the machine has; the comparison
then runs on the same one and, in CI, on lavapipe.

## The scaler

Lanczos with a half-width of 3, **stretched by the reduction factor** (6 taps an
axis at 1:1, 12 at 2:1, 17 at 2.67:1): reducing a picture means removing
frequencies, so the filter widens with the ratio. An interpolation kernel — six
fixed taps at the source's spacing — leaves a 1-px pattern as a beat pattern
instead of its mean; on a 5120x2880 black/white stripe frame scaled to 1080p
that is a 121.7/255 swing, and with the stretched filter 0.7/255 inside the
frame (the outermost column moves by 22.7 only because the edge pixel is
replicated, where ffmpeg's swscale reaches 22.6 and zimg 29.7 on the same
pattern).

Two independent checks:

- **ffmpeg**, on the test card, same RGB input, 4K → 1080p and 5K → 1080p:
  50.3 and 51.4 dB against `scale=flags=lanczos+accurate_rnd+full_chroma_int`,
  58.1 and 57.0 dB against zimg's Lanczos (`zscale`). Run by
  `scripts/selftest.sh compositor` (`cargo test gpu::tests::the_scaler_matches
  -- --ignored`).
- **A CPU reference** written from the textbook definition
  (`gpu::tests::lanczos_matches_a_cpu_reference`), which the GPU matches to
  1/255.

## Performance on the software adapter

Mesa lavapipe (`llvmpipe (LLVM 21.1.8, 256 bits)`, Vulkan 1.4, CPU), release
build, 24 frames a pass, the fastest and the median of five passes, readback
included:

| Case | Synchronous (ms/frame) | Pipelined (ms/frame) |
|---|---|---|
| 1080p output, 1080p screen | 7.0 / 7.2 | 5.0 / 5.0 |
| 1080p output, 4K screen (Lanczos 2:1) | 93 / 97 | 87 / 93 |
| 1080p output, 4K screen + 720p camera, NV12 | 79 / 83 | 84 / 85 |
| 4K output, 4K screen | 20 / 21 | 14 / 18 |
| 4K output, 4K screen + 720p camera, NV12 | 31 / 35 | 22 / 23 |
| 1080p output, 5K screen (Lanczos 2.67:1) | 122 / 141 | 129 / 142 |

- **Synchronous** is `pipeline_depth: 1` (wait for every frame);
  **pipelined** is the default depth of 2, where one frame's readback overlaps
  the next frame's GPU work. The difference is the readback stall.
- **The host is shared** (the run above had a load average of 19 on 20 cores,
  with two other dev containers building at the same time), so give these ±20%:
  a quieter moment measured 77 ms for the 4K → 1080p case, and the unscaled
  cases swing less than the scaled ones because they are mostly copies. That is
  why the table shows the fastest *and* the median pass — the benchmark itself
  takes five.
- **The bottleneck is the Lanczos taps on a CPU rasteriser**: 12–17 filtered
  taps a pixel over two separable passes (4K → 1080p writes 1920x2160 then
  1920x1080 pixels, about 75M taps). Scaling costs 4–6× what the same output
  from an already-sized screen costs (5–7 ms); antialiasing with the stretched
  kernel costs about 1.7× the six-tap interpolating one (48 ms before the
  filter was stretched, 77–93 ms after). On a GPU those taps are nothing — a
  GPU filters orders of magnitude more per frame — which is why these numbers
  are a sanity check and a way to name the ceiling, not a target.
