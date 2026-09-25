//! Persisted remembered DualSense pads and nicknames (`controllers.json`).

use crate::controller::dualsense::identity::is_storable_serial;
use crate::controller::model::ControllerStatus;
use crate::platform::app_log;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownController {
    pub serial: String,
    pub product: String,
    pub connection: String,
    pub percent: u8,
    #[serde(default)]
    pub kind: crate::controller::model::ControllerKind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct KnownFile {
    #[serde(default)]
    controllers: Vec<KnownController>,
    #[serde(default)]
    nicknames: HashMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct KnownControllers {
    by_serial: HashMap<String, KnownController>,
    nicknames: HashMap<String, String>,
    dirty: bool,
}

impl KnownController {
    fn from_status(status: &ControllerStatus) -> Self {
        Self {
            serial: status.serial.clone(),
            product: status.product.to_string(),
            connection: status.connection.to_string(),
            percent: status.percent,
            kind: status.kind,
        }
    }

    fn update_from_status(&mut self, status: &ControllerStatus) -> bool {
        let product = status.product.to_string();
        let connection = status.connection.to_string();
        let changed = self.product != product
            || self.connection != connection
            || self.percent != status.percent
            || self.kind != status.kind;
        if changed {
            self.product = product;
            self.connection = connection;
            self.percent = status.percent;
            self.kind = status.kind;
        }
        changed
    }
}

impl KnownControllers {
    pub fn load() -> Self {
        let path = store_path();
        let Ok(bytes) = fs::read(&path) else {
            return Self::default();
        };
        match serde_json::from_slice::<KnownFile>(&bytes) {
            Ok(file) => Self::from_file(file),
            Err(err) => {
                app_log::warn(format!(
                    "failed to parse controllers at {}: {err}; starting empty",
                    path.display()
                ));
                Self::default()
            }
        }
    }

    fn from_file(file: KnownFile) -> Self {
        let mut by_serial = HashMap::new();
        for record in file.controllers {
            if is_storable_serial(&record.serial) {
                by_serial.insert(record.serial.clone(), record);
            }
        }
        let mut nicknames = HashMap::new();
        for (serial, name) in file.nicknames {
            if !is_storable_serial(&serial) {
                continue;
            }
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                nicknames.insert(serial, trimmed.to_string());
            }
        }
        Self {
            by_serial,
            nicknames,
            dirty: false,
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
            app_log::warn(format!("failed to create controllers dir: {err}"));
            return;
        }

        let mut controllers: Vec<_> = self.by_serial.values().cloned().collect();
        controllers.sort_by(|a, b| a.serial.cmp(&b.serial));
        let file = KnownFile {
            controllers,
            nicknames: self.nicknames.clone(),
        };

        match serde_json::to_vec_pretty(&file) {
            Ok(bytes) => {
                if let Err(err) = fs::write(&path, bytes) {
                    app_log::warn(format!("failed to write controllers: {err}"));
                    return;
                }
                self.dirty = false;
            }
            Err(err) => app_log::warn(format!("failed to serialize controllers: {err}")),
        }
    }

    pub fn is_remembered(&self, serial: &str) -> bool {
        self.by_serial.contains_key(serial)
    }

    pub fn nickname(&self, serial: &str) -> Option<&str> {
        self.nicknames.get(serial).map(String::as_str)
    }

    /// Set or clear a nickname. Empty / whitespace clears. Returns true if stored data changed.
    pub fn set_nickname(&mut self, serial: &str, nickname: Option<String>) -> bool {
        if !is_storable_serial(serial) {
            return false;
        }
        let next = nickname
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let changed = match (&next, self.nicknames.get(serial)) {
            (Some(name), Some(existing)) => name != existing,
            (Some(_), None) | (None, Some(_)) => true,
            (None, None) => false,
        };
        if !changed {
            return false;
        }
        match next {
            Some(name) => {
                self.nicknames.insert(serial.to_string(), name);
            }
            None => {
                self.nicknames.remove(serial);
            }
        }
        self.dirty = true;
        true
    }

    pub fn remember(&mut self, controller: &ControllerStatus) -> bool {
        if !is_storable_serial(&controller.serial) {
            return false;
        }
        match self.by_serial.get_mut(&controller.serial) {
            Some(record) => {
                let changed = record.update_from_status(controller);
                if changed {
                    self.dirty = true;
                }
                changed
            }
            None => {
                self.by_serial.insert(
                    controller.serial.clone(),
                    KnownController::from_status(controller),
                );
                self.dirty = true;
                true
            }
        }
    }

    pub fn forget(&mut self, serial: &str) -> bool {
        if self.by_serial.remove(serial).is_some() {
            self.dirty = true;
            true
        } else {
            false
        }
    }

    /// Refresh last-known fields for remembered pads that are currently connected.
    pub fn sync_from_live(&mut self, live: &[ControllerStatus]) -> bool {
        let mut changed = false;
        for controller in live {
            if !is_storable_serial(&controller.serial) {
                continue;
            }
            if let Some(record) = self.by_serial.get_mut(&controller.serial)
                && record.update_from_status(controller)
            {
                changed = true;
            }
        }
        if changed {
            self.dirty = true;
        }
        changed
    }

    /// Remembered pads not in the live HID list.
    pub fn remembered_disconnected<'a>(
        &'a self,
        live: &'a [ControllerStatus],
    ) -> Vec<&'a KnownController> {
        let live_serials: std::collections::HashSet<&str> =
            live.iter().map(|c| c.serial.as_str()).collect();
        let mut out: Vec<_> = self
            .by_serial
            .values()
            .filter(|r| !live_serials.contains(r.serial.as_str()))
            .collect();
        out.sort_by(|a, b| a.serial.cmp(&b.serial));
        out
    }
}

fn store_path() -> PathBuf {
    crate::persist::paths::data_dir().join("controllers.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::dualsense::battery::dualsense_status;
    use crate::controller::model::{Connection, ControllerStatus, PowerState};

    fn pad(serial: &str, percent: u8, connection: Connection) -> ControllerStatus {
        dualsense_status(
            1,
            "DualSense",
            connection,
            serial.to_string(),
            percent,
            PowerState::Discharging,
        )
    }

    #[test]
    fn remember_and_forget() {
        let mut store = KnownControllers::default();
        let controller = pad("abc123", 50, Connection::Bluetooth);

        assert!(store.remember(&controller));
        assert!(store.is_remembered("abc123"));
        assert!(store.forget("abc123"));
        assert!(!store.is_remembered("abc123"));
    }

    #[test]
    fn skips_unknown_and_empty_serials() {
        let mut store = KnownControllers::default();
        assert!(!store.remember(&pad("unknown", 50, Connection::Usb)));
        assert!(!store.remember(&pad("", 50, Connection::Usb)));
        assert!(!store.is_remembered("unknown"));
        assert!(!store.is_remembered(""));
    }

    #[test]
    fn sync_from_live_updates_only_remembered() {
        let mut store = KnownControllers::default();
        store.remember(&pad("remembered", 50, Connection::Usb));
        store.dirty = false;

        let live = vec![
            pad("remembered", 75, Connection::Bluetooth),
            pad("other", 30, Connection::Usb),
        ];
        assert!(store.sync_from_live(&live));
        assert_eq!(store.by_serial["remembered"].percent, 75);
        assert_eq!(store.by_serial["remembered"].connection, "Bluetooth");
        assert!(!store.is_remembered("other"));
    }

    #[test]
    fn remembered_disconnected_excludes_live() {
        let mut store = KnownControllers::default();
        store.remember(&pad("live", 50, Connection::Usb));
        store.remember(&pad("gone", 25, Connection::Bluetooth));

        let live = vec![pad("live", 60, Connection::Usb)];
        let disconnected = store.remembered_disconnected(&live);
        assert_eq!(disconnected.len(), 1);
        assert_eq!(disconnected[0].serial, "gone");
    }

    #[test]
    fn nickname_set_and_clear() {
        let mut store = KnownControllers::default();
        assert!(store.set_nickname("abc123", Some("  Left pad  ".into())));
        assert_eq!(store.nickname("abc123"), Some("Left pad"));
        assert!(!store.set_nickname("abc123", Some("Left pad".into())));
        assert!(store.set_nickname("abc123", Some("   ".into())));
        assert_eq!(store.nickname("abc123"), None);
    }

    #[test]
    fn nickname_survives_forget() {
        let mut store = KnownControllers::default();
        store.remember(&pad("abc123", 50, Connection::Usb));
        store.set_nickname("abc123", Some("Couch".into()));
        assert!(store.forget("abc123"));
        assert!(!store.is_remembered("abc123"));
        assert_eq!(store.nickname("abc123"), Some("Couch"));
    }

    #[test]
    fn nickname_rejects_unknown_serials() {
        let mut store = KnownControllers::default();
        assert!(!store.set_nickname("unknown", Some("Nope".into())));
        assert!(!store.set_nickname("", Some("Nope".into())));
        assert_eq!(store.nickname("unknown"), None);
    }

    #[test]
    fn older_controllers_file_without_nicknames_loads() {
        let file: KnownFile = serde_json::from_str(
            r#"{"controllers":[{"serial":"abc","product":"DualSense","connection":"USB","percent":40}]}"#,
        )
        .unwrap();
        let store = KnownControllers::from_file(file);
        assert!(store.is_remembered("abc"));
        assert!(store.nicknames.is_empty());
    }
}
