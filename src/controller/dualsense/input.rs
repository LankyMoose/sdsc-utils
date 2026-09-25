//! DualSense input-report parsing into the shared [`PadSample`] nav contract.

use crate::ui::start::gesture::GestureControl;
use crate::ui::start::input::{PadSample, combined_stick};
use std::collections::BTreeSet;

const TRIGGER_ANALOG_THRESHOLD: u8 = 30;
const STICK_CENTER: f32 = 128.0;

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
