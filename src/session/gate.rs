//! Pure start-open / connect-cooldown / poll-hold helpers.

use crate::controller::model::ControllerStatus;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

/// Gate for automatic start-screen open on a 0→1 controller connect.
pub fn should_auto_open_start(
    enabled: bool,
    previous_empty: bool,
    next_nonempty: bool,
    already_open: bool,
    cooldown_active: bool,
    fullscreen: bool,
) -> bool {
    enabled && previous_empty && next_nonempty && !already_open && !cooldown_active && !fullscreen
}

/// Defer 0→1 Start until a connect toast finishes slide-in (both stay on screen).
pub fn should_defer_auto_open_start(want_open: bool, connect_toast_queued: bool) -> bool {
    want_open && connect_toast_queued
}

/// Consume the intentional power-off flag; returns true when cooldown must be skipped.
pub fn take_skip_connect_cooldown(flag: &mut bool) -> bool {
    let skip = *flag;
    *flag = false;
    skip
}

/// Whether to arm the ghost-flap cooldown after the list goes empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectCooldownDecision {
    SkipIntentional,
    SkipShortArrival,
    Arm,
}

/// Decide cooldown after 1→0. Short arrival blips must not suppress the next open.
pub fn connect_cooldown_on_empty(
    skip_intentional: bool,
    nonempty_stretch: Option<Duration>,
    min_stretch: Duration,
) -> ConnectCooldownDecision {
    if skip_intentional {
        ConnectCooldownDecision::SkipIntentional
    } else if nonempty_stretch.is_some_and(|d| d < min_stretch) {
        ConnectCooldownDecision::SkipShortArrival
    } else {
        ConnectCooldownDecision::Arm
    }
}

/// Latched flush: open if still enabled and not already open (ignore fullscreen/cooldown).
pub fn should_flush_latched_start(pending: bool, start_enabled: bool, already_open: bool) -> bool {
    pending && start_enabled && !already_open
}

/// Keep a pad for one consecutive poll miss; drop on the second.
///
/// Returns the reconciled list and serials held this tick (for diagnostics).
pub fn reconcile_poll_with_hold(
    previous: &[ControllerStatus],
    polled: Vec<ControllerStatus>,
    miss_counts: &mut HashMap<String, u8>,
) -> (Vec<ControllerStatus>, Vec<String>) {
    let polled_serials: HashSet<String> = polled.iter().map(|c| c.serial.clone()).collect();

    // Pads that returned clear their miss streak.
    miss_counts.retain(|serial, _| !polled_serials.contains(serial));

    let mut out = polled;
    let mut held = Vec::new();

    for prev in previous {
        if polled_serials.contains(&prev.serial) {
            continue;
        }
        let count = miss_counts.entry(prev.serial.clone()).or_insert(0);
        *count = count.saturating_add(1);
        if *count == 1 {
            held.push(prev.serial.clone());
            out.push(prev.clone());
        } else {
            // Second consecutive miss: accept the drop.
            miss_counts.remove(&prev.serial);
        }
    }

    out.sort_by(|a, b| a.serial.cmp(&b.serial));
    for (i, controller) in out.iter_mut().enumerate() {
        controller.index = i + 1;
    }
    (out, held)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::dualsense::battery::dualsense_status;
    use crate::controller::model::{Connection, PowerState};

    fn pad(serial: &str) -> ControllerStatus {
        dualsense_status(
            1,
            "DualSense",
            Connection::Bluetooth,
            serial.to_string(),
            50,
            PowerState::Discharging,
        )
    }

    #[test]
    fn opens_on_clean_zero_to_one() {
        assert!(should_auto_open_start(
            true, true, true, false, false, false
        ));
    }

    #[test]
    fn skips_when_disabled_empty_open_cooldown_or_fullscreen() {
        assert!(!should_auto_open_start(
            false, true, true, false, false, false
        ));
        assert!(!should_auto_open_start(
            true, false, true, false, false, false
        ));
        assert!(!should_auto_open_start(
            true, true, true, true, false, false
        ));
        assert!(!should_auto_open_start(
            true, true, true, false, true, false
        ));
        assert!(!should_auto_open_start(
            true, true, true, false, false, true
        ));
    }

    #[test]
    fn defers_start_only_when_connect_toast_pending() {
        assert!(should_defer_auto_open_start(true, true));
        assert!(!should_defer_auto_open_start(true, false));
        assert!(!should_defer_auto_open_start(false, true));
        assert!(!should_defer_auto_open_start(false, false));
    }

    #[test]
    fn latched_flush_ignores_fullscreen_and_requires_pending() {
        assert!(should_flush_latched_start(true, true, false));
        assert!(!should_flush_latched_start(true, true, true));
        assert!(!should_flush_latched_start(true, false, false));
        assert!(!should_flush_latched_start(false, true, false));
    }

    #[test]
    fn intentional_power_off_skips_connect_cooldown_once() {
        let mut flag = true;
        assert!(take_skip_connect_cooldown(&mut flag));
        assert!(!flag);
        assert!(!take_skip_connect_cooldown(&mut flag));
    }

    #[test]
    fn short_arrival_skips_cooldown_long_stretch_arms() {
        let min = Duration::from_secs(2);
        assert_eq!(
            connect_cooldown_on_empty(true, Some(Duration::from_secs(10)), min),
            ConnectCooldownDecision::SkipIntentional
        );
        assert_eq!(
            connect_cooldown_on_empty(false, Some(Duration::from_millis(500)), min),
            ConnectCooldownDecision::SkipShortArrival
        );
        assert_eq!(
            connect_cooldown_on_empty(false, Some(Duration::from_secs(3)), min),
            ConnectCooldownDecision::Arm
        );
        assert_eq!(
            connect_cooldown_on_empty(false, None, min),
            ConnectCooldownDecision::Arm
        );
    }

    #[test]
    fn hold_keeps_pad_one_miss_then_drops() {
        let mut misses = HashMap::new();
        let prev = vec![pad("aa:bb"), pad("cc:dd")];
        let (held_once, held_serials) =
            reconcile_poll_with_hold(&prev, vec![pad("aa:bb")], &mut misses);
        assert_eq!(held_serials, vec!["cc:dd".to_string()]);
        assert_eq!(held_once.len(), 2);

        let (dropped, held_again) =
            reconcile_poll_with_hold(&held_once, vec![pad("aa:bb")], &mut misses);
        assert!(held_again.is_empty());
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].serial, "aa:bb");
    }
}
