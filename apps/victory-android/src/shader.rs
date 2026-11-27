//! The stages the host draws with, and how they get here.
//!
//! Two roads lead to the same pair of modules. `victory-dxil` translates the
//! DXIL this host draws with — the road the console's own shaders take once
//! their bitcode survives — and if that fails, `naga` compiles the WGSL
//! transcription of the same shader, so a device that cannot run the converter
//! still draws and says why.
//!
//! DXIL is one program per stage, so the vertex and pixel halves are separate
//! modules with their own entry points (`vs_main`, `ps_main`). That is also the
//! shape Vulkan wants: a `PipelineShaderStageCreateInfo` per stage, each naming
//! the entry point in its own module.

use std::ffi::CString;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use victory_gpu::vulkan;

use crate::title;

/// One translated stage: the module, and the entry point the pipeline names.
pub struct Stage {
    pub words: Vec<u32>,
    pub entry: CString,
}

/// The pair the host's one pipeline runs.
pub struct Modules {
    pub vertex: Stage,
    pub fragment: Stage,
}

impl Modules {
    /// What each stage binds, in the order the two stages come.
    ///
    /// A module that will not parse reflects as binding nothing, which costs a
    /// constant buffer at worst and never fails a launch.
    pub fn reflections(&self) -> [vulkan::reflect::Module; 2] {
        [
            vulkan::reflect::module(&self.vertex.words).unwrap_or_default(),
            vulkan::reflect::module(&self.fragment.words).unwrap_or_default(),
        ]
    }
}

/// Everything the host will draw with.
pub struct Plan {
    /// The stages the frame's fill draws with: the title's own one-target pixel
    /// shader when one was dropped in, else the pair this host ships.
    pub fill: Modules,
    /// The title's own multi-target shader, when its image has one, and how
    /// many colour outputs it writes. It fills that many attachments offscreen
    /// before the fill draws.
    pub multi: Option<(u32, Modules)>,
}

/// What this device and directory can draw with.
///
/// The title's own shaders come first, because that is the title's graphics
/// running on the phone's GPU: its one-target clears are the fill, and its
/// multi-target clear is the offscreen pass. Failing that, the shader this host
/// ships — its DXIL pair if `draw_parameters` allows the translated vertex
/// stage, else the WGSL transcription. Every fallback says why in the log.
pub fn compile(draw_parameters: bool, title_dir: Option<&Path>) -> Result<Plan> {
    let pixels = title::find(title_dir)?;
    let fill = match pixels.iter().find(|pixel| pixel.colours == 1) {
        Some(pixel) => title_stages(pixel)?,
        None => embedded(draw_parameters)?,
    };
    let multi = match pixels
        .iter()
        .filter(|pixel| pixel.colours > 1)
        .max_by_key(|pixel| pixel.colours)
    {
        Some(pixel) => Some((pixel.colours, title_stages(pixel)?)),
        None => None,
    };
    Ok(Plan { fill, multi })
}

/// The shipped shader, translated or transcribed.
fn embedded(draw_parameters: bool) -> Result<Modules> {
    match dxil(draw_parameters) {
        Ok(modules) => Ok(modules),
        Err(error) => {
            tracing::warn!(
                %error,
                "DXIL translation failed; compiling the WGSL transcription instead"
            );
            wgsl()
        }
    }
}

/// The title's own pixel stage, fed by the host's full-screen vertex stage.
///
/// The vertex stage is WGSL because it is scaffolding, not the title's code:
/// three positions from the vertex index, and the `float4` varying the pixel
/// stage reads. Everything that computes a pixel below this line is the
/// title's own machine code, translated.
fn title_stages(pixel: &title::Pixel) -> Result<Modules> {
    let vertex = naga_stage(VERTEX_SOURCE, c"vs_main")?;
    tracing::info!(
        shader = %pixel.name,
        colours = pixel.colours,
        "drawing with the title's own pixel shader"
    );
    Ok(Modules {
        vertex: Stage {
            words: vertex,
            entry: c"vs_main".to_owned(),
        },
        fragment: Stage {
            words: pixel.translation.words.clone(),
            entry: CString::new(pixel.translation.entry.clone())
                .context("the title's entry point's name")?,
        },
    })
}

/// The road the console's shaders take: DXIL in, SPIR-V out, on the CPU.
fn dxil(draw_parameters: bool) -> Result<Modules> {
    if !draw_parameters {
        return Err(anyhow!(
            "the device does not support shader draw parameters, which the translated vertex shader uses"
        ));
    }
    let (major, minor, patch) = victory_dxil::version();
    let vertex = victory_dxil::translate(victory_dxil::TRIANGLE_VS)
        .context("translating the vertex shader")?;
    let fragment = victory_dxil::translate(victory_dxil::TRIANGLE_PS)
        .context("translating the pixel shader")?;
    if vertex.stage != victory_dxil::Stage::Vertex || fragment.stage != victory_dxil::Stage::Pixel {
        return Err(anyhow!(
            "the DXIL is not a vertex and pixel pair: {:?} and {:?}",
            vertex.stage,
            fragment.stage
        ));
    }
    tracing::info!(
        converter = format_args!("{major}.{minor}.{patch}"),
        vertex = vertex.words.len(),
        fragment = fragment.words.len(),
        "DXIL translated to SPIR-V"
    );
    Ok(Modules {
        vertex: Stage {
            words: vertex.words,
            entry: CString::new(vertex.entry).context("the vertex entry point's name")?,
        },
        fragment: Stage {
            words: fragment.words,
            entry: CString::new(fragment.entry).context("the pixel entry point's name")?,
        },
    })
}

/// The fallback: the same shader as WGSL, compiled in-process by `naga`.
fn wgsl() -> Result<Modules> {
    Ok(Modules {
        vertex: Stage {
            words: wgsl_stage(c"vs_main")?,
            entry: c"vs_main".to_owned(),
        },
        fragment: Stage {
            words: wgsl_stage(c"fs_main")?,
            entry: c"fs_main".to_owned(),
        },
    })
}

/// The vertex stage a title pixel shader is drawn with: the same full-screen
/// triangle as the shipped shader, plus the `float4` varying at location 1 that
/// this title's pixel shaders read.
///
/// The varying is `flat` because theirs is: Vulkan compares interpolation
/// qualifiers across stages, so a smooth output feeding a flat input is a
/// pipeline error. Its value is the frame's tint, which is what the title's
/// own vertex stage hands its clear shader.
const VERTEX_SOURCE: &str = r#"
struct Uniforms {
    tint: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(1) @interpolate(flat) uv: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    let p = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    var out: VsOut;
    out.position = vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
    out.uv = u.tint;
    return out;
}
"#;

/// A full-screen triangle and a radial glow tinted by the uniform buffer.
///
/// No vertex buffer: the three positions come from `vertex_index`
/// (`(0,0)`, `(2,0)`, `(0,2)` in uv space maps to a triangle past every corner
/// of the screen). That is the same trick the console's first pass uses for
/// its full-screen quads, and it removes vertex input state from the pipeline
/// while the backend is still being built.
const SOURCE: &str = r#"
struct Uniforms {
    tint: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    let p = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    var out: VsOut;
    out.position = vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
    out.uv = p;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let d = distance(in.uv, vec2<f32>(0.5, 0.5));
    let glow = 1.0 - smoothstep(0.02, 0.8, d);
    return vec4<f32>(u.tint.rgb * glow, 1.0);
}
"#;

/// The stages the textured draw uses: the same full-screen triangle, laid over
/// the middle of the window, sampling an image the host uploaded.
///
/// It exists so the backend's texture path — an upload through a staging
/// buffer, a barrier to `SHADER_READ_ONLY_OPTIMAL`, a sampler, and a
/// descriptor set laid out from reflection — runs every frame instead of
/// waiting untested for a title texture to arrive.
pub fn textured() -> Result<Modules> {
    Ok(Modules {
        vertex: Stage {
            words: naga_stage(TEXTURED_SOURCE, c"vs_main")?,
            entry: c"vs_main".to_owned(),
        },
        fragment: Stage {
            words: naga_stage(TEXTURED_SOURCE, c"fs_main")?,
            entry: c"fs_main".to_owned(),
        },
    })
}

/// A full-screen triangle shrunk to the middle half of the window, and the
/// image that covers it, at 60% alpha so the blend state a description asks for
/// shows in the pixels rather than only in the pipeline.
///
/// The image and the sampler are two bindings here because that is what the
/// front end emits, and what the descriptor layer has to handle: a title's
/// DXIL pair is usually one combined descriptor instead.
const TEXTURED_SOURCE: &str = r#"
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    let p = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    var out: VsOut;
    out.position = vec4<f32>(p * 0.5 - 0.5, 0.0, 1.0);
    out.uv = p * 0.5;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(textureSample(tex, samp, in.uv).rgb, 0.6);
}
"#;

/// Compiles [`SOURCE`] into the module holding just `entry`.
fn wgsl_stage(entry: &std::ffi::CStr) -> Result<Vec<u32>> {
    naga_stage(SOURCE, entry)
}

/// Compiles `source` into the module holding just `entry`.
///
/// `naga`'s default writer options target SPIR-V 1.0 with the coordinate
/// adjustment Vulkan needs, which is the widest floor a phone driver offers.
fn naga_stage(source: &str, entry: &std::ffi::CStr) -> Result<Vec<u32>> {
    let name = entry.to_str().context("the entry point's name")?;
    let mut module = naga::front::wgsl::parse_str(source)
        .map_err(|error| anyhow!("parsing a shader: {}", error.emit_to_string(source)))?;
    module
        .entry_points
        .retain(|candidate| candidate.name == name);
    if module.entry_points.is_empty() {
        return Err(anyhow!("the shader has no `{name}` entry point"));
    }
    let mut validator = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
    );
    let info = validator
        .validate(&module)
        .map_err(|error| anyhow!("validating a shader: {error}"))?;
    let options = naga::back::spv::Options::default();
    naga::back::spv::write_vec(&module, &info, &options, None)
        .map_err(|error| anyhow!("writing a shader as SPIR-V: {error}"))
}
