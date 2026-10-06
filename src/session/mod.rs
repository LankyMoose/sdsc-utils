//! Pure device session: presence, notify, analytics, start-open policy.
//!
//! No `window::Id`, no HWND. The iced shell (and later the named-pipe shell)
//! applies [`SessionEffect`] values.

mod gate;

pub use gate::{
    ConnectCooldownDecision, connect_cooldown_on_empty, has_start_presence,
    reconcile_poll_with_hold, should_auto_close_start, should_auto_open_start,
    should_defer_auto_open_start, should_flush_latched_start,
    should_skip_connect_cooldown_on_power_off, take_skip_connect_cooldown,
};

use crate::controller::dualsense::lightbar;
use crate::controller::known::KnownControllers;
use crate::controller::model::ControllerStatus;
use crate::persist::analytics::AnalyticsStore;
use crate::persist::notify::{NotifyEvent, NotifyTracker};
use crate::persist::prefs::Prefs;
use crate::platform::app_log;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Ignore 0→1 auto-open briefly after the last pad vanished (BT ghost flaps).
pub const START_CONNECT_COOLDOWN: Duration = Duration::from_secs(5);
/// Nonempty stretch shorter than this is an arrival blip — do not arm ghost cooldown.
pub const MIN_CONNECT_STRETCH: Duration = Duration::from_secs(2);

/// Side effects the shell must perform after a session step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionEffect {
    /// Update tray icon / tooltip from the live controller count.
    SetTray { connected: usize },
    /// Replace hid-worker low-battery pulse targets.
    SetLowBatteryTargets { targets: Vec<(String, u8)> },
    /// Enqueue overlay notification events.
    QueueNotifications {
        events: Vec<NotifyEvent>,
        /// Latch Start open onto the connect toast presentation machine.
        open_start_after_toast: bool,
    },
    /// Open the start screen now (no connect toast to wait on).
    OpenStart,
    /// Close the start screen (all pads gone).
    CloseStart,
    /// Clear latched auto-open and toast AfterToast::OpenStart.
    ClearStartLatch,
    /// Persist remembered controllers / nicknames.
    SaveKnown,
    /// Persist analytics store.
    SaveAnalytics,
    /// Rebuild Settings → Analytics panel from the store.
    RefreshAnalyticsPanel,
    /// Rebuild popup rows (and resize if open).
    SyncPopup,
}

/// Inputs the shell provides when applying a poll snapshot (UI facts only).
#[derive(Debug, Clone, Copy)]
pub struct ApplyContext {
    pub start_visible: bool,
    pub fullscreen: bool,
    pub now: Instant,
    pub lightbar_enabled: bool,
}

/// Owns prefs, known pads, notify edges, analytics, and start-open gate state.
pub struct DeviceSession {
    pub prefs: Prefs,
    pub known: KnownControllers,
    pub notify: NotifyTracker,
    pub analytics: AnalyticsStore,
    pub controllers: Vec<ControllerStatus>,
    /// Controllers currently in the critical low-battery bucket (serial, percent).
    pub low_battery: Vec<(String, u8)>,
    start_connect_cooldown_until: Option<Instant>,
    pub(crate) skip_next_connect_cooldown: bool,
    controllers_nonempty_since: Option<Instant>,
    missed_poll_counts: HashMap<String, u8>,
    pub start_auto_open_pending: bool,
}

impl DeviceSession {
    pub fn new(prefs: Prefs, known: KnownControllers, analytics: AnalyticsStore) -> Self {
        Self {
            prefs,
            known,
            notify: NotifyTracker::new(),
            analytics,
            controllers: Vec::new(),
            low_battery: Vec::new(),
            start_connect_cooldown_until: None,
            skip_next_connect_cooldown: false,
            controllers_nonempty_since: None,
            missed_poll_counts: HashMap::new(),
            start_auto_open_pending: false,
        }
    }

    pub fn clear_missed_polls(&mut self) {
        self.missed_poll_counts.clear();
    }

    pub fn mark_skip_connect_cooldown(&mut self) {
        self.skip_next_connect_cooldown = true;
    }

    pub fn cooldown_active(&self, now: Instant) -> bool {
        self.start_connect_cooldown_until
            .is_some_and(|until| now < until)
    }

    /// Reconcile a poll result with the one-miss hold, then apply.
    pub fn on_poll_result(
        &mut self,
        polled: Vec<ControllerStatus>,
        ctx: ApplyContext,
    ) -> Vec<SessionEffect> {
        let (reconciled, held) =
            reconcile_poll_with_hold(&self.controllers, polled, &mut self.missed_poll_counts);
        for serial in &held {
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: hold pad across missed read serial={serial}"
            ));
        }
        self.apply_controllers(reconciled, ctx)
    }

    /// Apply a new live controller snapshot.
    pub fn apply_controllers(
        &mut self,
        controllers: Vec<ControllerStatus>,
        ctx: ApplyContext,
    ) -> Vec<SessionEffect> {
        let mut effects = Vec::new();
        let known_changed = self.known.sync_from_live(&controllers);
        let controllers_changed = !controllers_equivalent(&self.controllers, &controllers);

        if self.prefs.analytics_enabled {
            let previous = self.controllers.clone();
            let next = if controllers_changed {
                controllers.as_slice()
            } else {
                self.controllers.as_slice()
            };
            let keep = |serial: &str| {
                self.known.is_remembered(serial) || self.known.nickname(serial).is_some()
            };
            self.analytics
                .observe(&previous, next, true, keep, std::time::SystemTime::now());
            effects.push(SessionEffect::SaveAnalytics);
            effects.push(SessionEffect::RefreshAnalyticsPanel);
        }

        if !controllers_changed && !known_changed {
            if self.prefs.analytics_enabled {
                effects.push(SessionEffect::SyncPopup);
            }
            return effects;
        }

        let include_usb = self.prefs.start_screen_usb_controllers;
        let mut events = Vec::new();
        let mut presence_edge: Option<(bool, bool)> = None;
        if controllers_changed {
            let previous = std::mem::replace(&mut self.controllers, controllers);
            let prev_present = has_start_presence(&previous, include_usb);
            let next_present = has_start_presence(&self.controllers, include_usb);
            presence_edge = Some((prev_present, next_present));
            if prev_present != next_present {
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: start presence include_usb={} prev={} next={}",
                    u8::from(include_usb),
                    u8::from(prev_present),
                    u8::from(next_present)
                ));
            } else if previous.is_empty()
                && !self.controllers.is_empty()
                && !include_usb
                && !next_present
            {
                // Raw 0→1 was USB-only while USB does not count.
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: start presence include_usb={} prev={} next={}",
                    u8::from(include_usb),
                    u8::from(prev_present),
                    u8::from(next_present)
                ));
            }
            if !prev_present && next_present {
                self.controllers_nonempty_since = Some(ctx.now);
            }
            if prev_present && !next_present {
                let stretch = self
                    .controllers_nonempty_since
                    .map(|since| ctx.now.saturating_duration_since(since));
                self.controllers_nonempty_since = None;
                let skip = take_skip_connect_cooldown(&mut self.skip_next_connect_cooldown);
                match connect_cooldown_on_empty(skip, stretch, MIN_CONNECT_STRETCH) {
                    ConnectCooldownDecision::SkipIntentional => {
                        crate::controller::hid::diag::diag_info(
                            "ui-diag: skip connect cooldown (intentional power-off)",
                        );
                    }
                    ConnectCooldownDecision::SkipShortArrival => {
                        crate::controller::hid::diag::diag_info(
                            "ui-diag: skip connect cooldown (short arrival)",
                        );
                    }
                    ConnectCooldownDecision::Arm => {
                        self.start_connect_cooldown_until = Some(ctx.now + START_CONNECT_COOLDOWN);
                    }
                }
            }
            events = self
                .notify
                .evaluate(&previous, &self.controllers, &self.prefs, |serial| {
                    self.known.nickname(serial).map(str::to_string)
                });
            self.sync_low_battery(ctx.lightbar_enabled, &mut effects);
        }

        effects.push(SessionEffect::SaveKnown);
        let connect_toast_queued = events.iter().any(|event| event.body == "Connected");
        let want_auto_open = presence_edge.is_some_and(|(prev_present, next_present)| {
            should_auto_open_start(
                self.prefs.start_screen_enabled,
                !prev_present,
                next_present,
                ctx.start_visible,
                self.cooldown_active(ctx.now),
                ctx.fullscreen,
            )
        });
        // When auto-open is desired, suppress the connect toast entirely.
        // This makes cold-launch and hot-connect behavior consistent:
        // - Cold-launch with controller already on: no connect toast
        // - Turning on a controller afterwards: no connect toast when auto-open triggers
        if want_auto_open {
            self.start_auto_open_pending = true;
        }
        let open_start_after_toast =
            want_auto_open && should_defer_auto_open_start(true, connect_toast_queued);

        effects.push(SessionEffect::SetTray {
            connected: self.controllers.len(),
        });
        if !events.is_empty() || open_start_after_toast {
            effects.push(SessionEffect::QueueNotifications {
                events,
                open_start_after_toast,
            });
        }

        if let Some((prev_present, next_present)) = presence_edge {
            if !next_present {
                self.start_auto_open_pending = false;
                effects.push(SessionEffect::ClearStartLatch);
                if should_auto_close_start(prev_present, next_present, ctx.start_visible) {
                    effects.push(SessionEffect::CloseStart);
                }
            } else if want_auto_open && !connect_toast_queued {
                effects.push(SessionEffect::OpenStart);
            }
        }

        effects
    }

    /// Re-evaluate close after `start_screen_usb_controllers` changes.
    ///
    /// A settings flip is not a connect edge — never auto-open Start here.
    /// Does not arm the ghost-flap cooldown (settings flip is not a pad leave).
    pub fn reevaluate_start_presence(&mut self, ctx: ApplyContext) -> Vec<SessionEffect> {
        let include_usb = self.prefs.start_screen_usb_controllers;
        let present = has_start_presence(&self.controllers, include_usb);
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: start presence include_usb={} present={} (prefs)",
            u8::from(include_usb),
            u8::from(present),
        ));

        let mut effects = Vec::new();
        if present {
            if self.controllers_nonempty_since.is_none() {
                self.controllers_nonempty_since = Some(ctx.now);
            }
        } else {
            self.start_auto_open_pending = false;
            self.controllers_nonempty_since = None;
            effects.push(SessionEffect::ClearStartLatch);
            if ctx.start_visible {
                effects.push(SessionEffect::CloseStart);
            }
        }
        effects
    }

    fn sync_low_battery(&mut self, lightbar_enabled: bool, effects: &mut Vec<SessionEffect>) {
        let list: Vec<(String, u8)> = if lightbar_enabled {
            let threshold = self.prefs.low_battery_percent;
            self.controllers
                .iter()
                .filter(|c| c.is_low_battery(threshold) && !is_emulated_serial(&c.serial))
                .map(|c| (c.serial.clone(), c.percent))
                .collect()
        } else {
            Vec::new()
        };
        if self.low_battery != list {
            self.low_battery = list.clone();
            effects.push(SessionEffect::SetLowBatteryTargets { targets: list });
        }
    }

    /// Recompute low-battery targets after prefs / lightbar enable changes.
    pub fn refresh_low_battery(&mut self) -> Option<SessionEffect> {
        let mut effects = Vec::new();
        self.sync_low_battery(lightbar::is_enabled(), &mut effects);
        effects.into_iter().next()
    }
}

/// True when live snapshots differ only in ways the UI ignores for row refresh.
pub fn controllers_equivalent(a: &[ControllerStatus], b: &[ControllerStatus]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).all(|(x, y)| {
        x.serial == y.serial
            && x.percent == y.percent
            && x.state == y.state
            && x.connection == y.connection
            && x.product == y.product
    })
}

fn is_emulated_serial(serial: &str) -> bool {
    #[cfg(feature = "dev-emulate")]
    {
        crate::controller::emulate::is_emulated(serial)
    }
    #[cfg(not(feature = "dev-emulate"))]
    {
        let _ = serial;
        false
    }
}

/// Log helper kept so callers that only warn on empty effects stay quiet.
#[allow(dead_code)]
pub fn log_apply(effects: &[SessionEffect]) {
    if effects.is_empty() {
        return;
    }
    app_log::info(format!("session: apply effects={}", effects.len()));
}
