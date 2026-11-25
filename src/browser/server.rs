//! Browser add-on process: host protocol over stdio + own iced window/CEF.
//!
//! No tray, HID, Home, Launcher, or OSK state lives here. The main app streams
//! pad/action/text/lifecycle messages; this process owns the page surface.

use crate::browser::engine::{EngineCommand, EngineEvent, EngineHandle, TabKey};
use crate::browser::keys::{RawKey, map_event, synthetic_named, synthetic_text};
use crate::domain::pad::{NavAction, NavStepper, PadSample};
use crate::hosted::{
    ActionInvoked, BrowserEvent, Hello, HostMessage, InputActionKind, InputButton,
    LifecycleCommand, LifecycleState, PadSnapshot, RealInput, TextAction, read_frame, write_frame,
};
use crate::ui::browser::{BrowserMessage, BrowserPanel};
use crate::ui::theme;
use iced::futures::SinkExt;
use iced::widget::text;
use iced::{Element, Fill, Point, Size, Subscription, Task, Theme, stream, window};
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

static NEXT_LINK_ID: AtomicU64 = AtomicU64::new(1);

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut stdin = std::io::stdin();
    let hello: Hello = read_frame(&mut stdin)?;
    hello.validate()?;

    iced::daemon(
        move || BrowserServer::boot(hello.clone()),
        BrowserServer::update,
        BrowserServer::view,
    )
    .subscription(BrowserServer::subscription)
    .title(BrowserServer::title)
    .theme(BrowserServer::theme)
    .style(BrowserServer::style)
    .run()?;
    Ok(())
}

#[derive(Debug, Clone)]
enum Message {
    Opened(window::Id),
    Sized(window::Id, Size),
    Closed(window::Id),
    Engine(EngineEvent),
    Host(HostMessage),
    Browser(BrowserMessage),
    Key(RawKey),
    Ime(iced::advanced::input_method::Event),
    Touch,
    Tick,
}

struct BrowserServer {
    panel: BrowserPanel,
    engine: Option<EngineHandle>,
    window: Option<window::Id>,
    stage: (f32, f32),
    host: Arc<Mutex<BufWriter<std::io::Stdout>>>,
    host_link: HostLink,
    engine_link: EngineLink,
    ready_sent: bool,
    osk_open: bool,
    dpad: NavStepper,
}

impl BrowserServer {
    fn boot(hello: Hello) -> (Self, Task<Message>) {
        let _ = hello;
        let cover =
            crate::ui::layout::primary_monitor_cover().unwrap_or(crate::ui::layout::MonitorCover {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
            });
        let (page_w, page_h) =
            crate::ui::browser::view::fullscreen_layout(cover.width, cover.height);
        let boot_url = crate::browser::tabs::boot_url();
        let mut panel = BrowserPanel::default();
        panel.set_viewport(page_w, page_h);
        panel.session.open_surface();
        panel.session.begin_load(boot_url.clone());

        #[cfg(feature = "cef-engine")]
        let engine = crate::browser::cef_backend::spawn_engine(
            &boot_url,
            (
                page_w.round().max(1.0) as u32,
                page_h.round().max(1.0) as u32,
            ),
        );
        #[cfg(not(feature = "cef-engine"))]
        let engine: Option<EngineHandle> = {
            let _ = (&boot_url, page_w, page_h);
            None
        };

        let host = Arc::new(Mutex::new(BufWriter::new(std::io::stdout())));
        let (host_tx, host_rx) = iced::futures::channel::mpsc::unbounded::<HostMessage>();
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            while let Ok(message) = read_frame::<_, HostMessage>(&mut stdin) {
                if host_tx.unbounded_send(message).is_err() {
                    break;
                }
            }
        });
        let host_link = HostLink {
            id: NEXT_LINK_ID.fetch_add(1, Ordering::Relaxed),
            rx: Arc::new(Mutex::new(Some(host_rx))),
        };
        let engine_link = EngineLink {
            id: NEXT_LINK_ID.fetch_add(1, Ordering::Relaxed),
            engine: engine.clone(),
        };

        let (id, open) = window::open(window::Settings {
            size: Size::new(cover.width, cover.height),
            position: window::Position::Specific(Point::new(cover.x, cover.y)),
            visible: false,
            transparent: crate::ui::chrome::transparent_windows(),
            resizable: false,
            decorations: false,
            level: window::Level::Normal,
            exit_on_close_request: true,
            platform_specific: crate::ui::layout::window_platform_specific(),
            ..window::Settings::default()
        });

        let state = Self {
            panel,
            engine,
            window: Some(id),
            stage: (cover.width, cover.height),
            host,
            host_link,
            engine_link,
            ready_sent: false,
            osk_open: false,
            dpad: NavStepper::default(),
        };
        let show = open
            .map(Message::Opened)
            .chain(crate::ui::layout::disable_window_transitions(id));
        (state, show)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Opened(id) => {
                if self.window != Some(id) {
                    return Task::none();
                }
                self.publish(true);
                crate::ui::layout::show_window(id).chain(crate::ui::layout::focus_window(id))
            }
            Message::Sized(id, size) => {
                if self.window != Some(id) {
                    return Task::none();
                }
                let (page_w, page_h) = crate::ui::browser::view::fullscreen_layout(
                    size.width.max(1.0),
                    size.height.max(1.0),
                );
                self.stage = (size.width.max(1.0), size.height.max(1.0));
                if self.panel.set_viewport(page_w, page_h)
                    && let Some(engine) = &self.engine
                {
                    engine.send(EngineCommand::Resize {
                        width: page_w.round().max(1.0) as u32,
                        height: page_h.round().max(1.0) as u32,
                    });
                }
                Task::none()
            }
            Message::Closed(id) => {
                if self.window == Some(id) {
                    self.shutdown_engine();
                    iced::exit()
                } else {
                    Task::none()
                }
            }
            Message::Engine(event) => self.on_engine_event(event),
            Message::Host(message) => self.on_host_message(message),
            Message::Browser(message) => self.on_browser_message(message),
            Message::Key(raw) => self.on_key(raw),
            Message::Ime(event) => self.on_ime(event),
            Message::Touch => {
                if self.osk_open {
                    self.send(BrowserEvent::RealInput(RealInput::Touch));
                }
                Task::none()
            }
            Message::Tick => {
                let now = Instant::now();
                self.panel.advance_stick(0.0, 0.0, 0.0, 0.0, false, now);
                self.sync_mouse();
                Task::none()
            }
        }
    }

    fn view(&self, id: window::Id) -> Element<'_, Message> {
        if self.window != Some(id) {
            return text("").width(Fill).height(Fill).into();
        }
        crate::ui::browser::view::root_view(
            &self.panel,
            self.engine.is_some(),
            self.stage.0,
            self.stage.1,
            theme::BACKDROP_ALPHA,
        )
        .map(Message::Browser)
    }

    fn title(&self, _id: window::Id) -> String {
        String::new()
    }

    fn theme(&self, _id: window::Id) -> Theme {
        theme::app_theme()
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subs = vec![
            window::close_events().map(Message::Closed),
            window::resize_events().map(|(id, size)| Message::Sized(id, size)),
            iced::time::every(std::time::Duration::from_millis(16)).map(|_| Message::Tick),
            Subscription::run_with(self.host_link.clone(), host_stream),
            iced::event::listen_with(|event, _status, _id| match event {
                iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                    key,
                    physical_key,
                    location,
                    modifiers,
                    repeat,
                    ..
                }) => Some(Message::Key(RawKey {
                    key,
                    physical: physical_key,
                    location,
                    modifiers,
                    pressed: true,
                    repeat,
                })),
                iced::Event::Keyboard(iced::keyboard::Event::KeyReleased {
                    key,
                    physical_key,
                    location,
                    modifiers,
                    ..
                }) => Some(Message::Key(RawKey {
                    key,
                    physical: physical_key,
                    location,
                    modifiers,
                    pressed: false,
                    repeat: false,
                })),
                iced::Event::InputMethod(event) => Some(Message::Ime(event)),
                iced::Event::Touch(_) => Some(Message::Touch),
                _ => None,
            }),
        ];
        if let Some(engine) = &self.engine {
            subs.push(Subscription::run_with(
                EngineLink {
                    id: self.engine_link.id,
                    engine: Some(engine.clone()),
                },
                engine_stream,
            ));
        }
        Subscription::batch(subs)
    }

    fn style(&self, theme: &Theme) -> iced::theme::Style {
        let palette = theme.palette();
        iced::theme::Style {
            background_color: if crate::ui::chrome::transparent_windows() {
                iced::Color::TRANSPARENT
            } else {
                palette.background
            },
            text_color: palette.text,
        }
    }

    fn on_host_message(&mut self, message: HostMessage) -> Task<Message> {
        match message {
            HostMessage::Hello(_) | HostMessage::Resize { .. } => Task::none(),
            HostMessage::Input(edge) => self.on_input(edge),
            HostMessage::Action(invoked) => self.on_action(invoked),
            HostMessage::Text(action) => {
                self.on_text(action);
                Task::none()
            }
            HostMessage::Lifecycle(command) => self.on_lifecycle(command),
            HostMessage::OskState { open } => {
                self.osk_open = open;
                Task::none()
            }
        }
    }

    fn on_input(&mut self, edge: crate::hosted::InputEdge) -> Task<Message> {
        let now = Instant::now();
        if let Some(reading) = edge.readings.first() {
            let sample = to_pad_sample(&reading.sample);
            let (dx, dy) = self.panel.advance_stick(
                sample.lstick_x,
                sample.lstick_y,
                sample.rstick_x,
                sample.rstick_y,
                sample.r2,
                now,
            );
            self.panel.last_pad_stick = (sample.lstick_x, sample.lstick_y);
            if (dx != 0.0 || dy != 0.0)
                && let Some(engine) = &self.engine
            {
                let (x, y) = (self.panel.cursor.x, self.panel.cursor.y);
                engine.send(EngineCommand::ScrollBy {
                    tab: self.panel.active_tab,
                    dx,
                    dy,
                    x,
                    y,
                });
            }
            let masked = crate::ui::launcher::input::dpad_only(&sample);
            if let Some(action) = self.dpad.update(&masked, now, true, true) {
                let dir = match action {
                    NavAction::Up => Some(crate::browser::cursor::JumpDir::Up),
                    NavAction::Down => Some(crate::browser::cursor::JumpDir::Down),
                    NavAction::Left => Some(crate::browser::cursor::JumpDir::Left),
                    NavAction::Right => Some(crate::browser::cursor::JumpDir::Right),
                    _ => None,
                };
                if let Some(dir) = dir {
                    self.panel.jump_cursor(dir);
                }
            }
        }
        for action in edge.actions {
            if action.action != InputActionKind::Tap {
                continue;
            }
            match action.button {
                InputButton::Cross => self.click_cursor(),
                InputButton::L3 => {
                    let cursor = &mut self.panel.cursor;
                    cursor.magnet = !cursor.magnet;
                    if !cursor.magnet {
                        cursor.attached = None;
                        cursor.no_dock = None;
                    }
                }
                InputButton::L1 => {
                    let _ = self.browser_go_back();
                }
                InputButton::R1 => {
                    let _ = self.browser_go_forward();
                }
                InputButton::Options => {
                    let _ = self.browser_reload();
                }
                _ => {}
            }
        }
        self.sync_mouse();
        Task::none()
    }

    fn click_cursor(&mut self) {
        let targets = &self.panel.focusables;
        let (x, y) = self.panel.cursor.click_point(targets);
        let attached = self.panel.cursor.attached.and_then(|id| {
            self.panel
                .focusables
                .iter()
                .find(|item| item.id == id)
                .cloned()
        });
        let mut target = None;
        let mut trusted = false;
        let mut text_field = false;
        if let Some(item) = &attached {
            match item.kind {
                crate::browser::cursor::FocusKind::Link => {
                    target = Some(item.id);
                    trusted = true;
                }
                crate::browser::cursor::FocusKind::Field => text_field = true,
                _ => {}
            }
        }
        if let Some(engine) = &self.engine {
            engine.send(EngineCommand::ClickAt {
                tab: self.panel.active_tab,
                x,
                y,
                target,
                trusted,
            });
        }
        self.panel.last_click = Some(((x, y), Instant::now()));
        if text_field {
            self.send(BrowserEvent::RequestTextInput);
        }
    }

    fn on_action(&mut self, invoked: ActionInvoked) -> Task<Message> {
        let spec = crate::app::actions::browser_spec(&self.panel);
        if !spec.action_enabled(&invoked.action) {
            return Task::none();
        }
        match invoked.action.as_str() {
            crate::app::actions::ACTION_BACK => {
                let _ = self.browser_go_back();
            }
            crate::app::actions::ACTION_FORWARD => {
                let _ = self.browser_go_forward();
            }
            crate::app::actions::ACTION_RELOAD => {
                let _ = self.browser_reload();
            }
            _ => {}
        }
        Task::none()
    }

    fn browser_go_back(&mut self) -> bool {
        if !self.panel.can_go_back() {
            return false;
        }
        if let Some(engine) = &self.engine {
            engine.send(EngineCommand::GoBack {
                tab: self.panel.active_tab,
            });
        }
        true
    }

    fn browser_go_forward(&mut self) -> bool {
        if !self.panel.can_go_forward() {
            return false;
        }
        if let Some(engine) = &self.engine {
            engine.send(EngineCommand::GoForward {
                tab: self.panel.active_tab,
            });
        }
        true
    }

    fn browser_reload(&mut self) -> bool {
        let url = self.panel.session.url().to_string();
        if url.is_empty() {
            return false;
        }
        let generation = self.panel.session.begin_reload();
        if let Some(engine) = &self.engine {
            engine.send(EngineCommand::Reload {
                tab: self.panel.active_tab,
                generation,
                ignore_cache: false,
            });
        }
        true
    }

    fn on_text(&mut self, action: TextAction) {
        match action {
            TextAction::Insert(text) => self.send_text(&text),
            TextAction::Backspace => self.send_named("Backspace"),
            TextAction::Enter => self.send_named("Enter"),
            TextAction::Cancel => {}
        }
    }

    fn send_text(&mut self, text: &str) {
        for raw in synthetic_text(text, iced::keyboard::Modifiers::empty()) {
            self.send_key(raw);
        }
    }

    fn send_named(&mut self, name: &str) {
        if let Some(events) = synthetic_named(name, iced::keyboard::Modifiers::empty()) {
            for raw in events {
                self.send_key(raw);
            }
        }
    }

    fn send_key(&self, raw: RawKey) {
        if let Some(engine) = &self.engine {
            engine.send(EngineCommand::Key(TabKey {
                tab: self.panel.active_tab,
                event: map_event(&raw),
            }));
        }
    }

    fn on_key(&mut self, raw: RawKey) -> Task<Message> {
        let escape = matches!(
            raw.key,
            iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape)
        );
        if self.osk_open {
            self.send(BrowserEvent::RealInput(RealInput::Key { escape }));
            // The system OSK consumes Escape on hand-off; other keys still
            // reach the page after the OSK closes.
            if escape {
                return Task::none();
            }
        }
        self.send_key(raw);
        Task::none()
    }

    fn on_ime(&mut self, event: iced::advanced::input_method::Event) -> Task<Message> {
        use iced::advanced::input_method::Event;
        if self.osk_open {
            self.send(BrowserEvent::RealInput(RealInput::Ime));
        }
        let tab = self.panel.active_tab;
        let Some(engine) = &self.engine else {
            return Task::none();
        };
        match event {
            Event::Opened => {}
            Event::Closed => engine.send(EngineCommand::ImeCancelComposition { tab }),
            Event::Preedit(text, _) => {
                if text.is_empty() {
                    engine.send(EngineCommand::ImeCancelComposition { tab });
                } else {
                    engine.send(EngineCommand::ImeSetComposition { tab, text });
                }
            }
            Event::Commit(text) => {
                if text.is_empty() {
                    engine.send(EngineCommand::ImeFinishComposition { tab });
                } else {
                    engine.send(EngineCommand::ImeSetComposition {
                        tab,
                        text: text.clone(),
                    });
                    engine.send(EngineCommand::ImeFinishComposition { tab });
                }
            }
        }
        Task::none()
    }

    fn on_browser_message(&mut self, message: BrowserMessage) -> Task<Message> {
        match message {
            BrowserMessage::PagePointerMoved { x, y } => {
                self.note_mouse_input();
                let (x, y) = self.clamp_mouse(x, y);
                self.panel.page_mouse_active = true;
                if let Some(engine) = &self.engine {
                    engine.send(EngineCommand::MouseMove {
                        tab: self.panel.active_tab,
                        x,
                        y,
                        modifiers: self.panel.page_mouse_modifiers,
                    });
                }
            }
            BrowserMessage::PagePress => {
                self.note_mouse_input();
                self.panel.page_mouse_active = true;
                self.panel.page_mouse_down = true;
                let (x, y) = self.panel.page_mouse;
                if let Some(engine) = &self.engine {
                    engine.send(EngineCommand::MouseButton {
                        tab: self.panel.active_tab,
                        x,
                        y,
                        button: crate::browser::engine::BrowserMouseButton::Left,
                        pressed: true,
                        click_count: 1,
                        modifiers: self.panel.page_mouse_modifiers,
                    });
                }
            }
            BrowserMessage::PageRelease => {
                self.panel.page_mouse_down = false;
                let (x, y) = self.panel.page_mouse;
                if let Some(engine) = &self.engine {
                    engine.send(EngineCommand::MouseButton {
                        tab: self.panel.active_tab,
                        x,
                        y,
                        button: crate::browser::engine::BrowserMouseButton::Left,
                        pressed: false,
                        click_count: 1,
                        modifiers: self.panel.page_mouse_modifiers,
                    });
                }
            }
            BrowserMessage::PagePointerLeft => {
                if let Some(engine) = &self.engine {
                    engine.send(EngineCommand::MouseLeave {
                        tab: self.panel.active_tab,
                    });
                }
            }
            BrowserMessage::PageWheel { dx, dy } => {
                self.note_mouse_input();
                let (x, y) = self.panel.page_mouse;
                if let Some(engine) = &self.engine {
                    engine.send(EngineCommand::MouseWheel {
                        tab: self.panel.active_tab,
                        x,
                        y,
                        dx,
                        dy,
                    });
                }
            }
            BrowserMessage::Back => {
                let _ = self.browser_go_back();
            }
            BrowserMessage::Forward => {
                let _ = self.browser_go_forward();
            }
            BrowserMessage::Reload => {
                let _ = self.browser_reload();
            }
            BrowserMessage::Scrolled(y) => self.panel.note_scrolled(y),
            #[cfg(debug_assertions)]
            BrowserMessage::Shot(_) => {}
        }
        Task::none()
    }

    fn note_mouse_input(&mut self) {
        if self.osk_open {
            self.send(BrowserEvent::RealInput(RealInput::Mouse));
        }
    }

    fn clamp_mouse(&mut self, x: f32, y: f32) -> (f32, f32) {
        let (w, h) = self.panel.viewport;
        let x = x.clamp(0.0, (w - 1.0).max(0.0));
        let y = y.clamp(0.0, (h - 1.0).max(0.0));
        self.panel.page_mouse = (x, y);
        (x, y)
    }

    fn sync_mouse(&mut self) {
        let (qx, qy) = (
            self.panel.cursor.x as i32,
            self.panel.cursor.y.max(0.0) as i32,
        );
        if self.panel.last_sent_mouse != Some((qx, qy)) {
            self.panel.last_sent_mouse = Some((qx, qy));
            if let Some(engine) = &self.engine {
                engine.send(EngineCommand::MouseMove {
                    tab: self.panel.active_tab,
                    x: qx as f32,
                    y: qy as f32,
                    modifiers: keyboard_types::Modifiers::empty(),
                });
            }
        }
        if self.panel.dom_reseed_pending && self.panel.cursor.fly_to.is_none() {
            self.panel.dom_reseed_pending = false;
            if let Some(engine) = &self.engine {
                engine.send(EngineCommand::ReseedDom {
                    tab: self.panel.active_tab,
                });
            }
        }
    }

    fn on_engine_event(&mut self, event: EngineEvent) -> Task<Message> {
        match event {
            EngineEvent::FrameSlot { tab, slot } => {
                if tab == self.panel.active_tab {
                    self.panel.session.set_frame_slot(slot);
                }
            }
            EngineEvent::TabOpened { .. }
            | EngineEvent::TabClosed { .. }
            | EngineEvent::PopupRequested { .. }
            | EngineEvent::HistoryJumpFailed { .. } => {}
            EngineEvent::Frame(frame) => {
                self.panel
                    .session
                    .write_frame(frame.width, frame.height, frame.rgba);
            }
            EngineEvent::TitleChanged { title, .. } => {
                self.panel.session.note_title(title);
                self.panel.save_tabs();
            }
            EngineEvent::UrlChanged { url, .. } => {
                self.panel.session.note_url(url);
                self.panel.save_tabs();
            }
            EngineEvent::FaviconChanged { .. } => {}
            EngineEvent::A11yTree { items, .. } => {
                self.panel.focusables = items;
                let targets = self.panel.focusables.clone();
                self.panel.cursor.retarget(&targets);
                if self.panel.pending_focus.is_some() && self.panel.cursor.fly_to.is_none() {
                    let id = self.panel.pending_focus.take().unwrap_or_default();
                    self.dock_focused(id);
                }
            }
            EngineEvent::ScrollOffset { x, y, .. } => {
                self.panel.scroll_offset = (x, y);
            }
            EngineEvent::ScrollExtent { extent, .. } => {
                self.panel.page_extent = extent;
            }
            EngineEvent::ActivityChanged { .. } => {}
            EngineEvent::HistoryCapability {
                can_back,
                can_forward,
                ..
            } => {
                self.panel.page.last_engine_can = Some((can_back, can_forward));
                self.publish(false);
            }
            EngineEvent::HistoryChanged {
                entries, current, ..
            } => {
                self.panel.page.last_engine_history =
                    Some(crate::ui::browser::state::EngineHistorySnapshot {
                        current,
                        entries: entries.clone(),
                    });
                self.panel.session.apply_engine_history(&entries, current);
            }
            EngineEvent::CursorChanged { cursor, .. } => {
                self.panel.page_cursor = cursor;
            }
            EngineEvent::FocusChanged { id, .. } => {
                if self.panel.cursor.magnet && !self.dock_focused(id) {
                    self.panel.pending_focus = Some(id);
                } else {
                    self.panel.pending_focus = None;
                }
            }
            EngineEvent::EditableFocusChanged { editable, .. } => {
                self.panel.page.text_session_active = editable;
                self.publish(false);
            }
            EngineEvent::LoadFinished {
                generation, result, ..
            } => match result {
                Ok(()) => {
                    self.panel.session.mark_loaded(generation);
                }
                Err(err) => {
                    self.panel.session.apply_failed(generation, err);
                }
            },
        }
        Task::none()
    }

    fn dock_focused(&mut self, id: u64) -> bool {
        let panel = &mut self.panel;
        if !panel.cursor.magnet || panel.cursor.fly_to.is_some() {
            return false;
        }
        let Some(item) = panel.focusables.iter().find(|item| item.id == id).cloned() else {
            return false;
        };
        let (view_w, view_h) = panel.viewport;
        let cursor = &mut panel.cursor;
        cursor.attached = Some(id);
        cursor.dock_blend = 1.0;
        cursor.vx = 0.0;
        cursor.vy = 0.0;
        cursor.x = (item.x + item.w / 2.0).clamp(0.0, view_w);
        cursor.y = (item.y + item.h / 2.0).clamp(0.0, view_h);
        cursor.last_dock = Some((item.x, item.y, item.w, item.h));
        true
    }

    fn on_lifecycle(&mut self, command: LifecycleCommand) -> Task<Message> {
        match command {
            LifecycleCommand::Focus => {
                if let Some(id) = self.window {
                    return crate::ui::layout::focus_window(id);
                }
            }
            LifecycleCommand::Pause => {
                if let Some(engine) = &self.engine {
                    engine.send(EngineCommand::SetVisible { visible: false });
                    engine.send(EngineCommand::Suspend);
                }
                self.publish_state(LifecycleState::Paused);
            }
            LifecycleCommand::Resume => {
                if let Some(engine) = &self.engine {
                    engine.send(EngineCommand::Resume);
                    engine.send(EngineCommand::SetVisible { visible: true });
                }
                self.publish_state(LifecycleState::Running);
            }
            LifecycleCommand::Close | LifecycleCommand::Shutdown => {
                self.publish_state(LifecycleState::Closing);
                self.shutdown_engine();
                self.panel.save_tabs();
                return iced::exit();
            }
        }
        Task::none()
    }

    fn publish(&mut self, ready: bool) {
        let spec = crate::app::actions::browser_spec(&self.panel);
        if ready && !self.ready_sent {
            self.ready_sent = true;
            self.send(BrowserEvent::Ready(spec));
        } else if self.ready_sent {
            self.send(BrowserEvent::State {
                actions: spec.actions,
                text_session: spec.text_session,
            });
        }
    }

    fn publish_state(&mut self, state: LifecycleState) {
        self.send(BrowserEvent::Lifecycle(state));
    }

    fn shutdown_engine(&mut self) {
        let Some(engine) = &self.engine else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        engine.send(EngineCommand::Shutdown { done: tx });
        let _ = rx.recv_timeout(std::time::Duration::from_secs(5));
    }

    fn send(&self, event: BrowserEvent) {
        if let Ok(mut writer) = self.host.lock() {
            let _ = write_frame(&mut *writer, &event);
            let _ = writer.flush();
        }
    }
}

fn to_pad_sample(snapshot: &PadSnapshot) -> PadSample {
    PadSample {
        stick_x: snapshot.stick_x,
        stick_y: snapshot.stick_y,
        lstick_x: snapshot.lstick_x,
        lstick_y: snapshot.lstick_y,
        rstick_x: snapshot.rstick_x,
        rstick_y: snapshot.rstick_y,
        dpad_up: snapshot.dpad_up,
        dpad_down: snapshot.dpad_down,
        dpad_left: snapshot.dpad_left,
        dpad_right: snapshot.dpad_right,
        cross: snapshot.cross,
        circle: snapshot.circle,
        square: snapshot.square,
        triangle: snapshot.triangle,
        options: snapshot.options,
        create: snapshot.create,
        l1: snapshot.l1,
        r1: snapshot.r1,
        l2: snapshot.l2,
        r2: snapshot.r2,
        l3: snapshot.l3,
        ..PadSample::default()
    }
}

#[derive(Clone)]
struct HostLink {
    id: u64,
    rx: Arc<Mutex<Option<iced::futures::channel::mpsc::UnboundedReceiver<HostMessage>>>>,
}

impl std::hash::Hash for HostLink {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

fn host_stream(link: &HostLink) -> impl iced::futures::Stream<Item = Message> + Send + use<> {
    use iced::futures::StreamExt;
    let rx = link.rx.lock().ok().and_then(|mut guard| guard.take());
    iced::futures::stream::unfold(rx, |rx| async move {
        let mut rx = rx?;
        rx.next()
            .await
            .map(|message| (Message::Host(message), Some(rx)))
    })
}

#[derive(Clone)]
struct EngineLink {
    id: u64,
    engine: Option<EngineHandle>,
}

impl std::hash::Hash for EngineLink {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

fn engine_stream(link: &EngineLink) -> impl iced::futures::Stream<Item = Message> + Send + use<> {
    use iced::futures::StreamExt;
    let engine = link.engine.clone();
    stream::channel(64, async move |mut output| {
        let Some(engine) = engine else {
            return;
        };
        let (frame_tx, mut frame_rx) =
            iced::futures::channel::mpsc::channel::<crate::browser::engine::TabFrame>(64);
        let (ctrl_tx, mut ctrl_rx) = iced::futures::channel::mpsc::unbounded::<EngineEvent>();
        engine.bind(frame_tx, ctrl_tx);
        loop {
            iced::futures::select! {
                control = ctrl_rx.next() => {
                    let Some(event) = control else { break };
                    if output.send(Message::Engine(event)).await.is_err() { break; }
                }
                frame = frame_rx.next() => {
                    let Some(first) = frame else { break };
                    let mut latest = first;
                    while let Ok(newer) = frame_rx.try_recv() { latest = newer; }
                    if output.send(Message::Engine(EngineEvent::Frame(latest))).await.is_err() { break; }
                }
            }
        }
        engine.clear();
    })
}
