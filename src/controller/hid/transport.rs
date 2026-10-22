//! Byte-pipe seam for HID I/O (additive, no behavior change).
//!
//! Production code still uses `hidapi` directly. This trait captures the two
//! operations `domain::pad` sampling needs so a later commit can move the
//! report parsing behind it and unit-test with canned reports.

use crate::{AppError, AppResult};

/// Minimal blocking HID surface needed for input sampling.
pub trait HidTransport {
    /// Blocking read with timeout (mirrors `hidapi::HidDevice::read_timeout`).
    fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> AppResult<usize>;
    /// Feature-report read (mirrors `hidapi::HidDevice::get_feature_report`).
    fn get_feature_report(&self, buf: &mut [u8]) -> AppResult<usize>;
}

#[cfg(test)]
use std::collections::VecDeque;
#[cfg(test)]
use std::sync::Mutex;

/// Direct `hidapi` delegation (byte-identical behavior).
pub struct HidapiTransport<'a>(pub &'a hidapi::HidDevice);

impl HidTransport for HidapiTransport<'_> {
    fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> AppResult<usize> {
        self.0.read_timeout(buf, timeout_ms).map_err(AppError::from)
    }

    fn get_feature_report(&self, buf: &mut [u8]) -> AppResult<usize> {
        self.0.get_feature_report(buf).map_err(AppError::from)
    }
}

/// Canned-report mock for transport/sample unit tests (no HID hardware).
#[cfg(test)]
pub(crate) struct MockTransport {
    reads: Mutex<VecDeque<AppResult<Vec<u8>>>>,
    features: Mutex<VecDeque<AppResult<Vec<u8>>>>,
}

#[cfg(test)]
impl MockTransport {
    pub(crate) fn new() -> Self {
        Self {
            reads: Mutex::new(VecDeque::new()),
            features: Mutex::new(VecDeque::new()),
        }
    }

    pub(crate) fn push_read(&self, v: AppResult<Vec<u8>>) {
        self.reads.lock().expect("mock lock").push_back(v);
    }
}

#[cfg(test)]
impl HidTransport for MockTransport {
    fn read_timeout(&self, buf: &mut [u8], _timeout_ms: i32) -> AppResult<usize> {
        let next = self
            .reads
            .lock()
            .expect("mock lock")
            .pop_front()
            .unwrap_or(Ok(Vec::new()));
        match next {
            Ok(bytes) => {
                let n = bytes.len().min(buf.len());
                buf[..n].copy_from_slice(&bytes[..n]);
                Ok(n)
            }
            Err(e) => Err(e),
        }
    }

    fn get_feature_report(&self, buf: &mut [u8]) -> AppResult<usize> {
        let next = self
            .features
            .lock()
            .expect("mock lock")
            .pop_front()
            .unwrap_or(Ok(Vec::new()));
        match next {
            Ok(bytes) => {
                let n = bytes.len().min(buf.len());
                buf[..n].copy_from_slice(&bytes[..n]);
                Ok(n)
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_transport_roundtrips() {
        let m = MockTransport::new();
        m.push_read(Ok(vec![1, 2, 3]));
        m.push_read(Err(AppError::Hid("timeout".into())));
        let mut buf = [0u8; 8];
        assert_eq!(m.read_timeout(&mut buf, 4).expect("read"), 3);
        assert_eq!(&buf[..3], &[1, 2, 3]);
        assert_eq!(
            m.read_timeout(&mut buf, 4).unwrap_err().to_string(),
            "timeout"
        );
    }
}
