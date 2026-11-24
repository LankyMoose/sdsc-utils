//! Read-only trace queries. These run before the single-instance guard and
//! never start a session.

use super::{latest_session_dir, session_dirs};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    List,
    Actions,
    Follow,
    Find,
    Audit,
    Repro,
    Path,
}

#[derive(Default)]
struct Args {
    command: Option<Command>,
    session: Option<String>,
    flow: Option<u64>,
    app: Option<String>,
    find: Option<String>,
    failures: bool,
    json: bool,
}

pub fn run(args: &[String]) -> ExitCode {
    let Some(args) = parse(args) else {
        eprintln!("error: invalid trace arguments");
        return ExitCode::FAILURE;
    };
    let Some(command) = args.command else {
        eprintln!("error: no --trace-* command");
        return ExitCode::FAILURE;
    };
    if command == Command::List {
        return list();
    }
    let Some(dir) = select_session(args.session.as_deref()) else {
        eprintln!("error: no trace session found");
        return ExitCode::FAILURE;
    };
    match command {
        Command::List => list(),
        Command::Actions => actions(&dir, &args),
        Command::Follow => follow(&dir, args.flow, args.json),
        Command::Find => find(&dir, args.find.as_deref().unwrap_or(""), args.json),
        Command::Audit => audit(&dir),
        Command::Repro => repro(&dir, args.flow, args.json),
        Command::Path => {
            println!("{}", dir.display());
            ExitCode::SUCCESS
        }
    }
}

fn parse(args: &[String]) -> Option<Args> {
    let mut out = Args::default();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if let Some(value) = arg.strip_prefix("--trace-follow=") {
            out.flow = value.parse().ok();
            out.command = Some(Command::Follow);
        } else if let Some(value) = arg.strip_prefix("--trace-repro=") {
            out.flow = value.parse().ok();
            out.command = Some(Command::Repro);
        } else if let Some(value) = arg.strip_prefix("--trace-find=") {
            out.find = Some(value.to_string());
            out.command = Some(Command::Find);
        } else if let Some(value) = arg
            .strip_prefix("--trace-session=")
            .or_else(|| arg.strip_prefix("--session="))
        {
            out.session = Some(value.to_string());
        } else if let Some(value) = arg
            .strip_prefix("--trace-app=")
            .or_else(|| arg.strip_prefix("--app="))
        {
            out.app = Some(value.to_string());
        } else {
            match arg.as_str() {
                "--trace-list" => out.command = Some(Command::List),
                "--trace-actions" | "--trace-last" => out.command = Some(Command::Actions),
                "--trace-follow" => {
                    index += 1;
                    out.flow = args.get(index).and_then(|v| v.parse().ok());
                    out.command = Some(Command::Follow);
                }
                "--trace-find" => {
                    index += 1;
                    out.find = args.get(index).cloned();
                    out.command = Some(Command::Find);
                }
                "--trace-repro" => {
                    index += 1;
                    out.flow = args.get(index).and_then(|v| v.parse().ok());
                    out.command = Some(Command::Repro);
                }
                "--flow" | "--trace-flow" => {
                    index += 1;
                    out.flow = args.get(index).and_then(|v| v.parse().ok());
                }
                "--trace-failures" => {
                    out.failures = true;
                    out.command = Some(Command::Actions);
                }
                "--trace-audit" => out.command = Some(Command::Audit),
                "--trace-path" => out.command = Some(Command::Path),
                "--session" | "--trace-session" => {
                    index += 1;
                    out.session = args.get(index).cloned();
                }
                "--app" | "--trace-app" => {
                    index += 1;
                    out.app = args.get(index).cloned();
                }
                "--json" => out.json = true,
                _ => {}
            }
        }
        index += 1;
    }
    if matches!(out.command, Some(Command::Follow | Command::Repro)) && out.flow.is_none() {
        return None;
    }
    if matches!(out.command, Some(Command::Find)) && out.find.as_deref().unwrap_or("").is_empty() {
        return None;
    }
    Some(out)
}

fn select_session(arg: Option<&str>) -> Option<PathBuf> {
    let Some(arg) = arg else {
        return latest_session_dir();
    };
    let arg = arg.trim();
    if arg.is_empty() {
        return latest_session_dir();
    }
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

fn lane_files(dir: &Path, actions_only: bool) -> Vec<PathBuf> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = read
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .filter(|path| {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if name.starts_with("manifest") {
                return false;
            }
            !actions_only || name.starts_with("actions")
        })
        .collect();
    files.sort();
    files
}

fn read_records(dir: &Path, actions_only: bool) -> Vec<Value> {
    let mut records = Vec::new();
    for file in lane_files(dir, actions_only) {
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
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

fn list() -> ExitCode {
    let dirs = session_dirs();
    if dirs.is_empty() {
        println!("No trace sessions.");
        return ExitCode::SUCCESS;
    }
    for dir in dirs.iter().rev() {
        let manifest = fs::read_to_string(dir.join("manifest.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        let version = manifest
            .as_ref()
            .and_then(|m| m.pointer("/app/version"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        let commit = manifest
            .as_ref()
            .and_then(|m| m.pointer("/app/commit"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        let started = manifest
            .as_ref()
            .and_then(|m| m.get("started_ts"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let records = read_records(dir, true);
        let actions = records
            .iter()
            .filter(|r| r.get("kind").and_then(Value::as_str) == Some("action"))
            .count();
        let outcomes = records
            .iter()
            .filter(|r| r.get("kind").and_then(Value::as_str) == Some("outcome"))
            .count();
        println!(
            "{}  version={version} commit={commit} started={started} actions={actions} outcomes={outcomes}",
            dir.file_name().and_then(|n| n.to_str()).unwrap_or_default()
        );
    }
    ExitCode::SUCCESS
}

fn actions(dir: &Path, args: &Args) -> ExitCode {
    let records = read_records(dir, true);
    if records.is_empty() {
        println!("No actions in {}.", dir.display());
        return ExitCode::SUCCESS;
    }
    let actions_by_id: HashMap<u64, Value> = records
        .iter()
        .filter(|r| r.get("kind").and_then(Value::as_str) == Some("action"))
        .filter_map(|r| {
            r.get("action_id")
                .and_then(Value::as_u64)
                .map(|id| (id, r.clone()))
        })
        .collect();
    let mut shown = 0usize;
    for record in &records {
        let kind = record.get("kind").and_then(Value::as_str).unwrap_or("");
        let action_id = record.get("action_id").and_then(Value::as_u64);
        let action_record = action_id.and_then(|id| actions_by_id.get(&id));
        let app = action_record
            .and_then(|r| r.get("app"))
            .and_then(Value::as_str);
        if let Some(want) = &args.app
            && app != Some(want.as_str())
        {
            continue;
        }
        if let Some(want) = args.flow
            && record.get("flow").and_then(Value::as_u64) != Some(want)
        {
            continue;
        }
        if args.failures && kind != "outcome" {
            continue;
        }
        if args.failures {
            let outcome = record.get("outcome").and_then(Value::as_str).unwrap_or("");
            if !is_failure(outcome) {
                continue;
            }
        }
        if args.json {
            println!(
                "{}",
                serde_json::to_string(record).unwrap_or_else(|_| "{}".into())
            );
            shown += 1;
            continue;
        }
        match kind {
            "action" => {
                let action = record.get("action").and_then(Value::as_str).unwrap_or("?");
                let flow = record.get("flow").and_then(Value::as_u64).unwrap_or(0);
                println!(
                    "{} flow={flow} begin {}{} {}",
                    stamp(record),
                    app.map(|a| format!("{a}:")).unwrap_or_default(),
                    action,
                    compact(record.get("params"))
                );
                shown += 1;
            }
            "outcome" => {
                let outcome = record.get("outcome").and_then(Value::as_str).unwrap_or("?");
                let action = record.get("action").and_then(Value::as_str).unwrap_or("?");
                let flow = record.get("flow").and_then(Value::as_u64).unwrap_or(0);
                let duration = record
                    .get("duration_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                println!(
                    "{} flow={flow}   -> {outcome} {action} {duration}ms {}{}",
                    stamp(record),
                    compact(record.get("checks")),
                    compact(record.get("resolution"))
                );
                shown += 1;
            }
            _ => {}
        }
    }
    if shown == 0 {
        println!("No matching actions.");
    }
    ExitCode::SUCCESS
}

fn follow(dir: &Path, flow: Option<u64>, json: bool) -> ExitCode {
    let Some(flow) = flow else {
        eprintln!("error: --trace-follow requires a flow id");
        return ExitCode::FAILURE;
    };
    let records = read_records(dir, false);
    let matching: Vec<&Value> = records
        .iter()
        .filter(|record| {
            record.get("flow").and_then(Value::as_u64) == Some(flow)
                || record.get("parent").and_then(Value::as_u64) == Some(flow)
        })
        .collect();
    if matching.is_empty() {
        println!("No records for flow {flow} in {}.", dir.display());
        return ExitCode::SUCCESS;
    }
    for record in matching {
        if json {
            println!(
                "{}",
                serde_json::to_string(record).unwrap_or_else(|_| "{}".into())
            );
            continue;
        }
        let lane = record.get("lane").and_then(Value::as_str).unwrap_or("?");
        let kind = record
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("event");
        let name = record
            .get("action")
            .or_else(|| record.get("event"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        let outcome = record
            .get("outcome")
            .and_then(Value::as_str)
            .map(|v| format!(" -> {v}"))
            .unwrap_or_default();
        let mut extras = Vec::new();
        for (key, value) in record.as_object().into_iter().flatten() {
            if matches!(
                key.as_str(),
                "v" | "ts"
                    | "up_ms"
                    | "seq"
                    | "kind"
                    | "action_id"
                    | "flow"
                    | "parent"
                    | "lane"
                    | "action"
                    | "event"
                    | "outcome"
                    | "duration_ms"
            ) {
                continue;
            }
            extras.push(format!("{key}={}", compact(Some(value))));
        }
        println!(
            "{} [{lane}] {kind} {name}{outcome} {}",
            stamp(record),
            extras.join(" ")
        );
    }
    ExitCode::SUCCESS
}

fn find(dir: &Path, needle: &str, json: bool) -> ExitCode {
    let needle = needle.to_lowercase();
    let records = read_records(dir, true);
    let mut seen = HashSet::new();
    let mut shown = 0usize;
    for record in records {
        let text = serde_json::to_string(&record)
            .unwrap_or_default()
            .to_lowercase();
        if !text.contains(&needle) {
            continue;
        }
        let action_id = record.get("action_id").and_then(Value::as_u64).unwrap_or(0);
        if !seen.insert(action_id) {
            continue;
        }
        if json {
            println!(
                "{}",
                serde_json::to_string(&record).unwrap_or_else(|_| "{}".into())
            );
        } else {
            let flow = record.get("flow").and_then(Value::as_u64).unwrap_or(0);
            let action = record.get("action").and_then(Value::as_str).unwrap_or("?");
            println!(
                "{} flow={flow} {action} {}",
                stamp(&record),
                compact(record.get("params"))
            );
        }
        shown += 1;
        if shown >= 50 {
            break;
        }
    }
    if shown == 0 {
        println!("No actions matching {needle:?}.");
    } else {
        println!("Use: sdsc-utils --trace-follow <flow>");
    }
    ExitCode::SUCCESS
}

fn repro(dir: &Path, flow: Option<u64>, json_mode: bool) -> ExitCode {
    let Some(flow) = flow else {
        eprintln!("error: --trace-repro requires a flow id");
        return ExitCode::FAILURE;
    };
    let records = read_records(dir, true);
    let mut steps = Vec::new();
    for record in records {
        if record.get("kind").and_then(Value::as_str) != Some("action")
            || record.get("flow").and_then(Value::as_u64) != Some(flow)
        {
            continue;
        }
        let Some(repro) = record.get("repro") else {
            continue;
        };
        let pre = record.get("pre");
        let requires = record
            .get("requires")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut checks = Vec::new();
        for requirement in &requires {
            let path = requirement
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let expected = requirement.get("value").unwrap_or(&Value::Null);
            let actual = pre.and_then(|pre| resolve_path(pre, path));
            checks.push(serde_json::json!({
                "path": path,
                "expected": expected,
                "actual": actual.unwrap_or(&Value::Null),
                "pass": actual == Some(expected),
            }));
        }
        steps.push(serde_json::json!({
            "action_id": record.get("action_id"),
            "action": record.get("action"),
            "repro": repro,
            "requires": checks,
            "pre": pre,
        }));
    }
    if steps.is_empty() {
        println!(
            "No replayable actions for flow {flow} in {}.",
            dir.display()
        );
        return ExitCode::SUCCESS;
    }
    if json_mode {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "flow": flow,
                "steps": steps,
            }))
            .unwrap_or_else(|_| "{}".into())
        );
        return ExitCode::SUCCESS;
    }
    println!("flow={flow} steps={}", steps.len());
    let mut failed = 0usize;
    for (index, step) in steps.iter().enumerate() {
        let action = step.get("action").and_then(Value::as_str).unwrap_or("?");
        let repro = step.get("repro").cloned().unwrap_or(Value::Null);
        let checks = step
            .get("requires")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let ok = checks
            .iter()
            .filter(|check| check.get("pass").and_then(Value::as_bool) == Some(true))
            .count();
        println!(
            "[{}] {action} repro={} checks={ok}/{}",
            index + 1,
            serde_json::to_string(&repro).unwrap_or_default(),
            checks.len()
        );
        for check in checks {
            if check.get("pass").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            failed += 1;
            println!(
                "    FAIL {} expected={} actual={}",
                check.get("path").and_then(Value::as_str).unwrap_or("?"),
                check.get("expected").unwrap_or(&Value::Null),
                check.get("actual").unwrap_or(&Value::Null)
            );
        }
    }
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn resolve_path<'a>(mut value: &'a Value, path: &str) -> Option<&'a Value> {
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

fn audit(dir: &Path) -> ExitCode {
    let records = read_records(dir, true);
    let mut actions: HashSet<u64> = HashSet::new();
    let mut outcomes: HashSet<u64> = HashSet::new();
    let mut duplicates = 0usize;
    let mut missing_action = 0usize;
    for record in &records {
        let Some(id) = record.get("action_id").and_then(Value::as_u64) else {
            continue;
        };
        match record.get("kind").and_then(Value::as_str) {
            Some("action") => {
                duplicates += usize::from(!actions.insert(id));
            }
            Some("outcome") => {
                duplicates += usize::from(!outcomes.insert(id));
            }
            _ => {}
        }
    }
    let mut problems = 0usize;
    for id in &actions {
        if !outcomes.contains(id) {
            println!("missing outcome action_id={id}");
            problems += 1;
        }
    }
    for id in &outcomes {
        if !actions.contains(id) {
            println!("orphan outcome action_id={id}");
            missing_action += 1;
        }
    }
    println!(
        "actions={} outcomes={} missing_outcomes={} orphan_outcomes={} duplicates={}",
        actions.len(),
        outcomes.len(),
        problems,
        missing_action,
        duplicates
    );
    if problems + missing_action + duplicates == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn is_failure(outcome: &str) -> bool {
    !matches!(
        outcome,
        "completed"
            | "settled"
            | "sent"
            | "dispatched"
            | "routed"
            | "opened"
            | "started"
            | "pass"
            | "pass_resolved"
            | "ok"
    )
}

fn stamp(record: &Value) -> String {
    let up = record.get("up_ms").and_then(Value::as_u64).unwrap_or(0);
    format!(
        "{:02}:{:02}.{:03}",
        up / 60_000,
        (up / 1_000) % 60,
        up % 1_000
    )
}

fn compact(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    let text = match value {
        Value::Null => return String::new(),
        Value::String(text) => text.clone(),
        Value::Array(items) if items.is_empty() => return String::new(),
        Value::Object(map) if map.is_empty() => return String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    truncate(&text, 180)
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_predicate_covers_expected_statuses() {
        assert!(!is_failure("settled"));
        assert!(!is_failure("pass_resolved"));
        assert!(is_failure("noop"));
        assert!(is_failure("suppressed"));
        assert!(is_failure("unfinished"));
    }

    #[test]
    fn stamp_formats_minutes() {
        let record = serde_json::json!({ "up_ms": 61_234 });
        assert_eq!(stamp(&record), "01:01.234");
    }

    #[test]
    fn lane_file_filters_exclude_manifest() {
        let dir = std::env::temp_dir().join(format!(
            "sdsc-trace-cli-{}-{}",
            std::process::id(),
            super::super::epoch_ms()
        ));
        let _ = fs::create_dir_all(&dir);
        fs::write(dir.join("actions.jsonl"), "").unwrap();
        fs::write(dir.join("actions.1.jsonl"), "").unwrap();
        fs::write(dir.join("browser.jsonl"), "").unwrap();
        fs::write(dir.join("manifest.jsonl"), "").unwrap();
        let files = lane_files(&dir, true);
        assert_eq!(files.len(), 2);
        let files = lane_files(&dir, false);
        assert_eq!(files.len(), 3);
        let _ = fs::remove_dir_all(dir);
    }
}
