//! The compositor's pipelines, bind group layouts and per-pass uniforms.
//!
//! Every pass declares one `vec4<f32>` block per parameter group, so the Rust
//! side never has to reason about WGSL's layout rules: [`Uni`] appends whole
//! vec4s and the shader reads `b0`, `b1`, …
//!
//! Each pass has its own uniform buffer. Two dispatches that shared one buffer
//! would both see the last write: `Queue::write_buffer` copies are executed at
//! the start of the submission, not where they were encoded.

use anyhow::Result;

use crate::gpu::{
    FMT_OUT, FMT_RGBA, FMT_UV, FMT_WORK, FMT_Y, SHADER_COMMON, SHADER_COMPOSITE, SHADER_CONVERT,
    SHADER_LANCZOS, SHADER_YUV,
};

/// A uniform block: 32 `vec4<f32>`, 128 bytes, plenty for every pass.
const UNIFORM_BYTES: usize = 128;

/// Which pass's uniform buffer.
///
/// One buffer per *dispatch*, not per pass: a frame runs the converter twice
/// (screen and camera) and the scaler twice (horizontally and vertically), and
/// two dispatches that shared a buffer would both read the last thing written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    ConvertScreen,
    ConvertCam,
    LanczosH,
    LanczosV,
    Composite,
    Yuv,
}

impl Slot {
    fn index(self) -> usize {
        match self {
            Slot::ConvertScreen => 0,
            Slot::ConvertCam => 1,
            Slot::LanczosH => 2,
            Slot::LanczosV => 3,
            Slot::Composite => 4,
            Slot::Yuv => 5,
        }
    }
}

/// One pass's parameters, laid out as `vec4<f32>` blocks.
pub struct Uni {
    bytes: [u8; UNIFORM_BYTES],
    len: usize,
}

impl Uni {
    pub fn new() -> Self {
        Uni {
            bytes: [0; UNIFORM_BYTES],
            len: 0,
        }
    }

    /// Append a vec4. The shader reads it as `b{len / 16}`.
    pub fn vec4(&mut self, v: &[f32; 4]) -> &mut Self {
        for x in v {
            self.bytes[self.len..self.len + 4].copy_from_slice(&x.to_le_bytes());
            self.len += 4;
        }
        self
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len.max(16)]
    }
}

impl Default for Uni {
    fn default() -> Self {
        Self::new()
    }
}

/// The pipelines, their layouts, the sampler and the uniform buffers.
pub struct Passes {
    pub convert_layout: wgpu::BindGroupLayout,
    pub convert_packed: wgpu::ComputePipeline,
    pub convert_nv12: wgpu::ComputePipeline,
    pub convert_i420: wgpu::ComputePipeline,
    pub lanczos_layout: wgpu::BindGroupLayout,
    pub lanczos: wgpu::ComputePipeline,
    pub composite_layout: wgpu::BindGroupLayout,
    pub composite: wgpu::RenderPipeline,
    pub yuv_layout: wgpu::BindGroupLayout,
    pub yuv_y: wgpu::RenderPipeline,
    pub yuv_nv12: wgpu::RenderPipeline,
    pub yuv_i420: wgpu::RenderPipeline,
    /// Bilinear, clamp to edge: CoreImage's default sampling for the camera.
    pub sampler: wgpu::Sampler,
    uniforms: [wgpu::Buffer; 6],
}

const COMPUTE: wgpu::ShaderStages = wgpu::ShaderStages::COMPUTE;
const FRAGMENT: wgpu::ShaderStages = wgpu::ShaderStages::FRAGMENT;

fn uniform_entry(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn texture_entry(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Texture {
            // Every format the passes read is filterable in core WebGPU; the
            // passes themselves use textureLoad except for the camera.
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn storage_entry(
    binding: u32,
    visibility: wgpu::ShaderStages,
    format: wgpu::TextureFormat,
) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::StorageTexture {
            access: wgpu::StorageTextureAccess::WriteOnly,
            format,
            view_dimension: wgpu::TextureViewDimension::D2,
        },
        count: None,
    }
}

fn module(device: &wgpu::Device, label: &str, common: &str, source: &str) -> wgpu::ShaderModule {
    // One string, so naga's line numbers match the file the code came from
    // closely enough to read.
    let full = format!("{common}\n{source}");
    device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(full.into()),
    })
}

/// Build a pass, turning a shader or layout error into a readable message: a
/// WGSL mistake otherwise only shows up as an uncaptured GPU error later.
fn scoped<T>(device: &wgpu::Device, what: &str, build: impl FnOnce() -> T) -> Result<T> {
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let out = build();
    match pollster::block_on(scope.pop()) {
        Some(err) => Err(anyhow::anyhow!("{what}: {err}")),
        None => Ok(out),
    }
}

fn layout(
    device: &wgpu::Device,
    label: &str,
    entries: &[wgpu::BindGroupLayoutEntry],
) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries,
    })
}

fn pipeline_layout(
    device: &wgpu::Device,
    label: &str,
    bgl: &wgpu::BindGroupLayout,
) -> wgpu::PipelineLayout {
    device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(bgl)],
        immediate_size: 0,
    })
}

fn compute_pipeline(
    device: &wgpu::Device,
    label: &str,
    bgl: &wgpu::BindGroupLayout,
    module: &wgpu::ShaderModule,
    entry: &str,
) -> Result<wgpu::ComputePipeline> {
    let layout = pipeline_layout(device, label, bgl);
    scoped(device, label, || {
        device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some(label),
            layout: Some(&layout),
            module,
            entry_point: Some(entry),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        })
    })
}

impl Passes {
    pub fn new(device: &wgpu::Device) -> Result<Passes> {
        let convert_module = module(device, "convert", SHADER_COMMON, SHADER_CONVERT);
        let lanczos_module = module(device, "lanczos", SHADER_COMMON, SHADER_LANCZOS);
        let composite_module = module(device, "composite", SHADER_COMMON, SHADER_COMPOSITE);
        let yuv_module = module(device, "yuv", SHADER_COMMON, SHADER_YUV);

        let convert_layout = layout(
            device,
            "convert",
            &[
                texture_entry(0, COMPUTE),
                texture_entry(1, COMPUTE),
                texture_entry(2, COMPUTE),
                uniform_entry(3, COMPUTE),
                storage_entry(4, COMPUTE, FMT_RGBA),
            ],
        );
        let convert_packed = compute_pipeline(
            device,
            "convert packed",
            &convert_layout,
            &convert_module,
            "from_packed",
        )?;
        let convert_nv12 = compute_pipeline(
            device,
            "convert nv12",
            &convert_layout,
            &convert_module,
            "from_nv12",
        )?;
        let convert_i420 = compute_pipeline(
            device,
            "convert i420",
            &convert_layout,
            &convert_module,
            "from_i420",
        )?;

        let lanczos_layout = layout(
            device,
            "lanczos",
            &[
                texture_entry(0, COMPUTE),
                uniform_entry(1, COMPUTE),
                storage_entry(2, COMPUTE, FMT_WORK),
            ],
        );
        let lanczos =
            compute_pipeline(device, "lanczos", &lanczos_layout, &lanczos_module, "scale")?;

        let composite_layout = layout(
            device,
            "composite",
            &[
                texture_entry(0, FRAGMENT),
                texture_entry(1, FRAGMENT),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                texture_entry(3, FRAGMENT),
                uniform_entry(4, FRAGMENT),
            ],
        );
        let composite_layout_pipeline = pipeline_layout(device, "composite", &composite_layout);
        let composite = scoped(device, "composite", || {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("composite"),
                layout: Some(&composite_layout_pipeline),
                vertex: wgpu::VertexState {
                    module: &composite_module,
                    entry_point: Some("vs"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &composite_module,
                    entry_point: Some("fs"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: FMT_OUT,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        })?;

        let yuv_layout = layout(
            device,
            "yuv",
            &[texture_entry(0, FRAGMENT), uniform_entry(1, FRAGMENT)],
        );
        let yuv_pipeline_layout = pipeline_layout(device, "yuv", &yuv_layout);
        let fullscreen = |entry: &str, targets: &[Option<wgpu::ColorTargetState>]| {
            scoped(device, entry, || {
                device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(entry),
                    layout: Some(&yuv_pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &yuv_module,
                        entry_point: Some("vs"),
                        compilation_options: wgpu::PipelineCompilationOptions::default(),
                        buffers: &[],
                    },
                    primitive: wgpu::PrimitiveState::default(),
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    fragment: Some(wgpu::FragmentState {
                        module: &yuv_module,
                        entry_point: Some(entry),
                        compilation_options: wgpu::PipelineCompilationOptions::default(),
                        targets,
                    }),
                    multiview_mask: None,
                    cache: None,
                })
            })
        };
        // Full-resolution luma, then interleaved or planar chroma at half
        // resolution.
        let luma_target = |format| {
            Some(wgpu::ColorTargetState {
                format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })
        };
        let yuv_y = fullscreen("fs_y", &[luma_target(FMT_Y)])?;
        let yuv_nv12 = fullscreen("fs_nv12", &[luma_target(FMT_UV)])?;
        let yuv_i420 = fullscreen("fs_i420", &[luma_target(FMT_Y), luma_target(FMT_Y)])?;

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("camera"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            lod_min_clamp: 0.0,
            lod_max_clamp: 0.0,
            ..Default::default()
        });

        let uniforms = std::array::from_fn(|_| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("pass uniforms"),
                size: UNIFORM_BYTES as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        });

        Ok(Passes {
            convert_layout,
            convert_packed,
            convert_nv12,
            convert_i420,
            lanczos_layout,
            lanczos,
            composite_layout,
            composite,
            yuv_layout,
            yuv_y,
            yuv_nv12,
            yuv_i420,
            sampler,
            uniforms,
        })
    }

    /// Upload a pass's parameters. The copy runs before the pass that reads
    /// them, because it is issued before the submission.
    pub fn write(&self, queue: &wgpu::Queue, slot: Slot, uni: &Uni) {
        queue.write_buffer(&self.uniforms[slot.index()], 0, uni.as_bytes());
    }

    pub fn uniform(&self, slot: Slot) -> wgpu::BindingResource<'_> {
        self.uniforms[slot.index()].as_entire_binding()
    }
}
