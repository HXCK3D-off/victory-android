# Android on aarch64: what this port changes, and what is left

The desktop build runs an Xbox Series title by mapping its original x86-64
image into the process and letting the host's own Windows loader and D3D12
driver do the work. Android has neither an x86-64 CPU to run that image on nor
D3D12 to draw with, so the port splits into three layers, and only the first
two of them are code you can build today.

| Layer | State | Where |
|---|---|---|
| Portable crates: PE analysis, formats, containers, shader translation, offline conversion | builds for `aarch64-linux-android` | `victory-pe`, `victory-formats`, `victory-game`, `victory-pipeline`, `victory-shader` |
| Shaders: DXIL in, SPIR-V out, on the CPU | **done**, runs on the phone | `victory-dxil` |
| Host: native activity, Vulkan device and swap chain, app-private storage, logcat | **done**, builds into an APK | `apps/victory-android`, `android/` |
| Title execution: x86-64 code on ARM64 | **missing** — needs a translation layer | `victory-loader` (today: Windows only) |
| Graphics: D3D12 on host D3D12 | **missing** — the Vulkan backend has started: pipeline state from a description (multi-target included), descriptor layouts from reflection, one texture path; no command lists yet | `victory-gpu` (executor today: `cfg(windows)` D3D12; `vulkan` for Android) |
| Xbox system modules | **missing** — needs an Android host for threads, TLS, files, audio, GameInput | `victory-hle` (today: Windows only) |

The host is the milestone to build first because everything else has to be
proven against something: a driver that opens, a surface that presents, and a
log that says what happened. It is deliberately small.

## One-time setup

Android Studio's SDK Manager installs the SDK, NDK (side by side, r27 or newer)
and a JDK 17+. Without Android Studio, `sdkmanager` is enough:

```bash
sdkmanager "platforms;android-35" "build-tools;35.0.0" "platform-tools" "ndk;27.2.12479018"
```

Then, once per checkout:

```bash
rustup target add aarch64-linux-android
```

`.cargo/config.toml` already names the linker and the 16 KB page-size flags for
`aarch64-linux-android`; `scripts/android/env.ps1` and `scripts/android/env.sh`
put the NDK's `clang` on `PATH` and set `CC_…`/`AR_…` for crates with C code.

## Build

Windows:

```powershell
.\scripts\build-android.ps1 -Assemble
.\scripts\build-android.ps1 -Assemble -Install     # and adb install -r
```

Linux and macOS:

```bash
scripts/build-android.sh --assemble
scripts/build-android.sh --assemble --install
```

What the script does, and what can be run by hand:

```bash
# 1. the native library
cargo build --release -p victory-android --target aarch64-linux-android

# 2. stage it next to the manifest
cp target/aarch64-linux-android/release/libvictory_android.so \
   android/app/src/main/jniLibs/arm64-v8a/

# 3. the APK
cd android && ./gradlew assembleDebug        # or assembleRelease
```

The script also refuses a library whose `LOAD` segments are not 16 KB aligned,
which is what Android 15 requires and what the Play Store checks on upload.

Signing a release build is environmental, so no key material lives here:

```powershell
$env:VICTORY_KEYSTORE          = 'C:\Users\me\victory.jks'
$env:VICTORY_KEYSTORE_PASSWORD = '…'
$env:VICTORY_KEY_ALIAS         = 'victory'
$env:VICTORY_KEY_PASSWORD      = '…'
.\scripts\build-android.ps1 -Assemble -Release
```

## Checking the port without an APK

The whole workspace type-checks for `aarch64-linux-android` except the Windows
command line front end, which is not part of the app: its `translate --validate`
loads `dxil.dll` and its `--pipelines` creates D3D12 pipelines, and neither has
a meaning on a phone.

```bash
cargo check --workspace --exclude victory --target aarch64-linux-android
```

## What this port touches, for review

| Path | Change |
|---|---|
| `apps/victory-android/` | new: the Android host (entry point, logcat, Vulkan) |
| `android/` | new: the Gradle project, manifest, and the packaging rules |
| `scripts/android/env.ps1`, `env.sh` | new: NDK/SDK/JDK discovery |
| `scripts/build-android.ps1`, `.sh` | new: build, strip, check alignment, package |
| `.cargo/config.toml` | `aarch64-linux-android` linker and 16 KB page flags |
| `rust-toolchain.toml` | the Android target joins the pinned toolchain |
| `crates/victory-core/src/logging.rs` | tracing goes to logcat on Android |
| `crates/victory-core/Cargo.toml` | `android_logger` and `log`, target-gated |
| `crates/victory-loader/src/lib.rs` | `crash` and `publish` are Windows-only, and now say so |
| `crates/victory-gpu/src/list.rs`, `resource.rs` | non-Windows answers for two host-only questions |
| `crates/victory-hle/src/` | the Windows-only edges of four modules are gated |
| `README.md` | a pointer to this file |

Nothing in those crates changes behaviour on Windows: every edit adds a
`cfg(not(windows))` counterpart or turns an existing Windows-only assumption
into an explicit gate.

## What the APK is, and what it proves

There is no Java or Kotlin in it. The activity is the framework's own
`android.app.NativeActivity`, which loads `libvictory_android.so` and calls its
`android_main`, so the whole app is one `.so` plus a manifest. On launch it:

1. installs `VICTORY_DATA_DIR`, `VICTORY_CACHE_DIR` and `HOME` under
   `<app>/files/victory` and starts the logcat logger;
2. enumerates Vulkan devices, logs each one with vendor, driver version and
   limits, and prefers the integrated, phone-vendor adapter;
3. creates the instance, the `VK_KHR_android_surface` on the activity's
   `ANativeWindow`, the device, the swap chain and the render pass;
4. publishes the phone's subgroup size to `victory-shader`
   (`set_host_max_wave`), because the translator's wave-size assumption is a
   desktop default otherwise;
5. loads and saves a Vulkan pipeline cache under the cache directory, which is
   the Android counterpart of `victory warm`'s cache and the difference between
   a first launch that stutters and one that does not;
6. draws a full-screen triangle over the clear, tinted from a mapped uniform
   buffer, through a pipeline built against the swap chain's render pass and
   its own pipeline cache;
7. uploads a 256x256 image through a staging buffer and a layout transition,
   and draws it over the title's clear from a descriptor set whose layout came
   out of the shaders' own SPIR-V;
8. presents, rebuilding the swap chain on a real resize, and writes a line to
   logcat when it cannot.

The triangle is the interesting part. Its two stages are DXIL — HLSL compiled
by `dxc`, the shape `victory-shader` emits for the console — translated to
SPIR-V on the phone by `victory-dxil` (`crates/victory-dxil`), so the renderer
receives plain SPIR-V words and nothing else. `naga` carries the same shader as
WGSL and is what the host falls back to when the translation cannot run, so the
pipeline still draws and logcat says which road it took. It has no vertex
buffer (positions come from the vertex index), one varying, one uniform buffer
and one colour target: the smallest pipeline that still exercises everything a
translated console shader needs.

Watch it:

```bash
adb logcat -s victory:V
```

A healthy start, captured from a Galaxy S25 Ultra (`SM-S938W`, Android 17,
SDK 37, Adreno 830) at 3120x1440, presenting at 62 fps on 4 KB pages:

```
V/victory: victory_android::boot: victory Android host version=0.1.0 abi=aarch64 sdk=37
V/victory: victory_android::boot: files data=/data/user/0/com.victory.android/files/victory cache=...
V/victory: victory_android::render: Vulkan instance api=1.1.0
V/victory: victory_android::render: Vulkan device device=Adreno (TM) 830 vendor=Qualcomm vendor_id=0x5143
             device_id=0x44050001 kind=INTEGRATED_GPU api=1.3.284 driver=512.800.64
V/victory: victory_android::render: Vulkan limits subgroup_size=64 max_subgroup_size=64 lanes=64
             max_push_constants=256 max_storage_buffer_range=2147483647 max_descriptor_sets=7
V/victory: victory_android::render: swap chain format=R8G8B8A8_UNORM present_mode=FIFO images=4
             extent=3120x1440 transform=ROTATE_90 composite_alpha=INHERIT min_images=4 max_images=64
V/victory: victory_android: Vulkan host ready width=3120 height=1440
V/victory: victory_android::render: loaded the Vulkan pipeline cache bytes=32
V/victory: victory_android::render: presenting frames=313 fps=62.6 extent=3120x1440 queue=0
```

`bytes=32` is the whole story of the Adreno pipeline cache: the driver accepts
an initial cache and returns a 32-byte header, never the compiled pipelines a
desktop driver would serialise. The path is wired anyway, because the desktop
counterpart (`victory warm`) is where the cost shows up and some mobile
drivers do return data.

The `lanes=64` is the number that matters to the shader side: Adreno 830 reports
a 64-lane subgroup, and the host hands that to `victory-shader` instead of the
desktop default.

### What the device found

Three things only hardware shows, kept here so the next layer does not repeat
them:

- `VK_SUBOPTIMAL_KHR` comes back from `vkQueuePresentKHR` on Adreno while the
  window's geometry settles, and an `ANativeWindow` resize callback can fire for
  a size that did not change. Rebuilding the swap chain for either produces a
  loop that never draws: the first build on the phone did exactly that, 313
  frames turned into 313 rebuilds. `Renderer::resize` now ignores a same-size
  request and `draw` rebuilds only on `VK_ERROR_OUT_OF_DATE_KHR` or a real size
  change.
- The Adreno surface lists `INHERIT` as its only supported composite alpha
  mode and accepts `OPAQUE` anyway. The host now picks the first supported mode
  rather than the one it would prefer.
- `ash` 0.38's `push_next` takes and returns the struct by value. Written as
  `let _ = props.push_next(..)` it silently drops the whole `p_next` chain, and
  the driver reports `subgroupSize = 0` for both the core and the
  `VK_EXT_subgroup_size_control` structs.

If the log stops at `Vulkan host failed`, the driver refused one of those
steps and the error is the driver's own `VkResult`; there is no fallback path,
on purpose, because a black window that retries forever is worse than a log
line that names the failure.

## What the remaining layers take

### 1. Executing the title's x86-64 code (the blocker)

`victory-loader` maps the title's PE image and runs its code as native x86-64
inside the process (see `crates/victory-loader/src/lib.rs`). That is not
portable: an ARM64 phone cannot execute those instructions, and writing a
translator from scratch is a decade of instruction coverage, not a milestone.

What is realistic is borrowing one, in the shape the PS4-on-Android ports
already use: **FEXCore**, the x86-64→ARM64 JIT at the heart of FEX-Emu (MIT),
built with its Linux frontend patched out (`BUILD_FEXCORE_ONLY`) and an
emulator's own HLE standing in for the guest's operating system. Those ports run
in an ordinary Android app — no rootfs, no Wine, no Box64 — which is this
project's shape exactly, with an Xbox guest instead of a PlayStation one.

What that costs, and what this port has to provide:

- **Guest address space**: FEXCore translates from guest addresses and needs to
  own the layout; the loader's PE mapping has to be laid out where it expects.
- **Entry thunks**: guest code pointers must go through the translator rather
  than a direct jump, and indirect calls have to be redirected the same way.
- **Host trampolines**: the title's imports resolve to native ARM64 stubs that
  call `victory-hle` — the same export table Xenia uses, arrived at from the
  other side.
- **TLS and exceptions**: the console's FS base and its SEH-style unwinding are
  the host's to emulate, not the translator's.
- **4 KiB pages.** FEXCore requires them. The S25 Ultra here runs a 4 KiB
  kernel (`6.6.127-android15-…-4k`, `getconf PAGE_SIZE` = 4096); a 16 KiB-page
  device cannot use this path at all, and the port should say so plainly.
- **A code cache on disk**: FEX's cache is what stops the title re-translating
  everything on each launch (~130 MB in those ports).

None of that touches the work below: formats, the HLE surface, the Vulkan
backend and the loader's PE mapping are all translator-independent, which is
what makes the CPU layer a known quantity rather than a rewrite.

The alternative — running the title on a translation layer off-device and
streaming frames to the phone — keeps the desktop pipeline intact and turns
Android into a thin client, but it is a different product.

### 2. A Vulkan backend for `victory-gpu`

`victory-gpu` implements the console's D3D12.X objects (`device`, `list`,
`resource`, `descriptor`, `pipeline`, `root`, `fence`, `queue`, …) and performs
them with host D3D12, which is why every one of those modules is `cfg(windows)`.
A Vulkan backend is the same object model with a different executor:

- `resource`/`descriptor`: heaps and descriptor tables become
  `VkDeviceMemory` allocations and descriptor sets with update-after-bind.
  `vulkan::reflect` reads a translated module's bindings back out of its
  SPIR-V, `vulkan::descriptor` turns them into set layouts and pipeline
  layouts, and `vulkan::image` is the first resource: a staging upload, the two
  barriers and a sampler. What is left is the heap as the title sees it (a
  table of 2^n slots), which maps onto one update-after-bind set per heap, and
  the resource states the title's own command stream will ask for.
- `list`/`queue`: command lists become `VkCommandBuffer`s; the console's
  put-pointer protocol is already abstracted in `list.rs`.
- `pipeline`: pipelines come from the SPIR-V that `victory-shader` already
  emits as DXIL. That conversion is `victory-dxil` (Valve's `dxil-spirv`, built
  from C++ into the APK and driven in-process), and it works on the phone
  today; see "DXIL to SPIR-V" below for what it took. What is left is the
  batch path: `victory translate` writes the emitted DXIL into the run's shader
  cache, and nothing yet feeds those containers back through the converter.
- barriers and render passes: a tile-based Adreno wants `LOAD_OP_CLEAR`/`DONT_CARE`
  where a desktop card would reload; the existing `host/` side already separates
  the console's states from the host's, which is where this belongs.

### 3. An Android host for `victory-hle`

The modules in `victory-hle` implement the Xbox system API on top of Windows:
threads, TLS, files, audio (`XAudio` on WASAPI), GameInput on XInput, and the
D3D12.X entry points themselves. Each needs an Android equivalent — `pthread`/
futex for threads, AAudio or OpenSL ES for audio, Android input events for
GameInput — but none of them can be tested before layer 1 runs code.

## DXIL to SPIR-V

`victory-shader` turns the console's machine code into DXIL, which is what a
D3D12 host wants and no phone driver accepts. `crates/victory-dxil` closes that
gap with Valve's `dxil-spirv`, the converter Proton runs: one function,
`translate(container) -> Translation`, over the C API, with the C++ built by
CMake for whatever target Cargo is building. The phone runs it in-process;
nothing is translated at build time.

Two host tools come with it, both handled by `scripts/android/env.ps1`:

- **CMake and Ninja.** `dxil-spirv-sys` configures and builds the C++ through
  CMake, and the Visual Studio generator cannot target Android, so the Android
  build runs Ninja (`python -m pip install cmake ninja`).
- **libclang.** The bindings come from bindgen; `env.ps1` finds `libclang` under
  `LIBCLANG_PATH`, an LLVM install, or the Python `clang` package
  (`python -m pip install libclang`).

Three things it cost, worth knowing before touching it again:

- **libc++ instead of libstdc++.** `dxil-spirv-sys` asks every non-Windows link
  for `-lstdc++`, which the NDK does not have. `victory-dxil`'s build script
  gives the link a private directory holding an empty `libstdc++.a` for that
  request and the NDK's `libc++_static.a` and `libc++abi.a` beside it. The
  directory has to be private: the NDK keeps `libc.a` next to those archives,
  the linker searches `-L` before its own sysroot, and pointing at the NDK's
  library directory makes `-lc` resolve to static bionic. That links a second
  copy of libc into the app, whose aux vector is null inside a shared library,
  and the process segfaults in `getauxval` during load before any Rust runs.
- **`shaderDrawParameters`.** A translated vertex stage reads `gl_VertexIndex`
  and `gl_BaseVertex` (D3D's `SV_VertexID` excludes the base vertex), so its
  module carries the `DrawParameters` capability and the device must have the
  feature enabled at creation. `create_device` queries and enables it, and the
  host falls back to the WGSL path on a device that does not have it.
- **The entry point is renamed.** The module's `OpEntryPoint` says `main`
  whatever the DXIL function was called, so naming the pipeline stage after the
  DXIL name fails pipeline creation with nothing more useful than
  `VK_ERROR_UNKNOWN`. `Translation::entry` is read back out of the emitted
  module for that reason.

What it proves on the phone (Galaxy S25 Ultra, Adreno 830): both stages
translate on the first frame, the pipeline builds, and the result presents at 62
fps with the glow the HLSL describes — centre pixel `(182, 26, 217)`,
quarter-point `(144, 21, 171)`, corner `(8, 1, 10)`.

### On this title's own shaders

The dump at hand is the Xbox build. Every `DXIL` part in `GTAVI.exe` carries the
Xbox program header, an `XDAT` table, an `XBCH` blob, and where the bitcode
would be the four bytes `NOBC`: the bitcode is stripped and only the native GCN
program is left. That does not block the pipeline — `victory translate` emits
DXIL from the native code, and `victory-dxil` converts that — it only means the
work starts one step earlier:

```powershell
# the title's programs into DXIL, one container each
$env:VICTORY_EMIT_DUMP = 'C:\scratch\dxil'
cargo run --release -p victory -- translate 'D:\GTA6\GTAVI.exe' --list
```

Of the twelve programs in that image, eleven are pixel shaders that read
`SV_Position` and one `float4` varying and write one colour, binding no
resources: they are the engine's render-target clear and copy shaders. Win32
got 12 emitted, 0 validated (validation needs `dxil.dll` and D3D12) and all
twelve then translated to SPIR-V.

Drop a container into the app's own directory and the host draws with it:

```bash
adb shell run-as com.victory.android mkdir -p files/victory/shaders
adb shell "run-as com.victory.android sh -c 'cat > files/victory/shaders/title.dxil'" < clear.dxil
```

`apps/victory-android/src/title.rs` takes the first container there whose
interface it can feed — a pixel stage, one `float4` colour at location 0, no
bound resources, and at most one `float4` varying at location 1 — translates it
on the phone and builds the pipeline around it. The vertex stage is the host's
scaffolding: the same full-screen triangle, handing the frame's tint over as the
`flat` varying the title's shader writes out. That is what the title's own
vertex stage does for these shaders, and it is why the frame is a flat fill of
the clear colour rather than a triangle: the shader is doing its job.

Anything else — a shader that binds textures, a mesh shader, a four-target
clear — is reported and skipped, and the host falls back to the shader it ships
with. Making those work is pipeline state, not translation: descriptors and
attachment counts come from the pipeline the title created, which is the
`victory-gpu` side of the port.

### Descriptors, and the first texture

The backend's second layer is the one every textured draw needs, and it is
built from the shader's point of view rather than from a hand-written binding
table: `vulkan::reflect` parses the SPIR-V `victory-dxil` emitted, walks each
`OpVariable` through its pointer to the type behind it, and reports the set,
the slot and what kind of descriptor it wants. `vulkan::descriptor` builds the
set layouts from the union over both stages, and the pool from what those
layouts reserve.

Two details cost real time and are worth keeping:

- An array's length operand in `OpTypeArray` is the **id of a constant**, not
  the number. Read as a number it becomes whatever id the compiler happened to
  assign, so a 16-entry bindless table lays out as 6 descriptors and the write
  goes out of bounds later. `Types::length` resolves it through `OpConstant`.
- A runtime array binds as a table with no length at all. Vulkan still wants a
  number, so reflection reports it as unbounded and the layout reserves
  `UNBOUNDED_CAP` (64) — which is also how the title's own descriptor heaps
  arrive, so they describe correctly instead of collapsing to one descriptor.

`vulkan::image` is the resource side: a host-visible staging buffer, a
`TRANSFER_DST | SAMPLED` image, one submit with an `UNDEFINED ->
TRANSFER_DST_OPTIMAL` barrier around the copy and a
`TRANSFER_DST_OPTIMAL -> SHADER_READ_ONLY_OPTIMAL` barrier after it, then the
view and a linear sampler. The host runs the whole path at startup and draws
its result over the title's clear every frame, so the phone proves it:

```
V/victory: victory_android::render: uploaded a sampleable image format=R8G8B8A8_UNORM extent=256x256 texels=65536
V/victory: victory_android: Vulkan host ready width=3120 height=1440
V/victory: victory_android::render: presenting frames=603 fps=60.2 extent=3120x1440 queue=0
```

The screenshot at that point is the title's shader filling the screen in its
clear colour with a grid of tiles over it, sampled through the descriptor set
the reflection built — image, sampler, barrier and set write all taken by the
driver in one frame. If a driver refuses any of it, the host logs
`the textured draw is off` and draws the title's clear alone: the frame is
never held hostage by the scaffolding.

### Pipeline state, from a description

A pipeline's shader half comes from the translator; its other half is fixed
state, and a title records that in its PSO description. `vulkan::pipeline`'s
`State` is that description in the form Vulkan wants, and it is built from
D3D12's own numbers rather than from a state the host invented:

- per colour target: format, blend factors and operations for colour and alpha,
  and the write mask;
- the rasterizer: fill mode, cull mode, front face, depth clip;
- the depth test: enable, write, compare;
- topology, primitive restart, and the sample count.

Two translations are worth spelling out because both are silent when wrong.
The front face **flips** (a console's clip space has y up and Vulkan's has it
down, and the host keeps the flip rather than a negative viewport height), so a
PSO that leaves `FrontCounterClockwise` at false becomes
`VK_FRONT_FACE_COUNTER_CLOCKWISE`. And blending is **forced off on integer
targets**: a title leaves `BlendEnable` on over an `R8_UINT` target, both APIs
refuse it at pipeline creation, and taking the description literally loses the
draw. Each target keeps its own blend state for that reason, which is also what
a four-target pass needs.

The host proves the state reaches the driver instead of only compiling: its
textured draw is built from a described state (`SRC_ALPHA` over
`INV_SRC_ALPHA`) with the shader writing 60% alpha, and the captured frame's
tile colour is `(156, 136, 93)` — 60% of the tile's `(242, 174, 11)` over 40% of
the title's `(26, 79, 217)` fill to within a unit per channel. An unblended
pipeline would have written the raw texel.

What `State` cannot express yet is vertex input, and reading the one vertex
program in this image says what that actually means here: it declares **no**
input variables at all. Its inputs are `gl_VertexIndex`, `gl_InstanceIndex` and
`gl_BaseVertex`, and its vertices arrive through a `Buffer` SRV — an
`OpTypeImage` with `Dim = Buffer`, which Vulkan binds as a *uniform texel
buffer*, not an image. So the title's vertex stage is not input-assembly at all:
it fetches its own vertices from a typed buffer, which is why the reflection now
distinguishes [`Kind::SampledTexelBuffer`] from a 2D image (`Dim = Buffer` versus
`Dim = 2D`) and `vulkan::image::Texels` can hold and bind one. A PSO's
attribute descriptions matter for a title that uses them; this one does not, and
the descriptor kind was the real gap.

### The title's own multi-target clear

`PS_ClearMulti4` — the one program of the twelve whose four colour outputs the
host refused — now has somewhere to write. The host builds a render pass with
four `R8G8B8A8_UNORM` attachments, a framebuffer over their views, and the
pipeline's `State` says four targets; the pass clears them, draws the title's
shader over all four, and leaves them in `SHADER_READ_ONLY_OPTIMAL` for the fill
that samples the first one. A frame is therefore two passes: the title's
multi-target clear into attachments of its own, then the title's one-target
clear as the fill with the multi-target result drawn over it.

```
the title's shader runs shader=title-multi4.dxil stage=Pixel varying=true colours=4 words=334
the title's multi-target clear has a pass colours=4 format=R8G8B8A8_UNORM extent=3120x1440
presenting frames=306 fps=61.1 extent=3120x1440 queue=0
```

The screenshot shows the fill colour with a brighter patch where the multi-target
pass's output is sampled, and none of the host's generated tiles: the texture the
overlay samples is now the title's own render output, so what is on screen came
through a four-attachment pass, a barrier into the sampling layout and a
descriptor write.

One thing a phone taught here: the first attempt crashed inside
`vkCmdPipelineBarrier` in the Adreno driver. The barriers were fine; they were
being recorded **before** `vkBeginCommandBuffer`, because the offscreen pass had
been wired in ahead of the function that begins recording. A driver that reads
barriers from a command buffer that is not recording faults on a null, which is
what the tombstone's `fault addr 0xbc` in `qglinternal::vkCmdPipelineBarrier`
was saying.

## Snapdragon 8 Elite (Adreno 830) notes

- **Tile-based deferred rendering.** The render pass above clears with
  `LOAD_OP_CLEAR` and stores, which is the cheap case. When real frames arrive,
  avoid mid-pass render target switches, and prefer `DONT_CARE` where the
  contents are overwritten.
- **Subgroup size.** Adreno 830 reports a subgroup size the shader translator
  has to know about; the host publishes it at startup. `victory-shader`'s
  `VICTORY_HOST_WAVE` exists for the desktop path for the same reason.
- **Descriptor and buffer churn.** Mobile drivers recompile when descriptor
  layouts change. `victory warm` and the pipeline cache matter more here than
  on a desktop card.
- **16 KB pages.** Android 15 devices can use 16 KB pages; the linker flags in
  `.cargo/config.toml` and the check in the build script keep the library
  loadable, and `android:extractNativeLibs="false"` keeps it mapped from the
  APK rather than copied.
- **Thermals and pacing.** `FIFO` present (what the host asks for) keeps the
  app at the panel rate with the fewest frames in flight; three swap chain
  images is the ceiling, because every extra image is memory the title cannot
  have on a phone.
- **Validation layers.** They do not ship on retail devices. Test with the
  desktop Vulkan build of the same code where possible, and treat a clean
  logcat on the phone as the only proof that counts.

## Where to plug the next layer in

`apps/victory-android/src/render.rs` owns the device, the swap chain, the
pipeline and the frame loop. A backend replaces `record`'s clear-plus-triangle
with the console's command stream; `shader.rs`'s `compile` already translates
DXIL with `victory-dxil` and only has to be pointed at the title's own
programs; `boot.rs`'s `report_layers` line should shrink as each layer lands.
Nothing in the portable crates has to change to get there.
