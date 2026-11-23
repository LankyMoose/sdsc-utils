# Packaging notes (SDSC Utils)

## Store identity

- [`AppxManifest.xml`](AppxManifest.xml) uses the Partner Center package identity:
  - **Name:** `LankyMoose.SDSCUtils`
  - **Publisher:** `CN=F2379117-7506-444F-AA08-EC697BF7DE9D`
  - **Family name** (derived): `LankyMoose.SDSCUtils_9wejh0h2znyz4`
- `pack-msix.ps1` stamps `Identity Version` from `Cargo.toml` (`1.3.1` → `1.3.1.0`) so each tagged upload is a newer package for Store auto-update.
- Microsoft re-signs the package on Store publish. Local sideload install requires a certificate whose subject matches `Publisher`.

## Local pack

```powershell
cargo build --release
./packaging/pack-msix.ps1 -ExePath target/release/sdsc-utils.exe -OutDir target/msix
```

Requires the Windows SDK (`makeappx.exe`). Logos are generated into `packaging/Assets/` at pack time (`cargo run --bin gen_msix_logos`).

The main MSIX layout ships `sdsc-utils.exe` (tray + HID + UI in one process)
only; it does not contain CEF. The portable download is the exe plus the CEF
sidecars in a `cef\` folder next to it (see the engine resolver below).

## Browser engine DLC (optional package)

The browser engine is an optional `BrowserEngine` MSIX in a **related set**
with the main package. Executable code forces the related set, so main and
engine are submitted/updated together in Partner Center; every Chromium
security patch needs both resubmitted.

```powershell
cargo build --release
# CEF lives in the cef-dll-sys build output after a normal build:
#   target/release/build/cef-dll-sys-*/out/cef_windows_x86_64
./packaging/pack-msix.ps1 -ExePath target/release/sdsc-utils.exe `
    -OutDir target/msix -CefDir <that directory>
```

This writes `target/msix/sdsc-utils.msix` and
`target/msix/BrowserEngine.msix`. The engine layout copies only runtime files
(`libcef.dll`, ANGLE/D3D/compiler DLLs, paks, `icudtl.dat`, locales) under
`cef\`, not CEF headers/CMake/import libs. [`BrowserEngine.AppxManifest.xml`](BrowserEngine.AppxManifest.xml)
uses the same publisher identity.

- **Store:** create the optional package in Partner Center, associate it with
  the main app, and submit both. The app currently opens the Store product
  page for install; wiring `Windows.Services.Store`
  (`RequestDownloadAndInstallStorePackagesAsync`) with the assigned Store IDs
  is the remaining in-app step.
- **Sideload:** an AppInstaller file must list both packages
  (`<MainPackage>` + `<OptionalPackages><Package .../></OptionalPackages>`);
  `Add-AppxPackage` cannot install an optional package as an unrelated app.
- **Dev/unpackaged:** set `SDSC_CEF_DLC_URL` + `SDSC_CEF_DLC_SHA256`; Home
  catalog Install button downloads the zip, verifies SHA-256, extracts to
  `%LOCALAPPDATA%\sdsc-utils\cef-engine`, and starts CEF without a restart.

Engine resolver order: `CEF_PATH` → `cef\` next to the exe → the
`BrowserEngine` package's `cef\` directory → `%LOCALAPPDATA%\sdsc-utils\cef-engine` → debug build `OUT_DIR`. A directory only counts as installed when
`libcef.dll`, `icudtl.dat`, `resources.pak`, and `locales/en-US.pak` exist.

## Store submission

See [STORE.md](STORE.md) for the Partner Center checklist (account, listing copy, first upload).

## Autostart

`desktop:StartupTask` TaskId `SdscUtilsStartup` must stay in sync with [`src/platform/autostart.rs`](../src/platform/autostart.rs). Manifest default is `Enabled="false"` (user opts in via Settings).
