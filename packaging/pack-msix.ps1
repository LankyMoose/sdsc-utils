# Pack a sideload / Store-submission MSIX for SDSC Utils.
#
# Usage:
#   ./packaging/pack-msix.ps1 -ExePath target/release/sdsc-utils.exe -OutDir target/msix
#
#   # phase 5: also build the optional BrowserEngine package
#   ./packaging/pack-msix.ps1 -ExePath target/release/sdsc-utils.exe `
#       -CefDir target/release/build/cef-dll-sys-*/out/cef_windows_x86_64
#
# Ships `sdsc-utils.exe` (tray + HID + UI in one process). The optional
# BrowserEngine MSIX carries the CEF sidecars under `cef\` so the main package
# stays lean; Store/AppInstaller must install both as a related set.
#
# Identity Version is stamped from Cargo.toml (e.g. 1.3.1 -> 1.3.1.0).
# Identity Name / Publisher in AppxManifest.xml must match Partner Center.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $ExePath,

    [Parameter(Mandatory = $false)]
    [string] $OutDir = "target/msix",

    [Parameter(Mandatory = $false)]
    [string] $RepoRoot = "",

    # Optional: CEF runtime directory (contains libcef.dll/resources/locales).
    # When set, also packs BrowserEngine.msix next to the main package.
    [Parameter(Mandatory = $false)]
    [string] $CefDir = ""
)

$ErrorActionPreference = "Stop"

if (-not $RepoRoot) {
    if ($PSScriptRoot) {
        $RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
    } else {
        $RepoRoot = (Resolve-Path (Join-Path (Split-Path -Parent $MyInvocation.MyCommand.Path) "..")).Path
    }
}

function Get-CargoVersion {
    $toml = Get-Content (Join-Path $RepoRoot "Cargo.toml") -Raw
    if ($toml -match '(?m)^version\s*=\s*"([^"]+)"') {
        return $Matches[1]
    }
    throw "Could not read version from Cargo.toml"
}

function Get-MakeAppx {
    $cmd = Get-Command makeappx.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }

    $roots = @(
        "${env:ProgramFiles(x86)}\Windows Kits\10\bin",
        "${env:ProgramFiles}\Windows Kits\10\bin"
    )
    foreach ($root in $roots) {
        if (-not (Test-Path $root)) { continue }
        $found = Get-ChildItem -Path $root -Recurse -Filter makeappx.exe -ErrorAction SilentlyContinue |
            Sort-Object FullName -Descending |
            Select-Object -First 1
        if ($found) { return $found.FullName }
    }
    throw "makeappx.exe not found. Install the Windows 10/11 SDK."
}

$exe = Resolve-Path $ExePath
$version = Get-CargoVersion
$parts = $version.Split('.')
while ($parts.Count -lt 4) { $parts += '0' }
$msixVersion = ($parts[0..3] -join '.')

Write-Host "Packing SDSC Utils MSIX version $msixVersion"

Push-Location $RepoRoot
try {
    Write-Host "Generating Store logos..."
    cargo run --quiet --bin gen_msix_logos -- packaging/Assets
    if ($LASTEXITCODE -ne 0) { throw "gen-msix-logos failed" }
}
finally {
    Pop-Location
}

$layout = Join-Path $OutDir "layout"
if (Test-Path $layout) { Remove-Item $layout -Recurse -Force }
New-Item -ItemType Directory -Path $layout | Out-Null
New-Item -ItemType Directory -Path (Join-Path $layout "Assets") | Out-Null

Copy-Item $exe (Join-Path $layout "sdsc-utils.exe")
Copy-Item (Join-Path $RepoRoot "packaging\Assets\*") (Join-Path $layout "Assets\") -Force

$manifestSrc = Join-Path $RepoRoot "packaging\AppxManifest.xml"
$manifestDst = Join-Path $layout "AppxManifest.xml"
$manifest = Get-Content $manifestSrc -Raw
$manifest = $manifest -replace 'Version="0\.0\.0\.0"', "Version=`"$msixVersion`""
Set-Content -Path $manifestDst -Value $manifest -Encoding UTF8

New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
$msixPath = Join-Path $OutDir "sdsc-utils.msix"
if (Test-Path $msixPath) { Remove-Item $msixPath -Force }

$makeappx = Get-MakeAppx
Write-Host "Using $makeappx"
& $makeappx pack /o /d $layout /p $msixPath
if ($LASTEXITCODE -ne 0) { throw "makeappx failed" }

Write-Host "Wrote $msixPath"

if ($CefDir) {
    $cef = (Resolve-Path $CefDir).Path
    if (-not (Test-Path (Join-Path $cef "libcef.dll"))) {
        throw "CefDir does not look like a CEF runtime (missing libcef.dll): $cef"
    }

    Write-Host "Packing optional BrowserEngine MSIX from $cef"
    $engineLayout = Join-Path $OutDir "engine-layout"
    if (Test-Path $engineLayout) { Remove-Item $engineLayout -Recurse -Force }
    New-Item -ItemType Directory -Path $engineLayout | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $engineLayout "Assets") | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $engineLayout "cef") | Out-Null

    # Runtime-only payload: CEF's headers, CMake files, import lib, and
    # wrapper sources are build artifacts, not install content.
    $cefFiles = @(
        "libcef.dll", "chrome_elf.dll", "d3dcompiler_47.dll", "dxcompiler.dll",
        "vk_swiftshader.dll", "vk_swiftshader_icd.json", "vulkan-1.dll",
        "icudtl.dat", "resources.pak", "chrome_100_percent.pak",
        "chrome_200_percent.pak", "v8_context_snapshot.bin", "CREDITS.html"
    )
    foreach ($name in $cefFiles) {
        $src = Join-Path $cef $name
        if (Test-Path $src) {
            Copy-Item $src (Join-Path $engineLayout "cef\$name") -Force
        } else {
            Write-Warning "CEF runtime file missing: $name"
        }
    }
    foreach ($dirName in @("locales", "swiftshader")) {
        $src = Join-Path $cef $dirName
        if (Test-Path $src) {
            Copy-Item $src (Join-Path $engineLayout "cef\$dirName") -Recurse -Force
        }
    }
    Copy-Item (Join-Path $RepoRoot "packaging\Assets\*") (Join-Path $engineLayout "Assets\") -Force

    $engineManifestSrc = Join-Path $RepoRoot "packaging\BrowserEngine.AppxManifest.xml"
    $engineManifestDst = Join-Path $engineLayout "AppxManifest.xml"
    $engineManifest = Get-Content $engineManifestSrc -Raw
    $engineManifest = $engineManifest -replace 'Version="0\.0\.0\.0"', "Version=`"$msixVersion`""
    Set-Content -Path $engineManifestDst -Value $engineManifest -Encoding UTF8

    $engineMsix = Join-Path $OutDir "BrowserEngine.msix"
    if (Test-Path $engineMsix) { Remove-Item $engineMsix -Force }
    & $makeappx pack /o /d $engineLayout /p $engineMsix
    if ($LASTEXITCODE -ne 0) { throw "makeappx failed for BrowserEngine" }
    Write-Host "Wrote $engineMsix"
}
