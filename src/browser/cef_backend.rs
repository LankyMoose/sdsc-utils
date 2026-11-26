//! CEF offscreen-rendering backend (pivot phase 3).
//!
//! One worker thread owns the engine,
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

use crate::browser::cursor::{FocusKind, Focusable};
use crate::browser::dom;
use crate::browser::engine::{
    BrowserCursor, BrowserMouseButton, EngineActivity, EngineCommand, EngineEvent, EngineHandle,
    TabId, TabKey,
};
use crate::browser::frame_slot::FrameSlot;
use cef::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DOM_QUERY_MSG: &str = "sdsc.dom.query";
const DOM_RESULT_MSG: &str = "sdsc.dom.result";
const DOM_POLL: Duration = Duration::from_millis(350);
/// Trusted link click -> navigation-start budget before the JS fallback.
const CLICK_FALLBACK: Duration = Duration::from_millis(700);
const BROWSER_BG: u32 = 0xFFFF_FFFF;

/// CEF is process-global; subprocess bootstrap and host init both need it.
/// `None` = not attempted, `Some(true)` = live, `Some(false)` = retryable
/// (usually a missing runtime that was installed afterwards).
static INIT: Mutex<Option<bool>> = Mutex::new(None);

/// Runtime directory actually loaded into this process. Uninstall/reinstall
/// keeps the old files mapped until exit, so the installer must not delete
/// this directory mid-session.
static LOADED_DIR: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);

pub(crate) fn loaded_engine_dir() -> Option<std::path::PathBuf> {
    LOADED_DIR
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone()
}

/// Resolve the CEF runtime directory. Search order:
/// 1. `CEF_PATH` (explicit/dev override; may point at the versioned parent).
/// 2. `cef\` next to the executable (portable bundle).
/// 3. The `BrowserEngine` optional MSIX package (`...\cef`).
/// 4. The local versioned store (`cef-engine.current` → `cef-engine-<stamp>`).
/// 5. The build-time `OUT_DIR` fallback (debug builds only).
///
/// A directory is only accepted when the runtime files CEF must load exist;
/// an empty/stale directory must not report the engine as installed.
pub fn resolve_engine_dir() -> Option<std::path::PathBuf> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(path) = std::env::var_os("CEF_PATH") {
        let path = std::path::PathBuf::from(path);
        candidates.push(path.join("cef_windows_x86_64"));
        candidates.push(path.join("cef"));
        candidates.push(path);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("cef"));
    }
    // QA/dev mock of the optional MSIX package: same resolver slot as the
    // real package, so priority and managed-package behavior are testable
    // without sideloading. `SDSC_MSIX_ENGINE_DIR` may point at the engine
    // directory itself or at a package root containing `cef\`.
    if let Some(root) = msix_mock_dir() {
        candidates.push(root.join("cef"));
        candidates.push(root);
    }
    #[cfg(windows)]
    candidates.extend(optional_package_dirs());
    if let Some(local) =
        crate::browser::engine_install::active_local_dir(&crate::persist::paths::data_dir())
    {
        candidates.push(local);
    }
    // Build-time distribution directory. It exists on the build machine
    // (including `cargo run --release`); portable/Store installs resolve via
    // the sidecar/package paths above instead. QA launches set
    // `SDSC_CEF_NO_BUILD_FALLBACK` to simulate a clean machine.
    let build_fallback = !(cfg!(any(debug_assertions, feature = "qa-feed"))
        && std::env::var_os("SDSC_CEF_NO_BUILD_FALLBACK").is_some());
    if build_fallback && let Some(dir) = cef::sys::get_cef_dir() {
        candidates.push(dir);
    }
    candidates
        .into_iter()
        .find(|candidate| validate_engine_dir(candidate))
}

pub fn engine_installed() -> bool {
    resolve_engine_dir().is_some()
}

/// Mock root for the optional `BrowserEngine` MSIX package. Set by the QA
/// console (and available to developers) to exercise resolver priority and
/// managed-package behavior without a sideload.
pub fn msix_mock_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("SDSC_MSIX_ENGINE_DIR")
        .map(std::path::PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

/// True when the runtime is package-owned (real MSIX or the resolver mock)
/// rather than a local/portable install the app may uninstall itself.
pub fn engine_is_managed(path: &std::path::Path) -> bool {
    if let Some(root) = msix_mock_dir()
        && path.starts_with(&root)
    {
        return true;
    }
    #[cfg(windows)]
    {
        let text = path.to_string_lossy().to_ascii_lowercase();
        if text.contains("\\windowsapps\\") {
            return true;
        }
    }
    false
}

pub(crate) fn validate_engine_dir(dir: &std::path::Path) -> bool {
    #[cfg(windows)]
    {
        dir.join("libcef.dll").is_file()
            && dir.join("icudtl.dat").is_file()
            && dir.join("resources.pak").is_file()
            && dir.join("locales").join("en-US.pak").is_file()
    }
    #[cfg(target_os = "macos")]
    {
        dir.join("Chromium Embedded Framework.framework").is_dir()
            && dir
                .join("Chromium Embedded Framework.framework")
                .join("Chromium Embedded Framework")
                .is_file()
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        dir.join("libcef.so").is_file()
            && dir.join("icudtl.dat").is_file()
            && dir.join("resources.pak").is_file()
    }
}

#[cfg(windows)]
fn optional_package_dirs() -> Vec<std::path::PathBuf> {
    use windows::ApplicationModel::Package;
    use windows::Management::Deployment::PackageManager;
    use windows::core::HSTRING;

    const ENGINE_PACKAGE: &str = "LankyMoose.SDSCUtils.BrowserEngine";
    let wanted = HSTRING::from(ENGINE_PACKAGE);
    let mut out = Vec::new();
    let mut push = |pkg: Package| {
        if pkg
            .Id()
            .and_then(|id| id.Name())
            .is_ok_and(|name| name == wanted)
            && let Ok(location) = pkg.InstalledLocation()
            && let Ok(path) = location.Path()
        {
            let root = std::path::PathBuf::from(path.to_string());
            out.push(root.join("cef"));
            out.push(root);
        }
    };
    if let Ok(current) = Package::Current()
        && let Ok(deps) = current.Dependencies()
    {
        for dep in deps {
            push(dep);
        }
    }
    if let Ok(manager) = PackageManager::new()
        && let Ok(packages) = manager.FindPackages()
    {
        for pkg in packages {
            push(pkg);
        }
    }
    out
}

fn cef_dir() -> Option<std::path::PathBuf> {
    let dir = resolve_engine_dir();
    if dir.is_none() {
        crate::platform::app_log::warn(
            "cef: runtime not found (portable sidecars, BrowserEngine package, or LocalAppData)",
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

/// Initialize CEF once on the calling (main) thread. Retryable when the
/// runtime was missing on the first attempt (install-then-open).
fn initialize_cef(dir: &std::path::Path) -> bool {
    #[cfg(windows)]
    set_dll_directory(dir);
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
        *LOADED_DIR.lock().unwrap_or_else(|err| err.into_inner()) = Some(dir.to_path_buf());
        crate::platform::app_log::info(format!("cef: initialized from {}", dir.display()));
    } else {
        crate::platform::app_log::warn("cef: CefInitialize failed");
    }
    ok
}

/// Initialize CEF on the calling (main) thread and start the engine worker.
pub fn spawn_engine(boot_url: &str, initial_size: (u32, u32)) -> Option<EngineHandle> {
    let dir = cef_dir()?;
    let initialized = {
        let mut guard = INIT.lock().unwrap_or_else(|e| e.into_inner());
        if *guard == Some(true) {
            true
        } else {
            let ok = initialize_cef(&dir);
            *guard = Some(ok);
            ok
        }
    };
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

/// Trusted link click awaiting a navigation-start callback. If CEF reports
/// no load/address change before the deadline, the worker falls back to the
/// old JS element click.
struct PendingClick {
    target: u64,
    at: Instant,
}

/// One in-flight DevTools method call. Exact history traversal is two
/// round trips: resolve the CDP entry id for a session index, then jump.
#[derive(Clone, Copy)]
enum DevToolsRequest {
    GetHistory { index: usize },
    Navigate { index: usize },
}

/// DevTools message ids are assigned here instead of by CEF: with the
/// multi-threaded message loop `ExecuteDevToolsMethod` posts the call to the
/// browser UI thread and returns 0, so the caller must know the id up front
/// to match the asynchronous result.
struct DevToolsState {
    next_id: AtomicI32,
    pending: Mutex<HashMap<i32, DevToolsRequest>>,
}

impl Default for DevToolsState {
    fn default() -> Self {
        Self {
            next_id: AtomicI32::new(1),
            pending: Mutex::new(HashMap::new()),
        }
    }
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
    last_editable: Arc<Mutex<Option<bool>>>,
    scroll: Arc<Mutex<(f32, f32)>>,
    extent: Arc<Mutex<(f32, f32)>>,
    /// Last icon candidate set fetched for this tab (dedupe repeated
    /// `on_favicon_urlchange` notifications).
    favicon_key: Arc<Mutex<String>>,
    /// Invalidates in-flight favicon callbacks after navigation/closure.
    favicon_request: Arc<AtomicU64>,
    /// Trusted link click awaiting navigation; fallback JS click on timeout.
    pending_click: Arc<Mutex<Option<PendingClick>>>,
    /// Last canonical history snapshot hash, so address/title/load callbacks
    /// do not emit duplicate HistoryChanged events.
    last_history_hash: Arc<Mutex<u64>>,
    /// Exact history traversal through the DevTools protocol.
    devtools: Arc<DevToolsState>,
    /// Last page cursor shape, so repeated CEF notifications are deduped.
    last_cursor: Arc<Mutex<crate::browser::engine::BrowserCursor>>,
    /// Causal flow of the last traced navigation/history command. CEF
    /// callbacks join engine log lines to the UI action that caused them.
    trace_flow: Arc<Mutex<Option<crate::platform::trace::FlowId>>>,
}

impl TabContext {
    fn trace_flow(&self) -> Option<crate::platform::trace::FlowId> {
        *self
            .trace_flow
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn trace_engine(ctx: &TabContext, event: &'static str) -> crate::platform::trace::EventBuilder {
    let mut builder = crate::platform::trace::event(crate::platform::trace::Lane::Engine, event)
        .field("tab", ctx.tab.0);
    if let Some(flow) = ctx.trace_flow() {
        builder = builder.flow(flow);
    }
    builder
}

cef::wrap_navigation_entry_visitor! {
    struct SdscNavigationVisitor {
        entries: Arc<Mutex<Vec<crate::browser::engine::EngineHistoryEntry>>>,
        current: Arc<AtomicU64>,
    }
    impl NavigationEntryVisitor {
        fn visit(
            &self,
            entry: Option<&mut NavigationEntry>,
            current: std::os::raw::c_int,
            index: std::os::raw::c_int,
            _total: std::os::raw::c_int,
        ) -> std::os::raw::c_int {
            let Some(entry) = entry else { return 1 };
            if entry.is_valid() == 0 || index < 0 {
                return 1;
            }
            let url = CefString::from(&entry.url()).to_string();
            if url.is_empty() {
                return 1;
            }
            let title = CefString::from(&entry.title()).to_string();
            self.entries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(crate::browser::engine::EngineHistoryEntry {
                    display_url: CefString::from(&entry.display_url()).to_string(),
                    original_url: CefString::from(&entry.original_url()).to_string(),
                    title: (!title.is_empty()).then_some(title),
                    has_post_data: entry.has_post_data() != 0,
                    url,
                });
            if current != 0 {
                self.current.store(index as u64, Ordering::Release);
            }
            1
        }
    }
}

/// Query CEF's canonical navigation entries and publish a snapshot if it
/// changed. There is no CEF "history changed" callback; call this after
/// address/title/load transitions and on explicit `QueryHistory`.
fn emit_history(ctx: &TabContext) {
    let Some(browser) = ctx.browser.get() else {
        return;
    };
    if browser.is_valid() == 0 {
        return;
    }
    let Some(host) = browser.host() else {
        return;
    };
    let entries = Arc::new(Mutex::new(Vec::new()));
    let current = Arc::new(AtomicU64::new(0));
    let mut visitor = SdscNavigationVisitor::new(entries.clone(), current.clone());
    host.navigation_entries(Some(&mut visitor), 0);
    let entries = entries
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if entries.is_empty() {
        return;
    }
    let current = current.load(Ordering::Acquire) as usize;
    let key = format!(
        "{current}\n{}",
        entries
            .iter()
            .map(|entry| {
                format!(
                    "{}\n{}\n{}\n{}",
                    entry.url,
                    entry.original_url,
                    entry.title.as_deref().unwrap_or(""),
                    u8::from(entry.has_post_data)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    );
    let hash = fnv1a(key.as_bytes());
    {
        let mut last = ctx
            .last_history_hash
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *last == hash {
            return;
        }
        *last = hash;
    }
    trace_engine(ctx, "history.snapshot")
        .field("current", current)
        .field("len", entries.len())
        .field(
            "urls",
            entries
                .iter()
                .map(|entry| entry.url.clone())
                .collect::<Vec<_>>(),
        )
        .emit();
    ctx.handle.emit(EngineEvent::HistoryChanged {
        tab: ctx.tab,
        entries,
        current,
    });
}

/// Record an exact history jump that could not be resolved or executed.
/// The UI settles the pending selector pick and falls back to a URL load.
fn history_jump_failed(ctx: &TabContext, index: usize, reason: &'static str) {
    trace_engine(ctx, "history.jump_failed")
        .field("index", index)
        .field("reason", reason)
        .emit();
    ctx.handle.emit(EngineEvent::HistoryJumpFailed {
        tab: ctx.tab,
        index,
        reason: reason.to_string(),
    });
}

/// Extract the DevTools entry id at a session-history index from a
/// `Page.getNavigationHistory` result payload.
fn devtools_entry_id(result: Option<&[u8]>, index: usize) -> Option<i32> {
    let value: serde_json::Value = serde_json::from_slice(result?).ok()?;
    let id = value
        .get("entries")?
        .as_array()?
        .get(index)?
        .get("id")?
        .as_i64()?;
    i32::try_from(id).ok()
}

fn next_devtools_id(state: &DevToolsState) -> std::os::raw::c_int {
    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
    if id <= 0 { 1 } else { id }
}

/// Second leg of an exact history jump: send `Page.navigateToHistoryEntry`
/// with the id resolved by `Page.getNavigationHistory`.
fn send_navigate_to_entry(ctx: &TabContext, host: &BrowserHost, index: usize, entry_id: i32) {
    let Some(mut params) = cef::dictionary_value_create() else {
        history_jump_failed(ctx, index, "params_failed");
        return;
    };
    let key: CefString = "entryId".into();
    params.set_int(Some(&key), entry_id);
    let method: CefString = "Page.navigateToHistoryEntry".into();
    // Assign the id here: with the multi-threaded message loop CEF posts the
    // call to the UI thread and returns 0, so the return value cannot supply
    // the id used to match the asynchronous result.
    let id = next_devtools_id(&ctx.devtools);
    ctx.devtools
        .pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(id, DevToolsRequest::Navigate { index });
    let returned = host.execute_dev_tools_method(id, Some(&method), Some(&mut params));
    if !cfg!(windows) && returned == 0 {
        ctx.devtools
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
        history_jump_failed(ctx, index, "devtools_send_failed");
        return;
    }
    trace_engine(ctx, "history.devtools")
        .field("method", "navigateToHistoryEntry")
        .field("index", index)
        .field("entry_id", entry_id)
        .field("message_id", id)
        .field("returned_id", returned)
        .emit();
}

/// Sends `Page.getNavigationHistory`, then jumps to the entry at `index`.
/// `NavigateToHistoryEntry` calls `NavigationController::GoToIndex`, which
/// addresses entries that Back/Forward intentionally skip.
fn go_to_history_index(ctx: &TabContext, index: usize) {
    let Some(host) = ctx.browser.get().and_then(|browser| browser.host()) else {
        history_jump_failed(ctx, index, "browser_missing");
        return;
    };
    let method: CefString = "Page.getNavigationHistory".into();
    let id = next_devtools_id(&ctx.devtools);
    ctx.devtools
        .pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(id, DevToolsRequest::GetHistory { index });
    let returned = host.execute_dev_tools_method(id, Some(&method), None);
    if !cfg!(windows) && returned == 0 {
        ctx.devtools
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
        history_jump_failed(ctx, index, "devtools_send_failed");
        return;
    }
    trace_engine(ctx, "history.devtools")
        .field("method", "getNavigationHistory")
        .field("index", index)
        .field("message_id", id)
        .field("returned_id", returned)
        .emit();
}

cef::wrap_dev_tools_message_observer! {
    struct SdscDevToolsObserver {
        ctx: TabContext,
    }
    impl DevToolsMessageObserver {
        fn on_dev_tools_method_result(
            &self,
            browser: Option<&mut Browser>,
            message_id: std::os::raw::c_int,
            success: std::os::raw::c_int,
            result: Option<&[u8]>,
        ) {
            let request = self
                .ctx
                .devtools
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&message_id);
            let Some(request) = request else {
                return;
            };
            match request {
                DevToolsRequest::GetHistory { index } => {
                    if success == 0 {
                        let error = result.map(|bytes| String::from_utf8_lossy(bytes).into_owned());
                        trace_engine(&self.ctx, "history.devtools")
                            .field("method", "getNavigationHistory")
                            .field("index", index)
                            .field("ok", false)
                            .field("error", error.as_deref())
                            .emit();
                        history_jump_failed(&self.ctx, index, "devtools_error");
                        return;
                    }
                    let Some(entry_id) = devtools_entry_id(result, index) else {
                        history_jump_failed(&self.ctx, index, "entry_missing");
                        return;
                    };
                    let Some(host) = browser.and_then(|browser| browser.host()) else {
                        history_jump_failed(&self.ctx, index, "browser_missing");
                        return;
                    };
                    send_navigate_to_entry(&self.ctx, &host, index, entry_id);
                }
                DevToolsRequest::Navigate { index } => {
                    if success == 0 {
                        history_jump_failed(&self.ctx, index, "devtools_error");
                    } else {
                        trace_engine(&self.ctx, "history.devtools")
                            .field("method", "navigateToHistoryEntry")
                            .field("index", index)
                            .field("ok", true)
                            .emit();
                    }
                }
            }
        }
    }
}

/// Register the DevTools observer once per browser. The registration handle
/// is stored on the worker-owned `TabEntry` because it is not `Send`.
fn ensure_devtools_observer(entry: &mut TabEntry, host: &BrowserHost) -> bool {
    if entry.devtools_registration.is_some() {
        return true;
    }
    let mut observer = SdscDevToolsObserver::new(entry.ctx.clone());
    entry.devtools_registration = host.add_dev_tools_message_observer(Some(&mut observer));
    entry.devtools_registration.is_some()
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

    fn clear(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
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
        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            let Some(cmd) = command_line else { return };
            // Tight, controller-paced scrolling: Chromium's smooth-scroll
            // easing makes every pad wheel tick feel laggy. sRGB keeps the
            // OSR buffer in the same color space as the shader's sRGB texture.
            cmd.append_switch(Some(&"disable-smooth-scrolling".into()));
            cmd.append_switch_with_value(
                Some(&"force-color-profile".into()),
                Some(&"srgb".into()),
            );
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
            browser: Option<&mut Browser>,
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
            let url = browser
                .as_ref()
                .and_then(|browser| browser.main_frame())
                .map(|frame| CefString::from(&frame.url()).to_string())
                .unwrap_or_default();
            self.ctx
                .frame
                .write(width as u32, height as u32, rgba, &url);
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
            target_url: Option<&CefString>,
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
            // Cancel the native popup browser: OSR popups would need a second
            // client/context and their own worker plumbing. The shell opens
            // the URL in a normal Browser tab instead.
            let url = target_url
                .map(|url| url.to_string())
                .filter(|url| !url.is_empty())
                .unwrap_or_else(|| "about:blank".to_string());
            // A trusted link click stays pending until a navigation starts;
            // a popup opens elsewhere, so the JS fallback must not fire too.
            *self
                .ctx
                .pending_click
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            trace_engine(&self.ctx, "popup.request")
                .field("url", &url)
                .emit();
            self.ctx.handle.emit(EngineEvent::PopupRequested {
                source: self.ctx.tab,
                url,
            });
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
            self.ctx.browser.clear();
            self.ctx.favicon_request.fetch_add(1, Ordering::AcqRel);
            *self
                .ctx
                .pending_click
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
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
            can_go_back: std::os::raw::c_int,
            can_go_forward: std::os::raw::c_int,
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
            self.ctx.handle.emit(EngineEvent::HistoryCapability {
                tab: self.ctx.tab,
                can_back: can_go_back != 0,
                can_forward: can_go_forward != 0,
            });
            if is_loading != 0 {
                *self
                    .ctx
                    .pending_click
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            }
            trace_engine(&self.ctx, "loading")
                .field("loading", is_loading != 0)
                .field("can_back", can_go_back != 0)
                .field("can_forward", can_go_forward != 0)
                .emit();
            if is_loading == 0 {
                emit_history(&self.ctx);
            }
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
            trace_engine(&self.ctx, "load.end")
                .field("generation", generation)
                .emit();
            // New document: recompute whether the focused node accepts text,
            // even if it carries the same focus state as the old page.
            *self
                .ctx
                .last_editable
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
            *self
                .ctx
                .last_dom_hash
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = 0;
            self.ctx.handle.emit(EngineEvent::LoadFinished {
                tab: self.ctx.tab,
                generation,
                result: Ok(()),
            });
            emit_history(&self.ctx);
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
            let message = error_text
                .map(|t| t.to_string())
                .unwrap_or_else(|| "load failed".to_string());
            trace_engine(&self.ctx, "load.error")
                .field("generation", generation)
                .field("error", &message)
                .emit();
            self.ctx.handle.emit(EngineEvent::LoadFinished {
                tab: self.ctx.tab,
                generation,
                result: Err(message),
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
            *self
                .ctx
                .pending_click
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            trace_engine(&self.ctx, "address")
                .field("url", &url)
                .emit();
            self.ctx.handle.emit(EngineEvent::UrlChanged {
                tab: self.ctx.tab,
                url,
            });
            emit_history(&self.ctx);
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
            emit_history(&self.ctx);
        }

        fn on_favicon_urlchange(
            &self,
            browser: Option<&mut Browser>,
            icon_urls: Option<&mut CefStringList>,
        ) {
            let Some(list) = icon_urls else { return };
            // Clone the CEF-owned list handle: the clone is a borrowed view
            // (freeing is reserved for owned lists), so iterating it is safe.
            let urls: Vec<String> = list
                .clone()
                .into_iter()
                .filter(|url| !url.is_empty())
                .collect();
            let page_url = browser
                .and_then(|b| b.main_frame())
                .map(|frame| CefString::from(&frame.url()).to_string())
                .unwrap_or_default();
            if page_url.is_empty() {
                return;
            }
            let key = format!(
                "{}
{}",
                crate::browser::bookmarks::origin_key(&page_url),
                urls.join("
")
            );
            {
                let mut last = self
                    .ctx
                    .favicon_key
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if *last == key {
                    return;
                }
                *last = key;
            }
            let request_id = self.ctx.favicon_request.fetch_add(1, Ordering::AcqRel) + 1;
            start_favicon_download(&self.ctx, Some(request_id), page_url, urls);
        }

        fn on_cursor_change(
            &self,
            _browser: Option<&mut Browser>,
            _cursor: cef::sys::HCURSOR,
            type_: CursorType,
            _custom_cursor_info: Option<&CursorInfo>,
        ) -> std::os::raw::c_int {
            let cursor = cursor_from_cef(type_);
            {
                let mut last = self
                    .ctx
                    .last_cursor
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if *last == cursor {
                    return 1;
                }
                *last = cursor;
            }
            trace_engine(&self.ctx, "cursor")
                .field("cursor", format!("{cursor:?}"))
                .emit();
            self.ctx.handle.emit(EngineEvent::CursorChanged {
                tab: self.ctx.tab,
                cursor,
            });
            1
        }
    }
}

/// One in-flight CEF favicon download. `request_id` is `None` for bookmark
/// prefetch, where `page_url` is not the live document.
struct FaviconDownload {
    ctx: TabContext,
    request_id: Option<u64>,
    page_url: String,
    icon_urls: Vec<String>,
}

fn favicon_candidates(page_url: &str, icon_urls: &[String]) -> Vec<String> {
    let mut candidates = icon_urls.to_vec();
    if let Some(origin) = crate::browser::favicon::origin_url(page_url) {
        let conventional = format!("{origin}/favicon.ico");
        if !candidates.iter().any(|url| url == &conventional) {
            candidates.push(conventional);
        }
    }
    candidates
}

fn favicon_is_http(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn favicon_page_matches(ctx: &TabContext, page_url: &str) -> bool {
    let Some(browser) = ctx.browser.get() else {
        return false;
    };
    if browser.is_valid() == 0 {
        return false;
    }
    let Some(frame) = browser.main_frame() else {
        return false;
    };
    let current = CefString::from(&frame.url()).to_string();
    crate::browser::bookmarks::origin_key(&current)
        == crate::browser::bookmarks::origin_key(page_url)
}

fn favicon_request_current(state: &FaviconDownload) -> bool {
    if let Some(request_id) = state.request_id
        && state.ctx.favicon_request.load(Ordering::Acquire) != request_id
    {
        return false;
    }
    if state.request_id.is_some() {
        favicon_page_matches(&state.ctx, &state.page_url)
    } else {
        state
            .ctx
            .browser
            .get()
            .is_some_and(|browser| browser.is_valid() != 0)
    }
}

/// Convert a CEF image to straight-alpha RGBA. Prefer the 2x representation;
/// fall back to the PNG representation if the bitmap path is unavailable.
fn favicon_image_to_rgba(image: &mut Image) -> Option<(u32, u32, Vec<u8>)> {
    if image.is_empty() != 0 {
        return None;
    }
    let scale = if image.has_representation(2.0) != 0 {
        2.0
    } else {
        1.0
    };
    let mut width = 0;
    let mut height = 0;
    if let Some(bitmap) = image.as_bitmap(
        scale,
        ColorType::RGBA_8888,
        AlphaType::POSTMULTIPLIED,
        Some(&mut width),
        Some(&mut height),
    ) && width > 0
        && height > 0
    {
        let len = bitmap.size();
        let expected = width as usize * height as usize * 4;
        if len == expected {
            let mut rgba = vec![0u8; len];
            if bitmap.data(Some(&mut rgba), 0) == len {
                return Some((width as u32, height as u32, rgba));
            }
        }
    }

    let png = image.as_png(scale, 1, Some(&mut width), Some(&mut height))?;
    let len = png.size();
    if len == 0 {
        return None;
    }
    let mut bytes = vec![0u8; len];
    if png.data(Some(&mut bytes), 0) != len {
        return None;
    }
    crate::browser::favicon::decode(&bytes)
}

cef::wrap_download_image_callback! {
    struct SdscFaviconDownload {
        state: Arc<FaviconDownload>,
    }
    impl DownloadImageCallback {
        fn on_download_image_finished(
            &self,
            image_url: Option<&CefString>,
            http_status_code: std::os::raw::c_int,
            image: Option<&mut Image>,
        ) {
            let state = &self.state;
            if !favicon_request_current(state) {
                return;
            }
            let source_url = image_url
                .map(|url| url.to_string())
                .unwrap_or_default();
            if let Some(image) = image
                && let Some((width, height, rgba)) = favicon_image_to_rgba(image)
            {
                trace_engine(&state.ctx, "favicon.result")
                    .field("page", &state.page_url)
                    .field("url", &source_url)
                    .field("status", http_status_code)
                    .field("source", "cef")
                    .field("width", width)
                    .field("height", height)
                    .emit();
                state.ctx.handle.emit(EngineEvent::FaviconChanged {
                    tab: state.ctx.tab,
                    url: state.page_url.clone(),
                    width,
                    height,
                    rgba,
                });
                return;
            }
            trace_engine(&state.ctx, "favicon.result")
                .field("page", &state.page_url)
                .field("url", &source_url)
                .field("status", http_status_code)
                .field("source", "cef")
                .field("ok", false)
                .emit();
            spawn_favicon_fetch(
                state.ctx.handle.clone(),
                state.ctx.tab,
                state.page_url.clone(),
                state.icon_urls.clone(),
                state
                    .request_id
                    .map(|request_id| (state.ctx.clone(), request_id)),
            );
        }
    }
}

/// Start the CEF-native favicon path. HTTP(S) goes through Chromium's network
/// stack; `data:` URLs and failures fall back to the local decoder.
fn start_favicon_download(
    ctx: &TabContext,
    request_id: Option<u64>,
    page_url: String,
    icon_urls: Vec<String>,
) {
    let candidate = favicon_candidates(&page_url, &icon_urls)
        .into_iter()
        .find(|url| favicon_is_http(url));
    let Some(candidate) = candidate else {
        spawn_favicon_fetch(
            ctx.handle.clone(),
            ctx.tab,
            page_url,
            icon_urls,
            request_id.map(|request_id| (ctx.clone(), request_id)),
        );
        return;
    };
    let Some(host) = ctx.browser.get().and_then(|browser| browser.host()) else {
        spawn_favicon_fetch(
            ctx.handle.clone(),
            ctx.tab,
            page_url,
            icon_urls,
            request_id.map(|request_id| (ctx.clone(), request_id)),
        );
        return;
    };

    trace_engine(ctx, "favicon.request")
        .field("page", &page_url)
        .field("url", &candidate)
        .field("source", "cef")
        .emit();
    let state = Arc::new(FaviconDownload {
        ctx: ctx.clone(),
        request_id,
        page_url,
        icon_urls,
    });
    let mut callback = SdscFaviconDownload::new(state);
    let image_url: CefString = candidate.as_str().into();
    host.download_image(
        Some(&image_url),
        1,
        crate::browser::favicon::MAX_ICON_PX,
        0,
        Some(&mut callback),
    );
}

/// Fetch off the CEF UI thread (blocking network + image decode), then post
/// the decoded icon through the normal engine event channel. This is the
/// fallback for `data:` URLs, unsupported formats, and CEF download failures.
fn spawn_favicon_fetch(
    handle: EngineHandle,
    tab: TabId,
    page_url: String,
    icon_urls: Vec<String>,
    validate: Option<(TabContext, u64)>,
) {
    std::thread::spawn(move || {
        let Some((width, height, rgba)) =
            crate::browser::favicon::fetch_best(&page_url, &icon_urls)
        else {
            return;
        };
        if let Some((ctx, request_id)) = &validate {
            if ctx.favicon_request.load(Ordering::Acquire) != *request_id
                || !favicon_page_matches(ctx, &page_url)
            {
                return;
            }
            trace_engine(ctx, "favicon.result")
                .field("page", &page_url)
                .field("source", "local")
                .field("width", width)
                .field("height", height)
                .emit();
        }
        handle.emit(EngineEvent::FaviconChanged {
            tab,
            url: page_url,
            width,
            height,
            rgba,
        });
    });
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
    let editable = root
        .get("ae")
        .and_then(|v| v.get("e"))
        .and_then(|v| v.as_u64())
        .is_some_and(|value| value != 0);
    let url = root
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

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
        url,
        items,
    });
    if active != 0 && active != before_active {
        *ctx.last_active.lock().unwrap_or_else(|e| e.into_inner()) = active;
        ctx.handle.emit(EngineEvent::FocusChanged {
            tab: ctx.tab,
            id: active,
        });
    }
    let before_editable = *ctx.last_editable.lock().unwrap_or_else(|e| e.into_inner());
    if before_editable != Some(editable) {
        *ctx.last_editable.lock().unwrap_or_else(|e| e.into_inner()) = Some(editable);
        ctx.handle.emit(EngineEvent::EditableFocusChanged {
            tab: ctx.tab,
            editable,
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
    /// Left mouse button held over the page (drag selection modifier).
    mouse_left_down: bool,
    /// Keeps the DevTools message observer registered for this browser.
    /// Lives outside `TabContext` because the registration handle is not
    /// `Send`; `TabContext` crosses into the favicon worker.
    devtools_registration: Option<Registration>,
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
    /// Bookmark favicon origins already asked for this session.
    prefetched: std::collections::HashSet<String>,
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
        prefetched: std::collections::HashSet::new(),
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
        worker.tick_pending_click();
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
            last_editable: Arc::new(Mutex::new(None)),
            scroll: Arc::new(Mutex::new((0.0, 0.0))),
            extent: Arc::new(Mutex::new((0.0, 0.0))),
            favicon_key: Arc::new(Mutex::new(String::new())),
            favicon_request: Arc::new(AtomicU64::new(0)),
            pending_click: Arc::new(Mutex::new(None)),
            last_history_hash: Arc::new(Mutex::new(0)),
            devtools: Arc::new(DevToolsState::default()),
            last_cursor: Arc::new(Mutex::new(crate::browser::engine::BrowserCursor::Default)),
            trace_flow: Arc::new(Mutex::new(None)),
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
                mouse_left_down: false,
                devtools_registration: None,
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
            EngineCommand::Traced { flow, command } => {
                if let Some(tab) = command.tab()
                    && let Some(entry) = self.tab_mut(tab)
                {
                    *entry
                        .ctx
                        .trace_flow
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(flow);
                }
                return self.apply(*command);
            }
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
            EngineCommand::Reload {
                tab,
                generation,
                ignore_cache,
            } => {
                if let Some(entry) = self.tab_mut(tab) {
                    entry.generation = generation;
                    *entry
                        .ctx
                        .generation
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = generation;
                    if let Some(browser) = entry.ctx.browser.get() {
                        if ignore_cache {
                            browser.reload_ignore_cache();
                        } else {
                            browser.reload();
                        }
                    }
                }
            }
            EngineCommand::ScrollBy { tab, dx, dy, x, y } => {
                let Some(entry) = self.tab(tab) else {
                    return false;
                };
                if let Some(browser) = entry.ctx.browser.get()
                    && let Some(frame) = browser.main_frame()
                {
                    // Analog pad scroll is applied as an instant `scrollBy` on
                    // the innermost scroller under the cursor. Wheel events
                    // feed Chromium's scroll animator: each 60Hz event starts
                    // an easing curve, which trails the stick and visibly
                    // keeps gliding after release. `behavior: 'instant'` makes
                    // the stop immediate and keeps the wheel path reserved for
                    // real wheel input.
                    let code = format!(
                        r#"(function(){{
                        var x={x:.0}, y={y:.0}, dx={dx:.2}, dy={dy:.2};
                        var e = document.elementFromPoint(x, y);
                        while (e && e !== document.documentElement && e !== document.body) {{
                          var style = getComputedStyle(e);
                          var canY = /(auto|scroll|overlay)/.test(style.overflowY)
                            && e.scrollHeight > e.clientHeight + 1;
                          var canX = /(auto|scroll|overlay)/.test(style.overflowX)
                            && e.scrollWidth > e.clientWidth + 1;
                          if ((dy !== 0 && canY) || (dx !== 0 && canX)) {{
                            e.scrollBy({{ left: dx, top: dy, behavior: 'instant' }});
                            return;
                          }}
                          e = e.parentElement;
                        }}
                        window.scrollBy({{ left: dx, top: dy, behavior: 'instant' }});
                        }})();"#
                    );
                    let code: CefString = code.as_str().into();
                    let url: CefString = "sdsc://scroll".into();
                    frame.execute_java_script(Some(&code), Some(&url), 0);
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
            EngineCommand::MouseMove {
                tab,
                x,
                y,
                modifiers: keys,
            } => {
                if let Some(entry) = self.tab(tab) {
                    *entry
                        .ctx
                        .last_mouse
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = (x, y);
                    let mut flags = modifiers(keys);
                    if entry.mouse_left_down {
                        flags |= 1 << 4;
                    }
                    if let Some(browser) = entry.ctx.browser.get()
                        && let Some(host) = browser.host()
                    {
                        host.send_mouse_move_event(
                            Some(&MouseEvent {
                                x: x as i32,
                                y: y as i32,
                                modifiers: flags,
                            }),
                            0,
                        );
                    }
                }
            }
            EngineCommand::MouseButton {
                tab,
                x,
                y,
                button,
                pressed,
                click_count,
                modifiers: keys,
            } => {
                if let Some(entry) = self.tab_mut(tab) {
                    if button == BrowserMouseButton::Left {
                        entry.mouse_left_down = pressed;
                    }
                    let (cef_button, flag) = match button {
                        BrowserMouseButton::Left => (MouseButtonType::LEFT, 1 << 4),
                        BrowserMouseButton::Middle => (MouseButtonType::MIDDLE, 1 << 5),
                        BrowserMouseButton::Right => (MouseButtonType::RIGHT, 1 << 6),
                    };
                    let mut flags = modifiers(keys);
                    if pressed {
                        flags |= flag;
                    }
                    if let Some(browser) = entry.ctx.browser.get()
                        && let Some(host) = browser.host()
                    {
                        if pressed {
                            host.set_focus(1);
                        }
                        let event = MouseEvent {
                            x: x as i32,
                            y: y as i32,
                            modifiers: flags,
                        };
                        host.send_mouse_move_event(Some(&event), 0);
                        host.send_mouse_click_event(
                            Some(&event),
                            cef_button,
                            i32::from(!pressed),
                            i32::from(click_count.max(1)),
                        );
                    }
                }
            }
            EngineCommand::MouseLeave { tab } => {
                if let Some(entry) = self.tab_mut(tab) {
                    entry.mouse_left_down = false;
                    let (x, y) = *entry
                        .ctx
                        .last_mouse
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some(browser) = entry.ctx.browser.get()
                        && let Some(host) = browser.host()
                    {
                        host.send_mouse_move_event(
                            Some(&MouseEvent {
                                x: x as i32,
                                y: y as i32,
                                modifiers: 0,
                            }),
                            1,
                        );
                    }
                }
            }
            EngineCommand::MouseWheel { tab, x, y, dx, dy } => {
                if let Some(entry) = self.tab(tab)
                    && let Some(browser) = entry.ctx.browser.get()
                    && let Some(host) = browser.host()
                {
                    host.send_mouse_wheel_event(
                        Some(&MouseEvent {
                            x: x as i32,
                            y: y as i32,
                            modifiers: 0,
                        }),
                        dx,
                        dy,
                    );
                }
            }
            EngineCommand::ImeSetComposition { tab, text } => {
                if let Some(entry) = self.tab(tab)
                    && let Some(browser) = entry.ctx.browser.get()
                    && let Some(host) = browser.host()
                {
                    let text: CefString = text.as_str().into();
                    let len = text.to_string().encode_utf16().count() as u32;
                    let underlines = [CompositionUnderline {
                        range: Range { from: 0, to: len },
                        color: 0xFF00_0000,
                        background_color: 0,
                        thick: 0,
                        ..CompositionUnderline::default()
                    }];
                    let selection = Range { from: len, to: len };
                    let replacement = Range {
                        from: u32::MAX,
                        to: u32::MAX,
                    };
                    trace_engine(&entry.ctx, "ime")
                        .field("kind", "set")
                        .field("len", len)
                        .emit();
                    host.ime_set_composition(
                        Some(&text),
                        Some(&underlines),
                        Some(&replacement),
                        Some(&selection),
                    );
                }
            }
            EngineCommand::ImeCommitText { tab, text } => {
                if let Some(entry) = self.tab(tab)
                    && let Some(browser) = entry.ctx.browser.get()
                    && let Some(host) = browser.host()
                {
                    let text: CefString = text.as_str().into();
                    trace_engine(&entry.ctx, "ime")
                        .field("kind", "commit")
                        .field("len", text.to_string().encode_utf16().count())
                        .emit();
                    host.ime_commit_text(Some(&text), None, 0);
                }
            }
            EngineCommand::ImeFinishComposition { tab } => {
                if let Some(entry) = self.tab(tab)
                    && let Some(browser) = entry.ctx.browser.get()
                    && let Some(host) = browser.host()
                {
                    host.ime_finish_composing_text(0);
                }
            }
            EngineCommand::ImeCancelComposition { tab } => {
                if let Some(entry) = self.tab(tab)
                    && let Some(browser) = entry.ctx.browser.get()
                    && let Some(host) = browser.host()
                {
                    host.ime_cancel_composition();
                }
            }
            EngineCommand::ReseedDom { tab } => {
                if let Some(entry) = self.tab_mut(tab) {
                    entry.last_dom = Instant::now() - DOM_POLL;
                }
            }
            EngineCommand::ClickAt {
                tab,
                x,
                y,
                target,
                trusted,
            } => {
                let Some(entry) = self.tab(tab) else {
                    return false;
                };
                let Some(browser) = entry.ctx.browser.get() else {
                    return false;
                };
                let Some(frame) = browser.main_frame() else {
                    return false;
                };
                if let Some(id) = target
                    && !trusted
                {
                    let code = format!("window.__sdsc_click && window.__sdsc_click({id})");
                    let code: CefString = code.as_str().into();
                    let url: CefString = "sdsc://click".into();
                    frame.execute_java_script(Some(&code), Some(&url), 0);
                    return false;
                }
                if let Some(host) = browser.host() {
                    host.set_focus(1);
                    let ev = MouseEvent {
                        x: x as i32,
                        y: y as i32,
                        modifiers: 0,
                    };
                    host.send_mouse_move_event(Some(&ev), 0);
                    host.send_mouse_click_event(Some(&ev), MouseButtonType::LEFT, 0, 1);
                    host.send_mouse_click_event(Some(&ev), MouseButtonType::LEFT, 1, 1);
                    if let Some(id) = target {
                        *entry
                            .ctx
                            .pending_click
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                            Some(PendingClick {
                                target: id,
                                at: Instant::now(),
                            });
                    }
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
            EngineCommand::GoBack { tab } => {
                if let Some(entry) = self.tab(tab)
                    && let Some(browser) = entry.ctx.browser.get()
                {
                    browser.go_back();
                }
            }
            EngineCommand::GoForward { tab } => {
                if let Some(entry) = self.tab(tab)
                    && let Some(browser) = entry.ctx.browser.get()
                {
                    browser.go_forward();
                }
            }
            EngineCommand::GoToHistoryIndex { tab, index } => {
                if let Some(entry) = self.tab_mut(tab) {
                    let host = entry.ctx.browser.get().and_then(|browser| browser.host());
                    let Some(host) = host else {
                        history_jump_failed(&entry.ctx, index, "browser_missing");
                        return false;
                    };
                    if !ensure_devtools_observer(entry, &host) {
                        history_jump_failed(&entry.ctx, index, "observer_failed");
                        return false;
                    }
                    go_to_history_index(&entry.ctx, index);
                }
            }
            EngineCommand::QueryHistory { tab } => {
                if let Some(entry) = self.tab(tab) {
                    emit_history(&entry.ctx);
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
                // A newly foregrounded tab must publish its focus state even
                // when the last snapshot while it was background already
                // matches: the UI closed any auto-OSK on the switch.
                if let Some(entry) = self.tab(tab) {
                    *entry
                        .ctx
                        .last_dom_hash
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = 0;
                    *entry
                        .ctx
                        .last_editable
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = None;
                }
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
            EngineCommand::PrefetchFavicon { url } => {
                let key = crate::browser::bookmarks::origin_key(&url);
                if self.prefetched.insert(key) {
                    if let Some(entry) = self.tab(self.active) {
                        let ctx = entry.ctx.clone();
                        start_favicon_download(&ctx, None, url, Vec::new());
                    } else {
                        spawn_favicon_fetch(
                            self.handle.clone(),
                            self.active,
                            url,
                            Vec::new(),
                            None,
                        );
                    }
                }
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

    /// A trusted link click that produced no loading/address callback by the
    /// deadline gets the old JS click. This keeps links usable if CEF's
    /// synthetic mouse path misses, while successful trusted clicks keep the
    /// navigation's user-activation bit.
    fn tick_pending_click(&self) {
        let Some(entry) = self.tab(self.active) else {
            return;
        };
        let pending = {
            let mut guard = entry
                .ctx
                .pending_click
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(pending) = guard.as_ref() else {
                return;
            };
            if pending.at.elapsed() < CLICK_FALLBACK {
                return;
            }
            guard.take()
        };
        let Some(pending) = pending else {
            return;
        };
        let Some(browser) = entry.ctx.browser.get() else {
            return;
        };
        let Some(frame) = browser.main_frame() else {
            return;
        };
        trace_engine(&entry.ctx, "click.fallback")
            .field("target", pending.target)
            .emit();
        let code = format!(
            "window.__sdsc_click && window.__sdsc_click({})",
            pending.target
        );
        let code: CefString = code.as_str().into();
        let url: CefString = "sdsc://click".into();
        frame.execute_java_script(Some(&code), Some(&url), 0);
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
        let js = dom::dom_query_js(center, entry.dom_epoch);
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

/// Map CEF's cursor vocabulary onto the smaller set the browser view uses.
/// Uncommon/drag cursors fall back to the default arrow rather than hiding
/// the pointer.
fn cursor_from_cef(type_: CursorType) -> BrowserCursor {
    if type_ == CursorType::HAND {
        BrowserCursor::Pointer
    } else if type_ == CursorType::IBEAM || type_ == CursorType::VERTICALTEXT {
        BrowserCursor::Text
    } else if type_ == CursorType::CROSS {
        BrowserCursor::Crosshair
    } else if type_ == CursorType::MOVE
        || type_ == CursorType::EASTPANNING
        || type_ == CursorType::WESTPANNING
        || type_ == CursorType::NORTHPANNING
        || type_ == CursorType::SOUTHPANNING
    {
        BrowserCursor::Move
    } else if type_ == CursorType::WAIT {
        BrowserCursor::Wait
    } else if type_ == CursorType::PROGRESS {
        BrowserCursor::Progress
    } else if type_ == CursorType::HELP {
        BrowserCursor::Help
    } else if type_ == CursorType::NOTALLOWED || type_ == CursorType::NODROP {
        BrowserCursor::NotAllowed
    } else if type_ == CursorType::CONTEXTMENU {
        BrowserCursor::ContextMenu
    } else if type_ == CursorType::COPY || type_ == CursorType::ALIAS {
        BrowserCursor::Copy
    } else if type_ == CursorType::CELL {
        BrowserCursor::Cell
    } else if type_ == CursorType::GRAB {
        BrowserCursor::Grab
    } else if type_ == CursorType::GRABBING {
        BrowserCursor::Grabbing
    } else if type_ == CursorType::ZOOMIN {
        BrowserCursor::ZoomIn
    } else if type_ == CursorType::ZOOMOUT {
        BrowserCursor::ZoomOut
    } else if type_ == CursorType::EASTRESIZE
        || type_ == CursorType::WESTRESIZE
        || type_ == CursorType::EASTWESTRESIZE
    {
        BrowserCursor::ResizeHorizontal
    } else if type_ == CursorType::NORTHRESIZE
        || type_ == CursorType::SOUTHRESIZE
        || type_ == CursorType::NORTHSOUTHRESIZE
    {
        BrowserCursor::ResizeVertical
    } else if type_ == CursorType::NORTHWESTSOUTHEASTRESIZE {
        BrowserCursor::ResizeNwSe
    } else if type_ == CursorType::NORTHEASTSOUTHWESTRESIZE {
        BrowserCursor::ResizeNeSw
    } else if type_ == CursorType::COLUMNRESIZE {
        BrowserCursor::ResizeColumn
    } else if type_ == CursorType::ROWRESIZE {
        BrowserCursor::ResizeRow
    } else {
        BrowserCursor::Default
    }
}

fn send_key(host: &BrowserHost, key: &TabKey) {
    for event in key_events(&key.event) {
        host.send_key_event(Some(&event));
    }
}

/// Translate one logical key transition into the CEF sequence Chromium
/// expects: `RAWKEYDOWN`, zero or more `CHAR` events for text input, then
/// `KEYUP` on release. Sending a character on `KEYDOWN` is not equivalent and
/// leaves text inputs empty.
fn key_events(event: &keyboard_types::KeyboardEvent) -> Vec<KeyEvent> {
    let (vk, _) = key_code(&event.key);
    let is_system_key = i32::from(event.modifiers.alt() && !event.modifiers.ctrl());
    let base = KeyEvent {
        modifiers: modifiers(event.modifiers),
        windows_key_code: vk,
        native_key_code: vk,
        is_system_key,
        ..KeyEvent::default()
    };
    if event.state == keyboard_types::KeyState::Up {
        return vec![KeyEvent {
            type_: KeyEventType::KEYUP,
            ..base
        }];
    }
    let mut events = vec![KeyEvent {
        type_: KeyEventType::RAWKEYDOWN,
        ..base.clone()
    }];
    for unit in text_units(event) {
        events.push(KeyEvent {
            type_: KeyEventType::CHAR,
            // WM_CHAR carries the character code in wParam; CEF derives the
            // inserted text from this field, not the virtual key. Leaving the
            // VK here makes lowercase input insert its uppercase twin.
            windows_key_code: i32::from(unit),
            native_key_code: i32::from(unit),
            character: unit,
            unmodified_character: unit,
            ..base.clone()
        });
    }
    events
}

/// Text inserted by this key press. Ctrl/Alt/Meta combinations are shortcuts,
/// not text; plain Shift is normal character input. AltGr currently reports
/// Ctrl+Alt here and is suppressed.
fn text_units(event: &keyboard_types::KeyboardEvent) -> Vec<u16> {
    if event.modifiers.ctrl() || event.modifiers.alt() || event.modifiers.meta() {
        return Vec::new();
    }
    match &event.key {
        keyboard_types::Key::Character(text) if !text.is_empty() => text
            .chars()
            .filter(|ch| !ch.is_control())
            .collect::<String>()
            .encode_utf16()
            .collect(),
        _ => Vec::new(),
    }
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

#[cfg(test)]
mod favicon_tests {
    use super::*;

    #[test]
    fn candidates_keep_advertised_order_and_append_conventional() {
        let urls = vec![
            "https://a.test/icon.png".to_string(),
            "https://a.test/icon.svg".to_string(),
        ];
        let candidates = favicon_candidates("https://a.test/page", &urls);
        assert_eq!(candidates[0], "https://a.test/icon.png");
        assert_eq!(candidates[1], "https://a.test/icon.svg");
        assert_eq!(candidates[2], "https://a.test/favicon.ico");
    }

    #[test]
    fn candidates_do_not_duplicate_conventional_icon() {
        let urls = vec!["https://a.test/favicon.ico".to_string()];
        let candidates = favicon_candidates("https://a.test/page", &urls);
        assert_eq!(candidates, urls);
    }

    #[test]
    fn only_http_candidates_reach_cef() {
        assert!(favicon_is_http("https://a.test/icon.png"));
        assert!(favicon_is_http("http://a.test/icon.png"));
        assert!(!favicon_is_http("data:image/png;base64,AAAA"));
        assert!(!favicon_is_http(""));
    }
}

#[cfg(test)]
mod devtools_tests {
    use super::*;
    const HISTORY: &[u8] = br#"{
        "currentIndex": 1,
        "entries": [
            {"id": 12, "url": "https://a.test/"},
            {"id": 34, "url": "https://b.test/"},
            {"id": 56, "url": "https://c.test/"}
        ]
    }"#;

    #[test]
    fn resolves_entry_id_by_session_index() {
        assert_eq!(devtools_entry_id(Some(HISTORY), 0), Some(12));
        assert_eq!(devtools_entry_id(Some(HISTORY), 1), Some(34));
        assert_eq!(devtools_entry_id(Some(HISTORY), 2), Some(56));
    }

    #[test]
    fn rejects_missing_index_and_malformed_payloads() {
        assert_eq!(devtools_entry_id(Some(HISTORY), 3), None);
        assert_eq!(devtools_entry_id(None, 0), None);
        assert_eq!(devtools_entry_id(Some(b"not json"), 0), None);
        assert_eq!(devtools_entry_id(Some(br#"{"entries":[]}"#), 0), None);
        assert_eq!(
            devtools_entry_id(Some(br#"{"entries":[{"id":4294967296}]}"#), 0),
            None
        );
    }

    #[test]
    fn devtools_ids_start_positive_and_increment() {
        let state = DevToolsState::default();
        assert_eq!(next_devtools_id(&state), 1);
        assert_eq!(next_devtools_id(&state), 2);
    }
}

#[cfg(test)]
mod key_event_tests {
    use super::*;
    use keyboard_types::{Code, Key, KeyState, Location, Modifiers, NamedKey};

    fn event(key: Key, state: KeyState, modifiers: Modifiers) -> keyboard_types::KeyboardEvent {
        keyboard_types::KeyboardEvent {
            state,
            key,
            code: Code::Unidentified,
            location: Location::Standard,
            modifiers,
            repeat: false,
            is_composing: false,
        }
    }

    #[test]
    fn printable_press_emits_rawkeydown_then_char() {
        let events = key_events(&event(
            Key::Character("a".into()),
            KeyState::Down,
            Modifiers::empty(),
        ));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].type_, KeyEventType::RAWKEYDOWN);
        assert_eq!(events[0].windows_key_code, i32::from(b'A'));
        assert_eq!(events[1].type_, KeyEventType::CHAR);
        assert_eq!(events[1].character, u16::from(b'a'));
        assert_eq!(events[1].windows_key_code, i32::from(b'a'));
    }

    #[test]
    fn ctrl_shortcut_suppresses_character_input() {
        let events = key_events(&event(
            Key::Character("c".into()),
            KeyState::Down,
            Modifiers::CONTROL,
        ));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].type_, KeyEventType::RAWKEYDOWN);
        assert_eq!(events[0].modifiers & (1 << 2), 1 << 2);
    }

    #[test]
    fn release_emits_only_keyup() {
        let events = key_events(&event(
            Key::Character("a".into()),
            KeyState::Up,
            Modifiers::empty(),
        ));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].type_, KeyEventType::KEYUP);
    }

    #[test]
    fn emoji_uses_utf16_surrogate_char_events() {
        let events = key_events(&event(
            Key::Character("😀".into()),
            KeyState::Down,
            Modifiers::empty(),
        ));
        assert_eq!(events.len(), 3);
        assert_eq!(events[1].type_, KeyEventType::CHAR);
        assert_eq!(events[2].type_, KeyEventType::CHAR);
        assert_eq!(events[1].character, 0xD83D);
        assert_eq!(events[2].character, 0xDE00);
    }

    #[test]
    fn named_enter_is_not_character_input() {
        let events = key_events(&event(
            Key::Named(NamedKey::Enter),
            KeyState::Down,
            Modifiers::empty(),
        ));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].type_, KeyEventType::RAWKEYDOWN);
        assert_eq!(events[0].windows_key_code, 0x0D);
    }
}
