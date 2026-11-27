# Work order: navigation dispatcher + feedback ownership

**Status:** NAV-1..4 landed 2026-11-28; NAV-5 dispatcher extraction
deferred under the cut line; NAV-6 QA/docs landed.
**Baseline:** `77b5a42` (newest-first tabs, reserved scrollbar lane, shell
foreground/topmost fallback).
**Scope:** the policy layer above `NavBank`/`NavStepper`: list focus validity,
wrap policy, action validity, single-cue ownership, held-analog sampling, and
the surfaces that currently own their own navigation reducers (Browser
tabs/bookmarks, Home apps, Launcher selectors, OSK).
**Related:** [refresh-navigation-work-order.md](refresh-navigation-work-order.md),
[browser-tab-grid.md](browser-tab-grid.md),
[browser-embedding.md](browser-embedding.md),
[launcher-immersive.md](launcher-immersive.md),
[pull-model.md](pull-model.md).

## 1. Why this work order exists

`NavBank`/`NavStepper` already centralize the low-level input mechanics:
per-pad arming, neutral grace, deadzone shaping, repeat cadence, and diagonal
dominance. They stop at a raw `NavAction`. They do not answer the questions
that actually decide behavior:

- Can this surface move in this direction?
- Should an input at the edge move focus or be rejected?
- Is wrapping allowed?
- Did the action change state, or was it a no-op?
- Should this input produce sound/haptics?
- How is a held analog sample delivered to the surface?

Each surface answers those separately, and cues are fired at call sites in
both the surface reducer and the domain operation. Four user-visible failures
fall out of that split:

1. Closing the active tab plays two close cues.
2. Navigation at a list boundary still plays a cue for a rejected move.
3. Holding the left stick does not repeat in the Browser tabs/bookmarks.
4. Home lists auto-wrap.

The existing [refresh-navigation-work-order.md](refresh-navigation-work-order.md)
WO-1 landed the shared mechanics and described "feedback policy" as part of the
step output. In practice the policy layer was never extracted; the surfaces
still own it. This work order is the follow-up, not a redo of WO-1.

## 2. Evidence

Line numbers are from baseline `77b5a42`.

### 2.1 Double close cue — the direct cause

There are two independent `Action` cues on the same close:

- `src/app/browser.rs:2077` — `browser_grid_close_tab` plays
  `UiSoundKind::Action` for every non-final close.
- `src/app/launcher.rs:579` — `close_browser_tab`, **active-tab branch**,
  plays `UiSoundKind::Action` again.

`browser_grid_close_tab` is the only caller of `close_browser_tab`
(`src/app/browser.rs:2079`).

| Close | Grid cue | Domain cue | Result |
|---|---|---|---|
| Background tab | `Action` | none | 1 |
| Active, non-final tab | `Action` | `Action` | **2** |
| Final remaining tab | skipped | `Nav` via `on_browser_go_default` | 1 |

With the newest-first strip the grid opens focused on the active tab, so
pressing Square immediately is the active-tab path — the common case. An
earlier synthetic test focused a background tab, which is why it logged one cue
and missed the duplicate. The `PadActionGate` tap latch remains useful for
duplicate release delivery, but it is not this bug. This is **cue ownership**,
not input dedupe.

The `cue: bool` parameter on `close_browser_grid_with` is the same smell: the
caller has to remember which layer already sounded.

### 2.2 Cues for inputs with no valid action

`src/app/browser.rs:3078-3080` cues `Nav` **before** calling
`browser_grid_move_focus`. That function returns `Task::none()` with no outcome
at `src/app/browser.rs:2278` when `next == focus`. Therefore a cue plays for:

- Left on the plus card,
- Right on the last tab,
- Up from the card row (no row above),
- Down with no bookmarks,
- Left/Right at the ends of the bookmark strip.

A held D-pad at a boundary keeps producing `NavBank` repeat steps, so it can
tick repeatedly while focus never moves.

The same class appears in Home:

- `src/app/home.rs:652-655` and `692-695` call `apps_menu_move(delta)` and then
  play `Nav` unconditionally. `apps_menu_move` returns early when the action
  list is empty (`src/ui/home/mod.rs:1192-1197`), so an empty menu still cues.
- `src/app/home.rs:671-679` cues for Info scroll even at the top/bottom.
- `HomeMessage::AppsMenuMove` (`src/app/home.rs:724`) takes the same move but
  does not cue; two routes, two policies.

Already correct: Square on Plus, Triangle on Plus, and Create in the tabs view
return `none` without a cue; bookmark removal only cues when a bookmark was
actually removed.

### 2.3 Held analog is dropped by the Browser edge filter

The Browser enables `browser_listening` whenever it is visible
(`src/app/mod.rs:1713-1718`). The edge bridge then applies
`browser_stream_wake` (`src/app/mod.rs:3665-3695`), which wakes on:

- a non-sample reason,
- a tap action,
- PS held,
- a digital control,
- a digital state change,
- `edge.analog_changed`,
- a stick state change (`stick != state.stick`).

A steadily held stick is `stick = true`, `state.stick = true`, no digital
change, no action, no `analog_changed` after the first frame, so the frame is
dropped. `on_browser_grid_pad` ticks `browser.grid_nav` only on `PadInput`
edges (`src/app/browser.rs:3056-3067`). `NavStepper` repeat is wall-clock, but
it is only evaluated when `step` is called (`src/domain/pad.rs:1352`). No
ticks means no repeat.

Result: the first stick deflection moves one card; holding it does nothing.
The D-pad works because every held D-pad frame is digital and wakes the
filter. The same edge path feeds the bookmark strip's left/right focus and the
right-stick scroll (`browser_grid_scroll_from_pad`, `src/app/browser.rs:2324`),
so a held right stick also stops scrolling.

| Surface | Analog source | Held analog works? |
|---|---|---|
| Home | unfiltered `PadInput` edges | yes |
| Launcher | unfiltered `PadInput` edges | yes |
| Browser page cursor | pull snapshot (`CursorSurface`) | yes |
| OSK | pull snapshot (`OskInputSlot`/`OskSurface`) | yes |
| Browser tabs/bookmarks | filtered `PadInput` edges | **no** |
| Tabs/bookmarks right-stick scroll | filtered `PadInput` edges | **no** |

This is a transport-contract violation: the filter assumes every analog
consumer pulls, but the tabs view was added later as an edge consumer.

### 2.4 Auto-wrapping

`rem_euclid` marks every wrapping list. Current sites:

| List | Code | Test that locks it in |
|---|---|---|
| Home tiles (`Launcher`/`Browser`) | `src/ui/home/mod.rs:599-604` | `selection_wraps_both_ways` (`~2789`) |
| Home controller dock | `src/ui/home/mod.rs:633` | controller selection tests |
| Home settings rows | `src/ui/home/mod.rs:1004` | `settings_move` tests |
| Home settings dropdown | `src/ui/home/mod.rs:1073` | dropdown tests |
| Home app actions (`Install`/`Info`) | `src/ui/home/mod.rs:1192-1197` | `apps_menu_focus_wraps_over_actions` (`~3086`) |
| Launcher settings selector | `src/app/launcher.rs:3510,3516` | selector menu tests |

The Launcher Games strip is already linear (`src/app/launcher.rs:2341`). Home
is the outlier, and the tests currently assert the old policy. "Home apps"
most likely means the tile row and the app-actions menu; the rule says **no
navigation list**, so the dock/settings/dropdown/selector lists are in scope
too.

### 2.5 Surface inventory

| Surface | Repeat engine | Input route | Wrap | Cue owner |
|---|---|---|---|---|
| Home tiles/apps menu | `home_view::State::nav` (`NavStepper`) | unfiltered `PadInput` → `pad_tick` → `HomeMessage` | yes | `on_home_message` |
| Home dock/settings | same stepper / direct messages | same | yes | `on_home_message` |
| Launcher | `App::pad_nav` (`NavBank`) | unfiltered `PadInput` | selector menu yes; Games no | launcher reducers |
| Browser page | `browser.nav` + `dpad_stepper` | edges | n/a | page reducers |
| Browser tabs | `browser.grid_nav` (`NavBank`) | filtered edges | no, but no validity signal | `on_browser_grid_pad` before move |
| Browser bookmarks | same as tabs | filtered edges | no, but no validity signal | same |
| OSK | `NavBank` in pull surface | snapshot | n/a | OSK surface |

### 2.6 How we arrived here

- The shared abstraction was built for **mechanics** (arming, repeat, neutral,
  profile), not for **surface policy** (validity, wrap, cue, focus graph).
- Home, Launcher, Browser page, OSK, and then tabs/bookmarks each added a
  reducer and copied the pattern. Each one owns its own focus state and decides
  feedback at the call site.
- The Browser introduced `browser_listening` as a latency/performance
  optimization for the pull-model cursor. The tabs view later reused the edge
  stream and silently inherited the filter.
- Cues were added incrementally as side effects. `close_browser_tab` and the
  new `browser_grid_close_tab` both sounded; the `cue: bool` flag was a
  workaround, not a contract.
- Home wrapping is old behavior encoded in `rem_euclid` and tests. The rule
  changed; the model and tests did not.

## 3. Locked rules

These are the rules this work order exists to encode in types and tests, not
prose:

1. **No navigation list wraps.** Default `Wrap::Never`; clamp at the ends.
   Applies to Home tiles, Home app actions, the controller dock, settings
   rows/dropdowns, Launcher selector menus, and every future list. The Games
   strip already follows this.
2. **Rejected input is silent.** If a direction cannot move focus, or an
   action cannot run, consume the input with no sound and no haptic.
3. **One accepted move/action, one cue.** Exactly one `play_ui_cue`, with the
   pad that produced the input.
4. **Cue is an outcome, not a call-site side effect.** Domain operations
   (`close_browser_tab`, `navigate_browser_to`, bookmark mutation, etc.) are
   silent state transitions. The dispatcher/surface returns the cue.
5. **Held analog is sampled, not inferred from edges.** The active surface
   ticks a `NavStepper` from the latest `InputHub` snapshot on its frame clock.
   Edges remain for taps and PS. `browser_listening` may keep filtering the
   cursor path; it must not starve list navigation.
6. **One dispatcher per active surface.** Per-surface `NavStepper`/`NavBank`
   fields are implementation details behind the dispatcher, not separately
   coordinated.
7. **Action validity is explicit.** `try_action` returns accepted/rejected;
   rejected actions produce no cue and no fallthrough.
8. **The rules live in tests.** No-wrap boundaries, no-cue rejection, exactly
   one close cue, held-stick repeat, and duplicate-tap delivery.

### Out of scope

- Pad navigation for Configure/Settings windows.
- Changing menu deadzone/repeat feel while extracting the policy layer.
- Browser process split / IPC, CEF cursor, or the tabs FLIP/card-shell visuals.
- Rewriting the tap interpreter or PS release detection; those layers stay.

## 4. Target architecture

### 4.1 Types

```text
NavList   { id, focus, len, axis, wrap: Wrap::Never, enabled }
NavSurface {
    lists: focus graph (row -> bookmarks -> ...),
    focused: NavListId,
    try_move(dir) -> MoveOutcome,
    try_action(button) -> ActionOutcome,
    scroll(delta) -> Option<ScrollEffect>,
}
MoveOutcome   { Moved { cue }, Rejected }
ActionOutcome { Accepted { cue }, Rejected }
NavEffect     { focus, scroll, cue, task }
NavDispatcher { per-pad NavBank, arming, active surface, snapshot tick }
```

The dispatcher owns:

- per-pad bank/arming and the existing menu profile,
- snapshot/frame ticking for held analog,
- move/action validity and wrap policy,
- the single `play_ui_cue` call and haptic pad selection,
- converting outcomes into app tasks/scroll operations.

Surfaces provide list state and state mutations. They do **not** call
`play_ui_cue`, and they do not inspect raw `NavAction` to decide feedback.

### 4.2 Tick and transport

- The active surface’s frame/tick calls `dispatcher.tick(surface, snapshot,
  now)`. Home uses its frame clock; OSK keeps its pull surface.
- The Browser tabs view has **no idle tick** today. `GridTicker`
  (`src/ui/browser/grid.rs:1619`) only requests frames while `active` (a
  FLIP/transition is live), so it cannot drive a held stick. Add a grid tick
  message requested from the grid view while a nav input is non-neutral, and
  let the app read the snapshot there.
- Taps/PS still arrive as edges. `PadActionGate` remains the edge-level
  dedupe/latch; the dispatcher asks `try_action` once per accepted tap.
- `browser_listening` remains a cursor optimization. List surfaces read the
  snapshot directly, so a held stick repeats regardless of edge filtering.
- Scroll effects are returned with the move; a held right stick can scroll
  from the same frame tick instead of depending on edge wake-ups.

### 4.3 Focus graph and wrap

- The tabs surface has two lists: the horizontal tab row (plus + tabs) and the
  bookmark strip. `Down`/`Up` are graph edges; `Left`/`Right` are list moves.
- `Wrap::Never` is the default. A surface must not call `rem_euclid`; a list
  reports `Rejected` at the bounds.
- Cues are emitted only for `Moved`/`Accepted`.

### 4.4 Cue ownership

- `close_browser_tab` returns an outcome (`Closed { next }`,
  `ResetDefault`, `Rejected`); it never cues.
- The close action’s dispatcher emits `Action` for a real close; the final-tab
  reset emits `Nav` only for the navigation it performs. No `cue: bool`.
- A future caller cannot accidentally double-sound because there is no cue
  call to forget.

## 5. Work items

### NAV-1 — Stop the current double close cue

- [x] Remove the cue from `close_browser_tab` (or from the grid close); keep
      exactly one. The grid close is currently the only caller, so the domain
      cue is redundant.
- [x] Make the close path return an outcome instead of relying on `cue: bool`.
- [x] Add a regression test that an active-tab close plays one `Action`, a
      background close plays one `Action`, and a final-tab reset plays one
      `Nav`.
- [x] Add a synthetic replay/QA close of the **active** tab (the previous QA
      only closed a background tab).

### NAV-2 — Outcome-based move cues

- [x] Change `browser_grid_move_focus` (and the list move below it) to return
      moved/rejected.
- [x] Move the `Nav` cue after a successful move, at one place.
- [x] Make `apps_menu_move` return moved/rejected; cue only on success; remove
      the unconditional Home cues at `src/app/home.rs:652-655`/`692-695`.
- [x] Make Info scroll report whether it scrolled; no cue at the ends.
- [x] Tests: Left on Plus, Right on last tab, Up from the row, Down with no
      bookmarks, bookmark edges, empty apps menu all produce **no cue**.

### NAV-3 — Held analog for tabs/bookmarks

- [x] Tick `browser.grid_nav` from a surface frame clock with the latest
      `InputHub` snapshot, not only `PadInput` edges. Do **not** widen the
      edge filter to compensate (see §7).
- [x] Feed the tabs/bookmark right-stick scroll from the same frame tick.
- [x] Keep the existing menu profile/repeat cadence.
- [x] Tests: a held stick produces First + Repeat steps; a release stops;
      a held right stick scrolls; D-pad behavior is unchanged.
- [x] QA: tabs/bookmarks held-stick repeat (left-stick repeat + release in
      `qa_browser_grid_scroll.py`); right-stick scroll is unit-tested.

### NAV-4 — No-wrap Home/Launcher lists

- [x] Replace `rem_euclid` with clamping in Home tiles, app actions, dock,
      settings rows, and settings dropdown.
- [x] Clamp the Launcher selector menu.
- [x] Update `selection_wraps_both_ways`,
      `apps_menu_focus_wraps_over_actions`, and the settings/dropdown tests to
      assert edge rejection.
- [x] Ensure a rejected edge does not cue.
- [x] Keep the Games strip linear (no behavior change).

### NAV-5 — Extract the dispatcher

**Deferred (cut line).** No surface was migrated, so there is no half-migrated
state to unwind. NAV-2/3 left the Browser and Home on their existing per-pad
banks with outcome-based reducers; re-open this item to add `NavList` /
`NavSurface` / `NavDispatcher` and migrate both surfaces together.

- [ ] Add `NavList`, `NavSurface`, `MoveOutcome`, `ActionOutcome`, and
      `NavDispatcher` under the pad/navigation domain layer.
- [ ] Migrate Browser tabs/bookmarks first: delete the per-surface move logic
      and `browser.grid_nav` orchestration; the surface exposes list state and
      `try_move`/`try_action`.
- [ ] Migrate Home apps (tiles + app actions) next; then Home dock/settings.
- [ ] Migrate Launcher and OSK after parity; remove per-surface
      `NavStepper`/`NavBank` fields.
- [ ] Keep `PadActionGate` at the input boundary for tap dedupe/latch; the
      dispatcher never re-implements edge dedupe.
- [ ] Remove `cue: bool`-style parameters and direct `play_ui_cue` calls from
      navigation/action reducers.

### NAV-6 — QA/docs

- [x] Unit-test the landed surface rules: no wrap, no cue on rejected, one cue
      on accepted close, held-stick repeat, duplicate-tap drop. Dispatcher-level
      tests travel with NAV-5.
- [x] Extend `qa_browser_grid.py`/`qa_browser_grid_scroll.py` with an
      active-tab close cue check (debug counter) and held-stick QA.
- [ ] Update `browser-tab-grid.md`, `browser-embedding.md`,
      `launcher-immersive.md`, `refresh-navigation-work-order.md`, and
      `AGENTS.md` once the dispatcher lands. `browser-tab-grid.md` and
      `AGENTS.md` are updated for the landed behavior; the other notes wait for
      NAV-5.
- [x] Run `bash scripts/ci.sh` and the Browser QA suites
      (`qa_browser_grid.py`, `qa_browser_grid_scroll.py`).

## 6. Acceptance criteria

- Closing an active tab, a background tab, or the final tab each emits exactly
  one cue; closing cannot double-sound.
- Rejected directions/actions emit no sound/haptic and change no focus or
  state.
- Holding the left stick repeats through tabs and bookmarks; releasing stops;
  a held right stick scrolls the row/bookmark strip; the same works with the
  D-pad.
- Home tiles, Home app actions, the dock, settings/dropdowns, and Launcher
  selectors clamp at their ends; no list auto-wraps.
- Navigation surfaces do not call `play_ui_cue` directly; domain operations
  are silent; the dispatcher is the one cue choke point.
- The shared dispatcher is used by tabs/bookmarks and Home apps; Launcher/OSK
  can follow without changing their feel.
- Existing Browser FLIP/card-shell, newest-first strip, scrollbar lane,
  persistence, tap latch, PS release, and shell focus behavior still pass
  their QA.

## 7. Risks / do-not-regress

- Do not fix held analog by flooding the edge stream. Prefer the snapshot tick;
  `browser_listening` exists for cursor latency.
- Preserve `NavStepper` menu feel: deadzone, engage/release, 280/90 repeat,
  100ms neutral grace, axis lock, per-pad arming.
- Preserve source-side tap delivery, `PadActionGate` sequencing/latch,
  `pending_pad_actions`, and one-tap-per-frame behavior.
- Preserve PS release-only semantics and the shell topmost/foreground fallback.
- Preserve the tabs FLIP/card shell, newest-first order, reserved scrollbar
  lane, focus-index carry-over, and Circle persistence.
- Preserve Home/Launcher focus scroll, controller dock, settings holds, and
  reduced motion.
- Do not re-enable wrapping "because it feels console-like"; the rule is no
  wrap.
- Update tests deliberately: several currently assert the old wrap policy.

## 8. References

- `src/app/browser.rs:2077` — grid close cue.
- `src/app/launcher.rs:579` — active-tab close cue.
- `src/app/browser.rs:2278` — focus move no-op without an outcome.
- `src/app/browser.rs:3078-3080` — cue before the move.
- `src/app/home.rs:652-655,692-695,724` — Home apps-menu cue paths.
- `src/ui/home/mod.rs:599-604,633,1004,1073,1192-1197` — wrap sites.
- `src/app/launcher.rs:3510,3516` — selector menu wrap.
- `src/app/mod.rs:1713-1718,3665-3695` — `browser_listening` + edge filter.
- `src/app/browser.rs:3056-3067` — grid nav ticked on edges.
- `src/domain/pad.rs:1352` — repeat only evaluated on a tick.
- `src/app/browser.rs:2324` — tabs/bookmark right-stick scroll on edges.
- [refresh-navigation-work-order.md](refresh-navigation-work-order.md) — WO-1
  shared mechanics; this is the policy-layer follow-up.
