//! Overlay toast message model (presented by the iced daemon via [`view`]).

pub mod machine;
pub mod view;

use crate::persist::notify::NotifyEvent;
use crate::ui::color::{BatterySpectrum, Rgb};
use crate::ui::toast::machine::AfterToast;
use std::collections::VecDeque;

/// Right-hand content of a toast card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToastTrailing {
    Percent { percent: u8, eta: Option<String> },
    Bug,
}

#[derive(Debug, Clone)]
pub struct ToastMessage {
    pub heading: String,
    pub body: String,
    /// Owning controller serial (empty for preview / crash cards). Coalescing key.
    pub serial: String,
    pub accent: Rgb,
    pub trailing: ToastTrailing,
    /// Latched at queue time (0→1 Connected → open Start after rest).
    pub after: AfterToast,
}

impl ToastMessage {
    /// Queue a toast, usurping any queued same-controller same-type card.
    ///
    /// A flapping pad otherwise queues `Connected` faster than cards can drain.
    /// Same `(serial, body)` keeps only the newest (fresh percent/eta wins);
    /// a usurped `AfterToast::OpenStart` transfers to the replacement so the
    /// compact auto-open latch can never be dropped by coalescing. Cards
    /// without a serial (preview / crash) never coalesce.
    pub fn enqueue(queue: &mut VecDeque<ToastMessage>, mut message: ToastMessage) {
        if !message.serial.is_empty() {
            let mut latched = false;
            queue.retain(|queued| {
                let same = queued.serial == message.serial && queued.body == message.body;
                if same && queued.after == AfterToast::OpenStart {
                    latched = true;
                }
                !same
            });
            if latched && message.after == AfterToast::Nothing {
                message.after = AfterToast::OpenStart;
            }
        }
        queue.push_back(message);
    }
    pub fn from_notification(
        event: NotifyEvent,
        spectrum: BatterySpectrum,
        eta: Option<String>,
    ) -> Self {
        let percent = event.percent.unwrap_or(100).min(100);
        Self {
            heading: event.heading,
            body: event.body,
            serial: event.serial,
            accent: spectrum.color_at_percent(percent),
            trailing: ToastTrailing::Percent { percent, eta },
            after: AfterToast::Nothing,
        }
    }

    pub fn preview(spectrum: &BatterySpectrum) -> Self {
        const PERCENT: u8 = 70;
        Self {
            heading: crate::platform::app_meta::DISPLAY_NAME.to_string(),
            body: "Toasts will appear here".to_string(),
            serial: String::new(),
            accent: spectrum.color_at_percent(PERCENT),
            trailing: ToastTrailing::Percent {
                percent: PERCENT,
                eta: Some("~3h 30m".to_string()),
            },
            after: AfterToast::Nothing,
        }
    }

    /// Shown once after a crash auto-relaunch (or `--test-crash-toast`).
    pub fn crash_restart() -> Self {
        // Matches `theme::ACCENT` (#4141FB).
        const ACCENT: Rgb = Rgb::new(0x41, 0x41, 0xFB);
        Self {
            heading: format!(
                "{} crashed and restarted",
                crate::platform::app_meta::DISPLAY_NAME
            ),
            body: "Please send app.log from the data directory to the developers.".to_string(),
            serial: String::new(),
            accent: ACCENT,
            trailing: ToastTrailing::Bug,
            after: AfterToast::Nothing,
        }
    }

    pub fn percent(&self) -> u8 {
        match &self.trailing {
            ToastTrailing::Percent { percent, .. } => *percent,
            ToastTrailing::Bug => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::model::{ControllerStatus, PowerState};
    use crate::persist::notify::NotifyTracker;
    use crate::persist::prefs::Prefs;

    fn pad(serial: &str, percent: u8) -> ControllerStatus {
        crate::controller::dualsense::battery::dualsense_status(
            1,
            "DualSense",
            crate::controller::model::Connection::Usb,
            serial.to_string(),
            percent,
            PowerState::Discharging,
        )
    }

    #[test]
    fn from_notification_uses_spectrum_accent_for_percent() {
        let mut tracker = NotifyTracker::new();
        let connected = vec![pad("a", 40)];
        let events = tracker.evaluate(&[], &connected, &Prefs::default(), |_| None);
        assert_eq!(events.len(), 1);
        let message =
            ToastMessage::from_notification(events[0].clone(), BatterySpectrum::default(), None);
        assert!(message.heading.contains("DualSense"));
        assert_eq!(message.percent(), 40);
        assert_eq!(
            message.accent,
            BatterySpectrum::default().color_at_percent(40)
        );
        assert!(matches!(
            message.trailing,
            ToastTrailing::Percent {
                percent: 40,
                eta: None
            }
        ));
    }

    #[test]
    fn queued_connects_keep_distinct_percents() {
        let mut tracker = NotifyTracker::new();
        let connected = vec![
            pad("a", 40),
            crate::controller::dualsense::battery::dualsense_status(
                2,
                "DualSense",
                crate::controller::model::Connection::Bluetooth,
                "b".to_string(),
                85,
                PowerState::Discharging,
            ),
        ];
        let events = tracker.evaluate(&[], &connected, &Prefs::default(), |_| None);
        assert_eq!(events.len(), 2);
        let messages: Vec<_> = events
            .into_iter()
            .map(|event| ToastMessage::from_notification(event, BatterySpectrum::default(), None))
            .collect();
        assert_eq!(messages[0].percent(), 40);
        assert_eq!(messages[1].percent(), 85);
        assert_ne!(messages[0].heading, messages[1].heading);
    }

    #[test]
    fn crash_restart_uses_bug_trailing() {
        let message = ToastMessage::crash_restart();
        assert!(matches!(message.trailing, ToastTrailing::Bug));
        assert!(message.heading.contains("crashed and restarted"));
        assert!(message.body.contains("app.log"));
    }

    fn notify(serial: &str, body: &str, percent: u8) -> NotifyEvent {
        NotifyEvent {
            heading: format!("DualSense ({serial})"),
            body: body.to_string(),
            percent: Some(percent),
            serial: serial.to_string(),
            state: PowerState::Discharging,
        }
    }

    fn queued(serial: &str, body: &str, percent: u8) -> ToastMessage {
        ToastMessage::from_notification(
            notify(serial, body, percent),
            BatterySpectrum::default(),
            None,
        )
    }

    #[test]
    fn enqueue_usurps_same_serial_and_body_with_newest() {
        let mut queue = std::collections::VecDeque::new();
        ToastMessage::enqueue(&mut queue, queued("aa", "Connected", 15));
        ToastMessage::enqueue(&mut queue, queued("aa", "Connected", 25));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].percent(), 25);
    }

    #[test]
    fn enqueue_keeps_different_body_or_serial() {
        let mut queue = std::collections::VecDeque::new();
        ToastMessage::enqueue(&mut queue, queued("aa", "Connected", 15));
        ToastMessage::enqueue(&mut queue, queued("aa", "Disconnected", 15));
        ToastMessage::enqueue(&mut queue, queued("bb", "Connected", 80));
        assert_eq!(queue.len(), 3);
    }

    #[test]
    fn enqueue_without_serial_never_coalesces() {
        let mut queue = std::collections::VecDeque::new();
        ToastMessage::enqueue(&mut queue, ToastMessage::crash_restart());
        ToastMessage::enqueue(&mut queue, ToastMessage::crash_restart());
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn enqueue_transfers_open_start_latch_to_replacement() {
        let mut queue = std::collections::VecDeque::new();
        let mut latched = queued("aa", "Connected", 15);
        latched.after = AfterToast::OpenStart;
        ToastMessage::enqueue(&mut queue, latched);
        // Unlatched duplicate usurps but must not drop the Start latch.
        ToastMessage::enqueue(&mut queue, queued("aa", "Connected", 25));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].after, AfterToast::OpenStart);
        assert_eq!(queue[0].percent(), 25);
    }
}
