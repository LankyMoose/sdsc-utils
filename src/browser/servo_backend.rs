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
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use dpi::PhysicalSize;
use embedder_traits::{
    InputEvent, KeyboardEvent, MouseButton, MouseButtonAction, MouseButtonEvent, Theme,
};
use euclid::{Box2D, Point2D};

use servo::{
    DevicePixel, DevicePoint, DeviceVector2D, EventLoopWaker, LoadStatus, Preferences,
    RenderingContext, Scroll, ServoBuilder, SoftwareRenderingContext, WebView, WebViewBuilder,
    WebViewDelegate, WebViewPoint, WebViewVector,
};

use crate::browser::cursor::{FocusKind, Focusable};
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
    /// Mirrored node snapshots, keyed by accesskit node id. Servo sends
    /// incremental updates without removal records, so the backend tracks
    /// parent→child links itself: a node that disappears from its parent's
    /// children drops with its whole subtree instead of lingering as a
    /// ghost. Reset on document change (see `clear_tree`).
    nodes: HashMap<servo::accesskit::NodeId, StoredNode>,
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
    /// Stable numeric ids for live accesskit nodes (the UI keys attachments
    /// by u64). Pruned with each walk, cleared on document change, never
    /// reused.
    ids: HashMap<servo::accesskit::NodeId, u64>,
    next_id: u64,
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
    /// Verbose per-node AX diagnostics (`SDSC_AX_DEBUG=1`): role, bounds,
    /// and keep/drop verdict for every node in each tree update.
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
        if matches!(status, LoadStatus::Complete) {
            let mut shared = self.shared.borrow_mut();
            shared.load_started_at = None;
            let generation = shared.generation;
            shared.handle.emit(EngineEvent::LoadFinished {
                generation,
                result: Ok(()),
            });
        }
    }

    fn notify_accessibility_tree_update(
        &self,
        _webview: WebView,
        update: servo::accesskit::TreeUpdate,
    ) {
        // Servo sends incremental updates (node id, node) without removal
        // records, so the backend mirrors parent→child links itself: a node
        // that disappears from its parent's children drops with its whole
        // subtree instead of lingering as a ghost. Empty heartbeats are
        // ignored; the mirror resets on document change.
        //
        // (An accesskit_consumer mirror was tried first: Servo never sends
        // TreeId::ROOT — every update carries a per-document id — and the
        // consumer refuses to start from anything else. It also panics on
        // unknown grafts, which killed the worker 3x. Hand-rolled: no
        // panics possible, no tree_id semantics needed.)
        if update.nodes.is_empty() {
            return;
        }
        let mut shared = self.shared.borrow_mut();
        if shared.ax_debug {
            crate::platform::app_log::info(format!(
                "servo-ax update tree_id={:?} nodes={}",
                update.tree_id,
                update.nodes.len(),
            ));
            for (id, node) in &update.nodes {
                let kind = focusable_kind(node.role());
                let bounds = node.bounds();
                let kept = kind.and_then(|kind| {
                    bounds.and_then(|rect| {
                        let (w, h) = (rect.width() as f32, rect.height() as f32);
                        (w > 0.0
                            && h > 0.0
                            && w <= VIEWPORT_WIDTH as f32
                            && h <= VIEWPORT_HEIGHT as f32)
                            .then_some(kind)
                    })
                });
                let verdict = match (&kept, bounds) {
                    (Some(_), _) => "kept",
                    (None, None) => "drop:nobounds",
                    (None, Some(rect))
                        if rect.width() as f32 > VIEWPORT_WIDTH as f32
                            || rect.height() as f32 > VIEWPORT_HEIGHT as f32 =>
                    {
                        "drop:oversize"
                    }
                    (None, Some(_)) => "drop:role",
                };
                let b = bounds
                    .map(|r| {
                        format!(
                            "({},{}) {}x{}",
                            r.x0 as i32,
                            r.y0 as i32,
                            r.width() as i32,
                            r.height() as i32
                        )
                    })
                    .unwrap_or_else(|| "-".to_string());
                // Accessible name fragments identify WHICH element a
                // boundless drop is (truncated; usually empty).
                let name: String = node
                    .label()
                    .into_iter()
                    .chain(node.value())
                    .flat_map(|s| s.chars().take(40))
                    .collect();
                crate::platform::app_log::info(format!(
                    "servo-ax node id={id:?} role={:?} bounds={b} verdict={verdict} name={name:?}",
                    node.role(),
                ));
            }
        }
        apply_node_updates(&mut shared, &update.nodes);
        // Publish the translated tree (diffed + logged inside).
        emit_tree(&mut shared);
    }
}

/// Stored node snapshot: just enough to filter targets and resolve
/// removals (children links) without re-querying Servo.
#[derive(Debug, Clone)]
struct StoredNode {
    role: servo::accesskit::Role,
    bounds: Option<(f32, f32, f32, f32)>,
    children: Vec<servo::accesskit::NodeId>,
}

/// Collect snap targets from the mirrored map, plus the document extent
/// (max far corner over every bounded node) for scroll clamping. Sorted by
/// numeric id so diffing is stable.
fn collect_focusables(shared: &mut Shared) -> (Vec<Focusable>, (f32, f32)) {
    let mut out = Vec::new();
    let mut extent = (0.0f32, 0.0f32);
    let Shared {
        nodes,
        ids,
        next_id,
        ax_debug,
        ..
    } = &mut *shared;
    for (nid, stored) in nodes.iter() {
        if let Some((_, _, x1, y1)) = stored.bounds {
            extent.0 = extent.0.max(x1);
            extent.1 = extent.1.max(y1);
        }
        let kept = focusable_kind(stored.role).and_then(|kind| {
            stored.bounds.and_then(|(x0, y0, x1, y1)| {
                let (w, h) = (x1 - x0, y1 - y0);
                // Non-zero area and smaller than the viewport: a
                // full-page container is not a snap target.
                (w > 0.0 && h > 0.0 && w <= VIEWPORT_WIDTH as f32 && h <= VIEWPORT_HEIGHT as f32)
                    .then_some(Focusable {
                        id: 0,
                        x: x0,
                        y: y0,
                        w,
                        h,
                        kind,
                    })
            })
        });
        if let Some(mut item) = kept {
            // Stable numeric id for the UI.
            item.id = *ids.entry(*nid).or_insert_with(|| {
                let id = *next_id;
                *next_id += 1;
                if *ax_debug {
                    crate::platform::app_log::info(format!(
                        "servo-ax map #{id} <= {nid:?} role={:?}",
                        stored.role
                    ));
                }
                id
            });
            out.push(item);
        }
    }
    ids.retain(|k, _| nodes.contains_key(k));
    out.sort_by_key(|o| o.id);
    (out, extent)
}

/// Actionable accesskit roles, mapped to cursor kinds. Covers every HTML
/// input type (each is its own role: password, email, textarea, ...) plus
/// buttons of all flavors. Everything else is not worth a magnet (static
/// text, plain containers, images without actions).
fn focusable_kind(role: servo::accesskit::Role) -> Option<FocusKind> {
    use servo::accesskit::Role;
    match role {
        Role::Link | Role::DocBackLink => Some(FocusKind::Link),
        Role::Button
        | Role::DefaultButton
        | Role::CheckBox
        | Role::RadioButton
        | Role::Switch
        | Role::MenuItem
        | Role::MenuItemCheckBox
        | Role::MenuItemRadio
        | Role::MenuListOption
        | Role::Tab
        | Role::ListBoxOption
        | Role::TreeItem
        | Role::DisclosureTriangle
        | Role::ColorWell
        | Role::Slider
        | Role::SpinButton => Some(FocusKind::Button),
        Role::TextInput
        | Role::MultilineTextInput
        | Role::SearchInput
        | Role::PasswordInput
        | Role::EmailInput
        | Role::NumberInput
        | Role::UrlInput
        | Role::PhoneNumberInput
        | Role::DateInput
        | Role::DateTimeInput
        | Role::TimeInput
        | Role::MonthInput
        | Role::WeekInput
        | Role::ComboBox
        | Role::EditableComboBox => Some(FocusKind::Field),
        _ => None,
    }
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
    let servo_prefs = Preferences {
        accessibility_enabled: true,
        ..Default::default()
    };
    let servo = ServoBuilder::default()
        .preferences(servo_prefs)
        .event_loop_waker(Box::new(ThreadWaker { tx: wake_tx }))
        .build();
    let shared = Rc::new(RefCell::new(Shared {
        ctx: ctx.clone(),
        size: (VIEWPORT_WIDTH, VIEWPORT_HEIGHT),
        handle,
        generation: 0,
        load_started_at: None,
        logged_first_frame: false,
        nodes: HashMap::new(),
        last_emitted: Vec::new(),
        visible: false,
        suspended: true,
        scroll: (0.0, 0.0),
        last_sent_scroll: (0.0, 0.0),
        ids: HashMap::new(),
        next_id: 0,
        content: None,
        last_url: None,
        ax_debug: std::env::var("SDSC_AX_DEBUG").is_ok(),
    }));
    let webview = WebViewBuilder::new(&servo, ctx.clone())
        .delegate(Rc::new(Delegate {
            shared: shared.clone(),
        }))
        .build();
    // Accessibility tree feed for the magnetic cursor: focusables + bounds.
    // Needs the pref above; without it this returns None and no updates flow.
    if webview.set_accessibility_active(true).is_none() {
        crate::platform::app_log::warn("servo-engine: accessibility tree unavailable");
    }
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
        {
            let suspended = shared.borrow().suspended;
            if !suspended {
                servo.spin_event_loop();
            }
            check_load_timeout(&shared);
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
                    // Fresh page: clear_tree drops nodes, scroll and extent.
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
        EngineCommand::ScrollBy { dx, dy } => {
            let mut state = shared.borrow_mut();
            if state.suspended {
                return;
            }
            let (w, h) = state.size;
            webview.notify_scroll_event(
                Scroll::Delta(WebViewVector::Device(DeviceVector2D::new(dx, dy))),
                WebViewPoint::Device(DevicePoint::new(w as f32 / 2.0, h as f32 / 2.0)),
            );
            // Servo reports document-relative bounds and never re-emits on
            // scroll: accumulate the commanded offset and re-emit translated
            // rects so outlines track the page. `try_send` + subscription
            // conflation absorb held-stick bursts; the emit itself stays
            // silent unless nodes join/leave (see `emit_tree`).
            let (sx, sy) = state.scroll;
            set_scroll(&mut state, (sx + dx, sy + dy));
            emit_tree(&mut state);
        }
        EngineCommand::Key(event) => {
            if shared.borrow().suspended {
                return;
            }
            webview.notify_input_event(InputEvent::Keyboard(KeyboardEvent::new(event)));
        }
        EngineCommand::SetDarkMode { dark } => {
            webview.notify_theme_change(if dark { Theme::Dark } else { Theme::Light });
        }
        EngineCommand::GoBack => {
            let mut state = shared.borrow_mut();
            if state.suspended {
                return;
            }
            // History travel swaps the document: clear_tree drops nodes,
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
        EngineCommand::ClickAt { x, y } => {
            if shared.borrow().suspended {
                return;
            }
            // Viewport-space point (same space as the cursor and the
            // translated tree); Servo maps to document internally.
            crate::platform::app_log::info(format!("servo-engine: click at ({x:.0},{y:.0})"));
            let point = WebViewPoint::Device(DevicePoint::new(x, y));
            webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                MouseButtonAction::Down,
                MouseButton::Primary,
                point,
            )));
            webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                MouseButtonAction::Up,
                MouseButton::Primary,
                point,
            )));
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

/// Document changed (fresh load or history travel): drop nodes, scroll and
/// content extent. Session history does not reliably restore scroll, and
/// stale anything misplaces every outline (observed in testing).
fn clear_tree(state: &mut Shared) {
    if !state.nodes.is_empty() {
        state.nodes.clear();
        state.ids.clear();
        state.last_emitted = Vec::new();
        state.handle.emit(EngineEvent::A11yTree(Vec::new()));
    }
    state.content = None;
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

/// Update the tracked scroll offset and report it when it moves, so the UI
/// overlay can correlate cursor against targets.
fn set_scroll(state: &mut Shared, scroll: (f32, f32)) {
    let clamped = clamp_scroll(scroll, state.content);
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

/// Upsert changed nodes; drop subtrees a parent no longer lists (unless
/// re-sent in the same update — reparent, not removal). Recomputes the
/// document extent from the live set.
fn apply_node_updates(
    shared: &mut Shared,
    updates: &[(servo::accesskit::NodeId, servo::accesskit::Node)],
) {
    use std::collections::HashSet;
    let live: HashSet<servo::accesskit::NodeId> = updates.iter().map(|(id, _)| *id).collect();
    // Removals first: children the parent dropped and nobody re-sent.
    let mut dropped = Vec::new();
    for (id, node) in updates {
        if let Some(old) = shared.nodes.get(id) {
            let updated: HashSet<servo::accesskit::NodeId> =
                node.children().iter().copied().collect();
            for child in &old.children {
                if !updated.contains(child) && !live.contains(child) {
                    dropped.push(*child);
                }
            }
        }
    }
    for (id, node) in updates {
        shared.nodes.insert(
            *id,
            StoredNode {
                role: node.role(),
                bounds: node
                    .bounds()
                    .map(|r| (r.x0 as f32, r.y0 as f32, r.x1 as f32, r.y1 as f32)),
                children: node.children().to_vec(),
            },
        );
    }
    for id in dropped {
        remove_subtree(&mut shared.nodes, id, &live);
    }
    // Recompute the extent from the live set (removals shrink it too).
    let mut extent = (0.0f32, 0.0f32);
    for n in shared.nodes.values() {
        if let Some((_, _, x1, y1)) = n.bounds {
            extent.0 = extent.0.max(x1);
            extent.1 = extent.1.max(y1);
        }
    }
    shared.content = Some(extent);
}

/// Drop a subtree, never touching nodes re-sent in the same update.
fn remove_subtree(
    map: &mut HashMap<servo::accesskit::NodeId, StoredNode>,
    id: servo::accesskit::NodeId,
    live: &std::collections::HashSet<servo::accesskit::NodeId>,
) {
    if live.contains(&id) {
        return;
    }
    if let Some(node) = map.remove(&id) {
        for child in node.children {
            remove_subtree(map, child, live);
        }
    }
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
            "servo-engine: a11y focusables={}{sample}",
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
    fn translate_shifts_by_scroll_and_preserves_order() {
        let items = vec![item(2, 100.0, 800.0), item(1, 0.0, 0.0)];
        let out = translate_tree(items, (0.0, 240.0));
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].x, out[0].y), (100.0, 560.0));
        assert_eq!((out[1].x, out[1].y), (0.0, -240.0));
    }
}
