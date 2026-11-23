# On-screen keyboard (plan, not built)

**Status:** design only. **When to read:** adding controller text entry to
the browser (or anywhere else). After Home pivot
([browser-home-pivot.md](browser-home-pivot.md)) the OSK belongs to the Browser
app's own window, not Launcher.

## Goal

Controller-only text entry, URL bar first. Physical keyboards keep working
unchanged (the address field is a real `text_input`); the OSK is a parallel
commit path, not a replacement.

## Why a surface, not a widget trick

Launcher already composites modal surfaces over the stage with one owner at a
time (`HintPressOwner`, overlay priority in `notes/launcher-immersive.md`).
After the pivot the Browser app owns its own window and the same modal-surface
pattern; the OSK becomes one more Browser surface (`OskPanel` next to the
browser panel), so pad ownership, Cancel-unwind, and idle-dim keep working
unchanged.

## Proposed shape (URL bar, phase 1)

- Grid QWERTY (3 rows + space row), one symbols layer toggled in place.
  Wheel layouts (retsurf-style) are nicer on sticks but cost a custom
  canvas; grid reuses buttons and the existing focus-chase patterns.
- State: `open: bool`, `focus: (row, col)`, `layer: Alpha | Symbol`.
  Lives in the Browser app's panel while URL-only; promotes to its own
  `State` field if page-field IME ever needs it.
- Pad while open (owns input like the browser surface does):
  D-pad moves cell focus (wrap), Cross commits the char into the URL draft
  (`BrowserUrlChanged`), Square toggles Alpha/Symbol, Triangle backspaces,
  Circle closes (draft kept). Cross on `Go`/`space` wide keys behaves the
  same as a commit.
- Rendering: centered card above the browser wash (same `overlay` builder),
  below the settings drawer. Hint row shows the OSK bindings via a new
  `HintPressOwner::Osk` so the browser hints go idle underneath.
- Coexistence with physical typing: commits go through `BrowserUrlChanged`,
  the same message the hardware path uses — both can interleave and the
  last writer wins per keystroke, exactly like pad-vs-mouse today.

## Explicitly deferred

- Page-field IME (`InputEvent::InputMethod` / `notify_input_method`): needs
  engine focus tracking (`notify_focus_changed` → which element) plus an
  OSK mode that commits into the page instead of the draft. URL-first keeps
  the engine side to key events only.
- Character layouts beyond QWERTY + symbols (retsurf ships ЙЦУКЕН too):
  add when a second handheld locale asks for it, not before.
- Predictive text / clipboard: out of scope for a URL bar.

## Build order (when approved)

1. `OskPanel` + messages (`ToggleOsk`, `OskCommit(char)`, focus moves as
   plain pad nav inside the surface, no `NavAction` changes).
2. `osk_view` card + overlay layer + `HintPressOwner::Osk`.
3. Wire Triangle (browser surface) / long-press? to open; Circle closes.
4. Tests: focus wrap, layer toggle, commit appends, close keeps draft.
