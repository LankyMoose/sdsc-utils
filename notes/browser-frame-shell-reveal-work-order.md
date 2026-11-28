# Work order: browser frame cadence + shell reveal presentation

**Status:** open. Investigation complete 2026-11-28. No source changes yet.
**Baseline:** `60c943a` (`browser/main`).
**Scope:** BF-1 (CEF frame wake) and RV-2 (shell reveal presentation gate),
plus the QA that must actually prove both. Phase 3 browser security, ad
filtering, tab-state refactors, and unrelated shell feedback are out of scope.
**Related:** [pull-model.md](pull-model.md),
[shell-feedback-transition-work-order.md](shell-feedback-transition-work-order.md),
[browser-embedding.md](browser-embedding.md),
[tracing.md](tracing.md).

## 1. Symptoms

1. Browser page animation/video drops to a few fps when the pad cursor is
   idle. Cursor movement restores smoothness, which masks the frame-delivery
   bug.
2. A PS open sometimes presents the previous settled shell frame for one
   frame, then resets and plays the entrance. The existing REV-1 trace and
   `qa_browser_grid.py` pass because they assert handler-computed values, not
   the presented frame.

## 2. Findings (investigation, do not re-derive)

### BF: CEF paints do not wake the UI

- `SdscRenderHandler::on_paint` only writes `FrameSlot`
  (`src/browser/cef_backend.rs:867-905`). It never emits an `EngineEvent`.
- `EngineEvent::Frame` exists but production code never constructs it. Its UI
  branch (`src/app/launcher.rs:1488`) is dead.
- `BrowserFrameProgram::update` only notices a newer slot generation when a
  redraw already happened (`src/ui/launcher/browser_frame.rs:420-433`). Its
  `FrameDemand::Now` self-request is not a wake source; one phase miss parks
  the page until the next unrelated message.
- While visible, `ShellCursorTick` fires every 500 ms
  (`src/app/mod.rs:1807-1810`). Every message redraws every window, so an idle
  page effectively presents at ~2-3 Hz. Cursor motion masks it because
  `CursorSurface` requests vsync redraws (`src/ui/browser/cursor_surface.rs:119-138`).
- Measured: `qa_idle_redraw.py --scenario browser` (`window_s=2.001`) showed
  `view.browser=5`, `redraw.browser=5`, `msg.browser_frame=0`, `msg.other=4`,
  `msg.tick=1`. The page clock is unrelated messages, not CEF paints.
- QA gap: `qa_idle_redraw.py:190-203` treats the nonexistent browser 60 Hz
  frame clock as accepted and only requires `msg.browser_frame == 0`; a
  frozen or 2 Hz page passes.

### RV: the reveal gate proves an event, not a presented entrance frame

- `on_shell_show_now` starts the entrance and primes offscreen
  (`src/app/shell.rs:42-85`).
- `on_shell_reveal_armed` restarts the entrance clock
  (`src/app/shell.rs:114-126`), so prime and gate use different clocks.
- `ShellRedraw` is not generation-tagged (`src/app/mod.rs:239-246`,
  `1701-1711`). A redraw queued before the arm can satisfy the new show under
  rapid PS.
- `on_shell_redraw` presents on any `RedrawRequested` while armed
  (`src/app/shell.rs:130-135`). iced broadcasts `RedrawRequested` before
  `present()` (`iced_winit-0.14.1 src/lib.rs:973-988`), so the event is not a
  composition guarantee.
- `on_shell_present` logs `anim.frame(now)` at message-handling time
  (`src/app/shell.rs:149-184`). That pose is not necessarily the pose in the
  pixels that were drawn, presented, or composited.
- `show_shell_offscreen` / `show_shell_at_cover` show and move the warm HWND,
  invalidating after the move (`src/ui/layout.rs:293-315`, `324-360`). The
  compositor can still hold the previous settled surface during the move.
- QA gap: `qa_browser_grid.py:476-502` only checks
  `source=redraw` and an alpha parsed from the handler log. It cannot detect a
  pre-arm redraw or a stale composited surface.

## 3. Locked rules

1. **No new global timers or global subscriptions.** CEF paints wake the UI
   event-driven. No 60 Hz `time::every`, no `window::frames()`.
2. **Keep `FrameSlot` as the pixel path.** Do not copy RGBA through messages
   again. The wake is a lightweight fact (`tab`, `generation`), not pixels.
3. **One reveal epoch.** Each show has one `shell_reveal_generation`. All
   reveal messages carry it and stale generations are ignored.
4. **The first visible frame is a frame drawn after that show's arm.** A
   redraw that was already queued/armed before the arm must not satisfy the
   gate.
5. **One entrance clock start per show.** Do not start the entrance in
   `on_shell_show_now` and restart it at arm. Start it once when the gate is
   armed; the offscreen prime may draw a non-entrance pose because it is not
   visible.
6. **The trace/QA must describe the drawn frame**, not `anim.frame(now)` in a
   later message handler.
7. Preserve: one shell HWND, 300 ms focus fallback, topmost/foreground
   behavior, reduced motion, the 500 ms cursor-hide tick, and the existing PS
   release semantics.
8. Smallest stable diff. No adjacent screen/animation refactors.

## 4. Work items

### BF-1 - CEF frame-ready wake

- Files: `src/browser/cef_backend.rs` (`on_paint`, `TabContext`),
  `src/browser/engine.rs` (`EngineEvent`), `src/app/launcher.rs`
  (`on_engine_event`), `src/ui/launcher/browser_frame.rs` (remove reliance on
  the self-request loop, keep the generation upload).
- Add `EngineEvent::FrameReady { tab: TabId, generation: u64 }` (or an
  equivalent lightweight control event).
- In `on_paint`, after `frame.write(...)`, emit `FrameReady` only when the tab
  is active and the shell is visible (the existing paint guard already checks
  this). Send at CEF paint cadence; do not send the buffer.
- In `on_engine_event`, for the active tab with an open Browser session,
  request `invalidate_browser_window()`. Ignore background/closed tabs.
- Keep the shader generation check for upload dedupe, but it must no longer be
  the only reason a redraw happens.
- Do not emit on `paint=popup` or for background tabs.
- Unit test: a slot write + `FrameReady` path requests an invalidation; a
  background tab does not.

### RV-2 - reveal epoch and drawn-frame gate

- Files: `src/app/shell.rs`, `src/app/mod.rs` (messages/subscription),
  `src/ui/home/mod.rs` (`ShellAnim`/`ShellFrame`), `src/ui/layout.rs`.
- Add the reveal generation to `ShellRedraw(id, generation)`; the raw listener
  captures the generation current at subscription time, and
  `on_shell_redraw` ignores mismatches.
- Remove `start_shell_enter` from `on_shell_show_now`. In
  `on_shell_reveal_armed`, reset the clock once and record
  `shell_reveal_arm_draw = current shell draw epoch`.
- Record a shell draw epoch and the `ShellFrame` pose actually used by
  `view_inner` for the shell (interior mutability on `App` is fine; this is
  debug/diagnostic state). Increment the epoch per shell view build.
- `on_shell_redraw` presents only when the recorded draw epoch is strictly
  greater than `shell_reveal_arm_draw`; otherwise ignore and wait for the next
  redraw (arm already invalidated). This rejects pre-arm redraws and stale
  generations.
- `on_shell_present` logs the recorded frame pose/epoch, not
  `anim.frame(now)`. The first-present log must include generation, draw epoch,
  scale, offset, and alpha.
- If the first on-monitor pixels still show the old surface after the epoch
  gate, add one Windows compositor flush/cloak step around
  `show_shell_at_cover` (e.g. `DwmFlush` on the window task after paint, before
  the cover move). Do not replace the gate with a timer.
- Keep Launcher's internal veil, Home/Browser entrance, and reduced-motion
  behavior. They remain subordinate to the one show epoch.
- Unit test: a pre-arm redraw/generation cannot present; a post-arm drawn frame
  can; a stale generation cannot present a newer show.

### QA-1 - prove the actual contracts

- Files: `scripts/qa_browser_grid.py`, `scripts/qa_idle_redraw.py`, plus a
  small new animated-page QA if needed.
- Browser: load a local page with `requestAnimationFrame` and a visible
  counter; with no pad/keyboard/mouse input, assert the Browser window
  presents/redraws at display rate for >= 1 s (e.g. `redraw.browser >= 45/s`)
  while `msg.browser_frame` is absent or only carries wake events. A static
  page must still idle.
- Shell: extend the PS stress block to assert every first-present line has
  `source=redraw`, the current reveal generation, and a draw epoch greater
  than the arm epoch. Fail on fallback, stale generation, or pre-arm draw.
- Add a first-visible-frame pixel check if cheap (window screenshot immediately
  after present or a monitor capture). If not automated, record the exact
  drawn-frame pose and require a manual visual confirmation of one PS cycle.
- QA remains offscreen/synthetic and must not drive OS input.

## 5. Acceptance

- Browser animation presents at display rate with no cursor movement or input,
  and the static idle page still does not spin.
- Repeated PS open/close never presents a settled frame before the entrance;
  every present is for a frame drawn after that show's arm and its generation
  matches.
- No new global timer, subscription, window, or IPC process. One shell HWND
  and one backdrop owner preserved.
- `cargo fmt --all -- --check`, debug/release clippy, all tests, debug/release
  builds, and the UI-only 1.89 MSRV check pass.
- Browser grid/flows/actions/popup QA and OSK/idle QA pass offscreen.

## 6. Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build
bash scripts/ci.sh
python scripts/qa_browser_grid.py
python scripts/qa_browser_flows.py
python scripts/qa_browser_actions.py
python scripts/qa_browser_popup.py
python scripts/qa_idle_redraw.py --scenario browser --assert
# new animated-page frame-rate QA
```

## 7. Risks / do-not-regress

- Do not turn CEF frame delivery back into an RGBA message or a global 60 Hz
  subscription.
- Do not remove the 500 ms cursor tick; it owns pointer hiding, not page paint.
- Do not let a background tab or hidden shell wake the visible page.
- Do not present on fallback/stale generation; keep the 300 ms focus fallback
  as a safety net only.
- Preserve the warm HWND, topmost/foreground fallback, reduced motion, and
  Launcher fade-only behavior.
- Keep changes confined to the files above; no security, tab-model, or adblock
  work in this pass.

## 8. References

- `src/browser/cef_backend.rs:867` - `on_paint` writes `FrameSlot` only.
- `src/browser/engine.rs:297` / `648` - dead `Frame` event and emit path.
- `src/ui/launcher/browser_frame.rs:420` - shader update depends on a redraw.
- `src/app/mod.rs:1807` - 2 Hz `ShellCursorTick` while a shell screen is visible.
- `src/app/shell.rs:42` / `114` / `130` / `138` - show, arm, redraw gate, present.
- `src/app/mod.rs:1701` - raw redraw listener without generation.
- `src/ui/layout.rs:293` / `324` - offscreen prime and cover move.
- `scripts/qa_browser_grid.py:476` - log-only reveal assertion.
- `scripts/qa_idle_redraw.py:190` - browser idle assertion hole.
