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
    CreateNewWebViewRequest, DevicePixel, DevicePoint, DeviceVector2D, EventLoopWaker, LoadStatus,
    Opts, RenderingContext, Scroll, Servo, ServoBuilder, SoftwareRenderingContext,
    UserContentManager, UserScript, WebView, WebViewBuilder, WebViewDelegate, WebViewId,
    WebViewPoint, WebViewVector,
};

use crate::browser::cursor::{FocusKind, Focusable};
use crate::browser::dom::{
    DOM_DIRTY_MARKER, DOM_INSTALL_JS, DOM_PING_URL, PROBE_ARM_JS, PROBE_READ_JS, dom_boot_js,
    dom_kind, dom_query_js, parse_dom_snapshot, parse_loop_probe,
};
use crate::browser::engine::{
    EngineActivity, EngineCommand, EngineEvent, EngineHandle, TabId, TabKey,
};

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

/// Per-tab worker state. Every Servo type here is `!Send` by design, so all
/// of Servo lives on this one thread for the process lifetime — the same
/// ownership shape as the HID worker (blocking I/O off the UI executor) and
/// the art worker. One [`TabState`] per tab; the truly global pieces (GL
/// context, size, visibility, suspension, the active tab) live in
/// [`Global`]. The iced shell talks in [`EngineCommand`]s and receives
/// [`EngineEvent`]s; nothing Servo ever touches the UI thread.
struct TabState {
    /// Which tab this state belongs to (stamped on every emitted event).
    tab: TabId,
    handle: EngineHandle,
    generation: u64,
    load_started_at: Option<Instant>,
    /// Last list sent to the UI (diffed to avoid spamming identical trees).
    last_emitted: Vec<Focusable>,
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
    /// Last focused UI id emitted via `FocusChanged` (None = nothing or
    /// unknown). Compared per ingest so autofocus / JS `.focus()` dock the
    /// cursor exactly once per change. Reset per document with the maps.
    last_focus: Option<u64>,
    /// Coarse engine phase for the status strip; emitted on change only.
    activity: EngineActivity,
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
    /// Wall time of the latest scroll-affecting activity (commanded scroll
    /// or a scroll ping). Pulls wait for this to go quiet: a snapshot's
    /// synchronous geometry pass blocks the page's script thread for up to
    /// a second, so scroll input queued behind it lands as one visible
    /// jump — and its older scroll reading then yanks the tracked offset
    /// back (the 0 <-> N blip).
    last_scroll_at: Option<Instant>,
    /// Wall time of the latest user-commanded scroll (`ScrollBy`). Unlike
    /// `last_scroll_at` (which scroll pings also refresh), this proves a
    /// gesture is still under our control, so the starvation cap must not
    /// force a pull through it.
    last_scroll_cmd_at: Option<Instant>,
    /// When the current continuous scroll period began. The starvation cap
    /// measures this, not `last_pull_at`: a pull from before the gesture
    /// must not make a 1s-old scroll look like 3s of continuous motion.
    scroll_hot_since: Option<Instant>,
    /// Latest page truth seen while a gesture was hot. Held instead of
    /// rebased: the script-thread scroll lags the visual scroll during
    /// input, so applying it mid-gesture rubber-bands the overlay. Applied
    /// once by `flush_scroll_truth` after the gesture goes quiet.
    pending_scroll_truth: Option<(f32, f32)>,
    /// A user action (click, key, cursor roam) wants fresh targets. Those
    /// pulls skip the idle breather so interaction stays responsive; the
    /// page's own mutation pings do not.
    user_dirty: bool,
    /// Wall time of the last frame written to the slot, for the SDSC_AX_DEBUG
    /// frame trace (cadence + content hash vs tracked scroll).
    last_frame_at: Option<Instant>,
    /// `notify_new_frame_ready` asked for a paint. The worker loop paints on
    /// its next turn instead of inside the callback: painting there can race
    /// the WebRender scene build and return the pre-scroll frame (the page
    /// visibly jumps to the top).
    needs_paint: bool,
    /// Content hash of the last frame painted at scroll 0. A frame at a
    /// non-zero scroll that matches it is a stale top-of-page frame; dropping
    /// it keeps the previous texture on screen instead of jumping.
    top_frame_hash: Option<u64>,
    /// Hash + tracked scroll of the last frame actually written. A new frame
    /// whose hash matches it after the scroll moved is a stale read from the
    /// previous scroll position; retry the paint/read once before dropping.
    last_frame: Option<(u64, (f32, f32))>,
    /// Per-window document token from the install script. A mismatch means
    /// the document was replaced without a Navigate/history event (same-URL
    /// reload); that ingest resets the tree. A spurious `Complete` (Servo
    /// re-reporting a load when the page injects a head resource) keeps the
    /// token and must not invalidate anything.
    doc_token: Option<String>,
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
    /// Full current URL (fragments kept): identity key for per-URL caches
    /// like favicons, where the stripped doc key would miss.
    last_full_url: String,
    /// Verbose per-target DOM diagnostics (`SDSC_AX_DEBUG=1`): js id, kind,
    /// and rect for every freshly mapped target.
    ax_debug: bool,
    /// Debug-only storage spike probe: set on load-complete, consumed when
    /// the one-shot census eval dispatches; the reply channel below.
    #[cfg(debug_assertions)]
    storage_probe_pending: bool,
    #[cfg(debug_assertions)]
    storage_probe_rx: Option<mpsc::Receiver<Result<JSValue, String>>>,
}

impl TabState {
    /// Fresh per-tab state: a new document boot is pending (the install +
    /// snapshot round-trip fires on the first pull), matching what a fresh
    /// `Navigate` used to set up via `clear_tree`.
    fn fresh(tab: TabId, handle: EngineHandle, ax_debug: bool) -> Self {
        let (dom_tx, dom_rx) = mpsc::channel();
        Self {
            tab,
            handle,
            generation: 0,
            load_started_at: None,
            last_emitted: Vec::new(),
            scroll: (0.0, 0.0),
            scroll_max: (0.0, 0.0),
            next_id: 0,
            dom: HashMap::new(),
            dom_uid: HashMap::new(),
            dom_seq: 0,
            last_focus: None,
            activity: EngineActivity::Idle,
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
            last_scroll_at: None,
            last_scroll_cmd_at: None,
            scroll_hot_since: None,
            pending_scroll_truth: None,
            user_dirty: false,
            last_frame_at: None,
            needs_paint: false,
            top_frame_hash: None,
            last_frame: None,
            doc_token: None,
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
            last_sent_scroll: (0.0, 0.0),
            content: None,
            last_url: None,
            last_full_url: String::new(),
            ax_debug,
            #[cfg(debug_assertions)]
            storage_probe_pending: false,
            #[cfg(debug_assertions)]
            storage_probe_rx: None,
        }
    }
}

/// Worker-global state: everything shared across tabs. One GL context, one
/// size, one suspension/visibility pair for the whole engine; per-tab
/// everything else. Follows the servoshell shape (one shared delegate +
/// context, one `WebView` per tab), minus its windowing.
struct Global {
    ctx: Rc<SoftwareRenderingContext>,
    size: (u32, u32),
    handle: EngineHandle,
    user_content: Rc<UserContentManager>,
    visible: bool,
    suspended: bool,
    /// Tab whose frames/DOM/input are live. All delegate paint/emit/input
    /// gating keys off this.
    active: TabId,
    /// Engine-owned tab ids count down from [`TabId::ENGINE_TOP`] (popups);
    /// UI-owned ids count up from [`TabId::BOOT`]. Disjoint, no handshake.
    engine_next: u64,
    tabs: HashMap<TabId, TabEntry>,
    by_view: HashMap<WebViewId, TabId>,
    logged_first_frame: bool,
    ax_debug: bool,
    /// Shared delegate handle for engine-side tab builds (`OpenTab` can't
    /// reach a parent view; popups use their parent's instead). Set once at
    /// boot — the `Delegate` needs the `Global` to exist first.
    delegate_source: Option<Rc<dyn WebViewDelegate>>,
}

/// One tab's view handle plus its state. Dropping the `WebView` closes the
/// view (`CloseWebView`), so `CloseTab`/`notify_closed` just remove the
/// entry and drop.
#[derive(Clone)]
struct TabEntry {
    webview: WebView,
    state: Rc<RefCell<TabState>>,
    /// Shared with the UI: the worker writes each painted frame here; the
    /// stable-texture shader primitive uploads unseen generations.
    frame: std::sync::Arc<crate::browser::frame_slot::FrameSlot>,
}

struct Delegate {
    global: Rc<RefCell<Global>>,
}

impl Delegate {
    /// Resolve a delegate callback's originating view to its tab. `None`
    /// for closed-race views (their handle is gone from the maps).
    fn entry_for(&self, webview: &WebView) -> Option<TabEntry> {
        let global = self.global.borrow();
        let tab = global.by_view.get(&webview.id()).copied()?;
        global.tabs.get(&tab).cloned()
    }
}

impl WebViewDelegate for Delegate {
    fn notify_new_frame_ready(&self, webview: WebView) {
        // Active tab only: background tabs never paint (their frames would
        // read over the active tab's back buffer on the shared context).
        // Suspended/hidden: no frames at all, same as the old single-view
        // gate.
        let entry = {
            let global = self.global.borrow();
            if global.suspended || !global.visible {
                return;
            }
            let tab = match global.by_view.get(&webview.id()).copied() {
                Some(tab) if tab == global.active => tab,
                _ => return,
            };
            global.tabs.get(&tab).cloned()
        };
        let Some(entry) = entry else {
            return;
        };
        // Defer the actual paint to the worker loop's next turn. Painting
        // inside the callback can race WebRender's scene build and hand back
        // the pre-scroll frame (the page jumps to the top for one frame).
        entry.state.borrow_mut().needs_paint = true;
    }

    /// Popup `request_create_new` (`window.open()`, `target=_blank`): build
    /// the auxiliary view on the shared context (servoshell shape) and
    /// foreground it at once — a single-view product has no background-tab
    /// concept, and dropping the request is exactly the dead-click bug.
    fn request_create_new(&self, parent_webview: WebView, request: CreateNewWebViewRequest) {
        let mut global = self.global.borrow_mut();
        let tab = TabId(global.engine_next);
        global.engine_next = global.engine_next.wrapping_sub(1);
        let ctx = global.ctx.clone();
        let user_content = global.user_content.clone();
        let delegate: Rc<dyn WebViewDelegate> = parent_webview.delegate();
        let webview = request
            .builder(ctx)
            .user_content_manager(user_content)
            .delegate(delegate)
            .build();
        // New tab takes the stage: hide + unfocus the old active view, show
        // the new one. UI adopts on `TabOpened` (stash live, reset, set
        // active) — no `SetActiveTab` round-trip needed.
        let old_active = global.active;
        if let Some(old) = global.tabs.get(&old_active).cloned() {
            old.webview.hide();
            old.webview.set_focused(false);
        }
        webview.show();
        webview.set_focused(global.visible && !global.suspended);
        let view_id = webview.id();
        crate::platform::app_log::info(format!(
            "servo-engine: request_create_new -> tab{} view{view_id:?}",
            tab.0
        ));
        global.by_view.insert(view_id, tab);
        let tab_handle = global.handle.clone();
        let tab_ax = global.ax_debug;
        let frame = std::sync::Arc::new(crate::browser::frame_slot::FrameSlot::default());
        global.tabs.insert(
            tab,
            TabEntry {
                webview,
                state: Rc::new(RefCell::new(TabState::fresh(tab, tab_handle, tab_ax))),
                frame: std::sync::Arc::clone(&frame),
            },
        );
        global.active = tab;
        global
            .handle
            .emit(EngineEvent::TabOpened { tab, slot: frame });
    }

    fn notify_url_changed(&self, webview: WebView, url: url::Url) {
        let Some(entry) = self.entry_for(&webview) else {
            return;
        };
        // NOTE: deliberately does not touch `last_url` — history comparison
        // owns document identity, and url-changed fires first on
        // client-side navigations. Updating here would pre-empt the clear.
        // The full URL is safe to track (favicon identity only).
        let mut state = entry.state.borrow_mut();
        state.last_full_url = url.to_string();
        let tab = state.tab;
        state.handle.emit(EngineEvent::UrlChanged {
            tab,
            url: url.to_string(),
        });
    }

    fn notify_history_changed(&self, webview: WebView, entries: Vec<url::Url>, current: usize) {
        let Some(entry) = self.entry_for(&webview) else {
            return;
        };
        // Client-side navigation (link click, redirect, form submit, JS):
        // a new document arrives through no EngineCommand. Same-document
        // traffic (fragments) keeps the tree; anything else clears it.
        let Some(url) = entries.get(current) else {
            return;
        };
        let urls: Vec<String> = entries.iter().map(|u| u.as_str().to_string()).collect();
        let mut state = entry.state.borrow_mut();
        let key = doc_key(url.as_str());
        if state.last_url.as_deref() == Some(key) {
            return;
        }
        crate::platform::app_log::info(format!("servo-engine: history cleared for {key}"));
        state.last_url = Some(key.to_string());
        state.last_full_url = url.as_str().to_string();
        clear_tree(&mut state);
        let tab = state.tab;
        let handle = state.handle.clone();
        drop(state);
        handle.emit(EngineEvent::HistoryChanged {
            tab,
            entries: urls,
            current,
        });
    }

    fn notify_favicon_changed(&self, webview: WebView) {
        let Some(entry) = self.entry_for(&webview) else {
            return;
        };
        // Servo parses `<link rel=icon>` out of band; the frame rides along
        // on the webview. Keyed by full URL for the bookmark/history caches.
        let state = entry.state.borrow();
        let url = state.last_full_url.clone();
        let tab = state.tab;
        let handle = state.handle.clone();
        drop(state);
        let Some(icon) = webview.favicon() else {
            return;
        };
        if url.is_empty() {
            return;
        }
        let Some((width, height, rgba)) = favicon_rgba(&icon) else {
            return;
        };
        handle.emit(EngineEvent::FaviconChanged {
            tab,
            url,
            width,
            height,
            rgba,
        });
    }

    fn notify_page_title_changed(&self, webview: WebView, title: Option<String>) {
        let Some(entry) = self.entry_for(&webview) else {
            return;
        };
        let state = entry.state.borrow();
        let tab = state.tab;
        state.handle.emit(EngineEvent::TitleChanged { tab, title });
    }

    fn notify_load_status_changed(&self, webview: WebView, status: LoadStatus) {
        let Some(entry) = self.entry_for(&webview) else {
            return;
        };
        let mut state = entry.state.borrow_mut();
        match status {
            LoadStatus::Started | LoadStatus::HeadParsed => {
                crate::platform::app_log::info(format!(
                    "servo-engine: load {status:?} url={}",
                    state.last_url.as_deref().unwrap_or("?"),
                ));
                // Engine-created tabs never go through `Navigate`, so arm the
                // load-timeout clock here when nothing set it yet.
                if state.load_started_at.is_none() {
                    state.load_started_at = Some(Instant::now());
                }
                set_activity(&mut state, EngineActivity::Loading);
            }
            LoadStatus::Complete => {
                if let Some(started) = state.load_started_at {
                    crate::platform::app_log::info(format!(
                        "servo-engine: load Complete in {}ms",
                        started.elapsed().as_millis()
                    ));
                }
                state.load_started_at = None;
                // Fresh document settling in: boot (idempotent install +
                // snapshot) once rendering lands. Do NOT invalidate here:
                // Servo re-reports `Complete` when the page injects a head
                // resource without replacing the document (same URL, same
                // WeakMap). Real document swaps own invalidation —
                // `clear_tree` for navigations, the document token in
                // `ingest_dom` for same-URL reloads.
                state.dom_boot = true;
                set_activity(&mut state, EngineActivity::Initializing);
                // Debug-only storage spike diagnostic: one in-page
                // localStorage/sessionStorage/cookie census per document.
                #[cfg(debug_assertions)]
                {
                    state.storage_probe_pending = true;
                }
                let generation = state.generation;
                let tab = state.tab;
                state.handle.emit(EngineEvent::LoadFinished {
                    tab,
                    generation,
                    result: Ok(()),
                });
            }
            LoadStatus::Stopped => {
                // Failed/stopped loads previously sat silent until the 30s
                // timeout misreported them. Surface immediately.
                crate::platform::app_log::warn("servo-engine: load stopped before complete");
                state.load_started_at = None;
                set_activity(&mut state, EngineActivity::Idle);
                let generation = state.generation;
                let tab = state.tab;
                state.handle.emit(EngineEvent::LoadFinished {
                    tab,
                    generation,
                    result: Err("load stopped before complete".to_string()),
                });
            }
        }
    }

    fn notify_crashed(&self, webview: WebView, reason: String, backtrace: Option<String>) {
        crate::platform::app_log::warn(format!(
            "servo-engine: crashed reason={reason} backtrace={}",
            backtrace
                .unwrap_or_default()
                .chars()
                .take(500)
                .collect::<String>()
        ));
        let Some(entry) = self.entry_for(&webview) else {
            return;
        };
        let mut state = entry.state.borrow_mut();
        state.load_started_at = None;
        let generation = state.generation;
        let tab = state.tab;
        state.handle.emit(EngineEvent::LoadFinished {
            tab,
            generation,
            result: Err(format!("page crashed: {reason}")),
        });
    }

    /// A tab's page called `window.close()`: drop the view (its `Drop`
    /// closes it constellation-side) and tell the UI to adopt a neighbor.
    fn notify_closed(&self, webview: WebView) {
        let tab = {
            let mut global = self.global.borrow_mut();
            let Some(tab) = global.by_view.remove(&webview.id()) else {
                return;
            };
            global.tabs.remove(&tab);
            crate::platform::app_log::info(format!("servo-engine: tab{} closed by page", tab.0));
            tab
        };
        self.global
            .borrow()
            .handle
            .emit(EngineEvent::TabClosed { tab });
    }

    fn load_web_resource(&self, webview: WebView, load: servo::WebResourceLoad) {
        // Dirty-ping channel: the in-page ping POSTs to the marker URL.
        // Intercept with an empty 200 before anything leaves the machine.
        let url = load.request().url.to_string();
        if !url.contains(DOM_DIRTY_MARKER) {
            return;
        }
        let Some(entry) = self.entry_for(&webview) else {
            return;
        };
        // Ping taxonomy: `sc` is scroll-only, `mc` is a structural change,
        // `ma` is a class/style/hidden flip. Verdict invalidation is
        // localized in-page: the mutation observer drops only the mutated
        // element and its cached descendants, so unrelated class churn
        // (Google's sticky header) no longer forces a full ancestor
        // re-walk. Epoch bumps stay for input (hover can restyle without
        // a mutation) and document events.
        let ev = ping_event(&url);
        let truth = parse_ping_scroll(&url);
        let now = Instant::now();
        // Scroll-only pings carry no structural change: rects are
        // document-space, so a scroll never needs a geometry pass. Marking
        // them dirty used to start a full pull 250ms into every gesture; on
        // heavy pages that synchronous pass blocks the script thread for
        // ~1s, so the scrolls queued behind it land as one visible jump.
        // Real mutations (`mc`/`ma`) and unknown tags still re-query, but
        // only once the gesture has gone quiet (see `maybe_pull_dom`).
        let scroll_only = ev == "sc";
        {
            let mut state = entry.state.borrow_mut();
            state.ping_at = Some(now);
            if scroll_only {
                note_scroll_activity(&mut state, now);
            }
        }
        if !scroll_only {
            mark_dom_dirty(&entry.state);
        }
        // Window truth rides on the ping query: a nested-container scroll
        // leaves window.scroll unchanged, so correcting to it here instantly
        // reverts any commanded accumulation the inner scroller absorbed —
        // outlines stop tracking a scroll the page never made.
        if let Some(truth) = truth {
            let mut state = entry.state.borrow_mut();
            note_scroll_truth(&mut state.scroll_max, truth);
            let before = state.scroll;
            // `window.scrollY` is read on the script thread and lags the
            // visual scroll while input is being applied. Rebasing to it
            // mid-gesture rubber-bands the overlay (364 -> 255 -> 364 in
            // the 17:31 trace). Hold the latest truth; the worker loop
            // applies it once the gesture is quiet.
            let hot = state
                .last_scroll_at
                .is_some_and(|t| now.saturating_duration_since(t) < DOM_SCROLL_QUIET)
                || state
                    .last_scroll_cmd_at
                    .is_some_and(|t| now.saturating_duration_since(t) < DOM_SCROLL_QUIET);
            if hot {
                state.pending_scroll_truth = Some(truth);
            } else {
                state.pending_scroll_truth = None;
                if rebase_scroll(&mut state, truth) {
                    // Rects are document-space: rebasing only needs a cheap
                    // re-translate, never a DOM query. Without this the live
                    // set stays at the pre-settle offset until the next pull,
                    // so a D-pad jump just after a scroll scores against
                    // geometry that no longer matches the page.
                    emit_tree(&mut state);
                }
            }
            // Every scroll ping logs its truth (page-scroll trace); other
            // pings only when they move the tracked offset or are held.
            if scroll_only || state.scroll != before || hot {
                crate::platform::app_log::info(format!(
                    "servo-dom: ping tab{} ev={ev} truth=({:.0},{:.0}) tracked=({:.0},{:.0}) held={}",
                    state.tab.0,
                    truth.0,
                    truth.1,
                    before.0,
                    before.1,
                    u8::from(hot)
                ));
            }
        }
        let response = url
            .parse()
            .map(WebResourceResponse::new)
            .unwrap_or_else(|_| WebResourceResponse::new(DOM_PING_URL.parse().unwrap()));
        load.intercept(response);
    }

    fn notify_input_event_handled(
        &self,
        webview: WebView,
        event_id: InputEventId,
        result: InputEventResult,
    ) {
        // Correlate outcomes with sent inputs: DispatchFailed means the
        // engine dropped it; handled-but-unconsumed means dispatch reached
        // the page with no handler acting. DefaultPrevented-but-unconsumed
        // is the tell for jsaction-style links: the page ate the event
        // (killing native activation) and its JS never followed through.
        // Moves log only on failure (pad-tick rate would spam).
        let Some(entry) = self.entry_for(&webview) else {
            return;
        };
        let mut state = entry.state.borrow_mut();
        let pos = state
            .pending_inputs
            .iter()
            .position(|(id, _, _)| *id == event_id);
        let Some(idx) = pos else {
            return;
        };
        let (_, kind, sent_at) = state.pending_inputs.remove(idx).unwrap();
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
        while let Some((_, kind, at)) = state.pending_inputs.front() {
            if at.elapsed() < Duration::from_secs(5) {
                break;
            }
            crate::platform::app_log::warn(format!(
                "servo-input: {kind} swallowed (no outcome in 5s)"
            ));
            state.pending_inputs.pop_front();
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
fn mark_dom_dirty(shared: &Rc<RefCell<TabState>>) {
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
    /// Resolved link target (empty when none): hovered-link status.
    url: String,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    missed: u8,
}

/// Completed DOM query from the page: poll sequence, dispatch time (for
/// pull round-trip logging), the tracked scroll at dispatch (stale-truth
/// guard), plus the raw JS value or the evaluation error.
type DomIngest = (u64, Instant, (f32, f32), Result<JSValue, String>);

/// What to do with a completed snapshot's in-page scroll reading. The read
/// happens inside the query; commands or pings that land while it is in
/// flight make the tracked offset newer than that reading, and applying the
/// older value yanks the page overlay (the 0 <-> N blip).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Rebase {
    /// Nothing moved since dispatch: the snapshot's reading is current.
    Apply,
    /// A scroll ping landed after dispatch and already rebased to newer truth.
    SkipPing,
    /// The tracked offset moved after dispatch (commanded scroll, cursor
    /// reseed, another correction). The reading is stale by definition.
    SkipScroll,
}

impl Rebase {
    fn label(self) -> &'static str {
        match self {
            Self::Apply => "apply",
            Self::SkipPing => "skip:ping",
            Self::SkipScroll => "skip:scroll",
        }
    }
}

fn rebase_decision(
    ping_at: Option<Instant>,
    dispatched: Instant,
    live: (f32, f32),
    at_dispatch: (f32, f32),
) -> Rebase {
    if ping_at.is_some_and(|t| t > dispatched) {
        Rebase::SkipPing
    } else if live != at_dispatch {
        Rebase::SkipScroll
    } else {
        Rebase::Apply
    }
}

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
fn collect_focusables(shared: &mut TabState) -> (Vec<Focusable>, (f32, f32)) {
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
                url: if entry.url.is_empty() {
                    None
                } else {
                    Some(entry.url.clone())
                },
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
/// Spawn the engine worker with its first view on `boot_url` (the restored
/// active tab, or home). Booting straight onto it leaves no stale home load
/// to race the first restore-navigate and clobber the live session.
pub fn spawn_engine(boot_url: &str, initial_size: (u32, u32)) -> Option<EngineHandle> {
    let boot_url = boot_url.to_string();
    let initial_size = (initial_size.0.max(1), initial_size.1.max(1));
    let (handle, rx) = EngineHandle::channel();
    let worker = handle.clone();
    let wake_tx = handle.sender();
    let (ready_tx, ready_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("servo-engine".to_string())
        .stack_size(32 * 1024 * 1024)
        .spawn(move || run(rx, wake_tx, worker, boot_url, initial_size, ready_tx))
        .expect("spawn servo engine thread");
    // Deterministic GL-before-Vulkan barrier: iced (and its wgpu/Vulkan
    // init) only starts after `App::new` returns. Context creation itself
    // is fast; the timeout only bites when the backend is broken.
    match ready_rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(())) => Some(handle),
        Ok(Err(err)) => {
            // Log file, not stderr: background/tray launches have nowhere
            // visible for eprintln, and a silent reader-fallback looks like
            // a broken browser.
            crate::platform::app_log::warn(format!(
                "servo-engine: software context failed ({err}), engine disabled"
            ));
            None
        }
        Err(_) => {
            crate::platform::app_log::warn(
                "servo-engine: context readiness timeout, engine disabled",
            );
            None
        }
    }
}

/// Apply every queued engine command. Returns `true` when a Shutdown was
/// received (the worker loop must break so the post-loop Drop can flush).
fn drain_commands(
    servo: &Servo,
    global: &Rc<RefCell<Global>>,
    rx: &mpsc::Receiver<EngineCommand>,
    shutdown_done: &mut Option<std::sync::mpsc::Sender<()>>,
) -> bool {
    while let Ok(command) = rx.try_recv() {
        if let EngineCommand::Shutdown { done } = command {
            *shutdown_done = Some(done);
            return true;
        }
        apply_command(servo, global, command);
    }
    false
}

/// Content hash for the frame trace / stale-top guard. Sampling every 997th
/// byte catches page changes without hashing 6.4 MiB per frame.
fn frame_hash(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .step_by(997)
        .fold(0xcbf2_9ce4_8422_2325u64, |acc, &b| {
            acc.wrapping_mul(0x0000_0100_0000_01b3)
                .wrapping_add(b as u64)
        })
}

/// Paint the active tab after `notify_new_frame_ready` asked for it. Runs
/// from the worker loop one turn later so the scene is built; painting in
/// the callback can hand back the pre-scroll frame.
fn paint_active_tab(global: &Rc<RefCell<Global>>) {
    let (entry, size) = {
        let g = global.borrow();
        if g.suspended || !g.visible {
            return;
        }
        let Some(entry) = g.tabs.get(&g.active).cloned() else {
            return;
        };
        (entry, g.size)
    };
    if !entry.state.borrow_mut().needs_paint {
        return;
    }
    entry.state.borrow_mut().needs_paint = false;
    let paint_start = Instant::now();
    entry.webview.paint();
    let paint_ms = paint_start.elapsed().as_millis();
    let (w, h) = size;
    let rect: Box2D<i32, DevicePixel> =
        Box2D::new(Point2D::new(0, 0), Point2D::new(w as i32, h as i32));
    let ctx = global.borrow().ctx.clone();
    let read_start = Instant::now();
    let Some(mut img) = ctx.read_to_image(rect) else {
        return;
    };
    let mut read_ms = read_start.elapsed().as_millis();
    let (w, h) = (img.width(), img.height());
    let mut hash = frame_hash(img.as_raw());
    let ax_debug = entry.state.borrow().ax_debug;
    let (tab, scroll, gap_ms, top_hash, last) = {
        let mut state = entry.state.borrow_mut();
        let now = Instant::now();
        let gap = state
            .last_frame_at
            .replace(now)
            .map_or(-1, |t| now.saturating_duration_since(t).as_millis() as i64);
        (
            state.tab,
            state.scroll,
            gap,
            state.top_frame_hash,
            state.last_frame,
        )
    };
    // A frame is suspect when its content is the scroll-0 page at a real
    // scroll, or the last written frame's content after the scroll moved.
    // Both mean the read raced the render / returned the previous buffer.
    let suspect = |hash: u64| {
        (scroll.1 > 8.0 && top_hash == Some(hash))
            || last.is_some_and(|(h, s)| h == hash && (scroll.1 - s.1).abs() > 4.0)
    };
    let mut retried = false;
    if suspect(hash) {
        // One immediate repaint/read usually returns the current scene.
        retried = true;
        let retry_start = Instant::now();
        entry.webview.paint();
        if let Some(img2) = ctx.read_to_image(rect) {
            let h2 = frame_hash(img2.as_raw());
            if h2 != hash {
                img = img2;
                hash = h2;
            }
        }
        read_ms += retry_start.elapsed().as_millis();
    }
    let stale = suspect(hash);
    if ax_debug {
        crate::platform::app_log::info(format!(
            "servo-frame tab{} gen={} scroll=({:.0},{:.0}) hash={hash:016x} paint={paint_ms}ms read={read_ms}ms gap={gap_ms}ms retry={} stale={} {w}x{h}",
            tab.0,
            entry.frame.generation().wrapping_add(1),
            scroll.0,
            scroll.1,
            u8::from(retried),
            u8::from(stale)
        ));
    }
    if stale {
        // Still the wrong scene after a retry: keep the previous texture so
        // the page pauses instead of jumping to a different scroll offset.
        return;
    }
    {
        let mut state = entry.state.borrow_mut();
        if scroll.1 <= 1.0 {
            state.top_frame_hash = Some(hash);
        }
        state.last_frame = Some((hash, scroll));
    }
    let mut g = global.borrow_mut();
    if !g.logged_first_frame {
        g.logged_first_frame = true;
        crate::platform::app_log::info(format!("servo-engine: first frame {w}x{h}, worker live"));
    }
    // Direct handoff: the stable-texture primitive reads the slot; no
    // per-frame event or view rebuild.
    entry.frame.write(w, h, img.into_raw());
}

fn run(
    rx: mpsc::Receiver<EngineCommand>,
    wake_tx: mpsc::Sender<EngineCommand>,
    handle: EngineHandle,
    boot_url: String,
    initial_size: (u32, u32),
    ready: mpsc::Sender<Result<(), String>>,
) {
    // GL context FIRST: see `spawn_engine` ordering contract. Created at
    // the final card size so the first document lays out correctly; a
    // later resize only runs when the monitor geometry actually changes.
    let ctx = match SoftwareRenderingContext::new(PhysicalSize::new(initial_size.0, initial_size.1))
    {
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
    // Servo-side diagnostics (opt-in via RUST_LOG, e.g.
    // `RUST_LOG=net::resource_thread=debug`): without this, Servo's own
    // shutdown/storage logs go nowhere, which once hid a cookie-flush
    // failure behind a successful-looking ack. Harmless when unset.
    servo.setup_logging();
    let ax_debug = std::env::var("SDSC_AX_DEBUG").is_ok();
    let delegate_global: Rc<RefCell<Global>> = Rc::new(RefCell::new(Global {
        ctx: ctx.clone(),
        size: initial_size,
        handle,
        user_content: Rc::new(UserContentManager::new(&servo)),
        visible: false,
        suspended: true,
        active: TabId::BOOT,
        engine_next: TabId::ENGINE_TOP,
        tabs: HashMap::new(),
        by_view: HashMap::new(),
        logged_first_frame: false,
        ax_debug,
        delegate_source: None,
    }));
    // Auto-install the DOM observers into every document (fresh loads,
    // history travel, reloads): no per-document injection timing on our
    // side. The evaluate-boot path below stays as backup. One manager for
    // all tabs (Servo shares it across views by design).
    delegate_global
        .borrow()
        .user_content
        .add_script(Rc::new(UserScript::new(DOM_INSTALL_JS.to_string(), None)));
    // Boot straight onto the given URL (restored active tab, else home):
    // without an initial URL Servo opens about:blank, which would pollute
    // history (and cost a throwaway load) before the first real navigate.
    // Falls back to blank if unparseable (cannot happen for our inputs,
    // but never panic the worker).
    let user_content = delegate_global.borrow().user_content.clone();
    let delegate = Rc::new(Delegate {
        global: delegate_global.clone(),
    });
    delegate_global.borrow_mut().delegate_source = Some(delegate.clone());
    let mut builder = WebViewBuilder::new(&servo, ctx.clone())
        .user_content_manager(user_content)
        .delegate(delegate);
    if let Ok(initial) = url::Url::parse(&boot_url) {
        builder = builder.url(initial);
    }
    let boot_view = builder.build();
    // NOTE: no hide() here — a hidden view produces no frames, and the
    // resume path (Resume/SetVisible) only re-focuses. Visibility gating
    // lives in `notify_new_frame_ready` (suspended/hidden/active checks);
    // background tabs are the ones that hide, on switch.
    boot_view.set_focused(false);
    {
        let mut global = delegate_global.borrow_mut();
        let view_id = boot_view.id();
        let tab_handle = global.handle.clone();
        let tab_ax = global.ax_debug;
        global.by_view.insert(view_id, TabId::BOOT);
        global.tabs.insert(
            TabId::BOOT,
            TabEntry {
                webview: boot_view,
                state: Rc::new(RefCell::new(TabState::fresh(
                    TabId::BOOT,
                    tab_handle,
                    tab_ax,
                ))),
                frame: std::sync::Arc::new(crate::browser::frame_slot::FrameSlot::default()),
            },
        );
    }
    let global = delegate_global;
    crate::platform::app_log::info(format!(
        "servo-engine: ready ({}x{} software surface)",
        initial_size.0, initial_size.1
    ));

    // Shutdown ack: carried out of the drain loop so the post-loop Drop
    // below can signal only after Servo (and its cookie flush) completes.
    let mut shutdown_done: Option<std::sync::mpsc::Sender<()>> = None;
    loop {
        // Paint requests set by the previous spin's callback. Painting here
        // (not inside the callback) lets the scene build finish first.
        paint_active_tab(&global);
        if drain_commands(&servo, &global, &rx, &mut shutdown_done) {
            break;
        }
        // Complete previous synthetic clicks: Down went out with its
        // command; Up waits for at least one full tick so presses have real
        // duration. Every tab flushes its own press (clicks only arm on the
        // active tab, but a tab switch mid-press must never strand one).
        // Runs even while suspended so a press can never strand.
        {
            let global_ref = global.borrow();
            let entries: Vec<TabEntry> = global_ref.tabs.values().cloned().collect();
            drop(global_ref);
            for entry in entries {
                let mut state = entry.state.borrow_mut();
                state.spin_seq = state.spin_seq.wrapping_add(1);
                match state.pending_click_up.take() {
                    // At least one full tick elapsed since Down: complete it.
                    Some((point, at)) if state.spin_seq > at => {
                        drop(state);
                        let id = entry.webview.notify_input_event(InputEvent::MouseButton(
                            MouseButtonEvent::new(
                                MouseButtonAction::Up,
                                MouseButton::Primary,
                                point,
                            ),
                        ));
                        track_input(&entry.state, id, "click");
                    }
                    // Not yet due (or nothing armed): put it back.
                    pending => {
                        state.pending_click_up = pending;
                    }
                }
            }
        }
        // Spin first: Servo renders and copies frames here, which can take
        // hundreds of milliseconds at wide viewports. Drain again afterwards
        // so a scroll command that arrived during the spin updates
        // `last_scroll_at` before the pull gate reads it; otherwise a fresh
        // gesture looks older than the quiet window and a query launches
        // mid-scroll (the page then stalls and the offset blips).
        let suspended = global.borrow().suspended;
        if !suspended {
            servo.spin_event_loop();
        }
        if drain_commands(&servo, &global, &rx, &mut shutdown_done) {
            break;
        }
        // Active-tab-only tick work: DOM pulls, probes, and their result
        // drains never run for background tabs (suspend contract), but loads
        // anywhere still time out.
        let suspended = global.borrow().suspended;
        let active_entry = {
            let global_ref = global.borrow();
            let tab = global_ref.active;
            global_ref.tabs.get(&tab).cloned()
        };
        check_load_timeout(&global);
        if let Some(entry) = &active_entry {
            // Settle held scroll truth before the pull decision: a snapshot
            // rebase computed against the pre-settle offset must not clobber
            // a truth that is now current.
            flush_scroll_truth(&entry.state);
            drain_dom_results(&entry.state);
            maybe_pull_dom(&entry.webview, &entry.state, &global);
            poll_probe(&entry.webview, &entry.state);
            #[cfg(debug_assertions)]
            maybe_probe_storage(&entry.webview, &entry.state);
        }
        std::thread::sleep(if suspended { SUSPENDED_IDLE } else { SPIN_IDLE });
    }
    // Synchronous constellation shutdown happens in Servo's Drop below — the
    // resource thread flushes cookie_jar.json before it returns. Signal the
    // waiter only after that completes, so process exit can't win the race.
    drop(servo);
    if let Some(done) = shutdown_done {
        let _ = done.send(());
    }
}

/// Resolve a per-tab command's target. `None` for closed-race tabs (the
/// view is gone from the maps).
fn tab_entry(global: &Rc<RefCell<Global>>, tab: TabId) -> Option<TabEntry> {
    global.borrow().tabs.get(&tab).cloned()
}

fn apply_command(servo: &Servo, global: &Rc<RefCell<Global>>, command: EngineCommand) {
    match command {
        EngineCommand::Navigate {
            tab,
            url,
            generation,
        } => {
            let Some(entry) = tab_entry(global, tab) else {
                return;
            };
            let mut state = entry.state.borrow_mut();
            match url.parse::<url::Url>() {
                Ok(parsed) => {
                    crate::platform::app_log::info(format!("servo-engine: navigate {url}"));
                    state.generation = generation;
                    state.load_started_at = Some(Instant::now());
                    // Fresh page: clear_tree drops DOM targets, scroll and extent.
                    state.last_url = Some(doc_key(&url).to_string());
                    state.last_full_url = url.clone();
                    clear_tree(&mut state);
                    // Explicit user navigation (overrides clear_tree's Loading).
                    set_activity(&mut state, EngineActivity::Navigating);
                    let tab = state.tab;
                    let handle = state.handle.clone();
                    drop(state);
                    handle.emit(EngineEvent::UrlChanged {
                        tab,
                        url: url.clone(),
                    });
                    let global_ref = global.borrow();
                    entry
                        .webview
                        .set_focused(global_ref.visible && !global_ref.suspended);
                    drop(global_ref);
                    entry.webview.load(parsed);
                }
                Err(err) => {
                    set_activity(&mut state, EngineActivity::Idle);
                    state.handle.emit(EngineEvent::LoadFinished {
                        tab,
                        generation,
                        result: Err(format!("bad URL: {err}")),
                    });
                }
            }
        }
        EngineCommand::ScrollBy { tab, dx, dy, x, y } => {
            let Some(entry) = tab_entry(global, tab) else {
                return;
            };
            // Input lands on the active tab only; anything else is a stale
            // (switched-away) command.
            let live = {
                let global_ref = global.borrow();
                !global_ref.suspended && global_ref.active == tab
            };
            if !live {
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
            // Direct scroll command, not a wheel input event. The wheel
            // path is DOM-correct but each event waits 40-80ms for script
            // (`servo-input: wheel handled dt=…`) — far too slow for 60Hz
            // pad scrolling, and it lets the tracked offset run a whole
            // gesture ahead of the page. This API applies on the paint
            // thread before the frame read.
            //
            // Scroll under the pad cursor, not viewport center: Servo routes
            // to the innermost scroller under the point, so center-targeting
            // stranded page scrolls inside carousels.
            let cmd_at = Instant::now();
            let mut state = entry.state.borrow_mut();
            note_scroll_activity(&mut state, cmd_at);
            state.last_scroll_cmd_at = Some(cmd_at);
            entry.webview.notify_scroll_event(
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
                let viewport = {
                    let g = global.borrow();
                    (g.size.0 as f32, g.size.1 as f32)
                };
                let (sx, sy) = state.scroll;
                set_scroll(&mut state, (sx + dx, sy + dy), viewport);
            }
            // No DOM re-query on scroll: stored rects are document-space, so
            // scrolling only re-translates. Virtualized insertions during a
            // scroll ping through the observers on their own.
            emit_tree(&mut state);
        }
        EngineCommand::Key(TabKey { tab, event }) => {
            let Some(entry) = tab_entry(global, tab) else {
                return;
            };
            let live = {
                let global_ref = global.borrow();
                !global_ref.suspended && global_ref.active == tab
            };
            if !live {
                return;
            }
            let id = entry
                .webview
                .notify_input_event(InputEvent::Keyboard(KeyboardEvent::new(event)));
            track_input(&entry.state, id, "key");
            // Keys can mutate the DOM (typing, shortcuts, focus Cruft):
            // re-query once settled, and invalidate cached verdicts.
            let mut state = entry.state.borrow_mut();
            state.dom_dirty = true;
            state.dom_dirty_at = Some(Instant::now());
            state.dom_epoch = state.dom_epoch.wrapping_add(1);
            state.user_dirty = true;
        }
        EngineCommand::SetDarkMode { dark } => {
            // OS-level theme: every tab's view, not just the active one.
            let global_ref = global.borrow();
            let theme = if dark { Theme::Dark } else { Theme::Light };
            for entry in global_ref.tabs.values() {
                entry.webview.notify_theme_change(theme);
            }
        }
        EngineCommand::GoBack { tab, steps } => {
            let Some(entry) = tab_entry(global, tab) else {
                return;
            };
            if global.borrow().suspended || steps == 0 {
                return;
            }
            // History travel swaps the document: clear_tree drops DOM targets,
            // scroll and extent (no reliable scroll restore to keep).
            clear_tree(&mut entry.state.borrow_mut());
            entry.webview.go_back(steps);
        }
        EngineCommand::GoForward { tab, steps } => {
            let Some(entry) = tab_entry(global, tab) else {
                return;
            };
            if global.borrow().suspended || steps == 0 {
                return;
            }
            clear_tree(&mut entry.state.borrow_mut());
            entry.webview.go_forward(steps);
        }
        EngineCommand::MouseMove { tab, x, y } => {
            let Some(entry) = tab_entry(global, tab) else {
                return;
            };
            let live = {
                let global_ref = global.borrow();
                !global_ref.suspended && global_ref.active == tab
            };
            if !live {
                return;
            }
            let point = WebViewPoint::Device(DevicePoint::new(x, y));
            let id = entry
                .webview
                .notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(point)));
            track_input(&entry.state, id, "move");
            // Roaming cursor re-seeds the cursor-culled set: once a cursor
            // position exists and the pull center is stale (or never set),
            // the live set no longer covers the cursor.
            let mut state = entry.state.borrow_mut();
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
                state.user_dirty = true;
            }
        }
        EngineCommand::ReseedDom { tab } => {
            let Some(entry) = tab_entry(global, tab) else {
                return;
            };
            let live = {
                let global_ref = global.borrow();
                !global_ref.suspended && global_ref.active == tab
            };
            if !live {
                return;
            }
            let mut state = entry.state.borrow_mut();
            state.dom_dirty = true;
            state.dom_dirty_at = Some(Instant::now());
            // User cursor action: take the short pull gap.
            state.user_dirty = true;
            // New cursor region: hover states differ, so cached verdicts
            // are suspect.
            state.dom_epoch = state.dom_epoch.wrapping_add(1);
            // Force a full cull around last_mouse at the next pull.
            state.last_cull_center = None;
        }
        EngineCommand::ClickAt { tab, x, y, target } => {
            let Some(entry) = tab_entry(global, tab) else {
                return;
            };
            let live = {
                let global_ref = global.borrow();
                !global_ref.suspended && global_ref.active == tab
            };
            if !live {
                crate::platform::app_log::info("servo-engine: click dropped (not live)");
                return;
            }
            // Viewport-space point (same space as the cursor and the
            // translated tree); Servo maps to document internally.
            crate::platform::app_log::info(format!("servo-engine: click at ({x:.0},{y:.0})"));
            // Complete any in-flight press first: rapid clicks stay paired
            // (Down/Up, Down/Up) instead of stranding a Down.
            flush_pending_up(&entry.webview, &entry.state);
            // Button- and link-kind dock targets activate on the exact
            // element via `__sdsc_click`: no hit-test staleness, and it
            // exercises whatever handler the page registered
            // (getEventListeners is DevTools-only and no DOM API exposes
            // listeners, so reaching the element IS the discovery).
            // Synthetic clicks provably never navigated (hit test never
            // visibly landed), so links take this path too; fields and free
            // points keep the trusted synthetic pair. A miss falls back to
            // synthetic at the point below.
            let jsid = target.and_then(|uid| {
                entry
                    .state
                    .borrow()
                    .dom_uid
                    .iter()
                    .find(|(_, u)| **u == uid)
                    .map(|(j, _)| *j)
            });
            let direct_kind = jsid.and_then(|j| {
                entry.state.borrow().dom.get(&j).and_then(|n| {
                    (n.kind == FocusKind::Button || n.kind == FocusKind::Link).then_some(n.kind)
                })
            });
            if let Some((jsid, kind)) = jsid.zip(direct_kind) {
                let tab_state = entry.state.clone();
                entry.webview.evaluate_javascript(
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
                            "servo-engine: element click js#{jsid} ({kind:?}) -> {status}"
                        ));
                        if status != "ok" {
                            // Element gone (or helper missing): fall back to
                            // the trusted synthetic pair at the point.
                            tab_state.borrow().handle.send(EngineCommand::ClickAt {
                                tab,
                                x,
                                y,
                                target: None,
                            });
                        }
                    },
                );
            } else {
                send_synthetic_down(&entry.webview, &entry.state, x, y);
            }
            // Clicks mutate the DOM (toggles, expands, navigation): re-query
            // once settled, and invalidate cached verdicts.
            let mut state = entry.state.borrow_mut();
            state.dom_dirty = true;
            state.dom_dirty_at = Some(Instant::now());
            state.dom_epoch = state.dom_epoch.wrapping_add(1);
            state.user_dirty = true;
        }
        EngineCommand::ProbeEventLoop { tab } => {
            let Some(entry) = tab_entry(global, tab) else {
                return;
            };
            let live = {
                let global_ref = global.borrow();
                !global_ref.suspended && global_ref.active == tab
            };
            if !live {
                return;
            }
            // Fire-and-forget arm; the loop dispatches the READ 600ms later
            // and bounds it with a deadline (see `poll_probe`).
            entry.state.borrow_mut().probe_armed_at = Some(Instant::now());
            crate::platform::app_log::info("servo-loop: probe armed");
            entry
                .webview
                .evaluate_javascript(PROBE_ARM_JS.to_string(), |_| {});
        }
        EngineCommand::OpenTab {
            tab,
            url,
            generation,
            frame,
        } => {
            let mut global_ref = global.borrow_mut();
            if global_ref.tabs.contains_key(&tab) {
                crate::platform::app_log::warn(format!(
                    "servo-engine: OpenTab tab{} already exists",
                    tab.0
                ));
                return;
            }
            let ctx = global_ref.ctx.clone();
            let user_content = global_ref.user_content.clone();
            let Some(delegate) = global_ref.delegate_source.clone() else {
                crate::platform::app_log::warn("servo-engine: OpenTab before delegate ready");
                return;
            };
            // URL from birth (see the variant docs): a blank-built view
            // races any post-build Navigate and wins it.
            let parsed = match url.parse::<url::Url>() {
                Ok(parsed) => Some(parsed),
                Err(err) => {
                    crate::platform::app_log::warn(format!(
                        "servo-engine: OpenTab bad url {url}: {err}"
                    ));
                    None
                }
            };
            let mut builder = WebViewBuilder::new(servo, ctx)
                .user_content_manager(user_content)
                .delegate(delegate);
            if let Some(ref initial) = parsed {
                builder = builder.url(initial.clone());
            }
            // Serve the new view un-shown: the UI foregrounds it with
            // `SetActiveTab` next (command order is FIFO).
            let webview = builder.build();
            webview.hide();
            webview.set_focused(false);
            let view_id = webview.id();
            crate::platform::app_log::info(format!(
                "servo-engine: OpenTab tab{} view{view_id:?} url={url}",
                tab.0
            ));
            let handle = global_ref.handle.clone();
            let ax_debug = global_ref.ax_debug;
            let mut tab_state = TabState::fresh(tab, handle, ax_debug);
            match parsed {
                Some(_) => {
                    // Mirror the Navigate arm's bookkeeping (fresh state has
                    // nothing to clear): session generation, timeout clock,
                    // identity, phase. Delegate Started/History/Complete
                    // drive the rest.
                    tab_state.generation = generation;
                    tab_state.load_started_at = Some(Instant::now());
                    tab_state.last_url = Some(doc_key(&url).to_string());
                    tab_state.last_full_url = url.clone();
                    set_activity(&mut tab_state, EngineActivity::Navigating);
                }
                None => {
                    tab_state.generation = generation;
                    set_activity(&mut tab_state, EngineActivity::Idle);
                }
            }
            let failed = parsed.is_none();
            global_ref.by_view.insert(view_id, tab);
            global_ref.tabs.insert(
                tab,
                TabEntry {
                    webview,
                    state: Rc::new(RefCell::new(tab_state)),
                    frame,
                },
            );
            let handle = global_ref.handle.clone();
            drop(global_ref);
            handle.emit(EngineEvent::UrlChanged {
                tab,
                url: url.clone(),
            });
            if failed {
                handle.emit(EngineEvent::LoadFinished {
                    tab,
                    generation,
                    result: Err(format!("bad URL: {url}")),
                });
            }
        }
        EngineCommand::CloseTab { tab } => {
            let mut global_ref = global.borrow_mut();
            let Some(entry) = global_ref.tabs.remove(&tab) else {
                return;
            };
            global_ref.by_view.remove(&entry.webview.id());
            if global_ref.active == tab {
                crate::platform::app_log::warn(format!(
                    "servo-engine: CloseTab closed the active tab{} (UI should switch first)",
                    tab.0
                ));
            }
            crate::platform::app_log::info(format!("servo-engine: CloseTab tab{}", tab.0));
            drop(global_ref);
            // Dropping the last `WebView` handle closes the view
            // constellation-side (`CloseWebView`).
            drop(entry);
        }
        EngineCommand::SetActiveTab { tab } => {
            let mut global_ref = global.borrow_mut();
            if !global_ref.tabs.contains_key(&tab) {
                crate::platform::app_log::warn(format!(
                    "servo-engine: SetActiveTab unknown tab{}",
                    tab.0
                ));
                return;
            }
            if global_ref.active == tab {
                return;
            }
            let old = global_ref.active;
            if let Some(prev) = global_ref.tabs.get(&old).cloned() {
                prev.webview.hide();
                prev.webview.set_focused(false);
            }
            global_ref.active = tab;
            let visible = global_ref.visible;
            let suspended = global_ref.suspended;
            if let Some(next) = global_ref.tabs.get(&tab).cloned() {
                next.webview.show();
                next.webview.set_focused(visible && !suspended);
                // Kick a frame so the newly foregrounded tab paints at once
                // instead of waiting for page-driven invalidation.
                next.webview.paint();
            }
            crate::platform::app_log::info(format!(
                "servo-engine: active tab{} -> tab{}",
                old.0, tab.0
            ));
        }
        EngineCommand::Resize { width, height } => {
            let mut global_ref = global.borrow_mut();
            let width = width.max(1);
            let height = height.max(1);
            let size = PhysicalSize::new(width, height);
            // WebView::resize resizes the shared RenderingContext *and*
            // notifies Servo, so the document viewport reflows to the new
            // size. Calling ctx.resize directly only grows the buffer: the
            // page stays at its old width in the top-left of a white surface.
            let view = global_ref
                .tabs
                .get(&global_ref.active)
                .or_else(|| global_ref.tabs.values().next())
                .map(|entry| entry.webview.clone());
            if let Some(view) = view {
                view.resize(size);
            } else {
                global_ref.ctx.resize(size);
            }
            global_ref.size = (width, height);
            crate::controller::hid::diag::diag_info(format!(
                "servo-engine: resize -> {width}x{height}"
            ));
        }
        EngineCommand::SetVisible { visible } => {
            let mut global_ref = global.borrow_mut();
            global_ref.visible = visible;
            let suspended = global_ref.suspended;
            let active = global_ref.active;
            if let Some(entry) = global_ref.tabs.get(&active).cloned() {
                entry.webview.set_focused(visible && !suspended);
                if visible {
                    // The UI may have started with a placeholder slot;
                    // hand over the worker-owned one for the boot tab.
                    global_ref.handle.emit(EngineEvent::FrameSlot {
                        tab: active,
                        slot: entry.frame.clone(),
                    });
                }
            }
        }
        EngineCommand::Suspend => {
            let mut global_ref = global.borrow_mut();
            global_ref.suspended = true;
            let active = global_ref.active;
            if let Some(entry) = global_ref.tabs.get(&active).cloned() {
                entry.webview.set_focused(false);
            }
        }
        EngineCommand::Resume => {
            let mut global_ref = global.borrow_mut();
            global_ref.suspended = false;
            let visible = global_ref.visible;
            let active = global_ref.active;
            if let Some(entry) = global_ref.tabs.get(&active).cloned() {
                entry.webview.set_focused(visible);
            }
        }
        EngineCommand::Shutdown { .. } | EngineCommand::Wake => {}
    }
}

/// Set the status-strip phase, emitting (and logging) only on change.
fn set_activity(state: &mut TabState, phase: EngineActivity) {
    if state.activity == phase {
        return;
    }
    crate::platform::app_log::info(format!(
        "servo-engine: activity {:?} -> {phase:?}",
        state.activity
    ));
    state.activity = phase;
    let tab = state.tab;
    state
        .handle
        .emit(EngineEvent::ActivityChanged { tab, phase });
}

/// Document changed (fresh load or history travel): drop DOM targets, scroll
/// and content extent. Session history does not reliably restore scroll,
/// and stale anything misplaces every outline (observed in testing).
fn clear_tree(state: &mut TabState) {
    // A new document is loading content from here.
    set_activity(state, EngineActivity::Loading);
    if !state.dom.is_empty() {
        state.dom.clear();
        state.dom_uid.clear();
        state.last_emitted = Vec::new();
        let tab = state.tab;
        state.handle.emit(EngineEvent::A11yTree {
            tab,
            items: Vec::new(),
        });
    }
    let had_content = state.content.is_some();
    state.content = None;
    if had_content {
        let tab = state.tab;
        state.handle.emit(EngineEvent::ScrollExtent {
            tab,
            extent: (0.0, 0.0),
        });
    }
    // A new document gets fresh DOM ids (the in-page WeakMap resets), a
    // fresh poll epoch (in-flight answers from the old page are dropped),
    // a fresh visibility epoch (no verdicts carry across documents), and
    // a boot round-trip (idempotent install + snapshot).
    state.dom_seq = state.dom_seq.wrapping_add(1);
    state.dom_epoch = state.dom_epoch.wrapping_add(1);
    state.last_focus = None;
    state.dom_in_flight = false;
    state.dom_in_flight_at = None;
    state.dom_boot = true;
    state.dom_dirty = false;
    state.dom_dirty_at = None;
    state.dom_ping = false;
    state.last_pull_at = None;
    state.last_pull_rtt_ms = 0;
    state.last_scroll_at = None;
    state.last_scroll_cmd_at = None;
    state.scroll_hot_since = None;
    state.needs_paint = false;
    state.top_frame_hash = None;
    state.last_frame = None;
    state.pending_scroll_truth = None;
    state.user_dirty = false;
    state.doc_token = None;
    // Truth bound resets with the document (see `scroll_max`).
    state.scroll_max = (0.0, 0.0);
    // Stale probe reply can never match this document; re-armed per load.
    #[cfg(debug_assertions)]
    {
        state.storage_probe_pending = false;
        state.storage_probe_rx = None;
    }
    if state.scroll != (0.0, 0.0) {
        state.scroll = (0.0, 0.0);
        state.last_sent_scroll = (0.0, 0.0);
        let tab = state.tab;
        state.handle.emit(EngineEvent::ScrollOffset {
            tab,
            x: 0.0,
            y: 0.0,
        });
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
fn clamp_scroll(
    scroll: (f32, f32),
    content: Option<(f32, f32)>,
    viewport: (f32, f32),
) -> (f32, f32) {
    let (max_x, max_y) = match content {
        Some((w, h)) => ((w - viewport.0).max(0.0), (h - viewport.1).max(0.0)),
        None => (f32::INFINITY, f32::INFINITY),
    };
    (scroll.0.clamp(0.0, max_x), scroll.1.clamp(0.0, max_y))
}

/// Quiet period after a dirty ping before the pull fires (lets the page
/// reflow after the mutation burst that triggered the ping). Tuned low:
/// mid-reflow pulls self-correct via the next ping.
const DOM_SETTLE: Duration = Duration::from_millis(250);

/// After scroll activity, hold pulls until the gesture has been quiet this
/// long. A snapshot's synchronous geometry pass blocks the page's script
/// thread for up to a second; running it mid-gesture queues the user's
/// scroll behind the pass, and the page then lands as one visible jump.
const DOM_SCROLL_QUIET: Duration = Duration::from_millis(400);

/// Hard cap on the scroll-quiet hold: a page with a continuously animating
/// scroller must still refresh its targets eventually.
const DOM_SCROLL_QUIET_MAX: Duration = Duration::from_secs(3);

/// A continuous scroll period survives this long without activity. Longer
/// than the in-page ping throttle (500ms) so a ping-only scroller keeps one
/// period, shorter than a deliberate pause.
const DOM_SCROLL_PERIOD_GAP: Duration = Duration::from_millis(800);

/// Breathing room after a completed pull for page-driven mutations. On
/// mutation-heavy pages (Google toggles classes constantly) back-to-back
/// 1s visibility walks keep the script thread saturated, which shows up as
/// pixel jitter. User-triggered pulls bypass this.
const DOM_IDLE_BREATHER: Duration = Duration::from_millis(2000);

/// Record scroll activity and mark the start of a continuous hot period.
/// A fresh period starts only after the quiet window lapsed, so the
/// starvation cap measures the current gesture, not the nightly pull
/// cadence before it.
/// True while the current continuous scroll period is still under the
/// starvation cap. A missing marker defers (no starvation evidence).
fn scroll_cap_ok(hot_since: Option<Instant>, now: Instant) -> bool {
    hot_since.is_none_or(|s| now.saturating_duration_since(s) < DOM_SCROLL_QUIET_MAX)
}

fn note_scroll_activity(state: &mut TabState, now: Instant) {
    let fresh = state
        .last_scroll_at
        .is_none_or(|t| now.saturating_duration_since(t) >= DOM_SCROLL_PERIOD_GAP);
    if fresh || state.scroll_hot_since.is_none() {
        state.scroll_hot_since = Some(now);
    }
    state.last_scroll_at = Some(now);
}

/// Minimum spacing from the previous pull. User actions keep the short
/// floor (hover/click feedback); page-driven mutations get a breather
/// scaled by the last pass so the script thread stays available for the
/// page itself.
fn pull_gap(user_dirty: bool, rtt_ms: u64) -> Duration {
    let rtt = Duration::from_millis(rtt_ms);
    if user_dirty {
        DOM_MIN_PULL_GAP.max(rtt / 2)
    } else {
        rtt + DOM_IDLE_BREATHER
    }
}

/// Pull the page for its actionable elements (see `browser::dom`). Purely
/// event-driven — no timers: fires for a pending boot (fresh document or
/// load settle) or a settled dirty ping from the in-page observers
/// (console/fetch/sendBeacon, ORed into one flag). Skipped while suspended,
/// with a pull in flight (5s timeout unsticks it), or while a scroll
/// gesture is still active.
fn maybe_pull_dom(webview: &WebView, shared: &Rc<RefCell<TabState>>, global: &Rc<RefCell<Global>>) {
    let now = Instant::now();
    let fire = {
        let mut state = shared.borrow_mut();
        // Suspended engine: no pulls anywhere (the loop only calls this for
        // the active tab anyway — belt and suspenders).
        if global.borrow().suspended {
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
        // Never measure geometry mid-scroll: the pass blocks the page's
        // script thread and the queued scroll lands as one jump. The dirty
        // flag and timestamp survive, so the refresh fires once the gesture
        // goes quiet. Boot pulls are not exempt: a false `Complete` (a head
        // script/resource re-report) or a real load landing mid-gesture is
        // exactly what produced the 0 <-> N blip.
        let command_hot = state
            .last_scroll_cmd_at
            .is_some_and(|t| now.saturating_duration_since(t) < DOM_SCROLL_QUIET);
        let scroll_hot = state
            .last_scroll_at
            .is_some_and(|t| now.saturating_duration_since(t) < DOM_SCROLL_QUIET);
        // A gesture we are commanding has no cap at all. Ping-only motion
        // (animated scroller) is capped by how long *this* scroll has been
        // continuous — not by the last pull age: a pull from before the
        // gesture made a 1s-old scroll look like 3s, which is what fired
        // mid-settle at scroll_age=281ms in the 17:46 trace.
        if command_hot || (scroll_hot && scroll_cap_ok(state.scroll_hot_since, now)) {
            return;
        }
        // Adaptive cooldown: a slow pull bought breathing room before the
        // next one. Page-driven mutation pings (Google's class churn) would
        // otherwise run 1.1s visibility walks back-to-back and keep the
        // script thread saturated, which reads as pixel jitter. User
        // actions skip the breather so clicks/typing stay responsive.
        if !boot && let Some(last) = state.last_pull_at {
            let gap = pull_gap(state.user_dirty, state.last_pull_rtt_ms);
            if now.saturating_duration_since(last) < gap {
                return;
            }
        }
        // Diagnostic ages for the pull trace: a fresh scroll timestamp makes
        // the quiet gate hold; a stale one is a loop-latency bug.
        let scroll_age_ms = state
            .last_scroll_at
            .map_or(-1, |t| now.saturating_duration_since(t).as_millis() as i64);
        let pull_age_ms = state
            .last_pull_at
            .map_or(-1, |t| now.saturating_duration_since(t).as_millis() as i64);
        let user_dirty = state.user_dirty;
        // Reserve the sequence first so a racing callback can't interleave.
        state.dom_seq = state.dom_seq.wrapping_add(1);
        let seq = state.dom_seq;
        state.dom_in_flight = true;
        state.dom_in_flight_at = Some(now);
        state.dom_boot = false;
        state.dom_dirty = false;
        state.dom_dirty_at = None;
        state.user_dirty = false;
        state.last_pull_at = Some(now);
        let pinged = std::mem::take(&mut state.dom_ping);
        let center = state.last_mouse;
        state.last_cull_center = center;
        let epoch = state.dom_epoch;
        let tab = state.tab;
        let scroll = state.scroll;
        let script = if boot {
            dom_boot_js(center, epoch)
        } else {
            dom_query_js(center, epoch)
        };
        Some((
            seq,
            state.dom_tx.clone(),
            script,
            boot,
            pinged,
            now,
            epoch,
            tab,
            scroll,
            scroll_age_ms,
            pull_age_ms,
            user_dirty,
        ))
    };
    let Some((
        seq,
        tx,
        script,
        boot,
        pinged,
        dispatched,
        epoch,
        tab,
        scroll,
        scroll_age_ms,
        pull_age_ms,
        user_dirty,
    )) = fire
    else {
        return;
    };
    let src = if boot {
        "boot"
    } else if pinged {
        "B"
    } else {
        "gesture"
    };
    crate::platform::app_log::info(format!(
        "servo-dom: pull tab{} seq={seq} src={src} ep={epoch} scroll=({:.0},{:.0}) scroll_age={scroll_age_ms}ms pull_age={pull_age_ms}ms user={}",
        tab.0,
        scroll.0,
        scroll.1,
        u8::from(user_dirty)
    ));
    webview.evaluate_javascript(script, move |result| {
        let _ = tx.send((
            seq,
            dispatched,
            scroll,
            result.map_err(|e| format!("{e:?}")),
        ));
    });
}

/// Drain completed DOM queries: parse, merge, re-emit through the normal
/// diff path (so the UI, cursor, and debug overlay see every update).
fn drain_dom_results(shared: &Rc<RefCell<TabState>>) {
    loop {
        let msg = shared
            .borrow_mut()
            .dom_rx
            .as_mut()
            .and_then(|rx| rx.try_recv().ok());
        let Some((seq, dispatched, scroll_at_dispatch, result)) = msg else {
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
        // The query's scroll read and box pass ran in the page while the
        // tracked offset was free to move (commanded scroll, cursor reseed,
        // ping correction). Applying that older reading yanks the overlay
        // backwards — the 0 <-> N blip. Pings also carry newer truth and
        // already applied it.
        let rebase = rebase_decision(state.ping_at, dispatched, state.scroll, scroll_at_dispatch);
        match result {
            Ok(value) => {
                ingest_dom(&mut state, &value, rtt, rebase);
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
fn track_input(shared: &Rc<RefCell<TabState>>, id: InputEventId, kind: &'static str) {
    let mut state = shared.borrow_mut();
    if state.pending_inputs.len() >= 64 {
        state.pending_inputs.pop_front();
    }
    state.pending_inputs.push_back((id, kind, Instant::now()));
}

/// Debug-only storage spike diagnostic: one-shot in-page census per
/// document (localStorage/sessionStorage sizes, probe-key round-trip,
/// cookie count — key names truncated, values never read). Dispatched when
/// the load-complete arm fires, drained below; logged once then forgotten.
#[cfg(debug_assertions)]
fn maybe_probe_storage(webview: &WebView, shared: &Rc<RefCell<TabState>>) {
    let msg = shared
        .borrow_mut()
        .storage_probe_rx
        .as_mut()
        .and_then(|rx| rx.try_recv().ok());
    if let Some(result) = msg {
        shared.borrow_mut().storage_probe_rx = None;
        let rendered: String = match result {
            Ok(JSValue::String(s)) => s.chars().take(300).collect(),
            Ok(other) => format!("non-string:{other:?}").chars().take(300).collect(),
            Err(err) => format!("eval-failed:{err}").chars().take(300).collect(),
        };
        let url = shared
            .borrow()
            .last_url
            .clone()
            .unwrap_or_else(|| "?".to_string());
        crate::platform::app_log::info(format!("servo-dom: storage probe url={url} {rendered}"));
        return;
    }
    if shared.borrow().storage_probe_rx.is_some() {
        return;
    }
    let mut state = shared.borrow_mut();
    if !state.storage_probe_pending {
        return;
    }
    state.storage_probe_pending = false;
    let (tx, rx) = mpsc::channel();
    state.storage_probe_rx = Some(rx);
    drop(state);
    webview.evaluate_javascript(
        crate::browser::dom::STORAGE_PROBE_JS.to_string(),
        move |result| {
            let _ = tx.send(result.map_err(|e| format!("{e:?}")));
        },
    );
}

/// Drain one-shot event-loop probe results and drive its two phases:
/// READ dispatch 600ms after the arm (lets the 100ms timer and one rAF
/// fire), then a 2s bound on the READ round-trip.
fn poll_probe(webview: &WebView, shared: &Rc<RefCell<TabState>>) {
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
/// [`TabState::pending_click_up`] for the next loop tick (see `run`), so
/// every press has real duration instead of a zero-width Down+Up pair.
fn send_synthetic_down(webview: &WebView, shared: &Rc<RefCell<TabState>>, x: f32, y: f32) {
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
fn flush_pending_up(webview: &WebView, shared: &Rc<RefCell<TabState>>) {
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

/// Merge one DOM snapshot into the live set. Rects arrive viewport-relative
/// and are translated to document space with the snapshot's own scroll
/// reading, so a skipped rebase still merges the right document positions.
/// The tracked offset rebases to the snapshot truth only when nothing moved
/// it after dispatch ([`Rebase::Apply`]): an in-flight command or ping
/// makes that reading stale, and applying it yanks every outline backwards
/// (the 0 <-> N blip). Absent nodes accumulate misses and drop at
/// [`DOM_MISSED_LIMIT`] (re-render flicker reads as one miss, real removals
/// as two in a row).
/// True when a snapshot proves the page was replaced without a Navigate or
/// history event (same-URL reload). Empty tokens (the page never ran our
/// install) never force a reset.
fn doc_token_changed(old: Option<&str>, new: &str) -> bool {
    !new.is_empty() && old.is_some_and(|t| t != new)
}

fn ingest_dom(state: &mut TabState, value: &JSValue, rtt_ms: u128, rebase: Rebase) {
    use std::collections::HashSet;
    let snap = parse_dom_snapshot(value);
    let token_changed = doc_token_changed(state.doc_token.as_deref(), &snap.doc_token);
    if token_changed {
        // Same-URL reload: no Navigate/history event ran clear_tree, so the
        // old document's tree is still here. Reset before merging this
        // snapshot, which is the new document's boot.
        crate::platform::app_log::info(format!(
            "servo-dom: document swapped under the same URL tab{} — resetting targets",
            state.tab.0
        ));
        clear_tree(state);
        set_activity(state, EngineActivity::Initializing);
        state.dom_boot = false;
        state.dom_dirty = false;
        state.dom_dirty_at = None;
        state.dom_ping = false;
    }
    if !snap.doc_token.is_empty() {
        state.doc_token = Some(snap.doc_token.clone());
    }
    // The snapshot's own truth counts as observed even when the rebase is
    // skipped (a newer ping already applied): it is still a position the
    // page proved reachable, so it feeds the commanded-lead bound.
    note_scroll_truth(&mut state.scroll_max, snap.scroll);
    // Tracked offset before the snapshot truth is applied: logged alongside
    // the snapshot's own truth so stale-box reports can separate "snapshot
    // measured old truth" from "rebase yanked backwards".
    let pre_scroll = state.scroll;
    // Truth wins only when it is still current. Heals unapplied commands
    // (edge clamping the page ignored) and history-restored offsets alike.
    // A document swap has no prior truth to protect: its boot snapshot is
    // authoritative. `rebase_scroll` is origin-pinned and never
    // upper-clamped (extent can lag truth).
    if token_changed || rebase == Rebase::Apply {
        rebase_scroll(state, snap.scroll);
    }
    // Document dims bound scroll clamping (the live set only spans
    // viewport+margin after culling, so entry corners can't). Grow-only per
    // document, reset by `clear_tree`.
    let (dw, dh) = (snap.doc.0.max(0.0), snap.doc.1.max(0.0));
    let old_content = state.content;
    if dw > 0.0 || dh > 0.0 {
        state.content = Some(match state.content {
            Some(prev) => (prev.0.max(dw), prev.1.max(dh)),
            None => (dw, dh),
        });
    }
    if state.content != old_content
        && let Some((w, h)) = state.content
    {
        let tab = state.tab;
        state.handle.emit(EngineEvent::ScrollExtent {
            tab,
            extent: (w, h),
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
            n.url = r.url.clone();
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
                    url: r.url.clone(),
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
                    "servo-dom map #{id} <= js#{} {kind:?} [{}]{}{} {}",
                    r.id,
                    r.tag,
                    if r.role.is_empty() {
                        String::new()
                    } else {
                        format!(" role={}", r.role)
                    },
                    if r.autofocus { " autofocus" } else { "" },
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
    // Page focus (autofocus attribute, JS .focus()): resolve the snapshot's
    // active id to a live UI id and emit on change only, so the cursor
    // docks exactly once per focus move. Unknown/culled/body targets (0 or
    // unmapped) resolve to None and stay silent — blur never undocks.
    let focus = (snap.active != 0)
        .then(|| state.dom_uid.get(&snap.active).copied())
        .flatten();
    if focus != state.last_focus {
        crate::platform::app_log::info(format!(
            "servo-dom: focus tab{} {:?} -> {focus:?} (snap.active js#{})",
            state.tab.0, state.last_focus, snap.active
        ));
        state.last_focus = focus;
        if let Some(id) = focus {
            let tab = state.tab;
            state.handle.emit(EngineEvent::FocusChanged { tab, id });
        }
    }
    // First snapshot after a completed document ends the initializing phase
    // (DOM observers are in, targets flowing). Gated on Initializing so
    // scroll-settle pulls can never Idle-out an in-flight load.
    if state.activity == EngineActivity::Initializing {
        set_activity(state, EngineActivity::Idle);
    }
    crate::platform::app_log::info(format!(
        "servo-dom: ingested fresh={fresh} dropped={dropped} retained={retained} live={} total={total} culled={culled} skip={skipped:?} boxA={box_anchors} boxKid={box_kids} boxAWrap={box_wraps} boxAEmpty={box_empty} forced={forced} hid={hid} clip={clip} t={timings:?} rtt={rtt_ms}ms rb={} snap=({:.0},{:.0}) trk=({:.0},{:.0})",
        state.dom.len(),
        rebase.label(),
        snap.scroll.0,
        snap.scroll.1,
        pre_scroll.0,
        pre_scroll.1,
    ));
}

/// Apply held page scroll truth once the gesture has gone quiet.
///
/// Ping truth is read on the page's script thread; while scroll input is
/// being applied that value lags the visual scroll (the first flick in the
/// 17:31 trace: commands at 364, script still at 255). Rebasing to it
/// mid-gesture is the 364 -> 255 -> 364 rubber-band. The ping handler holds
/// the latest truth instead; this applies it once, after quiet.
fn flush_scroll_truth(shared: &Rc<RefCell<TabState>>) {
    let now = Instant::now();
    let mut state = shared.borrow_mut();
    let Some(truth) = state.pending_scroll_truth else {
        return;
    };
    let hot = state
        .last_scroll_at
        .is_some_and(|t| now.saturating_duration_since(t) < DOM_SCROLL_QUIET)
        || state
            .last_scroll_cmd_at
            .is_some_and(|t| now.saturating_duration_since(t) < DOM_SCROLL_QUIET);
    if hot {
        return;
    }
    state.pending_scroll_truth = None;
    crate::platform::app_log::info(format!(
        "servo-dom: scroll settle tab{} truth=({:.0},{:.0}) tracked=({:.0},{:.0})",
        state.tab.0, truth.0, truth.1, state.scroll.0, state.scroll.1
    ));
    if rebase_scroll(&mut state, truth) {
        // Re-translate the stored tree at the settled offset; otherwise the
        // first D-pad press after a scroll sees the pre-scroll rects.
        emit_tree(&mut state);
    }
}

/// Rebase the tracked offset to the page's reported scroll. Unlike
/// [`set_scroll`] this deliberately skips the upper clamp: our extent can
/// lag the page (unseen far corners), and truth from the page must win —
/// clamping it would reintroduce the very drift this heals. Origin-pinned
/// (negative scroll is never real) and reported when it moves the UI value.
/// Returns whether the tracked offset moved; callers that run outside a
/// DOM ingest must re-emit the translated tree (see `emit_tree`).
fn rebase_scroll(state: &mut TabState, scroll: (f32, f32)) -> bool {
    let rebased = (scroll.0.max(0.0), scroll.1.max(0.0));
    if state.scroll == rebased {
        return false;
    }
    crate::platform::app_log::info(format!(
        "servo-dom: rebase tab{} scroll=({:.0},{:.0}) -> ({:.0},{:.0})",
        state.tab.0, state.scroll.0, state.scroll.1, rebased.0, rebased.1
    ));
    state.scroll = rebased;
    if state.last_sent_scroll == rebased {
        return true;
    }
    state.last_sent_scroll = rebased;
    let tab = state.tab;
    state.handle.emit(EngineEvent::ScrollOffset {
        tab,
        x: rebased.0,
        y: rebased.1,
    });
    true
}

/// Convert a Servo favicon frame to RGBA8 bytes for an iced image handle.
/// `None` for empty, oversized (>256px — favicons are tiny; anything larger
/// is a mislabeled hero image), short, or unsupported formats.
fn favicon_rgba(image: &embedder_traits::Image) -> Option<(u32, u32, Vec<u8>)> {
    use embedder_traits::PixelFormat;
    let (w, h) = (image.width, image.height);
    if w == 0 || h == 0 || w > 256 || h > 256 {
        return None;
    }
    let px = (w as usize) * (h as usize);
    let data = image.data();
    let rgba = match image.format {
        PixelFormat::RGBA8 => {
            if data.len() != px * 4 {
                return None;
            }
            data.to_vec()
        }
        PixelFormat::BGRA8 => {
            if data.len() != px * 4 {
                return None;
            }
            let mut out = Vec::with_capacity(data.len());
            for chunk in data.as_chunks::<4>().0 {
                out.extend_from_slice(&[chunk[2], chunk[1], chunk[0], chunk[3]]);
            }
            out
        }
        PixelFormat::RGB8 => {
            if data.len() != px * 3 {
                return None;
            }
            let mut out = Vec::with_capacity(px * 4);
            for chunk in data.as_chunks::<3>().0 {
                out.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
            }
            out
        }
        PixelFormat::K8 => {
            if data.len() != px {
                return None;
            }
            let mut out = Vec::with_capacity(px * 4);
            for g in data {
                out.extend_from_slice(&[*g, *g, *g, 255]);
            }
            out
        }
        PixelFormat::KA8 => {
            if data.len() != px * 2 {
                return None;
            }
            let mut out = Vec::with_capacity(px * 4);
            for chunk in data.as_chunks::<2>().0 {
                out.extend_from_slice(&[chunk[0], chunk[0], chunk[0], chunk[1]]);
            }
            out
        }
    };
    Some((w, h, rgba))
}

/// Parse the window-truth coordinates off a dirty-ping URL (`?wx=&wy=`).
/// `None` when absent or malformed — the ping still dirties, it just
/// carries no correction.
/// Event tag from a DOM ping URL: `sc` (scroll only), `mc` (structural
/// mutation), `ma` (attribute/visibility change). Missing or unknown tags
/// default to `""`, which the caller treats as invalidating.
fn ping_event(url: &str) -> &str {
    let Some((_, query)) = url.split_once('?') else {
        return "";
    };
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("ev="))
        .unwrap_or("")
}

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
/// `TabState::scroll_max`). Every observation — ping `wx/wy`, snapshot `sx/sy`
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
fn bound_commanded(scroll: (f32, f32), scroll_max: (f32, f32), viewport: (f32, f32)) -> (f32, f32) {
    (
        scroll.0.min(scroll_max.0 + viewport.0),
        scroll.1.min(scroll_max.1 + viewport.1),
    )
}

/// Update the tracked scroll offset and report it when it moves, so the UI
/// overlay can correlate cursor against targets.
fn set_scroll(state: &mut TabState, scroll: (f32, f32), viewport: (f32, f32)) {
    // Commanded lead is bounded by observed truth (see `bound_commanded`).
    let bounded = bound_commanded(scroll, state.scroll_max, viewport);
    let clamped = clamp_scroll(bounded, state.content, viewport);
    if state.scroll == clamped {
        return;
    }
    state.scroll = clamped;
    state.last_sent_scroll = clamped;
    let tab = state.tab;
    state.handle.emit(EngineEvent::ScrollOffset {
        tab,
        x: clamped.0,
        y: clamped.1,
    });
}

/// Diff + publish the translated tree. Logs only when the set size changes
/// so scroll ticks (which move rects without adding nodes) stay silent.
fn emit_tree(state: &mut TabState) {
    let (focusables, extent) = collect_focusables(state);
    // Extent only grows per document (reset by `clear_tree`).
    state.content = Some(match state.content {
        Some(prev) => (prev.0.max(extent.0), prev.1.max(extent.1)),
        None => extent,
    });
    let fresh = translate_tree(focusables, state.scroll);
    if fresh == state.last_emitted {
        return;
    }
    if fresh.len() != state.last_emitted.len() {
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
            "servo-engine: dom focusables={} tab{} scroll=({:.0},{:.0}){sample}",
            fresh.len(),
            state.tab.0,
            state.scroll.0,
            state.scroll.1
        ));
    }
    state.last_emitted = fresh.clone();
    let tab = state.tab;
    state
        .handle
        .emit(EngineEvent::A11yTree { tab, items: fresh });
}

/// Fail loads that never complete so the UI never spins forever. Every tab
/// times out on its own clock (background loads keep running in the
/// constellation even while suspended from our side).
fn check_load_timeout(global: &Rc<RefCell<Global>>) {
    let entries: Vec<TabEntry> = global.borrow().tabs.values().cloned().collect();
    for entry in entries {
        let mut state = entry.state.borrow_mut();
        let Some(started) = state.load_started_at else {
            continue;
        };
        if started.elapsed() < LOAD_TIMEOUT {
            continue;
        }
        state.load_started_at = None;
        let generation = state.generation;
        let tab = state.tab;
        crate::platform::app_log::warn(format!(
            "servo-engine: load timed out after {}s",
            LOAD_TIMEOUT.as_secs()
        ));
        set_activity(&mut state, EngineActivity::Idle);
        state.handle.emit(EngineEvent::LoadFinished {
            tab,
            generation,
            result: Err("load timed out".to_string()),
        });
    }
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
            url: None,
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
        assert_eq!(
            clamp_scroll((100.0, 200.0), None, (800.0, 600.0)),
            (100.0, 200.0)
        );
        // Negative is never real (pages sit at/after origin).
        assert_eq!(
            clamp_scroll((-57.0, -74.0), None, (800.0, 600.0)),
            (0.0, 0.0)
        );
        // Known 800x2481 document on an 800x600 viewport.
        let content = Some((800.0, 2481.0));
        assert_eq!(
            clamp_scroll((0.0, 2573.0), content, (800.0, 600.0)),
            (0.0, 1881.0)
        );
        assert_eq!(
            clamp_scroll((-225.0, 100.0), content, (800.0, 600.0)),
            (0.0, 100.0)
        );
        assert_eq!(
            clamp_scroll((0.0, 1881.0), content, (800.0, 600.0)),
            (0.0, 1881.0)
        );
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
        assert_eq!(
            bound_commanded((0.0, 1500.0), (0.0, 1000.0), (800.0, 600.0)),
            (0.0, 1500.0)
        );
        // Past the end the page proved: pinned to truth + viewport.
        assert_eq!(
            bound_commanded((0.0, 5000.0), (0.0, 1000.0), (800.0, 600.0)),
            (0.0, 1600.0)
        );
        assert_eq!(
            bound_commanded((5000.0, 0.0), (0.0, 0.0), (800.0, 600.0)),
            (800.0, 0.0)
        );
        // Fresh document (no truth yet): one viewport of exploration.
        assert_eq!(
            bound_commanded((0.0, 599.0), (0.0, 0.0), (800.0, 600.0)),
            (0.0, 599.0)
        );
        assert_eq!(
            bound_commanded((0.0, 601.0), (0.0, 0.0), (800.0, 600.0)),
            (0.0, 600.0)
        );
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

    #[test]
    fn ping_event_splits_scroll_from_mutations() {
        let base = "http://sdsc.invalid/__SDSC_DOM_DIRTY?wx=0&wy=256";
        assert_eq!(ping_event(&format!("{base}&ev=sc")), "sc");
        assert_eq!(ping_event(&format!("{base}&ev=mc")), "mc");
        assert_eq!(ping_event(&format!("{base}&ev=ma")), "ma");
        // Missing or malformed tags fall back to the invalidating path.
        assert_eq!(ping_event(base), "");
        assert_eq!(ping_event("http://sdsc.invalid/ping"), "");
    }

    #[test]
    fn doc_token_mismatch_means_a_real_document_swap() {
        // First snapshot: no prior token, never a reset.
        assert!(!doc_token_changed(None, "abc"));
        // Same document (spurious Complete / head-resource re-report).
        assert!(!doc_token_changed(Some("abc"), "abc"));
        // Real same-URL reload: new window, new token.
        assert!(doc_token_changed(Some("abc"), "def"));
        // Malformed/missing token never forces a reset.
        assert!(!doc_token_changed(Some("abc"), ""));
        assert!(!doc_token_changed(None, ""));
    }

    #[test]
    fn starvation_cap_measures_the_current_scroll_period() {
        let now = Instant::now();
        // No marker: defer (a fresh period is never capped).
        assert!(scroll_cap_ok(None, now));
        // The 17:46 case: the scroll is only 1.2s old; the last pull being
        // 3.1s ago must not force a query through it.
        assert!(scroll_cap_ok(Some(now - Duration::from_millis(1200)), now));
        // A genuinely continuous ping-only scroller eventually refreshes.
        assert!(!scroll_cap_ok(Some(now - Duration::from_millis(3100)), now));
    }

    #[test]
    fn idle_pulls_get_a_breather_but_user_pulls_do_not() {
        assert_eq!(pull_gap(true, 0), DOM_MIN_PULL_GAP);
        assert_eq!(pull_gap(true, 1200), Duration::from_millis(600));
        assert_eq!(pull_gap(false, 0), DOM_IDLE_BREATHER);
        assert_eq!(
            pull_gap(false, 1200),
            Duration::from_millis(1200) + DOM_IDLE_BREATHER
        );
    }

    #[test]
    fn stale_snapshot_never_rebases_over_newer_scroll() {
        let now = Instant::now();
        // Dispatch and drain with nothing moving: truth is current.
        assert_eq!(
            rebase_decision(None, now, (0.0, 256.0), (0.0, 256.0)),
            Rebase::Apply
        );
        // The 0 <-> 256 blip: the query read 0, commands moved the tracked
        // offset to 256 while it was in flight. Applying 0 would yank it.
        assert_eq!(
            rebase_decision(None, now, (0.0, 256.0), (0.0, 1.0)),
            Rebase::SkipScroll
        );
        // A ping after dispatch already applied newer truth.
        assert_eq!(
            rebase_decision(
                Some(now + Duration::from_millis(1)),
                now,
                (0.0, 256.0),
                (0.0, 1.0)
            ),
            Rebase::SkipPing
        );
        // A ping at/before dispatch did not supersede the snapshot.
        assert_eq!(
            rebase_decision(Some(now), now, (0.0, 256.0), (0.0, 256.0)),
            Rebase::Apply
        );
    }
}
