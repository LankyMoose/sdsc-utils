# Windows toast (z-order, remount, slide vs Launcher)

**Status:** do-not-regress. **When to read:** toast/Launcher present starvation, wrong toast content, connect-open races. Index: [README.md](README.md).

> **Home pivot (phases 1–2):** Launcher and the standalone Browser window are
> `Level::Normal`. The toast is the topmost shell surface, above Home.
> `AfterToast::OpenStart` is now `OpenHome`, and auto-open targets Home.
> The history below explains why the toast band must stay intact.

## Toast Z-order vs Settings / Launcher freeze (Windows)

iced multi-window present starvation: [iced#3108](https://github.com/iced-rs/iced/issues/3108) / [#3320](https://github.com/iced-rs/iced/issues/3320).

**Do not demote** the toast to `HWND_NOTOPMOST` when Home/Settings/popup are open — that puts the toast under Cursor.

**Do not** raise the toast every `ToastFrame` — that causes z-order churn/present starvation. Event-driven raises are fine (placement, Resting, shell open/focus).

**Do (current):** when no full-screen shell is hosting the toast, keep it `HWND_TOPMOST` (above Cursor) and raise it **last** above every shell surface — Home, Settings, popup — with `HWND_TOP` + `SWP_NOACTIVATE` so focus stays on the shell. Raise/focus the shell first, then the toast. Do **not** raise the toast every frame; only on placement, shell open/focus, and Resting sync (same cadence as `RaiseInteractive`). Launcher stays `Normal` and is under the toast.

**Composite host:** Launcher and Home are full-screen hosts. While either is visible, `immersive_hosts_toast()` hides the toast HWND and both windows draw the same `immersive_toast_overlay()` card above their stage/scrim. On host close (Launcher or Home) `rehost_toast_after_composite()` remounts/shows the HWND at the fixed target position; the card's slide offset comes from the machine pose. On host open the HWND is hidden again. This is what keeps the toast visible above Home scrim without a second toast.

**Pull-model slide (refresh work order):** the toast HWND is fixed at `placement.target_y`; the slide is a draw-time content offset (`toast_view::view(..., offset_y)`) driven by the machine's wall clock. `ToastFrame` is subscribed only while the machine is **not resting** (Placing/SlidingIn/SlidingOut), so a static 5s card produces zero global frames. Expiry is still the one-shot `ToastDismiss(generation)` task; z-order raises stay event-driven (placement, Resting sync, shell focus). Do not move the HWND per frame; do not reintroduce a resting `ToastFrame`.

**Immersive cover:** the primary-monitor Launcher HWND occludes the toast HWND (and unowned `rfd` file dialogs). Do **not** raise the toast above Launcher (present starvation). While immersive, composite the same toast card into the Launcher window (`Float` overlay at cover-local slide pose); keep the toast HWND blank/hidden (no `show_toast_without_activate`, no `move_to`). Parent add/edit shortcut file dialogs to the Launcher HWND via `rfd::FileDialog::set_parent` so the picker opens above the cover.

Debug grep: `ui-diag: place toast gen=`, `ui-diag: sync toast z-order (toast above UI)`, `ui-diag: immersive toast composite gen=`, `ui-diag: file dialog parent hwnd=`.

## Queued toast wrong content (Windows)

Symptom: two connect toasts both showed the first pad’s %, then recreate made only one toast appear.

`app.log` proved the **model is correct** (`toast show … percent=55` then `percent=100`). Stale swapchain on reuse; closing/recreating the HWND dropped the follow-up toast.

Fix: reuse one toast HWND; on handoff skip hide and **remount** (±1px resize + `RedrawWindow`); `PlaceToast` is generation-guarded.

Debug grep: `ui-diag: toast show`, `ui-diag: place toast gen=`, `ui-diag: toast handoff remount`.

## Connect toast slide vs Launcher open (Windows)

Symptom: on 0→1 connect (toast + Launcher together), the Connected toast sometimes never appears, freezes mid-slide, or pops into the rest pose.

Cause (historical): slide-in used a 250ms wall clock that stopped issuing `move_to` once elapsed ≥ 250ms, and the clock started in `PlaceToast` before show. Opening Launcher in the same batch stalled the UI.

Cause (session `1790509148457`): a 500ms Launcher deadline fired **before** `place toast`, so Launcher’s window create starved the toast present (iced#3108 / #3320). Log order: `toast show` → `defer start` → `start open after toast deadline` → `place toast gen=1`.

Cause (session `1790510886493`): after the state machine landed, a **2s Placing wall-clock failsafe** treated a stale `ToastFrame` `raw_dt` (time since app boot / last toast) as “placement gave up.” Log: `Idle->Placing` → `Placing->Idle` + `start open` in the same second — toast never reached `SlidingIn`. Reconnect after intentional power-off then showed toast with `after=Nothing` because `LAUNCHER_CONNECT_COOLDOWN` armed on the power-off 1→0.

Fix: pure presentation state machine in [`src/ui/toast/machine.rs`](../src/ui/toast/machine.rs). Phases `Placing → SlidingIn → Resting → SlidingOut`. **Placing is event-only** (`Frame` ignored for phase change; leave only on `Shown` or `Dismiss`) — **no placing wall-clock failsafe** (stays removed). Home opens only as an `OpenHome` effect from slide rest or dismiss finish. `RaiseInteractive` only after Resting. Placement uses `primary_toast_area()` immediately (no `monitor_size` hop). Intentional power-off sets `skip_next_connect_cooldown` only when that pad was the sole live controller, so the next reconnect can latch `OpenHome` again (unexpected disconnects still get the 5s ghost-flap cooldown; powering off one of several must not treat a sibling as a fresh 0→1). Do not put the toast on a second thread — iced presents on one UI thread.

**Reopen gesture during latch:** `suppresses_reopen_gesture()` while `after == OpenHome` and phase is Placing/SlidingIn. During Placing, suppress lifts after ~60 Frame ticks (~1s) so a lost `Shown` cannot block the gesture forever; phase stays Placing and no `OpenHome` is emitted from that budget.

### 0→1 Connected vs Launcher open order

Auto-open suppresses the Connected toast at the session layer
(cold-launch `launch_quiet` and hot connects behave identically), so Launcher
opens immediately with no toast to race. The shell keeps a second-layer
filter for service-race leftovers.

Auto-open is immediate: the session strips the Connected card on 0→1 auto-open and opens the **Home** at once, so there is no toast to race. The `AfterToast::OpenHome` latch remains as a second layer for service-race leftovers (a Connected card that slipped through still opens Home on slide rest via the machine effect, exactly once — settle consumes the latch, and early dismiss / slide-out finish also emit it). A mid-toast 1→0 clears the latch via `ClearHomeLatch`, so Home never opens after the pads left.

Debug grep: `ui-diag: defer start until toast slide settles`, `ui-diag: connect toast suppressed (immersive auto-open`, `ui-diag: launcher open after toast settle`.

### 0→1 Launcher survive toast (robustness)

Toast and Launcher are separate gates. To stop Connected-without-Launcher on turn-on:

- **One-miss hold:** a failed battery/open poll keeps the last status for one tick; the second consecutive miss accepts the drop. Presence-empty (HID list gone) still clears immediately.
- **Short arrival:** nonempty stretch under 2s does not arm the 5s ghost cooldown (enumerate blip). Stable disconnects still arm it; intentional Power Off still skips.
- **Pending retry:** `auto_open_pending` is set when 0→1 auto-open gates pass; cleared once Home is visible. Close-in-flight leaves it set; `WindowClosed` retries Home. Confirmed empty clears pending and `clear_after()` even when Home is not visible.

Debug grep: `ui-diag: defer Home until toast slide settles`, `ui-diag: toast Placing->SlidingIn`, `ui-diag: toast SlidingIn->Resting`, `ui-diag: toast show … body=Connected after=OpenHome`, `ui-diag: place toast gen=`, `ui-diag: home open after toast settle`, `ui-diag: skip connect cooldown (intentional power-off)`, `ui-diag: skip connect cooldown (short arrival)`, `ui-diag: hold pad across missed read serial=`, `ui-diag: retry Home open after close`, `ui-diag: reopen gesture suppressed (toast OpenHome pending)`, `ui-diag: immersive connect toast`, `ui-diag: connect toast suppressed (immersive auto-open`.
