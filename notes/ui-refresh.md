# UI refresh (1.7.0): immersive language everywhere

**Status:** in progress. **When to read:** restyling popup, Settings, toasts, compact Start; theme tokens; window corners/transparency. Index: [README.md](README.md).

Goal: bring the tray popup, Settings (configure), toasts, and compact Start up to the visual language of immersive Start ([start-immersive.md](start-immersive.md)) without regressing its behavior.

## Locked decisions

1. **Backend: Vulkan preferred, DX12 fallback.** `WGPU_BACKEND=vulkan,dx12` is set at boot when unset (`src/platform/wgpu_diag.rs`, `PREFERRED_BACKENDS`). wgpu-core inits HALs Vulkan → DX12 and stable-sorts adapters by device type, so Vulkan wins for the same GPU; if Vulkan is absent or cannot present to the surface, `request_adapter` falls through to DX12 itself. A user `WGPU_BACKEND` always wins. Boot predicts the selected backend with the same sort (no surface) and exposes `wgpu_diag::alpha_composite()`. Grep: `wgpu-diag: predicted adapter`.
   - `alpha_composite() == true` → transparent windows, custom radius, `CornerPreference::DoNotRound`.
   - `false` (DX12/GL fallback) → opaque windows + `CornerPreference::Round` (DWM; Windows 11 only, Windows 10 stays square). Same in-window glass; only the outer corners differ.
   - Must re-run Settings → Diagnostics → window stress test (debug builds) after each window-surface change; check `app.log` for `PANIC` / atlas lines ([wgpu-image-atlas-crash.md](wgpu-image-atlas-crash.md)).
2. **Window sizes may grow.** Settings ~720×520 (from 420×400); compact Start may grow too.
3. **Font:** keep iced's default font. Hierarchy comes from size/weight/color only.
4. **Utility-window backdrop: nearly static.** Ambient shader with frozen (or ~2 fps) time in popup / Settings / compact Start; no continuous redraw from idle utility windows. Full-motion ambient stays immersive-only.

## Immersive language (source of truth)

- Backdrop: `AmbientProgram` + `VignetteProgram` (`src/ui/shader/`), never flat fills.
- Glass islands: `theme::immersive_island_radius` — `darken(BASE_BG, 0.55) @ 0.58`, hairline `LINE @ 0.28`, large radii, concentric nesting (inner = outer − pad).
- Separation by spacing/islands; no rules or list separators.
- Selection: neutral wash (`INK @ 0.05`) + bold title, not accent fill.
- Type: 18px+ titles, muted 13px meta; strong size contrast.
- Motion: ease-in-out veils, `WidthReveal`, staggered slide-in/out chips, crossfades; 32 ms frame-dt cap.
- Hints: PS face glyphs in floating capsules.

## Legacy gaps (pre-refresh)

`RADIUS` 2 / `RADIUS_SM` 1 (0 on chips, toasts, tabs); 1px `framed` LINE outline + header rules + list separators; flat `BASE_BG`/`CONTENT`; accent-blue selection (tabs/chips @ 28%); stock checkbox/slider/pick_list/scrollbar; 12–14px type; no motion outside the toast slide; Settings 420×400 with a 120px sidebar; Start-screen prefs duplicated between `configure/mod.rs` and `start/settings.rs`.

## Screenshot / review tooling (debug builds)

`SDSC_DEBUG_OPEN=popup|settings[:start|notifications|toast|lightbar|analytics|diagnostics]|start|toast` (comma-separated) opens those windows ~1.5s after boot (`Message::DebugOpen`). Setting it also enables **quiet mode** (`platform::debug_quiet`): windows are created hidden, shown with `SW_SHOWNOACTIVATE` + `WS_EX_NOACTIVATE`, and relocated from the primary to the first secondary monitor at the same relative position (`layout::{focus_window, show_window, move_window}` → `win32::move_to_secondary_monitor`); UI sounds, Start haptics, and pad input to the shell are off. Safe to run while a game owns the primary monitor. All window focus / move / show in `app/mod.rs` must go through those `layout` helpers (they are plain `gain_focus` / `move_to` / `set_mode(Windowed)` outside quiet mode).

## Phases

- [x] **0a Backend switch** — Vulkan-first with DX12 fallback + `alpha_composite()` prediction.
- [x] **0b Foundations (no visual change)**
  - `src/ui/motion.rs`: easing (`ease_out/in/in_out_cubic`), `progress_linear`, `advance_capped`, `MAX_FRAME_DT(_MS)` = 32, durations `FAST_MS` 100 / `BASE_MS` 280 / `SLOW_MS` 450. `start::mode` and `layout` delegate (public names kept); constants equal to a motion value now reference it. Toast slide stays 250 ms.
  - `src/ui/reveal.rs`: `WidthReveal` moved from `ui/start/`.
  - `theme.rs` tokens: `radius::{SM 8, MD 12, LG 16, XL 20, PILL}`, `type_scale::{CAPTION 11, META 13, BODY 14, TITLE 18, HEADING 24, DISPLAY 32}` (BODY kept at the app's existing 14, not 15), `GLASS_FILL`, `GLASS_HAIRLINE`, `SELECT_WASH`. `glass(radius)` / `glass_row(selected, radius)` are the shared island + row styles; `immersive_island*` / `immersive_dock_row_surface` delegate. Legacy `RADIUS`/`RADIUS_SM` remain for unconverted chrome.
  - Form-control styles (defined, applied from Phase 1+): `theme::toggle` (iced `toggler`), `theme::slider`, `theme::pick_list` + `theme::pick_list_menu`, `theme::scrollbar`. Start settings drawer also uses stock `toggler`/`slider`, so both settings surfaces adopt these together.
  - `src/ui/backdrop.rs`: `backdrop()` / `with_backdrop(content)` — ambient at `FROZEN_TIME` 0.0 (immersive's first frame, breath mid-swing) + vignette `VIGNETTE_STRENGTH` 0.35 (immersive stage uses 0.55). No continuous redraw.
  - **Deferred:** face-glyph hint capsule stays in `start/view.rs` (coupled to `StartMessage` / press state, Start-only today). Promote when Settings gains pad navigation (Phase 4). Glass *tiers* beyond `GLASS_FILL` get defined in Phase 1 once they can be judged on screen.
- [ ] **1 Window chrome** — drop `framed` outline + header rule; corner strategy per `alpha_composite()`; backdrop + spacing. Stress-test.
- [ ] **2 Tray popup** — dock-style glass rows (bigger ring, 18px title, muted meta), pin toggle replaces Remember checkbox, quieter actions, empty state, staggered row entrance.
- [ ] **3 Toasts** — rounded glass card, accent glow instead of 3px rail, optional dwell bar; align with immersive composite toast ([windows-toast.md](windows-toast.md)).
- [ ] **4 Settings** — ~720×520, floating glass sidebar with icons + pill selection, grouped glass cards, toggles/sliders, lightbar editor polish (rounded bar, round handles, tinted controller preview); share rows with `start/settings.rs`.
- [ ] **5 Compact Start** — glass rows, neutral select, shared hint capsule, optional faint selected hero art. Visual-only; respect start-immersive do-not-regress list.
- [ ] **6 Polish** — icon stroke consistency, hover/press micro-motion, focus rings, README screenshots, CHANGELOG.

## Risks

GPU/power from backdrops in background windows (hence nearly static); per-window shader pipelines + atlas creation; DWM rounding absent on Windows 10; `clippy -D warnings`; `start/view.rs` (6.4k lines) refactors kept separate from restyles.
