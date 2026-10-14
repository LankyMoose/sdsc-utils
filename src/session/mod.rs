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

use crate::controller::dualsense::identity::normalize_identity;
use crate::controller::dualsense::lightbar;
use crate::controller::known::{KnownController, KnownControllers};
use crate::controller::model::ControllerStatus;
use crate::persist::analytics::{AnalyticsStore, SerialRecord};
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

/// Start-screen selection owned in memory by the service process.
///
/// Stable keys (never indices): game rows resolve via `play_key`, controller
/// rows via pad serial. Survives shell restarts, dies with the service, and is
/// never written to disk.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartSelection {
    pub game_key: Option<String>,
    pub controller_key: Option<String>,
}

/// Retained service→shell state: one message, one revision.
///
/// The service is the sole owner of this truth (live pads, remembered pads,
/// prefs, analytics records, stored start selection). The shell renders
/// purely from the last snapshot it applied and mutates only via intents.
/// New shared state later means a new `#[serde(default)]` field here — not
/// another shadow copy with its own sync loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// Monotonic revision; the shell applies wholesale iff `rev` is newer.
    #[serde(default)]
    pub rev: u64,
    /// Live HID controllers.
    #[serde(default)]
    pub live: Vec<ControllerStatus>,
    /// Remembered pads (connected or not).
    #[serde(default)]
    pub remembered: Vec<KnownController>,
    /// Nicknames by serial.
    #[serde(default)]
    pub nicknames: HashMap<String, String>,
    /// Full prefs (service-validated).
    #[serde(default)]
    pub prefs: Prefs,
    /// Stored start-screen selection (service process lifetime only).
    #[serde(default)]
    pub selection: StartSelection,
    /// Learned analytics records (shell recomputes ETAs/panel rows locally).
    #[serde(default)]
    pub analytics: HashMap<String, SerialRecord>,
}

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
    missed_poll_since: HashMap<String, Instant>,
    pub start_auto_open_pending: bool,
    /// Last start-screen selection reported by the shell (service process
    /// lifetime only — never persisted to disk).
    pub start_selection: StartSelection,
    /// Monotonic revision for the retained snapshot (see [`SessionSnapshot`]).
    /// Bumped on every mutation that changes snapshot content.
    snapshot_rev: u64,
    /// Developer emulator fleet (debug-only, service-owned in client mode).
    /// The shell never touches this: it renders the fleet derived from the
    /// snapshot and sends edits as intents. Release builds have no fleet.
    #[cfg(debug_assertions)]
    pub emu_fleet: Vec<crate::controller::emulate::EmulatedPad>,
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
            start_selection: StartSelection::default(),
            snapshot_rev: 1,
            #[cfg(debug_assertions)]
            emu_fleet: Vec::new(),
        }
    }

    pub fn clear_missed_polls(&mut self) {
        self.missed_poll_since.clear();
    }

    /// Clear miss streaks for pads seen in the latest snapshot.
    ///
    /// Must run on *every* ingestion, not just on change: the hot path skips
    /// evaluation when the live snapshot is equivalent, and a held pad's clone
    /// is identical — so a recovered pad's streak would otherwise survive until
    /// the next stall drops it instantly with no hold.
    pub fn mark_seen(&mut self, serials: impl IntoIterator<Item = impl AsRef<str>>) {
        for serial in serials {
            self.missed_poll_since
                .remove(&normalize_identity(serial.as_ref()));
        }
    }

    pub fn mark_skip_connect_cooldown(&mut self) {
        self.skip_next_connect_cooldown = true;
    }

    /// Remember the shell's start-screen selection (in memory only).
    pub fn remember_start_selection(&mut self, selection: StartSelection) {
        if self.start_selection != selection {
            self.start_selection = selection;
            self.bump_snapshot_rev();
        }
    }

    /// Current snapshot revision (monotonic; starts at 1).
    pub fn snapshot_rev(&self) -> u64 {
        self.snapshot_rev
    }

    fn bump_snapshot_rev(&mut self) {
        self.snapshot_rev = self.snapshot_rev.saturating_add(1).max(1);
    }

    /// Build the retained snapshot the shell renders from.
    pub fn snapshot(&self) -> SessionSnapshot {
        let (remembered, nicknames) = self.known.export_snapshot();
        SessionSnapshot {
            rev: self.snapshot_rev,
            live: self.controllers.clone(),
            remembered,
            nicknames,
            prefs: self.prefs.clone(),
            selection: self.start_selection.clone(),
            analytics: self.analytics.export_snapshot(),
        }
    }

    /// Replace prefs wholesale (service-validated) and bump the snapshot rev
    /// when anything changed. Prefer this over assigning `session.prefs`
    /// directly so the shell always converges.
    pub fn apply_prefs(&mut self, prefs: Prefs) {
        // Prefs has no PartialEq; compare the serialized form.
        let before = serde_json::to_vec(&self.prefs).unwrap_or_default();
        let after = serde_json::to_vec(&prefs).unwrap_or_default();
        self.prefs = prefs;
        if before != after {
            self.bump_snapshot_rev();
        }
    }

    /// Replace the remembered store wholesale (disk reload path) and bump the
    /// snapshot rev when anything changed.
    pub fn apply_known(&mut self, known: KnownControllers) {
        let (before_r, before_n) = self.known.export_snapshot();
        let (after_r, after_n) = known.export_snapshot();
        self.known = known;
        if before_r != after_r || before_n != after_n {
            self.bump_snapshot_rev();
        }
    }

    /// Force the snapshot rev forward (intent receipt echo).
    ///
    /// The service calls this after every mutating shell intent so the shell
    /// always converges — even when the intent was a no-op (e.g. remembering
    /// an already-remembered pad, or a pref write that changed nothing). The
    /// broadcast snapshot reconciles any optimistic shell state to service
    /// truth within one service loop.
    pub fn echo_intent(&mut self) {
        self.bump_snapshot_rev();
    }

    /// Apply a shell remember/forget intent (service side).
    ///
    /// Remembering resolves against the service's own live list (source of
    /// truth); an unknown serial is a no-op. Returns true when stored data
    /// changed.
    pub fn set_remembered(
        &mut self,
        serial: &str,
        remembered: bool,
        live: &[ControllerStatus],
    ) -> bool {
        if remembered {
            live.iter()
                .find(|c| c.serial == serial)
                .is_some_and(|c| self.known.remember(c))
        } else {
            self.known.forget(serial)
        }
    }

    /// Apply a shell nickname intent (service side, absolute: `None` clears).
    /// Returns true when stored data changed.
    pub fn set_nickname(&mut self, serial: &str, nickname: Option<String>) -> bool {
        self.known.set_nickname(serial, nickname)
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
            effects.push(SessionEffect::SaveAnalytics);
            effects.push(SessionEffect::RefreshAnalyticsPanel);
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
        // Live membership or remembered fields changed: the retained snapshot
        // the shell renders from must advance too.
        self.bump_snapshot_rev();
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
        // When presence newly opens immersive Start, suppress the Connected toast
        // entirely. Cold-launch already suppresses via launch_quiet; hot connects
        // must match so behaviour is identical. Compact keeps the toast (HWND).
        if want_auto_open && self.prefs.start_screen_always_immersive {
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
        // The shell maps this to `sync_popup_rows_and_fit`; the service
        // forwards it (it already owns the tray via `SetTray`).
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

/// Log helper kept so callers that only warn on empty effects stay quiet.
#[allow(dead_code)]
pub fn log_apply(effects: &[SessionEffect]) {
    if effects.is_empty() {
        return;
    }
    app_log::info(format!("session: apply effects={}", effects.len()));
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

    fn session_with(immersive: bool, mode: StartAutoOpen) -> DeviceSession {
        let prefs = Prefs {
            start_screen_enabled: true,
            start_screen_auto_open: mode,
            start_screen_always_immersive: immersive,
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
    fn immersive_auto_open_suppresses_connected_toast() {
        let mut session = session_with(true, StartAutoOpen::Any);
        let effects = session.apply_controllers(vec![bt_pad("aa")], ctx(false));
        assert!(has_open_start(&effects));
        assert!(
            !queued_bodies(&effects).contains(&"Connected".to_string()),
            "immersive open must not queue Connected, got {:?}",
            queued_bodies(&effects)
        );
    }

    #[test]
    fn compact_auto_open_keeps_connected_toast() {
        let mut session = session_with(false, StartAutoOpen::Any);
        let effects = session.apply_controllers(vec![bt_pad("aa")], ctx(false));
        assert!(!has_open_start(&effects));
        assert!(queued_bodies(&effects).contains(&"Connected".to_string()));
        // Deferred open latched for after-toast.
        assert!(
            effects.iter().any(|e| matches!(
                e,
                SessionEffect::QueueNotifications {
                    open_start_after_toast: true,
                    ..
                }
            )),
            "compact should defer open until toast"
        );
    }

    #[test]
    fn immersive_no_suppress_when_already_open() {
        let mut session = session_with(true, StartAutoOpen::Any);
        let effects = session.apply_controllers(vec![bt_pad("aa")], ctx(true));
        assert!(!has_open_start(&effects));
        assert!(queued_bodies(&effects).contains(&"Connected".to_string()));
    }

    #[test]
    fn auto_open_never_never_opens_but_still_toasts() {
        let mut session = session_with(true, StartAutoOpen::Never);
        let effects = session.apply_controllers(vec![bt_pad("aa")], ctx(false));
        assert!(!has_open_start(&effects));
        assert!(queued_bodies(&effects).contains(&"Connected".to_string()));
    }

    #[test]
    fn mark_seen_clears_stale_streak_so_next_stall_holds() {
        use std::time::Duration;

        // Regression: the hot path skips evaluation when the live snapshot is
        // equivalent (a held clone is identical), so a recovered pad's streak
        // survived and its next stall dropped instantly with no hold.
        let mut session = session_with(false, StartAutoOpen::Any);
        let pad_a = bt_pad("aa");
        let pad_b = bt_pad("bb");
        let t0 = Instant::now();
        let ctx_at = |now: Instant| ApplyContext {
            start_visible: false,
            fullscreen: false,
            now,
            lightbar_enabled: false,
        };
        let _ = session.apply_controllers(vec![pad_a.clone(), pad_b.clone()], ctx_at(t0));

        // Stale streak as left by a recovered-then-unevaluated stall.
        session
            .missed_poll_since
            .insert("bb".to_string(), t0 - Duration::from_secs(36));
        // Fresh sighting on every hot loop clears it, evaluation or not.
        session.mark_seen(["bb"]);
        assert!(session.missed_poll_since.is_empty());

        // The next stall therefore holds instead of dropping instantly.
        let effects = session.on_poll_result(vec![pad_a.clone()], ctx_at(t0));
        assert_eq!(session.controllers.len(), 2);
        assert!(!queued_bodies(&effects).contains(&"Disconnected".to_string()));
    }

    #[test]
    fn hot_path_stall_within_hold_keeps_pad_quiet() {
        use std::time::Duration;

        let mut session = session_with(false, StartAutoOpen::Any);
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
    fn bluetooth_mode_ignores_usb_only_presence() {
        // USB-only arrival is not qualifying presence in Bluetooth mode:
        // no open, and the toast stays a plain notification.
        let mut session = session_with(false, StartAutoOpen::Bluetooth);
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
        let mut session = session_with(false, StartAutoOpen::Bluetooth);
        let effects = session.apply_controllers(vec![bt_pad("aa")], ctx(false));
        assert!(queued_bodies(&effects).contains(&"Connected".to_string()));
        assert!(
            effects.iter().any(|e| matches!(
                e,
                SessionEffect::QueueNotifications {
                    open_start_after_toast: true,
                    ..
                }
            )),
            "Bluetooth arrival should latch an open in Bluetooth mode"
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
        let mut session = session_with(false, StartAutoOpen::Any);
        let effects = session.apply_controllers(vec![bt_pad("aa")], ctx(false));
        assert!(
            has_popup_sync(&effects),
            "connect must emit SyncPopup, got {effects:?}"
        );
    }

    #[test]
    fn membership_loss_emits_popup_sync() {
        // 1→0 shrinks the popup by a row: the shell must resize while open.
        let mut session = session_with(false, StartAutoOpen::Any);
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
        let mut session = session_with(false, StartAutoOpen::Any);
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
        let mut session = session_with(false, StartAutoOpen::Any);
        let _ = session.apply_controllers(vec![bt_pad("aa")], ctx(false));
        let effects = session.apply_controllers(vec![bt_pad("aa")], ctx(false));
        assert!(
            !has_popup_sync(&effects),
            "equivalent snapshot must not emit SyncPopup, got {effects:?}"
        );
    }

    #[test]
    fn start_selection_defaults_to_empty() {
        let session = session_with(false, StartAutoOpen::Any);
        assert_eq!(session.start_selection, StartSelection::default());
        assert_eq!(session.start_selection.game_key, None);
        assert_eq!(session.start_selection.controller_key, None);
    }

    #[test]
    fn start_selection_remember_overwrites() {
        let mut session = session_with(false, StartAutoOpen::Any);
        session.remember_start_selection(StartSelection {
            game_key: Some("steam:440".to_string()),
            controller_key: Some("pad-a".to_string()),
        });
        assert_eq!(
            session.start_selection.game_key.as_deref(),
            Some("steam:440")
        );
        assert_eq!(
            session.start_selection.controller_key.as_deref(),
            Some("pad-a")
        );
        session.remember_start_selection(StartSelection {
            game_key: Some("manual:xyz".to_string()),
            controller_key: None,
        });
        assert_eq!(
            session.start_selection.game_key.as_deref(),
            Some("manual:xyz")
        );
        assert_eq!(session.start_selection.controller_key, None);
    }

    /// Regression for the 2026-10-14 incident (shell showed a stale remembered
    /// `%` after full-charge + USB unplug while service/disk were correct).
    ///
    /// Service truth: `95% Charging → 100% Complete → disconnected` must leave
    /// the remembered record at 100%, and the retained snapshot the shell
    /// renders from must carry that 100% — not the boot-time value.
    #[test]
    fn full_charge_then_unplug_snapshot_carries_100() {
        let mut session = session_with(false, StartAutoOpen::Any);
        // Boot-time state: pad remembered at 95%.
        session
            .known
            .remember(&usb_pad_at("pad-1", 95, PowerState::Charging));
        let rev_boot = session.snapshot_rev();
        let snap_boot = session.snapshot();
        assert_eq!(
            snap_boot
                .remembered
                .iter()
                .find(|r| r.serial == "pad-1")
                .map(|r| r.percent),
            Some(95),
            "boot snapshot reflects the boot-time remembered %"
        );

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

        let snap = session.snapshot();
        assert!(
            snap.rev > rev_boot,
            "snapshot rev must advance past boot ({} <= {})",
            snap.rev,
            rev_boot
        );
        assert_eq!(
            snap.remembered
                .iter()
                .find(|r| r.serial == "pad-1")
                .map(|r| r.percent),
            Some(100),
            "shell-facing snapshot must carry 100, not the boot-time 95"
        );
        assert!(snap.live.is_empty(), "pad was unplugged");
    }

    #[test]
    fn snapshot_rev_bumps_on_selection_and_prefs() {
        let mut session = session_with(false, StartAutoOpen::Any);
        let rev0 = session.snapshot_rev();
        session.remember_start_selection(StartSelection {
            game_key: Some("steam:440".to_string()),
            controller_key: None,
        });
        assert!(session.snapshot_rev() > rev0);
        assert_eq!(
            session.snapshot().selection.game_key.as_deref(),
            Some("steam:440")
        );
        // Same value again: no bump.
        let rev1 = session.snapshot_rev();
        session.remember_start_selection(StartSelection {
            game_key: Some("steam:440".to_string()),
            controller_key: None,
        });
        assert_eq!(session.snapshot_rev(), rev1);

        // Prefs change bumps; identical prefs do not.
        let mut prefs = session.prefs.clone();
        prefs.notify_low = !prefs.notify_low;
        session.apply_prefs(prefs);
        assert!(session.snapshot_rev() > rev1);
        let rev2 = session.snapshot_rev();
        session.apply_prefs(session.prefs.clone());
        assert_eq!(session.snapshot_rev(), rev2);
    }

    #[test]
    fn snapshot_round_trips_over_json() {
        // The snapshot is the retained IPC payload: it must survive a
        // service→shell JSON hop with defaults intact.
        let mut session = session_with(false, StartAutoOpen::Any);
        session
            .known
            .remember(&usb_pad_at("pad-1", 100, PowerState::Complete));
        let snap = session.snapshot();
        let encoded = serde_json::to_vec(&snap).expect("encode snapshot");
        let decoded: SessionSnapshot = serde_json::from_slice(&encoded).expect("decode snapshot");
        assert_eq!(decoded.rev, snap.rev);
        assert_eq!(decoded.remembered.len(), snap.remembered.len());
        assert_eq!(decoded.remembered[0].percent, 100);
        // Sparse payloads decode with defaults (forward-compat).
        let sparse: SessionSnapshot = serde_json::from_str("{}").expect("sparse");
        assert_eq!(sparse.rev, 0);
        assert!(sparse.live.is_empty());
    }

    #[test]
    fn set_remembered_resolves_against_service_live() {
        let mut session = session_with(false, StartAutoOpen::Any);
        let live = vec![usb_pad_at("pad-1", 60, PowerState::Discharging)];
        // Remembering a live pad stores it; unknown serials are a no-op.
        assert!(session.set_remembered("pad-1", true, &live));
        assert!(session.known.is_remembered("pad-1"));
        assert!(!session.set_remembered("ghost", true, &live));
        assert!(!session.known.is_remembered("ghost"));
        // Forgetting drops the record (re-add would need live again).
        assert!(session.set_remembered("pad-1", false, &live));
        assert!(!session.known.is_remembered("pad-1"));
        assert!(!session.set_remembered("pad-1", false, &live));
    }

    #[test]
    fn set_nickname_is_absolute() {
        let mut session = session_with(false, StartAutoOpen::Any);
        assert!(session.set_nickname("pad-1", Some("Couch".into())));
        assert_eq!(session.known.nickname("pad-1"), Some("Couch"));
        assert!(!session.set_nickname("pad-1", Some("Couch".into())));
        assert!(session.set_nickname("pad-1", None));
        assert_eq!(session.known.nickname("pad-1"), None);
    }

    #[test]
    fn echo_intent_advances_rev_for_noop_convergence() {
        // Even a no-op intent must echo a newer rev so an optimistic shell
        // reconciles to service truth instead of sticking.
        let mut session = session_with(false, StartAutoOpen::Any);
        let rev = session.snapshot_rev();
        session.echo_intent();
        assert!(session.snapshot_rev() > rev);
    }
}
