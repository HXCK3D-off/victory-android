//! Images, samplers, and the one-shot path that gets pixels onto the device.
//!
//! A descriptor set can point at an image, but something has to make the image
//! first, copy the texels in, and move it from `UNDEFINED` to
//! `SHADER_READ_ONLY_OPTIMAL` before a shader samples it. That is a staging
//! buffer, a copy, two barriers and a wait — all of it once, outside a frame,
//! which is why it gets its own command pool instead of squeezing into the
//! frame's buffers.
//!
//! This is the smallest complete texture path: RGBA8, one mip, no array
//! layers. A title's own texture will arrive with its format and its mip chain
//! and its resource state to translate, and it will go through the same
//! upload, the same barriers and the same sampler.

use anyhow::{Context, Result, anyhow, bail};
use ash::vk;

/// How the texel format is laid out, which the upload checks against its
/// pixels and the image view names.
const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// Four bytes of RGBA8 in, per texel.
pub const TEXEL: usize = 4;

/// A command pool for work that happens once and waits for itself.
pub struct Copy {
    pool: vk::CommandPool,
}

impl Copy {
    /// # Safety
    ///
    /// The device must be alive, and `family` one of its queue families.
    pub unsafe fn new(device: &ash::Device, family: u32) -> Result<Self> {
        let create = vk::CommandPoolCreateInfo::default()
            .queue_family_index(family)
            .flags(vk::CommandPoolCreateFlags::TRANSIENT);
        let pool = unsafe { device.create_command_pool(&create, None) }
            .context("creating the copy command pool")?;
        Ok(Copy { pool })
    }

    /// Records `record` into a fresh one-shot buffer, submits it, and waits for
    /// the queue to finish it.
    ///
    /// # Safety
    ///
    /// `queue` must belong to the family the pool was made for.
    pub unsafe fn run<F>(&self, device: &ash::Device, queue: vk::Queue, record: F) -> Result<()>
    where
        F: FnOnce(vk::CommandBuffer),
    {
        let allocate = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let mut buffers = unsafe { device.allocate_command_buffers(&allocate) }
            .context("allocating a copy command buffer")?;
        let command = buffers
            .pop()
            .ok_or_else(|| anyhow!("no copy command buffer"))?;

        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        let done = (|| -> Result<()> {
            unsafe {
                device.begin_command_buffer(command, &begin)?;
                record(command);
                device.end_command_buffer(command)?;
                let commands = [command];
                let submit = vk::SubmitInfo::default().command_buffers(&commands);
                device.queue_submit(queue, &[submit], vk::Fence::null())?;
                device.queue_wait_idle(queue)?;
            }
            Ok(())
        })();
        unsafe {
            device.free_command_buffers(self.pool, &[command]);
        }
        done.context("running a copy")
    }

    /// # Safety
    ///
    /// Nothing recorded through this pool may still be running.
    pub unsafe fn destroy(&self, device: &ash::Device) {
        unsafe { device.destroy_command_pool(self.pool, None) };
    }
}

/// An image the shaders sample, its view, and the sampler that reads it.
pub struct Image {
    pub image: vk::Image,
    pub view: vk::ImageView,
    pub sampler: vk::Sampler,
    memory: vk::DeviceMemory,
    format: vk::Format,
    extent: vk::Extent2D,
}

impl Image {
    /// The format and size the image was made with.
    pub fn shape(&self) -> (vk::Format, vk::Extent2D) {
        (self.format, self.extent)
    }

    /// An image a pass renders into and a shader then samples: colour
    /// attachment, transfer source, and one view of it. It starts in no layout
    /// at all, so a recorded barrier has to put it in the one the pass wants.
    ///
    /// # Safety
    ///
    /// The device must be alive; the image's memory is bound but never written
    /// until something renders into it.
    pub unsafe fn target(
        device: &ash::Device,
        memory: &vk::PhysicalDeviceMemoryProperties,
        format: vk::Format,
        extent: vk::Extent2D,
        samples: vk::SampleCountFlags,
    ) -> Result<Self> {
        if extent.width == 0 || extent.height == 0 {
            bail!(
                "a render target needs a size, and {}x{} is not one",
                extent.width,
                extent.height
            );
        }
        let create = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(samples)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(
                vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::SAMPLED
                    | vk::ImageUsageFlags::TRANSFER_SRC,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image =
            unsafe { device.create_image(&create, None) }.context("creating a render target")?;
        let needs = unsafe { device.get_image_memory_requirements(image) };
        let allocated = unsafe {
            allocate(
                device,
                memory,
                needs,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
                "a render target",
            )
        }?;
        if let Err(error) = unsafe { device.bind_image_memory(image, allocated, 0) } {
            unsafe {
                device.destroy_image(image, None);
                device.free_memory(allocated, None);
            }
            return Err(error).context("binding a render target's memory");
        }

        let view_create = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .base_mip_level(0)
                    .level_count(1)
                    .base_array_layer(0)
                    .layer_count(1),
            );
        let view = match unsafe { device.create_image_view(&view_create, None) } {
            Ok(view) => view,
            Err(error) => {
                unsafe {
                    device.destroy_image(image, None);
                    device.free_memory(allocated, None);
                }
                return Err(error).context("creating a render target's view");
            }
        };
        let sampler_create = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .max_lod(0.0);
        let sampler = match unsafe { device.create_sampler(&sampler_create, None) } {
            Ok(sampler) => sampler,
            Err(error) => {
                unsafe {
                    device.destroy_image_view(view, None);
                    device.destroy_image(image, None);
                    device.free_memory(allocated, None);
                }
                return Err(error).context("creating a render target's sampler");
            }
        };

        Ok(Image {
            image,
            view,
            sampler,
            memory: allocated,
            format,
            extent,
        })
    }

    /// Records the barrier that moves this image between the two layouts a
    /// rendered-then-sampled image lives in.
    ///
    /// # Safety
    ///
    /// `command` must be recording, and no earlier command may still be reading
    /// or writing the image.
    pub unsafe fn transition(
        &self,
        device: &ash::Device,
        command: vk::CommandBuffer,
        from: vk::ImageLayout,
        to: vk::ImageLayout,
    ) {
        let (source_stage, target_stage, source_access, target_access) = match (from, to) {
            (_, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL) => (
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                vk::AccessFlags::SHADER_READ,
            ),
            (_, vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL) => (
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags::SHADER_READ,
                vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            ),
            _ => (
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags::empty(),
                vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            ),
        };
        let barriers = [barrier(self.image, from, to, source_access, target_access)];
        unsafe {
            device.cmd_pipeline_barrier(
                command,
                source_stage,
                target_stage,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &barriers,
            );
        }
    }

    /// Uploads RGBA8 `pixels` and leaves the image ready to sample.
    ///
    /// # Safety
    ///
    /// `copy` and `queue` must be usable together, and the device alive.
    pub unsafe fn upload(
        device: &ash::Device,
        memory: &vk::PhysicalDeviceMemoryProperties,
        copy: &Copy,
        queue: vk::Queue,
        pixels: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Self> {
        if width == 0 || height == 0 {
            bail!("an image needs a size, and {width}x{height} is not one");
        }
        let expected = width as usize * height as usize * TEXEL;
        if pixels.len() != expected {
            bail!(
                "RGBA8 {width}x{height} is {expected} bytes, and {} were given",
                pixels.len()
            );
        }
        let extent = vk::Extent2D { width, height };
        let bytes = expected as u64;

        // The texels reach the image through a buffer the host can see, because
        // a device-local image is usually not host-visible at all.
        let staging_create = vk::BufferCreateInfo::default()
            .size(bytes)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let staging = unsafe { device.create_buffer(&staging_create, None) }
            .context("creating the image's staging buffer")?;
        let needs = unsafe { device.get_buffer_memory_requirements(staging) };
        let staging_memory = unsafe {
            allocate(
                device,
                memory,
                needs,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                "the image's staging buffer",
            )
        }?;
        unsafe { device.bind_buffer_memory(staging, staging_memory, 0) }
            .context("binding the staging buffer's memory")?;
        {
            let mapped =
                unsafe { device.map_memory(staging_memory, 0, bytes, vk::MemoryMapFlags::empty()) }
                    .context("mapping the staging buffer")?;
            unsafe {
                std::ptr::copy_nonoverlapping(pixels.as_ptr(), mapped.cast::<u8>(), pixels.len());
                device.unmap_memory(staging_memory);
            }
        }

        let image_create = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(FORMAT)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { device.create_image(&image_create, None) }
            .context("creating the sampleable image")?;
        let needs = unsafe { device.get_image_memory_requirements(image) };
        let image_memory = unsafe {
            allocate(
                device,
                memory,
                needs,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
                "the sampleable image",
            )
        }?;
        unsafe { device.bind_image_memory(image, image_memory, 0) }
            .context("binding the image's memory")?;

        // One submit: undefined to a copy's layout, the copy, then the layout a
        // shader samples from. The barriers say what they are for, so the wait
        // on this queue is enough to order them.
        let recorded = unsafe {
            copy.run(device, queue, |command| {
                let to_copy = [barrier(
                    image,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::TRANSFER_WRITE,
                )];
                device.cmd_pipeline_barrier(
                    command,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &to_copy,
                );
                let region = [vk::BufferImageCopy::default()
                    .buffer_offset(0)
                    .buffer_row_length(0)
                    .buffer_image_height(0)
                    .image_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .mip_level(0)
                            .base_array_layer(0)
                            .layer_count(1),
                    )
                    .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
                    .image_extent(vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    })];
                device.cmd_copy_buffer_to_image(
                    command,
                    staging,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &region,
                );
                let to_sample = [barrier(
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::AccessFlags::TRANSFER_WRITE,
                    vk::AccessFlags::SHADER_READ,
                )];
                device.cmd_pipeline_barrier(
                    command,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &to_sample,
                );
            })
        };
        unsafe {
            device.destroy_buffer(staging, None);
            device.free_memory(staging_memory, None);
        }
        if let Err(error) = recorded {
            unsafe {
                device.destroy_image(image, None);
                device.free_memory(image_memory, None);
            }
            return Err(error);
        }

        let view_create = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(FORMAT)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .base_mip_level(0)
                    .level_count(1)
                    .base_array_layer(0)
                    .layer_count(1),
            );
        let view = unsafe { device.create_image_view(&view_create, None) }
            .context("creating the image's view")?;
        let sampler_create = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .max_lod(0.0);
        let sampler = match unsafe { device.create_sampler(&sampler_create, None) } {
            Ok(sampler) => sampler,
            Err(e) => {
                unsafe {
                    device.destroy_image_view(view, None);
                    device.destroy_image(image, None);
                    device.free_memory(image_memory, None);
                }
                return Err(e).context("creating the image's sampler");
            }
        };

        Ok(Image {
            image,
            view,
            sampler,
            memory: image_memory,
            format: FORMAT,
            extent,
        })
    }

    /// # Safety
    ///
    /// No submitted command buffer may still read this image.
    pub unsafe fn destroy(&self, device: &ash::Device) {
        unsafe {
            device.destroy_sampler(self.sampler, None);
            device.destroy_image_view(self.view, None);
            device.destroy_image(self.image, None);
            device.free_memory(self.memory, None);
        }
    }
}

/// A buffer a shader reads as texels: how a title's vertex stage gets its
/// vertices, and how any `Buffer` SRV arrives.
///
/// The texels are written straight into host-visible memory, because the only
/// thing that reads them is a shader and the only thing that writes them is the
/// CPU; a title's own vertex buffer will want a staging copy once its uploads
/// are real.
pub struct Texels {
    pub buffer: vk::Buffer,
    pub view: vk::BufferView,
    memory: vk::DeviceMemory,
    len: u64,
}

impl Texels {
    /// How many bytes the buffer holds.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Uploads `bytes` as texels of `format`.
    ///
    /// # Safety
    ///
    /// The device must be alive, and `format` a format the driver allows in a
    /// uniform texel buffer view.
    pub unsafe fn upload(
        device: &ash::Device,
        memory: &vk::PhysicalDeviceMemoryProperties,
        bytes: &[u8],
        format: vk::Format,
    ) -> Result<Self> {
        if bytes.is_empty() {
            bail!("a texel buffer needs at least one texel");
        }
        let len = bytes.len() as u64;
        let create = vk::BufferCreateInfo::default()
            .size(len)
            .usage(vk::BufferUsageFlags::UNIFORM_TEXEL_BUFFER)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer =
            unsafe { device.create_buffer(&create, None) }.context("creating a texel buffer")?;
        let needs = unsafe { device.get_buffer_memory_requirements(buffer) };
        let allocated = unsafe {
            allocate(
                device,
                memory,
                needs,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                "a texel buffer",
            )
        }?;
        if let Err(error) = unsafe { device.bind_buffer_memory(buffer, allocated, 0) } {
            unsafe {
                device.destroy_buffer(buffer, None);
                device.free_memory(allocated, None);
            }
            return Err(error).context("binding a texel buffer's memory");
        }
        {
            let mapped = match unsafe {
                device.map_memory(allocated, 0, len, vk::MemoryMapFlags::empty())
            } {
                Ok(mapped) => mapped,
                Err(error) => {
                    unsafe {
                        device.destroy_buffer(buffer, None);
                        device.free_memory(allocated, None);
                    }
                    return Err(error).context("mapping a texel buffer");
                }
            };
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), mapped.cast::<u8>(), bytes.len());
                device.unmap_memory(allocated);
            }
        }
        let view_create = vk::BufferViewCreateInfo::default()
            .buffer(buffer)
            .format(format)
            .offset(0)
            .range(len);
        let view = match unsafe { device.create_buffer_view(&view_create, None) } {
            Ok(view) => view,
            Err(error) => {
                unsafe {
                    device.destroy_buffer(buffer, None);
                    device.free_memory(allocated, None);
                }
                return Err(error).context("creating a texel buffer's view");
            }
        };
        Ok(Texels {
            buffer,
            view,
            memory: allocated,
            len,
        })
    }

    /// # Safety
    ///
    /// No submitted command buffer may still read it.
    pub unsafe fn destroy(&self, device: &ash::Device) {
        unsafe {
            device.destroy_buffer_view(self.view, None);
            device.destroy_buffer(self.buffer, None);
            device.free_memory(self.memory, None);
        }
    }
}

/// A barrier between the two layouts an upload passes through. The stage masks
/// that scope it belong to the `cmd_pipeline_barrier` call, not to the barrier.
fn barrier(
    image: vk::Image,
    from: vk::ImageLayout,
    to: vk::ImageLayout,
    source_access: vk::AccessFlags,
    target_access: vk::AccessFlags,
) -> vk::ImageMemoryBarrier<'static> {
    vk::ImageMemoryBarrier::default()
        .old_layout(from)
        .new_layout(to)
        .src_access_mask(source_access)
        .dst_access_mask(target_access)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .base_mip_level(0)
                .level_count(1)
                .base_array_layer(0)
                .layer_count(1),
        )
}

/// Allocates memory of the first type that satisfies both `needs` and `flags`.
///
/// # Safety
///
/// `memory` must describe the device `needs` came from.
unsafe fn allocate(
    device: &ash::Device,
    memory: &vk::PhysicalDeviceMemoryProperties,
    needs: vk::MemoryRequirements,
    flags: vk::MemoryPropertyFlags,
    what: &str,
) -> Result<vk::DeviceMemory> {
    let index = (0..memory.memory_type_count)
        .find(|index| {
            needs.memory_type_bits & (1 << index) != 0
                && memory.memory_types[*index as usize]
                    .property_flags
                    .contains(flags)
        })
        .ok_or_else(|| anyhow!("no memory type for {what}"))?;
    let allocate = vk::MemoryAllocateInfo::default()
        .allocation_size(needs.size)
        .memory_type_index(index);
    unsafe { device.allocate_memory(&allocate, None) }
        .with_context(|| format!("allocating memory for {what}"))
}
