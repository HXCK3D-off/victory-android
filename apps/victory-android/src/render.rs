//! The Vulkan device, and the surface the host presents into.
//!
//! The desktop front end asks the console's D3D12.X calls to be performed by
//! host D3D12. On Android those same calls have to reach a Vulkan driver, and
//! this module is where that driver starts: a `VkInstance`, the physical
//! device the phone reports, a device with a graphics and a present queue, and
//! a swap chain on the `ANativeWindow` the activity hands us.
//!
//! What is deliberately *not* here is the D3D12 command translation. That is
//! `victory-gpu`'s host side, which is `cfg(windows)` because it is D3D12 on
//! D3D12; a Vulkan backend is a separate and much larger piece of work than
//! getting pictures onto the screen. This file is the surface that backend
//! will sit on, plus one real graphics pipeline drawn into it every frame so
//! that the surface, the driver's compiler, the descriptor set and the
//! uniform buffer are all exercised rather than assumed.

use std::ffi::c_char;
use std::path::{Path, PathBuf};
use std::time::Instant;

use android_activity::ndk::native_window::NativeWindow;
use anyhow::{Context, Result, anyhow, bail};
use ash::khr;
use ash::vk;
use victory_gpu::vulkan::{self, Blend, D3d12Blend, Graphics, Layouts, Stage, State, Target};

use crate::shader;

/// The API the instance asks for. Vulkan 1.1 is the floor on Android 10 and
/// up; everything above it stays optional.
const API: u32 = vk::API_VERSION_1_1;

/// The formats we would rather present in, best first. sRGB first because the
/// console's target is linear-light and the driver's conversion is free in the
/// render pass.
const PREFERRED_FORMATS: [vk::Format; 4] = [
    vk::Format::B8G8R8A8_SRGB,
    vk::Format::R8G8B8A8_SRGB,
    vk::Format::B8G8R8A8_UNORM,
    vk::Format::R8G8B8A8_UNORM,
];

/// How many swap chain images we will keep, at most. A phone's memory budget
/// is shared with the title, and three 1440p images is already 24 MB.
const MAX_IMAGES: u32 = 3;

/// What a frame did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame {
    /// Acquired, drawn and presented.
    Presented,
    /// The surface no longer matched the window and the swap chain was
    /// rebuilt; the caller should treat this frame as skipped.
    Rebuilt,
}

pub struct Renderer {
    entry: ash::Entry,
    instance: ash::Instance,
    surface_loader: khr::surface::Instance,
    surface: vk::SurfaceKHR,
    physical: vk::PhysicalDevice,
    device: ash::Device,
    swapchain_loader: khr::swapchain::Device,
    queue: vk::Queue,
    queue_family: u32,
    swapchain: vk::SwapchainKHR,
    render_pass: vk::RenderPass,
    extent: vk::Extent2D,
    views: Vec<vk::ImageView>,
    framebuffers: Vec<vk::Framebuffer>,
    command_pool: vk::CommandPool,
    commands: Vec<vk::CommandBuffer>,
    image_available: vk::Semaphore,
    render_finished: Vec<vk::Semaphore>,
    in_flight: vk::Fence,
    pipeline_cache: Option<vk::PipelineCache>,
    cache_path: PathBuf,
    shaders: Shaders,
    layouts: Layouts,
    descriptor_pool: vk::DescriptorPool,
    descriptor_set: vk::DescriptorSet,
    pipeline: vk::Pipeline,
    /// What the shader side came to: the fill's stages and, when the title's
    /// image has one, its multi-target clear.
    plan: shader::Plan,
    memory: vk::PhysicalDeviceMemoryProperties,
    /// The title's own multi-target pass, when its image has a shader for one.
    offscreen: Option<Offscreen>,
    /// The textured draw, when this driver took its image and its sampler. A
    /// device that refuses them loses the tiles, never the frame.
    overlay: Option<Overlay>,
    uniform: vk::Buffer,
    uniform_memory: vk::DeviceMemory,
    uniform_mapped: *mut u8,
    frames: u64,
    started: Instant,
    last_report: Instant,
}

impl Renderer {
    /// Creates the instance, the device and the first swap chain for `window`.
    ///
    /// `cache_dir` receives the Vulkan pipeline cache. It is worth persisting
    /// on Adreno: the driver otherwise recompiles the same pipelines on every
    /// launch, which is seconds of stutter on a phone and exactly the cost
    /// `victory warm` avoids on the desktop.
    pub fn new(window: &NativeWindow, cache_dir: &Path, title_dir: &Path) -> Result<Self> {
        // A failure anywhere below abandons the instance rather than
        // half-destroying it: `victory_main` treats a failed start as fatal
        // and the process is on its way out, so there is nothing to keep.
        let entry = ash::Entry::linked();
        let instance = create_instance(&entry)?;
        let surface = unsafe { create_surface(&entry, &instance, window)? };
        let surface_loader = khr::surface::Instance::new(&entry, &instance);
        let (physical, queue_family) = pick_device(&instance, &surface_loader, surface)?;
        let (device, features) = unsafe { create_device(&instance, physical, queue_family)? };
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let swapchain_loader = khr::swapchain::Device::new(&instance, &device);

        let cache_path = cache_dir.join("vulkan").join("pipeline.bin");
        let pipeline_cache = unsafe { load_pipeline_cache(&device, &cache_path) };

        // Everything the draw needs that does not depend on the size of the
        // window: the shader, the descriptor set that feeds it, and the map it
        // reads its tint from.
        let memory = unsafe { instance.get_physical_device_memory_properties(physical) };
        let plan = shader::compile(features.draw_parameters, Some(title_dir))?;
        // What the modules bind is the only description of the descriptor
        // layouts; the backend reads it rather than assuming the host's one
        // constant buffer at set 0.
        let reflections = plan.fill.reflections();
        let borrowed: Vec<&vulkan::reflect::Module> = reflections.iter().collect();
        let stages = vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT;
        let layouts = vulkan::descriptor::layouts(&device, &borrowed, stages)?;
        let shaders = unsafe { create_shader_modules(&device, &plan.fill) }?;
        let descriptor_pool = vulkan::descriptor::pool(&device, &layouts)?;
        let descriptor_set = unsafe { allocate_set(&device, descriptor_pool, &layouts)? };
        let (uniform, uniform_memory, uniform_mapped) =
            unsafe { create_uniform_buffer(&device, &memory)? };
        unsafe { write_uniform_buffer(&device, descriptor_set, &layouts, uniform)? };

        let size = (window.width().max(1) as u32, window.height().max(1) as u32);
        let built = unsafe {
            build_swapchain(&device, &swapchain_loader, &surface_loader, physical, surface, size)
        }?;
        let state = State::screen(built.format);
        let pipeline = unsafe {
            create_pipeline(
                &device,
                built.render_pass,
                layouts.pipeline,
                &shaders,
                pipeline_cache,
                &state,
            )?
        };
        let (offscreen, overlay) = unsafe {
            build_extras(
                &device,
                &memory,
                queue_family,
                queue,
                &built,
                pipeline_cache,
                &plan,
                uniform,
            )
        };

        let (command_pool, commands) = unsafe {
            create_commands(&device, queue_family, built.framebuffers.len())
        }?;
        let image_available =
            unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }
                .context("creating the frame semaphore")?;
        let render_finished = (0..built.views.len())
            .map(|_| unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) })
            .collect::<Result<Vec<_>, _>>()
            .context("creating the present semaphores")?;
        let in_flight = unsafe {
            device.create_fence(
                &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                None,
            )
        }
        .context("creating the frame fence")?;

        let now = Instant::now();
        Ok(Renderer {
            entry,
            instance,
            surface_loader,
            surface,
            physical,
            device,
            swapchain_loader,
            queue,
            queue_family,
            swapchain: built.swapchain,
            render_pass: built.render_pass,
            extent: built.extent,
            views: built.views,
            framebuffers: built.framebuffers,
            command_pool,
            commands,
            image_available,
            render_finished,
            in_flight,
            pipeline_cache,
            cache_path,
            shaders,
            layouts,
            descriptor_pool,
            descriptor_set,
            pipeline,
            plan,
            memory,
            offscreen,
            overlay,
            uniform,
            uniform_memory,
            uniform_mapped,
            frames: 0,
            started: now,
            last_report: now,
        })
    }

    /// The size of the swap chain's images, in pixels.
    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    /// Rebuilds the swap chain for a new window size. Android does not always
    /// report the surface as out of date when the window changes, so the host
    /// says so explicitly when its window size changes.
    pub fn resize(&mut self, size: (u32, u32)) -> Result<()> {
        if size.0 == 0 || size.1 == 0 {
            return Ok(());
        }
        if size == (self.extent.width, self.extent.height) {
            return Ok(());
        }
        self.rebuild(size, "window resized")
    }

    /// Acquires, draws and presents one frame.
    pub fn draw(&mut self, size: (u32, u32)) -> Result<Frame> {
        if size.0 == 0 || size.1 == 0 {
            return Ok(Frame::Presented);
        }
        unsafe {
            self.device.wait_for_fences(&[self.in_flight], true, u64::MAX)?;
            let (index, suboptimal) = match self.swapchain_loader.acquire_next_image(
                self.swapchain,
                u64::MAX,
                self.image_available,
                vk::Fence::null(),
            ) {
                Ok(pair) => pair,
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                    self.rebuild(size, "acquire out of date")?;
                    return Ok(Frame::Rebuilt);
                }
                Err(e) => return Err(e.into()),
            };
            let index = index as usize;

            self.device.reset_fences(&[self.in_flight])?;
            self.device.reset_command_buffer(
                self.commands[index],
                vk::CommandBufferResetFlags::empty(),
            )?;
            self.write_uniform();
            record(
                &self.device,
                self.commands[index],
                self.render_pass,
                self.framebuffers[index],
                self.extent,
                self.frames,
                self.pipeline,
                self.layouts.pipeline,
                self.descriptor_set,
                self.overlay.as_ref(),
                self.offscreen.as_mut(),
            )?;

            let wait = [self.image_available];
            let stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
            let commands = [self.commands[index]];
            let signal = [self.render_finished[index]];
            let submit = vk::SubmitInfo::default()
                .wait_semaphores(&wait)
                .wait_dst_stage_mask(&stages)
                .command_buffers(&commands)
                .signal_semaphores(&signal);
            self.device
                .queue_submit(self.queue, &[submit], self.in_flight)?;

            let swapchains = [self.swapchain];
            let indices = [index as u32];
            let present = vk::PresentInfoKHR::default()
                .wait_semaphores(&signal)
                .swapchains(&swapchains)
                .image_indices(&indices);
            let presented = self.swapchain_loader.queue_present(self.queue, &present);
            self.frames += 1;
            self.report();
            let out_of_date = matches!(presented, Err(vk::Result::ERROR_OUT_OF_DATE_KHR));
            let suboptimal_present = matches!(presented, Ok(false));
            let resized = size != (self.extent.width, self.extent.height);
            // `VK_SUBOPTIMAL_KHR` is advice, and Adreno gives it every frame
            // while the window's own geometry settles; rebuilding for it, or
            // for a resize that does not change the pixels, is a loop that
            // never draws a frame.
            if out_of_date || resized {
                let why = if out_of_date {
                    "present out of date"
                } else {
                    "window resized"
                };
                self.rebuild(size, why)?;
                return Ok(Frame::Rebuilt);
            }
            if suboptimal_present || suboptimal {
                tracing::debug!(present = suboptimal_present, acquire = suboptimal, "suboptimal");
            }
            presented.map_err(|e| anyhow!("presenting: {e}"))?;
        }
        Ok(Frame::Presented)
    }

    /// Rebuilds the swap chain and the render pass for a new window size, or
    /// for a surface that went out of date. Called from `draw`, never
    /// recursively.
    fn rebuild(&mut self, size: (u32, u32), why: &'static str) -> Result<()> {
        unsafe { self.device.device_wait_idle()? };
        if self.pipeline != vk::Pipeline::null() {
            unsafe { self.device.destroy_pipeline(self.pipeline, None) };
            self.pipeline = vk::Pipeline::null();
        }
        self.destroy_swapchain();
        unsafe { self.device.destroy_render_pass(self.render_pass, None) };
        self.render_pass = vk::RenderPass::null();

        let built = unsafe {
            build_swapchain(
                &self.device,
                &self.swapchain_loader,
                &self.surface_loader,
                self.physical,
                self.surface,
                size,
            )
        }?;

        // The command buffers are one per image and indexed by the acquired
        // image, so a swap chain of a different length needs a new set.
        unsafe {
            self.device
                .reset_command_pool(self.command_pool, vk::CommandPoolResetFlags::empty())?;
        }
        self.commands = unsafe {
            self.device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(self.command_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(built.framebuffers.len() as u32),
            )
        }?;
        while self.render_finished.len() < built.views.len() {
            self.render_finished.push(unsafe {
                self.device
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?
            });
        }

        let state = State::screen(built.format);
        self.pipeline = unsafe {
            create_pipeline(
                &self.device,
                built.render_pass,
                self.layouts.pipeline,
                &self.shaders,
                self.pipeline_cache,
                &state,
            )?
        };
        // The offscreen pass and the textured draw are a function of the
        // window's size, so a rebuilt swap chain gets both again rather than
        // a stale framebuffer.
        if let Some(mut offscreen) = self.offscreen.take() {
            unsafe { offscreen.destroy(&self.device) };
        }
        if let Some(mut overlay) = self.overlay.take() {
            unsafe { overlay.destroy(&self.device) };
        }
        let (offscreen, overlay) = unsafe {
            build_extras(
                &self.device,
                &self.memory,
                self.queue_family,
                self.queue,
                &built,
                self.pipeline_cache,
                &self.plan,
                self.uniform,
            )
        };
        self.offscreen = offscreen;
        self.overlay = overlay;
        self.swapchain = built.swapchain;
        self.render_pass = built.render_pass;
        self.extent = built.extent;
        self.views = built.views;
        self.framebuffers = built.framebuffers;
        tracing::info!(
            why,
            extent = format_args!("{}x{}", self.extent.width, self.extent.height),
            "swap chain rebuilt"
        );
        Ok(())
    }

    /// Writes this frame's tint. The buffer is host-visible and coherent, so
    /// a copy is the whole upload; the fence above has already waited for the
    /// frame that read it last.
    fn write_uniform(&mut self) {
        let t = self.frames as f32 * 0.02;
        let tint = [
            0.55 + 0.45 * t.sin(),
            0.35 + 0.25 * (t * 1.3).cos(),
            0.85,
            1.0,
        ];
        let bits = [
            tint[0].to_bits(),
            tint[1].to_bits(),
            tint[2].to_bits(),
            tint[3].to_bits(),
        ];
        unsafe {
            std::ptr::copy_nonoverlapping(bits.as_ptr().cast::<u8>(), self.uniform_mapped, 16);
        }
    }

    fn report(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_report).as_secs_f64() < 5.0 {
            return;
        }
        let fps = self.frames as f64 / now.duration_since(self.started).as_secs_f64();
        tracing::info!(
            frames = self.frames,
            fps = format_args!("{fps:.1}"),
            extent = format_args!("{}x{}", self.extent.width, self.extent.height),
            queue = self.queue_family,
            "presenting"
        );
        self.last_report = now;
    }

    /// Destroys the swap chain and everything owned through it, leaving the
    /// device and the command pool alone.
    fn destroy_swapchain(&mut self) {
        unsafe {
            for framebuffer in self.framebuffers.drain(..) {
                self.device.destroy_framebuffer(framebuffer, None);
            }
            for view in self.views.drain(..) {
                self.device.destroy_image_view(view, None);
            }
            if self.swapchain != vk::SwapchainKHR::null() {
                self.swapchain_loader.destroy_swapchain(self.swapchain, None);
                self.swapchain = vk::SwapchainKHR::null();
            }
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            if let Some(cache) = self.pipeline_cache.take() {
                save_pipeline_cache(&self.device, cache, &self.cache_path);
                self.device.destroy_pipeline_cache(cache, None);
            }
            if self.pipeline != vk::Pipeline::null() {
                self.device.destroy_pipeline(self.pipeline, None);
            }
            self.destroy_swapchain();
            self.device.destroy_fence(self.in_flight, None);
            for semaphore in self.render_finished.drain(..) {
                self.device.destroy_semaphore(semaphore, None);
            }
            self.device.destroy_semaphore(self.image_available, None);
            self.device.destroy_command_pool(self.command_pool, None);
            if let Some(mut offscreen) = self.offscreen.take() {
                offscreen.destroy(&self.device);
            }
            if let Some(mut overlay) = self.overlay.take() {
                overlay.destroy(&self.device);
            }
            self.layouts.destroy(&self.device);
            self.device.destroy_descriptor_pool(self.descriptor_pool, None);
            self.shaders.destroy(&self.device);
            self.device.unmap_memory(self.uniform_memory);
            self.device.destroy_buffer(self.uniform, None);
            self.device.free_memory(self.uniform_memory, None);
            if self.render_pass != vk::RenderPass::null() {
                self.device.destroy_render_pass(self.render_pass, None);
            }
            self.device.destroy_device(None);
            self.surface_loader.destroy_surface(self.surface, None);
            self.instance.destroy_instance(None);
        }
        // The entry point keeps its handles alive; nothing to destroy for it,
        // but it must not drop before the instance does.
        let _ = &self.entry;
    }
}

/// A swap chain and the render pass that matches its format, which is why the
/// two are built together: a render pass whose attachment format does not
/// match the image is a validation error and an invisible frame on Adreno.
struct Built {
    swapchain: vk::SwapchainKHR,
    format: vk::Format,
    render_pass: vk::RenderPass,
    extent: vk::Extent2D,
    views: Vec<vk::ImageView>,
    framebuffers: Vec<vk::Framebuffer>,
}

fn create_instance(entry: &ash::Entry) -> Result<ash::Instance> {
    let extensions = [
        khr::surface::NAME.as_ptr(),
        khr::android_surface::NAME.as_ptr(),
    ];
    let app = vk::ApplicationInfo::default()
        .application_name(c"victory")
        .application_version(vk::make_api_version(0, 0, 1, 0))
        .engine_name(c"victory-android")
        .engine_version(vk::make_api_version(0, 0, 1, 0))
        .api_version(API);
    let create = vk::InstanceCreateInfo::default()
        .application_info(&app)
        .enabled_extension_names(&extensions);
    let instance =
        unsafe { entry.create_instance(&create, None) }.context("creating the Vulkan instance")?;
    tracing::info!(
        api = format_args!(
            "{}.{}.{}",
            vk::api_version_major(API),
            vk::api_version_minor(API),
            vk::api_version_patch(API)
        ),
        "Vulkan instance"
    );
    Ok(instance)
}

/// The surface wraps the activity's `ANativeWindow`. Vulkan keeps its own
/// reference, so the window need not outlive this call.
unsafe fn create_surface(
    entry: &ash::Entry,
    instance: &ash::Instance,
    window: &NativeWindow,
) -> Result<vk::SurfaceKHR> {
    let loader = khr::android_surface::Instance::new(entry, instance);
    let create =
        vk::AndroidSurfaceCreateInfoKHR::default().window(window.ptr().as_ptr().cast());
    unsafe { loader.create_android_surface(&create, None) }
        .context("creating the Android surface")
}

/// Picks the adapter. There is one interesting GPU on a phone, but which one
/// the driver hands out first differs between OEM builds, so the choice is
/// explicit and logged: a Snapdragon 8 Elite must land on its Adreno, not on a
/// software device.
fn pick_device(
    instance: &ash::Instance,
    surface_loader: &khr::surface::Instance,
    surface: vk::SurfaceKHR,
) -> Result<(vk::PhysicalDevice, u32)> {
    let devices =
        unsafe { instance.enumerate_physical_devices() }.context("enumerating physical devices")?;
    if devices.is_empty() {
        bail!("no Vulkan physical device: the driver or the APK's ABI is missing");
    }

    let mut best: Option<(vk::PhysicalDevice, u32, i32)> = None;
    for device in devices {
        let props = unsafe { instance.get_physical_device_properties(device) };
        let name = cstr_name(&props.device_name);
        let families = unsafe { instance.get_physical_device_queue_family_properties(device) };
        let queue = families.iter().enumerate().find_map(|(i, family)| {
            let i = i as u32;
            let graphics = family.queue_flags.contains(vk::QueueFlags::GRAPHICS);
            let present =
                unsafe { surface_loader.get_physical_device_surface_support(device, i, surface) }
                    .unwrap_or(false);
            (graphics && present).then_some(i)
        });
        let Some(queue) = queue else {
            tracing::info!(device = %name, "skipping: no graphics and present queue");
            continue;
        };

        // Discrete first, integrated next, and a bonus for the vendors whose
        // drivers actually ship in phones; that keeps lavapipe or a debug
        // driver from winning on a developer machine.
        let mut score = match props.device_type {
            vk::PhysicalDeviceType::DISCRETE_GPU => 300,
            vk::PhysicalDeviceType::INTEGRATED_GPU => 200,
            vk::PhysicalDeviceType::VIRTUAL_GPU => 100,
            _ => 0,
        };
        if matches!(props.vendor_id, 0x5143 | 0x13B5 | 0x1010) {
            score += 50;
        }
        tracing::info!(
            device = %name,
            vendor = vendor_name(props.vendor_id),
            vendor_id = format_args!("{:#06x}", props.vendor_id),
            device_id = format_args!("{:#06x}", props.device_id),
            kind = ?props.device_type,
            api = %api_version(props.api_version),
            driver = %driver_version(props.driver_version),
            "Vulkan device"
        );
        if best.is_none_or(|(_, _, s)| score > s) {
            best = Some((device, queue, score));
        }
    }

    let Some((device, family, _)) = best else {
        bail!("no Vulkan device can both draw and present");
    };
    log_limits(instance, device);
    Ok((device, family))
}

/// The numbers the shader translator has to respect. `victory-shader` lowers
/// console code with a host wave size baked in; the desktop default is 64, and
/// on Adreno it is whatever this reports.
fn log_limits(instance: &ash::Instance, device: vk::PhysicalDevice) {
    let mut subgroup = vk::PhysicalDeviceSubgroupProperties::default();
    let mut size_control = vk::PhysicalDeviceSubgroupSizeControlProperties::default();
    // `push_next` takes and returns the struct by value: dropping its result
    // (as `let _ =` does) throws the whole chain away.
    let mut props = vk::PhysicalDeviceProperties2::default()
        .push_next(&mut subgroup)
        .push_next(&mut size_control);
    unsafe {
        instance.get_physical_device_properties2(device, &mut props);
    }
    // The chain borrows both structures mutably, and `props` is not touched
    // again, which is what makes their filled-in fields readable below.
    let limits = props.properties.limits;
    // Adreno leaves `subgroupSize` zero in the core struct and reports the
    // range through `VK_EXT_subgroup_size_control`; use whichever is filled.
    let lanes = if subgroup.subgroup_size > 0 {
        subgroup.subgroup_size
    } else {
        size_control.max_subgroup_size
    };
    tracing::info!(
        subgroup_size = subgroup.subgroup_size,
        max_subgroup_size = size_control.max_subgroup_size,
        lanes,
        max_push_constants = limits.max_push_constants_size,
        max_storage_buffer_range = limits.max_storage_buffer_range,
        max_compute_invocations = limits.max_compute_work_group_invocations,
        max_compute_size_x = limits.max_compute_work_group_size[0],
        max_descriptor_sets = limits.max_bound_descriptor_sets,
        "Vulkan limits"
    );
    if lanes > 0 {
        victory_shader::emit::set_host_max_wave(lanes);
    } else {
        tracing::warn!("no subgroup size reported: keeping the translator's default wave");
    }
}

unsafe fn create_device(
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
    family: u32,
) -> Result<(ash::Device, Features)> {
    // `dxil-spirv` gives a vertex shader `gl_VertexIndex` and, with it, the
    // `DrawParameters` capability, which Vulkan only allows when the device
    // says it supports it. It is optional in 1.1 and Adreno does support it,
    // so ask and enable rather than assume; a device without it falls back to
    // the WGSL transcription.
    let mut query = vk::PhysicalDeviceShaderDrawParametersFeatures::default();
    let mut queried = vk::PhysicalDeviceFeatures2::default().push_next(&mut query);
    unsafe { instance.get_physical_device_features2(physical, &mut queried) };
    let features = Features {
        draw_parameters: query.shader_draw_parameters == vk::TRUE,
    };
    if !features.draw_parameters {
        tracing::warn!("the device has no shader draw parameters: DXIL cannot be translated");
    }

    let mut enable = vk::PhysicalDeviceShaderDrawParametersFeatures::default()
        .shader_draw_parameters(features.draw_parameters);
    let mut enabled = vk::PhysicalDeviceFeatures2::default().push_next(&mut enable);
    let priorities = [1.0f32];
    let queues = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(family)
        .queue_priorities(&priorities)];
    let extensions = [khr::swapchain::NAME.as_ptr()];
    let create = vk::DeviceCreateInfo::default()
        .queue_create_infos(&queues)
        .enabled_extension_names(&extensions)
        .push_next(&mut enabled);
    let device = unsafe { instance.create_device(physical, &create, None) }
        .context("creating the Vulkan device")?;
    Ok((device, features))
}

/// The optional device features this host switches on, and what the shaders
/// have to stay inside.
#[derive(Clone, Copy, Debug)]
struct Features {
    /// `shaderDrawParameters`: what `dxil-spirv`'s vertex prologue needs.
    draw_parameters: bool,
}

/// Creates the render pass, the swap chain, its views and its framebuffers.
/// `size` is the window in pixels; the surface's own `current_extent` wins
/// when the driver reports one, which on Android it usually does.
unsafe fn build_swapchain(
    device: &ash::Device,
    loader: &khr::swapchain::Device,
    surface_loader: &khr::surface::Instance,
    physical: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
    size: (u32, u32),
) -> Result<Built> {
    let caps = unsafe {
        surface_loader.get_physical_device_surface_capabilities(physical, surface)
    }
    .context("surface capabilities")?;
    let formats =
        unsafe { surface_loader.get_physical_device_surface_formats(physical, surface) }
            .context("surface formats")?;
    let modes =
        unsafe { surface_loader.get_physical_device_surface_present_modes(physical, surface) }
            .context("surface present modes")?;

    let format = formats
        .iter()
        .find(|f| {
            PREFERRED_FORMATS.contains(&f.format)
                && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
        })
        .or_else(|| formats.first())
        .copied()
        .ok_or_else(|| anyhow!("the surface reports no formats"))?;

    // FIFO is the one mode guaranteed everywhere and the one that keeps a
    // phone at its panel rate without drawing frames nobody sees. MAILBOX is
    // tempting for latency and costs a frame of bandwidth we would rather
    // spend on the title.
    let present_mode = if modes.contains(&vk::PresentModeKHR::FIFO) {
        vk::PresentModeKHR::FIFO
    } else {
        vk::PresentModeKHR::FIFO_RELAXED
    };

    let extent = if caps.current_extent.width != u32::MAX {
        caps.current_extent
    } else {
        vk::Extent2D {
            width: size.0.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
            height: size.1.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
        }
    };
    if extent.width == 0 || extent.height == 0 {
        bail!("the window has no pixels yet");
    }

    let mut images = (caps.min_image_count + 1).max(2).min(MAX_IMAGES);
    images = images.max(caps.min_image_count);
    if caps.max_image_count > 0 {
        images = images.min(caps.max_image_count);
    }

    // `OPAQUE` is what a full-screen game wants, but it has to be one of the
    // modes the surface lists: Adreno lists only `INHERIT`, and accepts
    // `OPAQUE` anyway, which is not something to rely on.
    let supported = caps.supported_composite_alpha;
    let composite_alpha = [
        vk::CompositeAlphaFlagsKHR::OPAQUE,
        vk::CompositeAlphaFlagsKHR::INHERIT,
        vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
        vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED,
    ]
    .into_iter()
    .find(|mode| supported.contains(*mode))
    .ok_or_else(|| anyhow!("the surface supports no composite alpha mode"))?;

    let render_pass = unsafe { create_render_pass(device, format.format)? };
    let create = vk::SwapchainCreateInfoKHR::default()
        .surface(surface)
        .min_image_count(images)
        .image_format(format.format)
        .image_color_space(format.color_space)
        .image_extent(extent)
        .image_array_layers(1)
        .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
        .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
        .pre_transform(caps.current_transform)
        .composite_alpha(composite_alpha)
        .present_mode(present_mode)
        .clipped(true);
    let swapchain = unsafe { loader.create_swapchain(&create, None) }
        .context("creating the swap chain")?;
    let swapchain_images =
        unsafe { loader.get_swapchain_images(swapchain) }.context("swap chain images")?;

    let mut views = Vec::with_capacity(swapchain_images.len());
    let mut framebuffers = Vec::with_capacity(swapchain_images.len());
    for image in &swapchain_images {
        let view_create = vk::ImageViewCreateInfo::default()
            .image(*image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format.format)
            .components(vk::ComponentMapping::default())
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .base_mip_level(0)
                    .level_count(1)
                    .base_array_layer(0)
                    .layer_count(1),
            );
        let view = unsafe { device.create_image_view(&view_create, None) }
            .context("swap chain image view")?;
        let attachments = [view];
        let framebuffer = unsafe {
            device.create_framebuffer(
                &vk::FramebufferCreateInfo::default()
                    .render_pass(render_pass)
                    .attachments(&attachments)
                    .width(extent.width)
                    .height(extent.height)
                    .layers(1),
                None,
            )
        }
        .context("swap chain framebuffer")?;
        views.push(view);
        framebuffers.push(framebuffer);
    }

    tracing::info!(
        format = ?format.format,
        present_mode = ?present_mode,
        images = views.len(),
        extent = format_args!("{}x{}", extent.width, extent.height),
        transform = ?caps.current_transform,
        composite_alpha = ?composite_alpha,
        min_images = caps.min_image_count,
        max_images = caps.max_image_count,
        "swap chain"
    );

    Ok(Built {
        swapchain,
        format: format.format,
        render_pass,
        extent,
        views,
        framebuffers,
    })
}

unsafe fn create_render_pass(device: &ash::Device, format: vk::Format) -> Result<vk::RenderPass> {
    let attachment = vk::AttachmentDescription::default()
        .format(format)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
        .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .final_layout(vk::ImageLayout::PRESENT_SRC_KHR);
    let reference = vk::AttachmentReference::default()
        .attachment(0)
        .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
    let color = [reference];
    let subpass = vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&color);
    let dependency = vk::SubpassDependency::default()
        .src_subpass(vk::SUBPASS_EXTERNAL)
        .dst_subpass(0)
        .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);
    let attachments = [attachment];
    let subpasses = [subpass];
    let dependencies = [dependency];
    let create = vk::RenderPassCreateInfo::default()
        .attachments(&attachments)
        .subpasses(&subpasses)
        .dependencies(&dependencies);
    unsafe { device.create_render_pass(&create, None) }.context("creating the render pass")
}

unsafe fn create_commands(
    device: &ash::Device,
    family: u32,
    framebuffers: usize,
) -> Result<(vk::CommandPool, Vec<vk::CommandBuffer>)> {
    let pool = unsafe {
        device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        )
    }
    .context("creating the command pool")?;
    let buffers = unsafe {
        device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(framebuffers as u32),
        )
    }
    .context("allocating command buffers")?;
    Ok((pool, buffers))
}

/// Records the whole frame: a clear, the pipeline's triangle, then present.
///
/// The clear alone cannot tell a working pipeline from a working surface; the
/// triangle is what puts the shader compiler, the descriptor set and the
/// uniform buffer on the clock, and it is the shape the console's first draw
/// will arrive in when the D3D12 translation is wired up.
#[allow(clippy::too_many_arguments)]
unsafe fn record(
    device: &ash::Device,
    command: vk::CommandBuffer,
    render_pass: vk::RenderPass,
    framebuffer: vk::Framebuffer,
    extent: vk::Extent2D,
    frame: u64,
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    descriptor_set: vk::DescriptorSet,
    overlay: Option<&Overlay>,
    offscreen: Option<&mut Offscreen>,
) -> Result<()> {
    let begin = vk::CommandBufferBeginInfo::default();
    unsafe { device.begin_command_buffer(command, &begin)? };
    // The title's own multi-target pass fills its attachments first, and this
    // frame's fill samples what it wrote.
    if let Some(offscreen) = offscreen {
        unsafe { offscreen.record(device, command)? };
    }

    // A slow pulse, so a screenshot or a recording shows frames arriving
    // rather than a frozen first frame that looks the same either way.
    let t = frame as f32 * 0.02;
    let clear = [0.05 + 0.05 * t.sin(), 0.09, 0.12 + 0.05 * t.cos(), 1.0];
    let clears = [vk::ClearValue {
        color: vk::ClearColorValue { float32: clear },
    }];
    let area = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent,
    };
    // Dynamic state: the viewport is the window's, not the pipeline's, so a
    // resize does not need a new pipeline.
    let viewport = vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: extent.width as f32,
        height: extent.height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    };
    let pass = vk::RenderPassBeginInfo::default()
        .render_pass(render_pass)
        .framebuffer(framebuffer)
        .render_area(area)
        .clear_values(&clears);
    unsafe {
        device.cmd_begin_render_pass(command, &pass, vk::SubpassContents::INLINE);
        device.cmd_bind_pipeline(command, vk::PipelineBindPoint::GRAPHICS, pipeline);
        device.cmd_set_viewport(command, 0, &[viewport]);
        device.cmd_set_scissor(command, 0, &[area]);
        device.cmd_bind_descriptor_sets(
            command,
            vk::PipelineBindPoint::GRAPHICS,
            pipeline_layout,
            0,
            &[descriptor_set],
            &[],
        );
        device.cmd_draw(command, 3, 1, 0, 0);
        if let Some(overlay) = overlay {
            device.cmd_bind_pipeline(command, vk::PipelineBindPoint::GRAPHICS, overlay.pipeline);
            device.cmd_bind_descriptor_sets(
                command,
                vk::PipelineBindPoint::GRAPHICS,
                overlay.layouts.pipeline,
                0,
                &[overlay.set],
                &[],
            );
            device.cmd_draw(command, 3, 1, 0, 0);
        }
        device.cmd_end_render_pass(command);
        device.end_command_buffer(command)?;
    }
    Ok(())
}

/// Reads a pipeline cache from a previous run. A corrupt or stale file is not
/// an error: Vulkan validates it, and a driver update invalidates it anyway.
unsafe fn load_pipeline_cache(device: &ash::Device, path: &Path) -> Option<vk::PipelineCache> {
    let data = std::fs::read(path).ok();
    let mut create = vk::PipelineCacheCreateInfo::default();
    if let Some(data) = &data {
        create = create.initial_data(data);
    }
    match unsafe { device.create_pipeline_cache(&create, None) } {
        Ok(cache) => {
            if let Some(data) = &data {
                tracing::info!(bytes = data.len(), "loaded the Vulkan pipeline cache");
            }
            Some(cache)
        }
        Err(e) => {
            tracing::warn!(error = %e, "no Vulkan pipeline cache");
            None
        }
    }
}

unsafe fn save_pipeline_cache(device: &ash::Device, cache: vk::PipelineCache, path: &Path) {
    let data = match unsafe { device.get_pipeline_cache_data(cache) } {
        Ok(data) if !data.is_empty() => data,
        Ok(_) => return,
        Err(e) => {
            tracing::warn!(error = %e, "not saving the Vulkan pipeline cache");
            return;
        }
    };
    if let Some(dir) = path.parent()
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        tracing::warn!(error = %e, dir = %dir.display(), "not saving the Vulkan pipeline cache");
        return;
    }
    match std::fs::write(path, &data) {
        Ok(()) => tracing::info!(bytes = data.len(), "saved the Vulkan pipeline cache"),
        Err(e) => tracing::warn!(error = %e, "not saving the Vulkan pipeline cache"),
    }
}

fn cstr_name(name: &[c_char]) -> String {
    let bytes: Vec<u8> = name
        .iter()
        .map(|c| *c as u8)
        .take_while(|b| *b != 0)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn vendor_name(id: u32) -> &'static str {
    match id {
        0x5143 => "Qualcomm",
        0x13B5 => "Arm",
        0x1010 => "Imagination",
        0x10005 => "Samsung",
        0x10DE => "NVIDIA",
        0x1002 => "AMD",
        0x8086 => "Intel",
        _ => "unknown",
    }
}

fn api_version(version: u32) -> String {
    format!(
        "{}.{}.{}",
        vk::api_version_major(version),
        vk::api_version_minor(version),
        vk::api_version_patch(version)
    )
}

/// `driver_version` is the major/minor/patch packing Vulkan defines, which is
/// what a bug report needs; the Adreno blob reports no vendor string instead.
fn driver_version(version: u32) -> String {
    format!(
        "{}.{}.{}",
        (version >> 22) & 0x3ff,
        (version >> 12) & 0x3ff,
        version & 0xfff
    )
}

/// The two stages the pipeline runs. DXIL carries one stage per container, so
/// the halves arrive as separate modules with their own entry points, which is
/// the shape `victory_gpu::vulkan` builds pipelines from.
struct Shaders {
    vertex: Stage,
    fragment: Stage,
}

impl Shaders {
    unsafe fn destroy(&self, device: &ash::Device) {
        unsafe {
            self.vertex.destroy(device);
            self.fragment.destroy(device);
        }
    }
}


/// The format of every attachment the title's multi-target clear fills.
const TARGET_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// The title's own multi-target clear, drawn into attachments of its own
/// before the fill.
///
/// This is what the backend's [`State`] was built to describe and what the
/// host's fixed one-target pipeline could not: a pass with four colour
/// attachments and a shader from the title's image that writes all four. The
/// frame samples the first attachment afterwards, so what the screen shows is
/// that shader's output rather than a copy of the clear colour.
struct Offscreen {
    targets: Vec<vulkan::Image>,
    render_pass: vk::RenderPass,
    framebuffer: vk::Framebuffer,
    layouts: Layouts,
    pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    shaders: Shaders,
    pipeline: vk::Pipeline,
    extent: vk::Extent2D,
    /// Whether the attachments have been rendered into yet: before the first
    /// pass they are in no layout at all.
    first: bool,
}

impl Offscreen {
    unsafe fn new(
        device: &ash::Device,
        memory: &vk::PhysicalDeviceMemoryProperties,
        extent: vk::Extent2D,
        colours: u32,
        modules: &shader::Modules,
        uniform: vk::Buffer,
        cache: Option<vk::PipelineCache>,
    ) -> Result<Self> {
        if !(1..=4).contains(&colours) {
            anyhow::bail!("a pass with {colours} colour attachments is not one this host builds");
        }
        let mut targets = Vec::with_capacity(colours as usize);
        for _ in 0..colours {
            targets.push(unsafe {
                vulkan::Image::target(
                    device,
                    memory,
                    TARGET_FORMAT,
                    extent,
                    vk::SampleCountFlags::TYPE_1,
                )?
            });
        }
        let views: Vec<vk::ImageView> = targets.iter().map(|image| image.view).collect();
        let render_pass = unsafe { create_multi_pass(device, views.len(), TARGET_FORMAT) }?;
        let framebuffer = unsafe {
            let create = vk::FramebufferCreateInfo::default()
                .render_pass(render_pass)
                .attachments(&views)
                .width(extent.width.max(1))
                .height(extent.height.max(1))
                .layers(1);
            device.create_framebuffer(&create, None)
        }
        .context("creating the multi-target framebuffer")?;

        let reflections = modules.reflections();
        let borrowed: Vec<&vulkan::reflect::Module> = reflections.iter().collect();
        let stages = vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT;
        let layouts = vulkan::descriptor::layouts(device, &borrowed, stages)?;
        let shaders = unsafe { create_shader_modules(device, modules) }?;
        let pool = vulkan::descriptor::pool(device, &layouts)?;
        let set = unsafe { allocate_set(device, pool, &layouts)? };
        unsafe { write_uniform_buffer(device, set, &layouts, uniform)? };

        // One target per attachment, each taking the shader's colour as it
        // writes it: a clear does not blend.
        let mut state = State::screen(TARGET_FORMAT);
        state.targets = vec![vulkan::pipeline::Output::opaque(TARGET_FORMAT); colours as usize];
        tracing::info!(
            colours,
            format = ?TARGET_FORMAT,
            extent = format_args!("{}x{}", extent.width, extent.height),
            "the title's multi-target clear has a pass"
        );
        let pipeline = unsafe {
            create_pipeline(device, render_pass, layouts.pipeline, &shaders, cache, &state)
        }?;

        Ok(Offscreen {
            targets,
            render_pass,
            framebuffer,
            layouts,
            pool,
            set,
            shaders,
            pipeline,
            extent,
            first: true,
        })
    }

    /// Records the pass: the barriers into the pass's layout, the clear of
    /// every attachment, the draw, and the barriers that make the result
    /// samplable.
    ///
    /// # Safety
    ///
    /// `command` must be recording.
    unsafe fn record(&mut self, device: &ash::Device, command: vk::CommandBuffer) -> Result<()> {
        for target in &self.targets {
            let from = if self.first {
                vk::ImageLayout::UNDEFINED
            } else {
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
            };
            unsafe {
                target.transition(
                    device,
                    command,
                    from,
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                )
            };
        }

        let clears = vec![
            vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: [0.02, 0.02, 0.04, 1.0],
                },
            };
            self.targets.len()
        ];
        let area = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: self.extent,
        };
        let viewport = vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: self.extent.width as f32,
            height: self.extent.height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        };
        let pass = vk::RenderPassBeginInfo::default()
            .render_pass(self.render_pass)
            .framebuffer(self.framebuffer)
            .render_area(area)
            .clear_values(&clears);
        unsafe {
            device.cmd_begin_render_pass(command, &pass, vk::SubpassContents::INLINE);
            device.cmd_bind_pipeline(command, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
            device.cmd_set_viewport(command, 0, &[viewport]);
            device.cmd_set_scissor(command, 0, &[area]);
            device.cmd_bind_descriptor_sets(
                command,
                vk::PipelineBindPoint::GRAPHICS,
                self.layouts.pipeline,
                0,
                &[self.set],
                &[],
            );
            device.cmd_draw(command, 3, 1, 0, 0);
            device.cmd_end_render_pass(command);
        }

        // The pass left the attachments in the sampling layout; this makes what
        // it wrote visible to the fragment stage that reads them next.
        for target in &self.targets {
            unsafe {
                target.transition(
                    device,
                    command,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                )
            };
        }
        self.first = false;
        Ok(())
    }

    /// The view the fill samples: the first attachment.
    fn texture(&self) -> (vk::ImageView, vk::Sampler) {
        (self.targets[0].view, self.targets[0].sampler)
    }

    /// # Safety
    ///
    /// No submitted command buffer may still use any of it.
    unsafe fn destroy(&mut self, device: &ash::Device) {
        unsafe {
            if self.pipeline != vk::Pipeline::null() {
                device.destroy_pipeline(self.pipeline, None);
                self.pipeline = vk::Pipeline::null();
            }
            self.shaders.destroy(device);
            self.layouts.destroy(device);
            device.destroy_descriptor_pool(self.pool, None);
            device.destroy_framebuffer(self.framebuffer, None);
            device.destroy_render_pass(self.render_pass, None);
            for target in &self.targets {
                target.destroy(device);
            }
        }
    }
}

/// A render pass with `colours` attachments, each cleared, stored and left in
/// the sampling layout so the pass that follows can read them.
unsafe fn create_multi_pass(
    device: &ash::Device,
    colours: usize,
    format: vk::Format,
) -> Result<vk::RenderPass> {
    let attachments: Vec<vk::AttachmentDescription> = (0..colours)
        .map(|_| {
            vk::AttachmentDescription::default()
                .format(format)
                .samples(vk::SampleCountFlags::TYPE_1)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
                .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
                .initial_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .final_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        })
        .collect();
    let references: Vec<vk::AttachmentReference> = (0..colours)
        .map(|at| {
            vk::AttachmentReference::default()
                .attachment(at as u32)
                .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        })
        .collect();
    let subpasses = [vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&references)];
    let dependencies = [vk::SubpassDependency::default()
        .src_subpass(vk::SUBPASS_EXTERNAL)
        .dst_subpass(0)
        .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)];
    let create = vk::RenderPassCreateInfo::default()
        .attachments(&attachments)
        .subpasses(&subpasses)
        .dependencies(&dependencies);
    unsafe { device.create_render_pass(&create, None) }.context("creating the multi-target pass")
}

/// Everything that hangs off a swap chain rather than off the device: the
/// title's multi-target pass when its image has a shader for one, and the
/// textured draw that goes over the fill. Both are a function of the window's
/// size, so a rebuilt swap chain asks for both again.
unsafe fn build_extras(
    device: &ash::Device,
    memory: &vk::PhysicalDeviceMemoryProperties,
    family: u32,
    queue: vk::Queue,
    built: &Built,
    cache: Option<vk::PipelineCache>,
    plan: &shader::Plan,
    uniform: vk::Buffer,
) -> (Option<Offscreen>, Option<Overlay>) {
    let offscreen = match &plan.multi {
        Some((colours, modules)) => {
            match unsafe {
                Offscreen::new(
                    device,
                    memory,
                    built.extent,
                    *colours,
                    modules,
                    uniform,
                    cache,
                )
            } {
                Ok(pass) => Some(pass),
                Err(error) => {
                    tracing::warn!(%error, "the title's multi-target pass is off");
                    None
                }
            }
        }
        None => None,
    };
    let texture = offscreen.as_ref().map(Offscreen::texture);
    let overlay = match unsafe {
        Overlay::new(
            device,
            memory,
            family,
            queue,
            built.render_pass,
            built.format,
            cache,
            texture,
        )
    } {
        Ok(overlay) => Some(overlay),
        Err(error) => {
            tracing::warn!(%error, "the textured draw is off");
            None
        }
    };
    (offscreen, overlay)
}

/// The size of the image the textured draw samples, in texels.
const TILES: u32 = 256;

/// The blend state the textured draw asks for, as a title's PSO would write it:
/// `SrcBlend = SRC_ALPHA`, `DestBlend = INV_SRC_ALPHA`, `ADD`, alpha `ONE` over
/// `ZERO`. The translation from these numbers is what draws the tiles, so the
/// frame proves the description reached the driver.
const OVER_IT: D3d12Blend = D3d12Blend {
    enable: true,
    source_colour: 5,
    destination_colour: 6,
    operation_colour: 1,
    source_alpha: 2,
    destination_alpha: 1,
    operation_alpha: 1,
};

/// The state a description describes: one target in the pass's own format, and
/// the blend the numbers ask for.
fn described_state(format: vk::Format, blend: D3d12Blend) -> Result<State> {
    let mut state = State::screen(format);
    state.targets[0].blend = Blend::d3d12(blend).context("translating a blend description")?;
    Ok(state)
}

/// The textured draw that goes over the title's clear.
///
/// Every piece of the backend's texture path runs through it: the upload and
/// its barrier happen once in [`Overlay::new`], the set comes from the
/// reflection, and the draw samples the image every frame. A phone that shows
/// the tiles is a phone whose driver took the image, the sampler and the
/// transition between two layouts.
struct Overlay {
    /// The tiles this host generates, when there is no offscreen pass to sample
    /// instead: a title's own render output always wins over them.
    image: Option<vulkan::Image>,
    layouts: Layouts,
    pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    shaders: Shaders,
    pipeline: vk::Pipeline,
}

impl Overlay {
    unsafe fn new(
        device: &ash::Device,
        memory: &vk::PhysicalDeviceMemoryProperties,
        family: u32,
        queue: vk::Queue,
        render_pass: vk::RenderPass,
        colour_format: vk::Format,
        cache: Option<vk::PipelineCache>,
        texture: Option<(vk::ImageView, vk::Sampler)>,
    ) -> Result<Self> {
        // A title's own render output, or the tiles this host generates.
        let generated = match texture {
            Some(_) => None,
            None => {
                let pixels = tiles(TILES);
                let copy = unsafe { vulkan::Copy::new(device, family)? };
                let uploaded = unsafe {
                    vulkan::Image::upload(device, memory, &copy, queue, &pixels, TILES, TILES)
                };
                unsafe { copy.destroy(device) };
                let image = uploaded?;
                let (format, extent) = image.shape();
                tracing::info!(
                    format = ?format,
                    extent = format_args!("{}x{}", extent.width, extent.height),
                    texels = pixels.len() / 4,
                    "uploaded a sampleable image"
                );
                Some(image)
            }
        };
        let (view, sampler) = match (&generated, texture) {
            (_, Some(texture)) => texture,
            (Some(image), None) => (image.view, image.sampler),
            (None, None) => unreachable!("the tiles are made when nothing else is sampled"),
        };

        let modules = shader::textured()?;
        let reflections = modules.reflections();
        let borrowed: Vec<&vulkan::reflect::Module> = reflections.iter().collect();
        let stages = vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT;
        let layouts = vulkan::descriptor::layouts(device, &borrowed, stages)?;
        let shaders = unsafe { create_shader_modules(device, &modules) }?;
        let pool = vulkan::descriptor::pool(device, &layouts)?;
        let set = unsafe { allocate_set(device, pool, &layouts)? };
        unsafe { vulkan::descriptor::bind_image(device, set, layouts.set(0), view, sampler)? };
        let blend = OVER_IT;
        let state = described_state(colour_format, blend)?;
        tracing::info!(
            format = ?state.targets[0].format,
            source = ?state.targets[0].blend.source_colour,
            destination = ?state.targets[0].blend.destination_colour,
            cull = ?state.raster.cull,
            topology = ?state.topology,
            samples = ?state.samples,
            "the textured draw's state comes from a description"
        );
        let pipeline = unsafe {
            create_pipeline(device, render_pass, layouts.pipeline, &shaders, cache, &state)
        }?;
        Ok(Overlay {
            image: generated,
            layouts,
            pool,
            set,
            shaders,
            pipeline,
        })
    }

    unsafe fn destroy(&mut self, device: &ash::Device) {
        unsafe {
            if self.pipeline != vk::Pipeline::null() {
                device.destroy_pipeline(self.pipeline, None);
                self.pipeline = vk::Pipeline::null();
            }
            self.shaders.destroy(device);
            self.layouts.destroy(device);
            device.destroy_descriptor_pool(self.pool, None);
            if let Some(image) = &self.image {
                image.destroy(device);
            }
        }
    }
}

/// The image the textured draw samples: tiles lit at their middle and dark at
/// their edge, warm against blue.
///
/// Every texel is a function of its own coordinates, so the pattern comes from
/// this file rather than from the title's data, and a screenshot shows both
/// that the upload arrived intact and that the sampler filtered it.
fn tiles(size: u32) -> Vec<u8> {
    const CELL: u32 = 32;
    let mut pixels = Vec::with_capacity(size as usize * size as usize * 4);
    for y in 0..size {
        for x in 0..size {
            let lit = (x / CELL + y / CELL) % 2 == 0;
            let across = (x % CELL) as f32 / CELL as f32;
            let down = (y % CELL) as f32 / CELL as f32;
            let edge = across.min(1.0 - across).min(down).min(1.0 - down) * 4.0;
            let base = if lit { 0.95 } else { 0.2 };
            let level = base * (0.3 + 0.7 * edge.min(1.0));
            pixels.extend_from_slice(&[
                (255.0 * level).round() as u8,
                (255.0 * level * 0.72).round() as u8,
                (255.0 * (1.0 - level) * 0.85).round() as u8,
                255,
            ]);
        }
    }
    pixels
}

unsafe fn create_shader_modules(
    device: &ash::Device,
    modules: &shader::Modules,
) -> Result<Shaders> {
    let stage = |stage: &shader::Stage, what: &str| {
        unsafe { Stage::new(device, &stage.words, stage.entry.clone()) }
            .with_context(|| format!("creating the {what} shader module"))
    };
    Ok(Shaders {
        vertex: stage(&modules.vertex, "vertex")?,
        fragment: stage(&modules.fragment, "fragment")?,
    })
}

/// Allocates the single set the host's draw uses.
unsafe fn allocate_set(
    device: &ash::Device,
    pool: vk::DescriptorPool,
    layouts: &Layouts,
) -> Result<vk::DescriptorSet> {
    let handles = layouts.handles();
    let create = vk::DescriptorSetAllocateInfo::default()
        .descriptor_pool(pool)
        .set_layouts(&handles);
    let mut sets = unsafe { device.allocate_descriptor_sets(&create) }
        .context("allocating the descriptor set")?;
    match sets.len() {
        1 => sets
            .pop()
            .ok_or_else(|| anyhow!("the driver allocated no descriptor set")),
        count => Err(anyhow!(
            "the shaders bind {count} descriptor sets, and this host feeds one"
        )),
    }
}

/// The tint buffer: sixteen bytes, host-visible, mapped for the renderer's
/// life. Small and permanent on purpose, because it is the one thing the draw
/// needs that is not a function of the swap chain.
unsafe fn create_uniform_buffer(
    device: &ash::Device,
    memory: &vk::PhysicalDeviceMemoryProperties,
) -> Result<(vk::Buffer, vk::DeviceMemory, *mut u8)> {
    const SIZE: u64 = 16;
    let create = vk::BufferCreateInfo::default()
        .size(SIZE)
        .usage(vk::BufferUsageFlags::UNIFORM_BUFFER)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let buffer = unsafe { device.create_buffer(&create, None) }
        .context("creating the uniform buffer")?;
    let needs = unsafe { device.get_buffer_memory_requirements(buffer) };
    let index = memory_type_index(
        memory,
        needs.memory_type_bits,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
    )
    .ok_or_else(|| anyhow!("no host-visible memory type for the uniform buffer"))?;
    let allocate = vk::MemoryAllocateInfo::default()
        .allocation_size(needs.size)
        .memory_type_index(index);
    let allocated = unsafe { device.allocate_memory(&allocate, None) }
        .context("allocating the uniform buffer's memory")?;
    unsafe { device.bind_buffer_memory(buffer, allocated, 0) }
        .context("binding the uniform buffer's memory")?;
    let mapped = unsafe { device.map_memory(allocated, 0, SIZE, vk::MemoryMapFlags::empty()) }
        .context("mapping the uniform buffer")?;
    Ok((buffer, allocated, mapped.cast::<u8>()))
}

/// Points the set's constant-buffer binding at `buffer`.
///
/// The slot comes from the reflection, not from a constant: a module that puts
/// its constant buffer somewhere else says so in its own SPIR-V.
unsafe fn write_uniform_buffer(
    device: &ash::Device,
    set: vk::DescriptorSet,
    layouts: &Layouts,
    buffer: vk::Buffer,
) -> Result<()> {
    let binding = layouts
        .set(0)
        .iter()
        .find(|binding| binding.kind == vulkan::reflect::Kind::UniformBuffer)
        .copied()
        .ok_or_else(|| anyhow!("the shaders bind no constant buffer"))?;
    let info = [vk::DescriptorBufferInfo::default()
        .buffer(buffer)
        .offset(0)
        .range(vk::WHOLE_SIZE)];
    let writes = [vk::WriteDescriptorSet::default()
        .dst_set(set)
        .dst_binding(binding.binding)
        .descriptor_type(binding.kind.descriptor_type())
        .buffer_info(&info)];
    unsafe { device.update_descriptor_sets(&writes, &[]) };
    Ok(())
}

/// The one pipeline, built by the backend that every Android frame runs
/// through: `victory-gpu`'s Vulkan executor, which is where the title's own
/// pipeline state will arrive.
unsafe fn create_pipeline(
    device: &ash::Device,
    render_pass: vk::RenderPass,
    layout: vk::PipelineLayout,
    shaders: &Shaders,
    cache: Option<vk::PipelineCache>,
    state: &State,
) -> Result<vk::Pipeline> {
    let mut target = Target::pass(render_pass, layout);
    target.cache = cache.unwrap_or(vk::PipelineCache::null());
    unsafe {
        victory_gpu::vulkan::pipeline::create(
            device,
            &Graphics {
                target: &target,
                state,
                vertex: &shaders.vertex,
                fragment: &shaders.fragment,
            },
        )
    }
}

fn memory_type_index(
    memory: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    flags: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..memory.memory_type_count).find(|index| {
        bits & (1 << index) != 0
            && memory.memory_types[*index as usize]
                .property_flags
                .contains(flags)
    })
}
