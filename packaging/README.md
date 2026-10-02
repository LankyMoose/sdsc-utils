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

The MSIX layout keeps `sdsc-utils.exe` and `sdsc-shell.exe` side by side. The GitHub portable download is a separate file: `bundle-portable` appends the shell onto a copy of the service exe, and the service unpacks it on first launch when no sibling shell is present.

## Store submission

See [STORE.md](STORE.md) for the Partner Center checklist (account, listing copy, first upload).

## Autostart

`desktop:StartupTask` TaskId `SdscUtilsStartup` must stay in sync with [`src/autostart.rs`](../src/autostart.rs). Manifest default is `Enabled="false"` (user opts in via Settings).
