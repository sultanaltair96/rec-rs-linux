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
| `screen-4k-to-1080.png` | the screen scaled 3840x2160 → 1920x1080 with Lanczos |
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

## Performance on the software adapter

Mesa lavapipe (`llvmpipe (LLVM 21.1.8, 256 bits)`, Vulkan 1.4, CPU), release
build, 24 frames per case, readback included:

| Case | Synchronous (ms/frame) | Pipelined (ms/frame) |
|---|---|---|
| 1080p output, 1080p screen | 7.5 | 5.2 |
| 1080p output, 4K screen (Lanczos 2:1) | 48.0 | 47.4 |
| 1080p output, 4K screen + 720p camera, NV12 | 58.2 | 54.6 |
| 4K output, 4K screen | 32.3 | 25.4 |
| 4K output, 4K screen + 720p camera, NV12 | 31.4 | 27.9 |
| 1080p output, 5K screen (Lanczos 2.67:1) | 71.8 | 59.2 |

- **Synchronous** is `pipeline_depth: 1` (wait for every frame);
  **pipelined** is the default depth of 2, where one frame's readback overlaps
  the next frame's GPU work. The difference is the readback stall, 2–12 ms.
- **The bottleneck is the Lanczos compute on a CPU rasteriser**: the two
  separable passes over 12 taps a pixel cost more than the copies (4K → 1080p
  writes 1920x2160 then 1920x1080 pixels). Scaled frames are 6–9× slower than
  the same output from an already-sized screen, which is a straight
  upload → composite → readback (~5 ms at 1080p, ~25 ms at 4K).
- The copies matter at 4K: a frame is 33 MB up and 33 MB down, which is most of
  the 25 ms.
- These numbers are from a shared 20-core host, so they carry a few ms of
  noise. They are not a 60 fps claim: on a real GPU the same shaders are
  trivial, and the numbers here exist to prove nothing is pathological (no
  per-frame pipeline rebuild, no readback per pass) and to name the software
  adapter's ceiling.

Run them again with:

```sh
scripts/dev.sh cargo test --release gpu::perf_tests -- --ignored --nocapture
```
