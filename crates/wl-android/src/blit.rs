use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::fd::{OwnedFd, RawFd};
use std::sync::Arc;

use ash::vk;
use tracing::{error, info, warn};

pub struct BlitEngine {
    instance: Option<ash::Instance>,
    device: Option<ash::Device>,
    physical_device: vk::PhysicalDevice,
    queue: vk::Queue,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    images: HashMap<u64, vk::Image>,
    image_memories: HashMap<u64, vk::DeviceMemory>,
    image_views: HashMap<u64, vk::ImageView>,
    fence: vk::Fence,
    initialized: bool,
}

impl BlitEngine {
    pub fn new() -> Self {
        Self {
            instance: None,
            device: None,
            physical_device: vk::PhysicalDevice::null(),
            queue: vk::Queue::null(),
            command_pool: vk::CommandPool::null(),
            command_buffer: vk::CommandBuffer::null(),
            images: HashMap::new(),
            image_memories: HashMap::new(),
            image_views: HashMap::new(),
            fence: vk::Fence::null(),
            initialized: false,
        }
    }

    pub fn init(&mut self) -> Result<(), String> {
        if self.initialized { return Ok(()); }

        let entry = unsafe { ash::Entry::load() }
            .map_err(|e| format!("ash Entry::load: {e}"))?;

        let app_name = CString::new("wl-android-blit").unwrap();
        let app_info = vk::ApplicationInfo::default()
            .application_name(&app_name)
            .application_version(0)
            .engine_name(&app_name)
            .engine_version(0)
            .api_version(vk::API_VERSION_1_3);

        let instance_extensions = [
            vk::KHR_EXTERNAL_MEMORY_CAPABILITIES_NAME,
            vk::KHR_EXTERNAL_SEMAPHORE_CAPABILITIES_NAME,
            vk::KHR_EXTERNAL_FENCE_CAPABILITIES_NAME,
        ];

        let ext_names: Vec<CString> = instance_extensions
            .iter().map(|e| CString::from(CStr::from_ptr(*e))).collect();
        let ext_ptrs: Vec<_> = ext_names.iter().map(|e| e.as_ptr()).collect();

        let create_info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_extension_names(&ext_ptrs);

        let instance = unsafe {
            entry.create_instance(&create_info, None)
                .map_err(|e| format!("create_instance: {e}"))?
        };

        let pdevices = unsafe { instance.enumerate_physical_devices() }
            .map_err(|e| format!("enumerate devices: {e}"))?;
        if pdevices.is_empty() {
            return Err("no Vulkan physical devices".into());
        }
        let physical_device = pdevices[0];

        let queue_families = unsafe {
            instance.get_physical_device_queue_family_properties(physical_device)
        };
        let queue_family_index = queue_families.iter()
            .position(|qf| qf.queue_flags.contains(vk::QueueFlags::GRAPHICS))
            .ok_or("no graphics queue")? as u32;

        let queue_priorities = [1.0f32];
        let queue_info = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family_index)
            .queue_priorities(&queue_priorities);

        let device_extensions = [
            vk::KHR_EXTERNAL_MEMORY_FD_NAME,
            vk::EXT_EXTERNAL_MEMORY_DMA_BUF_NAME,
            vk::KHR_EXTERNAL_MEMORY_NAME,
        ];

        let dev_ext_names: Vec<CString> = device_extensions
            .iter().map(|e| CString::from(CStr::from_ptr(*e))).collect();
        let dev_ext_ptrs: Vec<_> = dev_ext_names.iter().map(|e| e.as_ptr()).collect();

        let device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(std::slice::from_ref(&queue_info))
            .enabled_extension_names(&dev_ext_ptrs);

        let device = unsafe {
            instance.create_device(physical_device, &device_info, None)
                .map_err(|e| format!("create_device: {e}"))?
        };
        let queue = unsafe { device.get_device_queue(queue_family_index, 0) };

        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(queue_family_index)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let command_pool = unsafe {
            device.create_command_pool(&pool_info, None)
                .map_err(|e| format!("create_command_pool: {e}"))?
        };

        let cmd_alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .command_buffer_count(1);
        let command_buffers = unsafe {
            device.allocate_command_buffers(&cmd_alloc)
                .map_err(|e| format!("allocate_cmd_buf: {e}"))?
        };

        let fence_info = vk::FenceCreateInfo::default()
            .flags(vk::FenceCreateFlags::SIGNALED);
        let fence = unsafe {
            device.create_fence(&fence_info, None)
                .map_err(|e| format!("create_fence: {e}"))?
        };

        self.instance = Some(instance);
        self.device = Some(device);
        self.physical_device = physical_device;
        self.queue = queue;
        self.command_pool = command_pool;
        self.command_buffer = command_buffers[0];
        self.fence = fence;
        self.initialized = true;

        info!("blit engine initialized");
        Ok(())
    }

    fn device(&self) -> &ash::Device {
        self.device.as_ref().expect("blit engine not initialized")
    }

    /// Import a dmabuf fd as a VkImage. Returns an opaque handle.
    pub fn import_dmabuf(
        &mut self,
        fd: OwnedFd,
        width: u32,
        height: u32,
        format: vk::Format,
        modifier: u64,
    ) -> Result<u64, String> {
        if !self.initialized { return Err("not initialized".into()); }
        let device = self.device();

        use std::os::fd::AsRawFd;
        let raw_fd = fd.as_raw_fd();

        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D { width, height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);

        let image = unsafe {
            device.create_image(&image_info, None)
                .map_err(|e| format!("create_image: {e}"))?
        };

        let mem_req = unsafe { device.get_image_memory_requirements(image) };

        let import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(raw_fd);

        let mut alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_req.size)
            .push_next(&mut import_info);

        let mem_props = unsafe {
            if let Some(ref inst) = self.instance {
                inst.get_physical_device_memory_properties(self.physical_device)
            } else {
                return Err("no instance".into());
            }
        };
        let mem_type_index = (0..mem_props.memory_type_count)
            .find(|&i| (mem_req.memory_type_bits & (1 << i)) != 0)
            .ok_or("no suitable memory type")?;
        alloc_info = alloc_info.memory_type_index(mem_type_index);

        let memory = unsafe {
            device.allocate_memory(&alloc_info, None)
                .map_err(|e| format!("allocate_memory: {e}"))?
        };

        unsafe {
            device.bind_image_memory(image, memory, 0)
                .map_err(|e| format!("bind_image_memory: {e}"))?;
        }

        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0, level_count: 1,
                base_array_layer: 0, layer_count: 1,
            });
        let view = unsafe {
            device.create_image_view(&view_info, None)
                .map_err(|e| format!("create_image_view: {e}"))?
        };

        let handle = next_handle();
        self.images.insert(handle, image);
        self.image_memories.insert(handle, memory);
        self.image_views.insert(handle, view);

        std::mem::forget(fd); // consumed by import
        info!(handle, width, height, "imported dmabuf");
        Ok(handle)
    }

    /// Blit from src to dst, submit to GPU. Call wait_complete before reading dst.
    pub fn blit_submit(
        &self,
        src_handle: u64,
        dst_handle: u64,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        if !self.initialized { return Err("not initialized".into()); }
        let device = self.device();

        let src = *self.images.get(&src_handle).ok_or("src not found")?;
        let dst = *self.images.get(&dst_handle).ok_or("dst not found")?;

        unsafe { device.reset_fences(&[self.fence]) }
            .map_err(|e| format!("reset_fence: {e}"))?;

        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe { device.begin_command_buffer(self.command_buffer, &begin_info) }
            .map_err(|e| format!("begin_cmd: {e}"))?;

        let barrier_src = vk::ImageMemoryBarrier::default()
            .image(src)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0, level_count: 1,
                base_array_layer: 0, layer_count: 1,
            })
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ);

        let barrier_dst = vk::ImageMemoryBarrier::default()
            .image(dst)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0, level_count: 1,
                base_array_layer: 0, layer_count: 1,
            })
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);

        unsafe {
            device.cmd_pipeline_barrier(
                self.command_buffer,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[], &[], &[barrier_src, barrier_dst],
            );
        }

        let blit_region = vk::ImageBlit::default()
            .src_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0, base_array_layer: 0, layer_count: 1,
            })
            .src_offsets([
                vk::Offset3D { x: 0, y: 0, z: 0 },
                vk::Offset3D { x: width as i32, y: height as i32, z: 1 },
            ])
            .dst_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0, base_array_layer: 0, layer_count: 1,
            })
            .dst_offsets([
                vk::Offset3D { x: 0, y: 0, z: 0 },
                vk::Offset3D { x: width as i32, y: height as i32, z: 1 },
            ]);

        unsafe {
            device.cmd_blit_image(
                self.command_buffer,
                src, vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                dst, vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[blit_region],
                vk::Filter::NEAREST,
            );
        }

        unsafe { device.end_command_buffer(self.command_buffer) }
            .map_err(|e| format!("end_cmd: {e}"))?;

        let submit_info = vk::SubmitInfo::default()
            .command_buffers(std::slice::from_ref(&self.command_buffer));
        unsafe {
            device.queue_submit(self.queue, &[submit_info], self.fence)
                .map_err(|e| format!("queue_submit: {e}"))?;
        }

        Ok(())
    }

    pub fn wait_complete(&self) -> Result<(), String> {
        if !self.initialized { return Ok(()); }
        let device = self.device();
        unsafe {
            device.wait_for_fences(&[self.fence], true, u64::MAX)
                .map_err(|e| format!("wait_for_fences: {e}"))?;
        }
        Ok(())
    }

    pub fn destroy_image(&mut self, handle: u64) {
        if let Some(image) = self.images.remove(&handle) {
            unsafe { self.device.as_ref().unwrap().destroy_image(image, None); }
        }
        if let Some(mem) = self.image_memories.remove(&handle) {
            unsafe { self.device.as_ref().unwrap().free_memory(mem, None); }
        }
        if let Some(view) = self.image_views.remove(&handle) {
            unsafe { self.device.as_ref().unwrap().destroy_image_view(view, None); }
        }
    }
}

fn next_handle() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl Drop for BlitEngine {
    fn drop(&mut self) {
        if self.initialized {
            if let Some(ref device) = self.device {
                unsafe {
                    let _ = device.wait_for_fences(&[self.fence], true, u64::MAX);
                    device.destroy_fence(self.fence, None);
                    device.destroy_command_pool(self.command_pool, None);
                    device.destroy_device(None);
                }
            }
            if let Some(ref instance) = self.instance {
                unsafe { instance.destroy_instance(None); }
            }
        }
    }
}
