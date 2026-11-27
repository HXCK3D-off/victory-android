<#
.SYNOPSIS
    Puts the Android NDK and SDK's tools on PATH for this shell, and points
    Cargo and Gradle at them.

.DESCRIPTION
    Dot-source it to keep the variables in your shell:

        . .\scripts\android\env.ps1

    `scripts\build-android.ps1` does that itself, so running the build script
    needs no preparation.

    The C compiler is the NDK's `clang.exe` plus `--target`, rather than the
    `aarch64-linux-android<api>-clang` wrapper: the wrapper is a shell script
    and Windows cannot execute it, while `clang.exe --target=...` is the same
    driver on every host.

.PARAMETER Ndk
    The NDK root. Defaults to `ANDROID_NDK_HOME`, then the newest NDK under
    `ANDROID_HOME`, then the newest under the default Android SDK location.

.PARAMETER Sdk
    The SDK root. Defaults to `ANDROID_HOME`, then `%LOCALAPPDATA%\Android\Sdk`.

.PARAMETER Api
    The Android API level to link against. Must be at least the APK's
    `minSdk`; 33 is what `android/app/build.gradle.kts` declares.
#>
[CmdletBinding()]
param(
    [string]$Ndk,
    [string]$Sdk,
    [string]$Api = "33",
    [string]$JavaHome,
    [string]$Libclang
)

$ErrorActionPreference = 'Stop'

function Resolve-Sdk {
    param([string]$Explicit)
    if ($Explicit) { return (Resolve-Path $Explicit).Path }
    if ($env:ANDROID_HOME) { return (Resolve-Path $env:ANDROID_HOME).Path }
    if ($env:ANDROID_SDK_ROOT) { return (Resolve-Path $env:ANDROID_SDK_ROOT).Path }
    $default = Join-Path $env:LOCALAPPDATA 'Android\Sdk'
    if (Test-Path $default) { return (Resolve-Path $default).Path }
    return $null
}

function Resolve-Ndk {
    param([string]$Explicit, [string]$SdkRoot)
    if ($Explicit) { return (Resolve-Path $Explicit).Path }
    if ($env:ANDROID_NDK_HOME) { return (Resolve-Path $env:ANDROID_NDK_HOME).Path }
    if ($env:ANDROID_NDK_ROOT) { return (Resolve-Path $env:ANDROID_NDK_ROOT).Path }
    if ($SdkRoot) {
        $ndkRoot = Join-Path $SdkRoot 'ndk'
        if (Test-Path $ndkRoot) {
            $newest = Get-ChildItem $ndkRoot -Directory |
                Sort-Object { [version]($_.Name -replace '^([0-9]+\.[0-9]+\.[0-9]+).*$', '$1') } -Descending |
                Select-Object -First 1
            if ($newest) { return $newest.FullName }
        }
    }
    return $null
}

$SdkRoot = Resolve-Sdk -Explicit $Sdk
$NdkRoot = Resolve-Ndk -Explicit $Ndk -SdkRoot $SdkRoot

if (-not $NdkRoot) {
    throw @"
Android NDK not found.
Install it with Android Studio's SDK Manager ("NDK (Side by side)"), then either
set ANDROID_NDK_HOME or pass -Ndk:
    .\scripts\android\env.ps1 -Ndk 'C:\Users\me\AppData\Local\Android\Sdk\ndk\27.2.12479018'
"@
}
if (-not (Test-Path $NdkRoot)) { throw "NDK path does not exist: $NdkRoot" }

$HostTag =
    if ($IsWindows -or $env:OS -eq 'Windows_NT') { 'windows-x86_64' }
    elseif ($IsMacOS) { 'darwin-x86_64' }
    else { 'linux-x86_64' }

$ndkBin = Join-Path $NdkRoot "toolchains\llvm\prebuilt\$HostTag\bin"
if (-not (Test-Path (Join-Path $ndkBin 'clang.exe'))) {
    throw "the NDK at $NdkRoot has no llvm/prebuilt/$HostTag/bin/clang.exe"
}

$env:ANDROID_NDK_HOME = $NdkRoot
if ($SdkRoot) { $env:ANDROID_HOME = $SdkRoot }
$env:PATH = "$ndkBin;$env:PATH"
$env:VICTORY_NDK_BIN = $ndkBin

# `cc` (blake3, opus-pure's C fallback) and rustc's link step both read these.
$dashTarget = "aarch64-linux-android$Api"
$env:CC_aarch64_linux_android = 'clang'
$env:CFLAGS_aarch64_linux_android = "--target=$dashTarget"
$env:AR_aarch64_linux_android = 'llvm-ar'
$env:RANLIB_aarch64_linux_android = 'llvm-ranlib'

# The shader translator is C++: `victory-dxil` builds `dxil-spirv` through
# CMake for the target, which needs a C++ driver aimed at Android, a generator
# that can cross-compile (Ninja, since the Visual Studio generator cannot), and
# libclang for bindgen. CMake and Ninja are host tools, so they have to be on
# PATH already.
$env:CXX_aarch64_linux_android = 'clang++'
$env:CXXFLAGS_aarch64_linux_android = "--target=$dashTarget"
$env:CMAKE_GENERATOR = 'Ninja'
# CMake finds the NDK through `ANDROID_NDK`, which is not the variable the rest
# of the tooling uses, and it wants the resolved path to the generator's build
# program because `cargo`'s CMake crate does not look one up.
$env:ANDROID_NDK = $NdkRoot

foreach ($tool in @('cmake', 'ninja')) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        throw @"
$tool is not on PATH, and the shader translator's CMake build needs it:
    python -m pip install cmake ninja
"@
    }
}
$env:CMAKE_MAKE_PROGRAM = (Get-Command ninja).Source

# Gradle needs a JDK; JAVA_HOME often points at an old JRE or a JDK newer than
# Gradle runs on, so the usable one is picked explicitly.
#
# `java -version` writes to stderr, and a native command's stderr becomes a
# terminating error under `$ErrorActionPreference = 'Stop'`; ask it with that
# preference stood down.
function Get-JavaVersionText {
    param([string]$Java)
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        return ((& $Java -version 2>&1) -join "`n")
    } finally {
        $ErrorActionPreference = $previous
    }
}

function Resolve-Libclang {
    # bindgen loads libclang at build time. The NDK does not ship one on every
    # host, so this looks where LLVM and the Python `clang` package put it; a
    # machine with neither sets LIBCLANG_PATH or passes -Libclang.
    if ($Libclang) { return (Resolve-Path $Libclang).Path }
    if ($env:LIBCLANG_PATH -and (Test-Path (Join-Path $env:LIBCLANG_PATH 'libclang.dll'))) {
        return $env:LIBCLANG_PATH
    }
    $candidates = @(
        (Join-Path $NdkRoot "toolchains\llvm\prebuilt\$HostTag\bin"),
        "$env:ProgramFiles\LLVM\bin",
        "$env:LOCALAPPDATA\Programs\LLVM\bin")
    foreach ($pattern in @(
            "$env:LOCALAPPDATA\Python\pythoncore-*\Lib\site-packages\clang\native",
            "$env:LOCALAPPDATA\Programs\Python\Python3*\Lib\site-packages\clang\native",
            "$env:USERPROFILE\AppData\Roaming\Python\Python3*\site-packages\clang\native")) {
        $candidates += (Get-ChildItem $pattern -Directory -ErrorAction SilentlyContinue |
            ForEach-Object { $_.FullName })
    }
    foreach ($candidate in $candidates) {
        if ($candidate -and (Test-Path (Join-Path $candidate 'libclang.dll'))) { return $candidate }
    }
    return $null
}

function Resolve-Java {
    # Gradle 8.14 runs on Java 17 through 24 and refuses newer, so a machine
    # whose only JDK is 25 (or 8) has to be told where a usable one is. Android
    # Studio ships one in its own `jbr`, which is the one most people already
    # have.
    if ($JavaHome) {
        if (-not (Test-Path (Join-Path $JavaHome 'bin\java.exe'))) {
            throw "-JavaHome does not look like a JDK: $JavaHome"
        }
        $v = Get-JavaVersionText (Join-Path $JavaHome 'bin\java.exe')
        if ($v -match '"(\d+)') { return [pscustomobject]@{ Path = $JavaHome; Major = [int]$matches[1] } }
        throw "cannot read the Java version of $JavaHome"
    }
    $candidates = @()
    if ($env:JAVA_HOME) { $candidates += $env:JAVA_HOME }
    foreach ($base in @(
            "$env:ProgramFiles\Java",
            "$env:ProgramFiles\Eclipse Adoptium",
            "$env:ProgramFiles\Microsoft",
            "$env:ProgramFiles\Android\Android Studio\jbr",
            "$env:LOCALAPPDATA\Programs\Eclipse Adoptium",
            "$env:LOCALAPPDATA\Android\Android Studio\jbr")) {
        if (Test-Path $base) {
            if (Test-Path (Join-Path $base 'bin\java.exe')) {
                $candidates += $base
            } else {
                $candidates += (Get-ChildItem $base -Directory -ErrorAction SilentlyContinue |
                    Where-Object { $_.Name -match 'jdk|jbr' } | ForEach-Object { $_.FullName })
            }
        }
    }
    $usable = @()
    foreach ($candidate in $candidates) {
        $java = Join-Path $candidate 'bin\java.exe'
        if (-not (Test-Path $java)) { continue }
        $line = Get-JavaVersionText $java
        if ($line -match '"(\d+)') {
            $major = [int]$matches[1]
            if ($major -ge 17 -and $major -le 24) {
                $usable += [pscustomobject]@{ Path = $candidate; Major = $major }
            }
        }
    }
    if ($usable.Count -eq 0) { return $null }
    # 21 first: it is what the Android Gradle plugin is tested against.
    $preferred = $usable | Where-Object { $_.Major -eq 21 } | Select-Object -First 1
    if ($preferred) { return $preferred }
    return ($usable | Sort-Object Major -Descending | Select-Object -First 1)
}

$java = Resolve-Java
if ($java) {
    $env:JAVA_HOME = $java.Path
    $env:PATH = "$($java.Path)\bin;$env:PATH"
}

$env:LIBCLANG_PATH = Resolve-Libclang
if (-not $env:LIBCLANG_PATH) {
    throw @"
libclang not found, and the shader translator's bindings need it.
    python -m pip install libclang
or install LLVM and set LIBCLANG_PATH to the directory holding libclang.dll.
"@
}

$clangVersion = (& clang --version | Select-Object -First 1)
Write-Host "NDK   : $NdkRoot"
if ($SdkRoot) { Write-Host "SDK   : $SdkRoot" }
if ($java) { Write-Host "JDK   : $($java.Path) (Java $($java.Major))" }
else { Write-Warning 'no JDK 17-24 found (Gradle 8.14 refuses newer); install one or pass -JavaHome' }
Write-Host "clang : $clangVersion"
Write-Host "libclang: $env:LIBCLANG_PATH"
Write-Host "target: $dashTarget"
