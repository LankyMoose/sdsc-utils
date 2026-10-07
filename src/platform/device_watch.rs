//! OS device arrival/removal watcher (trigger, not truth).
//!
//! Fires `on_event` when HID hardware arrives or leaves. The hid-worker treats
//! it as a hint: refresh the device list (throttled); enumeration plus the
//! session gates still decide truth. Platforms without a watcher return `false`
//! and rely on the worker heartbeat cap (`HOT_ENUM_MAX_DEFER`).
//!
//! See `notes/device-arrival-watch.md` (plan) and
//! `notes/hot-enumeration-freeze.md` (incident that motivated this).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Burst window: repeat arrivals within this collapse into the leading edge
/// (BT pairing and USB+BT dual interfaces arrive as bursts).
pub const ARRIVAL_DEBOUNCE: Duration = Duration::from_secs(1);
/// Follow-up refresh after a suppressed burst, so a second pad arriving inside
/// the debounce window is still picked up quickly (dual-connect onboarding).
const TRAILING_DELAY: Duration = Duration::from_millis(1500);

/// Debounce outcome (pure, unit-tested).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DebounceDecision {
    /// Outside the window: fire now.
    Fire,
    /// Inside the window, follow-up already scheduled: drop.
    Suppress,
    /// Inside the window, nothing scheduled yet: drop + schedule follow-up.
    SuppressWithTrailing,
}

fn debounce_decision(
    last_fire: Option<Instant>,
    trailing_pending: bool,
    now: Instant,
) -> DebounceDecision {
    if last_fire.is_none_or(|at| now.saturating_duration_since(at) >= ARRIVAL_DEBOUNCE) {
        DebounceDecision::Fire
    } else if trailing_pending {
        DebounceDecision::Suppress
    } else {
        DebounceDecision::SuppressWithTrailing
    }
}

/// Leading-edge debouncer around an arrival callback with a trailing follow-up:
/// first event fires, repeats within [`ARRIVAL_DEBOUNCE`] collapse into one
/// delayed re-fire [`TRAILING_DELAY`] later. Pure apart from the clock.
pub fn debounced(on_event: impl Fn() + Send + Sync + 'static) -> impl Fn() + Send + 'static {
    struct State {
        last_fire: Option<Instant>,
        trailing_pending: bool,
    }
    let fire: Arc<dyn Fn() + Send + Sync> = Arc::new(on_event);
    let state = Arc::new(Mutex::new(State {
        last_fire: None,
        trailing_pending: false,
    }));
    move || {
        let decision = state
            .lock()
            .map(|mut guard| {
                let decision =
                    debounce_decision(guard.last_fire, guard.trailing_pending, Instant::now());
                match decision {
                    DebounceDecision::Fire => {
                        guard.last_fire = Some(Instant::now());
                        guard.trailing_pending = false;
                    }
                    DebounceDecision::SuppressWithTrailing => {
                        guard.trailing_pending = true;
                    }
                    DebounceDecision::Suppress => {}
                }
                decision
            })
            .unwrap_or(DebounceDecision::Suppress);
        if decision != DebounceDecision::SuppressWithTrailing {
            if decision == DebounceDecision::Fire {
                fire();
            }
            return;
        }
        let fire = Arc::clone(&fire);
        let state = Arc::clone(&state);
        let _ = std::thread::Builder::new()
            .name("device-watch-trailing".into())
            .spawn(move || {
                std::thread::sleep(TRAILING_DELAY);
                if let Ok(mut guard) = state.lock() {
                    guard.trailing_pending = false;
                }
                fire();
            });
    }
}

/// /dev node families backing gamepads (USB + BlueZ BT hidraw, evdev).
/// Used by the Linux snapshot watcher; kept at top level so it is testable on
/// every platform.
pub fn node_relevant(name: &str) -> bool {
    name.starts_with("hidraw") || name.starts_with("event")
}

/// Spawn the OS arrival/removal watcher; `true` when one is running.
/// macOS and other platforms have no watcher yet (heartbeat only).
pub fn spawn_arrival_watcher(on_event: impl Fn() + Send + Sync + 'static) -> bool {
    let fire = debounced(on_event);
    #[cfg(windows)]
    let watched = windows::spawn(fire);
    #[cfg(target_os = "linux")]
    let watched = linux::spawn(fire);
    #[cfg(not(any(windows, target_os = "linux")))]
    let watched = {
        let _ = fire;
        false
    };
    watched
}

/// Windows: `CM_Register_Notification` on the HID interface class. Callback
/// based (no message window/pump); the callback runs on a system thread.
/// Process-lifetime registration: the context is intentionally leaked, matching
/// the daemon's lifetime (precedent: hitch hotkey worker).
#[cfg(windows)]
mod windows {
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        CM_NOTIFY_ACTION, CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL,
        CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL, CM_NOTIFY_EVENT_DATA, CM_NOTIFY_FILTER,
        CM_NOTIFY_FILTER_0, CM_NOTIFY_FILTER_0_0, CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
        CM_Register_Notification, CR_SUCCESS, HCMNOTIFICATION,
    };
    use windows::Win32::Devices::HumanInterfaceDevice::GUID_DEVINTERFACE_HID;

    unsafe extern "system" fn on_notify(
        _hnotify: HCMNOTIFICATION,
        context: *const core::ffi::c_void,
        action: CM_NOTIFY_ACTION,
        _eventdata: *const CM_NOTIFY_EVENT_DATA,
        _eventdatasize: u32,
    ) -> u32 {
        if action == CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL
            || action == CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL
        {
            // SAFETY: `context` is the leaked callback box from `spawn`.
            let fire = unsafe { &*(context as *const Box<dyn Fn() + Send>) };
            fire();
        }
        0
    }

    pub fn spawn(fire: impl Fn() + Send + 'static) -> bool {
        let context: Box<Box<dyn Fn() + Send>> = Box::new(Box::new(fire));
        let context_raw = Box::into_raw(context) as *const core::ffi::c_void;
        // SAFETY: union init + registration; `context_raw` outlives the process.
        let registered = unsafe {
            let filter = CM_NOTIFY_FILTER {
                cbSize: std::mem::size_of::<CM_NOTIFY_FILTER>() as u32,
                Flags: 0,
                FilterType: CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
                Reserved: 0,
                u: CM_NOTIFY_FILTER_0 {
                    DeviceInterface: CM_NOTIFY_FILTER_0_0 {
                        ClassGuid: GUID_DEVINTERFACE_HID,
                    },
                },
            };
            let mut notify = HCMNOTIFICATION(std::ptr::null_mut());
            CM_Register_Notification(&filter, Some(context_raw), Some(on_notify), &mut notify)
                == CR_SUCCESS
        };
        if !registered {
            // SAFETY: registration failed; reclaim the leaked context.
            unsafe {
                drop(Box::from_raw(context_raw as *mut Box<dyn Fn() + Send>));
            }
            crate::platform::app_log::warn("device watch: CM_Register_Notification failed");
        }
        registered
    }
}

/// Linux: std-only `/dev` snapshot watcher (no libudev dependency).
/// Arrival/removal of `hidraw*` / `event*` nodes fires the callback at ~1s
/// precision with zero HID I/O — plenty for a refresh trigger.
#[cfg(target_os = "linux")]
mod linux {
    use super::node_relevant;
    use std::collections::HashSet;

    fn snapshot() -> HashSet<String> {
        std::fs::read_dir("/dev")
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| node_relevant(name))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn spawn(fire: impl Fn() + Send + 'static) -> bool {
        std::thread::Builder::new()
            .name("device-watch".into())
            .spawn(move || {
                let mut last = snapshot();
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    let now = snapshot();
                    if now != last {
                        last = now;
                        fire();
                    }
                }
            })
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounce_fires_leading_edge_then_suppresses_burst() {
        let calls = Arc::new(Mutex::new(0u32));
        let fire = debounced({
            let calls = Arc::clone(&calls);
            move || {
                *calls.lock().unwrap() += 1;
            }
        });
        fire();
        fire();
        fire();
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[test]
    fn debounce_decision_covers_window_and_trailing() {
        use DebounceDecision::{Fire, Suppress, SuppressWithTrailing};
        let now = Instant::now();
        assert_eq!(debounce_decision(None, false, now), Fire);
        assert_eq!(
            debounce_decision(Some(now), false, now),
            SuppressWithTrailing
        );
        assert_eq!(debounce_decision(Some(now), true, now), Suppress);
        assert_eq!(
            debounce_decision(Some(now - ARRIVAL_DEBOUNCE), false, now),
            Fire
        );
    }

    #[test]
    fn node_relevant_matches_gamepad_nodes_only() {
        assert!(node_relevant("hidraw0"));
        assert!(node_relevant("hidraw17"));
        assert!(node_relevant("event5"));
        assert!(!node_relevant("hiddev0"));
        assert!(!node_relevant("null"));
        assert!(!node_relevant(""));
    }
}
