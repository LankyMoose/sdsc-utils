//! Developer-only controller presets (feature `dev-emulate`).

use crate::controller::dualsense::battery::{LOW_BATTERY_PERCENT, dualsense_status};
use crate::controller::model::Connection;
use crate::controller::model::{ControllerStatus, PowerState};

pub const SERIAL_PREFIX: &str = "emu-";
pub const PRIMARY_SERIAL: &str = "emu-1";

pub fn is_emulated(serial: &str) -> bool {
    serial.starts_with(SERIAL_PREFIX)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    Discharging50,
    LowBattery,
    Charging,
    FullyCharged,
    /// Charging → Complete on successive applies when already charging.
    ChargeCompleteStep,
    TwoPads,
    Clear,
    // --- Battery analytics ---
    /// Enable analytics + seed typical charge/play samples; show a discharging pad.
    AnalyticsSeedEstimates,
    /// Start charging from ~empty (5%).
    AnalyticsPlugEmpty,
    /// Advance charge (+% and credit active time); finishes at Complete when near full.
    AnalyticsChargeAdvance,
    /// Leave Complete into discharging at 100%.
    AnalyticsUnplugFull,
    /// Advance drain (−% and credit active time); finishes at empty bucket.
    AnalyticsDrainAdvance,
    /// Disconnect (pause an open from-full discharge).
    AnalyticsPause,
    /// Reconnect still discharging (resume paused cycle).
    AnalyticsResume,
    /// Plug in while discharging (ends play cycle, starts charge).
    AnalyticsPlugMidDrain,
}

impl Preset {
    pub fn menu_label(self) -> &'static str {
        match self {
            Self::Discharging50 => "Emulate: discharging 50%",
            Self::LowBattery => "Emulate: low battery",
            Self::Charging => "Emulate: charging",
            Self::FullyCharged => "Emulate: fully charged",
            Self::ChargeCompleteStep => "Emulate: charge complete step",
            Self::TwoPads => "Emulate: two pads",
            Self::Clear => "Clear emulation",
            Self::AnalyticsSeedEstimates => "Analytics: seed estimates",
            Self::AnalyticsPlugEmpty => "Analytics: plug empty",
            Self::AnalyticsChargeAdvance => "Analytics: charge advance",
            Self::AnalyticsUnplugFull => "Analytics: unplug from full",
            Self::AnalyticsDrainAdvance => "Analytics: drain advance",
            Self::AnalyticsPause => "Analytics: pause (pad off)",
            Self::AnalyticsResume => "Analytics: resume",
            Self::AnalyticsPlugMidDrain => "Analytics: plug mid-drain",
        }
    }

    pub fn is_analytics(self) -> bool {
        matches!(
            self,
            Self::AnalyticsSeedEstimates
                | Self::AnalyticsPlugEmpty
                | Self::AnalyticsChargeAdvance
                | Self::AnalyticsUnplugFull
                | Self::AnalyticsDrainAdvance
                | Self::AnalyticsPause
                | Self::AnalyticsResume
                | Self::AnalyticsPlugMidDrain
        )
    }
}

fn one(
    index: usize,
    serial_suffix: &str,
    percent: u8,
    state: PowerState,
    connection: Connection,
) -> ControllerStatus {
    dualsense_status(
        index,
        "DualSense",
        connection,
        format!("{SERIAL_PREFIX}{serial_suffix}"),
        percent,
        state,
    )
}

fn primary(percent: u8, state: PowerState, connection: Connection) -> ControllerStatus {
    one(1, "1", percent, state, connection)
}

fn primary_from(current: &[ControllerStatus]) -> Option<&ControllerStatus> {
    current.iter().find(|c| c.serial == PRIMARY_SERIAL)
}

/// Apply a preset. `current` is the active emulated list (may be empty).
pub fn apply_preset(preset: Preset, current: &[ControllerStatus]) -> Vec<ControllerStatus> {
    match preset {
        Preset::Clear => Vec::new(),
        Preset::Discharging50 => {
            vec![primary(50, PowerState::Discharging, Connection::Usb)]
        }
        Preset::LowBattery => {
            vec![primary(
                LOW_BATTERY_PERCENT,
                PowerState::Discharging,
                Connection::Bluetooth,
            )]
        }
        Preset::Charging => {
            vec![primary(80, PowerState::Charging, Connection::Usb)]
        }
        Preset::FullyCharged => {
            vec![primary(100, PowerState::Complete, Connection::Usb)]
        }
        Preset::ChargeCompleteStep => {
            let charging = current.len() == 1
                && current[0].state == PowerState::Charging
                && is_emulated(&current[0].serial);
            if charging {
                vec![primary(100, PowerState::Complete, Connection::Usb)]
            } else {
                vec![primary(80, PowerState::Charging, Connection::Usb)]
            }
        }
        Preset::TwoPads => {
            vec![
                one(
                    1,
                    "1",
                    LOW_BATTERY_PERCENT,
                    PowerState::Discharging,
                    Connection::Bluetooth,
                ),
                one(2, "2", 80, PowerState::Charging, Connection::Usb),
            ]
        }
        Preset::AnalyticsSeedEstimates => {
            vec![primary(65, PowerState::Discharging, Connection::Bluetooth)]
        }
        Preset::AnalyticsPlugEmpty => {
            vec![primary(5, PowerState::Charging, Connection::Usb)]
        }
        Preset::AnalyticsChargeAdvance => {
            let Some(pad) = primary_from(current) else {
                return vec![primary(5, PowerState::Charging, Connection::Usb)];
            };
            if pad.state == PowerState::Charging {
                let next = (pad.percent.saturating_add(20)).min(100);
                if next >= 100 {
                    vec![primary(100, PowerState::Complete, Connection::Usb)]
                } else {
                    vec![primary(next, PowerState::Charging, Connection::Usb)]
                }
            } else if pad.state == PowerState::Complete {
                vec![primary(100, PowerState::Complete, Connection::Usb)]
            } else {
                vec![primary(5, PowerState::Charging, Connection::Usb)]
            }
        }
        Preset::AnalyticsUnplugFull => {
            vec![primary(100, PowerState::Discharging, Connection::Bluetooth)]
        }
        Preset::AnalyticsDrainAdvance => {
            let Some(pad) = primary_from(current) else {
                return vec![primary(100, PowerState::Discharging, Connection::Bluetooth)];
            };
            if pad.state.is_discharging() {
                let next = pad.percent.saturating_sub(20);
                let next = if next < LOW_BATTERY_PERCENT {
                    LOW_BATTERY_PERCENT
                } else {
                    next
                };
                vec![primary(
                    next,
                    PowerState::Discharging,
                    Connection::Bluetooth,
                )]
            } else {
                vec![primary(100, PowerState::Discharging, Connection::Bluetooth)]
            }
        }
        Preset::AnalyticsPause => Vec::new(),
        Preset::AnalyticsResume => {
            let percent = primary_from(current).map(|p| p.percent).unwrap_or(60);
            // After pause, current is empty — caller should pass last-known via resume helper.
            vec![primary(
                percent,
                PowerState::Discharging,
                Connection::Bluetooth,
            )]
        }
        Preset::AnalyticsPlugMidDrain => {
            let percent = primary_from(current)
                .filter(|p| p.state.is_discharging())
                .map(|p| p.percent)
                .unwrap_or(40);
            vec![primary(percent, PowerState::Charging, Connection::Usb)]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charge_complete_step_two_phase() {
        let first = apply_preset(Preset::ChargeCompleteStep, &[]);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].state, PowerState::Charging);

        let second = apply_preset(Preset::ChargeCompleteStep, &first);
        assert_eq!(second[0].state, PowerState::Complete);
    }

    #[test]
    fn clear_empties() {
        let pads = apply_preset(Preset::LowBattery, &[]);
        assert!(!pads.is_empty());
        assert!(apply_preset(Preset::Clear, &pads).is_empty());
    }

    #[test]
    fn charge_advance_reaches_complete() {
        let mut pads = apply_preset(Preset::AnalyticsPlugEmpty, &[]);
        assert_eq!(pads[0].percent, 5);
        pads = apply_preset(Preset::AnalyticsChargeAdvance, &pads);
        assert_eq!(pads[0].percent, 25);
        for _ in 0..5 {
            pads = apply_preset(Preset::AnalyticsChargeAdvance, &pads);
        }
        assert_eq!(pads[0].state, PowerState::Complete);
    }

    #[test]
    fn drain_advance_reaches_empty_bucket() {
        let mut pads = apply_preset(Preset::AnalyticsUnplugFull, &[]);
        assert_eq!(pads[0].percent, 100);
        for _ in 0..6 {
            pads = apply_preset(Preset::AnalyticsDrainAdvance, &pads);
        }
        assert_eq!(pads[0].percent, LOW_BATTERY_PERCENT);
    }
}
