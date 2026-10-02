//! HID + tray + session service process (no iced windows).
//!
//! Spawns `sdsc-shell` and talks to it over IPC. Owns DualSense HID and the tray.

use crate::controller::dualsense::lightbar;
use crate::controller::hid::poll::{
    BATTERY_INTERVAL, LIVENESS_INTERVAL, PRESENCE_INTERVAL_EMPTY, UNREAD_RETRY_INTERVAL,
};
use crate::controller::hid::worker::HidWorkerHandle;
use crate::controller::known::KnownControllers;
use crate::domain::color;
use crate::domain::pad::{self as start_input, GestureDetectorBank};
use crate::ipc::{
    PipeServer, SHELL_PIPE_ENV, ServiceMessage, ShellCommand, bound_port, shell_exe_path,
};
use crate::persist::analytics::AnalyticsStore;
use crate::persist::prefs::Prefs;
use crate::platform::app_log;
use crate::session::{ApplyContext, DeviceSession, SessionEffect};
use crate::ui::tray::{self, QUIT_ID, SETTINGS_ID};
use std::process::{Child, Command};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tray_icon::menu::MenuEvent;
use tray_icon::{MouseButton as TrayMouseButton, MouseButtonState, TrayIconEvent};

/// Run the service until Quit. Spawns and respawns the iced shell.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let prefs = Prefs::load();
    let known = KnownControllers::load();
    let analytics = AnalyticsStore::load();
    color::set_active_spectrum(prefs.spectrum.clone());
    lightbar::set_enabled(prefs.lightbar_enabled);

    let mut session = DeviceSession::new(prefs, known, analytics);
    let hid_worker = HidWorkerHandle::start();
    let pipe = PipeServer::start()?;
    let mut tray = tray::create_tray(&[]);
    let mut shell = spawn_shell()?;
    let mut last_discovered = hid_worker.presence_paths();
    let mut last_battery_poll = Instant::now();
    let mut last_shell_check = Instant::now();
    let mut last_presence_check = Instant::now();
    let mut last_persist_check = Instant::now();
    let mut start_visible = false;
    let mut shell_input_hot = false;
    let mut gesture_detectors = GestureDetectorBank::default();
    let mut reopen_needs_release = false;
    let mut pending_open_start = false;
    let mut last_client_count = 0usize;

    // Forward hid-worker pad edges over IPC (and run reopen gesture while shell is down).
    let (edge_tx, edge_rx) = mpsc::sync_channel::<start_input::InputEdge>(32);
    start_input::bind_service_edge_sender(edge_tx);
    let edge_pipe = pipe.handle();
    thread::Builder::new()
        .name("sdsc-edge-fwd".into())
        .spawn(move || {
            while let Ok(edge) = edge_rx.recv() {
                let _ = edge_pipe.send(ServiceMessage::PadInput(edge));
            }
        })
        .map_err(|e| e.to_string())?;

    app_log::info("service: started (HID + tray); shell spawned");

    loop {
        if last_shell_check.elapsed() >= Duration::from_secs(1) {
            last_shell_check = Instant::now();
            if let Some(status) = shell.try_wait()? {
                app_log::warn(format!(
                    "service: shell exited ({status}); respawning without reopening HID"
                ));
                // Chord may still be held across the restart — require release.
                reopen_needs_release = true;
                gesture_detectors.reset();
                last_client_count = 0;
                shell = spawn_shell()?;
            }
        }

        let clients = pipe.client_count();
        if clients > 0 && last_client_count == 0 && pending_open_start {
            pending_open_start = false;
            let _ = pipe.send(ServiceMessage::Effects(vec![SessionEffect::OpenStart]));
        }
        last_client_count = clients;

        if last_persist_check.elapsed() >= Duration::from_secs(2) {
            last_persist_check = Instant::now();
            reload_persist_if_needed(&mut session);
        }

        while let Some(cmd) = pipe.try_recv_command() {
            if matches!(
                handle_command(
                    cmd,
                    &mut session,
                    &hid_worker,
                    &pipe,
                    &mut start_visible,
                    &mut shell_input_hot,
                    &mut gesture_detectors,
                )?,
                LoopControl::Quit
            ) {
                let _ = pipe.send(ServiceMessage::Shutdown);
                let _ = shell.kill();
                start_input::clear_service_edge_sender();
                hid_worker.shutdown();
                return Ok(());
            }
        }

        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                rect,
                button: TrayMouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let anchor = crate::ui::layout::TrayAnchor {
                    x: rect.position.x as f32,
                    y: rect.position.y as f32,
                    width: rect.size.width as f32,
                    height: rect.size.height as f32,
                };
                let _ = pipe.send(ServiceMessage::TrayOpenPopup { anchor });
            }
        }
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            match event.id.0.as_str() {
                SETTINGS_ID => {
                    let _ = pipe.send(ServiceMessage::TrayOpenSettings);
                }
                QUIT_ID => {
                    let _ = pipe.send(ServiceMessage::Shutdown);
                    let _ = shell.kill();
                    start_input::clear_service_edge_sender();
                    hid_worker.shutdown();
                    return Ok(());
                }
                _ => {}
            }
        }

        let presence_due = last_presence_check.elapsed()
            >= if session.controllers.is_empty() {
                PRESENCE_INTERVAL_EMPTY
            } else {
                Duration::from_secs(3)
            };
        if presence_due {
            last_presence_check = Instant::now();
            let presence = hid_worker.presence_paths();
            let presence_changed = presence != last_discovered;
            if presence_changed {
                last_discovered = presence.clone();
            }

            if presence.is_empty() && !session.controllers.is_empty() {
                lightbar::sync_lightbar_claims(std::iter::empty::<&str>());
                session.clear_missed_polls();
                let effects = session.apply_controllers(
                    Vec::new(),
                    ApplyContext {
                        start_visible,
                        fullscreen: start_input::foreground_is_exclusive_fullscreen(),
                        now: Instant::now(),
                        lightbar_enabled: lightbar::is_enabled(),
                    },
                );
                dispatch_effects(&mut session, &hid_worker, &pipe, &mut tray, effects);
                last_battery_poll = Instant::now();
            } else {
                let battery_due = last_battery_poll.elapsed() >= BATTERY_INTERVAL;
                let liveness_due = last_battery_poll.elapsed() >= LIVENESS_INTERVAL
                    && !session.controllers.is_empty();
                let unread_due = last_battery_poll.elapsed() >= UNREAD_RETRY_INTERVAL
                    && !presence.is_empty()
                    && session.controllers.is_empty();
                if presence_changed || battery_due || liveness_due || unread_due {
                    let previously: Vec<String> = session
                        .controllers
                        .iter()
                        .map(|c| c.serial.clone())
                        .collect();
                    last_battery_poll = Instant::now();
                    match poll_sync(&hid_worker, previously) {
                        Ok(controllers) => {
                            let effects = session.on_poll_result(
                                controllers.clone(),
                                ApplyContext {
                                    start_visible,
                                    fullscreen: start_input::foreground_is_exclusive_fullscreen(),
                                    now: Instant::now(),
                                    lightbar_enabled: lightbar::is_enabled(),
                                },
                            );
                            let _ = pipe.send(ServiceMessage::Controllers(controllers));
                            dispatch_effects(&mut session, &hid_worker, &pipe, &mut tray, effects);
                        }
                        Err(err) => app_log::warn(format!("service poll failed: {err}")),
                    }
                }
            }
        }

        // Sample while start is enabled and pads are present, or the shell asked for hot input.
        let listening = shell_input_hot
            || (session.prefs.start_screen_enabled && !session.controllers.is_empty());
        hid_worker.set_input_hot(listening);

        // Reopen gesture in the service so a PS chord during shell restart still works.
        if listening
            && session.prefs.start_screen_enabled
            && !start_visible
            && pipe.client_count() == 0
        {
            if let start_input::NavReadingsOutcome::Readings { readings, .. } =
                start_input::read_nav_readings()
            {
                if reopen_needs_release {
                    let chord = &session.prefs.start_screen_gesture;
                    let still_held = readings
                        .iter()
                        .any(|r| chord.iter().all(|c| r.sample.held.contains(c)));
                    if !still_held {
                        reopen_needs_release = false;
                        gesture_detectors.reset();
                    }
                } else if !session.prefs.start_screen_gesture.is_empty()
                    && gesture_detectors.update(&session.prefs.start_screen_gesture, &readings)
                {
                    reopen_needs_release = true;
                    pending_open_start = true;
                }
            }
        }

        // Pump Windows messages so tray-icon delivers click/menu events.
        #[cfg(windows)]
        pump_win_messages();

        thread::sleep(Duration::from_millis(16));
    }
}

enum LoopControl {
    Continue,
    Quit,
}

fn handle_command(
    cmd: ShellCommand,
    session: &mut DeviceSession,
    hid_worker: &HidWorkerHandle,
    pipe: &PipeServer,
    start_visible: &mut bool,
    shell_input_hot: &mut bool,
    gesture_detectors: &mut GestureDetectorBank,
) -> Result<LoopControl, Box<dyn std::error::Error>> {
    match cmd {
        ShellCommand::Quit => Ok(LoopControl::Quit),
        ShellCommand::Ping => {
            let _ = pipe.send(ServiceMessage::Ack);
            Ok(LoopControl::Continue)
        }
        ShellCommand::OpenPopup { anchor } => {
            let _ = pipe.send(ServiceMessage::TrayOpenPopup { anchor });
            Ok(LoopControl::Continue)
        }
        ShellCommand::OpenSettings => {
            let _ = pipe.send(ServiceMessage::TrayOpenSettings);
            Ok(LoopControl::Continue)
        }
        ShellCommand::Identify { serial, percent } => {
            let _ = hid_worker.identify(serial, percent);
            Ok(LoopControl::Continue)
        }
        ShellCommand::PowerOff { serial } => {
            session.mark_skip_connect_cooldown();
            hid_worker.power_off(serial);
            Ok(LoopControl::Continue)
        }
        ShellCommand::SetRgb { serial, color } => {
            hid_worker.set_rgb(serial, color);
            Ok(LoopControl::Continue)
        }
        ShellCommand::SetLowBatteryTargets { targets } => {
            hid_worker.set_low_battery_targets(targets);
            Ok(LoopControl::Continue)
        }
        ShellCommand::SetInputHot { hot } => {
            *shell_input_hot = hot;
            Ok(LoopControl::Continue)
        }
        ShellCommand::ReportStartVisible { visible } => {
            if *start_visible && !visible {
                // Start closed — require chord release before reopen.
                gesture_detectors.reset();
            }
            *start_visible = visible;
            Ok(LoopControl::Continue)
        }
        ShellCommand::Rumble {
            serial,
            right,
            left,
            duration_ms,
        } => {
            hid_worker.rumble(serial, right, left, duration_ms);
            Ok(LoopControl::Continue)
        }
        ShellCommand::RumbleStopAll => {
            hid_worker.rumble_stop_all();
            Ok(LoopControl::Continue)
        }
        ShellCommand::ListControllers => {
            let list = session.controllers.clone();
            let _ = pipe.send(ServiceMessage::ControllerList(Ok(list)));
            Ok(LoopControl::Continue)
        }
        ShellCommand::SetLightbarAll { color } => {
            match lightbar::apply_lightbar_all(color) {
                Ok(n) => {
                    let _ = pipe.send(ServiceMessage::LightbarSet { count: n });
                }
                Err(err) => app_log::warn(format!("set-lightbar: {err}")),
            }
            Ok(LoopControl::Continue)
        }
        ShellCommand::ReloadPersist => {
            reload_persist(session);
            Ok(LoopControl::Continue)
        }
    }
}

fn dispatch_effects(
    session: &mut DeviceSession,
    hid_worker: &HidWorkerHandle,
    pipe: &PipeServer,
    tray: &mut Option<tray_icon::TrayIcon>,
    effects: Vec<SessionEffect>,
) {
    for effect in &effects {
        match effect {
            SessionEffect::SetTray { .. } => {
                if let Some(icon) = tray.as_mut() {
                    tray::apply_tray(icon, &session.controllers);
                } else {
                    *tray = tray::create_tray(&session.controllers);
                }
            }
            SessionEffect::SetLowBatteryTargets { targets } => {
                hid_worker.set_low_battery_targets(targets.clone());
            }
            SessionEffect::SaveKnown => session.known.save(),
            SessionEffect::SaveAnalytics => session.analytics.save(),
            SessionEffect::QueueNotifications {
                events,
                open_start_after_toast,
            } => {
                let _ = pipe.send(ServiceMessage::Notifications {
                    events: events.clone(),
                    open_start_after_toast: *open_start_after_toast,
                });
            }
            SessionEffect::OpenStart
            | SessionEffect::CloseStart
            | SessionEffect::ClearStartLatch
            | SessionEffect::RefreshAnalyticsPanel
            | SessionEffect::SyncPopup => {}
        }
    }
    // Shell applies UI-facing effects; skip persistence/HID that the service already did.
    let ui_effects: Vec<SessionEffect> = effects
        .into_iter()
        .filter(|e| {
            !matches!(
                e,
                SessionEffect::SetTray { .. }
                    | SessionEffect::SetLowBatteryTargets { .. }
                    | SessionEffect::SaveKnown
                    | SessionEffect::SaveAnalytics
            )
        })
        .collect();
    if !ui_effects.is_empty() {
        let _ = pipe.send(ServiceMessage::Effects(ui_effects));
    }
}

fn spawn_shell() -> Result<Child, Box<dyn std::error::Error>> {
    let exe = shell_exe_path();
    let port = bound_port();
    let mut cmd = Command::new(&exe);
    // Pass the real port so the shell attaches as a client (not port "1").
    cmd.env(SHELL_PIPE_ENV, port.to_string());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    Ok(cmd
        .spawn()
        .map_err(|e| format!("failed to spawn shell at {}: {e}", exe.display()))?)
}

fn poll_sync(
    worker: &HidWorkerHandle,
    previously: Vec<String>,
) -> Result<Vec<crate::controller::model::ControllerStatus>, String> {
    pollster_block_on(worker.poll(previously))
}

fn pollster_block_on<F: std::future::Future>(fut: F) -> F::Output {
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    fn noop(_: *const ()) {}
    fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(std::ptr::null(), &VTABLE)
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
    let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
    let mut fut = std::pin::pin!(fut);
    let mut cx = Context::from_waker(&waker);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => thread::sleep(Duration::from_millis(1)),
        }
    }
}

fn reload_persist_if_needed(session: &mut DeviceSession) {
    // Cheap: always reload prefs/known so Settings edits in the shell take effect.
    reload_persist(session);
}

fn reload_persist(session: &mut DeviceSession) {
    let prefs = Prefs::load();
    color::set_active_spectrum(prefs.spectrum.clone());
    lightbar::set_enabled(prefs.lightbar_enabled);
    session.prefs = prefs;
    session.known = KnownControllers::load();
}

#[cfg(windows)]
fn pump_win_messages() {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
    };
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(not(windows))]
fn pump_win_messages() {}
