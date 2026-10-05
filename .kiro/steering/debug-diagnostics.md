---
description: Require debug-only diagnostic logging for new work (lockup-investigation standard)
inclusion: auto
name: debug-diagnostics
---

# Debug-only diagnostics required

All improvements, bugfixes, and features must include a **similar level of non-release diagnostic logging** to the UI lockup / HID investigation. Release builds must stay quiet and cheap.

## Rules

1. Prefer `cfg(debug_assertions)` or helpers that no-op in release (`hid_diag::diag_info` / `diag_warn`, `app_log::hid_trace` / `mark_hitch`). Do **not** leave high-volume HID traces or hitch-report UI enabled in `--release`.

2. When chasing hangs, races, or intermittent failures: add phase markers / slow-op timing (`hid_diag::enter_op`), correlatable log tokens, and enough context to reconstruct a 5–10s window after a user mark.

3. If the UI can freeze, provide a **debug-only** way to stamp a mark that does not rely solely on the iced UI thread (pattern: F7/F8 + `HITCH_MARK` via `app_log::mark_hitch`).

4. Hitch-hunting worked example and shared grep tokens: `notes/ui-lockup-investigation.md`. Catalog of topic notes: `notes/README.md`. Add or extend the matching sibling under `notes/` when landing new investigation tooling; do not append unrelated sections to the lockup note.

5. Keep normal `app.log` warn/error for real user-facing failures in all builds; gate only investigation-volume / hitch-hunting output.

## Reference

- `notes/ui-lockup-investigation.md` — playbook (log paths, grep tokens, F7/F8, hidapi stalls, shell-client hitch marks)
- `notes/README.md` — index of all investigation notes
