//! DXIL to SPIR-V on the CPU.
//!
//! `victory-shader` reads a container and hands back DXIL, which is what a
//! D3D12 host wants and no phone driver accepts. The conversion to SPIR-V is
//! Valve's `dxil-spirv` — the converter Proton runs — compiled from C++ by
//! [`dxil_spirv_sys`] for whatever target Cargo is building, and reached
//! through [`translate`], the only `unsafe` boundary in the crate.
//!
//! A DXIL program carries one entry point and one stage, so a vertex and a
//! pixel shader are two containers and come back as two modules, each with the
//! name a `PipelineShaderStageCreateInfo` has to give that stage.

use std::slice;

use anyhow::{Result, anyhow, bail};
use dxil_spirv_sys as ffi;

/// The shader the host draws with, as DXIL.
///
/// `shaders/tri.hlsl` is the source; the blobs beside it are
/// `dxc -T vs_6_0 -E vs_main` and `dxc -T ps_6_0 -E ps_main`. They are both the
/// crate's test fixture and what the Android host translates on the device.
pub const TRIANGLE_VS: &[u8] = include_bytes!("../shaders/tri_vs.dxil");
pub const TRIANGLE_PS: &[u8] = include_bytes!("../shaders/tri_ps.dxil");

/// The stage a program belongs to, as its DXIL header reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Vertex,
    Hull,
    Domain,
    Geometry,
    Pixel,
    Compute,
    Amplification,
    Mesh,
    RayGeneration,
    AnyHit,
    ClosestHit,
    Miss,
    Intersection,
    Callable,
}

impl Stage {
    fn from_raw(raw: ffi::dxil_spv_shader_stage) -> Result<Self> {
        STAGES
            .iter()
            .find(|(stage, _)| *stage == raw)
            .map(|(_, stage)| *stage)
            .ok_or_else(|| {
                anyhow!("the program's header names stage {raw}, which has no Vulkan stage")
            })
    }
}

/// The stages a DXIL program can name, and the header value for each.
const STAGES: &[(ffi::dxil_spv_shader_stage, Stage)] = &[
    (
        ffi::dxil_spv_shader_stage_DXIL_SPV_STAGE_VERTEX,
        Stage::Vertex,
    ),
    (ffi::dxil_spv_shader_stage_DXIL_SPV_STAGE_HULL, Stage::Hull),
    (
        ffi::dxil_spv_shader_stage_DXIL_SPV_STAGE_DOMAIN,
        Stage::Domain,
    ),
    (
        ffi::dxil_spv_shader_stage_DXIL_SPV_STAGE_GEOMETRY,
        Stage::Geometry,
    ),
    (
        ffi::dxil_spv_shader_stage_DXIL_SPV_STAGE_PIXEL,
        Stage::Pixel,
    ),
    (
        ffi::dxil_spv_shader_stage_DXIL_SPV_STAGE_COMPUTE,
        Stage::Compute,
    ),
    (
        ffi::dxil_spv_shader_stage_DXIL_SPV_STAGE_AMPLIFICATION,
        Stage::Amplification,
    ),
    (ffi::dxil_spv_shader_stage_DXIL_SPV_STAGE_MESH, Stage::Mesh),
    (
        ffi::dxil_spv_shader_stage_DXIL_SPV_STAGE_RAY_GENERATION,
        Stage::RayGeneration,
    ),
    (
        ffi::dxil_spv_shader_stage_DXIL_SPV_STAGE_ANY_HIT,
        Stage::AnyHit,
    ),
    (
        ffi::dxil_spv_shader_stage_DXIL_SPV_STAGE_CLOSEST_HIT,
        Stage::ClosestHit,
    ),
    (ffi::dxil_spv_shader_stage_DXIL_SPV_STAGE_MISS, Stage::Miss),
    (
        ffi::dxil_spv_shader_stage_DXIL_SPV_STAGE_INTERSECTION,
        Stage::Intersection,
    ),
    (
        ffi::dxil_spv_shader_stage_DXIL_SPV_STAGE_CALLABLE,
        Stage::Callable,
    ),
];

/// One stage, translated: the module, the entry point to name it by, and the
/// stage it belongs in.
///
/// `entry` is the name the module's own `OpEntryPoint` gives the stage, not the
/// name the DXIL had: the converter renames it, so a pipeline that used the
/// DXIL name would name an entry point its module does not have.
#[derive(Debug)]
pub struct Translation {
    pub stage: Stage,
    pub entry: String,
    pub words: Vec<u32>,
}

/// The converter's version, for the boot line.
pub fn version() -> (u32, u32, u32) {
    let (mut major, mut minor, mut patch) = (0, 0, 0);
    unsafe { ffi::dxil_spv_get_version(&mut major, &mut minor, &mut patch) };
    (major, minor, patch)
}

/// Translates one DXIL container — the DXBC container `dxc -Fo` writes, with
/// its `DXIL` part — into the SPIR-V module for its stage.
pub fn translate(container: &[u8]) -> Result<Translation> {
    if container.is_empty() {
        bail!("a DXIL container is never empty");
    }

    // SAFETY: the container outlives every call below; each handle is freed by
    // its guard in declaration order, so the converter releases before the
    // parsed blob it points into.
    unsafe {
        let mut blob = std::ptr::null_mut();
        check(
            ffi::dxil_spv_parse_dxil_blob(container.as_ptr().cast(), container.len(), &mut blob),
            "parsing the DXIL container",
        )?;
        let blob = ParsedBlob(blob);

        let stage = Stage::from_raw(ffi::dxil_spv_parsed_blob_get_shader_stage(blob.0))?;

        let mut converter = std::ptr::null_mut();
        check(
            ffi::dxil_spv_create_converter(blob.0, &mut converter),
            "creating the converter",
        )?;
        let converter = Converter(converter);

        check(
            ffi::dxil_spv_converter_run(converter.0),
            "translating the DXIL program",
        )?;

        let mut compiled: ffi::dxil_spv_compiled_spirv = std::mem::zeroed();
        check(
            ffi::dxil_spv_converter_get_compiled_spirv(converter.0, &mut compiled),
            "reading the compiled module",
        )?;
        if compiled.data.is_null() || compiled.size < 5 * std::mem::size_of::<u32>() {
            bail!(
                "the converter returned {} bytes of SPIR-V, which is no module",
                compiled.size
            );
        }
        let words = slice::from_raw_parts(
            compiled.data.cast::<u32>(),
            compiled.size / std::mem::size_of::<u32>(),
        )
        .to_vec();

        let entry = entry_point_name(&words)?;
        Ok(Translation {
            stage,
            entry,
            words,
        })
    }
}

/// The name the module gives its entry point.
///
/// `OpEntryPoint` is `execution model, entry point id, name, interfaces...`, and
/// a DXIL program holds one entry point, so the first one found is the stage's.
fn entry_point_name(words: &[u32]) -> Result<String> {
    const OP_ENTRY_POINT: u32 = 15;
    const HEADER: usize = 5;

    let mut at = HEADER;
    while at < words.len() {
        let opcode = words[at] & 0xFFFF;
        let length = (words[at] >> 16) as usize;
        if length == 0 || at + length > words.len() {
            break;
        }
        if opcode == OP_ENTRY_POINT && length >= 4 {
            let mut name = Vec::with_capacity(8);
            for word in &words[at + 3..at + length] {
                let bytes = word.to_le_bytes();
                let end = bytes.iter().position(|&byte| byte == 0).unwrap_or(4);
                name.extend_from_slice(&bytes[..end]);
                if end < 4 {
                    break;
                }
            }
            if name.is_empty() {
                bail!("the translated module's entry point has no name");
            }
            return Ok(String::from_utf8_lossy(&name).into_owned());
        }
        at += length;
    }
    bail!("the translated module has no entry point")
}

struct ParsedBlob(ffi::dxil_spv_parsed_blob);

impl Drop for ParsedBlob {
    fn drop(&mut self) {
        unsafe { ffi::dxil_spv_parsed_blob_free(self.0) };
    }
}

struct Converter(ffi::dxil_spv_converter);

impl Drop for Converter {
    fn drop(&mut self) {
        unsafe { ffi::dxil_spv_converter_free(self.0) };
    }
}

fn check(code: ffi::dxil_spv_result, what: &str) -> Result<()> {
    if code == ffi::dxil_spv_result_DXIL_SPV_SUCCESS {
        return Ok(());
    }
    Err(anyhow!("{what}: {} (code {code})", describe(code)))
}

fn describe(code: ffi::dxil_spv_result) -> &'static str {
    const ERRORS: &[(ffi::dxil_spv_result, &str)] = &[
        (
            ffi::dxil_spv_result_DXIL_SPV_ERROR_OUT_OF_MEMORY,
            "out of memory",
        ),
        (
            ffi::dxil_spv_result_DXIL_SPV_ERROR_GENERIC,
            "the converter failed",
        ),
        (
            ffi::dxil_spv_result_DXIL_SPV_ERROR_UNSUPPORTED_FEATURE,
            "unsupported feature",
        ),
        (
            ffi::dxil_spv_result_DXIL_SPV_ERROR_PARSER,
            "the container did not parse",
        ),
        (
            ffi::dxil_spv_result_DXIL_SPV_ERROR_FAILED_VALIDATION,
            "the program failed validation",
        ),
        (
            ffi::dxil_spv_result_DXIL_SPV_ERROR_INVALID_ARGUMENT,
            "invalid argument",
        ),
        (ffi::dxil_spv_result_DXIL_SPV_ERROR_NO_DATA, "no data"),
    ];
    ERRORS
        .iter()
        .find(|(result, _)| *result == code)
        .map(|(_, text)| *text)
        .unwrap_or("unknown error")
}

#[cfg(test)]
mod tests {
    use naga::AddressSpace;
    use naga::valid::{Capabilities, ValidationFlags, Validator};

    use super::*;

    fn module(translation: &Translation) -> naga::Module {
        let bytes: Vec<u8> = translation
            .words
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .collect();
        let module = naga::front::spv::parse_u8_slice(
            &bytes,
            &naga::front::spv::Options {
                adjust_coordinate_space: false,
                strict_capabilities: false,
                block_ctx_dump_prefix: None,
            },
        )
        .expect("the translated module parses");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("the translated module validates");
        module
    }

    #[test]
    fn the_triangle_vertex_stage_translates() {
        let translation = translate(TRIANGLE_VS).expect("the vertex shader translates");
        assert_eq!(translation.stage, Stage::Vertex);
        assert_eq!(
            translation.entry, "main",
            "the module's own entry point name"
        );
        assert_eq!(translation.words[0], 0x0723_0203, "SPIR-V magic");
        assert!(translation.words.len() > 100, "a real module, not a stub");
        module(&translation);
    }

    #[test]
    fn the_triangle_pixel_stage_translates_and_keeps_its_uniform_buffer() {
        let translation = translate(TRIANGLE_PS).expect("the pixel shader translates");
        assert_eq!(translation.stage, Stage::Pixel);
        assert_eq!(translation.entry, "main");
        module(&translation);

        // The renderer binds one buffer at set 0, binding 0 and writes the tint
        // into it, so the translation has to land the constant buffer there.
        let module = module(&translation);
        let bindings: Vec<_> = module
            .global_variables
            .iter()
            .filter_map(|(_, variable)| Some((variable.binding?, variable.space)))
            .collect();
        assert_eq!(bindings.len(), 1, "one bound resource: {bindings:?}");
        let (binding, space) = bindings[0];
        assert_eq!((binding.group, binding.binding), (0, 0));
        assert!(
            matches!(space, AddressSpace::Uniform | AddressSpace::Storage { .. }),
            "the constant buffer is a buffer: {space:?}"
        );
    }

    #[test]
    fn a_truncated_container_is_an_error_not_a_crash() {
        let error =
            translate(&TRIANGLE_VS[..64]).expect_err("a cut-off container cannot translate");
        assert!(
            error.to_string().contains("parsing the DXIL container"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn the_converter_reports_its_version() {
        let (major, minor, _patch) = version();
        assert_eq!((major, minor), (2, 72));
    }
}
