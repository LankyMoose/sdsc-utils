# Pack a sideload / Store-submission MSIX for SDSC Utils.
#
# Usage:
#   ./packaging/pack-msix.ps1 -ExePath target/release/sdsc-utils.exe -OutDir target/msix
#
# Copies sdsc-shell.exe from the same directory as -ExePath (required for the
# HID/tray service to spawn the iced shell).
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
    [string] $RepoRoot = ""
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
$shellSrc = Join-Path (Split-Path -Parent $exe) "sdsc-shell.exe"
if (-not (Test-Path $shellSrc)) {
    throw "sdsc-shell.exe not found next to $exe (build both bins with cargo build --release --bins)"
}
Copy-Item $shellSrc (Join-Path $layout "sdsc-shell.exe")
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
