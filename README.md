# VIctory on Android

An Android host and a Vulkan backend for [VIctory](https://github.com/Marvisak/VIctory),
the compatibility layer that runs an Xbox Series title from files you supply.
This fork takes the GPU side to an ARM64 phone: the title's own shaders,
translated on the device, drawn by a Vulkan backend written for it.

It does **not** play the title yet. Its shaders run on the phone; its code does
not run anywhere on ARM64, and until that exists there is no menu, no frame of
the game, nothing but what the shaders themselves compute.

**This repository contains no game files, code or assets.** Shader containers
come from your own dump, on your own device, and only the SPIR-V the translator
derives from them is kept anywhere.

## Status: a proof of concept

This is a proof of concept, published so the GPU-side work is readable and
reusable. It is not a product and it is not on a schedule: it gets worked on
when its author has time, which means there is no roadmap, no support, and no
promise that a given bug gets fixed. Issues and pull requests are welcome, and
they may sit unanswered for a long while. Read it as a worked example of
running a console title's own shaders on a phone, not as a port that is coming.

## What runs on the phone

Verified on a Galaxy S25 Ultra (`SM-S938W`), Android 17 / SDK 37, Adreno 830,
Vulkan 1.3.284, driver 512.800.64, 3120×1440:

| | |
| --- | --- |
| Vulkan device, swap chain, frame loop | 60–62 fps at 3120×1440, `FIFO` present, pipelined cache persisted |
| DXIL → SPIR-V translation | in process, `dxil-spirv` 2.72.1 built into the APK, no PC in the loop |
| The title's shader programs | all 12 in the image translate; 11 build pipelines and draw |
| Its four-target clear | a four-attachment pass, then sampled back into the frame |
| Descriptor layouts | reflected out of each module's own SPIR-V, bindless and unbounded arrays included |
| Pipeline state | blend, rasterizer, depth, topology and samples translated from the console's D3D12 numbers |
| Textures | upload through a staging buffer, two barriers, a sampler, a reflected set |

The one program that still does not draw is the title's vertex stage. It takes
no vertex attributes at all: it reads `gl_VertexIndex`, `gl_InstanceIndex` and
`gl_BaseVertex`, and fetches its vertices from a `Buffer` SRV, which Vulkan binds
as a uniform texel buffer. The backend can describe and bind that; wiring a
synthetic vertex buffer into a pipeline and drawing it with the title's own
pixel stage is the next step, not a missing capability.

## Requirements

* An ARM64 Android phone with a Vulkan driver — Adreno 800-series is what this
  was developed and tested against.
* Rust: the toolchain is pinned in `rust-toolchain.toml` with the
  `aarch64-linux-android` target (`rustup toolchain install` picks it up).
* The Android NDK (r27c is what this was built with), the Android SDK with
  build-tools and platform 34+, and a JDK 21.
* `cmake` and `ninja` on `PATH` — the DXIL translator is C++ and CMake builds it.
* `libclang`, because the translator's bindings are generated. `LIBCLANG_PATH`
  pointing at `clang/native` is enough; `scripts/android/env.ps1` finds the one
  Python's `libclang` wheel installs.

## Build

Everything runs from `scripts/build-android.ps1`, which sets the toolchain up,
builds `libvictory_android.so`, stages it beside the manifest and (with
`-Assemble`) packages the APK:

```powershell
# find the toolchain, build, package
.\scripts\build-android.ps1 -Assemble `
    -Ndk  C:\path\to\android-ndk-r27c `
    -Sdk  C:\path\to\sdk `
    -JavaHome C:\path\to\jdk-21

# and install it on the device adb sees
.\scripts\build-android.ps1 -Assemble -Install -Ndk ... -Sdk ... -JavaHome ...

# debug and trace logging in logcat
.\scripts\build-android.ps1 -Assemble -Install -Diagnostics -Ndk ...
```

`-Ndk`, `-Sdk` and `-JavaHome` are optional when `ANDROID_NDK_HOME`,
`ANDROID_HOME` and `JAVA_HOME` already point at them. There is no Gradle
project to run by hand: `android/` is a two-file Gradle build whose only
JNI library is the one the script stages, and the activity is the framework's
own `android.app.NativeActivity`, so there is no Java or Kotlin in the app.

To build the library alone, without the APK:

```powershell
.\scripts\build-android.ps1 -Ndk ... -Sdk ... -JavaHome ...
```

## Install and run

```bash
adb install -r android/app/build/outputs/apk/debug/app-debug.apk
adb shell am start -n com.victory.android/android.app.NativeActivity
adb logcat -s victory:V
```

The app is `com.victory.android`; its whole surface is `libvictory_android.so`
plus a manifest. Dropping the title's own shaders in is optional — without them
the host draws the shader it ships — but it is the point of the exercise:

```bash
# one shader container per program, from your own dump
adb shell run-as com.victory.android mkdir -p files/victory/shaders

# a pixel stage, given as any .dxil container the translator takes
adb shell "run-as com.victory.android sh -c 'cat > files/victory/shaders/title.dxil'" < clear.dxil

# the four-target clear, if you have it
adb shell "run-as com.victory.android sh -c 'cat > files/victory/shaders/title-multi4.dxil'" < multi4.dxil
```

Containers come from `victory translate`, which writes the DXIL it emits for
each of the image's programs:

```powershell
$env:VICTORY_EMIT_DUMP = 'C:\scratch\dxil'
cargo run --release -p victory -- translate <path to your dump's executable> --list
```

The host takes the first container whose interface it can feed and reports the
rest, so a directory of them is fine. Nothing is uploaded anywhere.

## What you should see

A flat fill in the title's clear colour, pulsing slowly, with a brighter patch
where the four-target pass's output is sampled back into the frame. That is not
a screenshot of the game: the pixel shaders are the title's machine code,
translated, told only the frame's tint — which is exactly what they do for the
engine's own clears.

If a driver refuses part of it, the host stays up and says which part is off
(`the textured draw is off`, `the title's multi-target pass is off`) and draws
what remains.

## Logs from the device

```
victory_android::render: Vulkan device device=Adreno (TM) 830 vendor=Qualcomm vendor_id=0x5143
  device_id=0x44050001 kind=INTEGRATED_GPU api=1.3.284 driver=512.800.64
victory_android::render: swap chain format=R8G8B8A8_UNORM present_mode=FIFO images=4
  extent=3120x1440 transform=ROTATE_90 composite_alpha=INHERIT
victory_dxil: DXIL translated to SPIR-V converter=2.72.1 vertex=257 fragment=373
victory_android::title: the title's shader runs shader=title.dxil stage=Pixel varying=true words=196
victory_android::title: the title's shader runs shader=title-multi4.dxil stage=Pixel varying=true colours=4 words=334
victory_android::render: uploaded a sampleable image format=R8G8B8A8_UNORM extent=256x256 texels=65536
victory_android::render: the textured draw's state comes from a description format=R8G8B8A8_UNORM
  source=SRC_ALPHA destination=ONE_MINUS_SRC_ALPHA cull= topology=TRIANGLE_LIST samples=TYPE_1
victory_android::render: the title's multi-target clear has a pass colours=4 format=R8G8B8A8_UNORM extent=3120x1440
victory_android: Vulkan host ready width=3120 height=1440
victory_android::render: presenting frames=312 fps=62.2 extent=3120x1440 queue=0
```

## Tests

```
cargo test -p victory-gpu vulkan::        19 passed
cargo test -p victory-gpu                339 passed, 2 failed
cargo test -p xtask                        6 passed
cargo check --workspace                   clean
cargo check --workspace --exclude victory --target aarch64-linux-android
                                          clean
cargo xtask policy --staged               every staged file ok
```

The 19 are the backend's: SPIR-V reflection (uniform buffers, image and sampler
pairs, bindless arrays with their lengths, unbounded runtime arrays, texel
buffers), the descriptor union and unbounded cap, and the D3D12 → Vulkan state
translation (every blend factor and operation, all eight comparison functions,
write masks, the front-face flip, integer targets refusing to blend, four
targets becoming four blend attachments).

The 2 failures are `indirect::tests` against the Windows D3D12 host — a
subsystem this fork does not touch and cannot reach on Android. They fail on a
machine with a D3D12 adapter and skip on one without.

## What is missing

In the order it is being built:

1. **The title's vertex stage in a pipeline.** The descriptor kind exists; what
   remains is a host-supplied texel buffer of synthetic vertices, drawn with the
   title's vertex and pixel stages together.
2. **Descriptor heaps** as the title sees them: one update-after-bind set per
   heap, addressed by the 2^n-slot index its shaders use.
3. **Resource states and barriers** driven by the title's own command stream
   instead of this host's two passes.
4. **Command lists**: `list.rs`'s put-pointer protocol landing on
   `VkCommandBuffer`, which is when the backend stops drawing demo passes and
   starts executing recorded ones.
5. **The title's code.** An ARM64 phone cannot execute the x86-64 the loader
   maps, so this is the gate: FEXCore (FEX-Emu's JIT, MIT) built without its
   Linux frontend, with `victory-hle` supplying the console's kernel through
   host trampolines, guest address space, TLS and exception handling.
   [docs/android.md](docs/android.md) works through what that takes. On a
   16 KiB-page device it cannot work at all, because FEXCore needs 4 KiB pages.

Everything up to 4 is the GPU half and is where the work has been. 5 is the
half that decides whether a menu ever appears, and no amount of finished
backend gets there without it.

## Layout

```
apps/victory-android/     the host: NativeActivity, Vulkan surface, frame loop,
                          the shader plan, the textured draw, the offscreen pass
crates/victory-gpu/vulkan backend: pipelines and PSO state, reflection and
                          descriptor layouts, images and texel buffers
crates/victory-dxil/      DXIL → SPIR-V in process (dxil-spirv, built into the APK)
crates/victory-shader/    the console's shader model: DXIL emission and analysis
crates/victory-formats/   the title's container, texture and program formats
crates/victory-hle/       the console's kernel and system API (Windows today)
android/                  the Gradle project that packages the APK
docs/android.md           what is built, what each missing layer takes, and why
scripts/android/          toolchain discovery for the NDK/SDK/JDK builds
```

## Asset policy

The repository holds only source. `cargo xtask policy` is an allowlist — file
types, size limits, no binary signatures, no smuggled data — and it runs on
every commit through `.githooks/pre-commit` (`cargo xtask install-hooks`). Four
files are recorded exceptions with hashes and reasons, in
[docs/asset-policy.md](docs/asset-policy.md). Game data lives outside the tree,
never in it.

## Licence and lineage

GPL-3.0, the licence of the project this forks; see [LICENSE](LICENSE). A fork
of [VIctory](https://github.com/Marvisak/VIctory) by way of
[VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX); the fork this
tree was taken from is no longer published. The Android host, the Vulkan
backend and the DXIL translation path are what this fork adds. Not affiliated
with or endorsed by the title's developer or publisher; bring your own dump.
