# Transparent windows go opaque-black under forced DX12

> **Resolved (1.7.0):** boot now sets `WGPU_BACKEND=vulkan,dx12` (Vulkan preferred, DX12 fallback) and predicts alpha support via `wgpu_diag::alpha_composite()`. See [ui-refresh.md](ui-refresh.md). The analysis below is kept for the DX12-fallback path.

## Symptom

Rounded glass cards (iced `container` with `border.radius` on a
`transparent: true` window) render the radius correctly, but the pixels
outside the curve are opaque black instead of the desktop behind the
window. Affects popup, configure, compact Start, and toasts — anywhere we
rely on per-pixel alpha.

## Root cause

The app pins `WGPU_BACKEND=dx12` process-wide at boot
(`src/platform/wgpu_diag.rs:65-70`, introduced in `05b77ac` alongside the
wgpu atlas-crash hardening). On this setup (RTX 4070 SUPER, iced 0.14 /
winit 0.30.13), the DX12 swapchain path yields no pre/post-multiplied
alpha mode, so iced falls back to opaque composition and alpha-0 pixels
present as black. The default backend order picks Vulkan, whose swapchains
composite alpha correctly.

## Evidence (same machine, same compositor)

- Minimal iced daemon probe (transparent + `AlwaysOnTop` + `DoNotRound`,
  rounded card): **white/desktop corners** on default backend.
- Same probe with `WGPU_BACKEND=dx12`: **black corners**.
- Full app with `WGPU_BACKEND=vulkan` (env override wins over the
  built-in preference): **white/desktop corners**.
- Full app as shipped (dx12 forced): **black corners**.

Radius/border rendering was never broken — only corner alpha.
Backend results on this machine (RTX 4070 SUPER, same probe window):

- Vulkan (default order): transparent corners.
- DX12 (`WGPU_BACKEND=dx12`): opaque black corners.
- OpenGL (`WGPU_BACKEND=gl`): opaque black corners.

So among wgpu backends here, only Vulkan composites per-pixel alpha.

## Ruled out during investigation

Theme clear color (`toast_theme`, per-window `App::theme` — verified via
logging), window flags (`decorations`, `transparent`, `AlwaysOnTop`,
`exit_on_close_request`, `skip_taskbar`, size, `Centered` vs `Specific`),
card content (flat/gradient, simple/scrollable rows, svg/checkbox),
`clip(true)`, wgpu shaders as children, MSAA (`.antialiasing`), constant
redraws (subscriptions stripped to none), post-open HWND calls
(`gain_focus`, toast z-order sync), hidden toast pre-creation, tray icon,
HID/hotkey workers, service/shell split, manifest/subsystem, monitor/DPI.

## Implications for glass UI (1.7.0)

Any design needing transparent rounded corners requires an alpha-capable
backend. Options:

1. **Prefer Vulkan** (or drop the preference and take wgpu default
   order). Must re-validate the atlas-crash scenario from
   `notes/wgpu-image-atlas-crash.md` — the dx12 preference reads as
   diagnostic standardization rather than a proven crash fix, but that is
   unproven. Exercise concurrent toast + cold Start image pressure on
   Vulkan before committing.
2. **Keep dx12 + opaque windows** with OS rounding
   (`CornerPreference::Round` instead of `DoNotRound` in
   `src/ui/layout.rs:512-536`). Loses custom radius/glass-over-desktop;
   radius limited to what DWM provides.
3. **Keep dx12 + square cards.** No transparency needed.

## Can it work under DX12?

Short answer: not with wgpu 27 + iced 0.14 as-is. The limitation is a
one-line gate, not the OS:

- `wgpu-hal 27.0.4/src/dx12/adapter.rs:1006` advertises only
  `CompositeAlphaMode::Opaque` for `SurfaceTarget::WndHandle` (real HWND
  windows). iced only picks advertised modes, so per-pixel alpha is
  unreachable on DX12 regardless of driver.
- The present path already maps a requested mode onto
  `DXGI_SWAP_CHAIN_DESC1::AlphaMode` (`dx12/mod.rs`, flip-discard), so the
  OS side could plausibly work if the gate were lifted — but nobody has
  proven it for plain HWND windows, and upstream history is discouraging:
  `gfx-rs/wgpu#1375` ("DX12 doesn't respect alpha for the window", 2021,
  Vulkan worked even then) was closed unfixed with `help required`;
  `gfx-rs/wgpu#5150` ("Cannot create transparent window", 2024) was closed
  as not planned; `gfx-rs/wgpu#7117` ("fix(dx12): map composite alpha
  mode", merged Feb 2025) wired the AlphaMode mapping but only helps
  paths that advertise alpha (dcomp visuals, not HWND).

Options, none free:

1. **Patch/fork wgpu-hal** to advertise `PreMultiplied` for WndHandle
   and validate that DWM actually honors it (unknown — #1375 suggests
   the composition itself may ignore it). Fork maintenance burden on
   every wgpu upgrade.
2. **DirectComposition visuals** (`SurfaceTarget::Visual` advertises full
   alpha) — not usable from winit HWND windows; dead for iced.
3. **Layered windows** (`WS_EX_LAYERED` + `UpdateLayeredWindow`) — full
   alpha on any backend, but incompatible with DXGI flip swapchains
   (forces legacy blit path with tearing/perf cost) and unsupported by
   iced/winit. Dead in practice.
4. **Prefer Vulkan** (or drop the override and take wgpu default order),
   keeping DX12 as fallback where Vulkan is absent. Requires re-validating
   the atlas-crash scenario from `notes/wgpu-image-atlas-crash.md` on
   Vulkan, since the dx12 preference shipped with that hardening.

On the "Vulkan is still new" worry, for the record: Vulkan 1.0 shipped
February 2016; NVIDIA/AMD/Intel Windows drivers are mature; it is wgpu's
most exercised backend (all of Linux) and iced's default; our probes ran
flawlessly on it, and our demographic (PC gamers with DualSense pads,
overwhelmingly discrete GPUs) is the best-covered segment. Where Vulkan
is absent, wgpu falls back to DX12 automatically — opaque corners, fully
functional app.

## Validation (Vulkan stability)

`WGPU_BACKEND=vulkan` smoke-tested against the atlas-crash shape from
`notes/wgpu-image-atlas-crash.md`, no controllers attached (empty game
catalog — lighter image pressure than a big Steam library, but every cold
open still constructs a fresh empty atlas, which was the panic site):

- Slow matrix (~50s): 3× preview-toast + cold Start open under a live
  toast, configure cold opens, edit-mode toggles, closes.
- Fast matrix (~60s): 5 cycles of toast + cold open, second cold renderer
  (configure) while toast + Start live, 3× edit on/off churn per cycle,
  teardown with toast expiry overlapping the next open.
- Total: 9+ cold Start opens with interleaved toast Resting/hide, 0
  panics, 0 atlas errors, both processes healthy throughout.

Caveats: no real pads (presence-driven paths untested on Vulkan) and a
single machine/GPU (RTX 4070 SUPER, current drivers). Re-run on wider
hardware before release.

The driver lives in-tree as a debug-only tool: Settings → Diagnostics →
"Run window stress test" (`Message::StressWindows`, `#[cfg(debug_assertions)]`,
compiled out of release builds). It takes over window open/close for about
a minute; check `app.log` for `PANIC`/atlas lines afterwards.

## Follow-ups worth keeping regardless

- A glanceable build ID in the Settings footer (`<hash>[-dirty]@<unix>` baked
  by `build.rs`) proved invaluable for telling fresh test builds from stale
  ones during this investigation — especially with the service/shell split,
  where `cargo run` rebuilt only the service and a stale or lingering
  `sdsc-shell.exe` kept showing old UI (single-instance only guards one
  side; check `tasklist` for **both** names). Since 1.7.0, `cargo run`
  rebuilds the shell automatically (`shell-sync:` in `app.log`) and
  `scripts/ci.sh` ends with `cargo build`.
- If transparency is ever doubted again, the fastest separator is a probe
  window with a thick red border + big radius over a white background:
  curved-red + white corners = alpha works; curved-red + black = radius
  works but composition is opaque; square-red = radius itself fails.
