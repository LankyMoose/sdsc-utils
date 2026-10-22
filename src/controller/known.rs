//! Persisted remembered DualSense pads and nicknames (`controllers.json`).

use crate::controller::dualsense::identity::is_storable_serial;
use crate::controller::model::{ControllerStatus, Serial};
use crate::persist::json;
use serde::{Deserialize, Serialize};
use serde_json::Map;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// New persisted fields need `#[serde(default)]` (or the oldest-shape fixture fails).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownController {
    pub serial: Serial,
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
    nicknames: HashMap<Serial, String>,
}

#[derive(Debug, Clone, Default)]
pub struct KnownControllers {
    by_serial: HashMap<Serial, KnownController>,
    nicknames: HashMap<Serial, String>,
    dirty: bool,
}

impl KnownController {
    fn from_status(status: &ControllerStatus) -> Self {
        Self {
            serial: status.serial.clone(),
            product: status.product.clone(),
            connection: status.connection.to_string(),
            percent: status.percent,
            kind: status.kind,
        }
    }

    fn update_from_status(&mut self, status: &ControllerStatus) -> bool {
        let product = status.product.clone();
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
        let Some(root) = crate::persist::store::load_object(&path) else {
            return Self::default();
        };
        Self::from_root(&path, root)
    }

    fn from_root(path: &Path, mut root: Map<String, serde_json::Value>) -> Self {
        let items = json::take_array(&mut root, "controllers");
        let source_len = items.len();
        let (controllers, skipped) = json::decode_array::<KnownController>(items);
        if json::total_loss(path, "controllers", source_len, controllers.len()) {
            return Self::default();
        }
        json::warn_skipped(path, "controllers", skipped);
        let nicknames: HashMap<Serial, String> = json::take_default(&mut root, "nicknames");
        Self::from_file(KnownFile {
            controllers,
            nicknames,
        })
    }

    fn from_file(file: KnownFile) -> Self {
        let mut by_serial = HashMap::new();
        for record in file.controllers {
            if record.serial.is_storable() {
                by_serial.insert(record.serial.clone(), record);
            }
        }
        let mut nicknames = HashMap::new();
        for (serial, name) in file.nicknames {
            if !serial.is_storable() {
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
        let mut controllers: Vec<_> = self.by_serial.values().cloned().collect();
        controllers.sort_by(|a, b| a.serial.cmp(&b.serial));
        let file = KnownFile {
            controllers,
            nicknames: self.nicknames.clone(),
        };

        if crate::persist::store::save_pretty("controllers", &path, &file) {
            self.dirty = false;
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
                self.nicknames.insert(Serial::from(serial), name);
            }
            None => {
                self.nicknames.remove(serial);
            }
        }
        self.dirty = true;
        true
    }

    pub fn remember(&mut self, controller: &ControllerStatus) -> bool {
        if !controller.serial.is_storable() {
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
            if !controller.serial.is_storable() {
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
            live.iter().map(|c| c.serial.as_ref()).collect();
        let mut out: Vec<_> = self
            .by_serial
            .values()
            .filter(|r| !live_serials.contains(r.serial.as_ref()))
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

    /// The bug this guards: an emulated pad added in one run was remembered
    /// (so it lingered in the popup's controller list) but was never fed back
    /// into the Controllers-tab fleet, leaving it listed nowhere it could be
    /// edited. Rehydration reads the remembered store at boot.
    #[test]
    #[cfg(debug_assertions)]
    fn emulated_pads_survive_into_the_fleet() {
        let mut store = KnownControllers::default();
        store.remember(&pad("emu-1", 40, Connection::Bluetooth));
        store.remember(&pad("REAL-xyz", 70, Connection::Usb));

        // `remembered_disconnected` is the boot-time view (no live list yet).
        let remembered: Vec<(Serial, u8, Connection)> = store
            .remembered_disconnected(&[])
            .into_iter()
            .map(|r| {
                (
                    r.serial.clone(),
                    r.percent,
                    Connection::from_label(&r.connection).expect("persisted link parses"),
                )
            })
            .collect();

        let fleet = crate::controller::emulate::rehydrate(&remembered);
        let serials: Vec<&str> = fleet.iter().map(|p| p.serial().as_ref()).collect();
        assert_eq!(
            serials,
            vec!["emu-1"],
            "real hardware is not an emulated pad"
        );
        assert_eq!(fleet[0].status.percent, 40);
        assert!(
            !fleet[0].connected,
            "a remembered pad is not a plugged-in one"
        );
        assert!(
            fleet[0].remembered,
            "anything read back from the store is remembered by construction"
        );
    }

    /// Forgetting a pad must *remove* it from the store, not merely stop it
    /// being updated — otherwise it would still reappear after a restart.
    #[test]
    #[cfg(debug_assertions)]
    fn forgetting_a_pad_keeps_it_out_of_the_next_boot() {
        let mut store = KnownControllers::default();
        store.remember(&pad("emu-1", 40, Connection::Bluetooth));
        assert!(store.is_remembered("emu-1"));

        assert!(store.forget("emu-1"));
        assert!(!store.is_remembered("emu-1"));

        let remembered: Vec<(Serial, u8, Connection)> = store
            .remembered_disconnected(&[])
            .into_iter()
            .map(|r| {
                (
                    r.serial.clone(),
                    r.percent,
                    Connection::from_label(&r.connection).expect("parses"),
                )
            })
            .collect();
        assert!(
            crate::controller::emulate::rehydrate(&remembered).is_empty(),
            "a forgotten pad must not come back after a restart"
        );
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
        assert_eq!(disconnected[0].serial.as_ref(), "gone");
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

    #[test]
    fn older_controllers_shape_loads_and_skips_bad_record() {
        let root = serde_json::from_str(
            r#"{
                "controllers": [
                    {"serial":"abc","product":"DualSense","connection":"USB","percent":40},
                    {"serial":"def","product":"DualSense","connection":"USB","percent":"nope"}
                ]
            }"#,
        )
        .unwrap();
        let serde_json::Value::Object(root) = root else {
            panic!("expected object");
        };
        let store = KnownControllers::from_root(Path::new("controllers.json"), root);
        assert!(store.is_remembered("abc"));
        assert_eq!(store.by_serial["abc"].percent, 40);
        assert_eq!(
            store.by_serial["abc"].kind,
            crate::controller::model::ControllerKind::DualSense
        );
        assert!(!store.is_remembered("def"));
        assert!(store.nicknames.is_empty());
    }

    /// The `Serial` newtype must not change `controllers.json`: serials stay
    /// plain strings both as record fields and as nickname-map keys.
    #[test]
    fn known_file_round_trip_preserves_schema() {
        let mut store = KnownControllers::default();
        store.remember(&pad("abc", 40, Connection::Usb));
        store.set_nickname("abc", Some("Couch".into()));
        let mut controllers: Vec<_> = store.by_serial.values().cloned().collect();
        controllers.sort_by(|a, b| a.serial.cmp(&b.serial));
        let file = KnownFile {
            controllers,
            nicknames: store.nicknames.clone(),
        };
        let json = serde_json::to_string(&file).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["controllers"][0]["serial"], "abc");
        assert_eq!(value["nicknames"]["abc"], "Couch");

        // Byte-shape check against the pre-`Serial` layout.
        let expected: serde_json::Value = serde_json::from_str(
            r#"{"controllers":[{"serial":"abc","product":"DualSense","connection":"USB","percent":40,"kind":"dual_sense"}],"nicknames":{"abc":"Couch"}}"#,
        )
        .unwrap();
        assert_eq!(value, expected);

        let back: KnownFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, file);
        let reloaded = KnownControllers::from_file(back);
        assert!(reloaded.is_remembered("abc"));
        assert_eq!(reloaded.nickname("abc"), Some("Couch"));
    }
}
