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
2. Start interactive for ~5s (`start-nav`)
3. `Resting->SlidingOut->Idle`
4. `PANIC` in `create_renderer` / `Atlas::new`

Start was already up. Panic lined up with toast teardown, or deferred/async validation from the earlier open.

**Crash B (`1790511587`):**

1. `SlidingIn->Resting` → `start open after toast settle` → `sync toast z-order`
2. Immediate `PANIC` in `create_renderer`

Cold Start open under a live toast present path.

**Release (`1790503175`, v1.4.1):** same `create_view` label; no `ui-diag` (release). After relaunch, `start-nav` live.

Upstream: Cosmic / iced apps hit the same atlas `create_view` invalid pattern after GPU device faults (e.g. NVIDIA Xid) — device already dead, then atlas create panics.

## Repro matrix (manual)

| Case | Expect |
|------|--------|
| Connect → Connected toast Resting → OpenStart (cold open) | Crash B path if still broken; watch `start cold open` / `start opened` |
| Connect → Start open → toast expires to Idle | Crash A path; watch `toast hide` / deferred hide |
| Spam edit mode on/off | Should **not** panic (no new `create_renderer`) |
| Open Settings while toast Resting | Another cold `create_renderer`; compare |

Debug grep: `wgpu-diag:`, `ui-diag: start cold open`, `start opened`, `toast hide`, `PANIC at`, `PANIC_BACKTRACE`, `create_renderer` (in backtrace).

## Mitigations landed

1. **Warm Start reverted** — cold `window::open(visible: true)` + destroy on close so the OS animates open. Keep `start_visible` raise gate, reopen-release gate, unfocus grace.
2. **Do not `sync_toast_zorder` on the same turn as `window::open`** — raise/focus after `StartOpened`.
3. **Defer toast hide ~50ms when Start is visible** — avoid hide vs renderer stress (Crash A).
4. **Throttle Resting `RaiseInteractive`** — emit on slide settle / `sync_toast_zorder` only, not every `ToastFrame`.
5. **Boot:** log wgpu adapters; prefer `WGPU_BACKEND=dx12` on Windows when unset; panic hook appends `wgpu-diag:` lifecycle ages.
6. **Icon downscale cache** — still useful for grow/edit; not the primary create_renderer fix.
7. **Toast warm sentinel** — unchanged (compositor for overlays).

## Grep tokens

- `PANIC at` / `PANIC_BACKTRACE`
- `iced_wgpu::image texture atlas` / `Texture::create_view` / `create_renderer`
- `wgpu-diag: adapter` / `wgpu-diag: last_start_open`
- `ui-diag: start cold open` / `start opened` / `toast hide` / `toast hide deferred`
- `crash-restart: scheduled` / `launching` / `notice toast`

## Explicit non-goals

- Sequential toast↔Start hiding as the product UX
- Viewport icon virtualization as the primary fix
- Re-introducing warm Start for OS animation
- iced fork / toast on another thread
