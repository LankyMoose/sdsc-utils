//! Simple append-only file logger (visible under windows_subsystem = "windows").
//!
//! - `app.log` — edge-triggered summaries (second resolution, 1 MB rotate)
//! - `hid-trace.log` — every HID open/write/read attempt (ms resolution, 16 MB rotate);
//!   **debug builds only** (`cfg(debug_assertions)`)

use crate::platform::app_meta::{DISPLAY_NAME, PKG_NAME, PKG_VERSION};
use crate::platform::crash_restart::CrashArmed;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime};

const MAX_LOG_BYTES: u64 = 1_000_000;
#[cfg(debug_assertions)]
const MAX_HID_TRACE_BYTES: u64 = 16_000_000;

/// Owned file-logger handles (log paths + session clock).
///
/// Intentionally process-global (see `HANDLES` below): logging is cross-cutting
/// infrastructure with ~130 call sites across the HID worker thread, persist
/// atomic writes, session notify, and platform bring-up — threading `&LogHandles`
/// through all of them would churn far beyond the app root for no behavior
/// gain. The binary owns init ordering (`app_log::init(armed)` first in
/// `main`, with the boot-owned `&'static CrashArmed`);
/// everything else logs through the free functions, which delegate here.
pub struct LogHandles {
    log_path: Mutex<Option<PathBuf>>,
    #[cfg(debug_assertions)]
    hid_trace_path: Mutex<Option<PathBuf>>,
    session_id: AtomicU64,
    session_started: Mutex<Option<Instant>>,
}

impl LogHandles {
    pub const fn new() -> Self {
        Self {
            log_path: Mutex::new(None),
            #[cfg(debug_assertions)]
            hid_trace_path: Mutex::new(None),
            session_id: AtomicU64::new(0),
            session_started: Mutex::new(None),
        }
    }

    pub fn init(&self, crash_armed: &'static CrashArmed) {
        let path = log_file_path("app.log");
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        rotate_if_needed(&path, MAX_LOG_BYTES, "log.1");
        if let Ok(mut guard) = self.log_path.lock() {
            *guard = Some(path);
        }

        #[cfg(debug_assertions)]
        {
            let trace = log_file_path("hid-trace.log");
            rotate_if_needed(&trace, MAX_HID_TRACE_BYTES, "log.1");
            if let Ok(mut guard) = self.hid_trace_path.lock() {
                *guard = Some(trace);
            }
        }

        install_panic_hook(crash_armed);

        let session_id = epoch_ms() as u64;
        self.session_id.store(session_id, Ordering::Relaxed);
        if let Ok(mut guard) = self.session_started.lock() {
            *guard = Some(Instant::now());
        }

        self.info(format!(
            "{DISPLAY_NAME} ({PKG_NAME}) {PKG_VERSION} starting session={session_id}"
        ));
        #[cfg(debug_assertions)]
        self.hid_trace(format!("session start id={session_id}"));
    }

    /// Stable id for this process run (epoch ms at init). Appears in both log files.
    pub fn session_id(&self) -> u64 {
        self.session_id.load(Ordering::Relaxed)
    }

    /// Milliseconds since [`LogHandles::init`].
    pub fn session_uptime_ms(&self) -> u128 {
        self.session_started
            .lock()
            .ok()
            .and_then(|g| g.map(|t| t.elapsed().as_millis()))
            .unwrap_or(0)
    }

    /// User-marked hitch: stamp both logs so short sessions can be aligned later.
    /// No-op in release builds.
    pub fn mark_hitch(&self, extra: impl AsRef<str>) {
        #[cfg(not(debug_assertions))]
        let _ = extra;
        #[cfg(debug_assertions)]
        {
            let sid = self.session_id();
            let up_ms = self.session_uptime_ms();
            let extra = extra.as_ref();
            let detail = if extra.is_empty() {
                String::new()
            } else {
                format!(" {extra}")
            };
            let line = format!("HITCH_MARK session={sid} up_ms={up_ms}{detail}");
            self.warn(line.clone());
            self.hid_trace(line);
        }
    }

    /// Compact HID diagnostic stream (millisecond timestamps). Prefer for high-rate I/O.
    /// No-op in release builds.
    pub fn hid_trace(&self, message: impl AsRef<str>) {
        #[cfg(not(debug_assertions))]
        {
            let _ = message;
        }
        #[cfg(debug_assertions)]
        {
            let started = Instant::now();
            let line = format!("[{}] {}\n", epoch_ms(), message.as_ref());
            if let Ok(guard) = self.hid_trace_path.lock()
                && let Some(path) = guard.as_ref()
            {
                rotate_if_needed(path, MAX_HID_TRACE_BYTES, "log.1");
            }
            append_to(&self.hid_trace_path, &line);
            let ms = started.elapsed().as_millis();
            if ms >= 50 {
                let slow = format!("[{}] hid-diag: slow op=hid_trace_io ms={ms}\n", epoch_ms());
                append_to(&self.hid_trace_path, &slow);
                self.write_line("INFO", format!("hid-diag: slow op=hid_trace_io ms={ms}"));
            }
        }
    }

    /// Like [`LogHandles::hid_trace`] but never self-reports slow I/O (watchdog / nested use).
    /// No-op in release builds.
    pub fn hid_trace_raw(&self, message: impl AsRef<str>) {
        #[cfg(not(debug_assertions))]
        {
            let _ = message;
        }
        #[cfg(debug_assertions)]
        {
            let line = format!("[{}] {}\n", epoch_ms(), message.as_ref());
            append_to(&self.hid_trace_path, &line);
        }
    }

    pub fn info(&self, message: impl AsRef<str>) {
        self.write_line("INFO", message);
    }

    pub fn warn(&self, message: impl AsRef<str>) {
        self.write_line("WARN", message);
    }

    pub fn error(&self, message: impl AsRef<str>) {
        self.write_line("ERROR", message);
    }

    fn write_line(&self, level: &str, message: impl AsRef<str>) {
        let line = format!("[{}] {level}: {}\n", epoch_secs(), message.as_ref());
        append_to(&self.log_path, &line);
    }

    fn append_panic_line(&self, message: &str) {
        let line = format!("[{}] ERROR: {message}\n", epoch_secs());
        let path = self
            .log_path
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| log_file_path("app.log"));
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = file.write_all(line.as_bytes());
            let _ = file.flush();
        }
    }
}

impl Default for LogHandles {
    fn default() -> Self {
        Self::new()
    }
}

// Intentional remainder: process-global logger instance. `init` (called first
// by the binary) fills the paths/session clock; the free `info`/`warn`/`error`
// /`hid_trace` shims below delegate here so deep modules (HID worker, persist,
// platform bring-up) log without threading `&LogHandles` through every frame.
// Rotation, paths, and formats are unchanged; see `LogHandles` docs.
static HANDLES: LogHandles = LogHandles::new();

pub fn init(crash_armed: &'static CrashArmed) {
    HANDLES.init(crash_armed);
}

/// Log panics to `app.log` (and stderr when available). Needed because
/// `windows_subsystem = "windows"` discards the default stderr panic output.
///
/// Also appends a capped backtrace so GPU/renderer panics (e.g. iced atlas
/// `create_view`) can be attributed without a debugger.
fn install_panic_hook(crash_armed: &'static CrashArmed) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "Box<dyn Any>".to_string()
        };
        let line = format!("PANIC at {location}: {payload}");
        // Prefer a direct append so a poisoned logger mutex cannot hide the panic.
        append_panic_line(&line);
        append_panic_line(&crate::platform::wgpu_diag::context_line());
        append_panic_line(&format!("PANIC_BACKTRACE {}", panic_backtrace_capped()));
        crate::platform::crash_restart::schedule_from_panic(crash_armed);
        let _ = writeln!(std::io::stderr(), "{line}");
        previous(info);
    }));
}

/// Capture a backtrace for panic logging; keep it short enough for one rotate.
fn panic_backtrace_capped() -> String {
    const MAX_CHARS: usize = 4_000;
    let bt = std::backtrace::Backtrace::force_capture();
    let full = format!("{bt}");
    if full.len() <= MAX_CHARS {
        return full;
    }
    let mut end = MAX_CHARS;
    while end > 0 && !full.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…(truncated)", &full[..end])
}

fn append_panic_line(message: &str) {
    HANDLES.append_panic_line(message);
}

/// Stable id for this process run (epoch ms at init). Appears in both log files.
#[allow(dead_code)] // hitch marks (debug builds)
pub fn session_id() -> u64 {
    HANDLES.session_id()
}

/// Milliseconds since [`init`].
#[allow(dead_code)] // hitch marks (debug builds)
pub fn session_uptime_ms() -> u128 {
    HANDLES.session_uptime_ms()
}

/// User-marked hitch: stamp both logs so short sessions can be aligned later.
/// No-op in release builds.
#[allow(dead_code)] // hitch UI / hotkeys (debug builds)
pub fn mark_hitch(extra: impl AsRef<str>) {
    HANDLES.mark_hitch(extra);
}

fn log_file_path(file_name: &str) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join(PKG_NAME).join(file_name);
        }
    }

    #[cfg(not(windows))]
    {
        if let Ok(state) = std::env::var("XDG_STATE_HOME") {
            return PathBuf::from(state).join(PKG_NAME).join(file_name);
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("state")
                .join(PKG_NAME)
                .join(file_name);
        }
    }

    PathBuf::from(format!("{PKG_NAME}-{file_name}"))
}

fn rotate_if_needed(path: &Path, max_bytes: u64, bak_ext: &str) {
    if let Ok(meta) = fs::metadata(path)
        && meta.len() >= max_bytes
    {
        let bak = path.with_extension(bak_ext);
        let _ = fs::remove_file(&bak);
        let _ = fs::rename(path, bak);
    }
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Compact HID diagnostic stream (millisecond timestamps). Prefer for high-rate I/O.
/// No-op in release builds.
#[allow(dead_code)] // called from hid_diag (debug builds)
pub fn hid_trace(message: impl AsRef<str>) {
    HANDLES.hid_trace(message);
}

/// Like [`hid_trace`] but never self-reports slow I/O (watchdog / nested use).
/// No-op in release builds.
#[allow(dead_code)] // called from hid_diag (debug builds)
pub fn hid_trace_raw(message: impl AsRef<str>) {
    HANDLES.hid_trace_raw(message);
}

fn append_to(path_slot: &Mutex<Option<PathBuf>>, line: &str) {
    let path = path_slot.lock().ok().and_then(|g| g.clone());
    if let Some(path) = path
        && let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

pub fn info(message: impl AsRef<str>) {
    HANDLES.info(message);
}

pub fn warn(message: impl AsRef<str>) {
    HANDLES.warn(message);
}

pub fn error(message: impl AsRef<str>) {
    HANDLES.error(message);
}
