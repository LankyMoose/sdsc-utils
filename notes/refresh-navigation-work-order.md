# Work order: per-window refresh + shared navigation

**Status:** implemented on `browser/main` — WO-1 through WO-7 landed;
WO-7 sub-items explicitly deferred; WO-8 final measurements captured
2026-11-26.
**Baseline:** `fba7a00` (Dual QWERTY OSK, full-width overlay card, OSK pixel QA).
**Scope:** iced multi-window redraw amplification, pull-model OSK/toast/animation clocks,
pad-input transport, and one shared navigation profile for every controller surface.
**Related:** [controller-os-handoff.md](controller-os-handoff.md),
[windows-toast.md](windows-toast.md), [ui-refresh.md](ui-refresh.md),
[launcher-immersive.md](launcher-immersive.md), [browser-embedding.md](browser-embedding.md).

> **Follow-up:** WO-1 landed the shared mechanics. The surface policy layer
> then landed in `src/domain/navigation.rs` (`NavList`, `MoveOutcome`/
> `ActionOutcome`, `NavDispatcher`) across Browser tabs/bookmarks, Home
> tiles/apps menu/dock/settings, and Launcher selector menus. Launcher's
> Games-strip bank and the OSK pull surface still use their existing
> `NavBank`/`NavStepper` mechanics, with no behavior change. See
> [navigation-dispatcher-work-order.md](navigation-dispatcher-work-order.md).

## 1. Why this work order exists

The OSK and toast are topmost windows, but iced 0.14 has no per-window update
or redraw isolation:

- After **any** message, `iced_winit` rebuilds every window's `UserInterface`
  and calls `request_redraw()` for **every** window.
- A per-window animation driven by a global subscription therefore wakes the
  Browser, Launcher, Home, Settings, popup, and OSK.
- `window::frames()` is avoided for exactly this reason
  (`UI_TICK` comment, iced#3108 / #3320), but `time::every` has the same
  redraw-all effect. It fixed present starvation, not redraw amplification.

The reference implementation for the high-frequency half is
`C:\repos\valorant-companion\src\video_frame.rs` (`PreviewProgram`): a shader
widget polls a shared slot and returns `Action::request_redraw_at`, so frames
never become iced messages. Its own comment records the same finding: every
message rebuilds and presents both windows. Our `BrowserFrameProgram` already
uses this pattern for CEF frames.

### Confirmed hotspots

1. **OSK input is a 125–250Hz global message source.**
   With a pad connected and Browser/Launcher/Home visible, `pad_listening` is
   true, the HID worker publishes a sample every 4ms wired, and
   `pad_input_edge_stream` forwards every edge. Every `Message::PadInput`
   redraws all windows. `on_osk_pad` additionally calls
   `invalidate_window(osk)` unconditionally, which issues
   `RedrawWindow(RDW_INVALIDATE | RDW_ERASE | RDW_FRAME | RDW_ALLCHILDREN)`
   even for neutral/no-op samples. The DQ cursor only changes on discrete
   steps and trigger edges.
2. **Toast ticks for its entire lifetime.**
   `ToastFrame` is subscribed while `toast_message.is_some()` or
   `toast_machine.wants_frames()`. `wants_frames()` is true in `Resting`, so a
   static 5s toast produces ~300 global frames. Immersive Launcher/Home
   compositing redraws the full-screen host for all 5s.
3. **Idle Launcher is normally 60Hz.**
   `LauncherFrame` runs while `launcher_state.needs_frames()`; the bare idle
   arm returns true when `perf_throttle_immersive` is false (default), solely
   to advance `ambient_time` and ken-burns. It also stays on while chrome
   status/Steam-scan chrome is active, even when nothing is moving.
   `on_launcher_pad` additionally calls `tick_anim` on every 4ms pad edge.
4. **Browser has a global 60Hz tick while visible.**
   `BrowserFrame` runs unconditionally for cursor/scroll/mouse sync and debug
   shots; CEF frame presentation is already pull-model.
5. **Pure-redraw timers exist.**
   `RendererReloadTick` and `UpdateSpin` handlers are `Task::none()`; they only
   force a spinner redraw. `IdentifyFrame` and short Home/shell fades are
   similar (shorter-lived).

## 2. Locked decisions

1. **OSK = pull model.**
   A per-window canvas/custom widget owns analog polling, cursor highlight,
   and its own redraw clock. The app receives only discrete step/commit/layer/
   close messages, not a message per pad sample.
2. **OSK adopts the menu navigation gate.**
   One shared `NavProfile`/`NavStepper` path. Initial values match the menu
   profile: per-axis `0.9` + residual `0.15`, 280ms initial repeat, 90ms
   repeat, 100ms neutral grace. Tune the profile centrally; do not fork OSK
   thresholds. Expect a much heavier DQ cursor feel than the old light-touch
   soft-stick path.
3. **Feedback matches the shell.**
   - Navigate/step: `UiSoundKind::Nav` (sound + haptic).
   - Commit/click: `UiSoundKind::Action` for text, space, backspace, enter,
     layer, close.
   - Repeat steps follow the existing menu policy.
   - Set `haptic_pad_serial` from the source reading before playing the cue.
4. **All live pads.**
   Shared dual-nav bank; per-pad arming; first non-resting pad wins per tick
   and per half; trigger edges are consumed per pad. No single
   `preferred_reading` owner for OSK.
5. **Settings stays mouse/keyboard.**
   No pad navigation for `Configure` in this work order.
6. **Toast = pull model.**
   Animate a content offset inside a fixed window; no `ToastFrame` while
   resting; one publish at slide end; one-shot expiry.
7. **No process split.**
   The Browser/OSK remain in-process. Do not reintroduce the add-on process.

### Out of scope

- Pad navigation for Settings/Configure.
- Reintroducing the Browser add-on process or any inter-process OSK.
- Changing the menu deadzone/repeat feel in the same pass as the migration.
- Pull-modeling low-rate background work (presence, tray, update checks,
  running badge, art events).
- Predictive text, alternate layouts, or native OSK/touch input.

## 3. Target architecture

### 3.1 Message budget

Treat every `Message` as a global redraw. Rules:

- High-frequency, visual-only work must never be a global message.
- Discrete state changes (commit, show/dismiss, mode/layer, navigation step)
  may be messages; cap repeats at display rate or less.
- Low-rate background work (presence, tray, update checks, running badge)
  stays as-is.
- A globally ticking source prevents independent refresh for every other
  window. Pull-model OSK/toast only helps if Launcher/Browser/Toast do not
  keep emitting global ticks.

### 3.2 Shared navigation model

Move `NavStepper`/`PadNavBank` into one explicit API used by every surface:

```text
NavInput   { pad, stick: Left|Right|Combined, x, y, dpad }
NavProfile { deadzone, engage, release, repeat_delay, repeat_interval,
             neutral_grace, axis_lock, feedback }
NavStep    { direction, phase: First|Repeat|Release }
NavBank    { per-pad slots, arming on rest, first-step arbitration }
DualNav    { two independent NavStepper cursors + trigger edges }
```

Profiles:

| Profile | Used by | Notes |
|---|---|---|
| `Menu` | Launcher, Home, OSK DQ | Preserve current feel initially: per-axis 0.9 + residual 0.15; 280/90; 100ms neutral. OSK DQ applies it per stick. |
| `Dpad` | OSK Grid, Browser D-pad jumps | Digital; immediate press, repeat schedule. |
| `Cursor` | Browser cursor | Existing soft 0.12 proportional path; no per-step cue. |
| `Scroll` | Browser scroll | Existing soft 0.12 + axis lock; no per-step cue. |

Feedback policy:

- Step feedback is part of the step output, not scattered call sites.
- Haptic target comes from the `NavInput` pad, not a stale global.
- Repeat feedback is explicit (default: same as today until validated).
- Commits use `Action`; navigation uses `Nav`.

Do not change menu deadzones in the same step as migrating call sites. First
preserve behavior, then tune in one place.

### 3.3 Pad transport

Split the current edge stream into two semantics:

- **Actions** (`PadAction`/tap edges): publish immediately; never delayed by
  analog throttling.
- **Analog state**: publish at display rate and/or on meaningful change.

The current 250Hz neutral stream is not redundant yet:

- Browser cursor friction/glide decays on neutral samples.
- `NavStepper` uses neutral samples for release/hysteresis.
- D-pad fly animations advance on pad ticks.

Therefore, before throttling/dropping neutral samples:

- Move Browser cursor/scroll integration onto a Browser clock (or pull
  widget).
- Ensure a short neutral tail / release edge reaches the stepper.
- Ensure D-pad fly completion advances without a pad stream.

For the OSK, do not send analog state through `Message::PadInput` at all;
write it to a shared `OskInputSlot` that the pull-model OSK widget polls.

### 3.4 OSK pull model

- Keyboard surface: canvas/custom widget drawing keys, labels, layer, active
  highlight, and footer hints.
- The widget owns the per-window redraw clock and reads the shared input slot
  (all live pads).
- App messages only for:
  - `OskStep` (if step feedback needs the app's sound/haptic service),
  - text commit / space / backspace / enter / layer / close,
  - target/lifecycle changes.
- Preserve `OskTarget` text routing, URL editing, page IME handoff, Grid
  fallback, topmost no-activate behavior, close/cancel semantics, and the
  visual QA hooks.
- Remove the unconditional per-edge `invalidate_window`; only lifecycle or a
  real content change invalidates.

### 3.5 Toast pull model

- Fixed toast window; the card slides via content offset, not per-frame window
  moves.
- Wall-clock animation; no `ToastFrame` while resting.
- One message at slide end (for `OpenHome`/`RaiseInteractive`/handoff), plus
  the existing one-shot expiry.
- Keep the immersive Launcher/Home composite path and z-order rules.
- Do not raise the toast per frame; event-driven raises only.

### 3.6 Other pull-model candidates

In priority order:

1. **Launcher ambient + ken-burns** — `AmbientProgram` and `BackdropArt`
   compute from a wall-clock/start instant and request their own redraws.
   Keep `LauncherFrame` for real state animations. Remove `tick_anim` from
   per-pad edges. Note: the full-screen Launcher still composites at 60Hz over
   a game; `perf_throttle_immersive`/idle pause remains the game-performance
   lever.
2. **Browser cursor/scroll** — gate `BrowserFrame` on
   `browser.needs_frames()` (velocity, fly-to, dock blend, pending scroll,
   debug shots); CEF stays pull. Longer term move cursor/scroll integration
   onto a per-window clock.
3. **Spinner-only ticks** — `RendererReloadTick` and `UpdateSpin` are pure
   redraws; replace with a spinner widget using `request_redraw_at`.
4. **Identify/entrance** — `IdentifyFrame` ring flash + popup entrance; pure
   visual clock, short-lived.
5. **Home/Launcher short animations** — `HomeControllerTick`,
   `HomeProgressTick`, `ShellFadeTick`, face-press/hold meters. Pull if pure
   visual; gate otherwise.
6. **Utility ambient** — `UtilityAmbientTick` at 2fps; shader wall-clock could
   remove the subscription. Low priority.
7. **Pad stream** — the biggest source; covered above.

## 4. Work items

### WO-1 — Shared navigation service

- [x] Introduce `NavInput`/`NavProfile`/`NavStep`/`NavBank`/`DualNav` in the
      pad domain layer.
- [x] Preserve current menu values; express DQ per-stick use of the `Menu`
      profile.
- [x] Migrate Launcher (`PadNavBank`), Home (single stepper), Browser D-pad,
      OSK Grid, and OSK DQ to the shared path.
- [x] Centralize step/commit feedback and per-pad haptic targeting.
- [x] Unit tests: deadzone/engage/release, diagonal dominance, repeat cadence,
      arming, multi-pad arbitration, trigger edges.

### WO-2 — Pad transport split

- [x] Publish tap/action edges immediately.
- [x] Change-gate/throttle analog state at display rate.
- [x] Add `OskInputSlot` fed by all live pads while the OSK is open.
- [x] Preserve neutral release semantics and Browser cursor glide (moved to
      a Browser clock before reducing neutral samples).
- [x] Trace/diagnose action vs analog delivery separately (`pad.action`
      carries `kind=action`; analog publishes are display-rate `pad_input`
      counters).

### WO-3 — Pull-model OSK

- [x] Canvas/custom widget keyboard with own redraw clock (`OskSurface`).
- [x] Poll `OskInputSlot`; run `DualNav` + trigger latch; publish only
      step/commit/action messages.
- [x] Route DQ text/space/backspace/enter/layer/close through the existing
      `apply_osk_*` paths.
- [x] Keep Grid fallback and all `OskTarget` hosts working.
- [x] Remove per-edge `invalidate_window`; verify no OSK repaint at rest.
- [x] Visual parity: full-width card, both halves, both cursors, footer hints,
      feedback-on-input.

### WO-4 — Pull-model toast

- [x] Fixed window + content-offset wall-clock slide.
- [x] No `ToastFrame` while `Resting`; one slide-end message.
- [x] Keep 5s expiry one-shot; keep queue/handoff/remount behavior.
- [x] Keep immersive composite and z-order rules.
- [x] Add a resting no-redraw QA assertion (`qa_idle_redraw.py --scenario toast`).

### WO-5 — Launcher ambient/ken-burns pull

- [x] `AmbientProgram` wall-clock time / self-redraw.
- [x] `BackdropArt` draw-time ken-burns pose / self-redraw.
- [x] `LauncherFrame` only for real state animations; no bare idle arm.
- [x] Stop ticking Launcher solely for a static chrome-status chip/scan
      (frames now run only while a chip is actually animating; the scan banner
      clears).
- [x] Remove `tick_anim` from per-pad edges.
- [x] Preserve Home pause/clock-shift and reduced-motion/time-scale behavior.
- [x] Keep `perf_throttle_immersive` semantics for game-overlay scenarios
      (ambient/ken-burns freeze under throttle when no real animation runs).

### WO-6 — Browser gating/pull

- [x] Add `browser.needs_frames()` and gate `BrowserFrame`.
- [x] Keep CEF frame presentation pull-model.
- [x] Verify cursor glide, docking, D-pad fly, scroll, URL editor, and debug
      shots still work.
- [x] Longer-term: move cursor/scroll decay onto the Browser wall clock
      (pad edges still supply the input sample; neutral tails are optional).

### WO-7 — Remaining pull-model clocks

- [x] Spinner widgets for `RendererReloadTick` / `UpdateSpin`.
- [ ] **Deferred:** Identify ring / popup entrance pull clock. The existing
      `IdentifyFrame` clock is transient (connect/identify only, never idle)
      and gates both popup + Home together; pulling it needs a per-window
      flash clock and is outside this pass's measured idle scenarios.
- [x] Home controller/uninstall/shell fade gated (`HomeProgressTick` no
      longer runs while Home is hidden; controller/shell fades stay bounded).
- [ ] **Deferred:** Utility ambient shader clock (low priority, ≤2fps while a
      utility window is visible; not part of the idle redraw budget).

### WO-8 — QA/docs

- [x] Add idle-redraw instrumentation (debug counters of view rebuilds,
      actual per-window redraws, and message sources).
- [x] Add an automated “OSK open + pad at rest produces no continuous global
      traffic/redraws” check (message counters + actual `redraw.osk`; synthetic
      pad-at-rest covered by `InputHub`/`osk_stream_wake` unit tests).
- [x] Add a “toast resting produces no frames” check.
- [x] Add a “Launcher idle produces no LauncherFrame when ambient is pull”
      check.
- [x] Re-run existing OSK visual/functional, toast, Launcher, Browser, and
      CI suites.
- [x] Update [controller-os-handoff.md](controller-os-handoff.md),
      [windows-toast.md](windows-toast.md), [ui-refresh.md](ui-refresh.md),
      [tracing.md](tracing.md), and `AGENTS.md` quick commands/notes.

## 5. Acceptance criteria

- OSK open with an idle controller: no per-sample global messages, no OSK
  repaint when nothing changed; Browser/Launcher stay at their own rate.
- OSK navigation: both halves, all live pads, menu gate, Nav/Action feedback
  on the correct pad.
- Toast resting: no `ToastFrame`; slide only during placement/slide; expiry
  and queue behavior unchanged.
- Launcher idle: no global 60Hz tick from ambient/ken-burns; state animations
  still animate.
- Browser idle: no global `BrowserFrame` tick; CEF frames still present.
- Pure-redraw timers replaced by widget clocks (spinners, identify, etc.).
- Existing QA and `bash scripts/ci.sh` pass; no toast z-order/present
  starvation regression; no OSK text/IME/URL regression.

## 6. Risks / do-not-regress

- iced’s redraw-all is not configurable; any remaining global tick defeats
  independent refresh. Audit subscriptions after each work item.
- The OSK canvas rewrite must preserve visual QA, text routing, URL edit,
  page IME handoff, Circle/Escape semantics, no-activate/topmost behavior,
  and Grid fallback.
- Keep toast z-order/remount rules; do not raise per frame; do not put the
  toast on another thread.
- Do not change menu nav feel while extracting the profile; tune after
  parity.
- Preserve Home pause/clock-shift, reduced motion, debug tuning, and
  `perf_throttle_immersive`.
- All-live-pads needs explicit arbitration; first step per half per tick, no
  double text from duplicate pads.
- Haptics must target the pad that produced the step/commit.

## 7. Reference

- iced 0.14 `iced_winit` event loop: after update, rebuild all windows and
  request redraw for all.
- `C:\repos\valorant-companion\src\video_frame.rs` — `PreviewProgram`
  pull-model, shared slot, `request_redraw_at`, no per-frame messages.
- `src/ui/launcher/browser_frame.rs` — local `BrowserFrameProgram` pull-model.
- `src/domain/pad.rs` — `NavStepper`, `PadNavBank`, `combined_stick`,
  `soft_stick`, `preferred_reading`, `dpad_only`.
- `src/app/osk.rs`, `src/ui/osk/dual.rs` — current DQ implementation to be
  refactored.
- `src/app/toast.rs`, `src/ui/toast/machine.rs` — current toast state machine.
- `src/ui/launcher/view.rs` — `needs_frames`, `tick_anim`, ambient/ken-burns
  state.
- `src/ui/shader/ambient.rs`, `src/ui/launcher/translate.rs` — ambient/ken-burns
  pull candidates.
