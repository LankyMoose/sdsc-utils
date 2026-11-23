//! Home-owned install policy shared by every managed-app installer.
//!
//! The standalone Home (not an individual app) owns the download bandwidth cap:
//! it applies to the Browser engine today and to any future app installed from
//! the same standalone catalog. Installers receive the live [`AtomicU32`]
//! handle, so changing the cap in Home Settings takes effect at the next chunk
//! boundary even while an install is running.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// Download caps offered in Home Settings (`0` = unlimited).
pub const DOWNLOAD_LIMIT_OPTIONS_KBPS: [u32; 4] = [0, 2 * 1024, 5 * 1024, 10 * 1024];

/// Live install policy owned by the shell. Cheap to clone through the shared
/// atomic; the installer only ever reads it.
#[derive(Debug, Clone)]
pub struct AppInstallPolicy {
    limit_kbps: Arc<AtomicU32>,
}

impl AppInstallPolicy {
    pub fn new(limit_kbps: u32) -> Self {
        Self {
            limit_kbps: Arc::new(AtomicU32::new(clamp_download_limit(limit_kbps))),
        }
    }

    pub fn limit(&self) -> u32 {
        self.limit_kbps.load(Ordering::Relaxed)
    }

    pub fn set(&self, limit_kbps: u32) {
        self.limit_kbps
            .store(clamp_download_limit(limit_kbps), Ordering::Relaxed);
    }

    /// Handle passed to an installer worker.
    #[cfg_attr(not(feature = "cef-engine"), allow(dead_code))]
    pub fn handle(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.limit_kbps)
    }
}

impl Default for AppInstallPolicy {
    fn default() -> Self {
        Self::new(0)
    }
}

/// Label for a cap value in Home Settings.
pub fn download_limit_label(kbps: u32) -> String {
    if kbps == 0 {
        "Unlimited".to_string()
    } else if kbps.is_multiple_of(1024) {
        format!("{} MB/s", kbps / 1024)
    } else {
        format!("{kbps} KB/s")
    }
}

/// Snap an arbitrary cap to the supported option list.
pub fn clamp_download_limit(kbps: u32) -> u32 {
    DOWNLOAD_LIMIT_OPTIONS_KBPS
        .iter()
        .copied()
        .min_by_key(|option| option.abs_diff(kbps))
        .unwrap_or(0)
}

/// Step through the supported caps, wrapping. `delta > 0` moves forward.
pub fn step_download_limit(kbps: u32, delta: i32) -> u32 {
    if delta == 0 {
        return clamp_download_limit(kbps);
    }
    let current = clamp_download_limit(kbps);
    let index = DOWNLOAD_LIMIT_OPTIONS_KBPS
        .iter()
        .position(|option| *option == current)
        .unwrap_or(0) as i32;
    let next = (index + delta.signum()).rem_euclid(DOWNLOAD_LIMIT_OPTIONS_KBPS.len() as i32);
    DOWNLOAD_LIMIT_OPTIONS_KBPS[next as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_unlimited_and_formats_caps() {
        assert_eq!(AppInstallPolicy::default().limit(), 0);
        assert_eq!(download_limit_label(0), "Unlimited");
        assert_eq!(download_limit_label(2 * 1024), "2 MB/s");
        assert_eq!(download_limit_label(10 * 1024), "10 MB/s");
    }

    #[test]
    fn clamps_to_supported_caps() {
        assert_eq!(clamp_download_limit(3 * 1024), 2 * 1024);
        assert_eq!(clamp_download_limit(8 * 1024), 10 * 1024);
        assert_eq!(clamp_download_limit(u32::MAX), 10 * 1024);
    }

    #[test]
    fn step_wraps_both_ways() {
        assert_eq!(step_download_limit(0, 1), 2 * 1024);
        assert_eq!(step_download_limit(2 * 1024, -1), 0);
        assert_eq!(step_download_limit(10 * 1024, 1), 0);
        assert_eq!(step_download_limit(0, -1), 10 * 1024);
        assert_eq!(step_download_limit(0, 0), 0);
    }

    #[test]
    fn live_handle_sees_policy_updates() {
        let policy = AppInstallPolicy::new(0);
        let handle = policy.handle();
        assert_eq!(handle.load(Ordering::Relaxed), 0);
        policy.set(5 * 1024);
        assert_eq!(handle.load(Ordering::Relaxed), 5 * 1024);
        assert_eq!(policy.limit(), 5 * 1024);
        policy.set(3 * 1024);
        assert_eq!(handle.load(Ordering::Relaxed), 2 * 1024);
    }
}
