# wgpu iced image atlas crash (2026-09-27)

Handoff / post-fix notes for the iced/wgpu image-atlas panic seen on **v1.4.1**.

## Symptom

Tray app panic + auto-relaunch (`crash-restart`). User was on **latest release `v1.4.1`**.

## Captured panic (`%APPDATA%\sdsc-utils\app.log`)

```text
[1790503175] ERROR: PANIC at …\wgpu-27.0.1\src\backend\wgpu_core.rs:2218:18: wgpu error: Validation Error

Caused by:
  In Texture::create_view
    Texture with 'iced_wgpu::image texture atlas' label is invalid

[1790503175] ERROR: PANIC_BACKTRACE    0: <unknown>
   …
   7: hid_write
   9: hid_write
   … (symbols mostly unknown in release)

[1790503175] INFO: crash-restart: scheduled
[1790503176] INFO: crash-restart: launching
[1790503176] INFO: crash-restart: notice toast
```

Context in the same window: mostly `hid-worker: cmd=Poll` lines; **no** `start-nav:` / `ui-diag:` toast lines immediately before the panic (release builds also omit investigation-volume `ui-diag`).

`crash-restart.json` showed a single attempt around that timestamp; `notice` cleared after the toast.

## What this is (and is not)

| Is | Is not |
|----|--------|
| iced/wgpu **image texture atlas** invalid after atlas **grow** (or create) → `Texture::create_view` | Connect-toast slide miss / freeze / pop |
| Same class as the **1.4.1** changelog fix (incomplete) | Fixed by toast slide clock / Start deferral / ~500ms Start latch |

1.4.1 already shipped: stable shell-icon `image::Handle`s (build `Handle::from_rgba` once into `StartIcon::Rgba` instead of every `view()`), plus `PANIC_BACKTRACE` logging. User still hit the atlas panic on **1.4.1**, so that mitigation was incomplete.

## Actual cause (corrected)

`Handle::from_path` in iced 0.14 already uses a **path-hash** id (`Id::path`). Calling it every `view()` does **not** mint a new atlas entry. Caching path handles would not have fixed this.

`create_view` runs in iced’s `Atlas::new` / `Atlas::grow`. Growth is driven by uploading large / many unique RGBA ids:

1. **Full-size Steam capsules** — `library_600x900.jpg` (600×900) uploaded into the 2048² atlas for a 48×72 cell. Edit mode lists every installed game → many layers → `grow` → `create_view`. Failed `create_texture` (memory / device loss) leaves an invalid texture; wgpu panics by default.
2. **Shell handles rebuilt on every `refresh_start_rows()`** — `file_icon` cached pixels, but each row rebuild called `Handle::from_rgba` again (`Id::unique()`), so reopening Start / edit toggles re-uploaded shell icons. Atlas layers never shrink.

Controllers / macros / percent rings use canvas or SVG — not the raster atlas.

## Fix (landed)

[`src/ui/start/icon_cache.rs`](../src/ui/start/icon_cache.rs): process-lifetime cache of downscaled `Handle::from_rgba` (fit to 96×144). Steam paths warmed on the existing scan `spawn_blocking`; Start rows and the add-shortcut preview only clone cached handles. Debug: `ui-diag: icon handle` on cache miss.

## Grep tokens

- `PANIC at` / `PANIC_BACKTRACE`
- `iced_wgpu::image texture atlas`
- `Texture::create_view`
- `crash-restart: scheduled` / `launching` / `notice toast`
- `ui-diag: icon handle` (debug)

## Reproduce / capture next time

1. Prefer a **debug** build so `ui-diag:` / hitch marks are live; stamp **F7/F8** if UI freezes before panic.
2. Note whether Start, Settings, popup, or a toast was visible; library size / edit mode.
3. Keep `app.log` + `app.log.1` around the `PANIC at` line (±30s).
4. If possible, capture whether a GPU reset / fullscreen game / monitor sleep preceded it.

## Explicit non-goals

- Do not treat toast slide / Start ASAP latch work as the fix for this panic.
- Do not demote toast z-order “fixes” as the primary atlas cure without evidence.
- Device-loss recovery inside iced still requires process crash-restart (no per-image GPU retry).
