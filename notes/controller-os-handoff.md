# Controller OS handoff: Browser app, hosted UI contract, system OSK

**Status:** Phases A–C implemented on `browser/main` (Browser v0, system OSK,
hosted contract v0 + Launcher Settings proof). Phase D (the `sdsc-browser`
add-on process split) was implemented and then **reverted**: the Browser is
in-process again, and the optional CEF engine DLC is the only optional payload.
Phase E (chrome redesign) remains open. See
[single-process-browser.md](single-process-browser.md).
**Scope:** controller-OS architecture, Browser v0 chrome removal, system OSK,
hosted app/action contract; the app-process split is historical.
**Supersedes:** the browser-coupled OSK plan in
[browser-osk.md](browser-osk.md) and the OSK/popup/address/tab-UI parts of
[browser-embedding.md](browser-embedding.md). It also revised the ownership and
packaging direction in [browser-home-pivot.md](browser-home-pivot.md); that
separate-app split was reverted in favour of the single-process Browser.

Read this whole note before touching the Browser UI, the OSK, the pad routing,
or the MSIX packaging.

## TL;DR — start here

1. **Strip the Browser to a page surface.** Remove chrome/address/tabs UI/
   bookmarks/history/status; keep page cursor/docking/input/IME and all
   backends. Single tab, restore last page, deny popups. Keep only
   `back`/`forward`/`reload` actions mapped to L1/R1/Options (tap only).
2. **Make the OSK a system service.** Move it out of `ui/browser`/
   `app/browser.rs` into system-owned modules and a topmost no-activate
   overlay. Quadrant default, Grid fallback; Cross-only open; real input
   closes; first Cross on a page field opens.
3. **Prove reuse** with Launcher Settings Title/Arguments using the same OSK.
4. **Define contract v0**: manifest + surface + actions + text session +
   lifecycle. The Browser spec is tiny. Do not build a general UI toolkit or
   the Browser app split in the first pass.
5. The Browser stays **in-process**. A separate `sdsc-browser` add-on process
   (stdio protocol, Home install/update, remote input/text) was implemented and
   then reverted; do not reintroduce it without a committed packaging
   requirement. The optional CEF engine DLC remains.

---

## Contents

1. [Mental model](#1-mental-model)
2. [Locked decisions](#2-locked-decisions)
3. [Current state and what this supersedes](#3-current-state-and-what-this-supersedes)
4. [Architecture](#4-architecture)
5. [Hosted app contract v0](#5-hosted-app-contract-v0)
6. [System OSK design](#6-system-osk-design)
7. [Browser v0 scope](#7-browser-v0-scope)
8. [Browser app split and packaging](#8-browser-app-split-and-packaging)
9. [Implementation phases](#9-implementation-phases)
10. [Testing and QA](#10-testing-and-qa)
11. [File map](#11-file-map)
12. [Do not regress](#12-do-not-regress)
13. [Risks / open design items](#13-risks--open-design-items)
14. [Definition of done — first pass](#14-definition-of-done--first-pass)

---

## 1. Mental model

The main app is a **virtual OS / shell for controllers**.

| OS concept | SDSC-Utils concept |
|---|---|
| Input driver / service | HID worker, pad input routing |
| Shell / launcher | Home/PS overlay, Launcher |
| System settings | Settings, OSK mode |
| Input method | **OSK** |
| Notifications | Toasts |
| App catalog / package manager | Home apps, DLC install, Browser tile |
| System apps | Launcher, Settings, popup |
| User apps | Browser (in-process managed app; optional CEF engine DLC) |
| System overlays | Home, toasts, **OSK** |

Consequences:

- The Browser is an **app**, not a privileged part of the OS.
- The OSK is a **system input method**, not a Browser component and not a
  shared UI library embedded in every app.
- The OS owns input, focus, text input, lifecycle, overlays, theming, and
  controller binding.
- Apps provide content surfaces and small declarative specifications; the OS
  renders standard UI.
- Page-specific interaction stays in the app. The OS must not try to model
  page targets/docking.

The Browser is a managed app conceptually, but it runs **in-process** with the
shell. A separate `sdsc-browser` executable/package was trialled and reverted;
the optional CEF engine DLC is the only optional payload. The app/OS boundary
(actions, text session, lifecycle) is still useful, but it is a module
boundary, not an IPC boundary.

---

## 2. Locked decisions

Do not re-litigate these unless the user explicitly changes them.

### Ownership

- OSK lives in the **main app/OS**. It must not live under `ui/browser`,
  `app/browser.rs`, or any Browser component.
- The Browser app ships **no OSK UI, layout, gesture code, or mode setting**.
  It only implements a text-session client.
- The Browser app owns the **page surface, page DOM/focusables, page cursor,
  magnet/docking, D-pad jumps, page click/scroll/hover, page text/IME**.
  These are page semantics, not OS chrome.
- The OS owns **pad input routing, OSK, Home/PS, toasts, lifecycle, action
  mapping, and overlays**.
- The hosted UI contract must **not** define a generic hit-target/docking
  model. It provides a surface and input routing; the app owns in-surface
  behavior.

### Controller/text policy

- The OSK is **controller-only**.
- **Only Cross opens the OSK.**
  - The first Cross on a page text field focuses it and opens the OSK.
  - A Cross whose target is a page text field opens the OSK even if that field
    is already focused.
  - Cross on other page targets keeps its normal click behavior; a focused
    text field elsewhere does not hijack a link/button click.
  - Page-load autofocus, programmatic `.focus()`, mouse clicks, and keyboard
    focus must **not** open the OSK.
- **Any real mouse movement/button/wheel, keyboard/IME, or touch input closes
  the OSK.**
  - It also clears any pending Cross-driven open request.
  - The key that closes the OSK is then processed normally (hand-off to the
    page/app), except Escape, which is consumed.
- **F6 is removed** as an OSK shortcut.
- **Square no longer opens the OSK.**
- **Triangle no longer opens URL editing.** There is no address entry in v0.
- OSK keys and its control bar are **not mouse-clickable**; real mouse input
  dismisses the OSK.
- The OSK mode is a **system setting**: Quadrant default, Grid fallback.
  No per-app mode.
- The OSK renders as a **topmost, no-activate system overlay window** owned by
  the main app. It must not steal focus from the Browser/app. Controller input
  comes from the main app's HID path, not window focus.
- The OSK is **not** rendered as part of the Browser view.

### Action contract

- Apps declare **semantic actions**; the OS owns the controller binding table.
- The app never knows which button was pressed; the OS sends
  `ActionInvoked { id }`.
- The app publishes enabled state for actions.
- v0 Browser actions: `back`, `forward`, `reload`.
- OS defaults:
  - **L1 → back**
  - **R1 → forward**
  - **Options → reload**
- **Tap only. No hold actions, no history selector UI, no action hints/status.**
  (Current `Options` maps to `NavAction::ToggleSettings` and is ignored by the
  Browser tick; implement it as reload per this contract.)
- Future chrome (tabs, bookmarks, history, address) returns as actions and
  widgets under this contract. Do not build that now.

### Browser v0

- **Single tab.**
- **Restore the last page** on open.
- **Popups denied entirely.** `window.open` / `target=_blank` does nothing; no
  new tabs, no popup-as-tab.
- **No address entry.**
- **No tabs UI or tab bindings.**
- **No bookmarks/history UI or bindings.**
- **No status chrome** (no loading/error/title strip).
- **Close only via Home/PS.**
- Keep page surface, cursor, scroll, click, page text/IME.
- Keep back/forward/reload actions.
- Keep backends: tabs, session history, bookmarks, history data, exact
  DevTools history-jump code (dormant until the redesign).
- The Browser is **chrome-less, not UI-less**: it still owns the page surface
  and page-specific pointer/cursor behavior.

### Hosted app UI contract

v0 is intentionally tiny:

- `manifest` — id, name, icon, version, capabilities.
- `surface` — app-rendered content + input region.
- `actions` — semantic action IDs + enabled state.
- `text_session` — page text focus; OSK text actions.
- `lifecycle` — launch/pause/resume/close (OS-owned).

No widgets, tabs, toolbar, bookmark bar, status, hints, or text-prompt widgets
yet. Apps provide a specification, not pixels or their own widget tree. Native
surfaces are the escape hatch for custom content.

---

## 3. Current state and what this supersedes

Current code:

- Branch: `browser/main`.
- Single package; Browser UI is in the main `sdsc-utils.exe`.
- Optional `BrowserEngine` MSIX contains **only CEF runtime files** under
  `cef\`; it has no Browser UI/exe. See `packaging/README.md` and
  `packaging/BrowserEngine.AppxManifest.xml`.
- Browser chrome (tab strip, toolbar, omnibox, bookmark bar, suggestions,
  history selector, status overlay), Browser-owned OSK, auto-OSK on editable
  focus, popup-as-tab, and address editing all exist in the current branch.
- Recent commits:
  - `9e64d21` popups as tabs + page IME/OSK
  - `dcb3474` IME bridge render fix + pixel QA
  - `3ebc4e0` auto-OSK on editable focus

Reuse:

- CEF page input/mouse/IME path (`src/browser/keys.rs`,
  `src/browser/cef_backend.rs`, `src/app/browser.rs` IME handlers).
- Page DOM/focusables and cursor/docking (`src/browser/dom.rs`,
  `src/browser/cursor.rs`, `src/ui/browser/view.rs` cursor overlay).
- Engine/session/history/bookmarks/tabs data.
- Pixel/input QA harnesses, adjusted for the new scope.

Remove/supersede:

- Browser chrome UI/state/targets and chrome-specific docking.
- Popup-as-tab path; popups now deny.
- URL editing/omnibox.
- Browser-owned OSK and auto-open-on-focus.
- Tab strip/tab switching/bookmarks/history UI and their bindings.
- Bumper-hold history selector.
- Custom status/loading chrome.
- F6/Square OSK open; Triangle→URL.

The first implementation can keep the Browser in-process. The OSK modules can
be moved to system ownership without waiting for the full Browser app split.
Do not attempt the entire app split in the same change as the chrome/OSK work.

---

## 4. Architecture

```text
Main app (OS / shell)
  HID / pad input
  system OSK overlay (topmost, no-activate)
  Home / PS / toasts
  app lifecycle / install
  action mapping (L1/R1/Options)
  hosted UI renderer (future)
        │  pad input when OSK closed
        │  surface input, actions, text actions
        ▼
Browser app (add-on / transitional in-process)
  CEF engine + page surface
  page DOM/focusables
  page cursor / magnet / docking / D-pad jumps
  page click / scroll / hover
  text-session client
  back / forward / reload handlers
```

### Input split

- **Surface input:** sticks, D-pad, Cross click, scroll, pointer position →
  Browser. The Browser consumes it for cursor/docking/hit-testing/CEF.
- **Semantic actions:** Back, Forward, Reload → OS maps L1/R1/Options and sends
  action events.
- **System input:** Home/PS, OSK open/close, real-input dismissal.
- **Text:** Browser reports page text focus; OSK sends text actions; Browser
  applies to CEF.

### Action flow

```text
Browser publishes:
  actions: back(enabled), forward(enabled), reload
OS renders/maps:
  L1 -> back, R1 -> forward, Options -> reload
OS sends:
  ActionInvoked { id: "back" }
Browser:
  go_back(); republish enabled state
```

No hints/status for v0. The binding table can be silent; future app chrome can
render hints from it.

### Text flow

```text
Browser: Cross target is a page text field
Browser: TextSession { active: true } + open-text-input request
OS:      opens system OSK (validated against the Cross edge)
User:    types via Quadrant/Grid
OS:      sends TextAction::Insert/Backspace/Enter/Cancel
Browser: applies to CEF / focused page field
OS:      real mouse/keyboard/IME/touch -> close OSK
```

Page semantics stay in the Browser: it decides whether the Cross target is a
text field. The OS only opens the OSK on that request, tied to a Cross input.
The first Cross on a text field both focuses and requests the OSK. Page-load
autofocus alone never opens it.

### Real-input close

The OSK is controller-only. Any real mouse movement/button/wheel, keyboard/IME,
or touch event closes it and clears any pending open request. The event is then
processed normally (Escape is consumed). This policy belongs to the OS, not the
Browser.

---

## 5. Hosted app contract v0

The Browser v0 spec:

```text
App: Browser
Surface: CEF page
Actions:
  back     enabled = can_go_back
  forward  enabled = can_go_forward
  reload   enabled = true
Text: page text session
Lifecycle: OS-owned
```

No widgets, no status, no tabs/toolbar/bookmarks, no text prompt, no address.

Contract rules:

- The app provides a manifest and a serializable view model/specification; it
  does not provide pixels for chrome and does not render system UI.
- Semantic roles only for any future widgets; the OS owns theme/colors/fonts.
- Native surface nodes are the escape hatch for custom content (CEF, video,
  games). The Browser's page surface is one.
- The OS validates specs; apps cannot draw arbitrary chrome.
- Version the contract; apps declare required capabilities.
- Start narrow. Do not build a general-purpose UI toolkit until the first apps
  require it.

Future additions (not now):

- widgets: text, icon/image, button, list, tab strip, toolbar, text field,
  dialog/popover, progress/spinner;
- status/title;
- action hints;
- richer focus/navigation hints;
- third-party app API.

---

## 6. System OSK design

### Modes and settings

- `OskMode::{Quadrant, Grid}`, global system pref.
- Default **Quadrant**.
- Grid is the fallback/accessibility mode.
- Settings → **System** section, one row: **Controller keyboard** →
  Quadrant / Grid.
- Debug/QA override: `SDSC_OSK_MODE=quadrant|grid` (debug builds).
- Mode changes while the OSK is open reset both halves/gestures.

### Quadrant layout

Split QWERTY. Left stick covers the left half, right stick the right half.

```text
LEFT                          RIGHT
 Q W E R T                    Y U I O P
  A S D F G                   H J K L ;
   Z X C V B                  N M , . /
```

Derived clusters (the article's `FTGB` example is the left "right" quadrant):

| Half | Quadrant | Default / inner | Up | Down | Outer |
|---|---|---|---|---|---|
| Left | Right | F | T | B | G |
| Left | Left | S | Q | Z | A |
| Left | Up | E | — | — | W (left) / R (right) |
| Left | Down | C | — | — | X (left) / V (right) |
| Left | Home | D (L3 click) | | | |
| Right | Right | L | P | / | ; |
| Right | Left | J | Y | N | H |
| Right | Up | I | — | — | U (left) / O (right) |
| Right | Down | , | — | — | M (left) / . (right) |
| Right | Home | K (R3 click) | | | |

Sub-key rules:

- Horizontal quadrants: up/down if the perpendicular axis passes the sub
  threshold; outer if the along-axis magnitude is near full; otherwise the
  default inner key.
- Vertical quadrants: left/right if the perpendicular axis passes the sub
  threshold; otherwise the default center key. The outer slot duplicates the
  default (no fourth key).
- Full deflection selects the outer key only when not in an up/down sub-zone.
- Home keys are not in any quadrant; use L3/R3 clicks.

Symbol layer (proposal; tune during implementation):

```text
LEFT                          RIGHT
 1 2 3 4 5                    6 7 8 9 0
 - / : ; (                    ) $ & @ "
 . , ? ! '                    # % + = ~
```

Home symbols are the center keys (`:` and `&`) by default; rearrange if needed.

### Gesture state machine

Per half, independent:

1. **Idle** — inside arm radius.
2. **Arm** — cross arm radius; choose quadrant by dominant axis; latch it.
3. **Preview** — update sub-key with hysteresis.
4. **Commit** — returning inside release radius commits the preview once.
5. **Re-arm** — require neutral before the next character.

Rules:

- Commit on **release to neutral**, not on sub-key entry. This prevents typing
  the default key on the way to a sub-key.
- Quadrant latches until neutral for v1. Crossing the center into another
  quadrant cancels the gesture; require neutral. (Future: cross-center
  re-latch if playtesting wants it.)
- Use hysteresis on arm, release, sub-direction, and outer thresholds.
- Both sticks may be active independently.
- Opening/closing/layer change resets both halves and requires neutral.
- Starting thresholds (tune with a real pad):
  - arm radius `0.55`
  - release radius `0.22`
  - sub-direction enter `0.45`, leave `0.32`
  - outer enter `0.82`, leave `0.70`
- `PadSample.lstick_x/y` and `rstick_x/y` already have the soft `0.12`
  deadzone; use a radial arm threshold above that.

### Controls

| Input | Action |
|---|---|
| Left stick | Left half |
| Right stick | Right half |
| Cross | Space |
| Triangle | Backspace (tap; no repeat in v0) |
| R2 | Enter |
| Square | Alpha / Symbols |
| Circle | Close OSK |
| L3 | Home key left (D) |
| R3 | Home key right (K) |

- OSK keys/control bar are static; no mouse click handlers.
- Grid mode retains D-pad focus, Cross activate, Triangle backspace, Square
  layer, Circle close. No mouse-clickable keys.
- First Cross on a page text field opens the OSK; no focus auto-open.

### System overlay

- Topmost, no-activate window owned by the main app.
- Controller input comes from the main app's HID path; the OSK does not need
  keyboard focus.
- Mouse is not an interaction path: any real mouse movement/button/wheel
  closes the OSK. If the overlay is click-through, the focused app must report
  real input to the OS; if hit-testable, the system can close directly. Decide
  during implementation. The mouse event does not need to reach the app after
  dismissing the OSK; keyboard hand-off is required, mouse hand-off is not.
- Home/PS opens above the OSK; opening Home closes the OSK.
- Platform work: `WS_EX_NOACTIVATE`/topmost/click-through, z-order with
  Home/toasts. Reuse the existing overlay/window-level helpers where possible.
- Text actions route to the focused host. In the transitional in-process
  Browser, routing can be direct; after the app split, use the app protocol.

### Non-browser proof host

Use **Launcher Settings → Add shortcut → Title / Arguments** as the chosen
non-browser proof host (redirect only if the user asks).

- Make Title/Arguments pad-focusable (D-pad moves focus; visual focus ring).
- Cross on a focused text field opens the system OSK.
- OSK text/backspace edits the active draft (`ManualAddDraft.title` / `.args`).
- While the OSK is open, Cross = Space; save/add moves to R2 or the OSK Enter
  key (or the existing Save button after closing the OSK). Resolve the current
  Cross=Save/Add conflict explicitly.
- This is a system app, so it can apply text locally; the external app contract
  does not need a text-field widget yet.

Fallback proof host: popup nickname editor. Not preferred; the popup window is
too small for a full OSK overlay.

---

## 7. Browser v0 scope

### Keep

- CEF page surface and engine/session.
- Page DOM query/focusables.
- Page cursor, magnet, docking, D-pad jumps, page overlay.
- Page click/scroll/hover/pointer modality.
- Page text/IME.
- Back/forward/reload action handlers and enabled state.
- Session history, tabs/bookmarks/history data.
- Default page and last-page restore.
- Exact DevTools history-jump backend (dormant).
- Existing CEF build/engine packaging pieces (unless the app split changes
  them later).

### Remove

- Chrome view/layout and `ChromeTarget` generation.
- Chrome docking/header-unified cursor space.
- Tab strip, tab close/overflow, tab switching bindings.
- Toolbar, omnibox, URL editing/draft, Go/default/reload chrome buttons.
- Bookmark bar, star bubble, suggestions, history selector.
- Custom status/loading chrome.
- Bumper-hold history selector (`BumperHoldLeft/Right` handling and UI).
- Popup-as-tab; `PopupRequested` denies/no-ops instead of creating a tab.
- Auto-OSK on editable focus; Browser-owned OSK state/view/tick.
- F6/Square OSK open; Triangle→URL.
- Ctrl+L URL entry and the URL draft; keep ordinary page keyboard input and
  non-address navigation shortcuts.
- Circle/Esc Browser close; Home/PS is the only close path.

### Keep page cursor/docking

This is the critical boundary:

- Page focusables/DOM/cursor/docking stay in the Browser app.
- The OS routes raw pointer/stick/D-pad/Cross/scroll input to the Browser
  surface.
- The OS does not know about links/fields/focusables.
- The contract surface does not model hit targets or docking.
- The Browser renders the page cursor overlay; it is not browser chrome.

### Controller defaults

- L1 → Back (action)
- R1 → Forward (action)
- Options → Reload (action)
- Sticks/D-pad/Cross → surface input
- Home/PS → OS lifecycle
- Tap only; no hold; no hints/status.
- Back/forward enabled state comes from the session (`can_go_back/forward`).
- Deny popups entirely; `target=_blank` links do nothing.

### Navigation/edge cases

- No address entry. Navigation is via page links, page search fields, and
  back/forward/reload actions.
- Restore last page on open; back/forward history should make it recoverable.
- Page errors have no custom status chrome; the page surface is whatever CEF
  renders (error page/blank).
- Tabs remain single-tab in v0; tab code/data stays dormant. Keep the
  tab-capable backend and data; do not delete multi-tab support. No tab UI,
  no tab bindings, and popups are denied until the redesign.
- Bookmarks/history data stays dormant with no UI/bindings.

---

## 8. Browser app split and packaging

> **Reverted (2026-11-26).** The D2 add-on process (`sdsc-browser`), the
> `browser-protocol` transport crate, Home add-on install/update, remote
> OSK/text bridging, and the remote QA scripts were removed. The Browser
> window, CEF engine, cursor, and text session are in-process again; the
> optional `BrowserEngine` DLC is unchanged. The material below is retained as
> design history only. See [single-process-browser.md](single-process-browser.md).

The current packaging is **not** the target:

- Main MSIX ships `sdsc-utils.exe` with the Browser UI.
- Optional `BrowserEngine` MSIX contains only CEF runtime.
- Target: the **entire Browser app** (UI + CEF) is an optional add-on with its
  own executable/package.

Required design before splitting:

1. **Controller input ownership**
   - Main app owns HID.
   - Recommended: main app streams pad samples/edges to the Browser add-on
     over local IPC. One HID owner; Home/PS/toasts keep working.
   - Alternative: Browser owns HID while running; main app pauses. This breaks
     tray/toasts/Home input; not recommended.
   - Alternative: Browser UI as a module/DLL in the main process. Not its own
     exe; avoids IPC but is a different product model.

2. **System overlays**
   - OSK and Home/PS must appear above the Browser add-on.
   - Main app owns the topmost no-activate OSK window and Home overlay; Browser
     keeps focus.
   - Opening Home closes the OSK; pausing the Browser pauses its input.

3. **Packaging**
   - One Browser add-on MSIX containing `sdsc-browser.exe` + CEF runtime, or
     Browser UI package + existing `BrowserEngine` package.
   - Executable optional packages force a Store related set: main + add-on are
     submitted/updated together; Chromium patches require resubmission.
   - If independent app updates are desired, the Browser must be a separate
     Store app, not a DLC. Decide explicitly.

4. **State/lifecycle**
   - Tabs/bookmarks/prefs/data directory, last-page restore, OSK mode, app
     identity, launch/foreground/single-instance/crash-restart/update.
   - Pass/share the OSK mode? The OSK is system-owned, so the Browser app does
     not need it; the main app keeps it.

5. **Shared protocol boundary**
   - A small versioned app protocol crate for manifest/surface/actions/
     text-session/lifecycle. The OSK UI is **not** shared with apps.
   - Browser app has no iced chrome/OSK; it owns CEF + page surface + page
     cursor + text-session client + action handlers.
   - Main app renders system UI/overlays; system apps continue to use iced
     directly.

### Tree-shaking / ownership

- The OSK ships **only in the main app/OS**. The Browser add-on ships no OSK,
  layout, gesture, or mode code.
- The Browser add-on ships CEF + page surface + page cursor/docking + its text
  client + action handlers.
- The main app does **not** link CEF/Browser UI after the split.
- The shared boundary is the app protocol crate (manifest/surface/actions/
  text-session/lifecycle), not the OSK UI.
- Cargo features can gate Grid/Quadrant modes per build if a particular app
  does not need both; both apps use the system OSK service, not a shared UI
  library.

Recommended sequence: do the in-process chrome removal + system OSK + contract
v0 first. Design/implement the app split after the protocol, IPC, overlay, and
packaging decisions are settled. Do not attempt the full split in the first PR.

---

## 9. Implementation phases

### Phase A — Browser chrome removal

- Remove chrome UI/state/targets and chrome-specific docking, tab
  strip/toolbar/omnibox/bookmark bar/suggestions/history selector/status
  overlay.
- Remove URL editing and popup-as-tab; deny popups.
- Single tab; restore last page.
- Keep page surface, cursor/docking/D-pad jumps, page input, page text/IME,
  session/history/bookmarks/tab data.
- Add `back`/`forward`/`reload` actions; OS defaults L1/R1/Options; tap only.
- Remove bumper-hold/history selector bindings.
- Update tests/QA.

### Phase B — System OSK

- Move OSK state/view/tick out of `ui/browser`/`app/browser` into system
  ownership (`ui/osk/` + `app/osk.rs` or equivalent).
- Implement the topmost no-activate overlay window/platform bits.
- Grid mode first (move existing behavior), then Quadrant.
- Add global `OskMode` pref + System settings row + debug override.
- Controller-only policy: Cross-only open, real-input close, keyboard handoff,
  static keys, no F6/Square.
- Page text session: first Cross opens; text actions apply to CEF.
- Non-browser proof in Launcher Settings.

### Phase C — Hosted app contract v0

- Define manifest/surface/actions/text-session/lifecycle types.
- Browser spec: surface + actions + text session.
- Implement the OS action mapping table for back/forward/reload.
- Transitional in-process Browser host of the contract; no UI widgets yet.

### Phase D — Browser app split/packaging (reverted)

- ~~Settle IPC/input/overlay/packaging decisions.~~
- ~~Split Browser into its own executable/add-on; main app keeps OS/shell/OSK.~~
- ~~Home install/launch/update flow.~~
- ~~Remove Browser UI/CEF from the main binary; tree-shake.~~
- **Reverted 2026-11-26:** the split's cold launch, CEF lifecycle, IPC input
  queue, and add-on install/versioning cost more than the unlanded packaging
  benefit. Keep the Browser in-process; the engine DLC is the optional payload.

### Phase E — Future redesign

- Reintroduce tabs/bookmarks/history/address as semantic actions and standard
  widgets under the UI contract.
- Reuse the exact DevTools history-jump backend.
- Grow the UI contract only as new apps require it.

---

## 10. Testing and QA

CI order is always **fmt → clippy → test → build** (`bash scripts/ci.sh`); CI
also lints/builds release, where debug-only code disappears. Use synthetic
replay only; never OS input injection; keep `SDSC_DEBUG_INPUT=0`.

Unit tests:

- Quadrant mapping, thresholds/hysteresis, neutral re-arm, cross-center cancel,
  both sticks, home keys, symbols, mode switching.
- Action manifest/enabled state and OS mapping defaults.
- Browser v0: single tab, restore last page, popups denied, no chrome targets,
  page cursor/docking still works, back/forward/reload actions.
- OSK system policy: Cross-only, real-input close, keyboard handoff, static
  keys.
- Contract types/versioning; Launcher Settings text-draft application.

Replay/pixel QA:

- `qa_browser_input.py` — page input/IME; update for no chrome and system OSK.
- `qa_browser_popup.py` — assert popups are denied and no tab is created.
- `qa_browser_visual.py` — page surface only; no chrome/status; keep the pixel
  uniform-render guard.
- New action QA — L1/R1/Options fire back/forward/reload; enabled state
  respected; no hold/hints.
- New system OSK QA — first Cross on a page field opens; Quadrant gesture types;
  Grid fallback via `SDSC_OSK_MODE=grid`; mouse/keyboard close + hand-off.
- Non-browser OSK QA — Launcher Settings Title/Args editing; save/add path.
- Manual controller playtest for quadrant thresholds/feel, overlay z-order,
  no-activate focus, Home/OSK interaction.

---

## 11. File map

Current touch points:

- Browser UI/chrome/cursor: `src/ui/browser/view.rs`,
  `src/ui/browser/state.rs`.
- Browser app/pad/text: `src/app/browser.rs`, `src/app/launcher.rs`,
  `src/app/mod.rs`.
- Browser engine/page: `src/browser/cef_backend.rs`, `src/browser/engine.rs`,
  `src/browser/dom.rs`, `src/browser/cursor.rs`, `src/browser/keys.rs`.
- OSK current implementation: `src/ui/browser/state.rs` (`OskKey`, `OskLayer`,
  `OskState`, `OSK_ALPHA`, `OSK_SYMBOL`), `src/ui/browser/view.rs`
  (`browser_osk`), `src/app/browser.rs` (`toggle_browser_osk`,
  `osk_activate_key`, `osk_type`, `osk_backspace`, `osk_named`,
  `tick_browser_osk`, replay `osk.*`), `src/app/launcher.rs` (`panel.osk`
  close calls).
- Input/IME: `src/ui/input_method.rs`, `src/ui/input_lock.rs`.
- Prefs/settings: `src/persist/prefs.rs`, `src/ui/configure/mod.rs`.
- Non-browser host: `src/ui/launcher/settings.rs`,
  `src/ui/launcher/view.rs` (`ManualAddDraft`), `src/app/launcher.rs`.
- Packaging: `packaging/`, `src/platform/packaged.rs`,
  `src/browser/engine_install.rs`, `notes/install-qa.md`.

Likely new modules:

- `src/ui/osk/` — system OSK state/layout/view (Quadrant + Grid), no Browser
  imports.
- `src/app/osk.rs` — system OSK session/input/output/overlay lifecycle.
- `src/app/actions.rs` or equivalent — semantic action manifest/mapping.
- ~~Phase D: app protocol crate + Browser add-on executable/package.~~
  Removed in the single-process migration.

Docs to update during implementation:

- `notes/browser-osk.md` (superseded by this note).
- `notes/browser-embedding.md` (stale OSK/popup/address/tab-UI lines).
- `notes/browser-home-pivot.md` (ownership/packaging model).
- `notes/tracing.md` (new action/OSK events).
- `AGENTS.md` (QA commands, Browser/OSK ownership).

---

## 12. Do not regress

- Page cursor/docking/D-pad jumps stay in the Browser app; do not move them to
  the OS or drop them with chrome.
- OSK is a system input method; never move it back into the Browser app.
- Cross is the only OSK opener; first Cross on a text field.
- Real mouse/keyboard/IME/touch closes the OSK; keyboard key hand-off.
- F6, Square-open, Triangle→URL are removed.
- No OSK mouse key clicks.
- No browser chrome, tabs UI, address, bookmarks/history UI, or status chrome.
- Single tab v0; popups denied.
- L1/R1/Options actions; tap only; no hold/history selector/hints.
- Home/PS is the only Browser close path.
- Keep session/tab/bookmark/history backends and the exact DevTools history
  backend dormant.
- No OS input injection in QA; use synthetic replay with real input locked.
- Do not build a general UI toolkit before the contract needs it.

---

## 13. Risks / open design items

- **Browser app split**: HID ownership/IPC, Home overlay above the add-on,
  package layout, Store related-set/update cadence, shared data dir/lifecycle.
  Not settled; do not guess in the first PR.
- **OSK overlay**: topmost/no-activate/click-through and z-order with Home;
  text-session sequencing for the first Cross; cross-process text actions after
  the split.
- **Launcher Settings proof**: pad focus for text fields, Cross=Save conflict,
  applying text to iced text inputs without cursor/selection jank.
- **Contract scope**: keep v0 narrow; avoid building widgets/tabs/toolbars
  before the redesign.
- **No address/popups**: navigation is intentionally limited; back/forward/
  reload and last-page restore are the escape hatches. Accepted for v0.
- **Dormant backends**: exact DevTools history, multi-tab, bookmarks/favicons
  are kept but unreachable; avoid dead-code warnings/tests where practical.
- **Large removal**: the chrome removal touches many tests; keep page
  input/cursor/IME/engine intact and re-baseline QA.

---

## 14. Definition of done — first pass

Phases A–C are complete when:

- The Browser runs without chrome, address, tabs UI, bookmarks/history UI, or
  status chrome.
- Single tab, restore last page, popups denied, Home/PS close only.
- Page surface, page cursor/magnet/docking/D-pad jumps, scroll/click/hover,
  page text/IME all still work.
- Back/forward/reload actions work via L1/R1/Options with enabled state; no
  hold/hints.
- The OSK is system-owned and rendered as a topmost no-activate overlay, not a
  Browser component.
- Quadrant is the default mode; Grid fallback; System settings row; debug
  override; mode persists.
- Cross-only open, first Cross on a page field; real input closes; keyboard
  hand-off; F6/Square/Triangle→URL removed; static keys.
- Page text sessions use the system OSK; no address prompt.
- Launcher Settings Title/Arguments uses the same system OSK as the non-browser
  proof.
- Contract v0 types and Browser spec (surface/actions/text/lifecycle) are in
  place; no generic widget toolkit yet.
- Unit/replay/pixel QA updated; `bash scripts/ci.sh` passes (debug + release).

The Browser app split/packaging (Phase D) remains a follow-up, not part of the
first pass.
