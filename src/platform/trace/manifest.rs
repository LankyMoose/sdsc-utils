//! Session manifest: the starting state a replay needs, plus append-only
//! state records discovered later (prefs, controllers, tabs, engine, windows).

use super::{Level, SCHEMA_VERSION, epoch_ms, next_seq, write_record};
use crate::platform::app_meta::{DISPLAY_NAME, PKG_NAME, PKG_VERSION};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::fs;
use std::io;
use std::path::Path;

pub(super) fn write_initial(dir: &Path, session: u64, level: Level) -> io::Result<()> {
    let mut env = Map::new();
    for (key, value) in std::env::vars() {
        if key.starts_with("SDSC_") || key == "CEF_PATH" {
            env.insert(key, Value::String(value));
        }
    }

    let manifest = json!({
        "v": SCHEMA_VERSION,
        "session": session,
        "started_ts": epoch_ms(),
        "level": level.as_str(),
        "app": {
            "display": DISPLAY_NAME,
            "package": PKG_NAME,
            "version": PKG_VERSION,
            "commit": option_env!("SDSC_GIT_COMMIT").unwrap_or("unknown"),
            "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            "features": {
                "cef_engine": cfg!(feature = "cef-engine"),
            },
        },
        "os": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "family": std::env::consts::FAMILY,
            "computer": std::env::var("COMPUTERNAME").unwrap_or_default(),
        },
        "cwd": std::env::current_dir().unwrap_or_default(),
        "env": env,
    });
    let text = serde_json::to_string_pretty(&manifest)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    fs::write(dir.join("manifest.json"), text)
}

/// Append one state record to `manifest.jsonl`.
pub fn record(kind: &'static str, value: impl Serialize) {
    let Some(controller) = super::CONTROLLER.get() else {
        return;
    };
    let mut map = Map::new();
    map.insert("v".into(), json!(SCHEMA_VERSION));
    map.insert("ts".into(), json!(epoch_ms()));
    map.insert("up_ms".into(), json!(controller.uptime_ms()));
    map.insert("seq".into(), json!(next_seq()));
    map.insert("kind".into(), json!(kind));
    map.insert(
        "value".into(),
        serde_json::to_value(value).unwrap_or(Value::Null),
    );
    write_record(controller, "manifest.jsonl", &map);
}
