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
  Install and uninstall both show the active phase and an in-tile meter
  (`Downloading`, `Verifying`, `Installing`, `Uninstalling`); there is no
  separate progress panel.
- **Reset.** **Rebuild install** recreates the install dir and wipes its
  private appdata, including any engine downloaded during the last
  walk-through, so the clean flow can be repeated.
- **Launch controls.** `APPDATA` is rewritten to the QA dir and the mock feed
  + engine env appended. A tracked PID is stopped with `taskkill /T` (CEF
  subprocesses included). **Allow controller input** is on by default for
  manual testing; uncheck it for automation.
- **Display target.** Manual launches (`Automated` unchecked) use the primary
  monitor with normal focus and `SDSC_DEBUG_QUIET=0`. Automated launches
  (checkbox on, or `controller_input: false` via the API) set
  `SDSC_DEBUG_QUIET=1` for quiet secondary-monitor placement with no focus
  steal. `SDSC_DEBUG_INPUT=0` drops pad edges in either mode.

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

## Local engine store

Local DLC installs land in `%APPDATA%\sdsc-utils\cef-engine-<stamp>` and
`cef-engine.current` points at the active copy. A running CEF keeps
`libcef.dll` mapped, so uninstall drops the pointer immediately and schedules
the directory for cleanup on the next boot. Reinstall works in the same
session because it writes a new versioned directory; the still-loaded old
runtime is left intact until exit. The next boot deletes the pending old
copies and loads the pointer target.

Each install writes `engine.json` (version, CEF/Chromium source versions,
changelog, measured size). The app checks `/engine-manifest` on hub open and
compares that version with the installed copy; a newer feed shows an Update
badge and the Update action. Updates use the same versioned store, so the
running CEF stays loaded until restart (`Restart to finish` in Info).

## Update, cancel, and settings

- **Publish update** in the DLC card bumps the engine version and saves the
  changelog. The app sees it on the next hub open (relaunch, or close/reopen
  the hub) and offers **Update**.
- **Cancel** during Downloading/Verifying/Installing: Circle/Esc or the Cancel
  action row. The worker deletes staging, restores the previous install, and
  leaves the pointer untouched.
- **Download speed** lives in PS hub → Settings (`Unlimited / 2 / 5 / 10 MB/s`)
  and is shared with a live download. It is hidden in packaged builds because
  the Store owns downloads. The tile shows the live `current / target` size
  and MB/s; there is no “capped” label.
- **Mock MSIX engine package** stages `cef\` beside the exe and sets
  `SDSC_MSIX_ENGINE_DIR`, so the resolver treats it as the optional package:
  Browser reports **Managed by Store**, Uninstall is hidden, and Info shows the
  version/size without a local uninstall path.

## Environment overrides

| Variable | Read by |
|---|---|
| `SDSC_UPDATE_API_URL` | updater in debug or `qa-feed` builds |
| `SDSC_CEF_DLC_URL` / `SDSC_CEF_DLC_SHA256` | env-only engine feed (legacy fallback) |
| `SDSC_CEF_DLC_MANIFEST_URL` | version/changelog/download engine manifest |
| `SDSC_MSIX_ENGINE_DIR` | resolver mock for the optional BrowserEngine package |
| `SDSC_CEF_NO_BUILD_FALLBACK` | engine resolver in debug/qa-feed builds |
| `APPDATA` | app data dir (`%APPDATA%\sdsc-utils`) |
| `SDSC_DEBUG_OPEN` | debug builds: open `hub`/`browser`/etc. after boot |
| `SDSC_DEBUG_QUIET` | `0` makes debug-open windows focusable on the primary |
| `SDSC_DEBUG_INPUT` | `1` keeps pad edges live; `0` drops them (even outside quiet mode) |

`SDSC_DEBUG_OPEN` normally enables **debug quiet mode** (no focus, secondary
monitor, pad edges dropped) for screenshotting over a running game. The QA
console sets `SDSC_DEBUG_QUIET=0` for manual launches (primary monitor, normal
focus) and `=1` for automated launches (secondary monitor, no focus steal).
Manual launches default to `SDSC_DEBUG_INPUT=1` so controller navigation works.
Automated launches must pass `"controller_input": false` so
`SDSC_DEBUG_INPUT=0`; the physical controller must never navigate or close the
QA app while a test is running.

## Automation

Call `/api/prepare` with `"controller_input": false` (and leave **Allow
controller input** unchecked in the page). This also defaults
`"automated": true`, so the app opens on the secondary monitor with no focus
steal and pad edges are dropped. Never automate against the primary monitor or
with controller input enabled. Manual runs leave **Automated** unchecked and
open focused on the primary.

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
4. **Apps → Browser → Info** shows the engine version, `Size on disk`, pending
   cleanup, and the changelog (no app version line).
5. Set a new engine version, press **Publish update**, and reopen the hub to
   test Update; cancel mid-download with Circle/Esc.
6. **Relaunch** to exercise a process that finds the downloaded engine at boot.
7. To test the already-installed layout, check **Bundle browser engine** and
   **Rebuild install**, then **Build & launch**; the Browser tile is ready
   immediately. Check **Mock MSIX engine package** for the managed-package path.
8. Uninstall the engine from the apps menu, or **Rebuild install** / the
   `/api/clear` endpoint to reset the QA data.

The updater is check-only by design (README: it never downloads or applies
app updates). This console tests the app-install layout, launch, engine DLC,
and update-check plumbing; it does not add an app self-updater.
