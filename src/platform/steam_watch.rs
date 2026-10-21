//! Steam `Apps` registry watcher (trigger, not truth).
//!
//! Fires `on_event` when anything under `HKCU\Software\Valve\Steam\Apps`
//! changes (per-app `Running` flips, installs, updates). The badge state
//! machine treats it as a hint: re-read the session appid's `Running` value
//! plus one process snapshot (throttled); the decide logic still rules truth.
//! A failed launch never writes anything, so the launch-fail timeout remains
//! the backstop — this only makes real launches confirm/clear fast without
//! background polling.
//!
//! Windows-only (the flag is Windows-client-only). Other platforms return
//! `None` and keep the polling behavior.

#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

/// Guard; dropping it stops the watcher thread.
pub struct SteamAppsWatch {
    #[cfg(windows)]
    stop: std::sync::Arc<std::os::windows::io::OwnedHandle>,
}

#[cfg(windows)]
impl Drop for SteamAppsWatch {
    fn drop(&mut self) {
        // SAFETY: `stop` is a live event handle; signaling wakes the
        // watcher's wait so the thread exits and closes its own handles.
        // (`::windows` — the leading `::` sidesteps this file's `mod windows`.)
        unsafe {
            let _ = ::windows::Win32::System::Threading::SetEvent(
                ::windows::Win32::Foundation::HANDLE(self.stop.as_raw_handle()),
            );
        }
    }
}

/// Spawn the watcher; `None` when there is nothing to watch (non-Windows,
/// Steam not installed, key unopenable). The thread ends when the guard drops.
pub fn spawn_steam_apps_watcher(on_event: impl Fn() + Send + 'static) -> Option<SteamAppsWatch> {
    #[cfg(windows)]
    return windows::spawn(on_event);
    #[cfg(not(windows))]
    {
        let _ = on_event;
        None
    }
}

/// Windows: `RegNotifyChangeKeyValue` on the `Apps` key (subtree) with a stop
/// event multiplexed into the wait, so dropping the guard ends the thread.
/// No polling, no window/message pump.
#[cfg(windows)]
mod windows {
    use super::SteamAppsWatch;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::sync::Arc;
    use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE};
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_NOTIFY, REG_NOTIFY_CHANGE_LAST_SET, REG_NOTIFY_CHANGE_NAME,
        RegCloseKey, RegNotifyChangeKeyValue, RegOpenKeyExW,
    };
    use windows::Win32::System::Threading::{
        CreateEventW, INFINITE, ResetEvent, WaitForMultipleObjects,
    };
    use windows::core::w;

    pub fn spawn(on_event: impl Fn() + Send + 'static) -> Option<SteamAppsWatch> {
        // SAFETY: raw registry/event calls below; ownership rules noted inline.
        unsafe {
            let mut key = HKEY::default();
            if RegOpenKeyExW(
                HKEY_CURRENT_USER,
                w!("Software\\Valve\\Steam\\Apps"),
                None,
                KEY_NOTIFY,
                &mut key,
            ) != ERROR_SUCCESS
            {
                return None;
            }
            // `key` must stay open for the registration; the thread owns and
            // closes it. `usize` crosses the thread boundary (no Send questions).
            let key_usize = key.0 as usize;

            let make_event = || -> Option<OwnedHandle> {
                let handle = CreateEventW(None, true, false, None).ok()?;
                // SAFETY: freshly created event, ownership moved in.
                Some(OwnedHandle::from_raw_handle(handle.0))
            };
            let stop = Arc::new(make_event()?);
            let notify = make_event()?;
            let stop_thread = Arc::clone(&stop);

            let watched = std::thread::Builder::new()
                .name("steam-apps-watch".into())
                .spawn(move || {
                    // SAFETY: handles owned by this thread (moved in).
                    let key = HKEY(key_usize as *mut core::ffi::c_void);
                    let stop = HANDLE(stop_thread.as_raw_handle());
                    let notify_handle = HANDLE(notify.as_raw_handle());
                    loop {
                        let _ = ResetEvent(notify_handle);
                        if RegNotifyChangeKeyValue(
                            key,
                            true,
                            REG_NOTIFY_CHANGE_NAME | REG_NOTIFY_CHANGE_LAST_SET,
                            Some(notify_handle),
                            true,
                        ) != ERROR_SUCCESS
                        {
                            crate::platform::app_log::warn(
                                "steam watch: RegNotifyChangeKeyValue failed",
                            );
                            break;
                        }
                        // Index 0 = stop, 1 = registry change.
                        let waited =
                            WaitForMultipleObjects(&[stop, notify_handle], false, INFINITE);
                        if waited.0 != 1 {
                            break;
                        }
                        on_event();
                    }
                    let _ = RegCloseKey(key);
                    // `notify` + `stop_thread` Arc drops here: events closed.
                })
                .is_ok();
            if !watched {
                let _ = RegCloseKey(key);
                return None;
            }
            Some(SteamAppsWatch { stop })
        }
    }
}
