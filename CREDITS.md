# Credits and provenance

Project: VIctory on Android — an Android host and Vulkan backend for VIctory.
Author: HXCK3D <Hxck3dBuisness@gmail.com> (the Android host, the Vulkan
backend under `crates/victory-gpu/src/vulkan`, and the DXIL translation path).

Upstream, without which none of this exists:

* [VIctory](https://github.com/Marvisak/VIctory) — the compatibility layer this
  is a fork of, and where the loader, HLE, D3D12 host and shader model come
  from.
* [VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX) — the fork
  this tree was taken through.
* [dxil-spirv](https://github.com/unknownworlds/dxil-spirv) — DXIL to SPIR-V,
  built into the APK.
* [naga](https://github.com/gfx-rs/wgpu) — the WGSL front end the host's own
  stages are written in.

## Provenance

Every commit in this repository is authored as `HXCK3D`, and the release
archives are recorded by SHA-256 in the commit that produces them. If you find
this code in a repository that has dropped this file, the original is
`https://github.com/HXCK3D-off/victory-android` and the licence is GPL-3.0 —
copying it without this notice and the licence is a licence violation, not a
grey area.

## Licence

GPL-3.0; see [LICENSE](LICENSE). This applies to the whole tree, including the
parts this fork added.
