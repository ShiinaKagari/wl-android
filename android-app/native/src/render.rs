//! Vulkan swapchain renderer for the Android App (P3, TODO 26).
//!
//! The App presents compositor-rendered buffers through a Vulkan swapchain on
//! the SurfaceView's `ANativeWindow`, using only public API:
//! `VK_KHR_android_surface` + `VK_KHR_swapchain` (D3 — no dlsym, no hidden
//! APIs, no ASurfaceTransaction). The Vulkan device is the Android *host*
//! driver (Qualcomm proprietary), not the container's turnip.
//!
//! Swapchain images are gralloc-backed AHardwareBuffers; lane 27 extracts the
//! AHB per image (`VK_ANDROID_external_memory_android_hardware_buffer`) and
//! ships the fds to the server, lane 29 imports the server's SYNC_FD fence as
//! a semaphore and passes it to [`RenderState::present`] as a wait semaphore,
//! lane 30 wires the frame loop. This lane provides the swapchain plumbing
//! and leaves the fence seam open.
//!
//! AHB export requires `VK_SWAPCHAIN_CREATE_DEFERRED_MEMORY_ALLOCATION_BIT`
//! on the swapchain (the swapchain then ships images with no backing store);
//! this module allocates one dedicated, AHB-exportable `VkDeviceMemory` per
//! image and hands it out via [`RenderState::image_memory`], which is what
//! `AhbSlot::from_swapchain_image` exports.
//!
//! Thread-safety: `RenderState` lives behind `Arc<Mutex<Inner>>` in lib.rs;
//! all mutating entry points take `&mut self`, so the mutex in lib.rs is the
//! synchronization point. The struct contains no raw pointers (the
//! `ANativeWindow` is only borrowed during `init`), so it stays `Send`.

use std::ffi::c_void;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};

use ash::vk;

/// Preferred swapchain format: B8G8R8A8 is the de-facto Android compositor
/// format (gralloc maps it to AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM-class
/// buffers on Adreno). Fallback R8G8B8A8_UNORM matches lane 23's server blit
/// source (ABGR8888 == VK_FORMAT_R8G8B8A8_UNORM) exactly.
pub(crate) const PREFERRED_FORMATS: [vk::Format; 2] = [
    vk::Format::B8G8R8A8_UNORM,
    vk::Format::R8G8B8A8_UNORM,
];

/// Choose the swapchain surface format.
///
/// Preference order: B8G8R8A8_UNORM + SRGB_NONLINEAR, then R8G8B8A8_UNORM +
/// SRGB_NONLINEAR, then the first advertised format. A list whose only entry
/// is `FORMAT_UNDEFINED` (or an empty list, defensively) means "the driver
/// imposes no constraint", in which case we pick our first preference.
pub(crate) fn pick_format(formats: &[vk::SurfaceFormatKHR]) -> vk::SurfaceFormatKHR {
    let unconstrained = formats.is_empty()
        || (formats.len() == 1 && formats[0].format == vk::Format::UNDEFINED);
    if unconstrained {
        return vk::SurfaceFormatKHR {
            format: PREFERRED_FORMATS[0],
            color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
        };
    }
    for wanted in PREFERRED_FORMATS {
        if let Some(f) = formats
            .iter()
            .find(|f| f.format == wanted && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR)
        {
            return *f;
        }
    }
    // Accept a preferred format with any color space before giving up.
    for wanted in PREFERRED_FORMATS {
        if let Some(f) = formats.iter().find(|f| f.format == wanted) {
            return *f;
        }
    }
    formats[0]
}

/// PERF-15: MAILBOX (low-latency triple buffering, never blocks the producer)
/// when available; FIFO is guaranteed by the spec to always be present.
pub(crate) fn pick_present_mode(modes: &[vk::PresentModeKHR]) -> vk::PresentModeKHR {
    if modes.contains(&vk::PresentModeKHR::MAILBOX) {
        vk::PresentModeKHR::MAILBOX
    } else {
        vk::PresentModeKHR::FIFO
    }
}

/// Resolve the swapchain extent. Drivers may report `current_extent.width ==
/// u32::MAX` to mean "the app chooses within [min,max]" (Wayland-style);
/// Android SurfaceFlinger surfaces always report a real current extent, but
/// handle both per the spec.
pub(crate) fn clamp_extent(caps: &vk::SurfaceCapabilitiesKHR) -> vk::Extent2D {
    if caps.current_extent.width != u32::MAX {
        caps.current_extent
    } else {
        vk::Extent2D {
            width: caps
                .current_extent
                .width
                .clamp(caps.min_image_extent.width, caps.max_image_extent.width),
            height: caps
                .current_extent
                .height
                .clamp(caps.min_image_extent.height, caps.max_image_extent.height),
        }
    }
}

/// Triple-buffer target (PERF-15 with MAILBOX), clamped to what the surface
/// allows. `max_image_count == 0` means unlimited.
pub(crate) fn pick_image_count(caps: &vk::SurfaceCapabilitiesKHR) -> u32 {
    let max = if caps.max_image_count == 0 {
        u32::MAX
    } else {
        caps.max_image_count
    };
    3.clamp(caps.min_image_count, max)
}

/// Composite alpha: prefer OPAQUE (no blending against the wallpaper, the
/// common Android SurfaceView case), else INHERIT, else the first advertised
/// bit.
pub(crate) fn pick_composite_alpha(
    supported: vk::CompositeAlphaFlagsKHR,
) -> vk::CompositeAlphaFlagsKHR {
    for wanted in [
        vk::CompositeAlphaFlagsKHR::OPAQUE,
        vk::CompositeAlphaFlagsKHR::INHERIT,
    ] {
        if supported.contains(wanted) {
            return wanted;
        }
    }
    // First set bit, or OPAQUE if the driver reported nothing (invalid but
    // seen in the wild on old Adreno blobs).
    if supported.is_empty() {
        vk::CompositeAlphaFlagsKHR::OPAQUE
    } else {
        vk::CompositeAlphaFlagsKHR::from_raw(
            supported.as_raw() & supported.as_raw().wrapping_neg(),
        )
    }
}

/// Negotiate swapchain image usage. COLOR_ATTACHMENT is mandatory (the images
/// are render targets / blit destinations); TRANSFER_DST is required for the
/// lane 29 server blit path (vkCmdBlitImage/vkCmdCopyImage into the acquired
/// image). Intersect with what the surface supports and report the result —
/// if COLOR_ATTACHMENT itself is unsupported the surface is unusable and the
/// caller must error out.
pub(crate) fn pick_image_usage(supported: vk::ImageUsageFlags) -> vk::ImageUsageFlags {
    const WANTED: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
        vk::ImageUsageFlags::COLOR_ATTACHMENT.as_raw()
            | vk::ImageUsageFlags::TRANSFER_DST.as_raw(),
    );
    supported & WANTED
}

/// Vulkan swapchain rendering state. `RenderState::new()` is cheap and does
/// no Vulkan work; [`RenderState::init`] builds the full chain.
pub struct RenderState {
    pub initialized: bool,
    entry: Option<ash::Entry>,
    instance: Option<ash::Instance>,
    surface_loader: Option<ash::khr::surface::Instance>,
    android_surface_loader: Option<ash::khr::android_surface::Instance>,
    surface: vk::SurfaceKHR,
    physical_device: vk::PhysicalDevice,
    device: Option<ash::Device>,
    swapchain_loader: Option<ash::khr::swapchain::Device>,
    swapchain: vk::SwapchainKHR,
    /// VK_KHR_external_semaphore_fd loader (SYNC_FD fence import, F-12). None
    /// when the host driver lacks the extension — the fence path then falls
    /// back to the CPU poll ([`RenderState::wait_sync_fd`]).
    semaphore_fd_loader: Option<ash::khr::external_semaphore_fd::Device>,
    queue: vk::Queue,
    queue_family_index: u32,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    /// Imported AHB images (route-1 slots): opaque handle -> VkImage. The
    /// App allocates LINEAR AHardwareBuffers (route 1) so the server's turnip
    /// can import them without crashing; each frame we GPU-blit the AHB into
    /// the acquired swapchain image before presenting.
    ahb_images: std::collections::HashMap<u32, vk::Image>,
    /// AHB image backing memory (freed in Drop alongside the image).
    ahb_memories: std::collections::HashMap<u32, vk::DeviceMemory>,
    images: Vec<vk::Image>,
    image_memories: Vec<vk::DeviceMemory>,
    image_format: vk::Format,
    color_space: vk::ColorSpaceKHR,
    extent: vk::Extent2D,
    present_mode: vk::PresentModeKHR,
}

impl RenderState {
    pub fn new() -> Self {
        Self {
            initialized: false,
            entry: None,
            instance: None,
            surface_loader: None,
            android_surface_loader: None,
            surface: vk::SurfaceKHR::null(),
            physical_device: vk::PhysicalDevice::null(),
            device: None,
            swapchain_loader: None,
            semaphore_fd_loader: None,
            swapchain: vk::SwapchainKHR::null(),
            queue: vk::Queue::null(),
            queue_family_index: 0,
            command_pool: vk::CommandPool::null(),
            command_buffer: vk::CommandBuffer::null(),
            ahb_images: std::collections::HashMap::new(),
            ahb_memories: std::collections::HashMap::new(),
            images: Vec::new(),
            image_memories: Vec::new(),
            image_format: vk::Format::UNDEFINED,
            color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
            extent: vk::Extent2D { width: 0, height: 0 },
            present_mode: vk::PresentModeKHR::FIFO,
        }
    }

    /// Build the full Vulkan chain on `window` (an `ANativeWindow*` obtained
    /// from the C bridge / `jni_bridge::window_ptr`). The window is borrowed
    /// for surface creation only; ownership stays with jni_bridge (which
    /// acquired it). Recreating the surface on rotation is the caller's job:
    /// drop this state, call `init` again with the new window.
    pub fn init(&mut self, window: *mut c_void) -> Result<(), String> {
        if self.initialized {
            return Err("render: init called twice".into());
        }
        if window.is_null() {
            return Err("render: null window".into());
        }

        // --- Instance: VK_KHR_surface + VK_KHR_android_surface ---
        let entry = unsafe { ash::Entry::load() }
            .map_err(|e| format!("render: vkEnumerateInstanceVersion/libvulkan load: {e}"))?;
        let app_info = vk::ApplicationInfo::default()
            .application_name(c"wl-android")
            .engine_name(c"wl-android")
            .api_version(vk::API_VERSION_1_1);
        let instance_exts = [
            ash::khr::surface::NAME.as_ptr(),
            ash::khr::android_surface::NAME.as_ptr(),
        ];
        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default()
                    .application_info(&app_info)
                    .enabled_extension_names(&instance_exts),
                None,
            )
        }
        .map_err(|e| format!("render: vkCreateInstance: {e}"))?;

        // --- Surface on the SurfaceView's ANativeWindow ---
        let android_surface_loader = ash::khr::android_surface::Instance::new(&entry, &instance);
        let surface = unsafe {
            android_surface_loader.create_android_surface(
                &vk::AndroidSurfaceCreateInfoKHR::default().window(window as *mut vk::ANativeWindow),
                None,
            )
        }
        .map_err(|e| format!("render: vkCreateAndroidSurfaceKHR: {e}"))?;
        let surface_loader = ash::khr::surface::Instance::new(&entry, &instance);

        // --- Physical device: first GPU with a GRAPHICS+present queue family ---
        let pdevices = unsafe { instance.enumerate_physical_devices() }
            .map_err(|e| format!("render: vkEnumeratePhysicalDevices: {e}"))?;
        let mut picked = None;
        'outer: for pd in pdevices {
            let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
            for (i, qf) in families.iter().enumerate() {
                let present_ok = unsafe {
                    surface_loader.get_physical_device_surface_support(pd, i as u32, surface)
                }
                .unwrap_or(false);
                if qf.queue_flags.contains(vk::QueueFlags::GRAPHICS) && present_ok {
                    picked = Some((pd, i as u32));
                    break 'outer;
                }
            }
        }
        let (physical_device, queue_family_index) =
            picked.ok_or_else(|| "render: no GRAPHICS+present queue family".to_string())?;
        let pd_props = unsafe { instance.get_physical_device_properties(physical_device) };
        let pd_name = pd_props
            .device_name_as_c_str()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "<unknown>".into());
        log::info!("render: physical device = {pd_name}, queue family {queue_family_index}");

        // --- Device: VK_KHR_swapchain (+ optional VK_KHR_external_semaphore_fd) ---
        let priorities = [1.0f32];
        let queue_infos = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family_index)
            .queue_priorities(&priorities)];
        // Probe for VK_KHR_external_semaphore_fd (SYNC_FD semaphore import for
        // the lane 29 fence path). Expected on the Qualcomm host driver; when
        // absent we degrade to the CPU-poll fallback (wait_sync_fd) instead of
        // failing init.
        let ext_props = unsafe { instance.enumerate_device_extension_properties(physical_device) }
            .unwrap_or_default();
        let has_fd_ext = ext_props.iter().any(|e| {
            e.extension_name_as_c_str()
                .map(|c| c == ash::khr::external_semaphore_fd::NAME)
                .unwrap_or(false)
        });
        let mut device_exts = vec![
            ash::khr::swapchain::NAME.as_ptr(),
            ash::android::external_memory_android_hardware_buffer::NAME.as_ptr(),
        ];
        if has_fd_ext {
            device_exts.push(ash::khr::external_semaphore_fd::NAME.as_ptr());
            log::info!("render: VK_KHR_external_semaphore_fd present (SYNC_FD fence import)");
        } else {
            log::warn!(
                "render: VK_KHR_external_semaphore_fd MISSING — fence frames fall back to wait_sync_fd CPU poll"
            );
        }
        let device = unsafe {
            instance.create_device(
                physical_device,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&queue_infos)
                    .enabled_extension_names(&device_exts),
                None,
            )
        }
        .map_err(|e| format!("render: vkCreateDevice: {e}"))?;
        let queue = unsafe { device.get_device_queue(queue_family_index, 0) };
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(queue_family_index)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let command_pool = unsafe { device.create_command_pool(&pool_info, None) }
            .map_err(|e| format!("render: vkCreateCommandPool: {e}"))?;
        let cmd_alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .command_buffer_count(1);
        let command_buffers = unsafe { device.allocate_command_buffers(&cmd_alloc) }
            .map_err(|e| format!("render: vkAllocateCommandBuffers: {e}"))?;
        let swapchain_loader = ash::khr::swapchain::Device::new(&instance, &device);
        let semaphore_fd_loader = if has_fd_ext {
            Some(ash::khr::external_semaphore_fd::Device::new(&instance, &device))
        } else {
            None
        };

        self.entry = Some(entry);
        self.instance = Some(instance);
        self.surface_loader = Some(surface_loader);
        self.android_surface_loader = Some(android_surface_loader);
        self.surface = surface;
        self.physical_device = physical_device;
        self.device = Some(device);
        self.swapchain_loader = Some(swapchain_loader);
        self.semaphore_fd_loader = semaphore_fd_loader;
        self.queue = queue;
        self.queue_family_index = queue_family_index;
        self.command_pool = command_pool;
        self.command_buffer = command_buffers[0];

        // --- Swapchain (initial extent comes from the surface caps) ---
        self.create_swapchain_and_images()?;
        self.initialized = true;
        log::info!(
            "render: initialized — format={:?} extent={}x{} images={} mode={:?}",
            self.image_format,
            self.extent.width,
            self.extent.height,
            self.images.len(),
            self.present_mode,
        );
        Ok(())
    }

    /// Query the surface and (re)create the swapchain, then re-fetch its
    /// images. Called by `init` and by `recreate_swapchain`; any existing
    /// swapchain must already be destroyed.
    fn create_swapchain_and_images(&mut self) -> Result<(), String> {
        let surface_loader = self.surface_loader.as_ref().expect("surface_loader set");
        let swapchain_loader = self.swapchain_loader.as_ref().expect("swapchain_loader set");

        let caps = unsafe {
            surface_loader.get_physical_device_surface_capabilities(
                self.physical_device,
                self.surface,
            )
        }
        .map_err(|e| format!("render: vkGetPhysicalDeviceSurfaceCapabilitiesKHR: {e}"))?;
        let formats = unsafe {
            surface_loader
                .get_physical_device_surface_formats(self.physical_device, self.surface)
        }
        .map_err(|e| format!("render: vkGetPhysicalDeviceSurfaceFormatsKHR: {e}"))?;
        let modes = unsafe {
            surface_loader
                .get_physical_device_surface_present_modes(self.physical_device, self.surface)
        }
        .map_err(|e| format!("render: vkGetPhysicalDeviceSurfacePresentModesKHR: {e}"))?;

        let format = pick_format(&formats);
        let present_mode = pick_present_mode(&modes);
        let extent = clamp_extent(&caps);
        let image_count = pick_image_count(&caps);
        let usage = pick_image_usage(caps.supported_usage_flags);
        if extent.width == 0 || extent.height == 0 {
            return Err(format!(
                "render: zero surface extent {extent:?} (surface minimized?)"
            ));
        }
        if !usage.contains(vk::ImageUsageFlags::COLOR_ATTACHMENT) {
            return Err(format!(
                "render: surface supports no COLOR_ATTACHMENT usage (supported={:?})",
                caps.supported_usage_flags
            ));
        }
        if !usage.contains(vk::ImageUsageFlags::TRANSFER_DST) {
            // Lane 29's server blit needs TRANSFER_DST; degrade loudly, don't
            // silently break the blit.
            log::warn!(
                "render: TRANSFER_DST not in supported_usage_flags={:?}; server blit into swapchain images will fail",
                caps.supported_usage_flags
            );
        }
        let composite_alpha = pick_composite_alpha(caps.supported_composite_alpha);
        if !caps.current_transform.is_empty()
            && caps.current_transform != vk::SurfaceTransformFlagsKHR::IDENTITY
        {
            log::info!(
                "render: pre_transform={:?} (non-identity, surface is rotated)",
                caps.current_transform
            );
        }

        let swapchain = unsafe {
            swapchain_loader.create_swapchain(
                &vk::SwapchainCreateInfoKHR::default()
                    .flags(vk::SwapchainCreateFlagsKHR::DEFERRED_MEMORY_ALLOCATION_EXT)
                    .surface(self.surface)
                    .min_image_count(image_count)
                    .image_format(format.format)
                    .image_color_space(format.color_space)
                    .image_extent(extent)
                    .image_array_layers(1)
                    .image_usage(usage)
                    .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .pre_transform(caps.current_transform)
                    .composite_alpha(composite_alpha)
                    .present_mode(present_mode)
                    .clipped(true),
                None,
            )
        }
        .map_err(|e| format!("render: vkCreateSwapchainKHR: {e}"))?;
        let images = unsafe { swapchain_loader.get_swapchain_images(swapchain) }
            .map_err(|e| format!("render: vkGetSwapchainImagesKHR: {e}"))?;
        // Deferred-allocation swapchain: the driver created the images with no
        // backing store, so allocate+bind one dedicated, AHB-exportable
        // DEVICE_LOCAL memory per image (lane 27 exports these via
        // vkGetMemoryAndroidHardwareBufferANDROID).
        //
        // Acquire-then-bind dance (real-device blocker on UMA Adreno 830):
        // under VK_SWAPCHAIN_CREATE_DEFERRED_MEMORY_ALLOCATION_BIT an image
        // that has NEVER been acquired has no backing store, so
        // vkGetImageMemoryRequirements reports `memory_type_bits == 0`. Each
        // image is therefore acquired once (fence-signalled), its requirements
        // are THEN queried, memory is allocated + bound, and once every image
        // is bound they are all presented back to the presentation engine —
        // otherwise the frame loop's first acquire would block forever (every
        // image still in the "acquired by the app" state).
        let device = self.device.as_ref().expect("device set");
        let instance = self.instance.as_ref().expect("instance set");
        let mem_props =
            unsafe { instance.get_physical_device_memory_properties(self.physical_device) };
        let mut image_memories = vec![vk::DeviceMemory::null(); images.len()];
        let alloc = (|| -> Result<(), String> {
            // Plain (non-exportable) internal fence; created + destroyed
            // locally, it only lives for this init sequence. The dance needs
            // no semaphore: the fence is the (spec-required) non-null signal.
            let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }
                .map_err(|e| format!("render: vkCreateFence (init dance): {e}"))?;
            let dance = (|| -> Result<(), String> {
                let mut acquired: Vec<u32> = Vec::with_capacity(images.len());
                // Acquire exactly once per actual image; on a fresh swapchain
                // every image is available, so images.len() acquires yields
                // each image exactly once (any further acquire would block).
                for _ in 0..images.len() {
                    let (idx, suboptimal) = unsafe {
                        swapchain_loader.acquire_next_image(
                            swapchain,
                            u64::MAX,
                            vk::Semaphore::null(),
                            fence,
                        )
                    }
                    .map_err(|e| format!("render: vkAcquireNextImageKHR (init dance): {e}"))?;
                    if suboptimal {
                        log::warn!("render: init dance acquire slot={idx} SUBOPTIMAL");
                    }
                    unsafe { device.wait_for_fences(&[fence], true, u64::MAX) }
                        .map_err(|e| format!("render: vkWaitForFences (init dance): {e}"))?;
                    unsafe { device.reset_fences(&[fence]) }
                        .map_err(|e| format!("render: vkResetFences (init dance): {e}"))?;
                    acquired.push(idx);

                    // Post-acquire the image has a backing store, so the
                    // requirements are now meaningful.
                    let image = images[idx as usize];
                    let reqs = unsafe { device.get_image_memory_requirements(image) };
                    if reqs.memory_type_bits == 0 {
                        // Defensive: post-acquire this should never happen; do
                        // NOT silently proceed with a type-less allocation.
                        return Err(format!(
                            "render: vkGetImageMemoryRequirements image {idx} returned \
                             memory_type_bits=0 even after acquire (swapchain backing missing)"
                        ));
                    }
                    let mem_type_index = mem_props
                        .memory_types
                        .iter()
                        .take(mem_props.memory_type_count as usize)
                        .enumerate()
                        .find(|(mt_idx, mt)| {
                            (reqs.memory_type_bits & (1 << mt_idx)) != 0
                                && mt.property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
                        })
                        .map(|(mt_idx, _)| mt_idx as u32)
                        .ok_or_else(|| {
                            format!("render: no DEVICE_LOCAL memory type for image {idx}")
                        })?;
                    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
                    let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(
                        vk::ExternalMemoryHandleTypeFlags::ANDROID_HARDWARE_BUFFER_ANDROID,
                    );
                    let alloc_info = vk::MemoryAllocateInfo::default()
                        .allocation_size(reqs.size)
                        .memory_type_index(mem_type_index)
                        .push_next(&mut export)
                        .push_next(&mut dedicated);
                    let mem = unsafe { device.allocate_memory(&alloc_info, None) }
                        .map_err(|e| format!("render: vkAllocateMemory image {idx}: {e}"))?;
                    if let Err(e) = unsafe { device.bind_image_memory(image, mem, 0) } {
                        unsafe { device.free_memory(mem, None) };
                        return Err(format!("render: vkBindImageMemory image {idx}: {e}"));
                    }
                    image_memories[idx as usize] = mem;
                }

                // Present every image back so it returns to the presentation
                // engine's pool. Nothing was rendered into these images, so
                // there are no wait semaphores (waitSemaphoreCount=0);
                // per-swapchain results are captured in `present_results`.
                // OUT_OF_DATE/SUBOPTIMAL on a just-created swapchain is
                // unusual — log and continue rather than fail init.
                let mut present_results = [vk::Result::SUCCESS; 1];
                for &idx in &acquired {
                    let swapchains = [swapchain];
                    let indices = [idx];
                    let present_info = vk::PresentInfoKHR::default()
                        .swapchains(&swapchains)
                        .image_indices(&indices)
                        .results(&mut present_results);
                    match unsafe { swapchain_loader.queue_present(self.queue, &present_info) } {
                        Ok(suboptimal) => {
                            if suboptimal {
                                log::warn!("render: init dance present slot={idx} SUBOPTIMAL");
                            }
                        }
                        Err(e) => log::warn!(
                            "render: init dance present slot={idx} failed: {e} (image left acquired)"
                        ),
                    }
                }
                Ok(())
            })();
            let _ = unsafe { device.destroy_fence(fence, None) };
            dance
        })();
        if let Err(e) = alloc {
            for &mem in &image_memories {
                if mem != vk::DeviceMemory::null() {
                    unsafe { device.free_memory(mem, None) };
                }
            }
            unsafe { swapchain_loader.destroy_swapchain(swapchain, None) };
            return Err(e);
        }

        log::info!(
            "render: swapchain {:?} — {} images (min={} max={}), format={:?} colorspace={:?} (advertised {} formats), extent={}x{}, mode={:?} (advertised {} modes), usage={usage:?}, alpha={composite_alpha:?}",
            swapchain,
            images.len(),
            caps.min_image_count,
            caps.max_image_count,
            format.format,
            format.color_space,
            formats.len(),
            extent.width,
            extent.height,
            present_mode,
            modes.len(),
        );

        self.swapchain = swapchain;
        self.images = images;
        self.image_memories = image_memories;
        self.image_format = format.format;
        self.color_space = format.color_space;
        self.extent = extent;
        self.present_mode = present_mode;
        Ok(())
    }

    /// Acquire the next swapchain image slot. `timeout_ns` follows
    /// vkAcquireNextImageKHR semantics (`u64::MAX` = block). An optional
    /// acquire semaphore/fence may be signalled by the driver.
    ///
    /// Handles `OUT_OF_DATE` (rotation/resize, M5) by recreating the
    /// swapchain and retrying once; `SUBOPTIMAL` still yields a usable slot
    /// (the recreate happens at the next `present`).
    pub fn acquire_next_image(
        &mut self,
        timeout_ns: u64,
        semaphore: Option<vk::Semaphore>,
        fence: Option<vk::Fence>,
    ) -> Result<u32, String> {
        if !self.initialized {
            return Err("render: acquire before init".into());
        }
        let sem = semaphore.unwrap_or_default();
        let fen = fence.unwrap_or_default();
        let first_attempt = {
            let loader = self.swapchain_loader.as_ref().expect("swapchain_loader set");
            unsafe { loader.acquire_next_image(self.swapchain, timeout_ns, sem, fen) }
        };
        match first_attempt {
            Ok((index, suboptimal)) => {
                if suboptimal {
                    log::warn!("render: acquire slot={index} SUBOPTIMAL (recreate at next present)");
                }
                Ok(index)
            }
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                log::info!("render: acquire OUT_OF_DATE — recreating swapchain");
                self.recreate_swapchain()?;
                let loader = self.swapchain_loader.as_ref().expect("swapchain_loader set");
                let (index, _) =
                    unsafe { loader.acquire_next_image(self.swapchain, timeout_ns, sem, fen) }
                        .map_err(|e| format!("render: acquire after recreate: {e}"))?;
                Ok(index)
            }
            Err(e) => Err(format!("render: vkAcquireNextImageKHR: {e}")),
        }
    }

    /// Present an acquired slot. `wait_semaphores` is the lane 29 seam: the
    /// server's SYNC_FD fence imported as a `VkSemaphore` goes here, and the
    /// swapchain waits on it before the image is handed to SurfaceFlinger.
    /// Empty slice = no wait (CPU/blit already finished).
    ///
    /// `OUT_OF_DATE`/`SUBOPTIMAL` trigger a swapchain recreate; the frame is
    /// still counted as presented (the compositor will repaint next frame).
    pub fn present(
        &mut self,
        image_index: u32,
        wait_semaphores: &[vk::Semaphore],
    ) -> Result<(), String> {
        if !self.initialized {
            return Err("render: present before init".into());
        }
        let loader = self.swapchain_loader.as_ref().expect("swapchain_loader set");
        let swapchains = [self.swapchain];
        let indices = [image_index];
        let present_info = vk::PresentInfoKHR::default()
            .wait_semaphores(wait_semaphores)
            .swapchains(&swapchains)
            .image_indices(&indices);
        let outcome = unsafe { loader.queue_present(self.queue, &present_info) };
        log::info!("present: slot={image_index}");
        match outcome {
            Ok(suboptimal) => {
                if suboptimal {
                    log::info!("render: present SUBOPTIMAL — recreating swapchain");
                    self.recreate_swapchain()?;
                }
                Ok(())
            }
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                log::info!("render: present OUT_OF_DATE — recreating swapchain");
                self.recreate_swapchain()?;
                Ok(())
            }
            Err(e) => Err(format!("render: vkQueuePresentKHR: {e}")),
        }
    }

    /// Import the server's sync_file blit fence (FRAME_CARRIES_FENCE) as a
    /// temporary `VkSemaphore` the swapchain can wait on before presenting.
    ///
    /// VK_KHR_external_semaphore_fd SYNC_FD import: create an un-signaled
    /// binary semaphore, then `vkImportSemaphoreFdKHR` the fd into it. On
    /// success the semaphore carries the fence payload and the DRIVER owns the
    /// fd (F-12); the caller must destroy the semaphore via
    /// [`RenderState::destroy_semaphore`] after presenting.
    ///
    /// Failure modes — the caller must fall back to
    /// [`RenderState::wait_sync_fd`] + `present` with an empty wait list:
    /// * the extension was unavailable at init → Err;
    /// * the driver rejects the import → Err; the temp semaphore is destroyed
    ///   here and the fd is NOT consumed (ownership stays with the caller).
    pub fn import_sync_fd_as_semaphore(&mut self, fd: &OwnedFd) -> Result<vk::Semaphore, String> {
        let device = self.device.as_ref().ok_or("render: import before init")?;
        let loader = self.semaphore_fd_loader.as_ref().ok_or(
            "render: VK_KHR_external_semaphore_fd unavailable — use wait_sync_fd fallback",
        )?;
        let sem = unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }
            .map_err(|e| format!("render: vkCreateSemaphore (fence import): {e}"))?;
        let import_info = vk::ImportSemaphoreFdInfoKHR::default()
            .semaphore(sem)
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD)
            .fd(fd.as_raw_fd());
        if let Err(e) = unsafe { loader.import_semaphore_fd(&import_info) } {
            unsafe { device.destroy_semaphore(sem, None) };
            return Err(format!("render: vkImportSemaphoreFdKHR(SYNC_FD): {e}"));
        }
        Ok(sem)
    }

    /// Destroy a semaphore created by
    /// [`RenderState::import_sync_fd_as_semaphore`]. Must be called after
    /// `present` has consumed the wait (the import was temporary).
    pub fn destroy_semaphore(&self, sem: vk::Semaphore) {
        if let Some(device) = self.device.as_ref() {
            unsafe { device.destroy_semaphore(sem, None) };
        }
    }

    /// CPU fallback when SYNC_FD semaphore import is unavailable or fails:
    /// poll the sync_file fd until the server's blit fence signals, then
    /// present with no wait semaphore (the blit is known complete).
    ///
    /// Returns `Ok(true)` if the fence signaled within `timeout_ms`,
    /// `Ok(false)` on timeout.
    pub fn wait_sync_fd(&self, fd: &OwnedFd, timeout_ms: u32) -> Result<bool, String> {
        let mut pfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout = timeout_ms.min(i32::MAX as u32) as libc::c_int;
        loop {
            let rc = unsafe { libc::poll(&mut pfd, 1, timeout) };
            if rc < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(format!("render: poll(sync_fd): {e}"));
            }
            if rc == 0 {
                return Ok(false);
            }
            if (pfd.revents & libc::POLLIN) != 0 {
                return Ok(true);
            }
            return Err(format!("render: poll(sync_fd) unexpected revents={:#x}", pfd.revents));
        }
    }

    /// Import an App-allocated AHardwareBuffer as a `VkImage` (route 1: the
    /// App allocates LINEAR AHBs via `AHardwareBuffer_allocate` so the
    /// server's turnip can import the dma-buf without crashing; the App then
    /// GPU-blits the AHB into the swapchain each frame). Uses the
    /// `VK_ANDROID_external_memory_android_hardware_buffer` import path:
    /// create an image, then allocate its memory with
    /// `VkImportAndroidHardwareBufferInfoANDROID` (the AHB import struct
    /// extends `VkMemoryAllocateInfo`, not `VkImageCreateInfo`) and bind.
    ///
    /// The image is `SAMPLED | TRANSFER_SRC` (blit source). The slot -> image
    /// mapping is stored in `self.ahb_images`.
    pub fn import_ahb_image(
        &mut self,
        slot: u32,
        ahb: *mut ndk_sys::AHardwareBuffer,
        width: u32,
        height: u32,
    ) -> Result<u64, String> {
        let device = self.device.as_ref().ok_or("render: import_ahb before init")?;
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .extent(vk::Extent3D { width, height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { device.create_image(&image_info, None) }
            .map_err(|e| format!("render: vkCreateImage(ahb slot {slot}): {e}"))?;
        let reqs = unsafe { device.get_image_memory_requirements(image) };
        let instance = self.instance.as_ref().ok_or("render: no instance")?;
        let mem_props =
            unsafe { instance.get_physical_device_memory_properties(self.physical_device) };
        let mem_type_index = (0..mem_props.memory_type_count)
            .find(|&i| (reqs.memory_type_bits & (1 << i)) != 0)
            .ok_or("render: no memory type for AHB image")?;
        let mut import = vk::ImportAndroidHardwareBufferInfoANDROID::default()
            .buffer(ahb as *mut vk::AHardwareBuffer);
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(reqs.size)
            .memory_type_index(mem_type_index)
            .push_next(&mut import);
        let memory = unsafe { device.allocate_memory(&alloc_info, None) }
            .map_err(|e| format!("render: vkAllocateMemory(ahb slot {slot}): {e}"))?;
        if let Err(e) = unsafe { device.bind_image_memory(image, memory, 0) } {
            unsafe { device.free_memory(memory, None) };
            unsafe { device.destroy_image(image, None) };
            return Err(format!("render: vkBindImageMemory(ahb slot {slot}): {e}"));
        }
        // Track memory alongside the image so Drop can free both.
        self.ahb_memories.insert(slot, memory);
        self.ahb_images.insert(slot, image);
        Ok(slot as u64)
    }

    /// GPU-blit the App's LINEAR AHB slot image into the acquired swapchain
    /// image, then present. Waits on `fence_sem` (the server's SYNC_FD blit
    /// fence imported as a semaphore) before the blit so we never sample the
    /// AHB before the server finished writing it. Returns the present result.
    pub fn blit_ahb_to_swapchain(
        &mut self,
        slot: u32,
        swapchain_index: u32,
        fence_sem: Option<vk::Semaphore>,
    ) -> Result<(), String> {
        if !self.initialized {
            return Err("render: blit before init".into());
        }
        let device = self.device.as_ref().expect("device set");
        let src = *self
            .ahb_images
            .get(&slot)
            .ok_or_else(|| format!("render: slot {slot} AHB not imported"))?;
        let dst = *self
            .images
            .get(swapchain_index as usize)
            .ok_or_else(|| format!("render: swapchain image {swapchain_index} missing"))?;

        // Wait for the server blit fence (if provided) BEFORE sampling.
        if let Some(sem) = fence_sem {
            let wait = [sem];
            let submit = vk::SubmitInfo::default().wait_semaphores(&wait).wait_dst_stage_mask(
                &[vk::PipelineStageFlags::TRANSFER],
            );
            unsafe {
                device.queue_submit(self.queue, &[submit], vk::Fence::null())
                    .map_err(|e| format!("render: queue_submit (fence wait): {e}"))?;
            }
        }

        unsafe {
            device
                .reset_command_buffer(self.command_buffer, vk::CommandBufferResetFlags::empty())
                .map_err(|e| format!("render: reset_cmd: {e}"))?;
        }
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe {
            device
                .begin_command_buffer(self.command_buffer, &begin)
                .map_err(|e| format!("render: begin_cmd: {e}"))?;
        }

        // src: UNDEFINED -> TRANSFER_SRC_OPTIMAL; dst: UNDEFINED -> TRANSFER_DST_OPTIMAL
        let barrier_src = vk::ImageMemoryBarrier::default()
            .image(src)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        let barrier_dst = vk::ImageMemoryBarrier::default()
            .image(dst)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        unsafe {
            device.cmd_pipeline_barrier(
                self.command_buffer,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier_src, barrier_dst],
            );
        }

        let region = vk::ImageBlit::default()
            .src_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .src_offsets([
                vk::Offset3D { x: 0, y: 0, z: 0 },
                vk::Offset3D {
                    x: self.extent.width as i32,
                    y: self.extent.height as i32,
                    z: 1,
                },
            ])
            .dst_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .dst_offsets([
                vk::Offset3D { x: 0, y: 0, z: 0 },
                vk::Offset3D {
                    x: self.extent.width as i32,
                    y: self.extent.height as i32,
                    z: 1,
                },
            ]);
        unsafe {
            device.cmd_blit_image(
                self.command_buffer,
                src,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                dst,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
                vk::Filter::LINEAR,
            );
        }

        // dst -> PRESENT_SRC (must be COLOR_ATTACHMENT_OPTIMAL or PRESENT_SRC)
        let present_barrier = vk::ImageMemoryBarrier::default()
            .image(dst)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::empty())
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        unsafe {
            device.cmd_pipeline_barrier(
                self.command_buffer,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[present_barrier],
            );
        }

        unsafe {
            device
                .end_command_buffer(self.command_buffer)
                .map_err(|e| format!("render: end_cmd: {e}"))?;
        }
        let cmd_bufs = [self.command_buffer];
        let submit = vk::SubmitInfo::default().command_buffers(&cmd_bufs);
        unsafe {
            device
                .queue_submit(self.queue, &[submit], vk::Fence::null())
                .map_err(|e| format!("render: queue_submit (blit): {e}"))?;
        }

        // Present (OUT_OF_DATE/SUBOPTIMAL recreate handled inside present).
        self.present(swapchain_index, &[])
    }

    /// Destroy the current swapchain and re-create it against the same
    /// surface, re-fetching the images (anti-stale-state: image list, extent
    /// and format are all re-queried). Needed for rotation/resize (M5 dynamic
    /// config). Blocks until the device is idle so no in-flight frame still
    /// references the old images.
    pub fn recreate_swapchain(&mut self) -> Result<(), String> {
        if !self.initialized && self.device.is_none() {
            return Err("render: recreate before init".into());
        }
        let device = self.device.as_ref().expect("device set");
        let loader = self.swapchain_loader.as_ref().expect("swapchain_loader set");
        unsafe {
            device
                .device_wait_idle()
                .map_err(|e| format!("render: vkDeviceWaitIdle: {e}"))?;
            if self.swapchain != vk::SwapchainKHR::null() {
                loader.destroy_swapchain(self.swapchain, None);
                self.swapchain = vk::SwapchainKHR::null();
            }
            for &mem in &self.image_memories {
                device.free_memory(mem, None);
            }
            self.image_memories.clear();
        }
        self.images.clear();
        self.create_swapchain_and_images()
    }

    /// The negotiated swapchain image format (B8G8R8A8_UNORM or
    /// R8G8B8A8_UNORM in practice) — lane 29/30 must align the server blit
    /// src format with this; vkCmdBlitImage converts if they differ.
    pub fn image_format(&self) -> vk::Format {
        self.image_format
    }

    pub fn color_space(&self) -> vk::ColorSpaceKHR {
        self.color_space
    }

    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    pub fn present_mode(&self) -> vk::PresentModeKHR {
        self.present_mode
    }

    /// Swapchain images (gralloc-backed AHardwareBuffers) — lane 27 walks
    /// these to extract AHB handles for the TBUF handshake.
    pub fn images(&self) -> &[vk::Image] {
        &self.images
    }

    /// The `VkDeviceMemory` bound to swapchain image `index` — lane 27 passes
    /// this to `AhbSlot::from_swapchain_image` to export the AHardwareBuffer.
    pub fn image_memory(&self, index: u32) -> Option<vk::DeviceMemory> {
        self.image_memories.get(index as usize).copied()
    }

    /// Whether the host driver exposes `VK_KHR_external_semaphore_fd`
    /// (SYNC_FD fence import, F-12). True ⇒ fence frames import the server's
    /// sync_file as a wait semaphore and present on it; false ⇒ they degrade
    /// to [`RenderState::wait_sync_fd`] (CPU poll) + present with no wait.
    /// This is the App-side bring-up runtime assertion (V-33 / plan lane 30):
    /// probed during `init` (extension enumerate), surfaced by lib.rs after
    /// the swapchain comes up.
    pub fn semaphore_fd_supported(&self) -> bool {
        self.semaphore_fd_loader.is_some()
    }

    pub fn raw_instance(&self) -> Option<&ash::Instance> {
        self.instance.as_ref()
    }

    pub fn raw_device(&self) -> vk::Device {
        self.device.as_ref().map(|d| d.handle()).unwrap_or_default()
    }

    /// `&ash::Device` — lane 28 builds the AHB loader via
    /// `AhbLoader::new(&instance, &device)`.
    pub fn raw_device_ref(&self) -> Option<&ash::Device> {
        self.device.as_ref()
    }

    pub fn raw_physical_device(&self) -> vk::PhysicalDevice {
        self.physical_device
    }

    pub fn raw_queue(&self) -> vk::Queue {
        self.queue
    }
}

impl Drop for RenderState {
    fn drop(&mut self) {
        // Order matters: swapchain (needs device) → device (needs instance)
        // → surface (needs instance) → instance. Entry is a plain function
        // table and needs no teardown.
        if let (Some(device), Some(loader)) = (&self.device, &self.swapchain_loader) {
            unsafe {
                let _ = device.device_wait_idle();
                if self.swapchain != vk::SwapchainKHR::null() {
                    loader.destroy_swapchain(self.swapchain, None);
                    self.swapchain = vk::SwapchainKHR::null();
                }
                for &mem in &self.image_memories {
                    device.free_memory(mem, None);
                }
                self.image_memories.clear();
                for (_, img) in self.ahb_images.drain() {
                    device.destroy_image(img, None);
                }
                for (_, mem) in self.ahb_memories.drain() {
                    device.free_memory(mem, None);
                }
                if self.command_pool != vk::CommandPool::null() {
                    device.destroy_command_pool(self.command_pool, None);
                    self.command_pool = vk::CommandPool::null();
                }
            }
        }
        if let Some(device) = self.device.take() {
            unsafe { device.destroy_device(None) };
        }
        if let (Some(instance), Some(surface_loader)) = (&self.instance, &self.surface_loader) {
            if self.surface != vk::SurfaceKHR::null() {
                unsafe { surface_loader.destroy_surface(self.surface, None) };
                self.surface = vk::SurfaceKHR::null();
            }
            let _ = instance;
        }
        if let Some(instance) = self.instance.take() {
            unsafe { instance.destroy_instance(None) };
        }
        self.initialized = false;
        log::info!("render: dropped");
    }
}
