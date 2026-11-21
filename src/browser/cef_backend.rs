//! CEF offscreen-rendering backend (pivot phase 3).
//!
//! Mirrors the Servo worker's contract: one worker thread owns the engine,
//! commands arrive over `EngineHandle`'s channel, and events/frames come back
//! through the same sinks. CEF's multi-threaded message loop (Windows) proxies
//! browser-host calls from this thread.
//!
//! L1 paint: Chromium renders with its GPU and hands us a CPU BGRA buffer in
//! `on_paint`; we swizzle it to RGBA and write the tab's existing `FrameSlot`.
//! The stable-texture shader is untouched. L2 accelerated paint can replace
//! `on_paint` later without changing the seam.
//!
//! DOM targets: the UI still needs `Focusable` rects. A tiny process-message
//! bridge sends `dom_query_js` to the renderer, evaluates it in the frame's V8
//! context, and posts the JSON result back. The browser process parses it with
//! serde_json.

#[cfg(feature = "servo-engine")]
compile_error!(
    "cef-engine is mutually exclusive with servo-engine until phase 4 removes Servo;      build with `--no-default-features --features cef-engine`"
);

use crate::browser::cursor::{FocusKind, Focusable};
use crate::browser::dom;
use crate::browser::engine::{
    EngineActivity, EngineCommand, EngineEvent, EngineHandle, TabId, TabKey,
};
use crate::browser::frame_slot::FrameSlot;
use cef::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const DOM_QUERY_MSG: &str = "sdsc.dom.query";
const DOM_RESULT_MSG: &str = "sdsc.dom.result";
const DOM_POLL: Duration = Duration::from_millis(350);
const BROWSER_BG: u32 = 0xFF14_161C;

/// CEF is process-global; subprocess bootstrap and host init both need it.
static INIT: OnceLock<bool> = OnceLock::new();

fn cef_dir() -> Option<std::path::PathBuf> {
    let dir = cef::sys::get_cef_dir();
    if dir.is_none() {
        crate::platform::app_log::warn(
            "cef: binary distribution not found (set CEF_PATH or use the packaged sidecars)",
        );
    }
    dir
}

#[cfg(windows)]
fn set_dll_directory(dir: &std::path::Path) {
    use windows::Win32::System::LibraryLoader::SetDllDirectoryW;
    use windows::core::HSTRING;
    let wide = HSTRING::from(dir.as_os_str());
    // Ignore failure: an already-adjacent libcef.dll is fine.
    unsafe {
        let _ = SetDllDirectoryW(&wide);
    }
}

fn pin_api() {
    let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
}

/// CEF subprocess entry point. Must run before the iced app and before the
/// single-instance guard. Returns the subprocess exit code, or `None` for the
/// browser (host) process.
pub fn execute_subprocess() -> Option<i32> {
    let dir = cef_dir()?;
    #[cfg(windows)]
    set_dll_directory(&dir);
    pin_api();
    let args = cef::args::Args::new();
    let mut app = build_app();
    let code = execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    (code >= 0).then_some(code)
}

/// Initialize CEF on the calling (main) thread and start the engine worker.
pub fn spawn_engine(boot_url: &str, initial_size: (u32, u32)) -> Option<EngineHandle> {
    let dir = cef_dir()?;
    let initialized = *INIT.get_or_init(|| {
        #[cfg(windows)]
        set_dll_directory(&dir);
        pin_api();
        let cache = crate::persist::paths::data_dir().join("cef");
        let _ = std::fs::create_dir_all(&cache);
        let log = cache.join("cef.log");
        let mut settings = Settings {
            no_sandbox: 1,
            windowless_rendering_enabled: 1,
            multi_threaded_message_loop: cfg!(windows) as _,
            external_message_pump: cfg!(not(windows)) as _,
            persist_session_cookies: 1,
            log_severity: LogSeverity::WARNING,
            ..Settings::default()
        };
        settings.root_cache_path = cache.to_string_lossy().as_ref().into();
        settings.log_file = log.to_string_lossy().as_ref().into();
        settings.resources_dir_path = dir.to_string_lossy().as_ref().into();
        settings.locales_dir_path = dir.join("locales").to_string_lossy().as_ref().into();
        settings.locale = "en-US".into();
        settings.accept_language_list = "en-US,en".into();
        // A plain Chromium UA. CEF's default includes `Cef/...`, which bot
        // checks treat as a non-browser client.
        settings.user_agent = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
            AppleWebKit/537.36 (KHTML, like Gecko) Chrome/156.0.0.0 Safari/537.36"
            .into();
        let args = cef::args::Args::new();
        let mut app = build_app();
        let ok = initialize(
            Some(args.as_main_args()),
            Some(&settings),
            Some(&mut app),
            std::ptr::null_mut(),
        ) != 0;
        if ok {
            crate::platform::app_log::info(format!("cef: initialized from {}", dir.display()));
        } else {
            crate::platform::app_log::warn("cef: CefInitialize failed");
        }
        ok
    });
    if !initialized {
        return None;
    }

    let boot_url = boot_url.to_string();
    let mut size = initial_size;
    size.0 = size.0.max(1);
    size.1 = size.1.max(1);
    let (handle, rx) = EngineHandle::channel();
    let worker_handle = handle.clone();
    std::thread::Builder::new()
        .name("cef-engine".to_string())
        .stack_size(4 * 1024 * 1024)
        .spawn(move || run(rx, worker_handle, boot_url, size))
        .ok()?;
    Some(handle)
}

/// Shared by every CEF callback for one tab.
#[derive(Clone)]
struct TabContext {
    tab: TabId,
    frame: Arc<FrameSlot>,
    handle: EngineHandle,
    size: Arc<Mutex<(u32, u32)>>,
    generation: Arc<Mutex<u64>>,
    browser: BrowserSlot,
    active_tab: Arc<AtomicU64>,
    visible: Arc<AtomicBool>,
    last_mouse: Arc<Mutex<(f32, f32)>>,
    query_seq: Arc<AtomicU64>,
    last_dom_hash: Arc<Mutex<u64>>,
    last_active: Arc<Mutex<u64>>,
    scroll: Arc<Mutex<(f32, f32)>>,
    extent: Arc<Mutex<(f32, f32)>>,
}

/// `cef::Browser` is a ref-counted raw pointer without Rust `Send`; CEF's
/// Windows multi-threaded loop proxies host calls, matching welding's
/// contract. All access stays behind this mutex.
#[derive(Clone, Default)]
struct BrowserSlot(Arc<Mutex<Option<cef::Browser>>>);

unsafe impl Send for BrowserSlot {}
unsafe impl Sync for BrowserSlot {}

impl BrowserSlot {
    fn set(&self, browser: cef::Browser) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(browser);
    }

    fn get(&self) -> Option<cef::Browser> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

// ---------------------------------------------------------------------------
// App / renderer process handlers
// ---------------------------------------------------------------------------

cef::wrap_browser_process_handler! {
    struct SdscBrowserProcessHandler {}
    impl BrowserProcessHandler {}
}

cef::wrap_render_process_handler! {
    struct SdscRenderProcessHandler {}
    impl RenderProcessHandler {
        fn on_process_message_received(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _source_process: ProcessId,
            message: Option<&mut ProcessMessage>,
        ) -> std::os::raw::c_int {
            let (Some(frame), Some(message)) = (frame, message) else {
                return 0;
            };
            let name = CefString::from(&message.name()).to_string();
            if name != DOM_QUERY_MSG {
                return 0;
            }
            let Some(args) = message.argument_list() else {
                return 0;
            };
            let id = CefString::from(&args.string(0)).to_string();
            let script = CefString::from(&args.string(1)).to_string();
            let (ok, payload) = match frame.v8_context() {
                Some(ctx) => {
                    let code: CefString = script.as_str().into();
                    let url: CefString = "sdsc://dom".into();
                    let mut retval: Option<V8Value> = None;
                    let mut exception: Option<V8Exception> = None;
                    let ran = ctx.eval(
                        Some(&code),
                        Some(&url),
                        0,
                        Some(&mut retval),
                        Some(&mut exception),
                    );
                    if ran != 0 {
                        let value = retval
                            .as_ref()
                            .filter(|v| v.is_string() != 0)
                            .map(|v| CefString::from(&v.string_value()).to_string())
                            .unwrap_or_default();
                        (1, value)
                    } else {
                        let message = exception
                            .as_ref()
                            .map(|e| CefString::from(&e.message()).to_string())
                            .unwrap_or_else(|| "dom eval failed".to_string());
                        crate::platform::app_log::warn(format!(
                            "cef-dom: renderer eval failed: {message}"
                        ));
                        (0, message)
                    }
                }
                None => {
                    crate::platform::app_log::warn("cef-dom: renderer has no v8 context");
                    (0, "no v8 context".to_string())
                }
            };

            let Some(mut reply) = process_message_create(Some(&DOM_RESULT_MSG.into())) else {
                return 0;
            };
            if let Some(out) = reply.argument_list() {
                out.set_string(0, Some(&id.as_str().into()));
                out.set_int(1, ok);
                out.set_string(2, Some(&payload.as_str().into()));
            }
            frame.send_process_message(ProcessId::BROWSER, Some(&mut reply));
            1
        }
    }
}

fn build_app() -> App {
    SdscApp::new(
        SdscBrowserProcessHandler::new(),
        SdscRenderProcessHandler::new(),
    )
}

cef::wrap_app! {
    struct SdscApp {
        browser_process_handler: BrowserProcessHandler,
        render_process_handler: RenderProcessHandler,
    }
    impl App {
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(self.browser_process_handler.clone())
        }
        fn render_process_handler(&self) -> Option<RenderProcessHandler> {
            Some(self.render_process_handler.clone())
        }
    }
}

// ---------------------------------------------------------------------------
// Per-tab CEF handlers
// ---------------------------------------------------------------------------

cef::wrap_render_handler! {
    struct SdscRenderHandler {
        ctx: TabContext,
    }
    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            if let Some(rect) = rect {
                let (w, h) = *self.ctx.size.lock().unwrap_or_else(|e| e.into_inner());
                rect.x = 0;
                rect.y = 0;
                rect.width = w as i32;
                rect.height = h as i32;
            }
        }

        fn screen_info(
            &self,
            _browser: Option<&mut Browser>,
            info: Option<&mut ScreenInfo>,
        ) -> std::os::raw::c_int {
            let Some(info) = info else { return 0 };
            info.device_scale_factor = 1.0;
            1
        }

        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: std::os::raw::c_int,
            height: std::os::raw::c_int,
        ) {
            if type_ != PaintElementType::VIEW
                || buffer.is_null()
                || width <= 0
                || height <= 0
                // Background/suspended tabs must not burn CPU paint.
                || self.ctx.active_tab.load(Ordering::Acquire) != self.ctx.tab.0
                || !self.ctx.visible.load(Ordering::Acquire)
            {
                return;
            }
            let len = width as usize * height as usize * 4;
            // CEF's CPU buffer is BGRA (little-endian) on every supported
            // platform; the shader expects sRGB RGBA.
            let src = unsafe { std::slice::from_raw_parts(buffer, len) };
            let mut rgba = Vec::with_capacity(len);
            for px in src.as_chunks::<4>().0 {
                rgba.push(px[2]);
                rgba.push(px[1]);
                rgba.push(px[0]);
                rgba.push(px[3]);
            }
            self.ctx
                .frame
                .write(width as u32, height as u32, rgba);
        }

        fn on_scroll_offset_changed(
            &self,
            _browser: Option<&mut Browser>,
            x: f64,
            y: f64,
        ) {
            let now = (x as f32, y as f32);
            let before = *self.ctx.scroll.lock().unwrap_or_else(|e| e.into_inner());
            if before != now {
                *self.ctx.scroll.lock().unwrap_or_else(|e| e.into_inner()) = now;
                self.ctx.handle.emit(EngineEvent::ScrollOffset {
                    tab: self.ctx.tab,
                    x: now.0,
                    y: now.1,
                });
            }
        }
    }
}

cef::wrap_life_span_handler! {
    struct SdscLifeSpanHandler {
        ctx: TabContext,
    }
    impl LifeSpanHandler {
        #[allow(clippy::too_many_arguments)]
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: std::os::raw::c_int,
            _target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: std::os::raw::c_int,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut std::os::raw::c_int>,
        ) -> std::os::raw::c_int {
            // Engine-created tabs (window.open / target=_blank) need their own
            // UI-side tab adoption; deny until that lands rather than leaking
            // an untracked browser.
            1
        }

        fn on_after_created(&self, browser: Option<&mut Browser>) {
            let Some(browser) = browser else { return };
            self.ctx.browser.set(browser.clone());
            if let Some(host) = browser.host() {
                let show = self.ctx.active_tab.load(Ordering::Acquire) == self.ctx.tab.0
                    && self.ctx.visible.load(Ordering::Acquire);
                if !show {
                    host.was_hidden(1);
                }
                host.set_focus(show as i32);
                host.was_resized();
            }
        }

        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            self.ctx.handle.emit(EngineEvent::TabClosed { tab: self.ctx.tab });
        }
    }
}

cef::wrap_load_handler! {
    struct SdscLoadHandler {
        ctx: TabContext,
    }
    impl LoadHandler {
        fn on_loading_state_change(
            &self,
            _browser: Option<&mut Browser>,
            is_loading: std::os::raw::c_int,
            _can_go_back: std::os::raw::c_int,
            _can_go_forward: std::os::raw::c_int,
        ) {
            let phase = if is_loading != 0 {
                EngineActivity::Loading
            } else {
                EngineActivity::Idle
            };
            self.ctx.handle.emit(EngineEvent::ActivityChanged {
                tab: self.ctx.tab,
                phase,
            });
        }

        fn on_load_end(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _http_status_code: std::os::raw::c_int,
        ) {
            let is_main = frame.as_ref().is_some_and(|f| f.is_main() != 0);
            if !is_main {
                return;
            }
            let generation = *self.ctx.generation.lock().unwrap_or_else(|e| e.into_inner());
            self.ctx.handle.emit(EngineEvent::LoadFinished {
                tab: self.ctx.tab,
                generation,
                result: Ok(()),
            });
        }

        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _error_code: Errorcode,
            error_text: Option<&CefString>,
            _failed_url: Option<&CefString>,
        ) {
            let is_main = frame.as_ref().is_some_and(|f| f.is_main() != 0);
            if !is_main {
                return;
            }
            let generation = *self.ctx.generation.lock().unwrap_or_else(|e| e.into_inner());
            self.ctx.handle.emit(EngineEvent::LoadFinished {
                tab: self.ctx.tab,
                generation,
                result: Err(error_text
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "load failed".to_string())),
            });
        }
    }
}

cef::wrap_display_handler! {
    struct SdscDisplayHandler {
        ctx: TabContext,
    }
    impl DisplayHandler {
        fn on_address_change(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            url: Option<&CefString>,
        ) {
            if !frame.as_ref().is_some_and(|f| f.is_main() != 0) {
                return;
            }
            let url = url.map(|u| u.to_string()).unwrap_or_default();
            self.ctx.handle.emit(EngineEvent::UrlChanged {
                tab: self.ctx.tab,
                url,
            });
        }

        fn on_title_change(
            &self,
            _browser: Option<&mut Browser>,
            title: Option<&CefString>,
        ) {
            let title = title
                .map(|t| t.to_string())
                .filter(|t| !t.is_empty());
            self.ctx.handle.emit(EngineEvent::TitleChanged {
                tab: self.ctx.tab,
                title,
            });
        }
    }
}

cef::wrap_client! {
    struct SdscClient {
        ctx: TabContext,
        render_handler: RenderHandler,
        life_span_handler: LifeSpanHandler,
        load_handler: LoadHandler,
        display_handler: DisplayHandler,
    }
    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(self.render_handler.clone())
        }
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span_handler.clone())
        }
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.load_handler.clone())
        }
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(self.display_handler.clone())
        }

        fn on_process_message_received(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            source_process: ProcessId,
            message: Option<&mut ProcessMessage>,
        ) -> std::os::raw::c_int {
            if source_process != ProcessId::RENDERER {
                return 0;
            }
            let Some(message) = message else { return 0 };
            if CefString::from(&message.name()).to_string() != DOM_RESULT_MSG {
                return 0;
            }
            let Some(args) = message.argument_list() else {
                return 0;
            };
            let _id = CefString::from(&args.string(0)).to_string();
            let ok = args.int(1) != 0;
            let payload = CefString::from(&args.string(2)).to_string();
            if !ok {
                crate::platform::app_log::warn(format!(
                    "cef-dom: query failed tab{}: {payload}",
                    self.ctx.tab.0
                ));
                return 1;
            }
            handle_dom_json(&self.ctx, &payload);
            1
        }
    }
}

/// Parse the `dom_query_js` JSON and publish cursor targets + scroll truth.
fn handle_dom_json(ctx: &TabContext, json: &str) {
    let hash = fnv1a(json.as_bytes());
    if *ctx.last_dom_hash.lock().unwrap_or_else(|e| e.into_inner()) == hash {
        return;
    }
    *ctx.last_dom_hash.lock().unwrap_or_else(|e| e.into_inner()) = hash;

    let Ok(root) = serde_json::from_str::<serde_json::Value>(json) else {
        return;
    };
    let num = |key: &str| root.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
    let scroll = (num("sx"), num("sy"));
    let extent = (num("dw"), num("dh"));
    let active = num("ax").max(0.0) as u64;

    let mut items = Vec::new();
    if let Some(rows) = root.get("nodes").and_then(|v| v.as_array()) {
        for row in rows {
            let Some(cols) = row.as_array() else { continue };
            let id = cols.first().and_then(|v| v.as_f64()).unwrap_or(-1.0);
            let x = cols.get(4).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
            let y = cols.get(5).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
            let w = cols.get(6).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
            let h = cols.get(7).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
            if id < 0.0 || !(w > 0.0 && h > 0.0) || !(x.is_finite() && y.is_finite()) {
                continue;
            }
            let tag = cols.get(1).and_then(|v| v.as_str()).unwrap_or("");
            let role = cols.get(2).and_then(|v| v.as_str()).unwrap_or("");
            let typ = cols.get(3).and_then(|v| v.as_str()).unwrap_or("");
            let url = cols
                .get(9)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            items.push(Focusable {
                id: id as u64,
                x,
                y,
                w,
                h,
                kind: dom_kind(tag, role, typ),
                url,
            });
        }
    }

    let before_scroll = *ctx.scroll.lock().unwrap_or_else(|e| e.into_inner());
    let before_extent = *ctx.extent.lock().unwrap_or_else(|e| e.into_inner());
    let before_active = *ctx.last_active.lock().unwrap_or_else(|e| e.into_inner());

    if before_scroll != scroll {
        *ctx.scroll.lock().unwrap_or_else(|e| e.into_inner()) = scroll;
        ctx.handle.emit(EngineEvent::ScrollOffset {
            tab: ctx.tab,
            x: scroll.0,
            y: scroll.1,
        });
    }
    if before_extent != extent {
        *ctx.extent.lock().unwrap_or_else(|e| e.into_inner()) = extent;
        ctx.handle.emit(EngineEvent::ScrollExtent {
            tab: ctx.tab,
            extent,
        });
    }
    ctx.handle.emit(EngineEvent::A11yTree {
        tab: ctx.tab,
        items,
    });
    if active != 0 && active != before_active {
        *ctx.last_active.lock().unwrap_or_else(|e| e.into_inner()) = active;
        ctx.handle.emit(EngineEvent::FocusChanged {
            tab: ctx.tab,
            id: active,
        });
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |acc, b| {
        acc.wrapping_mul(0x0000_0100_0000_01b3) ^ u64::from(*b)
    })
}

fn dom_kind(tag: &str, role: &str, typ: &str) -> FocusKind {
    match role {
        "link" | "doc-backlink" => return FocusKind::Link,
        "button"
        | "menuitem"
        | "tab"
        | "switch"
        | "checkbox"
        | "radio"
        | "menuitemcheckbox"
        | "menuitemradio"
        | "menu-listbox-option"
        | "option" => return FocusKind::Button,
        "textbox" | "searchbox" | "combobox" => return FocusKind::Field,
        _ => {}
    }
    match tag {
        "a" => FocusKind::Link,
        "button" => FocusKind::Button,
        "textarea" | "select" => FocusKind::Field,
        "input" => match typ {
            "checkbox" | "radio" | "submit" | "button" | "reset" | "image" | "color" | "range" => {
                FocusKind::Button
            }
            _ => FocusKind::Field,
        },
        _ => FocusKind::Other,
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

struct TabEntry {
    ctx: TabContext,
    generation: u64,
    url: String,
    last_dom: Instant,
    dom_epoch: u64,
}

struct Worker {
    rx: std::sync::mpsc::Receiver<EngineCommand>,
    handle: EngineHandle,
    size: (u32, u32),
    tabs: HashMap<u64, TabEntry>,
    active: TabId,
    visible: bool,
    suspended: bool,
    active_tab: Arc<AtomicU64>,
    visible_flag: Arc<AtomicBool>,
}

fn run(
    rx: std::sync::mpsc::Receiver<EngineCommand>,
    handle: EngineHandle,
    boot_url: String,
    size: (u32, u32),
) {
    let active_tab = Arc::new(AtomicU64::new(TabId::BOOT.0));
    let visible_flag = Arc::new(AtomicBool::new(false));
    let mut worker = Worker {
        rx,
        handle,
        size,
        tabs: HashMap::new(),
        active: TabId::BOOT,
        visible: false,
        suspended: true,
        active_tab,
        visible_flag,
    };
    worker.create_tab(TabId::BOOT, &boot_url, Arc::new(FrameSlot::default()), 1);
    loop {
        match worker.rx.recv_timeout(Duration::from_millis(16)) {
            Ok(command) => {
                if worker.apply(command) {
                    break;
                }
                // Drain anything already queued before the DOM tick.
                while let Ok(next) = worker.rx.try_recv() {
                    if worker.apply(next) {
                        return;
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        worker.tick_dom();
    }
    // CEF shutdown is process-global and must run on the initializing thread
    // in the strictest reading of the API. The caller exits immediately after
    // acking, so cache/cookie persistence is left to CEF's own flush.
    crate::platform::app_log::info("cef: worker stopped");
}

impl Worker {
    fn create_tab(&mut self, tab: TabId, url: &str, frame: Arc<FrameSlot>, generation: u64) {
        let ctx = TabContext {
            tab,
            frame,
            handle: self.handle.clone(),
            size: Arc::new(Mutex::new(self.size)),
            generation: Arc::new(Mutex::new(generation)),
            browser: BrowserSlot::default(),
            active_tab: self.active_tab.clone(),
            visible: self.visible_flag.clone(),
            last_mouse: Arc::new(Mutex::new((0.0, 0.0))),
            query_seq: Arc::new(AtomicU64::new(0)),
            last_dom_hash: Arc::new(Mutex::new(0)),
            last_active: Arc::new(Mutex::new(0)),
            scroll: Arc::new(Mutex::new((0.0, 0.0))),
            extent: Arc::new(Mutex::new((0.0, 0.0))),
        };
        let mut client = SdscClient::new(
            ctx.clone(),
            SdscRenderHandler::new(ctx.clone()),
            SdscLifeSpanHandler::new(ctx.clone()),
            SdscLoadHandler::new(ctx.clone()),
            SdscDisplayHandler::new(ctx.clone()),
        );
        let window_info = WindowInfo {
            windowless_rendering_enabled: 1,
            ..WindowInfo::default()
        };
        let settings = BrowserSettings {
            windowless_frame_rate: 60,
            background_color: BROWSER_BG,
            ..BrowserSettings::default()
        };
        let url_c: CefString = url.into();
        let started = browser_host_create_browser(
            Some(&window_info),
            Some(&mut client),
            Some(&url_c),
            Some(&settings),
            None,
            None,
        );
        self.tabs.insert(
            tab.0,
            TabEntry {
                ctx,
                generation,
                url: url.to_string(),
                last_dom: Instant::now(),
                dom_epoch: 0,
            },
        );
        if started == 0 {
            crate::platform::app_log::warn(format!(
                "cef: browser_host_create_browser failed tab{}",
                tab.0
            ));
        }
    }

    fn tab(&self, tab: TabId) -> Option<&TabEntry> {
        self.tabs.get(&tab.0)
    }

    fn tab_mut(&mut self, tab: TabId) -> Option<&mut TabEntry> {
        self.tabs.get_mut(&tab.0)
    }

    fn active_host(&self) -> Option<BrowserHost> {
        self.tab(self.active)?.ctx.browser.get()?.host()
    }

    fn apply(&mut self, command: EngineCommand) -> bool {
        match command {
            EngineCommand::Navigate {
                tab,
                url,
                generation,
            } => {
                if let Some(entry) = self.tab_mut(tab) {
                    entry.generation = generation;
                    entry.url = url.clone();
                    entry.dom_epoch = entry.dom_epoch.wrapping_add(1);
                    *entry
                        .ctx
                        .generation
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = generation;
                    if let Some(browser) = entry.ctx.browser.get() {
                        if let Some(frame) = browser.main_frame() {
                            let url_c: CefString = url.as_str().into();
                            frame.load_url(Some(&url_c));
                        }
                    } else {
                        // Browser creation is async; recreate with the URL.
                        let frame = entry.ctx.frame.clone();
                        self.create_tab(tab, &url, frame, generation);
                    }
                } else {
                    self.create_tab(tab, &url, Arc::new(FrameSlot::default()), generation);
                }
            }
            EngineCommand::ScrollBy { tab, dx, dy, x, y } => {
                let Some(entry) = self.tab(tab) else {
                    return false;
                };
                if let Some(browser) = entry.ctx.browser.get()
                    && let Some(host) = browser.host()
                {
                    let ev = MouseEvent {
                        x: x as i32,
                        y: y as i32,
                        modifiers: 0,
                    };
                    // CEF wheel deltas are ±120-ish units; convert pixel
                    // intent from the pad scroll path.
                    let wheel_y = (-(dy / 100.0) * 120.0).round() as i32;
                    let wheel_x = (-(dx / 100.0) * 120.0).round() as i32;
                    host.send_mouse_wheel_event(Some(&ev), wheel_x, wheel_y);
                }
            }
            EngineCommand::Key(key) => {
                if let Some(entry) = self.tab(key.tab)
                    && let Some(browser) = entry.ctx.browser.get()
                    && let Some(host) = browser.host()
                {
                    send_key(&host, &key);
                }
            }
            EngineCommand::SetDarkMode { .. } => {}
            EngineCommand::MouseMove { tab, x, y } => {
                if let Some(entry) = self.tab(tab) {
                    *entry
                        .ctx
                        .last_mouse
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = (x, y);
                    if let Some(browser) = entry.ctx.browser.get()
                        && let Some(host) = browser.host()
                    {
                        host.send_mouse_move_event(
                            Some(&MouseEvent {
                                x: x as i32,
                                y: y as i32,
                                modifiers: 0,
                            }),
                            0,
                        );
                    }
                }
            }
            EngineCommand::ReseedDom { tab } => {
                if let Some(entry) = self.tab_mut(tab) {
                    entry.last_dom = Instant::now() - DOM_POLL;
                }
            }
            EngineCommand::ClickAt { tab, x, y, target } => {
                let Some(entry) = self.tab(tab) else {
                    return false;
                };
                let Some(browser) = entry.ctx.browser.get() else {
                    return false;
                };
                let Some(frame) = browser.main_frame() else {
                    return false;
                };
                if let Some(id) = target {
                    let code = format!("window.__sdsc_click && window.__sdsc_click({id})");
                    let code: CefString = code.as_str().into();
                    let url: CefString = "sdsc://click".into();
                    frame.execute_java_script(Some(&code), Some(&url), 0);
                } else if let Some(host) = browser.host() {
                    let ev = MouseEvent {
                        x: x as i32,
                        y: y as i32,
                        modifiers: 0,
                    };
                    host.send_mouse_click_event(Some(&ev), MouseButtonType::LEFT, 0, 1);
                    host.send_mouse_click_event(Some(&ev), MouseButtonType::LEFT, 1, 1);
                }
            }
            EngineCommand::ProbeEventLoop { tab } => {
                if let Some(entry) = self.tab(tab)
                    && let Some(browser) = entry.ctx.browser.get()
                    && let Some(frame) = browser.main_frame()
                {
                    let code: CefString = dom::PROBE_ARM_JS.into();
                    let url: CefString = "sdsc://probe".into();
                    frame.execute_java_script(Some(&code), Some(&url), 0);
                }
            }
            EngineCommand::GoBack { tab, steps } => {
                if let Some(entry) = self.tab(tab)
                    && let Some(browser) = entry.ctx.browser.get()
                {
                    for _ in 0..steps.max(1) {
                        browser.go_back();
                    }
                }
            }
            EngineCommand::GoForward { tab, steps } => {
                if let Some(entry) = self.tab(tab)
                    && let Some(browser) = entry.ctx.browser.get()
                {
                    for _ in 0..steps.max(1) {
                        browser.go_forward();
                    }
                }
            }
            EngineCommand::OpenTab {
                tab,
                url,
                generation,
                frame,
            } => {
                self.create_tab(tab, &url, frame, generation);
            }
            EngineCommand::CloseTab { tab } => {
                if let Some(entry) = self.tabs.remove(&tab.0)
                    && let Some(browser) = entry.ctx.browser.get()
                    && let Some(host) = browser.host()
                {
                    host.close_browser(1);
                }
                if self.active == tab {
                    self.active = self
                        .tabs
                        .keys()
                        .next()
                        .map(|id| TabId(*id))
                        .unwrap_or(TabId::BOOT);
                }
            }
            EngineCommand::SetActiveTab { tab } => {
                if !self.tabs.contains_key(&tab.0) {
                    return false;
                }
                self.active = tab;
                self.active_tab.store(tab.0, Ordering::Release);
                let show = self.visible && !self.suspended;
                for (id, entry) in &self.tabs {
                    let on = *id == tab.0;
                    if !on {
                        entry.ctx.visible.store(false, Ordering::Release);
                    }
                    if let Some(browser) = entry.ctx.browser.get()
                        && let Some(host) = browser.host()
                    {
                        host.set_focus(on as i32);
                        host.was_hidden((!(on && show)) as i32);
                    }
                }
                if show {
                    if let Some(entry) = self.tab(tab) {
                        entry.ctx.visible.store(true, Ordering::Release);
                    }
                    self.emit_frame_slot(tab);
                }
            }
            EngineCommand::Resize { width, height } => {
                let size = (width.max(1), height.max(1));
                if self.size == size {
                    return false;
                }
                self.size = size;
                for entry in self.tabs.values_mut() {
                    *entry.ctx.size.lock().unwrap_or_else(|e| e.into_inner()) = size;
                }
                if let Some(host) = self.active_host() {
                    host.was_resized();
                    host.invalidate(PaintElementType::VIEW);
                }
            }
            EngineCommand::SetVisible { visible } => {
                self.visible = visible;
                self.visible_flag
                    .store(visible && !self.suspended, Ordering::Release);
                if visible && !self.suspended {
                    self.emit_frame_slot(self.active);
                    if let Some(entry) = self.tab(self.active) {
                        entry.ctx.visible.store(true, Ordering::Release);
                    }
                    if let Some(host) = self.active_host() {
                        host.was_hidden(0);
                        host.set_focus(1);
                    }
                } else {
                    if let Some(entry) = self.tab(self.active) {
                        entry.ctx.visible.store(false, Ordering::Release);
                    }
                    if let Some(host) = self.active_host() {
                        host.was_hidden(1);
                    }
                }
            }
            EngineCommand::Suspend => {
                self.suspended = true;
                self.visible_flag.store(false, Ordering::Release);
                if let Some(entry) = self.tab(self.active) {
                    entry.ctx.visible.store(false, Ordering::Release);
                }
                if let Some(host) = self.active_host() {
                    host.was_hidden(1);
                }
            }
            EngineCommand::Resume => {
                self.suspended = false;
                let show = self.visible;
                self.visible_flag.store(show, Ordering::Release);
                if show {
                    self.emit_frame_slot(self.active);
                    if let Some(entry) = self.tab(self.active) {
                        entry.ctx.visible.store(true, Ordering::Release);
                    }
                    if let Some(host) = self.active_host() {
                        host.was_hidden(0);
                        host.set_focus(1);
                    }
                }
            }
            EngineCommand::Shutdown { done } => {
                for entry in self.tabs.values() {
                    if let Some(browser) = entry.ctx.browser.get()
                        && let Some(host) = browser.host()
                    {
                        host.close_browser(1);
                    }
                }
                let _ = done.send(());
                return true;
            }
            EngineCommand::Wake => {}
        }
        false
    }

    fn emit_frame_slot(&self, tab: TabId) {
        if let Some(entry) = self.tab(tab) {
            self.handle.emit(EngineEvent::FrameSlot {
                tab,
                slot: entry.ctx.frame.clone(),
            });
        }
    }

    fn tick_dom(&mut self) {
        if !self.visible || self.suspended {
            return;
        }
        let Some(entry) = self.tab_mut(self.active) else {
            return;
        };
        if entry.last_dom.elapsed() < DOM_POLL {
            return;
        }
        entry.last_dom = Instant::now();
        entry.dom_epoch = entry.dom_epoch.wrapping_add(1);
        let Some(browser) = entry.ctx.browser.get() else {
            return;
        };
        let Some(frame) = browser.main_frame() else {
            return;
        };
        let id = entry
            .ctx
            .query_seq
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let center = *entry
            .ctx
            .last_mouse
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let center = (center.0 > 0.0 || center.1 > 0.0).then_some(center);
        let js = dom::dom_query_js_json(center, entry.dom_epoch);
        let Some(mut msg) = process_message_create(Some(&DOM_QUERY_MSG.into())) else {
            return;
        };
        if let Some(args) = msg.argument_list() {
            args.set_string(0, Some(&id.to_string().as_str().into()));
            args.set_string(1, Some(&js.as_str().into()));
        }
        frame.send_process_message(ProcessId::RENDERER, Some(&mut msg));
    }
}

// ---------------------------------------------------------------------------
// Input translation
// ---------------------------------------------------------------------------

fn modifiers(m: keyboard_types::Modifiers) -> u32 {
    let mut flags = 0u32;
    if m.contains(keyboard_types::Modifiers::SHIFT) {
        flags |= 1 << 1;
    }
    if m.contains(keyboard_types::Modifiers::CONTROL) {
        flags |= 1 << 2;
    }
    if m.contains(keyboard_types::Modifiers::ALT) {
        flags |= 1 << 3;
    }
    if m.contains(keyboard_types::Modifiers::META) {
        flags |= 1 << 7;
    }
    flags
}

fn send_key(host: &BrowserHost, key: &TabKey) {
    let (vk, ch) = key_code(&key.event.key);
    let down = key.event.state == keyboard_types::KeyState::Down;
    let type_ = if down {
        if ch != 0 {
            KeyEventType::KEYDOWN
        } else {
            KeyEventType::RAWKEYDOWN
        }
    } else {
        KeyEventType::KEYUP
    };
    let event = KeyEvent {
        type_,
        modifiers: modifiers(key.event.modifiers),
        windows_key_code: vk,
        native_key_code: vk,
        character: ch,
        unmodified_character: ch,
        ..KeyEvent::default()
    };
    host.send_key_event(Some(&event));
}

fn key_code(key: &keyboard_types::Key) -> (i32, u16) {
    use keyboard_types::{Key, NamedKey};
    match key {
        Key::Character(text) => {
            let mut chars = text.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else {
                return (0, 0);
            };
            if c == ' ' {
                (0x20, ' ' as u16)
            } else if c.is_ascii_alphanumeric() {
                let vk = u32::from(c.to_ascii_uppercase());
                (vk as i32, c as u16)
            } else {
                (0, c as u16)
            }
        }
        Key::Named(named) => {
            let (vk, ch) = match named {
                NamedKey::Backspace => (0x08, 0),
                NamedKey::Tab => (0x09, 0),
                NamedKey::Enter => (0x0D, 0),
                NamedKey::Escape => (0x1B, 0),
                NamedKey::PageUp => (0x21, 0),
                NamedKey::PageDown => (0x22, 0),
                NamedKey::End => (0x23, 0),
                NamedKey::Home => (0x24, 0),
                NamedKey::ArrowLeft => (0x25, 0),
                NamedKey::ArrowUp => (0x26, 0),
                NamedKey::ArrowRight => (0x27, 0),
                NamedKey::ArrowDown => (0x28, 0),
                NamedKey::Insert => (0x2D, 0),
                NamedKey::Delete => (0x2E, 0),
                NamedKey::F1 => (0x70, 0),
                NamedKey::F2 => (0x71, 0),
                NamedKey::F3 => (0x72, 0),
                NamedKey::F4 => (0x73, 0),
                NamedKey::F5 => (0x74, 0),
                NamedKey::F6 => (0x75, 0),
                NamedKey::F7 => (0x76, 0),
                NamedKey::F8 => (0x77, 0),
                NamedKey::F9 => (0x78, 0),
                NamedKey::F10 => (0x79, 0),
                NamedKey::F11 => (0x7A, 0),
                NamedKey::F12 => (0x7B, 0),
                _ => (0, 0),
            };
            (vk, ch)
        }
    }
}

#[allow(dead_code)]
fn activity_label(phase: EngineActivity) -> &'static str {
    phase.label()
}
