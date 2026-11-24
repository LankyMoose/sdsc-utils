//! Replay launcher: reads a recorded flow, prepares an isolated data
//! directory, and exposes the semantic steps an in-process replay driver
//! dispatches through the real browser handlers.

use super::session_dirs;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const REPLAY_FLAG: &str = "--trace-replay";
const FLOW_FLAGS: [&str; 2] = ["--flow", "--trace-flow"];

static REQUEST: OnceLock<ReplayRequest> = OnceLock::new();
static REPLAYING: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone)]
pub struct ReplayRequest {
    pub session: PathBuf,
    pub flow: u64,
    pub sandbox: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ReplayStep {
    pub app: String,
    pub action: String,
    pub params: Value,
    pub requires: Vec<Value>,
    pub pre: Value,
}

#[derive(Debug, Clone)]
pub struct ReplayPlan {
    steps: Vec<ReplayStep>,
    index: usize,
    flow: u64,
}

impl ReplayPlan {
    pub fn flow(&self) -> u64 {
        self.flow
    }

    pub fn remaining(&self) -> usize {
        self.steps.len().saturating_sub(self.index)
    }

    /// App targeted by the next step, if any (used to avoid opening the
    /// Browser when a replay flow belongs to another app).
    pub fn peek_app(&self) -> Option<&str> {
        self.steps.get(self.index).map(|step| step.app.as_str())
    }

    pub fn next_step(&mut self) -> Option<ReplayStep> {
        let step = self.steps.get(self.index).cloned();
        if step.is_some() {
            self.index += 1;
        }
        step
    }
}

pub fn request() -> Option<&'static ReplayRequest> {
    REQUEST.get()
}

pub fn replaying() -> bool {
    REPLAYING.load(Ordering::Relaxed)
}

pub fn set_replaying(active: bool) {
    REPLAYING.store(active, Ordering::Relaxed);
}

/// Parse replay flags, resolve the recorded session, create the sandbox, and
/// export `SDSC_DATA_DIR`/`SDSC_REPLAY_*` before the logger or app boots.
pub fn prepare(args: &[String]) -> Option<ReplayRequest> {
    let flag = args
        .iter()
        .position(|arg| arg == REPLAY_FLAG || arg.starts_with("--trace-replay="))?;
    let session_arg = if let Some(value) = args[flag].strip_prefix("--trace-replay=") {
        value.to_string()
    } else {
        args.get(flag + 1)?.clone()
    };
    let flow = args
        .iter()
        .position(|arg| FLOW_FLAGS.iter().any(|f| arg == f))
        .and_then(|index| args.get(index + 1).map(String::as_str))
        .or_else(|| {
            args.iter().find_map(|arg| {
                ["--flow=", "--trace-flow="]
                    .iter()
                    .find_map(|prefix| arg.strip_prefix(prefix))
            })
        })
        .and_then(|value| value.parse::<u64>().ok())?;
    let session = select_session(&session_arg)?;

    let sandbox = std::env::temp_dir().join("sdsc-replay").join(format!(
        "{}-{flow}-{}",
        session
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("session"),
        epoch_ms()
    ));
    if let Err(err) = fs::create_dir_all(&sandbox) {
        crate::platform::app_log::warn(format!(
            "replay: cannot create sandbox {}: {err}",
            sandbox.display()
        ));
        return None;
    }
    seed_sandbox(&session, flow, &sandbox);

    // Before any threads start; the process environment is the supported
    // sandbox seam for the logger, prefs, tabs, and trace root.
    unsafe {
        std::env::set_var("SDSC_DATA_DIR", &sandbox);
        std::env::set_var("SDSC_REPLAY_SESSION", &session);
        std::env::set_var("SDSC_REPLAY_FLOW", flow.to_string());
        std::env::set_var("SDSC_DEBUG_QUIET", "1");
        std::env::set_var("SDSC_DEBUG_INPUT", "0");
    }
    let request = ReplayRequest {
        session,
        flow,
        sandbox,
    };
    let _ = REQUEST.set(request.clone());
    Some(request)
}

/// Drop replay-only flags before the normal argument parser runs.
pub fn strip_args(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == REPLAY_FLAG {
            index += 2;
            continue;
        }
        if arg.starts_with("--trace-replay=") {
            index += 1;
            continue;
        }
        if FLOW_FLAGS.iter().any(|flag| arg == flag) {
            index += 2;
            continue;
        }
        if ["--flow=", "--trace-flow="]
            .iter()
            .any(|prefix| arg.starts_with(prefix))
        {
            index += 1;
            continue;
        }
        out.push(arg.clone());
        index += 1;
    }
    out
}

/// Validate recorded `requires` against the live state snapshot.
pub fn check_requires(requires: &[Value], state: &Value) -> Vec<Value> {
    requires
        .iter()
        .map(|requirement| {
            let path = requirement
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let expected = requirement.get("value").unwrap_or(&Value::Null);
            let actual = lookup_path(state, path);
            serde_json::json!({
                "path": path,
                "expected": expected,
                "actual": actual.unwrap_or(&Value::Null),
                "pass": actual == Some(expected),
            })
        })
        .collect()
}

fn lookup_path<'a>(mut value: &'a Value, path: &str) -> Option<&'a Value> {
    for segment in path.split('.') {
        let name_end = segment.find('[').unwrap_or(segment.len());
        let name = &segment[..name_end];
        if !name.is_empty() {
            value = value.get(name)?;
        }
        let mut rest = &segment[name_end..];
        while let Some(stripped) = rest.strip_prefix('[') {
            let end = stripped.find(']')?;
            let index: usize = stripped[..end].parse().ok()?;
            value = value.get(index)?;
            rest = &stripped[end + 1..];
        }
    }
    Some(value)
}

pub fn plan(request: &ReplayRequest) -> Option<ReplayPlan> {
    let actions = read_actions(&request.session);
    let mut steps = Vec::new();
    for record in actions {
        if record.get("kind").and_then(Value::as_str) != Some("action")
            || record.get("flow").and_then(Value::as_u64) != Some(request.flow)
        {
            continue;
        }
        let Some(repro) = record.get("repro").and_then(Value::as_object) else {
            continue;
        };
        let Some(app) = repro.get("app").and_then(Value::as_str) else {
            continue;
        };
        let Some(action) = repro.get("action").and_then(Value::as_str) else {
            continue;
        };
        steps.push(ReplayStep {
            app: app.to_string(),
            action: action.to_string(),
            params: repro.get("params").cloned().unwrap_or(Value::Null),
            requires: record
                .get("requires")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            pre: record.get("pre").cloned().unwrap_or(Value::Null),
        });
    }
    if steps.is_empty() {
        crate::platform::app_log::warn(format!(
            "replay: no replayable steps for flow {} in {}",
            request.flow,
            request.session.display()
        ));
        return None;
    }
    Some(ReplayPlan {
        steps,
        index: 0,
        flow: request.flow,
    })
}

fn select_session(arg: &str) -> Option<PathBuf> {
    let path = Path::new(arg);
    if path.is_dir() {
        return Some(path.to_path_buf());
    }
    let id = arg.strip_prefix("session-").unwrap_or(arg);
    for dir in session_dirs().iter().rev() {
        let name = dir.file_name()?.to_str()?;
        let Some(session) = name.strip_prefix("session-") else {
            continue;
        };
        if session == id || session.starts_with(id) {
            return Some(dir.clone());
        }
    }
    None
}

fn read_actions(session: &Path) -> Vec<Value> {
    let Ok(read) = fs::read_dir(session) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = read
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("actions"))
        })
        .collect();
    files.sort();
    let mut records = Vec::new();
    for file in files {
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        for line in text.lines() {
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                records.push(value);
            }
        }
    }
    records.sort_by_key(|record| {
        (
            record.get("seq").and_then(Value::as_u64).unwrap_or(0),
            record.get("ts").and_then(Value::as_u64).unwrap_or(0),
        )
    });
    records
}

fn seed_sandbox(session: &Path, flow: u64, sandbox: &Path) {
    if let Some(prefs) = manifest_value(session, "prefs")
        && let Ok(text) = serde_json::to_string_pretty(&prefs)
    {
        let _ = fs::write(sandbox.join("prefs.json"), text);
    }
    if let Some(tabs) = manifest_value(session, "browser_tabs")
        && let Ok(text) = serde_json::to_string_pretty(&tabs)
    {
        let _ = fs::write(sandbox.join("tabs.json"), text);
    }
    let meta = serde_json::json!({
        "session": session,
        "flow": flow,
        "sandbox": sandbox,
    });
    if let Ok(text) = serde_json::to_string_pretty(&meta) {
        let _ = fs::write(sandbox.join("replay.json"), text);
    }
}

fn manifest_value(session: &Path, kind: &str) -> Option<Value> {
    let text = fs::read_to_string(session.join("manifest.jsonl")).ok()?;
    let mut found = None;
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if record.get("kind").and_then(Value::as_str) == Some(kind) {
            found = record.get("value").cloned();
        }
    }
    found
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lookup_path_handles_dots_and_indices() {
        let state = json!({
            "ui": {
                "nav": {
                    "current": 2,
                    "entries": [
                        { "url": "https://a.test/" },
                        { "url": "https://b.test/" }
                    ]
                }
            }
        });
        assert_eq!(lookup_path(&state, "ui.nav.current"), Some(&json!(2)));
        assert_eq!(
            lookup_path(&state, "ui.nav.entries[1].url"),
            Some(&json!("https://b.test/"))
        );
        assert_eq!(lookup_path(&state, "ui.nav.missing"), None);
    }

    #[test]
    fn check_requires_reports_pass_and_mismatch() {
        let state = json!({ "ui": { "nav": { "current": 2 } } });
        let checks = check_requires(
            &[
                json!({ "path": "ui.nav.current", "value": 2 }),
                json!({ "path": "ui.nav.current", "value": 3 }),
            ],
            &state,
        );
        assert_eq!(checks[0]["pass"], true);
        assert_eq!(checks[1]["pass"], false);
        assert_eq!(checks[1]["actual"], 2);
    }
}
