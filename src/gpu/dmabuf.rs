//! DMA-BUF screen frames: what a device can import, and whether to ask for them.
//!
//! The screen source gets CPU frames from PipeWire today and copies each one
//! (4K: 33 MB a frame) before the compositor uploads it again. A compositor that
//! offers `SPA_DATA_DmaBuf` would let the frame stay on the GPU: the buffer is
//! imported as a Vulkan image and sampled by the compositor directly.
//!
//! This module is the part of that which can be decided and checked without a
//! DMA-BUF-capable device (the dev container's software adapter has no DRM
//! device at all):
//!
//! - [`support`] asks the adapter whether it can import one
//!   (`wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF`, which wgpu implies for
//!   `VK_EXT_external_memory_dma_buf` + `VK_EXT_image_drm_format_modifier`).
//! - [`negotiation`] is the policy the screen source will apply to what
//!   PipeWire offers: DMA-BUF only when the device can import it *and* the
//!   format is one we understand (single-plane, known fourcc, explicit
//!   modifier); shared memory otherwise.
//! - [`import`] wraps `wgpu-hal`'s Vulkan `texture_from_dmabuf_fd`, which is the
//!   only public way into wgpu: the safe API has no DMA-BUF entry point.
//!
//! What is *not* here, and why: the buffer has to reach the compositor, and
//! [`crate::media_types::VideoFrame`] is CPU planes — a frame that carries a
//! `wgpu::Texture` is a contract change, and the compositor would then need to
//! know the screen's texture *and* its CPU frame for the same slot. Until that
//! decision, the screen source keeps negotiating shared memory, and this module
//! answers the question the moment it lands. `docs/DESKTOP_CHECKLIST.md` has the
//! manual check.

use crate::media_types::PixelFormat;

/// The DMA-BUF formats a Linux compositor sends for a screen share, as DRM
/// fourccs (little endian, as `DRM_FORMAT_*` spells them in the kernel).
pub const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258; // 'XR24'
pub const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241; // 'AR24'
pub const DRM_FORMAT_XBGR8888: u32 = 0x3432_4258; // 'XB24'
pub const DRM_FORMAT_ABGR8888: u32 = 0x3432_4241; // 'AB24'
pub const DRM_FORMAT_RGBX8888: u32 = 0x3432_5852; // 'RX24'
pub const DRM_FORMAT_RGBA8888: u32 = 0x3432_4152; // 'RA24'
/// `DRM_FORMAT_MOD_LINEAR`: no tiling, so no modifier extension needed to
/// describe it.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;
/// The implicit ("driver decides") modifier: the layout is not described, so it
/// cannot be imported with `VK_EXT_image_drm_format_modifier`'s explicit path.
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// What the adapter can do with DMA-BUF images.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaBufSupport {
    /// The device can import a DMA-BUF as a sampled texture.
    pub import: bool,
}

impl DmaBufSupport {
    /// Nothing: the source must ask for shared memory.
    pub const NONE: DmaBufSupport = DmaBufSupport { import: false };
}

/// Whether `adapter` can import DMA-BUF textures.
///
/// The feature covers both Vulkan extensions the import needs, and requesting it
/// on the device is a separate step (`Renderer` does not request it today: a
/// requested feature the driver lacks fails device creation, and nothing imports
/// yet).
pub fn support(adapter: &wgpu::Adapter) -> DmaBufSupport {
    DmaBufSupport {
        import: adapter
            .features()
            .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF),
    }
}

/// [`support`] for the adapter this process can get, probed once.
///
/// Only for diagnostics (the screen source's error message): a capture path
/// never creates a device just to answer a question, and the answer cannot
/// change while the process runs.
pub fn probe_support() -> DmaBufSupport {
    use std::sync::OnceLock;
    static PROBED: OnceLock<DmaBufSupport> = OnceLock::new();
    *PROBED.get_or_init(|| {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        match pollster::block_on(instance.request_adapter(&Default::default())) {
            Ok(adapter) => support(&adapter),
            Err(_) => DmaBufSupport::NONE,
        }
    })
}

/// One buffer layout a producer offers, in the terms the policy needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offered {
    /// `SPA_DATA_DmaBuf` versus `SPA_DATA_MemPtr` (shared memory).
    pub dmabuf: bool,
    /// DRM fourcc of the pixel format (`0` for a shared-memory offer, which does
    /// not carry one).
    pub fourcc: u32,
    /// The buffer's modifier; `DRM_FORMAT_MOD_INVALID` when the producer did not
    /// say (implicit tiling).
    pub modifier: u64,
    /// The first plane. The import path handles single-plane buffers only.
    pub planes: usize,
}

impl Offered {
    /// A shared-memory offer of a packed BGRA/XRGB buffer.
    pub fn shm() -> Offered {
        Offered {
            dmabuf: false,
            fourcc: DRM_FORMAT_XRGB8888,
            modifier: DRM_FORMAT_MOD_INVALID,
            planes: 1,
        }
    }

    /// A DMA-BUF offer of one plane.
    pub fn dmabuf(fourcc: u32, modifier: u64) -> Offered {
        Offered {
            dmabuf: true,
            fourcc,
            modifier,
            planes: 1,
        }
    }
}

/// What the screen source should ask for and accept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Negotiation {
    /// Only `SPA_DATA_MemPtr`: the device cannot import, or nothing offered is
    /// importable. What the port ships today.
    SharedMemory,
    /// `SPA_DATA_DmaBuf` with this format: import it, no CPU copy.
    DmaBuf { format: PixelFormat, modifier: u64 },
}

impl Negotiation {
    pub fn is_dmabuf(self) -> bool {
        matches!(self, Negotiation::DmaBuf { .. })
    }
}

/// The pixel format of a DRM fourcc, when the compositor can sample it.
///
/// The compositor's output is BGRA (and the screen path converts RGB variants
/// the same way), so both byte orders map onto the two packed formats the
/// contract already has.
pub fn pixel_format(fourcc: u32) -> Option<PixelFormat> {
    match fourcc {
        DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888 => Some(PixelFormat::Bgra),
        DRM_FORMAT_XBGR8888 | DRM_FORMAT_ABGR8888 => Some(PixelFormat::Bgra),
        DRM_FORMAT_RGBX8888 | DRM_FORMAT_RGBA8888 => Some(PixelFormat::Rgba),
        _ => None,
    }
}

/// Whether a buffer offered with this layout can be imported as it is.
pub fn importable(offer: &Offered, support: DmaBufSupport) -> Option<(PixelFormat, u64)> {
    if !support.import || !offer.dmabuf {
        return None;
    }
    // `wgpu-hal`'s importer takes one plane and an explicit modifier; an
    // implicit ("driver decides") layout cannot be described to Vulkan.
    if offer.planes != 1 || offer.modifier == DRM_FORMAT_MOD_INVALID {
        return None;
    }
    pixel_format(offer.fourcc).map(|format| (format, offer.modifier))
}

/// The policy: ask for and accept DMA-BUF when the device can import one of the
/// offers, otherwise shared memory.
///
/// The order of `offered` is the producer's preference; the first importable
/// entry wins, so a list that starts with an exotic tiled format and ends with a
/// linear one still gives a working import.
pub fn negotiation(offered: &[Offered], support: DmaBufSupport) -> Negotiation {
    for offer in offered {
        if let Some((format, modifier)) = importable(offer, support) {
            return Negotiation::DmaBuf { format, modifier };
        }
    }
    Negotiation::SharedMemory
}

/// Why a stream's format cannot be consumed, in terms that tell the user what to
/// do about it.
///
/// The screen source's error messages use this: "the device cannot import a
/// DMA-BUF" and "the device could, but this build does not import yet" send the
/// reader to different places.
pub fn refusal(support: DmaBufSupport) -> &'static str {
    if support.import {
        "this device can import a DMA-BUF, but the compositor's frame contract has no texture \
         frame yet (see docs/DESKTOP_CHECKLIST.md); ask the compositor for shared memory"
    } else {
        "this device cannot import DMA-BUFs (Mesa's software adapter has no DRM device, and wgpu \
         needs VK_EXT_external_memory_dma_buf + VK_EXT_image_drm_format_modifier); ask the \
         compositor for shared memory"
    }
}

/// A DMA-BUF imported as a wgpu texture.
#[derive(Debug)]
pub struct Imported {
    pub texture: wgpu::Texture,
    pub width: u32,
    pub height: u32,
    pub format: wgpu::TextureFormat,
}

/// One DMA-BUF to import: the fd and the layout the producer laid it out with.
#[derive(Debug)]
pub struct DmaBufFrame {
    /// The buffer's file descriptor. Consumed on success, closed on failure
    /// (Vulkan's and `wgpu-hal`'s rule).
    pub fd: std::os::unix::io::OwnedFd,
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
    pub modifier: u64,
    pub stride: u64,
    pub offset: u64,
}

/// Import one DMA-BUF as a sampled texture.
///
/// # Safety
///
/// The caller must own `frame.fd` and keep the producer from writing to the
/// buffer until the GPU has finished sampling it (the source must not release
/// the PipeWire buffer before the frame's submission completes).
///
/// Only reachable on a device created with
/// `Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF`; every other path returns an
/// error before touching Vulkan.
pub unsafe fn import(device: &wgpu::Device, frame: DmaBufFrame) -> Result<Imported, String> {
    let DmaBufFrame {
        fd,
        format,
        width,
        height,
        modifier,
        stride,
        offset,
    } = frame;
    if !device
        .features()
        .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
    {
        return Err("the device was not created with VULKAN_EXTERNAL_MEMORY_DMA_BUF".into());
    }
    if width == 0 || height == 0 {
        return Err("a zero-sized DMA-BUF".into());
    }
    let texture_format = match format {
        // The compositor's screen path wants packed BGRA; a BGRX buffer keeps
        // its (undefined) alpha and is composited as opaque.
        PixelFormat::Bgra => wgpu::TextureFormat::Bgra8Unorm,
        PixelFormat::Rgba => wgpu::TextureFormat::Rgba8Unorm,
        other => return Err(format!("{} cannot come from a DMA-BUF", other.label())),
    };
    let desc = wgpu::TextureDescriptor {
        label: Some("screen dmabuf"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: texture_format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    };
    // The hal descriptor, which the importer takes instead of wgpu's.
    let hal_desc = wgpu::hal::TextureDescriptor {
        label: Some("screen dmabuf"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: texture_format,
        usage: wgpu::TextureUses::RESOURCE | wgpu::TextureUses::COPY_SRC,
        memory_flags: wgpu::hal::MemoryFlags::empty(),
        view_formats: vec![],
    };
    // SAFETY: the device is a Vulkan device (the feature is Vulkan-only) and the
    // caller guarantees the fd and its layout.
    unsafe {
        let hal_device = device
            .as_hal::<wgpu::hal::api::Vulkan>()
            .ok_or_else(|| "the device is not a Vulkan device".to_string())?;
        let hal_texture = hal_device
            .texture_from_dmabuf_fd(fd, &hal_desc, modifier, stride, offset)
            .map_err(|e| format!("the driver refused the DMA-BUF: {e}"))?;
        let texture = device.create_texture_from_hal::<wgpu::hal::api::Vulkan>(
            hal_texture,
            &desc,
            wgpu::TextureUses::RESOURCE,
        );
        Ok(Imported {
            texture,
            width,
            height,
            format: texture_format,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> Option<wgpu::Adapter> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        pollster::block_on(instance.request_adapter(&Default::default())).ok()
    }

    #[test]
    fn fourccs_map_to_the_formats_the_compositor_samples() {
        assert_eq!(pixel_format(DRM_FORMAT_XRGB8888), Some(PixelFormat::Bgra));
        assert_eq!(pixel_format(DRM_FORMAT_ARGB8888), Some(PixelFormat::Bgra));
        assert_eq!(pixel_format(DRM_FORMAT_XBGR8888), Some(PixelFormat::Bgra));
        assert_eq!(pixel_format(DRM_FORMAT_ABGR8888), Some(PixelFormat::Bgra));
        assert_eq!(pixel_format(DRM_FORMAT_RGBA8888), Some(PixelFormat::Rgba));
        assert_eq!(pixel_format(DRM_FORMAT_RGBX8888), Some(PixelFormat::Rgba));
        // NV12 as a DMA-BUF would be a two-plane buffer (not handled), and an
        // unknown fourcc is not something to guess at.
        assert_eq!(pixel_format(0x3231_564e), None, "NV12 is two planes");
        assert_eq!(pixel_format(0), None);
    }

    #[test]
    fn shared_memory_is_the_default_and_the_fallback() {
        let supported = DmaBufSupport { import: true };
        // Nothing offered is importable: shared memory.
        assert_eq!(negotiation(&[], supported), Negotiation::SharedMemory);
        assert_eq!(
            negotiation(&[Offered::shm()], supported),
            Negotiation::SharedMemory,
            "a shared-memory offer is not a DMA-BUF one"
        );
        // A device that cannot import: shared memory even when offered one.
        assert_eq!(
            negotiation(
                &[Offered::dmabuf(DRM_FORMAT_XRGB8888, DRM_FORMAT_MOD_LINEAR)],
                DmaBufSupport::NONE
            ),
            Negotiation::SharedMemory
        );
    }

    #[test]
    fn an_importable_offer_is_taken() {
        let supported = DmaBufSupport { import: true };
        assert_eq!(
            negotiation(
                &[
                    Offered::shm(),
                    Offered::dmabuf(DRM_FORMAT_XRGB8888, DRM_FORMAT_MOD_LINEAR)
                ],
                supported
            ),
            Negotiation::DmaBuf {
                format: PixelFormat::Bgra,
                modifier: DRM_FORMAT_MOD_LINEAR
            }
        );
        // A vendor tiled modifier with an explicit value is importable too: the
        // importer describes it to Vulkan.
        let tiled = 0x0100_0000_0000_0003; // an AMD GFX9 tiling pattern
        assert_eq!(
            negotiation(&[Offered::dmabuf(DRM_FORMAT_ABGR8888, tiled)], supported),
            Negotiation::DmaBuf {
                format: PixelFormat::Bgra,
                modifier: tiled
            }
        );
        // The producer's order is respected: the first importable entry wins,
        // even when a later one would also work.
        assert_eq!(
            negotiation(
                &[
                    Offered::dmabuf(DRM_FORMAT_RGBA8888, 7),
                    Offered::dmabuf(DRM_FORMAT_XRGB8888, DRM_FORMAT_MOD_LINEAR)
                ],
                supported
            ),
            Negotiation::DmaBuf {
                format: PixelFormat::Rgba,
                modifier: 7
            }
        );
    }

    #[test]
    fn what_cannot_be_described_to_vulkan_is_refused() {
        let supported = DmaBufSupport { import: true };
        // An implicit modifier: the layout is unknown, so no explicit import.
        assert_eq!(
            negotiation(
                &[Offered::dmabuf(DRM_FORMAT_XRGB8888, DRM_FORMAT_MOD_INVALID)],
                supported
            ),
            Negotiation::SharedMemory
        );
        // Two planes (NV12): the importer handles one.
        let mut two_planes = Offered::dmabuf(DRM_FORMAT_XRGB8888, DRM_FORMAT_MOD_LINEAR);
        two_planes.planes = 2;
        assert_eq!(
            negotiation(&[two_planes], supported),
            Negotiation::SharedMemory
        );
        // An unknown fourcc.
        assert_eq!(
            negotiation(
                &[Offered::dmabuf(0xdead_beef, DRM_FORMAT_MOD_LINEAR)],
                supported
            ),
            Negotiation::SharedMemory
        );
    }

    /// The adapter the tests run on, and what it can do: on the container's
    /// software Vulkan device (llvmpipe, no DRM device) the answer is no, and
    /// the source keeps negotiating shared memory. On a GPU desktop this prints
    /// what the device would take.
    #[test]
    fn the_software_adapter_cannot_import() {
        let Some(adapter) = adapter() else {
            eprintln!("SKIP: no wgpu adapter");
            return;
        };
        let info = adapter.get_info();
        let support = support(&adapter);
        println!(
            "dmabuf support on {} ({:?}): import {}",
            info.name, info.device_type, support.import
        );
        if info.device_type == wgpu::DeviceType::Cpu {
            assert!(
                !support.import,
                "a CPU adapter cannot import DMA-BUFs (llvmpipe has no DRM device)"
            );
        }
        // Whatever the device is, the negotiation follows it.
        let offer = Offered::dmabuf(DRM_FORMAT_XRGB8888, DRM_FORMAT_MOD_LINEAR);
        assert_eq!(
            negotiation(&[offer], support).is_dmabuf(),
            support.import,
            "the policy must follow the device"
        );
        assert_eq!(
            negotiation(&[offer], DmaBufSupport::NONE),
            Negotiation::SharedMemory,
            "and never ask for what it cannot take"
        );
    }

    /// Importing without the feature is refused before any Vulkan call: the
    /// guard the source relies on.
    #[test]
    fn importing_without_the_feature_is_an_error_not_a_crash() {
        let Some(adapter) = adapter() else {
            eprintln!("SKIP: no wgpu adapter");
            return;
        };
        let Ok((device, _queue)) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
        else {
            eprintln!("SKIP: no wgpu device");
            return;
        };
        if device
            .features()
            .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
        {
            eprintln!("SKIP: this device requested the DMA-BUF feature");
            return;
        }
        // A pipe, not a DMA-BUF: it must never get as far as Vulkan, because the
        // feature check comes first.
        let (read, _write) = std::io::pipe().expect("a pipe");
        let fd = std::os::fd::OwnedFd::from(read);
        let frame = DmaBufFrame {
            fd,
            format: PixelFormat::Bgra,
            width: 64,
            height: 64,
            modifier: DRM_FORMAT_MOD_LINEAR,
            stride: 256,
            offset: 0,
        };
        let err = unsafe { import(&device, frame) }
            .expect_err("without the feature the import must fail");
        assert!(err.contains("VULKAN_EXTERNAL_MEMORY_DMA_BUF"), "{err}");
    }
}
