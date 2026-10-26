//! Pure device session: presence, notify, analytics, start-open policy.
//!
//! No `window::Id`, no HWND. The iced app applies [`SessionEffect`] values.

mod gate;
pub mod notify;

pub use gate::{
    ConnectCooldownDecision, POLL_MISS_HOLD, connect_cooldown_on_empty, has_start_presence,
    reconcile_poll_with_hold, should_auto_close_start, should_auto_open_start,
    should_defer_auto_open_start, should_flush_latched_start,
    should_skip_connect_cooldown_on_power_off, take_skip_connect_cooldown,
};

use crate::controller::known::KnownControllers;
use crate::controller::model::{ControllerStatus, Serial};
use crate::persist::analytics::AnalyticsStore;
use crate::persist::prefs::Prefs;
use crate::session::notify::{NotifyEvent, NotifyTracker};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Ignore 0→1 auto-open briefly after the last pad vanished (BT ghost flaps).
pub const START_CONNECT_COOLDOWN: Duration = Duration::from_secs(5);
/// Nonempty stretch shorter than this is an arrival blip — do not arm ghost cooldown.
pub const MIN_CONNECT_STRETCH: Duration = Duration::from_secs(2);

/// Start-screen selection held in memory (never written to disk).
///
/// Stable keys (never indices): game rows resolve via `play_key`, controller
/// rows via pad serial. `game_index` is the game-axis row position at report
/// time, used only when the `by_position` selection mode is on (clamped to
/// `index.min(len - 1)`; missing keeps the clamped index, never 0). There is
/// deliberately no `controller_index`: controller persistence is out of
/// scope. Dies with the process, and is never written to disk.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartSelection {
    pub game_key: Option<String>,
    pub controller_key: Option<String>,
    #[serde(default)]
    pub game_index: Option<usize>,
}

/// Side effects the app must perform after a session step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionEffect {
    /// Update tray icon / tooltip from the live controller count.
    SetTray { connected: usize },
    /// Replace hid-worker low-battery pulse targets.
    SetLowBatteryTargets { targets: Vec<(Serial, u8)> },
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
    /// Persist analytics store and rebuild Settings → Analytics panel.
    ///
    /// These two always co-occur (analytics is observed on every snapshot
    /// when enabled), so they are one effect: save first, then reload and
    /// refresh the panel from what was just written.
    SyncAnalytics,
    /// Rebuild popup rows (and resize if open).
    SyncPopup,
}

/// Inputs the app provides when applying a poll snapshot (UI facts only).
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
    pub low_battery: Vec<(Serial, u8)>,
    start_connect_cooldown_until: Option<Instant>,
    pub(crate) skip_next_connect_cooldown: bool,
    controllers_nonempty_since: Option<Instant>,
    missed_poll_since: HashMap<Serial, Instant>,
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
            missed_poll_since: HashMap::new(),
            start_auto_open_pending: false,
        }
    }

    pub fn clear_missed_polls(&mut self) {
        self.missed_poll_since.clear();
    }

    /// True while at least one previously-seen pad is inside the miss-hold
    /// window (present before, missing from the latest poll, hold unexpired).
    pub fn has_poll_misses(&self) -> bool {
        !self.missed_poll_since.is_empty()
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
        let (reconciled, held) = reconcile_poll_with_hold(
            &self.controllers,
            polled,
            &mut self.missed_poll_since,
            ctx.now,
        );
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
            effects.push(SessionEffect::SyncAnalytics);
        }

        if !controllers_changed && !known_changed {
            if self.prefs.analytics_enabled {
                effects.push(SessionEffect::SyncPopup);
            }
            return effects;
        }

        let include_usb = self.prefs.start_screen_auto_open.includes_usb();
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
        let want_auto_open = presence_edge.is_some_and(|(prev_present, next_present)| {
            should_auto_open_start(
                self.prefs.start_screen_enabled && self.prefs.start_screen_auto_open.auto_opens(),
                !prev_present,
                next_present,
                ctx.start_visible,
                self.cooldown_active(ctx.now),
                ctx.fullscreen,
            )
        });
        // When presence newly opens Start, suppress the Connected toast
        // entirely. Cold-launch already suppresses via launch_quiet; hot connects
        // must match so behaviour is identical.
        if want_auto_open {
            let before = events.len();
            events.retain(|event| event.body != "Connected");
            if events.len() != before {
                crate::controller::hid::diag::diag_info(
                    "ui-diag: connect toast suppressed (immersive auto-open)",
                );
            }
        }
        let connect_toast_queued = events.iter().any(|event| event.body == "Connected");
        if want_auto_open {
            self.start_auto_open_pending = true;
        }
        let open_start_after_toast =
            want_auto_open && should_defer_auto_open_start(true, connect_toast_queued);

        effects.push(SessionEffect::SetTray {
            connected: self.controllers.len(),
        });
        // Live membership or the remembered set changed, so the popup's row
        // list may have grown or shrunk — rebuild it and resize while open.
        effects.push(SessionEffect::SyncPopup);
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

    /// Re-evaluate close after the auto-open USB scope changes.
    ///
    /// A settings flip is not a connect edge — never auto-open Start here.
    /// Does not arm the ghost-flap cooldown (settings flip is not a pad leave).
    pub fn reevaluate_start_presence(&mut self, ctx: ApplyContext) -> Vec<SessionEffect> {
        let include_usb = self.prefs.start_screen_auto_open.includes_usb();
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
        let list: Vec<(Serial, u8)> = if lightbar_enabled {
            let threshold = self.prefs.low_battery_percent;
            self.controllers
                .iter()
                .filter(|c| c.is_low_battery(threshold) && !is_emulated_serial(c.serial.as_ref()))
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
    pub fn refresh_low_battery(&mut self, lightbar_enabled: bool) -> Option<SessionEffect> {
        let mut effects = Vec::new();
        self.sync_low_battery(lightbar_enabled, &mut effects);
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
    #[cfg(debug_assertions)]
    {
        crate::controller::emulate::is_emulated(serial)
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = serial;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::dualsense::battery::dualsense_status;
    use crate::controller::model::{Connection, PowerState};
    use crate::persist::prefs::StartAutoOpen;

    fn bt_pad(serial: &str) -> ControllerStatus {
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
            PowerState::Discharging,
        )
    }

    fn usb_pad_at(serial: &str, percent: u8, state: PowerState) -> ControllerStatus {
        dualsense_status(
            1,
            "DualSense",
            Connection::Usb,
            serial.to_string(),
            percent,
            state,
        )
    }

    fn ctx(start_visible: bool) -> ApplyContext {
        ApplyContext {
            start_visible,
            fullscreen: false,
            now: Instant::now(),
            lightbar_enabled: false,
        }
    }

    fn session_with(mode: StartAutoOpen) -> DeviceSession {
        let prefs = Prefs {
            start_screen_enabled: true,
            start_screen_auto_open: mode,
            notify_connect: true,
            analytics_enabled: false,
            ..Prefs::default()
        };
        DeviceSession::new(
            prefs,
            KnownControllers::default(),
            AnalyticsStore::default(),
        )
    }

    fn has_open_start(effects: &[SessionEffect]) -> bool {
        effects
            .iter()
            .any(|e| matches!(e, SessionEffect::OpenStart))
    }

    fn queued_bodies(effects: &[SessionEffect]) -> Vec<String> {
        effects
            .iter()
            .filter_map(|e| match e {
                SessionEffect::QueueNotifications { events, .. } => Some(events),
                _ => None,
            })
            .flatten()
            .map(|ev| ev.body.clone())
            .collect()
    }

    #[test]
    fn auto_open_suppresses_connected_toast() {
        let mut session = session_with(StartAutoOpen::Any);
        let effects = session.apply_controllers(vec![bt_pad("aa")], ctx(false));
        assert!(has_open_start(&effects));
        assert!(
            !queued_bodies(&effects).contains(&"Connected".to_string()),
            "auto-open must not queue Connected, got {:?}",
            queued_bodies(&effects)
        );
    }

    #[test]
    fn no_suppress_when_already_open() {
        let mut session = session_with(StartAutoOpen::Any);
        let effects = session.apply_controllers(vec![bt_pad("aa")], ctx(true));
        assert!(!has_open_start(&effects));
        assert!(queued_bodies(&effects).contains(&"Connected".to_string()));
    }

    #[test]
    fn auto_open_never_never_opens_but_still_toasts() {
        let mut session = session_with(StartAutoOpen::Never);
        let effects = session.apply_controllers(vec![bt_pad("aa")], ctx(false));
        assert!(!has_open_start(&effects));
        assert!(queued_bodies(&effects).contains(&"Connected".to_string()));
    }

    #[test]
    fn hot_path_stall_within_hold_keeps_pad_quiet() {
        use std::time::Duration;

        let mut session = session_with(StartAutoOpen::Any);
        let pad_a = bt_pad("aa");
        let pad_b = bt_pad("bb");
        let t0 = Instant::now();
        let ctx_at = |now: Instant| ApplyContext {
            start_visible: false,
            fullscreen: false,
            now,
            lightbar_enabled: false,
        };
        // Two pads connected.
        let _ = session.apply_controllers(vec![pad_a.clone(), pad_b.clone()], ctx_at(t0));
        assert_eq!(session.controllers.len(), 2);

        // Hot path misses pad B every ~16ms: held, no flap.
        for ms in [16, 32, 500, 1500] {
            let effects =
                session.on_poll_result(vec![pad_a.clone()], ctx_at(t0 + Duration::from_millis(ms)));
            assert_eq!(session.controllers.len(), 2, "held at +{ms}ms");
            assert!(
                !queued_bodies(&effects).contains(&"Disconnected".to_string()),
                "no disconnect at +{ms}ms"
            );
        }

        // Past the hold window: accepted drop with a Disconnect toast.
        let effects = session.on_poll_result(
            vec![pad_a.clone()],
            ctx_at(t0 + crate::session::gate::POLL_MISS_HOLD + Duration::from_secs(1)),
        );
        assert_eq!(session.controllers.len(), 1);
        assert!(queued_bodies(&effects).contains(&"Disconnected".to_string()));
    }

    #[test]
    fn poll_miss_flag_tracks_hold_lifecycle() {
        use std::time::Duration;

        let mut session = session_with(StartAutoOpen::Any);
        let pad_a = bt_pad("aa");
        let t0 = Instant::now();
        let ctx_at = |now: Instant| ApplyContext {
            start_visible: false,
            fullscreen: false,
            now,
            lightbar_enabled: false,
        };
        let _ = session.apply_controllers(vec![pad_a.clone()], ctx_at(t0));
        assert!(!session.has_poll_misses());

        // First miss: held, flag set (drives the app-side absence recheck).
        let _ = session.on_poll_result(vec![], ctx_at(t0 + Duration::from_millis(100)));
        assert_eq!(session.controllers.len(), 1);
        assert!(session.has_poll_misses());

        // Hold expiry drops the pad and clears the flag (no recheck loop).
        let _ = session.on_poll_result(
            vec![],
            ctx_at(t0 + crate::session::POLL_MISS_HOLD + Duration::from_millis(500)),
        );
        assert!(session.controllers.is_empty());
        assert!(!session.has_poll_misses());
    }

    #[test]
    fn bluetooth_mode_ignores_usb_only_presence() {
        // USB-only arrival is not qualifying presence in Bluetooth mode:
        // no open, and the toast stays a plain notification.
        let mut session = session_with(StartAutoOpen::Bluetooth);
        let effects = session.apply_controllers(vec![usb_pad("usb:1")], ctx(false));
        assert!(!has_open_start(&effects));
        assert!(queued_bodies(&effects).contains(&"Connected".to_string()));
        assert!(
            !effects.iter().any(|e| matches!(
                e,
                SessionEffect::QueueNotifications {
                    open_start_after_toast: true,
                    ..
                }
            )),
            "USB-only arrival must not latch an open in Bluetooth mode"
        );
    }

    #[test]
    fn bluetooth_mode_opens_on_bluetooth_arrival() {
        let mut session = session_with(StartAutoOpen::Bluetooth);
        let effects = session.apply_controllers(vec![bt_pad("aa")], ctx(false));
        assert!(has_open_start(&effects));
        assert!(
            !queued_bodies(&effects).contains(&"Connected".to_string()),
            "auto-open suppresses Connected, got {:?}",
            queued_bodies(&effects)
        );
    }

    fn has_popup_sync(effects: &[SessionEffect]) -> bool {
        effects
            .iter()
            .any(|e| matches!(e, SessionEffect::SyncPopup))
    }

    #[test]
    fn membership_gain_emits_popup_sync() {
        // 0→1 grows the popup by a row: the shell must resize while open.
        let mut session = session_with(StartAutoOpen::Any);
        let effects = session.apply_controllers(vec![bt_pad("aa")], ctx(false));
        assert!(
            has_popup_sync(&effects),
            "connect must emit SyncPopup, got {effects:?}"
        );
    }

    #[test]
    fn membership_loss_emits_popup_sync() {
        // 1→0 shrinks the popup by a row: the shell must resize while open.
        let mut session = session_with(StartAutoOpen::Any);
        let _ = session.apply_controllers(vec![bt_pad("aa")], ctx(false));
        let effects = session.apply_controllers(Vec::new(), ctx(false));
        assert!(
            has_popup_sync(&effects),
            "disconnect must emit SyncPopup, got {effects:?}"
        );
    }

    #[test]
    fn reconnecting_a_forgotten_pad_emits_popup_sync() {
        // Forgetting drops the remembered row; reconnecting re-adds the live
        // row. Both change the row count, so both must resize the popup.
        let mut session = session_with(StartAutoOpen::Any);
        session.known.remember(&bt_pad("aa"));
        assert!(session.known.forget("aa"));
        let effects = session.apply_controllers(vec![bt_pad("aa")], ctx(false));
        assert!(
            has_popup_sync(&effects),
            "forgotten-pad reconnect must emit SyncPopup, got {effects:?}"
        );
    }

    #[test]
    fn unchanged_snapshot_emits_no_popup_sync() {
        // Same pad, same readings: no row change, so no pointless resize.
        let mut session = session_with(StartAutoOpen::Any);
        let _ = session.apply_controllers(vec![bt_pad("aa")], ctx(false));
        let effects = session.apply_controllers(vec![bt_pad("aa")], ctx(false));
        assert!(
            !has_popup_sync(&effects),
            "equivalent snapshot must not emit SyncPopup, got {effects:?}"
        );
    }

    /// Regression for the 2026-10-14 incident (popup showed a stale remembered
    /// `%` after full-charge + USB unplug while disk was correct).
    ///
    /// `95% Charging → 100% Complete → disconnected` must leave the
    /// remembered record at 100%, not the boot-time 95%.
    #[test]
    fn full_charge_then_unplug_remembered_carries_100() {
        let mut session = session_with(StartAutoOpen::Any);
        // Boot-time state: pad remembered at 95%.
        session
            .known
            .remember(&usb_pad_at("pad-1", 95, PowerState::Charging));

        // Charge walk to full, then unplug.
        let _ = session.apply_controllers(
            vec![usb_pad_at("pad-1", 95, PowerState::Charging)],
            ctx(false),
        );
        let _ = session.apply_controllers(
            vec![usb_pad_at("pad-1", 100, PowerState::Complete)],
            ctx(false),
        );
        let _ = session.apply_controllers(Vec::new(), ctx(false));

        let remembered = session.known.remembered_disconnected(&session.controllers);
        assert_eq!(remembered.len(), 1);
        assert_eq!(remembered[0].percent, 100);
        assert!(session.controllers.is_empty(), "pad was unplugged");
    }
}
