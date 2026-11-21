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
- **Build & launch.** One button runs the selected Cargo build, stages a
  standalone install under `target/install-qa/installs/<tag>`, and starts it
  with a private `APPDATA`. **Debug** is fast; **Release (qa-feed)** builds
  with the feature that lets a release binary honor the mock feed. Preparing
  runs in a background worker, so the page keeps polling and shows
  `building` → `packaging engine` → `installing` → `launching`.
- **No CEF by default.** With **Bundle browser engine** unchecked, the staged
  install contains only `sdsc-utils.exe` (plus the exe's non-CEF runtime) and
  the private appdata has no `cef-engine`. The app binary delay-loads
  `libcef.dll`, so it starts cleanly and the hub shows Browser as not
  installed. **Apps → Browser → Install** downloads the engine zip from this
  server, verifies the SHA-256, extracts it under the private appdata, and
  attaches CEF in the same process — no relaunch required.
- **Installed state.** Check **Bundle browser engine** to stage a full `cef\`
  sidecar beside the exe. Use **Relaunch** to run the last install again; a
  previously downloaded engine in the private appdata survives relaunches.
- **Reset.** **Rebuild install** recreates the install dir and wipes its
  private appdata, including any engine downloaded during the last
  walk-through, so the clean flow can be repeated.
- **Launch controls.** `APPDATA` is rewritten to the QA dir and the mock feed
  + engine env appended. A tracked PID is stopped with `taskkill /T` (CEF
  subprocesses included).

## Why an engine-less exe starts

`libcef.dll` is a normal import of `sdsc-utils.exe`, but `build.rs` adds
`/DELAYLOAD:libcef.dll` for the app binary. The process only loads CEF when a
CEF entry point is first called. Every entry point resolves the runtime
directory and calls `SetDllDirectoryW` first (`browser::cef_backend`), so:

- no runtime → `execute_subprocess()` returns `None`, the app starts, the hub
  offers Install;
- runtime installed later → the next `spawn_engine` loads `libcef.dll` from
  the resolved directory and initializes CEF in-process.

This is why the QA console no longer needs a “loader-only” sidecar: a truly
empty install is the default test state.

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
the engine zip. A clean launch builds the DLC zip on first use; later launches
reuse it.

## Typical flow

1. Set the version/manifest (default `v1.7.4`); the binary is 1.7.3.
2. Leave **Bundle browser engine** unchecked; choose **Debug**; press
   **Build & launch**. The hub opens with no CEF sidecar and clean appdata.
3. **Apps → Browser → Install** (X/Cross). The catalog downloads the local CEF
   zip, verifies the SHA-256, installs under the private `APPDATA`, and starts
   the engine. The Browser tile becomes usable.
4. **Relaunch** to exercise a process that finds the downloaded engine at boot.
5. To test the already-installed layout, check **Bundle browser engine** and
   **Rebuild install**, then **Build & launch**; the Browser tile is ready
   immediately.
6. Uninstall the engine from the apps menu, or **Rebuild install** / the
   `/api/clear` endpoint to reset the QA data.

The updater is check-only by design (README: it never downloads or applies
app updates). This console tests the app-install layout, launch, engine DLC,
and update-check plumbing; it does not add an app self-updater.
