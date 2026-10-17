//! WGPU adapter logging and panic-context stamps for multi-window atlas investigation.
//!
//! iced owns the real Device; we cannot install its uncaptured-error handler without a
//! fork. We enumerate adapters at boot and stamp UI lifecycle timestamps so a
//! `create_renderer` / atlas panic log can be correlated with toast/Start phase.

use crate::platform::app_log;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// Backend list applied on Windows when `WGPU_BACKEND` is unset.
///
/// Vulkan first, DX12 fallback. wgpu-core adds HAL instances in a fixed order
/// (Vulkan → Metal → DX12 → GL) and stable-sorts adapters by device type, so for
/// the same GPU Vulkan wins; when Vulkan is missing or cannot present to the
/// window surface, `request_adapter` falls through to DX12 on its own. Only
/// Vulkan composites per-pixel alpha for HWND surfaces — see
/// `notes/transparent-windows-dx12.md`.
#[cfg(windows)]
const PREFERRED_BACKENDS: &str = "vulkan,dx12";

/// True until boot proves the selected backend is opaque-only (DX12/GL on Windows).
static ALPHA_COMPOSITE: AtomicBool = AtomicBool::new(true);

/// Whether transparent windows are expected to composite per-pixel alpha.
///
/// Predicted at boot from the backend iced will select (see
/// [`log_adapters_at_boot`]). When false, rounded corners must come from the
/// OS (`CornerPreference::Round`) on opaque windows instead of alpha.
pub fn alpha_composite() -> bool {
    ALPHA_COMPOSITE.load(Ordering::Relaxed)
}

/// Whether a backend can present per-pixel alpha on a plain window surface.
fn backend_composites_alpha(backend: wgpu::Backend) -> bool {
    match backend {
        // wgpu-hal advertises only `Opaque` for DX12 HWND surfaces; GL presents
        // opaque on Windows in our probes.
        wgpu::Backend::Dx12 => false,
        wgpu::Backend::Gl => !cfg!(windows),
        _ => true,
    }
}

/// Mirror of wgpu-core's `request_adapter` device-type ordering.
fn device_type_order(device_type: wgpu::DeviceType, prefer_integrated: bool) -> u8 {
    match device_type {
        wgpu::DeviceType::DiscreteGpu if prefer_integrated => 2,
        wgpu::DeviceType::IntegratedGpu if prefer_integrated => 1,
        wgpu::DeviceType::DiscreteGpu => 1,
        wgpu::DeviceType::IntegratedGpu => 2,
        wgpu::DeviceType::Other => 3,
        wgpu::DeviceType::VirtualGpu => 4,
        wgpu::DeviceType::Cpu => 5,
    }
}

/// Pick the adapter iced is expected to select (no surface — best effort).
///
/// `adapters` must be in wgpu enumeration order; the sort is stable like
/// wgpu-core's, so backend order breaks device-type ties.
fn predict_selected(adapters: &[wgpu::AdapterInfo], prefer_integrated: bool) -> Option<usize> {
    let mut order: Vec<usize> = (0..adapters.len()).collect();
    order.sort_by_key(|&i| device_type_order(adapters[i].device_type, prefer_integrated));
    order.first().copied()
}

/// Choose the wgpu backend list, enumerate adapters, and predict alpha support.
///
/// Call once before the iced event loop (single-threaded: sets `WGPU_BACKEND`).
pub fn log_adapters_at_boot() {
    #[cfg(windows)]
    {
        if std::env::var_os("WGPU_BACKEND").is_none() {
            // SAFETY: single-threaded before iced::run; no concurrent env readers yet.
            unsafe {
                std::env::set_var("WGPU_BACKEND", PREFERRED_BACKENDS);
            }
            app_log::info(format!(
                "wgpu-diag: prefer WGPU_BACKEND={PREFERRED_BACKENDS} (unset previously)"
            ));
        } else {
            app_log::info(format!(
                "wgpu-diag: WGPU_BACKEND override={:?}",
                std::env::var("WGPU_BACKEND").unwrap_or_default()
            ));
        }
    }

    // Same resolution iced uses: env list, else all backends.
    let backends = wgpu::Backends::from_env().unwrap_or(wgpu::Backends::all());
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends,
        ..Default::default()
    });
    let adapters: Vec<wgpu::AdapterInfo> = instance
        .enumerate_adapters(backends)
        .iter()
        .map(wgpu::Adapter::get_info)
        .collect();
    if adapters.is_empty() {
        app_log::warn("wgpu-diag: no adapters enumerated");
        return;
    }
    for (i, info) in adapters.iter().enumerate() {
        app_log::info(format!(
            "wgpu-diag: adapter[{i}] name={:?} backend={:?} device={:?} vendor={:#x} device_type={:?}",
            info.name, info.backend, info.device, info.vendor, info.device_type
        ));
    }

    // iced: `PowerPreference::from_env().unwrap_or(HighPerformance)`.
    let prefer_integrated = matches!(
        wgpu::PowerPreference::from_env(),
        Some(wgpu::PowerPreference::LowPower)
    );
    if let Some(i) = predict_selected(&adapters, prefer_integrated) {
        let info = &adapters[i];
        let alpha = backend_composites_alpha(info.backend);
        ALPHA_COMPOSITE.store(alpha, Ordering::Relaxed);
        app_log::info(format!(
            "wgpu-diag: predicted adapter[{i}] backend={:?} name={:?} alpha_composite={alpha}",
            info.backend, info.name
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(backend: wgpu::Backend, device_type: wgpu::DeviceType) -> wgpu::AdapterInfo {
        wgpu::AdapterInfo {
            name: format!("{backend:?}"),
            vendor: 0,
            device: 0,
            device_type,
            driver: String::new(),
            driver_info: String::new(),
            backend,
        }
    }

    #[test]
    fn same_gpu_prefers_first_enumerated_backend() {
        let adapters = [
            info(wgpu::Backend::Vulkan, wgpu::DeviceType::DiscreteGpu),
            info(wgpu::Backend::Dx12, wgpu::DeviceType::DiscreteGpu),
        ];
        assert_eq!(predict_selected(&adapters, false), Some(0));
    }

    #[test]
    fn discrete_dx12_beats_integrated_vulkan_on_high_performance() {
        let adapters = [
            info(wgpu::Backend::Vulkan, wgpu::DeviceType::IntegratedGpu),
            info(wgpu::Backend::Dx12, wgpu::DeviceType::DiscreteGpu),
        ];
        assert_eq!(predict_selected(&adapters, false), Some(1));
        assert_eq!(predict_selected(&adapters, true), Some(0));
    }

    #[test]
    fn cpu_vulkan_loses_to_hardware_dx12() {
        let adapters = [
            info(wgpu::Backend::Vulkan, wgpu::DeviceType::Cpu),
            info(wgpu::Backend::Dx12, wgpu::DeviceType::IntegratedGpu),
        ];
        assert_eq!(predict_selected(&adapters, false), Some(1));
    }

    #[test]
    fn dx12_is_opaque_vulkan_composites() {
        assert!(!backend_composites_alpha(wgpu::Backend::Dx12));
        assert!(backend_composites_alpha(wgpu::Backend::Vulkan));
    }

    #[test]
    fn empty_adapter_list_predicts_nothing() {
        assert_eq!(predict_selected(&[], false), None);
    }
}
