//! WGPU adapter logging and panic-context stamps for multi-window atlas investigation.
//!
//! iced owns the real Device; we cannot install its uncaptured-error handler without a
//! fork. We enumerate adapters at boot and stamp UI lifecycle timestamps so a
//! `create_renderer` / atlas panic log can be correlated with toast/Start phase.

use crate::platform::app_log;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static LAST_START_OPEN_MS: AtomicU64 = AtomicU64::new(0);
static LAST_TOAST_HIDE_MS: AtomicU64 = AtomicU64::new(0);
static LAST_TOAST_REMOUNT_MS: AtomicU64 = AtomicU64::new(0);

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn note_start_open() {
    LAST_START_OPEN_MS.store(epoch_ms(), Ordering::Relaxed);
}

pub fn note_toast_hide() {
    LAST_TOAST_HIDE_MS.store(epoch_ms(), Ordering::Relaxed);
}

pub fn note_toast_remount() {
    LAST_TOAST_REMOUNT_MS.store(epoch_ms(), Ordering::Relaxed);
}

/// Clear lifecycle stamps (process Exit).
pub fn reset() {
    LAST_START_OPEN_MS.store(0, Ordering::Relaxed);
    LAST_TOAST_HIDE_MS.store(0, Ordering::Relaxed);
    LAST_TOAST_REMOUNT_MS.store(0, Ordering::Relaxed);
}

/// One-line snapshot for panic / diag correlation.
pub fn context_line() -> String {
    let now = epoch_ms();
    let age = |stamp: u64| -> String {
        if stamp == 0 {
            "never".into()
        } else {
            format!("{}ms", now.saturating_sub(stamp))
        }
    };
    format!(
        "wgpu-diag: last_start_open={} last_toast_hide={} last_toast_remount={}",
        age(LAST_START_OPEN_MS.load(Ordering::Relaxed)),
        age(LAST_TOAST_HIDE_MS.load(Ordering::Relaxed)),
        age(LAST_TOAST_REMOUNT_MS.load(Ordering::Relaxed)),
    )
}

/// Enumerate adapters (best-effort). Call once before the iced event loop.
pub fn log_adapters_at_boot() {
    // Prefer DX12 on Windows for the process (wgpu reads this before instance create).
    // iced creates its own Instance after this; setting early biases backend choice.
    #[cfg(windows)]
    {
        if std::env::var_os("WGPU_BACKEND").is_none() {
            // SAFETY: single-threaded before iced::run; no concurrent env readers yet.
            unsafe {
                std::env::set_var("WGPU_BACKEND", "dx12");
            }
            app_log::info("wgpu-diag: prefer WGPU_BACKEND=dx12 (unset previously)");
        }
    }

    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..Default::default()
    });
    let adapters: Vec<_> = instance.enumerate_adapters(wgpu::Backends::all());
    if adapters.is_empty() {
        app_log::warn("wgpu-diag: no adapters enumerated");
        return;
    }
    for (i, adapter) in adapters.iter().enumerate() {
        let info = adapter.get_info();
        app_log::info(format!(
            "wgpu-diag: adapter[{i}] name={:?} backend={:?} device={:?} vendor={:#x} device_type={:?}",
            info.name, info.backend, info.device, info.vendor, info.device_type
        ));
    }
}
