<#
.SYNOPSIS
    Builds the Android host (libvictory_android.so) and, optionally, the APK.

.DESCRIPTION
    Three steps, each of which can be run on its own:

      1. find the NDK/SDK/JDK (`scripts\android\env.ps1`),
      2. `cargo build --release -p victory-android --target aarch64-linux-android`,
      3. stage the library into `android\app\src\main\jniLibs\arm64-v8a\`.

    `-Assemble` then runs Gradle, and `-Install` pushes the result to a
    connected device with adb.

    Examples:
        .\scripts\build-android.ps1
        .\scripts\build-android.ps1 -Assemble
        .\scripts\build-android.ps1 -Assemble -Release -Install
        .\scripts\build-android.ps1 -Diagnostics      # debug/trace logging

.PARAMETER Assemble
    Run Gradle after staging the library.

.PARAMETER Release
    With -Assemble, build a release APK instead of a debug one. Signing comes
    from VICTORY_KEYSTORE and friends; see android/app/build.gradle.kts.

.PARAMETER Install
    Install the built APK on the device adb sees.

.PARAMETER Diagnostics
    Build with the `diagnostics` feature, which lets VICTORY_LOG raise the
    Android host to debug and trace.

.PARAMETER Api
    Android API level to link against; must be at least the APK's minSdk.
#>
[CmdletBinding()]
param(
    [switch]$Assemble,
    [switch]$Release,
    [switch]$Install,
    [switch]$Diagnostics,
    [string]$Ndk,
    [string]$Sdk,
    [string]$Api = "33",
    [string]$JavaHome
)

$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'android\env.ps1') -Ndk $Ndk -Sdk $Sdk -Api $Api -JavaHome $JavaHome

$root = Split-Path $PSScriptRoot -Parent
$target = 'aarch64-linux-android'
$library = Join-Path $root "target\$target\release\libvictory_android.so"
$jniLibs = Join-Path $root 'android\app\src\main\jniLibs\arm64-v8a'

Push-Location $root
try {
    $cargo = @('build', '--package', 'victory-android', '--target', $target, '--release')
    if ($Diagnostics) { $cargo += @('--features', 'diagnostics') }
    Write-Host "cargo $($cargo -join ' ')"
    & cargo @cargo
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
    if (-not (Test-Path $library)) { throw "cargo reported success but $library is missing" }

    New-Item -ItemType Directory -Force -Path $jniLibs | Out-Null
    $staged = Join-Path $jniLibs 'libvictory_android.so'
    Copy-Item -Path $library -Destination $staged -Force

    # The packaged copy is stripped; the one in `target` keeps its symbols for
    # a debugger. Without this the APK carries 28 MB of line tables, which is
    # what Gradle says ("unable to strip") when it cannot find the NDK's
    # stripper itself.
    $strip = Join-Path $env:VICTORY_NDK_BIN 'llvm-strip.exe'
    & $strip --strip-unneeded $staged
    if ($LASTEXITCODE -ne 0) { throw "llvm-strip failed with exit code $LASTEXITCODE" }

    # 16 KB pages are not optional on Android 15 and the Play Store now checks
    # them; catching it here beats a device that loads nothing.
    $readelf = Join-Path $env:VICTORY_NDK_BIN 'llvm-readelf.exe'
    $misaligned = & $readelf -lW $staged | Select-String '^\s*LOAD' | Where-Object { $_ -notmatch '0x4000' }
    if ($misaligned) {
        throw "libvictory_android.so is not 16 KB page aligned:`n$($misaligned -join "`n")"
    }
    $nm = Join-Path $env:VICTORY_NDK_BIN 'llvm-nm.exe'
    if (-not ((& $nm -D --defined-only $staged) -match '\bandroid_main\b')) {
        throw "android_main is not exported from the stripped library"
    }
    $size = (Get-Item $staged).Length / 1MB
    Write-Host "alignment: 16 KB (OK), android_main exported"
    Write-Host ("staged: {0} ({1:N1} MB)" -f $staged, $size)

    if ($Assemble) {
        $sdkDir = $env:ANDROID_HOME
        if ($sdkDir) {
            $local = Join-Path $root 'android\local.properties'
            Set-Content -Path $local -Value "sdk.dir=$($sdkDir -replace '\\', '/')"
        }
        $task = if ($Release) { 'assembleRelease' } else { 'assembleDebug' }
        $apk = if ($Release) {
            'android\app\build\outputs\apk\release\app-release.apk'
        } else {
            'android\app\build\outputs\apk\debug\app-debug.apk'
        }
        # A packaging step that rewrites the APK in place leaves the old tail
        # behind when the library shrank; deleting first avoids an APK that is
        # 30 MB of stale bytes with a 2 MB zip in front of it.
        Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $root $apk)
        Push-Location (Join-Path $root 'android')
        try {
            & (Join-Path $PWD 'gradlew.bat') --no-daemon $task
            if ($LASTEXITCODE -ne 0) { throw "gradle $task failed with exit code $LASTEXITCODE" }
        } finally {
            Pop-Location
        }
        Write-Host "apk   : $(Join-Path $root $apk)"

        if ($Install) {
            $adb = Join-Path $sdkDir 'platform-tools\adb.exe'
            if (-not (Test-Path $adb)) { $adb = 'adb' }
            & $adb install -r (Join-Path $root $apk)
            if ($LASTEXITCODE -ne 0) { throw "adb install failed with exit code $LASTEXITCODE" }
        }
    }
} finally {
    Pop-Location
}
