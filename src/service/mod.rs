//! HID + tray + session service process (no iced windows).
//!
//! Spawns `sdsc-shell` and talks to it over IPC. Owns DualSense HID and the tray.

use crate::controller::dualsense::identity as dualsense;
use crate::controller::dualsense::lightbar;
use crate::controller::hid::poll::{
    BATTERY_INTERVAL, LIVENESS_INTERVAL, PRESENCE_INTERVAL_EMPTY, UNREAD_RETRY_INTERVAL,
};
use crate::controller::hid::worker::HidWorkerHandle;
use crate::controller::known::KnownControllers;
use crate::controller::model::ControllerStatus;
use crate::domain::color::{self, color_for_battery_percent};
use crate::domain::gesture::{self, ChordReleaseGate, ChordReleaseTick};
use crate::domain::pad::{self as start_input, GestureDetectorBank};
use crate::ipc::{PipeServer, SHELL_PIPE_ENV, ServiceMessage, ShellCommand, bound_port};
use crate::persist::analytics::AnalyticsStore;
use crate::persist::prefs::Prefs;
use crate::platform::app_log;
use crate::session::{
    ApplyContext, DeviceSession, SessionEffect, controllers_equivalent,
    should_skip_connect_cooldown_on_power_off,
};
use crate::ui::tray::{self, QUIT_ID, SETTINGS_ID};
use std::collections::HashMap;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use tray_icon::menu::MenuEvent;
use tray_icon::{MouseButton as TrayMouseButton, MouseButtonState, TrayIconEvent};

enum TrayServiceEvent {
    OpenPopup {
        anchor: crate::ui::layout::TrayAnchor,
    },
    OpenSettings,
    Quit,
}

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
    // Pads already present at launch must not wait for BATTERY/UNREAD cadence —
    // schedule an immediate first poll so auto-open can fire as soon as the shell is up.
    let mut last_battery_poll = Instant::now()
        .checked_sub(BATTERY_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut last_shell_check = Instant::now();
    let mut last_presence_check = Instant::now()
        .checked_sub(PRESENCE_INTERVAL_EMPTY)
        .unwrap_or_else(Instant::now);
    let mut last_persist_check = Instant::now();
    let mut start_visible = false;
    let shell_input_hot = Arc::new(AtomicBool::new(false));
    let mut gesture_detectors = GestureDetectorBank::default();
    let mut chord_release_gate = ChordReleaseGate::default();
    let mut pending_open_start = false;
    let mut last_client_count = 0usize;

    // Forward hid-worker pad edges over IPC (and run reopen gesture while shell is down).
    let (edge_tx, edge_rx) = mpsc::sync_channel::<start_input::InputEdge>(32);
    start_input::bind_service_edge_sender(edge_tx);
    let edge_pipe = pipe.handle();
    let forward_pad_edges = Arc::clone(&shell_input_hot);
    thread::Builder::new()
        .name("sdsc-edge-fwd".into())
        .spawn(move || {
            while let Ok(edge) = edge_rx.recv() {
                if !forward_pad_edges.load(Ordering::Relaxed) {
                    continue;
                }
                let _ = edge_pipe.send(ServiceMessage::PadInput(edge));
            }
        })
        .map_err(|e| e.to_string())?;

    // tray-icon / muda deliver via OnceLock handlers; mirror the iced bridge so
    // clicks are not lost when we only poll `receiver()` between long HID polls.
    let (tray_tx, tray_rx) = mpsc::sync_channel::<TrayServiceEvent>(16);
    {
        let tx = tray_tx.clone();
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
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
                let _ = tx.try_send(TrayServiceEvent::OpenPopup { anchor });
            }
        }));
        let tx = tray_tx;
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| match event.id.0.as_str() {
            SETTINGS_ID => {
                let _ = tx.try_send(TrayServiceEvent::OpenSettings);
            }
            QUIT_ID => {
                let _ = tx.try_send(TrayServiceEvent::Quit);
            }
            _ => {}
        }));
    }

    app_log::info("service: started (HID + tray); shell spawned");

    loop {
        // Pump before draining tray events so Win32 delivers click/menu callbacks.
        #[cfg(windows)]
        pump_win_messages();

        if last_shell_check.elapsed() >= Duration::from_secs(1) {
            last_shell_check = Instant::now();
            if let Some(status) = shell.try_wait()? {
                app_log::warn(format!(
                    "service: shell exited ({status}); respawning without reopening HID"
                ));
                // Chord may still be held across the restart — require release.
                chord_release_gate.arm();
                gesture_detectors.reset();
                last_client_count = 0;
                shell = spawn_shell()?;
            }
        }

        let clients = pipe.client_count();
        if clients > 0 && last_client_count == 0 {
            // Shell just attached — push live pad list (earlier Controllers IPC may have
            // been dropped while client_count was 0) then flush a latched OpenStart.
            // Keep the latch until ReportStartVisible(true): a brief connect (or a
            // message sent before the shell's recv loop is live) must not clear it.
            let _ = pipe.send(ServiceMessage::Controllers(session.controllers.clone()));
            if pending_open_start {
                app_log::info("service: flushing latched OpenStart (shell up)");
                let _ = pipe.send(ServiceMessage::Effects(vec![SessionEffect::OpenStart]));
            }
        }
        last_client_count = clients;

        if last_persist_check.elapsed() >= Duration::from_secs(2) {
            last_persist_check = Instant::now();
            if let Some(effects) = reload_persist_if_needed(&mut session, start_visible) {
                dispatch_effects(
                    &mut session,
                    &hid_worker,
                    &pipe,
                    &mut tray,
                    effects,
                    &mut pending_open_start,
                );
            }
        }

        while let Some(cmd) = pipe.try_recv_command() {
            if matches!(
                &cmd,
                ShellCommand::ReportStartVisible { visible: false } if start_visible
            ) {
                // Start closed — require debounced chord release before reopen.
                chord_release_gate.arm();
            }
            if matches!(&cmd, ShellCommand::ReportStartVisible { visible: true }) {
                // Start HWND is up — drop any auto-open latch.
                pending_open_start = false;
            }
            if matches!(
                handle_command(
                    cmd,
                    &mut session,
                    &hid_worker,
                    &pipe,
                    &mut start_visible,
                    &shell_input_hot,
                    &mut gesture_detectors,
                )?,
                LoopControl::Quit
            ) {
                let _ = pipe.send(ServiceMessage::Shutdown);
                stop_shell_graceful(&mut shell);
                start_input::clear_service_edge_sender();
                hid_worker.shutdown();
                return Ok(());
            }
        }

        while let Ok(event) = tray_rx.try_recv() {
            match event {
                TrayServiceEvent::OpenPopup { anchor } => {
                    if pipe.client_count() == 0 {
                        app_log::warn("service: tray popup ignored (shell not connected)");
                    } else {
                        app_log::info("service: tray open popup → shell");
                        let _ = pipe.send(ServiceMessage::TrayOpenPopup { anchor });
                    }
                }
                TrayServiceEvent::OpenSettings => {
                    if pipe.client_count() == 0 {
                        app_log::warn("service: tray settings ignored (shell not connected)");
                    } else {
                        app_log::info("service: tray open settings → shell");
                        let _ = pipe.send(ServiceMessage::TrayOpenSettings);
                    }
                }
                TrayServiceEvent::Quit => {
                    let _ = pipe.send(ServiceMessage::Shutdown);
                    stop_shell_graceful(&mut shell);
                    start_input::clear_service_edge_sender();
                    hid_worker.shutdown();
                    return Ok(());
                }
            }
        }

        let nav_priority = start_visible || shell_input_hot.load(Ordering::Relaxed);
        // Elevate sampling before the hot Controllers path so live battery is fresh.
        let listening = shell_input_hot.load(Ordering::Relaxed)
            || start_visible
            || (session.prefs.start_screen_enabled && !session.controllers.is_empty());
        hid_worker.set_input_hot(listening);

        // While Start owns input, check presence/membership more often so a BT
        // disconnect is not deferred until the window closes (liveness Poll).
        let presence_due = last_presence_check.elapsed()
            >= if session.controllers.is_empty() || nav_priority {
                PRESENCE_INTERVAL_EMPTY
            } else {
                Duration::from_secs(3)
            };
        let mut presence_changed = false;
        if presence_due {
            last_presence_check = Instant::now();
            let presence = hid_worker.presence_paths();
            presence_changed = presence != last_discovered;
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
                // Shell refreshes compact Controllers from Controllers IPC only.
                let _ = pipe.send(ServiceMessage::Controllers(session.controllers.clone()));
                dispatch_effects(
                    &mut session,
                    &hid_worker,
                    &pipe,
                    &mut tray,
                    effects,
                    &mut pending_open_start,
                );
                last_battery_poll = Instant::now();
            } else if !nav_priority {
                // Cold path (tray idle): classic exclusive Poll for battery/liveness.
                let battery_due = last_battery_poll.elapsed() >= BATTERY_INTERVAL;
                let liveness_due = last_battery_poll.elapsed() >= LIVENESS_INTERVAL
                    && !session.controllers.is_empty();
                let unread_due = last_battery_poll.elapsed() >= UNREAD_RETRY_INTERVAL
                    && !presence.is_empty()
                    && session.controllers.is_empty();
                let schedule_due = presence_changed || battery_due || liveness_due || unread_due;
                if schedule_due {
                    let previously: Vec<String> = session
                        .controllers
                        .iter()
                        .map(|c| c.serial.clone())
                        .collect();
                    last_battery_poll = Instant::now();
                    match poll_sync(&hid_worker, previously) {
                        Ok(controllers) => {
                            session
                                .notify
                                .queue_launch_quiet(hid_worker.take_launch_serials());
                            let effects = session.on_poll_result(
                                controllers,
                                ApplyContext {
                                    start_visible,
                                    fullscreen: start_input::foreground_is_exclusive_fullscreen(),
                                    now: Instant::now(),
                                    lightbar_enabled: lightbar::is_enabled(),
                                },
                            );
                            let _ =
                                pipe.send(ServiceMessage::Controllers(session.controllers.clone()));
                            dispatch_effects(
                                &mut session,
                                &hid_worker,
                                &pipe,
                                &mut tray,
                                effects,
                                &mut pending_open_start,
                            );
                        }
                        Err(err) => app_log::warn(format!("service poll failed: {err}")),
                    }
                }
            }
        }

        // Hot path: every service loop (~16ms) so silence drops update Start quickly.
        // Skip when HID presence is empty — stale live_pads must not re-add pads.
        if nav_priority && !last_discovered.is_empty() {
            let live = hid_worker.live_controllers();
            if hot_path_wait_for_first_sample(
                live.is_empty(),
                !last_discovered.is_empty(),
                session.controllers.is_empty(),
            ) {
                if presence_changed {
                    crate::controller::hid::diag::diag_info(
                        "service: hot path waiting for live battery sample",
                    );
                }
            } else if !controllers_equivalent(&session.controllers, &live) || presence_changed {
                apply_hot_path_lightbar(&hid_worker, &session.controllers, &live);
                last_battery_poll = Instant::now();
                session
                    .notify
                    .queue_launch_quiet(hid_worker.take_launch_serials());
                let effects = session.on_poll_result(
                    live,
                    ApplyContext {
                        start_visible,
                        fullscreen: start_input::foreground_is_exclusive_fullscreen(),
                        now: Instant::now(),
                        lightbar_enabled: lightbar::is_enabled(),
                    },
                );
                let _ = pipe.send(ServiceMessage::Controllers(session.controllers.clone()));
                dispatch_effects(
                    &mut session,
                    &hid_worker,
                    &pipe,
                    &mut tray,
                    effects,
                    &mut pending_open_start,
                );
            }
        }

        // Reopen gesture lives in the service so the shell can stay idle until a match.
        // Also covers a PS chord held across a shell restart (client_count == 0).
        if listening
            && session.prefs.start_screen_enabled
            && !start_visible
            && let start_input::NavReadingsOutcome::Readings { readings, .. } =
                start_input::read_nav_readings()
        {
            let chord = gesture::default_gesture();
            let chord_held = readings
                .iter()
                .any(|r| chord.iter().all(|c| r.sample.held.contains(c)));
            let now = Instant::now();
            let (tick, glitch) = chord_release_gate.tick(chord_held, now);
            if glitch {
                crate::controller::hid::diag::diag_info("ui-diag: reopen chord glitch ignored");
            }
            let rising = match tick {
                ChordReleaseTick::Blocked => {
                    if chord_held {
                        gesture_detectors.consume_pending_match(&readings);
                    }
                    false
                }
                ChordReleaseTick::Cleared => {
                    // Absent sample disarms detectors; next held rising edge may fire.
                    let _ = gesture_detectors.update(&chord, &readings);
                    false
                }
                ChordReleaseTick::Open => gesture_detectors.update(&chord, &readings),
            };
            match reopen_gesture_outcome(
                tick == ChordReleaseTick::Blocked,
                chord_held,
                rising,
                pipe.client_count() > 0,
            ) {
                ReopenGestureOutcome::Hold => {}
                ReopenGestureOutcome::Idle => {}
                ReopenGestureOutcome::OpenNow => {
                    chord_release_gate.arm();
                    pending_open_start = false;
                    app_log::info("service: reopen gesture → OpenStart");
                    let _ = pipe.send(ServiceMessage::Effects(vec![SessionEffect::OpenStart]));
                }
                ReopenGestureOutcome::Latch => {
                    chord_release_gate.arm();
                    pending_open_start = true;
                    app_log::info("service: reopen gesture latched (shell down)");
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
    shell_input_hot: &Arc<AtomicBool>,
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
            // Only the last pad's intentional power-off should skip the 0→1 ghost
            // cooldown; powering off one of two must not reopen Start on the sibling.
            if should_skip_connect_cooldown_on_power_off(
                &session.controllers,
                &serial,
                session.prefs.start_screen_auto_open.includes_usb(),
            ) {
                session.mark_skip_connect_cooldown();
            }
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
            shell_input_hot.store(hot, Ordering::Relaxed);
            Ok(LoopControl::Continue)
        }
        ShellCommand::ReportStartVisible { visible } => {
            if *start_visible && !visible {
                // Detectors reset here; chord_release_gate is armed in the loop.
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
            if let Some(effects) = reload_persist(session, *start_visible) {
                let ui_effects: Vec<SessionEffect> = effects
                    .into_iter()
                    .filter(|e| {
                        matches!(
                            e,
                            SessionEffect::OpenStart
                                | SessionEffect::CloseStart
                                | SessionEffect::ClearStartLatch
                        )
                    })
                    .collect();
                if !ui_effects.is_empty() {
                    let _ = pipe.send(ServiceMessage::Effects(ui_effects));
                }
            }
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
    pending_open_start: &mut bool,
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
    // Shell applies UI-facing effects; skip work the service already did locally
    // or already pushed on a dedicated IPC message (Notifications).
    let mut ui_effects: Vec<SessionEffect> = effects
        .into_iter()
        .filter(|e| {
            !matches!(
                e,
                SessionEffect::SetTray { .. }
                    | SessionEffect::SetLowBatteryTargets { .. }
                    | SessionEffect::SaveKnown
                    | SessionEffect::SaveAnalytics
                    | SessionEffect::QueueNotifications { .. }
            )
        })
        .collect();
    if ui_effects.is_empty() {
        return;
    }
    // Early HID poll (pads already present) can race shell attach — OpenStart would
    // be dropped with client_count==0. Latch like the reopen-gesture path.
    if pipe.client_count() == 0 {
        if ui_effects
            .iter()
            .any(|e| matches!(e, SessionEffect::OpenStart))
        {
            *pending_open_start = true;
            crate::controller::hid::diag::diag_info(
                "ui-diag: service OpenStart latched (shell down)",
            );
        }
        ui_effects.retain(|e| !matches!(e, SessionEffect::OpenStart));
        if ui_effects.is_empty() {
            return;
        }
    }
    let _ = pipe.send(ServiceMessage::Effects(ui_effects));
}

/// Let the shell run `Message::Exit` (art wipe + join) before we force-kill.
fn stop_shell_graceful(shell: &mut Child) {
    const GRACE_MS: u64 = 2500;
    const STEP_MS: u64 = 25;
    let deadline = Instant::now() + Duration::from_millis(GRACE_MS);
    loop {
        match shell.try_wait() {
            Ok(Some(status)) => {
                app_log::info(format!("service: shell exited gracefully status={status}"));
                return;
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(STEP_MS));
            }
            Ok(None) => {
                app_log::warn("service: shell Exit timed out; killing");
                let _ = shell.kill();
                let _ = shell.wait();
                return;
            }
            Err(err) => {
                app_log::warn(format!("service: shell try_wait failed: {err}; killing"));
                let _ = shell.kill();
                let _ = shell.wait();
                return;
            }
        }
    }
}

fn spawn_shell() -> Result<Child, Box<dyn std::error::Error>> {
    let exe = crate::platform::shell_bundle::prepare_shell_exe()?;
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

fn reload_persist_if_needed(
    session: &mut DeviceSession,
    start_visible: bool,
) -> Option<Vec<SessionEffect>> {
    // Cheap: always reload prefs/known so Settings edits in the shell take effect.
    reload_persist(session, start_visible)
}

fn reload_persist(session: &mut DeviceSession, start_visible: bool) -> Option<Vec<SessionEffect>> {
    let prefs = Prefs::load();
    // Re-evaluate close only when the USB scope actually flips while auto-open
    // can fire; switching to Never never closes (matches the shell apply rule).
    let scope_changed = prefs.start_screen_auto_open.includes_usb()
        != session.prefs.start_screen_auto_open.includes_usb();
    let usb_changed = prefs.start_screen_auto_open.auto_opens() && scope_changed;
    color::set_active_spectrum(prefs.spectrum.clone());
    lightbar::set_enabled(prefs.lightbar_enabled);
    session.prefs = prefs;
    session.known = KnownControllers::load();
    if !usb_changed {
        return None;
    }
    Some(session.reevaluate_start_presence(ApplyContext {
        start_visible,
        fullscreen: start_input::foreground_is_exclusive_fullscreen(),
        now: Instant::now(),
        lightbar_enabled: lightbar::is_enabled(),
    }))
}

/// Apply connect / battery-color lightbar via exclusive SetRgb while input is hot.
///
/// Membership gains clear the claim (`prepare_connect_apply`); percent bucket
/// changes reassert RGB. Same open-write-close cost as Identify — not a full Poll.
fn apply_hot_path_lightbar(
    hid_worker: &HidWorkerHandle,
    previous: &[ControllerStatus],
    next: &[ControllerStatus],
) {
    lightbar::sync_lightbar_claims(
        next.iter()
            .filter(|c| c.supports_lightbar)
            .map(|c| c.serial.as_str()),
    );
    if !lightbar::is_enabled() {
        return;
    }
    let prev_by_serial: HashMap<&str, &ControllerStatus> =
        previous.iter().map(|c| (c.serial.as_str(), c)).collect();
    for pad in next {
        if !pad.supports_lightbar {
            continue;
        }
        let color = color_for_battery_percent(pad.percent);
        match prev_by_serial.get(pad.serial.as_str()) {
            None => {
                lightbar::prepare_connect_apply(&pad.serial);
                crate::controller::hid::diag::diag_info(format!(
                    "service: hot lightbar connect serial={}",
                    dualsense::normalize_identity(&pad.serial)
                ));
                hid_worker.set_rgb(pad.serial.clone(), color);
            }
            Some(old) if color_for_battery_percent(old.percent) != color => {
                crate::controller::hid::diag::diag_info(format!(
                    "service: hot lightbar color serial={} percent={}",
                    dualsense::normalize_identity(&pad.serial),
                    pad.percent
                ));
                hid_worker.set_rgb(pad.serial.clone(), color);
            }
            _ => {}
        }
    }
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

/// Keep the session while HID lists a pad but the hot sample stream has no
/// battery yet — only when we do not already show that pad as connected.
fn hot_path_wait_for_first_sample(
    live_empty: bool,
    presence_nonempty: bool,
    session_empty: bool,
) -> bool {
    live_empty && presence_nonempty && session_empty
}

/// What the service should do after one reopen-gesture sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReopenGestureOutcome {
    /// Chord still held after a prior fire or Start close — stay silent.
    Hold,
    /// Rising-edge match while a shell client is attached — open Start now.
    OpenNow,
    /// Rising-edge match while the shell is down — latch until attach.
    Latch,
    /// No fire this tick (including a completed release that clears the gate).
    Idle,
}

/// Decide send-now / latch / wait-for-release for the service reopen detector.
fn reopen_gesture_outcome(
    needs_release: bool,
    chord_held: bool,
    rising_edge: bool,
    client_connected: bool,
) -> ReopenGestureOutcome {
    if needs_release {
        if chord_held {
            return ReopenGestureOutcome::Hold;
        }
        return ReopenGestureOutcome::Idle;
    }
    if rising_edge {
        if client_connected {
            return ReopenGestureOutcome::OpenNow;
        }
        return ReopenGestureOutcome::Latch;
    }
    ReopenGestureOutcome::Idle
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_for_first_sample_only_when_session_empty() {
        assert!(hot_path_wait_for_first_sample(true, true, true));
        assert!(!hot_path_wait_for_first_sample(true, true, false));
        assert!(!hot_path_wait_for_first_sample(true, false, true));
        assert!(!hot_path_wait_for_first_sample(false, true, true));
    }

    #[test]
    fn reopen_sends_now_when_shell_connected() {
        assert_eq!(
            reopen_gesture_outcome(false, true, true, true),
            ReopenGestureOutcome::OpenNow
        );
    }

    #[test]
    fn reopen_latches_when_shell_down() {
        assert_eq!(
            reopen_gesture_outcome(false, true, true, false),
            ReopenGestureOutcome::Latch
        );
    }

    #[test]
    fn reopen_waits_for_release_after_fire() {
        assert_eq!(
            reopen_gesture_outcome(true, true, false, true),
            ReopenGestureOutcome::Hold
        );
        // Rising edge while still gated must not fire again.
        assert_eq!(
            reopen_gesture_outcome(true, true, true, true),
            ReopenGestureOutcome::Hold
        );
    }

    #[test]
    fn reopen_clears_gate_after_release() {
        assert_eq!(
            reopen_gesture_outcome(true, false, false, true),
            ReopenGestureOutcome::Idle
        );
    }

    #[test]
    fn reopen_idle_without_rising_edge() {
        assert_eq!(
            reopen_gesture_outcome(false, false, false, true),
            ReopenGestureOutcome::Idle
        );
        assert_eq!(
            reopen_gesture_outcome(false, true, false, true),
            ReopenGestureOutcome::Idle
        );
    }
}
