# Pull model contract

Status: current. Read this before adding a window clock, an animated widget, or
a continuous-input consumer.

## The rule

Anything that is a function of elapsed time is advanced from `now` on that
surface's clock. Input edges carry **discrete actions** and **wake-ups** only;
they are not the integrator for analog state.

| Concern | Owner | Don't |
|---|---|---|
| Latest pad value | `InputHub` snapshot (`latest_reading`) | Reconstruct held state from the edge stream |
| Button taps / PS / mode changes | `InputEvent` edge stream | Send analog samples as UI messages |
| Pose, decay, velocity, ken-burns | Surface clock (`FrameDemand`) | Accumulate `dt` in an app message handler |
| Window placement / union size | One geometry helper | Duplicate constants between create/remount |
| Discrete surface updates | `Message` | Repaint globally at 60 Hz |

## Primitives

- `ui::pull::FrameDemand` — `Idle` / `Now` / `At(Instant)`; maps to
  `Shell::request_redraw*`, shader `Action`, or canvas `Action`. Use it for
  every per-window clock request.
- `ui::motion::Timeline` — wall-clock segment with pause support; pose is
  derived, never accumulated.
- `InputHub::latest_reading(max_age)` — preferred live pad reading for
  pull-model consumers. A stale snapshot is neutral.
- `HoldTracker::progress_at(now)` — release decay is derived from
  `released_at`; `NavBank::hold_progress(now)` pulls it without mutating.
- `ui::layout::toast_window_size()` — single source for toast create/remount;
  the union window must stay tall enough for both slide extremes.

## Frame clocks

- Widgets: `FrameDemand::next(now).request(shell)` while moving, then `Idle`.
- Shader/canvas: `FrameDemand::...into_program()` / `into_canvas()`.
- App-level `iced::time::every` is only for real global state (the session
  tick) or a window whose *entire* surface is animating. Prefer a widget clock
  so hidden windows never subscribe.
- Gate a hold/cursor clock on the model's own activity
  (`needs_frames`, `hold_animating`), not on raw pad edges.

## Checklist for a new animated surface

1. Can the pose be derived from `started + now`? If yes, use `Timeline`.
2. Does it need continuous input? Read the latest snapshot, not edges.
3. Does it need a redraw? Request `FrameDemand` from the widget/program.
4. Is it hidden? Do not subscribe or request frames.
5. Does it own a window? Derive create, remount, and hit-test geometry from
   one helper.
6. Add an idle QA assertion and a "held input with no new edges" unit test.
