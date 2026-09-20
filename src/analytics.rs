//! Opt-in local battery step analytics (charge / play duration estimates).
//!
//! Learns per DualSense bucket edge (last-3 durations). Interruptions clear only
//! the in-progress bucket timer — committed steps survive. Remaining-time ETA
//! can speculate from a single qualifying step (half-bucket 100↔95 excluded as
//! a rate source), and interpolates within the current bucket using accrued
//! active time. Display labels floor to duration-tier breakpoints. Nothing
//! leaves the machine.

use crate::app_log;
use crate::battery::{ControllerStatus, LOW_BATTERY_PERCENT, PowerState};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How many durations to keep per drain/charge step edge.
pub const STEP_RING_CAP: usize = 3;
/// Max controller serials retained in the store.
pub const SERIAL_CAP: usize = 32;
/// Drop pads not seen for this long (unless remembered / nicknamed).
pub const STALE_SERIAL_DAYS: u64 = 180;

/// Empty DualSense bucket (level 0 mid-point).
const EMPTY_PERCENT: u8 = LOW_BATTERY_PERCENT;
/// Full pack.
const FULL_PERCENT: u8 = 100;

/// DualSense reported percents, high → low (100, then 95…5).
const DRAIN_LEVELS: [u8; 11] = [100, 95, 85, 75, 65, 55, 45, 35, 25, 15, 5];

const MIN_STEP_SECS: u64 = 60;
const MAX_STEP_SECS: u64 = 6 * 60 * 60;

/// Heartbeat gaps larger than this are treated as "app was closed" and skipped.
pub const HEARTBEAT_MAX_GAP: Duration = Duration::from_secs(150);

/// After connect, DualSense percents are often wrong for a while (especially BT).
/// Do not open or commit bucket windows until this grace elapses.
pub const CONNECT_GRACE: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BucketDirection {
    Drain,
    Charge,
}

/// In-progress time in the current DualSense bucket (not a session).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InProgressBucket {
    pub direction: BucketDirection,
    pub percent: u8,
    pub active_ms: u64,
}

/// Last-3 duration samples for one directed percent edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepSample {
    pub from_percent: u8,
    pub to_percent: u8,
    #[serde(default)]
    pub samples_ms: Vec<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct SerialRecord {
    last_seen_ms: u64,
    #[serde(default)]
    drain_steps: Vec<StepSample>,
    #[serde(default)]
    charge_steps: Vec<StepSample>,
    #[serde(default)]
    in_progress: Option<InProgressBucket>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct AnalyticsFile {
    #[serde(default)]
    controllers: HashMap<String, SerialRecord>,
}

/// One step on the coverage chart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepCoverage {
    pub from_percent: u8,
    pub to_percent: u8,
    /// Measured median when this edge has samples.
    pub typical_ms: Option<u64>,
    /// True when unmeasured but a qualifying rate exists to fill this gap.
    pub speculative: bool,
}

/// Snapshot of estimates / coverage for UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerAnalytics {
    pub serial: String,
    pub typical_charge: Option<Duration>,
    pub typical_play: Option<Duration>,
    pub in_progress: Option<InProgressBucket>,
    pub drain_coverage: Vec<StepCoverage>,
    pub charge_coverage: Vec<StepCoverage>,
}

/// Remaining-time hint for tray/toast rings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EtaHint {
    PlayLeft(Duration),
    ChargeToFull(Duration),
}

#[derive(Debug, Clone, Default)]
pub struct AnalyticsStore {
    by_serial: HashMap<String, SerialRecord>,
    dirty: bool,
    last_tick_ms: Option<u64>,
    /// Wall-clock ms when the current connect grace began (runtime only).
    connected_at_ms: HashMap<String, u64>,
    /// Serials that have been reconciled once after leaving connect grace.
    grace_reconciled: std::collections::HashSet<String>,
}

impl AnalyticsStore {
    pub fn load() -> Self {
        let path = store_path();
        let Ok(bytes) = fs::read(&path) else {
            return Self::default();
        };
        match serde_json::from_slice::<AnalyticsFile>(&bytes) {
            Ok(file) => Self {
                by_serial: file.controllers,
                dirty: false,
                last_tick_ms: None,
                connected_at_ms: HashMap::new(),
                grace_reconciled: std::collections::HashSet::new(),
            },
            Err(err) => {
                app_log::warn(format!(
                    "failed to parse analytics at {}: {err}; starting empty",
                    path.display()
                ));
                Self::default()
            }
        }
    }

    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let path = store_path();
        if let Some(parent) = path.parent()
            && let Err(err) = fs::create_dir_all(parent)
        {
            app_log::warn(format!("failed to create analytics dir: {err}"));
            return;
        }
        let file = AnalyticsFile {
            controllers: self.by_serial.clone(),
        };
        match serde_json::to_vec_pretty(&file) {
            Ok(bytes) => {
                if let Err(err) = fs::write(&path, bytes) {
                    app_log::warn(format!("failed to write analytics: {err}"));
                    return;
                }
                self.dirty = false;
            }
            Err(err) => app_log::warn(format!("failed to serialize analytics: {err}")),
        }
    }

    /// Wipe learned analytics (kept for tests / future tooling; Settings no longer exposes this).
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.by_serial.clear();
        self.last_tick_ms = None;
        self.connected_at_ms.clear();
        self.grace_reconciled.clear();
        self.dirty = true;
        self.save();
    }

    pub fn is_storable_serial(serial: &str) -> bool {
        if !crate::dualsense::is_storable_serial(serial) {
            return false;
        }
        #[cfg(not(feature = "dev-emulate"))]
        if serial.starts_with("emu-") {
            return false;
        }
        true
    }

    /// Emulated pads skip connect grace so developer presets start windows immediately.
    fn skips_connect_grace(serial: &str) -> bool {
        serial.starts_with("emu-")
    }

    fn in_connect_grace(&self, serial: &str, now_ms: u64) -> bool {
        if Self::skips_connect_grace(serial) {
            return false;
        }
        self.connected_at_ms.get(serial).is_some_and(|started| {
            now_ms.saturating_sub(*started) < CONNECT_GRACE.as_millis() as u64
        })
    }

    /// Record a connect edge. Brief HID absences keep the original grace clock;
    /// longer absences (or first connect) start a new grace and drop any stale window.
    fn on_connect(&mut self, serial: &str, now_ms: u64, previous_last_seen: Option<u64>) {
        if Self::skips_connect_grace(serial) {
            return;
        }
        let brief_blip = previous_last_seen
            .filter(|_| self.connected_at_ms.contains_key(serial))
            .is_some_and(|last| {
                now_ms.saturating_sub(last) <= HEARTBEAT_MAX_GAP.as_millis() as u64
            });
        if brief_blip {
            return;
        }
        self.connected_at_ms.insert(serial.to_string(), now_ms);
        self.grace_reconciled.remove(serial);
        self.clear_in_progress(serial, now_ms);
    }

    /// Developer helper: seed full drain/charge step chains so ETA UI can be checked.
    #[cfg(feature = "dev-emulate")]
    pub fn dev_seed_estimates(&mut self, serial: &str) {
        if !Self::is_storable_serial(serial) {
            return;
        }
        let now_ms = system_time_ms(SystemTime::now());
        self.ensure_record(serial, now_ms);
        let record = self.by_serial.get_mut(serial).expect("ensured");
        // ~8h full drain across 10 steps; ~2h full charge across 10 steps.
        let drain_step = 8 * 60 * 60 * 1000 / 10;
        let charge_step = 2 * 60 * 60 * 1000 / 10;
        record.drain_steps.clear();
        record.charge_steps.clear();
        for (from, to) in drain_edges() {
            push_step_sample(&mut record.drain_steps, from, to, drain_step);
        }
        for (from, to) in charge_edges() {
            push_step_sample(&mut record.charge_steps, from, to, charge_step);
        }
        record.in_progress = None;
        record.last_seen_ms = now_ms;
        self.dirty = true;
    }

    /// Developer helper: credit active time on the in-progress bucket.
    #[cfg(feature = "dev-emulate")]
    pub fn dev_credit_active(&mut self, serial: &str, extra: Duration) {
        let Some(record) = self.by_serial.get_mut(serial) else {
            return;
        };
        let Some(progress) = record.in_progress.as_mut() else {
            return;
        };
        progress.active_ms = progress.active_ms.saturating_add(extra.as_millis() as u64);
        self.dirty = true;
    }

    /// Observe a poll snapshot. When `enabled` is false, do nothing.
    pub fn observe(
        &mut self,
        previous: &[ControllerStatus],
        next: &[ControllerStatus],
        enabled: bool,
        keep_serial: impl Fn(&str) -> bool,
        now: SystemTime,
    ) {
        if !enabled {
            return;
        }
        let now_ms = system_time_ms(now);
        let gap_ms = self
            .last_tick_ms
            .map(|prev| now_ms.saturating_sub(prev))
            .unwrap_or(0);
        let heartbeat_ok = gap_ms > 0 && gap_ms <= HEARTBEAT_MAX_GAP.as_millis() as u64;
        self.last_tick_ms = Some(now_ms);

        let prev_by: HashMap<&str, &ControllerStatus> =
            previous.iter().map(|c| (c.serial.as_str(), c)).collect();
        let next_by: HashMap<&str, &ControllerStatus> =
            next.iter().map(|c| (c.serial.as_str(), c)).collect();

        // Heartbeat: accrue only while the pad is present.
        if heartbeat_ok {
            for controller in next {
                if !Self::is_storable_serial(&controller.serial) {
                    continue;
                }
                if let Some(record) = self.by_serial.get_mut(&controller.serial)
                    && let Some(progress) = record.in_progress.as_mut()
                    && direction_matches(progress.direction, controller.state)
                    && progress.percent == controller.percent
                {
                    progress.active_ms = progress.active_ms.saturating_add(gap_ms);
                    record.last_seen_ms = now_ms;
                    self.dirty = true;
                }
            }
        }

        // Disconnect: pause accrual (keep in_progress; no commit).
        for prev in previous {
            if !Self::is_storable_serial(&prev.serial) {
                continue;
            }
            if next_by.contains_key(prev.serial.as_str()) {
                continue;
            }
            if let Some(record) = self.by_serial.get_mut(&prev.serial) {
                record.last_seen_ms = now_ms;
                self.dirty = true;
            }
        }

        for controller in next {
            if !Self::is_storable_serial(&controller.serial) {
                continue;
            }
            let prev = prev_by.get(controller.serial.as_str()).copied();
            self.on_controller(prev, controller, now_ms);
        }

        self.prune(now_ms, keep_serial);
    }

    fn on_controller(
        &mut self,
        prev: Option<&ControllerStatus>,
        next: &ControllerStatus,
        now_ms: u64,
    ) {
        let serial = next.serial.as_str();
        let previous_last_seen = self.by_serial.get(serial).map(|r| r.last_seen_ms);
        self.ensure_record(serial, now_ms);

        if prev.is_none() {
            self.on_connect(serial, now_ms, previous_last_seen);
        }

        if self.in_connect_grace(serial, now_ms) {
            if let Some(record) = self.by_serial.get_mut(serial) {
                record.last_seen_ms = now_ms;
            }
            return;
        }

        if !Self::skips_connect_grace(serial) && !self.grace_reconciled.contains(serial) {
            self.grace_reconciled.insert(serial.to_string());
            let progress = self
                .by_serial
                .get(serial)
                .and_then(|r| r.in_progress.clone());
            if let Some(p) = progress {
                let mismatch =
                    p.percent != next.percent || !direction_matches(p.direction, next.state);
                if mismatch {
                    self.clear_in_progress(serial, now_ms);
                }
            }
        }

        let want = match next.state {
            PowerState::Discharging => Some(BucketDirection::Drain),
            PowerState::Charging => Some(BucketDirection::Charge),
            PowerState::Complete => {
                self.finish_charge_to_full(serial, prev, now_ms);
                return;
            }
            _ => {
                self.clear_in_progress(serial, now_ms);
                return;
            }
        };
        let want = want.expect("discharging or charging");

        let progress = self
            .by_serial
            .get(serial)
            .and_then(|r| r.in_progress.clone());

        // Mode flip or wrong-way percent: drop timer only.
        if let Some(ref p) = progress {
            let wrong_way = match want {
                BucketDirection::Drain => next.percent > p.percent,
                BucketDirection::Charge => next.percent < p.percent,
            };
            if p.direction != want || wrong_way {
                self.clear_in_progress(serial, now_ms);
            }
        }

        let progress = self
            .by_serial
            .get(serial)
            .and_then(|r| r.in_progress.clone());

        match want {
            BucketDirection::Drain => {
                if let Some(p) = progress {
                    if next.percent < p.percent {
                        self.commit_step(
                            serial,
                            BucketDirection::Drain,
                            p.percent,
                            next.percent,
                            p.active_ms,
                            now_ms,
                        );
                        self.start_in_progress(
                            serial,
                            BucketDirection::Drain,
                            next.percent,
                            now_ms,
                        );
                    } else if let Some(record) = self.by_serial.get_mut(serial) {
                        record.last_seen_ms = now_ms;
                    }
                } else {
                    self.start_in_progress(serial, BucketDirection::Drain, next.percent, now_ms);
                }
            }
            BucketDirection::Charge => {
                if let Some(p) = progress {
                    if next.percent > p.percent {
                        self.commit_step(
                            serial,
                            BucketDirection::Charge,
                            p.percent,
                            next.percent,
                            p.active_ms,
                            now_ms,
                        );
                        self.start_in_progress(
                            serial,
                            BucketDirection::Charge,
                            next.percent,
                            now_ms,
                        );
                    } else if next.percent == p.percent
                        && let Some(record) = self.by_serial.get_mut(serial)
                    {
                        record.last_seen_ms = now_ms;
                    }
                } else {
                    self.start_in_progress(serial, BucketDirection::Charge, next.percent, now_ms);
                }
            }
        }
    }

    fn finish_charge_to_full(
        &mut self,
        serial: &str,
        prev: Option<&ControllerStatus>,
        now_ms: u64,
    ) {
        self.ensure_record(serial, now_ms);
        let progress = self
            .by_serial
            .get(serial)
            .and_then(|r| r.in_progress.clone());
        if let Some(p) = progress {
            if p.direction == BucketDirection::Charge && p.percent < FULL_PERCENT {
                self.commit_step(
                    serial,
                    BucketDirection::Charge,
                    p.percent,
                    FULL_PERCENT,
                    p.active_ms,
                    now_ms,
                );
            }
        } else if let Some(prev) = prev
            && prev.state == PowerState::Charging
            && prev.percent < FULL_PERCENT
        {
            // Edge Complete without in_progress (e.g. just enabled) — nothing to commit.
        }
        self.clear_in_progress(serial, now_ms);
    }

    fn start_in_progress(
        &mut self,
        serial: &str,
        direction: BucketDirection,
        percent: u8,
        now_ms: u64,
    ) {
        let record = self.by_serial.get_mut(serial).expect("ensured");
        record.in_progress = Some(InProgressBucket {
            direction,
            percent,
            active_ms: 0,
        });
        record.last_seen_ms = now_ms;
        self.dirty = true;
    }

    fn clear_in_progress(&mut self, serial: &str, now_ms: u64) {
        let Some(record) = self.by_serial.get_mut(serial) else {
            return;
        };
        if record.in_progress.take().is_some() {
            self.dirty = true;
        }
        record.last_seen_ms = now_ms;
    }

    fn commit_step(
        &mut self,
        serial: &str,
        direction: BucketDirection,
        from: u8,
        to: u8,
        active_ms: u64,
        now_ms: u64,
    ) {
        if from == to {
            return;
        }
        let secs = active_ms / 1000;
        if !(MIN_STEP_SECS..=MAX_STEP_SECS).contains(&secs) {
            if let Some(record) = self.by_serial.get_mut(serial) {
                record.last_seen_ms = now_ms;
                self.dirty = true;
            }
            return;
        }
        let edges = match direction {
            BucketDirection::Drain => expand_drain(from, to),
            BucketDirection::Charge => expand_charge(from, to),
        };
        let Some(record) = self.by_serial.get_mut(serial) else {
            return;
        };
        let steps = match direction {
            BucketDirection::Drain => &mut record.drain_steps,
            BucketDirection::Charge => &mut record.charge_steps,
        };
        if edges.len() <= 1 {
            // Single known edge, or unrecognized jump stored as-is.
            let (a, b) = edges.first().copied().unwrap_or((from, to));
            push_step_sample(steps, a, b, active_ms);
        } else {
            let per = active_ms / edges.len() as u64;
            if per >= MIN_STEP_SECS * 1000 {
                for (a, b) in edges {
                    push_step_sample(steps, a, b, per);
                }
            }
            // Multi-bucket jump with too little time per edge — skip rather than poison.
        }
        record.last_seen_ms = now_ms;
        self.dirty = true;
    }

    fn ensure_record(&mut self, serial: &str, now_ms: u64) {
        self.by_serial.entry(serial.to_string()).or_insert_with(|| {
            self.dirty = true;
            SerialRecord {
                last_seen_ms: now_ms,
                ..SerialRecord::default()
            }
        });
    }

    fn prune(&mut self, now_ms: u64, keep_serial: impl Fn(&str) -> bool) {
        let stale_ms = Duration::from_secs(STALE_SERIAL_DAYS * 24 * 60 * 60).as_millis() as u64;
        let before = self.by_serial.len();
        self.by_serial.retain(|serial, record| {
            if keep_serial(serial) {
                return true;
            }
            now_ms.saturating_sub(record.last_seen_ms) <= stale_ms
        });
        if self.by_serial.len() != before {
            self.dirty = true;
        }

        while self.by_serial.len() > SERIAL_CAP {
            let oldest = self
                .by_serial
                .iter()
                .filter(|(serial, _)| !keep_serial(serial))
                .min_by_key(|(_, r)| r.last_seen_ms)
                .map(|(s, _)| s.clone());
            let Some(serial) = oldest else {
                break;
            };
            self.by_serial.remove(&serial);
            self.dirty = true;
        }
    }

    pub fn typical_charge(&self, serial: &str) -> Option<Duration> {
        let record = self.by_serial.get(serial)?;
        let ms = speculative_chain_ms(&record.charge_steps, &charge_edges())?;
        Some(Duration::from_millis(ms))
    }

    pub fn typical_play(&self, serial: &str) -> Option<Duration> {
        let record = self.by_serial.get(serial)?;
        let ms = speculative_chain_ms(&record.drain_steps, &drain_edges())?;
        Some(Duration::from_millis(ms))
    }

    pub fn in_progress(&self, serial: &str) -> Option<&InProgressBucket> {
        self.by_serial
            .get(serial)
            .and_then(|r| r.in_progress.as_ref())
    }

    pub fn panel_rows(&self) -> Vec<ControllerAnalytics> {
        let mut serials: Vec<_> = self.by_serial.keys().cloned().collect();
        serials.sort();
        serials
            .into_iter()
            .filter_map(|serial| {
                let record = self.by_serial.get(&serial)?;
                if record.drain_steps.is_empty()
                    && record.charge_steps.is_empty()
                    && record.in_progress.is_none()
                {
                    return None;
                }
                Some(ControllerAnalytics {
                    serial: serial.clone(),
                    typical_charge: self.typical_charge(&serial),
                    typical_play: self.typical_play(&serial),
                    in_progress: self.in_progress(&serial).cloned(),
                    drain_coverage: coverage_list(&record.drain_steps, &drain_edges()),
                    charge_coverage: coverage_list(&record.charge_steps, &charge_edges()),
                })
            })
            .collect()
    }

    pub fn eta_for(&self, controller: &ControllerStatus) -> Option<EtaHint> {
        match controller.state {
            PowerState::Discharging => self.eta_play_at(&controller.serial, controller.percent),
            PowerState::Charging => self.eta_charge_at(&controller.serial, controller.percent),
            _ => None,
        }
    }

    /// Remaining play time at `percent` (used for live drain and disconnected last-known %).
    pub fn eta_play_at(&self, serial: &str, percent: u8) -> Option<EtaHint> {
        let record = self.by_serial.get(serial)?;
        let edges = drain_edges_from(percent)?;
        let ms = speculative_chain_ms(&record.drain_steps, &edges)?;
        let ms = interpolate_remaining_ms(
            ms,
            &edges,
            &record.drain_steps,
            record.in_progress.as_ref(),
            BucketDirection::Drain,
            percent,
        );
        let left = Duration::from_millis(ms);
        if left.is_zero() {
            None
        } else {
            Some(EtaHint::PlayLeft(left))
        }
    }

    fn eta_charge_at(&self, serial: &str, percent: u8) -> Option<EtaHint> {
        let record = self.by_serial.get(serial)?;
        let edges = charge_edges_from(percent)?;
        let ms = speculative_chain_ms(&record.charge_steps, &edges)?;
        let ms = interpolate_remaining_ms(
            ms,
            &edges,
            &record.charge_steps,
            record.in_progress.as_ref(),
            BucketDirection::Charge,
            percent,
        );
        let left = Duration::from_millis(ms);
        if left.is_zero() {
            None
        } else {
            Some(EtaHint::ChargeToFull(left))
        }
    }
}

fn direction_matches(direction: BucketDirection, state: PowerState) -> bool {
    match direction {
        BucketDirection::Drain => state.is_discharging(),
        BucketDirection::Charge => state == PowerState::Charging,
    }
}

fn drain_edges() -> Vec<(u8, u8)> {
    DRAIN_LEVELS.windows(2).map(|w| (w[0], w[1])).collect()
}

fn charge_edges() -> Vec<(u8, u8)> {
    let mut levels = DRAIN_LEVELS;
    levels.reverse();
    levels.windows(2).map(|w| (w[0], w[1])).collect()
}

fn drain_edges_from(current: u8) -> Option<Vec<(u8, u8)>> {
    if current <= EMPTY_PERCENT {
        return None;
    }
    let start = DRAIN_LEVELS.iter().position(|&p| p <= current)?;
    if start + 1 >= DRAIN_LEVELS.len() {
        return None;
    }
    Some(
        DRAIN_LEVELS[start..]
            .windows(2)
            .map(|w| (w[0], w[1]))
            .collect(),
    )
}

fn charge_edges_from(current: u8) -> Option<Vec<(u8, u8)>> {
    if current >= FULL_PERCENT {
        return None;
    }
    let mut levels = DRAIN_LEVELS;
    levels.reverse(); // 5, 15, …, 100
    let start = levels.iter().position(|&p| p >= current)?;
    if start + 1 >= levels.len() {
        return None;
    }
    Some(levels[start..].windows(2).map(|w| (w[0], w[1])).collect())
}

fn expand_drain(from: u8, to: u8) -> Vec<(u8, u8)> {
    if from <= to {
        return Vec::new();
    }
    let Some(i0) = DRAIN_LEVELS.iter().position(|&p| p == from) else {
        return Vec::new();
    };
    let Some(i1) = DRAIN_LEVELS.iter().position(|&p| p == to) else {
        return Vec::new();
    };
    if i1 <= i0 {
        return Vec::new();
    }
    DRAIN_LEVELS[i0..=i1]
        .windows(2)
        .map(|w| (w[0], w[1]))
        .collect()
}

fn expand_charge(from: u8, to: u8) -> Vec<(u8, u8)> {
    if to <= from {
        return Vec::new();
    }
    let mut levels = DRAIN_LEVELS;
    levels.reverse();
    let Some(i0) = levels.iter().position(|&p| p == from) else {
        return Vec::new();
    };
    let Some(i1) = levels.iter().position(|&p| p == to) else {
        return Vec::new();
    };
    if i1 <= i0 {
        return Vec::new();
    }
    levels[i0..=i1].windows(2).map(|w| (w[0], w[1])).collect()
}

fn push_step_sample(steps: &mut Vec<StepSample>, from: u8, to: u8, duration_ms: u64) {
    if let Some(existing) = steps
        .iter_mut()
        .find(|s| s.from_percent == from && s.to_percent == to)
    {
        existing.samples_ms.push(duration_ms);
        while existing.samples_ms.len() > STEP_RING_CAP {
            existing.samples_ms.remove(0);
        }
    } else {
        steps.push(StepSample {
            from_percent: from,
            to_percent: to,
            samples_ms: vec![duration_ms],
        });
    }
}

fn step_median(steps: &[StepSample], from: u8, to: u8) -> Option<u64> {
    steps
        .iter()
        .find(|s| s.from_percent == from && s.to_percent == to)
        .and_then(|s| median_ms(&s.samples_ms))
}

/// Half-bucket DualSense tops — recorded, but never used as speculative rate sources.
fn is_half_bucket_edge(from: u8, to: u8) -> bool {
    (from == FULL_PERCENT && to == 95) || (from == 95 && to == FULL_PERCENT)
}

/// Median ms-per-percent from qualifying (non half-bucket) learned steps.
fn default_rate_ms_per_percent(steps: &[StepSample]) -> Option<u64> {
    let mut rates = Vec::new();
    for step in steps {
        if is_half_bucket_edge(step.from_percent, step.to_percent) {
            continue;
        }
        let Some(med) = median_ms(&step.samples_ms) else {
            continue;
        };
        let delta = u64::from(step.from_percent.abs_diff(step.to_percent));
        if delta == 0 {
            continue;
        }
        rates.push(med / delta);
    }
    median_ms(&rates)
}

/// Sum path duration: measured medians where present, else rate × percent delta.
fn speculative_chain_ms(steps: &[StepSample], edges: &[(u8, u8)]) -> Option<u64> {
    if edges.is_empty() {
        return None;
    }
    let rate = default_rate_ms_per_percent(steps)?;
    let mut total = 0u64;
    for &(from, to) in edges {
        let delta = u64::from(from.abs_diff(to));
        let ms = step_median(steps, from, to).unwrap_or_else(|| rate.saturating_mul(delta));
        total = total.saturating_add(ms);
    }
    Some(total)
}

/// Expected duration of one edge (measured median, else rate × Δ%).
fn edge_duration_ms(steps: &[StepSample], from: u8, to: u8) -> Option<u64> {
    let delta = u64::from(from.abs_diff(to));
    if delta == 0 {
        return None;
    }
    if let Some(med) = step_median(steps, from, to) {
        return Some(med);
    }
    let rate = default_rate_ms_per_percent(steps)?;
    Some(rate.saturating_mul(delta))
}

/// Subtract elapsed time in the current bucket from a full remaining-path sum.
/// Clamps so remaining never drops below the path from the next breakpoint.
fn interpolate_remaining_ms(
    chain_ms: u64,
    edges: &[(u8, u8)],
    steps: &[StepSample],
    in_progress: Option<&InProgressBucket>,
    expected_direction: BucketDirection,
    percent: u8,
) -> u64 {
    let Some(progress) = in_progress else {
        return chain_ms;
    };
    if progress.direction != expected_direction || progress.percent != percent {
        return chain_ms;
    }
    let Some(&(from, to)) = edges.first() else {
        return chain_ms;
    };
    if from != percent {
        return chain_ms;
    }
    let Some(edge_ms) = edge_duration_ms(steps, from, to) else {
        return chain_ms;
    };
    let elapsed = progress.active_ms.min(edge_ms);
    chain_ms.saturating_sub(elapsed)
}

fn coverage_list(steps: &[StepSample], edges: &[(u8, u8)]) -> Vec<StepCoverage> {
    let can_speculate = default_rate_ms_per_percent(steps).is_some();
    edges
        .iter()
        .map(|&(from, to)| {
            let typical_ms = step_median(steps, from, to);
            StepCoverage {
                from_percent: from,
                to_percent: to,
                typical_ms,
                speculative: typical_ms.is_none() && can_speculate,
            }
        })
        .collect()
}

/// Floor minutes to the display tier for the unrounded duration.
/// ≥4h → 30m, ≥2h → 15m, else 5m. Always rounds down.
fn floor_display_mins(total_mins: u64) -> u64 {
    let step = if total_mins >= 4 * 60 {
        30
    } else if total_mins >= 2 * 60 {
        15
    } else {
        5
    };
    (total_mins / step) * step
}

fn format_floored_mins(floored_mins: u64) -> String {
    if floored_mins == 0 {
        return "~<5m".to_string();
    }
    let hours = floored_mins / 60;
    let mins = floored_mins % 60;
    if hours == 0 {
        format!("~{mins}m")
    } else if mins == 0 {
        format!("~{hours}h")
    } else {
        format!("~{hours}h {mins}m")
    }
}

/// Format a duration for UI, floored to tier breakpoints: `~2h 15m`, `~40m`, `~1h`.
pub fn format_duration_short(duration: Duration) -> String {
    let total_mins = duration.as_secs() / 60;
    format_floored_mins(floor_display_mins(total_mins))
}

/// Ring label using the same floored breakpoints as Settings.
pub fn format_eta_ring(hint: EtaHint) -> String {
    let duration = match hint {
        EtaHint::PlayLeft(d) | EtaHint::ChargeToFull(d) => d,
    };
    format_duration_short(duration)
}

fn median_ms(samples: &[u64]) -> Option<u64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        Some((sorted[mid - 1] + sorted[mid]) / 2)
    } else {
        Some(sorted[mid])
    }
}

fn system_time_ms(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn store_path() -> PathBuf {
    crate::paths::data_dir().join("analytics.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad(serial: &str, percent: u8, state: PowerState) -> ControllerStatus {
        ControllerStatus {
            index: 1,
            product: "DualSense",
            connection: "USB",
            serial: serial.to_string(),
            percent,
            state,
        }
    }

    fn ms(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn observe_enabled(
        store: &mut AnalyticsStore,
        prev: &[ControllerStatus],
        next: &[ControllerStatus],
        at: SystemTime,
    ) {
        store.observe(prev, next, true, |_| false, at);
    }

    /// Connect at `t0`, then observe again after connect grace so a bucket window can open.
    fn connect_past_grace(
        store: &mut AnalyticsStore,
        pads: &[ControllerStatus],
        t0: SystemTime,
    ) -> SystemTime {
        observe_enabled(store, &[], pads, t0);
        let after = t0 + CONNECT_GRACE + Duration::from_secs(1);
        observe_enabled(store, pads, pads, after);
        after
    }

    /// Accrue `minutes` of active time at a fixed percent/state via heartbeats.
    fn accrue(
        store: &mut AnalyticsStore,
        serial: &str,
        percent: u8,
        state: PowerState,
        start: SystemTime,
        minutes: u64,
    ) -> SystemTime {
        let cur = vec![pad(serial, percent, state)];
        let mut t = start;
        // 90s heartbeats; need minutes*60/90 ticks.
        let ticks = (minutes * 60 / 90).max(1);
        for _ in 0..ticks {
            t += Duration::from_secs(90);
            observe_enabled(store, &cur, &cur, t);
        }
        t
    }

    #[test]
    fn drain_heartbeats_accrue_until_drop() {
        let mut store = AnalyticsStore::default();
        let forty_five = vec![pad("a", 45, PowerState::Discharging)];
        let t0 = connect_past_grace(&mut store, &forty_five, ms(1_000));
        assert_eq!(store.in_progress("a").map(|p| p.percent), Some(45));
        let t = accrue(&mut store, "a", 45, PowerState::Discharging, t0, 5);
        assert!(store.by_serial["a"].drain_steps.is_empty());
        let thirty_five = vec![pad("a", 35, PowerState::Discharging)];
        observe_enabled(
            &mut store,
            &forty_five,
            &thirty_five,
            t + Duration::from_secs(90),
        );
        let steps = &store.by_serial["a"].drain_steps;
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].from_percent, 45);
        assert_eq!(steps[0].to_percent, 35);
        assert!(steps[0].samples_ms[0] >= MIN_STEP_SECS * 1000);
    }

    #[test]
    fn drain_step_survives_charge() {
        let mut store = AnalyticsStore::default();
        let forty_five = vec![pad("a", 45, PowerState::Discharging)];
        let thirty_five = vec![pad("a", 35, PowerState::Discharging)];
        let t0 = connect_past_grace(&mut store, &forty_five, ms(1_000));
        let t = accrue(&mut store, "a", 45, PowerState::Discharging, t0, 5);
        observe_enabled(
            &mut store,
            &forty_five,
            &thirty_five,
            t + Duration::from_secs(90),
        );
        let charging = vec![pad("a", 35, PowerState::Charging)];
        observe_enabled(
            &mut store,
            &thirty_five,
            &charging,
            t + Duration::from_secs(180),
        );
        assert_eq!(store.by_serial["a"].drain_steps.len(), 1);
        assert_eq!(
            store.in_progress("a").map(|p| p.direction),
            Some(BucketDirection::Charge)
        );
    }

    #[test]
    fn drain_one_step_unlocks_speculative_eta() {
        let mut store = AnalyticsStore::default();
        store.by_serial.insert(
            "a".into(),
            SerialRecord {
                last_seen_ms: 1,
                drain_steps: vec![StepSample {
                    from_percent: 45,
                    to_percent: 35,
                    samples_ms: vec![5 * 60 * 1000],
                }],
                ..SerialRecord::default()
            },
        );
        // 10% in 5 min → 0.5 min per %. At 45%: 45→5 = 40% → 20 min.
        match store.eta_for(&pad("a", 45, PowerState::Discharging)) {
            Some(EtaHint::PlayLeft(d)) => assert_eq!(d.as_secs(), 20 * 60),
            other => panic!("expected ETA at 45%, got {other:?}"),
        }
        // At 25%: 25→5 = 20% → 10 min.
        match store.eta_for(&pad("a", 25, PowerState::Discharging)) {
            Some(EtaHint::PlayLeft(d)) => assert_eq!(d.as_secs(), 10 * 60),
            other => panic!("expected ETA at 25%, got {other:?}"),
        }
        let typical = store.typical_play("a").expect("speculative typical");
        // Full 100→5 = 95% → 47.5 min.
        assert_eq!(typical.as_secs(), 47 * 60 + 30);
    }

    #[test]
    fn drain_half_bucket_alone_does_not_speculate() {
        let mut store = AnalyticsStore::default();
        store.by_serial.insert(
            "a".into(),
            SerialRecord {
                last_seen_ms: 1,
                drain_steps: vec![StepSample {
                    from_percent: 100,
                    to_percent: 95,
                    samples_ms: vec![5 * 60 * 1000],
                }],
                ..SerialRecord::default()
            },
        );
        assert!(
            store
                .eta_for(&pad("a", 100, PowerState::Discharging))
                .is_none()
        );
        assert!(store.typical_play("a").is_none());
    }

    #[test]
    fn drain_measured_preferred_over_speculation() {
        let mut store = AnalyticsStore::default();
        store.by_serial.insert(
            "a".into(),
            SerialRecord {
                last_seen_ms: 1,
                drain_steps: vec![
                    StepSample {
                        from_percent: 45,
                        to_percent: 35,
                        samples_ms: vec![10 * 60 * 1000], // 1 min/%
                    },
                    StepSample {
                        from_percent: 25,
                        to_percent: 15,
                        samples_ms: vec![2 * 60 * 1000], // measured faster
                    },
                ],
                ..SerialRecord::default()
            },
        );
        // Rate from both qualifying: medians 1 min/% and 0.2 min/% → median of rates.
        // rates = [60000, 12000], median of 2 = average = 36000 ms/%
        // At 25%: 25→15 measured 2m + 15→5 speculated 36000*10 = 6m → 8m
        match store.eta_for(&pad("a", 25, PowerState::Discharging)) {
            Some(EtaHint::PlayLeft(d)) => assert_eq!(d.as_secs(), 8 * 60),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn drain_disconnect_pauses_then_drop_commits() {
        let mut store = AnalyticsStore::default();
        let thirty_five = vec![pad("a", 35, PowerState::Discharging)];
        let t0 = connect_past_grace(&mut store, &thirty_five, ms(1_000));
        let t = accrue(&mut store, "a", 35, PowerState::Discharging, t0, 3);
        // Brief absence (under HEARTBEAT_MAX_GAP): pause accrual, keep window + grace clock.
        observe_enabled(&mut store, &thirty_five, &[], t + Duration::from_secs(90));
        assert!(store.in_progress("a").is_some());
        let resume = t + Duration::from_secs(120);
        observe_enabled(&mut store, &[], &thirty_five, resume);
        assert_eq!(store.in_progress("a").map(|p| p.percent), Some(35));
        let t2 = accrue(&mut store, "a", 35, PowerState::Discharging, resume, 3);
        let twenty_five = vec![pad("a", 25, PowerState::Discharging)];
        observe_enabled(
            &mut store,
            &thirty_five,
            &twenty_five,
            t2 + Duration::from_secs(90),
        );
        let sample = store.by_serial["a"].drain_steps[0].samples_ms[0];
        // ~6 minutes total across sittings, not the gap.
        assert!((5 * 60 * 1000..15 * 60 * 1000).contains(&sample));
    }

    #[test]
    fn full_drain_chain_gives_typical_and_eta() {
        let mut store = AnalyticsStore::default();
        let mut steps = Vec::new();
        for (from, to) in drain_edges() {
            steps.push(StepSample {
                from_percent: from,
                to_percent: to,
                samples_ms: vec![10 * 60 * 1000],
            });
        }
        store.by_serial.insert(
            "a".into(),
            SerialRecord {
                last_seen_ms: 1,
                drain_steps: steps,
                ..SerialRecord::default()
            },
        );
        let typical = store.typical_play("a").expect("full chain");
        assert_eq!(typical.as_secs(), 10 * 60 * drain_edges().len() as u64);
        match store.eta_for(&pad("a", 85, PowerState::Discharging)) {
            Some(EtaHint::PlayLeft(d)) => {
                let edges = drain_edges_from(85).unwrap();
                assert_eq!(d.as_secs(), 10 * 60 * edges.len() as u64);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn charge_step_survives_unplug() {
        let mut store = AnalyticsStore::default();
        let five = vec![pad("a", 5, PowerState::Charging)];
        let fifteen = vec![pad("a", 15, PowerState::Charging)];
        let t0 = connect_past_grace(&mut store, &five, ms(1_000));
        let t = accrue(&mut store, "a", 5, PowerState::Charging, t0, 5);
        observe_enabled(&mut store, &five, &fifteen, t + Duration::from_secs(90));
        let discharging = vec![pad("a", 15, PowerState::Discharging)];
        observe_enabled(
            &mut store,
            &fifteen,
            &discharging,
            t + Duration::from_secs(180),
        );
        assert_eq!(store.by_serial["a"].charge_steps.len(), 1);
        assert_eq!(store.by_serial["a"].charge_steps[0].from_percent, 5);
    }

    #[test]
    fn charge_one_step_unlocks_speculative_eta() {
        let mut store = AnalyticsStore::default();
        store.by_serial.insert(
            "a".into(),
            SerialRecord {
                last_seen_ms: 1,
                charge_steps: vec![StepSample {
                    from_percent: 5,
                    to_percent: 15,
                    samples_ms: vec![5 * 60 * 1000],
                }],
                ..SerialRecord::default()
            },
        );
        // 10% in 5 min → 0.5 min/%. At 5%: 5→100 = 95% → 47.5 min.
        match store.eta_for(&pad("a", 5, PowerState::Charging)) {
            Some(EtaHint::ChargeToFull(d)) => assert_eq!(d.as_secs(), 47 * 60 + 30),
            other => panic!("unexpected {other:?}"),
        }
        assert!(store.typical_charge("a").is_some());
    }

    #[test]
    fn charge_half_bucket_alone_does_not_speculate() {
        let mut store = AnalyticsStore::default();
        store.by_serial.insert(
            "a".into(),
            SerialRecord {
                last_seen_ms: 1,
                charge_steps: vec![StepSample {
                    from_percent: 95,
                    to_percent: 100,
                    samples_ms: vec![5 * 60 * 1000],
                }],
                ..SerialRecord::default()
            },
        );
        assert!(store.eta_for(&pad("a", 95, PowerState::Charging)).is_none());
        assert!(store.typical_charge("a").is_none());
    }

    #[test]
    fn charge_complete_commits_final_step() {
        let mut store = AnalyticsStore::default();
        let ninety_five = vec![pad("a", 95, PowerState::Charging)];
        let complete = vec![pad("a", 100, PowerState::Complete)];
        let t0 = connect_past_grace(&mut store, &ninety_five, ms(1_000));
        let t = accrue(&mut store, "a", 95, PowerState::Charging, t0, 5);
        observe_enabled(
            &mut store,
            &ninety_five,
            &complete,
            t + Duration::from_secs(90),
        );
        assert!(store.in_progress("a").is_none());
        let step = store.by_serial["a"]
            .charge_steps
            .iter()
            .find(|s| s.from_percent == 95 && s.to_percent == 100)
            .expect("95→100");
        assert!(!step.samples_ms.is_empty());
    }

    #[test]
    fn connect_grace_skips_window_and_percent_jumps() {
        let mut store = AnalyticsStore::default();
        let wrong = vec![pad("a", 100, PowerState::Discharging)];
        observe_enabled(&mut store, &[], &wrong, ms(1_000));
        assert!(store.in_progress("a").is_none());

        // Settling jump during grace must not open a window or commit a step.
        let settled = vec![pad("a", 65, PowerState::Discharging)];
        observe_enabled(
            &mut store,
            &wrong,
            &settled,
            ms(1_000) + Duration::from_secs(10 * 60),
        );
        assert!(store.in_progress("a").is_none());
        assert!(store.by_serial["a"].drain_steps.is_empty());
    }

    #[test]
    fn connect_grace_then_window_at_settled_percent() {
        let mut store = AnalyticsStore::default();
        let wrong = vec![pad("a", 100, PowerState::Discharging)];
        observe_enabled(&mut store, &[], &wrong, ms(1_000));
        let settled = vec![pad("a", 65, PowerState::Discharging)];
        observe_enabled(
            &mut store,
            &wrong,
            &settled,
            ms(1_000) + Duration::from_secs(5 * 60),
        );

        let after_grace = ms(1_000) + CONNECT_GRACE + Duration::from_secs(1);
        observe_enabled(&mut store, &settled, &settled, after_grace);
        assert_eq!(store.in_progress("a").map(|p| p.percent), Some(65));

        let t = accrue(&mut store, "a", 65, PowerState::Discharging, after_grace, 5);
        let fifty_five = vec![pad("a", 55, PowerState::Discharging)];
        observe_enabled(
            &mut store,
            &settled,
            &fifty_five,
            t + Duration::from_secs(90),
        );
        let steps = &store.by_serial["a"].drain_steps;
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].from_percent, 65);
        assert_eq!(steps[0].to_percent, 55);
    }

    #[test]
    fn overnight_reconnect_resets_connect_grace() {
        let mut store = AnalyticsStore::default();
        let pad35 = vec![pad("a", 35, PowerState::Discharging)];
        let t0 = connect_past_grace(&mut store, &pad35, ms(1_000));
        assert!(store.in_progress("a").is_some());

        observe_enabled(&mut store, &pad35, &[], t0 + Duration::from_secs(90));
        let day2 = t0 + Duration::from_secs(20 * 60 * 60);
        observe_enabled(&mut store, &[], &pad35, day2);
        // True reconnect: stale window cleared and grace starts again.
        assert!(store.in_progress("a").is_none());
        assert!(store.in_connect_grace("a", system_time_ms(day2)));

        let still_grace = day2 + Duration::from_secs(10 * 60);
        observe_enabled(&mut store, &pad35, &pad35, still_grace);
        assert!(store.in_progress("a").is_none());
    }

    #[test]
    fn brief_absence_does_not_reset_connect_grace() {
        let mut store = AnalyticsStore::default();
        let pad45 = vec![pad("a", 45, PowerState::Discharging)];
        let t0 = connect_past_grace(&mut store, &pad45, ms(1_000));
        let connected_at = *store.connected_at_ms.get("a").expect("grace clock");
        assert_eq!(store.in_progress("a").map(|p| p.percent), Some(45));

        observe_enabled(&mut store, &pad45, &[], t0 + Duration::from_secs(30));
        let resume = t0 + Duration::from_secs(90);
        observe_enabled(&mut store, &[], &pad45, resume);
        assert_eq!(
            store.connected_at_ms.get("a").copied(),
            Some(connected_at),
            "brief HID blip must keep original connect time"
        );
        assert_eq!(store.in_progress("a").map(|p| p.percent), Some(45));
        assert!(!store.in_connect_grace("a", system_time_ms(resume)));
    }

    #[test]
    fn step_ring_keeps_three() {
        let mut steps = Vec::new();
        push_step_sample(&mut steps, 45, 35, 1);
        push_step_sample(&mut steps, 45, 35, 2);
        push_step_sample(&mut steps, 45, 35, 3);
        push_step_sample(&mut steps, 45, 35, 4);
        assert_eq!(steps[0].samples_ms, vec![2, 3, 4]);
        assert_eq!(step_median(&steps, 45, 35), Some(3));
    }

    #[test]
    fn format_duration_floors_to_tier() {
        assert_eq!(format_duration_short(Duration::from_secs(40 * 60)), "~40m");
        assert_eq!(
            format_duration_short(Duration::from_secs(2 * 3600 + 15 * 60)),
            "~2h 15m"
        );
        assert_eq!(format_duration_short(Duration::from_secs(3600)), "~1h");
        assert_eq!(
            format_duration_short(Duration::from_secs(4 * 3600 + 29 * 60)),
            "~4h"
        );
        assert_eq!(
            format_duration_short(Duration::from_secs(3 * 3600 + 59 * 60)),
            "~3h 45m"
        );
        assert_eq!(
            format_duration_short(Duration::from_secs(3600 + 59 * 60)),
            "~1h 55m"
        );
        assert_eq!(format_duration_short(Duration::from_secs(4 * 60)), "~<5m");
        assert_eq!(format_duration_short(Duration::from_secs(8 * 3600)), "~8h");
        assert_eq!(format_duration_short(Duration::from_secs(2 * 3600)), "~2h");
    }

    #[test]
    fn format_eta_ring_uses_same_floored_label() {
        assert_eq!(
            format_eta_ring(EtaHint::PlayLeft(Duration::from_secs(8 * 3600))),
            "~8h"
        );
        assert_eq!(
            format_eta_ring(EtaHint::ChargeToFull(Duration::from_secs(40 * 60))),
            "~40m"
        );
        assert_eq!(
            format_eta_ring(EtaHint::PlayLeft(Duration::from_secs(3 * 3600 + 30 * 60))),
            "~3h 30m"
        );
    }

    #[test]
    fn drain_interpolates_within_current_bucket() {
        // 35→25 = 1h, 25→15 = 1h, 15→5 = 1h → at 35%: 3h path; at 25%: 2h.
        // Plan example scaled: 35% = 4h / 25% = 3h needs a longer remaining tail;
        // use 35→25 = 1h and remaining 25→5 = 3h so entry is 4h.
        let mut store = AnalyticsStore::default();
        store.by_serial.insert(
            "a".into(),
            SerialRecord {
                last_seen_ms: 1,
                drain_steps: vec![
                    StepSample {
                        from_percent: 35,
                        to_percent: 25,
                        samples_ms: vec![60 * 60 * 1000],
                    },
                    StepSample {
                        from_percent: 25,
                        to_percent: 15,
                        samples_ms: vec![90 * 60 * 1000],
                    },
                    StepSample {
                        from_percent: 15,
                        to_percent: 5,
                        samples_ms: vec![90 * 60 * 1000],
                    },
                ],
                in_progress: Some(InProgressBucket {
                    direction: BucketDirection::Drain,
                    percent: 35,
                    active_ms: 0,
                }),
                ..SerialRecord::default()
            },
        );
        match store.eta_for(&pad("a", 35, PowerState::Discharging)) {
            Some(EtaHint::PlayLeft(d)) => assert_eq!(d.as_secs(), 4 * 3600),
            other => panic!("expected 4h at entry, got {other:?}"),
        }
        store.by_serial.get_mut("a").unwrap().in_progress = Some(InProgressBucket {
            direction: BucketDirection::Drain,
            percent: 35,
            active_ms: 30 * 60 * 1000, // half of 35→25
        });
        match store.eta_for(&pad("a", 35, PowerState::Discharging)) {
            Some(EtaHint::PlayLeft(d)) => assert_eq!(d.as_secs(), 3 * 3600 + 30 * 60),
            other => panic!("expected 3h 30m, got {other:?}"),
        }
        store.by_serial.get_mut("a").unwrap().in_progress = Some(InProgressBucket {
            direction: BucketDirection::Drain,
            percent: 35,
            active_ms: 45 * 60 * 1000, // three-quarters
        });
        match store.eta_for(&pad("a", 35, PowerState::Discharging)) {
            Some(EtaHint::PlayLeft(d)) => assert_eq!(d.as_secs(), 3 * 3600 + 15 * 60),
            other => panic!("expected 3h 15m, got {other:?}"),
        }
    }

    #[test]
    fn drain_interpolation_clamps_to_next_breakpoint() {
        let mut store = AnalyticsStore::default();
        store.by_serial.insert(
            "a".into(),
            SerialRecord {
                last_seen_ms: 1,
                drain_steps: vec![
                    StepSample {
                        from_percent: 35,
                        to_percent: 25,
                        samples_ms: vec![60 * 60 * 1000],
                    },
                    StepSample {
                        from_percent: 25,
                        to_percent: 15,
                        samples_ms: vec![60 * 60 * 1000],
                    },
                    StepSample {
                        from_percent: 15,
                        to_percent: 5,
                        samples_ms: vec![60 * 60 * 1000],
                    },
                ],
                in_progress: Some(InProgressBucket {
                    direction: BucketDirection::Drain,
                    percent: 35,
                    active_ms: 3 * 60 * 60 * 1000, // past the edge
                }),
                ..SerialRecord::default()
            },
        );
        match store.eta_for(&pad("a", 35, PowerState::Discharging)) {
            Some(EtaHint::PlayLeft(d)) => assert_eq!(d.as_secs(), 2 * 3600),
            other => panic!("expected clamp to 25% path (2h), got {other:?}"),
        }
    }

    #[test]
    fn drain_wrong_in_progress_does_not_interpolate() {
        let mut store = AnalyticsStore::default();
        store.by_serial.insert(
            "a".into(),
            SerialRecord {
                last_seen_ms: 1,
                drain_steps: vec![
                    StepSample {
                        from_percent: 35,
                        to_percent: 25,
                        samples_ms: vec![60 * 60 * 1000],
                    },
                    StepSample {
                        from_percent: 25,
                        to_percent: 15,
                        samples_ms: vec![60 * 60 * 1000],
                    },
                    StepSample {
                        from_percent: 15,
                        to_percent: 5,
                        samples_ms: vec![60 * 60 * 1000],
                    },
                ],
                in_progress: Some(InProgressBucket {
                    direction: BucketDirection::Charge,
                    percent: 35,
                    active_ms: 30 * 60 * 1000,
                }),
                ..SerialRecord::default()
            },
        );
        match store.eta_for(&pad("a", 35, PowerState::Discharging)) {
            Some(EtaHint::PlayLeft(d)) => assert_eq!(d.as_secs(), 3 * 3600),
            other => panic!("expected full path, got {other:?}"),
        }
    }

    #[test]
    fn charge_interpolates_within_current_bucket() {
        let mut store = AnalyticsStore::default();
        store.by_serial.insert(
            "a".into(),
            SerialRecord {
                last_seen_ms: 1,
                charge_steps: vec![
                    StepSample {
                        from_percent: 5,
                        to_percent: 15,
                        samples_ms: vec![20 * 60 * 1000],
                    },
                    StepSample {
                        from_percent: 15,
                        to_percent: 25,
                        samples_ms: vec![20 * 60 * 1000],
                    },
                ],
                in_progress: Some(InProgressBucket {
                    direction: BucketDirection::Charge,
                    percent: 5,
                    active_ms: 10 * 60 * 1000, // half of 5→15
                }),
                ..SerialRecord::default()
            },
        );
        // Remaining path from 5%: need full chain to 100 for eta_charge — seed rate via steps.
        // With only two steps, speculative fills the rest from rate (20min/10% = 2 min/%).
        // Full 5→100 = 95% → 190 min; minus 10 min elapsed → 180 min.
        match store.eta_for(&pad("a", 5, PowerState::Charging)) {
            Some(EtaHint::ChargeToFull(d)) => assert_eq!(d.as_secs(), 180 * 60),
            other => panic!("unexpected {other:?}"),
        }
        // No matching in_progress → full path.
        store.by_serial.get_mut("a").unwrap().in_progress = None;
        match store.eta_for(&pad("a", 5, PowerState::Charging)) {
            Some(EtaHint::ChargeToFull(d)) => assert_eq!(d.as_secs(), 190 * 60),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn prune_stale_and_lru() {
        let mut store = AnalyticsStore::default();
        let now = ms(STALE_SERIAL_DAYS * 24 * 60 * 60 + 10_000);
        let now_ms = system_time_ms(now);
        for i in 0..35 {
            let serial = format!("pad{i:02}");
            store.by_serial.insert(
                serial,
                SerialRecord {
                    last_seen_ms: now_ms.saturating_sub((i as u64 + 1) * 1_000),
                    ..SerialRecord::default()
                },
            );
        }
        store.by_serial.insert(
            "ancient".into(),
            SerialRecord {
                last_seen_ms: 1,
                ..SerialRecord::default()
            },
        );
        store.dirty = true;
        store.prune(now_ms, |s| s == "pad00");
        assert!(!store.by_serial.contains_key("ancient"));
        assert!(store.by_serial.len() <= SERIAL_CAP);
        assert!(store.by_serial.contains_key("pad00"));
    }

    #[test]
    fn disabled_observe_is_noop() {
        let mut store = AnalyticsStore::default();
        let mid = vec![pad("a", 45, PowerState::Discharging)];
        store.observe(&[], &mid, false, |_| false, ms(1_000));
        assert!(store.by_serial.is_empty());
    }
}
