//! What a translated module needs bound.
//!
//! The title's shaders arrive as DXIL and leave `victory-dxil` as SPIR-V, and
//! the bindings in that module are the only description of the descriptor sets,
//! layouts and push constants a draw needs. This module reads them back out:
//! variable, then pointer, then the type behind it, with the `DescriptorSet`
//! and `Binding` decorations on the variable itself.
//!
//! It is the front half of the descriptor layer; the back half turns a
//! [`Binding`] list into a `VkDescriptorSetLayout`. Both exist so a shader the
//! title ships works without anyone writing its bindings out by hand, which is
//! the difference between one demo pipeline and a backend.

/// What a binding is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A constant buffer.
    UniformBuffer,
    /// A structured or typed buffer the shader writes.
    StorageBuffer,
    /// An image the shader samples.
    SampledImage,
    /// A sampler on its own.
    Sampler,
    /// A buffer the shader reads as texels, which is what a `Buffer` SRV
    /// becomes: a vertex stage fetching its vertices, or any shader reading a
    /// typed buffer.
    SampledTexelBuffer,
    /// A buffer the shader writes as texels.
    StorageTexelBuffer,
    /// An image and its sampler in one binding, which is what DXIL's `t#` and
    /// `s#` pairs become by default.
    CombinedImageSampler,
    /// An image the shader reads and writes.
    StorageImage,
}

impl Kind {
    /// The Vulkan descriptor type for this kind.
    pub fn descriptor_type(self) -> ash::vk::DescriptorType {
        use ash::vk::DescriptorType;
        match self {
            Kind::UniformBuffer => DescriptorType::UNIFORM_BUFFER,
            Kind::StorageBuffer => DescriptorType::STORAGE_BUFFER,
            Kind::SampledImage => DescriptorType::SAMPLED_IMAGE,
            Kind::Sampler => DescriptorType::SAMPLER,
            Kind::SampledTexelBuffer => DescriptorType::UNIFORM_TEXEL_BUFFER,
            Kind::StorageTexelBuffer => DescriptorType::STORAGE_TEXEL_BUFFER,
            Kind::CombinedImageSampler => DescriptorType::COMBINED_IMAGE_SAMPLER,
            Kind::StorageImage => DescriptorType::STORAGE_IMAGE,
        }
    }
}

/// How many descriptors an unbounded array reserves, since a layout has to
/// name a number. A title's descriptor heap is a runtime array with no length
/// in the module at all, and a phone cannot reserve a million descriptors, so
/// the layout reserves this many and the writer fills them.
pub const UNBOUNDED_CAP: u32 = 64;

/// One resource the module reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    pub set: u32,
    pub binding: u32,
    pub kind: Kind,
    /// How many descriptors the binding holds: 1 for a plain binding, the
    /// array's declared length for a bounded array, and 0 for a runtime array,
    /// which declares none. [`Binding::reserve`] turns that into the number a
    /// layout can be built with.
    pub count: u32,
}

impl Binding {
    /// How many descriptors the layout reserves, which is [`UNBOUNDED_CAP`]
    /// when the module declared no length.
    pub fn reserve(self) -> u32 {
        if self.count == 0 {
            UNBOUNDED_CAP
        } else {
            self.count
        }
    }

    /// Whether this binding is an array rather than one descriptor.
    pub fn array(self) -> bool {
        self.count != 1
    }
}

/// A module's interface to the outside world.
#[derive(Default, Debug)]
pub struct Module {
    pub bindings: Vec<Binding>,
    /// The size of the push-constant block the module declares, in bytes.
    pub push_constants: u32,
}

impl Module {
    /// The bindings in `set`, ascending by binding.
    pub fn set(&self, set: u32) -> Vec<Binding> {
        let mut bindings: Vec<Binding> = self
            .bindings
            .iter()
            .copied()
            .filter(|binding| binding.set == set)
            .collect();
        bindings.sort_by_key(|binding| binding.binding);
        bindings
    }

    /// How many sets the module binds.
    pub fn sets(&self) -> u32 {
        self.bindings
            .iter()
            .map(|binding| binding.set + 1)
            .max()
            .unwrap_or(0)
    }

    /// The bindless array in this module, if it declared one: bounded or not.
    pub fn bindless(&self) -> Option<Binding> {
        self.bindings
            .iter()
            .copied()
            .find(|binding| binding.array())
    }
}

// The opcodes this reads, at the numbers the SPIR-V grammar gives them.
const OP_TYPE_IMAGE: u32 = 25;
const OP_TYPE_SAMPLER: u32 = 26;
const OP_TYPE_SAMPLED_IMAGE: u32 = 27;
const OP_TYPE_ARRAY: u32 = 28;
const OP_TYPE_RUNTIME_ARRAY: u32 = 29;
const OP_TYPE_STRUCT: u32 = 30;
const OP_CONSTANT: u32 = 43;
const OP_TYPE_POINTER: u32 = 32;
const OP_VARIABLE: u32 = 59;
const OP_DECORATE: u32 = 71;
const OP_MEMBER_DECORATE: u32 = 72;

const DECORATION_BLOCK: u32 = 2;
const DECORATION_BUFFER_BLOCK: u32 = 3;
const DECORATION_BINDING: u32 = 33;
const DECORATION_DESCRIPTOR_SET: u32 = 34;

const STORAGE_UNIFORM_CONSTANT: u32 = 0;
const STORAGE_UNIFORM: u32 = 2;
const STORAGE_PUSH_CONSTANT: u32 = 9;
const STORAGE_STORAGE_BUFFER: u32 = 12;

/// The length operand a runtime array has not got. Result ids start at one, so
/// zero is no constant's id and no length either.
const RUNTIME: u32 = 0;

/// `OpTypeImage`'s `Sampled` operand: 1 reads through a sampler, 2 writes.
const SAMPLED_IMAGE_FLAG: u32 = 1;
const STORAGE_IMAGE_FLAG: u32 = 2;

/// `OpTypeImage`'s `Dim` operand for a buffer, which Vulkan binds as a texel
/// buffer rather than an image. A title's vertex stage reads its vertices this
/// way, so the distinction is not academic.
const DIM_BUFFER: u32 = 5;

/// Reads the bindings out of a SPIR-V module.
pub fn module(words: &[u32]) -> anyhow::Result<Module> {
    if words.len() < 5 || words[0] != 0x0723_0203 {
        anyhow::bail!("not a SPIR-V module");
    }

    let mut images: Vec<(u32, u32, u32)> = Vec::new();
    let mut samplers = Vec::new();
    let mut sampled_images = Vec::new();
    let mut arrays: Vec<(u32, u32, u32)> = Vec::new();
    let mut constants: Vec<(u32, u32)> = Vec::new();
    let mut structs = Vec::new();
    let mut pointers = Vec::new();
    let mut variables = Vec::new();
    let mut sets = Vec::new();
    let mut slots = Vec::new();
    let mut buffer_blocks = Vec::new();
    let mut blocks = 0usize;
    let mut push_constants = 0u32;

    let mut at = 5;
    while at < words.len() {
        let opcode = words[at] & 0xFFFF;
        let length = (words[at] >> 16) as usize;
        if length == 0 || at + length > words.len() {
            break;
        }
        let operands = &words[at + 1..at + length];
        match opcode {
            OP_TYPE_IMAGE if operands.len() >= 7 => {
                images.push((operands[0], operands[6], operands[2]))
            }
            OP_TYPE_SAMPLER if !operands.is_empty() => samplers.push(operands[0]),
            OP_TYPE_SAMPLED_IMAGE if !operands.is_empty() => sampled_images.push(operands[0]),
            OP_TYPE_ARRAY if operands.len() >= 3 => {
                arrays.push((operands[0], operands[1], operands[2]));
            }
            OP_TYPE_RUNTIME_ARRAY if operands.len() >= 2 => {
                arrays.push((operands[0], operands[1], RUNTIME));
            }
            OP_CONSTANT if operands.len() >= 3 => constants.push((operands[1], operands[2])),
            OP_TYPE_STRUCT if !operands.is_empty() => {
                structs.push((operands[0], operands[1..].to_vec()));
            }
            OP_TYPE_POINTER if operands.len() >= 3 => {
                pointers.push((operands[0], operands[1], operands[2]));
            }
            OP_VARIABLE if operands.len() >= 3 => {
                variables.push((operands[1], operands[2], operands[0]))
            }
            OP_DECORATE if operands.len() >= 2 => match operands[1] {
                DECORATION_DESCRIPTOR_SET if operands.len() >= 3 => {
                    sets.push((operands[0], operands[2]));
                }
                DECORATION_BINDING if operands.len() >= 3 => slots.push((operands[0], operands[2])),
                DECORATION_BLOCK => blocks += 1,
                DECORATION_BUFFER_BLOCK => buffer_blocks.push(operands[0]),
                _ => {}
            },
            OP_MEMBER_DECORATE if operands.len() >= 3 => {
                if operands[1] == DECORATION_BUFFER_BLOCK {
                    buffer_blocks.push(operands[0]);
                } else if operands[1] == DECORATION_BLOCK {
                    blocks += 1;
                }
            }
            _ => {}
        }
        at += length;
    }

    // The type behind a variable's pointer, and how many of it a binding
    // holds. An array is one level deep, which is the depth a title's
    // descriptors actually use.
    let types = Types {
        images: &images,
        samplers: &samplers,
        sampled_images: &sampled_images,
        arrays: &arrays,
        constants: &constants,
        buffer_blocks: &buffer_blocks,
    };

    let mut module = Module::default();
    for (variable, storage, ty) in &variables {
        let Some((_, _, pointee)) = pointers
            .iter()
            .find(|(result, _, _)| result == ty)
            .map(|(result, storage_class, pointee)| (*result, *storage_class, *pointee))
        else {
            continue;
        };

        if *storage == STORAGE_PUSH_CONSTANT {
            push_constants = arrays
                .iter()
                .find(|(result, _, _)| *result == pointee)
                .map_or(0, |(_, _, length)| types.length(*length) * 4);
            continue;
        }
        if *storage != STORAGE_UNIFORM_CONSTANT
            && *storage != STORAGE_UNIFORM
            && *storage != STORAGE_STORAGE_BUFFER
        {
            continue;
        }
        let Some((kind, count)) = types.classify(*storage, pointee) else {
            continue;
        };
        let set = sets
            .iter()
            .find(|(target, _)| target == variable)
            .map_or(0, |(_, set)| *set);
        let binding = slots
            .iter()
            .find(|(target, _)| target == variable)
            .map_or(0, |(_, slot)| *slot);
        module.bindings.push(Binding {
            set,
            binding,
            kind,
            count,
        });
    }

    // One slot, one entry: a module that splits an image and its sampler still
    // has one descriptor to bind.
    module
        .bindings
        .sort_by_key(|binding| (binding.set, binding.binding));
    module
        .bindings
        .dedup_by(|a, b| a.set == b.set && a.binding == b.binding);
    let _ = (structs.len(), blocks);
    module.push_constants = push_constants;
    Ok(module)
}

/// The types the scan found, so classification can recurse through arrays.
struct Types<'a> {
    /// Result id, its `Sampled` operand, and its `Dim` operand.
    images: &'a [(u32, u32, u32)],
    samplers: &'a [u32],
    sampled_images: &'a [u32],
    arrays: &'a [(u32, u32, u32)],
    /// `OpConstant`'s result id and its value, which is how an array's length
    /// operand is read back.
    constants: &'a [(u32, u32)],
    buffer_blocks: &'a [u32],
}

impl Types<'_> {
    /// How many elements an array holds. The operand is the id of a constant
    /// rather than the number itself, and a constant this does not find leaves
    /// the array unbounded, which is what a runtime array is anyway.
    fn length(&self, operand: u32) -> u32 {
        if operand == RUNTIME {
            return 0;
        }
        self.constants
            .iter()
            .find(|(result, _)| *result == operand)
            .map_or(0, |(_, value)| *value)
    }

    fn classify(&self, storage: u32, ty: u32) -> Option<(Kind, u32)> {
        if let Some((_, element, length)) = self.arrays.iter().find(|(result, _, _)| *result == ty)
        {
            let (kind, _) = self.classify(storage, *element)?;
            return Some((kind, self.length(*length)));
        }
        if storage == STORAGE_STORAGE_BUFFER {
            return Some((Kind::StorageBuffer, 1));
        }
        if storage == STORAGE_UNIFORM {
            return Some((Kind::UniformBuffer, 1));
        }
        if self.sampled_images.contains(&ty) {
            return Some((Kind::CombinedImageSampler, 1));
        }
        if self.samplers.contains(&ty) {
            return Some((Kind::Sampler, 1));
        }
        if let Some((_, sampled, dim)) = self.images.iter().find(|(result, _, _)| *result == ty) {
            return Some((
                match (*sampled, *dim == DIM_BUFFER) {
                    (_, true) if *sampled == STORAGE_IMAGE_FLAG => Kind::StorageTexelBuffer,
                    (_, true) => Kind::SampledTexelBuffer,
                    (STORAGE_IMAGE_FLAG, false) => Kind::StorageImage,
                    (SAMPLED_IMAGE_FLAG, false) => Kind::SampledImage,
                    _ => Kind::CombinedImageSampler,
                },
                1,
            ));
        }
        if self.buffer_blocks.contains(&ty) {
            return Some((Kind::StorageBuffer, 1));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compiles `wgsl` to SPIR-V and reflects it.
    fn reflected(wgsl: &str) -> Module {
        let module = naga::front::wgsl::parse_str(wgsl).expect("the test shader parses");
        let info = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            // The descriptions below use whatever the title's own shaders use,
            // bindless sampler arrays included.
            naga::valid::Capabilities::TEXTURE_AND_SAMPLER_BINDING_ARRAY,
        )
        .validate(&module)
        .expect("the test shader validates");
        let words =
            naga::back::spv::write_vec(&module, &info, &naga::back::spv::Options::default(), None)
                .expect("the test shader writes as SPIR-V");
        module_of(&words)
    }

    const TEXTURED: &str = r#"
        struct Uniforms { tint: vec4<f32> };
        @group(0) @binding(0) var<uniform> u: Uniforms;
        @group(0) @binding(1) var texture: texture_2d<f32>;
        @group(0) @binding(2) var samp: sampler;

        @fragment
        fn fs_main(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
            return textureSample(texture, samp, uv) * u.tint;
        }
    "#;

    #[test]
    fn a_constant_buffer_is_a_uniform_buffer() {
        let module = reflected(TEXTURED);
        let binding = module.set(0)[0];
        assert_eq!(binding.binding, 0);
        assert_eq!(binding.kind, Kind::UniformBuffer);
        assert_eq!(binding.count, 1);
    }

    #[test]
    fn an_image_and_its_sampler_are_two_bindings() {
        let module = reflected(TEXTURED);
        let bindings = module.set(0);
        assert_eq!(bindings.len(), 3, "{bindings:?}");
        assert_eq!(bindings[1].kind, Kind::SampledImage);
        assert_eq!(bindings[2].kind, Kind::Sampler);
    }

    #[test]
    fn a_bindless_array_reports_its_length() {
        let module = reflected(
            r#"
            enable wgpu_binding_array;
            @group(0) @binding(0) var textures: binding_array<texture_2d<f32>, 16>;
            @group(0) @binding(1) var samp: sampler;

            @fragment
            fn fs_main(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
                return textureSample(textures[3], samp, uv);
            }
            "#,
        );
        let bindless = module.bindless().expect("the array is bindless");
        assert_eq!(bindless.binding, 0);
        assert_eq!(bindless.count, 16);
    }

    #[test]
    fn sets_are_counted_from_what_the_module_binds() {
        let module = reflected(
            r#"
            @group(1) @binding(0) var<uniform> u: vec4<f32>;

            @fragment
            fn fs_main() -> @location(0) vec4<f32> {
                return u;
            }
            "#,
        );
        assert_eq!(module.sets(), 2);
        assert!(module.set(0).is_empty());
        assert_eq!(module.set(1)[0].kind, Kind::UniformBuffer);
    }

    #[test]
    fn a_buffer_image_is_a_texel_buffer() {
        // `naga`'s SPIR-V front end cannot read a buffer-dimension image, so
        // this fixture is written by hand: `%1 = OpTypeInt 32 1`,
        // `%2 = OpTypeImage %1 Buffer ... sampled`,
        // `%3 = OpTypePointer UniformConstant %2`, `%4 = OpVariable %3 ...`
        // decorated set 1 binding 0. It is the shape the title's vertex stage
        // arrives in, and the difference between a texel buffer and an image.
        const OP_CAPABILITY: u32 = 17;
        const OP_MEMORY_MODEL: u32 = 14;
        const OP_TYPE_INT: u32 = 21;
        const fn head(opcode: u32, words: u32) -> u32 {
            (words << 16) | opcode
        }
        let module_words: Vec<u32> = vec![
            0x0723_0203,
            0x0001_0000,
            0,
            20,
            0,
            head(OP_CAPABILITY, 2),
            1,
            head(OP_MEMORY_MODEL, 3),
            0,
            1,
            head(OP_TYPE_INT, 4),
            1,
            32,
            1,
            head(OP_TYPE_IMAGE, 9),
            2,
            1,
            5,
            0,
            0,
            0,
            1,
            0,
            head(32, 4),
            3,
            0,
            2,
            head(59, 4),
            3,
            4,
            0,
            head(71, 4),
            4,
            34,
            1,
            head(71, 4),
            4,
            33,
            0,
        ];
        let module = module_of(&module_words);
        let bindings = module.set(1);
        assert_eq!(bindings.len(), 1, "{bindings:?}");
        assert_eq!(bindings[0].binding, 0);
        assert_eq!(bindings[0].kind, Kind::SampledTexelBuffer);
        assert_eq!(
            bindings[0].kind.descriptor_type(),
            ash::vk::DescriptorType::UNIFORM_TEXEL_BUFFER
        );
    }

    #[test]
    fn an_array_without_a_length_reserves_the_cap() {
        let array = Binding {
            set: 0,
            binding: 0,
            kind: Kind::SampledImage,
            count: 16,
        };
        assert_eq!(array.reserve(), 16);
        assert!(array.array());
        let unbounded = Binding { count: 0, ..array };
        assert_eq!(unbounded.reserve(), UNBOUNDED_CAP);
        assert!(unbounded.array());
        let single = Binding { count: 1, ..array };
        assert_eq!(single.reserve(), 1);
        assert!(!single.array());
    }

    /// The module half of [`super::module`] under a name the tests can call.
    fn module_of(words: &[u32]) -> Module {
        module(words).expect("the compiled module reflects")
    }
}
