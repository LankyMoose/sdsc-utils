# Task Organization - PS5 Battery Display (1.6.1 Focus)

## 1.6.1 RELEASE — Small, Focused Fixes Only

These tasks are appropriate for 1.6.1 (bug fixes, small improvements, polish):

### ✅ TASK A: Presence/Toast Behavior — Bullet-Proof
**Priority: HIGH — Bug fix for 1.6.1**
- [ ] Verify current behavior: when 'presence' is newly detected and will open immersive mode → do NOT show 'connected' toast
- [ ] Currently there's a difference between:
  - Cold-launch with controller already on (no connect toast)
  - Turning on a controller afterwards (shows connect toast)
- [ ] Make bullet-proof: unify the behavior so both scenarios are consistent
- [ ] Fix: if presence detects a controller that will trigger immersive mode, skip the connect toast entirely
- [ ] Check `src/service/mod.rs` and toast generation logic
- [ ] The fix should handle both cold-start and hot-restart cases

### ✅ TASK B: Start Screen — Two Independent Settings
**Priority: HIGH — Setting separation for 1.6.1**
- [ ] Separate 'connecting a controller opens the start screen' into two settings, both on by default:
  - **Setting 1: Start Screen Enabled** (toggle) — whether the start screen feature is active at all
  - **Setting 2: Controller Connection Opens Start Screen** (toggle) — whether connecting a controller triggers the start screen
- [ ] Both should default to `true`
- [ ] Currently these are likely combined into one gate
- [ ] Add new pref key in `prefs.json` for `start_screen_enabled` (bool, default true)
- [ ] Modify the connection logic to check both settings
- [ ] Update the configure UI to have two separate toggles

### ✅ TASK C: Center-Top Clock Widget
**Priority: MEDIUM — Small UI addition for 1.6.1**
- [ ] Add center-top 'clock' widget that uses well-formatted system time
- [ ] Display time (e.g., "3:45 PM", "14:45")
- [ ] Position at center-top of the immersive start screen or main UI
- [ ] May use iced `time` widget or custom rendering
- [ ] Add to the immersive start screen drawer/overlay

---

## 1.7.0 RELEASE — Deferred to Next Major Version

These tasks are too large for 1.6.1 and should be deferred:

### 📦 TASK 1: Immersive Layout Configurability (DEFERRED to 1.7.0)
- Add horizontal mode toggle for immersive layout
- Anchor status chips to top-left *(only when horizontal mode enabled)*
- Reverse slide animations/content order *(only when horizontal mode enabled)*
- **Reason:** Requires significant UI rework, better suited for 1.7.0

### 📦 TASK 2: Changelog Scrollable Modal in Configure UI (DEFERRED to 1.7.0)
- Add scrollable modal display in the main configure UI
- Include all release notes, well separated
- **Reason:** Feature addition, not a bug fix

### 📦 TASK 3: Bring Immersive UI Style/Theming to Full UI (DEFERRED to 1.7.0)
- Apply newer UI style from immersive mode to all screens
- **Reason:** Theme consistency work, better as a larger refactor

### 📦 TASK 4: Immersive Navigation — Section-by-Section for Massive Libraries (DEFERRED to 1.7.0)
- After holding for >1sec, move by section instead of game-by-game
- Stop rendering non-first games per section for performance
- **Reason:** Performance optimization, requires library scanning changes

### 📦 TASK 5: Extra Sources for Steam Scan (DEFERRED to 1.7.0)
- Enable adding extra sources for Steam scan beyond default
- **Reason:** Feature addition, scan logic changes

### 📦 TASK 6: Game Close Status Text + Disable Uninstalled Games (DEFERRED to 1.7.0)
- Change status from 'playing' to 'closing' when closing a game
- Disable game rows if scan determines they are no longer installed
- Handle immediate launch → invalid game launch gracefully
- **Reason:** Multiple game launch/state changes, better as 1.7.0

### 📦 TASK 7: Battery/Connect Edge Case Replication (DEFERRED to 1.7.0)
- First controller low battery → connecting second controller causes 'blip' → first periodically dc's
- Replicate with local dev build and flag hitch
- **Reason:** Complex HID/battery interaction, needs dedicated investigation

---

## Branch Structure

```
feat/1.6.1  ← contains TASK_LIST.md (current branch)
task/a-toast-behavior          ← Task A: Presence/toast fix
task/b-start-screen-settings   ← Task B: Two settings separation
task/c-clock-widget            ← Task C: Center-top clock

# 1.7.0 tasks would be on separate branches:
# task/1-immersive-layout
# task/2-changelog-modal
# etc.
```

## Currently Ready

You're on **`feat/1.6.1`** with the task list committed. The three 1.6.1-appropriate tasks (A, B, C) are ready to start. Task A (toast behavior) is the highest priority bug fix.

**Would you like to start with Task A — the Presence/Toast behavior bullet-proof fix?** This is the most critical 1.6.1 fix since it addresses inconsistent toast behavior between cold-launch and hot-controller-on scenarios.