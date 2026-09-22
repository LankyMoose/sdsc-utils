//! Connect, low-battery, and charge-complete edge detection for overlay toasts.

use crate::battery::{ControllerStatus, PowerState};
use crate::prefs::Prefs;
use std::collections::HashMap;

#[derive(Debug, Default)]
struct SerialFlags {
    notified_low: bool,
    notified_complete: bool,
}

#[derive(Debug, Default)]
pub struct NotifyTracker {
    by_serial: HashMap<String, SerialFlags>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NotifyKind {
    Connect,
    Disconnect,
    Low,
    Charged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
                events.push((controller, NotifyKind::Connect));
            }

            let was_low = prev.is_some_and(|p| p.is_low_battery(prefs.low_battery_percent));
            if controller.is_low_battery(prefs.low_battery_percent) {
                if !was_low && !flags.notified_low && prefs.notify_low {
                    events.push((controller, NotifyKind::Low));
                    flags.notified_low = true;
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

fn format_event(
    controller: &ControllerStatus,
    kind: NotifyKind,
    nickname: Option<&str>,
) -> NotifyEvent {
    let name = nickname
        .filter(|value| !value.is_empty())
        .unwrap_or(controller.product);
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
    use crate::battery::PowerState;

    fn pad(
        serial: &str,
        percent: u8,
        state: PowerState,
        connection: &'static str,
    ) -> ControllerStatus {
        ControllerStatus {
            index: 1,
            product: "DualSense",
            connection,
            serial: serial.to_string(),
            percent,
            state,
        }
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
        let connected = vec![pad("a", 65, PowerState::Discharging, "Bluetooth")];

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
        let connected = vec![pad("a", 40, PowerState::Discharging, "USB")];

        assert_eq!(tracker.collect_events(&[], &connected, &p).len(), 1);
        assert!(tracker.collect_events(&connected, &[], &p).is_empty());
        assert_eq!(tracker.collect_events(&[], &connected, &p).len(), 1);
    }

    #[test]
    fn disconnect_notifies_with_last_known_battery() {
        let mut tracker = NotifyTracker::new();
        let connected = vec![pad("a", 40, PowerState::Discharging, "Bluetooth")];
        let events = tracker.evaluate(&connected, &[], &prefs(true, true, false), |_| None);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].body, "Disconnected");
        assert_eq!(events[0].percent, Some(40));
        assert_eq!(events[0].heading, "DualSense (Bluetooth)");
    }

    #[test]
    fn toast_heading_uses_nickname_when_set() {
        let mut tracker = NotifyTracker::new();
        let connected = vec![pad("a", 40, PowerState::Discharging, "USB")];
        let events = tracker.evaluate(&[], &connected, &prefs(true, true, true), |serial| {
            (serial == "a").then(|| "Left pad".to_string())
        });
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].heading, "Left pad (USB)");
    }

    #[test]
    fn disconnect_notifications_can_be_disabled() {
        let mut tracker = NotifyTracker::new();
        let connected = vec![pad("a", 40, PowerState::Discharging, "Bluetooth")];
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
        let connected = vec![pad("a", 65, PowerState::Discharging, "USB")];

        assert!(tracker.collect_events(&[], &connected, &p).is_empty());
    }

    #[test]
    fn connect_and_low_can_both_fire() {
        let mut tracker = NotifyTracker::new();
        let p = prefs(true, true, true);
        let low = vec![pad("a", 5, PowerState::Discharging, "USB")];

        let events = tracker.collect_events(&[], &low, &p);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].1, NotifyKind::Connect);
        assert_eq!(events[1].1, NotifyKind::Low);
    }

    #[test]
    fn enters_low_once() {
        let mut tracker = NotifyTracker::new();
        let p = prefs(true, true, false);
        let mid = vec![pad("a", 50, PowerState::Discharging, "USB")];
        let low = vec![pad("a", 5, PowerState::Discharging, "USB")];

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
        let mid = vec![pad("a", 35, PowerState::Discharging, "USB")];
        let at_threshold = vec![pad("a", 25, PowerState::Discharging, "USB")];

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
        let mid = vec![pad("a", 50, PowerState::Discharging, "USB")];
        let still_ok = vec![pad("a", 35, PowerState::Discharging, "USB")];

        assert!(tracker.collect_events(&mid, &still_ok, &p).is_empty());
        assert!(!tracker.by_serial["a"].notified_low);
    }

    #[test]
    fn leave_low_allows_reentry() {
        let mut tracker = NotifyTracker::new();
        let p = prefs(true, true, false);
        let low = vec![pad("a", 5, PowerState::Discharging, "BT")];
        let mid = vec![pad("a", 50, PowerState::Discharging, "BT")];

        assert_eq!(tracker.collect_events(&[], &low, &p).len(), 1);
        assert!(tracker.collect_events(&low, &mid, &p).is_empty());
        assert!(!tracker.by_serial["a"].notified_low);
        assert_eq!(tracker.collect_events(&mid, &low, &p).len(), 1);
    }

    #[test]
    fn charging_to_complete_notifies() {
        let mut tracker = NotifyTracker::new();
        let p = prefs(true, true, false);
        let charging = vec![pad("a", 80, PowerState::Charging, "USB")];
        let complete = vec![pad("a", 100, PowerState::Complete, "USB")];

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
        let complete = vec![pad("a", 100, PowerState::Complete, "USB")];

        assert!(tracker.collect_events(&[], &complete, &p).is_empty());
        assert!(!tracker.by_serial["a"].notified_complete);
    }

    #[test]
    fn prefs_can_disable() {
        let mut tracker = NotifyTracker::new();
        let p = prefs(false, false, false);
        let mid = vec![pad("a", 50, PowerState::Discharging, "USB")];
        let low = vec![pad("a", 5, PowerState::Discharging, "USB")];
        let charging = vec![pad("a", 80, PowerState::Charging, "USB")];
        let complete = vec![pad("a", 100, PowerState::Complete, "USB")];

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
            pad("a", 50, PowerState::Discharging, "USB"),
            pad("b", 80, PowerState::Charging, "Bluetooth"),
        ];
        let next = vec![
            pad("a", 5, PowerState::Discharging, "USB"),
            pad("b", 100, PowerState::Complete, "Bluetooth"),
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
        let low = vec![pad("a", 5, PowerState::Discharging, "USB")];

        let _ = tracker.collect_events(&[], &low, &p);
        assert!(tracker.by_serial.contains_key("a"));

        assert!(tracker.collect_events(&low, &[], &p).is_empty());
        assert!(!tracker.by_serial.contains_key("a"));
    }
}
