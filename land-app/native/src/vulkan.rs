use std::ffi::CString;
use std::os::unix::io::RawFd;

use ash::vk;

const VERT_SPV: &[u8] = include_bytes!("../shaders/fullscreen_quad.vert.spv");
const FRAG_SPV: &[u8] = include_bytes!("../shaders/texture.frag.spv");

pub struct VulkanRenderer {
    entry: ash::Entry,
    instance: ash::Instance,
    physical_device: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    queue_family: u32,
    swapchain_loader: Option<ash::khr::swapchain::Device>,
    surface: Option<vk::SurfaceKHR>,
    swapchain: Option<vk::SwapchainKHR>,
    swapchain_images: Vec<vk::Image>,
    swapchain_image_views: Vec<vk::ImageView>,
    swapchain_framebuffers: Vec<vk::Framebuffer>,
    swapchain_format: vk::Format,
    swapchain_extent: vk::Extent2D,
    render_pass: vk::RenderPass,
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    command_pool: vk::CommandPool,
    command_buffers: Vec<vk::CommandBuffer>,
    submit_fence: vk::Fence,
    image_available: vk::Semaphore,
    render_finished: vk::Semaphore,
    sampler: vk::Sampler,
    descriptor_pool: vk::DescriptorPool,
    descriptor_set_layout: vk::DescriptorSetLayout,
    memory_properties: vk::PhysicalDeviceMemoryProperties,
    has_external_memory_fd: bool,
}

impl VulkanRenderer {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let entry = match unsafe { ash::Entry::load() } {
            Ok(e) => e,
            Err(e) => {
                log::error!("[vulkan] Entry::load() failed: {:?}", e);
                return Err(Box::new(e));
            }
        };
        let engine_name = CString::new("wl-android")?;
        let app_info = vk::ApplicationInfo::default()
            .api_version(vk::API_VERSION_1_0)
            .engine_name(&engine_name);

        // Android 需要 VK_KHR_android_surface + VK_KHR_surface 才能创建 surface
        let android_surf = CString::new("VK_KHR_android_surface").unwrap();
        let surface = CString::new("VK_KHR_surface").unwrap();
        let ext_names = vec![android_surf.as_ptr(), surface.as_ptr()];

        let inst = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_extension_names(ext_names.as_slice());
        let instance = match unsafe { entry.create_instance(&inst, None) } {
            Ok(i) => i,
            Err(e) => {
                log::error!("[vulkan] create_instance failed: {:?}", e);
                return Err(Box::new(e));
            }
        };

        let pds = unsafe { instance.enumerate_physical_devices()? };
        let (pd, qf) = pds.iter().find_map(|&pd| {
            let props = unsafe { instance.get_physical_device_queue_family_properties(pd) };
            props.iter().position(|qf| qf.queue_flags.contains(vk::QueueFlags::GRAPHICS))
                .map(|i| (pd, i as u32))
        }).ok_or("no GPU")?;

        let swp_name = CString::new("VK_KHR_swapchain")?;
        let ext_fd = CString::new("VK_KHR_external_memory_fd")?;
        let dev_ext = vec![swp_name.as_ptr(), ext_fd.as_ptr()];
        let qci = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(qf).queue_priorities(&[1.0f32]);
        let queues = [qci];
        let dci = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queues).enabled_extension_names(&dev_ext);
        let device = unsafe { instance.create_device(pd, &dci, None)? };
        let queue = unsafe { device.get_device_queue(qf, 0) };
        let memory_properties = unsafe { instance.get_physical_device_memory_properties(pd) };
        let has_ext_mem_fd = false; // TODO: check device ext

        let cmd_pool = unsafe { device.create_command_pool(
            &vk::CommandPoolCreateInfo::default().queue_family_index(qf)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER), None)? };
        let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None)? };
        let ia = unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)? };
        let rf = unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)? };
        let sampler = unsafe { device.create_sampler(
            &vk::SamplerCreateInfo::default().mag_filter(vk::Filter::LINEAR)
                .min_filter(vk::Filter::LINEAR)
                .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE), None)? };

        let dsl = unsafe { device.create_descriptor_set_layout(
            &vk::DescriptorSetLayoutCreateInfo::default().bindings(&[
                vk::DescriptorSetLayoutBinding::default().binding(0)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .descriptor_count(1).stage_flags(vk::ShaderStageFlags::FRAGMENT)]),
            None)? };
        let pll = unsafe { device.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default().set_layouts(&[dsl]), None)? };
        let dp = unsafe { device.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default()
                .pool_sizes(&[vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER).descriptor_count(1)])
                .max_sets(1), None)? };

        let rp = Self::render_pass(&device, vk::Format::R8G8B8A8_SRGB)?;
        let pipeline = Self::graphics_pipeline(&device, rp, pll)?;

        Ok(Self {
            entry, instance, physical_device: pd, device, queue, queue_family: qf,
            swapchain_loader: None, surface: None, swapchain: None,
            swapchain_images: vec![], swapchain_image_views: vec![],
            swapchain_framebuffers: vec![],
            swapchain_format: vk::Format::R8G8B8A8_SRGB,
            swapchain_extent: vk::Extent2D::default(),
            render_pass: rp, pipeline, pipeline_layout: pll, command_pool: cmd_pool,
            command_buffers: vec![], submit_fence: fence,
            image_available: ia, render_finished: rf, sampler,
            descriptor_pool: dp, descriptor_set_layout: dsl,
            memory_properties, has_external_memory_fd: has_ext_mem_fd,
        })
    }

    fn has_ext(instance: &ash::Instance, pd: vk::PhysicalDevice, name: &CString) -> bool {
        let props = unsafe { instance.enumerate_device_extension_properties(pd).unwrap_or_default() };
        props.iter().any(|p| unsafe {
            std::ffi::CStr::from_ptr(p.extension_name.as_ptr()) == name.as_c_str()
        })
    }

    fn render_pass(device: &ash::Device, fmt: vk::Format) -> Result<vk::RenderPass, Box<dyn std::error::Error>> {
        let att_ref = vk::AttachmentReference::default().attachment(0)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
        Ok(unsafe { device.create_render_pass(
            &vk::RenderPassCreateInfo::default()
                .attachments(&[vk::AttachmentDescription::default().format(fmt)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .load_op(vk::AttachmentLoadOp::CLEAR).store_op(vk::AttachmentStoreOp::STORE)
                    .initial_layout(vk::ImageLayout::UNDEFINED)
                    .final_layout(vk::ImageLayout::PRESENT_SRC_KHR)])
                .subpasses(&[vk::SubpassDescription::default()
                    .color_attachments(&[att_ref])
                    .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)]),
            None)? })
    }

    fn graphics_pipeline(device: &ash::Device, rp: vk::RenderPass, pll: vk::PipelineLayout) -> Result<vk::Pipeline, Box<dyn std::error::Error>> {
        let name = CString::new("main")?;
        // SAFETY: SPIR-V binary bytes reinterpreted as u32 slices
        let vert_code = unsafe { std::slice::from_raw_parts(
            VERT_SPV.as_ptr() as *const u32, VERT_SPV.len() / 4) };
        let frag_code = unsafe { std::slice::from_raw_parts(
            FRAG_SPV.as_ptr() as *const u32, FRAG_SPV.len() / 4) };
        let vert = unsafe { device.create_shader_module(
            &vk::ShaderModuleCreateInfo::default().code(vert_code), None)? };
        let frag = unsafe { device.create_shader_module(
            &vk::ShaderModuleCreateInfo::default().code(frag_code), None)? };
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX).module(vert).name(&name),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT).module(frag).name(&name),
        ];
        let vp = [vk::Viewport::default().width(1.0).height(1.0).max_depth(1.0)];
        let sc = [vk::Rect2D::default()];
        let ba = [vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(false).color_write_mask(vk::ColorComponentFlags::RGBA)];
        let ds = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let p = unsafe {
            device.create_graphics_pipelines(vk::PipelineCache::null(), &[
                vk::GraphicsPipelineCreateInfo::default()
                    .stages(&stages)
                    .vertex_input_state(&vk::PipelineVertexInputStateCreateInfo::default())
                    .input_assembly_state(&vk::PipelineInputAssemblyStateCreateInfo::default()
                        .topology(vk::PrimitiveTopology::TRIANGLE_STRIP))
                    .viewport_state(&vk::PipelineViewportStateCreateInfo::default()
                        .viewports(&vp).scissors(&sc))
                    .rasterization_state(&vk::PipelineRasterizationStateCreateInfo::default()
                        .cull_mode(vk::CullModeFlags::NONE)
                        .front_face(vk::FrontFace::COUNTER_CLOCKWISE).line_width(1.0))
                    .multisample_state(&vk::PipelineMultisampleStateCreateInfo::default()
                        .rasterization_samples(vk::SampleCountFlags::TYPE_1))
                    .color_blend_state(&vk::PipelineColorBlendStateCreateInfo::default()
                        .attachments(&ba))
                    .dynamic_state(&vk::PipelineDynamicStateCreateInfo::default()
                        .dynamic_states(&ds))
                    .layout(pll).render_pass(rp)], None)
                .map_err(|(_, e)| e)?.remove(0)
        };
        unsafe { device.destroy_shader_module(vert, None); }
        unsafe { device.destroy_shader_module(frag, None); }
        Ok(p)
    }

    pub fn set_surface(&mut self, surface: vk::SurfaceKHR, w: u32, h: u32) -> Result<(), Box<dyn std::error::Error>> {
        let sl = ash::khr::surface::Instance::new(&self.entry, &self.instance);
        let sw = ash::khr::swapchain::Device::new(&self.instance, &self.device);
        let fmts = unsafe { sl.get_physical_device_surface_formats(self.physical_device, surface)? };
        let fmt = fmts.iter().find(|f| {
            f.format == vk::Format::R8G8B8A8_SRGB && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
        }).unwrap_or(&fmts[0]);
        let caps = unsafe { sl.get_physical_device_surface_capabilities(self.physical_device, surface)? };
        let ext = if caps.current_extent.width != 0 { caps.current_extent }
            else { vk::Extent2D { width: w.max(1), height: h.max(1) } };
        let pm = unsafe { sl.get_physical_device_surface_present_modes(self.physical_device, surface)? };
        let mode = if pm.contains(&vk::PresentModeKHR::MAILBOX) { vk::PresentModeKHR::MAILBOX }
            else { vk::PresentModeKHR::FIFO };

        let sc = unsafe { sw.create_swapchain(
            &vk::SwapchainCreateInfoKHR::default().surface(surface)
                .min_image_count(caps.min_image_count + 1)
                .image_format(fmt.format).image_color_space(fmt.color_space)
                .image_extent(ext).image_array_layers(1)
                .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_DST)
                .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                .pre_transform(caps.current_transform)
                .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
                .present_mode(mode).clipped(true), None)? };
        let images = unsafe { sw.get_swapchain_images(sc)? };

        self.cleanup(Some(surface), &sl);
        let rp = Self::render_pass(&self.device, fmt.format)?;
        let pl = Self::graphics_pipeline(&self.device, rp, self.pipeline_layout)?;

        let mut ivs = Vec::with_capacity(images.len());
        let mut fbs = Vec::with_capacity(images.len());
        for &img in &images {
            let sr = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR).level_count(1).layer_count(1);
            let iv = unsafe { self.device.create_image_view(
                &vk::ImageViewCreateInfo::default().image(img)
                    .view_type(vk::ImageViewType::TYPE_2D).format(fmt.format)
                    .subresource_range(sr), None)? };
            let fb = unsafe { self.device.create_framebuffer(
                &vk::FramebufferCreateInfo::default().render_pass(rp).attachments(&[iv])
                    .width(ext.width).height(ext.height).layers(1), None)? };
            ivs.push(iv); fbs.push(fb);
        }

        let cmds = unsafe { self.device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default().command_pool(self.command_pool)
                .level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(images.len() as u32))? };
        for (i, &cmd) in cmds.iter().enumerate() {
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::SIMULTANEOUS_USE);
            let clear = vk::ClearValue { color: vk::ClearColorValue { float32: [0.0, 0.0, 0.0, 1.0] } };
            let clears = [clear];
            let rp_b = vk::RenderPassBeginInfo::default().render_pass(rp).framebuffer(fbs[i])
                .render_area(vk::Rect2D { offset: vk::Offset2D { x: 0, y: 0 }, extent: ext })
                .clear_values(&clears);
            let vp = vk::Viewport {
                x: 0.0, y: 0.0, width: ext.width as f32, height: ext.height as f32,
                min_depth: 0.0, max_depth: 1.0,
            };
            unsafe {
                self.device.begin_command_buffer(cmd, &begin)?;
                self.device.cmd_begin_render_pass(cmd, &rp_b, vk::SubpassContents::INLINE);
                self.device.cmd_set_viewport(cmd, 0, &[vp]);
                self.device.cmd_set_scissor(cmd, 0, &[vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 }, extent: ext }]);
                self.device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pl);
                self.device.cmd_end_render_pass(cmd);
                self.device.end_command_buffer(cmd)?;
            }
        }

        self.swapchain_loader = Some(sw);
        self.surface = Some(surface);
        self.swapchain = Some(sc);
        self.swapchain_images = images;
        self.swapchain_image_views = ivs;
        self.swapchain_framebuffers = fbs;
        self.swapchain_format = fmt.format;
        self.swapchain_extent = ext;
        self.render_pass = rp;
        self.pipeline = pl;
        self.command_buffers = cmds;
        Ok(())
    }

    fn cleanup(&self, new_surface: Option<vk::SurfaceKHR>, sl: &ash::khr::surface::Instance) {
        unsafe {
            if self.render_pass != vk::RenderPass::null() { self.device.destroy_render_pass(self.render_pass, None); }
            if self.pipeline != vk::Pipeline::null() { self.device.destroy_pipeline(self.pipeline, None); }
            for &fb in &self.swapchain_framebuffers { self.device.destroy_framebuffer(fb, None); }
            for &iv in &self.swapchain_image_views { self.device.destroy_image_view(iv, None); }
            if let Some(ref sw) = self.swapchain {
                if let Some(ref sl) = self.swapchain_loader { sl.destroy_swapchain(*sw, None); }
            }
            if let Some(s) = self.surface {
                if new_surface.map_or(true, |ns| ns != s) { sl.destroy_surface(s, None); }
            }
        }
    }

    pub fn import_and_render(&self, fd: RawFd, w: u32, h: u32) -> Result<(), Box<dyn std::error::Error>> {
        if !self.has_external_memory_fd { return Err("VK_KHR_external_memory_fd not available".into()); }

        let mut ext = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let ii = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D).format(vk::Format::R8G8B8A8_SRGB)
            .extent(vk::Extent3D { width: w, height: h, depth: 1 })
            .mip_levels(1).array_layers(1).samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::LINEAR)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE).push_next(&mut ext);
        let img = unsafe { self.device.create_image(&ii, None)? };

        let mr = unsafe { self.device.get_image_memory_requirements(img) };
        let mt = Self::find_mt(&self.memory_properties, mr.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL).ok_or("no mem type")?;

        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT).fd(fd);
        let ai = vk::MemoryAllocateInfo::default()
            .allocation_size(mr.size).memory_type_index(mt).push_next(&mut import);
        let mem = unsafe { self.device.allocate_memory(&ai, None)? };
        unsafe { self.device.bind_image_memory(img, mem, 0)?; }

        let sr = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR).level_count(1).layer_count(1);
        let iv = unsafe { self.device.create_image_view(
            &vk::ImageViewCreateInfo::default().image(img)
                .view_type(vk::ImageViewType::TYPE_2D).format(vk::Format::R8G8B8A8_SRGB)
                .subresource_range(sr), None)? };

        let tc = unsafe { self.device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default().command_pool(self.command_pool)
                .level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(1))?[0] };
        unsafe {
            self.device.begin_command_buffer(tc, &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))?;
            let bar = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .image(img).subresource_range(sr);
            self.device.cmd_pipeline_barrier(tc,
                vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(), &[], &[], &[bar]);
            self.device.end_command_buffer(tc)?;
            self.device.queue_submit(self.queue, &[vk::SubmitInfo::default()
                .command_buffers(&[tc])], vk::Fence::null())?;
            self.device.queue_wait_idle(self.queue)?;
            self.device.free_command_buffers(self.command_pool, &[tc]);
        }

        let ds = unsafe { self.device.allocate_descriptor_sets(
            &vk::DescriptorSetAllocateInfo::default().descriptor_pool(self.descriptor_pool)
                .set_layouts(&[self.descriptor_set_layout]))?[0] };
        let di = vk::DescriptorImageInfo::default()
            .sampler(self.sampler).image_view(iv)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        unsafe { self.device.update_descriptor_sets(
            &[vk::WriteDescriptorSet::default().dst_set(ds).dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&[di])], &[]); }

        if let Some(ref sw) = self.swapchain_loader {
            let (idx, _) = unsafe { sw.acquire_next_image(
                self.swapchain.unwrap(), std::u64::MAX, self.image_available, vk::Fence::null())? };
            let cmd = self.command_buffers[idx as usize];
            let clear = vk::ClearValue { color: vk::ClearColorValue { float32: [0.0, 0.0, 0.0, 1.0] } };
            let clears = [clear];
            let rp_b = vk::RenderPassBeginInfo::default().render_pass(self.render_pass)
                .framebuffer(self.swapchain_framebuffers[idx as usize])
                .render_area(vk::Rect2D { offset: vk::Offset2D { x: 0, y: 0 }, extent: self.swapchain_extent })
                .clear_values(&clears);
            let vp = vk::Viewport {
                x: 0.0, y: 0.0, width: self.swapchain_extent.width as f32,
                height: self.swapchain_extent.height as f32, min_depth: 0.0, max_depth: 1.0,
            };
            unsafe {
                self.device.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
                self.device.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::SIMULTANEOUS_USE))?;
                self.device.cmd_begin_render_pass(cmd, &rp_b, vk::SubpassContents::INLINE);
                self.device.cmd_set_viewport(cmd, 0, &[vp]);
                self.device.cmd_set_scissor(cmd, 0, &[vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 }, extent: self.swapchain_extent }]);
                self.device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
                self.device.cmd_bind_descriptor_sets(cmd, vk::PipelineBindPoint::GRAPHICS,
                    self.pipeline_layout, 0, &[ds], &[]);
                self.device.cmd_draw(cmd, 4, 1, 0, 0);
                self.device.cmd_end_render_pass(cmd);
                self.device.end_command_buffer(cmd)?;
            }
            let ws = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
            unsafe {
                self.device.queue_submit(self.queue, &[vk::SubmitInfo::default()
                    .wait_semaphores(&[self.image_available]).wait_dst_stage_mask(&ws)
                    .command_buffers(&[cmd]).signal_semaphores(&[self.render_finished])],
                    self.submit_fence)?;
                self.device.wait_for_fences(&[self.submit_fence], true, std::u64::MAX)?;
                self.device.reset_fences(&[self.submit_fence])?;
                sw.queue_present(self.queue, &vk::PresentInfoKHR::default()
                    .wait_semaphores(&[self.render_finished])
                    .swapchains(&[self.swapchain.unwrap()]).image_indices(&[idx]))?;
            }
        }

        unsafe {
            self.device.free_descriptor_sets(self.descriptor_pool, &[ds]);
            self.device.destroy_image_view(iv, None);
            self.device.destroy_image(img, None);
            self.device.free_memory(mem, None);
        }
        Ok(())
    }

    pub fn create_android_surface(&mut self, window: *mut std::ffi::c_void, w: u32, h: u32) -> Result<(), Box<dyn std::error::Error>> {
        let android = ash::khr::android_surface::Instance::new(&self.entry, &self.instance);
        let ci = vk::AndroidSurfaceCreateInfoKHR::default().window(window);
        let surface = unsafe { android.create_android_surface(&ci, None)? };
        self.set_surface(surface, w, h)
    }

    fn find_mt(props: &vk::PhysicalDeviceMemoryProperties, filter: u32, req: vk::MemoryPropertyFlags) -> Option<u32> {
        for i in 0..props.memory_type_count {
            if (filter & (1 << i)) != 0 && props.memory_types[i as usize].property_flags.contains(req) {
                return Some(i);
            }
        }
        None
    }
}

impl Drop for VulkanRenderer {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_sampler(self.sampler, None);
            self.device.destroy_descriptor_pool(self.descriptor_pool, None);
            self.device.destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            self.device.destroy_pipeline_layout(self.pipeline_layout, None);
            if self.pipeline != vk::Pipeline::null() { self.device.destroy_pipeline(self.pipeline, None); }
            if self.render_pass != vk::RenderPass::null() { self.device.destroy_render_pass(self.render_pass, None); }
            for &fb in &self.swapchain_framebuffers { self.device.destroy_framebuffer(fb, None); }
            for &iv in &self.swapchain_image_views { self.device.destroy_image_view(iv, None); }
            if let Some(ref sw) = self.swapchain {
                if let Some(ref sl) = self.swapchain_loader { sl.destroy_swapchain(*sw, None); }
            }
            if let Some(s) = self.surface {
                let sl = ash::khr::surface::Instance::new(&self.entry, &self.instance);
                sl.destroy_surface(s, None);
            }
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_semaphore(self.image_available, None);
            self.device.destroy_semaphore(self.render_finished, None);
            self.device.destroy_fence(self.submit_fence, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}
