//! Servo offscreen worker thread (`servo-engine` feature only).
//!
//! Every Servo type here is `!Send` by design, so all of Servo lives on this
//! one thread for the process lifetime — the same ownership shape as the HID
//! worker (blocking I/O off the UI executor) and the art worker. The iced
//! shell talks in [`EngineCommand`]s and receives [`EngineEvent`]s; nothing
//! Servo ever touches the UI thread.
//!
//! Backend contract: software rendering at a fixed viewport, frames as RGBA
//! for an iced image handle. No GL context is shared with iced/wgpu (that
//! airspace fight is exactly what the system-webview child-HWND approach
//! loses), so Start chrome, toasts, and veils composite above the page.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use dpi::PhysicalSize;
use embedder_traits::{
    InputEvent, InputEventId, InputEventResult, JSValue, KeyboardEvent, MouseButton,
    MouseButtonAction, MouseButtonEvent, MouseMoveEvent, Theme, WebResourceResponse,
};
use euclid::{Box2D, Point2D};

use servo::{
    DevicePixel, DevicePoint, DeviceVector2D, EventLoopWaker, LoadStatus, Opts, RenderingContext,
    Scroll, ServoBuilder, SoftwareRenderingContext, UserContentManager, UserScript, WebView,
    WebViewBuilder, WebViewDelegate, WebViewPoint, WebViewVector,
};

use crate::browser::cursor::{FocusKind, Focusable};
use crate::browser::dom::{
    DOM_DIRTY_MARKER, DOM_INSTALL_JS, DOM_PING_URL, PROBE_ARM_JS, PROBE_READ_JS, dom_boot_js,
    dom_kind, dom_query_js, parse_dom_snapshot, parse_loop_probe,
};
use crate::browser::engine::{EngineCommand, EngineEvent, EngineHandle, PageFrame};

/// Fixed offscreen viewport (phase 1; the iced image scales to the card).
const VIEWPORT_WIDTH: u32 = 800;
const VIEWPORT_HEIGHT: u32 = 600;
/// Spin cadence while foreground (matches the ~60Hz UI tick budget).
const SPIN_IDLE: Duration = Duration::from_millis(8);
/// Spin cadence while suspended: timers parked, just drain commands.
const SUSPENDED_IDLE: Duration = Duration::from_millis(100);
/// Give up on a load that never completes (DNS blackhole, captive portal).
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// Wakes the worker loop; Servo calls this from arbitrary threads.
#[derive(Clone)]
struct ThreadWaker {
    tx: mpsc::Sender<EngineCommand>,
}

impl EventLoopWaker for ThreadWaker {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(self.clone())
    }

    fn wake(&self) {
        let _ = self.tx.send(EngineCommand::Wake);
    }
}

struct Shared {
    ctx: Rc<SoftwareRenderingContext>,
    size: (u32, u32),
    handle: EngineHandle,
    generation: u64,
    load_started_at: Option<Instant>,
    logged_first_frame: bool,
    /// Last list sent to the UI (diffed to avoid spamming identical trees).
    last_emitted: Vec<Focusable>,
    visible: bool,
    suspended: bool,
    /// Commanded scroll offset, page pixels. Servo reports document-relative
    /// bounds and never re-emits them on scroll, so the backend translates
    /// to viewport space at emit time. Reset on fresh navigate and on
    /// history travel — session history does not reliably restore the
    /// offset, and a stale value misplaces every outline (observed). Page-
    /// driven scrolls (anchors, JS) are invisible to us and self-heal only
    /// via fresh bounds — a known gap.
    scroll: (f32, f32),
    /// Highest scroll truth this document ever reported (snapshot `sx/sy`,
    /// ping `wx/wy`), per axis, monotonically growing. Bounds commanded
    /// accumulation: at the page end the truth pins, so translations stop
    /// instead of sailing past into the void. Truth always proves
    /// reachability, so legitimate scrolling can never block — the bound
    /// only ever grows. Reset on document change, like `scroll`.
    scroll_max: (f32, f32),
    /// Stable numeric ids for live DOM targets (the UI keys attachments
    /// by u64). Never reused within a document; cleared on document change.
    next_id: u64,
    /// DOM-supplement targets (see `browser::dom`): keyed by the in-page
    /// WeakMap id, stable per document. `missed` counts consecutive polls
    /// without a sighting; pruned at 2 (re-renders flicker once, ghosts
    /// don't survive twice).
    dom: HashMap<u64, DomEntry>,
    dom_uid: HashMap<u64, u64>,
    dom_seq: u64,
    /// Fresh document (or fresh load) needs the install+snapshot boot
    /// round-trip. Set by every document event, cleared on dispatch.
    dom_boot: bool,
    /// Set when a fetch ping dirtied the set; drained at pull time so the
    /// log can tell ping-driven pulls (`src=B`) from gesture ones.
    dom_ping: bool,
    dom_dirty: bool,
    dom_dirty_at: Option<Instant>,
    dom_in_flight: bool,
    dom_in_flight_at: Option<Instant>,
    dom_tx: mpsc::Sender<DomIngest>,
    dom_rx: Option<mpsc::Receiver<DomIngest>>,
    /// DOM epoch: bumped whenever the DOM (or anything affecting per-element
    /// visibility verdicts) may have changed — navigate/history/load, input,
    /// reseed, MutationObserver pings. Scroll-only pings don't bump, so
    /// settle pulls after pure scrolling reuse the cached verdicts instead
    /// of re-walking every ancestor. Passed into each query as `__sdsc_v`.
    dom_epoch: u64,
    /// Wall time of the latest scroll ping, for the ingest rebase guard:
    /// if a ping landed after the in-flight pull was dispatched, its truth
    /// is newer — the snapshot must not rebase backwards over it.
    ping_at: Option<Instant>,
    /// Monotonic loop counter: proves at least one full tick elapsed.
    spin_seq: u64,
    /// Armed mouse-up completing the previous synthetic click. Down goes
    /// out immediately; Up waits for the next loop tick so every press has
    /// real duration instead of a zero-width Down+Up pair. Flushed early if
    /// another click arrives first (no stranded presses).
    pending_click_up: Option<(WebViewPoint, u64)>,
    /// Last completed pull (time + round-trip ms). The next pull waits at
    /// least the last round-trip (floor 300ms): slow pulls back off instead
    /// of saturating the script thread, which would stall input dispatch
    /// and page JS behind our own queries.
    last_pull_at: Option<Instant>,
    last_pull_rtt_ms: u64,
    /// Last cursor position seen via MouseMove (viewport px). Drives the
    /// per-pull cursor cull and re-seeds it when the cursor travels.
    last_mouse: Option<(f32, f32)>,
    /// Cursor position the last pull culled around. A roam past 300px marks
    /// dirty so the live set follows the cursor.
    last_cull_center: Option<(f32, f32)>,
    /// In-flight input events awaiting their handled callback: (id, kind,
    /// sent-at). Bounded; entries older than 5s mean the input pipeline
    /// swallowed an event — the stall signature. Drained on completion.
    pending_inputs: VecDeque<(InputEventId, &'static str, Instant)>,
    /// One-shot event-loop probe (R3 diagnostic): result channel + read
    /// deadline. Armed by `ProbeEventLoop`, read once, then cleared.
    /// `probe_armed_at` marks the arm dispatch; the READ goes out 600ms
    /// later so the 100ms timer and one rAF have a chance to fire.
    /// `probe_rx` carries the READ result; `probe_deadline` bounds it.
    probe_armed_at: Option<Instant>,
    probe_rx: Option<mpsc::Receiver<Result<JSValue, String>>>,
    probe_deadline: Option<Instant>,
    /// Last scroll offset sent to the UI (diffed like the tree).
    last_sent_scroll: (f32, f32),
    /// Document extent (max far corner seen, page pixels). Bounds the
    /// tracked scroll: pages can't scroll past origin or past
    /// extent-viewport, but commanded deltas arrive regardless (clamped
    /// no-ops at edges, overshoot while held). Reset on document change.
    content: Option<(f32, f32)>,
    /// Fragment-stripped URL of the current document. History entries are
    /// compared against it: a new document clears the tree, same-document
    /// fragment/pushState traffic keeps it (no flash, no stale nodes).
    last_url: Option<String>,
    /// Verbose per-target DOM diagnostics (`SDSC_AX_DEBUG=1`): js id, kind,
    /// and rect for every freshly mapped target.
    ax_debug: bool,
}

struct Delegate {
    shared: Rc<RefCell<Shared>>,
}

impl WebViewDelegate for Delegate {
    fn notify_new_frame_ready(&self, webview: WebView) {
        webview.paint();
        let mut shared = self.shared.borrow_mut();
        if shared.suspended {
            return;
        }
        let (w, h) = shared.size;
        let rect: Box2D<i32, DevicePixel> =
            Box2D::new(Point2D::new(0, 0), Point2D::new(w as i32, h as i32));
        if let Some(img) = shared.ctx.read_to_image(rect) {
            let (w, h) = (img.width(), img.height());
            if !shared.logged_first_frame {
                shared.logged_first_frame = true;
                crate::platform::app_log::info(format!(
                    "servo-engine: first frame {w}x{h}, worker live"
                ));
            }
            shared.handle.emit(EngineEvent::Frame(PageFrame {
                width: w,
                height: h,
                rgba: img.into_raw(),
            }));
        }
    }

    fn notify_url_changed(&self, _webview: WebView, url: url::Url) {
        // NOTE: deliberately does not touch `last_url` — history comparison
        // owns document identity, and url-changed fires first on
        // client-side navigations. Updating here would pre-empt the clear.
        self.shared.borrow().handle.emit(EngineEvent::UrlChanged {
            url: url.to_string(),
        });
    }

    fn notify_history_changed(&self, _webview: WebView, entries: Vec<url::Url>, current: usize) {
        // Client-side navigation (link click, redirect, form submit, JS):
        // a new document arrives through no EngineCommand. Same-document
        // traffic (fragments) keeps the tree; anything else clears it.
        let Some(url) = entries.get(current) else {
            return;
        };
        let mut shared = self.shared.borrow_mut();
        let key = doc_key(url.as_str());
        if shared.last_url.as_deref() == Some(key) {
            return;
        }
        crate::platform::app_log::info(format!("servo-engine: history cleared for {key}"));
        shared.last_url = Some(key.to_string());
        clear_tree(&mut shared);
    }

    fn notify_page_title_changed(&self, _webview: WebView, title: Option<String>) {
        self.shared
            .borrow()
            .handle
            .emit(EngineEvent::TitleChanged { title });
    }

    fn notify_load_status_changed(&self, _webview: WebView, status: LoadStatus) {
        let mut shared = self.shared.borrow_mut();
        match status {
            LoadStatus::Started | LoadStatus::HeadParsed => {
                crate::platform::app_log::info(format!(
                    "servo-engine: load {status:?} url={}",
                    shared.last_url.as_deref().unwrap_or("?"),
                ));
            }
            LoadStatus::Complete => {
                if let Some(started) = shared.load_started_at {
                    crate::platform::app_log::info(format!(
                        "servo-engine: load Complete in {}ms",
                        started.elapsed().as_millis()
                    ));
                }
                shared.load_started_at = None;
                // Fresh document settling in: boot (idempotent install +
                // snapshot) once rendering lands. New load, new verdicts.
                shared.dom_boot = true;
                shared.dom_epoch = shared.dom_epoch.wrapping_add(1);
                let generation = shared.generation;
                shared.handle.emit(EngineEvent::LoadFinished {
                    generation,
                    result: Ok(()),
                });
            }
            LoadStatus::Stopped => {
                // Failed/stopped loads previously sat silent until the 30s
                // timeout misreported them. Surface immediately.
                crate::platform::app_log::warn("servo-engine: load stopped before complete");
                shared.load_started_at = None;
                let generation = shared.generation;
                shared.handle.emit(EngineEvent::LoadFinished {
                    generation,
                    result: Err("load stopped before complete".to_string()),
                });
            }
        }
    }

    fn notify_crashed(&self, _webview: WebView, reason: String, backtrace: Option<String>) {
        crate::platform::app_log::warn(format!(
            "servo-engine: crashed reason={reason} backtrace={}",
            backtrace
                .unwrap_or_default()
                .chars()
                .take(500)
                .collect::<String>()
        ));
        let mut shared = self.shared.borrow_mut();
        shared.load_started_at = None;
        let generation = shared.generation;
        shared.handle.emit(EngineEvent::LoadFinished {
            generation,
            result: Err(format!("page crashed: {reason}")),
        });
    }

    fn load_web_resource(&self, _webview: WebView, load: servo::WebResourceLoad) {
        // Dirty-ping channel: the in-page ping POSTs to the marker URL.
        // Intercept with an empty 200 before anything leaves the machine.
        let url = load.request().url.to_string();
        if !url.contains(DOM_DIRTY_MARKER) {
            return;
        }
        // Mutation pings invalidate the cached per-element visibility
        // verdicts only when attributes changed (`ma`: class/style/hidden
        // flips can hide whole subtrees). Structural pings (`mc`:
        // insert/remove/text/resize) and scroll pings (`sc`) leave
        // surviving nodes' verdicts valid — new nodes miss the cache and
        // compute fresh. Unknown tags (incl. pre-taxonomy `mo`) bump:
        // safe default.
        {
            let mut state = self.shared.borrow_mut();
            state.ping_at = Some(Instant::now());
            if !(url.contains("ev=sc") || url.contains("ev=mc")) {
                state.dom_epoch = state.dom_epoch.wrapping_add(1);
            }
        }
        mark_dom_dirty(&self.shared);
        // Window truth rides on the ping query: a nested-container scroll
        // leaves window.scroll unchanged, so correcting to it here instantly
        // reverts any commanded accumulation the inner scroller absorbed —
        // outlines stop tracking a scroll the page never made.
        if let Some(truth) = parse_ping_scroll(&url) {
            let mut state = self.shared.borrow_mut();
            note_scroll_truth(&mut state.scroll_max, truth);
            rebase_scroll(&mut state, truth);
        }
        let response = url
            .parse()
            .map(WebResourceResponse::new)
            .unwrap_or_else(|_| WebResourceResponse::new(DOM_PING_URL.parse().unwrap()));
        load.intercept(response);
    }

    fn notify_input_event_handled(
        &self,
        _webview: WebView,
        event_id: InputEventId,
        result: InputEventResult,
    ) {
        // Correlate outcomes with sent inputs: DispatchFailed means the
        // engine dropped it; handled-but-unconsumed means dispatch reached
        // the page with no handler acting. DefaultPrevented-but-unconsumed
        // is the tell for jsaction-style links: the page ate the event
        // (killing native activation) and its JS never followed through.
        // Moves log only on failure (pad-tick rate would spam).
        let mut shared = self.shared.borrow_mut();
        let pos = shared
            .pending_inputs
            .iter()
            .position(|(id, _, _)| *id == event_id);
        let Some(idx) = pos else {
            return;
        };
        let (_, kind, sent_at) = shared.pending_inputs.remove(idx).unwrap();
        let failed = result.contains(InputEventResult::DispatchFailed);
        let consumed = result.contains(InputEventResult::Consumed);
        let prevented = result.contains(InputEventResult::DefaultPrevented);
        let dt_ms = sent_at.elapsed().as_millis();
        if failed {
            crate::platform::app_log::warn(format!(
                "servo-input: {kind} dispatch failed dt={dt_ms}ms"
            ));
        } else if kind != "move" {
            crate::platform::app_log::info(format!(
                "servo-input: {kind} handled consumed={consumed} prevented={prevented} dt={dt_ms}ms"
            ));
        }
        // Entries older than 5s with no outcome: the input pipeline
        // swallowed an event — the stall signature.
        while let Some((_, kind, at)) = shared.pending_inputs.front() {
            if at.elapsed() < Duration::from_secs(5) {
                break;
            }
            crate::platform::app_log::warn(format!(
                "servo-input: {kind} swallowed (no outcome in 5s)"
            ));
            shared.pending_inputs.pop_front();
        }
    }

    fn notify_accessibility_tree_update(
        &self,
        _webview: WebView,
        _update: servo::accesskit::TreeUpdate,
    ) {
        // AX tree intentionally ignored: the DOM query (see `browser::dom`)
        // is the sole snap-target source. Servo's AX feed withholds real
        // controls and freezes rects; keeping the mirror only grew ghosts.
    }
}

/// Mark the DOM set dirty from a fetch ping. Logs only on the
/// clean→dirty transition so animation bursts don't spam.
fn mark_dom_dirty(shared: &Rc<RefCell<Shared>>) {
    let mut state = shared.borrow_mut();
    if !state.dom_dirty {
        crate::platform::app_log::info("servo-dom: dirty via B");
    }
    state.dom_dirty = true;
    state.dom_dirty_at = Some(Instant::now());
    state.dom_ping = true;
}

/// DOM target: actionable element found by the in-page query. Rects are
/// document-space (viewport rect + scroll at ingest). `missed` counts consecutive polls
/// without a sighting; pruned at [`DOM_MISSED_LIMIT`] so re-render flicker
/// doesn't flap targets, but removed nodes still go away. `tag`/`role` are
/// the raw element identity for diagnostics (which selector arm matched).
#[derive(Debug, Clone)]
struct DomEntry {
    kind: FocusKind,
    tag: String,
    role: String,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    missed: u8,
}

/// Completed DOM query from the page: poll sequence, dispatch time (for
/// pull round-trip logging), plus the raw JS value or the evaluation error.
type DomIngest = (u64, Instant, Result<JSValue, String>);

/// Cursor travel that re-seeds the cursor-culled set: past this distance
/// from the last pull center, the live set no longer covers the cursor.
const DOM_CULL_RESEED_DIST: f32 = 300.0;

/// Minimum gap between pulls (healthy pages): the settle window already
/// paces those. Above it, the gap tracks the last round-trip so sick
/// pages back off instead of saturating the script thread.
const DOM_MIN_PULL_GAP: Duration = Duration::from_millis(300);

/// Consecutive missed polls before a DOM target is dropped.
const DOM_MISSED_LIMIT: u8 = 2;
/// Give up waiting on a DOM round-trip and allow the next poll.
const DOM_TIMEOUT: Duration = Duration::from_secs(5);
/// Cap on DOM targets kept (the query already caps at 1000; this is the
/// pathological-reparent backstop).
const DOM_MAX_TARGETS: usize = 3000;

/// Collect snap targets from the live DOM set, plus the document extent
/// (max far corner over every target) for scroll clamping. Sorted by
/// numeric id so diffing is stable.
fn collect_focusables(shared: &mut Shared) -> (Vec<Focusable>, (f32, f32)) {
    let mut out = Vec::with_capacity(shared.dom.len());
    let mut extent = (0.0f32, 0.0f32);
    for (jsid, entry) in shared.dom.iter() {
        extent.0 = extent.0.max(entry.x + entry.w);
        extent.1 = extent.1.max(entry.y + entry.h);
        if let Some(id) = shared.dom_uid.get(jsid) {
            out.push(Focusable {
                id: *id,
                x: entry.x,
                y: entry.y,
                w: entry.w,
                h: entry.h,
                kind: entry.kind,
            });
        }
    }
    out.sort_by_key(|o| o.id);
    (out, extent)
}

/// Spawn the engine thread. Returns `None` when the GL context cannot be
/// established: the caller falls back to the reader backend.
///
/// Ordering contract (proven by the `coexist gl-first` spike): the worker
/// creates its WGL context BEFORE returning, and the caller must not let
/// iced/wgpu initialize Vulkan until this returns — NVIDIA's driver
/// segfaults worker-thread GL creation once its Vulkan ICD is up
/// (`nvoglv64.dll`). `App::new` blocks on this before the iced runtime
/// boots, so the order is deterministic, not a thread race.
///
/// The stack is oversized on purpose: SpiderMonkey + layout recurse deep
/// during init, and a default 2MiB spawned-thread stack segfaults where the
/// 8MiB main thread survives (proven by the spike, which ran on main).
pub fn spawn_engine() -> Option<EngineHandle> {
    let (handle, rx) = EngineHandle::channel();
    let worker = handle.clone();
    let wake_tx = handle.sender();
    let (ready_tx, ready_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("servo-engine".to_string())
        .stack_size(32 * 1024 * 1024)
        .spawn(move || run(rx, wake_tx, worker, ready_tx))
        .expect("spawn servo engine thread");
    // Deterministic GL-before-Vulkan barrier: iced (and its wgpu/Vulkan
    // init) only starts after `App::new` returns. Context creation itself
    // is fast; the timeout only bites when the backend is broken.
    match ready_rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(())) => Some(handle),
        Ok(Err(err)) => {
            eprintln!("servo-engine: software context failed: {err}");
            None
        }
        Err(_) => {
            eprintln!("servo-engine: context readiness timeout");
            None
        }
    }
}

fn run(
    rx: mpsc::Receiver<EngineCommand>,
    wake_tx: mpsc::Sender<EngineCommand>,
    handle: EngineHandle,
    ready: mpsc::Sender<Result<(), String>>,
) {
    // GL context FIRST: see `spawn_engine` ordering contract.
    let ctx =
        match SoftwareRenderingContext::new(PhysicalSize::new(VIEWPORT_WIDTH, VIEWPORT_HEIGHT)) {
            Ok(ctx) => Rc::new(ctx),
            Err(err) => {
                let _ = ready.send(Err(format!("{err:?}")));
                return;
            }
        };
    let _ = ready.send(Ok(()));
    // Persistent web storage: Servo reads/writes `localstorage.json` (and
    // friends) under `config_dir` — without it everything is memory-only and
    // search history, logins-in-localStorage etc. evaporate on restart. Power
    // our own data dir (namespaced subdir, created up front: Servo's writer
    // panics on a missing directory). Cookies stay memory-only: Servo 0.8
    // has no persistent cookie jar, so cookie-backed personalization still
    // resets per launch — an upstream gap, not a config one.
    let storage_dir = crate::persist::paths::data_dir().join("servo");
    if let Err(err) = std::fs::create_dir_all(&storage_dir) {
        crate::platform::app_log::info(format!(
            "servo-engine: storage dir unavailable ({}), web storage stays memory-only",
            err
        ));
    }
    let servo_opts = Opts {
        config_dir: Some(storage_dir.clone()),
        temporary_storage: false,
        ..Default::default()
    };
    crate::platform::app_log::info(format!(
        "servo-engine: storage dir={} temporary={}",
        storage_dir.display(),
        servo_opts.temporary_storage
    ));
    let servo = ServoBuilder::default()
        .opts(servo_opts)
        .event_loop_waker(Box::new(ThreadWaker { tx: wake_tx }))
        .build();
    let (dom_tx, dom_rx) = mpsc::channel();
    let shared = Rc::new(RefCell::new(Shared {
        ctx: ctx.clone(),
        size: (VIEWPORT_WIDTH, VIEWPORT_HEIGHT),
        handle,
        generation: 0,
        load_started_at: None,
        logged_first_frame: false,
        last_emitted: Vec::new(),
        visible: false,
        suspended: true,
        scroll: (0.0, 0.0),
        scroll_max: (0.0, 0.0),
        last_sent_scroll: (0.0, 0.0),
        next_id: 0,
        dom: HashMap::new(),
        dom_uid: HashMap::new(),
        dom_seq: 0,
        dom_boot: true,
        dom_ping: false,
        dom_dirty: false,
        dom_dirty_at: None,
        dom_in_flight: false,
        dom_in_flight_at: None,
        dom_tx,
        dom_rx: Some(dom_rx),
        dom_epoch: 0,
        ping_at: None,
        spin_seq: 0,
        pending_click_up: None,
        last_pull_at: None,
        last_pull_rtt_ms: 0,
        last_mouse: None,
        last_cull_center: None,
        pending_inputs: VecDeque::with_capacity(64),
        probe_armed_at: None,
        probe_rx: None,
        probe_deadline: None,
        content: None,
        last_url: None,
        ax_debug: std::env::var("SDSC_AX_DEBUG").is_ok(),
    }));
    // Auto-install the DOM observers into every document (fresh loads,
    // history travel, reloads): no per-document injection timing on our
    // side. The evaluate-boot path below stays as backup.
    let user_content = Rc::new(UserContentManager::new(&servo));
    user_content.add_script(Rc::new(UserScript::new(DOM_INSTALL_JS.to_string(), None)));
    let webview = WebViewBuilder::new(&servo, ctx.clone())
        .user_content_manager(user_content)
        .delegate(Rc::new(Delegate {
            shared: shared.clone(),
        }))
        .build();
    crate::platform::app_log::info(format!(
        "servo-engine: ready ({VIEWPORT_WIDTH}x{VIEWPORT_HEIGHT} software surface)"
    ));

    loop {
        let mut shutdown = false;
        while let Ok(command) = rx.try_recv() {
            if matches!(command, EngineCommand::Shutdown) {
                shutdown = true;
                break;
            }
            apply_command(&webview, &shared, command);
        }
        if shutdown {
            break;
        }
        // Complete the previous synthetic click: Down went out with its
        // command; Up waits for at least one full tick so presses have real
        // duration. Runs even while suspended so a press can never strand.
        {
            let mut state = shared.borrow_mut();
            state.spin_seq = state.spin_seq.wrapping_add(1);
            match state.pending_click_up.take() {
                // At least one full tick elapsed since Down: complete it.
                Some((point, at)) if state.spin_seq > at => {
                    drop(state);
                    let id = webview.notify_input_event(InputEvent::MouseButton(
                        MouseButtonEvent::new(MouseButtonAction::Up, MouseButton::Primary, point),
                    ));
                    track_input(&shared, id, "click");
                }
                // Not yet due (or nothing armed): put it back.
                pending => {
                    state.pending_click_up = pending;
                }
            }
        }
        {
            let suspended = shared.borrow().suspended;
            if !suspended {
                servo.spin_event_loop();
            }
            check_load_timeout(&shared);
            drain_dom_results(&shared);
            maybe_pull_dom(&webview, &shared);
            poll_probe(&webview, &shared);
            std::thread::sleep(if suspended { SUSPENDED_IDLE } else { SPIN_IDLE });
        }
    }
}

fn apply_command(webview: &WebView, shared: &Rc<RefCell<Shared>>, command: EngineCommand) {
    match command {
        EngineCommand::Navigate { url, generation } => {
            let mut state = shared.borrow_mut();
            match url.parse::<url::Url>() {
                Ok(parsed) => {
                    crate::platform::app_log::info(format!("servo-engine: navigate {url}"));
                    state.generation = generation;
                    state.load_started_at = Some(Instant::now());
                    // Fresh page: clear_tree drops DOM targets, scroll and extent.
                    state.last_url = Some(doc_key(&url).to_string());
                    clear_tree(&mut state);
                    state
                        .handle
                        .emit(EngineEvent::UrlChanged { url: url.clone() });
                    webview.load(parsed);
                    webview.set_focused(state.visible && !state.suspended);
                }
                Err(err) => {
                    state.handle.emit(EngineEvent::LoadFinished {
                        generation,
                        result: Err(format!("bad URL: {err}")),
                    });
                }
            }
        }
        EngineCommand::ScrollBy { dx, dy, x, y } => {
            let mut state = shared.borrow_mut();
            if state.suspended {
                return;
            }
            // Dominant axis only: a diagonal stick deflection must not leak
            // the minor axis into the wrong scroller (the carousel trap:
            // horizontal intent with slight vertical drift scrolls the
            // carousel instead of the page, or vice versa).
            let (dx, dy) = if dx.abs() >= dy.abs() {
                (dx, 0.0)
            } else {
                (0.0, dy)
            };
            // Scroll under the pad cursor, not viewport center: Servo routes
            // to the innermost scroller under the point, so center-targeting
            // stranded page scrolls inside carousels.
            webview.notify_scroll_event(
                Scroll::Delta(WebViewVector::Device(DeviceVector2D::new(dx, dy))),
                WebViewPoint::Device(DevicePoint::new(x, y)),
            );
            // Servo reports document-relative bounds and never re-emits on
            // scroll: accumulate the commanded offset and re-emit translated
            // rects so outlines track the page. `try_send` + subscription
            // conflation absorb held-stick bursts; the emit itself stays
            // silent unless nodes join/leave (see `emit_tree`). No known
            // bounds yet (pre-first-ingest): notify Servo but don't
            // integrate — unbounded accumulation with an infinite clamp is
            // exactly the overscroll-into-the-void; the first pull rebases.
            if state.content.is_some() {
                let (sx, sy) = state.scroll;
                set_scroll(&mut state, (sx + dx, sy + dy));
            }
            // No DOM re-query on scroll: stored rects are document-space, so
            // scrolling only re-translates. Virtualized insertions during a
            // scroll ping through the observers on their own.
            emit_tree(&mut state);
        }
        EngineCommand::Key(event) => {
            if shared.borrow().suspended {
                return;
            }
            let id = webview.notify_input_event(InputEvent::Keyboard(KeyboardEvent::new(event)));
            track_input(shared, id, "key");
            // Keys can mutate the DOM (typing, shortcuts, focus Cruft):
            // re-query once settled, and invalidate cached verdicts.
            let mut state = shared.borrow_mut();
            state.dom_dirty = true;
            state.dom_dirty_at = Some(Instant::now());
            state.dom_epoch = state.dom_epoch.wrapping_add(1);
        }
        EngineCommand::SetDarkMode { dark } => {
            webview.notify_theme_change(if dark { Theme::Dark } else { Theme::Light });
        }
        EngineCommand::GoBack => {
            let mut state = shared.borrow_mut();
            if state.suspended {
                return;
            }
            // History travel swaps the document: clear_tree drops DOM targets,
            // scroll and extent (no reliable scroll restore to keep).
            clear_tree(&mut state);
            webview.go_back(1);
        }
        EngineCommand::GoForward => {
            let mut state = shared.borrow_mut();
            if state.suspended {
                return;
            }
            clear_tree(&mut state);
            webview.go_forward(1);
        }
        EngineCommand::MouseMove { x, y } => {
            if shared.borrow().suspended {
                return;
            }
            let point = WebViewPoint::Device(DevicePoint::new(x, y));
            let id = webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(point)));
            track_input(shared, id, "move");
            // Roaming cursor re-seeds the cursor-culled set: once a cursor
            // position exists and the pull center is stale (or never set),
            // the live set no longer covers the cursor.
            let mut state = shared.borrow_mut();
            let roamed = match (state.last_mouse, state.last_cull_center) {
                (None, _) => false,
                (Some(_), None) => true,
                (Some(_), Some((cx, cy))) => (cx - x).hypot(cy - y) >= DOM_CULL_RESEED_DIST,
            };
            state.last_mouse = Some((x, y));
            if roamed {
                state.dom_dirty = true;
                state.dom_dirty_at = Some(Instant::now());
                // Fresh cursor region: hover states may differ here, so
                // cached visibility verdicts are suspect.
                state.dom_epoch = state.dom_epoch.wrapping_add(1);
            }
        }
        EngineCommand::ClickAt { x, y, target } => {
            if shared.borrow().suspended {
                crate::platform::app_log::info("servo-engine: click dropped (suspended)");
                return;
            }
            // Viewport-space point (same space as the cursor and the
            // translated tree); Servo maps to document internally.
            crate::platform::app_log::info(format!("servo-engine: click at ({x:.0},{y:.0})"));
            // Complete any in-flight press first: rapid clicks stay paired
            // (Down/Up, Down/Up) instead of stranding a Down.
            flush_pending_up(webview, shared);
            // Button-kind dock targets activate on the exact element via
            // `__sdsc_click` (no hit-test staleness, exercises whatever
            // handler the page registered — getEventListeners is DevTools-only
            // and no DOM API exposes listeners, so reaching the element IS
            // the discovery). Anything else (links, fields, free points,
            // canvas) keeps the trusted synthetic path.
            let jsid = target.and_then(|uid| {
                shared
                    .borrow()
                    .dom_uid
                    .iter()
                    .find(|(_, u)| **u == uid)
                    .map(|(j, _)| *j)
            });
            let button = jsid.is_some_and(|j| {
                shared
                    .borrow()
                    .dom
                    .get(&j)
                    .is_some_and(|n| n.kind == FocusKind::Button)
            });
            if button {
                let jsid = jsid.unwrap_or(0);
                let handle = shared.borrow().handle.clone();
                webview.evaluate_javascript(
                    format!("window.__sdsc_click ? window.__sdsc_click({jsid}) : 'missing'"),
                    move |result| {
                        let status = match result {
                            Ok(v) => match v {
                                JSValue::String(s) => s,
                                _ => "bad-shape".to_string(),
                            },
                            Err(e) => format!("eval-failed:{e:?}"),
                        };
                        crate::platform::app_log::info(format!(
                            "servo-engine: element click js#{jsid} -> {status}"
                        ));
                        if status != "ok" {
                            // Element gone (or helper missing): fall back to
                            // the trusted synthetic pair at the point.
                            handle.send(EngineCommand::ClickAt { x, y, target: None });
                        }
                    },
                );
            } else {
                send_synthetic_down(webview, shared, x, y);
            }
            // Clicks mutate the DOM (toggles, expands, navigation): re-query
            // once settled, and invalidate cached verdicts.
            let mut state = shared.borrow_mut();
            state.dom_dirty = true;
            state.dom_dirty_at = Some(Instant::now());
            state.dom_epoch = state.dom_epoch.wrapping_add(1);
        }
        EngineCommand::ProbeEventLoop => {
            if shared.borrow().suspended {
                return;
            }
            // Fire-and-forget arm; the loop dispatches the READ 600ms later
            // and bounds it with a deadline (see `poll_probe`).
            shared.borrow_mut().probe_armed_at = Some(Instant::now());
            crate::platform::app_log::info("servo-loop: probe armed");
            webview.evaluate_javascript(PROBE_ARM_JS.to_string(), |_| {});
        }
        EngineCommand::Resize { width, height } => {
            let mut state = shared.borrow_mut();
            let width = width.max(1);
            let height = height.max(1);
            state.ctx.resize(PhysicalSize::new(width, height));
            state.size = (width, height);
        }
        EngineCommand::SetVisible { visible } => {
            let mut state = shared.borrow_mut();
            state.visible = visible;
            webview.set_focused(visible && !state.suspended);
        }
        EngineCommand::Suspend => {
            let mut state = shared.borrow_mut();
            state.suspended = true;
            webview.set_focused(false);
        }
        EngineCommand::Resume => {
            let mut state = shared.borrow_mut();
            state.suspended = false;
            webview.set_focused(state.visible);
        }
        EngineCommand::Shutdown | EngineCommand::Wake => {}
    }
}

/// Document changed (fresh load or history travel): drop DOM targets, scroll
/// and content extent. Session history does not reliably restore scroll,
/// and stale anything misplaces every outline (observed in testing).
fn clear_tree(state: &mut Shared) {
    if !state.dom.is_empty() {
        state.dom.clear();
        state.dom_uid.clear();
        state.last_emitted = Vec::new();
        state.handle.emit(EngineEvent::A11yTree(Vec::new()));
    }
    state.content = None;
    // A new document gets fresh DOM ids (the in-page WeakMap resets), a
    // fresh poll epoch (in-flight answers from the old page are dropped),
    // a fresh visibility epoch (no verdicts carry across documents), and
    // a boot round-trip (idempotent install + snapshot).
    state.dom_seq = state.dom_seq.wrapping_add(1);
    state.dom_epoch = state.dom_epoch.wrapping_add(1);
    state.dom_in_flight = false;
    state.dom_in_flight_at = None;
    state.dom_boot = true;
    state.dom_dirty = false;
    state.dom_dirty_at = None;
    state.dom_ping = false;
    state.last_pull_at = None;
    state.last_pull_rtt_ms = 0;
    // Truth bound resets with the document (see `scroll_max`).
    state.scroll_max = (0.0, 0.0);
    if state.scroll != (0.0, 0.0) {
        state.scroll = (0.0, 0.0);
        state.last_sent_scroll = (0.0, 0.0);
        state
            .handle
            .emit(EngineEvent::ScrollOffset { x: 0.0, y: 0.0 });
    }
}

/// Viewport-space snapshot of a sorted focusable list: rects translated by
/// the tracked scroll offset (Servo reports document-relative bounds and
/// never re-emits them on scroll). Order-preserving, so the walk order
/// (sorted by id) survives for stable diffing.
fn translate_tree(mut items: Vec<Focusable>, scroll: (f32, f32)) -> Vec<Focusable> {
    for o in items.iter_mut() {
        o.x -= scroll.0;
        o.y -= scroll.1;
    }
    items
}

/// Document identity for history comparison: URL minus fragment.
/// Same-document traffic (fragment jumps, some pushState) must not clear
/// the tree; anything else is a new document.
fn doc_key(url: &str) -> &str {
    url.split('#').next().unwrap_or(url)
}

/// Clamp a tracked offset to the physically possible range: pages never sit
/// above/left of origin, nor past extent-viewport. `None` extent (no
/// bounded node seen yet) means no upper bound.
fn clamp_scroll(scroll: (f32, f32), content: Option<(f32, f32)>) -> (f32, f32) {
    let (max_x, max_y) = match content {
        Some((w, h)) => (
            (w - VIEWPORT_WIDTH as f32).max(0.0),
            (h - VIEWPORT_HEIGHT as f32).max(0.0),
        ),
        None => (f32::INFINITY, f32::INFINITY),
    };
    (scroll.0.clamp(0.0, max_x), scroll.1.clamp(0.0, max_y))
}

/// Quiet period after a dirty ping before the pull fires (lets the page
/// reflow after the mutation burst that triggered the ping). Tuned low:
/// mid-reflow pulls self-correct via the next ping.
const DOM_SETTLE: Duration = Duration::from_millis(250);

/// Pull the page for its actionable elements (see `browser::dom`). Purely
/// event-driven — no timers: fires for a pending boot (fresh document or
/// load settle) or a settled dirty ping from the in-page observers
/// (console/fetch/sendBeacon, ORed into one flag). Skipped while suspended
/// or with a pull in flight (5s timeout unsticks it).
fn maybe_pull_dom(webview: &WebView, shared: &Rc<RefCell<Shared>>) {
    let now = Instant::now();
    let fire = {
        let mut state = shared.borrow_mut();
        if state.suspended {
            return;
        }
        if let Some(t) = state.dom_in_flight_at
            && now.saturating_duration_since(t) >= DOM_TIMEOUT
        {
            state.dom_in_flight = false;
            state.dom_in_flight_at = None;
        }
        if state.dom_in_flight {
            return;
        }
        let boot = state.dom_boot;
        let settled = state.dom_dirty
            && state
                .dom_dirty_at
                .is_some_and(|t| now.saturating_duration_since(t) >= DOM_SETTLE);
        if !boot && !settled {
            return;
        }
        // Adaptive cooldown: never pull sooner than the last round-trip
        // took (300ms floor). Boot bypasses it — a fresh document needs
        // its first snapshot promptly.
        if !boot && let Some(last) = state.last_pull_at {
            let gap = DOM_MIN_PULL_GAP.max(Duration::from_millis(state.last_pull_rtt_ms));
            if now.saturating_duration_since(last) < gap {
                return;
            }
        }
        // Reserve the sequence first so a racing callback can't interleave.
        state.dom_seq = state.dom_seq.wrapping_add(1);
        let seq = state.dom_seq;
        state.dom_in_flight = true;
        state.dom_in_flight_at = Some(now);
        state.dom_boot = false;
        state.dom_dirty = false;
        state.dom_dirty_at = None;
        state.last_pull_at = Some(now);
        let pinged = std::mem::take(&mut state.dom_ping);
        let center = state.last_mouse;
        state.last_cull_center = center;
        let epoch = state.dom_epoch;
        let script = if boot {
            dom_boot_js(center, epoch)
        } else {
            dom_query_js(center, epoch)
        };
        Some((seq, state.dom_tx.clone(), script, boot, pinged, now, epoch))
    };
    let Some((seq, tx, script, boot, pinged, dispatched, epoch)) = fire else {
        return;
    };
    let src = if boot {
        "boot"
    } else if pinged {
        "B"
    } else {
        "gesture"
    };
    crate::platform::app_log::info(format!("servo-dom: pull seq={seq} src={src} ep={epoch}"));
    webview.evaluate_javascript(script, move |result| {
        let _ = tx.send((seq, dispatched, result.map_err(|e| format!("{e:?}"))));
    });
}

/// Drain completed DOM queries: parse, merge, re-emit through the normal
/// diff path (so the UI, cursor, and debug overlay see every update).
fn drain_dom_results(shared: &Rc<RefCell<Shared>>) {
    loop {
        let msg = shared
            .borrow_mut()
            .dom_rx
            .as_mut()
            .and_then(|rx| rx.try_recv().ok());
        let Some((seq, dispatched, result)) = msg else {
            return;
        };
        let mut state = shared.borrow_mut();
        if seq != state.dom_seq {
            continue;
        }
        state.dom_in_flight = false;
        state.dom_in_flight_at = None;
        let rtt = dispatched.elapsed().as_millis();
        state.last_pull_rtt_ms = rtt.min(u64::MAX as u128) as u64;
        // A ping that landed after this pull was dispatched carries newer
        // scroll truth (already applied by its handler): re-running the
        // snapshot's older truth backwards over it is the overscroll jitter.
        let skip_rebase = state.ping_at.is_some_and(|t| t > dispatched);
        match result {
            Ok(value) => {
                ingest_dom(&mut state, &value, rtt, skip_rebase);
                emit_tree(&mut state);
            }
            Err(err) => {
                crate::platform::app_log::info(format!(
                    "servo-dom: query failed: {err} rtt={rtt}ms"
                ));
            }
        }
    }
}

/// Record a sent input event for outcome correlation. Bounded: moves stream
/// at pad-tick rate, so the oldest entry drops when full (its outcome just
/// goes unmatched — fine at that cadence).
fn track_input(shared: &Rc<RefCell<Shared>>, id: InputEventId, kind: &'static str) {
    let mut state = shared.borrow_mut();
    if state.pending_inputs.len() >= 64 {
        state.pending_inputs.pop_front();
    }
    state.pending_inputs.push_back((id, kind, Instant::now()));
}

/// Drain one-shot event-loop probe results and drive its two phases:
/// READ dispatch 600ms after the arm (lets the 100ms timer and one rAF
/// fire), then a 2s bound on the READ round-trip.
fn poll_probe(webview: &WebView, shared: &Rc<RefCell<Shared>>) {
    // Phase 1: time to dispatch the READ.
    let dispatch_read = {
        let mut state = shared.borrow_mut();
        match (state.probe_armed_at, state.probe_rx.is_some()) {
            (Some(armed), false) if armed.elapsed() >= Duration::from_millis(600) => {
                let (tx, rx) = mpsc::channel();
                state.probe_rx = Some(rx);
                state.probe_armed_at = None;
                state.probe_deadline = Some(Instant::now() + Duration::from_secs(2));
                Some(tx)
            }
            _ => None,
        }
    };
    if let Some(tx) = dispatch_read {
        webview.evaluate_javascript(PROBE_READ_JS.to_string(), move |result| {
            let _ = tx.send(result.map_err(|e| format!("{e:?}")));
        });
        return;
    }
    // Phase 2: READ result or deadline expiry.
    let msg = shared
        .borrow_mut()
        .probe_rx
        .as_mut()
        .and_then(|rx| rx.try_recv().ok());
    if let Some(result) = msg {
        let mut state = shared.borrow_mut();
        state.probe_rx = None;
        state.probe_deadline = None;
        match result {
            Ok(value) => match parse_loop_probe(&value) {
                Some([timer, raf, io]) => {
                    crate::platform::app_log::info(format!(
                        "servo-loop: probe timer={timer} raf={raf} io={io}"
                    ));
                    if !raf {
                        crate::platform::app_log::warn(
                            "servo-loop: rAF starved — frame-gated page JS is stalled",
                        );
                    }
                }
                None => {
                    crate::platform::app_log::info("servo-loop: probe unread (arm never ran)");
                }
            },
            Err(err) => {
                crate::platform::app_log::info(format!("servo-loop: probe failed: {err}"));
            }
        }
        return;
    }
    let expired = shared
        .borrow()
        .probe_deadline
        .is_some_and(|t| Instant::now() >= t);
    if expired {
        let mut state = shared.borrow_mut();
        state.probe_rx = None;
        state.probe_deadline = None;
        crate::platform::app_log::warn(
            "servo-loop: probe unread after 700ms — script thread unresponsive",
        );
    }
}

/// Trusted synthetic mouse-down; the matching Up is armed on
/// [`Shared::pending_click_up`] for the next loop tick (see `run`), so
/// every press has real duration instead of a zero-width Down+Up pair.
fn send_synthetic_down(webview: &WebView, shared: &Rc<RefCell<Shared>>, x: f32, y: f32) {
    let point = WebViewPoint::Device(DevicePoint::new(x, y));
    let id = webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
        MouseButtonAction::Down,
        MouseButton::Primary,
        point,
    )));
    track_input(shared, id, "click");
    let mut state = shared.borrow_mut();
    state.pending_click_up = Some((point, state.spin_seq));
}

/// Complete any in-flight synthetic press immediately (used when a new
/// click arrives before the tick flush, and to keep rapid clicks paired).
fn flush_pending_up(webview: &WebView, shared: &Rc<RefCell<Shared>>) {
    let pending = shared.borrow_mut().pending_click_up.take();
    if let Some((point, _)) = pending {
        let id = webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
            MouseButtonAction::Up,
            MouseButton::Primary,
            point,
        )));
        track_input(shared, id, "click");
    }
}

/// Merge one DOM snapshot into the live set. Rects arrive viewport-relative;
/// document space is viewport plus the tracked offset (pulls fire on
/// settle, so measure and offset agree). The tracked offset then rebases by
/// the snapshot truth's *delta from current* — never to an absolute value:
/// an absolute rebase would clobber commanded scrolls that landed while the
/// round-trip was in flight, yanking every outline backward (the overscroll
/// jitter). A stale truth can still win one round when commands interleave
/// the flight window, but those same commands already marked dirty, so the
/// next pull corrects it. Absent nodes accumulate misses and drop at
/// [`DOM_MISSED_LIMIT`] (re-render flicker reads as one miss, real removals
/// as two in a row). `skip_rebase` arrives from the drain path when a ping
/// landed after this pull was dispatched: that ping's truth is newer and
/// already applied, so the snapshot's older truth must not yank outlines
/// backwards.
fn ingest_dom(state: &mut Shared, value: &JSValue, rtt_ms: u128, skip_rebase: bool) {
    use std::collections::HashSet;
    let snap = parse_dom_snapshot(value);
    // The snapshot's own truth counts as observed even when the rebase is
    // skipped (a newer ping already applied): it is still a position the
    // page proved reachable, so it feeds the commanded-lead bound.
    note_scroll_truth(&mut state.scroll_max, snap.scroll);
    // Tracked offset before the snapshot truth is applied: logged alongside
    // the snapshot's own truth so stale-box reports can separate "snapshot
    // measured old truth" from "rebase yanked backwards".
    let pre_scroll = state.scroll;
    // Truth delta vs current: heals unapplied commands (edge clamping the
    // page ignored) and history-restored offsets alike. Origin-pinned;
    // never upper-clamped (extent can lag truth). Skipped when a ping landed
    // after this pull was dispatched — that truth is newer and already
    // applied; re-applying the snapshot's older truth would yank every
    // outline backwards (the overscroll jitter).
    if !skip_rebase {
        let dx = (snap.scroll.0 - state.scroll.0).max(-state.scroll.0);
        let dy = (snap.scroll.1 - state.scroll.1).max(-state.scroll.1);
        if dx != 0.0 || dy != 0.0 {
            rebase_scroll(state, (state.scroll.0 + dx, state.scroll.1 + dy));
        }
    }
    // Document dims bound scroll clamping (the live set only spans
    // viewport+margin after culling, so entry corners can't). Grow-only per
    // document, reset by `clear_tree`.
    let (dw, dh) = (snap.doc.0.max(0.0), snap.doc.1.max(0.0));
    if dw > 0.0 || dh > 0.0 {
        state.content = Some(match state.content {
            Some(prev) => (prev.0.max(dw), prev.1.max(dh)),
            None => (dw, dh),
        });
    }
    let (total, culled) = (snap.total, snap.culled);
    let skipped = snap.skipped;
    let (box_anchors, box_kids, box_wraps, box_empty, forced, hid, clip, timings) = (
        snap.box_anchors,
        snap.boxless_with_kids,
        snap.box_anchor_wraps,
        snap.box_anchor_empty,
        snap.forced_boxes,
        snap.hidden_subtree,
        snap.clipped_away,
        snap.timings,
    );
    let mut seen = HashSet::with_capacity(snap.nodes.len() + snap.culled_ids.len());
    let mut fresh = 0usize;
    let mut retained = 0usize;
    // Translate by the snapshot's own scroll truth (read in-page in the
    // same pass as the rects), NOT the tracked offset: when the rebase is
    // skipped (a ping landed mid-flight with newer truth), the tracked
    // offset is ahead of what these rects were measured against, and
    // translating by it misplaces every merged box by the difference —
    // the stale-boxes-that-never-heal from the first-scroll flag (that
    // pull merged scroll-0 rects at offset 814). In the apply case both
    // agree (rebase just set tracked := snapshot), so this is identical.
    let (sx, sy) = snap.scroll;
    for r in snap.nodes {
        seen.insert(r.id);
        let kind = dom_kind(&r.tag, &r.role, &r.typ);
        if let Some(n) = state.dom.get_mut(&r.id) {
            n.kind = kind;
            n.tag = r.tag.clone();
            n.role = r.role.clone();
            n.x = r.x + sx;
            n.y = r.y + sy;
            n.w = r.w;
            n.h = r.h;
            n.missed = 0;
        } else {
            // Fresh node: assign a UI id from the document counter.
            let id = state.next_id;
            state.next_id += 1;
            state.dom_uid.insert(r.id, id);
            state.dom.insert(
                r.id,
                DomEntry {
                    kind,
                    tag: r.tag.clone(),
                    role: r.role.clone(),
                    x: r.x + sx,
                    y: r.y + sy,
                    w: r.w,
                    h: r.h,
                    missed: 0,
                },
            );
            fresh += 1;
            if state.ax_debug {
                crate::platform::app_log::info(format!(
                    "servo-dom map #{id} <= js#{} {kind:?} [{}]{} {}",
                    r.id,
                    r.tag,
                    if r.role.is_empty() {
                        String::new()
                    } else {
                        format!(" role={}", r.role)
                    },
                    r.text.chars().take(40).collect::<String>()
                ));
            }
        }
    }
    // Cursor-culled but seen: hold position (translated by the tracked
    // scroll, exact for window scrolls) and reset the miss counter. Without
    // this the set churns fresh/drop every pull while scrolling and the
    // cursor re-docks constantly; with it, ids survive range exits and only
    // truly-removed DOM drops.
    for jsid in &snap.culled_ids {
        seen.insert(*jsid);
        if let Some(n) = state.dom.get_mut(jsid) {
            n.missed = 0;
            retained += 1;
        }
    }
    // Missed++ for the absent; drop the long-gone; prune dead uid mappings.
    // Bound the map so a pathological page can't grow it without limit.
    let mut dropped = 0usize;
    state.dom.retain(|jsid, n| {
        if seen.contains(jsid) {
            true
        } else {
            n.missed += 1;
            let keep = n.missed < DOM_MISSED_LIMIT;
            if !keep {
                dropped += 1;
            }
            keep
        }
    });
    state.dom_uid.retain(|jsid, _| state.dom.contains_key(jsid));
    if state.dom.len() > DOM_MAX_TARGETS {
        crate::platform::app_log::info(format!(
            "servo-dom: over cap ({}), clearing and re-learning",
            state.dom.len()
        ));
        state.dom.clear();
        state.dom_uid.clear();
    }
    crate::platform::app_log::info(format!(
        "servo-dom: ingested fresh={fresh} dropped={dropped} retained={retained} live={} total={total} culled={culled} skip={skipped:?} boxA={box_anchors} boxKid={box_kids} boxAWrap={box_wraps} boxAEmpty={box_empty} forced={forced} hid={hid} clip={clip} t={timings:?} rtt={rtt_ms}ms rb={} snap=({:.0},{:.0}) trk=({:.0},{:.0})",
        state.dom.len(),
        if skip_rebase { "skip" } else { "apply" },
        snap.scroll.0,
        snap.scroll.1,
        pre_scroll.0,
        pre_scroll.1,
    ));
}

/// Rebase the tracked offset to the page's reported scroll. Unlike
/// [`set_scroll`] this deliberately skips the upper clamp: our extent can
/// lag the page (unseen far corners), and truth from the page must win —
/// clamping it would reintroduce the very drift this heals. Origin-pinned
/// (negative scroll is never real) and reported when it moves the UI value.
fn rebase_scroll(state: &mut Shared, scroll: (f32, f32)) {
    let rebased = (scroll.0.max(0.0), scroll.1.max(0.0));
    if state.scroll == rebased {
        return;
    }
    state.scroll = rebased;
    if state.last_sent_scroll == rebased {
        return;
    }
    state.last_sent_scroll = rebased;
    state.handle.emit(EngineEvent::ScrollOffset {
        x: rebased.0,
        y: rebased.1,
    });
}

/// Parse the window-truth coordinates off a dirty-ping URL (`?wx=&wy=`).
/// `None` when absent or malformed — the ping still dirties, it just
/// carries no correction.
fn parse_ping_scroll(url: &str) -> Option<(f32, f32)> {
    let parsed = url.parse::<url::Url>().ok()?;
    let mut wx = None;
    let mut wy = None;
    for (k, v) in parsed.query_pairs() {
        match k.as_ref() {
            "wx" => wx = v.parse::<f32>().ok(),
            "wy" => wy = v.parse::<f32>().ok(),
            _ => {}
        }
    }
    Some((wx?, wy?))
}

/// Record observed scroll truth toward the per-document maximum (see
/// `Shared::scroll_max`). Every observation — ping `wx/wy`, snapshot `sx/sy`
/// — is a position the page proved reachable, so the running maximum is
/// always a safe upper bound for commanded accumulation.
fn note_scroll_truth(scroll_max: &mut (f32, f32), truth: (f32, f32)) {
    scroll_max.0 = scroll_max.0.max(truth.0);
    scroll_max.1 = scroll_max.1.max(truth.1);
}

/// Bound a commanded scroll offset by the highest truth this document ever
/// reported, plus one viewport of exploration slop. At the page end the
/// truth pins and translations stop instead of sailing past into the void.
/// (Extent alone can't do this — virtualized filler overstates it.)
fn bound_commanded(scroll: (f32, f32), scroll_max: (f32, f32)) -> (f32, f32) {
    (
        scroll.0.min(scroll_max.0 + VIEWPORT_WIDTH as f32),
        scroll.1.min(scroll_max.1 + VIEWPORT_HEIGHT as f32),
    )
}

/// Update the tracked scroll offset and report it when it moves, so the UI
/// overlay can correlate cursor against targets.
fn set_scroll(state: &mut Shared, scroll: (f32, f32)) {
    // Commanded lead is bounded by observed truth (see `bound_commanded`).
    let bounded = bound_commanded(scroll, state.scroll_max);
    let clamped = clamp_scroll(bounded, state.content);
    if state.scroll == clamped {
        return;
    }
    state.scroll = clamped;
    state.last_sent_scroll = clamped;
    state.handle.emit(EngineEvent::ScrollOffset {
        x: clamped.0,
        y: clamped.1,
    });
}

/// Diff + publish the translated tree. Logs only when the set size changes
/// so scroll ticks (which move rects without adding nodes) stay silent.
fn emit_tree(shared: &mut Shared) {
    let (focusables, extent) = collect_focusables(shared);
    // Extent only grows per document (reset by `clear_tree`).
    shared.content = Some(match shared.content {
        Some(prev) => (prev.0.max(extent.0), prev.1.max(extent.1)),
        None => extent,
    });
    let fresh = translate_tree(focusables, shared.scroll);
    if fresh == shared.last_emitted {
        return;
    }
    if fresh.len() != shared.last_emitted.len() {
        let sample = fresh
            .first()
            .map(|o| {
                format!(
                    " first=({},{}) {}x{}",
                    o.x as i32, o.y as i32, o.w as i32, o.h as i32
                )
            })
            .unwrap_or_default();
        crate::platform::app_log::info(format!(
            "servo-engine: dom focusables={}{sample}",
            fresh.len()
        ));
    }
    shared.last_emitted = fresh.clone();
    shared.handle.emit(EngineEvent::A11yTree(fresh));
}

/// Fail loads that never complete so the UI never spins forever.
fn check_load_timeout(shared: &Rc<RefCell<Shared>>) {
    let mut state = shared.borrow_mut();
    let Some(started) = state.load_started_at else {
        return;
    };
    if started.elapsed() < LOAD_TIMEOUT {
        return;
    }
    state.load_started_at = None;
    let generation = state.generation;
    crate::platform::app_log::warn(format!(
        "servo-engine: load timed out after {}s",
        LOAD_TIMEOUT.as_secs()
    ));
    state.handle.emit(EngineEvent::LoadFinished {
        generation,
        result: Err("load timed out".to_string()),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: u64, x: f32, y: f32) -> Focusable {
        Focusable {
            id,
            x,
            y,
            w: 100.0,
            h: 30.0,
            kind: FocusKind::Link,
        }
    }

    #[test]
    fn doc_key_strips_fragments_only() {
        assert_eq!(doc_key("https://google.com/"), "https://google.com/");
        assert_eq!(doc_key("https://x.test/p#frag"), "https://x.test/p");
        assert_eq!(doc_key("https://x.test/p?q=1#frag"), "https://x.test/p?q=1");
        assert_eq!(doc_key("https://x.test/p?q=1"), "https://x.test/p?q=1");
    }

    #[test]
    fn clamp_scroll_pins_to_possible_range() {
        // No extent known yet: anything non-negative passes.
        assert_eq!(clamp_scroll((100.0, 200.0), None), (100.0, 200.0));
        // Negative is never real (pages sit at/after origin).
        assert_eq!(clamp_scroll((-57.0, -74.0), None), (0.0, 0.0));
        // Known 800x2481 document on an 800x600 viewport.
        let content = Some((800.0, 2481.0));
        assert_eq!(clamp_scroll((0.0, 2573.0), content), (0.0, 1881.0));
        assert_eq!(clamp_scroll((-225.0, 100.0), content), (0.0, 100.0));
        assert_eq!(clamp_scroll((0.0, 1881.0), content), (0.0, 1881.0));
    }

    #[test]
    fn scroll_truth_max_grows_monotonically() {
        let mut max = (0.0, 0.0);
        note_scroll_truth(&mut max, (0.0, 500.0));
        assert_eq!(max, (0.0, 500.0));
        // scrolling back up never lowers the proven-reachable bound.
        note_scroll_truth(&mut max, (0.0, 200.0));
        assert_eq!(max, (0.0, 500.0));
        // infinite scroll grows it again.
        note_scroll_truth(&mut max, (30.0, 900.0));
        assert_eq!(max, (30.0, 900.0));
    }

    #[test]
    fn commanded_lead_bounded_by_truth_plus_viewport() {
        // Within one viewport of proven truth: untouched.
        assert_eq!(bound_commanded((0.0, 1500.0), (0.0, 1000.0)), (0.0, 1500.0));
        // Past the end the page proved: pinned to truth + viewport.
        assert_eq!(bound_commanded((0.0, 5000.0), (0.0, 1000.0)), (0.0, 1600.0));
        assert_eq!(bound_commanded((5000.0, 0.0), (0.0, 0.0)), (800.0, 0.0));
        // Fresh document (no truth yet): one viewport of exploration.
        assert_eq!(bound_commanded((0.0, 599.0), (0.0, 0.0)), (0.0, 599.0));
        assert_eq!(bound_commanded((0.0, 601.0), (0.0, 0.0)), (0.0, 600.0));
    }

    #[test]
    fn translate_shifts_by_scroll_and_preserves_order() {
        let items = vec![item(2, 100.0, 800.0), item(1, 0.0, 0.0)];
        let out = translate_tree(items, (0.0, 240.0));
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].x, out[0].y), (100.0, 560.0));
        assert_eq!((out[1].x, out[1].y), (0.0, -240.0));
    }

    #[test]
    fn ping_scroll_parses_window_truth() {
        assert_eq!(
            parse_ping_scroll("http://sdsc.invalid/__SDSC_DOM_DIRTY?wx=0&wy=1240"),
            Some((0.0, 1240.0))
        );
        // Missing coords: no correction, ping still dirties elsewhere.
        assert_eq!(
            parse_ping_scroll("http://sdsc.invalid/__SDSC_DOM_DIRTY"),
            None
        );
        assert_eq!(
            parse_ping_scroll("http://sdsc.invalid/__SDSC_DOM_DIRTY?wx=abc&wy=1"),
            None
        );
        // Source tags ride alongside without disturbing truth parsing.
        assert_eq!(
            parse_ping_scroll("http://sdsc.invalid/__SDSC_DOM_DIRTY?wx=3&wy=44&ev=mc"),
            Some((3.0, 44.0))
        );
    }
}
