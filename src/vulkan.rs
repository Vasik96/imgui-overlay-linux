//! Minimal Vulkan context: instance, device, swapchain with *alpha compositing*,
//! one render pass, one frame in flight. The caller records draw commands inside
//! the render pass through a closure.

use anyhow::{anyhow, Result};
use ash::{extensions::khr, vk, Device, Entry, Instance};
use raw_window_handle::{HasRawDisplayHandle, HasRawWindowHandle};
use winit::{dpi::PhysicalSize, window::Window};

pub struct Vulkan {
    _entry: Entry,
    pub instance: Instance,
    surface_loader: khr::Surface,
    surface: vk::SurfaceKHR,
    pub physical_device: vk::PhysicalDevice,
    pub device: Device,
    pub queue: vk::Queue,
    swapchain_loader: khr::Swapchain,
    swapchain: vk::SwapchainKHR,
    surface_format: vk::SurfaceFormatKHR,
    extent: vk::Extent2D,
    views: Vec<vk::ImageView>,
    framebuffers: Vec<vk::Framebuffer>,
    pub render_pass: vk::RenderPass,
    pub command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    image_available: vk::Semaphore,
    render_finished: Vec<vk::Semaphore>,
    in_flight: vk::Fence,
}

impl Vulkan {
    pub unsafe fn new(window: &Window) -> Result<Self> {
        let entry = Entry::load()?;

        // ---- instance -------------------------------------------------------
        let app_info = vk::ApplicationInfo::builder()
            .application_name(std::ffi::CStr::from_bytes_with_nul_unchecked(
                b"imgui-x11-overlay\0",
            ))
            .api_version(vk::make_api_version(0, 1, 0, 0));
        let ext_names = ash_window::enumerate_required_extensions(window.raw_display_handle())?;
        let instance_info = vk::InstanceCreateInfo::builder()
            .application_info(&app_info)
            .enabled_extension_names(ext_names);
        let instance = entry.create_instance(&instance_info, None)?;

        // ---- surface --------------------------------------------------------
        let surface = ash_window::create_surface(
            &entry,
            &instance,
            window.raw_display_handle(),
            window.raw_window_handle(),
            None,
        )?;
        let surface_loader = khr::Surface::new(&entry, &instance);

        // ---- physical device + queue family ---------------------------------
        let mut best: Option<(u32, vk::PhysicalDevice, u32)> = None;
        for pd in instance.enumerate_physical_devices()? {
            let props = instance.get_physical_device_properties(pd);
            let score = match props.device_type {
                vk::PhysicalDeviceType::DISCRETE_GPU => 2,
                vk::PhysicalDeviceType::INTEGRATED_GPU => 1,
                _ => 0,
            };
            for (i, qf) in instance
                .get_physical_device_queue_family_properties(pd)
                .iter()
                .enumerate()
            {
                let supported = surface_loader
                    .get_physical_device_surface_support(pd, i as u32, surface)
                    .unwrap_or(false);
                if qf.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                    && supported
                    && best.map_or(true, |(s, _, _)| score > s)
                {
                    best = Some((score, pd, i as u32));
                }
            }
        }
        let (_, physical_device, queue_family) =
            best.ok_or_else(|| anyhow!("no Vulkan device can present to this X11 surface"))?;

        // ---- logical device -------------------------------------------------
        let priorities = [1.0f32];
        let queue_infos = [vk::DeviceQueueCreateInfo::builder()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities)
            .build()];
        let dev_exts = [khr::Swapchain::name().as_ptr()];
        let device_info = vk::DeviceCreateInfo::builder()
            .queue_create_infos(&queue_infos)
            .enabled_extension_names(&dev_exts);
        let device = instance.create_device(physical_device, &device_info, None)?;
        let queue = device.get_device_queue(queue_family, 0);
        let swapchain_loader = khr::Swapchain::new(&instance, &device);

        // ---- surface format (UNORM so ImGui colours aren't gamma-shifted) ----
        let formats = surface_loader.get_physical_device_surface_formats(physical_device, surface)?;
        let surface_format = formats
            .iter()
            .copied()
            .find(|f| {
                f.format == vk::Format::B8G8R8A8_UNORM
                    && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
            })
            .or_else(|| formats.first().copied())
            .ok_or_else(|| anyhow!("surface reports no formats"))?;

        // ---- render pass (clear to transparent, present) --------------------
        let attachments = [vk::AttachmentDescription::builder()
            .format(surface_format.format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::PRESENT_SRC_KHR)
            .build()];
        let color_refs = [vk::AttachmentReference::builder()
            .attachment(0)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .build()];
        let subpasses = [vk::SubpassDescription::builder()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&color_refs)
            .build()];
        let deps = [vk::SubpassDependency::builder()
            .src_subpass(vk::SUBPASS_EXTERNAL)
            .dst_subpass(0)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .build()];
        let rp_info = vk::RenderPassCreateInfo::builder()
            .attachments(&attachments)
            .subpasses(&subpasses)
            .dependencies(&deps);
        let render_pass = device.create_render_pass(&rp_info, None)?;

        // ---- command pool / buffer / sync -----------------------------------
        let command_pool = device.create_command_pool(
            &vk::CommandPoolCreateInfo::builder()
                .queue_family_index(queue_family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        )?;
        let command_buffer = device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::builder()
                .command_pool(command_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1),
        )?[0];
        let image_available = device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?;
        let in_flight = device.create_fence(
            &vk::FenceCreateInfo::builder().flags(vk::FenceCreateFlags::SIGNALED),
            None,
        )?;

        let mut this = Self {
            _entry: entry,
            instance,
            surface_loader,
            surface,
            physical_device,
            device,
            queue,
            swapchain_loader,
            swapchain: vk::SwapchainKHR::null(),
            surface_format,
            extent: vk::Extent2D::default(),
            views: Vec::new(),
            framebuffers: Vec::new(),
            render_pass,
            command_pool,
            command_buffer,
            image_available,
            render_finished: Vec::new(),
            in_flight,
        };
        this.create_swapchain(window.inner_size(), vk::SwapchainKHR::null())?;
        Ok(this)
    }

    unsafe fn create_swapchain(
        &mut self,
        size: PhysicalSize<u32>,
        old: vk::SwapchainKHR,
    ) -> Result<()> {
        let caps = self
            .surface_loader
            .get_physical_device_surface_capabilities(self.physical_device, self.surface)?;

        self.extent = if caps.current_extent.width != u32::MAX {
            caps.current_extent
        } else {
            vk::Extent2D {
                width: size
                    .width
                    .clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                height: size
                    .height
                    .clamp(caps.min_image_extent.height, caps.max_image_extent.height),
            }
        };

        // Latency: prefer MAILBOX (newest frame replaces queued ones) over FIFO, which can
        // queue ~2 frames and makes dragged windows trail the (hardware) cursor.
        let modes = self
            .surface_loader
            .get_physical_device_surface_present_modes(self.physical_device, self.surface)?;
        let present_mode = if modes.contains(&vk::PresentModeKHR::MAILBOX) {
            vk::PresentModeKHR::MAILBOX
        } else {
            vk::PresentModeKHR::FIFO
        };
        let mut image_count = if present_mode == vk::PresentModeKHR::MAILBOX {
            caps.min_image_count + 1
        } else {
            caps.min_image_count.max(2)
        };
        if caps.max_image_count > 0 {
            image_count = image_count.min(caps.max_image_count);
        }

        // This is what makes the overlay see-through. Needs an ARGB window visual
        // (winit's `with_transparent(true)`) and a running compositing manager.
        let composite_alpha = [
            vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
            vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED,
            vk::CompositeAlphaFlagsKHR::OPAQUE,
        ]
        .into_iter()
        .find(|c| caps.supported_composite_alpha.contains(*c))
        .unwrap_or(vk::CompositeAlphaFlagsKHR::OPAQUE);
        if composite_alpha == vk::CompositeAlphaFlagsKHR::OPAQUE {
            eprintln!(
                "warning: swapchain has no alpha compositing; the overlay will be opaque. \
                 Is a compositor running?"
            );
        }

        let info = vk::SwapchainCreateInfoKHR::builder()
            .surface(self.surface)
            .min_image_count(image_count)
            .image_format(self.surface_format.format)
            .image_color_space(self.surface_format.color_space)
            .image_extent(self.extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(caps.current_transform)
            .composite_alpha(composite_alpha)
            .present_mode(present_mode)
            .clipped(true)
            .old_swapchain(old);
        self.swapchain = self.swapchain_loader.create_swapchain(&info, None)?;

        let images = self.swapchain_loader.get_swapchain_images(self.swapchain)?;
        for image in images {
            let view = self.device.create_image_view(
                &vk::ImageViewCreateInfo::builder()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(self.surface_format.format)
                    .subresource_range(
                        vk::ImageSubresourceRange::builder()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .level_count(1)
                            .layer_count(1)
                            .build(),
                    ),
                None,
            )?;
            let attachments = [view];
            let fb = self.device.create_framebuffer(
                &vk::FramebufferCreateInfo::builder()
                    .render_pass(self.render_pass)
                    .attachments(&attachments)
                    .width(self.extent.width)
                    .height(self.extent.height)
                    .layers(1),
                None,
            )?;
            let sem = self
                .device
                .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?;
            self.views.push(view);
            self.framebuffers.push(fb);
            self.render_finished.push(sem);
        }
        Ok(())
    }

    unsafe fn destroy_swapchain_resources(&mut self) {
        for fb in self.framebuffers.drain(..) {
            self.device.destroy_framebuffer(fb, None);
        }
        for v in self.views.drain(..) {
            self.device.destroy_image_view(v, None);
        }
        for s in self.render_finished.drain(..) {
            self.device.destroy_semaphore(s, None);
        }
    }

    pub unsafe fn recreate_swapchain(&mut self, size: PhysicalSize<u32>) -> Result<()> {
        self.device.device_wait_idle()?;
        self.destroy_swapchain_resources();
        let old = self.swapchain;
        self.create_swapchain(size, old)?;
        self.swapchain_loader.destroy_swapchain(old, None);
        Ok(())
    }

    /// Renders one frame. `record` is called with a command buffer that is
    /// already inside the render pass. Returns `true` if the swapchain must be
    /// recreated.
    pub unsafe fn render(
        &mut self,
        record: impl FnOnce(vk::CommandBuffer) -> Result<()>,
    ) -> Result<bool> {
        self.device
            .wait_for_fences(&[self.in_flight], true, u64::MAX)?;

        let index = match self.swapchain_loader.acquire_next_image(
            self.swapchain,
            u64::MAX,
            self.image_available,
            vk::Fence::null(),
        ) {
            Ok((i, _suboptimal)) => i,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => return Ok(true),
            Err(e) => return Err(e.into()),
        };
        self.device.reset_fences(&[self.in_flight])?;

        let cmd = self.command_buffer;
        self.device
            .reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
        self.device.begin_command_buffer(
            cmd,
            &vk::CommandBufferBeginInfo::builder()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;

        // Fully transparent clear colour.
        let clear = [vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [0.0, 0.0, 0.0, 0.0],
            },
        }];
        let rp_begin = vk::RenderPassBeginInfo::builder()
            .render_pass(self.render_pass)
            .framebuffer(self.framebuffers[index as usize])
            .render_area(vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: self.extent,
            })
            .clear_values(&clear);
        self.device
            .cmd_begin_render_pass(cmd, &rp_begin, vk::SubpassContents::INLINE);

        // Never skip the submit below, otherwise the fence would never signal again.
        let recorded = record(cmd);

        self.device.cmd_end_render_pass(cmd);
        self.device.end_command_buffer(cmd)?;

        let wait = [self.image_available];
        let stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
        let cmds = [cmd];
        let signal = [self.render_finished[index as usize]];
        let submit = vk::SubmitInfo::builder()
            .wait_semaphores(&wait)
            .wait_dst_stage_mask(&stages)
            .command_buffers(&cmds)
            .signal_semaphores(&signal)
            .build();
        self.device
            .queue_submit(self.queue, &[submit], self.in_flight)?;

        let swapchains = [self.swapchain];
        let indices = [index];
        let present = vk::PresentInfoKHR::builder()
            .wait_semaphores(&signal)
            .swapchains(&swapchains)
            .image_indices(&indices);
        let needs_recreate = match self.swapchain_loader.queue_present(self.queue, &present) {
            Ok(suboptimal) => suboptimal,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => true,
            Err(e) => return Err(e.into()),
        };

        recorded?;
        Ok(needs_recreate)
    }

    pub unsafe fn wait_idle(&self) {
        let _ = self.device.device_wait_idle();
    }

    /// Call after everything that owns Vulkan objects (the ImGui renderer) has been dropped.
    pub unsafe fn destroy(&mut self) {
        let _ = self.device.device_wait_idle();
        self.destroy_swapchain_resources();
        self.swapchain_loader.destroy_swapchain(self.swapchain, None);
        self.device.destroy_fence(self.in_flight, None);
        self.device.destroy_semaphore(self.image_available, None);
        self.device.destroy_command_pool(self.command_pool, None);
        self.device.destroy_render_pass(self.render_pass, None);
        self.device.destroy_device(None);
        self.surface_loader.destroy_surface(self.surface, None);
        self.instance.destroy_instance(None);
    }
}
