//! Byte-pipe seam for HID I/O (additive, no behavior change).
//!
//! Production code still uses `hidapi` directly. This trait captures the two
//! operations `domain::pad` sampling needs so a later commit can move the
//! report parsing behind it and unit-test with canned reports.

/// Minimal blocking HID surface needed for input sampling.
pub trait HidTransport {
    /// Blocking read with timeout (mirrors `hidapi::HidDevice::read_timeout`).
    fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> Result<usize, String>;
    /// Feature-report read (mirrors `hidapi::HidDevice::get_feature_report`).
    fn get_feature_report(&self, buf: &mut [u8]) -> Result<usize, String>;
}

/// Direct `hidapi` delegation (byte-identical behavior).
pub struct HidapiTransport<'a>(pub &'a hidapi::HidDevice);

impl HidTransport for HidapiTransport<'_> {
    fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> Result<usize, String> {
        self.0
            .read_timeout(buf, timeout_ms)
            .map_err(|e| e.to_string())
    }

    fn get_feature_report(&self, buf: &mut [u8]) -> Result<usize, String> {
        self.0.get_feature_report(buf).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Canned-report mock for future sample tests (no HID hardware).
    pub struct MockTransport {
        reads: Mutex<VecDeque<Result<Vec<u8>, String>>>,
        features: Mutex<VecDeque<Result<Vec<u8>, String>>>,
    }

    impl MockTransport {
        pub fn new() -> Self {
            Self {
                reads: Mutex::new(VecDeque::new()),
                features: Mutex::new(VecDeque::new()),
            }
        }

        pub fn push_read(&self, v: Result<Vec<u8>, String>) {
            self.reads.lock().expect("mock lock").push_back(v);
        }
    }

    impl HidTransport for MockTransport {
        fn read_timeout(&self, buf: &mut [u8], _timeout_ms: i32) -> Result<usize, String> {
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

        fn get_feature_report(&self, buf: &mut [u8]) -> Result<usize, String> {
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

    #[test]
    fn mock_transport_roundtrips() {
        let m = MockTransport::new();
        m.push_read(Ok(vec![1, 2, 3]));
        m.push_read(Err("timeout".to_string()));
        let mut buf = [0u8; 8];
        assert_eq!(m.read_timeout(&mut buf, 4).expect("read"), 3);
        assert_eq!(&buf[..3], &[1, 2, 3]);
        assert_eq!(m.read_timeout(&mut buf, 4).unwrap_err(), "timeout");
    }
}
