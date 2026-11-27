<#
.SYNOPSIS
    Builds a review copy of this repository for one named reviewer.

.DESCRIPTION
    Makes a zip that a reviewer can actually use and that a leak can be traced
    back through:

      dist\victory-android-review-<reviewer>-<sha>\      the bundle
        source\                                          `git archive` of HEAD:
                                                         tracked files only, no
                                                         .git, no history
        REVIEW-COPY.md                                   who it was made for,
                                                         when, and from which
                                                         commit
        SHA256SUMS                                       every file, hashed
        victory-android-<sha>.apk                        if -Apk was given

    The bundle identifies its recipient by name and by a hash of
    (reviewer, commit), so two reviewers holding the same commit still hold
    distinguishable copies. That does not stop a copy being passed on — GPL-3
    says it may be — but it means a leaked bundle says whose it was.

    Nothing here weakens the licence: the archive keeps LICENSE and
    CREDITS.md, and REVIEW-COPY.md says so in plain words.

.PARAMETER Reviewer
    Handle of the person this copy is for. Goes into the bundle name and into
    REVIEW-COPY.md.

.PARAMETER Apk
    Optional path to an APK to include, so a reviewer can run it without
    building. `scripts\build-android.ps1 -Assemble` writes one at
    `android\app\build\outputs\apk\debug\app-debug.apk`.

.PARAMETER Out
    Where to write the zip. Defaults to `dist\` beside the repository root,
    which is ignored by git.

.EXAMPLE
    .\scripts\review-bundle.ps1 -Reviewer ada -Apk android\app\build\outputs\apk\debug\app-debug.apk
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Reviewer,
    [string]$Apk,
    [string]$Out
)

$ErrorActionPreference = 'Stop'

$root = (& git rev-parse --show-toplevel).Trim()
if (-not $root) { throw 'not inside a git checkout' }
$sha = (& git -C $root rev-parse --short=12 HEAD).Trim()
$committed = (& git -C $root log -1 --format=%cI HEAD).Trim()
$dirty = (& git -C $root status --porcelain).Trim()
if ($dirty) {
    Write-Warning "the working tree has uncommitted changes; the bundle is built from $sha, not from them"
}

$safe = ($Reviewer -replace '[^A-Za-z0-9_.-]', '-')
if (-not $Out) { $Out = Join-Path $root 'dist' }
New-Item -ItemType Directory -Force -Path $Out | Out-Null

# Two reviewers on the same commit get different bundles, on purpose.
$mark = [System.BitConverter]::ToString(
    [System.Security.Cryptography.SHA256]::HashData(
        [System.Text.Encoding]::UTF8.GetBytes("$safe@$sha"))
).Replace('-', '').Substring(0, 12).ToLowerInvariant()

$name = "victory-android-review-$safe-$sha"
$work = Join-Path $Out $name
if (Test-Path $work) { Remove-Item -Recurse -Force $work }
New-Item -ItemType Directory -Force -Path $work | Out-Null

$archive = Join-Path $work 'source.zip'
& git -C $root archive --format=zip -o $archive HEAD
Expand-Archive -Path $archive -DestinationPath (Join-Path $work 'source')
Remove-Item $archive

if ($Apk) {
    if (-not (Test-Path $Apk)) { throw "no APK at $Apk" }
    Copy-Item $Apk (Join-Path $work "victory-android-$sha.apk")
}

$notice = @"
# Review copy

Prepared for: $safe
Commit: $sha ($committed)
Copy mark: $mark
Prepared: $([DateTime]::UtcNow.ToString('u'))

This is a review copy of VIctory on Android, prepared for one named reviewer.
The code is GPL-3.0 (see ``source/LICENSE``) and keeps its ``CREDITS.md``: if
you pass this on, the licence and credits travel with it.

The copy mark above is part of the bundle's name and of this file, so a copy
found in the wild names the reviewer it was made for.

Feedback worth having, in order of usefulness:

1. The Vulkan backend's shape: ``crates/victory-gpu/src/vulkan/`` — pipeline
   state from a PSO description, descriptor layouts reflected from SPIR-V,
   image uploads and barriers. Is anything there wrong for a driver that is
   not Adreno?
2. The host's frame: ``apps/victory-android/src/render.rs`` — two passes, one
   offscreen with four attachments, one on the swap chain. Anything unsafe in
   the ordering or the layouts?
3. The translator seam: ``docs/android.md`` describes where FEXCore would plug
   in. Is that boundary drawn in the right place?
4. Anything that would stop you building or running it (``README.md`` has the
   three commands).
"@
Set-Content -Path (Join-Path $work 'REVIEW-COPY.md') -Value $notice -Encoding utf8

# Every file, hashed, so a modified copy is visible.
$lines = Get-ChildItem -Path $work -Recurse -File |
    Where-Object { $_.Name -ne 'SHA256SUMS' } |
    Sort-Object FullName |
    ForEach-Object {
        $relative = $_.FullName.Substring($work.Length + 1).Replace('\', '/')
        $hash = (Get-FileHash -Algorithm SHA256 -Path $_.FullName).Hash.ToLowerInvariant()
        "$hash  $relative"
    }
Set-Content -Path (Join-Path $work 'SHA256SUMS') -Value $lines -Encoding utf8

$zip = Join-Path $Out "$name.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path (Join-Path $work '*') -DestinationPath $zip -CompressionLevel Optimal
Remove-Item -Recurse -Force $work

$bytes = (Get-Item $zip).Length
$hash = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash.ToLowerInvariant()
Write-Host "bundle : $zip"
Write-Host "size   : $([math]::Round($bytes / 1MB, 2)) MB"
Write-Host "sha256 : $hash"
Write-Host "mark   : $mark  (for $safe at $sha)"
Write-Host ''
Write-Host 'Send the zip, not the checkout. Keep the sha256 above: it is what proves'
Write-Host 'which bundle a leaked copy came from.'
