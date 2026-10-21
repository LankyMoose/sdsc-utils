//! Central HID cadence policy (additive, no behavior change).
//!
//! Gathers the timeout / interval / threshold consts currently scattered
//! across `hid::worker`, `hid::poll` and `domain::pad` into one `Policy`
//! struct. `Policy::default()` reproduces the current values exactly;
//! `poll_controllers` / `poll_controllers_timed` / `poll_controllers_with_api`
//! take `&Policy` (callers pass `&Policy::default()`). Remaining call sites
//! still use the original consts; later commits migrate them the same way
//! without changing timing.

use std::time::Duration;

/// Single owner for all HID polling / presence / backoff cadences.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// Block length when input queue is empty (one wired report interval).
    pub report_wait_ms: i32,
    /// Cap on non-blocking drains per wake (Windows HID buffer depth).
    pub drain_cap: usize,
    /// Consecutive sample timeouts before dropping pad from hot live map.
    pub live_silence_threshold: u32,
    /// Hot-sample battery newer than this skips blocking read.
    pub poll_live_battery_stale: Duration,
    /// Presence scan interval when pads are known.
    pub presence: Duration,
    /// Presence scan interval while tray is empty.
    pub presence_empty: Duration,
    /// Battery / lightbar re-read when stable.
    pub battery: Duration,
    /// Re-probe while tray shows connected pads.
    pub liveness: Duration,
    /// Retry when HID lists pads but reads fail.
    pub unread_retry: Duration,
    /// Upper bound on hot enumeration deferral.
    pub hot_enum_max_defer: Duration,
    /// Cold-idle presence heartbeat (no OS watcher fallback).
    pub cold_heartbeat: Duration,
    /// Minimum gap between OS arrival-hint refreshes.
    pub refresh_hint_throttle: Duration,
    /// Consecutive open failures before path backoff.
    pub open_backoff_threshold: u32,
    /// How long a backed-off path is skipped.
    pub open_backoff_window: Duration,
    /// Active input wake (wired report interval).
    pub active_poll: Duration,
    /// Identify-flash input sample interval.
    pub identify_sample_poll: Duration,
    /// Sample stall log threshold.
    pub sample_stall_ms: u128,
    /// Slow HID command log threshold.
    pub slow_cmd_log_ms: u128,
    /// Low-battery pulse sample log interval.
    pub pulse_sample_log_interval: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            report_wait_ms: 4,
            drain_cap: 32,
            live_silence_threshold: 50,
            poll_live_battery_stale: Duration::from_secs(30),
            presence: Duration::from_secs(3),
            presence_empty: Duration::from_secs(2),
            battery: Duration::from_secs(60),
            liveness: Duration::from_secs(5),
            unread_retry: Duration::from_secs(15),
            hot_enum_max_defer: Duration::from_secs(20),
            cold_heartbeat: Duration::from_secs(2),
            refresh_hint_throttle: Duration::from_secs(1),
            open_backoff_threshold: 3,
            open_backoff_window: Duration::from_secs(5),
            active_poll: Duration::from_millis(4),
            identify_sample_poll: Duration::from_millis(4),
            sample_stall_ms: 100,
            slow_cmd_log_ms: 20,
            pulse_sample_log_interval: Duration::from_secs(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::hid::poll::{
        BATTERY_INTERVAL, LIVENESS_INTERVAL, POLL_LIVE_BATTERY_STALE, PRESENCE_INTERVAL,
        PRESENCE_INTERVAL_EMPTY, UNREAD_RETRY_INTERVAL,
    };
    use crate::controller::hid::worker::HOT_ENUM_MAX_DEFER;
    use crate::domain::pad::INPUT_REPORT_WAIT_MS;

    #[test]
    fn default_matches_existing_consts() {
        let p = Policy::default();
        assert_eq!(p.report_wait_ms, INPUT_REPORT_WAIT_MS);
        assert_eq!(p.presence, PRESENCE_INTERVAL);
        assert_eq!(p.presence_empty, PRESENCE_INTERVAL_EMPTY);
        assert_eq!(p.battery, BATTERY_INTERVAL);
        assert_eq!(p.liveness, LIVENESS_INTERVAL);
        assert_eq!(p.unread_retry, UNREAD_RETRY_INTERVAL);
        assert_eq!(p.poll_live_battery_stale, POLL_LIVE_BATTERY_STALE);
        assert_eq!(p.hot_enum_max_defer, HOT_ENUM_MAX_DEFER);
    }
}
