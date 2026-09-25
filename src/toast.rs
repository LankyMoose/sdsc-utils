//! Overlay toast message model (presented by the iced daemon via `toast_view`).

use crate::color::{BatterySpectrum, Rgb};
use crate::notify::NotifyEvent;

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
    pub accent: Rgb,
    pub trailing: ToastTrailing,
}

impl ToastMessage {
    pub fn from_notification(
        event: NotifyEvent,
        spectrum: BatterySpectrum,
        eta: Option<String>,
    ) -> Self {
        let percent = event.percent.unwrap_or(100).min(100);
        Self {
            heading: event.heading,
            body: event.body,
            accent: spectrum.color_at_percent(percent),
            trailing: ToastTrailing::Percent { percent, eta },
        }
    }

    pub fn preview(spectrum: &BatterySpectrum) -> Self {
        const PERCENT: u8 = 70;
        Self {
            heading: crate::app_meta::DISPLAY_NAME.to_string(),
            body: "Toasts will appear here".to_string(),
            accent: spectrum.color_at_percent(PERCENT),
            trailing: ToastTrailing::Percent {
                percent: PERCENT,
                eta: Some("~3h 30m".to_string()),
            },
        }
    }

    /// Shown once after a crash auto-relaunch (or `--test-crash-toast`).
    pub fn crash_restart() -> Self {
        // Matches `theme::ACCENT` (#4141FB).
        const ACCENT: Rgb = Rgb::new(0x41, 0x41, 0xFB);
        Self {
            heading: format!("{} crashed and restarted", crate::app_meta::DISPLAY_NAME),
            body: "Please send app.log from the data directory to the developers.".to_string(),
            accent: ACCENT,
            trailing: ToastTrailing::Bug,
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
    use crate::battery::{ControllerStatus, PowerState};
    use crate::notify::NotifyTracker;
    use crate::prefs::Prefs;

    fn pad(serial: &str, percent: u8) -> ControllerStatus {
        ControllerStatus {
            index: 1,
            product: "DualSense",
            connection: "USB",
            serial: serial.to_string(),
            percent,
            state: PowerState::Discharging,
        }
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
            ControllerStatus {
                index: 2,
                product: "DualSense",
                connection: "Bluetooth",
                serial: "b".to_string(),
                percent: 85,
                state: PowerState::Discharging,
            },
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
}
