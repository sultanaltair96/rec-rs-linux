//! The GPU compositor and scaler (phase 1C).
//!
//! wgpu draws what upstream's CoreImage graph draws: the screen scaled with
//! Lanczos to the output size, the camera overlay with every shape, border,
//! shadow and mirror setting, the camera full frame, and a captured window's
//! transparent corners filled with its edge colour. The geometry, masks and
//! shadow maths are [`crate::compose`]'s; this module only draws them.
//!
//! It runs headless on Mesa's software Vulkan adapter (lavapipe) and on any
//! real Vulkan or GL device, and it can share the app's device, so the
//! recording and the live preview composite on the same GPU.
//!
//! # The pipeline, per frame
//!
//! ```text
//! screen frame ─upload─► BGRA/RGBA ─lanczos x─► RGBA16F ─lanczos y─► canvas
//!  (BGRA/RGBA/NV12/I420,             (separable a=3,    edge clamp,  (RGBA16F)
//!   corner fill by clamping)          no upscaling)
//! camera frame ─upload─► BGRA/RGBA ─┐
//!                                   ▼
//!                          composite ──► final (BGRA8) ──┬─► readback (BGRA)
//!                          (render pass)                └─► BT.709 ─► NV12/I420
//!                                ▲
//!                  decor (shadow + border + mask, cached)
//! ```
//!
//! The screen is scaled in the **encoded** domain, plane for plane, which is
//! where upstream's `MPSImageLanczosScale` runs over the Y and CbCr planes: no
//! linearisation, so text keeps its contrast, and the taps past an edge repeat
//! the edge pixel. The kernel is a Lanczos with a half-width of 3 (a = 3)
//! **stretched by the reduction factor** — six taps an axis at 1:1, twelve at
//! 2:1, seventeen at 2.67:1 — because reducing a picture means removing
//! frequencies: a fixed six-tap interpolator would fold everything above the
//! destination's Nyquist frequency back as moiré (a 5K screen at 1080p is the
//! worst case this app sees). It never upscales: a source smaller than the
//! output is drawn at 1:1 and centred with video black around it, and a source
//! that is already the output size skips both passes.
//!
//! The camera is sampled bilinearly by the composite pass, like CoreImage's
//! default `kCISamplerFilterModeLinear` on upstream's transform, and is
//! cover-fitted into the overlay (or into the frame, in the camera scene) with
//! [`crate::compose::fit`].
//!
//! # Colours
//!
//! Everything up to the output conversion is gamma-encoded (non-sRGB) RGBA:
//! blending in that domain is what a recorder's overlay wants (the border is
//! the colour the user picked, not that colour raised to a power), and it is
//! what the encoders and the preview expect.
//!
//! Output is either
//!
//! - **NV12** (the default) or **I420**: Y at full resolution, chroma at half
//!   resolution in both directions, converted to **BT.709 limited range**
//!   (Y 16..235, Cb/Cr 16..240) on the gamma-encoded values, with chroma
//!   averaged over each 2x2 block. That is the matrix the encoders are told
//!   about and the one every HD player assumes.
//! - **BGRA**, for the preview and the pixel tests: the final texture, read
//!   back unchanged.
//!
//! # Lag: the readback is pipelined
//!
//! Reading a frame back to the CPU means waiting for the GPU. [`Renderer`]
//! keeps two staging buffers and returns frames one call behind
//! ([`Options::pipeline_depth`] = 2): [`Renderer::render`] submits the frame for
//! `pts` and returns the frame the previous call submitted, whose own `pts`
//! travels with it. Slot N's GPU work then overlaps slot N-1's readback instead
//! of blocking it. That means
//!
//! - `render` returns `None` until the pipeline is full (once, at the start);
//! - the caller **must** call [`Renderer::flush`] at the end of a recording, or
//!   the last frame stays on the GPU;
//! - `pipeline_depth: 1` makes it synchronous (always `Some`, no overlap),
//!   which is what the preview and the unit tests use.
//!
//! # Live changes
//!
//! [`Renderer::set_scene`], [`Renderer::set_pip`] and a change of camera aspect
//! ratio apply from the next frame. The overlay's masks, border and shadow are
//! built once per size and style ([`decor`]) and cached, so moving the overlay
//! only changes a transform, exactly like upstream's `Decor` cache.
//!
//! # Threading
//!
//! [`Renderer`] is `Send`: the writer creates it on its own thread and keeps it
//! there. It needs no display and no window, and on a machine with no GPU it
//! runs on Mesa's software Vulkan device (lavapipe) instead of failing:
//! [`Renderer::describe_adapter`] says which one
//! ([`Renderer::is_software`] when it is the CPU). Nothing here is `Sync`:
//! one renderer belongs to one thread.
//!
//! # Performance
//!
//! `tests/golden/README.md` records the measured cost per frame on the software
//! adapter (with the readback) for 1080p and 4K, with and without the camera:
//! 5 ms for a 1080p screen at the output size, ~48 ms for a 4K screen scaled to
//! 1080p, ~25 ms for 4K at 4K. The Lanczos passes are the cost on a CPU
//! rasteriser; the copies dominate at 4K. Nothing is rebuilt per frame — the
//! pipelines, the textures and the overlay's decor are created on demand and
//! then reused — so the same shaders on a real GPU are close to free.
//!
//! # Tests
//!
//! `cargo test gpu::` runs the ported upstream render test, the shapes against
//! `compose::coverage`, borders, the shadow, mirroring, placement, the camera
//! scene, Lanczos against an independent CPU implementation (1/255), BT.709
//! against ffmpeg's own numbers, the corner fill, the pipelined readback and
//! the golden images under `tests/golden/`; `--ignored` adds the performance
//! measurement. Without an adapter they print why and return.

// The writer (phase 1G) and the app's preview (phase 1E) construct the
// renderer; until those land the binary has no other caller than the tests.
#![allow(dead_code)]

mod decor;
mod device;
pub mod dmabuf;
mod readback;
mod uniforms;
#[cfg(test)]
mod yuv;

#[cfg(test)]
mod golden_tests;
#[cfg(test)]
mod perf_tests;
#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tests;

use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::compose::{self, PipSpec, Scene};
use crate::media_types::{PixelFormat, VideoFrame};

use decor::{Decor, DecorKey};
use device::Gpu;
use readback::Layout;
use uniforms::Uni;

/// The canvas and intermediate format: 16-bit float, so a two-pass Lanczos
/// never quantises twice, gamma encoded like the source.
const FMT_WORK: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// The composited frame: BGRA, the order the encoder and egui both want, and
/// not sRGB (see the module docs).
const FMT_OUT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;
/// The working colour conversion of an upload.
const FMT_RGBA: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const FMT_Y: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;
const FMT_UV: wgpu::TextureFormat = wgpu::TextureFormat::Rg8Unorm;

use wgpu::TextureUsages as Usage;

const SHADER_COMMON: &str = include_str!("shaders/common.wgsl");
const SHADER_CONVERT: &str = include_str!("shaders/convert.wgsl");
const SHADER_LANCZOS: &str = include_str!("shaders/lanczos.wgsl");
const SHADER_COMPOSITE: &str = include_str!("shaders/composite.wgsl");
const SHADER_YUV: &str = include_str!("shaders/yuv.wgsl");

/// The pixel format of the frames [`Renderer::render`] returns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputFormat {
    /// Y plane plus interleaved CbCr at half resolution (BT.709, limited
    /// range).
    #[default]
    Nv12,
    /// Y, Cb and Cr planes (BT.709, limited range).
    I420,
    /// BGRA unchanged from the composited texture: the preview and the tests.
    Bgra,
}

impl OutputFormat {
    pub fn pixel_format(self) -> PixelFormat {
        match self {
            OutputFormat::Nv12 => PixelFormat::Nv12,
            OutputFormat::I420 => PixelFormat::I420,
            OutputFormat::Bgra => PixelFormat::Bgra,
        }
    }

    pub fn label(self) -> &'static str {
        self.pixel_format().label()
    }
}

/// A device the caller already owns, so the compositor draws on egui's device
/// instead of one of its own.
///
/// The point is the preview: on a shared device the composited frame can be
/// drawn straight from [`Renderer::preview_frame`], with no readback and no
/// upload. Everything else is unchanged, and a renderer built with
/// [`Renderer::with_device`] behaves exactly like one that owns its device.
#[derive(Clone)]
pub struct SharedGpu {
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    /// What the adapter calls itself, for the status line (a device cannot be
    /// asked).
    pub info: wgpu::AdapterInfo,
}

/// One composited frame as a GPU texture, for the app's preview.
///
/// `texture` is the renderer's own output texture: it is reused every frame, so
/// a preview must draw it before the next frame is composited (one queue, in
/// submission order, guarantees the reads see whole frames). Cloning the handle
/// keeps the texture alive; nothing is copied.
#[derive(Clone)]
pub struct PreviewFrame {
    pub texture: wgpu::Texture,
    /// The frame's size in pixels (the output size).
    pub width: u32,
    pub height: u32,
    /// When the frame belongs on the recording's grid, seconds on
    /// [`crate::media_types::now`]'s clock.
    pub pts: f64,
    /// Bumped when the texture is replaced (a new output size or format), so a
    /// consumer can cache a bind group for it without comparing handles.
    pub generation: u64,
}

/// Where a [`Renderer`] hands its composited texture to the app's preview.
///
/// Called from the writer's emission loop at the preview rate (30 fps), must not
/// block. Only wired when the renderer shares the app's device: a texture from
/// another device cannot be drawn in egui's pass.
pub type PreviewSink = Arc<dyn Fn(PreviewFrame) + Send + Sync>;

/// How a [`Renderer`] is built.
#[derive(Clone, Debug)]
pub struct Options {
    /// Output size in pixels.
    pub width: usize,
    pub height: usize,
    pub scene: Scene,
    pub pip: PipSpec,
    pub output: OutputFormat,
    /// The screen is a single window: fill its transparent rounded corners with
    /// the window's own edge colour (upstream's `CornerFill`).
    pub corner_fill: bool,
    /// Frames allowed in flight before a readback is collected: 2 overlaps one
    /// frame's readback with the next frame's GPU work, 1 is synchronous.
    pub pipeline_depth: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            width: 1920,
            height: 1080,
            scene: Scene::ScreenCam,
            pip: PipSpec::default(),
            output: OutputFormat::Nv12,
            corner_fill: false,
            pipeline_depth: 2,
        }
    }
}

/// A texture and its view, recreated when its size or format changes.
struct Tex {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
}

impl Tex {
    fn new(
        device: &wgpu::Device,
        label: &str,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
        usage: wgpu::TextureUsages,
    ) -> Tex {
        let (width, height) = (width.max(1), height.max(1));
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Tex {
            texture,
            view,
            width,
            height,
            format,
        }
    }

    /// The slot's texture, rebuilt if it is missing or does not match.
    fn ensure<'a>(
        slot: &'a mut Option<Tex>,
        device: &wgpu::Device,
        label: &str,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
        usage: wgpu::TextureUsages,
    ) -> &'a Tex {
        let reuse = slot.as_ref().is_some_and(|t| {
            t.width == width.max(1) && t.height == height.max(1) && t.format == format
        });
        if !reuse {
            *slot = Some(Tex::new(device, label, width, height, format, usage));
        }
        slot.as_ref().expect("just created")
    }
}

/// A captured frame's textures: the planes as uploaded, plus the working RGBA
/// conversion when one is needed.
#[derive(Default)]
struct Input {
    key: Option<(u32, u32, PixelFormat)>,
    planes: Vec<Tex>,
    rgba: Option<Tex>,
    /// Keep the allocations alive so pointer identity cannot be recycled.
    uploaded: Vec<(Arc<[u8]>, usize)>,
}

impl Input {
    /// True exactly when the plane buffers or their layout changed. Cloned
    /// frames share immutable Arc-backed planes; PTS alone is not an identity.
    fn camera_needs_upload(&mut self, frame: &VideoFrame) -> bool {
        let same = self.uploaded.len() == frame.planes.len()
            && self
                .uploaded
                .iter()
                .zip(&frame.planes)
                .all(|((data, stride), plane)| {
                    *stride == plane.stride && Arc::ptr_eq(data, &plane.data)
                });
        if !same {
            self.uploaded = frame
                .planes
                .iter()
                .map(|p| (p.data.clone(), p.stride))
                .collect();
        }
        !same
    }
}

/// Everything that only depends on the output size.
#[derive(Default)]
struct Textures {
    /// The scaled screen.
    canvas: Option<Tex>,
    /// The first axis of the Lanczos scale.
    mid: Option<Tex>,
    /// The composited frame: what the preview and the readback use.
    frame: Option<Tex>,
    y: Option<Tex>,
    uv: Option<Tex>,
    u: Option<Tex>,
    v: Option<Tex>,
    /// A 1x1 stand-in for bindings a pass does not use.
    dummy: Option<Tex>,
    screen: Input,
    cam: Input,
    /// The cached overlay layers.
    decor: Option<(DecorKey, Tex)>,
}

/// A submitted frame whose readback is still on the GPU.
struct InFlight {
    slot: usize,
    pts: f64,
    index: wgpu::SubmissionIndex,
}

/// Where a texture comes from in a pass.
struct Source {
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

/// The overlay's layers placed in the canvas, for the composite pass.
struct DecorFrame {
    view: wgpu::TextureView,
    /// The decor's top-left in canvas pixels (the overlay's, less the shadow
    /// padding).
    origin: [f32; 2],
    size: [f32; 2],
    pad: f32,
}

/// The GPU compositor for one output size.
pub struct Renderer {
    gpu: Gpu,
    width: u32,
    height: u32,
    scene: Scene,
    pip: PipSpec,
    output: OutputFormat,
    corner_fill: bool,
    depth: usize,
    cam_aspect: f64,
    /// Size of the newest camera frame, for the composite pass.
    cam_size: (u32, u32),
    passes: uniforms::Passes,
    tex: Textures,
    slots: Vec<(wgpu::Buffer, Layout)>,
    in_flight: VecDeque<InFlight>,
    next_slot: usize,
    frames: u64,
    /// Bumped whenever the composited texture is rebuilt, so a preview can cache
    /// its bind group (see [`PreviewFrame`]).
    preview_generation: u64,
}

impl Renderer {
    /// A renderer for `width`x`height` output with this overlay, on a device of
    /// its own.
    pub fn new(width: usize, height: usize, pip: PipSpec) -> Result<Self> {
        Self::with_options(&Options {
            width,
            height,
            pip,
            ..Default::default()
        })
    }

    /// A renderer on a device of its own, headless.
    pub fn with_options(opts: &Options) -> Result<Self> {
        Self::build(Gpu::headless()?, opts)
    }

    /// A renderer on a device the caller already has (egui's, in the app).
    ///
    /// `info` is only for the status line; pass what the app's adapter
    /// reported.
    pub fn with_device(
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        info: wgpu::AdapterInfo,
        opts: &Options,
    ) -> Result<Self> {
        Self::build(Gpu::shared(device, queue, info), opts)
    }

    fn build(gpu: Gpu, opts: &Options) -> Result<Self> {
        if opts.width < 2 || opts.height < 2 {
            bail!("output size {}x{} is too small", opts.width, opts.height);
        }
        let depth = opts.pipeline_depth.clamp(1, 4);
        let passes = uniforms::Passes::new(&gpu.device)?;
        let layout = Layout::new(opts.width, opts.height, opts.output.pixel_format());
        let slots = (0..depth)
            .map(|_| (new_readback(&gpu.device, &layout), layout.clone()))
            .collect();
        Ok(Renderer {
            width: opts.width as u32,
            height: opts.height as u32,
            scene: opts.scene,
            pip: opts.pip.clone(),
            output: opts.output,
            corner_fill: opts.corner_fill,
            depth,
            cam_aspect: 16.0 / 9.0,
            cam_size: (0, 0),
            passes,
            tex: Textures::default(),
            slots,
            in_flight: VecDeque::new(),
            next_slot: 0,
            frames: 0,
            preview_generation: 0,
            gpu,
        })
    }

    /// Switch scenes live; applies from the next frame.
    pub fn set_scene(&mut self, scene: Scene) {
        self.scene = scene;
    }

    pub fn scene(&self) -> Scene {
        self.scene
    }

    /// Change the overlay live (position, shape, size, style); applies from the
    /// next frame.
    pub fn set_pip(&mut self, pip: PipSpec) {
        self.pip = pip;
    }

    pub fn pip(&self) -> &PipSpec {
        &self.pip
    }

    /// Switch the output pixel format; applies from the next frame.
    pub fn set_output(&mut self, output: OutputFormat) {
        if output != self.output {
            self.output = output;
            self.reset_slots();
        }
    }

    pub fn output(&self) -> OutputFormat {
        self.output
    }

    /// Whether the screen is a single window whose transparent corners are
    /// filled with its edge colour.
    pub fn set_corner_fill(&mut self, on: bool) {
        self.corner_fill = on;
    }

    pub fn corner_fill(&self) -> bool {
        self.corner_fill
    }

    /// Aspect ratio of the last camera frame (16:9 until one arrives).
    pub fn cam_aspect(&self) -> f64 {
        self.cam_aspect
    }

    pub fn size(&self) -> (usize, usize) {
        (self.width as usize, self.height as usize)
    }

    pub fn options(&self) -> Options {
        Options {
            width: self.width as usize,
            height: self.height as usize,
            scene: self.scene,
            pip: self.pip.clone(),
            output: self.output,
            corner_fill: self.corner_fill,
            pipeline_depth: self.depth,
        }
    }

    /// The device the compositor draws on, for the app to share.
    pub fn device(&self) -> &Arc<wgpu::Device> {
        &self.gpu.device
    }

    pub fn queue(&self) -> &Arc<wgpu::Queue> {
        &self.gpu.queue
    }

    /// What the adapter calls itself.
    pub fn describe_adapter(&self) -> String {
        self.gpu.describe()
    }

    pub fn adapter_info(&self) -> &wgpu::AdapterInfo {
        &self.gpu.info
    }

    /// True when the compositor runs on a CPU rasteriser.
    pub fn is_software(&self) -> bool {
        self.gpu.is_software()
    }

    /// True when the renderer draws on the device it was handed
    /// ([`Renderer::with_device`]), so its [`PreviewFrame`]s can be drawn in
    /// the caller's own render passes. False when it made its own device: a
    /// texture from another device cannot be used there.
    pub fn shares_app_device(&self) -> bool {
        self.gpu.is_shared()
    }

    /// The last composited frame as a texture, for the live preview: BGRA8,
    /// [`size`](Self::size) big, valid until the next render. Draw it 1:1 (it is
    /// already gamma encoded, so a non-sRGB view is right).
    pub fn texture(&self) -> Option<&wgpu::Texture> {
        self.tex.frame.as_ref().map(|t| &t.texture)
    }

    pub fn texture_format(&self) -> wgpu::TextureFormat {
        FMT_OUT
    }

    /// The last composited frame as a [`PreviewFrame`] (a cloned handle, no
    /// copy), stamped with `pts`.
    ///
    /// `None` before the first frame. The texture is the output size,
    /// `texture_format()`, and is reused: draw it before the next frame is
    /// composited.
    pub fn preview_frame(&self, pts: f64) -> Option<PreviewFrame> {
        let frame = self.tex.frame.as_ref()?;
        Some(PreviewFrame {
            texture: frame.texture.clone(),
            width: frame.width,
            height: frame.height,
            pts,
            generation: self.preview_generation,
        })
    }

    /// Frames submitted and not yet read back: 0 when the pipeline is empty.
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    pub fn frames_rendered(&self) -> u64 {
        self.frames
    }

    /// Composite `screen` + `cam` for `pts` and return the frame the previous
    /// call submitted (see the module docs on the pipelined readback). Returns
    /// `None` until the pipeline is full.
    pub fn render(
        &mut self,
        screen: Option<&VideoFrame>,
        cam: Option<&VideoFrame>,
        pts: f64,
    ) -> Result<Option<VideoFrame>> {
        self.submit(screen, cam, pts)?;
        if self.in_flight.len() >= self.depth {
            let oldest = self.in_flight.pop_front().expect("checked");
            return Ok(Some(self.collect(oldest)?));
        }
        Ok(None)
    }

    /// Composite one frame and wait for it: the synchronous path for tests and
    /// the preview. Call it only when nothing else is in flight, or the frames
    /// in flight come back with it (in order).
    pub fn render_now(
        &mut self,
        screen: Option<&VideoFrame>,
        cam: Option<&VideoFrame>,
        pts: f64,
    ) -> Result<VideoFrame> {
        self.submit(screen, cam, pts)?;
        self.flush()?
            .pop()
            .context("the frame just submitted was not read back")
    }

    /// Read back every frame still in flight, oldest first. Call it once when a
    /// recording stops, or the last frame stays on the GPU.
    pub fn flush(&mut self) -> Result<Vec<VideoFrame>> {
        let mut out = Vec::with_capacity(self.in_flight.len());
        while let Some(f) = self.in_flight.pop_front() {
            out.push(self.collect(f)?);
        }
        Ok(out)
    }

    /// Drop the frames in flight without reading them back, and rebuild the
    /// staging buffers for the current format.
    fn reset_slots(&mut self) {
        self.in_flight.clear();
        let layout = Layout::new(
            self.width as usize,
            self.height as usize,
            self.output.pixel_format(),
        );
        self.slots = (0..self.depth)
            .map(|_| (new_readback(&self.gpu.device, &layout), layout.clone()))
            .collect();
        self.next_slot = 0;
    }

    // -- per frame ---------------------------------------------------------

    fn submit(
        &mut self,
        screen: Option<&VideoFrame>,
        cam: Option<&VideoFrame>,
        pts: f64,
    ) -> Result<()> {
        for (name, frame) in [("screen", screen), ("camera", cam)] {
            if let Some(f) = frame
                && !f.is_well_formed()
            {
                bail!(
                    "the {name} frame is not well formed ({}x{}, {})",
                    f.width,
                    f.height,
                    f.format.label()
                );
            }
        }
        // Live changes: the aspect ratio of the newest camera frame decides the
        // overlay's size from this frame on.
        if let Some(c) = cam.filter(|c| c.width > 0 && c.height > 0) {
            self.cam_aspect = c.width as f64 / c.height as f64;
            self.cam_size = (c.width as u32, c.height as u32);
        }
        let device = self.gpu.device.clone();
        let queue = self.gpu.queue.clone();
        let (w, h) = (self.width, self.height);
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("compositor"),
        });
        self.ensure_outputs(&device);

        // 1. The screen: upload, corner fill, Lanczos to the output size.
        let mut scaled: Option<Source> = None;
        if self.scene != Scene::Cam
            && let Some(frame) = screen
        {
            let src = self.upload(&device, &queue, &mut enc, Role::Screen, frame)?;
            scaled = Some(self.scale(&device, &queue, &mut enc, src)?);
        }

        // 2. The camera, when this scene shows it.
        let cam_src = match cam.filter(|_| self.scene.uses_cam()) {
            Some(frame) => Some(self.upload(&device, &queue, &mut enc, Role::Cam, frame)?),
            None => None,
        };

        // 3. Composite into the output texture.
        let dummy = self.tex.dummy.as_ref().expect("ensured").view.clone();
        let canvas = scaled
            .as_ref()
            .map_or_else(|| dummy.clone(), |s| s.view.clone());
        let cam_view = cam_src
            .as_ref()
            .map_or_else(|| dummy.clone(), |s| s.view.clone());
        let decor = self.decor_for(&device, &queue);
        let frame_view = self.tex.frame.as_ref().expect("ensured").view.clone();
        self.composite(
            &device,
            &queue,
            &mut enc,
            &canvas,
            &cam_view,
            scaled.is_some(),
            cam_src.is_some(),
            decor,
            &frame_view,
        );

        // 4. Output: BT.709 NV12/I420, or BGRA straight from the texture.
        let slot = self.next_slot % self.slots.len();
        self.next_slot = self.next_slot.wrapping_add(1);
        let (buffer, layout) = &self.slots[slot];
        match self.output {
            OutputFormat::Bgra => {
                let plane = &layout.planes[0];
                enc.copy_texture_to_buffer(
                    edit_source(&self.tex.frame.as_ref().expect("ensured").texture),
                    wgpu::TexelCopyBufferInfo {
                        buffer,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(plane.pitch as u32),
                            rows_per_image: Some(h),
                        },
                    },
                    wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                );
            }
            OutputFormat::Nv12 | OutputFormat::I420 => {
                self.yuv(&device, &queue, &mut enc);
                for (i, plane) in layout.planes.iter().enumerate() {
                    let tex = match (self.output, i) {
                        (_, 0) => self.tex.y.as_ref(),
                        (OutputFormat::Nv12, 1) => self.tex.uv.as_ref(),
                        (OutputFormat::I420, 1) => self.tex.u.as_ref(),
                        (_, 2) => self.tex.v.as_ref(),
                        _ => None,
                    }
                    .context("a YUV plane texture is missing")?;
                    enc.copy_texture_to_buffer(
                        edit_source(&tex.texture),
                        wgpu::TexelCopyBufferInfo {
                            buffer,
                            layout: wgpu::TexelCopyBufferLayout {
                                offset: plane.offset as u64,
                                bytes_per_row: Some(plane.pitch as u32),
                                rows_per_image: Some(plane.height as u32),
                            },
                        },
                        wgpu::Extent3d {
                            width: plane.width as u32,
                            height: plane.height as u32,
                            depth_or_array_layers: 1,
                        },
                    );
                }
            }
        }

        let index = queue.submit(Some(enc.finish()));
        self.in_flight.push_back(InFlight { slot, pts, index });
        self.frames += 1;
        Ok(())
    }

    fn collect(&mut self, frame: InFlight) -> Result<VideoFrame> {
        let (buffer, layout) = &self.slots[frame.slot];
        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.gpu
            .device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(frame.index),
                timeout: None,
            })
            .map_err(|e| anyhow::anyhow!("waiting for the GPU failed: {e}"))?;
        rx.recv()
            .context("the readback callback never ran")?
            .map_err(|e| anyhow::anyhow!("the readback buffer could not be mapped: {e}"))?;
        let data = slice
            .get_mapped_range()
            .map_err(|e| anyhow::anyhow!("the readback buffer could not be read: {e}"))?;
        let out = layout.frame_from(&data, frame.pts);
        drop(data);
        buffer.unmap();
        Ok(out)
    }

    // -- passes ------------------------------------------------------------

    /// Upload a captured frame and, when the working format needs one, convert
    /// it to gamma-encoded RGBA8. The corner fill happens here, on the source's
    /// own pixels, the way upstream fills the captured buffer before the
    /// compositor sees it.
    fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        enc: &mut wgpu::CommandEncoder,
        role: Role,
        frame: &VideoFrame,
    ) -> Result<Source> {
        let (w, h) = (frame.width as u32, frame.height as u32);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let kind = frame.format;
        let input = match role {
            Role::Screen => &mut self.tex.screen,
            Role::Cam => &mut self.tex.cam,
        };
        let prefix = match role {
            Role::Screen => "screen",
            Role::Cam => "camera",
        };
        if input.key != Some((w, h, kind)) {
            input.key = Some((w, h, kind));
            input.rgba = None;
            input.uploaded.clear();
            let upload = Usage::TEXTURE_BINDING | Usage::COPY_DST;
            input.planes = match kind {
                PixelFormat::Bgra => vec![Tex::new(
                    device,
                    prefix,
                    w,
                    h,
                    wgpu::TextureFormat::Bgra8Unorm,
                    upload,
                )],
                PixelFormat::Rgba => vec![Tex::new(
                    device,
                    prefix,
                    w,
                    h,
                    wgpu::TextureFormat::Rgba8Unorm,
                    upload,
                )],
                PixelFormat::Nv12 => vec![
                    Tex::new(device, prefix, w, h, FMT_Y, upload),
                    Tex::new(device, prefix, cw, ch, FMT_UV, upload),
                ],
                PixelFormat::I420 => vec![
                    Tex::new(device, prefix, w, h, FMT_Y, upload),
                    Tex::new(device, prefix, cw, ch, FMT_Y, upload),
                    Tex::new(device, prefix, cw, ch, FMT_Y, upload),
                ],
            };
        }
        let upload = match role {
            Role::Screen => true,
            Role::Cam => input.camera_needs_upload(frame),
        };
        for (i, plane) in frame.planes.iter().enumerate().filter(|_| upload) {
            let tex = &input.planes[i];
            let rows = frame.format.plane_rows(i, frame.height);
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &tex.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &plane.data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(plane.stride as u32),
                    rows_per_image: Some(rows as u32),
                },
                wgpu::Extent3d {
                    width: tex.width,
                    height: tex.height,
                    depth_or_array_layers: 1,
                },
            );
        }
        // A packed frame with no corner fill is already the working format.
        let packed = matches!(kind, PixelFormat::Bgra | PixelFormat::Rgba);
        if packed && !self.corner_fill {
            return Ok(Source {
                view: input.planes[0].view.clone(),
                width: w,
                height: h,
            });
        }
        let rgba = Tex::ensure(
            &mut input.rgba,
            device,
            "input rgba",
            w,
            h,
            FMT_RGBA,
            Usage::TEXTURE_BINDING | Usage::STORAGE_BINDING,
        );
        let dst = rgba.view.clone();
        let luma = input.planes[0].view.clone();
        let dummy = Tex::ensure(
            &mut self.tex.dummy,
            device,
            "dummy",
            1,
            1,
            FMT_Y,
            Usage::TEXTURE_BINDING,
        )
        .view
        .clone();
        let cb = input
            .planes
            .get(1)
            .map_or_else(|| dummy.clone(), |t| t.view.clone());
        let cr = input
            .planes
            .get(2)
            .map_or_else(|| dummy.clone(), |t| t.view.clone());
        let pipeline = match kind {
            PixelFormat::Bgra | PixelFormat::Rgba => &self.passes.convert_packed,
            PixelFormat::Nv12 => &self.passes.convert_nv12,
            PixelFormat::I420 => &self.passes.convert_i420,
        };
        // Upstream's `CornerFill` only handles a BGRA buffer (`fill` returns
        // None for anything else): a YUV window frame has no alpha to find the
        // transparent corners with, and clamping a band of it would smear real
        // window content. So the fill applies to packed frames only.
        let inset = if self.corner_fill && packed {
            compose::corner_inset(w as f64, h as f64) as f32
        } else {
            0.0
        };
        let slot = match role {
            Role::Screen => uniforms::Slot::ConvertScreen,
            Role::Cam => uniforms::Slot::ConvertCam,
        };
        let mut uni = Uni::new();
        uni.vec4(&[w as f32, h as f32, inset, 0.0]);
        uni.vec4(&[cw as f32, ch as f32, 0.0, 0.0]);
        self.passes.write(queue, slot, &uni);
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("convert"),
            layout: &self.passes.convert_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&luma),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&cb),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&cr),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: self.passes.uniform(slot),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&dst),
                },
            ],
        });
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("convert"),
            timestamp_writes: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &bind, &[]);
        let (gx, gy) = groups(w, h);
        pass.dispatch_workgroups(gx, gy, 1);
        drop(pass);
        Ok(Source {
            view: dst,
            width: w,
            height: h,
        })
    }

    /// Scale the screen to the output size: one axis at a time, skipping an
    /// axis that does not change.
    fn scale(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        enc: &mut wgpu::CommandEncoder,
        src: Source,
    ) -> Result<Source> {
        let (w, h) = (self.width, self.height);
        let (sx, sy, tx, ty) =
            compose::fill_no_upscale(src.width as f64, src.height as f64, w as f64, h as f64);
        let mut cur = src;
        if cur.width != w {
            let mid = Tex::ensure(
                &mut self.tex.mid,
                device,
                "lanczos intermediate",
                w,
                cur.height,
                FMT_WORK,
                Usage::TEXTURE_BINDING | Usage::STORAGE_BINDING,
            );
            let dst = mid.view.clone();
            let view = cur.view.clone();
            self.lanczos(
                device,
                queue,
                enc,
                &view,
                (cur.width, cur.height),
                (w, cur.height),
                sx,
                tx,
                0.0,
                uniforms::Slot::LanczosH,
                &dst,
            );
            cur = Source {
                view: dst,
                width: w,
                height: cur.height,
            };
        }
        if cur.height != h {
            let canvas = Tex::ensure(
                &mut self.tex.canvas,
                device,
                "canvas",
                w,
                h,
                FMT_WORK,
                Usage::TEXTURE_BINDING | Usage::STORAGE_BINDING,
            );
            let dst = canvas.view.clone();
            let view = cur.view.clone();
            self.lanczos(
                device,
                queue,
                enc,
                &view,
                (cur.width, cur.height),
                (w, h),
                sy,
                ty,
                1.0,
                uniforms::Slot::LanczosV,
                &dst,
            );
            cur = Source {
                view: dst,
                width: w,
                height: h,
            };
        }
        Ok(cur)
    }

    /// One axis of the separable Lanczos scale.
    #[allow(clippy::too_many_arguments)]
    fn lanczos(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        enc: &mut wgpu::CommandEncoder,
        src: &wgpu::TextureView,
        (sw, sh): (u32, u32),
        (dw, dh): (u32, u32),
        scale: f64,
        offset: f64,
        axis: f32,
        slot: uniforms::Slot,
        dst: &wgpu::TextureView,
    ) {
        let extent = if axis == 0.0 {
            sw as f64 * scale
        } else {
            sh as f64 * scale
        };
        let mut uni = Uni::new();
        uni.vec4(&[sw as f32, sh as f32, dw as f32, dh as f32]);
        uni.vec4(&[scale as f32, offset as f32, extent as f32, axis]);
        self.passes.write(queue, slot, &uni);
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("lanczos"),
            layout: &self.passes.lanczos_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(src),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: self.passes.uniform(slot),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(dst),
                },
            ],
        });
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("lanczos"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&self.passes.lanczos);
        pass.set_bind_group(0, &bind, &[]);
        let (gx, gy) = groups(dw, dh);
        pass.dispatch_workgroups(gx, gy, 1);
    }

    /// The screen (or nothing) plus the overlay, into the output texture.
    #[allow(clippy::too_many_arguments)]
    fn composite(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        enc: &mut wgpu::CommandEncoder,
        canvas: &wgpu::TextureView,
        cam: &wgpu::TextureView,
        has_screen: bool,
        has_cam: bool,
        decor: Option<DecorFrame>,
        dst: &wgpu::TextureView,
    ) {
        let (w, h) = (self.width, self.height);
        let (cw, ch) = self.cam_size;
        // The overlay rect and the camera's cover fit into it, or the whole
        // frame in the camera scene.
        let overlay = (self.scene == Scene::ScreenCam && has_cam && decor.is_some()).then(|| {
            let [x, y, ow, oh] =
                compose::overlay_rect(&self.pip, w as f64, h as f64, self.cam_aspect);
            let (s, ox, oy) = compose::fit(cw as f64, ch as f64, ow, oh, true);
            (
                [x as f32, y as f32, ow as f32, oh as f32],
                [s as f32, ox as f32, oy as f32],
            )
        });
        let (rect, fit) = overlay.unwrap_or_else(|| {
            let (s, ox, oy) = compose::fit(cw as f64, ch as f64, w as f64, h as f64, true);
            (
                [0.0, 0.0, w as f32, h as f32],
                [s as f32, ox as f32, oy as f32],
            )
        });
        let mode = if self.scene == Scene::Cam && has_cam {
            2.0
        } else if overlay.is_some() {
            1.0
        } else {
            0.0
        };
        let (origin, size, pad) = match &decor {
            Some(d) => (d.origin, d.size, d.pad),
            None => ([0.0, 0.0], [1.0, 1.0], 0.0),
        };
        let colour = self.pip.border_color;
        let mut uni = Uni::new();
        uni.vec4(&[w as f32, h as f32, cw as f32, ch as f32]);
        uni.vec4(&rect);
        uni.vec4(&[fit[0], fit[1], fit[2], pad]);
        uni.vec4(&[origin[0], origin[1], size[0], size[1]]);
        uni.vec4(&[
            colour[0] as f32 / 255.0,
            colour[1] as f32 / 255.0,
            colour[2] as f32 / 255.0,
            mode,
        ]);
        uni.vec4(&[
            if self.pip.mirror { 1.0 } else { 0.0 },
            if has_screen { 1.0 } else { 0.0 },
            0.0,
            0.0,
        ]);
        self.passes.write(queue, uniforms::Slot::Composite, &uni);
        let decor_view = match &decor {
            Some(d) => d.view.clone(),
            None => self.tex.dummy.as_ref().expect("ensured").view.clone(),
        };
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("composite"),
            layout: &self.passes.composite_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(canvas),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(cam),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.passes.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&decor_view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: self.passes.uniform(uniforms::Slot::Composite),
                },
            ],
        });
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("composite"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: dst,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.passes.composite);
        pass.set_bind_group(0, &bind, &[]);
        pass.draw(0..3, 0..1);
    }

    /// The composited frame to BT.709 limited range, NV12 or I420: a luma pass
    /// at full resolution and a chroma pass at half resolution.
    fn yuv(&self, device: &wgpu::Device, queue: &wgpu::Queue, enc: &mut wgpu::CommandEncoder) {
        let (w, h) = (self.width, self.height);
        let src = self.tex.frame.as_ref().expect("ensured").view.clone();
        let mut uni = Uni::new();
        uni.vec4(&[w as f32, h as f32, 0.0, 0.0]);
        self.passes.write(queue, uniforms::Slot::Yuv, &uni);
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("yuv"),
            layout: &self.passes.yuv_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&src),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: self.passes.uniform(uniforms::Slot::Yuv),
                },
            ],
        });
        let y = self.tex.y.as_ref().expect("ensured").view.clone();
        // Luma: one fragment per output pixel.
        self.fullscreen(
            enc,
            "yuv luma",
            &self.passes.yuv_y,
            &bind,
            &[Some(wgpu::RenderPassColorAttachment {
                view: &y,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
        );
        // Chroma: one fragment per 2x2 block.
        match self.output {
            OutputFormat::I420 => {
                let u = self.tex.u.as_ref().expect("ensured").view.clone();
                let v = self.tex.v.as_ref().expect("ensured").view.clone();
                self.fullscreen(
                    enc,
                    "yuv chroma i420",
                    &self.passes.yuv_i420,
                    &bind,
                    &[
                        Some(wgpu::RenderPassColorAttachment {
                            view: &u,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                                store: wgpu::StoreOp::Store,
                            },
                        }),
                        Some(wgpu::RenderPassColorAttachment {
                            view: &v,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                                store: wgpu::StoreOp::Store,
                            },
                        }),
                    ],
                );
            }
            _ => {
                let uv = self.tex.uv.as_ref().expect("ensured").view.clone();
                self.fullscreen(
                    enc,
                    "yuv chroma nv12",
                    &self.passes.yuv_nv12,
                    &bind,
                    &[Some(wgpu::RenderPassColorAttachment {
                        view: &uv,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                );
            }
        }
    }

    /// A fullscreen-triangle pass over `size`.
    #[allow(clippy::too_many_arguments)]
    fn fullscreen(
        &self,
        enc: &mut wgpu::CommandEncoder,
        label: &str,
        pipeline: &wgpu::RenderPipeline,
        bind: &wgpu::BindGroup,
        attachments: &[Option<wgpu::RenderPassColorAttachment<'_>>],
    ) {
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some(label),
            color_attachments: attachments,
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, bind, &[]);
        pass.draw(0..3, 0..1);
    }

    /// Build (or reuse) the output-sized textures.
    fn ensure_outputs(&mut self, device: &wgpu::Device) {
        let before = self
            .tex
            .frame
            .as_ref()
            .map(|t| (t.width, t.height, t.format));
        let (w, h) = (self.width, self.height);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        Tex::ensure(
            &mut self.tex.frame,
            device,
            "composited frame",
            w,
            h,
            FMT_OUT,
            Usage::RENDER_ATTACHMENT | Usage::TEXTURE_BINDING | Usage::COPY_SRC,
        );
        Tex::ensure(
            &mut self.tex.dummy,
            device,
            "dummy",
            1,
            1,
            FMT_Y,
            Usage::TEXTURE_BINDING,
        );
        match self.output {
            OutputFormat::Nv12 => {
                Tex::ensure(
                    &mut self.tex.y,
                    device,
                    "y",
                    w,
                    h,
                    FMT_Y,
                    Usage::RENDER_ATTACHMENT | Usage::COPY_SRC,
                );
                Tex::ensure(
                    &mut self.tex.uv,
                    device,
                    "uv",
                    cw,
                    ch,
                    FMT_UV,
                    Usage::RENDER_ATTACHMENT | Usage::COPY_SRC,
                );
            }
            OutputFormat::I420 => {
                for (slot, label) in [
                    (&mut self.tex.y, "y"),
                    (&mut self.tex.u, "u"),
                    (&mut self.tex.v, "v"),
                ] {
                    let (tw, th) = if label == "y" { (w, h) } else { (cw, ch) };
                    Tex::ensure(
                        slot,
                        device,
                        label,
                        tw,
                        th,
                        FMT_Y,
                        Usage::RENDER_ATTACHMENT | Usage::COPY_SRC,
                    );
                }
            }
            OutputFormat::Bgra => {}
        }
        if self
            .tex
            .frame
            .as_ref()
            .map(|t| (t.width, t.height, t.format))
            != before
        {
            // A different composited texture: a preview's cached bind group is
            // stale (see `PreviewFrame::generation`).
            self.preview_generation += 1;
        }
    }

    /// The overlay's cached layers, rebuilt only when the size or the style
    /// changed.
    fn decor_for(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> Option<DecorFrame> {
        if self.scene != Scene::ScreenCam || self.cam_size.0 == 0 {
            return None;
        }
        let (w, h) = (self.width as f64, self.height as f64);
        let [x, y, ow, oh] = compose::overlay_rect(&self.pip, w, h, self.cam_aspect);
        let key = DecorKey::new(&self.pip, ow, oh, h);
        if self.tex.decor.as_ref().is_none_or(|(k, _)| *k != key) {
            let built = Decor::build(&self.pip, ow, oh, h);
            let tex = Tex::new(
                device,
                "overlay decor",
                built.width as u32,
                built.height as u32,
                FMT_RGBA,
                Usage::TEXTURE_BINDING | Usage::COPY_DST,
            );
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &tex.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &built.pixels,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(built.width as u32 * 4),
                    rows_per_image: Some(built.height as u32),
                },
                wgpu::Extent3d {
                    width: built.width as u32,
                    height: built.height as u32,
                    depth_or_array_layers: 1,
                },
            );
            self.tex.decor = Some((key, tex));
        }
        let (_, tex) = self.tex.decor.as_ref()?;
        let pad = key.pad as f32;
        Some(DecorFrame {
            view: tex.view.clone(),
            origin: [x as f32 - pad, y as f32 - pad],
            size: [tex.width as f32, tex.height as f32],
            pad,
        })
    }
}

/// The upload / convert source of a frame.
#[derive(Clone, Copy)]
enum Role {
    Screen,
    Cam,
}

fn new_readback(device: &wgpu::Device, layout: &Layout) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("compositor readback"),
        size: layout.len as u64,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn edit_source(texture: &wgpu::Texture) -> wgpu::TexelCopyTextureInfo<'_> {
    wgpu::TexelCopyTextureInfo {
        texture,
        mip_level: 0,
        origin: wgpu::Origin3d::ZERO,
        aspect: wgpu::TextureAspect::All,
    }
}

/// Workgroups for a `w`x`h` dispatch at 8x8.
fn groups(w: u32, h: u32) -> (u32, u32) {
    (w.div_ceil(8).max(1), h.div_ceil(8).max(1))
}
