//! Top-level Browser app state.
//!
//! Phase 0 of the hub pivot lifts the browser out of `LauncherState`: the panel,
//! engine handle, capability flag, and browser-only D-pad stepper live here so
//! the browser no longer disappears when the Launcher is not the owner.
//! Behavior is unchanged — Launcher still renders the panel until the standalone
//! Browser window lands. See `notes/browser-hub-pivot.md`.

use crate::browser::engine::{EngineCommand, EngineHandle};
use crate::domain::pad::NavStepper;
use crate::platform::ui_sound::UiSoundKind;
use crate::ui::browser::BrowserPanel;
use std::path::PathBuf;

/// Result of a background local install/update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineInstallOutcome {
    Installed(PathBuf),
    /// The user asked to cancel; staging was cleaned and the previous state
    /// is restored.
    Cancelled,
    Failed(String),
}

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
    /// A local uninstall is in flight.
    Uninstalling,
    /// The last install attempt failed; message is user-facing.
    Failed(String),
}

impl EngineStatus {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }
}

/// EWMA sample used to render a stable live throughput while bytes move.
#[cfg(feature = "cef-engine")]
#[derive(Debug, Clone, Copy)]
pub struct EngineRateSample {
    pub at: Instant,
    pub bytes: u64,
    pub ewma_bps: f64,
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
    /// Guards live install progress against stragglers after completion.
    #[cfg(feature = "cef-engine")]
    pub engine_install_generation: u64,
    /// Cancel flag for the live install/update worker.
    #[cfg(feature = "cef-engine")]
    pub engine_install_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// State to restore when an install/update is cancelled.
    #[cfg(feature = "cef-engine")]
    pub engine_install_previous: Option<EngineStatus>,
    /// Latest fetched DLC manifest (feed metadata).
    #[cfg(feature = "cef-engine")]
    pub engine_manifest: Option<crate::browser::engine_install::EngineManifest>,
    /// Guards late manifest responses.
    #[cfg(feature = "cef-engine")]
    pub engine_manifest_generation: u64,
    /// Throughput sample for the in-tile MB/s readout.
    #[cfg(feature = "cef-engine")]
    pub engine_rate: Option<EngineRateSample>,
    /// Guards the background directory-size walk for Info.
    pub engine_info_generation: u64,
    /// Resolver selected a package-owned runtime (real MSIX or mock).
    pub engine_managed: bool,
    /// An update is on disk but the running CEF is still the old version.
    #[cfg(feature = "cef-engine")]
    pub engine_restart_required: bool,
    /// Accumulated analog scroll for the next BrowserFrame tick.
    pub pending_scroll: (f32, f32),
    /// Page point the pending scroll belongs to (cursor at accumulation).
    pub pending_scroll_at: (f32, f32),
    /// Engine-mode D-pad cursor jumps run on this standalone stepper (fed a
    /// stickless `dpad_only` mask): the Launcher bank stays parked in engine
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
use std::time::Instant;

use super::Message;

impl super::App {
    /// Browser-window message router. Phase 2 is moving the reducer here; the
    /// methods still live beside the Launcher reducer until the move completes.
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
            BrowserMessage::DefaultPage => self.on_browser_go_default(),
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
        // Launcher and Browser are mutually exclusive: opening one dismisses
        // the other rather than stacking two fullscreen apps.
        let close_start = if self.launcher_visible {
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
        self.browser_pending_show = true;
        let was_open = self.browser.panel.session.is_open();
        let (id, open) = iced::window::open(iced::window::Settings {
            size: cover.size(),
            position: iced::window::Position::Specific(cover.origin()),
            // Hidden until the final cover size is configured and DWM
            // transitions are disabled; showing earlier lets Windows animate
            // the create-time default size into the full-screen rect.
            visible: false,
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
            // Fresh surface: open the tab strip (restore) or the default page.
            opened.chain(self.open_browser_session())
        };
        close_start
            .chain(crate::ui::layout::disable_window_transitions(id))
            .chain(opened)
            .chain(Task::done(Message::WindowShowNow(id)))
    }

    /// Close the standalone Browser window and suspend its engine (cached
    /// page survives for the next open).
    pub(crate) fn close_browser_window(&mut self) -> Task<Message> {
        self.browser_visible = false;
        self.browser_monitor_cover = None;
        self.browser_window_size = None;
        self.browser_pending_show = false;
        self.browser_backdrop_fade = None;
        let task = self.close_browser_surface();
        let Some(id) = self.browser_window.take() else {
            return task;
        };
        crate::controller::hid::diag::diag_info("ui-diag: browser window close");
        let close = iced::window::close(id);
        let task = task.chain(close);
        if self.launcher_visible
            && let Some(start) = self.launcher_window
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
        Task::none()
    }

    /// Show the hidden Browser window after the DWM transition task has run
    /// and the surface has had a moment to settle at the cover size. Showing
    /// immediately lets Windows animate the create-time default size into the
    /// fullscreen rect (the visible "scale" on open).
    pub(crate) fn on_browser_show_now(&mut self) -> Task<Message> {
        if !self.browser_pending_show {
            return Task::none();
        }
        self.browser_pending_show = false;
        let Some(id) = self.browser_window else {
            return Task::none();
        };
        self.browser_backdrop_fade = Some(Instant::now());
        self.sync_browser_viewport();
        crate::ui::layout::show_window(id).chain(crate::ui::layout::focus_window(id))
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
        readings: &[crate::ui::launcher::input::NavReading],
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
                self.play_ui_cue(UiSoundKind::Nav);
                self.browser.panel.jump_cursor(jump);
            }
            // R3 issue flag (R3 carries no NavAction).
            let r3_down = readings.iter().any(|r| {
                r.sample
                    .held
                    .contains(&crate::ui::launcher::gesture::GestureControl::R3)
            });
            let latched = std::mem::replace(&mut self.browser.panel.r3_latched, r3_down);
            let flag = if r3_down && !latched {
                self.flag_browser_issue(now)
            } else {
                Task::none()
            };
            let action = match tick.action {
                Some(NavAction::Up) => {
                    self.play_ui_cue(UiSoundKind::Nav);
                    self.browser.panel.jump_cursor(JumpDir::Up);
                    Task::none()
                }
                Some(NavAction::Down) => {
                    self.play_ui_cue(UiSoundKind::Nav);
                    self.browser.panel.jump_cursor(JumpDir::Down);
                    Task::none()
                }
                Some(NavAction::Left) => {
                    self.play_ui_cue(UiSoundKind::Nav);
                    self.browser.panel.jump_cursor(JumpDir::Left);
                    Task::none()
                }
                Some(NavAction::Right) => {
                    self.play_ui_cue(UiSoundKind::Nav);
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
                    self.play_ui_cue(UiSoundKind::Action);
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
                    self.play_ui_cue(UiSoundKind::Nav);
                    self.scroll_browser(-crate::ui::browser::view::BROWSER_SCROLL_STEP)
                }
                Some(NavAction::Down) | Some(NavAction::Right) => {
                    self.play_ui_cue(UiSoundKind::Nav);
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
            // The engine may already be attached at boot (sidecar/package);
            // managed state still has to be derived from the loaded path.
            if let Some(dir) = crate::browser::cef_backend::loaded_engine_dir() {
                self.browser.engine_managed = crate::browser::cef_backend::engine_is_managed(&dir);
            }
            return;
        }
        let Some(path) = crate::browser::cef_backend::resolve_engine_dir() else {
            self.browser.engine_status = EngineStatus::Missing;
            self.browser.engine_managed = false;
            return;
        };
        self.browser.engine_managed = crate::browser::cef_backend::engine_is_managed(&path);
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
        self.browser.engine_managed = false;
    }

    /// Start the optional engine install/update. Packaged builds hand off to
    /// the Store; unpackaged/dev builds use the configured zip feed.
    #[cfg(feature = "cef-engine")]
    pub(crate) fn start_engine_install(&mut self) -> Task<Message> {
        if matches!(
            self.browser.engine_status,
            EngineStatus::Installing | EngineStatus::Uninstalling
        ) {
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

        self.browser.engine_install_previous = Some(self.browser.engine_status.clone());
        self.browser.engine_status = EngineStatus::Installing;
        self.browser.engine_rate = None;
        self.hub_state.set_installing(true);
        self.browser.engine_install_generation =
            self.browser.engine_install_generation.wrapping_add(1);
        let generation = self.browser.engine_install_generation;
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.browser.engine_install_cancel = std::sync::Arc::clone(&cancel);
        let limit = self.install_policy.handle();
        let (progress_tx, progress_rx) = iced::futures::channel::mpsc::unbounded::<
            crate::browser::engine_install::InstallProgress,
        >();
        let progress_task =
            Task::stream(progress_rx).map(move |progress| Message::EngineInstallProgress {
                generation,
                progress,
            });
        let install = Task::perform(
            async move {
                super::spawn_blocking(move || {
                    let mut progress = |progress| {
                        let _ = progress_tx.unbounded_send(progress);
                    };
                    match crate::browser::engine_install::install_with_progress(
                        &mut progress,
                        crate::browser::engine_install::InstallOptions {
                            download_limit_kbps: limit,
                            cancel,
                        },
                    ) {
                        Ok(path) => EngineInstallOutcome::Installed(path),
                        Err(crate::browser::engine_install::InstallError::Cancelled) => {
                            EngineInstallOutcome::Cancelled
                        }
                        Err(crate::browser::engine_install::InstallError::Failed(err)) => {
                            EngineInstallOutcome::Failed(err)
                        }
                    }
                })
                .await
            },
            move |result| Message::EngineInstallDone {
                generation,
                outcome: match result {
                    Ok(outcome) => outcome,
                    Err(err) => EngineInstallOutcome::Failed(err.to_string()),
                },
            },
        );
        Task::batch([progress_task, install])
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
        let close_browser = if self.browser_visible {
            self.close_browser_window()
        } else {
            Task::none()
        };
        self.browser.engine_status = EngineStatus::Uninstalling;
        self.hub_state.set_uninstalling();
        self.browser.engine_install_generation =
            self.browser.engine_install_generation.wrapping_add(1);
        let generation = self.browser.engine_install_generation;
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        if let Some(engine) = self.browser.engine.take() {
            engine.send(EngineCommand::Shutdown { done: done_tx });
        }
        self.browser.engine_available = false;
        let uninstall = Task::perform(
            async move {
                super::spawn_blocking(move || {
                    // Let the engine worker close its browsers before the
                    // pointer is dropped. The DLL stays mapped either way;
                    // the directory itself is deferred to next boot.
                    let _ = done_rx.recv_timeout(std::time::Duration::from_secs(2));
                    // Keep the inline meter visible long enough to read.
                    std::thread::sleep(std::time::Duration::from_millis(600));
                    crate::browser::engine_install::uninstall_local(&path)
                        .map_err(crate::AppError::Persist)
                })
                .await
            },
            move |result| Message::EngineUninstallDone {
                generation,
                result: match result {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(err)) => Err(err.to_string()),
                    Err(err) => Err(err.to_string()),
                },
            },
        );
        close_browser.chain(uninstall)
    }

    #[cfg(not(feature = "cef-engine"))]
    pub(crate) fn start_engine_uninstall(&mut self) -> Task<Message> {
        self.hub_state.install_done(
            false,
            Some("This build has no browser engine support".to_string()),
        );
        Task::none()
    }

    /// Request a cooperative cancel of the live install/update. The worker
    /// stops at the next chunk boundary, deletes staging, and reports
    /// `Cancelled`; the generation guard then restores the prior state.
    #[cfg(feature = "cef-engine")]
    pub(crate) fn cancel_engine_install(&mut self) -> Task<Message> {
        if !matches!(self.browser.engine_status, EngineStatus::Installing)
            || self.hub_state.cancelling
        {
            return Task::none();
        }
        self.browser
            .engine_install_cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.hub_state.set_cancelling();
        crate::platform::app_log::info("cef: BrowserEngine install cancel requested");
        Task::none()
    }

    #[cfg(not(feature = "cef-engine"))]
    pub(crate) fn cancel_engine_install(&mut self) -> Task<Message> {
        Task::none()
    }

    /// Fetch the DLC manifest in the background when the feed is configured.
    /// Called on hub open; no polling, no telemetry.
    #[cfg(feature = "cef-engine")]
    pub(crate) fn check_engine_manifest(&mut self) -> Task<Message> {
        if std::env::var_os("SDSC_CEF_DLC_MANIFEST_URL").is_none() {
            return Task::none();
        }
        self.browser.engine_manifest_generation =
            self.browser.engine_manifest_generation.wrapping_add(1);
        let generation = self.browser.engine_manifest_generation;
        Task::perform(
            async move { super::spawn_blocking(crate::browser::engine_install::fetch_manifest).await },
            move |result| Message::EngineManifestReady {
                generation,
                result: match result {
                    Ok(inner) => inner,
                    Err(err) => Err(err.to_string()),
                },
            },
        )
    }

    #[cfg(not(feature = "cef-engine"))]
    pub(crate) fn check_engine_manifest(&mut self) -> Task<Message> {
        Task::none()
    }

    #[cfg(feature = "cef-engine")]
    pub(crate) fn on_engine_manifest_ready(
        &mut self,
        generation: u64,
        result: Result<Option<crate::browser::engine_install::EngineManifest>, String>,
    ) -> Task<Message> {
        if generation != self.browser.engine_manifest_generation {
            return Task::none();
        }
        match result {
            Ok(Some(manifest)) => {
                self.browser.engine_manifest = Some(manifest);
                self.sync_hub_engine_info();
            }
            Ok(None) => {}
            Err(err) => {
                crate::platform::app_log::warn(format!("cef: engine manifest check failed: {err}"));
            }
        }
        Task::none()
    }

    /// Measure the active engine and pending-cleanup directories off the UI
    /// thread for the Info panel.
    #[cfg(feature = "cef-engine")]
    pub(crate) fn load_engine_info_sizes(&mut self) -> Task<Message> {
        self.sync_hub_engine_info();
        self.browser.engine_info_generation = self.browser.engine_info_generation.wrapping_add(1);
        let generation = self.browser.engine_info_generation;
        Task::perform(
            async move {
                super::spawn_blocking(|| {
                    // Measure the runtime the resolver actually uses, not just
                    // the local store (covers sidecar and MSIX/package dirs).
                    let size = crate::browser::cef_backend::resolve_engine_dir()
                        .or_else(crate::browser::cef_backend::loaded_engine_dir)
                        .map(|path| crate::browser::engine_install::dir_size(&path))
                        .unwrap_or(0);
                    let pending = crate::browser::engine_install::pending_cleanup_size();
                    (size, pending)
                })
                .await
            },
            move |result| Message::EngineInfoSize {
                generation,
                result: match result {
                    Ok(sizes) => Ok(sizes),
                    Err(err) => Err(err.to_string()),
                },
            },
        )
    }

    /// Refresh the hub's engine version/update/changelog view from the local
    /// install metadata and the last fetched manifest.
    #[cfg(feature = "cef-engine")]
    pub(crate) fn sync_hub_engine_info(&mut self) {
        let installed = crate::browser::engine_install::active_meta();
        let remote = self.browser.engine_manifest.clone();
        let installed_version = installed.as_ref().map(|(_, meta)| meta.version.clone());
        let managed = self.browser.engine_managed;
        let update_ready = !managed
            && remote.as_ref().is_some_and(|remote| {
                installed_version.as_deref().is_some_and(|current| {
                    crate::browser::engine_install::version_is_newer(&remote.version, current)
                })
            });
        let changelog = if update_ready {
            remote
                .as_ref()
                .map(|remote| remote.changelog.clone())
                .filter(|text| !text.trim().is_empty())
                .or_else(|| installed.as_ref().map(|(_, meta)| meta.changelog.clone()))
        } else {
            installed
                .as_ref()
                .map(|(_, meta)| meta.changelog.clone())
                .filter(|text| !text.trim().is_empty())
                .or_else(|| remote.as_ref().map(|remote| remote.changelog.clone()))
        };
        self.hub_state.engine_version = if managed {
            remote
                .as_ref()
                .map(|remote| remote.version.clone())
                .or(installed_version)
        } else {
            installed_version
        };
        self.hub_state.engine_latest_version = remote.as_ref().map(|remote| remote.version.clone());
        self.hub_state.engine_changelog = changelog.filter(|text| !text.trim().is_empty());
        self.hub_state.engine_size = installed
            .as_ref()
            .map(|(_, meta)| meta.size)
            .filter(|size| *size > 0);
        self.hub_state.engine_update_ready = update_ready;
        self.hub_state.engine_managed = managed;
        self.hub_state.engine_restart_required = self.browser.engine_restart_required;
    }

    #[cfg(not(feature = "cef-engine"))]
    pub(crate) fn sync_hub_engine_info(&mut self) {}

    #[cfg(feature = "cef-engine")]
    pub(crate) fn on_engine_install_progress(
        &mut self,
        generation: u64,
        progress: crate::browser::engine_install::InstallProgress,
    ) -> Task<Message> {
        if generation != self.browser.engine_install_generation
            || !matches!(self.browser.engine_status, EngineStatus::Installing)
            || self.hub_state.cancelling
        {
            return Task::none();
        }
        let rate = self.note_engine_rate(&progress);
        let mut mapped = hub_install_progress(progress);
        mapped.rate_bps = rate;
        self.hub_state.set_install_progress(mapped);
        Task::none()
    }

    #[cfg(feature = "cef-engine")]
    fn note_engine_rate(
        &mut self,
        progress: &crate::browser::engine_install::InstallProgress,
    ) -> Option<f64> {
        use crate::browser::engine_install::InstallProgress as Engine;
        let done = match progress {
            Engine::Downloading { received, .. } => *received,
            Engine::Verifying { done, .. } | Engine::Extracting { done, .. } => *done,
            _ => {
                self.browser.engine_rate = None;
                return None;
            }
        };
        let now = Instant::now();
        let sample = match self.browser.engine_rate {
            Some(prev) if done >= prev.bytes => {
                let elapsed = now.saturating_duration_since(prev.at).as_secs_f64();
                if elapsed < 0.02 {
                    prev
                } else {
                    let instant = (done - prev.bytes) as f64 / elapsed;
                    EngineRateSample {
                        at: now,
                        bytes: done,
                        ewma_bps: prev.ewma_bps * 0.65 + instant * 0.35,
                    }
                }
            }
            _ => EngineRateSample {
                at: now,
                bytes: done,
                ewma_bps: 0.0,
            },
        };
        self.browser.engine_rate = Some(sample);
        (sample.ewma_bps > 0.0).then_some(sample.ewma_bps)
    }

    #[cfg(feature = "cef-engine")]
    pub(crate) fn on_engine_install_done(
        &mut self,
        generation: u64,
        outcome: EngineInstallOutcome,
    ) -> Task<Message> {
        if generation != self.browser.engine_install_generation
            || !matches!(self.browser.engine_status, EngineStatus::Installing)
        {
            return Task::none();
        }
        // Invalidate any progress messages still buffered behind the result.
        self.browser.engine_install_generation =
            self.browser.engine_install_generation.wrapping_add(1);
        self.browser.engine_rate = None;
        match outcome {
            EngineInstallOutcome::Installed(path) => {
                self.browser.engine_install_previous = None;
                self.browser.engine_status = EngineStatus::Ready(path.clone());
                self.browser.engine_available = true;
                self.browser.engine_managed = false;
                if self.browser.engine.is_some() {
                    // Update: CEF is process-global, so the old runtime stays
                    // loaded until restart. The new directory is selected by
                    // the pointer on the next boot.
                    self.browser.engine_restart_required =
                        crate::browser::cef_backend::loaded_engine_dir().as_deref()
                            != Some(path.as_path());
                } else {
                    self.refresh_engine_status();
                }
                if self.browser.engine.is_some() {
                    self.hub_state.install_done(true, None);
                    crate::platform::app_log::info(format!(
                        "cef: BrowserEngine install complete{}",
                        if self.browser.engine_restart_required {
                            " (restart required)"
                        } else {
                            ""
                        }
                    ));
                } else {
                    let msg = "Browser engine installed but failed to start".to_string();
                    self.browser.engine_status = EngineStatus::Failed(msg.clone());
                    self.hub_state.install_done(false, Some(msg));
                }
            }
            EngineInstallOutcome::Cancelled => {
                let previous = self.browser.engine_install_previous.take();
                let ready = previous.as_ref().is_some_and(EngineStatus::is_ready);
                self.browser.engine_status = previous.unwrap_or(EngineStatus::Missing);
                self.browser.engine_available = self.browser.engine.is_some();
                self.hub_state.install_cancelled(ready);
                crate::platform::app_log::info("cef: BrowserEngine install cancelled");
            }
            EngineInstallOutcome::Failed(err) => {
                crate::platform::app_log::warn(format!("cef: BrowserEngine install failed: {err}"));
                let previous = self.browser.engine_install_previous.take();
                let ready = previous.as_ref().is_some_and(EngineStatus::is_ready);
                // A failed update keeps the working engine and offers Retry
                // instead of dimming the app to Missing.
                self.browser.engine_status = previous
                    .filter(|status| status.is_ready())
                    .unwrap_or_else(|| EngineStatus::Failed(err.clone()));
                self.hub_state.install_done(ready, Some(err));
            }
        }
        self.sync_hub_engine_info();
        Task::none()
    }

    #[cfg(feature = "cef-engine")]
    pub(crate) fn on_engine_uninstall_done(
        &mut self,
        generation: u64,
        result: Result<(), String>,
    ) -> Task<Message> {
        if generation != self.browser.engine_install_generation
            || !matches!(self.browser.engine_status, EngineStatus::Uninstalling)
        {
            return Task::none();
        }
        self.browser.engine_install_generation =
            self.browser.engine_install_generation.wrapping_add(1);
        match result {
            Ok(()) => {
                self.browser.engine_status = EngineStatus::Missing;
                self.browser.engine_available = false;
                self.browser.engine_managed = false;
                self.browser.engine_restart_required = false;
                self.hub_state.uninstall_done(None);
                crate::platform::app_log::info("cef: BrowserEngine uninstalled");
            }
            Err(err) => {
                crate::platform::app_log::warn(format!("cef: uninstall failed: {err}"));
                self.browser.engine_status = EngineStatus::Failed(err.clone());
                self.hub_state.uninstall_done(Some(err));
            }
        }
        self.sync_hub_engine_info();
        Task::none()
    }

    #[cfg(not(feature = "cef-engine"))]
    pub(crate) fn on_engine_install_done(
        &mut self,
        _generation: u64,
        _outcome: EngineInstallOutcome,
    ) -> Task<Message> {
        Task::none()
    }
}

#[cfg(feature = "cef-engine")]
fn hub_install_progress(
    progress: crate::browser::engine_install::InstallProgress,
) -> crate::ui::hub::EngineProgress {
    use crate::browser::engine_install::InstallProgress as Engine;
    use crate::ui::hub::{EnginePhase, EngineProgress};
    match progress {
        Engine::Starting => EngineProgress {
            phase: EnginePhase::Preparing,
            fraction: None,
            bytes_done: 0,
            bytes_total: None,
            rate_bps: None,
        },
        Engine::Downloading { received, total } => EngineProgress {
            phase: EnginePhase::Downloading,
            fraction: total
                .filter(|total| *total > 0)
                .map(|total| received as f32 / total as f32),
            bytes_done: received,
            bytes_total: total,
            rate_bps: None,
        },
        Engine::Verifying { done, total } => EngineProgress {
            phase: EnginePhase::Verifying,
            fraction: (total > 0).then(|| done as f32 / total as f32),
            bytes_done: done,
            bytes_total: Some(total),
            rate_bps: None,
        },
        Engine::Extracting { done, total } => EngineProgress {
            phase: EnginePhase::Installing,
            fraction: (total > 0).then(|| done as f32 / total as f32),
            bytes_done: done,
            bytes_total: Some(total),
            rate_bps: None,
        },
        Engine::Finishing => EngineProgress {
            phase: EnginePhase::Finishing,
            fraction: None,
            bytes_done: 0,
            bytes_total: None,
            rate_bps: None,
        },
    }
}
