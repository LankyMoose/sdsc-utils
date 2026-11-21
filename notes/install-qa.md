# Install QA console

Local, loopback-only web console for exercising the standalone install and
update flows without touching the real user data.

```bash
python scripts/install_qa_web.py --open          # http://127.0.0.1:8765/
```

Everything lives under `target/install-qa/` (gitignored): staged portable
zips, the CEF DLC zip, installed copies, and private `APPDATA` dirs.

## What it covers

- **Mock GitHub release feed.** `/repos/LankyMoose/sdsc-utils/releases/latest`
  returns the highest enabled version (or the selected `latest`). The web page
  adds/removes/toggles versions and chooses the latest, so Settings → System →
  Version can show “update available” or “up to date”.
- **Release QA build.** `Build release (qa-feed)` runs
  `cargo build --release --features qa-feed`. That feature is the only way a
  release build may honor the feed overrides; normal release/Store builds never
  do. Debug builds honor them unconditionally.
- **Portable installs.** `Stage portable` copies the release exe and zips it.
  `Install staged portable` extracts it into `target/install-qa/installs/<tag>`
  with a private `APPDATA`; `Launch` runs it; `Uninstall` removes both.
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
- **Launch controls.** Debug, release QA, or any installed portable, with
  `APPDATA` rewritten to the QA dir and the mock feed + engine env appended.
  A tracked PID is stopped with `taskkill /T` (CEF subprocesses included).

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

1. Build release QA, stage **loader-only**, install.
2. Launch — the app starts, the Browser engine is missing.
3. Open the hub (controller PS; or run the debug flavor with `SDSC_DEBUG_OPEN=hub`).
4. Add apps → Browser → X/Cross → **Install**. The catalog downloads the local
   CEF zip, verifies the SHA-256, and installs under the private `APPDATA`.
5. Relaunch: the engine resolves from the data dir. The release feed advertises
   the selected latest version; Settings → System → Version shows it.
6. Uninstall the engine from the apps menu, or clear the QA install/appdata.

The updater is check-only by design (README: it never downloads or applies
app updates). This console tests the app-install layout, launch, engine DLC,
and update-check plumbing; it does not add an app self-updater.
