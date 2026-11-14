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
    /// Focusables keyed by accesskit node id. Servo sends incremental
    /// updates, so this map is the accumulated truth: upsert nodes that
    /// arrive, drop ids that arrive non-focusable, ignore empty (focus-only)
    /// heartbeats, clear on navigate.
    focusables: std::collections::HashMap<u64, Focusable>,
    /// Last list sent to the UI (diffed to avoid spamming identical trees).
    last_emitted: Vec<Focusable>,
    visible: bool,
    suspended: bool,
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
        self.shared.borrow().handle.emit(EngineEvent::UrlChanged {
            url: url.to_string(),
        });
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
        // Servo sends incremental updates (plus empty focus-only heartbeats),
        // so the backend owns the accumulated map: upsert changed nodes,
        // drop ids that arrive non-focusable, ignore empties, clear on
        // navigate (see `apply_command`). Replaces the whole list each time
        // flickered as pages streamed their trees in.
        if update.nodes.is_empty() {
            return;
        }
        let mut shared = self.shared.borrow_mut();
        for (id, node) in &update.nodes {
            // Role-typed actionables, plus anything Servo marks clickable
            // (Click action): div-buttons, image links, custom controls.
            let kind = focusable_kind(node.role()).or_else(|| {
                node.supports_action(servo::accesskit::Action::Click)
                    .then_some(FocusKind::Other)
            });
            match kind.and_then(|kind| {
                node.bounds().and_then(|rect| {
                    let (w, h) = (rect.width() as f32, rect.height() as f32);
                    // Non-zero area and smaller than the viewport: a
                    // full-page click listener is not a snap target.
                    (w > 0.0
                        && h > 0.0
                        && w <= VIEWPORT_WIDTH as f32
                        && h <= VIEWPORT_HEIGHT as f32)
                        .then_some(Focusable {
                            id: id.0,
                            x: rect.x0 as f32,
                            y: rect.y0 as f32,
                            w,
                            h,
                            kind,
                        })
                })
            }) {
                Some(item) => {
                    shared.focusables.insert(item.id, item);
                }
                None => {
                    shared.focusables.remove(&id.0);
                }
            }
        }
        let mut focusables: Vec<Focusable> = shared.focusables.values().cloned().collect();
        focusables.sort_by_key(|o| o.id);
        if focusables != shared.last_emitted {
            let sample = focusables
                .first()
                .map(|o| {
                    format!(
                        " first=({},{} {}x{})",
                        o.x as i32, o.y as i32, o.w as i32, o.h as i32
                    )
                })
                .unwrap_or_default();
            crate::platform::app_log::info(format!(
                "servo-engine: a11y focusables={}{sample}",
                focusables.len()
            ));
            shared.last_emitted = focusables.clone();
            shared.handle.emit(EngineEvent::A11yTree(focusables));
        }
    }
}

/// Actionable accesskit roles, mapped to cursor kinds. Covers every HTML
/// input type (each is its own role: password, email, textarea, ...) plus
/// buttons of all flavors. Clickable-but-untyped nodes (div-buttons, image
/// links) fall through to the Click-action check at the call site.
/// Everything else is not worth a magnet (static text, plain containers,
// images without actions).
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
        focusables: std::collections::HashMap::new(),
        last_emitted: Vec::new(),
        visible: false,
        suspended: true,
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
                    // Fresh page, fresh tree: drop the old page's nodes so
                    // no stale outline survives the navigation.
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
            // Destructure the reborrow so the two maps borrow disjointly
            // (`RefMut` field borrows don't split on their own).
            let Shared {
                focusables,
                last_emitted,
                ..
            } = &mut *state;
            shift_tree(focusables, last_emitted, dx, dy);
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
            // History travel swaps the document: same staleness as navigate.
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

/// Drop the accumulated tree (fresh page, history travel): stale outlines
/// must never survive a document change.
fn clear_tree(state: &mut Shared) {
    if state.focusables.is_empty() {
        return;
    }
    state.focusables.clear();
    state.last_emitted = Vec::new();
    state.handle.emit(EngineEvent::A11yTree(Vec::new()));
}

/// Shift cached rects by a commanded scroll: Servo reports bounds in
/// viewport coordinates but only re-emits changed nodes, so without this
/// the whole tree lags one scroll behind. Map and diff-baseline shift
/// together. Clamp drift (hitting top/bottom) and page-driven scrolls
/// self-heal on the next tree update.
fn shift_tree(
    focusables: &mut std::collections::HashMap<u64, Focusable>,
    last_emitted: &mut [Focusable],
    dx: f32,
    dy: f32,
) {
    if dx == 0.0 && dy == 0.0 {
        return;
    }
    for item in focusables.values_mut() {
        item.x -= dx;
        item.y -= dy;
    }
    for item in last_emitted.iter_mut() {
        item.x -= dx;
        item.y -= dy;
    }
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
    use std::collections::HashMap;

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
    fn scroll_shifts_cache_and_baseline_together() {
        let mut map: HashMap<u64, Focusable> = [(1, item(1, 100.0, 200.0))].into_iter().collect();
        let mut last = vec![item(1, 100.0, 200.0)];
        shift_tree(&mut map, &mut last, 0.0, 240.0);
        assert_eq!((map[&1].x, map[&1].y), (100.0, -40.0));
        assert_eq!((last[0].x, last[0].y), (100.0, -40.0));
        // No-op scroll leaves everything untouched.
        shift_tree(&mut map, &mut last, 0.0, 0.0);
        assert_eq!((map[&1].x, map[&1].y), (100.0, -40.0));
    }
}
