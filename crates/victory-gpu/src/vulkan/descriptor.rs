//! Descriptor sets, laid out from what the modules asked for.
//!
//! The backend never writes a binding table by hand: it asks each translated
//! module what it binds ([`reflect`]) and builds the layouts from the union. A
//! title's shader then works when the host has something to put in each slot,
//! which is the only remaining problem, and the one the draw has to solve
//! anyway.
//!
//! Bindless arrays are the case worth getting right early: an array binding's
//! declared length is what the layout reserves and what the set gets allocated
//! with, so a title that indexes a descriptor table costs one binding instead
//! of one per resource. A table with no declared length reserves
//! [`reflect::UNBOUNDED_CAP`], because Vulkan's layout wants a number and the
//! writer has to fill that many.

use anyhow::{Context, Result};
use ash::vk;

use super::reflect::{self, Binding};

/// One set's layout, and what it expects in each slot.
pub struct SetLayout {
    pub layout: vk::DescriptorSetLayout,
    pub bindings: Vec<Binding>,
}

/// Every set the modules use, and the pipeline layout that goes with them.
pub struct Layouts {
    pub sets: Vec<SetLayout>,
    pub pipeline: vk::PipelineLayout,
    /// The push-constant size the modules declared, in bytes.
    pub push_constants: u32,
}

impl Layouts {
    /// The set layouts, in order, for `vkAllocateDescriptorSets`.
    pub fn handles(&self) -> Vec<vk::DescriptorSetLayout> {
        self.sets.iter().map(|set| set.layout).collect()
    }

    /// The bindings of one set, which is what the writes have to fill.
    pub fn set(&self, index: usize) -> &[Binding] {
        self.sets
            .get(index)
            .map_or(&[], |set| set.bindings.as_slice())
    }

    /// # Safety
    ///
    /// The layouts must not be in use by a submitted command buffer.
    pub unsafe fn destroy(&self, device: &ash::Device) {
        unsafe {
            device.destroy_pipeline_layout(self.pipeline, None);
            for set in &self.sets {
                device.destroy_descriptor_set_layout(set.layout, None);
            }
        }
    }
}

/// Builds the set layouts and the pipeline layout for `modules`.
///
/// `stages` is what the sets are visible to; the host's draws are one vertex
/// and one fragment stage, and a title's compute work would say compute.
pub fn layouts(
    device: &ash::Device,
    modules: &[&reflect::Module],
    stages: vk::ShaderStageFlags,
) -> Result<Layouts> {
    let set_count = modules
        .iter()
        .map(|module| module.sets())
        .max()
        .unwrap_or(0);

    let mut sets = Vec::with_capacity(set_count as usize);
    for index in 0..set_count {
        // One slot, one entry, across every module that binds to this set.
        let mut bindings: Vec<Binding> = Vec::new();
        for module in modules {
            for binding in module.set(index) {
                match bindings
                    .iter_mut()
                    .find(|existing| existing.binding == binding.binding)
                {
                    Some(existing) => existing.count = widest(existing.count, binding.count),
                    None => bindings.push(binding),
                }
            }
        }

        let descriptors: Vec<vk::DescriptorSetLayoutBinding> = bindings
            .iter()
            .map(|binding| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(binding.binding)
                    .descriptor_type(binding.kind.descriptor_type())
                    .descriptor_count(binding.reserve())
                    .stage_flags(stages)
            })
            .collect();
        let create = vk::DescriptorSetLayoutCreateInfo::default().bindings(&descriptors);
        let layout = unsafe { device.create_descriptor_set_layout(&create, None) }
            .with_context(|| format!("creating descriptor set layout {index}"))?;
        sets.push(SetLayout { layout, bindings });
    }

    let push_constants = modules
        .iter()
        .map(|module| module.push_constants)
        .max()
        .unwrap_or(0);
    let handles = sets.iter().map(|set| set.layout).collect::<Vec<_>>();
    let mut create = vk::PipelineLayoutCreateInfo::default().set_layouts(&handles);
    let range = [vk::PushConstantRange::default()
        .stage_flags(stages)
        .offset(0)
        .size(push_constants)];
    if push_constants > 0 {
        create = create.push_constant_ranges(&range);
    }
    let pipeline = unsafe { device.create_pipeline_layout(&create, None) }
        .context("creating the pipeline layout")?;

    Ok(Layouts {
        sets,
        pipeline,
        push_constants,
    })
}

/// Two modules disagreeing on how long a binding is: the wider one wins, and an
/// unbounded array is wider than any declared length.
fn widest(one: u32, other: u32) -> u32 {
    if one == 0 || other == 0 {
        0
    } else {
        one.max(other)
    }
}

/// Points every image and sampler binding in `bindings` at `view` and
/// `sampler`.
///
/// Which slot holds what comes from the reflection: a module that samples
/// through a `texture_2d` and a `sampler` gets two descriptors, one that binds
/// a combined image sampler gets one. The image is written in
/// `SHADER_READ_ONLY_OPTIMAL`, which is the layout [`super::image::Image`]
/// leaves it in.
///
/// # Safety
///
/// The set must be allocated from a layout built for these modules, and
/// neither descriptor may be in use.
pub unsafe fn bind_image(
    device: &ash::Device,
    set: vk::DescriptorSet,
    bindings: &[Binding],
    view: vk::ImageView,
    sampler: vk::Sampler,
) -> Result<()> {
    let layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
    let mut plans = Vec::new();
    for binding in bindings {
        let (info, ty) = match binding.kind {
            reflect::Kind::SampledImage => (
                vk::DescriptorImageInfo::default()
                    .image_view(view)
                    .image_layout(layout),
                vk::DescriptorType::SAMPLED_IMAGE,
            ),
            reflect::Kind::CombinedImageSampler => (
                vk::DescriptorImageInfo::default()
                    .image_view(view)
                    .sampler(sampler)
                    .image_layout(layout),
                vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
            ),
            reflect::Kind::Sampler => (
                vk::DescriptorImageInfo::default().sampler(sampler),
                vk::DescriptorType::SAMPLER,
            ),
            _ => continue,
        };
        plans.push((binding.binding, ty, info));
    }
    if plans.is_empty() {
        return Err(anyhow::anyhow!("the module binds no image to point at"));
    }

    let infos: Vec<vk::DescriptorImageInfo> = plans.iter().map(|(_, _, info)| *info).collect();
    let writes: Vec<vk::WriteDescriptorSet> = plans
        .iter()
        .enumerate()
        .map(|(at, (binding, ty, _))| {
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(*binding)
                .descriptor_type(*ty)
                .image_info(&infos[at..at + 1])
        })
        .collect();
    unsafe { device.update_descriptor_sets(&writes, &[]) };
    Ok(())
}

/// Points the texel-buffer binding in `bindings` at `view`.
///
/// # Safety
///
/// The set must be allocated from a layout built for these modules, `view` must
/// be a texel buffer view of the format the module declared, and neither may be
/// in use.
pub unsafe fn bind_texels(
    device: &ash::Device,
    set: vk::DescriptorSet,
    bindings: &[Binding],
    view: vk::BufferView,
) -> Result<()> {
    let mut plans = Vec::new();
    for binding in bindings {
        let ty = match binding.kind {
            reflect::Kind::SampledTexelBuffer => vk::DescriptorType::UNIFORM_TEXEL_BUFFER,
            reflect::Kind::StorageTexelBuffer => vk::DescriptorType::STORAGE_TEXEL_BUFFER,
            _ => continue,
        };
        plans.push((binding.binding, ty));
    }
    if plans.is_empty() {
        return Err(anyhow::anyhow!("the module binds no texel buffer"));
    }
    let views = [view];
    let writes: Vec<vk::WriteDescriptorSet> = plans
        .iter()
        .map(|(binding, ty)| {
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(*binding)
                .descriptor_type(*ty)
                .texel_buffer_view(&views)
        })
        .collect();
    unsafe { device.update_descriptor_sets(&writes, &[]) };
    Ok(())
}

/// A pool large enough for one set per layout, and for `images` images and
/// samplers in each.
pub fn pool(device: &ash::Device, layouts: &Layouts) -> Result<vk::DescriptorPool> {
    let mut sizes = Vec::new();
    for kind in [
        reflect::Kind::UniformBuffer,
        reflect::Kind::StorageBuffer,
        reflect::Kind::SampledImage,
        reflect::Kind::Sampler,
        reflect::Kind::CombinedImageSampler,
        reflect::Kind::StorageImage,
        reflect::Kind::SampledTexelBuffer,
        reflect::Kind::StorageTexelBuffer,
    ] {
        let count: u32 = layouts
            .sets
            .iter()
            .flat_map(|set| set.bindings.iter())
            .filter(|binding| binding.kind == kind)
            .map(|binding| binding.reserve())
            .sum();
        if count > 0 {
            sizes.push(
                vk::DescriptorPoolSize::default()
                    .ty(kind.descriptor_type())
                    .descriptor_count(count),
            );
        }
    }
    let set_count = layouts.sets.len().max(1) as u32;
    let create = vk::DescriptorPoolCreateInfo::default()
        .max_sets(set_count)
        .pool_sizes(&sizes);
    unsafe { device.create_descriptor_pool(&create, None) }.context("creating the descriptor pool")
}

#[cfg(test)]
mod tests {
    use super::widest;

    #[test]
    fn an_unbounded_array_wins_over_a_declared_length() {
        assert_eq!(widest(16, 4), 16);
        assert_eq!(widest(4, 16), 16);
        assert_eq!(widest(0, 16), 0);
        assert_eq!(widest(16, 0), 0);
        assert_eq!(widest(0, 0), 0);
    }
}
