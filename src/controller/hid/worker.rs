//! Single-threaded DualSense HID owner for the iced daemon.
//!
//! Owns **all** DualSense HID I/O: lightbar / battery poll / Identify / power-off /
//! rumble, plus short input reads published to a shared snapshot. The UI PadPoll path
//! only clones that snapshot — it never opens HID — so Identify cannot stall iced.
//!
//! **Lightbar writes never use the input cache handle.** Long-lived read handles on
//! Windows/DualSense often accept `write` with `Ok` without updating the bar.
//! SetRgb / Identify always drop the cached device, then open-write-close (claim-once
//! `LIGHT_OUT` only when needed); input sampling may reopen afterward.
//!
//! **Rumble uses a separate long-lived output handle** (not `write_rgb_exclusive`) so
//! start-nav pulses do not drop the input cache. Opens ranked DualSense collections
//! (same order as lightbar) and requests BT calibration once per rumble handle.
//! Poll / PowerOff / Shutdown still drop rumble handles so those paths can open.
//! PowerOff waits for any in-flight rumble pulse to finish first (hold cue), then runs.
//!
//! Timing lines (grep `hid-worker:`) record enumerate / open / io / total.

use crate::controller::driver;
use crate::controller::dualsense::battery;
use crate::controller::dualsense::identity::{
    is_dualsense_device, is_dualsense_gamepad, normalize_identity, product_name,
    resolve_device_identity,
};
use crate::controller::dualsense::lightbar::{
    self, HidPhaseTiming, IDENTIFY_FLASH_COUNT, IDENTIFY_FLASH_MS, LOW_BATTERY_ORANGE,
    LOW_BATTERY_PULSE_GAP_MS, LOW_BATTERY_PULSE_ON_MS,
};
use crate::controller::dualsense::rumble;
use crate::controller::hid::poll::{self, PRESENCE_INTERVAL, PRESENCE_INTERVAL_EMPTY};
use crate::controller::model::ControllerStatus;
use crate::domain::color::{Rgb, color_for_battery_percent};
use crate::domain::pad::{
    self as start_input, InputSnapshot, NavReading, ShortSampleOutcome, SnapshotMeta,
    SnapshotReason,
};
use crate::platform::app_log;
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
/// Background input wake when nothing is listening (command wait only).
const BACKGROUND_POLL: Duration = Duration::from_millis(50);
/// Active input wake ≈ one wired DualSense report (`bInterval` 4).
const ACTIVE_POLL: Duration = Duration::from_millis(4);
/// While waiting for the next Identify flash, still sample this often.
const IDENTIFY_SAMPLE_POLL: Duration = Duration::from_millis(4);
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
    /// Pulse DualSense motors; worker schedules a stop after `duration_ms`.
    Rumble {
        serial: String,
        right: u8,
        left: u8,
        duration_ms: u64,
    },
    /// Zero motors on every open rumble handle and drop them.
    RumbleStopAll,
    /// Replace the low-battery orange pulse target list (empty = stop pulsing).
    SetLowBatteryTargets {
        targets: Vec<(String, u8)>,
    },
    Shutdown,
}

struct OpenDevice {
    device: HidDevice,
    is_bluetooth: bool,
    product: String,
}

/// Keeps `HidApi` alive with per-serial open handles for **input** sampling.
/// Lightbar writes must not use these handles — see [`write_rgb_exclusive`].
struct DeviceCache {
    api: HidApi,
    devices: HashMap<String, OpenDevice>,
    last_enum_at: Option<Instant>,
    presence: Arc<Mutex<Vec<String>>>,
}

/// Last battery + product label observed while sampling (hot path).
struct LivePadStatus {
    product: String,
    reading: battery::BatteryReading,
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
        let _op = crate::controller::hid::diag::enter_op("hidapi_refresh");
        self.api.refresh_devices().map_err(|e| e.to_string())?;
        self.last_enum_at = Some(Instant::now());
        let mut paths: Vec<String> = self
            .api
            .device_list()
            .filter(|d| driver::is_gamepad(d))
            .map(|d| d.path().to_string_lossy().into_owned())
            .collect();
        paths.sort();
        if let Ok(mut guard) = self.presence.lock() {
            *guard = paths;
        }
        Ok(())
    }

    fn enum_due(&self, input_hot: bool) -> bool {
        match self.last_enum_at {
            None => true,
            Some(at) => {
                let interval = if self.devices.is_empty() || input_hot {
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
        for info in self.api.device_list().filter(|d| driver::is_gamepad(d)) {
            let open_started = Instant::now();
            let hint = info.serial_number().unwrap_or("");
            let device = {
                let _op = crate::controller::hid::diag::enter_op("open_device");
                match info.open_device(&self.api) {
                    Ok(d) => {
                        crate::controller::hid::diag::trace_open(
                            "sample",
                            info,
                            hint,
                            open_started.elapsed().as_millis(),
                            Ok(()),
                        );
                        d
                    }
                    Err(err) => {
                        crate::controller::hid::diag::trace_open(
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
                        product: product_name(info.product_id()).to_string(),
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
    fn ensure_all_pads(&mut self, input_hot: bool) {
        if self.devices.is_empty() || self.enum_due(input_hot) {
            if self.refresh_device_list().is_err() {
                return;
            }
            // Drop handles for pads that vanished from the list.
            let live: std::collections::HashSet<String> = self
                .api
                .device_list()
                .filter(|d| driver::is_gamepad(d))
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
        for info in self.api.device_list().filter(|d| driver::is_gamepad(d)) {
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
                let _op = crate::controller::hid::diag::enter_op("open_device");
                match info.open_device(&self.api) {
                    Ok(d) => {
                        crate::controller::hid::diag::trace_open(
                            "sample",
                            info,
                            identity_hint,
                            open_started.elapsed().as_millis(),
                            Ok(()),
                        );
                        d
                    }
                    Err(err) => {
                        crate::controller::hid::diag::trace_open(
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
                            product: product_name(info.product_id()).to_string(),
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

/// Separate open handles for rumble output. Never used for input reads.
/// Dropped on Poll / PowerOff / Shutdown so those paths can reopen HID.
struct RumbleCache {
    devices: HashMap<String, OpenDevice>,
    /// Log once when falling back to the input-cache handle.
    fallback_warned: bool,
}

struct ActiveRumble {
    serial: String,
    stop_at: Instant,
}

/// Power-off deferred until in-flight rumble pulses reach their stop time.
struct PendingPowerOff {
    serials: Vec<String>,
    not_before: Instant,
}

/// Latest future stop among in-flight rumble pulses, if any.
fn rumble_defer_until(active: &[ActiveRumble], now: Instant) -> Option<Instant> {
    active
        .iter()
        .filter(|e| e.stop_at > now)
        .map(|e| e.stop_at)
        .max()
}

fn queue_pending_power_off(
    pending: &mut Option<PendingPowerOff>,
    serial: String,
    not_before: Instant,
) {
    let target = normalize_identity(&serial);
    match pending {
        Some(p) => {
            if !p.serials.iter().any(|s| normalize_identity(s) == target) {
                p.serials.push(serial);
            }
            if not_before > p.not_before {
                p.not_before = not_before;
            }
        }
        None => {
            *pending = Some(PendingPowerOff {
                serials: vec![serial],
                not_before,
            });
        }
    }
}

/// Ready when the defer deadline has passed, or no pulse is left to wait for.
fn pending_power_off_ready(
    pending: &PendingPowerOff,
    active: &[ActiveRumble],
    now: Instant,
) -> bool {
    now >= pending.not_before || rumble_defer_until(active, now).is_none()
}

fn take_due_power_offs(
    pending: &mut Option<PendingPowerOff>,
    active: &[ActiveRumble],
    now: Instant,
) -> Vec<String> {
    let ready = pending
        .as_ref()
        .is_some_and(|p| pending_power_off_ready(p, active, now));
    if ready {
        pending.take().map(|p| p.serials).unwrap_or_default()
    } else {
        Vec::new()
    }
}

impl RumbleCache {
    fn new() -> Self {
        Self {
            devices: HashMap::new(),
            fallback_warned: false,
        }
    }

    fn drop_all(&mut self) {
        self.devices.clear();
    }

    fn drop_serial(&mut self, serial: &str) {
        self.devices.remove(&normalize_identity(serial));
    }

    fn ensure<'a>(&'a mut self, api: &HidApi, serial: &str) -> Result<&'a OpenDevice, String> {
        let target = normalize_identity(serial);
        if self.devices.contains_key(&target) {
            return Ok(self.devices.get(&target).expect("contains"));
        }
        let open = open_rumble_device(api, &target)
            .ok_or_else(|| format!("rumble: DualSense {target} not found / open failed"))?;
        self.devices.insert(target.clone(), open);
        Ok(self.devices.get(&target).expect("just inserted"))
    }
}

/// Open a DualSense for rumble output using the same collection ranking as lightbar.
/// Bluetooth pads get a one-time calibration feature request so motors are accepted.
fn open_rumble_device(api: &HidApi, target: &str) -> Option<OpenDevice> {
    // Prefer USB gamepad, then USB other, then BT gamepad, then BT other.
    let mut best: Option<(OpenDevice, i32)> = None;
    for info in api.device_list().filter(|d| is_dualsense_device(d)) {
        let open_started = Instant::now();
        let hint = info.serial_number().unwrap_or("");
        let device = {
            let _op = crate::controller::hid::diag::enter_op("open_device");
            match info.open_device(api) {
                Ok(d) => {
                    crate::controller::hid::diag::trace_open(
                        "rumble",
                        info,
                        hint,
                        open_started.elapsed().as_millis(),
                        Ok(()),
                    );
                    d
                }
                Err(err) => {
                    crate::controller::hid::diag::trace_open(
                        "rumble",
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
        let is_gamepad = is_dualsense_gamepad(info);
        let rank = match (is_usb, is_gamepad) {
            (true, true) => 0,
            (true, false) => 1,
            (false, true) => 2,
            (false, false) => 3,
        };
        let replace = match &best {
            None => true,
            Some((_, prev_rank)) => rank < *prev_rank,
        };
        if replace {
            if is_bluetooth {
                lightbar::prepare_bt_output_mode(&device);
            }
            best = Some((
                OpenDevice {
                    device,
                    is_bluetooth,
                    product: product_name(info.product_id()).to_string(),
                },
                rank,
            ));
        }
    }
    best.map(|(open, _)| open)
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

/// Worker-owned low-battery orange pulse (replaces the App side thread).
///
/// Timing matches the old pulse thread: gap → for each target orange → restore → gap.
/// Yields to Identify (no RGB writes while an Identify session is live) and to the
/// lightbar enable flag.
struct LowBatteryPulse {
    targets: Vec<(String, u8)>,
    phase: PulsePhase,
}

enum PulsePhase {
    /// Waiting before the next orange burst.
    Gap { until: Instant },
    /// Holding orange on `targets[index]` until restore.
    Orange { index: usize, until: Instant },
}

impl LowBatteryPulse {
    fn new() -> Self {
        Self {
            targets: Vec::new(),
            phase: PulsePhase::Gap {
                until: Instant::now() + Duration::from_millis(LOW_BATTERY_PULSE_GAP_MS),
            },
        }
    }

    fn set_targets(&mut self, targets: Vec<(String, u8)>) {
        self.targets = targets;
        if self.targets.is_empty() {
            self.phase = PulsePhase::Gap {
                until: Instant::now() + Duration::from_millis(LOW_BATTERY_PULSE_GAP_MS),
            };
        }
    }

    fn wake_at(&self) -> Option<Instant> {
        if self.targets.is_empty() {
            return None;
        }
        Some(match &self.phase {
            PulsePhase::Gap { until } | PulsePhase::Orange { until, .. } => *until,
        })
    }

    /// Advance the pulse schedule. Returns true when an RGB write ran.
    fn tick(&mut self, cache: &mut DeviceCache, identifying: bool, now: Instant) -> bool {
        if self.targets.is_empty() {
            return false;
        }
        match self.phase {
            PulsePhase::Gap { until } if now >= until => {
                if identifying || !lightbar::is_enabled() {
                    // Reschedule gap without writing — Identify / disable owns the bar.
                    self.phase = PulsePhase::Gap {
                        until: now + Duration::from_millis(LOW_BATTERY_PULSE_GAP_MS),
                    };
                    return false;
                }
                let (serial, _) = &self.targets[0];
                let _ = write_rgb_exclusive(cache, serial, LOW_BATTERY_ORANGE);
                self.phase = PulsePhase::Orange {
                    index: 0,
                    until: now + Duration::from_millis(LOW_BATTERY_PULSE_ON_MS),
                };
                true
            }
            PulsePhase::Orange { index, until } if now >= until => {
                if identifying || !lightbar::is_enabled() {
                    self.phase = PulsePhase::Gap {
                        until: now + Duration::from_millis(LOW_BATTERY_PULSE_GAP_MS),
                    };
                    return false;
                }
                let (serial, percent) = self.targets[index].clone();
                let color = color_for_battery_percent(percent);
                let _ = write_rgb_exclusive(cache, &serial, color);
                let next = index + 1;
                if next < self.targets.len() {
                    let (serial, _) = &self.targets[next];
                    let _ = write_rgb_exclusive(cache, serial, LOW_BATTERY_ORANGE);
                    self.phase = PulsePhase::Orange {
                        index: next,
                        until: now + Duration::from_millis(LOW_BATTERY_PULSE_ON_MS),
                    };
                } else {
                    self.phase = PulsePhase::Gap {
                        until: now + Duration::from_millis(LOW_BATTERY_PULSE_GAP_MS),
                    };
                }
                true
            }
            _ => false,
        }
    }
}

/// Cloneable handle to enqueue DualSense HID work.
#[derive(Clone)]
pub struct HidWorkerHandle {
    tx: Sender<HidCmd>,
    identifying: Arc<AtomicBool>,
    input_hot: Arc<AtomicBool>,
    presence: Arc<Mutex<Vec<String>>>,
    /// Latest battery/product per serial from the input sample path.
    live_pads: Arc<Mutex<HashMap<String, LivePadStatus>>>,
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
        let live_pads = Arc::new(Mutex::new(HashMap::new()));
        let live_pads_worker = Arc::clone(&live_pads);
        let input_snapshot = Arc::new(Mutex::new(InputSnapshot::default()));
        start_input::set_input_snapshot(Arc::clone(&input_snapshot));
        let last_noisy_log = Arc::new(Mutex::new(Instant::now() - PULSE_SAMPLE_LOG_INTERVAL));
        let last_noisy_log_worker = Arc::clone(&last_noisy_log);

        thread::Builder::new()
            .name("hid-worker".into())
            .spawn(move || {
                crate::controller::hid::diag::start_worker_watchdog();
                worker_loop(
                    rx,
                    identifying_worker,
                    input_hot_worker,
                    last_noisy_log_worker,
                    input_snapshot,
                    presence_worker,
                    live_pads_worker,
                );
            })
            .expect("spawn hid-worker");

        Self {
            tx,
            identifying,
            input_hot,
            presence,
            live_pads,
        }
    }

    pub fn identifying(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.identifying)
    }

    /// Latest DualSense gamepad HID paths from the worker's throttled refresh.
    pub fn presence_paths(&self) -> Vec<String> {
        self.presence.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Controller statuses built from the hot input sample path (no exclusive Poll).
    pub fn live_controllers(&self) -> Vec<ControllerStatus> {
        let Ok(guard) = self.live_pads.lock() else {
            return Vec::new();
        };
        let mut out: Vec<ControllerStatus> = guard
            .iter()
            .map(|(serial, pad)| {
                battery::dualsense_status(
                    0,
                    pad.product.clone(),
                    pad.reading.connection,
                    serial.clone(),
                    pad.reading.percent,
                    pad.reading.state,
                )
            })
            .collect();
        out.sort_by(|a, b| a.serial.cmp(&b.serial));
        for (i, c) in out.iter_mut().enumerate() {
            c.index = i + 1;
        }
        out
    }

    /// Elevate input sampling to ~60 Hz while start-nav / gesture record need it.
    pub fn set_input_hot(&self, hot: bool) {
        self.input_hot.store(hot, Ordering::Relaxed);
    }

    pub fn input_hot(&self) -> bool {
        self.input_hot.load(Ordering::Relaxed)
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

    /// Pulse DualSense motors for `duration_ms`, then auto-stop.
    pub fn rumble(&self, serial: String, right: u8, left: u8, duration_ms: u64) {
        let _ = self.tx.send(HidCmd::Rumble {
            serial,
            right,
            left,
            duration_ms,
        });
    }

    /// Stop all active rumble and drop rumble handles.
    pub fn rumble_stop_all(&self) {
        let _ = self.tx.send(HidCmd::RumbleStopAll);
    }

    /// Replace the low-battery orange pulse targets (empty clears).
    pub fn set_low_battery_targets(&self, targets: Vec<(String, u8)>) {
        let _ = self.tx.send(HidCmd::SetLowBatteryTargets { targets });
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(HidCmd::Shutdown);
    }
}

fn identify_flash_color(step: u32, normal: Rgb) -> Rgb {
    if step.is_multiple_of(2) {
        lightbar::IDENTIFY_FLASH
    } else {
        normal
    }
}

/// Lightbar output must not use the input-pump handle: long-lived read handles on
/// Windows/DualSense often accept `write` with `Ok` without updating the bar.
/// Drop the cached handle, then open fresh for claim-once + RGB (do **not** force
/// `LIGHT_OUT` just because the input handle existed — that fades the bar before
/// every SetRgb while start-nav is sampling).
fn write_rgb_exclusive(
    cache: &mut DeviceCache,
    serial: &str,
    color: Rgb,
) -> (Result<(), String>, HidPhaseTiming) {
    cache.drop_serial(serial);
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

fn sample_one(
    cache: &mut DeviceCache,
    live_pads: &Mutex<HashMap<String, LivePadStatus>>,
    serial: &str,
    input_hot: bool,
) -> SampleOneResult {
    let _op = crate::controller::hid::diag::enter_op("sample_one");
    let open = match cache.ensure(serial) {
        Ok(o) => o,
        Err(_) => {
            forget_live_pad(live_pads, serial);
            return SampleOneResult::HardFail;
        }
    };
    let is_bluetooth = open.is_bluetooth;
    let product = open.product.clone();
    match start_input::read_device_sample_short(&open.device, is_bluetooth, serial, input_hot) {
        ShortSampleOutcome::Ok { sample, battery } => {
            if let Some(reading) = battery {
                remember_live_pad(live_pads, serial, product, reading);
            }
            SampleOneResult::Ok(start_input::hid_nav_reading(serial, sample))
        }
        ShortSampleOutcome::Timeout => SampleOneResult::Timeout,
        ShortSampleOutcome::Fail(_) => {
            cache.drop_serial(serial);
            forget_live_pad(live_pads, serial);
            SampleOneResult::HardFail
        }
    }
}

fn remember_live_pad(
    live_pads: &Mutex<HashMap<String, LivePadStatus>>,
    serial: &str,
    product: String,
    reading: battery::BatteryReading,
) {
    let key = normalize_identity(serial);
    if let Ok(mut guard) = live_pads.lock() {
        guard.insert(key, LivePadStatus { product, reading });
    }
}

fn forget_live_pad(live_pads: &Mutex<HashMap<String, LivePadStatus>>, serial: &str) {
    let key = normalize_identity(serial);
    if let Ok(mut guard) = live_pads.lock() {
        guard.remove(&key);
    }
}

fn prune_live_pads(live_pads: &Mutex<HashMap<String, LivePadStatus>>, live_serials: &[String]) {
    let Ok(mut guard) = live_pads.lock() else {
        return;
    };
    let live: std::collections::HashSet<&str> = live_serials.iter().map(|s| s.as_str()).collect();
    guard.retain(|serial, _| live.contains(serial.as_str()));
}

enum SampleOneResult {
    Ok(NavReading),
    /// Keep handle; caller should reuse the previous snapshot reading.
    Timeout,
    HardFail,
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
    emit_edge: bool,
) {
    let _op = crate::controller::hid::diag::enter_op("publish_snapshot");
    let prev_pads = snapshot.lock().map(|g| g.readings.len()).unwrap_or(0);
    let gap_ms = snapshot_age_ms(snapshot);
    let was_empty = prev_pads == 0;
    let now_empty = readings.is_empty();
    let pad_count = readings.len();

    let meta = if let Ok(mut guard) = snapshot.lock() {
        guard.seq = guard.seq.saturating_add(1);
        guard.published_at = Instant::now();
        guard.reason = reason;
        guard.readings = readings.clone();
        SnapshotMeta {
            published_at: guard.published_at,
            seq: guard.seq,
            reason: guard.reason,
            pad_count: guard.readings.len(),
        }
    } else {
        SnapshotMeta {
            published_at: Instant::now(),
            seq: 0,
            reason,
            pad_count,
        }
    };

    if emit_edge {
        start_input::push_input_edge(start_input::InputEdge::from_snapshot(readings, &meta));
    }

    let clearing = matches!(
        reason,
        SnapshotReason::ClearedPoll
            | SnapshotReason::ClearedPowerOff
            | SnapshotReason::ClearedShutdown
    );
    if clearing && !was_empty {
        crate::controller::hid::diag::diag_info(format!(
            "hid-diag: snapshot clear reason={} prev_pads={prev_pads}",
            reason.as_str()
        ));
    } else if !now_empty && was_empty {
        let after = after_cmd.unwrap_or("-");
        crate::controller::hid::diag::diag_info(format!(
            "hid-diag: snapshot restore pads={pad_count} gap_ms={gap_ms} after={after}"
        ));
    }
    let _ = last_noisy_log;
}

/// Sample all cached DualSense pads; open any missing connected pads first.
fn sample_all_inputs(
    cache: &mut DeviceCache,
    live_pads: &Mutex<HashMap<String, LivePadStatus>>,
    snapshot: &Mutex<InputSnapshot>,
    input_hot: bool,
    last_noisy_log: &Mutex<Instant>,
    after_cmd: Option<&str>,
) {
    cache.ensure_all_pads(input_hot);
    let serials: Vec<String> = cache.devices.keys().cloned().collect();
    prune_live_pads(live_pads, &serials);
    let cached = serials.len();
    let prev_by_id: HashMap<String, NavReading> = snapshot
        .lock()
        .map(|g| {
            g.readings
                .iter()
                .map(|r| (r.id.0.clone(), r.clone()))
                .collect()
        })
        .unwrap_or_default();
    let mut out = Vec::with_capacity(serials.len());
    for serial in serials {
        match sample_one(cache, live_pads, &serial, input_hot) {
            SampleOneResult::Ok(reading) => out.push(reading),
            SampleOneResult::Timeout => {
                let key = format!("hid:{serial}");
                if let Some(prev) = prev_by_id.get(&key) {
                    out.push(prev.clone());
                }
            }
            SampleOneResult::HardFail => {}
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
            crate::controller::hid::diag::diag_info(format!(
                "hid-diag: sample short cached={cached} published={}",
                out.len()
            ));
        }
    }
    publish_snapshot(snapshot, out, reason, after_cmd, last_noisy_log, input_hot);
}

/// Refresh the snapshot entry for one serial (Identify path).
fn sample_serial_into_snapshot(
    cache: &mut DeviceCache,
    live_pads: &Mutex<HashMap<String, LivePadStatus>>,
    snapshot: &Mutex<InputSnapshot>,
    serial: &str,
    input_hot: bool,
) {
    match sample_one(cache, live_pads, serial, input_hot) {
        SampleOneResult::Ok(reading) => {
            let edge = if let Ok(mut guard) = snapshot.lock() {
                guard.seq = guard.seq.saturating_add(1);
                guard.published_at = Instant::now();
                guard.reason = SnapshotReason::Identify;
                if let Some(slot) = guard.readings.iter_mut().find(|r| r.id.0 == reading.id.0) {
                    *slot = reading;
                } else {
                    guard.readings.push(reading);
                }
                Some(start_input::InputEdge::from_snapshot(
                    guard.readings.clone(),
                    &SnapshotMeta {
                        published_at: guard.published_at,
                        seq: guard.seq,
                        reason: guard.reason,
                        pad_count: guard.readings.len(),
                    },
                ))
            } else {
                None
            };
            if input_hot && let Some(edge) = edge {
                start_input::push_input_edge(edge);
            }
        }
        SampleOneResult::Timeout | SampleOneResult::HardFail => {}
    }
}

fn log_cmd_begin(cmd: &str, snapshot: &Mutex<InputSnapshot>) {
    let since_pub_ms = snapshot_age_ms(snapshot);
    crate::controller::hid::diag::diag_info(format!(
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
    crate::controller::hid::diag::diag_info(format!(
        "hid-diag: sample stalled gap_ms={gap_ms} in_cmd={in_cmd}"
    ));
}

#[allow(clippy::too_many_arguments)]
fn begin_identify(
    cache: &mut DeviceCache,
    live_pads: &Mutex<HashMap<String, LivePadStatus>>,
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
        let _op = crate::controller::hid::diag::enter_op("cmd_Identify");
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
    sample_serial_into_snapshot(cache, live_pads, snapshot, &serial, input_hot);
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
    live_pads: &Mutex<HashMap<String, LivePadStatus>>,
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
        sample_all_inputs(
            cache,
            live_pads,
            snapshot,
            input_hot,
            last_noisy_log,
            Some("Identify"),
        );
        return;
    }

    log_cmd_begin("IdentifyFlash", snapshot);
    let color = identify_flash_color(s.next_write, s.normal);
    let serial = s.serial.clone();
    let (result, t) = {
        let _op = crate::controller::hid::diag::enter_op("cmd_IdentifyFlash");
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
    sample_serial_into_snapshot(cache, live_pads, snapshot, &serial, input_hot);
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

fn stop_all_rumble(rumble: &mut RumbleCache, active: &mut Vec<ActiveRumble>) {
    for (serial, open) in rumble.devices.iter() {
        let _ = rumble::stop_rumble_on_device(&open.device, open.is_bluetooth, serial);
    }
    active.clear();
    rumble.drop_all();
}

fn write_rumble_pulse(
    cache: &mut DeviceCache,
    rumble: &mut RumbleCache,
    serial: &str,
    right: u8,
    left: u8,
) -> Result<(), String> {
    let target = normalize_identity(serial);

    // Prefer long-lived exclusive rumble handle. On write failure, drop and re-probe.
    for attempt in 0..2u8 {
        if attempt > 0 {
            rumble.drop_serial(&target);
            crate::controller::hid::diag::diag_info(format!(
                "hid-diag: rumble re-probe serial={target}"
            ));
        }
        match rumble.ensure(&cache.api, &target) {
            Ok(open) => {
                let is_bt = open.is_bluetooth;
                match rumble::set_rumble_on_device(&open.device, is_bt, &target, right, left) {
                    Ok(()) => {
                        crate::controller::hid::diag::diag_info(format!(
                            "hid-diag: rumble ok serial={target} r={right} l={left} bt={}",
                            u8::from(is_bt)
                        ));
                        return Ok(());
                    }
                    Err(err) => {
                        crate::controller::hid::diag::diag_warn(format!(
                            "hid-diag: rumble write fail serial={target} attempt={attempt} err={err}"
                        ));
                        if attempt == 0 {
                            continue;
                        }
                        // Fall through to input-handle fallback below.
                        break;
                    }
                }
            }
            Err(open_err) => {
                crate::controller::hid::diag::diag_warn(format!(
                    "hid-diag: rumble open fail serial={target} err={open_err}"
                ));
                break;
            }
        }
    }

    // Last resort: input-cache handle (often accepts Ok without motors — warn once).
    if let Ok(open) = cache.ensure(&target) {
        if !rumble.fallback_warned {
            app_log::warn(
                "rumble: exclusive open/write failed; writing on input handle (may be silent)",
            );
            rumble.fallback_warned = true;
        }
        return rumble::set_rumble_on_device(&open.device, open.is_bluetooth, &target, right, left)
            .map_err(|e| e.to_string());
    }
    Err(format!("rumble: no handle for {target}"))
}

fn expire_rumble(rumble: &mut RumbleCache, active: &mut Vec<ActiveRumble>, now: Instant) {
    let mut still = Vec::with_capacity(active.len());
    for entry in active.drain(..) {
        if entry.stop_at > now {
            still.push(entry);
            continue;
        }
        if let Some(open) = rumble.devices.get(&normalize_identity(&entry.serial)) {
            let _ = rumble::stop_rumble_on_device(&open.device, open.is_bluetooth, &entry.serial);
        }
    }
    *active = still;
}

/// Run the PowerOff body: stop rumble, drop handles, clear snapshot, send feature report.
fn execute_power_off(
    serial: &str,
    session: &mut Option<IdentifySession>,
    cache: &mut DeviceCache,
    live_pads: &Mutex<HashMap<String, LivePadStatus>>,
    rumble: &mut RumbleCache,
    active_rumble: &mut Vec<ActiveRumble>,
    snapshot: &Mutex<InputSnapshot>,
    identifying: &AtomicBool,
    last_noisy_log: &Mutex<Instant>,
    input_hot: bool,
) {
    if session.as_ref().is_some_and(|s| s.serial == serial) {
        abort_identify(session, cache, identifying);
    }
    stop_all_rumble(rumble, active_rumble);
    cache.drop_all();
    if let Ok(mut guard) = live_pads.lock() {
        guard.clear();
    }
    publish_snapshot(
        snapshot,
        Vec::new(),
        SnapshotReason::ClearedPowerOff,
        Some("PowerOff"),
        last_noisy_log,
        input_hot,
    );
    log_cmd_begin("PowerOff", snapshot);
    let started = Instant::now();
    let (result, timing) = {
        let _ = cache.refresh_device_list();
        battery::power_off_bluetooth_timed(&cache.api, serial)
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
}

#[allow(clippy::too_many_arguments)]
fn handle_cmd(
    cmd: HidCmd,
    session: &mut Option<IdentifySession>,
    cache: &mut DeviceCache,
    live_pads: &Mutex<HashMap<String, LivePadStatus>>,
    rumble: &mut RumbleCache,
    active_rumble: &mut Vec<ActiveRumble>,
    pending_power_off: &mut Option<PendingPowerOff>,
    pulse: &mut LowBatteryPulse,
    snapshot: &Mutex<InputSnapshot>,
    identifying: &AtomicBool,
    last_noisy_log: &Mutex<Instant>,
    input_hot: bool,
) -> bool {
    match cmd {
        HidCmd::Shutdown => {
            *pending_power_off = None;
            abort_identify(session, cache, identifying);
            stop_all_rumble(rumble, active_rumble);
            pulse.set_targets(Vec::new());
            cache.drop_all();
            if let Ok(mut guard) = live_pads.lock() {
                guard.clear();
            }
            publish_snapshot(
                snapshot,
                Vec::new(),
                SnapshotReason::ClearedShutdown,
                Some("Shutdown"),
                last_noisy_log,
                false,
            );
            true
        }
        HidCmd::Identify { serial, percent } => {
            abort_identify(session, cache, identifying);
            identifying.store(true, Ordering::SeqCst);
            *session = begin_identify(
                cache,
                live_pads,
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
            // Abort Identify for this pad immediately (going away), even if deferred.
            if session.as_ref().is_some_and(|s| s.serial == serial) {
                abort_identify(session, cache, identifying);
            }
            let now = Instant::now();
            if let Some(not_before) = rumble_defer_until(active_rumble, now) {
                let remain_ms = not_before.saturating_duration_since(now).as_millis();
                crate::controller::hid::diag::diag_info(format!(
                    "hid-diag: power-off deferred serial={} remain_ms={remain_ms}",
                    normalize_identity(&serial)
                ));
                queue_pending_power_off(pending_power_off, serial, not_before);
                return false;
            }
            execute_power_off(
                &serial,
                session,
                cache,
                live_pads,
                rumble,
                active_rumble,
                snapshot,
                identifying,
                last_noisy_log,
                input_hot,
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
        HidCmd::Rumble {
            serial,
            right,
            left,
            duration_ms,
        } => {
            let target = normalize_identity(&serial);
            // Retrigger: replace any pending stop for this serial.
            active_rumble.retain(|e| normalize_identity(&e.serial) != target);
            match write_rumble_pulse(cache, rumble, &target, right, left) {
                Ok(()) => {
                    if duration_ms > 0 && (right > 0 || left > 0) {
                        active_rumble.push(ActiveRumble {
                            serial: target,
                            stop_at: Instant::now() + Duration::from_millis(duration_ms),
                        });
                    } else if let Some(open) = rumble.devices.get(&target) {
                        let _ =
                            rumble::stop_rumble_on_device(&open.device, open.is_bluetooth, &target);
                    }
                }
                Err(err) => {
                    crate::controller::hid::diag::diag_warn(format!(
                        "rumble write failed for {target}: {err}"
                    ));
                }
            }
            false
        }
        HidCmd::RumbleStopAll => {
            stop_all_rumble(rumble, active_rumble);
            false
        }
        HidCmd::SetLowBatteryTargets { targets } => {
            pulse.set_targets(targets);
            false
        }
        HidCmd::Poll { previously, reply } => {
            // Release input + rumble handles so Poll can open; keep last nav readings.
            stop_all_rumble(rumble, active_rumble);
            cache.drop_all();
            // Cold Poll refreshes battery itself; clear hot-path map so stale
            // readings cannot shadow a later hot reconnect.
            if let Ok(mut guard) = live_pads.lock() {
                guard.clear();
            }
            log_cmd_begin("Poll", snapshot);
            let started = Instant::now();
            let (result, timing) = {
                let _op = crate::controller::hid::diag::enter_op("cmd_Poll");
                let _ = cache.refresh_device_list();
                poll::poll_controllers_timed(&cache.api, &previously)
            };
            // Seed live map from Poll so a hot transition has immediate statuses.
            if let Ok(ref controllers) = result
                && let Ok(mut guard) = live_pads.lock()
            {
                for c in controllers {
                    guard.insert(
                        normalize_identity(&c.serial),
                        LivePadStatus {
                            product: c.product.clone(),
                            reading: battery::BatteryReading {
                                percent: c.percent,
                                state: c.state,
                                connection: c.connection,
                            },
                        },
                    );
                }
            }
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
    live_pads: Arc<Mutex<HashMap<String, LivePadStatus>>>,
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
    let mut rumble = RumbleCache::new();
    let mut active_rumble: Vec<ActiveRumble> = Vec::new();
    let mut pending_power_off: Option<PendingPowerOff> = None;
    let mut pulse = LowBatteryPulse::new();
    let mut session: Option<IdentifySession> = None;
    let mut stall_logged = false;
    let mut in_cmd = "none";

    loop {
        crate::controller::hid::diag::note_progress("worker_loop");
        let hot = input_hot.load(Ordering::Relaxed);
        let now = Instant::now();
        expire_rumble(&mut rumble, &mut active_rumble, now);
        for serial in take_due_power_offs(&mut pending_power_off, &active_rumble, now) {
            execute_power_off(
                &serial,
                &mut session,
                &mut cache,
                &live_pads,
                &mut rumble,
                &mut active_rumble,
                &input_snapshot,
                &identifying,
                &last_noisy_log,
                hot,
            );
        }
        let identifying_now = session.is_some() || identifying.load(Ordering::SeqCst);
        if pulse.tick(&mut cache, identifying_now, now) {
            in_cmd = "LowBatteryPulse";
        }
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
                &live_pads,
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

        // Prefer immediate cmd handling; never sleep before draining HID when hot.
        match rx.try_recv() {
            Ok(cmd) => {
                in_cmd = match &cmd {
                    HidCmd::Poll { .. } => "Poll",
                    HidCmd::Identify { .. } => "Identify",
                    HidCmd::PowerOff { .. } => "PowerOff",
                    HidCmd::SetRgb { .. } => "SetRgb",
                    HidCmd::Rumble { .. } => "Rumble",
                    HidCmd::RumbleStopAll => "RumbleStopAll",
                    HidCmd::SetLowBatteryTargets { .. } => "SetLowBatteryTargets",
                    HidCmd::Shutdown => "Shutdown",
                };
                if handle_cmd(
                    cmd,
                    &mut session,
                    &mut cache,
                    &live_pads,
                    &mut rumble,
                    &mut active_rumble,
                    &mut pending_power_off,
                    &mut pulse,
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
                continue;
            }
            Err(mpsc::TryRecvError::Disconnected) => break,
            Err(mpsc::TryRecvError::Empty) => {}
        }

        let pulse_pending = pulse
            .wake_at()
            .is_some_and(|t| t <= Instant::now() + ACTIVE_POLL);
        let power_off_waiting = pending_power_off.is_some();
        if hot
            || session.is_some()
            || !active_rumble.is_empty()
            || pulse_pending
            || power_off_waiting
        {
            if let Some(s) = session.as_ref() {
                let serial = s.serial.clone();
                let until_flash = s
                    .wake_at
                    .saturating_duration_since(Instant::now())
                    .min(IDENTIFY_SAMPLE_POLL);
                sample_serial_into_snapshot(&mut cache, &live_pads, &input_snapshot, &serial, hot);
                if !until_flash.is_zero() {
                    thread::sleep(until_flash.min(ACTIVE_POLL));
                }
            } else if hot {
                sample_all_inputs(
                    &mut cache,
                    &live_pads,
                    &input_snapshot,
                    hot,
                    &last_noisy_log,
                    None,
                );
            } else {
                // Rumble/pulse stop pending while input is cold — wake soon, no sample.
                let sleep_for = pulse
                    .wake_at()
                    .map(|t| t.saturating_duration_since(Instant::now()))
                    .unwrap_or(ACTIVE_POLL)
                    .min(ACTIVE_POLL);
                if !sleep_for.is_zero() {
                    thread::sleep(sleep_for);
                }
            }
            continue;
        }

        // Idle: wait for a command, presence wake, or next pulse tick.
        let idle_timeout = pulse
            .wake_at()
            .map(|t| t.saturating_duration_since(Instant::now()))
            .unwrap_or(BACKGROUND_POLL)
            .min(BACKGROUND_POLL)
            .max(Duration::from_millis(1));
        match rx.recv_timeout(idle_timeout) {
            Ok(cmd) => {
                in_cmd = match &cmd {
                    HidCmd::Poll { .. } => "Poll",
                    HidCmd::Identify { .. } => "Identify",
                    HidCmd::PowerOff { .. } => "PowerOff",
                    HidCmd::SetRgb { .. } => "SetRgb",
                    HidCmd::Rumble { .. } => "Rumble",
                    HidCmd::RumbleStopAll => "RumbleStopAll",
                    HidCmd::SetLowBatteryTargets { .. } => "SetLowBatteryTargets",
                    HidCmd::Shutdown => "Shutdown",
                };
                if handle_cmd(
                    cmd,
                    &mut session,
                    &mut cache,
                    &live_pads,
                    &mut rumble,
                    &mut active_rumble,
                    &mut pending_power_off,
                    &mut pulse,
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
                sample_all_inputs(
                    &mut cache,
                    &live_pads,
                    &input_snapshot,
                    hot,
                    &last_noisy_log,
                    None,
                );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rumble_defer_until_none_when_empty() {
        let now = Instant::now();
        assert!(rumble_defer_until(&[], now).is_none());
    }

    #[test]
    fn rumble_defer_until_ignores_already_due() {
        let now = Instant::now();
        let active = vec![ActiveRumble {
            serial: "a".into(),
            stop_at: now,
        }];
        assert!(rumble_defer_until(&active, now).is_none());
    }

    #[test]
    fn rumble_defer_until_is_latest_future_stop() {
        let now = Instant::now();
        let early = now + Duration::from_millis(40);
        let late = now + Duration::from_millis(90);
        let active = vec![
            ActiveRumble {
                serial: "a".into(),
                stop_at: early,
            },
            ActiveRumble {
                serial: "b".into(),
                stop_at: late,
            },
        ];
        assert_eq!(rumble_defer_until(&active, now), Some(late));
    }

    #[test]
    fn pending_power_off_waits_until_deadline() {
        let now = Instant::now();
        let stop = now + Duration::from_millis(90);
        let mut pending = None;
        queue_pending_power_off(&mut pending, "pad1".into(), stop);

        let active = vec![ActiveRumble {
            serial: "pad1".into(),
            stop_at: stop,
        }];
        assert!(take_due_power_offs(&mut pending, &active, now).is_empty());
        assert!(pending.is_some());

        let due = take_due_power_offs(&mut pending, &[], stop);
        assert_eq!(due, vec!["pad1".to_string()]);
        assert!(pending.is_none());
    }

    #[test]
    fn pending_power_off_ready_when_pulses_cleared_early() {
        let now = Instant::now();
        let stop = now + Duration::from_millis(90);
        let mut pending = None;
        queue_pending_power_off(&mut pending, "pad1".into(), stop);
        // RumbleStopAll / expire cleared pulses before the clock deadline.
        let due = take_due_power_offs(&mut pending, &[], now);
        assert_eq!(due, vec!["pad1".to_string()]);
    }

    #[test]
    fn second_power_off_joins_pending_with_shared_deadline() {
        let now = Instant::now();
        let early = now + Duration::from_millis(50);
        let late = now + Duration::from_millis(90);
        let mut pending = None;
        queue_pending_power_off(&mut pending, "a".into(), early);
        queue_pending_power_off(&mut pending, "b".into(), late);
        let p = pending.as_ref().expect("pending");
        assert_eq!(p.serials, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(p.not_before, late);
    }
}
