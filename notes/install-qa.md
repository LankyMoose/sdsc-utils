# Install QA console

Local, loopback-only web console for exercising the standalone install and
update flows without touching the real user data.

```bash
python scripts/install_qa_web.py --open          # http://127.0.0.1:8765/
```

Everything lives under `target/install-qa/` (gitignored): staged portable
zips, the CEF DLC zip, installed copies, and private `APPDATA` dirs.

## What it covers

- **Version & manifest.** The web page has one version field, one release URL,
  and an editable manifest JSON (tag, URL, notes, assets). Saving updates the
  mock `/repos/.../releases/latest` payload immediately, so Settings → System →
  Version can show “update available” or “up to date”. (The binary itself is
  compiled at the Cargo version; the field is the manifest/install version.)
- **Release QA build.** `Build release (qa-feed)` runs
  `cargo build --release --features qa-feed`. That feature is the only way a
  release build may honor the feed overrides; normal release/Store builds never
  do. Debug builds honor them unconditionally.
- **One launch flow.** Choose `Direct app` (fast; no install) or `Installed
  portable`; debug/release; full/loader CEF. `Launch` prepares the private
  install (first time only unless **Rebuild portable** is checked) and starts
  the app. `Relaunch`/`Stop` reuse the last launch. The portable lives in
  `target/install-qa/installs/<tag>` with a private `APPDATA`.
- **Engine DLC flow.** `Build engine DLC zip` packs the CEF runtime and
  SHA-256. Launch env sets `SDSC_CEF_DLC_URL`/`SDSC_CEF_DLC_SHA256`;
  `SDSC_CEF_NO_BUILD_FALLBACK=1` disables the build-time `OUT_DIR` fallback in
  debug/qa-feed builds so the hub catalog must install the engine.
  - `full` CEF mode stages a runnable portable (engine sidecar present).
  - `loader-only` puts only the DLLs libcef needs to load next to the exe and
    omits resources/locales, so the resolver reports “not installed” and the
    catalog install/uninstall path is testable.
  - `none` copies no CEF; the app will not start (libcef is a load-time
    dependency), but it is useful to confirm the failure mode.
- **Launch controls.** The last launch is tracked; `APPDATA` is rewritten to
  the QA dir and the mock feed + engine env appended. A tracked PID is stopped
  with `taskkill /T` (CEF subprocesses included).

## Environment overrides

| Variable | Read by |
|---|---|
| `SDSC_UPDATE_API_URL` | updater in debug or `qa-feed` builds |
| `SDSC_CEF_DLC_URL` / `SDSC_CEF_DLC_SHA256` | engine install (all builds) |
| `SDSC_CEF_NO_BUILD_FALLBACK` | engine resolver in debug/qa-feed builds |
| `APPDATA` | app data dir (`%APPDATA%\sdsc-utils`) |
| `SDSC_DEBUG_OPEN` | debug builds: open `hub`/`browser`/etc. after boot |

## Build prerequisites

CEF builds need CMake + Ninja on `PATH`. The console prepends the winget Links
dir on Windows when present. The first CEF build downloads ~427 MB into the
target `OUT_DIR`; later builds reuse it. The console does **not** set
`CEF_PATH` for cargo (the crate expects its archive layout, not the extracted
runtime dir); `CEF_PATH` is only used by the console to find the runtime for
the engine zip.

## Typical flow

1. Set the version/manifest (default `v1.7.4`); the binary is 1.7.3.
2. Fastest: Target **Direct app**, Debug, **Launch**. The hub opens with a
   private appdata, the mock feed, and the engine DLC env (engine installable).
3. Install layout: Target **Installed portable**, Debug, CEF **loader-only**,
   **Launch**. The first launch stages/installs; later launches reuse it.
4. Add apps → Browser → X/Cross → **Install**. The catalog downloads the local
   CEF zip, verifies the SHA-256, and installs under the private `APPDATA`.
5. **Relaunch** so the engine initializes at boot. The feed advertises the
   configured version; Settings → System → Version shows it.
6. Uninstall the engine from the apps menu, or `Clear` the QA install/appdata.

`Build release qa-feed` remains available for testing a release binary; a
normal release ignores the mock feed URL by design.

The updater is check-only by design (README: it never downloads or applies
app updates). This console tests the app-install layout, launch, engine DLC,
and update-check plumbing; it does not add an app self-updater.
