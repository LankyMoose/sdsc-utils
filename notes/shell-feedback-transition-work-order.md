# Work order: shell feedback, transitions, backdrop, and reveal

**Status:** landed — 2026-11-28. FB-1..4, TR-1, UI-1, UI-2, and REV-1 are in
the source commit; this file is the follow-up docs commit.
**Baseline:** `07ae2f3` (navigation dispatcher + outcome-based cues landed).
**Scope:** feedback consistency for shell commits, Launcher→Home transition,
Home settings dropdown overlay, shared-backdrop click semantics, and the PS
reveal first frame.
**Related:** [browser-home-pivot.md](browser-home-pivot.md),
[pull-model.md](pull-model.md), [ui-refresh.md](ui-refresh.md),
[browser-embedding.md](browser-embedding.md),
[navigation-dispatcher-work-order.md](navigation-dispatcher-work-order.md).

## 1. Symptoms

1. Cross on a Home tile enters Launcher/Browser with no sound or haptic.
2. Circle to close Launcher plays the sound but no haptic pulse.
3. Circle from Browser to Home plays no cue.
4. Circle from Home settings to Home plays no cue.
5. Cross in the Browser tabs view plays `Nav` for tab, plus/new-tab, and
   bookmark activation; those are commits, not moves.
6. Launcher→Home has no outgoing animation (hard cut to Home entering).
7. Home settings dropdown reflows the rows; Launcher settings already overlays
   its dropdown without moving rows.
8. Clicking the shell backdrop must always act like PS (hide to desktop):
   Browser currently does nothing; Home settings closes the settings surface.
9. PS open occasionally presents the final resting UI first, then reverts and
   plays the reveal animation.

## 2. How we landed here

- **Feedback is a call-site convention, not a shell contract.** `App::apply_*`
  exists (`src/app/feedback.rs:49-67`), but screen commits are added in
  different reducers. `home_activate` just batches close/open
  (`src/app/home.rs:1019-1049`); `return_to_home_screen` only transitions
  (`src/app/home.rs:407-425`); the Home settings close branch only closes state
  (`src/app/home.rs:778-790`).
- **Haptic lifetime is not owned by the cue.** `on_launcher_close` plays the
  cue before `return_to_home_screen` (`src/app/launcher.rs:346-395`), but
  `retire_launcher` clears the haptic target and calls `rumble_stop_all`
  (`src/app/launcher.rs:4545-4571`), cancelling the pulse it just started.
- **Browser tab activation uses the back/close cue path.**
  `browser_grid_activate_focus` calls `close_browser_grid`
  (`src/app/browser.rs:3319-3326`), whose wrapper always applies `Nav`
  (`src/app/browser.rs:2032-2043`); `browser_grid_select` does the same
  (`src/app/browser.rs:2228-2235`). `open_browser_tab` applies `Nav` for plus
  and bookmark opens (`src/app/launcher.rs:539-575`).
- **Only Home/Browser have shell animations.** `ShellAnim` and
  `home_shell_anim`/`browser_shell_anim` drive enter/exit
  (`src/ui/home/mod.rs:131-180`, `src/app/shell.rs:311-333`); Launcher has no
  `launcher_shell_anim`, and `return_to_home_screen` retires it before Home
  enters.
- **Home settings has a second dropdown implementation.** It pushes the panel
  into the settings `column` (`src/ui/home/mod.rs:2092-2163`), so rows move.
  Launcher overlays its panel in a `stack` with fixed geometry
  (`src/ui/launcher/settings.rs:1050-1192, 1494, 1542, 1762`).
- **The backdrop click is a screen message, not a shell message.** Home's
  backdrop sends `HomeMessage::Close`
  (`src/ui/home/mod.rs:1540-1553`), which unwinds settings first; Browser's
  root has no backdrop click handler (`src/ui/browser/view.rs:526-646`);
  Launcher's immersive root has none either
  (`src/ui/launcher/immersive.rs:43-205`).
- **The reveal prime is a fixed timer.** `on_shell_show_now` starts the enter,
  shows offscreen, waits 32 ms, then presents (`src/app/shell.rs:39-73`);
  `show_shell_offscreen`/`show_shell_at_cover`
  (`src/ui/layout.rs:290-334`) assume the offscreen frame has painted. A slow
  first paint re-presents the warm window's stale final frame.

## 3. Locked rules

1. **Cue semantics are explicit.**
   - `Action`: an accepted commit that changes screen/app/tab state — launch
     from Home, activate/open a tab or bookmark, close a tab, close
     Browser/Launcher to Home, close the Home settings surface.
   - `Nav`: focus movement/scroll and dismissing a transient in-screen overlay
     (dropdown/menu) that leaves the screen active.
   - Exactly one cue per accepted operation. Rejected/no-op paths are silent.
   - Domain operations stay silent; use
     `App::apply_move_outcome`/`apply_action_outcome`/`apply_nav_cue`, never a
     direct `play_ui_cue` in a shell reducer.
2. **One backdrop contract.** A click outside the active card always performs
   the same action as PS: dismiss the current shell screen to the desktop
   (`ps_screen_toggle`). It never closes an inner settings/modal panel, and it
   works on Home, Home settings, Browser page, Browser tabs, and Launcher.
3. **One screen-transition routine.** Home, Browser, and Launcher enter/exit
   through one shell transition path. The outgoing card fades before the
   incoming card enters; Launcher gets the same treatment as Home/Browser.
   Preserve the warm single HWND, topmost/foreground fallback, and one backdrop
   owner.
4. **Home dropdowns are overlays.** Opening a Home settings dropdown must not
   move any row or the card. Reuse the Launcher settings overlay/geometry
   pattern (or extract a shared widget) rather than pushing the panel into the
   column.
5. **The first visible shell frame is the entrance pose.** No final-resting
   frame may be presented before the reveal. Prime must be event-driven or
   proven by a counter/trace, not a fixed delay.
6. **No new global timers/subscriptions.** Use the existing shell frame demand
   and pull-model helpers.

## 4. Work items

### FB-1 — Home tile activation cue

- File: `src/app/home.rs` (`home_activate` 1019-1049, `Confirm` 760-778).
- Apply one `Action` cue only after the activation is accepted (Launcher
  enabled; Browser engine ready). Do not cue the disabled/missing-engine
  branches. Ensure `open_launcher`/`open_browser_window` do not also cue.
- Test: debug cue tally around one Cross activation for each tile; no cue on a
  disabled Launcher tile or missing Browser engine.

### FB-2 — Launcher close haptic

- Files: `src/app/launcher.rs` (`on_launcher_close` 346-395,
  `retire_launcher` 4545-4571).
- Keep the existing `Action` sound. Stop cancelling the just-sent pulse: do not
  clear `haptic_pad_serial`/`rumble_stop_all` on a Launcher→Home mode switch;
  stop only on hide-to-desktop/teardown (or defer the stop past the pulse).
- Add a debug haptic tally/log next to the cue counter so QA can assert one
  haptic command per close.

### FB-3 — Circle to Home and Home settings close

- Files: `src/app/home.rs` (`return_to_home_screen` 407-425,
  `HomeMessage::Close` 778-790), `src/app/launcher.rs` (`on_launcher_close`
  388-395).
- Add exactly one `Action` cue when Browser or Launcher closes to Home. Move
  the Launcher top-level cue out of `on_launcher_close` into the shared
  transition path (or remove one) so the close cannot double-sound.
- Add one `Action` cue when Circle closes the Home settings surface; keep the
  dropdown-close cue as `Nav`.
- Test: cue tally for Browser→Home, Launcher→Home, Home settings→Home, and
  dropdown close.

### FB-4 — Browser Cross activation cues

- Files: `src/app/browser.rs` (`browser_grid_activate_focus` 3319-3326,
  `browser_grid_select` 2228-2235, `close_browser_grid` 2032-2043,
  `browser_grid_open_plus` 2238, `browser_grid_open_bookmark` 2416),
  `src/app/launcher.rs` (`open_browser_tab` 539-575).
- Make Cross on a tab, plus, or bookmark an `Action` cue. Keep Circle/back and
  PS close on the `Nav` path. `open_browser_tab` is the activation cue for
  plus/bookmark; do not add a second cue in the grid close.
- Make the close cue intent explicit (`Back` vs `Activate`) instead of a bool
  or a blanket `Nav`.
- Tests: one `Action` for tab activation, plus/new tab, bookmark open; one
  `Nav` for Circle/back; no cue for rejected/empty targets.
- QA: extend `qa_browser_grid.py` with a Cross-activation cue tally; keep the
  existing close tally.

### TR-1 — Shared shell transition + Launcher exit

- Files: `src/app/shell.rs` (`activate_shell_mode` 204-246,
  `switch_shell_mode` 249-269, `start_shell_enter` 311-333),
  `src/app/home.rs` (`return_to_home_screen` 407-425, `open_home_inner`
  478-535), `src/app/launcher.rs` (`retire_launcher` 4545-4571,
  `close_launcher` 4575-4582).
- Add a Launcher shell anim (or generalize to `ShellTransition`) so
  Launcher→Home starts an exit fade and defers retire/mode switch until the
  fade completes. Include Launcher in the "outgoing closing" deferral that
  currently checks only Home/Browser.
- Route PS dismiss, Circle-to-Home, and Home tile launches through the same
  begin/finish transition helpers where possible. Preserve reduced motion
  snapping and the existing home/browser exit capture.
- Test/QA: trace outgoing/incoming mode + duration; mid-transition screenshot
  shows both cards fading (not a hard cut).

### UI-1 — Home settings dropdown overlay

- Files: `src/ui/home/mod.rs` (`settings_view` 2092-2163,
  `settings_menu_panel` 2165-2185), `src/ui/launcher/settings.rs`
  (`settings_panel`/overlay 1184-1192, 1494, 1542, 1762).
- Render the Home dropdown in a `stack` overlay anchored below its row (or
  extract/reuse the Launcher panel widget). Opening/closing must not change row
  positions or the card height. Keep the existing option rows, focus, scroll
  into view, and mouse behavior.
- Test: geometry assertion that every row's rect is unchanged when a menu opens;
  visual QA screenshot baseline for both selectors.

### UI-2 — Shared backdrop click

- Files: `src/ui/home/mod.rs` (backdrop 1540-1553),
  `src/ui/browser/view.rs` (`root_view` 526-646),
  `src/ui/launcher/immersive.rs` (root 43-205),
  `src/app/home.rs` (`ps_screen_toggle` 259-325), `src/app/mod.rs` (Message
  enum/route).
- Add one shell-level message (e.g. `Message::ShellBackdropPressed`) handled by
  `ps_screen_toggle`. Put a full-screen outside-card `mouse_area` beneath the
  active card in Home, Home settings, Browser page, Browser tabs, and
  Launcher; click sends that message. Remove `HomeMessage::Close` from the Home
  backdrop so settings is not closed by an outside click.
- QA: synthetic `mouse.click` outside the card on Home, Home settings, Browser
  page, and Launcher; assert `screen.dismiss` with `source=backdrop`, no
  settings close, and the shell hides to desktop.

### REV-1 — Reveal first-frame gate

- Files: `src/app/shell.rs` (`on_shell_show_now` 39-73, `on_shell_present`
  101-131), `src/ui/layout.rs` (`show_shell_offscreen` 290-317,
  `show_shell_at_cover` 321-334), `src/ui/home/mod.rs` (`ShellAnim` 131-180).
- Replace the fixed 32 ms offscreen wait with an event-driven first-frame gate
  (wait for the offscreen window's first redraw/present, with a timeout
  fallback). Ensure the entrance pose is rendered before moving the HWND
  onscreen; apply the same rule to Home, Browser, and Launcher.
- Add a debug trace/counter for the first on-monitor pose (mode, elapsed,
  panel scale/alpha) so QA can fail a stale full-pose frame.
- Add/extend a replay QA that toggles PS open/close several times and asserts
  the first visible frame is entrance t≈0, not final pose. Preserve the 300 ms
  focus fallback.

### Landed notes

- **FB:** `home_activate` and `return_to_home_screen` emit one `Action`;
  `open_browser_tab` and `browser_grid_select` are `Action`; Circle/back stay
  `Nav`. Launcher top-level close no longer cues before the shared path.
  `retire_launcher(stop_haptics)` keeps the pulse alive on a Home swap and
  stops only when hiding to desktop. Debug haptic tally added.
- **TR-1:** `launcher_shell_anim` mirrors the Home/Browser `ShellAnim`;
  Launcher exit is a black veil (panel alpha) plus gated scale, and the mode
  switch/hide runs in `finish_launcher_close`. Shell switch deferral includes
  `launcher_closing`.
- **UI-1:** Home dropdown is a stack overlay positioned from the row geometry
  table (below, flipping above near the card bottom); rows and card height do
  not change when it opens.
- **UI-2:** `HomeMessage::Backdrop`, `BrowserMessage::Backdrop`, and
  `LauncherMessage::Backdrop` all call `screen_toggle("backdrop")`, which is
  the same path as PS. Home settings is no longer closed by an outside click.
- **REV-1:** `show_shell_offscreen` is followed by `ShellRevealArmed`; the
  first `RedrawRequested` for the shell calls `on_shell_present` (250 ms timer
  only as a lost-event fallback). Trace:
  `ui-diag: shell first present mode=… elapsed_ms=… scale=… alpha=…`.

## 5. Acceptance

- All symptoms in §1 are gone; the cue matrix in §3.1 holds for every shell
  commit and every rejected path is silent.
- Launcher→Home and Home→Launcher both show outgoing/incoming animation; no
  hard cut, no window create/destroy.
- Home settings dropdown opens/closes without moving any row or changing card
  height.
- Backdrop click behaves exactly like PS on every shell view and nested screen.
- Repeated PS open/close never presents the final pose before the entrance;
  trace/QA proves the first frame pose.
- Existing Browser/Launcher/Home/OSK QA still passes; no new global timer.

## 6. Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build
bash scripts/ci.sh          # once, after all edits
python scripts/qa_browser_grid.py
python scripts/qa_browser_grid_scroll.py
python scripts/qa_launcher_osk.py
python scripts/qa_browser_flows.py
python scripts/qa_osk_visual.py
# new/extended shell feedback/reveal QA
```

Use synthetic replay/offscreen only. Add a debug cue tally (existing
`App::ui_cue_count` at `src/app/feedback.rs:36`), a haptic tally, and a
first-present pose trace.

## 7. Risks / do-not-regress

- Preserve `NavBank` feel: MENU deadzone, 280/90 repeat, 100 ms neutral grace,
  per-pad arming, `PadActionGate` tap latch.
- Preserve PS release-only semantics and the shell topmost/foreground fallback.
- Preserve warm single HWND, one backdrop owner, shell cursor behavior.
- Preserve newest-first tabs, reserved scrollbar lane, FLIP/card shell, focus
  carry-over, Circle persistence.
- Do not add a second cue, a second backdrop owner, or a global frame timer.
- Do not stop haptics before the cue pulse finishes.

## 8. References

- `src/app/home.rs:1019` — Home tile activation (no cue).
- `src/app/home.rs:407` — Circle return to Home (no cue).
- `src/app/home.rs:778` — Home settings close (no cue).
- `src/app/launcher.rs:346` — Launcher close cue; `4545` retire stops rumble.
- `src/app/browser.rs:3319` — Cross activation; `2032` close cue; `2228`
  select cue.
- `src/app/launcher.rs:539` — tab open uses `Nav`.
- `src/app/shell.rs:39` — offscreen prime; `204` mode switch deferral.
- `src/ui/home/mod.rs:1540` — backdrop `HomeMessage::Close`;
  `2092` inline dropdown; `131` ShellAnim.
- `src/ui/browser/view.rs:526` — Browser root has no backdrop click.
- `src/ui/launcher/settings.rs:1184` — Launcher dropdown overlay geometry.
- `src/ui/layout.rs:290` — offscreen prime/show cover.
