# Windows toast (z-order, remount, slide vs Start)

**Status:** do-not-regress. **When to read:** toast/Start present starvation, wrong toast content, connect-open races. Index: [README.md](README.md).

## Toast Z-order vs Settings / Start freeze (Windows)

iced multi-window present starvation: [iced#3108](https://github.com/iced-rs/iced/issues/3108) / [#3320](https://github.com/iced-rs/iced/issues/3320).

**Do not demote** the toast to `HWND_NOTOPMOST` when Settings/Start/popup are open — that puts the toast under Cursor.

**Do not** raise toast above Start every `ToastFrame` — that starves Start presents.

**Do:** keep toast `HWND_TOPMOST` (above Cursor); raise Start/Settings into the same topmost band *above* the toast (corner toast stays visible beside centered Start); `gain_focus` the interactive window.

**Immersive cover:** the primary-monitor Start HWND occludes the toast HWND (and unowned `rfd` file dialogs). Do **not** raise the toast above Start (present starvation). While immersive, composite the same toast card into the Start window (`Float` overlay at cover-local slide pose); keep the toast HWND blank/hidden (no `show_toast_without_activate`, no `move_to`). Parent add/edit shortcut file dialogs to the Start HWND via `rfd::FileDialog::set_parent` so the picker opens above the cover.

Debug grep: `ui-diag: place toast gen=`, `ui-diag: sync toast z-order (toast + raise UI)`, `ui-diag: immersive toast composite gen=`, `ui-diag: file dialog parent hwnd=`.

## Queued toast wrong content (Windows)

Symptom: two connect toasts both showed the first pad’s %, then recreate made only one toast appear.

`app.log` proved the **model is correct** (`toast show … percent=55` then `percent=100`). Stale swapchain on reuse; closing/recreating the HWND dropped the follow-up toast.

Fix: reuse one toast HWND; on handoff skip hide and **remount** (±1px resize + `RedrawWindow`); `PlaceToast` is generation-guarded.

Debug grep: `ui-diag: toast show`, `ui-diag: place toast gen=`, `ui-diag: toast handoff remount`.

## Connect toast slide vs Start open (Windows)

Symptom: on 0→1 connect (toast + Start together), the Connected toast sometimes never appears, freezes mid-slide, or pops into the rest pose.

Cause (historical): slide-in used a 250ms wall clock that stopped issuing `move_to` once elapsed ≥ 250ms, and the clock started in `PlaceToast` before show. Opening Start in the same batch stalled the UI.

Cause (session `1790509148457`): a 500ms Start deadline fired **before** `place toast`, so Start’s window create starved the toast present (iced#3108 / #3320). Log order: `toast show` → `defer start` → `start open after toast deadline` → `place toast gen=1`.

Cause (session `1790510886493`): after the state machine landed, a **2s Placing wall-clock failsafe** treated a stale `ToastFrame` `raw_dt` (time since app boot / last toast) as “placement gave up.” Log: `Idle->Placing` → `Placing->Idle` + `start open` in the same second — toast never reached `SlidingIn`. Reconnect after intentional power-off then showed toast with `after=Nothing` because `START_CONNECT_COOLDOWN` armed on the power-off 1→0.

Fix: pure presentation state machine in [`src/ui/toast/machine.rs`](../src/ui/toast/machine.rs). Phases `Placing → SlidingIn → Resting → SlidingOut`. **Placing is event-only** (`Frame` ignored for phase change; leave only on `Shown` or `Dismiss`) — **no placing wall-clock failsafe** (stays removed). Start opens only as an `OpenStart` effect from slide rest or dismiss finish. `RaiseInteractive` only after Resting. Placement uses `primary_toast_area()` immediately (no `monitor_size` hop). Intentional power-off sets `skip_next_connect_cooldown` only when that pad was the sole live controller, so the next reconnect can latch `OpenStart` again (unexpected disconnects still get the 5s ghost-flap cooldown; powering off one of several must not treat a sibling as a fresh 0→1). Do not put the toast on a second thread — iced presents on one UI thread.

**Reopen gesture during latch:** `suppresses_reopen_gesture()` while `after == OpenStart` and phase is Placing/SlidingIn. During Placing, suppress lifts after ~60 Frame ticks (~1s) so a lost `Shown` cannot block the gesture forever; phase stays Placing and no `OpenStart` is emitted from that budget.

### Immersive 0→1 Connected (composite only)

When Connected would auto-open Start **and** `start_screen_always_immersive` is on, do **not** slide the toast on the desktop HWND first (that double-up’d with the in-cover composite). Instead:

1. Queue Connected with `after=Nothing` (no `AfterToast::OpenStart`).
2. Set `defer_toast_for_immersive_start` so `show_next_toast` no-ops (holds the **whole** queue — same-tick or staggered multi-pad Connecteds append only).
3. Open immersive Start immediately.
4. On `StartOpened` (or already-hosting), clear the defer flag and show; toast HWND stays hidden; card slides only via `immersive toast composite`.
5. If Start cannot host (disabled / compact / close abandoned), fall back to HWND toast so Connected is never dropped.

Compact auto-open keeps defer-until-toast-rest (`AfterToast::OpenStart`).

Debug grep: `ui-diag: immersive connect toast (start first, composite only)`, `ui-diag: immersive connect toast release (start hosting)`, `ui-diag: immersive connect toast fallback (hwnd)`, `ui-diag: place toast … composite_only=1`, `ui-diag: immersive toast composite gen=`.

### 0→1 Start survive toast (robustness)

Toast and Start are separate gates. To stop Connected-without-Start on turn-on:

- **One-miss hold:** a failed battery/open poll keeps the last status for one tick; the second consecutive miss accepts the drop. Presence-empty (HID list gone) still clears immediately.
- **Short arrival:** nonempty stretch under 2s does not arm the 5s ghost cooldown (enumerate blip). Stable disconnects still arm it; intentional Power Off still skips.
- **Pending retry:** `start_auto_open_pending` is set when 0→1 auto-open gates pass; cleared only once Start is visible. Close-in-flight leaves it set; `WindowClosed` retries. Confirmed empty clears pending and `clear_after()` even when Start is not visible.

Debug grep: `ui-diag: defer start until toast slide settles`, `ui-diag: toast Placing->SlidingIn`, `ui-diag: toast SlidingIn->Resting`, `ui-diag: toast show … body=Connected after=OpenStart`, `ui-diag: place toast gen=`, `ui-diag: start open after toast settle`, `ui-diag: skip connect cooldown (intentional power-off)`, `ui-diag: skip connect cooldown (short arrival)`, `ui-diag: hold pad across missed read serial=`, `ui-diag: retry start open after close`, `ui-diag: reopen gesture suppressed (toast OpenStart pending)`, `ui-diag: immersive connect toast`.
