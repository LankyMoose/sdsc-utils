//! Single-threaded DualSense HID owner for the iced daemon.
//!
//! Owns **all** DualSense HID I/O: lightbar / battery poll / Identify / power-off,
//! plus short input reads published to a shared snapshot. The UI PadPoll path only
//! clones that snapshot — it never opens HID — so Identify cannot stall iced.
//!
//! **Lightbar writes never use the input cache handle.** Long-lived read handles on
//! Windows/DualSense often accept `write` with `Ok` without updating the bar, and can
//! poison `LIGHT_OUT` claims. SetRgb / Identify always drop the cached device, then
//! open-write-close; input sampling may reopen afterward.
//!
//! Timing lines (grep `hid-worker:`) record enumerate / open / io / total.

use crate::app_log;
use crate::battery::{self, ControllerStatus};
use crate::color::{Rgb, color_for_battery_percent};
use crate::dualsense::{is_dualsense_gamepad, normalize_identity, resolve_device_identity};
use crate::lightbar::{self, HidPhaseTiming, IDENTIFY_FLASH_COUNT, IDENTIFY_FLASH_MS};
use crate::poll::{self, PRESENCE_INTERVAL, PRESENCE_INTERVAL_EMPTY};
use crate::start_input::{self, InputSnapshot, NavReading, ShortSampleOutcome, SnapshotReason};
use hidapi::{BusType, HidApi, HidDevice};
use iced::futures::channel::oneshot;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const PULSE_SAMPLE_LOG_INTERVAL: Duration = Duration::from_secs(1);
const SLOW_CMD_LOG_MS: u128 = 20;
/// Background input wake cadence (reopen-gesture / Pad Input / idle).
const BACKGROUND_POLL: Duration = Duration::from_millis(50);
/// Active input wake cadence while start-nav or gesture recording needs low latency.
const ACTIVE_POLL: Duration = Duration::from_millis(16);
/// While waiting for the next Identify flash, still sample this often.
const IDENTIFY_SAMPLE_POLL: Duration = Duration::from_millis(16);
const IDENTIFY_WRITES: u32 = IDENTIFY_FLASH_COUNT * 2;
const SAMPLE_STALL_MS: u128 = 100;

enum HidCmd {
    Poll {
        previously: Vec<String>,
        reply: oneshot::Sender<Result<Vec<ControllerStatus>, String>>,
    },
    Identify {
        serial: String,
        percent: u8,
    },
    PowerOff {
        serial: String,
    },
    SetRgb {
        serial: String,
        color: Rgb,
    },
    Shutdown,
}

struct OpenDevice {
    device: HidDevice,
    is_bluetooth: bool,
}

/// Keeps `HidApi` alive with per-serial open handles for **input** sampling.
/// Lightbar writes must not use these handles — see [`write_rgb_exclusive`].
struct DeviceCache {
    api: HidApi,
    devices: HashMap<String, OpenDevice>,
    last_enum_at: Option<Instant>,
    presence: Arc<Mutex<Vec<String>>>,
}

impl DeviceCache {
    fn new(presence: Arc<Mutex<Vec<String>>>) -> Result<Self, String> {
        let api = HidApi::new().map_err(|e| e.to_string())?;
        let mut cache = Self {
            api,
            devices: HashMap::new(),
            last_enum_at: None,
            presence,
        };
        let _ = cache.refresh_device_list();
        Ok(cache)
    }

    /// Re-enumerate in place (does **not** reconstruct `HidApi`).
    fn refresh_device_list(&mut self) -> Result<(), String> {
        let _op = crate::hid_diag::enter_op("hidapi_refresh");
        self.api.refresh_devices().map_err(|e| e.to_string())?;
        self.last_enum_at = Some(Instant::now());
        let mut paths: Vec<String> = self
            .api
            .device_list()
            .filter(|d| is_dualsense_gamepad(d))
            .map(|d| d.path().to_string_lossy().into_owned())
            .collect();
        paths.sort();
        if let Ok(mut guard) = self.presence.lock() {
            *guard = paths;
        }
        Ok(())
    }

    fn enum_due(&self) -> bool {
        match self.last_enum_at {
            None => true,
            Some(at) => {
                let interval = if self.devices.is_empty() {
                    PRESENCE_INTERVAL_EMPTY
                } else {
                    PRESENCE_INTERVAL
                };
                at.elapsed() >= interval
            }
        }
    }

    fn drop_all(&mut self) {
        self.devices.clear();
    }

    fn drop_serial(&mut self, serial: &str) {
        self.devices.remove(&normalize_identity(serial));
    }

    /// Try to open `serial` from the current device list (no refresh).
    fn try_open_serial(&mut self, serial: &str) -> Option<OpenDevice> {
        let target = normalize_identity(serial);
        let mut best: Option<(OpenDevice, bool)> = None;
        for info in self.api.device_list().filter(|d| is_dualsense_gamepad(d)) {
            let open_started = Instant::now();
            let hint = info.serial_number().unwrap_or("");
            let device = {
                let _op = crate::hid_diag::enter_op("open_device");
                match info.open_device(&self.api) {
                    Ok(d) => {
                        crate::hid_diag::trace_open(
                            "sample",
                            info,
                            hint,
                            open_started.elapsed().as_millis(),
                            Ok(()),
                        );
                        d
                    }
                    Err(err) => {
                        crate::hid_diag::trace_open(
                            "sample",
                            info,
                            hint,
                            open_started.elapsed().as_millis(),
                            Err(&err.to_string()),
                        );
                        continue;
                    }
                }
            };
            let identity = resolve_device_identity(info, &device);
            if identity != target {
                continue;
            }
            let is_bluetooth = matches!(info.bus_type(), BusType::Bluetooth);
            let is_usb = matches!(info.bus_type(), BusType::Usb);
            let replace = match &best {
                None => true,
                Some((_, prev_usb)) => is_usb && !*prev_usb,
            };
            if replace {
                best = Some((
                    OpenDevice {
                        device,
                        is_bluetooth,
                    },
                    is_usb,
                ));
            }
        }
        best.map(|(open, _)| open)
    }

    /// Open (or reuse) the DualSense matching `serial`. Prefers USB when both exist.
    fn ensure(&mut self, serial: &str) -> Result<&OpenDevice, String> {
        let target = normalize_identity(serial);
        if self.devices.contains_key(&target) {
            return Ok(self.devices.get(&target).expect("contains"));
        }

        if let Some(open) = self.try_open_serial(serial) {
            self.devices.insert(target.clone(), open);
            return Ok(self.devices.get(&target).expect("just inserted"));
        }

        self.refresh_device_list()?;
        let Some(open) = self.try_open_serial(serial) else {
            return Err(format!("controller {serial} not found"));
        };
        self.devices.insert(target.clone(), open);
        Ok(self.devices.get(&target).expect("just inserted"))
    }

    /// Ensure every connected DualSense gamepad has a cached handle (USB preferred).
    /// Refreshes the device list only when empty or on the presence cadence.
    fn ensure_all_pads(&mut self) {
        if self.devices.is_empty() || self.enum_due() {
            if self.refresh_device_list().is_err() {
                return;
            }
            // Drop handles for pads that vanished from the list.
            let live: std::collections::HashSet<String> = self
                .api
                .device_list()
                .filter(|d| is_dualsense_gamepad(d))
                .filter_map(|d| {
                    d.serial_number()
                        .filter(|s| !s.is_empty())
                        .map(normalize_identity)
                })
                .collect();
            if !live.is_empty() {
                self.devices.retain(|id, _| live.contains(id));
            }
        }

        let mut best: HashMap<String, (OpenDevice, bool)> = HashMap::new();
        for info in self.api.device_list().filter(|d| is_dualsense_gamepad(d)) {
            let identity_hint = info.serial_number().filter(|s| !s.is_empty()).unwrap_or("");
            // Skip open if we already hold this pad (second open often fails exclusive).
            if !identity_hint.is_empty()
                && self
                    .devices
                    .contains_key(&normalize_identity(identity_hint))
            {
                continue;
            }
            let open_started = Instant::now();
            let device = {
                let _op = crate::hid_diag::enter_op("open_device");
                match info.open_device(&self.api) {
                    Ok(d) => {
                        crate::hid_diag::trace_open(
                            "sample",
                            info,
                            identity_hint,
                            open_started.elapsed().as_millis(),
                            Ok(()),
                        );
                        d
                    }
                    Err(err) => {
                        crate::hid_diag::trace_open(
                            "sample",
                            info,
                            identity_hint,
                            open_started.elapsed().as_millis(),
                            Err(&err.to_string()),
                        );
                        continue;
                    }
                }
            };
            let identity = resolve_device_identity(info, &device);
            if self.devices.contains_key(&identity) {
                continue;
            }
            let is_bluetooth = matches!(info.bus_type(), BusType::Bluetooth);
            let is_usb = matches!(info.bus_type(), BusType::Usb);
            let replace = match best.get(&identity) {
                None => true,
                Some((_, prev_usb)) => is_usb && !*prev_usb,
            };
            if replace {
                best.insert(
                    identity,
                    (
                        OpenDevice {
                            device,
                            is_bluetooth,
                        },
                        is_usb,
                    ),
                );
            }
        }
        for (id, (open, _)) in best {
            self.devices.insert(id, open);
        }
    }
}

struct IdentifySession {
    serial: String,
    normal: Rgb,
    next_write: u32,
    wake_at: Instant,
    timing: HidPhaseTiming,
    flashes: u32,
    started: Instant,
}

/// Cloneable handle to enqueue DualSense HID work.
#[derive(Clone)]
pub struct HidWorkerHandle {
    tx: Sender<HidCmd>,
    identifying: Arc<AtomicBool>,
    input_hot: Arc<AtomicBool>,
    presence: Arc<Mutex<Vec<String>>>,
}

impl HidWorkerHandle {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::channel::<HidCmd>();
        let identifying = Arc::new(AtomicBool::new(false));
        let identifying_worker = Arc::clone(&identifying);
        let input_hot = Arc::new(AtomicBool::new(false));
        let input_hot_worker = Arc::clone(&input_hot);
        let presence = Arc::new(Mutex::new(Vec::new()));
        let presence_worker = Arc::clone(&presence);
        let input_snapshot = Arc::new(Mutex::new(InputSnapshot::default()));
        start_input::set_input_snapshot(Arc::clone(&input_snapshot));
        let last_noisy_log = Arc::new(Mutex::new(Instant::now() - PULSE_SAMPLE_LOG_INTERVAL));
        let last_noisy_log_worker = Arc::clone(&last_noisy_log);

        thread::Builder::new()
            .name("hid-worker".into())
            .spawn(move || {
                crate::hid_diag::start_worker_watchdog();
                worker_loop(
                    rx,
                    identifying_worker,
                    input_hot_worker,
                    last_noisy_log_worker,
                    input_snapshot,
                    presence_worker,
                );
            })
            .expect("spawn hid-worker");

        Self {
            tx,
            identifying,
            input_hot,
            presence,
        }
    }

    pub fn identifying(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.identifying)
    }

    /// Latest DualSense gamepad HID paths from the worker's throttled refresh.
    pub fn presence_paths(&self) -> Vec<String> {
        self.presence.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Elevate input sampling to ~60 Hz while start-nav / gesture record need it.
    pub fn set_input_hot(&self, hot: bool) {
        self.input_hot.store(hot, Ordering::Relaxed);
    }

    pub fn poll(
        &self,
        previously: Vec<String>,
    ) -> impl std::future::Future<Output = Result<Vec<ControllerStatus>, String>> + Send + use<>
    {
        let (reply, rx) = oneshot::channel();
        let _ = self.tx.send(HidCmd::Poll { previously, reply });
        async move {
            rx.await
                .map_err(|_| "hid-worker poll cancelled".to_string())?
        }
    }

    pub fn identify(&self, serial: String, percent: u8) -> bool {
        if self
            .identifying
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return false;
        }
        if self.tx.send(HidCmd::Identify { serial, percent }).is_err() {
            self.identifying.store(false, Ordering::SeqCst);
            return false;
        }
        true
    }

    pub fn power_off(&self, serial: String) {
        let _ = self.tx.send(HidCmd::PowerOff { serial });
    }

    pub fn set_rgb(&self, serial: String, color: Rgb) {
        let _ = self.tx.send(HidCmd::SetRgb { serial, color });
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(HidCmd::Shutdown);
    }
}

fn identify_flash_color(step: u32, normal: Rgb) -> Rgb {
    if step.is_multiple_of(2) {
        Rgb::WHITE
    } else {
        normal
    }
}

/// Lightbar output must not use the input-pump handle: long-lived read handles on
/// Windows/DualSense often accept `write` with `Ok` without updating the bar.
fn write_rgb_exclusive(
    cache: &mut DeviceCache,
    serial: &str,
    color: Rgb,
) -> (Result<(), String>, HidPhaseTiming) {
    let target = normalize_identity(serial);
    let was_cached = cache.devices.contains_key(&target);
    cache.drop_serial(serial);
    if was_cached {
        // Input handles can "succeed" RGB writes without a real claim; force LIGHT_OUT.
        lightbar::prepare_connect_apply(serial);
    }
    let (result, mut timing) = lightbar::apply_lightbar_rgb_timed(&cache.api, serial, color);
    if result.is_err() {
        // Stale device list — refresh once and retry.
        if cache.refresh_device_list().is_ok() {
            let (retry, t2) = lightbar::apply_lightbar_rgb_timed(&cache.api, serial, color);
            timing.add_assign(t2);
            return (retry, timing);
        }
    }
    (result, timing)
}

fn sample_one(cache: &mut DeviceCache, serial: &str, input_hot: bool) -> Option<NavReading> {
    let _op = crate::hid_diag::enter_op("sample_one");
    let open = cache.ensure(serial).ok()?;
    let is_bluetooth = open.is_bluetooth;
    match start_input::read_device_sample_short(&open.device, is_bluetooth, serial, input_hot) {
        ShortSampleOutcome::Ok(sample) => Some(start_input::hid_nav_reading(serial, sample)),
        ShortSampleOutcome::Fail(_) => {
            cache.drop_serial(serial);
            None
        }
    }
}

fn snapshot_age_ms(snapshot: &Mutex<InputSnapshot>) -> u128 {
    snapshot
        .lock()
        .map(|g| g.published_at.elapsed().as_millis())
        .unwrap_or(0)
}

fn publish_snapshot(
    snapshot: &Mutex<InputSnapshot>,
    readings: Vec<NavReading>,
    reason: SnapshotReason,
    after_cmd: Option<&str>,
    last_noisy_log: &Mutex<Instant>,
) {
    let _op = crate::hid_diag::enter_op("publish_snapshot");
    let prev_pads = snapshot.lock().map(|g| g.readings.len()).unwrap_or(0);
    let gap_ms = snapshot_age_ms(snapshot);
    let was_empty = prev_pads == 0;
    let now_empty = readings.is_empty();
    let pad_count = readings.len();

    if let Ok(mut guard) = snapshot.lock() {
        guard.seq = guard.seq.saturating_add(1);
        guard.published_at = Instant::now();
        guard.reason = reason;
        guard.readings = readings;
    }

    let clearing = matches!(
        reason,
        SnapshotReason::ClearedPoll
            | SnapshotReason::ClearedPowerOff
            | SnapshotReason::ClearedShutdown
    );
    if clearing && !was_empty {
        crate::hid_diag::diag_info(format!(
            "hid-diag: snapshot clear reason={} prev_pads={prev_pads}",
            reason.as_str()
        ));
    } else if !now_empty && was_empty {
        let after = after_cmd.unwrap_or("-");
        crate::hid_diag::diag_info(format!(
            "hid-diag: snapshot restore pads={pad_count} gap_ms={gap_ms} after={after}"
        ));
    }
    let _ = last_noisy_log;
}

/// Sample all cached DualSense pads; open any missing connected pads first.
fn sample_all_inputs(
    cache: &mut DeviceCache,
    snapshot: &Mutex<InputSnapshot>,
    input_hot: bool,
    last_noisy_log: &Mutex<Instant>,
    after_cmd: Option<&str>,
) {
    cache.ensure_all_pads();
    let serials: Vec<String> = cache.devices.keys().cloned().collect();
    let cached = serials.len();
    let mut out = Vec::with_capacity(serials.len());
    for serial in serials {
        if let Some(reading) = sample_one(cache, &serial, input_hot) {
            out.push(reading);
        }
    }
    let reason = if out.len() < cached && cached > 0 {
        SnapshotReason::SamplePartial
    } else {
        SnapshotReason::Sample
    };
    // For SamplePartial, also emit the short line with real cached count.
    if matches!(reason, SnapshotReason::SamplePartial) {
        let due = last_noisy_log
            .lock()
            .map(|t| t.elapsed() >= PULSE_SAMPLE_LOG_INTERVAL)
            .unwrap_or(true);
        if due {
            if let Ok(mut guard) = last_noisy_log.lock() {
                *guard = Instant::now();
            }
            crate::hid_diag::diag_info(format!(
                "hid-diag: sample short cached={cached} published={}",
                out.len()
            ));
        }
    }
    publish_snapshot(snapshot, out, reason, after_cmd, last_noisy_log);
}

/// Refresh the snapshot entry for one serial (Identify path).
fn sample_serial_into_snapshot(
    cache: &mut DeviceCache,
    snapshot: &Mutex<InputSnapshot>,
    serial: &str,
    input_hot: bool,
) {
    let Some(reading) = sample_one(cache, serial, input_hot) else {
        return;
    };
    if let Ok(mut guard) = snapshot.lock() {
        guard.seq = guard.seq.saturating_add(1);
        guard.published_at = Instant::now();
        guard.reason = SnapshotReason::Identify;
        if let Some(slot) = guard.readings.iter_mut().find(|r| r.id.0 == reading.id.0) {
            *slot = reading;
        } else {
            guard.readings.push(reading);
        }
    }
}

fn log_cmd_begin(cmd: &str, snapshot: &Mutex<InputSnapshot>) {
    let since_pub_ms = snapshot_age_ms(snapshot);
    crate::hid_diag::diag_info(format!(
        "hid-diag: cmd begin={cmd} since_publish_ms={since_pub_ms}"
    ));
}

fn maybe_log_sample_stall(
    snapshot: &Mutex<InputSnapshot>,
    input_hot: bool,
    in_cmd: &str,
    stall_logged: &mut bool,
    last_noisy_log: &Mutex<Instant>,
) {
    if !input_hot {
        *stall_logged = false;
        return;
    }
    let gap_ms = snapshot_age_ms(snapshot);
    if gap_ms < SAMPLE_STALL_MS {
        *stall_logged = false;
        return;
    }
    if *stall_logged {
        let due = last_noisy_log
            .lock()
            .map(|t| t.elapsed() >= PULSE_SAMPLE_LOG_INTERVAL)
            .unwrap_or(true);
        if !due {
            return;
        }
    }
    *stall_logged = true;
    if let Ok(mut guard) = last_noisy_log.lock() {
        *guard = Instant::now();
    }
    crate::hid_diag::diag_info(format!(
        "hid-diag: sample stalled gap_ms={gap_ms} in_cmd={in_cmd}"
    ));
}

fn begin_identify(
    cache: &mut DeviceCache,
    snapshot: &Mutex<InputSnapshot>,
    serial: String,
    percent: u8,
    identifying: &AtomicBool,
    last_noisy_log: &Mutex<Instant>,
    input_hot: bool,
) -> Option<IdentifySession> {
    let normal = color_for_battery_percent(percent);
    log_cmd_begin("Identify", snapshot);
    let started = Instant::now();
    let (result, timing) = {
        let _op = crate::hid_diag::enter_op("cmd_Identify");
        write_rgb_exclusive(cache, &serial, identify_flash_color(0, normal))
    };
    if let Err(err) = result {
        app_log::warn(format!("identify failed for {serial}: {err}"));
        log_cmd(
            "Identify",
            &timing,
            started.elapsed(),
            Some(1),
            true,
            last_noisy_log,
        );
        identifying.store(false, Ordering::SeqCst);
        return None;
    }
    sample_serial_into_snapshot(cache, snapshot, &serial, input_hot);
    Some(IdentifySession {
        serial,
        normal,
        next_write: 1,
        wake_at: Instant::now() + Duration::from_millis(IDENTIFY_FLASH_MS),
        timing,
        flashes: 1,
        started,
    })
}

fn advance_identify(
    session: &mut Option<IdentifySession>,
    cache: &mut DeviceCache,
    snapshot: &Mutex<InputSnapshot>,
    identifying: &AtomicBool,
    last_noisy_log: &Mutex<Instant>,
    input_hot: bool,
) {
    let Some(s) = session.as_mut() else {
        return;
    };
    if Instant::now() < s.wake_at {
        return;
    }

    if s.next_write >= IDENTIFY_WRITES {
        let finished = session.take().expect("session present");
        // Exclusive writes already dropped the handle; refresh input snapshot.
        log_cmd(
            "Identify",
            &finished.timing,
            finished.started.elapsed(),
            Some(finished.flashes),
            true,
            last_noisy_log,
        );
        identifying.store(false, Ordering::SeqCst);
        sample_all_inputs(cache, snapshot, input_hot, last_noisy_log, Some("Identify"));
        return;
    }

    log_cmd_begin("IdentifyFlash", snapshot);
    let color = identify_flash_color(s.next_write, s.normal);
    let serial = s.serial.clone();
    let (result, t) = {
        let _op = crate::hid_diag::enter_op("cmd_IdentifyFlash");
        write_rgb_exclusive(cache, &serial, color)
    };
    s.timing.add_assign(t);
    s.flashes += 1;
    if let Err(err) = result {
        app_log::warn(format!("identify failed for {serial}: {err}"));
        let finished = session.take().expect("session present");
        cache.drop_serial(&finished.serial);
        log_cmd(
            "Identify",
            &finished.timing,
            finished.started.elapsed(),
            Some(finished.flashes),
            true,
            last_noisy_log,
        );
        identifying.store(false, Ordering::SeqCst);
        return;
    }
    sample_serial_into_snapshot(cache, snapshot, &serial, input_hot);
    s.next_write += 1;
    s.wake_at = Instant::now() + Duration::from_millis(IDENTIFY_FLASH_MS);
}

fn abort_identify(
    session: &mut Option<IdentifySession>,
    cache: &mut DeviceCache,
    identifying: &AtomicBool,
) {
    if let Some(finished) = session.take() {
        cache.drop_serial(&finished.serial);
        identifying.store(false, Ordering::SeqCst);
    }
}

fn handle_cmd(
    cmd: HidCmd,
    session: &mut Option<IdentifySession>,
    cache: &mut DeviceCache,
    snapshot: &Mutex<InputSnapshot>,
    identifying: &AtomicBool,
    last_noisy_log: &Mutex<Instant>,
    input_hot: bool,
) -> bool {
    match cmd {
        HidCmd::Shutdown => {
            abort_identify(session, cache, identifying);
            cache.drop_all();
            publish_snapshot(
                snapshot,
                Vec::new(),
                SnapshotReason::ClearedShutdown,
                Some("Shutdown"),
                last_noisy_log,
            );
            true
        }
        HidCmd::Identify { serial, percent } => {
            abort_identify(session, cache, identifying);
            identifying.store(true, Ordering::SeqCst);
            *session = begin_identify(
                cache,
                snapshot,
                serial,
                percent,
                identifying,
                last_noisy_log,
                input_hot,
            );
            false
        }
        HidCmd::PowerOff { serial } => {
            if session.as_ref().is_some_and(|s| s.serial == serial) {
                abort_identify(session, cache, identifying);
            }
            cache.drop_all();
            publish_snapshot(
                snapshot,
                Vec::new(),
                SnapshotReason::ClearedPowerOff,
                Some("PowerOff"),
                last_noisy_log,
            );
            log_cmd_begin("PowerOff", snapshot);
            let started = Instant::now();
            let (result, timing) = {
                let _ = cache.refresh_device_list();
                battery::power_off_bluetooth_timed(&cache.api, &serial)
            };
            match result {
                Ok(()) => app_log::info(format!("power-off sent for {serial}")),
                Err(err) => app_log::warn(format!("power-off failed for {serial}: {err}")),
            }
            log_cmd(
                "PowerOff",
                &timing,
                started.elapsed(),
                None,
                true,
                last_noisy_log,
            );
            false
        }
        HidCmd::SetRgb { serial, color } => {
            if session.as_ref().is_some_and(|s| s.serial == serial) {
                return false;
            }
            log_cmd_begin("SetRgb", snapshot);
            let started = Instant::now();
            let (result, timing) = write_rgb_exclusive(cache, &serial, color);
            if let Err(err) = result {
                app_log::warn(format!("lightbar write failed for {serial}: {err}"));
            }
            log_cmd(
                "SetRgb",
                &timing,
                started.elapsed(),
                None,
                false,
                last_noisy_log,
            );
            false
        }
        HidCmd::Poll { previously, reply } => {
            // Release input handles so Poll can open; keep last nav readings published.
            cache.drop_all();
            log_cmd_begin("Poll", snapshot);
            let started = Instant::now();
            let (result, timing) = {
                let _op = crate::hid_diag::enter_op("cmd_Poll");
                let _ = cache.refresh_device_list();
                poll::poll_controllers_timed(&cache.api, &previously)
            };
            log_cmd(
                "Poll",
                &timing,
                started.elapsed(),
                None,
                true,
                last_noisy_log,
            );
            let _ = reply.send(result);
            false
        }
    }
}

fn worker_loop(
    rx: Receiver<HidCmd>,
    identifying: Arc<AtomicBool>,
    input_hot: Arc<AtomicBool>,
    last_noisy_log: Arc<Mutex<Instant>>,
    input_snapshot: Arc<Mutex<InputSnapshot>>,
    presence: Arc<Mutex<Vec<String>>>,
) {
    let mut cache = match DeviceCache::new(presence) {
        Ok(c) => c,
        Err(err) => {
            app_log::warn(format!("hid-worker: HidApi init failed: {err}"));
            return;
        }
    };
    // Clear claims poisoned by prior no-op writes on input-cache handles.
    lightbar::forget_all_claims();
    let mut session: Option<IdentifySession> = None;
    let mut stall_logged = false;
    let mut in_cmd = "none";

    loop {
        crate::hid_diag::note_progress("worker_loop");
        let hot = input_hot.load(Ordering::Relaxed);
        maybe_log_sample_stall(
            &input_snapshot,
            hot,
            in_cmd,
            &mut stall_logged,
            &last_noisy_log,
        );

        if session
            .as_ref()
            .is_some_and(|s| Instant::now() >= s.wake_at)
        {
            in_cmd = "Identify";
            advance_identify(
                &mut session,
                &mut cache,
                &input_snapshot,
                &identifying,
                &last_noisy_log,
                hot,
            );
            if session.is_none() {
                in_cmd = "none";
            }
            continue;
        }

        let timeout = if let Some(s) = session.as_ref() {
            let until_flash = s
                .wake_at
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1));
            until_flash.min(IDENTIFY_SAMPLE_POLL)
        } else if hot {
            ACTIVE_POLL
        } else {
            BACKGROUND_POLL
        };

        match rx.recv_timeout(timeout) {
            Ok(cmd) => {
                in_cmd = match &cmd {
                    HidCmd::Poll { .. } => "Poll",
                    HidCmd::Identify { .. } => "Identify",
                    HidCmd::PowerOff { .. } => "PowerOff",
                    HidCmd::SetRgb { .. } => "SetRgb",
                    HidCmd::Shutdown => "Shutdown",
                };
                if handle_cmd(
                    cmd,
                    &mut session,
                    &mut cache,
                    &input_snapshot,
                    &identifying,
                    &last_noisy_log,
                    hot,
                ) {
                    break;
                }
                if session.is_none() {
                    in_cmd = "none";
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(s) = session.as_ref() {
                    let serial = s.serial.clone();
                    sample_serial_into_snapshot(&mut cache, &input_snapshot, &serial, hot);
                } else {
                    sample_all_inputs(&mut cache, &input_snapshot, hot, &last_noisy_log, None);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn log_cmd(
    cmd: &str,
    timing: &HidPhaseTiming,
    total: Duration,
    flashes: Option<u32>,
    always: bool,
    last_noisy_log: &Mutex<Instant>,
) {
    let total_ms = total.as_millis();
    if !always {
        let slow = total_ms >= SLOW_CMD_LOG_MS;
        let due = last_noisy_log
            .lock()
            .map(|t| t.elapsed() >= PULSE_SAMPLE_LOG_INTERVAL)
            .unwrap_or(true);
        if !slow && !due {
            return;
        }
        if let Ok(mut guard) = last_noisy_log.lock() {
            *guard = Instant::now();
        }
    }

    let flash_part = flashes.map(|n| format!(" flashes={n}")).unwrap_or_default();
    app_log::info(format!(
        "hid-worker: cmd={cmd}{flash_part} enumerate_ms={} open_ms={} io_ms={} total_ms={}",
        timing.enumerate_ms, timing.open_ms, timing.io_ms, total_ms
    ));
}
