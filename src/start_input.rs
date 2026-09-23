//! Start-screen navigation via DualSense HID (hid-worker snapshot) and reopen gestures.
//!
//! PadPoll never opens HID on the UI thread — it clones the latest samples published
//! by [`crate::hid_worker`]. The worker interleaves Identify lightbar writes with
//! short input reads on the same handle so nav stays live during flashes.

use crate::gesture::{GestureControl, GestureDetector};
use hidapi::HidDevice;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Per-axis deadzone applied to each stick before combining (kills rest bias).
const STICK_AXIS_DEADZONE: f32 = 0.9;
/// Residual deadzone on the combined vector after per-stick cleaning.
const STICK_COMBINED_DEADZONE: f32 = 0.15;
const TRIGGER_ANALOG_THRESHOLD: u8 = 30;
const STICK_CENTER: f32 = 128.0;
const NAV_INITIAL_DELAY: Duration = Duration::from_millis(280);
const NAV_REPEAT: Duration = Duration::from_millis(90);
/// One wired DualSense report interval (`bInterval` 4). Block this long when the queue is empty.
pub const INPUT_REPORT_WAIT_MS: i32 = 4;
/// Cap on non-blocking drains per wake (32 = default Windows HID buffer depth).
const INPUT_DRAIN_CAP: usize = 32;

const USB_REPORT_SIZE: usize = 64;
const BT_REPORT_SIZE: usize = 78;
const BT_REPORT_TRUNCATED: u8 = 0x01;
const BT_REPORT_FULL: u8 = 0x31;
const USB_REPORT_ID: u8 = 0x01;
const CALIBRATION_FEATURE_REPORT: u8 = 0x05;
const CALIBRATION_FEATURE_SIZE: usize = 41;

static INPUT_SNAPSHOT: OnceLock<Arc<Mutex<InputSnapshot>>> = OnceLock::new();

/// Why the worker last published (or cleared) the input snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavAction {
    Up,
    Down,
    Confirm,
    Cancel,
    PrevSlide,
    NextSlide,
    ToggleEdit,
    CycleSort,
    /// Triangle edge — edit selected manual while in edit mode.
    Triangle,
}

#[derive(Debug, Clone, Default)]
pub struct PadSample {
    pub held: BTreeSet<GestureControl>,
    /// Combined stick Y after deadzone; negative = up. Non-zero means past deadzone.
    pub stick_y: f32,
    pub dpad_up: bool,
    pub dpad_down: bool,
    pub cross: bool,
    pub circle: bool,
    pub square: bool,
    pub triangle: bool,
    pub options: bool,
    pub l2: bool,
    pub r2: bool,
}

/// Which backend produced a start-screen nav sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavSource {
    Hid,
}

impl NavSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hid => "hid",
        }
    }
}

/// Stable pad identity for per-controller edge / hold / gesture state.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PadId(pub String);

impl PadId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct NavReading {
    pub id: PadId,
    pub sample: PadSample,
    pub source: NavSource,
}

/// Coarse stick band for edge-triggered diagnostics (not every poll).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StickBand {
    #[default]
    Neutral,
    Up,
    Down,
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

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Neutral => "neutral",
            Self::Up => "up",
            Self::Down => "down",
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
            l2: sample.l2,
            r2: sample.r2,
            stick: StickBand::from_stick_y(sample.stick_y),
            resting: sample_nav_resting(sample),
        }
    }

    pub fn format_line(self, source: NavSource) -> String {
        format!(
            "start-nav: src={} cross={} circle={} l2={} r2={} dpad_up={} dpad_down={} stick={} resting={}",
            source.as_str(),
            u8::from(self.cross),
            u8::from(self.circle),
            u8::from(self.l2),
            u8::from(self.r2),
            u8::from(self.dpad_up),
            u8::from(self.dpad_down),
            self.stick.as_str(),
            u8::from(self.resting),
        )
    }
}

/// Shared nav stepper for D-pad + sticks (prevents double-fire).
#[derive(Debug, Clone, Default)]
pub struct NavStepper {
    direction: i8,
    next_fire: Option<Instant>,
}

impl NavStepper {
    pub fn update(&mut self, sample: &PadSample, now: Instant) -> Option<NavAction> {
        let stick_up = sample.stick_y < 0.0;
        let stick_down = sample.stick_y > 0.0;
        let want_up = sample.dpad_up || stick_up;
        let want_down = sample.dpad_down || stick_down;

        let direction = if want_up && !want_down {
            -1
        } else if want_down && !want_up {
            1
        } else {
            0
        };

        if direction == 0 {
            self.direction = 0;
            self.next_fire = None;
            return None;
        }

        if self.direction != direction {
            self.direction = direction;
            self.next_fire = Some(now + NAV_INITIAL_DELAY);
            return Some(if direction < 0 {
                NavAction::Up
            } else {
                NavAction::Down
            });
        }

        if self.next_fire.is_some_and(|t| now >= t) {
            self.next_fire = Some(now + NAV_REPEAT);
            return Some(if direction < 0 {
                NavAction::Up
            } else {
                NavAction::Down
            });
        }
        None
    }

    pub fn reset(&mut self) {
        self.direction = 0;
        self.next_fire = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EdgeButton {
    Cross,
    Circle,
    Square,
    Triangle,
    Options,
    L2,
    R2,
}

impl EdgeButton {
    const ALL: [Self; 7] = [
        Self::Cross,
        Self::Circle,
        Self::Square,
        Self::Triangle,
        Self::Options,
        Self::L2,
        Self::R2,
    ];

    /// Face + Options: activate on release. L2/R2: activate on press.
    const RELEASE_FIRE: [Self; 5] = [
        Self::Cross,
        Self::Circle,
        Self::Square,
        Self::Triangle,
        Self::Options,
    ];

    const PRESS_FIRE: [Self; 2] = [Self::L2, Self::R2];

    fn action(self) -> Option<NavAction> {
        match self {
            Self::Cross => Some(NavAction::Confirm),
            Self::Circle => Some(NavAction::Cancel),
            // Browse: sort. Edit: remapped to EditManual in PadNavBank.
            Self::Square => Some(NavAction::CycleSort),
            // Browse: enter edit. Edit: save (ToggleEdit commits).
            Self::Triangle => Some(NavAction::ToggleEdit),
            // Options no longer drives start-screen actions (Square sorts).
            Self::Options => None,
            Self::L2 => Some(NavAction::PrevSlide),
            Self::R2 => Some(NavAction::NextSlide),
        }
    }

    fn held(self, sample: &PadSample) -> bool {
        match self {
            Self::Cross => sample.cross,
            Self::Circle => sample.circle,
            Self::Square => sample.square,
            Self::Triangle => sample.triangle,
            Self::Options => sample.options,
            Self::L2 => sample.l2,
            Self::R2 => sample.r2,
        }
    }
}

/// Face/Options fire on release; L2/R2 on press. `hold_owned` never fires here.
#[derive(Debug, Clone, Default)]
pub struct ButtonEdges {
    cross: bool,
    circle: bool,
    square: bool,
    triangle: bool,
    options: bool,
    l2: bool,
    r2: bool,
    /// Face/Options that went down but were cancelled by a foreign face press.
    cancel_cross: bool,
    cancel_circle: bool,
    cancel_square: bool,
    cancel_triangle: bool,
    cancel_options: bool,
}

impl ButtonEdges {
    fn prev_held(&self, button: EdgeButton) -> bool {
        match button {
            EdgeButton::Cross => self.cross,
            EdgeButton::Circle => self.circle,
            EdgeButton::Square => self.square,
            EdgeButton::Triangle => self.triangle,
            EdgeButton::Options => self.options,
            EdgeButton::L2 => self.l2,
            EdgeButton::R2 => self.r2,
        }
    }

    fn cancelled(&self, button: EdgeButton) -> bool {
        match button {
            EdgeButton::Cross => self.cancel_cross,
            EdgeButton::Circle => self.cancel_circle,
            EdgeButton::Square => self.cancel_square,
            EdgeButton::Triangle => self.cancel_triangle,
            EdgeButton::Options => self.cancel_options,
            EdgeButton::L2 | EdgeButton::R2 => false,
        }
    }

    fn set_cancelled(&mut self, button: EdgeButton, value: bool) {
        match button {
            EdgeButton::Cross => self.cancel_cross = value,
            EdgeButton::Circle => self.cancel_circle = value,
            EdgeButton::Square => self.cancel_square = value,
            EdgeButton::Triangle => self.cancel_triangle = value,
            EdgeButton::Options => self.cancel_options = value,
            EdgeButton::L2 | EdgeButton::R2 => {}
        }
    }

    /// True when any control other than `except` just went down.
    pub fn foreign_press(&self, sample: &PadSample, except: Option<EdgeButton>) -> bool {
        EdgeButton::ALL
            .iter()
            .any(|&b| Some(b) != except && b.held(sample) && !self.prev_held(b))
    }

    /// L2/R2: rising edge. Face/Options: falling edge unless cancelled or `hold_owned`.
    pub fn update(
        &mut self,
        sample: &PadSample,
        hold_owned: Option<EdgeButton>,
    ) -> Option<NavAction> {
        // Foreign face/Options press cancels other held face/Options pending releases.
        for &b in &EdgeButton::RELEASE_FIRE {
            let now = b.held(sample);
            let was = self.prev_held(b);
            if now && !was {
                for &other in &EdgeButton::RELEASE_FIRE {
                    if other != b && self.prev_held(other) {
                        self.set_cancelled(other, true);
                    }
                }
            }
        }

        let mut action = None;
        for &b in &EdgeButton::PRESS_FIRE {
            let now = b.held(sample);
            let was = self.prev_held(b);
            if now && !was && action.is_none() {
                action = b.action();
            }
        }
        for &b in &EdgeButton::RELEASE_FIRE {
            let now = b.held(sample);
            let was = self.prev_held(b);
            if was && !now {
                let cancelled = self.cancelled(b);
                self.set_cancelled(b, false);
                if !cancelled && Some(b) != hold_owned && action.is_none() {
                    action = b.action();
                }
            }
        }

        self.sync(sample);
        action
    }

    /// Track held buttons without firing (used while nav is disarmed / animating).
    pub fn sync(&mut self, sample: &PadSample) {
        self.cross = sample.cross;
        self.circle = sample.circle;
        self.square = sample.square;
        self.triangle = sample.triangle;
        self.options = sample.options;
        self.l2 = sample.l2;
        self.r2 = sample.r2;
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Mark every currently held face/Options so its pending release will not fire.
    pub fn cancel_held_releases(&mut self) {
        for &b in &EdgeButton::RELEASE_FIRE {
            if self.prev_held(b) {
                self.set_cancelled(b, true);
            }
        }
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
            return (0.0, false);
        }

        let dt = self
            .last_tick
            .map(|t| now.saturating_duration_since(t).as_secs_f32())
            .unwrap_or(0.0);
        self.last_tick = Some(now);

        if held {
            self.holding = true;
            self.charge = (self.charge + dt / BUTTON_HOLD.as_secs_f32()).min(1.0);
            return (hold_ease_out(self.charge), false);
        }

        self.holding = false;
        if self.charge >= 1.0 {
            self.charge = 0.0;
            self.last_tick = None;
            return (0.0, true);
        }

        if self.charge <= 0.0 {
            self.charge = 0.0;
            self.last_tick = None;
            return (0.0, false);
        }

        let mut progress = hold_ease_out(self.charge);
        progress = (progress - dt / BUTTON_DECAY.as_secs_f32()).max(0.0);
        if progress <= 0.0 {
            self.charge = 0.0;
            self.last_tick = None;
            return (0.0, false);
        }
        self.charge = hold_ease_out_inv(progress);
        (progress, false)
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
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Triangle / Cross hold trackers share the same timing.
pub type TriangleHold = HoldTracker;
pub type CrossHold = HoldTracker;

/// True when face/system buttons, triggers, D-pad, and stick are at rest (safe to arm start nav).
pub fn sample_nav_resting(sample: &PadSample) -> bool {
    sample.held.is_empty()
        && !sample.cross
        && !sample.circle
        && !sample.square
        && !sample.triangle
        && !sample.options
        && !sample.l2
        && !sample.r2
        && !sample.dpad_up
        && !sample.dpad_down
        && sample.stick_y == 0.0
}

/// Pick a single reading for debug UI: prefer a non-resting pad, else the first.
pub fn preferred_reading(readings: &[NavReading]) -> Option<&NavReading> {
    readings
        .iter()
        .find(|r| !sample_nav_resting(&r.sample))
        .or_else(|| readings.first())
}

/// Install the hid-worker input snapshot (called once at worker start).
pub fn set_input_snapshot(snapshot: Arc<Mutex<InputSnapshot>>) {
    let _ = INPUT_SNAPSHOT.set(snapshot);
}

/// PadPoll entry: O(1) clone of hid-worker snapshot (never opens HID on the UI thread).
pub fn read_nav_readings() -> NavReadingsOutcome {
    let Some(slot) = INPUT_SNAPSHOT.get() else {
        return NavReadingsOutcome::Missing {
            meta: SnapshotMeta {
                published_at: Instant::now(),
                seq: 0,
                reason: SnapshotReason::NeverPublished,
                pad_count: 0,
            },
        };
    };
    let Ok(guard) = slot.lock() else {
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
        log_source_once(NavSource::Hid, guard.readings.len());
        NavReadingsOutcome::Readings {
            readings: guard.readings.clone(),
            meta,
        }
    }
}

fn log_source_once(source: NavSource, pads: usize) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        crate::app_log::info(format!(
            "start-nav: backend={} pads={pads} (hid-worker snapshot)",
            source.as_str(),
        ));
    }
}

/// Result of a multi-pad start-screen nav poll.
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
    Ok(PadSample),
    /// No new report this wake — keep the open handle and the previous snapshot reading.
    Timeout,
    /// Hard failure — drop the handle (I/O / bad report / feature).
    #[allow(dead_code)] // matched by hid_worker; field read for logging there
    Fail(SampleFail),
}

fn sample_summary(sample: &PadSample) -> String {
    format!(
        "ok stick_y={:.2} l2={} r2={} cross={} circle={} square={} triangle={} options={} dpad_u={} dpad_d={}",
        sample.stick_y,
        u8::from(sample.l2),
        u8::from(sample.r2),
        u8::from(sample.cross),
        u8::from(sample.circle),
        u8::from(sample.square),
        u8::from(sample.triangle),
        u8::from(sample.options),
        u8::from(sample.dpad_up),
        u8::from(sample.dpad_down),
    )
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
        out.l2 |= s.l2;
        out.r2 |= s.r2;
        out.dpad_up |= s.dpad_up;
        out.dpad_down |= s.dpad_down;
        out.held = out.held.union(&s.held).copied().collect();
    }
    out.stick_y = last.stick_y;
    Some(out)
}

fn latch_diag(batch: &[PadSample], merged: &PadSample) {
    let Some(last) = batch.last() else {
        return;
    };
    let latched = (merged.cross && !last.cross)
        || (merged.circle && !last.circle)
        || (merged.square && !last.square)
        || (merged.triangle && !last.triangle)
        || (merged.options && !last.options)
        || (merged.l2 && !last.l2)
        || (merged.r2 && !last.r2);
    if latched {
        crate::hid_diag::diag_info(format!(
            "hid-diag: drain latched buttons n={} cross={} circle={} square={} triangle={}",
            batch.len(),
            u8::from(merged.cross && !last.cross),
            u8::from(merged.circle && !last.circle),
            u8::from(merged.square && !last.square),
            u8::from(merged.triangle && !last.triangle),
        ));
    }
}

/// One DualSense report read (optional block). Used by the drain loop.
#[allow(clippy::too_many_arguments)]
fn read_one_report(
    device: &HidDevice,
    is_bluetooth: bool,
    serial: &str,
    bus: &str,
    report_size: usize,
    timeout_ms: i32,
    requested_full: &mut bool,
    started: Instant,
) -> Result<Option<PadSample>, SampleFail> {
    let mut buf = vec![0u8; report_size];
    let n = {
        let _op = crate::hid_diag::enter_op("read_timeout");
        match device.read_timeout(&mut buf, timeout_ms) {
            Ok(n) => n,
            Err(err) => {
                drop(_op);
                let fail = if err.to_string().to_lowercase().contains("timeout") {
                    SampleFail::Timeout
                } else {
                    SampleFail::Io
                };
                if fail == SampleFail::Timeout && timeout_ms == 0 {
                    return Ok(None);
                }
                if fail == SampleFail::Timeout {
                    crate::hid_diag::diag_info(format!(
                        "hid-diag: sample timeout keep_handle serial={serial} bus={bus}"
                    ));
                    return Err(SampleFail::Timeout);
                }
                crate::hid_diag::trace_read(
                    "sample",
                    serial,
                    bus,
                    started.elapsed().as_millis(),
                    &format!("fail={} err={err}", fail.as_str()),
                    false,
                );
                return Err(fail);
            }
        }
    };
    if n == 0 {
        return Ok(None);
    }

    if is_bluetooth && buf[0] == BT_REPORT_TRUNCATED {
        if !*requested_full {
            let mut feature = vec![0u8; CALIBRATION_FEATURE_SIZE];
            feature[0] = CALIBRATION_FEATURE_REPORT;
            let feature_result = {
                let _op = crate::hid_diag::enter_op("get_feature_report");
                device.get_feature_report(&mut feature)
            };
            match feature_result {
                Ok(_) => {
                    *requested_full = true;
                    return Ok(None);
                }
                Err(err) => {
                    crate::hid_diag::trace_read(
                        "sample",
                        serial,
                        bus,
                        started.elapsed().as_millis(),
                        &format!("fail=feature err={err}"),
                        false,
                    );
                    return Err(SampleFail::Feature);
                }
            }
        }
        crate::hid_diag::trace_read(
            "sample",
            serial,
            bus,
            started.elapsed().as_millis(),
            "fail=truncated",
            false,
        );
        return Err(SampleFail::Truncated);
    }

    let expected = if is_bluetooth {
        BT_REPORT_FULL
    } else {
        USB_REPORT_ID
    };
    if buf[0] != expected {
        crate::hid_diag::trace_read(
            "sample",
            serial,
            bus,
            started.elapsed().as_millis(),
            &format!("fail=bad_report id=0x{:02x} n={n}", buf[0]),
            false,
        );
        return Err(SampleFail::BadReport);
    }

    let base = if is_bluetooth { 1 } else { 0 };
    Ok(Some(parse_report(&buf, base)))
}

/// Drain queued DualSense reports, wait one report interval, then drain again.
///
/// Newest sticks win; digital buttons are OR'd across the batch so a tap that
/// released inside the drain still appears held for one snapshot. The blocking
/// wait runs even after a non-empty zero-timeout drain so the hot path paces to
/// the pad's report clock instead of busy-spinning.
///
/// Never call from the UI thread. Truncated BT: one feature request (one-time) + retry.
pub fn read_device_sample_short(
    device: &HidDevice,
    is_bluetooth: bool,
    serial: &str,
    input_hot: bool,
) -> ShortSampleOutcome {
    let bus = if is_bluetooth { "bt" } else { "usb" };
    let report_size = if is_bluetooth {
        BT_REPORT_SIZE
    } else {
        USB_REPORT_SIZE
    };
    let mut requested_full = false;
    let started = Instant::now();
    let mut batch: Vec<PadSample> = Vec::new();

    let drain_queued =
        |batch: &mut Vec<PadSample>, requested_full: &mut bool| -> Result<(), SampleFail> {
            for _ in 0..INPUT_DRAIN_CAP {
                match read_one_report(
                    device,
                    is_bluetooth,
                    serial,
                    bus,
                    report_size,
                    0,
                    requested_full,
                    started,
                )? {
                    Some(sample) => batch.push(sample),
                    None => break,
                }
            }
            Ok(())
        };

    // Drain anything already queued (immediate).
    if let Err(fail) = drain_queued(&mut batch, &mut requested_full) {
        return match fail {
            SampleFail::Timeout => ShortSampleOutcome::Timeout,
            other => ShortSampleOutcome::Fail(other),
        };
    }

    // Always wait one report interval so a non-empty drain cannot busy-spin the
    // worker: pace to the pad's report clock, and merge any report that arrives.
    match read_one_report(
        device,
        is_bluetooth,
        serial,
        bus,
        report_size,
        INPUT_REPORT_WAIT_MS,
        &mut requested_full,
        started,
    ) {
        Ok(Some(sample)) => batch.push(sample),
        Ok(None) | Err(SampleFail::Timeout) => {
            if batch.is_empty() {
                return ShortSampleOutcome::Timeout;
            }
        }
        Err(other) => return ShortSampleOutcome::Fail(other),
    }
    if let Err(fail) = drain_queued(&mut batch, &mut requested_full) {
        return match fail {
            SampleFail::Timeout => ShortSampleOutcome::Timeout,
            other => ShortSampleOutcome::Fail(other),
        };
    }

    let Some(merged) = merge_nav_samples(&batch) else {
        return ShortSampleOutcome::Timeout;
    };
    latch_diag(&batch, &merged);

    let _ = input_hot;
    crate::hid_diag::trace_read(
        "sample",
        serial,
        bus,
        started.elapsed().as_millis(),
        &format!("{} drain_n={}", sample_summary(&merged), batch.len()),
        true,
    );
    ShortSampleOutcome::Ok(merged)
}

/// Build a nav reading from a HID sample + device identity.
pub fn hid_nav_reading(identity: &str, sample: PadSample) -> NavReading {
    NavReading {
        id: PadId(format!("hid:{identity}")),
        sample,
        source: NavSource::Hid,
    }
}

/// Parse sticks/buttons from a DualSense input report.
///
/// `base` indexes the report-ID byte. Common payload starts at `base + 1`.
/// USB uses `base = 0`; BT `0x31` uses `base = 1`.
pub fn parse_report(buf: &[u8], base: usize) -> PadSample {
    let lx = axis_u8(buf.get(base + 1).copied().unwrap_or(128));
    let ly = axis_u8(buf.get(base + 2).copied().unwrap_or(128));
    let rx = axis_u8(buf.get(base + 3).copied().unwrap_or(128));
    let ry = axis_u8(buf.get(base + 4).copied().unwrap_or(128));
    let l2_analog = buf.get(base + 5).copied().unwrap_or(0);
    let r2_analog = buf.get(base + 6).copied().unwrap_or(0);
    let buttons0 = buf.get(base + 8).copied().unwrap_or(0);
    let buttons1 = buf.get(base + 9).copied().unwrap_or(0);
    let buttons2 = buf.get(base + 10).copied().unwrap_or(0);

    let (_dx, dy) = combined_stick(lx, ly, rx, ry);

    let dpad = buttons0 & 0x0F;
    let (dpad_up, dpad_down, dpad_left, dpad_right) = match dpad {
        0 => (true, false, false, false),
        1 => (true, false, false, true),
        2 => (false, false, false, true),
        3 => (false, true, false, true),
        4 => (false, true, false, false),
        5 => (false, true, true, false),
        6 => (false, false, true, false),
        7 => (true, false, true, false),
        8..=15 => (false, false, false, false),
        _ => (false, false, false, false),
    };

    let square = buttons0 & (1 << 4) != 0;
    let cross = buttons0 & (1 << 5) != 0;
    let circle = buttons0 & (1 << 6) != 0;
    let triangle = buttons0 & (1 << 7) != 0;

    let l1 = buttons1 & (1 << 0) != 0;
    let r1 = buttons1 & (1 << 1) != 0;
    let l2_digital = buttons1 & (1 << 2) != 0;
    let r2_digital = buttons1 & (1 << 3) != 0;
    let create = buttons1 & (1 << 4) != 0;
    let options = buttons1 & (1 << 5) != 0;
    let l3 = buttons1 & (1 << 6) != 0;
    let r3 = buttons1 & (1 << 7) != 0;

    let ps = buttons2 & (1 << 0) != 0;
    let touchpad = buttons2 & (1 << 1) != 0;
    let mute = buttons2 & (1 << 2) != 0;

    let l2 = l2_digital || l2_analog >= TRIGGER_ANALOG_THRESHOLD;
    let r2 = r2_digital || r2_analog >= TRIGGER_ANALOG_THRESHOLD;

    let mut held = BTreeSet::new();
    if cross {
        held.insert(GestureControl::Cross);
    }
    if circle {
        held.insert(GestureControl::Circle);
    }
    if square {
        held.insert(GestureControl::Square);
    }
    if triangle {
        held.insert(GestureControl::Triangle);
    }
    if l1 {
        held.insert(GestureControl::L1);
    }
    if r1 {
        held.insert(GestureControl::R1);
    }
    if l2 {
        held.insert(GestureControl::L2);
    }
    if r2 {
        held.insert(GestureControl::R2);
    }
    if l3 {
        held.insert(GestureControl::L3);
    }
    if r3 {
        held.insert(GestureControl::R3);
    }
    if create {
        held.insert(GestureControl::Create);
    }
    if options {
        held.insert(GestureControl::Options);
    }
    if ps {
        held.insert(GestureControl::Ps);
    }
    if mute {
        held.insert(GestureControl::Mute);
    }
    if touchpad {
        held.insert(GestureControl::Touchpad);
    }
    if dpad_up {
        held.insert(GestureControl::DpadUp);
    }
    if dpad_down {
        held.insert(GestureControl::DpadDown);
    }
    if dpad_left {
        held.insert(GestureControl::DpadLeft);
    }
    if dpad_right {
        held.insert(GestureControl::DpadRight);
    }

    PadSample {
        held,
        stick_y: dy,
        dpad_up,
        dpad_down,
        cross,
        circle,
        square,
        triangle,
        options,
        l2,
        r2,
    }
}

fn axis_u8(raw: u8) -> f32 {
    (f32::from(raw) - STICK_CENTER) / STICK_CENTER
}

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

/// Per-pad edge / hold / arming state for start-screen navigation.
#[derive(Debug, Clone, Default)]
struct PadSlot {
    nav_stepper: NavStepper,
    button_edges: ButtonEdges,
    triangle_hold: TriangleHold,
    cross_hold: CrossHold,
    armed: bool,
}

/// Bank of per-pad nav machines. Never merges samples across pads.
#[derive(Debug, Clone, Default)]
pub struct PadNavBank {
    slots: HashMap<PadId, PadSlot>,
    /// When true, newly seen pads start armed (e.g. after start screen opens cleanly).
    arm_new_pads: bool,
}

/// Which face / Options buttons are currently held (OR across armed pads).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FaceHeld {
    pub cross: bool,
    pub circle: bool,
    pub square: bool,
    pub triangle: bool,
    pub options: bool,
}

impl FaceHeld {
    fn from_sample(sample: &PadSample) -> Self {
        Self {
            cross: sample.cross,
            circle: sample.circle,
            square: sample.square,
            triangle: sample.triangle,
            options: sample.options,
        }
    }

    pub fn or(self, other: Self) -> Self {
        Self {
            cross: self.cross || other.cross,
            circle: self.circle || other.circle,
            square: self.square || other.square,
            triangle: self.triangle || other.triangle,
            options: self.options || other.options,
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
    pub cross_progress: f32,
    pub cross_completed: bool,
    /// Face buttons held on any armed pad this tick.
    pub held: FaceHeld,
    /// How many live pads are armed this tick.
    pub armed_pads: usize,
    /// How many live pads are still waiting for a resting sample.
    pub unarmed_pads: usize,
    /// Sample for edge logging: action pad, else first non-resting, else first.
    pub diag: Option<NavReading>,
}

impl PadNavBank {
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

    /// Cancel pending face/Options releases on every pad (e.g. after a slide change).
    pub fn cancel_held_face_releases(&mut self) {
        for slot in self.slots.values_mut() {
            slot.button_edges.cancel_held_releases();
        }
    }

    /// Sync presence, update every pad's edge/hold state, emit at most one action.
    ///
    /// `allow_nav_move` gates D-pad/stick repeats (false while a slide animates).
    /// `replace_confirm` switches to Cross-hold / Circle-cancel mode.
    /// `editing` when true: Triangle saves (ToggleEdit); Square is EditManual.
    /// `hold_cross_close` (games browse): Cross hold closes the running game.
    /// `hold_triangle_power` (controllers): Triangle hold powers off Bluetooth.
    #[allow(clippy::too_many_arguments)]
    pub fn tick(
        &mut self,
        readings: &[NavReading],
        now: Instant,
        allow_nav_move: bool,
        replace_confirm: bool,
        editing: bool,
        hold_cross_close: bool,
        hold_triangle_power: bool,
    ) -> PadTickResult {
        self.sync_presence(readings);

        let mut result = PadTickResult {
            diag: preferred_reading(readings).cloned(),
            ..Default::default()
        };

        let mut button_action: Option<(PadId, NavAction)> = None;
        let mut nav_action: Option<(PadId, NavAction)> = None;
        let mut newly_armed: Vec<PadId> = Vec::new();

        for reading in readings {
            let Some(slot) = self.slots.get_mut(&reading.id) else {
                continue;
            };

            if !slot.armed {
                slot.button_edges.clear();
                slot.button_edges.sync(&reading.sample);
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
                }
                slot.triangle_hold.reset();

                let stick_interrupt = reading.sample.dpad_up
                    || reading.sample.dpad_down
                    || reading.sample.stick_y != 0.0;
                let foreign = slot
                    .button_edges
                    .foreign_press(&reading.sample, Some(EdgeButton::Cross));
                if slot.cross_hold.is_active() && (stick_interrupt || foreign) {
                    slot.cross_hold.cancel();
                    result.cross_progress = 0.0;
                    result.cross_completed = false;
                }

                let edge = slot
                    .button_edges
                    .update(&reading.sample, Some(EdgeButton::Cross));
                if let Some(action) = edge
                    && action == NavAction::Cancel
                    && button_action.is_none()
                {
                    button_action = Some((reading.id.clone(), action));
                }
                continue;
            }

            // Games browse: Cross hold closes the running game (short Cross still Confirms).
            if hold_cross_close {
                let (c_progress, c_completed) = slot.cross_hold.update(reading.sample.cross, now);
                if c_progress > result.cross_progress {
                    result.cross_progress = c_progress;
                }
                if c_completed {
                    result.cross_completed = true;
                }
                let stick_interrupt = reading.sample.dpad_up
                    || reading.sample.dpad_down
                    || reading.sample.stick_y != 0.0;
                let foreign = slot
                    .button_edges
                    .foreign_press(&reading.sample, Some(EdgeButton::Cross));
                if slot.cross_hold.is_active() && (stick_interrupt || foreign) {
                    slot.cross_hold.cancel();
                    result.cross_progress = 0.0;
                    result.cross_completed = false;
                }
            } else {
                slot.cross_hold.reset();
            }

            // Controllers: Triangle hold powers off. Games browse / edit: short Triangle.
            let hold_owned = if hold_triangle_power {
                let (t_progress, t_completed) =
                    slot.triangle_hold.update(reading.sample.triangle, now);
                if t_progress > result.triangle_progress {
                    result.triangle_progress = t_progress;
                }
                if t_completed {
                    result.triangle_completed = true;
                }
                Some(EdgeButton::Triangle)
            } else {
                slot.triangle_hold.reset();
                None
            };

            let nav = if allow_nav_move {
                slot.nav_stepper.update(&reading.sample, now)
            } else {
                None
            };

            let foreign = slot.button_edges.foreign_press(&reading.sample, hold_owned);
            if hold_triangle_power && slot.triangle_hold.is_active() && (nav.is_some() || foreign) {
                slot.triangle_hold.cancel();
                result.triangle_progress = 0.0;
                result.triangle_completed = false;
            }

            let edge = slot.button_edges.update(&reading.sample, hold_owned);
            let edge = edge.map(|action| match action {
                // Edit mode: Square edits the selected manual (was Triangle).
                NavAction::CycleSort if editing => NavAction::Triangle,
                other => other,
            });

            if let Some(action) = nav
                && nav_action.is_none()
            {
                nav_action = Some((reading.id.clone(), action));
            }
            if let Some(action) = edge
                && button_action.is_none()
            {
                // A completed Cross-hold close must not also Confirm/Launch.
                if result.cross_completed && action == NavAction::Confirm {
                    continue;
                }
                button_action = Some((reading.id.clone(), action));
            }
        }

        // Prefer button over stick/D-pad when both fire this tick.
        let chosen = button_action.or(nav_action);
        if let Some((id, action)) = chosen {
            result.diag = readings.iter().find(|r| r.id == id).cloned();
            result.action_pad = Some(id);
            result.action = Some(action);
        }

        if !newly_armed.is_empty() {
            crate::app_log::info(format!(
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
            // First sight: sync edges without firing so held Cross is not a Confirm.
            slot.button_edges.sync(&reading.sample);
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
    /// reopen the start screen (used right after gesture recording commits).
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

/// Latch gesture recording to the first pad that holds a control (no cross-pad union).
#[derive(Debug, Clone, Default)]
pub struct GestureRecordLatch {
    pad: Option<PadId>,
}

impl GestureRecordLatch {
    pub fn clear(&mut self) {
        self.pad = None;
    }

    /// Returns the sample to feed the recorder, if any pad is latched / should latch.
    pub fn select<'a>(&mut self, readings: &'a [NavReading]) -> Option<&'a PadSample> {
        if let Some(id) = self.pad.clone() {
            if let Some(reading) = readings.iter().find(|r| r.id == id) {
                return Some(&reading.sample);
            }
            // Latched pad disappeared; allow another to take over this tick.
            self.pad = None;
        }
        for reading in readings {
            if !reading.sample.held.is_empty() {
                self.pad = Some(reading.id.clone());
                return Some(&reading.sample);
            }
        }
        None
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

/// True only for borderless cover-monitor windows (not ordinary maximized framed apps).
pub fn is_exclusive_fullscreen(facts: ForegroundWindowFacts) -> bool {
    if facts.width < facts.monitor_width || facts.height < facts.monitor_height {
        return false;
    }
    // Maximized framed windows keep WS_CAPTION / WS_THICKFRAME; exclusive FS usually does not.
    let framed =
        (facts.style & crate_win32_caption()) != 0 || (facts.style & crate_win32_thickframe()) != 0;
    !framed
}

fn crate_win32_caption() -> isize {
    #[cfg(windows)]
    {
        crate::win32::WS_CAPTION
    }
    #[cfg(not(windows))]
    {
        0x00C0_0000
    }
}

fn crate_win32_thickframe() -> isize {
    #[cfg(windows)]
    {
        crate::win32::WS_THICKFRAME
    }
    #[cfg(not(windows))]
    {
        0x0004_0000
    }
}

/// Whether the foreground window looks like exclusive fullscreen.
pub fn foreground_is_exclusive_fullscreen() -> bool {
    #[cfg(windows)]
    {
        use crate::win32;
        unsafe {
            let hwnd = win32::GetForegroundWindow();
            if hwnd == 0 {
                return false;
            }
            let mut rect = win32::Rect::default();
            if win32::GetWindowRect(hwnd, &mut rect) == 0 {
                return false;
            }
            let width = rect.right - rect.left;
            let height = rect.bottom - rect.top;
            let point = win32::Point {
                x: rect.left,
                y: rect.top,
            };
            let monitor = win32::MonitorFromPoint(point, win32::MONITOR_DEFAULTTOPRIMARY);
            if monitor == 0 {
                return false;
            }
            let mut info = win32::MonitorInfo {
                size: std::mem::size_of::<win32::MonitorInfo>() as u32,
                ..Default::default()
            };
            if win32::GetMonitorInfoW(monitor, &mut info) == 0 {
                return false;
            }
            let mon_w = info.monitor.right - info.monitor.left;
            let mon_h = info.monitor.bottom - info.monitor.top;
            let style = win32::GetWindowLongW(hwnd, win32::GWL_STYLE);
            is_exclusive_fullscreen(ForegroundWindowFacts {
                width,
                height,
                monitor_width: mon_w,
                monitor_height: mon_h,
                style,
            })
        }
    }
    #[cfg(not(windows))]
    {
        false
    }
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
    fn merge_nav_samples_keeps_newest_stick() {
        let a = PadSample {
            stick_y: -0.8,
            ..Default::default()
        };
        let b = PadSample {
            stick_y: 0.6,
            ..Default::default()
        };
        let merged = merge_nav_samples(&[a, b]).expect("batch");
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
            style: crate_win32_caption() | crate_win32_thickframe(),
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
    fn button_edges_fires_square_cycle_sort_on_release() {
        let mut edges = ButtonEdges::default();
        let held = PadSample {
            square: true,
            ..Default::default()
        };
        assert!(edges.update(&held, None).is_none());
        assert_eq!(
            edges.update(&PadSample::default(), None),
            Some(NavAction::CycleSort)
        );
    }

    #[test]
    fn button_edges_fires_triangle_toggle_edit_on_release() {
        let mut edges = ButtonEdges::default();
        let held = PadSample {
            triangle: true,
            ..Default::default()
        };
        assert!(edges.update(&held, None).is_none());
        assert_eq!(
            edges.update(&PadSample::default(), None),
            Some(NavAction::ToggleEdit)
        );
    }

    #[test]
    fn button_edges_options_does_not_fire() {
        let mut edges = ButtonEdges::default();
        let held = PadSample {
            options: true,
            ..Default::default()
        };
        assert!(edges.update(&held, None).is_none());
        assert!(edges.update(&PadSample::default(), None).is_none());
    }

    #[test]
    fn button_edges_fires_cross_on_release() {
        let mut edges = ButtonEdges::default();
        let held = PadSample {
            cross: true,
            ..Default::default()
        };
        assert!(edges.update(&held, None).is_none());
        assert_eq!(
            edges.update(&PadSample::default(), None),
            Some(NavAction::Confirm)
        );
    }

    #[test]
    fn button_edges_cancel_held_releases_blocks_square_toggle() {
        let mut edges = ButtonEdges::default();
        let square = PadSample {
            square: true,
            ..Default::default()
        };
        assert!(edges.update(&square, None).is_none());
        edges.cancel_held_releases();
        assert!(
            edges.update(&PadSample::default(), None).is_none(),
            "cancelled held Square must not CycleSort on release"
        );
    }

    #[test]
    fn button_edges_foreign_face_cancels_pending_release() {
        let mut edges = ButtonEdges::default();
        let square = PadSample {
            square: true,
            ..Default::default()
        };
        assert!(edges.update(&square, None).is_none());
        let both = PadSample {
            square: true,
            circle: true,
            ..Default::default()
        };
        assert!(edges.update(&both, None).is_none());
        let circle_only = PadSample {
            circle: true,
            ..Default::default()
        };
        // Square released while cancelled — no CycleSort.
        assert!(edges.update(&circle_only, None).is_none());
        assert_eq!(
            edges.update(&PadSample::default(), None),
            Some(NavAction::Cancel)
        );
    }

    #[test]
    fn button_edges_l2_r2_still_fire_on_press() {
        let mut edges = ButtonEdges::default();
        let r2 = PadSample {
            r2: true,
            ..Default::default()
        };
        assert_eq!(edges.update(&r2, None), Some(NavAction::NextSlide));
        assert!(edges.update(&r2, None).is_none());
        assert!(edges.update(&PadSample::default(), None).is_none());
    }

    #[test]
    fn button_edges_sync_then_held_r2_does_not_fire() {
        let mut edges = ButtonEdges::default();
        let held = PadSample {
            r2: true,
            ..Default::default()
        };
        edges.sync(&held);
        assert!(edges.update(&held, None).is_none());
        let released = PadSample::default();
        assert!(edges.update(&released, None).is_none());
        assert_eq!(edges.update(&held, None), Some(NavAction::NextSlide));
        assert!(edges.update(&released, None).is_none());
    }

    #[test]
    fn button_edges_hold_owned_triangle_never_arms() {
        let mut edges = ButtonEdges::default();
        let held = PadSample {
            triangle: true,
            ..Default::default()
        };
        assert!(edges.update(&held, Some(EdgeButton::Triangle)).is_none());
        assert!(
            edges
                .update(&PadSample::default(), Some(EdgeButton::Triangle))
                .is_none()
        );
    }

    #[test]
    fn sample_nav_resting_requires_clear_controls() {
        assert!(sample_nav_resting(&PadSample::default()));
        assert!(!sample_nav_resting(&PadSample {
            r2: true,
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
            source: NavSource::Hid,
        }
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
                _ => {}
            }
        }
        sample
    }

    #[test]
    fn pad_b_can_confirm_while_pad_a_holds_open_chord() {
        let mut bank = PadNavBank::default();
        bank.prepare_on_open(false);
        let now = Instant::now();

        // Pad A still holding open chord (unarmed); pad B at rest then Cross.
        let open = chord_sample(&[
            GestureControl::L2,
            GestureControl::R2,
            GestureControl::L3,
            GestureControl::R3,
        ]);
        let tick1 = bank.tick(
            &[
                reading("a", open.clone()),
                reading("b", PadSample::default()),
            ],
            now,
            true,
            false,
            false,
            false,
            false,
        );
        assert!(tick1.action.is_none());

        let cross = PadSample {
            cross: true,
            held: [GestureControl::Cross].into_iter().collect(),
            ..Default::default()
        };
        let tick2 = bank.tick(
            &[reading("a", open.clone()), reading("b", cross.clone())],
            now,
            true,
            false,
            false,
            false,
            false,
        );
        assert!(
            tick2.action.is_none(),
            "Cross arms on press, fires on release"
        );
        let tick3 = bank.tick(
            &[reading("a", open), reading("b", PadSample::default())],
            now,
            true,
            false,
            false,
            false,
            false,
        );
        assert_eq!(tick3.action, Some(NavAction::Confirm));
        assert_eq!(tick3.action_pad.as_ref().map(|p| p.as_str()), Some("b"));
    }

    #[test]
    fn same_tick_two_pads_emit_only_first_action() {
        let mut bank = PadNavBank::default();
        bank.prepare_on_open(true);
        let now = Instant::now();

        // Arm both at rest first.
        let _ = bank.tick(
            &[
                reading("a", PadSample::default()),
                reading("b", PadSample::default()),
            ],
            now,
            true,
            false,
            false,
            false,
            false,
        );

        let circle = PadSample {
            circle: true,
            held: [GestureControl::Circle].into_iter().collect(),
            ..Default::default()
        };
        let cross = PadSample {
            cross: true,
            held: [GestureControl::Cross].into_iter().collect(),
            ..Default::default()
        };
        let tick = bank.tick(
            &[reading("a", circle.clone()), reading("b", cross)],
            now,
            true,
            false,
            false,
            false,
            false,
        );
        assert!(tick.action.is_none(), "face actions arm on press");
        let tick2 = bank.tick(
            &[
                reading("a", PadSample::default()),
                reading("b", PadSample::default()),
            ],
            now,
            true,
            false,
            false,
            false,
            false,
        );
        assert_eq!(tick2.action, Some(NavAction::Cancel));
        assert_eq!(tick2.action_pad.as_ref().map(|p| p.as_str()), Some("a"));
    }

    #[test]
    fn reopen_gesture_requires_full_chord_on_one_pad() {
        let mut bank = GestureDetectorBank::default();
        let required = crate::gesture::default_gesture();

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
        let required = crate::gesture::default_gesture();
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

    #[test]
    fn gesture_record_latch_stays_on_first_pad() {
        let mut latch = GestureRecordLatch::default();
        let a = chord_sample(&[GestureControl::L2]);
        let b = chord_sample(&[GestureControl::R2]);
        let readings = [reading("a", a.clone()), reading("b", b.clone())];
        let sample = latch.select(&readings).expect("latch a");
        assert!(sample.held.contains(&GestureControl::L2));

        // Still latched to a even if empty; b ignored.
        let empty_a = PadSample::default();
        let readings2 = [reading("a", empty_a), reading("b", b)];
        let sample2 = latch.select(&readings2).expect("still a");
        assert!(sample2.held.is_empty());
    }
}
