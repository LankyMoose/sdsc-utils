//! Paths present on the first HID enumerate — treat their first read as already connected.

use std::collections::HashSet;

/// Tracks DualSense gamepad HID paths seen at process start until each is identified.
#[derive(Debug, Default)]
pub struct LaunchPaths {
    /// Paths from the first device-list refresh that have not yet resolved to a serial.
    pending: HashSet<String>,
    /// True after the first refresh has seeded [`Self::pending`].
    seeded: bool,
}

impl LaunchPaths {
    pub fn new() -> Self {
        Self::default()
    }

    /// First refresh copies all gamepad paths; later refreshes drop paths that vanished.
    pub fn on_device_list(&mut self, paths: impl IntoIterator<Item = String>) {
        let live: HashSet<String> = paths.into_iter().collect();
        if !self.seeded {
            self.pending = live;
            self.seeded = true;
            return;
        }
        self.pending.retain(|path| live.contains(path));
    }

    /// If `path` was present at launch, remove it and return `serial` to quiet once.
    pub fn take_serial_for_path(&mut self, path: &str, serial: &str) -> Option<String> {
        if self.pending.remove(path) {
            Some(serial.to_string())
        } else {
            None
        }
    }

    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_list_seeds_pending() {
        let mut launch = LaunchPaths::new();
        launch.on_device_list(["\\\\?\\hid\\a".into(), "\\\\?\\hid\\b".into()]);
        assert_eq!(launch.pending_len(), 2);
        assert_eq!(
            launch.take_serial_for_path("\\\\?\\hid\\a", "serial-a"),
            Some("serial-a".into())
        );
        assert!(
            launch
                .take_serial_for_path("\\\\?\\hid\\a", "serial-a")
                .is_none()
        );
        assert_eq!(
            launch.take_serial_for_path("\\\\?\\hid\\b", "serial-b"),
            Some("serial-b".into())
        );
    }

    #[test]
    fn later_list_drops_vanished_paths() {
        let mut launch = LaunchPaths::new();
        launch.on_device_list(["\\\\?\\hid\\a".into(), "\\\\?\\hid\\b".into()]);
        launch.on_device_list(["\\\\?\\hid\\b".into()]);
        assert!(
            launch
                .take_serial_for_path("\\\\?\\hid\\a", "serial-a")
                .is_none()
        );
        assert_eq!(
            launch.take_serial_for_path("\\\\?\\hid\\b", "serial-b"),
            Some("serial-b".into())
        );
    }

    #[test]
    fn path_appearing_after_seed_is_not_quiet() {
        let mut launch = LaunchPaths::new();
        launch.on_device_list(["\\\\?\\hid\\a".into()]);
        launch.on_device_list(["\\\\?\\hid\\a".into(), "\\\\?\\hid\\new".into()]);
        assert!(
            launch
                .take_serial_for_path("\\\\?\\hid\\new", "serial-new")
                .is_none()
        );
    }
}
