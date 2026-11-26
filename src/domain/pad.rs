//! Pad samples, input snapshot, and launcher nav banks.
//!
//! PadPoll never opens HID on the UI thread — it clones the latest samples published
//! by [`crate::controller::hid::worker`]. The worker interleaves Identify lightbar writes with
//! short input reads on the same handle so nav stays live during flashes.

use crate::controller::hid::policy::Policy;
use crate::domain::gesture::{GestureControl, GestureDetector};
use hidapi::HidDevice;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Per-axis deadzone applied to each stick before combining (kills rest bias).
const STICK_AXIS_DEADZONE: f32 = 0.9;
/// Residual deadzone on the combined vector after per-stick cleaning.
const STICK_COMBINED_DEADZONE: f32 = 0.15;
const NAV_INITIAL_DELAY: Duration = Duration::from_millis(280);
const NAV_REPEAT: Duration = Duration::from_millis(90);
/// Consecutive raw reports a digital button must hold a new state before the
/// source-level interpreter accepts it. Report-rate (~4ms), so this is a
/// sub-frame signal filter, not a user-visible delay.
const BUTTON_STABLE_REPORTS: u8 = 3;
/// Neutral-dropout grace: a neutral sample held this long is a deliberate
/// release (reset gating); shorter gaps are threshold flicker and keep the
/// active direction + repeat schedule. Longer than several input-report
/// intervals (4ms) and UI ticks, shorter than the initial delay so a fresh
/// press after a real release still fires at once.
const NAV_NEUTRAL_GRACE: Duration = Duration::from_millis(100);
/// One wired DualSense report interval (`bInterval` 4). Block this long when the queue is empty.
pub const INPUT_REPORT_WAIT_MS: i32 = Policy::DEFAULT.report_wait_ms;

/// Owned pad-input unit: the hid-worker publication (snapshot) plus the
/// async edge bridge the iced subscription drains.
///
/// Owned by [`crate::controller::hid::worker::HidWorkerHandle`] (one `Arc`
/// clone moves into the worker thread) and by [`crate::app::App`]; call sites
/// take `&InputHub` / `&Arc<InputHub>` — no process-global instance.
pub struct InputHub {
    snapshot: Mutex<InputSnapshot>,
    /// Async bridge: hid-worker `try_send`s edges; iced subscription drains them.
    edge_tx: Mutex<Option<iced::futures::channel::mpsc::Sender<InputEdge>>>,
    /// Display-rate / meaningful-change gate for analog edges (actions bypass).
    gate: Mutex<EdgeGate>,
    /// Latest all-pad analog snapshot for the pull-model OSK widget.
    osk_slot: Arc<OskInputSlot>,
}

/// Analog edge gate state (hid-worker side).
#[derive(Debug, Default)]
struct EdgeGate {
    last: std::collections::HashMap<PadId, PadSample>,
    last_push: Option<Instant>,
    neutral_tail_until: Option<Instant>,
}

/// One display-rate analog publication plus release-tail bookkeeping.
const ANALOG_PUBLISH_INTERVAL: Duration = Duration::from_millis(16);
/// Keep publishing neutral samples long enough for [`NavProfile::neutral_grace`]
/// (100ms) to expire on a stepper that only sees pad edges.
const ANALOG_NEUTRAL_TAIL: Duration = Duration::from_millis(140);
/// Stick movement below this is report noise, not a meaningful change.
const ANALOG_STICK_EPSILON: f32 = 0.02;

impl InputHub {
    pub fn new() -> Self {
        Self {
            snapshot: Mutex::new(InputSnapshot::default()),
            edge_tx: Mutex::new(None),
            gate: Mutex::new(EdgeGate::default()),
            osk_slot: Arc::new(OskInputSlot::default()),
        }
    }

    /// Direct snapshot access for the hid-worker hot paths that must hold
    /// one lock across read-modify-write (Identify merge, power-off drop).
    pub fn snapshot_mutex(&self) -> &Mutex<InputSnapshot> {
        &self.snapshot
    }

    /// Store fresh worker readings (bump seq, stamp, replace); returns the
    /// meta the edge bridge attaches. Lock scope matches the old inline block.
    pub fn publish(&self, readings: Vec<NavReading>, reason: SnapshotReason) -> SnapshotMeta {
        if let Ok(mut guard) = self.snapshot.lock() {
            guard.seq = guard.seq.saturating_add(1);
            guard.published_at = Instant::now();
            guard.reason = reason;
            guard.readings = readings;
            self.osk_slot.publish(&guard.readings, guard.seq);
            SnapshotMeta {
                published_at: guard.published_at,
                seq: guard.seq,
                reason: guard.reason,
                pad_count: guard.readings.len(),
            }
        } else {
            SnapshotMeta {
                published_at: Instant::now(),
                seq: 0,
                reason,
                pad_count: 0,
            }
        }
    }

    /// PadPoll entry: O(1) clone of hid-worker snapshot (never opens HID on the UI thread).
    pub fn read(&self) -> NavReadingsOutcome {
        let Ok(guard) = self.snapshot.lock() else {
            return NavReadingsOutcome::Missing {
                meta: SnapshotMeta {
                    published_at: Instant::now(),
                    seq: 0,
                    reason: SnapshotReason::LockPoisoned,
                    pad_count: 0,
                },
            };
        };
        let meta = SnapshotMeta {
            published_at: guard.published_at,
            seq: guard.seq,
            reason: guard.reason,
            pad_count: guard.readings.len(),
        };
        if guard.readings.is_empty() {
            NavReadingsOutcome::Missing { meta }
        } else {
            log_source_once(NAV_SOURCE_HID, guard.readings.len());
            NavReadingsOutcome::Readings {
                readings: guard.readings.clone(),
                meta,
            }
        }
    }

    /// Latest preferred reading for pull-model consumers (cursor, holds).
    ///
    /// The snapshot is published by the worker at report rate regardless of
    /// whether an edge subscription is active. `max_age` treats a stalled or
    /// disconnected worker as neutral so a stale held stick cannot steer.
    pub fn latest_reading(&self, max_age: Duration) -> Option<NavReading> {
        let guard = self.snapshot.lock().ok()?;
        if guard.published_at.elapsed() > max_age {
            return None;
        }
        preferred_reading(&guard.readings).cloned()
    }

    /// Install the iced-side sender so the worker can push edges (subscription start).
    pub fn bind(&self, tx: iced::futures::channel::mpsc::Sender<InputEdge>) {
        if let Ok(mut guard) = self.edge_tx.lock() {
            *guard = Some(tx);
        }
        // A fresh listener must see the next sample even if the worker state
        // has not changed (e.g. OSK opens while a stick is held at rest).
        if let Ok(mut gate) = self.gate.lock() {
            *gate = EdgeGate::default();
        }
    }

    /// Clear the iced-side sender (subscription end / shutdown).
    pub fn clear(&self) {
        if let Ok(mut guard) = self.edge_tx.lock() {
            *guard = None;
        }
    }

    /// Push a sample edge to the UI bridge (no-op when nothing is listening).
    ///
    /// Actions (taps/holds) publish immediately. Analog state is gated to
    /// display rate plus meaningful change; a neutral tail keeps release
    /// semantics intact for steppers that only see edges.
    pub fn push_edge(&self, edge: InputEdge) {
        self.push_edge_at(edge, Instant::now());
    }

    fn push_edge_at(&self, edge: InputEdge, now: Instant) {
        if self.edge_tx.lock().map(|g| g.is_none()).unwrap_or(true) {
            return;
        }
        let Ok(mut gate) = self.gate.lock() else {
            return;
        };
        let digital_change = digital_changed(&gate.last, &edge.readings);
        let stick_change = stick_changed(&gate.last, &edge.readings);
        let all_resting = edge
            .readings
            .iter()
            .all(|reading| sample_nav_resting(&reading.sample));
        let was_resting = gate.last.values().all(sample_nav_resting);
        if !all_resting && was_resting {
            gate.neutral_tail_until = None;
        } else if all_resting && !was_resting {
            gate.neutral_tail_until = Some(now + ANALOG_NEUTRAL_TAIL);
        }
        let immediate = !edge.actions.is_empty()
            || !matches!(
                edge.reason,
                SnapshotReason::Sample | SnapshotReason::SamplePartial
            )
            || digital_change;
        let due = gate
            .last_push
            .is_none_or(|last| now.saturating_duration_since(last) >= ANALOG_PUBLISH_INTERVAL);
        let tail_active = gate.neutral_tail_until.is_some_and(|until| now <= until);
        let should_push = immediate
            || (due && (!all_resting || tail_active) && (stick_change || !all_resting))
            || (due && tail_active);
        if !should_push {
            return;
        }
        gate.last = edge
            .readings
            .iter()
            .map(|reading| (reading.id.clone(), reading.sample.clone()))
            .collect();
        gate.last_push = Some(now);
        if all_resting && !tail_active {
            gate.neutral_tail_until = None;
        }
        if let Ok(mut guard) = self.edge_tx.lock()
            && let Some(tx) = guard.as_mut()
        {
            // Prefer a fresh sample over blocking the HID thread.
            let _ = tx.try_send(edge);
        }
    }

    /// Pull-model OSK source. Enabled while the overlay is open; the keyboard
    /// widget polls it directly instead of receiving `PadInput` messages.
    pub fn osk_slot(&self) -> Arc<OskInputSlot> {
        Arc::clone(&self.osk_slot)
    }

    /// Toggle analog publication into the pull-model OSK slot.
    pub fn set_osk_listening(&self, enabled: bool) {
        self.osk_slot.set_enabled(enabled);
    }

    /// True while the OSK owns the pad (edge stream becomes wake-only).
    pub fn osk_listening(&self) -> bool {
        self.osk_slot.is_enabled()
    }
}

/// Latest all-pad analog state for the pull-model OSK widget.
///
/// Fed from every HID publication (not the gated edge stream): the OSK needs
/// the current stick/trigger state at display rate without a global message.
#[derive(Debug, Default)]
pub struct OskInputSlot {
    state: Mutex<OskSlotState>,
    enabled: std::sync::atomic::AtomicBool,
}

#[derive(Debug, Default)]
struct OskSlotState {
    seq: u64,
    readings: Vec<NavReading>,
    /// Debug replay frames, consumed one per widget poll (press then release).
    replay: std::collections::VecDeque<Vec<NavReading>>,
}

impl OskInputSlot {
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn publish(&self, readings: &[NavReading], seq: u64) {
        if !self.enabled.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        if let Ok(mut guard) = self.state.lock() {
            guard.seq = seq;
            guard.readings = readings.to_vec();
        }
    }

    /// Debug replay: queue a synthetic frame for the pull-model widget.
    #[cfg(debug_assertions)]
    pub fn push_replay(&self, readings: Vec<NavReading>) {
        if let Ok(mut guard) = self.state.lock() {
            guard.replay.push_back(readings);
        }
    }

    /// Latest `(sequence, readings)`; a sequence change means new analog data.
    pub fn read(&self) -> (u64, Vec<NavReading>) {
        match self.state.lock() {
            Ok(mut guard) => {
                if let Some(readings) = guard.replay.pop_front() {
                    guard.seq = guard.seq.saturating_add(1);
                    return (guard.seq, readings);
                }
                (guard.seq, guard.readings.clone())
            }
            Err(_) => (0, Vec::new()),
        }
    }
}

fn digital_changed(
    last: &std::collections::HashMap<PadId, PadSample>,
    readings: &[NavReading],
) -> bool {
    if last.len() != readings.len() {
        return true;
    }
    readings.iter().any(|reading| {
        let Some(previous) = last.get(&reading.id) else {
            return true;
        };
        let a = previous;
        let b = &reading.sample;
        a.cross != b.cross
            || a.circle != b.circle
            || a.square != b.square
            || a.triangle != b.triangle
            || a.options != b.options
            || a.create != b.create
            || a.l1 != b.l1
            || a.r1 != b.r1
            || a.l3 != b.l3
            || a.l2 != b.l2
            || a.r2 != b.r2
            || a.dpad_up != b.dpad_up
            || a.dpad_down != b.dpad_down
            || a.dpad_left != b.dpad_left
            || a.dpad_right != b.dpad_right
            || a.held != b.held
    })
}

fn stick_changed(
    last: &std::collections::HashMap<PadId, PadSample>,
    readings: &[NavReading],
) -> bool {
    if last.len() != readings.len() {
        return true;
    }
    readings.iter().any(|reading| {
        let Some(previous) = last.get(&reading.id) else {
            return true;
        };
        let a = previous;
        let b = &reading.sample;
        (a.stick_x - b.stick_x).abs() > ANALOG_STICK_EPSILON
            || (a.stick_y - b.stick_y).abs() > ANALOG_STICK_EPSILON
            || (a.lstick_x - b.lstick_x).abs() > ANALOG_STICK_EPSILON
            || (a.lstick_y - b.lstick_y).abs() > ANALOG_STICK_EPSILON
            || (a.rstick_x - b.rstick_x).abs() > ANALOG_STICK_EPSILON
            || (a.rstick_y - b.rstick_y).abs() > ANALOG_STICK_EPSILON
    })
}

impl Default for InputHub {
    fn default() -> Self {
        Self::new()
    }
}

impl std::hash::Hash for InputHub {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // Subscription identity: `Arc` clones share the heap allocation, so
        // hash the allocation address (stable across clones, distinct per Home).
        (self as *const Self as usize).hash(state);
    }
}

/// One pad-input publication from the hid-worker (replaces UI PadPoll timer).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InputEdge {
    pub readings: Vec<NavReading>,
    /// Ordered source-side button taps generated since the previous edge.
    pub actions: Vec<PadAction>,
    pub seq: u64,
    pub reason: SnapshotReason,
    pub pad_count: usize,
}

impl InputEdge {
    pub fn from_snapshot(
        readings: Vec<NavReading>,
        actions: Vec<PadAction>,
        meta: &SnapshotMeta,
    ) -> Self {
        Self {
            readings,
            actions,
            seq: meta.seq,
            reason: meta.reason,
            pad_count: meta.pad_count,
        }
    }

    pub fn meta(&self) -> SnapshotMeta {
        SnapshotMeta {
            published_at: Instant::now(),
            seq: self.seq,
            reason: self.reason,
            pad_count: self.pad_count,
        }
    }
}

/// Why the worker last published (or cleared) the input snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SnapshotReason {
    Sample,
    SamplePartial,
    /// Cleared before a battery/liveness Poll (legacy; Poll no longer clears the snapshot).
    #[allow(dead_code)]
    ClearedPoll,
    ClearedPowerOff,
    ClearedShutdown,
    Identify,
    LockPoisoned,
    NeverPublished,
}

impl SnapshotReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sample => "Sample",
            Self::SamplePartial => "SamplePartial",
            Self::ClearedPoll => "ClearedPoll",
            Self::ClearedPowerOff => "ClearedPowerOff",
            Self::ClearedShutdown => "ClearedShutdown",
            Self::Identify => "Identify",
            Self::LockPoisoned => "LockPoisoned",
            Self::NeverPublished => "NeverPublished",
        }
    }
}

/// Shared pad samples published by the hid-worker.
#[derive(Debug, Clone)]
pub struct InputSnapshot {
    pub readings: Vec<NavReading>,
    pub published_at: Instant,
    pub seq: u64,
    pub reason: SnapshotReason,
}

impl Default for InputSnapshot {
    fn default() -> Self {
        Self {
            readings: Vec::new(),
            published_at: Instant::now(),
            seq: 0,
            reason: SnapshotReason::NeverPublished,
        }
    }
}

/// Metadata returned with every PadPoll snapshot read.
#[derive(Debug, Clone)]
pub struct SnapshotMeta {
    pub published_at: Instant,
    pub seq: u64,
    pub reason: SnapshotReason,
    #[allow(dead_code)] // exposed for UI/diag consumers
    pub pad_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NavAction {
    Up,
    Down,
    Confirm,
    Cancel,
    Left,
    Right,
    /// L1 bumper edge — history back in the browser; ignored elsewhere.
    /// Fires on release (hold-vs-tap disambiguation for the B2.4 history
    /// selector: a hold past [`BUMPER_HOLD`] opens the selector instead).
    BumperLeft,
    /// R1 bumper edge — history forward in the browser; ignored elsewhere.
    /// Fires on release, like [`NavAction::BumperLeft`].
    BumperRight,
    /// L1 held past the threshold — open the back-history selector.
    BumperHoldLeft,
    /// R1 held past the threshold — open the forward-history selector.
    BumperHoldRight,
    ToggleEdit,
    CycleSort,
    /// L3 click — toggle virtual-cursor magnetism (browser only).
    ToggleMagnetism,
    /// Triangle edge — edit selected manual while in edit mode.
    Triangle,
    /// Options — open/close Launcher settings.
    ToggleSettings,
    /// Create (Select) — legacy controllers-surface toggle (inert: the PS Home
    /// owns the controllers strip now).
    ToggleControllers,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PadSample {
    pub held: BTreeSet<GestureControl>,
    /// Combined stick X after deadzone; negative = left. Non-zero means past deadzone.
    ///
    /// Legacy sum of both sticks (games nav reads this; unchanged).
    pub stick_x: f32,
    /// Combined stick Y after deadzone; negative = up. Non-zero means past deadzone.
    pub stick_y: f32,
    /// Left stick alone (browser cursor). Same deadzone feel as the combined
    /// path, without the right stick folded in.
    #[serde(default)]
    pub lstick_x: f32,
    #[serde(default)]
    pub lstick_y: f32,
    /// Right stick alone (browser scroll).
    #[serde(default)]
    pub rstick_x: f32,
    #[serde(default)]
    pub rstick_y: f32,
    pub dpad_up: bool,
    pub dpad_down: bool,
    pub dpad_left: bool,
    pub dpad_right: bool,
    pub cross: bool,
    pub circle: bool,
    pub square: bool,
    pub triangle: bool,
    pub options: bool,
    pub create: bool,
    pub l2: bool,
    pub r2: bool,
    /// L1/R1 bumpers (browser history). Parsed but previously dropped.
    #[serde(default)]
    pub l1: bool,
    #[serde(default)]
    pub r1: bool,
    /// L3 click (browser magnetism toggle). Parsed but previously dropped.
    #[serde(default)]
    pub l3: bool,
}

/// Which backend produced a launcher nav sample.
///
/// Single-backend stub: HID is the only producer. A `&'static str` const (not an
/// enum) so `NavReading.source`, `format_line`, `log_source_once`, and the one-shot
/// `Option<&'static str>` log guard keep working unchanged.
pub const NAV_SOURCE_HID: &str = "hid";

fn default_nav_source() -> &'static str {
    NAV_SOURCE_HID
}

/// Stable pad identity for per-controller edge / hold / gesture state.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct PadId(pub String);

impl PadId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Digital buttons interpreted at the HID source. L1/R1 stay on the legacy
/// bumper tracker until that moves to event actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PadButton {
    Cross,
    Circle,
    Square,
    Triangle,
    Options,
    Create,
    L1,
    R1,
    L3,
    R3,
}

impl PadButton {
    pub const COUNT: usize = 10;
    pub const ALL: [Self; Self::COUNT] = [
        Self::Cross,
        Self::Circle,
        Self::Square,
        Self::Triangle,
        Self::Options,
        Self::Create,
        Self::L1,
        Self::R1,
        Self::L3,
        Self::R3,
    ];
    const L1_INDEX: usize = 6;
    const R1_INDEX: usize = 7;

    fn raw(self, sample: &PadSample) -> bool {
        match self {
            Self::Cross => sample.cross,
            Self::Circle => sample.circle,
            Self::Square => sample.square,
            Self::Triangle => sample.triangle,
            Self::Options => sample.options,
            Self::Create => sample.create,
            Self::L1 => sample.l1,
            Self::R1 => sample.r1,
            Self::L3 => sample.l3,
            Self::R3 => sample.held.contains(&GestureControl::R3),
        }
    }

    /// R3's issue flag is a press action; every other tap resolves on release
    /// so a completed press -> release is required.
    fn tap_on_press(self) -> bool {
        matches!(self, Self::R3)
    }

    /// Bumpers use the shared tap/hold state machine rather than the generic
    /// tap path (both-held and side-switch semantics live there).
    fn is_bumper(self) -> bool {
        matches!(self, Self::L1 | Self::R1)
    }

    /// Face/Options/Create presses cancel each other's pending tap, matching
    /// the old release-cancellation discipline. Bumpers/L3/R3 are independent.
    fn cancels_others(self) -> bool {
        !matches!(self, Self::L1 | Self::R1 | Self::L3 | Self::R3)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PadButtonAction {
    Tap,
    /// Bumper held past [`BUMPER_HOLD`]; the matching release emits nothing.
    Hold,
}

/// One semantic button action from the source interpreter. Ordered and
/// debounced before it ever reaches a window-specific handler.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PadAction {
    pub pad: PadId,
    pub button: PadButton,
    pub action: PadButtonAction,
}

#[derive(Debug, Clone, Copy, Default)]
struct ButtonDebounce {
    raw: bool,
    stable: bool,
    count: u8,
}

impl ButtonDebounce {
    /// Accept `raw` only after it survives [`BUTTON_STABLE_REPORTS`]
    /// consecutive reports. Returns the accepted state transition.
    fn update(&mut self, raw: bool) -> Option<bool> {
        if raw == self.raw {
            self.count = self.count.saturating_add(1);
        } else {
            self.raw = raw;
            self.count = 1;
        }
        if self.count >= BUTTON_STABLE_REPORTS && self.stable != raw {
            self.stable = raw;
            return Some(raw);
        }
        None
    }
}

/// Per-pad source-side tap interpreter. Feeds on the ordered reports of one
/// drain (not on a pre-merged snapshot) so batch boundaries cannot create or
/// erase a press/release edge.
#[derive(Debug, Clone, Default)]
pub struct PadInterpreter {
    buttons: [ButtonDebounce; PadButton::COUNT],
    cancelled: [bool; PadButton::COUNT],
    bumpers: BumperHold,
}

impl PadInterpreter {
    /// Test/legacy convenience: process reports with a fresh clock.
    pub fn ingest(&mut self, pad: &PadId, reports: &[PadSample]) -> Vec<PadAction> {
        self.ingest_at(pad, reports, Instant::now())
    }

    pub fn ingest_at(
        &mut self,
        pad: &PadId,
        reports: &[PadSample],
        now: Instant,
    ) -> Vec<PadAction> {
        let mut actions = Vec::new();
        for sample in reports {
            let was_stable: [bool; PadButton::COUNT] =
                std::array::from_fn(|index| self.buttons[index].stable);
            let mut transitions = Vec::with_capacity(PadButton::COUNT);
            for (index, button) in PadButton::ALL.into_iter().enumerate() {
                if let Some(pressed) = self.buttons[index].update(button.raw(sample)) {
                    transitions.push((index, pressed));
                }
            }
            // A foreign face/Options/Create press cancels another held
            // button's pending release, before any taps for this report fire.
            for &(index, pressed) in &transitions {
                if pressed && PadButton::ALL[index].cancels_others() {
                    for (other, stable) in was_stable.iter().enumerate() {
                        if other != index && PadButton::ALL[other].cancels_others() && *stable {
                            self.cancelled[other] = true;
                        }
                    }
                }
            }
            for (index, pressed) in transitions {
                let button = PadButton::ALL[index];
                if button.is_bumper() {
                    continue;
                }
                let tap = if pressed {
                    button.tap_on_press()
                } else {
                    let tap = !self.cancelled[index];
                    self.cancelled[index] = false;
                    tap
                };
                if tap {
                    actions.push(PadAction {
                        pad: pad.clone(),
                        button,
                        action: PadButtonAction::Tap,
                    });
                }
            }
        }
        // Bumpers keep their original at-most-one-side tap/hold semantics.
        if let Some(event) = self.bumpers.update(
            self.buttons[PadButton::L1_INDEX].stable,
            self.buttons[PadButton::R1_INDEX].stable,
            now,
        ) {
            let (button, action) = match event {
                BumperHoldEvent::Tap(BumperSide::Left) => (PadButton::L1, PadButtonAction::Tap),
                BumperHoldEvent::Tap(BumperSide::Right) => (PadButton::R1, PadButtonAction::Tap),
                BumperHoldEvent::Open(BumperSide::Left) => (PadButton::L1, PadButtonAction::Hold),
                BumperHoldEvent::Open(BumperSide::Right) => (PadButton::R1, PadButtonAction::Hold),
            };
            actions.push(PadAction {
                pad: pad.clone(),
                button,
                action,
            });
        }
        actions
    }
}

/// Per-serial interpreters owned by the HID worker for the current live pads.
#[derive(Debug, Default)]
pub struct PadInterpreterBank {
    pads: HashMap<PadId, PadInterpreter>,
}

impl PadInterpreterBank {
    pub fn ingest(&mut self, pad: &PadId, reports: &[PadSample]) -> Vec<PadAction> {
        self.ingest_at(pad, reports, Instant::now())
    }

    pub fn ingest_at(
        &mut self,
        pad: &PadId,
        reports: &[PadSample],
        now: Instant,
    ) -> Vec<PadAction> {
        self.pads
            .entry(pad.clone())
            .or_default()
            .ingest_at(pad, reports, now)
    }

    pub fn retain_live(&mut self, live: &HashSet<PadId>) {
        self.pads.retain(|id, _| live.contains(id));
    }
}

/// UI-side delivery gate for source-side actions: monotonic sequence dedupe
/// plus owner-change arming. A pad must reach full rest after a surface takes
/// over before its taps are accepted.
#[derive(Debug, Default)]
pub struct PadActionGate {
    last_seq: u64,
    armed: HashSet<PadId>,
}

impl PadActionGate {
    /// Returns `None` for a duplicate/stale edge; otherwise the actions that
    /// are allowed through, in order.
    pub fn begin(
        &mut self,
        seq: u64,
        reset_arming: bool,
        readings: &[NavReading],
        actions: &[PadAction],
    ) -> Option<Vec<PadAction>> {
        if seq <= self.last_seq {
            return None;
        }
        self.last_seq = seq;
        if reset_arming {
            self.armed.clear();
        }
        let live: HashSet<PadId> = readings.iter().map(|r| r.id.clone()).collect();
        self.armed.retain(|id| live.contains(id));
        let accepted = actions
            .iter()
            .filter(|action| self.armed.contains(&action.pad))
            .cloned()
            .collect();
        for reading in readings {
            if sample_nav_resting(&reading.sample) {
                self.armed.insert(reading.id.clone());
            }
        }
        Some(accepted)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NavReading {
    pub id: PadId,
    pub sample: PadSample,
    // In-memory only (hid-worker snapshot / edge bridge; never persisted — see
    // `persist::store`, which has no `NavReading` callers). `&'static str`
    // cannot borrow from a deserializer, so the wire value is ignored and the
    // single-backend const is restored on deserialize.
    #[serde(skip_deserializing, default = "default_nav_source")]
    pub source: &'static str,
}

/// Coarse stick band for edge-triggered diagnostics (not every poll).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StickBand {
    #[default]
    Neutral,
    Up,
    Down,
    Left,
    Right,
}

impl StickBand {
    pub fn from_stick_y(stick_y: f32) -> Self {
        if stick_y < 0.0 {
            Self::Up
        } else if stick_y > 0.0 {
            Self::Down
        } else {
            Self::Neutral
        }
    }

    /// Dominant-axis band from combined stick X/Y (ties prefer vertical).
    pub fn from_stick(stick_x: f32, stick_y: f32) -> Self {
        let ax = stick_x.abs();
        let ay = stick_y.abs();
        if ax == 0.0 && ay == 0.0 {
            Self::Neutral
        } else if ax > ay {
            if stick_x < 0.0 {
                Self::Left
            } else {
                Self::Right
            }
        } else if stick_y < 0.0 {
            Self::Up
        } else if stick_y > 0.0 {
            Self::Down
        } else {
            Self::Neutral
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Neutral => "neutral",
            Self::Up => "up",
            Self::Down => "down",
            Self::Left => "left",
            Self::Right => "right",
        }
    }
}

/// Snapshot of nav-relevant bits for change detection / logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NavLogSnapshot {
    pub cross: bool,
    pub circle: bool,
    pub dpad_up: bool,
    pub dpad_down: bool,
    pub dpad_left: bool,
    pub dpad_right: bool,
    pub l2: bool,
    pub r2: bool,
    pub stick: StickBand,
    pub resting: bool,
}

impl NavLogSnapshot {
    pub fn from_sample(sample: &PadSample) -> Self {
        Self {
            cross: sample.cross,
            circle: sample.circle,
            dpad_up: sample.dpad_up,
            dpad_down: sample.dpad_down,
            dpad_left: sample.dpad_left,
            dpad_right: sample.dpad_right,
            l2: sample.l2,
            r2: sample.r2,
            stick: StickBand::from_stick(sample.stick_x, sample.stick_y),
            resting: sample_nav_resting(sample),
        }
    }

    pub fn format_line(self, source: &str) -> String {
        format!(
            "start-nav: src={} cross={} circle={} l2={} r2={} dpad_up={} dpad_down={} dpad_l={} dpad_r={} stick={} resting={}",
            source,
            u8::from(self.cross),
            u8::from(self.circle),
            u8::from(self.l2),
            u8::from(self.r2),
            u8::from(self.dpad_up),
            u8::from(self.dpad_down),
            u8::from(self.dpad_left),
            u8::from(self.dpad_right),
            self.stick.as_str(),
            u8::from(self.resting),
        )
    }
}

/// Shared navigation profile. One place for the menu deadzone/repeat feel and
/// the surface-specific variants; call sites must not fork thresholds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NavProfile {
    /// Per-axis deadzone applied before combining / using a stick axis.
    pub axis_deadzone: f32,
    /// Residual deadzone on the combined stick vector magnitude.
    pub combined_deadzone: f32,
    pub repeat_delay: Duration,
    pub repeat_interval: Duration,
    pub neutral_grace: Duration,
    /// Keep the currently active axis on an equal-magnitude diagonal (menu).
    /// When false, the vertical axis wins ties.
    pub axis_lock: bool,
    /// Emit a step cue for first steps (surface maps it to `UiSoundKind::Nav`).
    pub step_feedback: bool,
    /// Emit a cue for repeat steps.
    pub repeat_feedback: bool,
}

impl Default for NavProfile {
    fn default() -> Self {
        Self::MENU
    }
}

impl NavProfile {
    /// Launcher / Home / OSK DQ. Values are the shipped menu feel; tune here.
    pub const MENU: Self = Self {
        axis_deadzone: STICK_AXIS_DEADZONE,
        combined_deadzone: STICK_COMBINED_DEADZONE,
        repeat_delay: NAV_INITIAL_DELAY,
        repeat_interval: NAV_REPEAT,
        neutral_grace: NAV_NEUTRAL_GRACE,
        axis_lock: true,
        step_feedback: true,
        repeat_feedback: true,
    };

    /// Digital D-pad surface (OSK Grid, Browser cursor jumps). Same repeat
    /// schedule as the menu; sticks are ignored by the caller's mask.
    pub const DPAD: Self = Self {
        axis_deadzone: 0.0,
        combined_deadzone: 0.0,
        ..Self::MENU
    };

    /// Browser virtual cursor: soft proportional stick, no per-step cue.
    pub const CURSOR: Self = Self {
        axis_deadzone: 0.12,
        combined_deadzone: 0.0,
        step_feedback: false,
        repeat_feedback: false,
        ..Self::MENU
    };

    /// Browser scroll: soft proportional stick + axis lock, no step cue.
    pub const SCROLL: Self = Self {
        axis_deadzone: 0.12,
        combined_deadzone: 0.0,
        step_feedback: false,
        repeat_feedback: false,
        ..Self::MENU
    };

    /// Shape one stick through the profile: per-axis deadzone, then the
    /// residual combined-vector deadzone for the menu profile.
    pub fn shape_stick(&self, x: f32, y: f32) -> (f32, f32) {
        let x = if x.abs() < self.axis_deadzone { 0.0 } else { x };
        let y = if y.abs() < self.axis_deadzone { 0.0 } else { y };
        let mag = (x * x + y * y).sqrt();
        if mag < self.combined_deadzone {
            (0.0, 0.0)
        } else {
            (x, y)
        }
    }
}

/// Which stick a [`NavInput`] came from (or the legacy combined vector).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavStick {
    Left,
    Right,
    Combined,
}

/// One raw navigation sample before profile shaping. Carries pad identity so
/// every step can target the pad that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct NavInput {
    pub pad: PadId,
    pub stick: NavStick,
    pub x: f32,
    pub y: f32,
    pub dpad_up: bool,
    pub dpad_down: bool,
    pub dpad_left: bool,
    pub dpad_right: bool,
}

impl NavInput {
    pub fn from_sample(pad: PadId, sample: &PadSample, stick: NavStick) -> Self {
        let (x, y) = match stick {
            NavStick::Left => (sample.lstick_x, sample.lstick_y),
            NavStick::Right => (sample.rstick_x, sample.rstick_y),
            NavStick::Combined => (sample.stick_x, sample.stick_y),
        };
        Self {
            pad,
            stick,
            x,
            y,
            dpad_up: sample.dpad_up,
            dpad_down: sample.dpad_down,
            dpad_left: sample.dpad_left,
            dpad_right: sample.dpad_right,
        }
    }

    /// Build a raw input, then shape the stick with `profile`. Use for OSK DQ
    /// and any other raw-stick host; prepared combined vectors pass
    /// [`NavProfile::MENU`] already shaped (x/y effectively unchanged because
    /// the residual/deadzone are already applied upstream).
    pub fn shaped_from_sample(
        pad: PadId,
        sample: &PadSample,
        stick: NavStick,
        profile: NavProfile,
    ) -> Self {
        let mut input = Self::from_sample(pad, sample, stick);
        let (x, y) = profile.shape_stick(input.x, input.y);
        input.x = x;
        input.y = y;
        input
    }

    /// Mask to the fields a [`NavStepper`] consumes.
    pub fn to_sample(&self) -> PadSample {
        PadSample {
            stick_x: self.x,
            stick_y: self.y,
            dpad_up: self.dpad_up,
            dpad_down: self.dpad_down,
            dpad_left: self.dpad_left,
            dpad_right: self.dpad_right,
            ..PadSample::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavPhase {
    First,
    Repeat,
    Release,
}

/// One shared navigation step. Feedback policy is on the profile; the app
/// maps it to sound/haptic on `pad`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavStep {
    pub direction: NavAction,
    pub phase: NavPhase,
}

/// Shared 2D nav stepper for D-pad + sticks (dominant axis; prevents double-fire).
#[derive(Debug, Clone, Default)]
pub struct NavStepper {
    profile: NavProfile,
    /// Encoded direction: -2 Left, -1 Up, 1 Down, 2 Right, 0 idle.
    direction: i8,
    next_fire: Option<Instant>,
    /// Whether the active direction is currently driven by the digital D-pad
    /// (as opposed to the analog stick). D-pad releases are clean digital
    /// edges — a neutral sample means a deliberate release and resets repeat
    /// gating at once, so fast re-taps fire per press. Stick releases keep
    /// the neutral-dropout grace below (worn-stick threshold flicker).
    direction_from_dpad: bool,
    /// When the sample drops to neutral while a direction is active, the moment
    /// of the first neutral tick. Transient dropouts (analog threshold hover,
    /// e.g. a worn stick wavering across the stick deadzone) must not reset
    /// repeat gating — only a neutral held past [`NavProfile::neutral_grace`]
    /// is a deliberate release.
    neutral_since: Option<Instant>,
}

impl NavStepper {
    fn action_for(direction: i8) -> Option<NavAction> {
        match direction {
            -2 => Some(NavAction::Left),
            -1 => Some(NavAction::Up),
            1 => Some(NavAction::Down),
            2 => Some(NavAction::Right),
            _ => None,
        }
    }

    pub fn with_profile(profile: NavProfile) -> Self {
        Self {
            profile,
            ..Self::default()
        }
    }

    pub fn profile(&self) -> NavProfile {
        self.profile
    }

    /// Shared step entry point: returns First/Repeat/Release.
    ///
    /// `horizontal`: left/right move. Vertical-only keeps Up/Down.
    /// `allow_vertical`: false on the horizontal Games strip where Up/Down are
    /// suppressed — vertical stick wobble must not enter direction state, or a
    /// held-Left stick oscillating Left↔Up/Down would fire immediately on every
    /// flip (Up/Down flips are dropped after the stepper but still reset the
    /// repeat timer). Forcing want_up/want_down false here keeps direction
    /// pinned so initial-delay + repeat gating holds.
    ///
    /// A neutral sample only resets direction state after
    /// [`NavProfile::neutral_grace`]: analog sticks hovering at the deadzone
    /// edge flicker between deflected and neutral (0↔dir), and an instant
    /// reset would fire immediately on every return, bypassing repeat gating.
    /// Bridging brief dropouts keeps a steady hold gated; a deliberate release
    /// still resets, and any direction change (including the opposite
    /// direction) still fires at once.
    ///
    /// Digital D-pad input is exempt from the grace: D-pad contacts don't
    /// flicker, so a neutral sample after a D-pad-driven direction is a
    /// deliberate release and resets at once. Without this, fast same-direction
    /// re-taps (release shorter than the grace) stay on the repeat schedule
    /// and the re-press is swallowed instead of firing.
    pub fn step(
        &mut self,
        sample: &PadSample,
        now: Instant,
        horizontal: bool,
        allow_vertical: bool,
    ) -> Option<NavStep> {
        let want_left = horizontal && (sample.dpad_left || sample.stick_x < 0.0);
        let want_right = horizontal && (sample.dpad_right || sample.stick_x > 0.0);
        let want_up = allow_vertical && (sample.dpad_up || sample.stick_y < 0.0);
        let want_down = allow_vertical && (sample.dpad_down || sample.stick_y > 0.0);

        let horiz = if want_left && !want_right {
            Some(-1i8)
        } else if want_right && !want_left {
            Some(1)
        } else {
            None
        };
        let vert = if want_up && !want_down {
            Some(-1i8)
        } else if want_down && !want_up {
            Some(1)
        } else {
            None
        };

        let direction = match (horiz, vert) {
            (Some(h), Some(_v)) => {
                let ax = sample.stick_x.abs();
                let ay = sample.stick_y.abs();
                if ax > ay {
                    h * 2
                } else if ay > ax {
                    // Vertical wins on stronger Y (or equal when both sticks zero → dpad tie).
                    vert.unwrap_or(0)
                } else if self.profile.axis_lock && self.direction.abs() == 2 {
                    // Stick-magnitude tie: keep the active axis so diagonals don't flicker.
                    h * 2
                } else {
                    vert.unwrap_or(0)
                }
            }
            (Some(h), None) => h * 2,
            (None, Some(v)) => v,
            (None, None) => 0,
        };

        if direction == 0 {
            if self.direction == 0 {
                self.neutral_since = None;
                return None;
            }
            // Digital D-pad release: deliberate by construction, reset at once
            // so a fast re-tap fires as a new press (no repeat-gating swallow).
            if self.direction_from_dpad {
                return Some(self.release_step(NavPhase::Release));
            }
            // Active hold dropped to neutral: bridge transient dropouts so a
            // flickering stick cannot reset repeat gating; only a deliberate
            // (grace-held) release resets.
            let since = *self.neutral_since.get_or_insert(now);
            if now.saturating_duration_since(since) < self.profile.neutral_grace {
                return None;
            }
            return Some(self.release_step(NavPhase::Release));
        }
        self.neutral_since = None;
        // Whether the D-pad (not just the stick) drives this direction now.
        // Mixed holds count as D-pad: while any D-pad contact holds the
        // direction, a later all-neutral sample is a deliberate release.
        let from_dpad = match direction {
            -2 => sample.dpad_left,
            2 => sample.dpad_right,
            -1 => sample.dpad_up,
            1 => sample.dpad_down,
            _ => false,
        };

        if self.direction != direction {
            self.direction = direction;
            self.direction_from_dpad = from_dpad;
            self.next_fire = Some(now + self.profile.repeat_delay);
            return Self::action_for(direction).map(|direction| NavStep {
                direction,
                phase: NavPhase::First,
            });
        }

        self.direction_from_dpad = from_dpad;
        if self.next_fire.is_some_and(|t| now >= t) {
            self.next_fire = Some(now + self.profile.repeat_interval);
            return Self::action_for(direction).map(|direction| NavStep {
                direction,
                phase: NavPhase::Repeat,
            });
        }
        None
    }

    /// Shared step from a pad-identified input.
    pub fn step_input(
        &mut self,
        input: &NavInput,
        now: Instant,
        horizontal: bool,
        allow_vertical: bool,
    ) -> Option<NavStep> {
        let sample = input.to_sample();
        self.step(&sample, now, horizontal, allow_vertical)
    }

    fn release_step(&mut self, phase: NavPhase) -> NavStep {
        let direction = Self::action_for(self.direction).unwrap_or(NavAction::Up);
        self.direction = 0;
        self.direction_from_dpad = false;
        self.next_fire = None;
        self.neutral_since = None;
        NavStep { direction, phase }
    }

    /// Behavior-preserving wrapper: First/Repeat map to the legacy action,
    /// Release stays invisible to existing call sites.
    pub fn update(
        &mut self,
        sample: &PadSample,
        now: Instant,
        horizontal: bool,
        allow_vertical: bool,
    ) -> Option<NavAction> {
        match self.step(sample, now, horizontal, allow_vertical) {
            Some(step) if step.phase != NavPhase::Release => Some(step.direction),
            _ => None,
        }
    }

    pub fn reset(&mut self) {
        self.direction = 0;
        self.direction_from_dpad = false;
        self.next_fire = None;
        self.neutral_since = None;
    }
}

/// Which hold owns the press for foreign-press cancellation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldOwner {
    Cross,
    Triangle,
}

/// Previous face/Options/Create held state. Taps are source-side actions now;
/// this only exists so a Triangle/Cross hold can tell whether another button
/// just went down.
#[derive(Debug, Clone, Default)]
struct HeldButtons {
    cross: bool,
    circle: bool,
    square: bool,
    triangle: bool,
    options: bool,
    create: bool,
}

impl HeldButtons {
    fn foreign_press(&self, sample: &PadSample, except: Option<HoldOwner>) -> bool {
        (sample.cross && except != Some(HoldOwner::Cross) && !self.cross)
            || (sample.triangle && except != Some(HoldOwner::Triangle) && !self.triangle)
            || (sample.circle && !self.circle)
            || (sample.square && !self.square)
            || (sample.options && !self.options)
            || (sample.create && !self.create)
    }

    fn sync(&mut self, sample: &PadSample) {
        self.cross = sample.cross;
        self.circle = sample.circle;
        self.square = sample.square;
        self.triangle = sample.triangle;
        self.options = sample.options;
        self.create = sample.create;
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Tracks a face-button hold (0..=1). Charge eases out visually; early release decays.
/// Completes only on release when charge is full and the press was not cancelled.
#[derive(Debug, Clone, Default)]
pub struct HoldTracker {
    /// Linear charge 0..=1 (before ease-out). Continuous hold fills this in [`BUTTON_HOLD`].
    charge: f32,
    last_tick: Option<Instant>,
    /// Button is currently down (for [`Self::is_active`]).
    holding: bool,
    /// After cancel (e.g. Circle), ignore this press until release.
    suppress_until_release: bool,
    /// Release stamp for wall-clock decay. Progress is pulled from this, so a
    /// stopped pad edge stream cannot freeze the ring or make the next edge
    /// subtract the whole gap in one step.
    released_at: Option<Instant>,
}

const BUTTON_HOLD: Duration = Duration::from_secs(1);
/// Full-to-empty visual decay after early release.
const BUTTON_DECAY: Duration = Duration::from_millis(1500);

/// Cubic ease-out: fast start, slow finish. `u` is linear charge 0..=1.
pub fn hold_ease_out(u: f32) -> f32 {
    let t = 1.0 - u.clamp(0.0, 1.0);
    1.0 - t * t * t
}

/// Inverse of [`hold_ease_out`] so decay can unwind visual progress then restore charge.
pub fn hold_ease_out_inv(p: f32) -> f32 {
    let p = p.clamp(0.0, 1.0);
    1.0 - (1.0 - p).cbrt()
}

impl HoldTracker {
    /// Returns `(progress, just_completed)`.
    ///
    /// Progress is ease-out of charge (ring fill). `just_completed` is true on the
    /// release frame when charge was full and the press was not cancelled.
    pub fn update(&mut self, held: bool, now: Instant) -> (f32, bool) {
        if self.suppress_until_release {
            if held {
                return (0.0, false);
            }
            self.suppress_until_release = false;
            self.charge = 0.0;
            self.last_tick = None;
            self.holding = false;
            self.released_at = None;
            return (0.0, false);
        }

        if held {
            // Re-press during decay: materialize the wall-clock pose before
            // adding new charge, so a brief tap resumes from where the ring
            // actually is instead of from the stale release value.
            if self.released_at.is_some() {
                self.charge = hold_ease_out_inv(self.progress_at(now));
                self.released_at = None;
            }
            let dt = self
                .last_tick
                .map(|t| now.saturating_duration_since(t).as_secs_f32())
                .unwrap_or(0.0);
            self.last_tick = Some(now);
            self.holding = true;
            self.charge = (self.charge + dt / BUTTON_HOLD.as_secs_f32()).min(1.0);
            return (hold_ease_out(self.charge), false);
        }

        self.holding = false;
        if self.charge >= 1.0 {
            self.charge = 0.0;
            self.last_tick = None;
            self.released_at = None;
            return (0.0, true);
        }

        if self.charge <= 0.0 {
            self.charge = 0.0;
            self.last_tick = None;
            self.released_at = None;
            return (0.0, false);
        }

        // First release stamps the decay start; later samples (or frame
        // polls) read the same stamp instead of integrating per-message dt.
        if self.released_at.is_none() {
            self.released_at = Some(now);
            self.last_tick = None;
        }
        let progress = self.progress_at(now);
        if progress <= 0.0 {
            self.charge = 0.0;
            self.last_tick = None;
            self.released_at = None;
        }
        (progress, false)
    }

    /// Current ring fill without mutating the tracker. Decay is derived from
    /// the release stamp, so this is safe to call from a window frame clock
    /// even when no pad edges are arriving.
    pub fn progress_at(&self, now: Instant) -> f32 {
        if self.suppress_until_release || self.charge <= 0.0 {
            return 0.0;
        }
        let base = hold_ease_out(self.charge);
        if self.holding {
            return base;
        }
        let Some(released_at) = self.released_at else {
            return base;
        };
        let elapsed = now.saturating_duration_since(released_at).as_secs_f32();
        (base - elapsed / BUTTON_DECAY.as_secs_f32()).max(0.0)
    }

    /// True while charging or while released progress is still decaying.
    pub fn is_animating(&self, now: Instant) -> bool {
        self.holding || self.progress_at(now) > 0.0
    }

    /// True while this tracker still owns charge (including a decay that has
    /// reached zero but has not yet been reaped by an update).
    pub fn is_tracking(&self) -> bool {
        self.holding || self.charge > 0.0
    }

    /// True while the button is held and charging or full (not decaying, not suppressed).
    pub fn is_active(&self) -> bool {
        self.holding && !self.suppress_until_release
    }

    /// Abort charge/decay immediately. If still held, ignore until release.
    pub fn cancel(&mut self) {
        self.charge = 0.0;
        self.last_tick = None;
        self.holding = false;
        self.suppress_until_release = true;
        self.released_at = None;
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Triangle / Cross hold trackers share the same timing.
pub type TriangleHold = HoldTracker;
pub type CrossHold = HoldTracker;

/// Hold threshold for the browser history selector (B2.4): holding L1/R1
/// this long opens the directional dropdown instead of single-stepping
/// history on release.
pub const BUMPER_HOLD: Duration = Duration::from_millis(400);

/// Which bumper the hold tracker is following.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BumperSide {
    Left,
    Right,
}

/// One bumper lifecycle event. `Tap` fires on release when the hold never
/// opened the selector (the old press-edge path, now release-timed so a
/// hold can claim the press first). `Open` fires once when the hold
/// threshold is crossed while still held; the later release carries no
/// event (the open selector stays up for D-pad browsing; Cross confirms,
/// Circle cancels).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BumperHoldEvent {
    Tap(BumperSide),
    Open(BumperSide),
}

/// Per-pad L1/R1 hold-vs-tap disambiguation. Tracks at most one side: a
/// fresh press starts the clock, crossing [`BUMPER_HOLD`] while held
/// emits `Open` once, releasing first emits `Tap`. Both-held is
/// ambiguous — the existing side keeps its clock, a fresh both-press is
/// ignored. Side switches without a release cancel the old press silently
/// (no tap) and start the new side.
#[derive(Debug, Clone, Default)]
pub struct BumperHold {
    side: Option<BumperSide>,
    since: Option<Instant>,
    opened: bool,
}

impl BumperHold {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn tracking(&self) -> Option<BumperSide> {
        self.side
    }

    pub fn is_open(&self) -> bool {
        self.opened
    }

    pub fn update(&mut self, l1: bool, r1: bool, now: Instant) -> Option<BumperHoldEvent> {
        let held = match (l1, r1) {
            (true, false) => Some(BumperSide::Left),
            (false, true) => Some(BumperSide::Right),
            // Both held: keep the existing clock so a grazing second
            // finger cannot restart the hold; fresh both-press ignored.
            (true, true) => {
                if self.side.is_some() {
                    return self.tick_hold(now);
                }
                return None;
            }
            (false, false) => None,
        };
        match (self.side, held) {
            // Release.
            (Some(prev), None) => {
                let opened = self.opened;
                self.reset();
                if opened {
                    None
                } else {
                    Some(BumperHoldEvent::Tap(prev))
                }
            }
            // Fresh press.
            (None, Some(next)) => {
                self.side = Some(next);
                self.since = Some(now);
                self.opened = false;
                None
            }
            // Side switch without a release: old press cancelled silently.
            (Some(prev), Some(next)) if prev != next => {
                self.side = Some(next);
                self.since = Some(now);
                self.opened = false;
                None
            }
            // Steady hold: maybe cross the threshold.
            (Some(_), Some(_)) => self.tick_hold(now),
            // Idle.
            (None, None) => None,
        }
    }

    fn tick_hold(&mut self, now: Instant) -> Option<BumperHoldEvent> {
        let (Some(side), Some(since)) = (self.side, self.since) else {
            return None;
        };
        if !self.opened && now.saturating_duration_since(since) >= BUMPER_HOLD {
            self.opened = true;
            Some(BumperHoldEvent::Open(side))
        } else {
            None
        }
    }
}

/// Stickless copy of a pad sample: D-pad bits preserved, sticks zeroed,
/// face/system buttons and the gesture set dropped. Feeds a standalone
/// [`NavStepper`] where directionals must flow without stick input — the
/// engine browser's D-pad cursor jumps run on this while the banked stepper
/// stays parked (so stick moves never double-fire into both cursor and nav).
pub fn dpad_only(sample: &PadSample) -> PadSample {
    PadSample {
        dpad_up: sample.dpad_up,
        dpad_down: sample.dpad_down,
        dpad_left: sample.dpad_left,
        dpad_right: sample.dpad_right,
        ..PadSample::default()
    }
}

/// True when face/system buttons, triggers, D-pad, and stick are at rest (safe to arm start nav).
pub fn sample_nav_resting(sample: &PadSample) -> bool {
    sample.held.is_empty()
        && !sample.cross
        && !sample.circle
        && !sample.square
        && !sample.triangle
        && !sample.options
        && !sample.create
        && !sample.l1
        && !sample.r1
        && !sample.l3
        && !sample.l2
        && !sample.r2
        && !sample.dpad_up
        && !sample.dpad_down
        && !sample.dpad_left
        && !sample.dpad_right
        && sample.stick_x == 0.0
        && sample.stick_y == 0.0
        && sample.lstick_x == 0.0
        && sample.lstick_y == 0.0
        && sample.rstick_x == 0.0
        && sample.rstick_y == 0.0
}

/// Pick a single reading for debug UI: prefer a non-resting pad, else the first.
pub fn preferred_reading(readings: &[NavReading]) -> Option<&NavReading> {
    readings
        .iter()
        .find(|r| !sample_nav_resting(&r.sample))
        .or_else(|| readings.first())
}

fn log_source_once(source: &str, pads: usize) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        crate::platform::app_log::info(format!(
            "start-nav: backend={source} pads={pads} (hid-worker snapshot)",
        ));
    }
}

/// Result of a multi-pad launcher nav poll.
#[derive(Debug)]
pub enum NavReadingsOutcome {
    Readings {
        readings: Vec<NavReading>,
        meta: SnapshotMeta,
    },
    /// No DualSense HID sample in the worker snapshot yet.
    Missing { meta: SnapshotMeta },
}

/// Why a short input read failed (no sample).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFail {
    Timeout,
    Io,
    Truncated,
    BadReport,
    Feature,
}

impl SampleFail {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Io => "io",
            Self::Truncated => "truncated",
            Self::BadReport => "bad_report",
            Self::Feature => "feature",
        }
    }
}

/// Outcome of a short DualSense input read for hid-worker.
#[derive(Debug)]
pub enum ShortSampleOutcome {
    Ok {
        sample: PadSample,
        /// Ordered reports from this drain, before button OR-latching. The
        /// source interpreter consumes these so batch boundaries cannot
        /// create or erase a tap.
        reports: Vec<PadSample>,
        /// Newest battery fields parsed from the same report(s), if present.
        battery: Option<crate::controller::dualsense::battery::BatteryReading>,
    },
    /// No new report this wake — keep the open handle and the previous snapshot reading.
    Timeout,
    /// Hard failure — drop the handle (I/O / bad report / feature).
    #[allow(dead_code)] // matched by hid_worker; field read for logging there
    Fail(SampleFail),
}

/// Merge a drained batch: newest sticks, OR digital buttons / triggers / D-pad.
///
/// Latches a tap that went down and up inside one wake so the next wake can see the release.
pub fn merge_nav_samples(batch: &[PadSample]) -> Option<PadSample> {
    let last = batch.last()?.clone();
    let mut out = last.clone();
    for s in batch {
        out.cross |= s.cross;
        out.circle |= s.circle;
        out.square |= s.square;
        out.triangle |= s.triangle;
        out.options |= s.options;
        out.create |= s.create;
        out.l1 |= s.l1;
        out.r1 |= s.r1;
        out.l3 |= s.l3;
        out.l2 |= s.l2;
        out.r2 |= s.r2;
        out.dpad_up |= s.dpad_up;
        out.dpad_down |= s.dpad_down;
        out.dpad_left |= s.dpad_left;
        out.dpad_right |= s.dpad_right;
        out.held = out.held.union(&s.held).copied().collect();
    }
    out.stick_x = last.stick_x;
    out.stick_y = last.stick_y;
    Some(out)
}

/// Drain queued DualSense reports, wait one report interval, then drain again.
///
/// Compat wrapper over [`crate::controller::hid::sample::drain_sample`] so
/// existing callers keep working unchanged; HID I/O lives in
/// `controller::hid::sample` behind `HidTransport`, pure merge types stay here.
///
/// Never call from the UI thread. Truncated BT: one feature request (one-time) + retry.
pub fn read_device_sample_short(
    device: &HidDevice,
    is_bluetooth: bool,
    serial: &str,
) -> ShortSampleOutcome {
    let policy = crate::controller::hid::policy::Policy::default();
    let transport = crate::controller::hid::transport::HidapiTransport(device);
    let mut opts = crate::controller::hid::sample::SampleOpts::new(is_bluetooth, serial);
    crate::controller::hid::sample::drain_sample(&transport, &mut opts, &policy)
}

/// Build a nav reading from a HID sample + device identity.
pub fn hid_nav_reading(identity: &str, sample: PadSample) -> NavReading {
    NavReading {
        id: PadId(format!("hid:{identity}")),
        sample,
        source: NAV_SOURCE_HID,
    }
}

#[allow(unused_imports)]
// re-exported for tests and callers that still import from start::input
pub use crate::controller::dualsense::input::parse_report;

fn axis_deadzone(value: f32) -> f32 {
    if value.abs() < STICK_AXIS_DEADZONE {
        0.0
    } else {
        value
    }
}

/// Combine left + right sticks: per-axis deadzone first, then sum, then residual deadzone.
pub fn combined_stick(lx: f32, ly: f32, rx: f32, ry: f32) -> (f32, f32) {
    let lx = axis_deadzone(lx);
    let ly = axis_deadzone(ly);
    let rx = axis_deadzone(rx);
    let ry = axis_deadzone(ry);
    let dx = lx + rx;
    let dy = ly + ry;
    let mag = (dx * dx + dy * dy).sqrt();
    if mag < STICK_COMBINED_DEADZONE {
        (0.0, 0.0)
    } else {
        (dx, dy)
    }
}

/// Gentle per-axis deadzone for the split cursor/scroll sticks. Deliberately
/// softer than the nav path (0.9): the browser cursor needs proportional
/// control from a light touch, not a 90%-throw gate. No residual term —
/// each stick stands alone.
pub fn soft_stick(x: f32, y: f32) -> (f32, f32) {
    let deadzone = NavProfile::CURSOR.axis_deadzone;
    let x = if x.abs() < deadzone { 0.0 } else { x };
    let y = if y.abs() < deadzone { 0.0 } else { y };
    (x, y)
}

/// Right-stick scroll shaping (same soft touch as the cursor, separate profile).
pub fn soft_scroll(x: f32, y: f32) -> (f32, f32) {
    let deadzone = NavProfile::SCROLL.axis_deadzone;
    let x = if x.abs() < deadzone { 0.0 } else { x };
    let y = if y.abs() < deadzone { 0.0 } else { y };
    (x, y)
}

/// Per-pad edge / hold / arming state for launcher navigation.
#[derive(Debug, Clone, Default)]
struct PadSlot {
    nav_stepper: NavStepper,
    held: HeldButtons,
    triangle_hold: TriangleHold,
    cross_hold: CrossHold,
    armed: bool,
}

/// Bank of per-pad nav machines. Never merges samples across pads.
#[derive(Debug, Clone, Default)]
pub struct NavBank {
    slots: HashMap<PadId, PadSlot>,
    /// When true, newly seen pads start armed (e.g. after launcher opens cleanly).
    arm_new_pads: bool,
}

/// Transitional alias while call sites move to [`NavBank`].
pub type PadNavBank = NavBank;

/// Which DQ half / stick a step belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavHalf {
    Left,
    Right,
}

/// One step from one pad's half. Feedback target is `pad`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DualStep {
    pub pad: PadId,
    pub half: NavHalf,
    pub step: NavStep,
}

/// A trigger press edge for one pad's half (L2 = left, R2 = right).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerEdge {
    pub pad: PadId,
    pub half: NavHalf,
}

/// Result of one all-live-pad DQ tick. At most one step per half; trigger
/// edges are consumed per pad so a held trigger on one pad cannot mask another.
#[derive(Debug, Clone, Default)]
pub struct DualNavTick {
    pub steps: Vec<DualStep>,
    pub triggers: Vec<TriggerEdge>,
    pub armed_pads: usize,
    pub unarmed_pads: usize,
}

#[derive(Debug, Clone, Default)]
struct DualSlot {
    left: NavStepper,
    right: NavStepper,
    triggers: TriggerLatch,
    armed: bool,
}

/// All-live-pad Dual QWERTY navigation: two independent menu-profile cursors
/// plus trigger edges, with per-pad arming and first-step arbitration per half.
#[derive(Debug, Clone, Default)]
pub struct DualNav {
    slots: HashMap<PadId, DualSlot>,
    arm_new_pads: bool,
    profile: NavProfile,
}

impl DualNav {
    pub fn new(profile: NavProfile) -> Self {
        Self {
            profile,
            ..Self::default()
        }
    }

    pub fn reset(&mut self) {
        self.slots.clear();
        self.arm_new_pads = false;
    }

    /// Clear machines; new pads arm immediately when `arm_immediately`.
    pub fn prepare_on_open(&mut self, arm_immediately: bool) {
        self.slots.clear();
        self.arm_new_pads = arm_immediately;
    }

    /// Feed every live pad. DQ keeps the halves independent: each half reads
    /// only its own stick (D-pad and the other stick zeroed).
    pub fn tick(&mut self, readings: &[NavReading], now: Instant) -> DualNavTick {
        self.sync_presence(readings);
        let mut result = DualNavTick::default();
        for reading in readings {
            let Some(slot) = self.slots.get_mut(&reading.id) else {
                continue;
            };
            if !slot.armed {
                slot.left.reset();
                slot.right.reset();
                slot.triggers.reset();
                if sample_nav_resting(&reading.sample) {
                    slot.armed = true;
                    result.armed_pads += 1;
                } else {
                    result.unarmed_pads += 1;
                }
                continue;
            }
            result.armed_pads += 1;

            let left = NavInput::shaped_from_sample(
                reading.id.clone(),
                &reading.sample,
                NavStick::Left,
                self.profile,
            );
            let right = NavInput::shaped_from_sample(
                reading.id.clone(),
                &reading.sample,
                NavStick::Right,
                self.profile,
            );
            let left_sample = left.to_sample();
            let right_sample = right.to_sample();
            // DQ halves are stick-only; the shared D-pad drives Grid.
            let mut left_sample = left_sample;
            let mut right_sample = right_sample;
            left_sample.dpad_up = false;
            left_sample.dpad_down = false;
            left_sample.dpad_left = false;
            left_sample.dpad_right = false;
            right_sample.dpad_up = false;
            right_sample.dpad_down = false;
            right_sample.dpad_left = false;
            right_sample.dpad_right = false;
            // Every armed pad advances its steppers, even when another pad
            // already owns the half this tick; otherwise a losing pad keeps a
            // stale direction/next_fire and can fire an immediate repeat when
            // it later wins.
            let left_step = slot.left.step(&left_sample, now, true, true);
            let right_step = slot.right.step(&right_sample, now, true, true);
            if !result.steps.iter().any(|step| step.half == NavHalf::Left)
                && let Some(step) = left_step
            {
                result.steps.push(DualStep {
                    pad: reading.id.clone(),
                    half: NavHalf::Left,
                    step,
                });
            }
            if !result.steps.iter().any(|step| step.half == NavHalf::Right)
                && let Some(step) = right_step
            {
                result.steps.push(DualStep {
                    pad: reading.id.clone(),
                    half: NavHalf::Right,
                    step,
                });
            }
            for half in slot.triggers.update(reading.sample.l2, reading.sample.r2) {
                result.triggers.push(TriggerEdge {
                    pad: reading.id.clone(),
                    half,
                });
            }
        }
        result
    }

    fn sync_presence(&mut self, readings: &[NavReading]) {
        let live: HashSet<PadId> = readings.iter().map(|r| r.id.clone()).collect();
        self.slots.retain(|id, _| live.contains(id));
        for reading in readings {
            if self.slots.contains_key(&reading.id) {
                continue;
            }
            let mut slot = DualSlot {
                left: NavStepper::with_profile(self.profile),
                right: NavStepper::with_profile(self.profile),
                ..DualSlot::default()
            };
            slot.armed = self.arm_new_pads;
            self.slots.insert(reading.id.clone(), slot);
        }
    }
}

/// Rising-edge latch for L2/R2 on one pad. Holding a trigger fires once.
#[derive(Debug, Clone, Copy, Default)]
pub struct TriggerLatch {
    l2: bool,
    r2: bool,
}

impl TriggerLatch {
    /// Returns the halves whose trigger went down this sample (left first).
    pub fn update(&mut self, l2: bool, r2: bool) -> Vec<NavHalf> {
        let mut out = Vec::new();
        if l2 && !self.l2 {
            out.push(NavHalf::Left);
        }
        if r2 && !self.r2 {
            out.push(NavHalf::Right);
        }
        self.l2 = l2;
        self.r2 = r2;
        out
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Which face / Options / Create buttons are currently held (OR across armed pads).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FaceHeld {
    pub cross: bool,
    pub circle: bool,
    pub square: bool,
    pub triangle: bool,
    pub options: bool,
    pub create: bool,
}

impl FaceHeld {
    fn from_sample(sample: &PadSample) -> Self {
        Self {
            cross: sample.cross,
            circle: sample.circle,
            square: sample.square,
            triangle: sample.triangle,
            options: sample.options,
            create: sample.create,
        }
    }

    pub fn or(self, other: Self) -> Self {
        Self {
            cross: self.cross || other.cross,
            circle: self.circle || other.circle,
            square: self.square || other.square,
            triangle: self.triangle || other.triangle,
            options: self.options || other.options,
            create: self.create || other.create,
        }
    }
}

/// Result of one multi-pad nav tick (at most one [`NavAction`]).
#[derive(Debug, Clone, Default)]
pub struct PadTickResult {
    pub action: Option<NavAction>,
    /// Pad that produced `action` (for diagnostics).
    pub action_pad: Option<PadId>,
    pub triangle_progress: f32,
    pub triangle_completed: bool,
    /// Pad that completed Triangle hold this tick.
    pub triangle_completed_pad: Option<PadId>,
    pub cross_progress: f32,
    pub cross_completed: bool,
    /// Pad that completed Cross hold this tick.
    pub cross_completed_pad: Option<PadId>,
    /// Face buttons held on any armed pad this tick.
    pub held: FaceHeld,
    /// How many live pads are armed this tick.
    pub armed_pads: usize,
    /// How many live pads are still waiting for a resting sample.
    pub unarmed_pads: usize,
    /// Sample for edge logging: action pad, else first non-resting, else first.
    pub diag: Option<NavReading>,
}

impl NavBank {
    pub fn reset(&mut self) {
        self.slots.clear();
        self.arm_new_pads = false;
    }

    /// Clear machines; new pads arm immediately when `arm_immediately`.
    pub fn prepare_on_open(&mut self, arm_immediately: bool) {
        self.slots.clear();
        self.arm_new_pads = arm_immediately;
    }

    /// Abort Triangle / Cross charge and decay immediately.
    pub fn cancel_holds(&mut self) {
        for slot in self.slots.values_mut() {
            slot.triangle_hold.cancel();
            slot.cross_hold.cancel();
        }
    }

    /// Wall-clock hold progress across every live pad, without mutating the
    /// trackers. Surfaces pull this from their frame clock so release decay
    /// continues after the pad edge stream goes quiet.
    pub fn hold_progress(&self, now: Instant) -> (f32, f32) {
        self.slots
            .values()
            .fold((0.0, 0.0), |(cross, triangle), slot| {
                (
                    cross.max(slot.cross_hold.progress_at(now)),
                    triangle.max(slot.triangle_hold.progress_at(now)),
                )
            })
    }

    /// True while any pad still owns hold charge (including a decay that has
    /// reached zero). Frame clocks use this to avoid overwriting keyboard
    /// hold progress owned by the same surface.
    pub fn has_hold_activity(&self) -> bool {
        self.slots
            .values()
            .any(|slot| slot.cross_hold.is_tracking() || slot.triangle_hold.is_tracking())
    }

    /// Sync presence, update every pad's edge/hold state, emit at most one action.
    ///
    /// `allow_nav_move` gates D-pad/stick repeats (false while chrome animates).
    /// `horizontal_nav`: immersive left/right move on the strip. Triggers are inert.
    /// `horizontal_games_nav`: horizontal immersive Games strip — left/right scroll
    /// games (remapped to Up/Down here); Up/Down are suppressed (Left/Right only).
    /// `replace_confirm` switches to Cross-hold / Circle-cancel mode.
    /// `_editing` is retained for call-site compatibility (Triangle saves via
    /// ToggleEdit in both modes); Square still cycles sort and manual Edit is
    /// mouse-only via the row Edit button.
    /// `hold_cross_close` (games browse, running row): Cross hold closes the game.
    /// `hold_triangle_power` (controllers, selected row shows Power off): Triangle hold powers off Bluetooth.
    #[allow(clippy::too_many_arguments)]
    pub fn tick(
        &mut self,
        readings: &[NavReading],
        now: Instant,
        allow_nav_move: bool,
        horizontal_nav: bool,
        horizontal_games_nav: bool,
        replace_confirm: bool,
        _editing: bool,
        hold_cross_close: bool,
        hold_triangle_power: bool,
    ) -> PadTickResult {
        self.sync_presence(readings);

        let mut result = PadTickResult {
            diag: preferred_reading(readings).cloned(),
            ..Default::default()
        };

        let mut nav_action: Option<(PadId, NavAction)> = None;
        let mut newly_armed: Vec<PadId> = Vec::new();

        for reading in readings {
            let Some(slot) = self.slots.get_mut(&reading.id) else {
                continue;
            };

            if !slot.armed {
                slot.held.clear();
                slot.held.sync(&reading.sample);
                slot.nav_stepper.reset();
                slot.triangle_hold.reset();
                slot.cross_hold.reset();
                if sample_nav_resting(&reading.sample) {
                    slot.armed = true;
                    newly_armed.push(reading.id.clone());
                    result.armed_pads += 1;
                } else {
                    result.unarmed_pads += 1;
                }
                continue;
            }
            result.armed_pads += 1;
            result.held = result.held.or(FaceHeld::from_sample(&reading.sample));

            if replace_confirm {
                let (progress, completed) = slot.cross_hold.update(reading.sample.cross, now);
                if progress > result.cross_progress {
                    result.cross_progress = progress;
                }
                if completed {
                    result.cross_completed = true;
                    result.cross_completed_pad = Some(reading.id.clone());
                }
                slot.triangle_hold.cancel();

                let stick_interrupt = reading.sample.dpad_up
                    || reading.sample.dpad_down
                    || reading.sample.dpad_left
                    || reading.sample.dpad_right
                    || reading.sample.stick_x != 0.0
                    || reading.sample.stick_y != 0.0;
                let foreign = slot
                    .held
                    .foreign_press(&reading.sample, Some(HoldOwner::Cross));
                if slot.cross_hold.is_active() && (stick_interrupt || foreign) {
                    slot.cross_hold.cancel();
                    result.cross_progress = 0.0;
                    result.cross_completed = false;
                    result.cross_completed_pad = None;
                }
                slot.held.sync(&reading.sample);
                continue;
            }

            // Games browse on the running row: Cross hold closes the game (owned; early
            // release decays instead of Confirm). Controllers: Triangle hold powers off.
            if hold_cross_close {
                let (c_progress, c_completed) = slot.cross_hold.update(reading.sample.cross, now);
                if c_progress > result.cross_progress {
                    result.cross_progress = c_progress;
                }
                if c_completed {
                    result.cross_completed = true;
                    result.cross_completed_pad = Some(reading.id.clone());
                }
                let stick_interrupt = reading.sample.dpad_up
                    || reading.sample.dpad_down
                    || reading.sample.dpad_left
                    || reading.sample.dpad_right
                    || reading.sample.stick_x != 0.0
                    || reading.sample.stick_y != 0.0;
                let foreign = slot
                    .held
                    .foreign_press(&reading.sample, Some(HoldOwner::Cross));
                if slot.cross_hold.is_active() && (stick_interrupt || foreign) {
                    slot.cross_hold.cancel();
                    result.cross_progress = 0.0;
                    result.cross_completed = false;
                    result.cross_completed_pad = None;
                }
            } else {
                slot.cross_hold.cancel();
            }

            let hold_owned = if hold_cross_close {
                slot.triangle_hold.cancel();
                Some(HoldOwner::Cross)
            } else if hold_triangle_power {
                let (t_progress, t_completed) =
                    slot.triangle_hold.update(reading.sample.triangle, now);
                if t_progress > result.triangle_progress {
                    result.triangle_progress = t_progress;
                }
                if t_completed {
                    result.triangle_completed = true;
                    result.triangle_completed_pad = Some(reading.id.clone());
                }
                Some(HoldOwner::Triangle)
            } else {
                slot.triangle_hold.cancel();
                None
            };

            let nav = if allow_nav_move {
                slot.nav_stepper.update(
                    &reading.sample,
                    now,
                    horizontal_nav || horizontal_games_nav,
                    // Horizontal Games strip suppresses Up/Down: keep vertical
                    // wobble out of stepper direction state (see `NavStepper::update`).
                    // Slide mode (`horizontal_nav` without games) keeps vertical live.
                    !horizontal_games_nav,
                )
            } else {
                None
            };
            // Horizontal Games strip: left/right scroll games, so stepper slide
            // actions become vertical moves. Up/Down do nothing here.
            let nav = nav.and_then(|action| match action {
                NavAction::Left if horizontal_games_nav => Some(NavAction::Up),
                NavAction::Right if horizontal_games_nav => Some(NavAction::Down),
                NavAction::Up | NavAction::Down if horizontal_games_nav => None,
                other => Some(other),
            });

            let foreign = slot.held.foreign_press(&reading.sample, hold_owned);
            if hold_triangle_power && slot.triangle_hold.is_active() && (nav.is_some() || foreign) {
                slot.triangle_hold.cancel();
                result.triangle_progress = 0.0;
                result.triangle_completed = false;
                result.triangle_completed_pad = None;
            }

            if let Some(action) = nav
                && nav_action.is_none()
            {
                nav_action = Some((reading.id.clone(), action));
            }
            slot.held.sync(&reading.sample);
        }

        if let Some((id, action)) = nav_action {
            result.diag = readings.iter().find(|r| r.id == id).cloned();
            result.action_pad = Some(id);
            result.action = Some(action);
        }

        if !newly_armed.is_empty() {
            crate::platform::app_log::info(format!(
                "start-nav: armed pads={:?} (armed={} unarmed={})",
                newly_armed.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
                result.armed_pads,
                result.unarmed_pads,
            ));
        }

        result
    }

    fn sync_presence(&mut self, readings: &[NavReading]) {
        let live: HashSet<PadId> = readings.iter().map(|r| r.id.clone()).collect();
        self.slots.retain(|id, _| live.contains(id));
        for reading in readings {
            if self.slots.contains_key(&reading.id) {
                continue;
            }
            let mut slot = PadSlot::default();
            // First sight: sync hold-cancellation state without firing.
            slot.held.sync(&reading.sample);
            if self.arm_new_pads {
                slot.armed = true;
            } else {
                // Unarmed until full rest (covers leftover open-chord and mid-press connects).
                slot.armed = false;
            }
            self.slots.insert(reading.id.clone(), slot);
        }
    }
}

/// Per-pad reopen-gesture detectors (chord must complete on a single pad).
#[derive(Debug, Clone, Default)]
pub struct GestureDetectorBank {
    detectors: HashMap<PadId, GestureDetector>,
}

impl GestureDetectorBank {
    pub fn reset(&mut self) {
        self.detectors.clear();
    }

    /// Mark every live pad as already armed so the current/sticky hold cannot
    /// reopen the launcher (e.g. after open / close).
    pub fn consume_pending_match(&mut self, readings: &[NavReading]) {
        let live: HashSet<PadId> = readings.iter().map(|r| r.id.clone()).collect();
        self.detectors.retain(|id, _| live.contains(id));
        for reading in readings {
            self.detectors
                .entry(reading.id.clone())
                .or_default()
                .mark_armed();
        }
    }

    /// Returns true when any single pad completes the chord this tick.
    pub fn update(&mut self, required: &[GestureControl], readings: &[NavReading]) -> bool {
        let live: HashSet<PadId> = readings.iter().map(|r| r.id.clone()).collect();
        self.detectors.retain(|id, _| live.contains(id));

        let mut fired = false;
        for reading in readings {
            let detector = self.detectors.entry(reading.id.clone()).or_default();
            if detector.update(required, &reading.sample.held) {
                fired = true;
            }
        }
        fired
    }
}

/// Inputs for the exclusive-fullscreen predicate (testable without HWND).
#[derive(Debug, Clone, Copy)]
pub struct ForegroundWindowFacts {
    pub width: i32,
    pub height: i32,
    pub monitor_width: i32,
    pub monitor_height: i32,
    /// Win32 `GWL_STYLE` bits.
    pub style: isize,
}

/// Win32 `GWL_STYLE` bits for the fullscreen predicate (values match
/// `WS_CAPTION` / `WS_THICKFRAME`). Kept here so this pure predicate needs
/// no Win32 import; the live query lives behind `Host::foreground_facts`.
const WS_CAPTION: isize = 0x00C0_0000;
const WS_THICKFRAME: isize = 0x0004_0000;

/// True only for borderless cover-monitor windows (not ordinary maximized framed apps).
pub fn is_exclusive_fullscreen(facts: ForegroundWindowFacts) -> bool {
    if facts.width < facts.monitor_width || facts.height < facts.monitor_height {
        return false;
    }
    // Maximized framed windows keep WS_CAPTION / WS_THICKFRAME; exclusive FS usually does not.
    let framed = (facts.style & WS_CAPTION) != 0 || (facts.style & WS_THICKFRAME) != 0;
    !framed
}

/// Whether the foreground window looks like exclusive fullscreen.
///
/// Geometry comes from [`crate::platform::host::Host::foreground_facts`];
/// the predicate stays pure ([`is_exclusive_fullscreen`]) for tests.
pub fn foreground_is_exclusive_fullscreen() -> bool {
    use crate::platform::host::Host;
    let Some((width, height, monitor_width, monitor_height, style)) = Host::foreground_facts()
    else {
        return false;
    };
    is_exclusive_fullscreen(ForegroundWindowFacts {
        width,
        height,
        monitor_width,
        monitor_height,
        style,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_nav_samples_latches_button_released_in_batch() {
        let down = PadSample {
            cross: true,
            ..Default::default()
        };
        let up = PadSample::default();
        let merged = merge_nav_samples(&[down, up]).expect("batch");
        assert!(
            merged.cross,
            "tap that released mid-drain must stay latched"
        );
        assert_eq!(merged.stick_y, 0.0);
    }

    #[test]
    fn merge_nav_samples_latches_browser_buttons_released_in_batch() {
        // L1/R1/L3 were added after the merge helper; a tail release report
        // must not erase a press seen earlier in the same drain batch.
        let pressed = PadSample {
            l1: true,
            r1: true,
            l3: true,
            held: [GestureControl::L1, GestureControl::R1, GestureControl::L3]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let merged = merge_nav_samples(&[pressed, PadSample::default()]).expect("batch");
        assert!(merged.l1, "L1 press must stay latched");
        assert!(merged.r1, "R1 press must stay latched");
        assert!(merged.l3, "L3 press must stay latched");
    }

    #[test]
    fn merge_nav_samples_keeps_newest_stick() {
        let a = PadSample {
            stick_x: -0.8,
            stick_y: -0.8,
            ..Default::default()
        };
        let b = PadSample {
            stick_x: 0.4,
            stick_y: 0.6,
            ..Default::default()
        };
        let merged = merge_nav_samples(&[a, b]).expect("batch");
        assert!((merged.stick_x - 0.4).abs() < f32::EPSILON);
        assert!((merged.stick_y - 0.6).abs() < f32::EPSILON);
    }

    #[test]
    fn combined_stick_either_side_moves() {
        let (_, dy) = combined_stick(0.0, -0.95, 0.0, 0.0);
        assert!(dy < -0.3);
        let (_, dy2) = combined_stick(0.0, 0.0, 0.0, 0.95);
        assert!(dy2 > 0.3);
    }

    #[test]
    fn combined_stick_below_deadzone_does_not_move() {
        let (dx, dy) = combined_stick(0.0, -0.8, 0.0, 0.0);
        assert_eq!(dx, 0.0);
        assert_eq!(dy, 0.0);
    }

    #[test]
    fn parse_report_reads_cross_and_l2() {
        let mut buf = [0u8; 64];
        buf[0] = 0x01;
        buf[1] = 128;
        buf[2] = 128;
        buf[3] = 128;
        buf[4] = 128;
        buf[5] = 200; // L2 analog
        buf[8] = 0x08 | (1 << 5); // resting hat + Cross
        let sample = parse_report(&buf, 0);
        assert!(sample.cross);
        assert!(sample.l2);
        assert!(!sample.circle);
    }

    #[test]
    fn parse_report_seq_zero_is_not_dpad_up() {
        let mut buf = [0u8; 64];
        buf[0] = 0x01;
        buf[1] = 128;
        buf[2] = 128;
        buf[3] = 128;
        buf[4] = 128;
        buf[8] = 0x08; // hat null / released
        let sample = parse_report(&buf, 0);
        assert!(!sample.dpad_up);
        assert!(!sample.dpad_down);
        assert!(!sample.dpad_left);
        assert!(!sample.dpad_right);
    }

    #[test]
    fn parse_report_reads_dpad_left_right_and_stick_x() {
        let mut buf = [0u8; 64];
        buf[0] = 0x01;
        buf[1] = 10; // left stick hard left
        buf[2] = 128;
        buf[3] = 128;
        buf[4] = 128;
        buf[8] = 0x06; // hat left
        let sample = parse_report(&buf, 0);
        assert!(sample.dpad_left);
        assert!(!sample.dpad_right);
        assert!(sample.stick_x < -0.3);

        buf[1] = 128;
        buf[8] = 0x02; // hat right
        let sample = parse_report(&buf, 0);
        assert!(sample.dpad_right);
        assert!(!sample.dpad_left);
        assert_eq!(sample.stick_x, 0.0);
    }

    #[test]
    fn nav_stepper_horizontal_fires_slide_actions() {
        let mut stepper = NavStepper::default();
        let now = Instant::now();
        let left = PadSample {
            dpad_left: true,
            ..Default::default()
        };
        assert_eq!(
            stepper.update(&left, now, true, true),
            Some(NavAction::Left)
        );
        assert!(stepper.update(&left, now, true, true).is_none());

        stepper.reset();
        let right = PadSample {
            stick_x: 0.9,
            ..Default::default()
        };
        assert_eq!(
            stepper.update(&right, now, true, true),
            Some(NavAction::Right)
        );
    }

    #[test]
    fn nav_stepper_vertical_still_fires_up_down() {
        let mut stepper = NavStepper::default();
        let now = Instant::now();
        let up = PadSample {
            dpad_up: true,
            ..Default::default()
        };
        assert_eq!(stepper.update(&up, now, true, true), Some(NavAction::Up));
        stepper.reset();
        let down = PadSample {
            stick_y: 0.9,
            ..Default::default()
        };
        assert_eq!(
            stepper.update(&down, now, true, true),
            Some(NavAction::Down)
        );
    }

    #[test]
    fn nav_stepper_dpad_fast_retap_fires_each_press() {
        // Digital D-pad: a clean release resets gating at once, so fast
        // same-direction re-taps (release well under the stick neutral
        // grace and the initial repeat delay) fire per press.
        let mut stepper = NavStepper::default();
        let t0 = Instant::now();
        let down = PadSample {
            dpad_down: true,
            ..Default::default()
        };
        let neutral = PadSample::default();
        assert_eq!(stepper.update(&down, t0, true, true), Some(NavAction::Down));
        // 20ms release then re-press: must fire again, not stay repeat-gated.
        assert!(
            stepper
                .update(&neutral, t0 + Duration::from_millis(20), true, true)
                .is_none()
        );
        assert_eq!(
            stepper.update(&down, t0 + Duration::from_millis(40), true, true),
            Some(NavAction::Down)
        );
        // And a third tap in quick succession.
        assert!(
            stepper
                .update(&neutral, t0 + Duration::from_millis(60), true, true)
                .is_none()
        );
        assert_eq!(
            stepper.update(&down, t0 + Duration::from_millis(80), true, true),
            Some(NavAction::Down)
        );
    }

    #[test]
    fn nav_stepper_stick_fast_retap_stays_gated() {
        // Analog stick: the same quick release/press pattern must stay on
        // the repeat schedule (neutral grace bridges stick flicker).
        let mut stepper = NavStepper::default();
        let t0 = Instant::now();
        let down = PadSample {
            stick_y: 0.9,
            ..Default::default()
        };
        let neutral = PadSample::default();
        assert_eq!(stepper.update(&down, t0, true, true), Some(NavAction::Down));
        assert!(
            stepper
                .update(&neutral, t0 + Duration::from_millis(20), true, true)
                .is_none()
        );
        assert!(
            stepper
                .update(&down, t0 + Duration::from_millis(40), true, true)
                .is_none(),
            "stick re-press inside grace + initial delay must stay gated"
        );
        // After a deliberate (grace-held) release, the next press fires anew.
        let mut stepper = NavStepper::default();
        assert_eq!(stepper.update(&down, t0, true, true), Some(NavAction::Down));
        assert!(
            stepper
                .update(&neutral, t0 + Duration::from_millis(20), true, true)
                .is_none()
        );
        assert!(
            stepper
                .update(&neutral, t0 + Duration::from_millis(130), true, true)
                .is_none()
        );
        assert_eq!(
            stepper.update(&down, t0 + Duration::from_millis(140), true, true),
            Some(NavAction::Down)
        );
    }

    #[test]
    fn nav_stepper_dominant_axis_prefers_stronger_stick() {
        let mut stepper = NavStepper::default();
        let now = Instant::now();
        let diag = PadSample {
            stick_x: 0.9,
            stick_y: 0.3,
            ..Default::default()
        };
        assert_eq!(
            stepper.update(&diag, now, true, true),
            Some(NavAction::Right)
        );
        stepper.reset();
        let diag_v = PadSample {
            stick_x: 0.3,
            stick_y: -0.9,
            ..Default::default()
        };
        assert_eq!(
            stepper.update(&diag_v, now, true, true),
            Some(NavAction::Up)
        );
    }

    #[test]
    fn combined_stick_reinforces_and_cancels() {
        let (_, dy) = combined_stick(0.0, -0.95, 0.0, -0.95);
        assert!(dy < -1.0);
        let (_, dy2) = combined_stick(0.0, -0.95, 0.0, 0.95);
        assert_eq!(dy2, 0.0);
    }

    #[test]
    fn dual_stick_rest_bias_does_not_move() {
        let (dx, dy) = combined_stick(0.0, -0.2, 0.0, -0.2);
        assert_eq!(dx, 0.0);
        assert_eq!(dy, 0.0);
    }

    #[test]
    fn maximized_framed_window_is_not_exclusive() {
        let facts = ForegroundWindowFacts {
            width: 1920,
            height: 1080,
            monitor_width: 1920,
            monitor_height: 1080,
            style: WS_CAPTION | WS_THICKFRAME,
        };
        assert!(!is_exclusive_fullscreen(facts));
    }

    #[test]
    fn borderless_cover_monitor_is_exclusive() {
        let facts = ForegroundWindowFacts {
            width: 1920,
            height: 1080,
            monitor_width: 1920,
            monitor_height: 1080,
            style: 0,
        };
        assert!(is_exclusive_fullscreen(facts));
    }

    #[test]
    fn smaller_than_monitor_is_not_exclusive() {
        let facts = ForegroundWindowFacts {
            width: 1280,
            height: 720,
            monitor_width: 1920,
            monitor_height: 1080,
            style: 0,
        };
        assert!(!is_exclusive_fullscreen(facts));
    }

    #[test]
    fn nav_stepper_horizontal_disabled_ignores_left_right() {
        let mut stepper = NavStepper::default();
        let now = Instant::now();
        let left = PadSample {
            dpad_left: true,
            ..Default::default()
        };
        assert!(stepper.update(&left, now, false, true).is_none());
        let up = PadSample {
            dpad_up: true,
            ..Default::default()
        };
        assert_eq!(stepper.update(&up, now, false, true), Some(NavAction::Up));
    }

    #[test]
    fn sample_nav_resting_requires_clear_controls() {
        assert!(sample_nav_resting(&PadSample::default()));
        assert!(!sample_nav_resting(&PadSample {
            r2: true,
            ..Default::default()
        }));
        assert!(!sample_nav_resting(&PadSample {
            create: true,
            ..Default::default()
        }));
        assert!(!sample_nav_resting(&PadSample {
            options: true,
            ..Default::default()
        }));
        assert!(!sample_nav_resting(&PadSample {
            dpad_left: true,
            ..Default::default()
        }));
        assert!(!sample_nav_resting(&PadSample {
            stick_x: 0.5,
            ..Default::default()
        }));
        assert!(!sample_nav_resting(&PadSample {
            held: [GestureControl::Ps].into_iter().collect(),
            ..Default::default()
        }));
    }

    fn reading(id: &str, sample: PadSample) -> NavReading {
        NavReading {
            id: PadId(id.to_string()),
            sample,
            source: NAV_SOURCE_HID,
        }
    }

    /// Selector contract (B2.4): D-pad moves fire even with a bumper held.
    /// Engine mode gates the stepper off for the cursor; the App forces it
    /// back on while the history selector is open, which only works because
    /// bumper holds never disturb stepper state.
    #[test]
    fn dpad_moves_fire_with_bumper_held() {
        let mut bank = PadNavBank::default();
        bank.prepare_on_open(true);
        let now = Instant::now();
        let _ = bank.tick(
            &[reading("a", PadSample::default())],
            now,
            true,
            false,
            false,
            false,
            false,
            false,
            false,
        );
        let both = PadSample {
            l1: true,
            dpad_down: true,
            held: [GestureControl::L1].into_iter().collect(),
            ..Default::default()
        };
        let tick = bank.tick(
            &[reading("a", both)],
            now,
            true,
            false,
            false,
            false,
            false,
            false,
            false,
        );
        assert_eq!(tick.action, Some(NavAction::Down));
    }

    fn l3_sample(pressed: bool) -> PadSample {
        PadSample {
            l3: pressed,
            held: if pressed {
                [GestureControl::L3].into_iter().collect()
            } else {
                Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn interpreter_emits_l3_tap_only_after_debounced_press_release() {
        let mut interpreter = PadInterpreter::default();
        let pad = PadId("hid:a".into());
        let pressed = vec![l3_sample(true); usize::from(BUTTON_STABLE_REPORTS)];
        let released = vec![l3_sample(false); usize::from(BUTTON_STABLE_REPORTS)];
        assert!(
            interpreter.ingest(&pad, &pressed).is_empty(),
            "press alone must not tap"
        );
        let actions = interpreter.ingest(&pad, &released);
        assert_eq!(
            actions,
            vec![PadAction {
                pad: pad.clone(),
                button: PadButton::L3,
                action: PadButtonAction::Tap,
            }]
        );
        assert!(
            interpreter.ingest(&pad, &released).is_empty(),
            "a held release state must fire once"
        );
    }

    #[test]
    fn interpreter_foreign_press_cancels_pending_tap() {
        let mut interpreter = PadInterpreter::default();
        let pad = PadId("hid:a".into());
        let cross = PadSample {
            cross: true,
            ..Default::default()
        };
        let both = PadSample {
            cross: true,
            circle: true,
            ..Default::default()
        };
        let circle = PadSample {
            circle: true,
            ..Default::default()
        };
        let n = usize::from(BUTTON_STABLE_REPORTS);
        assert!(interpreter.ingest(&pad, &vec![cross.clone(); n]).is_empty());
        // Circle's press cancels Cross's pending release tap.
        assert!(interpreter.ingest(&pad, &vec![both.clone(); n]).is_empty());
        assert!(
            interpreter
                .ingest(&pad, &vec![circle.clone(); n])
                .is_empty(),
            "cancelled Cross release must not tap"
        );
        let released = interpreter.ingest(&pad, &vec![PadSample::default(); n]);
        assert_eq!(
            released,
            vec![PadAction {
                pad,
                button: PadButton::Circle,
                action: PadButtonAction::Tap,
            }]
        );
    }

    #[test]
    fn interpreter_rejects_l3_chatter() {
        let mut interpreter = PadInterpreter::default();
        let pad = PadId("hid:a".into());
        // Alternating reports never survive the stable-report window.
        let mut chatter = Vec::new();
        for _ in 0..8 {
            chatter.push(l3_sample(true));
            chatter.push(l3_sample(false));
        }
        assert!(interpreter.ingest(&pad, &chatter).is_empty());

        // A single short false blip while held is not a release.
        let n = usize::from(BUTTON_STABLE_REPORTS);
        assert!(
            interpreter
                .ingest(&pad, &vec![l3_sample(true); n])
                .is_empty()
        );
        assert!(interpreter.ingest(&pad, &[l3_sample(false)]).is_empty());
        assert!(
            interpreter
                .ingest(&pad, &vec![l3_sample(true); n])
                .is_empty()
        );
        assert_eq!(
            interpreter.ingest(&pad, &vec![l3_sample(false); n]).len(),
            1,
            "only a stable release completes the tap"
        );

        // A real held click after the chatter still produces exactly one tap.
        let press = vec![l3_sample(true); n];
        let release = vec![l3_sample(false); n];
        assert!(interpreter.ingest(&pad, &press).is_empty());
        assert_eq!(interpreter.ingest(&pad, &release).len(), 1);
    }

    #[test]
    fn action_gate_dedupes_and_rearms_only_after_rest() {
        let pad = PadId("hid:a".into());
        let action = PadAction {
            pad: pad.clone(),
            button: PadButton::Cross,
            action: PadButtonAction::Tap,
        };
        let resting = [reading("hid:a", PadSample::default())];
        let mut gate = PadActionGate::default();

        // Owner change: the action is filtered, then the resting frame arms.
        assert!(
            gate.begin(1, true, &resting, std::slice::from_ref(&action))
                .expect("first edge")
                .is_empty()
        );
        // Same sequence is a duplicate.
        assert!(
            gate.begin(1, false, &resting, std::slice::from_ref(&action))
                .is_none()
        );
        // Subsequent action on the armed pad passes.
        assert_eq!(
            gate.begin(2, false, &resting, std::slice::from_ref(&action))
                .expect("second edge"),
            vec![action.clone()]
        );

        // New owner + held pad: action is suppressed until a rest frame.
        let held = [reading(
            "hid:a",
            PadSample {
                l3: true,
                held: [GestureControl::L3].into_iter().collect(),
                ..Default::default()
            },
        )];
        assert!(
            gate.begin(3, true, &held, std::slice::from_ref(&action))
                .expect("held edge")
                .is_empty()
        );
        assert!(
            gate.begin(4, false, &resting, std::slice::from_ref(&action))
                .expect("rest edge")
                .is_empty()
        );
        assert_eq!(
            gate.begin(5, false, &resting, std::slice::from_ref(&action))
                .expect("post-rest edge")
                .len(),
            1
        );
    }

    fn l1_sample(pressed: bool) -> PadSample {
        PadSample {
            l1: pressed,
            held: if pressed {
                [GestureControl::L1].into_iter().collect()
            } else {
                Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn interpreter_emits_bumper_tap_on_short_release() {
        let mut interpreter = PadInterpreter::default();
        let pad = PadId("hid:a".into());
        let now = Instant::now();
        let n = usize::from(BUTTON_STABLE_REPORTS);
        assert!(
            interpreter
                .ingest_at(&pad, &vec![l1_sample(true); n], now)
                .is_empty()
        );
        assert_eq!(
            interpreter.ingest_at(
                &pad,
                &vec![l1_sample(false); n],
                now + Duration::from_millis(20)
            ),
            vec![PadAction {
                pad,
                button: PadButton::L1,
                action: PadButtonAction::Tap,
            }]
        );
    }

    #[test]
    fn interpreter_emits_bumper_hold_once_and_no_release_tap() {
        let mut interpreter = PadInterpreter::default();
        let pad = PadId("hid:a".into());
        let now = Instant::now();
        let n = usize::from(BUTTON_STABLE_REPORTS);
        assert!(
            interpreter
                .ingest_at(&pad, &vec![l1_sample(true); n], now)
                .is_empty()
        );
        assert_eq!(
            interpreter.ingest_at(&pad, &[], now + BUMPER_HOLD),
            vec![PadAction {
                pad: pad.clone(),
                button: PadButton::L1,
                action: PadButtonAction::Hold,
            }]
        );
        assert!(
            interpreter
                .ingest_at(&pad, &[], now + BUMPER_HOLD + Duration::from_millis(10))
                .is_empty(),
            "hold opens once"
        );
        assert!(
            interpreter
                .ingest_at(
                    &pad,
                    &vec![l1_sample(false); n],
                    now + BUMPER_HOLD + Duration::from_millis(20)
                )
                .is_empty(),
            "release after a hold is not also a tap"
        );
    }

    #[test]
    fn interpreter_emits_r3_tap_on_press() {
        let mut interpreter = PadInterpreter::default();
        let pad = PadId("hid:a".into());
        let reports = vec![
            PadSample {
                held: [GestureControl::R3].into_iter().collect(),
                ..Default::default()
            };
            usize::from(BUTTON_STABLE_REPORTS)
        ];
        assert_eq!(
            interpreter.ingest(&pad, &reports),
            vec![PadAction {
                pad,
                button: PadButton::R3,
                action: PadButtonAction::Tap,
            }]
        );
    }

    #[test]
    fn dpad_only_masks_sticks_buttons_and_gestures() {
        let full = PadSample {
            held: [GestureControl::L1, GestureControl::R3]
                .into_iter()
                .collect(),
            stick_x: 0.7,
            stick_y: -0.4,
            lstick_x: 0.5,
            lstick_y: 0.2,
            rstick_x: -0.3,
            rstick_y: 0.9,
            dpad_up: true,
            dpad_left: true,
            cross: true,
            l1: true,
            r2: true,
            ..Default::default()
        };
        let masked = dpad_only(&full);
        assert!(masked.dpad_up);
        assert!(masked.dpad_left);
        assert!(!masked.dpad_down);
        assert!(!masked.dpad_right);
        assert_eq!(masked.stick_x, 0.0);
        assert_eq!(masked.stick_y, 0.0);
        assert_eq!(masked.lstick_x, 0.0);
        assert_eq!(masked.rstick_x, 0.0);
        assert!(masked.held.is_empty());
        assert!(!masked.cross && !masked.l1 && !masked.r2);
    }

    #[test]
    fn input_hub_read_missing_before_publish() {
        let hub = InputHub::new();
        match hub.read() {
            NavReadingsOutcome::Missing { meta } => {
                assert_eq!(meta.seq, 0);
                assert_eq!(meta.reason, SnapshotReason::NeverPublished);
                assert_eq!(meta.pad_count, 0);
            }
            NavReadingsOutcome::Readings { .. } => panic!("expected Missing before publish"),
        }
    }

    #[test]
    fn input_hub_publish_then_read_bumps_seq() {
        let hub = InputHub::new();
        let meta = hub.publish(
            vec![reading("hid:pad1", PadSample::default())],
            SnapshotReason::Sample,
        );
        assert_eq!(meta.seq, 1);
        assert_eq!(meta.reason, SnapshotReason::Sample);
        assert_eq!(meta.pad_count, 1);
        match hub.read() {
            NavReadingsOutcome::Readings { readings, meta } => {
                assert_eq!(readings.len(), 1);
                assert_eq!(readings[0].id.0, "hid:pad1");
                assert_eq!(meta.seq, 1);
            }
            NavReadingsOutcome::Missing { .. } => panic!("expected Readings after publish"),
        }
        let meta = hub.publish(Vec::new(), SnapshotReason::ClearedPowerOff);
        assert_eq!(meta.seq, 2);
        match hub.read() {
            NavReadingsOutcome::Missing { meta } => {
                assert_eq!(meta.reason, SnapshotReason::ClearedPowerOff);
                assert_eq!(meta.seq, 2);
            }
            NavReadingsOutcome::Readings { .. } => panic!("expected Missing after clear publish"),
        }
    }

    #[test]
    fn input_hub_latest_reading_honors_age() {
        let hub = InputHub::new();
        hub.publish(
            vec![reading(
                "hid:pad1",
                PadSample {
                    lstick_x: -0.95,
                    ..Default::default()
                },
            )],
            SnapshotReason::Sample,
        );
        let latest = hub
            .latest_reading(Duration::from_millis(200))
            .expect("fresh snapshot");
        assert_eq!(latest.sample.lstick_x, -0.95);
        std::thread::sleep(Duration::from_millis(2));
        assert!(
            hub.latest_reading(Duration::from_millis(1)).is_none(),
            "a stale snapshot must be treated as neutral"
        );
    }

    #[test]
    fn input_hub_push_edge_without_listener_is_noop() {
        let hub = InputHub::new();
        let edge = InputEdge::from_snapshot(
            Vec::new(),
            Vec::new(),
            &SnapshotMeta {
                published_at: Instant::now(),
                seq: 7,
                reason: SnapshotReason::Sample,
                pad_count: 0,
            },
        );
        hub.push_edge(edge);
    }

    #[test]
    fn input_hub_bind_push_clear_edge_bridge() {
        let hub = InputHub::new();
        let (tx, mut rx) = iced::futures::channel::mpsc::channel::<InputEdge>(8);
        hub.bind(tx);
        let edge = InputEdge::from_snapshot(
            vec![reading("hid:pad1", PadSample::default())],
            Vec::new(),
            &SnapshotMeta {
                published_at: Instant::now(),
                seq: 3,
                reason: SnapshotReason::Sample,
                pad_count: 1,
            },
        );
        hub.push_edge(edge);
        let got = rx.try_recv().expect("edge bridged while bound");
        assert_eq!(got.seq, 3);
        assert_eq!(got.pad_count, 1);
        hub.clear();
        hub.push_edge(InputEdge::from_snapshot(
            Vec::new(),
            Vec::new(),
            &SnapshotMeta {
                published_at: Instant::now(),
                seq: 4,
                reason: SnapshotReason::Sample,
                pad_count: 0,
            },
        ));
        assert!(rx.try_recv().is_err());
    }

    fn edge_at(
        readings: Vec<NavReading>,
        actions: Vec<PadAction>,
        seq: u64,
        reason: SnapshotReason,
    ) -> InputEdge {
        InputEdge::from_snapshot(
            readings,
            actions,
            &SnapshotMeta {
                published_at: Instant::now(),
                seq,
                reason,
                pad_count: 0,
            },
        )
    }

    #[test]
    fn input_hub_gates_resting_neutral_edges() {
        let hub = InputHub::new();
        let (tx, mut rx) = iced::futures::channel::mpsc::channel::<InputEdge>(16);
        hub.bind(tx);
        let t0 = Instant::now();
        hub.push_edge_at(
            edge_at(
                vec![reading("a", PadSample::default())],
                Vec::new(),
                1,
                SnapshotReason::Sample,
            ),
            t0,
        );
        assert!(rx.try_recv().is_ok(), "first sample publishes");
        hub.push_edge_at(
            edge_at(
                vec![reading("a", PadSample::default())],
                Vec::new(),
                2,
                SnapshotReason::Sample,
            ),
            t0 + Duration::from_millis(1),
        );
        assert!(rx.try_recv().is_err(), "resting neutral produces no edge");
    }

    #[test]
    fn input_hub_throttles_stick_changes_to_display_rate() {
        let hub = InputHub::new();
        let (tx, mut rx) = iced::futures::channel::mpsc::channel::<InputEdge>(16);
        hub.bind(tx);
        let t0 = Instant::now();
        hub.push_edge_at(
            edge_at(
                vec![reading("a", PadSample::default())],
                Vec::new(),
                1,
                SnapshotReason::Sample,
            ),
            t0,
        );
        let _ = rx.try_recv();
        let held = PadSample {
            lstick_x: -0.95,
            stick_x: -0.95,
            ..Default::default()
        };
        hub.push_edge_at(
            edge_at(
                vec![reading("a", held.clone())],
                Vec::new(),
                2,
                SnapshotReason::Sample,
            ),
            t0 + Duration::from_millis(1),
        );
        assert!(
            rx.try_recv().is_err(),
            "change inside the frame is coalesced"
        );
        hub.push_edge_at(
            edge_at(
                vec![reading("a", held.clone())],
                Vec::new(),
                3,
                SnapshotReason::Sample,
            ),
            t0 + Duration::from_millis(20),
        );
        assert!(rx.try_recv().is_ok(), "display-rate sample publishes");
        hub.push_edge_at(
            edge_at(
                vec![reading("a", held)],
                Vec::new(),
                4,
                SnapshotReason::Sample,
            ),
            t0 + Duration::from_millis(40),
        );
        assert!(rx.try_recv().is_ok(), "held analog keeps repeat cadence");
    }

    #[test]
    fn input_hub_keeps_neutral_tail_then_stops() {
        let hub = InputHub::new();
        let (tx, mut rx) = iced::futures::channel::mpsc::channel::<InputEdge>(16);
        hub.bind(tx);
        let t0 = Instant::now();
        hub.push_edge_at(
            edge_at(
                vec![reading("a", PadSample::default())],
                Vec::new(),
                1,
                SnapshotReason::Sample,
            ),
            t0,
        );
        let _ = rx.try_recv();
        let held = PadSample {
            lstick_x: -0.95,
            stick_x: -0.95,
            ..Default::default()
        };
        hub.push_edge_at(
            edge_at(
                vec![reading("a", held)],
                Vec::new(),
                2,
                SnapshotReason::Sample,
            ),
            t0 + Duration::from_millis(20),
        );
        let _ = rx.try_recv();
        hub.push_edge_at(
            edge_at(
                vec![reading("a", PadSample::default())],
                Vec::new(),
                3,
                SnapshotReason::Sample,
            ),
            t0 + Duration::from_millis(40),
        );
        assert!(rx.try_recv().is_ok(), "release publishes at once");
        hub.push_edge_at(
            edge_at(
                vec![reading("a", PadSample::default())],
                Vec::new(),
                4,
                SnapshotReason::Sample,
            ),
            t0 + Duration::from_millis(60),
        );
        assert!(
            rx.try_recv().is_ok(),
            "neutral tail keeps the stepper ticking"
        );
        hub.push_edge_at(
            edge_at(
                vec![reading("a", PadSample::default())],
                Vec::new(),
                5,
                SnapshotReason::Sample,
            ),
            t0 + Duration::from_millis(240),
        );
        assert!(rx.try_recv().is_err(), "tail ends after the grace window");
    }

    #[test]
    fn input_hub_actions_bypass_analog_throttle() {
        let hub = InputHub::new();
        let (tx, mut rx) = iced::futures::channel::mpsc::channel::<InputEdge>(16);
        hub.bind(tx);
        let t0 = Instant::now();
        hub.push_edge_at(
            edge_at(
                vec![reading("a", PadSample::default())],
                Vec::new(),
                1,
                SnapshotReason::Sample,
            ),
            t0,
        );
        let _ = rx.try_recv();
        hub.push_edge_at(
            edge_at(
                vec![reading("a", PadSample::default())],
                vec![PadAction {
                    pad: PadId("a".into()),
                    button: PadButton::Cross,
                    action: PadButtonAction::Tap,
                }],
                2,
                SnapshotReason::Sample,
            ),
            t0 + Duration::from_millis(1),
        );
        assert!(rx.try_recv().is_ok(), "taps are never delayed");
    }

    #[test]
    fn osk_slot_tracks_every_hid_publication() {
        let hub = InputHub::new();
        hub.set_osk_listening(true);
        hub.publish(
            vec![reading("a", PadSample::default())],
            SnapshotReason::Sample,
        );
        let slot = hub.osk_slot();
        let (seq, readings) = slot.read();
        assert_eq!(seq, 1);
        assert_eq!(readings.len(), 1);
        hub.publish(Vec::new(), SnapshotReason::ClearedPowerOff);
        let (seq, readings) = slot.read();
        assert_eq!(seq, 2);
        assert!(readings.is_empty());
    }

    fn chord_sample(controls: &[GestureControl]) -> PadSample {
        let mut sample = PadSample::default();
        for c in controls {
            sample.held.insert(*c);
            match c {
                GestureControl::L2 => sample.l2 = true,
                GestureControl::R2 => sample.r2 = true,
                GestureControl::Cross => sample.cross = true,
                GestureControl::Circle => sample.circle = true,
                GestureControl::Square => sample.square = true,
                GestureControl::Triangle => sample.triangle = true,
                GestureControl::Options => sample.options = true,
                GestureControl::Create => sample.create = true,
                _ => {}
            }
        }
        sample
    }

    #[test]
    fn interpreter_keeps_taps_per_pad() {
        let mut interpreters = PadInterpreterBank::default();
        let pad_a = PadId("hid:a".into());
        let pad_b = PadId("hid:b".into());
        let open = chord_sample(&[GestureControl::L2, GestureControl::R2]);
        assert!(
            interpreters
                .ingest(&pad_a, &[open.clone(), open.clone(), open])
                .is_empty()
        );

        let cross = PadSample {
            cross: true,
            held: [GestureControl::Cross].into_iter().collect(),
            ..Default::default()
        };
        let press = [cross.clone(), cross.clone(), cross];
        let release = [
            PadSample::default(),
            PadSample::default(),
            PadSample::default(),
        ];
        assert!(interpreters.ingest(&pad_b, &press).is_empty());
        assert_eq!(
            interpreters.ingest(&pad_b, &release),
            vec![PadAction {
                pad: pad_b,
                button: PadButton::Cross,
                action: PadButtonAction::Tap,
            }]
        );
    }

    #[test]
    fn reopen_gesture_requires_full_chord_on_one_pad() {
        let mut bank = GestureDetectorBank::default();
        let required = crate::ui::launcher::gesture::default_gesture();

        let a = chord_sample(&[GestureControl::L2]);
        let b = chord_sample(&[GestureControl::R2]);
        assert!(!bank.update(&required, &[reading("a", a), reading("b", b)]));

        let full = chord_sample(&[GestureControl::Ps]);
        assert!(bank.update(
            &required,
            &[reading("a", full), reading("b", PadSample::default())]
        ));
    }

    #[test]
    fn consume_pending_match_suppresses_sticky_reopen() {
        let mut bank = GestureDetectorBank::default();
        let required = crate::ui::launcher::gesture::default_gesture();
        let empty = [reading("a", PadSample::default())];
        bank.consume_pending_match(&empty);

        let sticky = chord_sample(&[GestureControl::Ps]);
        assert!(
            !bank.update(&required, &[reading("a", sticky.clone())]),
            "sticky rematch after record must not fire"
        );

        assert!(!bank.update(&required, &[reading("a", PadSample::default())]));
        assert!(bank.update(&required, &[reading("a", sticky)]));
    }

    #[test]
    fn new_pad_holding_cross_does_not_confirm() {
        let mut bank = PadNavBank::default();
        bank.prepare_on_open(false);
        let now = Instant::now();
        let cross = PadSample {
            cross: true,
            held: [GestureControl::Cross].into_iter().collect(),
            ..Default::default()
        };
        let tick = bank.tick(
            &[reading("new", cross)],
            now,
            true,
            false,
            false,
            false,
            false,
            false,
            false,
        );
        assert!(tick.action.is_none());
    }

    #[test]
    fn hold_completes_on_release_when_full() {
        let mut hold = HoldTracker::default();
        let t0 = Instant::now();
        let (p0, c0) = hold.update(true, t0);
        assert!(p0 < 1.0 && !c0);
        let (p1, c1) = hold.update(true, t0 + Duration::from_millis(500));
        assert!(p1 > 0.0 && p1 < 1.0 && !c1);
        // Ease-out is well ahead of linear time at mid-hold.
        assert!(
            p1 > 0.75,
            "ease-out should be ahead of linear 0.5 at 500ms, got {p1}"
        );
        let (p2, c2) = hold.update(true, t0 + Duration::from_secs(1));
        assert_eq!(p2, 1.0);
        assert!(!c2, "stays armed while held at full; does not fire yet");
        assert!(hold.is_active(), "full hold remains cancellable");
        let (p3, c3) = hold.update(
            true,
            t0 + Duration::from_secs(1) + Duration::from_millis(10),
        );
        assert_eq!(p3, 1.0);
        assert!(!c3, "does not fire while still held");
        let (p4, c4) = hold.update(false, t0 + Duration::from_secs(2));
        assert_eq!(p4, 0.0);
        assert!(c4, "fires on release when full and not cancelled");
    }

    #[test]
    fn hold_full_cancel_then_release_does_not_fire() {
        let mut hold = HoldTracker::default();
        let t0 = Instant::now();
        let _ = hold.update(true, t0);
        let _ = hold.update(true, t0 + Duration::from_secs(1));
        assert!(hold.is_active());
        hold.cancel();
        assert!(!hold.is_active());
        let (p, c) = hold.update(
            true,
            t0 + Duration::from_secs(1) + Duration::from_millis(10),
        );
        assert_eq!(p, 0.0);
        assert!(!c);
        let (_, c2) = hold.update(false, t0 + Duration::from_secs(2));
        assert!(!c2, "cancelled full hold must not fire on release");
    }

    #[test]
    fn hold_ease_out_is_ahead_of_linear_early() {
        let mut hold = HoldTracker::default();
        let t0 = Instant::now();
        let _ = hold.update(true, t0);
        let (p, _) = hold.update(true, t0 + Duration::from_millis(100));
        // Linear would be 0.1; ease-out ≈ 1 - 0.9^3 ≈ 0.271.
        assert!(p > 0.25 && p < 0.35, "expected ~0.27 at 100ms, got {p}");
        let (p2, _) = hold.update(true, t0 + Duration::from_millis(250));
        assert!(p2 > 0.55 && p2 < 0.65, "expected ~0.58 at 250ms, got {p2}");
    }

    #[test]
    fn hold_early_release_decays_instead_of_snap() {
        let mut hold = HoldTracker::default();
        let t0 = Instant::now();
        let _ = hold.update(true, t0);
        let (p_held, c) = hold.update(true, t0 + Duration::from_millis(400));
        assert!(!c);
        assert!(p_held > 0.5);
        let (p_rel, c_rel) = hold.update(false, t0 + Duration::from_millis(400));
        assert!(!c_rel);
        assert!(
            (p_rel - p_held).abs() < 0.02,
            "first release frame keeps progress"
        );
        assert!(!hold.is_active(), "decaying is not an active hold");
        let (p_later, _) = hold.update(
            false,
            t0 + Duration::from_millis(400) + Duration::from_millis(300),
        );
        assert!(
            p_later < p_rel && p_later > 0.0,
            "progress decays over time"
        );
    }

    #[test]
    fn hold_decay_is_pulled_from_wall_clock_without_edges() {
        let mut hold = HoldTracker::default();
        let t0 = Instant::now();
        let _ = hold.update(true, t0);
        let _ = hold.update(true, t0 + Duration::from_millis(400));
        let (p_rel, _) = hold.update(false, t0 + Duration::from_millis(400));
        assert!(p_rel > 0.5);
        // No further update calls: the ring pose must still decay.
        let p_mid = hold.progress_at(t0 + Duration::from_millis(700));
        assert!(p_mid < p_rel && p_mid > 0.0, "mid decay={p_mid}");
        let p_end = hold.progress_at(t0 + Duration::from_millis(2000));
        assert_eq!(p_end, 0.0, "decay completes on the wall clock");
        assert!(!hold.is_animating(t0 + Duration::from_millis(2000)));
    }

    #[test]
    fn hold_repress_mid_decay_resumes_from_drawn_pose() {
        let mut hold = HoldTracker::default();
        let t0 = Instant::now();
        let _ = hold.update(true, t0);
        let (p_held, _) = hold.update(true, t0 + Duration::from_millis(500));
        let (_, _) = hold.update(false, t0 + Duration::from_millis(500));
        let p_mid = hold.progress_at(t0 + Duration::from_millis(800));
        assert!(p_mid < p_held && p_mid > 0.0);
        let (p_press, _) = hold.update(true, t0 + Duration::from_millis(800));
        assert!(
            (p_press - p_mid).abs() < 0.001,
            "re-press resumes from the visible pose"
        );
    }

    #[test]
    fn hold_partial_presses_stack_to_completion() {
        let mut hold = HoldTracker::default();
        let t0 = Instant::now();
        let _ = hold.update(true, t0);
        let _ = hold.update(true, t0 + Duration::from_millis(500));
        let (p0, c0) = hold.update(false, t0 + Duration::from_millis(500));
        assert!(!c0 && p0 > 0.8);
        // Brief decay, then press again — finish without a full unbroken second.
        let t1 = t0 + Duration::from_millis(550);
        let _ = hold.update(false, t1);
        let _ = hold.update(true, t1);
        let (p1, c1) = hold.update(true, t1 + Duration::from_millis(550));
        assert!(!c1, "reaches full while held without firing");
        assert_eq!(p1, 1.0);
        let (_, c2) = hold.update(false, t1 + Duration::from_millis(560));
        assert!(c2, "stacked partial holds fire on release when full");
    }

    #[test]
    fn hold_cancel_suppresses_until_release() {
        let mut hold = HoldTracker::default();
        let t0 = Instant::now();
        let _ = hold.update(true, t0);
        let _ = hold.update(true, t0 + Duration::from_millis(500));
        assert!(hold.is_active());
        hold.cancel();
        assert!(!hold.is_active());
        let (p, c) = hold.update(
            true,
            t0 + Duration::from_secs(1) + Duration::from_millis(50),
        );
        assert_eq!(p, 0.0);
        assert!(!c);
        let (_, c2) = hold.update(false, t0 + Duration::from_secs(2));
        assert!(!c2);
        // New press after cancel+release can arm again, then fire on release.
        let _ = hold.update(true, t0 + Duration::from_secs(2));
        let (_, c3) = hold.update(true, t0 + Duration::from_secs(3));
        assert!(!c3);
        let (_, c4) = hold.update(
            false,
            t0 + Duration::from_secs(3) + Duration::from_millis(10),
        );
        assert!(c4);
    }

    #[test]
    fn hold_cancel_clears_decaying_charge() {
        let mut hold = HoldTracker::default();
        let t0 = Instant::now();
        let _ = hold.update(true, t0);
        let _ = hold.update(true, t0 + Duration::from_millis(400));
        let _ = hold.update(false, t0 + Duration::from_millis(400));
        hold.cancel();
        let (p, _) = hold.update(false, t0 + Duration::from_millis(500));
        assert_eq!(p, 0.0);
    }

    #[test]
    fn hold_cross_close_early_release_decays_without_confirm() {
        let mut bank = PadNavBank::default();
        bank.prepare_on_open(true);
        let t0 = Instant::now();
        let _ = bank.tick(
            &[reading("a", PadSample::default())],
            t0,
            true,
            false,
            false,
            false,
            false,
            true,
            false,
        );

        let cross = PadSample {
            cross: true,
            held: [GestureControl::Cross].into_iter().collect(),
            ..Default::default()
        };
        let t_press = t0 + Duration::from_millis(10);
        let _ = bank.tick(
            &[reading("a", cross.clone())],
            t_press,
            true,
            false,
            false,
            false,
            false,
            true,
            false,
        );
        let tick_held = bank.tick(
            &[reading("a", cross)],
            t_press + Duration::from_millis(400),
            true,
            false,
            false,
            false,
            false,
            true,
            false,
        );
        assert!(tick_held.cross_progress > 0.5);
        assert!(!tick_held.cross_completed);
        assert!(tick_held.action.is_none());

        let t_rel = t_press + Duration::from_millis(400);
        let tick_rel = bank.tick(
            &[reading("a", PadSample::default())],
            t_rel,
            true,
            false,
            false,
            false,
            false,
            true,
            false,
        );
        assert!(
            tick_rel.action.is_none(),
            "owned Cross must not Confirm on early release"
        );
        assert!(!tick_rel.cross_completed);
        assert!(
            (tick_rel.cross_progress - tick_held.cross_progress).abs() < 0.02,
            "first release frame keeps progress"
        );

        let tick_later = bank.tick(
            &[reading("a", PadSample::default())],
            t_rel + Duration::from_millis(300),
            true,
            false,
            false,
            false,
            false,
            true,
            false,
        );
        assert!(
            tick_later.cross_progress < tick_rel.cross_progress && tick_later.cross_progress > 0.0,
            "progress decays over time"
        );
        assert!(tick_later.action.is_none());
    }

    #[test]
    fn nav_bank_hold_progress_decays_after_edges_stop() {
        let mut bank = PadNavBank::default();
        bank.prepare_on_open(true);
        let t0 = Instant::now();
        let cross = PadSample {
            cross: true,
            held: [GestureControl::Cross].into_iter().collect(),
            ..Default::default()
        };
        let _ = bank.tick(
            &[reading("a", PadSample::default())],
            t0,
            true,
            false,
            false,
            false,
            false,
            true,
            false,
        );
        let tick_held = bank.tick(
            &[reading("a", cross.clone())],
            t0 + Duration::from_millis(100),
            true,
            false,
            false,
            false,
            false,
            true,
            false,
        );
        assert_eq!(tick_held.cross_progress, 0.0, "first press just arms");
        let tick_held = bank.tick(
            &[reading("a", cross)],
            t0 + Duration::from_millis(300),
            true,
            false,
            false,
            false,
            false,
            true,
            false,
        );
        assert!(tick_held.cross_progress > 0.0);
        let _ = bank.tick(
            &[reading("a", PadSample::default())],
            t0 + Duration::from_millis(400),
            true,
            false,
            false,
            false,
            false,
            true,
            false,
        );
        let (released, _) = bank.hold_progress(t0 + Duration::from_millis(400));
        assert!(released > 0.0);
        // No further tick calls: the frame clock still sees decay.
        let (later, _) = bank.hold_progress(t0 + Duration::from_millis(900));
        assert!(later < released && later > 0.0, "later={later}");
        assert!(bank.has_hold_activity());
        let (done, _) = bank.hold_progress(t0 + Duration::from_millis(2500));
        assert_eq!(done, 0.0);
    }

    #[test]
    fn pad_tick_reports_held_face_buttons() {
        let mut bank = PadNavBank::default();
        bank.prepare_on_open(true);
        let now = Instant::now();
        let resting = PadSample::default();
        let _ = bank.tick(
            &[reading("a", resting)],
            now,
            true,
            false,
            false,
            false,
            false,
            false,
            false,
        );
        let sample = PadSample {
            cross: true,
            square: true,
            held: [GestureControl::Cross, GestureControl::Square]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let tick = bank.tick(
            &[reading("a", sample)],
            now,
            true,
            false,
            false,
            false,
            false,
            false,
            false,
        );
        assert!(tick.held.cross);
        assert!(tick.held.square);
        assert!(!tick.held.circle);
        assert!(!tick.held.triangle);
    }

    #[test]
    fn hold_ease_out_roundtrip() {
        for u in [0.0_f32, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0] {
            let p = hold_ease_out(u);
            let back = hold_ease_out_inv(p);
            assert!((back - u).abs() < 1e-5, "u={u} p={p} back={back}");
        }
    }

    fn tick_games(
        bank: &mut PadNavBank,
        sample: PadSample,
        now: Instant,
        horizontal_games_nav: bool,
    ) -> PadTickResult {
        bank.tick(
            &[reading("a", sample)],
            now,
            true,
            true,
            horizontal_games_nav,
            false,
            false,
            false,
            false,
        )
    }

    #[test]
    fn tick_horizontal_games_nav_left_right_scroll_games() {
        let mut bank = PadNavBank::default();
        bank.prepare_on_open(true);
        let now = Instant::now();

        let left = PadSample {
            dpad_left: true,
            ..Default::default()
        };
        assert_eq!(
            tick_games(&mut bank, left, now, true).action,
            Some(NavAction::Up)
        );

        // Release resets the stepper so the opposite direction fires at once.
        assert!(
            tick_games(&mut bank, PadSample::default(), now, true)
                .action
                .is_none()
        );
        let right = PadSample {
            stick_x: 0.9,
            ..Default::default()
        };
        assert_eq!(
            tick_games(&mut bank, right, now, true).action,
            Some(NavAction::Down)
        );

        // Up/Down do nothing on the horizontal Games strip (Left/Right only).
        assert!(
            tick_games(&mut bank, PadSample::default(), now, true)
                .action
                .is_none()
        );
        let up = PadSample {
            dpad_up: true,
            ..Default::default()
        };
        assert!(tick_games(&mut bank, up, now, true).action.is_none());
        assert!(
            tick_games(&mut bank, PadSample::default(), now, true)
                .action
                .is_none()
        );
        let down = PadSample {
            dpad_down: true,
            ..Default::default()
        };
        assert!(tick_games(&mut bank, down, now, true).action.is_none());
    }

    #[test]
    fn tick_horizontal_games_nav_triggers_are_inert() {
        let mut bank = PadNavBank::default();
        bank.prepare_on_open(true);
        let now = Instant::now();
        // Register the pad at rest first (first sight syncs edges without firing).
        assert!(
            tick_games(&mut bank, PadSample::default(), now, true)
                .action
                .is_none()
        );

        let l2 = PadSample {
            l2: true,
            ..Default::default()
        };
        assert!(tick_games(&mut bank, l2, now, true).action.is_none());
        assert!(
            tick_games(&mut bank, PadSample::default(), now, true)
                .action
                .is_none()
        );
        let r2 = PadSample {
            r2: true,
            ..Default::default()
        };
        assert!(tick_games(&mut bank, r2, now, true).action.is_none());
    }

    fn tick_games_editing(bank: &mut PadNavBank, sample: PadSample, now: Instant) -> PadTickResult {
        bank.tick(
            &[reading("a", sample)],
            now,
            true,
            true,
            true,
            false,
            true,
            false,
            false,
        )
    }

    #[test]
    fn tick_horizontal_games_nav_editing_left_right_move_selection() {
        // Edit mode on the horizontal Games strip: Left/Right must scroll
        // games (Up/Down moves). Up/Down stay suppressed; face taps are
        // source-side actions and no longer flow through this bank.
        let mut bank = PadNavBank::default();
        bank.prepare_on_open(true);
        let now = Instant::now();
        // Register the pad at rest first (first sight syncs edges without firing).
        assert!(
            tick_games_editing(&mut bank, PadSample::default(), now)
                .action
                .is_none()
        );

        let left = PadSample {
            dpad_left: true,
            ..Default::default()
        };
        assert_eq!(
            tick_games_editing(&mut bank, left, now).action,
            Some(NavAction::Up)
        );

        // Release resets the stepper so the opposite direction fires at once.
        assert!(
            tick_games_editing(&mut bank, PadSample::default(), now)
                .action
                .is_none()
        );
        let right = PadSample {
            dpad_right: true,
            ..Default::default()
        };
        assert_eq!(
            tick_games_editing(&mut bank, right, now).action,
            Some(NavAction::Down)
        );

        // Up/Down do nothing on the horizontal strip, including in edit mode.
        assert!(
            tick_games_editing(&mut bank, PadSample::default(), now)
                .action
                .is_none()
        );
        let up = PadSample {
            dpad_up: true,
            ..Default::default()
        };
        assert!(tick_games_editing(&mut bank, up, now).action.is_none());
        assert!(
            tick_games_editing(&mut bank, PadSample::default(), now)
                .action
                .is_none()
        );
        let down = PadSample {
            dpad_down: true,
            ..Default::default()
        };
        assert!(tick_games_editing(&mut bank, down, now).action.is_none());

        // L2/R2 are inert in edit mode.
        assert!(
            tick_games_editing(&mut bank, PadSample::default(), now)
                .action
                .is_none()
        );
        let l2 = PadSample {
            l2: true,
            ..Default::default()
        };
        assert!(tick_games_editing(&mut bank, l2, now).action.is_none());
        assert!(
            tick_games_editing(&mut bank, PadSample::default(), now)
                .action
                .is_none()
        );
        let r2 = PadSample {
            r2: true,
            ..Default::default()
        };
        assert!(tick_games_editing(&mut bank, r2, now).action.is_none());
    }

    #[test]
    fn tick_vertical_suppresses_triggers() {
        let mut bank = PadNavBank::default();
        bank.prepare_on_open(true);
        let now = Instant::now();

        // Triggers stay suppressed.
        let left = PadSample {
            dpad_left: true,
            ..Default::default()
        };
        assert_eq!(
            tick_games(&mut bank, left, now, false).action,
            Some(NavAction::Left)
        );
        assert!(
            tick_games(&mut bank, PadSample::default(), now, false)
                .action
                .is_none()
        );
        let l2 = PadSample {
            l2: true,
            ..Default::default()
        };
        assert!(tick_games(&mut bank, l2, now, false).action.is_none());
    }

    #[test]
    fn nav_stepper_suppressed_vertical_wobble_stays_gated() {
        // Left stick held leftward with small Y wobble: vertical must not enter
        // direction state, so repeat gating holds (1 immediate + gated repeats).
        let mut stepper = NavStepper::default();
        let t0 = Instant::now();
        let left = PadSample {
            stick_x: -0.9,
            ..Default::default()
        };
        let left_wobble_up = PadSample {
            stick_x: -0.9,
            stick_y: -0.2,
            ..Default::default()
        };
        assert_eq!(
            stepper.update(&left, t0, true, false),
            Some(NavAction::Left)
        );
        // Alternate pure-left / left+slight-up every 10ms across the initial
        // delay: no wobble tick may fire.
        let mut fires = 1;
        for ms in (10..280).step_by(10) {
            let sample = if (ms / 10) % 2 == 0 {
                left.clone()
            } else {
                left_wobble_up.clone()
            };
            let fired = stepper
                .update(&sample, t0 + Duration::from_millis(ms), true, false)
                .is_some();
            assert!(!fired, "wobble tick at +{ms}ms must stay gated");
            if fired {
                fires += 1;
            }
        }
        // First gated repeat at +280ms even on a wobble sample.
        assert_eq!(
            stepper.update(
                &left_wobble_up,
                t0 + Duration::from_millis(280),
                true,
                false
            ),
            Some(NavAction::Left)
        );
        fires += 1;
        // Then every NAV_REPEAT (90ms): +370ms fires, midpoints stay gated.
        assert!(
            stepper
                .update(&left, t0 + Duration::from_millis(325), true, false)
                .is_none()
        );
        assert_eq!(
            stepper.update(
                &left_wobble_up,
                t0 + Duration::from_millis(370),
                true,
                false
            ),
            Some(NavAction::Left)
        );
        fires += 1;
        assert_eq!(fires, 3);
    }

    #[test]
    fn tick_horizontal_games_nav_left_wobble_stays_gated() {
        // End-to-end: horizontal Games strip maps stepper Left/Right to Up/Down and
        // drops raw Up/Down; alternating left / left+slight-up must scroll games
        // at gated repeat rate, not once per wobble flip.
        let mut bank = PadNavBank::default();
        bank.prepare_on_open(true);
        let t0 = Instant::now();
        let left = PadSample {
            stick_x: -0.9,
            ..Default::default()
        };
        let left_wobble_up = PadSample {
            stick_x: -0.9,
            stick_y: -0.2,
            ..Default::default()
        };
        assert!(
            tick_games(&mut bank, PadSample::default(), t0, true)
                .action
                .is_none()
        );
        let mut fires = 0;
        // 61 ticks every 10ms over 600ms: fires at 0, 280, 370, 460, 550.
        for ms in (0..=600).step_by(10) {
            let sample = if (ms / 10) % 2 == 0 {
                left.clone()
            } else {
                left_wobble_up.clone()
            };
            let action = tick_games(
                &mut bank,
                sample,
                t0 + Duration::from_millis(ms as u64),
                true,
            )
            .action;
            if let Some(action) = action {
                assert_eq!(action, NavAction::Up, "left scrolls games at +{ms}ms");
                fires += 1;
            }
        }
        assert_eq!(fires, 5, "wobble must not bypass repeat gating");
    }

    /// Repro: steady left-held LEFT stick whose report bytes hover at the 0.9
    /// per-axis deadzone (raw 11 <-> 14, a 3-LSB wobble from a worn stick that
    /// can no longer hold full deflection) must stay repeat-gated. Each
    /// dropout to neutral currently resets the stepper, so every return fires
    /// immediately and leftward scroll runs far too fast.
    #[test]
    fn repro_left_stick_boundary_hover_stays_gated() {
        // Parse-level: prove the byte jitter really does straddle the deadzone.
        let mut left_hold = [0u8; 64];
        left_hold[0] = 0x01;
        left_hold[1] = 11; // (11-128)/128 = -0.914 -> passes 0.9
        left_hold[2] = 128;
        left_hold[3] = 128;
        left_hold[4] = 128;
        left_hold[8] = 0x08; // hat neutral
        let mut left_drop = left_hold;
        left_drop[1] = 14; // (14-128)/128 = -0.890625 -> killed -> 0.0
        let held = parse_report(&left_hold, 0);
        let dropped = parse_report(&left_drop, 0);
        assert!(held.stick_x < 0.0, "raw 11 must deflect left");
        assert_eq!(dropped.stick_x, 0.0, "raw 14 must read neutral");

        // Stepper alone: alternate every 10ms for 1s (100 ticks).
        // Gated expectation: fires at 0, 280, 370, ..., 910 -> 9 total.
        let mut stepper = NavStepper::default();
        let t0 = Instant::now();
        let mut fires = 0;
        for ms in 0..100 {
            let sample = if ms % 2 == 0 {
                held.clone()
            } else {
                dropped.clone()
            };
            if stepper
                .update(&sample, t0 + Duration::from_millis(ms * 10), true, false)
                .is_some()
            {
                fires += 1;
            }
        }
        assert_eq!(fires, 9, "boundary hover must stay gated (stepper)");

        // End-to-end through the bank in horizontal_games_nav mode.
        let mut bank = PadNavBank::default();
        bank.prepare_on_open(true);
        let mut bank_fires = 0;
        for ms in 0..100 {
            let sample = if ms % 2 == 0 {
                held.clone()
            } else {
                dropped.clone()
            };
            let action =
                tick_games(&mut bank, sample, t0 + Duration::from_millis(ms * 10), true).action;
            if let Some(action) = action {
                assert_eq!(action, NavAction::Up, "left scrolls games");
                bank_fires += 1;
            }
        }
        assert_eq!(bank_fires, 9, "boundary hover must stay gated (bank)");
    }

    /// Controls: solid full deflection on either stick / either direction, and
    /// d-pad left, are already stable and must stay gated at the same rate.
    #[test]
    fn repro_solid_holds_stay_gated_all_sticks_both_directions() {
        fn usb_report(lx: u8, ly: u8, rx: u8, ry: u8, hat: u8) -> PadSample {
            let mut buf = [0u8; 64];
            buf[0] = 0x01;
            buf[1] = lx;
            buf[2] = ly;
            buf[3] = rx;
            buf[4] = ry;
            buf[8] = hat;
            parse_report(&buf, 0)
        }
        let cases: [(&str, PadSample); 5] = [
            ("left-stick left", usb_report(4, 128, 128, 128, 0x08)),
            ("left-stick right", usb_report(252, 128, 128, 128, 0x08)),
            ("right-stick left", usb_report(128, 128, 4, 128, 0x08)),
            ("right-stick right", usb_report(128, 128, 252, 128, 0x08)),
            ("dpad left", usb_report(128, 128, 128, 128, 0x06)),
        ];
        for (name, sample) in cases {
            assert!(
                sample.stick_x != 0.0 || sample.dpad_left,
                "{name} must deflect"
            );
            let mut stepper = NavStepper::default();
            let t0 = Instant::now();
            let mut fires = 0;
            for ms in 0..100 {
                if stepper
                    .update(&sample, t0 + Duration::from_millis(ms * 10), true, true)
                    .is_some()
                {
                    fires += 1;
                }
            }
            assert_eq!(fires, 9, "{name} solid hold must stay gated");
        }
    }

    /// Sticks split at parse: left-only drives the legacy combined vector,
    /// each stick is readable alone, and bumpers arrive as bools.
    #[test]
    fn parse_report_splits_sticks_and_bumpers() {
        fn usb_report(lx: u8, ly: u8, rx: u8, ry: u8, buttons1: u8) -> PadSample {
            let mut buf = [0u8; 64];
            buf[0] = 0x01;
            buf[1] = lx;
            buf[2] = ly;
            buf[3] = rx;
            buf[4] = ry;
            buf[8] = 0x08; // dpad neutral
            buf[9] = buttons1;
            parse_report(&buf, 0)
        }
        // Left full-left: legacy vector follows, right stays neutral.
        let left = usb_report(4, 128, 128, 128, 0x00);
        assert!(left.stick_x < 0.0);
        assert!(left.lstick_x < 0.0);
        assert_eq!(left.rstick_x, 0.0);
        assert_eq!(left.rstick_y, 0.0);
        // Right full-down: legacy vector follows, left stays neutral.
        let right = usb_report(128, 128, 128, 252, 0x00);
        assert!(right.stick_y > 0.0);
        assert_eq!(right.lstick_x, 0.0);
        assert_eq!(right.lstick_y, 0.0);
        assert!(right.rstick_y > 0.0);
        // Bumpers land on their own fields (L1 = bit0, R1 = bit1).
        let bumpers = usb_report(128, 128, 128, 128, 0x03);
        assert!(bumpers.l1 && bumpers.r1);
        let none = usb_report(128, 128, 128, 128, 0x00);
        assert!(!none.l1 && !none.r1);
        assert!(sample_nav_resting(&none));
        // A bumper alone breaks rest (arming waits for full release).
        let mut l1_only = none.clone();
        l1_only.l1 = true;
        assert!(!sample_nav_resting(&l1_only));
    }

    /// L1/R1 taps fire on release (hold-vs-tap disambiguation for the
    /// history selector), never repeat while held, and fire again after a
    /// release — without disturbing the face-button path.
    #[test]
    fn bumper_hold_opens_selector_instead_of_tapping() {
        let mut hold = BumperHold::default();
        let t0 = Instant::now();
        // Fresh press: silent.
        assert!(hold.update(true, false, t0).is_none());
        // Still held just under the threshold: silent.
        assert!(
            hold.update(true, false, t0 + BUMPER_HOLD - Duration::from_millis(50))
                .is_none()
        );
        // Crossing the threshold: opens once.
        assert_eq!(
            hold.update(true, false, t0 + BUMPER_HOLD),
            Some(BumperHoldEvent::Open(BumperSide::Left))
        );
        // Still held: silent (fires once).
        assert!(
            hold.update(true, false, t0 + BUMPER_HOLD + Duration::from_millis(500))
                .is_none()
        );
        // Release after open: no tap.
        assert!(
            hold.update(false, false, t0 + BUMPER_HOLD + Duration::from_millis(600))
                .is_none()
        );
    }

    #[test]
    fn bumper_hold_side_switch_cancels_silently() {
        let mut hold = BumperHold::default();
        let t0 = Instant::now();
        assert!(hold.update(true, false, t0).is_none());
        // L1 → R1 without a release: old press cancelled, no tap, new clock.
        assert!(
            hold.update(false, true, t0 + Duration::from_millis(100))
                .is_none()
        );
        assert_eq!(hold.tracking(), Some(BumperSide::Right));
        assert!(!hold.is_open());
        // Quick release: taps the NEW side only.
        assert_eq!(
            hold.update(false, false, t0 + Duration::from_millis(200)),
            Some(BumperHoldEvent::Tap(BumperSide::Right))
        );
    }

    #[test]
    fn bumper_hold_both_held_keeps_clock() {
        let mut hold = BumperHold::default();
        let t0 = Instant::now();
        assert!(hold.update(true, false, t0).is_none());
        // Grazing second finger: clock preserved, still opens on time.
        assert!(
            hold.update(true, true, t0 + BUMPER_HOLD - Duration::from_millis(10))
                .is_none()
        );
        assert_eq!(
            hold.update(true, true, t0 + BUMPER_HOLD),
            Some(BumperHoldEvent::Open(BumperSide::Left))
        );
        // Fresh both-press with no side tracked: ignored.
        let mut fresh = BumperHold::default();
        assert!(fresh.update(true, true, t0).is_none());
        assert_eq!(fresh.tracking(), None);
    }

    /// Deliberate release (neutral past the dropout grace) then re-press must
    /// still fire immediately, and the opposite direction must fire at once
    /// even after only a brief neutral gap.
    #[test]
    fn deliberate_release_then_repress_fires_immediately() {
        let mut stepper = NavStepper::default();
        let t0 = Instant::now();
        let left = PadSample {
            stick_x: -0.95,
            ..Default::default()
        };
        let right = PadSample {
            stick_x: 0.95,
            ..Default::default()
        };
        assert_eq!(
            stepper.update(&left, t0, true, false),
            Some(NavAction::Left)
        );
        // Brief neutral then opposite: direction change always fires at once.
        assert!(
            stepper
                .update(
                    &PadSample::default(),
                    t0 + Duration::from_millis(30),
                    true,
                    false
                )
                .is_none()
        );
        assert_eq!(
            stepper.update(&right, t0 + Duration::from_millis(30), true, false),
            Some(NavAction::Right)
        );
        // Same-direction re-press inside the grace stays gated (dropout, not
        // release): pre-fix the brief neutral above reset state and this fired.
        assert!(
            stepper
                .update(
                    &PadSample::default(),
                    t0 + Duration::from_millis(100),
                    true,
                    false
                )
                .is_none()
        );
        assert!(
            stepper
                .update(&right, t0 + Duration::from_millis(110), true, false)
                .is_none()
        );
        // Long neutral: past the dropout grace -> state resets.
        assert!(
            stepper
                .update(
                    &PadSample::default(),
                    t0 + Duration::from_millis(300),
                    true,
                    false
                )
                .is_none()
        );
        // Neutral held past the grace is a deliberate release: reset, so the
        // same-direction re-press below fires via the fresh path.
        assert!(
            stepper
                .update(
                    &PadSample::default(),
                    t0 + Duration::from_millis(450),
                    true,
                    false
                )
                .is_none()
        );
        // Fresh press fires at once.
        assert_eq!(
            stepper.update(&left, t0 + Duration::from_millis(460), true, false),
            Some(NavAction::Left)
        );
    }

    #[test]
    fn menu_profile_shapes_raw_dq_stick_and_ignores_light_touch() {
        let profile = NavProfile::MENU;
        assert_eq!(profile.shape_stick(0.65, 0.0), (0.0, 0.0));
        let (x, y) = profile.shape_stick(-0.95, 0.0);
        assert!(x < 0.0 && y == 0.0);
        // Residual combined deadzone still kills tiny vector sums.
        assert_eq!(profile.shape_stick(0.1, 0.1), (0.0, 0.0));
        // D-pad profile is digital: no stick shaping.
        assert_eq!(NavProfile::DPAD.shape_stick(0.65, 0.0), (0.65, 0.0));
    }

    #[test]
    fn nav_step_reports_first_repeat_and_release() {
        let mut stepper = NavStepper::with_profile(NavProfile::MENU);
        let t0 = Instant::now();
        let left = PadSample {
            stick_x: -0.95,
            ..Default::default()
        };
        assert_eq!(
            stepper.step(&left, t0, true, true),
            Some(NavStep {
                direction: NavAction::Left,
                phase: NavPhase::First
            })
        );
        assert!(
            stepper
                .step(&left, t0 + Duration::from_millis(100), true, true)
                .is_none()
        );
        assert_eq!(
            stepper.step(&left, t0 + Duration::from_millis(280), true, true),
            Some(NavStep {
                direction: NavAction::Left,
                phase: NavPhase::Repeat
            })
        );
        // Wrapper still hides the release step from legacy call sites.
        assert!(
            stepper
                .update(
                    &PadSample::default(),
                    t0 + Duration::from_millis(400),
                    true,
                    true
                )
                .is_none()
        );
        assert_eq!(
            stepper.step(
                &PadSample::default(),
                t0 + Duration::from_millis(520),
                true,
                true
            ),
            Some(NavStep {
                direction: NavAction::Left,
                phase: NavPhase::Release
            })
        );
    }

    #[test]
    fn nav_profile_axis_lock_keeps_active_horizontal_on_tie() {
        let mut stepper = NavStepper::with_profile(NavProfile::MENU);
        let t0 = Instant::now();
        let left = PadSample {
            stick_x: -0.5,
            ..Default::default()
        };
        assert_eq!(stepper.update(&left, t0, true, true), Some(NavAction::Left));
        // Equal-magnitude diagonal keeps the horizontal axis while locked:
        // no direction change, and the repeat is still gated until +280ms.
        let tie = PadSample {
            stick_x: -0.5,
            stick_y: 0.5,
            ..Default::default()
        };
        assert!(
            stepper
                .update(&tie, t0 + Duration::from_millis(100), true, true)
                .is_none()
        );
        assert_eq!(
            stepper.step(&tie, t0 + Duration::from_millis(280), true, true),
            Some(NavStep {
                direction: NavAction::Left,
                phase: NavPhase::Repeat
            })
        );
        let mut unlocked = NavStepper::with_profile(NavProfile {
            axis_lock: false,
            ..NavProfile::MENU
        });
        assert_eq!(
            unlocked.update(&tie, t0, true, true),
            Some(NavAction::Down),
            "vertical wins ties without the lock"
        );
        assert_eq!(
            unlocked.update(&tie, t0 + Duration::from_millis(100), true, true),
            None
        );
    }

    #[test]
    fn dual_nav_requires_rest_before_arming_and_moves_independent_halves() {
        let mut nav = DualNav::new(NavProfile::MENU);
        nav.prepare_on_open(false);
        let t0 = Instant::now();
        let mut held = PadSample {
            lstick_x: -0.95,
            ..Default::default()
        };
        held.stick_x = held.lstick_x;
        let tick = nav.tick(&[reading("a", held.clone())], t0);
        assert!(tick.steps.is_empty(), "held pad must not move before rest");
        assert_eq!(tick.unarmed_pads, 1);
        let tick = nav.tick(&[reading("a", PadSample::default())], t0);
        assert_eq!(tick.armed_pads, 1);
        let tick = nav.tick(&[reading("a", held)], t0 + Duration::from_millis(10));
        assert_eq!(tick.steps.len(), 1);
        assert_eq!(tick.steps[0].half, NavHalf::Left);
        assert_eq!(tick.steps[0].step.direction, NavAction::Left);

        let mut right = PadSample {
            rstick_y: 0.95,
            ..Default::default()
        };
        right.stick_y = right.rstick_y;
        let tick = nav.tick(&[reading("a", right)], t0 + Duration::from_millis(20));
        assert_eq!(tick.steps.len(), 1);
        assert_eq!(tick.steps[0].half, NavHalf::Right);
    }

    #[test]
    fn dual_nav_first_step_wins_per_half_per_tick() {
        let mut nav = DualNav::new(NavProfile::MENU);
        nav.prepare_on_open(true);
        let t0 = Instant::now();
        let mut first = PadSample {
            lstick_x: -0.95,
            ..Default::default()
        };
        first.stick_x = first.lstick_x;
        let mut second = PadSample {
            rstick_y: 0.95,
            ..Default::default()
        };
        second.stick_y = second.rstick_y;
        let tick = nav.tick(&[reading("a", first), reading("b", second)], t0);
        assert_eq!(tick.steps.len(), 2);
        assert_eq!(tick.steps[0].pad.as_str(), "a");
        assert_eq!(tick.steps[0].half, NavHalf::Left);
        assert_eq!(tick.steps[1].pad.as_str(), "b");
        assert_eq!(tick.steps[1].half, NavHalf::Right);
        // The first pad keeps owning repeat steps for its half.
        let tick = nav.tick(
            &[
                reading(
                    "a",
                    PadSample {
                        lstick_x: -0.95,
                        stick_x: -0.95,
                        ..Default::default()
                    },
                ),
                reading(
                    "b",
                    PadSample {
                        lstick_y: 0.95,
                        stick_y: 0.95,
                        ..Default::default()
                    },
                ),
            ],
            t0 + Duration::from_millis(280),
        );
        assert_eq!(tick.steps.len(), 1);
        assert_eq!(tick.steps[0].pad.as_str(), "a");
    }

    #[test]
    fn dual_nav_triggers_are_latched_per_pad() {
        let mut nav = DualNav::new(NavProfile::MENU);
        nav.prepare_on_open(true);
        let t0 = Instant::now();
        let a_held = PadSample {
            l2: true,
            ..Default::default()
        };
        let tick = nav.tick(&[reading("a", a_held.clone())], t0);
        assert_eq!(tick.triggers.len(), 1);
        assert_eq!(tick.triggers[0].half, NavHalf::Left);
        // Holding A's L2 must not mask a fresh B press.
        let tick = nav.tick(
            &[
                reading("a", a_held),
                reading(
                    "b",
                    PadSample {
                        l2: true,
                        ..Default::default()
                    },
                ),
            ],
            t0 + Duration::from_millis(10),
        );
        assert_eq!(tick.triggers.len(), 1);
        assert_eq!(tick.triggers[0].pad.as_str(), "b");
        // Holding both still fires once per pad.
        let tick = nav.tick(
            &[
                reading(
                    "a",
                    PadSample {
                        l2: true,
                        ..Default::default()
                    },
                ),
                reading(
                    "b",
                    PadSample {
                        r2: true,
                        ..Default::default()
                    },
                ),
            ],
            t0 + Duration::from_millis(20),
        );
        assert_eq!(tick.triggers.len(), 1);
        assert_eq!(tick.triggers[0].half, NavHalf::Right);
    }
}
