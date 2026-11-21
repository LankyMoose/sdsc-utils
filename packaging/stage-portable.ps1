# Stage a portable SDSC Utils folder: sdsc-utils.exe + cef\ sidecars.
#
# Usage:
#   ./packaging/stage-portable.ps1 -ExePath target/release/sdsc-utils.exe `
#       -CefDir target/release/build/cef-dll-sys-*/out/cef_windows_x86_64 `
#       -OutDir target/portable
#
# The Browser app resolves `cef\` next to the exe before any packaged path.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $ExePath,

    [Parameter(Mandatory = $true)]
    [string] $CefDir,

    [Parameter(Mandatory = $false)]
    [string] $OutDir = "target/portable"
)

$ErrorActionPreference = "Stop"

$exe = Resolve-Path $ExePath
$cef = (Resolve-Path $CefDir).Path
if (-not (Test-Path (Join-Path $cef "libcef.dll"))) {
    throw "CefDir does not look like a CEF runtime (missing libcef.dll): $cef"
}

$out = Join-Path (Get-Location) $OutDir
if (Test-Path $out) { Remove-Item $out -Recurse -Force }
New-Item -ItemType Directory -Path (Join-Path $out "cef") -Force | Out-Null

Copy-Item $exe (Join-Path $out "sdsc-utils.exe") -Force

$cefFiles = @(
    "libcef.dll", "chrome_elf.dll", "d3dcompiler_47.dll", "dxcompiler.dll",
    "vk_swiftshader.dll", "vk_swiftshader_icd.json", "vulkan-1.dll",
    "icudtl.dat", "resources.pak", "chrome_100_percent.pak",
    "chrome_200_percent.pak", "v8_context_snapshot.bin", "CREDITS.html"
)
foreach ($name in $cefFiles) {
    $src = Join-Path $cef $name
    if (Test-Path $src) {
        Copy-Item $src (Join-Path $out "cef\$name") -Force
    } else {
        Write-Warning "CEF runtime file missing: $name"
    }
}
foreach ($dirName in @("locales", "swiftshader")) {
    $src = Join-Path $cef $dirName
    if (Test-Path $src) {
        Copy-Item $src (Join-Path $out "cef\$dirName") -Recurse -Force
    }
}

Write-Host "Wrote $out"
