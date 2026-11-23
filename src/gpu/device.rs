//! The wgpu instance, adapter and device the compositor draws on.
//!
//! The renderer can own its device (the CLI writer, headless) or take the one
//! the app already has from egui (phase 1E), so a recording and the preview
//! share one device and one queue. Either way the device is a Vulkan or GL
//! device: on a machine without a GPU, Mesa's software Vulkan driver
//! (lavapipe) is picked up as an ordinary adapter, which is why the tests run
//! everywhere.

use std::sync::Arc;

use anyhow::{Context, Result};

/// The device, its queue, and what the adapter calls itself.
pub struct Gpu {
    /// The instance the adapter came from. Adapters and devices keep it alive
    /// internally; holding it makes that explicit and costs nothing.
    instance: Option<wgpu::Instance>,
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    pub info: wgpu::AdapterInfo,
    /// True when the device belongs to the caller (the app's egui-wgpu
    /// device) rather than to the renderer.
    shared: bool,
}

impl Gpu {
    /// A device of our own, for the writer: no surface, no window.
    pub fn headless() -> Result<Self> {
        // WGPU_BACKEND and friends override, like every wgpu program.
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends =
            wgpu::Backends::from_env().unwrap_or(wgpu::Backends::VULKAN | wgpu::Backends::GL);
        let instance = wgpu::Instance::new(descriptor);
        let request = |fallback: bool| {
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::None,
                force_fallback_adapter: fallback,
                compatible_surface: None,
                apply_limit_buckets: false,
            }))
        };
        let adapter = match request(false) {
            Ok(a) => a,
            Err(first) => request(true).with_context(|| {
                format!(
                    "no GPU adapter for the compositor: {first}; no software fallback either \
                     (install Mesa's Vulkan drivers, `mesa-vulkan-drivers`, for lavapipe)"
                )
            })?,
        };
        let info = adapter.get_info();
        let (device, queue) = Self::request_device(&adapter)?;
        Ok(Gpu {
            instance: Some(instance),
            device: Arc::new(device),
            queue: Arc::new(queue),
            info,
            shared: false,
        })
    }

    /// Wrap a device the caller owns (egui's, in the app).
    ///
    /// The adapter information cannot be recovered from a device, so the app
    /// passes what its own adapter reported; it is only used for the status
    /// line.
    pub fn shared(
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        info: wgpu::AdapterInfo,
    ) -> Self {
        Gpu {
            instance: None,
            device,
            queue,
            info,
            shared: true,
        }
    }

    fn request_device(adapter: &wgpu::Adapter) -> Result<(wgpu::Device, wgpu::Queue)> {
        // Downlevel defaults: what a device made by egui-wgpu may have, so a
        // renderer sharing that device needs nothing more. Resolutions (texture
        // sizes) follow the adapter, which is what 4K output needs.
        let required_limits = wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits());
        let desc = wgpu::DeviceDescriptor {
            label: Some("rec-rs compositor"),
            required_features: wgpu::Features::empty(),
            required_limits,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&desc))
            .with_context(|| format!("could not create a GPU device on {adapter:?}"))?;
        device.on_uncaptured_error(Arc::new(|err| {
            eprintln!("rec-rs: GPU error: {err}");
        }));
        Ok((device, queue))
    }

    /// One line for the status output and the report: which adapter, which
    /// backend, hardware or software.
    pub fn describe(&self) -> String {
        let info = &self.info;
        format!(
            "{} ({}, {:?}, {:?})",
            info.name,
            info.driver_info
                .split(['(', ' '])
                .next()
                .unwrap_or(&info.driver_info),
            info.backend,
            info.device_type,
        )
    }

    /// True when this is the caller's device (the app's), so its textures can
    /// be drawn in the caller's render passes.
    pub fn is_shared(&self) -> bool {
        self.shared
    }

    /// True when the device is a CPU rasteriser rather than a GPU: the tests
    /// report it and the performance numbers are only comparable to each
    /// other.
    pub fn is_software(&self) -> bool {
        matches!(self.info.device_type, wgpu::DeviceType::Cpu)
    }
}
