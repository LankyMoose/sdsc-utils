# wgpu iced image atlas crash

**Status:** do-not-regress. **When to read:** tray panic + `crash-restart`, atlas / `Texture::create_view`. Index: [README.md](README.md).

Handoff / investigation notes for the iced/wgpu image-atlas panic.

## Symptom

Tray app panic + auto-relaunch (`crash-restart`). Label: `iced_wgpu::image texture atlas` / `Texture::create_view`.

## Corrected root cause (2026-09-28)

Debug backtraces (sessions `1790511578` / `1790511587`) show:

```text
Atlas::with_size → Atlas::new → Cache::new → Pipeline::create_cache
  → Engine::create_image_cache → Renderer::new → compositor::create_renderer
```

This is **empty atlas construction for a new window renderer** (`create_renderer`), **not** `Atlas::grow` from uploading many row icons.

### Why edit-mode is not the smoking gun

Edit mode attaches icons for every installed Steam game. Rapid edit on/off has not reproduced the panic. Browse with a few enabled games only draws those rows. A single-flight art worker warms CPU-side `icon_cache` Handles for a ±10 selection window (not the whole library); they enter the GPU atlas only when drawn.

### Captured sequences

**Crash A (`1790511578`):**

1. `SlidingIn->Resting` → `start open after toast settle` → `sync toast z-order`
2. Launcher interactive for ~5s (`start-nav`)
3. `Resting->SlidingOut->Idle`
4. `PANIC` in `create_renderer` / `Atlas::new`

Launcher was already up. Panic lined up with toast teardown, or deferred/async validation from the earlier open.

**Crash B (`1790511587`):**

1. `SlidingIn->Resting` → `start open after toast settle` → `sync toast z-order`
2. Immediate `PANIC` in `create_renderer`

Cold Launcher open under a live toast present path.

**Release (`1790503175`, v1.4.1):** same `create_view` label; no `ui-diag` (release). After relaunch, `start-nav` live.

**Crash C (session `1792992062`, 2026-10-26, debug):** same signature, new timing —
empty-atlas `create_view` failure inside `create_renderer`, **5ms after Launcher
cold open** (`wgpu-diag: last_start_open=5ms`):

1. 46s idle (`sample stalled gap_ms=45995`), then 0→1 connect → auto-open
2. `launcher immersive enter` → `launcher cold open` → 5ms later `PANIC`
3. Backtrace: `Atlas::with_size → Atlas::new → Cache::new → create_cache →
   create_image_cache → Renderer::new → create_renderer` (identical to A/B)

So the failure is the new window's renderer construction racing cold open,
not toast teardown (A) or a live-toast present path (B). No `err=` on any HID
op; `steam=1` throughout (not implicated — same as prior sessions).
Recovery held: `crash-restart: scheduled → launching` in ~3s + notice toast.
Predicted adapter was Vulkan on RTX 4070 SUPER (`alpha_composite=true`).

Upstream: Cosmic / iced apps hit the same atlas `create_view` invalid pattern after GPU device faults (e.g. NVIDIA Xid) — device already dead, then atlas create panics.

**Crash D (session `1794977648`, 2026-11-18, debug, user AFK):**
same empty-atlas `create_view` signature, but the device was already lost
*before* the window open:

1. 18:01:57 — display sleep/lock. `iced_winit` logs hundreds of
   `Error Other when presenting surface.` (surface lost; no panic yet).
2. 18:24:09 — user returns and presses the **PS button**; the next window
   renderer construction panics (`Renderer::new` → `create_renderer`,
   `WindowManager::insert`) with
   `Texture with 'iced_wgpu::image texture atlas' label is invalid`.
3. `crash-restart: scheduled`, successor logs only
   `starting session=…` and exits; no surviving process (separate
   recovery gap — the successor may have hit the same dead device or lost
   the single-instance race).

The PS button is the trigger for the new window (Launcher open), not the
cause: any new `create_renderer` after device loss constructs an atlas on
the dead device. Unlike A/B/C, no toast was live and no cold-open race
was involved; the surface-error burst before the panic is the tell.

## Repro matrix (manual)

| Case | Expect |
|------|--------|
| Connect → Connected toast Resting → OpenStart (cold open) | Crash B path if still broken; watch `launcher cold open` / `launcher opened` |
| Connect → Launcher open → toast expires to Idle | Crash A path; watch `toast hide` / deferred hide |
| Spam edit mode on/off | Should **not** panic (no new `create_renderer`) |
| Open Settings while toast Resting | Another cold `create_renderer`; compare |
| Display sleep/lock → wake → PS button (open Launcher) | Crash D: surface-error burst first (`Error Other when presenting surface`), then atlas `create_view` panic on the dead device |

Debug grep: `wgpu-diag:`, `ui-diag: launcher cold open`, `launcher opened`, `toast hide`, `PANIC at`, `PANIC_BACKTRACE`, `create_renderer` (in backtrace), `presenting surface` (device-loss tell before D).

## Mitigations landed

1. **Warm Launcher reverted** — cold `window::open(visible: true)` + destroy on close so the OS animates open. Keep `launcher_visible` raise gate, reopen-release gate, unfocus grace.
2. **Do not `sync_toast_zorder` on the same turn as `window::open`** — raise/focus after `LauncherOpened`.
3. **Defer toast hide ~50ms when Launcher is visible** — avoid hide vs renderer stress (Crash A).
4. **Throttle Resting `RaiseInteractive`** — emit on slide settle / `sync_toast_zorder` only, not every `ToastFrame`.
5. **Boot:** log wgpu adapters; prefer `WGPU_BACKEND=dx12` on Windows when unset; panic hook appends `wgpu-diag:` lifecycle ages.
6. **Icon downscale cache** — still useful for grow/edit; not the primary create_renderer fix.
7. **Toast warm sentinel** — unchanged (compositor for overlays).

## Grep tokens

- `PANIC at` / `PANIC_BACKTRACE`
- `iced_wgpu::image texture atlas` / `Texture::create_view` / `create_renderer`
- `wgpu-diag: adapter` / `wgpu-diag: last_start_open`
- `ui-diag: launcher cold open` / `launcher opened` / `toast hide` / `toast hide deferred`
- `crash-restart: scheduled` / `launching` / `notice toast`

## Explicit non-goals

- Sequential toast↔Launcher hiding as the product UX
- Viewport icon virtualization as the primary fix
- Re-introducing warm Launcher for OS animation
- iced fork / toast on another thread
