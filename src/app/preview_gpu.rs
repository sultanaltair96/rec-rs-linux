//! The preview on the GPU (G10): the compositor's frame is drawn inside egui's
//! render pass, by [`preview.wgsl`](shaders/preview.wgsl).
//!
//! Two sources, in order of preference:
//!
//! - **The compositor's texture** ([`PreviewCallback::texture`]): when the
//!   recording composites on this window's device (`gpu::SharedGpu`), the writer
//!   hands the composited frame over as a `wgpu::Texture` and the preview draws
//!   it 1:1 with `fs_texture`. Nothing is read back and nothing is uploaded —
//!   the whole per-frame cost is one bind group and one draw.
//! - **A CPU frame** ([`PreviewCallback::new`]): the frame's planes are
//!   uploaded (BGRA/RGBA, NV12 or I420) and converted and scaled by
//!   `fs_bgra`/`fs_rgba`/`fs_nv12`/`fs_i420`. This is the fallback for a
//!   compositor on its own device (the CLI, or a device that could not be
//!   shared) and for the no-camera path, which never had a GPU frame.
//!
//! Upstream wraps the frame's IOSurface as a Metal texture and converts it with
//! a VideoToolbox transfer; that is 0.25–0.39 ms per frame. The port used to
//! convert and resample every frame on the CPU (6.6 ms at 1080p, 8.2 ms at 4K)
//! — see the audit's G10. Now the CPU cost is one copy of the frame's planes
//! into the queue (`upload`), measured by `preview_cost`.
//!
//! Textures and pipelines live in egui-wgpu's callback resources, created once
//! per window.

use egui_wgpu::{CallbackResources, CallbackTrait, ScreenDescriptor};
use std::sync::Arc;
use wgpu::util::StagingBelt;

use crate::media_types::{PixelFormat, VideoFrame};

/// One pipeline per input format (the fragment entry points in the shader).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Bgra,
    Rgba,
    Nv12,
    I420,
}

impl Mode {
    fn of(format: PixelFormat) -> Option<Mode> {
        match format {
            PixelFormat::Bgra => Some(Mode::Bgra),
            PixelFormat::Rgba => Some(Mode::Rgba),
            PixelFormat::Nv12 => Some(Mode::Nv12),
            PixelFormat::I420 => Some(Mode::I420),
        }
    }

    fn index(self) -> usize {
        match self {
            Mode::Bgra => 0,
            Mode::Rgba => 1,
            Mode::Nv12 => 2,
            Mode::I420 => 3,
        }
    }

    fn entry(self) -> &'static str {
        match self {
            Mode::Bgra => "fs_bgra",
            Mode::Rgba => "fs_rgba",
            Mode::Nv12 => "fs_nv12",
            Mode::I420 => "fs_i420",
        }
    }
}

/// The plane textures for one frame size and format, plus the bind group that
/// reads them.
struct Uploaded {
    key: (usize, usize, PixelFormat),
    mode: Mode,
    /// One per plane; kept alive with the bind group.
    textures: Vec<wgpu::Texture>,
    bind: wgpu::BindGroup,
}

/// Everything a window's preview needs: pipelines, sampler and the last
/// upload. Inserted into the egui-wgpu renderer's callback resources once.
pub struct PreviewPainter {
    pipelines: Vec<wgpu::RenderPipeline>,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// A 1×1 texture bound to the planes a format does not use.
    dummy: wgpu::TextureView,
    uploaded: Option<Uploaded>,
    /// Mapped staging buffers for the plane copies (one copy, into memory the
    /// GPU reads directly; no per-frame allocation).
    belt: StagingBelt,
    /// The composited texture drawn directly (G10): one pipeline, and the last
    /// bind group built for it (rebuilt only when the texture changes).
    texture_pipeline: wgpu::RenderPipeline,
    texture_layout: wgpu::BindGroupLayout,
    /// The bind group for the composited texture, keyed by the frame's
    /// generation (the compositor reuses one texture per recording).
    texture_bound: Option<(u64, wgpu::BindGroup)>,
}

impl PreviewPainter {
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> PreviewPainter {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("preview"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/preview.wgsl").into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("preview planes"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                texture_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("preview"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipelines = [Mode::Bgra, Mode::Rgba, Mode::Nv12, Mode::I420]
            .into_iter()
            .map(|mode| {
                device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(mode.entry()),
                    layout: Some(&pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: Some("vs"),
                        compilation_options: Default::default(),
                        buffers: &[],
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: Some(mode.entry()),
                        compilation_options: Default::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: target_format,
                            blend: Some(wgpu::BlendState::REPLACE),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                    }),
                    primitive: wgpu::PrimitiveState::default(),
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    multiview_mask: None,
                    cache: None,
                })
            })
            .collect();
        // The composited frame as it is: one texture, sampled 1:1. Its values
        // are already gamma encoded BGRA (the compositor's output format), so
        // no conversion and no sRGB view.
        let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("preview texture"),
            entries: &[
                texture_entry(0),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let texture_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("preview texture"),
                bind_group_layouts: &[Some(&texture_layout)],
                immediate_size: 0,
            });
        let texture_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("fs_texture"),
            layout: Some(&texture_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_texture"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("preview"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        let dummy = device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("preview dummy plane"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
            .create_view(&Default::default());
        PreviewPainter {
            pipelines,
            layout,
            sampler,
            dummy,
            uploaded: None,
            texture_pipeline,
            texture_layout,
            texture_bound: None,
            // A 4K NV12 frame fits in one chunk; the belt grows if not.
            belt: StagingBelt::new(device.clone(), 16 << 20),
        }
    }

    /// Upload `frame`'s planes, replacing the previous frame, and encode the
    /// copies into `encoder` (egui's, so they are submitted with this pass).
    /// Returns false for a format the shader cannot read (nothing is drawn).
    ///
    /// One CPU copy per plane, straight into mapped staging memory; the GPU
    /// does the format conversion and the scaling.
    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        frame: &VideoFrame,
    ) -> bool {
        let Some(mode) = Mode::of(frame.format) else {
            return false;
        };
        if !frame.is_well_formed() || frame.width == 0 || frame.height == 0 {
            return false;
        }
        let key = (frame.width, frame.height, frame.format);
        // Textures only change when the size or format does: a resize or a
        // scene change, not per frame.
        if self.uploaded.as_ref().map(|u| (u.key, u.mode)) != Some((key, mode)) {
            self.uploaded = Some(self.allocate(device, frame, mode));
        }
        let (w, h) = (frame.width, frame.height);
        /// One plane's staging slice, ready to be copied into its texture.
        struct Copy {
            buffer: wgpu::Buffer,
            offset: u64,
            row: u32,
            rows: u32,
            width: u32,
            height: u32,
            plane: usize,
        }
        let mut copies: Vec<Copy> = Vec::with_capacity(3);
        for (i, plane) in frame.planes.iter().enumerate() {
            let (tw, th, bytes_per_pixel) = match (mode, i) {
                (_, 0) => (
                    w,
                    h,
                    if mode == Mode::Bgra || mode == Mode::Rgba {
                        4
                    } else {
                        1
                    },
                ),
                (Mode::Nv12, 1) => (w.div_ceil(2), h.div_ceil(2), 2),
                (Mode::I420, 1 | 2) => (w.div_ceil(2), h.div_ceil(2), 1),
                _ => continue,
            };
            let row = tw * bytes_per_pixel;
            // `copy_buffer_to_texture` wants 256-byte rows in the buffer.
            let aligned = row.next_multiple_of(256);
            let size = wgpu::BufferSize::new((aligned * th) as u64).expect("non-empty plane");
            let slice = self
                .belt
                .allocate(size, wgpu::BufferSize::new(256).unwrap());
            let (buffer, offset) = (slice.buffer().clone(), slice.offset());
            {
                let mut view = slice
                    .get_mapped_range_mut()
                    .expect("the staging belt's buffer is mapped");
                if plane.stride == aligned {
                    // Already laid out the way the copy wants (a 4K NV12 plane
                    // has 3840-byte rows, a multiple of 256): one memcpy.
                    let len = (row * th).min(plane.data.len());
                    view.slice(..len).copy_from_slice(&plane.data[..len]);
                } else {
                    // Pad each row to 256 bytes as the copy requires.
                    for y in 0..th {
                        let from = y * plane.stride;
                        let to = (from + row).min(plane.data.len());
                        view.slice(y * aligned..y * aligned + (to - from))
                            .copy_from_slice(&plane.data[from..to]);
                    }
                }
            }
            copies.push(Copy {
                buffer,
                offset,
                row: aligned as u32,
                rows: th as u32,
                width: tw as u32,
                height: th as u32,
                plane: i,
            });
        }
        let uploaded = self.uploaded.as_ref().expect("just allocated");
        for c in copies {
            encoder.copy_buffer_to_texture(
                wgpu::TexelCopyBufferInfo {
                    buffer: &c.buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: c.offset,
                        bytes_per_row: Some(c.row),
                        rows_per_image: Some(c.rows),
                    },
                },
                wgpu::TexelCopyTextureInfo {
                    texture: &uploaded.textures[c.plane],
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d {
                    width: c.width,
                    height: c.height,
                    depth_or_array_layers: 1,
                },
            );
        }
        self.belt.finish_and_recall_on_submit(encoder);
        true
    }

    /// The bind group for a composited texture, rebuilt only when the composer
    /// replaced it (a new output size or format, or a new renderer).
    fn texture_bind(&mut self, device: &wgpu::Device, frame: &crate::gpu::PreviewFrame) -> bool {
        let same = self
            .texture_bound
            .as_ref()
            .is_some_and(|(generation, _)| *generation == frame.generation);
        if !same {
            let view = frame.texture.create_view(&Default::default());
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("preview texture"),
                layout: &self.texture_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            self.texture_bound = Some((frame.generation, bind));
        }
        true
    }

    fn allocate(&self, device: &wgpu::Device, frame: &VideoFrame, mode: Mode) -> Uploaded {
        let (w, h) = (frame.width as u32, frame.height as u32);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        // One texture per plane of this format (the bind group and the copies
        // must use the same ones).
        let specs: Vec<(&str, u32, u32, wgpu::TextureFormat)> = match mode {
            Mode::Bgra | Mode::Rgba => {
                vec![("preview", w, h, wgpu::TextureFormat::Rgba8Unorm)]
            }
            Mode::Nv12 => vec![
                ("preview y", w, h, wgpu::TextureFormat::R8Unorm),
                ("preview uv", cw, ch, wgpu::TextureFormat::Rg8Unorm),
            ],
            Mode::I420 => vec![
                ("preview y", w, h, wgpu::TextureFormat::R8Unorm),
                ("preview u", cw, ch, wgpu::TextureFormat::R8Unorm),
                ("preview v", cw, ch, wgpu::TextureFormat::R8Unorm),
            ],
        };
        let textures: Vec<wgpu::Texture> = specs
            .iter()
            .map(|(label, width, height, format)| {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d {
                        width: *width,
                        height: *height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: *format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                })
            })
            .collect();
        let views: Vec<wgpu::TextureView> = textures
            .iter()
            .map(|t| t.create_view(&Default::default()))
            .collect();
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("preview planes"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&views[0]),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(
                        views.get(1).unwrap_or(&self.dummy),
                    ),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(
                        views.get(2).unwrap_or(&self.dummy),
                    ),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        Uploaded {
            key: (frame.width, frame.height, frame.format),
            mode,
            textures,
            bind,
        }
    }
}

/// What the texture path costs per preview frame, in milliseconds: the CPU side
/// (bind group lookup + encoding the pass) and the GPU side as a throughput
/// (`frames` passes submitted back to back, one wait at the end, divided by
/// `frames` — what the draw adds to egui's own pass, rather than a round trip).
///
/// The compositor's texture is the passed one, the preview rect `target`.
/// `frames` iterations after one warm-up. Only the ignored cost test calls it
/// (`cargo test --release app::preview::tests::preview_cost -- --ignored`).
#[cfg_attr(not(test), expect(dead_code))]
pub fn texture_path_cost(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    painter: &mut PreviewPainter,
    texture: &wgpu::Texture,
    target_format: wgpu::TextureFormat,
    target: (u32, u32),
    frames: usize,
) -> (f64, f64) {
    use std::time::Instant;
    let (tw, th) = target;
    let frame = crate::gpu::PreviewFrame {
        texture: texture.clone(),
        width: texture.width(),
        height: texture.height(),
        pts: 0.0,
        generation: 1,
    };
    let target_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("preview cost target"),
        size: wgpu::Extent3d {
            width: tw,
            height: th,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: target_format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = target_tex.create_view(&Default::default());
    let encode = |painter: &mut PreviewPainter, enc: &mut wgpu::CommandEncoder| {
        painter.texture_bind(device, &frame);
        let mut pass = enc
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("preview cost"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
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
            })
            .forget_lifetime();
        pass.set_pipeline(&painter.texture_pipeline);
        pass.set_bind_group(0, &painter.texture_bound.as_ref().expect("bound").1, &[]);
        pass.draw(0..3, 0..1);
    };
    // Warm up: the bind group and the pipeline.
    let mut warm = device.create_command_encoder(&Default::default());
    encode(painter, &mut warm);
    queue.submit([warm.finish()]);
    let _ = device.poll(wgpu::PollType::wait_indefinitely());

    let started = Instant::now();
    for _ in 0..frames {
        let mut enc = device.create_command_encoder(&Default::default());
        encode(painter, &mut enc);
        std::hint::black_box(enc.finish());
    }
    let cpu = started.elapsed().as_secs_f64() * 1000.0 / frames as f64;

    // Throughput: no wait between submissions, so the number is the GPU's own
    // per-frame cost, not a submit-to-done round trip.
    let started = Instant::now();
    for _ in 0..frames {
        let mut enc = device.create_command_encoder(&Default::default());
        encode(painter, &mut enc);
        queue.submit([enc.finish()]);
    }
    let _ = device.poll(wgpu::PollType::wait_indefinitely());
    let gpu = started.elapsed().as_secs_f64() * 1000.0 / frames as f64;
    (cpu, gpu)
}

fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

/// One preview frame, drawn by egui in its own render pass.
///
/// Either the compositor's texture ([`texture`](Self::texture), no CPU work) or
/// a CPU frame that is uploaded and converted
/// ([`new`](Self::new), the fallback).
pub struct PreviewCallback {
    frame: Option<Arc<VideoFrame>>,
    texture: Option<Arc<crate::gpu::PreviewFrame>>,
}

impl PreviewCallback {
    pub fn new(frame: Arc<VideoFrame>) -> PreviewCallback {
        PreviewCallback {
            frame: Some(frame),
            texture: None,
        }
    }

    /// Draw the compositor's own texture: no readback, no upload, no
    /// conversion — one bound texture and one draw.
    pub fn texture(frame: Arc<crate::gpu::PreviewFrame>) -> PreviewCallback {
        PreviewCallback {
            frame: None,
            texture: Some(frame),
        }
    }
}

impl CallbackTrait for PreviewCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        _queue: &wgpu::Queue,
        _screen_descriptor: &ScreenDescriptor,
        egui_encoder: &mut wgpu::CommandEncoder,
        resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if let Some(painter) = resources.get_mut::<PreviewPainter>() {
            match (&self.texture, &self.frame) {
                // A texture needs no preparation: it is already on the GPU.
                (Some(tex), _) => {
                    painter.texture_bind(device, tex);
                }
                (None, Some(frame)) => {
                    painter.upload(device, egui_encoder, frame);
                }
                (None, None) => {}
            }
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::epaint::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        resources: &CallbackResources,
    ) {
        let Some(painter) = resources.get::<PreviewPainter>() else {
            return;
        };
        if let Some(tex) = &self.texture {
            let Some((_, bind)) = painter
                .texture_bound
                .as_ref()
                .filter(|(generation, _)| *generation == tex.generation)
            else {
                return;
            };
            render_pass.set_pipeline(&painter.texture_pipeline);
            render_pass.set_bind_group(0, bind, &[]);
            render_pass.draw(0..3, 0..1);
            return;
        }
        let Some(frame) = &self.frame else {
            return;
        };
        let Some(uploaded) = painter.uploaded.as_ref() else {
            return;
        };
        if uploaded.key != (frame.width, frame.height, frame.format) {
            return;
        }
        render_pass.set_pipeline(&painter.pipelines[uploaded.mode.index()]);
        render_pass.set_bind_group(0, &uploaded.bind, &[]);
        render_pass.draw(0..3, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_types::{PixelFormat, Plane};

    fn gpu() -> Option<(wgpu::Device, wgpu::Queue)> {
        device_with_info().map(|(d, q, _)| (d, q))
    }

    /// A device, its queue and what the adapter calls itself (the app hands all
    /// three to the session so the compositor can share the device).
    fn device_with_info() -> Option<(wgpu::Device, wgpu::Queue, wgpu::AdapterInfo)> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = pollster::block_on(instance.request_adapter(&Default::default())).ok()?;
        let info = adapter.get_info();
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()?;
        Some((device, queue, info))
    }

    /// One frame of the compositor's own texture, for the texture-path tests:
    /// BGRA with a known colour (bytes are B,G,R,A).
    fn composited_texture(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        w: u32,
        h: u32,
        bgra: [u8; 4],
    ) -> wgpu::Texture {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("composited frame"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let row = (w * 4).next_multiple_of(256) as usize;
        let mut data = vec![0u8; row * h as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                data[y * row + x * 4..y * row + x * 4 + 4].copy_from_slice(&bgra);
            }
        }
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row as u32),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        texture
    }

    /// The frame drawn into `target` through one preview pass, as RGBA bytes.
    /// With no composited frame, draw the existing CPU camera upload unchanged.
    fn draw_and_read(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        painter: &mut PreviewPainter,
        frame: Option<&crate::gpu::PreviewFrame>,
        target: &wgpu::Texture,
        (w, h): (u32, u32),
    ) -> Vec<u8> {
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("preview readback"),
            size: (w * h * 4) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let view = target.create_view(&Default::default());
        let mut enc = device.create_command_encoder(&Default::default());
        if let Some(frame) = frame {
            assert!(painter.texture_bind(device, frame), "bound the texture");
        }
        {
            let mut pass = enc
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("preview texture test"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
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
                })
                .forget_lifetime();
            if frame.is_some() {
                pass.set_pipeline(&painter.texture_pipeline);
                pass.set_bind_group(0, &painter.texture_bound.as_ref().expect("bound").1, &[]);
            } else {
                let uploaded = painter.uploaded.as_ref().expect("camera uploaded");
                pass.set_pipeline(&painter.pipelines[uploaded.mode.index()]);
                pass.set_bind_group(0, &uploaded.bind, &[]);
            }
            pass.draw(0..3, 0..1);
        }
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(w * 4),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([enc.finish()]);
        let slice = readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        slice.get_mapped_range().expect("mapped").to_vec()
    }

    fn nv12(w: usize, h: usize, y: u8, u: u8, v: u8) -> VideoFrame {
        VideoFrame {
            pts: 1.0,
            width: w,
            height: h,
            format: PixelFormat::Nv12,
            planes: vec![
                Plane::new(vec![y; w * h], w),
                Plane::new(
                    vec![0u8; w.div_ceil(2) * 2 * h.div_ceil(2)]
                        .into_iter()
                        .flat_map(|_| [u, v])
                        .collect::<Vec<u8>>(),
                    w.div_ceil(2) * 2,
                ),
            ],
        }
    }

    /// Uploading every format the compositor can emit passes wgpu validation
    /// (a bad layout panics inside `copy_buffer_to_texture`). The shader's
    /// colour maths is the one `preview::yuv_to_rgb` is unit tested for; its
    /// output is compared with that reference in `the_shader_matches_the_cpu_reference`
    /// (below, on a real device).
    #[test]
    fn every_format_uploads() {
        let Some((device, _queue)) = gpu() else {
            eprintln!("SKIP: no wgpu adapter");
            return;
        };
        let mut painter = PreviewPainter::new(&device, wgpu::TextureFormat::Bgra8Unorm);
        let frames = [
            crate::media_types::VideoFrame::packed(
                1.0,
                64,
                32,
                PixelFormat::Bgra,
                vec![9; 64 * 32 * 4],
            ),
            crate::media_types::VideoFrame::packed(
                1.0,
                64,
                32,
                PixelFormat::Rgba,
                vec![9; 64 * 32 * 4],
            ),
            nv12(64, 32, 120, 128, 128),
            VideoFrame {
                pts: 1.0,
                width: 64,
                height: 32,
                format: PixelFormat::I420,
                planes: vec![
                    Plane::new(vec![120; 64 * 32], 64),
                    Plane::new(vec![128; 32 * 16], 32),
                    Plane::new(vec![128; 32 * 16], 32),
                ],
            },
        ];
        for frame in &frames {
            let mut encoder = device.create_command_encoder(&Default::default());
            assert!(
                painter.upload(&device, &mut encoder, frame),
                "{:?} uploads",
                frame.format
            );
            // Submitting the copies runs wgpu's validation of every layout.
            let _ = encoder.finish();
        }
    }

    /// A clear-only pass and a full copy, to prove the test's readback path
    /// itself (a broken copy would make every shader test meaningless).
    #[test]
    fn the_readback_path_works() {
        let Some((device, queue)) = gpu() else {
            eprintln!("SKIP: no wgpu adapter");
            return;
        };
        let (w, h) = (64u32, 32u32);
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("readback target"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (w * h * 4) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let view = target.create_view(&Default::default());
        let mut enc = device.create_command_encoder(&Default::default());
        {
            let _pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 1.0,
                            g: 0.25,
                            b: 0.0,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(w * 4),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([enc.finish()]);
        let slice = readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        let data = slice.get_mapped_range().expect("mapped");
        assert_eq!(
            &data[0..4],
            &[255, 64, 0, 255],
            "the clear colour reads back"
        );
    }

    /// The compositor's texture drawn as it is (G10): no upload, no conversion,
    /// the BGRA bytes swapped to RGBA by the shader, and the bind group built
    /// once for a texture that is reused frame after frame.
    #[test]
    fn the_texture_path_draws_the_composited_frame() {
        let Some((device, queue)) = gpu() else {
            eprintln!("SKIP: no wgpu adapter");
            return;
        };
        let mut painter = PreviewPainter::new(&device, wgpu::TextureFormat::Rgba8Unorm);
        // BGRA (30, 20, 10) is RGB (10, 20, 30).
        let texture = composited_texture(&device, &queue, 1920, 1080, [30, 20, 10, 255]);
        let (w, h) = (704u32, 396u32);
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("preview target"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let frame = crate::gpu::PreviewFrame {
            texture: texture.clone(),
            width: 1920,
            height: 1080,
            pts: 1.0,
            generation: 1,
        };
        let data = draw_and_read(&device, &queue, &mut painter, Some(&frame), &target, (w, h));
        let centre = (((h / 2) * w + w / 2) * 4) as usize;
        assert_eq!(
            &data[centre..centre + 4],
            &[10, 20, 30, 255],
            "the composited frame's colour, unswapped"
        );
        // The corner too: the pass fills the whole rect.
        assert_eq!(&data[0..4], &[10, 20, 30, 255]);
        // The same texture again: the cached bind group is reused (the
        // generation did not change), and it still draws.
        let data = draw_and_read(&device, &queue, &mut painter, Some(&frame), &target, (w, h));
        assert_eq!(&data[centre..centre + 4], &[10, 20, 30, 255]);
    }

    /// An idle camera is uploaded once but painted on every preview redraw.
    /// Compare full GPU readbacks so a blank second pass cannot go unnoticed.
    #[test]
    fn unchanged_camera_upload_draws_twice_without_flicker() {
        let Some((device, queue)) = gpu() else {
            eprintln!("SKIP: no wgpu adapter");
            return;
        };
        let mut painter = PreviewPainter::new(&device, wgpu::TextureFormat::Rgba8Unorm);
        let (w, h) = (64, 32);
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("unchanged camera preview target"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let packed = VideoFrame::packed(
            1.0,
            w as usize,
            h as usize,
            PixelFormat::Bgra,
            [30, 20, 10, 255].repeat((w * h) as usize),
        );
        let (r, g, b) = crate::app::preview::yuv_to_rgb(180, 60, 200);
        for (camera, expected) in [
            (packed, [10, 20, 30, 255]),
            (nv12(w as usize, h as usize, 180, 60, 200), [r, g, b, 255]),
        ] {
            let mut encoder = device.create_command_encoder(&Default::default());
            assert!(painter.upload(&device, &mut encoder, &camera));
            queue.submit([encoder.finish()]);
            let first = draw_and_read(&device, &queue, &mut painter, None, &target, (w, h));
            for pixel in first.chunks_exact(4) {
                for (got, want) in pixel.iter().zip(expected) {
                    assert!(got.abs_diff(want) <= 1, "camera colour: {pixel:?}");
                }
            }
            // A separate submission clears and repaints the target without a
            // new upload: the retained camera texture must still be sampled.
            let second = draw_and_read(&device, &queue, &mut painter, None, &target, (w, h));
            assert_eq!(second, first, "{:?} camera flickered", camera.format);
        }
    }

    /// The shader, run for real: a 64×32 NV12 frame of one colour, drawn into a
    /// texture, and the centre pixel compared with the CPU reference
    /// (`preview::yuv_to_rgb`, the BT.709 maths the WGSL copies).
    #[test]
    fn the_shader_matches_the_cpu_reference() {
        let Some((device, queue)) = gpu() else {
            eprintln!("SKIP: no wgpu adapter");
            return;
        };
        let mut painter = PreviewPainter::new(&device, wgpu::TextureFormat::Rgba8Unorm);
        let (w, h) = (64usize, 32usize);
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("preview test target"),
            size: wgpu::Extent3d {
                width: w as u32,
                height: h as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("preview test readback"),
            size: (w * h * 4) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        // A colour with all three components, so a swapped channel shows.
        for (y, u, v) in [(180u8, 60u8, 200u8), (81, 90, 240), (235, 128, 128)] {
            let frame = nv12(w, h, y, u, v);
            let mut encoder = device.create_command_encoder(&Default::default());
            assert!(painter.upload(&device, &mut encoder, &frame));
            {
                let uploaded = painter.uploaded.as_ref().expect("uploaded");
                // The view must outlive the pass (a temporary would be dropped
                // before it is used).
                let target_view = target.create_view(&Default::default());
                let mut pass = encoder
                    .begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("preview test"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &target_view,
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
                    })
                    .forget_lifetime();
                pass.set_pipeline(&painter.pipelines[uploaded.mode.index()]);
                pass.set_bind_group(0, &uploaded.bind, &[]);
                pass.draw(0..3, 0..1);
            }
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &target,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some((w * 4) as u32),
                        rows_per_image: Some(h as u32),
                    },
                },
                wgpu::Extent3d {
                    width: w as u32,
                    height: h as u32,
                    depth_or_array_layers: 1,
                },
            );
            queue.submit([encoder.finish()]);
            let slice = readback.slice(..);
            slice.map_async(wgpu::MapMode::Read, |_| {});
            device
                .poll(wgpu::PollType::wait_indefinitely())
                .expect("poll");
            let data = slice.get_mapped_range().expect("mapped readback");
            let centre = ((h / 2) * w + w / 2) * 4;
            let got = [
                data[centre],
                data[centre + 1],
                data[centre + 2],
                data[centre + 3],
            ];
            let (r, g, b) = crate::app::preview::yuv_to_rgb(y, u, v);
            // A sampler's linear filter on a uniform plane is exact; allow a
            // single step for the unorm conversion.
            // A sampler's linear filter on a uniform plane is exact; allow one
            // step for the unorm conversion.
            for (i, want) in [r, g, b].into_iter().enumerate() {
                assert!(
                    got[i].abs_diff(want) <= 1,
                    "yuv({y},{u},{v}) -> shader {got:?}, cpu ({r},{g},{b})"
                );
            }
            drop(data);
            readback.unmap();
        }
    }
}
