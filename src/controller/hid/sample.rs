//! HID report sampling behind [`HidTransport`](super::transport::HidTransport).
//!
//! Behavior-preserving move of the drain loop previously in
//! [`crate::domain::pad`]: pure pad types (`PadSample`, `NavReading`,
//! `ShortSampleOutcome`, `SampleFail`, `merge_nav_samples`) stay in
//! `domain::pad`; this module owns the HID I/O (blocking `read_timeout`
//! drain + Bluetooth full-report sniff) so tests can feed canned reports via
//! a mock transport. All logging / diag strings are unchanged.

use super::policy::Policy;
use super::transport::HidTransport;
use crate::controller::dualsense::battery;
use crate::domain::pad::{PadSample, SampleFail, ShortSampleOutcome, merge_nav_samples};
use crate::domain::protocol::{
    BT_REPORT_FULL, BT_REPORT_SIZE, BT_REPORT_TRUNCATED, CALIBRATION_FEATURE_REPORT,
    CALIBRATION_FEATURE_SIZE, USB_REPORT_ID, USB_REPORT_SIZE,
};
use std::time::Instant;

/// Options for one [`drain_sample`] call (owns the BT full-report sniff state).
pub(crate) struct SampleOpts {
    pub(crate) is_bluetooth: bool,
    pub(crate) serial: String,
    pub(crate) bus: String,
    pub(crate) started: Instant,
    pub(crate) requested_full: bool,
}

impl SampleOpts {
    pub(crate) fn new(is_bluetooth: bool, serial: &str) -> Self {
        Self {
            is_bluetooth,
            serial: serial.to_string(),
            bus: if is_bluetooth { "bt" } else { "usb" }.to_string(),
            started: Instant::now(),
            requested_full: false,
        }
    }
}

/// One DualSense report read (optional block). Used by the drain loop.
pub(crate) fn read_one_report<T: HidTransport>(
    device: &T,
    opts: &mut SampleOpts,
    report_size: usize,
    timeout_ms: i32,
) -> Result<Option<(PadSample, Option<battery::BatteryReading>)>, SampleFail> {
    let mut buf = vec![0u8; report_size];
    let n = {
        let _op = crate::controller::hid::diag::enter_op("read_timeout");
        match device.read_timeout(&mut buf, timeout_ms) {
            Ok(n) => n,
            Err(err) => {
                drop(_op);
                let fail = if err.to_string().to_lowercase().contains("timeout") {
                    SampleFail::Timeout
                } else {
                    SampleFail::Io
                };
                if fail == SampleFail::Timeout && timeout_ms == 0 {
                    return Ok(None);
                }
                if fail == SampleFail::Timeout {
                    crate::controller::hid::diag::diag_info(format!(
                        "hid-diag: sample timeout keep_handle serial={} bus={}",
                        opts.serial, opts.bus
                    ));
                    return Err(SampleFail::Timeout);
                }
                crate::controller::hid::diag::trace_read(
                    "sample",
                    &opts.serial,
                    &opts.bus,
                    opts.started.elapsed().as_millis(),
                    &format!("fail={} err={err}", fail.as_str()),
                    false,
                );
                return Err(fail);
            }
        }
    };
    if n == 0 {
        return Ok(None);
    }

    if opts.is_bluetooth && buf[0] == BT_REPORT_TRUNCATED {
        if !opts.requested_full {
            let mut feature = vec![0u8; CALIBRATION_FEATURE_SIZE];
            feature[0] = CALIBRATION_FEATURE_REPORT;
            let feature_result = {
                let _op = crate::controller::hid::diag::enter_op("get_feature_report");
                device.get_feature_report(&mut feature)
            };
            match feature_result {
                Ok(_) => {
                    opts.requested_full = true;
                    return Ok(None);
                }
                Err(err) => {
                    crate::controller::hid::diag::trace_read(
                        "sample",
                        &opts.serial,
                        &opts.bus,
                        opts.started.elapsed().as_millis(),
                        &format!("fail=feature err={err}"),
                        false,
                    );
                    return Err(SampleFail::Feature);
                }
            }
        }
        crate::controller::hid::diag::trace_read(
            "sample",
            &opts.serial,
            &opts.bus,
            opts.started.elapsed().as_millis(),
            "fail=truncated",
            false,
        );
        return Err(SampleFail::Truncated);
    }

    let expected = if opts.is_bluetooth {
        BT_REPORT_FULL
    } else {
        USB_REPORT_ID
    };
    if buf[0] != expected {
        crate::controller::hid::diag::trace_read(
            "sample",
            &opts.serial,
            &opts.bus,
            opts.started.elapsed().as_millis(),
            &format!("fail=bad_report id=0x{:02x} n={n}", buf[0]),
            false,
        );
        return Err(SampleFail::BadReport);
    }

    let sample = crate::controller::dualsense::input::parse_report(
        &buf,
        if opts.is_bluetooth { 1 } else { 0 },
    );
    let battery = crate::controller::dualsense::battery::parse_battery_from_report(
        &buf[..n],
        opts.is_bluetooth,
    );
    Ok(Some((sample, battery)))
}

fn drain_queued<T: HidTransport>(
    device: &T,
    opts: &mut SampleOpts,
    report_size: usize,
    policy: &Policy,
    batch: &mut Vec<PadSample>,
    battery: &mut Option<battery::BatteryReading>,
) -> Result<(), SampleFail> {
    for _ in 0..policy.drain_cap {
        match read_one_report(device, opts, report_size, 0)? {
            Some((sample, batt)) => {
                batch.push(sample);
                if batt.is_some() {
                    *battery = batt;
                }
            }
            None => break,
        }
    }
    Ok(())
}

/// Drain queued DualSense reports, wait one report interval, then drain again.
///
/// Newest sticks win; digital buttons are OR'd across the batch so a tap that
/// released inside the drain still appears held for one snapshot. The blocking
/// wait runs even after a non-empty zero-timeout drain so the hot path paces to
/// the pad's report clock instead of busy-spinning.
///
/// Never call from the UI thread. Truncated BT: one feature request (one-time) + retry.
pub(crate) fn drain_sample<T: HidTransport>(
    device: &T,
    opts: &mut SampleOpts,
    policy: &Policy,
) -> ShortSampleOutcome {
    let report_size = if opts.is_bluetooth {
        BT_REPORT_SIZE
    } else {
        USB_REPORT_SIZE
    };
    let mut batch: Vec<PadSample> = Vec::new();
    let mut battery = None;

    // Drain anything already queued (immediate).
    if let Err(fail) = drain_queued(device, opts, report_size, policy, &mut batch, &mut battery) {
        return match fail {
            SampleFail::Timeout => ShortSampleOutcome::Timeout,
            other => ShortSampleOutcome::Fail(other),
        };
    }

    // Always wait one report interval so a non-empty drain cannot busy-spin the
    // worker: pace to the pad's report clock, and merge any report that arrives.
    match read_one_report(device, opts, report_size, policy.report_wait_ms) {
        Ok(Some((sample, batt))) => {
            batch.push(sample);
            if batt.is_some() {
                battery = batt;
            }
        }
        Ok(None) | Err(SampleFail::Timeout) => {
            if batch.is_empty() {
                return ShortSampleOutcome::Timeout;
            }
        }
        Err(other) => return ShortSampleOutcome::Fail(other),
    }
    if let Err(fail) = drain_queued(device, opts, report_size, policy, &mut batch, &mut battery) {
        return match fail {
            SampleFail::Timeout => ShortSampleOutcome::Timeout,
            other => ShortSampleOutcome::Fail(other),
        };
    }

    let Some(merged) = merge_nav_samples(&batch) else {
        return ShortSampleOutcome::Timeout;
    };
    latch_diag(&batch, &merged);

    crate::controller::hid::diag::trace_read(
        "sample",
        &opts.serial,
        &opts.bus,
        opts.started.elapsed().as_millis(),
        &format!("{} drain_n={}", sample_summary(&merged), batch.len()),
        true,
    );
    ShortSampleOutcome::Ok {
        sample: merged,
        battery,
    }
}

fn sample_summary(sample: &PadSample) -> String {
    format!(
        "ok stick_x={:.2} stick_y={:.2} l2={} r2={} cross={} circle={} square={} triangle={} options={} create={} dpad_u={} dpad_d={} dpad_l={} dpad_r={}",
        sample.stick_x,
        sample.stick_y,
        u8::from(sample.l2),
        u8::from(sample.r2),
        u8::from(sample.cross),
        u8::from(sample.circle),
        u8::from(sample.square),
        u8::from(sample.triangle),
        u8::from(sample.options),
        u8::from(sample.create),
        u8::from(sample.dpad_up),
        u8::from(sample.dpad_down),
        u8::from(sample.dpad_left),
        u8::from(sample.dpad_right),
    )
}

fn latch_diag(batch: &[PadSample], merged: &PadSample) {
    let Some(last) = batch.last() else {
        return;
    };
    let latched = (merged.cross && !last.cross)
        || (merged.circle && !last.circle)
        || (merged.square && !last.square)
        || (merged.triangle && !last.triangle)
        || (merged.options && !last.options)
        || (merged.create && !last.create)
        || (merged.l2 && !last.l2)
        || (merged.r2 && !last.r2);
    if latched {
        crate::controller::hid::diag::diag_info(format!(
            "hid-diag: drain latched buttons n={} cross={} circle={} square={} triangle={}",
            batch.len(),
            u8::from(merged.cross && !last.cross),
            u8::from(merged.circle && !last.circle),
            u8::from(merged.square && !last.square),
            u8::from(merged.triangle && !last.triangle),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::hid::transport::MockTransport;
    use crate::domain::protocol::{USB_REPORT_ID, USB_REPORT_SIZE};

    /// Transport seam proof: one canned USB report drains to a parsed sample.
    #[test]
    fn drain_sample_parses_canned_usb_report() {
        let mut report = vec![0u8; USB_REPORT_SIZE];
        report[0] = USB_REPORT_ID;
        report[1] = 128;
        report[2] = 128;
        report[3] = 128;
        report[4] = 128;
        // D-pad neutral (0x08) + Cross (0x20).
        report[8] = 0x28;
        // Battery level 5, discharging -> 55%.
        report[53] = 0x05;

        let transport = MockTransport::new();
        transport.push_read(Ok(report));

        let mut opts = SampleOpts::new(false, "test-serial");
        let policy = Policy::default();
        match drain_sample(&transport, &mut opts, &policy) {
            ShortSampleOutcome::Ok { sample, battery } => {
                assert!(sample.cross);
                assert!(!sample.circle);
                let reading = battery.expect("battery parsed from same report");
                assert_eq!(reading.percent, 55);
                assert!(reading.connection.is_usb());
            }
            other => panic!("expected Ok sample, got {other:?}"),
        }
    }
}
