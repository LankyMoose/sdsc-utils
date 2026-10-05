---
description: Do-not-regress areas with mandatory reference notes before modifying
inclusion: auto
name: do-not-regress
---

# Do-not-regress areas

These areas have detailed investigation notes that **must be read** before modifying them. Each note documents root causes, fixes applied, and the exact behavior that must be preserved.

| Area                                                        | Note                              | Open when…                                                                     |
| ----------------------------------------------------------- | --------------------------------- | ------------------------------------------------------------------------------ |
| Toast z-order, slide state machine, Start/toast races       | `notes/windows-toast.md`          | Modifying toast presentation, z-order, or Start↔toast interaction              |
| Immersive Start (promote/demote, chord, dock, art, Options) | `notes/start-immersive.md`        | Touching immersive cover, reopen chord, controllers dock, Start settings panel |
| Live HID session (hot-path battery, silence drop, rumble)   | `notes/hid-live-session.md`       | Changing live pad sampling, disconnect handling, rumble vs lightbar handles    |
| wgpu image-atlas crash (`create_renderer` panic)            | `notes/wgpu-image-atlas-crash.md` | Touching tray panic/crash-restart, atlas growth, window open/hide sequencing   |

## Summary of key invariants

### Toast (`notes/windows-toast.md`)

- Toast state is a pure state machine (`src/ui/toast/machine.rs`): Placing → SlidingIn → Resting → SlidingOut
- Do not add window open/close calls outside this machine; they cause present starvation (iced#3108)
- Connect toasts must not show stale content on reuse — the machine resets content atomically with state
- Connected toast on 0→1 auto-open shows **inside** immersive Start, not as a separate desktop card
- Connected toasts are **skipped** for controllers already connected when the app starts

### Immersive Start (`notes/start-immersive.md`)

- Always-immersive is the **default**; a second PS press promotes the compact launcher; Circle demotes (always-immersive closes instead of demoting)
- Opening Start does **not** promote to immersive on the same PS press that opened it
- Promote/demote is a ceremony with defined phases; do not short-circuit it
- Chord latch (PS button) must not fire on partial chords or re-fire after release
- Controllers dock and idle/sleep overlay are independent state machines; keep them independent
- Games strip does **not** wrap; pad navigation stops at the ends
- Connected toast on 0→1 auto-open shows **inside** immersive Start, not as a separate desktop card
- Compact reopen-chord hint is hidden while in temporary immersive Start
- Toasts and file pickers stay **above** immersive Start (z-order)

### HID live session (`notes/hid-live-session.md`)

- Battery is read from the hot-path input stream; do **not** add a separate poll while input is live
- Silence drop (~200ms): if no input report arrives, the pad is dropped from live_pads — do not regress this threshold
- Rumble uses a separate output handle from lightbar; never merge them

### wgpu atlas (`notes/wgpu-image-atlas-crash.md`)

- Do not open a new window and raise it to topmost in the same iced update turn
- Keep the warm toast sentinel logic; cold window open under GPU pressure triggers the atlas panic
- Deferred hide must remain deferred; same-turn raise+hide is not safe
