#[cfg(not(target_family = "wasm"))]
use anyhow::Context as _;
#[cfg(not(target_family = "wasm"))]
use gpui_util::ResultExt;
#[cfg(not(target_family = "wasm"))]
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use wgpu::TextureFormat;

pub struct WgpuContext {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    dual_source_blending: bool,
    color_texture_format: wgpu::TextureFormat,
    device_lost: Arc<AtomicBool>,
}

#[derive(Clone, Copy)]
pub struct CompositorGpuHint {
    pub vendor_id: u32,
    pub device_id: u32,
}

impl WgpuContext {
    #[cfg(not(target_family = "wasm"))]
    pub fn new(
        instance: wgpu::Instance,
        surface: &wgpu::Surface<'_>,
        compositor_gpu: Option<CompositorGpuHint>,
    ) -> anyhow::Result<Self> {
        Self::new_with_options(instance, surface, compositor_gpu, false)
    }

    #[cfg(not(target_family = "wasm"))]
    pub fn new_rejecting_software(
        instance: wgpu::Instance,
        surface: &wgpu::Surface<'_>,
        compositor_gpu: Option<CompositorGpuHint>,
    ) -> anyhow::Result<Self> {
        Self::new_with_options(instance, surface, compositor_gpu, true)
    }

    #[cfg(not(target_family = "wasm"))]
    fn new_with_options(
        instance: wgpu::Instance,
        surface: &wgpu::Surface<'_>,
        compositor_gpu: Option<CompositorGpuHint>,
        reject_software: bool,
    ) -> anyhow::Result<Self> {
        let device_id_filter = match std::env::var("ZED_DEVICE_ID") {
            Ok(val) => parse_pci_id(&val)
                .context("Failed to parse device ID from `ZED_DEVICE_ID` environment variable")
                .log_err(),
            Err(std::env::VarError::NotPresent) => None,
            err => {
                err.context("Failed to read value of `ZED_DEVICE_ID` environment variable")
                    .log_err();
                None
            }
        };

        // Device enumeration and creation are slow on cold start (Mesa shader cache, driver init).
        // Never use gpui::block_on (pollster on the foreground thread) for that work — it freezes
        // the UI for tens of seconds during Workspace::open. Surface configuration stays on the
        // caller thread because the window surface is not portable across threads.
        let (adapter, device, queue, dual_source_blending, color_texture_format) =
            Self::select_adapter_and_device_off_foreground(
                &instance,
                device_id_filter,
                surface,
                compositor_gpu.as_ref(),
                reject_software,
            )?;

        let device_lost = Arc::new(AtomicBool::new(false));
        device.set_device_lost_callback({
            let device_lost = Arc::clone(&device_lost);
            move |reason, message| {
                log::error!("wgpu device lost: reason={reason:?}, message={message}");
                if reason != wgpu::DeviceLostReason::Destroyed {
                    device_lost.store(true, Ordering::Relaxed);
                }
            }
        });

        log::info!(
            "Selected GPU adapter: {:?} ({:?})",
            adapter.get_info().name,
            adapter.get_info().backend
        );

        Ok(Self {
            instance,
            adapter,
            device: Arc::new(device),
            queue: Arc::new(queue),
            dual_source_blending,
            color_texture_format,
            device_lost,
        })
    }

    #[cfg(target_family = "wasm")]
    pub async fn new_web() -> anyhow::Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::BROWSER_WEBGPU | wgpu::Backends::GL,
            flags: wgpu::InstanceFlags::default(),
            backend_options: wgpu::BackendOptions::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            display: None,
        });

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
            })
            .await
            .map_err(|e| anyhow::anyhow!("Failed to request GPU adapter: {e}"))?;

        log::info!(
            "Selected GPU adapter: {:?} ({:?})",
            adapter.get_info().name,
            adapter.get_info().backend
        );

        let device_lost = Arc::new(AtomicBool::new(false));
        let (device, queue, dual_source_blending, color_texture_format) =
            Self::create_device(&adapter).await?;

        Ok(Self {
            instance,
            adapter,
            device: Arc::new(device),
            queue: Arc::new(queue),
            dual_source_blending,
            color_texture_format,
            device_lost,
        })
    }

    async fn create_device(
        adapter: &wgpu::Adapter,
    ) -> anyhow::Result<(wgpu::Device, wgpu::Queue, bool, TextureFormat)> {
        let dual_source_blending = adapter
            .features()
            .contains(wgpu::Features::DUAL_SOURCE_BLENDING);

        let mut required_features = wgpu::Features::empty();
        if dual_source_blending {
            required_features |= wgpu::Features::DUAL_SOURCE_BLENDING;
        } else {
            log::warn!(
                "Dual-source blending not available on this GPU. \
                Subpixel text antialiasing will be disabled."
            );
        }

        let color_atlas_texture_format = Self::select_color_texture_format(adapter)?;

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("gpui_device"),
                required_features,
                required_limits: wgpu::Limits::downlevel_defaults()
                    .using_resolution(adapter.limits())
                    .using_alignment(adapter.limits()),
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                trace: wgpu::Trace::Off,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
            })
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create wgpu device: {e}"))?;

        Ok((
            device,
            queue,
            dual_source_blending,
            color_atlas_texture_format,
        ))
    }

    #[cfg(not(target_family = "wasm"))]
    pub fn instance(display: Box<dyn wgpu::wgt::WgpuHasDisplayHandle>) -> wgpu::Instance {
        wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            flags: wgpu::InstanceFlags::default(),
            backend_options: wgpu::BackendOptions::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            display: Some(display),
        })
    }

    pub fn check_compatible_with_surface(&self, surface: &wgpu::Surface<'_>) -> anyhow::Result<()> {
        let caps = surface.get_capabilities(&self.adapter);
        if caps.formats.is_empty() {
            let info = self.adapter.get_info();
            anyhow::bail!(
                "Adapter {:?} (backend={:?}, device={:#06x}) is not compatible with the \
                 display surface for this window.",
                info.name,
                info.backend,
                info.device,
            );
        }
        Ok(())
    }

    /// Runs a GPU-selection future on a dedicated worker thread so the foreground executor
    /// can keep pumping during slow driver initialization.
    #[cfg(not(target_family = "wasm"))]
    fn block_on_gpu_worker<R>(future: impl Future<Output = R> + Send + 'static) -> R
    where
        R: Send + 'static,
    {
        std::thread::spawn(move || pollster::block_on(future))
            .join()
            .expect("GPU worker thread panicked during adapter selection")
    }

    /// Adapter ranking tiers (lower sorts first):
    /// 1. ZED_DEVICE_ID match
    /// 2. Compositor GPU match
    /// 3. Device type (Discrete > Integrated > Other > Virtual > Cpu)
    /// 4. Backend (Vulkan/Metal/Dx12 before GL)
    #[cfg(not(target_family = "wasm"))]
    fn adapter_sort_key(
        info: &wgpu::AdapterInfo,
        device_id_filter: Option<u32>,
        compositor_gpu: Option<&CompositorGpuHint>,
    ) -> (u8, u8, u8, u8) {
        let device_known = info.device != 0;

        let user_override: u8 = match device_id_filter {
            Some(id) if device_known && info.device == id => 0,
            _ => 1,
        };

        let compositor_match: u8 = match compositor_gpu {
            Some(hint)
                if device_known
                    && info.vendor == hint.vendor_id
                    && info.device == hint.device_id =>
            {
                0
            }
            _ => 1,
        };

        let type_priority: u8 = if info.device_type == wgpu::DeviceType::Cpu {
            4
        } else {
            match info.device_type {
                wgpu::DeviceType::DiscreteGpu => 0,
                wgpu::DeviceType::IntegratedGpu => 1,
                wgpu::DeviceType::Other => 2,
                wgpu::DeviceType::VirtualGpu => 3,
                wgpu::DeviceType::Cpu => 4,
            }
        };

        let backend_priority: u8 = match info.backend {
            wgpu::Backend::Vulkan | wgpu::Backend::Metal | wgpu::Backend::Dx12 => 0,
            _ => 1,
        };

        (
            user_override,
            compositor_match,
            type_priority,
            backend_priority,
        )
    }

    #[cfg(not(target_family = "wasm"))]
    fn sort_adapters(
        adapters: &mut [wgpu::Adapter],
        device_id_filter: Option<u32>,
        compositor_gpu: Option<&CompositorGpuHint>,
    ) {
        adapters.sort_by_key(|adapter| {
            Self::adapter_sort_key(&adapter.get_info(), device_id_filter, compositor_gpu)
        });
    }

    #[cfg(not(target_family = "wasm"))]
    async fn enumerate_sorted_adapters(
        instance: &wgpu::Instance,
        device_id_filter: Option<u32>,
        compositor_gpu: Option<&CompositorGpuHint>,
    ) -> anyhow::Result<Vec<wgpu::Adapter>> {
        let mut adapters: Vec<_> = instance.enumerate_adapters(wgpu::Backends::all()).await;

        if adapters.is_empty() {
            anyhow::bail!("No GPU adapters found");
        }

        if let Some(device_id) = device_id_filter {
            log::info!("ZED_DEVICE_ID filter: {:#06x}", device_id);
        }

        Self::sort_adapters(&mut adapters, device_id_filter, compositor_gpu);

        log::info!("Found {} GPU adapter(s):", adapters.len());
        for adapter in &adapters {
            let info = adapter.get_info();
            log::info!(
                "  - {} (vendor={:#06x}, device={:#06x}, backend={:?}, type={:?})",
                info.name,
                info.vendor,
                info.device,
                info.backend,
                info.device_type,
            );
        }

        Ok(adapters)
    }

    /// Select an adapter and device off the foreground thread; validate surface on caller thread.
    #[cfg(not(target_family = "wasm"))]
    fn select_adapter_and_device_off_foreground(
        instance: &wgpu::Instance,
        device_id_filter: Option<u32>,
        surface: &wgpu::Surface<'_>,
        compositor_gpu: Option<&CompositorGpuHint>,
        reject_software: bool,
    ) -> anyhow::Result<(
        wgpu::Adapter,
        wgpu::Device,
        wgpu::Queue,
        bool,
        TextureFormat,
    )> {
        let instance = instance.clone();
        let compositor_gpu = compositor_gpu.copied();
        let adapters = Self::block_on_gpu_worker(async move {
            Self::enumerate_sorted_adapters(&instance, device_id_filter, compositor_gpu.as_ref())
                .await
        })?;

        for adapter in adapters {
            let info = adapter.get_info();

            if reject_software && info.device_type == wgpu::DeviceType::Cpu {
                log::info!(
                    "Skipping software renderer: {} ({:?})",
                    info.name,
                    info.backend
                );
                continue;
            }

            log::info!("Testing adapter: {} ({:?})...", info.name, info.backend);

            let adapter_for_device = adapter.clone();
            let device_bundle = match Self::block_on_gpu_worker(async move {
                Self::create_device(&adapter_for_device).await
            }) {
                Ok(bundle) => bundle,
                Err(error) => {
                    log::info!(
                        "  Adapter {} ({:?}) device creation failed: {error:#}, trying next...",
                        info.name,
                        info.backend
                    );
                    continue;
                }
            };

            match Self::validate_surface_with_device(surface, &adapter, device_bundle) {
                Ok(bundle) => {
                    log::info!(
                        "Selected GPU (passed configuration test): {} ({:?})",
                        info.name,
                        info.backend
                    );
                    return Ok((adapter, bundle.0, bundle.1, bundle.2, bundle.3));
                }
                Err(error) => {
                    log::info!(
                        "  Adapter {} ({:?}) failed: {error:#}, trying next...",
                        info.name,
                        info.backend
                    );
                }
            }
        }

        anyhow::bail!("No GPU adapter found that can configure the display surface")
    }

    /// Validate that an already-created device can configure the window surface.
    #[cfg(not(target_family = "wasm"))]
    fn validate_surface_with_device(
        surface: &wgpu::Surface<'_>,
        adapter: &wgpu::Adapter,
        (device, queue, dual_source_blending, color_atlas_texture_format): (
            wgpu::Device,
            wgpu::Queue,
            bool,
            TextureFormat,
        ),
    ) -> anyhow::Result<(wgpu::Device, wgpu::Queue, bool, TextureFormat)> {
        let caps = surface.get_capabilities(adapter);
        if caps.formats.is_empty() {
            anyhow::bail!("no compatible surface formats");
        }
        if caps.alpha_modes.is_empty() {
            anyhow::bail!("no compatible alpha modes");
        }

        let error_scope = device.push_error_scope(wgpu::ErrorFilter::Validation);

        let test_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: caps.formats[0],
            width: 64,
            height: 64,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };

        surface.configure(&device, &test_config);

        let error = pollster::block_on(error_scope.pop());
        if let Some(error) = error {
            anyhow::bail!("surface configuration failed: {error}");
        }

        Ok((
            device,
            queue,
            dual_source_blending,
            color_atlas_texture_format,
        ))
    }

    fn select_color_texture_format(adapter: &wgpu::Adapter) -> anyhow::Result<wgpu::TextureFormat> {
        let required_usages = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST;
        let bgra_features = adapter.get_texture_format_features(wgpu::TextureFormat::Bgra8Unorm);
        if bgra_features.allowed_usages.contains(required_usages) {
            return Ok(wgpu::TextureFormat::Bgra8Unorm);
        }

        let rgba_features = adapter.get_texture_format_features(wgpu::TextureFormat::Rgba8Unorm);
        if rgba_features.allowed_usages.contains(required_usages) {
            let info = adapter.get_info();
            log::warn!(
                "Adapter {} ({:?}) does not support Bgra8Unorm atlas textures with usages {:?}; \
                 falling back to Rgba8Unorm atlas textures.",
                info.name,
                info.backend,
                required_usages,
            );
            return Ok(wgpu::TextureFormat::Rgba8Unorm);
        }

        let info = adapter.get_info();
        Err(anyhow::anyhow!(
            "Adapter {} ({:?}, device={:#06x}) does not support a usable color atlas texture \
             format with usages {:?}. Bgra8Unorm allowed usages: {:?}; \
             Rgba8Unorm allowed usages: {:?}.",
            info.name,
            info.backend,
            info.device,
            required_usages,
            bgra_features.allowed_usages,
            rgba_features.allowed_usages,
        ))
    }
    pub fn supports_dual_source_blending(&self) -> bool {
        self.dual_source_blending
    }

    pub fn color_texture_format(&self) -> wgpu::TextureFormat {
        self.color_texture_format
    }

    /// Returns true if the GPU device was lost (e.g., due to driver crash, suspend/resume).
    /// When this returns true, the context should be recreated.
    pub fn device_lost(&self) -> bool {
        self.device_lost.load(Ordering::Relaxed)
    }

    /// Returns a clone of the device_lost flag for sharing with renderers.
    pub(crate) fn device_lost_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.device_lost)
    }
}

#[cfg(not(target_family = "wasm"))]
fn parse_pci_id(id: &str) -> anyhow::Result<u32> {
    let mut id = id.trim();

    if id.starts_with("0x") || id.starts_with("0X") {
        id = &id[2..];
    }
    let is_hex_string = id.chars().all(|c| c.is_ascii_hexdigit());
    let is_4_chars = id.len() == 4;
    anyhow::ensure!(
        is_4_chars && is_hex_string,
        "Expected a 4 digit PCI ID in hexadecimal format"
    );

    u32::from_str_radix(id, 16).context("parsing PCI ID as hex")
}

#[cfg(test)]
mod tests {
    use super::{CompositorGpuHint, WgpuContext, parse_pci_id};
    use std::fs;
    use std::path::Path;
    use wgpu::Backend;

    #[test]
    fn wgpu_context_does_not_block_foreground_with_gpui_block_on() {
        let forbidden = concat!("gpui", "::block_on");
        let source =
            fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/wgpu_context.rs"))
                .expect("read wgpu_context.rs");
        assert!(
            !source.lines().any(|line| {
                let trimmed = line.split("//").next().unwrap_or(line).trim();
                trimmed.contains(forbidden)
            }),
            "WgpuContext must not call gpui block_on on the foreground thread; use block_on_gpu_worker"
        );
    }

    #[test]
    fn adapter_sort_key_prefers_compositor_gpu_match() {
        let compositor = CompositorGpuHint {
            vendor_id: 0x1002,
            device_id: 0x1114,
        };
        let compositor_match = wgpu::AdapterInfo {
            name: "Compositor GPU".into(),
            vendor: 0x1002,
            device: 0x1114,
            device_type: wgpu::DeviceType::IntegratedGpu,
            backend: Backend::Vulkan,
            driver: "".into(),
            driver_info: "".into(),
            device_pci_bus_id: "".into(),
            subgroup_min_size: 0,
            subgroup_max_size: 0,
            transient_saves_memory: false,
        };
        let other_gpu = wgpu::AdapterInfo {
            name: "Other GPU".into(),
            vendor: 0x1002,
            device: 0x2222,
            device_type: wgpu::DeviceType::DiscreteGpu,
            backend: Backend::Vulkan,
            driver: "".into(),
            driver_info: "".into(),
            device_pci_bus_id: "".into(),
            subgroup_min_size: 0,
            subgroup_max_size: 0,
            transient_saves_memory: false,
        };

        let compositor_key =
            WgpuContext::adapter_sort_key(&compositor_match, None, Some(&compositor));
        let other_key = WgpuContext::adapter_sort_key(&other_gpu, None, Some(&compositor));
        assert!(
            compositor_key < other_key,
            "compositor-matched adapter must sort before other adapters (compositor={compositor_key:?}, other={other_key:?})"
        );
    }

    #[test]
    fn test_parse_device_id() {
        assert!(parse_pci_id("0xABCD").is_ok());
        assert!(parse_pci_id("ABCD").is_ok());
        assert!(parse_pci_id("abcd").is_ok());
        assert!(parse_pci_id("1234").is_ok());
        assert!(parse_pci_id("123").is_err());
        assert_eq!(
            parse_pci_id(&format!("{:x}", 0x1234)).unwrap(),
            parse_pci_id(&format!("{:X}", 0x1234)).unwrap(),
        );

        assert_eq!(
            parse_pci_id(&format!("{:#x}", 0x1234)).unwrap(),
            parse_pci_id(&format!("{:#X}", 0x1234)).unwrap(),
        );
    }
}
