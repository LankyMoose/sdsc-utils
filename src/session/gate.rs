//! Pure start-open / connect-cooldown / poll-hold helpers.

use crate::controller::dualsense::identity::normalize_identity;
use crate::controller::model::ControllerStatus;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// How long a pad missing from polls stays held before the session drops it.
///
/// Wall-time (not miss-count) so the ~16ms service hot path tolerates the same
/// radio stalls as the 5s cold poll: a ~200ms input silence never reaches the
/// session, while a genuinely gone pad still drops on the next cold poll.
pub const POLL_MISS_HOLD: Duration = Duration::from_secs(2);

/// Whether any pad counts toward start-screen auto-open / auto-close.
///
/// When `include_usb` is false, only Bluetooth pads qualify.
pub fn has_start_presence(controllers: &[ControllerStatus], include_usb: bool) -> bool {
    controllers.iter().any(|c| {
        if include_usb {
            true
        } else {
            c.connection.is_bluetooth()
        }
    })
}

/// Gate for automatic start-screen open on a qualifying 0→1 connect.
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

/// Close Start when qualifying presence drops 1→0 while the window is visible.
pub fn should_auto_close_start(
    prev_present: bool,
    next_present: bool,
    start_visible: bool,
) -> bool {
    prev_present && !next_present && start_visible
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

/// Arm skip-connect-cooldown only when powering off the sole qualifying controller.
///
/// Powering off one of several qualifying pads must not skip the ghost-flap cooldown
/// that guards a later 0→1 open for a sibling that never left. Non-qualifying pads
/// (USB when `include_usb` is false) are ignored.
pub fn should_skip_connect_cooldown_on_power_off(
    controllers: &[ControllerStatus],
    serial: &str,
    include_usb: bool,
) -> bool {
    let target = normalize_identity(serial);
    let qualifying: Vec<&ControllerStatus> = controllers
        .iter()
        .filter(|c| include_usb || c.connection.is_bluetooth())
        .collect();
    qualifying.len() == 1
        && qualifying
            .first()
            .is_some_and(|c| normalize_identity(&c.serial) == target)
}

/// Whether to arm the ghost-flap cooldown after qualifying presence goes empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectCooldownDecision {
    SkipIntentional,
    SkipShortArrival,
    Arm,
}

/// Decide cooldown after qualifying 1→0. Short arrival blips must not suppress the next open.
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

/// Hold pads missing from polls for [`POLL_MISS_HOLD`] wall time, then drop.
///
/// Returns the reconciled list and serials held this tick (for diagnostics).
pub fn reconcile_poll_with_hold(
    previous: &[ControllerStatus],
    polled: Vec<ControllerStatus>,
    miss_since: &mut HashMap<String, Instant>,
    now: Instant,
) -> (Vec<ControllerStatus>, Vec<String>) {
    let polled_serials: HashSet<String> = polled.iter().map(|c| c.serial.clone()).collect();

    // Pads that returned clear their miss streak.
    miss_since.retain(|serial, _| !polled_serials.contains(serial));

    let mut out = polled;
    let mut held = Vec::new();

    for prev in previous {
        if polled_serials.contains(&prev.serial) {
            continue;
        }
        let since = *miss_since.entry(prev.serial.clone()).or_insert(now);
        if now.saturating_duration_since(since) < POLL_MISS_HOLD {
            held.push(prev.serial.clone());
            out.push(prev.clone());
        } else {
            // Held long enough: accept the drop.
            miss_since.remove(&prev.serial);
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

    fn usb_pad(serial: &str) -> ControllerStatus {
        dualsense_status(
            1,
            "DualSense",
            Connection::Usb,
            serial.to_string(),
            50,
            PowerState::Charging,
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
    fn presence_includes_usb_when_allowed() {
        let usb = vec![usb_pad("aa:bb")];
        assert!(has_start_presence(&usb, true));
        assert!(!has_start_presence(&usb, false));
        assert!(has_start_presence(&[pad("aa:bb")], false));
        assert!(has_start_presence(&[usb_pad("aa:bb"), pad("cc:dd")], false));
        assert!(!has_start_presence(&[], false));
    }

    #[test]
    fn usb_off_usb_only_connect_does_not_open() {
        let prev = has_start_presence(&[], false);
        let next = has_start_presence(&[usb_pad("aa:bb")], false);
        assert!(!should_auto_open_start(
            true, !prev, next, false, false, false
        ));
    }

    #[test]
    fn usb_off_bluetooth_connect_while_usb_present_opens() {
        let prev = has_start_presence(&[usb_pad("aa:bb")], false);
        let next = has_start_presence(&[usb_pad("aa:bb"), pad("cc:dd")], false);
        assert!(!prev);
        assert!(next);
        assert!(should_auto_open_start(
            true, !prev, next, false, false, false
        ));
    }

    #[test]
    fn usb_off_last_bluetooth_leave_closes_with_usb_remaining() {
        let prev = has_start_presence(&[usb_pad("aa:bb"), pad("cc:dd")], false);
        let next = has_start_presence(&[usb_pad("aa:bb")], false);
        assert!(should_auto_close_start(prev, next, true));
        assert!(!should_auto_close_start(prev, next, false));
    }

    #[test]
    fn usb_off_usb_leave_while_bluetooth_remains_does_not_close() {
        let prev = has_start_presence(&[usb_pad("aa:bb"), pad("cc:dd")], false);
        let next = has_start_presence(&[pad("cc:dd")], false);
        assert!(!should_auto_close_start(prev, next, true));
    }

    #[test]
    fn usb_on_presence_matches_raw_list() {
        let usb = vec![usb_pad("aa:bb")];
        assert!(has_start_presence(&usb, true));
        assert!(should_auto_open_start(
            true,
            !has_start_presence(&[], true),
            has_start_presence(&usb, true),
            false,
            false,
            false
        ));
        assert!(should_auto_close_start(
            has_start_presence(&usb, true),
            has_start_presence(&[], true),
            true
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
    fn power_off_skips_cooldown_only_for_sole_qualifying_controller() {
        let alone = vec![pad("aa:bb")];
        assert!(should_skip_connect_cooldown_on_power_off(
            &alone, "AA-BB", true
        ));
        assert!(!should_skip_connect_cooldown_on_power_off(
            &alone, "cc:dd", true
        ));

        let two = vec![pad("aa:bb"), pad("cc:dd")];
        assert!(!should_skip_connect_cooldown_on_power_off(
            &two, "aa:bb", true
        ));
        assert!(!should_skip_connect_cooldown_on_power_off(
            &[],
            "aa:bb",
            true
        ));

        // USB ignored when include_usb is false: sole Bluetooth still qualifies.
        let usb_and_bt = vec![usb_pad("usb:1"), pad("aa:bb")];
        assert!(should_skip_connect_cooldown_on_power_off(
            &usb_and_bt,
            "aa:bb",
            false
        ));
        assert!(!should_skip_connect_cooldown_on_power_off(
            &usb_and_bt,
            "usb:1",
            false
        ));
        assert!(!should_skip_connect_cooldown_on_power_off(
            &usb_and_bt,
            "aa:bb",
            true
        ));
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
    fn hold_keeps_pad_until_hold_expires() {
        let mut misses = HashMap::new();
        let t0 = Instant::now();
        let prev = vec![pad("aa:bb"), pad("cc:dd")];
        // First miss starts the hold window.
        let (held_once, held_serials) =
            reconcile_poll_with_hold(&prev, vec![pad("aa:bb")], &mut misses, t0);
        assert_eq!(held_serials, vec!["cc:dd".to_string()]);
        assert_eq!(held_once.len(), 2);

        // Still missing inside the window: kept (hot path misses every ~16ms).
        let (held_twice, held_again) = reconcile_poll_with_hold(
            &held_once,
            vec![pad("aa:bb")],
            &mut misses,
            t0 + POLL_MISS_HOLD - Duration::from_millis(100),
        );
        assert_eq!(held_again, vec!["cc:dd".to_string()]);
        assert_eq!(held_twice.len(), 2);

        // Past the window: accepted drop (covers the 5s cold poll too).
        let (dropped, held_after) = reconcile_poll_with_hold(
            &held_twice,
            vec![pad("aa:bb")],
            &mut misses,
            t0 + POLL_MISS_HOLD + Duration::from_millis(100),
        );
        assert!(held_after.is_empty());
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].serial, "aa:bb");

        // Drop resets the streak: a later miss holds again from scratch.
        let both = vec![pad("aa:bb"), pad("cc:dd")];
        let t1 = t0 + POLL_MISS_HOLD + Duration::from_secs(1);
        let (reheld, reheld_serials) =
            reconcile_poll_with_hold(&both, vec![pad("aa:bb")], &mut misses, t1);
        assert_eq!(reheld_serials, vec!["cc:dd".to_string()]);
        assert_eq!(reheld.len(), 2);
    }
}
