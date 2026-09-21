//! Start-screen navigation (Windows.Gaming.Input + DualSense HID fallback) and reopen gestures.

use crate::dualsense::{self, is_dualsense_gamepad};
use crate::gesture::GestureControl;
use hidapi::{BusType, HidApi, HidDevice};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

const USB_REPORT_ID: u8 = 0x01;
const BT_REPORT_FULL: u8 = 0x31;
const BT_REPORT_TRUNCATED: u8 = 0x01;
const CALIBRATION_FEATURE_REPORT: u8 = 0x05;
const CALIBRATION_FEATURE_SIZE: usize = 41;

const STICK_CENTER: f32 = 128.0;
/// Per-axis deadzone applied to each stick before combining (kills rest bias).
const STICK_AXIS_DEADZONE: f32 = 0.25;
/// Residual deadzone on the combined vector after per-stick cleaning.
const STICK_COMBINED_DEADZONE: f32 = 0.15;
const TRIGGER_ANALOG_THRESHOLD: u8 = 64; // ~25% of 255
const NAV_INITIAL_DELAY: Duration = Duration::from_millis(280);
const NAV_REPEAT: Duration = Duration::from_millis(90);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavAction {
    Up,
    Down,
    Confirm,
    Cancel,
    PrevSlide,
    NextSlide,
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
    pub triangle: bool,
    pub l2: bool,
    pub r2: bool,
    /// Raw DualSense `buttons[0]` (hat + face). Useful for offset diagnostics.
    pub buttons0: u8,
}

/// Which backend produced a start-screen nav sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavSource {
    Gamepad,
    Hid,
}

impl NavSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gamepad => "gamepad",
            Self::Hid => "hid",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NavReading {
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
    pub stick: StickBand,
}

impl NavLogSnapshot {
    pub fn from_sample(sample: &PadSample) -> Self {
        Self {
            cross: sample.cross,
            circle: sample.circle,
            dpad_up: sample.dpad_up,
            dpad_down: sample.dpad_down,
            stick: StickBand::from_stick_y(sample.stick_y),
        }
    }

    pub fn format_line(self, source: NavSource) -> String {
        format!(
            "start-nav: src={} cross={} circle={} dpad_up={} dpad_down={} stick={}",
            source.as_str(),
            u8::from(self.cross),
            u8::from(self.circle),
            u8::from(self.dpad_up),
            u8::from(self.dpad_down),
            self.stick.as_str(),
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

#[derive(Debug, Clone, Default)]
pub struct ButtonEdges {
    cross: bool,
    circle: bool,
    l2: bool,
    r2: bool,
}

impl ButtonEdges {
    pub fn update(&mut self, sample: &PadSample) -> Option<NavAction> {
        let mut action = None;
        if sample.cross && !self.cross {
            action = Some(NavAction::Confirm);
        } else if sample.circle && !self.circle {
            action = Some(NavAction::Cancel);
        } else if sample.l2 && !self.l2 {
            action = Some(NavAction::PrevSlide);
        } else if sample.r2 && !self.r2 {
            action = Some(NavAction::NextSlide);
        }
        self.sync(sample);
        action
    }

    /// Track held buttons without emitting rising-edge actions (used while nav is disarmed).
    pub fn sync(&mut self, sample: &PadSample) {
        self.cross = sample.cross;
        self.circle = sample.circle;
        self.l2 = sample.l2;
        self.r2 = sample.r2;
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Tracks a face-button hold (0..=1) and fires once after [`BUTTON_HOLD`].
#[derive(Debug, Clone, Default)]
pub struct HoldTracker {
    started: Option<Instant>,
    fired: bool,
}

const BUTTON_HOLD: Duration = Duration::from_secs(1);

impl HoldTracker {
    /// Returns `(progress, just_completed)`.
    pub fn update(&mut self, held: bool, now: Instant) -> (f32, bool) {
        if !held {
            self.started = None;
            self.fired = false;
            return (0.0, false);
        }
        let started = *self.started.get_or_insert(now);
        let elapsed = now.saturating_duration_since(started);
        let progress = (elapsed.as_secs_f32() / BUTTON_HOLD.as_secs_f32()).min(1.0);
        if progress >= 1.0 && !self.fired {
            self.fired = true;
            return (1.0, true);
        }
        (progress, false)
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Triangle / Cross hold trackers share the same timing.
pub type TriangleHold = HoldTracker;
pub type CrossHold = HoldTracker;

/// True when face buttons, triggers, D-pad, and stick are at rest (safe to arm start nav).
pub fn sample_nav_resting(sample: &PadSample) -> bool {
    !sample.cross
        && !sample.circle
        && !sample.triangle
        && !sample.l2
        && !sample.r2
        && !sample.dpad_up
        && !sample.dpad_down
        && sample.stick_y == 0.0
}

/// Read start-screen navigation: Gaming.Input Gamepad first, then short DualSense HID.
///
/// HID path is open/read/close (same as gestures) so it does not hold the device.
pub fn read_nav_sample() -> NavReadOutcome {
    if let Some(sample) = read_gamepad_nav_sample() {
        return NavReadOutcome::Sample(NavReading {
            sample,
            source: NavSource::Gamepad,
        });
    }
    match read_gesture_sample() {
        Some(sample) => NavReadOutcome::Sample(NavReading {
            sample,
            source: NavSource::Hid,
        }),
        None => NavReadOutcome::Missing {
            hid_fallback_attempted: true,
        },
    }
}

/// Result of a start-screen nav poll.
#[derive(Debug)]
pub enum NavReadOutcome {
    Sample(NavReading),
    /// Both backends failed. `hid_fallback_attempted` is true when Gaming.Input
    /// had no usable pad and DualSense HID was tried.
    Missing {
        hid_fallback_attempted: bool,
    },
}

/// Windows.Gaming.Input only (A=Cross, B=Circle). Empty when DualSense is not a Gamepad.
#[cfg(windows)]
fn read_gamepad_nav_sample() -> Option<PadSample> {
    use windows::Gaming::Input::{Gamepad, GamepadButtons};

    let gamepads = Gamepad::Gamepads().ok()?;
    let count = gamepads.Size().ok()?;
    if count == 0 {
        return None;
    }
    // Prefer the last-connected pad (highest index).
    let pad = gamepads.GetAt(count - 1).ok()?;
    let reading = pad.GetCurrentReading().ok()?;

    let buttons = reading.Buttons;
    let dpad_up = buttons.contains(GamepadButtons::DPadUp);
    let dpad_down = buttons.contains(GamepadButtons::DPadDown);
    // Standard gamepad: A ≈ Cross, B ≈ Circle, Y ≈ Triangle on DualSense via Windows.
    let cross = buttons.contains(GamepadButtons::A);
    let circle = buttons.contains(GamepadButtons::B);
    let triangle = buttons.contains(GamepadButtons::Y);
    let l2 = reading.LeftTrigger >= 0.25;
    let r2 = reading.RightTrigger >= 0.25;

    let lx = reading.LeftThumbstickX as f32;
    let ly = -(reading.LeftThumbstickY as f32); // Windows Y is up-positive; we use up-negative.
    let rx = reading.RightThumbstickX as f32;
    let ry = -(reading.RightThumbstickY as f32);
    let (_dx, dy) = combined_stick(lx, ly, rx, ry);

    let mut held = BTreeSet::new();
    if cross {
        held.insert(GestureControl::Cross);
    }
    if circle {
        held.insert(GestureControl::Circle);
    }
    if triangle {
        held.insert(GestureControl::Triangle);
    }
    if l2 {
        held.insert(GestureControl::L2);
    }
    if r2 {
        held.insert(GestureControl::R2);
    }
    if dpad_up {
        held.insert(GestureControl::DpadUp);
    }
    if dpad_down {
        held.insert(GestureControl::DpadDown);
    }

    Some(PadSample {
        held,
        stick_y: dy,
        dpad_up,
        dpad_down,
        cross,
        circle,
        triangle,
        l2,
        r2,
        buttons0: 0,
    })
}

#[cfg(not(windows))]
fn read_gamepad_nav_sample() -> Option<PadSample> {
    None
}

/// Short DualSense HID open/read/close for reopen-gesture chords (L2/R2/L3/R3, etc.).
pub fn read_gesture_sample() -> Option<PadSample> {
    dualsense::with_hid_lock(read_gesture_sample_unlocked)
}

fn read_gesture_sample_unlocked() -> Option<PadSample> {
    let api = HidApi::new().ok()?;
    for info in api.device_list().filter(|d| is_dualsense_gamepad(d)) {
        let Ok(device) = info.open_device(&api) else {
            continue;
        };
        let is_bluetooth = matches!(info.bus_type(), BusType::Bluetooth);
        if let Some(sample) = read_device_sample_once(&device, is_bluetooth) {
            return Some(sample);
        }
    }
    None
}

fn read_device_sample_once(device: &HidDevice, is_bluetooth: bool) -> Option<PadSample> {
    let report_size = if is_bluetooth { 78 } else { 64 };
    let mut requested_full = false;

    for _ in 0..6 {
        let mut buf = vec![0u8; report_size];
        let n = device.read_timeout(&mut buf, 80).ok()?;
        if n == 0 {
            continue;
        }

        if is_bluetooth && buf[0] == BT_REPORT_TRUNCATED {
            if !requested_full {
                let mut feature = vec![0u8; CALIBRATION_FEATURE_SIZE];
                feature[0] = CALIBRATION_FEATURE_REPORT;
                let _ = device.get_feature_report(&mut feature);
                requested_full = true;
            }
            continue;
        }

        let expected = if is_bluetooth {
            BT_REPORT_FULL
        } else {
            USB_REPORT_ID
        };
        if buf[0] != expected {
            continue;
        }

        let base = if is_bluetooth { 1 } else { 0 };
        return Some(parse_report(&buf, base));
    }
    None
}

/// Parse sticks/buttons from a DualSense input report.
///
/// `base` indexes the report-ID byte. Common payload (Linux `dualsense_input_report`)
/// starts at `base + 1`: sticks, triggers, then `seq_number` at `base + 7`, then
/// `buttons[]` at `base + 8`. USB uses `base = 0`; BT `0x31` uses `base = 1`.
pub fn parse_report(buf: &[u8], base: usize) -> PadSample {
    let lx = axis(buf.get(base + 1).copied().unwrap_or(128));
    let ly = axis(buf.get(base + 2).copied().unwrap_or(128));
    let rx = axis(buf.get(base + 3).copied().unwrap_or(128));
    let ry = axis(buf.get(base + 4).copied().unwrap_or(128));
    let l2_analog = buf.get(base + 5).copied().unwrap_or(0);
    let r2_analog = buf.get(base + 6).copied().unwrap_or(0);
    // base + 7 is seq_number; buttons start at base + 8.
    let buttons0 = buf.get(base + 8).copied().unwrap_or(0);
    let buttons1 = buf.get(base + 9).copied().unwrap_or(0);
    let buttons2 = buf.get(base + 10).copied().unwrap_or(0);

    let (dx, dy) = combined_stick(lx, ly, rx, ry);
    let _ = dx;

    let dpad = buttons0 & 0x0F;
    // DualSense hat: 0..=7 directions, 8..=15 = released (null).
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
        triangle,
        l2,
        r2,
        buttons0,
    }
}

fn axis(raw: u8) -> f32 {
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
    fn combined_stick_either_side_moves() {
        let (_, dy) = combined_stick(0.0, -0.8, 0.0, 0.0);
        assert!(dy < -0.3);
        let (_, dy2) = combined_stick(0.0, 0.0, 0.0, 0.8);
        assert!(dy2 > 0.3);
    }

    #[test]
    fn combined_stick_reinforces_and_cancels() {
        let (_, dy) = combined_stick(0.0, -0.5, 0.0, -0.5);
        assert!(dy < -0.9);
        let (_, dy2) = combined_stick(0.0, -0.5, 0.0, 0.5);
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
    fn parse_report_reads_cross_and_l2() {
        let mut buf = vec![0u8; 64];
        buf[0] = USB_REPORT_ID;
        buf[1] = 128;
        buf[2] = 128;
        buf[3] = 128;
        buf[4] = 128;
        buf[5] = 200; // L2 analog
        buf[7] = 0; // seq_number (must not be read as buttons)
        buf[8] = 0x08 | (1 << 5); // resting hat + Cross
        buf[9] = (1 << 6) | (1 << 7); // L3 + R3
        let sample = parse_report(&buf, 0);
        assert!(sample.cross);
        assert!(!sample.dpad_up);
        assert_eq!(sample.buttons0, 0x08 | (1 << 5));
        assert!(sample.held.contains(&GestureControl::L2));
        assert!(sample.held.contains(&GestureControl::L3));
        assert!(sample.held.contains(&GestureControl::R3));
    }

    #[test]
    fn parse_report_seq_zero_is_not_dpad_up() {
        let mut buf = vec![0u8; 64];
        buf[0] = USB_REPORT_ID;
        buf[1] = 128;
        buf[2] = 128;
        buf[3] = 128;
        buf[4] = 128;
        buf[7] = 0; // seq
        buf[8] = 0x08; // neutral hat
        let sample = parse_report(&buf, 0);
        assert!(!sample.dpad_up);
        assert!(!sample.dpad_down);
        assert!(!sample.cross);
    }

    #[test]
    fn button_edges_fires_cross_once() {
        let mut edges = ButtonEdges::default();
        let sample = PadSample {
            cross: true,
            ..Default::default()
        };
        assert_eq!(edges.update(&sample), Some(NavAction::Confirm));
        assert!(edges.update(&sample).is_none());
    }

    #[test]
    fn button_edges_sync_then_held_r2_does_not_fire() {
        let mut edges = ButtonEdges::default();
        let held = PadSample {
            r2: true,
            ..Default::default()
        };
        edges.sync(&held);
        assert!(edges.update(&held).is_none());
        let released = PadSample::default();
        assert!(edges.update(&released).is_none());
        assert_eq!(edges.update(&held), Some(NavAction::NextSlide));
    }

    #[test]
    fn sample_nav_resting_requires_clear_controls() {
        assert!(sample_nav_resting(&PadSample::default()));
        assert!(!sample_nav_resting(&PadSample {
            r2: true,
            ..Default::default()
        }));
    }
}
