//! Top-level Browser app state.
//!
//! Phase 0 of the hub pivot lifts the browser out of `StartState`: the panel,
//! engine handle, capability flag, and browser-only D-pad stepper live here so
//! the browser no longer disappears when the Start screen is not the owner.
//! Behavior is unchanged — Start still renders the panel until the standalone
//! Browser window lands. See `notes/browser-hub-pivot.md`.

use crate::browser::engine::EngineHandle;
use crate::domain::pad::NavStepper;
use crate::ui::browser::BrowserPanel;

#[derive(Default)]
pub struct BrowserAppState {
    /// Tabs, chrome targets, cursor, bookmarks, and per-tab page state.
    pub panel: BrowserPanel,
    /// CEF OSR worker. `None` in UI-only builds or when CEF is missing.
    pub engine: Option<EngineHandle>,
    /// Process-static capability: true when an engine worker is live. Drives
    /// the page placeholder copy and, later, the hub tile install state.
    pub engine_available: bool,
    /// Engine-mode D-pad cursor jumps run on this standalone stepper (fed a
    /// stickless `dpad_only` mask): the Start bank stays parked in engine
    /// mode so stick moves never double-fire into both cursor and nav.
    pub dpad_stepper: NavStepper,
    /// Browser-window pad bank: face/bumper edges + reader nav. The engine
    /// cursor uses `dpad_stepper` and raw stick samples instead.
    pub nav: crate::domain::pad::PadNavBank,
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
        let cover =
            crate::ui::layout::primary_monitor_cover().unwrap_or(crate::ui::layout::MonitorCover {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
            });
        self.browser_monitor_cover = Some(cover);
        self.browser_visible = true;
        let was_open = self.browser.panel.session.is_open();
        let (id, open) = iced::window::open(iced::window::Settings {
            size: cover.size(),
            position: iced::window::Position::Specific(cover.origin()),
            visible: crate::ui::layout::open_visible(),
            // Opaque app window: the root view paints the full backdrop.
            transparent: false,
            resizable: false,
            decorations: false,
            level: iced::window::Level::Normal,
            exit_on_close_request: true,
            platform_specific: crate::ui::layout::window_platform_specific(),
            ..iced::window::Settings::default()
        });
        self.browser_window = Some(id);
        self.arm_chord_release_latch();
        crate::controller::hid::diag::diag_info("ui-diag: browser window open");
        let opened = open.map(Message::BrowserOpened);
        if was_open {
            // Reopen after close: cached page, suspended engine.
            self.browser.panel.session.open_surface();
            self.resume_browser_engine();
            opened
        } else {
            // Fresh surface: open the tab strip (restore) or home.
            opened.chain(self.open_browser_session())
        }
    }

    /// Close the standalone Browser window and suspend its engine (cached
    /// page survives for the next open).
    pub(crate) fn close_browser_window(&mut self) -> Task<Message> {
        self.browser_visible = false;
        self.browser_monitor_cover = None;
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

    pub(crate) fn on_browser_opened(&mut self, id: iced::window::Id) -> Task<Message> {
        if self.browser_window != Some(id) {
            return Task::none();
        }
        self.browser_visible = true;
        crate::controller::hid::diag::diag_info(format!("ui-diag: browser opened id={id:?}"));
        self.sync_browser_viewport();
        crate::ui::layout::focus_window(id)
    }

    pub(crate) fn on_browser_frame(&mut self) -> Task<Message> {
        if !self.browser_visible {
            return Task::none();
        }
        // Cursor glides and script-driven scroll still need one mouse sync
        // per paint even when no new pad edge arrived.
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
        let now = std::time::Instant::now();
        let history_open = self.browser.panel.history_selector.is_some();
        let engine_mode = self.browser.panel.session.has_frame();
        // History selector and the reader path use D-pad nav; engine mode
        // parks it (cursor jumps run on the dedicated stepper).
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
