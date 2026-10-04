# Investigation notes

Topic handoffs for agents. Prefer the matching sibling over appending unrelated sections to an existing note. New tooling or investigations: add a sibling here (and list it below), or extend the note whose topic matches.

| Note | Status | Purpose | Open this when… |
|------|--------|---------|-----------------|
| [ui-lockup-investigation.md](ui-lockup-investigation.md) | playbook | HID freeze / hitch-hunting: log paths, grep tokens, F7/F8, `hidapi_new` / `hidapi_refresh` stalls, shell-client mark honesty | Chasing UI/pad freezes, extending debug diagnostics, reading hitch marks |
| [windows-bt-lightbar.md](windows-bt-lightbar.md) | historical | Windows DualSense BT lightbar silent no-ops (interrupt pad, control path, Steam claim) | Lightbar writes look `ok` but RGB does not change; BT output transport |
| [windows-toast.md](windows-toast.md) | do-not-regress | Toast z-order, remount, slide state machine, 0→1 Start survive | Toast/Start present starvation, wrong toast content, connect-open races |
| [start-immersive.md](start-immersive.md) | do-not-regress | Immersive Start promote/demote, chord latch, dock, Options settings, art | Immersive cover, reopen chord, controllers dock, Start settings panel |
| [hid-live-session.md](hid-live-session.md) | do-not-regress | Hot-path battery from input stream, silence drop, Start rumble handle | Live pad sampling, disconnect while BT still listed, rumble vs lightbar handles |
| [wgpu-image-atlas-crash.md](wgpu-image-atlas-crash.md) | do-not-regress | iced/wgpu image-atlas `create_renderer` panic | Tray panic + `crash-restart`, atlas / `Texture::create_view` |
