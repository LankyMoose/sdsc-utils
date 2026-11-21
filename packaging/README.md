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

The MSIX layout ships the single `sdsc-utils.exe` (tray + HID + UI in one process). The GitHub portable download is the same exe copied to `target/portable/`.

## Planned: browser engine DLC (not implemented)

The hub/CEF pivot splits the browser out of Start and moves the engine into an
optional `BrowserEngine` package in a **related set** (mandatory for executable
code), so the main MSIX stays lean. Store users install it through
`Windows.Services.Store`; sideload uses AppInstaller `<OptionalPackages>`;
dev/unpackaged uses a `LocalAppData` download+hash-verify fallback. See
[`notes/browser-hub-pivot.md`](../notes/browser-hub-pivot.md) before changing
`AppxManifest.xml`/`pack-msix.ps1` for multi-package output. The current
single-package layout below remains accurate until that work lands.

## Store submission

See [STORE.md](STORE.md) for the Partner Center checklist (account, listing copy, first upload).

## Autostart

`desktop:StartupTask` TaskId `SdscUtilsStartup` must stay in sync with [`src/platform/autostart.rs`](../src/platform/autostart.rs). Manifest default is `Enabled="false"` (user opts in via Settings).
