//! Graphics pipelines, built from the SPIR-V the shader translator produces
//! and the state a console pipeline description carries.
//!
//! A pipeline is the first object the title's first draw needs, and its
//! fixed-function half is the part this backend has to reproduce exactly:
//! blending for each target, the rasterizer's cull and front face, the depth
//! test and its compare, the assembly topology, the sample count, and how many
//! colour attachments the pass has. All of it arrives in a title's PSO
//! description, so [`State`] is that description in the form Vulkan wants:
//! the constructors here take D3D12's own numbers and translate them rather
//! than inventing a state the title never asked for. [`State::screen`] is the
//! one-target, no-depth, no-blend state the Android host draws its own passes
//! with.
//!
//! A field a description leaves out keeps D3D12's default, which is what the
//! `D3d12*` argument types describe.
//!
//! What is deliberately not here yet is vertex input: a PSO names its elements
//! by DXIL semantic, and matching those to the locations a translated module
//! declares is its own step. The pipelines drawn today take their positions
//! from the vertex index, which is what a full-screen pass does anyway.

use std::ffi::CString;

use anyhow::{Context, Result, anyhow, bail};
use ash::vk;

/// One stage's module, and the entry point inside it the pipeline names.
pub struct Stage {
    pub module: vk::ShaderModule,
    pub entry: CString,
}

impl Stage {
    /// Creates the module for `words`, naming `entry`, which has to be the
    /// entry point the module's own `OpEntryPoint` declares.
    ///
    /// # Safety
    ///
    /// `device` must be live, and `words` a complete SPIR-V module.
    pub unsafe fn new(device: &ash::Device, words: &[u32], entry: CString) -> Result<Self> {
        let create = vk::ShaderModuleCreateInfo::default().code(words);
        let module = unsafe { device.create_shader_module(&create, None) }
            .with_context(|| format!("creating the module for {}", entry.to_string_lossy()))?;
        Ok(Self { module, entry })
    }

    /// # Safety
    ///
    /// The module must not be in use by any submitted command buffer.
    pub unsafe fn destroy(&self, device: &ash::Device) {
        unsafe { device.destroy_shader_module(self.module, None) };
    }
}

/// What the pipeline has to agree with about the pass it draws in.
pub struct Target {
    /// The render pass the pipeline is created for.
    pub render_pass: vk::RenderPass,
    /// The layout describing the descriptor sets the stages use.
    pub layout: vk::PipelineLayout,
    /// The driver's cache, so a launch after the first does not recompile.
    pub cache: vk::PipelineCache,
}

impl Target {
    /// The pass and its layout. The rest of the fixed state is the
    /// [`State`]'s, because that is where a title's description puts it.
    pub fn pass(render_pass: vk::RenderPass, layout: vk::PipelineLayout) -> Self {
        Self {
            render_pass,
            layout,
            cache: vk::PipelineCache::null(),
        }
    }
}

/// One colour attachment: its format, how it blends, and which channels it
/// writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Output {
    pub format: vk::Format,
    pub blend: Blend,
    pub write: vk::ColorComponentFlags,
}

impl Output {
    /// A target the shader's colour lands in unmodified.
    pub fn opaque(format: vk::Format) -> Self {
        Self {
            format,
            blend: Blend::OPAQUE,
            write: vk::ColorComponentFlags::RGBA,
        }
    }
}

/// A render-target blend description.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Blend {
    pub enable: bool,
    pub source_colour: vk::BlendFactor,
    pub destination_colour: vk::BlendFactor,
    pub operation_colour: vk::BlendOp,
    pub source_alpha: vk::BlendFactor,
    pub destination_alpha: vk::BlendFactor,
    pub operation_alpha: vk::BlendOp,
}

impl Blend {
    /// `SrcBlend = ONE`, `DestBlend = ZERO`, blending off: the target keeps
    /// what the shader writes.
    pub const OPAQUE: Blend = Blend {
        enable: false,
        source_colour: vk::BlendFactor::ONE,
        destination_colour: vk::BlendFactor::ZERO,
        operation_colour: vk::BlendOp::ADD,
        source_alpha: vk::BlendFactor::ONE,
        destination_alpha: vk::BlendFactor::ZERO,
        operation_alpha: vk::BlendOp::ADD,
    };

    /// A `D3D12_RENDER_TARGET_BLEND_DESC`'s numbers, translated.
    pub fn d3d12(desc: D3d12Blend) -> Result<Blend> {
        Ok(Blend {
            enable: desc.enable,
            source_colour: blend_factor(desc.source_colour)?,
            destination_colour: blend_factor(desc.destination_colour)?,
            operation_colour: blend_op(desc.operation_colour)?,
            source_alpha: blend_factor(desc.source_alpha)?,
            destination_alpha: blend_factor(desc.destination_alpha)?,
            operation_alpha: blend_op(desc.operation_alpha)?,
        })
    }
}

/// The fields of a `D3D12_RENDER_TARGET_BLEND_DESC` a Vulkan pipeline needs,
/// as the numbers D3D12 stores in them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct D3d12Blend {
    pub enable: bool,
    pub source_colour: u32,
    pub destination_colour: u32,
    pub operation_colour: u32,
    pub source_alpha: u32,
    pub destination_alpha: u32,
    pub operation_alpha: u32,
}

impl D3d12Blend {
    /// D3D12's default: blending off, `ONE`/`ZERO` with `ADD`.
    pub const OFF: D3d12Blend = D3d12Blend {
        enable: false,
        source_colour: 2,
        destination_colour: 1,
        operation_colour: 1,
        source_alpha: 2,
        destination_alpha: 1,
        operation_alpha: 1,
    };
}

/// The rasterizer, as a description names it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Raster {
    pub fill: vk::PolygonMode,
    pub cull: vk::CullModeFlags,
    pub front_face: vk::FrontFace,
    /// `DepthClipEnable`, which Vulkan spells the other way round.
    pub depth_clip: bool,
    pub line_width: f32,
}

impl Raster {
    /// D3D12's default: solid, back faces culled, depth clipped, and front
    /// faces clockwise (in the y-up clip space a console has, which Vulkan's
    /// y-down one turns into counter-clockwise).
    pub const SOLID: Raster = Raster {
        fill: vk::PolygonMode::FILL,
        cull: vk::CullModeFlags::BACK,
        front_face: vk::FrontFace::COUNTER_CLOCKWISE,
        depth_clip: true,
        line_width: 1.0,
    };

    /// The host's own state: everything drawn, no culling, which is what a
    /// full-screen pass wants.
    pub const ANY: Raster = Raster {
        fill: vk::PolygonMode::FILL,
        cull: vk::CullModeFlags::NONE,
        front_face: vk::FrontFace::COUNTER_CLOCKWISE,
        depth_clip: true,
        line_width: 1.0,
    };

    /// A `D3D12_RASTERIZER_DESC`'s numbers, translated.
    ///
    /// The front face flips because a console's clip space has y up and
    /// Vulkan's has it down. The host keeps the flip rather than a negative
    /// viewport height, so which side a triangle shows is decided here; cull
    /// modes pass through unchanged, since both APIs cull the side that is not
    /// the front one.
    pub fn d3d12(desc: D3d12Raster) -> Result<Raster> {
        Ok(Raster {
            fill: fill_mode(desc.fill)?,
            cull: cull_mode(desc.cull)?,
            front_face: if desc.front_counter_clockwise {
                vk::FrontFace::CLOCKWISE
            } else {
                vk::FrontFace::COUNTER_CLOCKWISE
            },
            depth_clip: desc.depth_clip,
            line_width: 1.0,
        })
    }
}

/// The fields of a `D3D12_RASTERIZER_DESC` a Vulkan pipeline needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct D3d12Raster {
    pub fill: u32,
    pub cull: u32,
    pub front_counter_clockwise: bool,
    pub depth_clip: bool,
}

impl D3d12Raster {
    /// D3D12's default.
    pub const SOLID: D3d12Raster = D3d12Raster {
        fill: 3,
        cull: 3,
        front_counter_clockwise: false,
        depth_clip: true,
    };
}

/// The depth test, as a description names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Depth {
    pub test: bool,
    pub write: bool,
    pub compare: vk::CompareOp,
}

impl Depth {
    /// No test, no writes: what a 2D pass asks for.
    pub const OFF: Depth = Depth {
        test: false,
        write: false,
        compare: vk::CompareOp::ALWAYS,
    };

    /// A `D3D12_DEPTH_STENCIL_DESC`'s numbers, as far as Vulkan's depth test
    /// takes them. Stencil is not translated yet.
    pub fn d3d12(desc: D3d12Depth) -> Result<Depth> {
        Ok(Depth {
            test: desc.enable,
            write: desc.enable && desc.write,
            compare: compare_op(desc.compare)?,
        })
    }
}

/// The fields of a `D3D12_DEPTH_STENCIL_DESC` a Vulkan depth test needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct D3d12Depth {
    pub enable: bool,
    pub write: bool,
    pub compare: u32,
}

impl D3d12Depth {
    /// D3D12's default: enabled, writing, `LESS`.
    pub const LESS: D3d12Depth = D3d12Depth {
        enable: true,
        write: true,
        compare: 2,
    };
}

/// Everything about a pipeline that a description decides and a shader does
/// not: the attachments, the blend state, the rasterizer, depth, topology and
/// samples.
#[derive(Clone, Debug, PartialEq)]
pub struct State {
    /// The colour attachments, in the order the pass declares them.
    pub targets: Vec<Output>,
    /// The depth attachment's format, when the pass has one.
    pub depth_format: Option<vk::Format>,
    pub depth: Depth,
    pub raster: Raster,
    pub topology: vk::PrimitiveTopology,
    /// `primitiveRestartEnable`: a strip restarts at the cut value instead of
    /// ending.
    pub restart: bool,
    pub samples: vk::SampleCountFlags,
}

impl State {
    /// One target, one sample, no depth, no blending: the state the host's own
    /// full-screen passes draw with.
    pub fn screen(format: vk::Format) -> State {
        State {
            targets: vec![Output::opaque(format)],
            depth_format: None,
            depth: Depth::OFF,
            raster: Raster::ANY,
            topology: vk::PrimitiveTopology::TRIANGLE_LIST,
            restart: false,
            samples: vk::SampleCountFlags::TYPE_1,
        }
    }

    /// The blend attachment for each target.
    ///
    /// Blending is switched off on an integer target, where both APIs refuse
    /// it: a title leaves blending on over an `R8_UINT` target and its draws
    /// are lost if the state is taken literally. Each target keeps its own
    /// copy of the description here, so the others are unaffected.
    pub fn attachments(&self) -> Vec<vk::PipelineColorBlendAttachmentState> {
        self.targets
            .iter()
            .map(|target| {
                let blend = target.blend;
                vk::PipelineColorBlendAttachmentState::default()
                    .blend_enable(blend.enable && !integer_format(target.format))
                    .src_color_blend_factor(blend.source_colour)
                    .dst_color_blend_factor(blend.destination_colour)
                    .color_blend_op(blend.operation_colour)
                    .src_alpha_blend_factor(blend.source_alpha)
                    .dst_alpha_blend_factor(blend.destination_alpha)
                    .alpha_blend_op(blend.operation_alpha)
                    .color_write_mask(target.write)
            })
            .collect()
    }

    /// The rasterizer state.
    pub fn rasterization(&self) -> vk::PipelineRasterizationStateCreateInfo<'static> {
        vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(self.raster.fill)
            .cull_mode(self.raster.cull)
            .front_face(self.raster.front_face)
            .depth_clamp_enable(!self.raster.depth_clip)
            .line_width(self.raster.line_width)
    }

    /// The depth-stencil state, when the pass has a depth attachment.
    pub fn depth_stencil(&self) -> Option<vk::PipelineDepthStencilStateCreateInfo<'static>> {
        self.depth_format.map(|_| {
            vk::PipelineDepthStencilStateCreateInfo::default()
                .depth_test_enable(self.depth.test)
                .depth_write_enable(self.depth.write)
                .depth_compare_op(self.depth.compare)
        })
    }
}

/// The two stages and the pass, target and state they draw with.
pub struct Graphics<'a> {
    pub target: &'a Target,
    pub state: &'a State,
    pub vertex: &'a Stage,
    pub fragment: &'a Stage,
}

/// Creates the graphics pipeline `graphics` describes.
///
/// Viewport and scissor are dynamic state, so a resize costs nothing; every
/// other piece of the fixed state comes from `graphics.state`, which is what a
/// title's own description decided.
///
/// # Safety
///
/// Every handle in `graphics` must belong to the device the pipeline is created
/// on, and the render pass must have a subpass whose attachments agree with
/// `state.targets` and its depth format.
pub unsafe fn create(device: &ash::Device, graphics: &Graphics<'_>) -> Result<vk::Pipeline> {
    let Graphics {
        target,
        state,
        vertex,
        fragment,
    } = graphics;
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vertex.module)
            .name(&vertex.entry),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(fragment.module)
            .name(&fragment.entry),
    ];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
    let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(state.topology)
        .primitive_restart_enable(state.restart);
    let viewport = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let raster = state.rasterization();
    let multisample =
        vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(state.samples);
    let attachments = state.attachments();
    let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&attachments);
    let depth = state.depth_stencil();
    let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic_state = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
    let mut create = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&assembly)
        .viewport_state(&viewport)
        .rasterization_state(&raster)
        .multisample_state(&multisample)
        .color_blend_state(&blend)
        .dynamic_state(&dynamic_state)
        .layout(target.layout)
        .render_pass(target.render_pass)
        .subpass(0);
    if let Some(depth) = &depth {
        create = create.depth_stencil_state(depth);
    }
    let pipelines = unsafe { device.create_graphics_pipelines(target.cache, &[create], None) }
        .map_err(|(_, error)| anyhow!("creating the graphics pipeline: {error}"))?;
    pipelines
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("the driver created no graphics pipeline"))
}

/// A `D3D12_BLEND` value as Vulkan's blend factor. The two enums agree on what
/// the factors mean and disagree on their numbers, and Vulkan has no factor
/// that takes a constant's alpha alone, which is where this can fail.
pub fn blend_factor(d3d12: u32) -> Result<vk::BlendFactor> {
    Ok(match d3d12 {
        1 => vk::BlendFactor::ZERO,
        2 => vk::BlendFactor::ONE,
        3 => vk::BlendFactor::SRC_COLOR,
        4 => vk::BlendFactor::ONE_MINUS_SRC_COLOR,
        5 => vk::BlendFactor::SRC_ALPHA,
        6 => vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
        7 => vk::BlendFactor::DST_ALPHA,
        8 => vk::BlendFactor::ONE_MINUS_DST_ALPHA,
        9 => vk::BlendFactor::DST_COLOR,
        10 => vk::BlendFactor::ONE_MINUS_DST_COLOR,
        11 => vk::BlendFactor::SRC_ALPHA_SATURATE,
        14 => vk::BlendFactor::CONSTANT_COLOR,
        15 => vk::BlendFactor::ONE_MINUS_CONSTANT_COLOR,
        16 => vk::BlendFactor::SRC1_COLOR,
        17 => vk::BlendFactor::ONE_MINUS_SRC1_COLOR,
        18 => vk::BlendFactor::SRC1_ALPHA,
        19 => vk::BlendFactor::ONE_MINUS_SRC1_ALPHA,
        other => bail!("D3D12 blend factor {other} has no Vulkan equivalent"),
    })
}

/// A `D3D12_BLEND_OP` value as Vulkan's blend operation.
pub fn blend_op(d3d12: u32) -> Result<vk::BlendOp> {
    Ok(match d3d12 {
        1 => vk::BlendOp::ADD,
        2 => vk::BlendOp::SUBTRACT,
        3 => vk::BlendOp::REVERSE_SUBTRACT,
        4 => vk::BlendOp::MIN,
        5 => vk::BlendOp::MAX,
        other => bail!("D3D12 blend operation {other} has no Vulkan equivalent"),
    })
}

/// A `D3D12_COMPARISON_FUNC` value as Vulkan's compare operation. Both list
/// the same functions from `NEVER` to `ALWAYS`, so only the numbers differ.
pub fn compare_op(d3d12: u32) -> Result<vk::CompareOp> {
    Ok(match d3d12 {
        1 => vk::CompareOp::NEVER,
        2 => vk::CompareOp::LESS,
        3 => vk::CompareOp::EQUAL,
        4 => vk::CompareOp::LESS_OR_EQUAL,
        5 => vk::CompareOp::GREATER,
        6 => vk::CompareOp::NOT_EQUAL,
        7 => vk::CompareOp::GREATER_OR_EQUAL,
        8 => vk::CompareOp::ALWAYS,
        other => bail!("D3D12 comparison function {other} has no Vulkan equivalent"),
    })
}

/// A `D3D12_COLOR_WRITE_ENABLE` mask as Vulkan's channel mask.
pub fn write_mask(d3d12: u8) -> vk::ColorComponentFlags {
    let mut mask = vk::ColorComponentFlags::empty();
    for (bit, channel) in [
        (1, vk::ColorComponentFlags::R),
        (2, vk::ColorComponentFlags::G),
        (4, vk::ColorComponentFlags::B),
        (8, vk::ColorComponentFlags::A),
    ] {
        if d3d12 & bit != 0 {
            mask |= channel;
        }
    }
    mask
}

/// A `D3D12_CULL_MODE` value as Vulkan's cull mode.
pub fn cull_mode(d3d12: u32) -> Result<vk::CullModeFlags> {
    Ok(match d3d12 {
        1 => vk::CullModeFlags::NONE,
        2 => vk::CullModeFlags::FRONT,
        3 => vk::CullModeFlags::BACK,
        other => bail!("D3D12 cull mode {other} has no Vulkan equivalent"),
    })
}

/// A `D3D12_FILL_MODE` value as Vulkan's polygon mode.
pub fn fill_mode(d3d12: u32) -> Result<vk::PolygonMode> {
    Ok(match d3d12 {
        2 => vk::PolygonMode::LINE,
        3 => vk::PolygonMode::FILL,
        other => bail!("D3D12 fill mode {other} has no Vulkan equivalent"),
    })
}

/// A `D3D12_PRIMITIVE_TOPOLOGY_TYPE` value as the topology a pipeline declares.
/// The type is the class; which list or strip of it a draw uses is draw state,
/// so a strip's restart is asked for separately ([`State::restart`]).
pub fn topology(kind: u32) -> Result<vk::PrimitiveTopology> {
    Ok(match kind {
        0 => bail!("a pipeline with no primitive topology type is not a pipeline"),
        1 => vk::PrimitiveTopology::POINT_LIST,
        2 => vk::PrimitiveTopology::LINE_LIST,
        3 => vk::PrimitiveTopology::TRIANGLE_LIST,
        other => bail!("D3D12 primitive topology type {other} has no Vulkan list"),
    })
}

/// An `DXGI_SAMPLE_DESC` count as Vulkan's sample count.
pub fn sample_count(count: u32) -> Result<vk::SampleCountFlags> {
    Ok(match count {
        1 => vk::SampleCountFlags::TYPE_1,
        2 => vk::SampleCountFlags::TYPE_2,
        4 => vk::SampleCountFlags::TYPE_4,
        8 => vk::SampleCountFlags::TYPE_8,
        16 => vk::SampleCountFlags::TYPE_16,
        other => bail!("{other} samples per pixel is not a sample count Vulkan names"),
    })
}

/// Whether a format carries integers, on which neither API allows blending.
///
/// The list is the colour formats a title blends into; an integer format that
/// is not here still cannot blend, and the driver's own validation says so at
/// pipeline creation.
pub fn integer_format(format: vk::Format) -> bool {
    matches!(
        format,
        vk::Format::R8_UINT
            | vk::Format::R8_SINT
            | vk::Format::R8G8_UINT
            | vk::Format::R8G8_SINT
            | vk::Format::R8G8B8A8_UINT
            | vk::Format::R8G8B8A8_SINT
            | vk::Format::R16_UINT
            | vk::Format::R16_SINT
            | vk::Format::R16G16_UINT
            | vk::Format::R16G16_SINT
            | vk::Format::R16G16B16A16_UINT
            | vk::Format::R16G16B16A16_SINT
            | vk::Format::R32_UINT
            | vk::Format::R32_SINT
            | vk::Format::R32G32_UINT
            | vk::Format::R32G32_SINT
            | vk::Format::R32G32B32_UINT
            | vk::Format::R32G32B32_SINT
            | vk::Format::R32G32B32A32_UINT
            | vk::Format::R32G32B32A32_SINT
            | vk::Format::A2B10G10R10_UINT_PACK32
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_console_s_blend_numbers_become_vulkan_factors() {
        assert_eq!(blend_factor(2).unwrap(), vk::BlendFactor::ONE);
        assert_eq!(blend_factor(5).unwrap(), vk::BlendFactor::SRC_ALPHA);
        assert_eq!(
            blend_factor(6).unwrap(),
            vk::BlendFactor::ONE_MINUS_SRC_ALPHA
        );
        assert_eq!(
            blend_factor(11).unwrap(),
            vk::BlendFactor::SRC_ALPHA_SATURATE
        );
        assert_eq!(blend_factor(16).unwrap(), vk::BlendFactor::SRC1_COLOR);
        assert!(blend_factor(20).is_err(), "D3D12's alpha factor");
        assert!(blend_factor(0).is_err());
    }

    #[test]
    fn a_blend_description_keeps_its_factors_and_operations() {
        let blend = Blend::d3d12(D3d12Blend {
            enable: true,
            source_colour: 5,
            destination_colour: 6,
            operation_colour: 1,
            source_alpha: 2,
            destination_alpha: 6,
            operation_alpha: 3,
        })
        .expect("the console's alpha blend");
        assert!(blend.enable);
        assert_eq!(blend.source_colour, vk::BlendFactor::SRC_ALPHA);
        assert_eq!(
            blend.destination_colour,
            vk::BlendFactor::ONE_MINUS_SRC_ALPHA
        );
        assert_eq!(blend.operation_colour, vk::BlendOp::ADD);
        assert_eq!(blend.source_alpha, vk::BlendFactor::ONE);
        assert_eq!(
            blend.destination_alpha,
            vk::BlendFactor::ONE_MINUS_SRC_ALPHA
        );
        assert_eq!(blend.operation_alpha, vk::BlendOp::REVERSE_SUBTRACT);
    }

    #[test]
    fn every_comparison_function_keeps_its_meaning() {
        let order = [
            vk::CompareOp::NEVER,
            vk::CompareOp::LESS,
            vk::CompareOp::EQUAL,
            vk::CompareOp::LESS_OR_EQUAL,
            vk::CompareOp::GREATER,
            vk::CompareOp::NOT_EQUAL,
            vk::CompareOp::GREATER_OR_EQUAL,
            vk::CompareOp::ALWAYS,
        ];
        for (at, expected) in order.into_iter().enumerate() {
            assert_eq!(compare_op(at as u32 + 1).unwrap(), expected);
        }
        assert!(compare_op(0).is_err());
        assert!(compare_op(9).is_err());
    }

    #[test]
    fn a_write_mask_keeps_the_channels_it_names() {
        assert_eq!(write_mask(0b1111), vk::ColorComponentFlags::RGBA);
        assert_eq!(write_mask(0b0001), vk::ColorComponentFlags::R);
        assert_eq!(
            write_mask(0b0110),
            vk::ColorComponentFlags::G | vk::ColorComponentFlags::B
        );
        assert!(write_mask(0).is_empty());
    }

    #[test]
    fn the_front_face_flips_because_vulkan_s_clip_space_is_y_down() {
        let clockwise = Raster::d3d12(D3d12Raster {
            front_counter_clockwise: true,
            ..D3d12Raster::SOLID
        })
        .expect("a clockwise-front rasterizer");
        assert_eq!(clockwise.front_face, vk::FrontFace::CLOCKWISE);
        assert_eq!(clockwise.cull, vk::CullModeFlags::BACK);

        let counter = Raster::d3d12(D3d12Raster::SOLID).expect("the default rasterizer");
        assert_eq!(counter.front_face, vk::FrontFace::COUNTER_CLOCKWISE);
        assert_eq!(counter.cull, vk::CullModeFlags::BACK);
        assert!(counter.depth_clip);

        let unculled = Raster::d3d12(D3d12Raster {
            cull: 1,
            fill: 2,
            depth_clip: false,
            ..D3d12Raster::SOLID
        })
        .expect("a wireframe rasterizer");
        assert_eq!(unculled.cull, vk::CullModeFlags::NONE);
        assert_eq!(unculled.fill, vk::PolygonMode::LINE);
        assert!(!unculled.depth_clip);
    }

    #[test]
    fn a_depth_description_says_which_way_the_test_writes() {
        let less = Depth::d3d12(D3d12Depth::LESS).expect("the default depth state");
        assert!(less.test);
        assert!(less.write);
        assert_eq!(less.compare, vk::CompareOp::LESS);

        let read_only = Depth::d3d12(D3d12Depth {
            write: false,
            compare: 7,
            ..D3d12Depth::LESS
        })
        .expect("a depth test that keeps what it reads");
        assert!(read_only.test);
        assert!(!read_only.write, "a write mask of zero writes nothing");
        assert_eq!(read_only.compare, vk::CompareOp::GREATER_OR_EQUAL);
    }

    #[test]
    fn the_screen_state_writes_one_target_and_tests_no_depth() {
        let state = State::screen(vk::Format::R8G8B8A8_UNORM);
        let attachments = state.attachments();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].blend_enable, 0);
        assert_eq!(
            attachments[0].color_write_mask,
            vk::ColorComponentFlags::RGBA
        );
        assert!(state.depth_stencil().is_none());
        let raster = state.rasterization();
        assert_eq!(raster.cull_mode, vk::CullModeFlags::NONE);
        assert_eq!(raster.depth_clamp_enable, 0);
    }

    #[test]
    fn four_targets_are_four_blend_attachments() {
        let mut state = State::screen(vk::Format::R8G8B8A8_UNORM);
        state.targets = vec![Output::opaque(vk::Format::R16G16B16A16_SFLOAT); 4];
        state.targets[2].blend = Blend {
            enable: true,
            ..Blend::OPAQUE
        };
        let attachments = state.attachments();
        assert_eq!(attachments.len(), 4);
        assert_ne!(attachments[2].blend_enable, 0, "the third target blends");
        assert_eq!(attachments[0].blend_enable, 0, "and the others do not");
    }

    #[test]
    fn an_integer_target_never_blends() {
        let mut state = State::screen(vk::Format::R8_UINT);
        state.targets[0].blend = Blend {
            enable: true,
            source_colour: vk::BlendFactor::SRC_ALPHA,
            ..Blend::OPAQUE
        };
        let attachments = state.attachments();
        assert_eq!(
            attachments[0].blend_enable, 0,
            "a title leaves blending on over an integer target, and both APIs refuse it"
        );
    }

    #[test]
    fn a_depth_attachment_brings_a_depth_stencil_state() {
        let mut state = State::screen(vk::Format::R8G8B8A8_UNORM);
        state.depth_format = Some(vk::Format::D32_SFLOAT);
        state.depth = Depth::d3d12(D3d12Depth::LESS).expect("the default depth state");
        let depth = state.depth_stencil().expect("a depth state");
        assert_ne!(depth.depth_test_enable, 0);
        assert_ne!(depth.depth_write_enable, 0);
        assert_eq!(depth.depth_compare_op, vk::CompareOp::LESS);
    }

    #[test]
    fn a_topology_type_is_the_class_its_draws_use() {
        assert_eq!(topology(1).unwrap(), vk::PrimitiveTopology::POINT_LIST);
        assert_eq!(topology(2).unwrap(), vk::PrimitiveTopology::LINE_LIST);
        assert_eq!(topology(3).unwrap(), vk::PrimitiveTopology::TRIANGLE_LIST);
        assert!(topology(0).is_err(), "an undefined topology type");
        assert!(topology(4).is_err(), "patches need tessellation stages");
    }

    #[test]
    fn a_sample_count_is_one_vulkan_names() {
        assert_eq!(sample_count(1).unwrap(), vk::SampleCountFlags::TYPE_1);
        assert_eq!(sample_count(4).unwrap(), vk::SampleCountFlags::TYPE_4);
        assert!(sample_count(3).is_err());
    }
}
