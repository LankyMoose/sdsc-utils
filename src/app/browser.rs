//! Top-level Browser app state.
//!
//! Phase 0 of the hub pivot lifts the browser out of `StartState`: the panel,
//! engine handle, capability flag, and browser-only D-pad stepper live here so
//! the browser no longer disappears when the Start screen is not the owner.
//! Behavior is unchanged — Start still renders the panel until the standalone
//! Browser window lands. See `notes/browser-hub-pivot.md`.

use crate::browser::engine::{EngineCommand, EngineHandle};
use crate::domain::pad::NavStepper;
use crate::ui::browser::BrowserPanel;
use std::path::PathBuf;

/// Install/runtime state for the optional CEF engine package.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
// UI-only builds construct only `Missing`; the install states are exercised
// with `cef-engine` enabled.
#[allow(dead_code)]
pub enum EngineStatus {
    /// No valid runtime found. The hub Browser tile offers install.
    #[default]
    Missing,
    /// Runtime directory verified and the worker is live.
    Ready(PathBuf),
    /// An install/download is in flight.
    Installing,
    /// The last install attempt failed; message is user-facing.
    Failed(String),
}

impl EngineStatus {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }
}

#[derive(Default)]
pub struct BrowserAppState {
    /// Tabs, chrome targets, cursor, bookmarks, and per-tab page state.
    pub panel: BrowserPanel,
    /// CEF OSR worker. `None` in UI-only builds or when CEF is missing.
    pub engine: Option<EngineHandle>,
    /// Process-static capability: true when an engine worker is live. Drives
    /// the page placeholder copy and the hub tile install state.
    pub engine_available: bool,
    /// Runtime/install status for the optional engine package.
    pub engine_status: EngineStatus,
    /// Accumulated analog scroll for the next BrowserFrame tick.
    pub pending_scroll: (f32, f32),
    /// Page point the pending scroll belongs to (cursor at accumulation).
    pub pending_scroll_at: (f32, f32),
    /// Engine-mode D-pad cursor jumps run on this standalone stepper (fed a
    /// stickless `dpad_only` mask): the Start bank stays parked in engine
    /// mode so stick moves never double-fire into both cursor and nav.
    pub dpad_stepper: NavStepper,
    /// Browser-window pad bank: face/bumper edges + reader nav. The engine
    /// cursor uses `dpad_stepper` and raw stick samples instead.
    pub nav: crate::domain::pad::PadNavBank,
    /// First fresh Browser surface attempted the persisted-strip restore.
    /// The engine preloads the saved active URL at boot, so "session URL is
    /// empty" is not a usable freshness test on later opens.
    pub tabs_restored: bool,
}

use iced::Task;

use super::Message;

impl super::App {
    /// Browser-window message router. Phase 2 is moving the reducer here; the
    /// methods still live beside the Start reducer until the move completes.
    pub(crate) fn on_browser_message(
        &mut self,
        message: crate::ui::browser::BrowserMessage,
    ) -> Task<Message> {
        use crate::ui::browser::BrowserMessage;
        match message {
            BrowserMessage::Close => self.close_browser_window(),
            BrowserMessage::UrlChanged(draft) => {
                self.browser.panel.draft = draft;
                Task::none()
            }
            BrowserMessage::ToggleUrlEntry => self.on_toggle_url_entry(),
            BrowserMessage::Navigate => self.on_browser_navigate(),
            BrowserMessage::AutocompletePick(url) => {
                self.browser.panel.draft = url;
                self.on_browser_navigate()
            }
            BrowserMessage::Back => self.on_browser_go_back(),
            BrowserMessage::Forward => self.on_browser_go_forward(),
            BrowserMessage::HistoryOpen { back } => self.on_browser_history_open(back),
            BrowserMessage::HistoryPick(index) => self.on_browser_history_pick(index),
            BrowserMessage::HistoryCancel => self.on_browser_history_cancel(),
            BrowserMessage::OpenExternal => self.on_browser_open_external(),
            BrowserMessage::Reload => self.on_browser_reload(),
            BrowserMessage::Home => self.on_browser_go_home(),
            BrowserMessage::TabNew => self.on_browser_tab_new(),
            BrowserMessage::TabNext => self.on_browser_tab_next(),
            BrowserMessage::TabSwitch(id) => self.on_browser_tab_switch(id),
            BrowserMessage::TabClose(id) => self.on_browser_tab_close(id),
            BrowserMessage::BookmarkStar => self.on_browser_bookmark_star(),
            BrowserMessage::BookmarkNameChanged(draft) => {
                self.browser.panel.bookmark_name_draft = draft;
                Task::none()
            }
            BrowserMessage::BookmarkSave => self.on_browser_bookmark_save(),
            BrowserMessage::BookmarkRemove => self.on_browser_bookmark_remove(),
            BrowserMessage::Scrolled(y) => {
                self.browser.panel.note_scrolled(y);
                Task::none()
            }
            #[cfg(debug_assertions)]
            BrowserMessage::Shot(shot) => self.save_browser_shot(shot),
        }
    }
}

impl super::App {
    /// Show the standalone Browser window (idempotent). Restores the persisted
    /// tab strip on a fresh session, else resumes the cached page.
    pub(crate) fn open_browser_window(&mut self) -> Task<Message> {
        if self.browser_visible
            && let Some(id) = self.browser_window
        {
            return crate::ui::layout::focus_window(id);
        }
        if self.browser_window.is_some() {
            crate::controller::hid::diag::diag_info(
                "ui-diag: browser open skipped (window still closing)",
            );
            return Task::none();
        }
        if self.browser.engine.is_none() {
            crate::controller::hid::diag::diag_info(
                "ui-diag: browser open skipped (engine not installed)",
            );
            return Task::none();
        }
        // Start and Browser are mutually exclusive: opening one dismisses
        // the other rather than stacking two fullscreen apps.
        let close_start = if self.start_visible {
            self.close_start_screen()
        } else {
            Task::none()
        };
        let cover =
            crate::ui::layout::primary_monitor_cover().unwrap_or(crate::ui::layout::MonitorCover {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
            });
        self.browser_monitor_cover = Some(cover);
        // A new window can land on a different monitor; the cover drives the
        // first layout until its own resize event arrives.
        self.browser_window_size = None;
        self.sync_browser_viewport();
        // Center before the first frame, not after the window has painted the
        // previous open's cursor position.
        self.browser.panel.cursor.reset_for_page();
        self.browser.panel.last_sent_mouse = None;
        self.browser_visible = true;
        let was_open = self.browser.panel.session.is_open();
        let (id, open) = iced::window::open(iced::window::Settings {
            size: cover.size(),
            position: iced::window::Position::Specific(cover.origin()),
            visible: crate::ui::layout::open_visible(),
            // The 16:9 card sits over a dimmed desktop; the root view draws
            // the scrim on backends that can composite alpha.
            transparent: crate::ui::chrome::transparent_windows(),
            resizable: false,
            decorations: false,
            level: iced::window::Level::Normal,
            exit_on_close_request: true,
            platform_specific: crate::ui::layout::window_platform_specific(),
            ..iced::window::Settings::default()
        });
        self.browser_window = Some(id);
        self.arm_chord_release_latch();
        // Fetch conventional bookmark icons once the engine and the UI event
        // subscription are alive; restored bookmarks would otherwise stay
        // iconless until each site is visited.
        self.prefetch_bookmark_favicons();
        crate::controller::hid::diag::diag_info("ui-diag: browser window open");
        let opened = open.map(Message::BrowserOpened);
        let opened = if was_open {
            // Reopen after close: cached page, suspended engine.
            self.browser.panel.session.open_surface();
            self.resume_browser_engine();
            opened
        } else {
            // Fresh surface: open the tab strip (restore) or home.
            opened.chain(self.open_browser_session())
        };
        close_start.chain(opened)
    }

    /// Close the standalone Browser window and suspend its engine (cached
    /// page survives for the next open).
    pub(crate) fn close_browser_window(&mut self) -> Task<Message> {
        self.browser_visible = false;
        self.browser_monitor_cover = None;
        self.browser_window_size = None;
        let task = self.close_browser_surface();
        let Some(id) = self.browser_window.take() else {
            return task;
        };
        crate::controller::hid::diag::diag_info("ui-diag: browser window close");
        let close = iced::window::close(id);
        let task = task.chain(close);
        if self.start_visible
            && let Some(start) = self.start_window
        {
            return task.chain(crate::ui::layout::focus_window(start));
        }
        task
    }

    /// Ask the engine for `/favicon.ico` for remembered sites that have no
    /// cached icon yet. Space is bounded and origins are deduped; the worker
    /// also dedupes across opens.
    fn prefetch_bookmark_favicons(&mut self) {
        let Some(engine) = &self.browser.engine else {
            return;
        };
        let mut seen = std::collections::HashSet::new();
        for bookmark in self.browser.panel.bookmarks.iter().take(32) {
            let key = crate::browser::bookmarks::origin_key(&bookmark.url);
            if key == bookmark.url
                || self.browser.panel.favicons.contains_key(&key)
                || !seen.insert(key)
            {
                continue;
            }
            engine.send(crate::browser::engine::EngineCommand::PrefetchFavicon {
                url: bookmark.url.clone(),
            });
        }
    }

    pub(crate) fn on_browser_opened(&mut self, id: iced::window::Id) -> Task<Message> {
        if self.browser_window != Some(id) {
            return Task::none();
        }
        self.browser_visible = true;
        crate::controller::hid::diag::diag_info(format!("ui-diag: browser opened id={id:?}"));
        // The cover is logical pixels and already drives this first layout.
        // The real size arrives through `window::resize_events()` once the
        // fullscreen surface is configured; a one-shot `window::size()` here
        // can race creation and report the pre-fullscreen default, which
        // would briefly shrink the cursor space (the quarter-screen bug).
        self.sync_browser_viewport();
        crate::ui::layout::focus_window(id)
    }

    pub(crate) fn on_browser_window_sized(&mut self, size: iced::Size) -> Task<Message> {
        let size = (size.width.max(1.0), size.height.max(1.0));
        // The Browser window is a borderless fullscreen window sized to the
        // monitor cover. iced configures the surface once at its pre-fullscreen
        // default (observed 1440x789) before the final resize; adopting that
        // would briefly shrink the card, engine viewport, and cursor space.
        // The cover is the intended size, so ignore any other size.
        if let Some(cover) = self.browser_monitor_cover
            && ((size.0 - cover.width).abs() > 1.0 || (size.1 - cover.height).abs() > 1.0)
        {
            return Task::none();
        }
        if self.browser_window_size == Some(size) {
            return Task::none();
        }
        self.browser_window_size = Some(size);
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: browser window size {:.0}x{:.0}",
            size.0, size.1
        ));
        self.sync_browser_viewport();
        Task::none()
    }

    pub(crate) fn on_browser_frame(&mut self) -> Task<Message> {
        if !self.browser_visible {
            return Task::none();
        }
        // Cursor glides and script-driven scroll still need one mouse sync
        // per paint even when no new pad edge arrived.
        if let Some(engine) = &self.browser.engine {
            let (dx, dy) = std::mem::take(&mut self.browser.pending_scroll);
            if dx != 0.0 || dy != 0.0 {
                let (x, y) = self.browser.pending_scroll_at;
                engine.send(EngineCommand::ScrollBy {
                    tab: self.browser_tab(),
                    dx,
                    dy,
                    x,
                    y,
                });
            }
        }
        let sync = self.sync_browser_mouse();
        #[cfg(debug_assertions)]
        {
            let shot = self.shoot_browser_window(false);
            sync.chain(shot)
        }
        #[cfg(not(debug_assertions))]
        {
            sync
        }
    }
}

impl super::App {
    /// Pad tick while the standalone Browser window is the owner. The hub is
    /// routed before this, so the browser never sees hub presses.
    pub(crate) fn on_browser_pad(
        &mut self,
        readings: &[crate::ui::start::input::NavReading],
    ) -> Task<Message> {
        use crate::browser::cursor::JumpDir;
        use crate::domain::pad::NavAction;
        if readings.is_empty() {
            return Task::none();
        }
        // PS is global: while Browser owns the pad it must still open the hub
        // (the hub-open path routes PS to close once visible).
        if self.hub_chord_fires(readings) {
            crate::controller::hid::diag::diag_info("ui-diag: PS opens hub over Browser");
            return self.toggle_hub();
        }
        let now = std::time::Instant::now();
        let history_open = self.browser.panel.history_selector.is_some();
        let engine_mode = self.browser.panel.session.has_frame();
        // History selector uses D-pad nav; engine mode parks it (cursor
        // jumps run on the dedicated stepper).
        let allow_nav = history_open || !engine_mode;
        let tick = self.browser.nav.tick(
            readings, now, allow_nav, true, false, false, false, false, false,
        );
        self.set_haptic_pad_from_id(
            tick.action_pad
                .as_ref()
                .or(tick.cross_completed_pad.as_ref())
                .or(tick.triangle_completed_pad.as_ref()),
        );
        if history_open {
            return self.tick_browser_history_selector(readings, tick.action);
        }
        if engine_mode {
            self.tick_browser_cursor(readings, now);
            if let Some(jump) = self.poll_browser_dpad_jump(readings, now) {
                self.browser.panel.jump_cursor(jump);
            }
            // R3 issue flag (R3 carries no NavAction).
            let r3_down = readings.iter().any(|r| {
                r.sample
                    .held
                    .contains(&crate::ui::start::gesture::GestureControl::R3)
            });
            let latched = std::mem::replace(&mut self.browser.panel.r3_latched, r3_down);
            let flag = if r3_down && !latched {
                self.flag_browser_issue(now)
            } else {
                Task::none()
            };
            let action = match tick.action {
                Some(NavAction::Up) => {
                    self.browser.panel.jump_cursor(JumpDir::Up);
                    Task::none()
                }
                Some(NavAction::Down) => {
                    self.browser.panel.jump_cursor(JumpDir::Down);
                    Task::none()
                }
                Some(NavAction::Left) => {
                    self.browser.panel.jump_cursor(JumpDir::Left);
                    Task::none()
                }
                Some(NavAction::Right) => {
                    self.browser.panel.jump_cursor(JumpDir::Right);
                    Task::none()
                }
                Some(NavAction::Confirm) => {
                    if self.browser.panel.url_editing {
                        self.on_browser_message(crate::ui::browser::BrowserMessage::Navigate)
                    } else {
                        self.click_browser_cursor()
                    }
                }
                Some(NavAction::Cancel) => self.browser_cancel(),
                Some(NavAction::BumperLeft) => {
                    self.on_browser_message(crate::ui::browser::BrowserMessage::Back)
                }
                Some(NavAction::BumperRight) => {
                    self.on_browser_message(crate::ui::browser::BrowserMessage::Forward)
                }
                Some(NavAction::BumperHoldLeft) => {
                    self.on_browser_message(crate::ui::browser::BrowserMessage::HistoryOpen {
                        back: true,
                    })
                }
                Some(NavAction::BumperHoldRight) => {
                    self.on_browser_message(crate::ui::browser::BrowserMessage::HistoryOpen {
                        back: false,
                    })
                }
                Some(NavAction::Triangle) => {
                    self.on_browser_message(crate::ui::browser::BrowserMessage::ToggleUrlEntry)
                }
                Some(NavAction::ToggleMagnetism) => {
                    let cursor = &mut self.browser.panel.cursor;
                    cursor.magnet = !cursor.magnet;
                    if !cursor.magnet {
                        cursor.attached = None;
                        cursor.no_dock = None;
                    }
                    crate::platform::app_log::info(format!(
                        "ui-diag: browser magnet {}",
                        if cursor.magnet { "on" } else { "off" }
                    ));
                    Task::none()
                }
                _ => Task::none(),
            };
            let sync = self.sync_browser_mouse();
            Task::batch(vec![flag, action, sync])
        } else {
            // Reader fallback: D-pad scrolls text, Cross navigates.
            match tick.action {
                Some(NavAction::Up) | Some(NavAction::Left) => {
                    self.scroll_browser(-crate::ui::browser::view::BROWSER_SCROLL_STEP)
                }
                Some(NavAction::Down) | Some(NavAction::Right) => {
                    self.scroll_browser(crate::ui::browser::view::BROWSER_SCROLL_STEP)
                }
                Some(NavAction::Confirm) => {
                    self.on_browser_message(crate::ui::browser::BrowserMessage::Navigate)
                }
                Some(NavAction::Triangle) => {
                    self.on_browser_message(crate::ui::browser::BrowserMessage::ToggleUrlEntry)
                }
                Some(NavAction::Cancel) => self.browser_cancel(),
                Some(NavAction::BumperLeft) => {
                    self.on_browser_message(crate::ui::browser::BrowserMessage::Back)
                }
                Some(NavAction::BumperRight) => {
                    self.on_browser_message(crate::ui::browser::BrowserMessage::Forward)
                }
                Some(NavAction::BumperHoldLeft) => {
                    self.on_browser_message(crate::ui::browser::BrowserMessage::HistoryOpen {
                        back: true,
                    })
                }
                Some(NavAction::BumperHoldRight) => {
                    self.on_browser_message(crate::ui::browser::BrowserMessage::HistoryOpen {
                        back: false,
                    })
                }
                _ => Task::none(),
            }
        }
    }

    /// Circle/Esc at the root unwinds the browser's own modal layers before
    /// closing the window.
    pub(crate) fn browser_cancel(&mut self) -> Task<Message> {
        if self.browser.panel.bookmark_bubble {
            self.browser.panel.bookmark_bubble = false;
            return Task::none();
        }
        if self.browser.panel.url_editing {
            return self.on_browser_message(crate::ui::browser::BrowserMessage::ToggleUrlEntry);
        }
        if self.browser.panel.history_selector.is_some() {
            return self.on_browser_message(crate::ui::browser::BrowserMessage::HistoryCancel);
        }
        self.close_browser_window()
    }

    /// Keyboard while the standalone Browser window has focus.
    pub(crate) fn on_browser_key(&mut self, raw: crate::browser::keys::RawKey) -> Task<Message> {
        use crate::ui::browser::BrowserMessage;
        use iced::keyboard::Key;
        use iced::keyboard::key::Named;
        if matches!(raw.key, Key::Named(Named::Escape)) {
            if !raw.pressed {
                return Task::none();
            }
            return self.browser_cancel();
        }
        // The address field owns printable input natively while editing.
        if self.browser.panel.url_editing {
            return Task::none();
        }
        if raw.pressed {
            if raw.modifiers.control()
                && let Key::Character(text) = &raw.key
            {
                match text.to_lowercase().as_str() {
                    "l" => return self.on_browser_message(BrowserMessage::ToggleUrlEntry),
                    "r" => return self.on_browser_message(BrowserMessage::Reload),
                    _ => {}
                }
            }
            if raw.modifiers.alt() {
                match &raw.key {
                    Key::Named(Named::ArrowLeft) => {
                        return self.on_browser_message(BrowserMessage::Back);
                    }
                    Key::Named(Named::ArrowRight) => {
                        return self.on_browser_message(BrowserMessage::Forward);
                    }
                    _ => {}
                }
            }
        }
        if self.browser.engine.is_some() {
            if let Some(engine) = &self.browser.engine {
                let tab = self.browser_tab();
                engine.send(crate::browser::engine::EngineCommand::Key(
                    crate::browser::engine::TabKey {
                        tab,
                        event: crate::browser::keys::map_event(&raw),
                    },
                ));
            }
            return Task::none();
        }
        // Reader fallback keyboard.
        if !raw.pressed {
            return Task::none();
        }
        match raw.key {
            Key::Named(Named::ArrowUp) => {
                self.scroll_browser(-crate::ui::browser::view::BROWSER_SCROLL_STEP)
            }
            Key::Named(Named::ArrowDown) => {
                self.scroll_browser(crate::ui::browser::view::BROWSER_SCROLL_STEP)
            }
            Key::Named(Named::Enter) => self.on_browser_message(BrowserMessage::Navigate),
            _ => Task::none(),
        }
    }
}

impl super::App {
    /// Re-check the optional engine package and spawn CEF if it appeared
    /// (Store install completed while the app stayed open, dev download, or a
    /// portable sidecar placed next to the exe).
    #[cfg(feature = "cef-engine")]
    pub(crate) fn refresh_engine_status(&mut self) {
        if self.browser.engine.is_some() {
            self.browser.engine_available = true;
            return;
        }
        let Some(path) = crate::browser::cef_backend::resolve_engine_dir() else {
            self.browser.engine_status = EngineStatus::Missing;
            return;
        };
        let (w, h) = self
            .browser_monitor_cover
            .map(|c| (c.width, c.height))
            .unwrap_or((1920.0, 1080.0));
        let (page_w, page_h) = crate::ui::browser::view::fullscreen_layout(w, h);
        if let Some(engine) =
            crate::browser::cef_backend::spawn_engine(&crate::browser::tabs::boot_url(), {
                (page_w.max(1.0) as u32, page_h.max(1.0) as u32)
            })
        {
            self.browser.engine = Some(engine);
            self.browser.engine_available = true;
            self.browser.engine_status = EngineStatus::Ready(path);
            crate::platform::app_log::info("cef: engine attached after install");
        } else {
            self.browser.engine_status =
                EngineStatus::Failed("Browser engine could not be initialized".to_string());
        }
    }

    #[cfg(not(feature = "cef-engine"))]
    pub(crate) fn refresh_engine_status(&mut self) {
        self.browser.engine_status = EngineStatus::Missing;
        self.browser.engine_available = false;
    }

    /// Start the optional engine install. Packaged builds hand off to the
    /// Store; unpackaged/dev builds use the configured zip feed.
    #[cfg(feature = "cef-engine")]
    pub(crate) fn start_engine_install(&mut self) -> Task<Message> {
        if matches!(self.browser.engine_status, EngineStatus::Installing) {
            return Task::none();
        }
        if self.browser.engine.is_some() {
            self.browser.engine_status = EngineStatus::Ready(
                crate::browser::cef_backend::resolve_engine_dir().unwrap_or_default(),
            );
            self.hub_state.install_done(true, None);
            return Task::none();
        }
        if crate::platform::packaged::is_packaged() {
            let url = std::env::var("SDSC_BROWSER_ENGINE_STORE_URL").unwrap_or_else(|_| {
                "ms-windows-store://search/?query=SDSC%20Utils%20Browser%20Engine".to_string()
            });
            let launched = crate::games::launch::launch_target(&url, "").map_err(|e| e.to_string());
            if let Err(err) = launched {
                crate::platform::app_log::warn(format!(
                    "engine install: could not open Store page: {err}"
                ));
            }
            let msg = "Finish installing Browser Engine in the Store, then reopen the hub";
            self.browser.engine_status = EngineStatus::Missing;
            self.hub_state.install_done(false, Some(msg.to_string()));
            return Task::none();
        }

        self.browser.engine_status = EngineStatus::Installing;
        self.hub_state.set_installing(true);
        Task::perform(
            async {
                super::spawn_blocking(|| {
                    crate::browser::engine_install::install_from_env()
                        .map_err(crate::AppError::Persist)
                })
                .await
            },
            |result| {
                Message::EngineInstallDone(match result {
                    Ok(Ok(path)) => Ok(path),
                    Ok(Err(err)) => Err(err.to_string()),
                    Err(err) => Err(err.to_string()),
                })
            },
        )
    }

    #[cfg(not(feature = "cef-engine"))]
    pub(crate) fn start_engine_install(&mut self) -> Task<Message> {
        self.hub_state.install_done(
            false,
            Some("This build has no browser engine support".to_string()),
        );
        Task::none()
    }

    #[cfg(feature = "cef-engine")]
    pub(crate) fn start_engine_uninstall(&mut self) -> Task<Message> {
        let EngineStatus::Ready(path) = &self.browser.engine_status else {
            return Task::none();
        };
        let path = path.clone();
        if crate::platform::packaged::is_packaged() {
            let _ = crate::games::launch::launch_target("ms-settings:appsfeatures", "");
            self.hub_state.install_done(
                false,
                Some("Remove Browser Engine in Windows Settings, then reopen the hub".to_string()),
            );
            return Task::none();
        }
        if let Some(engine) = self.browser.engine.take() {
            let (tx, _rx) = std::sync::mpsc::channel();
            engine.send(EngineCommand::Shutdown { done: tx });
        }
        self.browser.engine_available = false;
        self.browser.engine_status = EngineStatus::Missing;
        match crate::browser::engine_install::uninstall_local(&path) {
            Ok(()) => {
                self.hub_state.install_done(false, None);
                crate::platform::app_log::info("cef: BrowserEngine uninstalled");
            }
            Err(err) => {
                crate::platform::app_log::warn(format!("cef: uninstall failed: {err}"));
                self.hub_state.install_done(false, Some(err));
            }
        }
        Task::none()
    }

    #[cfg(not(feature = "cef-engine"))]
    pub(crate) fn start_engine_uninstall(&mut self) -> Task<Message> {
        self.hub_state.install_done(
            false,
            Some("This build has no browser engine support".to_string()),
        );
        Task::none()
    }

    #[cfg(feature = "cef-engine")]
    pub(crate) fn on_engine_install_done(
        &mut self,
        result: Result<std::path::PathBuf, String>,
    ) -> Task<Message> {
        match result {
            Ok(path) => {
                self.browser.engine_status = EngineStatus::Ready(path);
                self.browser.engine_available = true;
                self.refresh_engine_status();
                if self.browser.engine.is_some() {
                    self.hub_state.install_done(true, None);
                    crate::platform::app_log::info("cef: BrowserEngine install complete");
                } else {
                    let msg = "Browser engine installed but failed to start".to_string();
                    self.browser.engine_status = EngineStatus::Failed(msg.clone());
                    self.hub_state.install_done(false, Some(msg));
                }
            }
            Err(err) => {
                crate::platform::app_log::warn(format!("cef: BrowserEngine install failed: {err}"));
                self.browser.engine_available = false;
                self.browser.engine_status = EngineStatus::Failed(err.clone());
                self.hub_state.install_done(false, Some(err));
            }
        }
        Task::none()
    }

    #[cfg(not(feature = "cef-engine"))]
    pub(crate) fn on_engine_install_done(
        &mut self,
        _result: Result<std::path::PathBuf, String>,
    ) -> Task<Message> {
        Task::none()
    }
}
