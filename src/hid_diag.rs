//! HID diagnostic helpers (debug builds only for the heavy path).
//!
//! Release (`not(debug_assertions)`): public API is callable no-ops so call sites
//! stay unchanged; no hid-trace I/O, watchdog, or Steam scans.

use hidapi::BusType;

/// Bus tag for compact hid-trace lines (cheap; available in all builds).
pub fn bus_tag(bus: BusType) -> &'static str {
    match bus {
        BusType::Usb => "usb",
        BusType::Bluetooth => "bt",
        _ => "?",
    }
}

/// Investigation-volume line → `app.log` INFO (no-op in release).
pub fn diag_info(message: impl AsRef<str>) {
    #[cfg(debug_assertions)]
    crate::app_log::info(message);
    #[cfg(not(debug_assertions))]
    let _ = message;
}

/// Investigation-volume line → `app.log` WARN (no-op in release).
#[allow(dead_code)]
pub fn diag_warn(message: impl AsRef<str>) {
    #[cfg(debug_assertions)]
    crate::app_log::warn(message);
    #[cfg(not(debug_assertions))]
    let _ = message;
}

#[cfg(debug_assertions)]
mod active {
    use super::bus_tag;
    use crate::app_log;
    use hidapi::DeviceInfo;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    const STEAM_REFRESH: Duration = Duration::from_secs(1);
    const SLOW_OP_MS: u128 = 50;
    const WATCHDOG_POLL: Duration = Duration::from_millis(100);

    static STEAM_CACHE: Mutex<SteamCache> = Mutex::new(SteamCache {
        steam: false,
        refreshed_at: None,
    });

    static PHASE: Mutex<WorkerPhase> = Mutex::new(WorkerPhase {
        op: "boot",
        entered_at: None,
        last_progress: None,
    });

    static WATCHDOG_STARTED: AtomicBool = AtomicBool::new(false);
    static PROGRESS_TICK: AtomicU64 = AtomicU64::new(0);

    struct SteamCache {
        steam: bool,
        refreshed_at: Option<Instant>,
    }

    struct WorkerPhase {
        op: &'static str,
        entered_at: Option<Instant>,
        last_progress: Option<Instant>,
    }

    pub fn device_fields(info: &DeviceInfo) -> String {
        let path = info.path().to_string_lossy();
        format!(
            "path={path} iface={} usage_page=0x{:04x} usage=0x{:04x} bus={}",
            info.interface_number(),
            info.usage_page(),
            info.usage(),
            bus_tag(info.bus_type())
        )
    }

    pub fn steam_present() -> bool {
        let now = Instant::now();
        if let Ok(mut guard) = STEAM_CACHE.lock() {
            if guard
                .refreshed_at
                .is_some_and(|t| now.duration_since(t) < STEAM_REFRESH)
            {
                return guard.steam;
            }
            let steam = {
                let _scan = enter_op("steam_process_scan");
                process_name_present("steam.exe")
            };
            guard.steam = steam;
            guard.refreshed_at = Some(now);
            return steam;
        }
        process_name_present("steam.exe")
    }

    fn process_name_present(needle: &str) -> bool {
        #[cfg(windows)]
        {
            crate::process_match::any_process_name_eq_ignore_case(needle)
        }
        #[cfg(not(windows))]
        {
            let _ = needle;
            false
        }
    }

    pub fn failure_context() -> String {
        let steam = u8::from(steam_present());
        #[cfg(windows)]
        {
            let fg = crate::process_match::foreground_process_name().unwrap_or_else(|| "?".into());
            let fs = u8::from(crate::start_input::foreground_is_exclusive_fullscreen());
            format!("steam={steam} fg={fg} fs={fs}")
        }
        #[cfg(not(windows))]
        {
            format!("steam={steam} fg=? fs=0")
        }
    }

    pub fn steam_tag() -> String {
        format!("steam={}", u8::from(steam_present()))
    }

    pub fn trace_open(
        caller: &str,
        info: &DeviceInfo,
        serial_hint: &str,
        ms: u128,
        result: Result<(), &str>,
    ) {
        let fields = device_fields(info);
        let serial = if serial_hint.is_empty() {
            "-"
        } else {
            serial_hint
        };
        match result {
            Ok(()) => app_log::hid_trace(format!(
                "open caller={caller} serial={serial} {fields} ms={ms} ok {}",
                steam_tag()
            )),
            Err(err) => app_log::hid_trace(format!(
                "open caller={caller} serial={serial} {fields} ms={ms} err={err} {}",
                failure_context()
            )),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn trace_write(
        caller: &str,
        phase: &str,
        serial: &str,
        bus: &str,
        rgb: Option<(u8, u8, u8)>,
        claim_needed: bool,
        retry: bool,
        ms: u128,
        result: Result<(usize, usize), String>,
    ) {
        let rgb_part = match rgb {
            Some((r, g, b)) => format!(" rgb={r:02x}{g:02x}{b:02x}"),
            None => String::new(),
        };
        match result {
            Ok((bytes, expected)) => app_log::hid_trace(format!(
                "write caller={caller} phase={phase} serial={serial} bus={bus}{rgb_part} claim={claim_needed} retry={retry} ms={ms} ok bytes={bytes} expected={expected} {}",
                steam_tag()
            )),
            Err(err) => app_log::hid_trace(format!(
                "write caller={caller} phase={phase} serial={serial} bus={bus}{rgb_part} claim={claim_needed} retry={retry} ms={ms} err={err} {}",
                failure_context()
            )),
        }
    }

    pub fn trace_read(caller: &str, serial: &str, bus: &str, ms: u128, detail: &str, ok: bool) {
        if ok {
            app_log::hid_trace(format!(
                "read caller={caller} serial={serial} bus={bus} ms={ms} {detail} {}",
                steam_tag()
            ));
        } else {
            app_log::hid_trace(format!(
                "read caller={caller} serial={serial} bus={bus} ms={ms} {detail} {}",
                failure_context()
            ));
        }
    }

    pub struct OpGuard {
        name: &'static str,
        started: Instant,
        silent: bool,
    }

    impl Drop for OpGuard {
        fn drop(&mut self) {
            let ms = self.started.elapsed().as_millis();
            leave_op(self.name);
            if !self.silent && ms >= SLOW_OP_MS {
                let line = format!("hid-diag: slow op={} ms={ms}", self.name);
                app_log::info(line.clone());
                app_log::hid_trace_raw(line);
            }
        }
    }

    pub fn enter_op(name: &'static str) -> OpGuard {
        enter_op_inner(name, false)
    }

    fn enter_op_inner(name: &'static str, silent: bool) -> OpGuard {
        let now = Instant::now();
        if let Ok(mut g) = PHASE.lock() {
            g.op = name;
            g.entered_at = Some(now);
            g.last_progress = Some(now);
        }
        PROGRESS_TICK.fetch_add(1, Ordering::Relaxed);
        OpGuard {
            name,
            started: now,
            silent,
        }
    }

    fn leave_op(name: &'static str) {
        let now = Instant::now();
        if let Ok(mut g) = PHASE.lock() {
            if g.op == name {
                g.op = "idle";
                g.entered_at = None;
            }
            g.last_progress = Some(now);
        }
        PROGRESS_TICK.fetch_add(1, Ordering::Relaxed);
    }

    pub fn note_progress(label: &'static str) {
        let now = Instant::now();
        if let Ok(mut g) = PHASE.lock() {
            if g.entered_at.is_none() {
                g.op = label;
            }
            g.last_progress = Some(now);
        }
        PROGRESS_TICK.fetch_add(1, Ordering::Relaxed);
    }

    pub fn start_worker_watchdog() {
        if WATCHDOG_STARTED.swap(true, Ordering::SeqCst) {
            return;
        }
        thread::Builder::new()
            .name("hid-watchdog".into())
            .spawn(|| {
                let mut last_logged_tier = 0u128;
                let mut last_tick = PROGRESS_TICK.load(Ordering::Relaxed);
                loop {
                    thread::sleep(WATCHDOG_POLL);
                    let tick = PROGRESS_TICK.load(Ordering::Relaxed);
                    let (op, entered_at, last_progress) = match PHASE.lock() {
                        Ok(g) => (g.op, g.entered_at, g.last_progress),
                        Err(_) => continue,
                    };

                    let in_op = entered_at.is_some();
                    let gap_ms = if let Some(entered) = entered_at {
                        entered.elapsed().as_millis()
                    } else if let Some(progress) = last_progress {
                        progress.elapsed().as_millis()
                    } else {
                        0
                    };

                    if tick != last_tick || gap_ms < 250 {
                        last_tick = tick;
                        last_logged_tier = 0;
                        continue;
                    }

                    let tier = if gap_ms >= 5000 {
                        5000
                    } else if gap_ms >= 2000 {
                        2000
                    } else if gap_ms >= 1000 {
                        1000
                    } else if gap_ms >= 500 {
                        500
                    } else if gap_ms >= 250 {
                        250
                    } else {
                        0
                    };

                    if tier == 0 || tier <= last_logged_tier {
                        continue;
                    }
                    last_logged_tier = tier;

                    let kind = if in_op { "op" } else { "idle" };
                    let line = format!(
                        "hid-diag: worker stall kind={kind} last_op={op} gap_ms={gap_ms} tier={tier}"
                    );
                    app_log::warn(line.clone());
                    app_log::hid_trace_raw(format!("{line} {}", failure_context_lightweight()));
                }
            })
            .expect("spawn hid-watchdog");
    }

    fn failure_context_lightweight() -> String {
        let steam = STEAM_CACHE.lock().map(|g| u8::from(g.steam)).unwrap_or(0);
        format!("steam={steam}")
    }
}

#[cfg(debug_assertions)]
pub use active::{
    enter_op, failure_context, note_progress, start_worker_watchdog, trace_open, trace_read,
    trace_write,
};

#[cfg(debug_assertions)]
#[allow(unused_imports)]
pub use active::{OpGuard, steam_present, steam_tag};

#[cfg(not(debug_assertions))]
#[allow(dead_code)]
mod release {
    use hidapi::DeviceInfo;

    pub fn device_fields(_info: &DeviceInfo) -> String {
        String::new()
    }

    pub fn steam_present() -> bool {
        false
    }

    pub fn failure_context() -> String {
        String::new()
    }

    pub fn steam_tag() -> String {
        String::new()
    }

    pub fn trace_open(
        _caller: &str,
        _info: &DeviceInfo,
        _serial_hint: &str,
        _ms: u128,
        _result: Result<(), &str>,
    ) {
    }

    #[allow(clippy::too_many_arguments)]
    pub fn trace_write(
        _caller: &str,
        _phase: &str,
        _serial: &str,
        _bus: &str,
        _rgb: Option<(u8, u8, u8)>,
        _claim_needed: bool,
        _retry: bool,
        _ms: u128,
        _result: Result<(usize, usize), String>,
    ) {
    }

    pub fn trace_read(
        _caller: &str,
        _serial: &str,
        _bus: &str,
        _ms: u128,
        _detail: &str,
        _ok: bool,
    ) {
    }

    pub struct OpGuard;

    pub fn enter_op(_name: &'static str) -> OpGuard {
        OpGuard
    }

    pub fn note_progress(_label: &'static str) {}

    pub fn start_worker_watchdog() {}
}

#[cfg(not(debug_assertions))]
#[allow(unused_imports)]
pub use release::{
    OpGuard, enter_op, failure_context, note_progress, start_worker_watchdog, steam_present,
    steam_tag, trace_open, trace_read, trace_write,
};
