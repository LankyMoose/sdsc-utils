//! Developer-only emulated controllers (debug builds only).
//!
//! The emulator owns a **fleet**: every pad it knows about, whether or not it
//! is currently plugged in. Only connected pads are published as the live
//! controller snapshot, so toggling a pad runs the ordinary connect/disconnect
//! path — toasts, Start-open policy, analytics edges — while disconnected pads
//! stay remembered like any other pad the app has seen.
//!
//! Serials are prefixed [`SERIAL_PREFIX`] so HID frames, analytics, and the
//! dock can tell emulated pads from real hardware.

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

/// One emulated DualSense and whether it is currently plugged in.
///
/// `ControllerStatus` has no `PartialEq`, so identity is the serial — which is
/// what callers actually compare on.
#[derive(Debug, Clone)]
pub struct EmulatedPad {
    pub status: ControllerStatus,
    /// Pads start disconnected; publishing one runs the real connect path.
    pub connected: bool,
}

impl EmulatedPad {
    pub fn serial(&self) -> &str {
        &self.status.serial
    }
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

/// Create a pad on the lowest free `emu-N` slot.
///
/// Starts **disconnected**: a fresh pad is remembered but not live, so it does
/// not fire a connect toast or open Start until you switch it on.
pub fn new_pad(
    fleet: &[EmulatedPad],
    percent: u8,
    state: PowerState,
    connection: Connection,
) -> EmulatedPad {
    let taken: Vec<usize> = fleet
        .iter()
        .filter_map(|p| p.serial().strip_prefix(SERIAL_PREFIX))
        .filter_map(|n| n.parse::<usize>().ok())
        .collect();
    let index = (1..=taken.len() + 1)
        .find(|n| !taken.contains(n))
        .unwrap_or(1);
    EmulatedPad {
        status: pad(index, percent, state, connection),
        connected: false,
    }
}

/// Edit the pad with `serial` in place; unknown serials are a no-op.
pub fn update(
    fleet: &[EmulatedPad],
    serial: &str,
    edit: impl FnOnce(&mut EmulatedPad),
) -> Vec<EmulatedPad> {
    let target = fleet.iter().position(|p| p.serial() == serial);
    let mut next: Vec<EmulatedPad> = fleet.to_vec();
    if let Some(index) = target {
        edit(&mut next[index]);
    }
    next
}

/// The connected pads — the list to publish as the live controller snapshot.
pub fn live(fleet: &[EmulatedPad]) -> Vec<ControllerStatus> {
    fleet
        .iter()
        .filter(|p| p.connected)
        .map(|p| p.status.clone())
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

    fn first() -> EmulatedPad {
        new_pad(&[], 50, PowerState::Discharging, Connection::Usb)
    }

    #[test]
    fn new_pads_start_disconnected() {
        let pad = first();
        assert_eq!(pad.serial(), PRIMARY_SERIAL);
        assert!(!pad.connected);
        assert_eq!(pad.status.percent, 50);
        // Nothing is published until it is switched on.
        assert!(live(std::slice::from_ref(&pad)).is_empty());
    }

    #[test]
    fn new_pad_uses_lowest_free_slot_and_reuses_holes() {
        let a = first();
        let b = new_pad(
            std::slice::from_ref(&a),
            80,
            PowerState::Charging,
            Connection::Bluetooth,
        );
        assert_eq!(b.serial(), "emu-2");

        let without_a = vec![b.clone()];
        let reused = new_pad(&without_a, 10, PowerState::Discharging, Connection::Usb);
        assert_eq!(reused.serial(), PRIMARY_SERIAL);
    }

    #[test]
    fn live_publishes_only_connected_pads() {
        let a = first();
        let b = new_pad(
            std::slice::from_ref(&a),
            80,
            PowerState::Charging,
            Connection::Usb,
        );
        let fleet = update(&[a, b], "emu-1", |p| p.connected = true);
        let live_pads = live(&fleet);
        assert_eq!(live_pads.len(), 1);
        assert_eq!(live_pads[0].serial, PRIMARY_SERIAL);
        assert_eq!(live_pads[0].percent, 50);
    }

    #[test]
    fn update_edits_the_named_pad_only() {
        let fleet = [
            first(),
            new_pad(&[first()], 80, PowerState::Charging, Connection::Usb),
        ];
        let next = update(&fleet, "emu-2", |p| {
            p.status.percent = 33;
            p.status.state = PowerState::Complete;
            p.status.connection = Connection::Bluetooth;
            p.connected = true;
        });
        assert_eq!(next[0].status.percent, 50, "emu-1 untouched");
        assert_eq!(next[1].status.percent, 33);
        assert_eq!(next[1].status.state, PowerState::Complete);
        assert_eq!(next[1].status.connection, Connection::Bluetooth);
        assert!(next[1].connected);
        // Unknown serial is a no-op.
        let untouched = update(&fleet, "nope", |p| p.connected = true);
        assert_eq!(untouched.len(), fleet.len());
        assert!(untouched.iter().all(|p| !p.connected));
    }

    #[test]
    fn charge_step_plugs_in_then_walks_up_to_complete() {
        let mut pad = first().status;
        // Discharging pad plugs in at its current level.
        let (percent, state) = step_charge(&pad);
        pad.percent = percent;
        pad.state = state;
        assert_eq!((pad.percent, pad.state), (50, PowerState::Charging));

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
        let mut pad = first().status;
        pad.state = PowerState::Complete;
        pad.percent = 100;
        // Complete pad unplugs at its current level.
        let (percent, state) = step_drain(&pad);
        assert_eq!((percent, state), (100, PowerState::Discharging));

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
    fn is_emulated_matches_the_serial_prefix() {
        assert!(is_emulated(PRIMARY_SERIAL));
        assert!(is_emulated("emu-42"));
        assert!(!is_emulated("REAL-1"));
        assert!(!is_emulated(""));
    }
}
