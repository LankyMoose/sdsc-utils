//! Connect, low-battery, and charge-complete edge detection for overlay toasts.
//!
//! Low battery fires only after an observed bucket moves from above the
//! threshold into the low window. A pad that appears already low does not.
//! Connect toasts skip serials queued as already present at process launch.

use crate::controller::model::{ControllerStatus, PowerState};
use crate::persist::prefs::Prefs;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default)]
struct SerialFlags {
    notified_low: bool,
    notified_complete: bool,
}

#[derive(Debug, Default)]
pub struct NotifyTracker {
    by_serial: HashMap<String, SerialFlags>,
    /// Serials already connected at process start — suppress one Connected toast.
    launch_quiet: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NotifyKind {
    Connect,
    Disconnect,
    Low,
    Charged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyEvent {
    pub heading: String,
    pub body: String,
    pub percent: Option<u8>,
    pub serial: String,
    pub state: PowerState,
}

impl NotifyTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark serials that were already present when the app started (no Connected toast).
    pub fn queue_launch_quiet(&mut self, serials: impl IntoIterator<Item = String>) {
        self.launch_quiet.extend(serials);
    }

    /// Collect overlay toasts for connect / low-battery / charge-complete transitions.
    pub fn evaluate(
        &mut self,
        previous: &[ControllerStatus],
        next: &[ControllerStatus],
        prefs: &Prefs,
        nickname: impl Fn(&str) -> Option<String>,
    ) -> Vec<NotifyEvent> {
        let mut events: Vec<_> = self
            .collect_events(previous, next, prefs)
            .into_iter()
            .map(|(controller, kind)| {
                format_event(controller, kind, nickname(&controller.serial).as_deref())
            })
            .collect();
        if prefs.notify_disconnect {
            events.extend(
                previous
                    .iter()
                    .filter(|controller| !next.iter().any(|next| next.serial == controller.serial))
                    .map(|controller| {
                        format_event(
                            controller,
                            NotifyKind::Disconnect,
                            nickname(&controller.serial).as_deref(),
                        )
                    }),
            );
        }
        events
    }

    fn collect_events<'a>(
        &mut self,
        previous: &[ControllerStatus],
        next: &'a [ControllerStatus],
        prefs: &Prefs,
    ) -> Vec<(&'a ControllerStatus, NotifyKind)> {
        let prev_by_serial: HashMap<&str, &ControllerStatus> =
            previous.iter().map(|c| (c.serial.as_str(), c)).collect();

        self.by_serial
            .retain(|serial, _| next.iter().any(|c| c.serial == *serial));

        let mut events = Vec::new();

        for controller in next {
            let flags = self.by_serial.entry(controller.serial.clone()).or_default();
            let prev = prev_by_serial.get(controller.serial.as_str()).copied();

            if prev.is_none() && prefs.notify_connect {
                if self.launch_quiet.remove(&controller.serial) {
                    diag_notify(format!(
                        "ui-diag: connect suppressed (present at launch) serial={}",
                        controller.serial
                    ));
                } else {
                    events.push((controller, NotifyKind::Connect));
                }
            }

            let threshold = prefs.low_battery_percent;
            if controller.is_low_battery(threshold) {
                // Require a prior window above the threshold. A pad that connects
                // (or is first seen) already inside the low window has not crossed.
                let crossed = bucket_crossed_down(prev, controller.percent, threshold);
                if crossed && !flags.notified_low && prefs.notify_low {
                    diag_notify(format!(
                        "ui-diag: low battery crossed serial={} from={}% to={}% threshold={threshold}",
                        controller.serial,
                        prev.map(|prior| prior.percent)
                            .unwrap_or(controller.percent),
                        controller.percent,
                    ));
                    events.push((controller, NotifyKind::Low));
                    flags.notified_low = true;
                } else if prev.is_none() && prefs.notify_low {
                    diag_notify(format!(
                        "ui-diag: low battery suppressed (appeared inside window) serial={} percent={} threshold={threshold}",
                        controller.serial, controller.percent,
                    ));
                }
            } else {
                flags.notified_low = false;
            }

            let was_complete = prev.is_some_and(|p| p.state == PowerState::Complete);
            if controller.state == PowerState::Complete {
                // Require a known prior state so plugging in an already-full pad stays quiet.
                if prev.is_some()
                    && !was_complete
                    && !flags.notified_complete
                    && prefs.notify_charged
                {
                    events.push((controller, NotifyKind::Charged));
                    flags.notified_complete = true;
                }
            } else {
                flags.notified_complete = false;
            }
        }

        events
    }
}

/// The prior snapshot's battery window was above `threshold` and this snapshot
/// is at or below it. Appearing already inside the low window is not a crossing.
fn bucket_crossed_down(prev: Option<&ControllerStatus>, next_percent: u8, threshold: u8) -> bool {
    prev.is_some_and(|prior| prior.percent > threshold) && next_percent <= threshold
}

fn diag_notify(message: impl AsRef<str>) {
    #[cfg(debug_assertions)]
    crate::platform::app_log::info(message);
    #[cfg(not(debug_assertions))]
    let _ = message;
}

fn format_event(
    controller: &ControllerStatus,
    kind: NotifyKind,
    nickname: Option<&str>,
) -> NotifyEvent {
    let name = nickname
        .filter(|value| !value.is_empty())
        .unwrap_or(controller.product.as_str());
    let heading = format!("{name} ({})", controller.connection);
    match kind {
        NotifyKind::Connect => NotifyEvent {
            heading,
            body: "Connected".to_string(),
            percent: Some(controller.percent),
            serial: controller.serial.clone(),
            state: controller.state,
        },
        NotifyKind::Disconnect => NotifyEvent {
            heading,
            body: "Disconnected".to_string(),
            percent: Some(controller.percent),
            serial: controller.serial.clone(),
            state: controller.state,
        },
        NotifyKind::Low => NotifyEvent {
            heading,
            body: "Is low".to_string(),
            percent: Some(controller.percent),
            serial: controller.serial.clone(),
            state: controller.state,
        },
        NotifyKind::Charged => NotifyEvent {
            heading,
            body: "Finished charging".to_string(),
            percent: Some(100),
            serial: controller.serial.clone(),
            state: PowerState::Complete,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::model::PowerState;

    fn pad(
        serial: &str,
        percent: u8,
        state: PowerState,
        connection: crate::controller::model::Connection,
    ) -> ControllerStatus {
        crate::controller::dualsense::battery::dualsense_status(
            1,
            "DualSense",
            connection,
            serial.to_string(),
            percent,
            state,
        )
    }

    fn prefs(notify_low: bool, notify_charged: bool, notify_connect: bool) -> Prefs {
        Prefs {
            notify_low,
            notify_charged,
            notify_connect,
            notify_disconnect: true,
            ..Prefs::default()
        }
    }

    #[test]
    fn connects_notifies_with_percent() {
        let mut tracker = NotifyTracker::new();
        let p = prefs(true, true, true);
        let connected = vec![pad(
            "a",
            65,
            PowerState::Discharging,
            crate::controller::model::Connection::Bluetooth,
        )];

        let events = tracker.collect_events(&[], &connected, &p);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1, NotifyKind::Connect);
        assert_eq!(events[0].0.percent, 65);

        assert!(
            tracker
                .collect_events(&connected, &connected, &p)
                .is_empty()
        );
    }

    #[test]
    fn reconnect_after_disconnect_notifies_again() {
        let mut tracker = NotifyTracker::new();
        let p = prefs(true, true, true);
        let connected = vec![pad(
            "a",
            40,
            PowerState::Discharging,
            crate::controller::model::Connection::Usb,
        )];

        assert_eq!(tracker.collect_events(&[], &connected, &p).len(), 1);
        assert!(tracker.collect_events(&connected, &[], &p).is_empty());
        assert_eq!(tracker.collect_events(&[], &connected, &p).len(), 1);
    }

    #[test]
    fn launch_quiet_skips_connect_for_queued_serial_only() {
        let mut tracker = NotifyTracker::new();
        tracker.queue_launch_quiet(["a".to_string()]);
        let p = prefs(true, true, true);
        let connected = vec![
            pad(
                "a",
                40,
                PowerState::Discharging,
                crate::controller::model::Connection::Usb,
            ),
            pad(
                "b",
                85,
                PowerState::Discharging,
                crate::controller::model::Connection::Bluetooth,
            ),
        ];

        let events = tracker.collect_events(&[], &connected, &p);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0.serial, "b");
        assert_eq!(events[0].1, NotifyKind::Connect);
    }

    #[test]
    fn launch_quiet_reconnect_after_disconnect_notifies() {
        let mut tracker = NotifyTracker::new();
        tracker.queue_launch_quiet(["a".to_string()]);
        let p = prefs(true, true, true);
        let connected = vec![pad(
            "a",
            40,
            PowerState::Discharging,
            crate::controller::model::Connection::Usb,
        )];

        assert!(tracker.collect_events(&[], &connected, &p).is_empty());
        assert!(tracker.collect_events(&connected, &[], &p).is_empty());
        assert_eq!(tracker.collect_events(&[], &connected, &p).len(), 1);
    }

    #[test]
    fn launch_quiet_disconnect_still_notifies() {
        let mut tracker = NotifyTracker::new();
        tracker.queue_launch_quiet(["a".to_string()]);
        let connected = vec![pad(
            "a",
            40,
            PowerState::Discharging,
            crate::controller::model::Connection::Bluetooth,
        )];
        assert!(
            tracker
                .evaluate(&[], &connected, &prefs(true, true, true), |_| None)
                .is_empty()
        );
        let events = tracker.evaluate(&connected, &[], &prefs(true, true, false), |_| None);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].body, "Disconnected");
    }

    #[test]
    fn disconnect_notifies_with_last_known_battery() {
        let mut tracker = NotifyTracker::new();
        let connected = vec![pad(
            "a",
            40,
            PowerState::Discharging,
            crate::controller::model::Connection::Bluetooth,
        )];
        let events = tracker.evaluate(&connected, &[], &prefs(true, true, false), |_| None);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].body, "Disconnected");
        assert_eq!(events[0].percent, Some(40));
        assert_eq!(events[0].heading, "DualSense (Bluetooth)");
    }

    #[test]
    fn toast_heading_uses_nickname_when_set() {
        let mut tracker = NotifyTracker::new();
        let connected = vec![pad(
            "a",
            40,
            PowerState::Discharging,
            crate::controller::model::Connection::Usb,
        )];
        let events = tracker.evaluate(&[], &connected, &prefs(true, true, true), |serial| {
            (serial == "a").then(|| "Left pad".to_string())
        });
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].heading, "Left pad (USB)");
    }

    #[test]
    fn disconnect_notifications_can_be_disabled() {
        let mut tracker = NotifyTracker::new();
        let connected = vec![pad(
            "a",
            40,
            PowerState::Discharging,
            crate::controller::model::Connection::Bluetooth,
        )];
        let mut preferences = prefs(true, true, false);
        preferences.notify_disconnect = false;
        assert!(
            tracker
                .evaluate(&connected, &[], &preferences, |_| None)
                .is_empty()
        );
    }

    #[test]
    fn connect_prefs_can_disable() {
        let mut tracker = NotifyTracker::new();
        let p = prefs(true, true, false);
        let connected = vec![pad(
            "a",
            65,
            PowerState::Discharging,
            crate::controller::model::Connection::Usb,
        )];

        assert!(tracker.collect_events(&[], &connected, &p).is_empty());
    }

    #[test]
    fn connect_already_low_is_only_a_connect() {
        let mut tracker = NotifyTracker::new();
        let p = prefs(true, true, true);
        let low = vec![pad(
            "a",
            5,
            PowerState::Discharging,
            crate::controller::model::Connection::Bluetooth,
        )];

        let events = tracker.evaluate(&[], &low, &p, |_| None);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].body, "Connected");
        assert_eq!(events[0].percent, Some(5));
        assert!(!tracker.by_serial["a"].notified_low);

        assert!(tracker.evaluate(&low, &low, &p, |_| None).is_empty());
    }

    #[test]
    fn enters_low_once() {
        let mut tracker = NotifyTracker::new();
        let p = prefs(true, true, false);
        let mid = vec![pad(
            "a",
            50,
            PowerState::Discharging,
            crate::controller::model::Connection::Usb,
        )];
        let low = vec![pad(
            "a",
            5,
            PowerState::Discharging,
            crate::controller::model::Connection::Usb,
        )];

        assert!(tracker.collect_events(&[], &mid, &p).is_empty());
        assert!(!tracker.by_serial["a"].notified_low);

        let events = tracker.collect_events(&mid, &low, &p);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1, NotifyKind::Low);
        assert!(tracker.by_serial["a"].notified_low);

        assert!(tracker.collect_events(&low, &low, &p).is_empty());
    }

    #[test]
    fn low_threshold_is_configurable() {
        let mut tracker = NotifyTracker::new();
        let mut p = prefs(true, true, false);
        p.low_battery_percent = 25;
        let mid = vec![pad(
            "a",
            35,
            PowerState::Discharging,
            crate::controller::model::Connection::Usb,
        )];
        let at_threshold = vec![pad(
            "a",
            25,
            PowerState::Discharging,
            crate::controller::model::Connection::Usb,
        )];

        assert!(tracker.collect_events(&[], &mid, &p).is_empty());
        let events = tracker.collect_events(&mid, &at_threshold, &p);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1, NotifyKind::Low);
        assert!(
            tracker
                .collect_events(&at_threshold, &at_threshold, &p)
                .is_empty()
        );
    }

    #[test]
    fn above_custom_threshold_is_not_low() {
        let mut tracker = NotifyTracker::new();
        let mut p = prefs(true, true, false);
        p.low_battery_percent = 25;
        let mid = vec![pad(
            "a",
            50,
            PowerState::Discharging,
            crate::controller::model::Connection::Usb,
        )];
        let still_ok = vec![pad(
            "a",
            35,
            PowerState::Discharging,
            crate::controller::model::Connection::Usb,
        )];

        assert!(tracker.collect_events(&mid, &still_ok, &p).is_empty());
        assert!(!tracker.by_serial["a"].notified_low);
    }

    #[test]
    fn leave_low_allows_reentry() {
        let mut tracker = NotifyTracker::new();
        let p = prefs(true, true, false);
        let low = vec![pad(
            "a",
            5,
            PowerState::Discharging,
            crate::controller::model::Connection::Bluetooth,
        )];
        let mid = vec![pad(
            "a",
            50,
            PowerState::Discharging,
            crate::controller::model::Connection::Bluetooth,
        )];

        assert!(tracker.collect_events(&[], &low, &p).is_empty());
        assert!(!tracker.by_serial["a"].notified_low);
        assert!(tracker.collect_events(&low, &mid, &p).is_empty());
        assert!(!tracker.by_serial["a"].notified_low);
        let events = tracker.collect_events(&mid, &low, &p);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1, NotifyKind::Low);
    }

    #[test]
    fn same_window_unplug_is_not_a_low_crossing() {
        let mut tracker = NotifyTracker::new();
        let p = prefs(true, true, false);
        let charging = vec![pad(
            "a",
            5,
            PowerState::Charging,
            crate::controller::model::Connection::Usb,
        )];
        let discharging = vec![pad(
            "a",
            5,
            PowerState::Discharging,
            crate::controller::model::Connection::Usb,
        )];

        assert!(tracker.collect_events(&[], &charging, &p).is_empty());
        assert!(
            tracker
                .collect_events(&charging, &discharging, &p)
                .is_empty()
        );
        assert!(!tracker.by_serial["a"].notified_low);
    }

    #[test]
    fn higher_window_then_low_while_unplugging_notifies() {
        let mut tracker = NotifyTracker::new();
        let p = prefs(true, true, false);
        let charging = vec![pad(
            "a",
            15,
            PowerState::Charging,
            crate::controller::model::Connection::Usb,
        )];
        let low = vec![pad(
            "a",
            5,
            PowerState::Discharging,
            crate::controller::model::Connection::Usb,
        )];

        assert!(tracker.collect_events(&[], &charging, &p).is_empty());
        let events = tracker.collect_events(&charging, &low, &p);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1, NotifyKind::Low);
    }

    #[test]
    fn drop_inside_an_already_low_window_stays_quiet() {
        let mut tracker = NotifyTracker::new();
        let mut p = prefs(true, true, false);
        p.low_battery_percent = 25;
        let at_threshold = vec![pad(
            "a",
            25,
            PowerState::Discharging,
            crate::controller::model::Connection::Usb,
        )];
        let lower = vec![pad(
            "a",
            5,
            PowerState::Discharging,
            crate::controller::model::Connection::Usb,
        )];

        assert!(tracker.collect_events(&[], &at_threshold, &p).is_empty());
        assert!(tracker.collect_events(&at_threshold, &lower, &p).is_empty());
        assert!(!tracker.by_serial["a"].notified_low);
    }

    #[test]
    fn charging_to_complete_notifies() {
        let mut tracker = NotifyTracker::new();
        let p = prefs(true, true, false);
        let charging = vec![pad(
            "a",
            80,
            PowerState::Charging,
            crate::controller::model::Connection::Usb,
        )];
        let complete = vec![pad(
            "a",
            100,
            PowerState::Complete,
            crate::controller::model::Connection::Usb,
        )];

        assert!(tracker.collect_events(&[], &charging, &p).is_empty());
        let events = tracker.collect_events(&charging, &complete, &p);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1, NotifyKind::Charged);
        assert!(tracker.by_serial["a"].notified_complete);
        assert!(tracker.collect_events(&complete, &complete, &p).is_empty());
    }

    #[test]
    fn already_complete_on_connect_is_quiet() {
        let mut tracker = NotifyTracker::new();
        let p = prefs(true, true, false);
        let complete = vec![pad(
            "a",
            100,
            PowerState::Complete,
            crate::controller::model::Connection::Usb,
        )];

        assert!(tracker.collect_events(&[], &complete, &p).is_empty());
        assert!(!tracker.by_serial["a"].notified_complete);
    }

    #[test]
    fn prefs_can_disable() {
        let mut tracker = NotifyTracker::new();
        let p = prefs(false, false, false);
        let mid = vec![pad(
            "a",
            50,
            PowerState::Discharging,
            crate::controller::model::Connection::Usb,
        )];
        let low = vec![pad(
            "a",
            5,
            PowerState::Discharging,
            crate::controller::model::Connection::Usb,
        )];
        let charging = vec![pad(
            "a",
            80,
            PowerState::Charging,
            crate::controller::model::Connection::Usb,
        )];
        let complete = vec![pad(
            "a",
            100,
            PowerState::Complete,
            crate::controller::model::Connection::Usb,
        )];

        assert!(tracker.collect_events(&mid, &low, &p).is_empty());
        assert!(!tracker.by_serial["a"].notified_low);
        assert!(tracker.collect_events(&charging, &complete, &p).is_empty());
        assert!(!tracker.by_serial["a"].notified_complete);
    }

    #[test]
    fn multi_controller_independent() {
        let mut tracker = NotifyTracker::new();
        let p = prefs(true, true, false);
        let prev = vec![
            pad(
                "a",
                50,
                PowerState::Discharging,
                crate::controller::model::Connection::Usb,
            ),
            pad(
                "b",
                80,
                PowerState::Charging,
                crate::controller::model::Connection::Bluetooth,
            ),
        ];
        let next = vec![
            pad(
                "a",
                5,
                PowerState::Discharging,
                crate::controller::model::Connection::Usb,
            ),
            pad(
                "b",
                100,
                PowerState::Complete,
                crate::controller::model::Connection::Bluetooth,
            ),
        ];

        let events = tracker.collect_events(&prev, &next, &p);
        assert_eq!(events.len(), 2);
        assert!(tracker.by_serial["a"].notified_low);
        assert!(tracker.by_serial["b"].notified_complete);
    }

    #[test]
    fn disconnect_clears_flags() {
        let mut tracker = NotifyTracker::new();
        let p = prefs(true, true, false);
        let low = vec![pad(
            "a",
            5,
            PowerState::Discharging,
            crate::controller::model::Connection::Usb,
        )];

        let _ = tracker.collect_events(&[], &low, &p);
        assert!(tracker.by_serial.contains_key("a"));

        assert!(tracker.collect_events(&low, &[], &p).is_empty());
        assert!(!tracker.by_serial.contains_key("a"));
    }
}
