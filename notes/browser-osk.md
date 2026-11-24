# On-screen keyboard and page IME (built)

**Status:** functional, intentionally plain. **When to read:** changing text
entry in the Browser (page fields or URL bar).

## What exists

- **On-screen keyboard overlay** (`ui::browser::OskState` + `browser_osk`
  in `ui/browser/view.rs`). Square toggles it from the Browser surface
  (F6 on a hardware keyboard). While open:
  - D-pad moves the key focus (the existing D-pad purchase stepper),
  - Cross activates the focused key,
  - Triangle backspaces,
  - Square switches Alpha/Symbol,
  - Circle/Esc closes.
- Keys are ordinary iced buttons, so the mouse can click them too.
- Character and Space keys append to the URL draft while URL editing, else
  they synthesize page key events through the same `on_browser_key` path as
  hardware input.
- Enter navigates the URL draft or sends Enter to the page; Backspace edits
  the draft or sends Backspace.
- The overlay is modal for pad input and hides the virtual ring. Physical
  keyboard input keeps working while it is open.

## Page IME

- `ui::input_method::ime_bridge` wraps the Browser root and asks iced/winit to
  enable the platform IME while the page owns input (not URL editing and not
  an automated session).
- `iced::Event::InputMethod` events are routed to `on_browser_ime`:
  - preedit → `EngineCommand::ImeSetComposition`,
  - empty preedit/closed → `ImeCancelComposition`,
  - commit → stage the text as a composition and finish it.
- CEF's windowless `ImeCommitText` path did not insert text in the observed
  156 runtime, so commit uses `ImeSetComposition` + `ImeFinishComposition`
  with an invalid replacement range (`u32::MAX..u32::MAX`) meaning "current
  selection". That inserts at the caret instead of position 0.
- Composition underlines are a single full-text span; visual polish and
  candidate-window placement are intentionally minimal.

## Automation

- `SDSC_DEBUG_INPUT=0` locks real IME/keyboard/mouse input through
  `ui::input_lock`.
- Replay steps `ime.preedit`, `ime.commit`, `osk.toggle`, and `osk.key` call
  the same handlers with synthetic events. `scripts/qa_browser_input.py`
  verifies typed text (`hello`), IME commit (`é`), OSK commit (`!`), wheel
  scroll, drag/double-click selection, and cursor-shape tracing without using
  the OS keyboard/mouse.

## Deferred

- Predictive text, clipboard history, alternate layouts beyond QWERTY +
  symbols, pretty key art, and native OSK-touch-keyboard integration.
