# CEF API map

**Status:** living reference. Checked against `cef = 156.7.0` / CEF
`156.0.31` (Chromium 156). Update this note whenever the `cef` dependency or a
browser capability changes.

**When to read:** adding or changing anything that talks to the CEF engine:
navigation, history, tabs/popups, favicons, find, zoom, fullscreen, downloads,
dialogs, permissions, input, paint, crash recovery, or browser settings.

This is a curated index, not a replacement for the CEF headers. The C++ headers
are the source of truth; the Rust wrapper is generated from them.

## Fast lookup sources

| Source | Where |
|---|---|
| Exact build headers | `target/debug/build/cef-dll-sys-*/out/cef_windows_x86_64/include/` (or `target/release/...`) |
| Full CEF distribution | `$CEF_PATH/cef_windows_x86_64/include/` |
| Generated Rust bindings | `%USERPROFILE%\.cargo\registry\src\index.crates.io-*\cef-156.7.0+156.0.31\src\bindings\x86_64_pc_windows_msvc.rs` |
| Rust wrapper docs | <https://docs.rs/cef/156.7.0/cef/> |
| `BrowserHost` wrapper | <https://docs.rs/cef/156.7.0/cef/struct.BrowserHost.html> |
| CEF class index (156.0) | <https://cef-builds.spotifycdn.com/docs/156.0/classes.html> |
| CEF class list (156.0) | <https://cef-builds.spotifycdn.com/docs/156.0/annotated.html> |
| CEF file list (156.0) | <https://cef-builds.spotifycdn.com/docs/156.0/files.html> |
| CEF source + tests | <https://github.com/chromiumembedded/cef> (see `tests/cefclient`) |

Useful commands:

```bash
# Locate the exact headers used by the current build.
find target/debug/build -path '*cef_windows_x86_64/include/cef_browser.h'

# Find a wrapper method/trait in the generated Rust bindings.
rg 'fn navigation_entries|struct NavigationEntryVisitor' \
  "$HOME/.cargo/registry/src" -g 'x86_64_pc_windows_msvc.rs'

# List the handlers this backend actually implements.
rg 'wrap_.*handler' src/browser/cef_backend.rs
```

The packaged engine DLC contains runtime files only. Headers come from the
build tree or a full CEF binary distribution.

## C++ → Rust naming

The Rust wrapper keeps CEF's object model but renames accessors:

| CEF C++ | Rust wrapper |
|---|---|
| `CefBrowserHost::GetNavigationEntries` | `BrowserHost::navigation_entries` |
| `CefBrowserHost::GetVisibleNavigationEntry` | `BrowserHost::visible_navigation_entry` |
| `CefDisplayHandler::OnAddressChange` | `DisplayHandler::on_address_change` |
| `CefLoadHandler::OnLoadingStateChange` | `LoadHandler::on_loading_state_change` |
| `CefLifeSpanHandler::OnBeforePopup` | `LifeSpanHandler::on_before_popup` |
| `CefImage::GetAsBitmap` | `Image::as_bitmap` |

Handlers are implemented with `cef::wrap_*_handler!` and returned from
`SdscClient` (`src/browser/cef_backend.rs`). Enum constants keep CEF names in
CamelCase, e.g. `ColorType::RGBA_8888`.

## Capability matrix

Status legend:

- **wired** — used by the app
- **wrapper only** — available, not called
- **unhandled** — no handler; CEF's default behavior applies
- **custom** — deliberately app-level; CEF has no equivalent or it is a poor fit

| Capability | CEF API | Handler | Status | Our code |
|---|---|---|---|---|
| Session history | `BrowserHost::GetNavigationEntries`, `GetVisibleNavigationEntry`, `NavigationEntry`, `NavigationEntryVisitor` | visitor callback | **wired** (snapshot is authoritative; UI projects it) | `src/browser/cef_backend.rs`, `src/browser/mod.rs` |
| Back/forward | `Browser::CanGoBack/GoBack/CanGoForward/GoForward`; `LoadHandler::OnLoadingStateChange` flags | load | **wired** (CEF flags gate; one step per snapshot) | `src/app/browser.rs`, `src/app/launcher.rs` |
| Reload / stop | `Browser::Reload`, `ReloadIgnoreCache`, `StopLoad` | none | **reload wired**; stop wrapper only | `src/app/launcher.rs:on_browser_reload` |
| URL / title / favicon | `DisplayHandler::OnAddressChange/OnTitleChange/OnFaviconURLChange` | display | **wired** | `src/browser/cef_backend.rs` |
| Page favicon image | `BrowserHost::DownloadImage`, `DownloadImageCallback`, `Image::GetAsBitmap/GetAsPNG` | callback | **wired** (CEF primary; local decoder fallback) | `src/browser/cef_backend.rs`, `src/browser/favicon.rs` |
| Tabs | no CEF tab strip; one `Browser` per tab | — | **custom** | `src/ui/browser/state.rs`, `src/browser/engine.rs` |
| Popups / new windows | `LifeSpanHandler::OnBeforePopup`, `RequestHandler::OnOpenURLFromTab` | life-span/request | **unhandled** (all popups denied) | `src/browser/cef_backend.rs:on_before_popup` |
| Find in page | `BrowserHost::Find`, `StopFinding`; `FindHandler::OnFindResult` | find | **wrapper only** | — |
| Zoom | `BrowserHost::CanZoom`, `Zoom`, `GetZoomLevel`, `SetZoomLevel` | none | **wrapper only** | — |
| Fullscreen | `DisplayHandler::OnFullscreenModeChange`; `BrowserHost::IsFullscreen`, `ExitFullscreen` | display | **unhandled** | — |
| Downloads | `DownloadHandler::OnBeforeDownload/OnDownloadUpdated`; `BrowserHost::StartDownload` | download | **unhandled** | — |
| File picker | `DialogHandler::OnFileDialog`; `BrowserHost::RunFileDialog` | dialog | **unhandled** | — |
| JS dialogs | `JSDialogHandler::OnJSDialog/OnBeforeUnloadDialog` | JS dialog | **unhandled** | — |
| Permissions | `PermissionHandler::OnRequestPermission` | permission | **unhandled** | — |
| Auth / certs | `RequestHandler::GetAuthCredentials`, `OnCertificateError`, `OnSelectClientCertificate` | request | **unhandled** | — |
| Renderer crash | `RequestHandler::OnRenderProcessTerminated`, `OnRenderProcessUnresponsive` | request | **unhandled** | `src/platform/crash_restart.rs` is app-process only |
| Context menu | `ContextMenuHandler` | context menu | **unhandled** | — |
| Audio mute | `BrowserHost::SetAudioMuted`, `IsAudioMuted` | none | **wrapper only** | — |
| Print / PDF | `BrowserHost::Print`, `PrintToPDF`; `PrintHandler` | print | **wrapper only** | — |
| DevTools | `BrowserHost::ShowDevTools`, `ExecuteDevToolsMethod`, `DevToolsMessageObserver` | none | **wrapper only** | — |
| Accessibility | `AccessibilityHandler::OnAccessibilityTreeChange`; `BrowserHost::SetAccessibilityState` | accessibility | **custom** (DOM query) | `src/browser/dom.rs` |
| Keyboard / mouse / wheel | `BrowserHost::SendKeyEvent`, `SendMouseClickEvent`, `SendMouseMoveEvent`, `SendMouseWheelEvent` | none | **partial**: keyboard text (`RAWKEYDOWN`+`CHAR`+`KEYUP`), pad trusted clicks, hardware left click and wheel; no drag/right/middle | `src/browser/cef_backend.rs` |
| IME | `BrowserHost::ImeSetComposition`, `ImeCommitText`, `ImeFinishComposingText`, `ImeCancelComposition` | none | **unhandled** | `notes/browser-osk.md` |
| OSR paint | `RenderHandler::OnPaint`; `OnAcceleratedPaint` | render | **wired** (CPU L1; accelerated not wired) | `src/browser/cef_backend.rs` |
| Cookies / cache / profile | `CookieManager`, `RequestContext`, `Settings.root_cache_path` | settings | **wired** (CEF defaults) | `src/browser/cef_backend.rs:initialize_cef` |
| Preferences | `PreferenceManager`, `PreferenceObserver`, `PreferenceRegistrar` | preferences | **wrapper only** | — |
| Sandbox / subprocess | `CefExecuteProcess`, `Settings.no_sandbox` | app | **custom** (sandbox off) | `src/bin/sdsc-utils.rs`, `src/browser/cef_backend.rs` |

## History deep dive

CEF exposes the per-browser session history directly. Use it; do not infer
history from address-change events.

Current wiring: `src/browser/cef_backend.rs` polls `GetNavigationEntries` and
emits `EngineEvent::HistoryChanged`; `BrowserSession::apply_engine_history`
projects it for the selector. `App::start_history_step` treats L1/R1 as one
CEF step and accepts the landing; `start_history_travel` /
`advance_history_travel` sequence selector jumps one snapshot at a time and
detect a skipped target. Address changes only update the live URL and
autocomplete MRU.

### Read

```text
BrowserHost::GetNavigationEntries(visitor, current_only = false)
```

The visitor runs on CEF's browser-process UI thread and receives:

- `entry` — `CefNavigationEntry`
- `current` — true for the current entry
- `index` — 0-based position
- `total` — number of entries

Copy strings out of the callback; do not retain `CefNavigationEntry`.

`CefNavigationEntry` provides:

- `GetURL` — actual URL
- `GetDisplayURL` — display-friendly URL
- `GetOriginalURL` — user-entered URL before redirects
- `GetTitle` — page title (may be empty)
- `GetTransitionType`
- `HasPostData`
- `GetCompletionTime`
- `GetHttpStatusCode`
- `GetSSLStatus`

`GetVisibleNavigationEntry()` returns only the current entry and is
UI-thread-only. Prefer `GetNavigationEntries` from the engine worker.

### When to query

CEF has no `HistoryChanged` callback. Query after:

- `LoadHandler::OnLoadingStateChange` when loading ends
- `DisplayHandler::OnAddressChange`
- `DisplayHandler::OnTitleChange`
- `LoadHandler::OnLoadEnd`
- tab activation / replay start (`EngineCommand::QueryHistory`)

Dedupe snapshots by a hash of `(url, title, current)` so redirects do not emit
a burst of identical events.

### Limits

- History is **per browser**, in-memory, and not persisted across restarts.
- CEF does not expose a global browsing-history database or omnibox.
- CEF's public API has no `GoToIndex`. `GoBack`/`GoForward` move one
  *non-skippable* entry and are asynchronous. L1/R1 sends one step per press
  and accepts the reported landing.
- **Back/Forward skip history-manipulation entries.** Chromium marks some
  entries `should_skip_on_back_forward_ui`; `GetNavigationEntries` lists
  them, but `GoBack`/`GoForward` skip them.
- **Exact jumps use the DevTools protocol.** `Page.getNavigationHistory`
  returns controller entries with stable `id`s, and
  `Page.navigateToHistoryEntry` calls `NavigationController::GoToIndex`
  directly, bypassing the skip filter and preserving the forward tail. CEF's
  `SendDevToolsMessage` / `ExecuteDevToolsMethod` /
  `AddDevToolsMessageObserver` do not require a DevTools window or
  remote-debugging session.
- Selector picks (`browser.history.pick`) use this exact path. If the CDP
  round trip fails or times out, the action records `history.fallback` and
  loads the requested URL (a new history entry; forward tail lost) instead of
  oscillating.
- Redirects remain one navigation entry; use `GetOriginalURL` for replay
  matching.
- `HasPostData` means a URL-only replay is not equivalent.

## DevTools protocol for exact history

CEF exposes no index-based traversal, so the engine uses raw CDP messages on
the browser host:

1. `Page.getNavigationHistory` (no params) → `{currentIndex, entries[]}`.
   Match the session index from the canonical CEF snapshot to
   `entries[index].id`. CDP ids are `NavigationEntry::GetUniqueID()`, not the
   array index.
2. `Page.navigateToHistoryEntry` with `{entryId: <id>}` → `GoToIndex`.

Implementation notes (`src/browser/cef_backend.rs`):

- Register one observer per tab with `AddDevToolsMessageObserver`; the
  `Registration` is stored on the worker-owned tab entry.
- Message ids are assigned by the engine, not passed as `0`. With
  `multi_threaded_message_loop` (Windows) `ExecuteDevToolsMethod` posts the
  call to CEF's UI thread and returns `0`, so the id must be known before the
  call to match the asynchronous result.
- `OnDevToolsMethodResult` receives the UTF-8 JSON *result dictionary*
  directly; no envelope parsing.
- A failed `getNavigationHistory`, missing entry id, or rejected
  `navigateToHistoryEntry` emits `HistoryJumpFailed`; the UI settles the
  action and falls back to a URL load.
- No `Page.enable` is required for these methods.

## Keyboard and mouse input

- CEF's `CefKeyEvent` follows the Windows message model: `RAWKEYDOWN` for the
  key transition, `CHAR` for inserted text, and `KEYUP` on release. For
  `CHAR`, `windows_key_code` carries the character code (what `WM_CHAR`
  passes in `wParam`), not the virtual key; using the VK here changes
  lowercase input to uppercase.
- `CHAR` takes UTF-16 code units, so supplementary characters need two
  events. Modifier flags ride every event; Ctrl/Alt/Meta combinations are
  shortcuts, not text. AltGr currently arrives as Ctrl+Alt and is therefore
  not inserted yet.
- Page mouse input is page-local: `SendMouseClickEvent` for left clicks and
  `SendMouseWheelEvent` with CEF's 120-units-per-line wheel scale. Native
  wheel events keep Chromium's nested-scroller handling.
- Automated sessions never receive real iced key/mouse events. Replay calls
  the same app handlers with explicit coordinates (`text`, `key`,
  `mouse.click`, `mouse.wheel`) and `SDSC_DEBUG_INPUT=0` locks the widget
  tree.

## Threading and ownership rules

- CEF browser-host methods may be called on any browser-process thread unless
  the header says otherwise.
- `GetVisibleNavigationEntry` is explicitly UI-thread-only.
- Navigation-entry and download-image visitors call back on CEF's UI thread.
- Convert CEF strings/images to owned Rust values inside the callback.
- Never hold a CEF object reference outside its callback unless CEF documents
  it as ref-counted and safe to retain (e.g. `BrowserHost`).
- Publish engine facts through `EngineHandle`; never mutate UI state from a CEF
  callback.
- Check browser/tab validity before using a callback result; a navigation or
  tab close may have superseded it.

## Adding a browser capability

1. Find the C++ API and its header (start from `cef_browser.h`).
2. Find the Rust wrapper method in the generated bindings and note its name.
3. Add the command/event shape to `src/browser/engine.rs`.
4. Implement the CEF handler/command in `src/browser/cef_backend.rs`.
5. Route active/background events in `src/app/launcher.rs`.
6. Add UI state/message handling (`src/ui/browser/`, `src/app/browser.rs`).
7. Add a trace action/outcome and a replay step if the action is user-visible.
8. Add tests; run `bash scripts/ci.sh`.
9. Update the capability matrix above and `notes/browser-embedding.md`.

Keep new work inside the existing `EngineCommand` / `EngineEvent` seam. Do not
add a second CEF/CEF-like engine layer per feature.
