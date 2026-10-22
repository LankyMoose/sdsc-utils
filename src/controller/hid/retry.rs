//! Central retry/fail policy: one error taxonomy, per-path attempt tables.
//!
//! Why a new module instead of extending `hid::policy`: `policy::Policy`
//! owns *cadences* (durations / counts consumed via `Policy::default()`).
//! This module owns *failure classification* (`HID error text ->
//! [`ErrorKind`]) — a different axis. Merging them would conflate timing
//! config with the transient/fatal taxonomy, so `retry` stays separate and
//! only routes the existing string-classified decision through one
//! classifier. No threshold, attempt count, TTL, log string, or control
//! flow changed here.
//!
//! # Policy table (verified against the code cited; behavior unchanged)
//!
//! | Path | Attempts | Backoff | On final failure |
//! |------|----------|---------|--------------------|
//! | Lightbar timed apply (`dualsense::lightbar::apply_lightbar_rgb_timed`) | 1 + 1 retry iff [`is_transient`], 75ms `WRITE_RETRY_DELAY` sleep between | none | `Err`; caller warns (`worker::write_rgb_exclusive` SetRgb arm: `lightbar write failed for {serial}`; `poll::reassert_lightbar_for_pads`: `failed to set lightbar on {product}`) |
//! | Lightbar bulk apply-all (`dualsense::lightbar::apply_lightbar_all_timed`, CLI/debug) | 1 per pad, no retry | none | per-pad `app_log::warn` (`lightbar open failed` / `lightbar write failed`), continue; returns applied count |
//! | `worker::write_rgb_exclusive` (SetRgb / Identify / pulse) | `backend.apply` then, on ANY error, one device-list refresh + full re-apply (each apply carries its own transient retry) | none | `Err` → `identify failed for {serial}` / `lightbar write failed for {serial}` warn; Identify aborts the session |
//! | Rumble pulse (`worker::write_rumble_pulse`) | up to 2 exclusive-handle attempts (drop + re-probe between, unconditional — NOT gated on [`is_transient`]) then one input-handle fallback write | none | per-attempt `diag_warn` only (`hid-diag: rumble write/open fail ...`, debug-only); one-time `app_log::warn` on input-handle fallback; final `Err` → `diag_warn` (`rumble write failed for {target}`, debug-only, no `app.log` line) |
//! | Rumble stop (`stop_all_rumble` / `stop_rumble_for_serial` / `expire_rumble`) | 1, best-effort, result discarded (`let _ =`) | none | silent |
//! | Battery full (`dualsense::battery::read_battery`, 6 attempts): full poll (`poll::poll_controllers_with_api`, `worker::poll_incremental` full probe) + new/unknown pads | up to 6 x 150ms `read_timeout`; `n == 0` or truncated-BT (one calibration feature request + 200ms sleep) continue; unexpected report id / short buffer / missing fields / unsupported bus / `read_timeout` error → immediate `Err` (fatal, no further attempts) | none | `Err` → poll warns `failed to read {product} (hid serial {hid})` + pad skipped this tick (session hold covers); fail-open |
//! | Battery fast (`dualsense::battery::read_battery_fast`, 2 attempts): incremental-poll stale-pad read on the cached handle (`worker::poll_incremental`) | up to 2, same continue/abort rules as full | none | `Err` → drop handle, full re-probe below in the same tick; re-probe miss skips the tick (session hold covers) |
//! | Open backoff (`worker::DeviceCache::ensure_all_pads` sampling loop only) | backed-off paths skipped for the window (see Backoff column) | skip after `OPEN_BACKOFF_THRESHOLD` (3) consecutive open failures for `OPEN_BACKOFF_WINDOW` (5s); records pruned when the path vanishes from the device list; any success clears | pad absent until re-probe |
//! | Open elsewhere (`lightbar::try_apply_by_serial_timed`, full + incremental poll probes, `power_off_bluetooth_*`, `worker::try_open_serial`) | no backoff, no retry: one open per listed device per pass | none | lightbar: skip device, continue; poll: `app_log::warn` (`failed to open {product} ...`) + skip; power-off: `app_log::warn` (`power-off: open failed for {path}`) + continue; `try_open_serial`: `None` → `controller {serial} not found` |
//! | Power-off (`dualsense::battery::power_off_bluetooth_*`) | every candidate interface x 2 CRC seeds x 3 report sizes until first `Ok` (no per-error retry, no backoff) | none | `Err` (`power-off feature report failed after N attempt(s): <last>`) → worker warns `power-off failed for {serial}`; fail-closed (pad stays on, error surfaced) |
//! | Sample reads (`hid::sample::drain_sample`) | structural, not string-classified: zero-timeout drain (cap `drain_cap`) + one `report_wait_ms` blocking read + drain; truncated-BT: one feature request then retry once | silence streak: 50 consecutive timeouts (`LIVE_SILENCE_TIMEOUTS`) drops the pad from the live map (handle stays open); non-timeout I/O error drops the handle at once (`HardFail`) | `Timeout` → keep handle, `diag_info` only; `Fail` → drop handle + forget live pad, `trace_read` diag-only — no `app.log` warn either way |
//! | `hid::diag::trace_open` / `trace_read` / `trace_write` | — | — | always `hid-trace.log` (no-op in release); `app.log` WARN only at the call sites above (poll open/read, `lightbar_all`, SetRgb/Identify, power-off, rumble fallback-once). Sample read failures, rumble write failures, and lightbar per-device opens stay diag-only. |
//!
//! # Remainders (deliberately NOT routed through [`is_transient`])
//!
//! - Rumble re-probe retries unconditionally (any write error, first attempt);
//!   gating it on [`ErrorKind`] would change which errors get a second chance.
//! - Power-off exhausts the size/seed matrix unconditionally; classifying each
//!   `send_feature_report` error could stop early — a behavior change.
//! - Open backoff counts every open failure; only counting transient ones would
//!   change ghost-path suppression.
//! - Battery/sample loops are structural (attempt counts + immediate-abort
//!   conditions, no error-text matching); there is no string decision to route.

use std::fmt::Display;

/// Failure taxonomy for HID errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Worth one more attempt: overlapped-I/O pending (`0x3E5` family),
    /// transient timeouts, or a pad that is simply not there yet.
    Transient,
    /// Retrying cannot help: unsupported reports, parse failures, and other
    /// terminal conditions.
    Fatal,
}

/// Classify raw HID error text.
///
/// Transient today = the exact `0x000003E5` / `WaitForSingleObject` /
/// `Overlapped I/O` family that historically meant "retry once after 75ms".
/// Everything else (not-found, unsupported report, short/truncated parse,
/// exhausted-attempt timeouts, power-off matrix failures) is fatal: the
/// per-path tables above already encode how many structural attempts each
/// such error gets, and none of them consult this classifier.
pub fn classify(text: &str) -> ErrorKind {
    if text.contains("0x000003E5")
        || text.contains("WaitForSingleObject")
        || text.contains("Overlapped I/O")
    {
        ErrorKind::Transient
    } else {
        ErrorKind::Fatal
    }
}

/// The ONE write-retry classifier. `true` iff [`classify`] says [`ErrorKind::Transient`].
///
/// `dualsense::lightbar::is_retryable_write_error` delegates here (thin
/// wrapper, same name/signature); no other path string-matches errors, per
/// the remainders above.
pub fn is_transient(err: &impl Display) -> bool {
    matches!(classify(&err.to_string()), ErrorKind::Transient)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_write_errors_are_retryable() {
        assert!(is_transient(
            &"hidapi error: hid_write/WaitForSingleObject: (0x000003E5) Overlapped I/O operation is in progress."
        ));
        assert!(is_transient(&"WaitForSingleObject timed out"));
        assert!(is_transient(&"Overlapped I/O operation is in progress"));
        assert!(is_transient(&"write failed: (0x000003E5) nope"));
        assert_eq!(
            classify("boom (0x000003E5) Overlapped I/O"),
            ErrorKind::Transient
        );
    }

    #[test]
    fn fatal_errors_are_not_retryable() {
        for fatal in [
            "controller not found",
            "unsupported connection type: Unknown",
            "unexpected report id 0x09 (expected 0x31)",
            "input report too short (12 bytes)",
            "input report missing battery fields",
            "timed out waiting for a DualSense input report with battery data",
            "hidapi error: open_device: file not found",
            "power-off feature report failed after 6 attempt(s): hidapi error: nope",
            "rumble: no handle for aabbcc",
            "",
        ] {
            assert!(!is_transient(&fatal), "must be fatal: {fatal:?}");
            assert_eq!(classify(fatal), ErrorKind::Fatal, "{fatal:?}");
        }
    }

    #[test]
    fn classify_and_is_transient_agree() {
        for text in [
            "ok",
            "0x000003E5",
            "WaitForSingleObject",
            "Overlapped I/O",
            "controller not found",
        ] {
            assert_eq!(
                is_transient(&text),
                matches!(classify(text), ErrorKind::Transient),
                "{text:?}"
            );
        }
    }

    /// Rumble outlier (`worker::write_rumble_pulse`): `for attempt in 0..2u8`
    /// retries the exclusive-handle write unconditionally on attempt 0, breaks
    /// on open failure; gating the `continue` on [`is_transient`] would narrow
    /// fatal writes 2 -> 1 (timing/behavior change), so it stays unwired.
    #[test]
    fn rumble_unconditional_reprobe_would_narrow_if_gated() {
        fn attempts_gated(write_err: &str) -> u8 {
            if is_transient(&write_err) { 2 } else { 1 }
        }
        const UNCONDITIONAL: u8 = 2;
        assert_eq!(
            attempts_gated(
                "hidapi error: hid_write/WaitForSingleObject: (0x000003E5) Overlapped I/O operation is in progress."
            ),
            UNCONDITIONAL
        );
        for fatal in [
            "hidapi error: hid_write failed: device not connected",
            "hidapi error: open_device: file not found",
            "rumble: no handle for aabbcc",
        ] {
            assert!(!is_transient(&fatal), "must stay fatal: {fatal:?}");
            assert_eq!(attempts_gated(fatal), 1, "{fatal:?}");
            assert_ne!(attempts_gated(fatal), UNCONDITIONAL, "{fatal:?}");
        }
    }

    /// Power-off outlier (`dualsense::battery::power_off_bluetooth_*`): every
    /// candidate interface x 2 CRC seeds x 3 report sizes until first `Ok`;
    /// aborting the sweep on the first fatal [`classify`] would narrow 6 -> 1
    /// per device (timing/behavior + final `after N attempt(s)` text change),
    /// so it stays unwired. Sizes/seeds are a variant sweep, not a retry.
    #[test]
    fn power_off_matrix_sweep_would_narrow_if_gated() {
        const SEEDS: usize = 2; // FEATURE_CRC32_SEEDS in battery.rs
        const SIZES: usize = 3; // BT_CONTROL_FEATURE_SIZE{,_ALT,_PADDED}
        fn attempts_unconditional(candidates: usize) -> usize {
            candidates * SEEDS * SIZES
        }
        const GATED_FIRST_FATAL: usize = 1;
        assert_eq!(attempts_unconditional(1), 6);
        for fatal in [
            "hidapi error: send_feature_report failed: access denied",
            "power-off: open failed for \\\\?\\hid#1: hidapi error: open_device: file not found",
            "power-off feature report failed after 6 attempt(s): hidapi error: nope",
        ] {
            assert!(!is_transient(&fatal), "must stay fatal: {fatal:?}");
            assert_eq!(classify(fatal), ErrorKind::Fatal, "{fatal:?}");
        }
        assert_ne!(GATED_FIRST_FATAL, attempts_unconditional(1));
    }

    /// Open-backoff outlier (`worker::DeviceCache::note_open_result`): counts
    /// every open failure; counting only transient ones would never suppress
    /// the Fatal ghost paths backoff exists for (`open_device: file not found`),
    /// widening opens to every sample. Stays unwired.
    #[test]
    fn open_backoff_unconditional_would_widen_if_gated() {
        assert_eq!(
            classify("hidapi error: open_device: file not found"),
            ErrorKind::Fatal
        );
        assert!(!is_transient(&"hidapi error: open_device: file not found"));
        fn counts_gated(err: &str) -> u32 {
            u32::from(is_transient(&err))
        }
        assert_eq!(counts_gated("hidapi error: open_device: file not found"), 0);
        assert_eq!(
            crate::controller::hid::policy::Policy::DEFAULT.open_backoff_threshold,
            3
        );
        assert_eq!(
            crate::controller::hid::policy::Policy::DEFAULT.open_backoff_window,
            std::time::Duration::from_secs(5)
        );
    }
}
