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

/// One emulated DualSense, whether it is plugged in, and whether it is
/// remembered across restarts.
///
/// `ControllerStatus` has no `PartialEq`, so identity is the serial — which is
/// what callers actually compare on.
#[derive(Debug, Clone)]
pub struct EmulatedPad {
    pub status: ControllerStatus,
    /// Pads start disconnected; publishing one runs the real connect path.
    pub connected: bool,
    /// Whether the pad persists to `controllers.json` when unplugged — the
    /// same "remembered" pin a real controller has in the popup.
    ///
    /// Forgetting a pad only takes effect once it is also unplugged: while it
    /// is still connected it stays on screen like any live pad, it simply
    /// stops being written to the remembered store.
    pub remembered: bool,
}

impl EmulatedPad {
    pub fn serial(&self) -> &str {
        &self.status.serial
    }
}

pub fn serial_for(index: usize) -> String {
    format!("{SERIAL_PREFIX}{index}")
}

fn pad(index: usize, percent: u8, connection: Connection) -> ControllerStatus {
    dualsense_status(
        index,
        "DualSense",
        connection,
        serial_for(index),
        percent,
        state_for(connection, percent),
    )
}

/// Charge state a pad reports on a given link at a given level.
///
/// The firmware has no independent "charging" switch: a pad on USB draws
/// power (reporting `Complete` once it tops off), one on Bluetooth runs
/// down. Deriving it keeps the emulator from describing combinations real
/// hardware cannot be in — there is no such thing as a discharging USB pad
/// or a charging Bluetooth one — so the UI exposes the link alone.
pub fn state_for(connection: Connection, percent: u8) -> PowerState {
    match connection {
        Connection::Usb if percent >= 100 => PowerState::Complete,
        Connection::Usb => PowerState::Charging,
        Connection::Bluetooth => PowerState::Discharging,
    }
}

/// Create a pad on the lowest free `emu-N` slot. Its charge state follows
/// from the link.
///
/// Starts **disconnected**: a fresh pad is remembered but not live, so it does
/// not fire a connect toast or open Start until you switch it on.
pub fn new_pad(fleet: &[EmulatedPad], percent: u8, connection: Connection) -> EmulatedPad {
    let taken: Vec<usize> = fleet.iter().map(|p| slot_of(p.serial())).collect();
    let index = (1..=taken.len() + 1)
        .find(|n| !taken.contains(n))
        .unwrap_or(1);
    EmulatedPad {
        status: pad(index, percent, connection),
        connected: false,
        remembered: true,
    }
}

/// Drop pads that are no longer worth listing.
///
/// A pad is dropped when it is **unplugged and forgotten** — that is exactly
/// what the popup's remember pin means for real hardware, and it is why the
/// two events are symmetric: forgetting an unplugged pad removes it now, and
/// unplugging an already-forgotten pad removes it then. A pad that is merely
/// unplugged stays listed so the user can plug it back in.
pub fn prune(fleet: Vec<EmulatedPad>) -> Vec<EmulatedPad> {
    fleet
        .into_iter()
        .filter(|p| p.connected || p.remembered)
        .collect()
}

/// Rebuild the fleet from remembered controllers, so pads added in a previous
/// run come back after a restart.
///
/// Callers pass only the emulated records; real hardware is not this fleet's
/// business, and the remembered store keeps no charge state to restore. Every
/// pad comes back **disconnected**: a remembered pad is not a plugged-in one,
/// and reconnecting here would fire connect toasts and open Start unbidden at
/// boot. Each keeps its remembered level and link, so it resumes where it was
/// left rather than at the `AddPad` default.
pub fn rehydrate(remembered: &[(String, u8, Connection)]) -> Vec<EmulatedPad> {
    let mut fleet: Vec<EmulatedPad> = remembered
        .iter()
        .filter(|(serial, _, _)| is_emulated(serial))
        .map(|(serial, percent, connection)| EmulatedPad {
            // Number by the pad's own slot so the rebuilt fleet keeps the
            // identity its serial implies; `pad` re-derives charge state from
            // the link (the remembered store persists no state).
            status: pad(slot_of(serial), *percent, *connection),
            connected: false,
            // Only remembered pads are ever written to `controllers.json`, so
            // anything read back from it is remembered by construction.
            remembered: true,
        })
        .collect();
    // Order by slot so emu-2 precedes emu-10.
    fleet.sort_by_key(|p| slot_of(p.serial()));
    fleet
}

/// Slot number encoded in an `emu-N` serial; non-emulated serials sort last.
fn slot_of(serial: &str) -> usize {
    serial
        .strip_prefix(SERIAL_PREFIX)
        .and_then(|n| n.parse().ok())
        .unwrap_or(usize::MAX)
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

/// Set the battery level, re-deriving the charge state (a USB pad dragged to
/// 100% becomes `Complete`, not still `Charging`).
pub fn set_percent(fleet: &[EmulatedPad], serial: &str, percent: u8) -> Vec<EmulatedPad> {
    update(fleet, serial, |p| {
        p.status.percent = percent;
        p.status.state = state_for(p.status.connection, percent);
    })
}

/// Move a pad to a new link, re-deriving its charge state.
pub fn set_link(fleet: &[EmulatedPad], serial: &str, connection: Connection) -> Vec<EmulatedPad> {
    update(fleet, serial, |p| {
        p.status.connection = connection;
        p.status.state = state_for(connection, p.status.percent);
    })
}

/// The connected pads — the list to publish as the live controller snapshot.
pub fn live(fleet: &[EmulatedPad]) -> Vec<ControllerStatus> {
    fleet
        .iter()
        .filter(|p| p.connected)
        .map(|p| p.status.clone())
        .collect()
}

/// Next `(percent, connection, state)` for one analytics charge advance.
///
/// Plugs the pad in at its current level; charging pads step up and land on
/// `Complete` at 100.
pub fn step_charge(pad: &ControllerStatus) -> (u8, Connection, PowerState) {
    let percent = if pad.state.is_discharging() {
        pad.percent
    } else {
        pad.percent.saturating_add(STEP_PERCENT).min(100)
    };
    (
        percent,
        Connection::Usb,
        state_for(Connection::Usb, percent),
    )
}

/// Next `(percent, connection, state)` for one analytics drain advance.
///
/// Unplugs the pad at its current level; discharging pads step down to the
/// low-battery floor.
pub fn step_drain(pad: &ControllerStatus) -> (u8, Connection, PowerState) {
    let percent = if pad.state.is_discharging() {
        pad.percent
            .saturating_sub(STEP_PERCENT)
            .max(LOW_BATTERY_PERCENT)
    } else {
        pad.percent
    };
    (percent, Connection::Bluetooth, PowerState::Discharging)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first() -> EmulatedPad {
        new_pad(&[], 50, Connection::Bluetooth)
    }

    #[test]
    fn new_pads_start_disconnected() {
        let pad = first();
        assert_eq!(pad.serial(), PRIMARY_SERIAL);
        assert!(!pad.connected);
        assert!(pad.remembered, "a pad persists until it is forgotten");
        assert_eq!(pad.status.percent, 50);
        // Nothing is published until it is switched on.
        assert!(live(std::slice::from_ref(&pad)).is_empty());
    }

    /// Forgetting an unplugged pad removes it immediately.
    #[test]
    fn forgetting_an_unplugged_pad_removes_it() {
        let pad = first();
        assert!(!pad.connected);
        let next = prune(update(std::slice::from_ref(&pad), PRIMARY_SERIAL, |p| {
            p.remembered = false
        }));
        assert!(next.is_empty(), "off and forgotten means gone");
    }

    /// Unplugging a forgotten pad removes it too — the same end state, reached
    /// from the other side. This is what makes the pair symmetric.
    #[test]
    fn unplugging_a_forgotten_pad_removes_it() {
        let plugged = update(std::slice::from_ref(&first()), PRIMARY_SERIAL, |p| {
            p.connected = true;
            p.remembered = false;
        });
        assert_eq!(plugged.len(), 1, "forgetting does not drop a live pad");
        let unplugged = prune(update(&plugged, PRIMARY_SERIAL, |p| p.connected = false));
        assert!(unplugged.is_empty());
    }

    #[test]
    fn forgetting_a_live_pad_keeps_it_until_unplugged() {
        let plugged = update(std::slice::from_ref(&first()), PRIMARY_SERIAL, |p| {
            p.connected = true;
        });
        // While plugged in it stays listed, like any connected controller.
        assert_eq!(prune(plugged).len(), 1);
    }

    #[test]
    fn unplugping_a_remembered_pad_keeps_it() {
        let plugged = update(std::slice::from_ref(&first()), PRIMARY_SERIAL, |p| {
            p.connected = true;
        });
        let unplugged = prune(update(&plugged, PRIMARY_SERIAL, |p| p.connected = false));
        assert_eq!(
            unplugged.len(),
            1,
            "an unplugged but remembered pad can be plugged back in"
        );
        assert!(unplugged[0].remembered);
    }

    #[test]
    fn re_remembering_restores_the_pad() {
        let pad = first();
        let forgotten = prune(update(std::slice::from_ref(&pad), PRIMARY_SERIAL, |p| {
            p.remembered = false
        }));
        assert!(forgotten.is_empty());
        // Nothing left to re-remember — the pad is gone, so a new one is needed.
        assert!(live(&forgotten).is_empty());
    }

    #[test]
    fn prune_leaves_other_pads_alone() {
        let a = update(std::slice::from_ref(&first()), PRIMARY_SERIAL, |p| {
            p.remembered = false;
        });
        let b = new_pad(&a, 70, Connection::Usb);
        let fleet = vec![a[0].clone(), b.clone()];
        let next = prune(fleet);
        let serials: Vec<&str> = next.iter().map(|p| p.serial()).collect();
        assert_eq!(serials, vec!["emu-2"], "only the off+forgotten pad goes");
    }

    #[test]
    fn new_pad_uses_lowest_free_slot_and_reuses_holes() {
        let a = first();
        let b = new_pad(std::slice::from_ref(&a), 80, Connection::Bluetooth);
        assert_eq!(b.serial(), "emu-2");

        let without_a = vec![b.clone()];
        let reused = new_pad(&without_a, 10, Connection::Bluetooth);
        assert_eq!(reused.serial(), PRIMARY_SERIAL);
    }

    #[test]
    fn live_publishes_only_connected_pads() {
        let a = first();
        let b = new_pad(std::slice::from_ref(&a), 80, Connection::Usb);
        let fleet = update(&[a, b], "emu-1", |p| p.connected = true);
        let live_pads = live(&fleet);
        assert_eq!(live_pads.len(), 1);
        assert_eq!(live_pads[0].serial, PRIMARY_SERIAL);
        assert_eq!(live_pads[0].percent, 50);
    }

    #[test]
    fn charge_state_follows_the_link() {
        // The firmware has no independent charging switch; the link implies it.
        assert_eq!(state_for(Connection::Bluetooth, 5), PowerState::Discharging);
        assert_eq!(
            state_for(Connection::Bluetooth, 100),
            PowerState::Discharging
        );
        assert_eq!(state_for(Connection::Usb, 5), PowerState::Charging);
        assert_eq!(state_for(Connection::Usb, 99), PowerState::Charging);
        // A topped-off USB pad reports Complete, not Charging.
        assert_eq!(state_for(Connection::Usb, 100), PowerState::Complete);
    }

    #[test]
    fn update_edits_the_named_pad_only() {
        let fleet = [first(), new_pad(&[first()], 80, Connection::Usb)];
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
        let (percent, connection, state) = step_charge(&pad);
        pad.percent = percent;
        pad.connection = connection;
        pad.state = state;
        assert_eq!(pad.connection, Connection::Usb);
        assert_eq!((pad.percent, pad.state), (50, PowerState::Charging));

        for _ in 0..10 {
            let (next_percent, next_connection, next_state) = step_charge(&pad);
            pad.percent = next_percent;
            pad.connection = next_connection;
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
        pad.connection = Connection::Usb;
        pad.percent = 100;
        // Complete pad unplugs at its current level.
        let (percent, connection, state) = step_drain(&pad);
        assert_eq!(connection, Connection::Bluetooth);
        assert_eq!((percent, state), (100, PowerState::Discharging));

        pad.percent = percent;
        pad.connection = connection;
        pad.state = state;
        for _ in 0..10 {
            let (next_percent, next_connection, next_state) = step_drain(&pad);
            pad.percent = next_percent;
            pad.connection = next_connection;
            pad.state = next_state;
            if pad.percent == LOW_BATTERY_PERCENT {
                break;
            }
        }
        assert_eq!(pad.percent, LOW_BATTERY_PERCENT);
        assert_eq!(pad.state, PowerState::Discharging);
    }

    #[test]
    fn set_percent_re_derives_the_charge_state() {
        let pad = new_pad(&[], 95, Connection::Usb);
        assert_eq!(pad.status.state, PowerState::Charging);

        // Dragging a USB pad to the top means it is done charging.
        let full = set_percent(std::slice::from_ref(&pad), PRIMARY_SERIAL, 100);
        assert_eq!(full[0].status.state, PowerState::Complete);
        // Dropping back below the top puts it on charge again.
        let back = set_percent(&full, PRIMARY_SERIAL, 95);
        assert_eq!(back[0].status.state, PowerState::Charging);
        // A Bluetooth pad is discharging either way.
        let bt = set_link(&back, PRIMARY_SERIAL, Connection::Bluetooth);
        let bt = set_percent(&bt, PRIMARY_SERIAL, 100);
        assert_eq!(bt[0].status.state, PowerState::Discharging);
    }

    #[test]
    fn set_link_re_derives_the_charge_state() {
        let pad = new_pad(&[], 60, Connection::Bluetooth);
        assert_eq!(pad.status.state, PowerState::Discharging);

        let plugged = set_link(std::slice::from_ref(&pad), PRIMARY_SERIAL, Connection::Usb);
        assert_eq!(plugged[0].status.state, PowerState::Charging);
        // Level is untouched; only the link and what follows from it move.
        assert_eq!(plugged[0].status.percent, 60);

        let full = set_percent(&plugged, PRIMARY_SERIAL, 100);
        let unplugged = set_link(&full, PRIMARY_SERIAL, Connection::Bluetooth);
        assert_eq!(unplugged[0].status.state, PowerState::Discharging);
        assert_eq!(unplugged[0].status.percent, 100);
    }

    #[test]
    fn rehydrate_restores_remembered_pads_disconnected() {
        // A pad added in a previous run is in the remembered store but not the
        // live fleet; it must come back in the tab, and must not come back
        // plugged in (that would fire a connect toast at boot).
        let remembered = [
            ("emu-1".to_string(), 40, Connection::Bluetooth),
            ("emu-2".to_string(), 90, Connection::Usb),
        ];
        let fleet = rehydrate(&remembered);
        assert_eq!(fleet.len(), 2);
        assert!(fleet.iter().all(|p| !p.connected));
        assert!(
            live(&fleet).is_empty(),
            "nothing is published until switched on"
        );

        // Level and link are remembered, not reset to the AddPad default.
        assert_eq!(fleet[0].status.percent, 40);
        assert_eq!(fleet[0].status.connection, Connection::Bluetooth);
        assert_eq!(fleet[0].status.state, PowerState::Discharging);
        assert_eq!(fleet[1].status.percent, 90);
        assert_eq!(fleet[1].status.connection, Connection::Usb);
        assert_eq!(fleet[1].status.state, PowerState::Charging);
    }

    #[test]
    fn rehydrate_ignores_real_hardware_and_orders_by_slot() {
        // Real hardware belongs to HID, not the emulator fleet.
        let remembered = [
            ("emu-10".to_string(), 50, Connection::Bluetooth),
            ("REAL-abc".to_string(), 50, Connection::Usb),
            ("emu-2".to_string(), 50, Connection::Bluetooth),
        ];
        let fleet = rehydrate(&remembered);
        let serials: Vec<&str> = fleet.iter().map(|p| p.serial()).collect();
        assert_eq!(
            serials,
            vec!["emu-2", "emu-10"],
            "slot order, not string order"
        );

        // The rebuilt slots must not collide with what new_pad hands out.
        let next = new_pad(&fleet, 50, Connection::Bluetooth);
        assert_eq!(next.serial(), "emu-1", "slot 1 is the first free one");
    }

    #[test]
    fn rehydrate_of_nothing_is_empty() {
        assert!(rehydrate(&[]).is_empty());
    }

    #[test]
    fn is_emulated_matches_the_serial_prefix() {
        assert!(is_emulated(PRIMARY_SERIAL));
        assert!(is_emulated("emu-42"));
        assert!(!is_emulated("REAL-1"));
        assert!(!is_emulated(""));
    }
}
