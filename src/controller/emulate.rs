//! Developer-only emulated controllers (debug builds only).
//!
//! The emulator owns the *set* of emulated pads the daemon publishes to the
//! session; serials are prefixed [`SERIAL_PREFIX`] so HID frames, analytics,
//! and the dock can tell them from real hardware. Every operation takes the
//! current controller list and returns the next one, so a caller applies a
//! single snapshot at a time instead of threading hidden state.

use crate::controller::dualsense::battery::{LOW_BATTERY_PERCENT, dualsense_status};
use crate::controller::model::Connection;
use crate::controller::model::{ControllerStatus, PowerState};
use std::time::Duration;

pub const SERIAL_PREFIX: &str = "emu-";
/// Serial of the first emulated pad. Analytics helpers default to it.
pub const PRIMARY_SERIAL: &str = "emu-1";
/// Battery step for one analytics charge/drain advance.
pub const STEP_PERCENT: u8 = 20;
/// Active time credited by a charge advance (a full charge cycle is ~2h).
pub const CHARGE_STEP_TIME: Duration = Duration::from_secs(15 * 60);
/// Active time credited by a drain advance (a full play cycle is ~8h).
pub const DRAIN_STEP_TIME: Duration = Duration::from_secs(25 * 60);

pub fn is_emulated(serial: &str) -> bool {
    serial.starts_with(SERIAL_PREFIX)
}

/// The emulated subset of `current` (the session may also hold real hardware).
pub fn emulated_pads(current: &[ControllerStatus]) -> Vec<ControllerStatus> {
    current
        .iter()
        .filter(|c| is_emulated(&c.serial))
        .cloned()
        .collect()
}

pub fn serial_for(index: usize) -> String {
    format!("{SERIAL_PREFIX}{index}")
}

fn pad(index: usize, percent: u8, state: PowerState, connection: Connection) -> ControllerStatus {
    dualsense_status(
        index,
        "DualSense",
        connection,
        serial_for(index),
        percent,
        state,
    )
}

/// Add a pad on the lowest free `emu-N` slot, leaving real pads untouched.
pub fn add(
    current: &[ControllerStatus],
    percent: u8,
    state: PowerState,
    connection: Connection,
) -> Vec<ControllerStatus> {
    let used: Vec<usize> = current
        .iter()
        .filter_map(|c| c.serial.strip_prefix(SERIAL_PREFIX))
        .filter_map(|n| n.parse::<usize>().ok())
        .collect();
    let index = (1..=used.len() + 1)
        .find(|n| !used.contains(n))
        .unwrap_or(1);
    let mut next = current.to_vec();
    next.push(pad(index, percent, state, connection));
    next
}

/// Drop the pad with `serial`.
pub fn remove(current: &[ControllerStatus], serial: &str) -> Vec<ControllerStatus> {
    current
        .iter()
        .filter(|c| c.serial != serial)
        .cloned()
        .collect()
}

/// Mutate the pad with `serial` in place; unknown serials are a no-op.
pub fn update(
    current: &[ControllerStatus],
    serial: &str,
    edit: impl Fn(&mut ControllerStatus),
) -> Vec<ControllerStatus> {
    current
        .iter()
        .map(|c| {
            let mut next = c.clone();
            if next.serial == serial {
                edit(&mut next);
            }
            next
        })
        .collect()
}

/// Next `(percent, state)` for one analytics charge advance.
///
/// Discharging pads plug in at their current level; charging pads step up and
/// land on `Complete` at 100.
pub fn step_charge(pad: &ControllerStatus) -> (u8, PowerState) {
    if pad.state.is_discharging() {
        return (pad.percent, PowerState::Charging);
    }
    let next = pad.percent.saturating_add(STEP_PERCENT).min(100);
    if next >= 100 {
        (100, PowerState::Complete)
    } else {
        (next, PowerState::Charging)
    }
}

/// Next `(percent, state)` for one analytics drain advance.
///
/// Charging/Complete pads unplug at their current level; discharging pads step
/// down to the low-battery floor.
pub fn step_drain(pad: &ControllerStatus) -> (u8, PowerState) {
    if pad.state.is_discharging() {
        let next = pad
            .percent
            .saturating_sub(STEP_PERCENT)
            .max(LOW_BATTERY_PERCENT);
        (next, PowerState::Discharging)
    } else {
        (pad.percent, PowerState::Discharging)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_pad() -> Vec<ControllerStatus> {
        add(&[], 50, PowerState::Discharging, Connection::Usb)
    }

    #[test]
    fn add_uses_lowest_free_slot_and_keeps_real_pads() {
        let mut current = one_pad();
        current.push(dualsense_status(
            7,
            "DualSense",
            Connection::Usb,
            "REAL-1".to_string(),
            90,
            PowerState::Discharging,
        ));
        let next = add(&current, 20, PowerState::Discharging, Connection::Usb);
        assert_eq!(next.len(), 3);
        assert_eq!(next[0].serial, PRIMARY_SERIAL);
        assert_eq!(next[1].serial, "REAL-1");
        assert_eq!(next[2].serial, "emu-2");

        // A hole is reused rather than growing the index forever.
        let without_first = remove(&next, PRIMARY_SERIAL);
        let again = add(&without_first, 20, PowerState::Discharging, Connection::Usb);
        assert_eq!(again.len(), 3);
        assert_eq!(again.last().unwrap().serial, PRIMARY_SERIAL);
    }

    #[test]
    fn add_defaults_are_a_healthy_discharging_pad() {
        let pads = one_pad();
        assert_eq!(pads[0].percent, 50);
        assert!(pads[0].state.is_discharging());
        assert_eq!(pads[0].index, 1);
    }

    #[test]
    fn remove_only_drops_the_named_pad() {
        let pads = add(&one_pad(), 80, PowerState::Charging, Connection::Usb);
        assert_eq!(pads.len(), 2);
        let next = remove(&pads, PRIMARY_SERIAL);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].serial, "emu-2");
        // Unknown serial is a no-op.
        assert_eq!(remove(&pads, "nope").len(), 2);
    }

    #[test]
    fn update_edits_the_named_pad_only() {
        let pads = add(&one_pad(), 80, PowerState::Charging, Connection::Usb);
        let next = update(&pads, "emu-2", |c| {
            c.percent = 33;
            c.state = PowerState::Complete;
            c.connection = Connection::Bluetooth;
        });
        assert_eq!(next[0].percent, 50, "emu-1 untouched");
        assert_eq!(next[1].percent, 33);
        assert_eq!(next[1].state, PowerState::Complete);
        assert_eq!(next[1].connection, Connection::Bluetooth);
    }

    #[test]
    fn charge_step_plugs_in_then_walks_up_to_complete() {
        let pads = one_pad();
        // Discharging pad plugs in at its current level.
        let (percent, state) = step_charge(&pads[0]);
        assert_eq!((percent, state), (50, PowerState::Charging));

        let mut pad = pads[0].clone();
        pad.percent = percent;
        pad.state = state;
        for _ in 0..10 {
            let (next_percent, next_state) = step_charge(&pad);
            pad.percent = next_percent;
            pad.state = next_state;
            if pad.state == PowerState::Complete {
                break;
            }
        }
        assert_eq!(pad.state, PowerState::Complete);
        assert_eq!(pad.percent, 100);
    }

    #[test]
    fn drain_step_unplugs_then_walks_down_to_the_floor() {
        let mut pads = one_pad();
        pads[0].state = PowerState::Complete;
        pads[0].percent = 100;
        // Complete pad unplugs at its current level.
        let (percent, state) = step_drain(&pads[0]);
        assert_eq!((percent, state), (100, PowerState::Discharging));

        let mut pad = pads[0].clone();
        pad.percent = percent;
        pad.state = state;
        for _ in 0..10 {
            let (next_percent, next_state) = step_drain(&pad);
            pad.percent = next_percent;
            pad.state = next_state;
            if pad.percent == LOW_BATTERY_PERCENT {
                break;
            }
        }
        assert_eq!(pad.percent, LOW_BATTERY_PERCENT);
    }

    #[test]
    fn emulated_pads_filters_out_real_hardware() {
        let mut current = one_pad();
        current.push(dualsense_status(
            7,
            "DualSense",
            Connection::Usb,
            "REAL-1".to_string(),
            90,
            PowerState::Discharging,
        ));
        let emulated = emulated_pads(&current);
        assert_eq!(emulated.len(), 1);
        assert_eq!(emulated[0].serial, PRIMARY_SERIAL);
    }

    #[test]
    fn is_emulated_matches_the_serial_prefix() {
        assert!(is_emulated(PRIMARY_SERIAL));
        assert!(is_emulated("emu-42"));
        assert!(!is_emulated("REAL-1"));
        assert!(!is_emulated(""));
    }
}
