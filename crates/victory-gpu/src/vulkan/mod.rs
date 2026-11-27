//! The Vulkan backend.
//!
//! `victory-gpu` is written against D3D12's object model because that is what
//! the title records into: heaps, pipelines, descriptor tables, root
//! signatures. On Windows, [`crate::host`] and the modules beside it execute
//! that model on D3D12. On Android there is no D3D12, so this module is the
//! executor's counterpart: the same objects, filled in with Vulkan.
//!
//! What exists today is the bottom of the chain the title's draw calls need,
//! in the order Vulkan's own dependencies impose:
//!
//! - [`pipeline`]: a graphics pipeline built from SPIR-V that `victory-dxil`
//!   produced from the title's own DXIL, and the fixed state a title's PSO
//!   description carries — blend per target, rasterizer, depth, topology and
//!   samples, translated from the console's own numbers.
//! - [`reflect`] and [`descriptor`]: the module's own description of what it
//!   binds, turned into set layouts, a pipeline layout and a pool. Bindless
//!   tables are described here too, bounded or not.
//! - [`image`]: the first resource, and the first barriers — a staging upload
//!   into a device-local image, moved to `SHADER_READ_ONLY_OPTIMAL` before a
//!   shader reads it.
//!
//! The Android host builds and draws all three, so they are exercised every
//! frame rather than waiting for a caller.
//!
//! What is next, still in dependency order: vertex input, which is the one
//! piece of a PSO description `State` cannot express yet (a title names its
//! elements by DXIL semantic, a translated module by location); then the
//! descriptor heap as the title sees it on `UPDATE_AFTER_BIND` sets; then the
//! resource states its command stream asks for; and finally command lists,
//! which is where [`crate::list`]'s put-pointer protocol lands on
//! `VkCommandBuffer`.

pub mod descriptor;
pub mod image;
pub mod pipeline;
pub mod reflect;

pub use descriptor::Layouts;
pub use image::{Copy, Image, Texels};
pub use pipeline::{
    Blend, D3d12Blend, D3d12Depth, D3d12Raster, Graphics, Raster, Stage, State, Target,
};
