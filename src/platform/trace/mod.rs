//! Session tracing: per-lane JSONL event files, causal flows, and a thin
//! action timeline that is structured for replay.
//!
//! `app.log` remains the human-facing error/warning stream. This controller is
//! for structured, searchable session recordings:
//!
//! - every lane owns its own file (`browser.jsonl`, `engine.jsonl`, ...),
//! - every record carries a global sequence and optional flow id,
//! - semantic user actions go to `actions.jsonl` and always get an outcome,
//! - high-level actions are always recorded; lane detail is debug/full only.
//!
//! See `notes/tracing.md` for the schema and CLI recipes.

mod cli;
mod manifest;
pub mod replay;

pub use cli::run as run_cli;

use crate::platform::app_log;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime};

const SCHEMA_VERSION: u64 = 1;
const MAX_LANE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 64 * 1024;

/// Fixed infrastructure lanes. Apps are a scope inside a lane, not a new lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    Actions,
    Input,
    App,
    Home,
    Launcher,
    Browser,
    Engine,
    Device,
    Games,
    Persist,
    Platform,
}

impl Lane {
    pub const ALL: [Lane; 11] = [
        Lane::Actions,
        Lane::Input,
        Lane::App,
        Lane::Home,
        Lane::Launcher,
        Lane::Browser,
        Lane::Engine,
        Lane::Device,
        Lane::Games,
        Lane::Persist,
        Lane::Platform,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Actions => "actions",
            Self::Input => "input",
            Self::App => "app",
            Self::Home => "home",
            Self::Launcher => "launcher",
            Self::Browser => "browser",
            Self::Engine => "engine",
            Self::Device => "device",
            Self::Games => "games",
            Self::Persist => "persist",
            Self::Platform => "platform",
        }
    }

    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Actions => "actions.jsonl",
            Self::Input => "input.jsonl",
            Self::App => "app.jsonl",
            Self::Home => "home.jsonl",
            Self::Launcher => "launcher.jsonl",
            Self::Browser => "browser.jsonl",
            Self::Engine => "engine.jsonl",
            Self::Device => "device.jsonl",
            Self::Games => "games.jsonl",
            Self::Persist => "persist.jsonl",
            Self::Platform => "platform.jsonl",
        }
    }
}

/// How much tracing is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Off,
    /// Only semantic actions/outcomes + the manifest.
    Actions,
    /// Actions plus every lane detail event.
    Full,
}

impl Level {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Actions => "actions",
            Self::Full => "full",
        }
    }

    fn from_env() -> Self {
        match std::env::var("SDSC_TRACE").ok().as_deref() {
            Some("off" | "0" | "false") => Self::Off,
            Some("actions" | "1" | "true") => Self::Actions,
            Some("full" | "2") => Self::Full,
            _ if cfg!(debug_assertions) => Self::Full,
            _ => Self::Actions,
        }
    }
}

/// Causal chain id. One top-level user intent gets one flow; async
/// continuations carry it explicitly across threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct FlowId(pub u64);

impl FlowId {
    pub fn raw(self) -> u64 {
        self.0
    }
}

static CONTROLLER: OnceLock<Controller> = OnceLock::new();
static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);
static NEXT_FLOW: AtomicU64 = AtomicU64::new(1);
static NEXT_ACTION: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static CURRENT_FLOW: Cell<Option<FlowId>> = const { Cell::new(None) };
}

/// Initialize tracing for this process. Call once, after the single-instance
/// guard and before `app::run()`. The CEF helper processes must not call it.
pub fn init() {
    if CONTROLLER.get().is_some() {
        return;
    }
    let level = Level::from_env();
    if level == Level::Off {
        return;
    }
    let session = match app_log::session_id() {
        0 => epoch_ms(),
        id => id,
    };
    let dir = root().join(format!("session-{session}"));
    if let Err(err) = fs::create_dir_all(&dir) {
        app_log::warn(format!(
            "trace: cannot create {}: {err}; tracing disabled",
            dir.display()
        ));
        return;
    }
    let controller = Controller::new(dir.clone(), level);
    if let Err(err) = manifest::write_initial(&dir, session, level) {
        app_log::warn(format!("trace: manifest write failed: {err}"));
    }
    if CONTROLLER.set(controller).is_err() {
        return;
    }
    manifest::record("trace.start", json!({ "level": level.as_str() }));
    event(Lane::Platform, "trace.start")
        .field("level", level.as_str())
        .emit();
}

/// Flush every open lane. Safe before initialization.
pub fn flush_all() {
    if let Some(controller) = CONTROLLER.get() {
        controller.flush_all();
    }
}

/// Close the session cleanly. Safe to call twice.
pub fn shutdown() {
    if let Some(controller) = CONTROLLER.get() {
        manifest::record(
            "trace.end",
            json!({
                "uptime_ms": controller.uptime_ms(),
                "seq": NEXT_SEQ.load(Ordering::Relaxed),
            }),
        );
        event(Lane::Platform, "trace.end").emit();
        controller.flush_all();
    }
}

/// Current session directory, or the newest one on disk.
pub fn session_dir() -> Option<PathBuf> {
    CONTROLLER
        .get()
        .map(|c| c.dir.clone())
        .or_else(latest_session_dir)
}

/// Root directory containing all trace sessions.
pub fn root() -> PathBuf {
    crate::persist::paths::data_dir().join("trace")
}

pub fn session_dirs() -> Vec<PathBuf> {
    let Ok(read) = fs::read_dir(root()) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = read
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("session-"))
        })
        .collect();
    dirs.sort();
    dirs
}

pub fn latest_session_dir() -> Option<PathBuf> {
    session_dirs().pop()
}

/// True when semantic action records are active.
pub fn actions_enabled() -> bool {
    CONTROLLER.get().is_some_and(|c| c.level >= Level::Actions)
}

/// True when the given lane's detail events are active.
pub fn detail_enabled(lane: Lane) -> bool {
    CONTROLLER
        .get()
        .is_some_and(|c| c.level == Level::Full && lane != Lane::Actions)
}

/// Current flow inherited by nested events on this thread.
pub fn current_flow() -> Option<FlowId> {
    CURRENT_FLOW.with(Cell::get)
}

/// Allocate a fresh flow.
pub fn new_flow() -> FlowId {
    FlowId(NEXT_FLOW.fetch_add(1, Ordering::Relaxed))
}

/// Explicitly set/restore the thread-local flow. Usually managed by
/// [`ActionGuard`]; use this only at async boundaries where a guard cannot be
/// kept alive.
pub fn set_current_flow(flow: Option<FlowId>) {
    CURRENT_FLOW.with(|cell| cell.set(flow));
}

/// Start a semantic user action. The returned guard installs the flow for
/// nested calls and logs `outcome=unfinished` if dropped without finishing.
pub fn action(lane: Lane, action: &'static str) -> ActionBuilder {
    ActionBuilder::new(lane, action)
}

/// Emit one detail event in a lane. In `actions` level this is a no-op.
pub fn event(lane: Lane, event: &'static str) -> EventBuilder {
    EventBuilder::new(lane, event)
}

/// Append a state record to `manifest.jsonl` (prefs, controllers, tabs,
/// engine paths, window layout, ...).
pub fn manifest(kind: &'static str, value: impl Serialize) {
    manifest::record(kind, value);
}

fn next_seq() -> u64 {
    NEXT_SEQ.fetch_add(1, Ordering::Relaxed)
}

fn next_action() -> u64 {
    NEXT_ACTION.fetch_add(1, Ordering::Relaxed)
}

fn value_of<T: Serialize>(value: T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn insert_opt(map: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        map.insert(key.to_string(), value);
    }
}

fn insert_str(map: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        map.insert(key.to_string(), Value::String(value.to_string()));
    }
}

fn write_record(controller: &Controller, base: &'static str, record: &Map<String, Value>) {
    if record.len() > 512 {
        return;
    }
    let line = match serde_json::to_string(record) {
        Ok(mut line) => {
            line.push('\n');
            line
        }
        Err(_) => return,
    };
    if line.len() > MAX_RECORD_BYTES {
        return;
    }
    controller.write(base, &line);
}

/// A semantic action begin. Build it with [`action`], then call [`begin`].
pub struct ActionBuilder {
    lane: Lane,
    action: &'static str,
    app: Option<String>,
    instance: Option<String>,
    component: Option<String>,
    source: Option<String>,
    params: Map<String, Value>,
    pre: Option<Value>,
    requires: Vec<Value>,
    repro: Option<Value>,
    parent: Option<FlowId>,
    span: Option<String>,
}

impl ActionBuilder {
    fn new(lane: Lane, action: &'static str) -> Self {
        Self {
            lane,
            action,
            app: None,
            instance: None,
            component: None,
            source: None,
            params: Map::new(),
            pre: None,
            requires: Vec::new(),
            repro: None,
            parent: None,
            span: None,
        }
    }

    pub fn app(mut self, app: impl Into<String>) -> Self {
        self.app = Some(app.into());
        self
    }

    pub fn instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    pub fn component(mut self, component: impl Into<String>) -> Self {
        self.component = Some(component.into());
        self
    }

    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    pub fn field(mut self, key: &str, value: impl Serialize) -> Self {
        self.params.insert(key.to_string(), value_of(value));
        self
    }

    /// Bounded before-state snapshot.
    pub fn pre(mut self, value: impl Serialize) -> Self {
        self.pre = Some(value_of(value));
        self
    }

    /// Stable precondition (`path=value`). The full `pre` snapshot is for
    /// forensics; these are the fields replay validates.
    pub fn require(mut self, path: &str, value: impl Serialize) -> Self {
        self.requires
            .push(json!({ "path": path, "op": "eq", "value": value_of(value) }));
        self
    }

    /// Structured replay command, e.g. `{app, action, params}`.
    pub fn repro(mut self, value: impl Serialize) -> Self {
        self.repro = Some(value_of(value));
        self
    }

    /// Start a child flow from an existing flow.
    pub fn parent(mut self, flow: FlowId) -> Self {
        self.parent = Some(flow);
        self
    }

    pub fn span(mut self, span: impl Into<String>) -> Self {
        self.span = Some(span.into());
        self
    }

    pub fn begin(self) -> ActionGuard {
        let enabled = actions_enabled();
        if !enabled {
            return ActionGuard::disabled();
        }
        let previous = current_flow();
        let (flow, parent) = match self.parent {
            Some(parent) => (new_flow(), Some(parent)),
            None => (previous.unwrap_or_else(new_flow), None),
        };
        let action_id = next_action();
        let started = Instant::now();
        let controller = match CONTROLLER.get() {
            Some(controller) => controller,
            None => return ActionGuard::disabled(),
        };

        let mut record = Map::new();
        record.insert("v".into(), json!(SCHEMA_VERSION));
        record.insert("ts".into(), json!(epoch_ms()));
        record.insert("up_ms".into(), json!(controller.uptime_ms()));
        record.insert("seq".into(), json!(next_seq()));
        record.insert("kind".into(), json!("action"));
        record.insert("action_id".into(), json!(action_id));
        record.insert("flow".into(), json!(flow.0));
        insert_opt(&mut record, "parent", parent.map(|p| json!(p.0)));
        insert_str(&mut record, "span", self.span.as_deref());
        record.insert("lane".into(), json!(self.lane.as_str()));
        insert_str(&mut record, "app", self.app.as_deref());
        insert_str(&mut record, "instance", self.instance.as_deref());
        insert_str(&mut record, "component", self.component.as_deref());
        record.insert("action".into(), json!(self.action));
        insert_str(&mut record, "source", self.source.as_deref());
        if !self.params.is_empty() {
            record.insert("params".into(), Value::Object(self.params));
        }
        insert_opt(&mut record, "pre", self.pre);
        if !self.requires.is_empty() {
            record.insert("requires".into(), Value::Array(self.requires));
        }
        insert_opt(&mut record, "repro", self.repro);
        write_record(controller, Lane::Actions.file_name(), &record);

        set_current_flow(Some(flow));
        ActionGuard {
            flow,
            action_id,
            lane: self.lane,
            action: self.action,
            started,
            enabled: true,
            previous,
            finished: false,
        }
    }
}

/// Live action guard. Finishing restores the previous thread-local flow.
pub struct ActionGuard {
    flow: FlowId,
    action_id: u64,
    lane: Lane,
    action: &'static str,
    started: Instant,
    enabled: bool,
    previous: Option<FlowId>,
    finished: bool,
}

impl ActionGuard {
    fn disabled() -> Self {
        Self {
            flow: FlowId(0),
            action_id: 0,
            lane: Lane::Actions,
            action: "",
            started: Instant::now(),
            enabled: false,
            previous: None,
            finished: true,
        }
    }

    pub fn flow(&self) -> FlowId {
        self.flow
    }

    pub fn action_id(&self) -> u64 {
        self.action_id
    }

    /// Finish synchronously. The returned builder auto-emits on drop, so
    /// `guard.finish("opened").post(state);` is enough.
    pub fn finish(mut self, outcome: &'static str) -> OutcomeBuilder {
        self.finished = true;
        if self.enabled {
            set_current_flow(self.previous);
        }
        OutcomeBuilder::new(
            self.flow,
            self.action_id,
            self.lane,
            self.action,
            self.started,
            true,
            outcome,
        )
    }

    /// Keep the flow open across an async boundary. The pending value logs
    /// `outcome=unfinished` if dropped without `settle`.
    pub fn detach(mut self) -> PendingAction {
        self.finished = true;
        if self.enabled {
            set_current_flow(self.previous);
        }
        PendingAction {
            flow: self.flow,
            action_id: self.action_id,
            lane: self.lane,
            action: self.action,
            started: self.started,
            enabled: self.enabled,
            settled: false,
        }
    }
}

impl Drop for ActionGuard {
    fn drop(&mut self) {
        if self.finished || !self.enabled {
            return;
        }
        set_current_flow(self.previous);
        OutcomeBuilder::new(
            self.flow,
            self.action_id,
            self.lane,
            self.action,
            self.started,
            true,
            "unfinished",
        )
        .emit();
    }
}

/// Detached action flow awaiting an async outcome.
pub struct PendingAction {
    flow: FlowId,
    action_id: u64,
    lane: Lane,
    action: &'static str,
    started: Instant,
    enabled: bool,
    settled: bool,
}

impl PendingAction {
    pub fn flow(&self) -> FlowId {
        self.flow
    }

    pub fn action_id(&self) -> u64 {
        self.action_id
    }

    pub fn settle(mut self, outcome: &'static str) -> OutcomeBuilder {
        self.settled = true;
        OutcomeBuilder::new(
            self.flow,
            self.action_id,
            self.lane,
            self.action,
            self.started,
            self.enabled,
            outcome,
        )
    }

    /// Close as unfinished with a reason.
    pub fn abandon(mut self, outcome: &'static str) -> OutcomeBuilder {
        self.settled = true;
        OutcomeBuilder::new(
            self.flow,
            self.action_id,
            self.lane,
            self.action,
            self.started,
            self.enabled,
            outcome,
        )
    }
}

impl Drop for PendingAction {
    fn drop(&mut self) {
        if self.settled || !self.enabled {
            return;
        }
        OutcomeBuilder::new(
            self.flow,
            self.action_id,
            self.lane,
            self.action,
            self.started,
            true,
            "unfinished",
        )
        .emit();
    }
}

/// Action outcome builder. Emits on drop when not explicitly emitted.
pub struct OutcomeBuilder {
    flow: FlowId,
    action_id: u64,
    lane: Lane,
    action: &'static str,
    started: Instant,
    enabled: bool,
    outcome: &'static str,
    fields: Map<String, Value>,
    post: Option<Value>,
    checks: Vec<Value>,
    resolution: Option<Value>,
    written: bool,
}

impl OutcomeBuilder {
    fn new(
        flow: FlowId,
        action_id: u64,
        lane: Lane,
        action: &'static str,
        started: Instant,
        enabled: bool,
        outcome: &'static str,
    ) -> Self {
        Self {
            flow,
            action_id,
            lane,
            action,
            started,
            enabled,
            outcome,
            fields: Map::new(),
            post: None,
            checks: Vec::new(),
            resolution: None,
            written: false,
        }
    }

    pub fn field(mut self, key: &str, value: impl Serialize) -> Self {
        self.fields.insert(key.to_string(), value_of(value));
        self
    }

    /// Bounded after-state snapshot.
    pub fn post(mut self, value: impl Serialize) -> Self {
        self.post = Some(value_of(value));
        self
    }

    /// Stable postcondition result for replay/audit.
    pub fn check(mut self, name: &str, pass: bool) -> Self {
        self.checks.push(json!({ "name": name, "pass": pass }));
        self
    }

    /// Replay resolution details (matched_by, requested/actual index, ...).
    pub fn resolution(mut self, value: impl Serialize) -> Self {
        self.resolution = Some(value_of(value));
        self
    }

    pub fn emit(mut self) {
        self.written = true;
        self.write();
    }

    fn write(&self) {
        if !self.enabled {
            return;
        }
        let Some(controller) = CONTROLLER.get() else {
            return;
        };
        let mut record = Map::new();
        record.insert("v".into(), json!(SCHEMA_VERSION));
        record.insert("ts".into(), json!(epoch_ms()));
        record.insert("up_ms".into(), json!(controller.uptime_ms()));
        record.insert("seq".into(), json!(next_seq()));
        record.insert("kind".into(), json!("outcome"));
        record.insert("action_id".into(), json!(self.action_id));
        record.insert("flow".into(), json!(self.flow.0));
        record.insert("lane".into(), json!(self.lane.as_str()));
        record.insert("action".into(), json!(self.action));
        record.insert("outcome".into(), json!(self.outcome));
        record.insert(
            "duration_ms".into(),
            json!(self.started.elapsed().as_millis() as u64),
        );
        insert_opt(&mut record, "post", self.post.clone());
        if !self.fields.is_empty() {
            for (key, value) in &self.fields {
                record.insert(key.clone(), value.clone());
            }
        }
        if !self.checks.is_empty() {
            record.insert("checks".into(), Value::Array(self.checks.clone()));
        }
        insert_opt(&mut record, "resolution", self.resolution.clone());
        write_record(controller, Lane::Actions.file_name(), &record);
    }
}

impl Drop for OutcomeBuilder {
    fn drop(&mut self) {
        if !self.written {
            self.written = true;
            self.write();
        }
    }
}

/// One detail event in a lane.
pub struct EventBuilder {
    lane: Lane,
    event: &'static str,
    flow: Option<FlowId>,
    parent: Option<FlowId>,
    span: Option<String>,
    app: Option<String>,
    instance: Option<String>,
    component: Option<String>,
    fields: Map<String, Value>,
    state: Option<Value>,
    written: bool,
}

impl EventBuilder {
    fn new(lane: Lane, event: &'static str) -> Self {
        Self {
            lane,
            event,
            flow: None,
            parent: None,
            span: None,
            app: None,
            instance: None,
            component: None,
            fields: Map::new(),
            state: None,
            written: false,
        }
    }

    pub fn flow(mut self, flow: FlowId) -> Self {
        self.flow = Some(flow);
        self
    }

    pub fn parent(mut self, flow: FlowId) -> Self {
        self.parent = Some(flow);
        self
    }

    pub fn span(mut self, span: impl Into<String>) -> Self {
        self.span = Some(span.into());
        self
    }

    pub fn app(mut self, app: impl Into<String>) -> Self {
        self.app = Some(app.into());
        self
    }

    pub fn instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    pub fn component(mut self, component: impl Into<String>) -> Self {
        self.component = Some(component.into());
        self
    }

    pub fn field(mut self, key: &str, value: impl Serialize) -> Self {
        self.fields.insert(key.to_string(), value_of(value));
        self
    }

    pub fn state(mut self, value: impl Serialize) -> Self {
        self.state = Some(value_of(value));
        self
    }

    pub fn emit(mut self) {
        self.written = true;
        self.write();
    }

    fn write(&self) {
        if !detail_enabled(self.lane) {
            return;
        }
        let Some(controller) = CONTROLLER.get() else {
            return;
        };
        let mut record = Map::new();
        record.insert("v".into(), json!(SCHEMA_VERSION));
        record.insert("ts".into(), json!(epoch_ms()));
        record.insert("up_ms".into(), json!(controller.uptime_ms()));
        record.insert("seq".into(), json!(next_seq()));
        record.insert("kind".into(), json!("event"));
        insert_opt(
            &mut record,
            "flow",
            self.flow.or_else(current_flow).map(|f| json!(f.0)),
        );
        insert_opt(&mut record, "parent", self.parent.map(|f| json!(f.0)));
        insert_str(&mut record, "span", self.span.as_deref());
        record.insert("lane".into(), json!(self.lane.as_str()));
        insert_str(&mut record, "app", self.app.as_deref());
        insert_str(&mut record, "instance", self.instance.as_deref());
        insert_str(&mut record, "component", self.component.as_deref());
        record.insert("event".into(), json!(self.event));
        for (key, value) in &self.fields {
            record.insert(key.clone(), value.clone());
        }
        insert_opt(&mut record, "state", self.state.clone());
        write_record(controller, self.lane.file_name(), &record);
    }
}

impl Drop for EventBuilder {
    fn drop(&mut self) {
        if !self.written {
            self.written = true;
            self.write();
        }
    }
}

/// `trace_event!(Lane::Browser, "history.entry", index = 0, url = url);`
/// Fields are only evaluated when the lane is active.
#[macro_export]
macro_rules! trace_event {
    ($lane:expr, $event:expr $(, $key:ident = $value:expr)* $(,)?) => {{
        let lane = $lane;
        if $crate::platform::trace::detail_enabled(lane) {
            $crate::platform::trace::event(lane, $event)
                $(.field(stringify!($key), $value))*
                .emit();
        }
    }};
}

struct Controller {
    dir: PathBuf,
    started: Instant,
    level: Level,
    lanes: Mutex<BTreeMap<&'static str, LaneWriter>>,
    failed: AtomicBool,
}

impl Controller {
    fn new(dir: PathBuf, level: Level) -> Self {
        Self {
            dir,
            started: Instant::now(),
            level,
            lanes: Mutex::new(BTreeMap::new()),
            failed: AtomicBool::new(false),
        }
    }

    fn uptime_ms(&self) -> u128 {
        self.started.elapsed().as_millis()
    }

    fn write(&self, base: &'static str, line: &str) {
        let mut lanes = self
            .lanes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !lanes.contains_key(base) {
            match LaneWriter::open(&self.dir, base) {
                Ok(writer) => {
                    lanes.insert(base, writer);
                }
                Err(err) => {
                    self.report(err);
                    return;
                }
            }
        }
        let Some(writer) = lanes.get_mut(base) else {
            return;
        };
        if let Err(err) = writer.write_line(line) {
            self.report(err);
        }
    }

    fn report(&self, err: std::io::Error) {
        if !self.failed.swap(true, Ordering::Relaxed) {
            app_log::warn(format!("trace: write failed: {err}"));
        }
    }

    fn flush_all(&self) {
        let mut lanes = self
            .lanes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for writer in lanes.values_mut() {
            writer.flush();
        }
    }
}

struct LaneWriter {
    dir: PathBuf,
    base: &'static str,
    generation: u32,
    file: BufWriter<File>,
    bytes: u64,
}

impl LaneWriter {
    fn open(dir: &Path, base: &'static str) -> std::io::Result<Self> {
        Self::open_generation(dir, base, 0)
    }

    fn open_generation(dir: &Path, base: &'static str, generation: u32) -> std::io::Result<Self> {
        let path = lane_path(dir, base, generation);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            dir: dir.to_path_buf(),
            base,
            generation,
            file: BufWriter::new(file),
            bytes,
        })
    }

    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        if self.bytes + line.len() as u64 > MAX_LANE_BYTES {
            self.rotate()?;
        }
        self.file.write_all(line.as_bytes())?;
        self.file.flush()?;
        self.bytes += line.len() as u64;
        Ok(())
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        self.file.flush()?;
        self.generation = self.generation.wrapping_add(1);
        *self = Self::open_generation(&self.dir, self.base, self.generation)?;
        Ok(())
    }

    fn flush(&mut self) {
        let _ = self.file.flush();
    }
}

fn lane_path(dir: &Path, base: &'static str, generation: u32) -> PathBuf {
    if generation == 0 {
        return dir.join(base);
    }
    let stem = base.strip_suffix(".jsonl").unwrap_or(base);
    dir.join(format!("{stem}.{generation}.jsonl"))
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sdsc-trace-test-{name}-{}-{}",
            std::process::id(),
            epoch_ms()
        ));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn lane_names_and_files_are_stable() {
        for lane in Lane::ALL {
            assert_eq!(lane.as_str(), lane.file_name().trim_end_matches(".jsonl"));
        }
    }

    #[test]
    fn lanes_write_to_separate_files() {
        let dir = temp_dir("lanes");
        let controller = Controller::new(dir.clone(), Level::Full);
        let mut event = Map::new();
        event.insert("kind".into(), json!("event"));
        event.insert("event".into(), json!("one"));
        write_record(&controller, Lane::Browser.file_name(), &event);
        let mut other = Map::new();
        other.insert("kind".into(), json!("event"));
        other.insert("event".into(), json!("two"));
        write_record(&controller, Lane::Engine.file_name(), &other);
        controller.flush_all();

        assert!(dir.join("browser.jsonl").exists());
        assert!(dir.join("engine.jsonl").exists());
        let browser = fs::read_to_string(dir.join("browser.jsonl")).unwrap();
        let engine = fs::read_to_string(dir.join("engine.jsonl")).unwrap();
        assert!(browser.contains("\"one\""));
        assert!(!browser.contains("\"two\""));
        assert!(engine.contains("\"two\""));
        assert!(!engine.contains("\"one\""));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rotation_creates_new_generation() {
        let dir = temp_dir("rotate");
        let controller = Controller::new(dir.clone(), Level::Full);
        controller.write(Lane::Browser.file_name(), "{\"event\":\"seed\"}\n");
        let mut lanes = controller.lanes.lock().unwrap_or_else(|e| e.into_inner());
        let writer = lanes.get_mut(Lane::Browser.file_name()).unwrap();
        writer.bytes = MAX_LANE_BYTES;
        writer.write_line("{\"event\":\"after-rotate\"}\n").unwrap();
        drop(lanes);
        controller.flush_all();
        assert!(dir.join("browser.1.jsonl").exists());
        let _ = fs::remove_dir_all(dir);
    }
}
