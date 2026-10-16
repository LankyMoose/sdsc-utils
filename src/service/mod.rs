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
    ApplyContext, DeviceSession, SessionEffect, StartSelection, controllers_equivalent,
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
    // Developer emulator fleet (debug-only): remembered `emu-` pads come
    // back disconnected, exactly like the shell's former boot rehydrate —
    // but owned here, so there is a single writer for `controllers.json`.
    #[cfg(debug_assertions)]
    {
        let remembered: Vec<(String, u8, crate::controller::model::Connection)> = session
            .known
            .remembered_disconnected(&[])
            .into_iter()
            .map(|r| {
                (
                    r.serial.clone(),
                    r.percent,
                    crate::controller::model::Connection::from_label(&r.connection)
                        .unwrap_or(crate::controller::model::Connection::Bluetooth),
                )
            })
            .collect();
        session.emu_fleet = crate::controller::emulate::rehydrate(&remembered);
    }
    let hid_worker = HidWorkerHandle::start();
    let pipe = PipeServer::start()?;
    // Kill shells left over from a previous install before spawning ours.
    // An unversioned pre-handshake shell (1.6.1 and earlier) reconnects
    // forever and would otherwise attach to this service; it understands
    // `Shutdown`, so let stragglers attach, tell them to quit, and only then
    // spawn the matching shell (which can never catch this broadcast).
    shutdown_stale_shells(&pipe);
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
    // Newest client id already sent the snapshot; see `send_to_newcomers`.
    let mut pushed_watermark: u64 = 0;
    // Last retained-snapshot rev broadcast to all clients (change-pushes).
    let mut last_snapshot_rev: u64 = 0;
    // Recent shell exit times (crash-loop backoff) and whether the service
    // still manages the shell (false after a protocol-quit or repeated
    // crashes — tray/HID continue headless).
    let mut shell_exits: Vec<Instant> = Vec::new();
    let mut shell_managed = true;

    // Forward hid-worker pad edges over IPC (and run reopen gesture while shell is down).
    // Deep enough to ride out shell-frame hitches without dropping taps.
    let (edge_tx, edge_rx) = mpsc::sync_channel::<start_input::InputEdge>(128);
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

        if shell_managed && last_shell_check.elapsed() >= Duration::from_secs(1) {
            last_shell_check = Instant::now();
            if let Some(status) = shell.try_wait()? {
                // Exit code 3 is the shell's deliberate protocol-quit (version
                // mismatch / no service): respawning the same binary would loop
                // forever. Stay headless and say how to fix it.
                if status.code() == Some(3) {
                    app_log::error(
                        "service: shell quit on IPC version mismatch; not respawning \
                         (reinstall/restart matched binaries, then relaunch)",
                    );
                    shell_managed = false;
                } else {
                    app_log::warn(format!(
                        "service: shell exited ({status}); respawning without reopening HID"
                    ));
                    // Chord may still be held across the restart — require release.
                    chord_release_gate.arm();
                    gesture_detectors.reset();
                    // Back off on rapid exits (crash loop): a shell that dies
                    // instantly three times running is never going to survive
                    // the fourth second. Tray/HID keep running headless.
                    shell_exits.push(Instant::now());
                    shell_exits.retain(|t| t.elapsed() < Duration::from_secs(15));
                    if shell_exits.len() > 3 {
                        app_log::error(
                            "service: shell exiting repeatedly; not respawning \
                             (relaunch the app to try again)",
                        );
                        shell_managed = false;
                    } else {
                        shell = spawn_shell()?;
                    }
                }
            }
        }

        // Push the live snapshot only to transports that connected since the
        // last push — usually the shell's broadcast receiver landing after its
        // command connection (which wins the attach race and is never read).
        // Newcomers are detected by watermark, not by count: disconnects only
        // prune, and ids are monotonic, so a respawned shell is always newer.
        // Keep the OpenStart latch until ReportStartVisible(true): a brief
        // connect (or a message sent before the shell's recv loop is live)
        // must not clear it.
        if pipe.has_newcomers(pushed_watermark) {
            // Retained snapshot only: live + remembered + prefs + analytics
            // + stored selection arrive together under one rev, so a
            // restarted shell converges without waiting for the next HID
            // edge (and keys always meet the rows they point at).
            let mut batch = vec![ServiceMessage::Snapshot(Box::new(session.snapshot()))];
            let latched = pending_open_start;
            if latched {
                batch.push(ServiceMessage::Effects(vec![SessionEffect::OpenStart]));
            }
            if pipe.send_to_newcomers(&batch, &mut pushed_watermark) > 0 {
                last_snapshot_rev = session.snapshot_rev();
                if latched {
                    app_log::info("service: flushing latched OpenStart (new transport)");
                }
            }
        }

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
                    &mut tray,
                    &mut pending_open_start,
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
                // HID is gone but emulated pads (debug-only) may still be
                // live — publish those, not an empty list.
                #[cfg(debug_assertions)]
                let cleared = with_emu_live(&[], &session);
                let effects = session.apply_controllers(
                    #[cfg(debug_assertions)]
                    cleared,
                    #[cfg(not(debug_assertions))]
                    Vec::new(),
                    ApplyContext {
                        start_visible,
                        fullscreen: start_input::foreground_is_exclusive_fullscreen(),
                        now: Instant::now(),
                        lightbar_enabled: lightbar::is_enabled(),
                    },
                );
                // Retained snapshot (rev-gated change-push at loop end) is the
                // shell's only live-rows path now — no `Controllers` push.
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
                            #[cfg(debug_assertions)]
                            let controllers = with_emu_live(&controllers, &session);
                            let effects = session.on_poll_result(
                                controllers,
                                ApplyContext {
                                    start_visible,
                                    fullscreen: start_input::foreground_is_exclusive_fullscreen(),
                                    now: Instant::now(),
                                    lightbar_enabled: lightbar::is_enabled(),
                                },
                            );
                            // Snapshot change-push at loop end covers the shell.
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
            let hid_live = hid_worker.live_controllers();
            // Always clear streaks for seen pads, even when the snapshot is
            // equivalent and evaluation is skipped (else a recovered pad's
            // stale streak drops it instantly on its next stall).
            session.mark_seen(hid_live.iter().map(|c| c.serial.as_str()));
            if hot_path_wait_for_first_sample(
                hid_live.is_empty(),
                !last_discovered.is_empty(),
                session.controllers.is_empty(),
            ) {
                if presence_changed {
                    crate::controller::hid::diag::diag_info(
                        "service: hot path waiting for live battery sample",
                    );
                }
            } else {
                // Gate on the merged snapshot (hardware + emulator): with an
                // emulated pad connected the HID-only list is never
                // equivalent, and without this the hot path would re-apply
                // every 16ms. Lightbar stays hardware-only — emu pads have
                // no lamp to drive.
                #[cfg(debug_assertions)]
                let live = with_emu_live(&hid_live, &session);
                #[cfg(not(debug_assertions))]
                let live = hid_live;
                if !controllers_equivalent(&session.controllers, &live) || presence_changed {
                    #[cfg(debug_assertions)]
                    let hid_only: Vec<ControllerStatus> = live
                        .iter()
                        .filter(|c| !crate::controller::emulate::is_emulated(&c.serial))
                        .cloned()
                        .collect();
                    #[cfg(debug_assertions)]
                    apply_hot_path_lightbar(&hid_worker, &session.controllers, &hid_only);
                    #[cfg(not(debug_assertions))]
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
                    // Snapshot change-push at loop end covers the shell.
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

        // Converge the shell's retained snapshot on any session change from
        // this iteration (poll applies, persist reloads, selection reports).
        push_snapshot_if_changed(&session, &pipe, &mut last_snapshot_rev);

        thread::sleep(Duration::from_millis(16));
    }
}

enum LoopControl {
    Continue,
    Quit,
}

/// One shell command: HID proxies, session intents, Start visibility.
/// Carries the service loop's shared state by parameter (single call site);
/// a `LoopState` struct split is the natural cleanup if this grows again.
///
/// `tray` / `pending_open_start` are only touched by the debug-only emulator
/// arm; the underscore names keep release builds (no emulator) quiet.
#[allow(clippy::too_many_arguments)]
fn handle_command(
    cmd: ShellCommand,
    session: &mut DeviceSession,
    hid_worker: &HidWorkerHandle,
    pipe: &PipeServer,
    _tray: &mut Option<tray_icon::TrayIcon>,
    _pending_open_start: &mut bool,
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
        ShellCommand::SetPrefs { prefs } => {
            // Service is the prefs authority: validate, persist via the
            // snapshot rev path, and mirror the disk-reload side effects
            // (spectrum, lightbar, USB-scope close) so shell edits and
            // hand-edits converge through one code path.
            let scope_before = (
                session.prefs.start_screen_auto_open.includes_usb(),
                session.prefs.start_screen_auto_open.auto_opens(),
            );
            session.apply_prefs(prefs.normalized());
            session.prefs.save();
            color::set_active_spectrum(session.prefs.spectrum.clone());
            lightbar::set_enabled(session.prefs.lightbar_enabled);
            let scope_after = (
                session.prefs.start_screen_auto_open.includes_usb(),
                session.prefs.start_screen_auto_open.auto_opens(),
            );
            if scope_after.1 && scope_before.0 != scope_after.0 {
                let effects = session.reevaluate_start_presence(ApplyContext {
                    start_visible: *start_visible,
                    fullscreen: start_input::foreground_is_exclusive_fullscreen(),
                    now: Instant::now(),
                    lightbar_enabled: lightbar::is_enabled(),
                });
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
            session.echo_intent();
            Ok(LoopControl::Continue)
        }
        ShellCommand::SetRemembered { serial, remembered } => {
            let live = session.controllers.clone();
            session.set_remembered(&serial, remembered, &live);
            session.known.save();
            session.echo_intent();
            Ok(LoopControl::Continue)
        }
        ShellCommand::SetNickname { serial, nickname } => {
            session.set_nickname(&serial, nickname);
            session.known.save();
            session.echo_intent();
            Ok(LoopControl::Continue)
        }
        ShellCommand::SetStartSelection {
            game_key,
            controller_key,
            game_index,
        } => {
            // In-memory only for the service process lifetime — never persisted.
            // Folded into the retained snapshot (no separate echo: the
            // reporting shell already holds the value; newcomers get it via
            // the snapshot).
            session.remember_start_selection(StartSelection {
                game_key,
                controller_key,
                game_index,
            });
            Ok(LoopControl::Continue)
        }
        ShellCommand::RequestSnapshot => {
            let _ = pipe.send(ServiceMessage::Snapshot(Box::new(session.snapshot())));
            Ok(LoopControl::Continue)
        }
        // Developer emulator edit (debug-only): the service owns the fleet.
        // The variant itself is debug-gated, so release needs no arm here.
        #[cfg(debug_assertions)]
        ShellCommand::Emu { command } => {
            apply_emu_command(
                command,
                session,
                hid_worker,
                pipe,
                _tray,
                _pending_open_start,
                *start_visible,
            );
            Ok(LoopControl::Continue)
        }
        // Transport-level hello, already answered on the accepting socket;
        // a repeat mid-connection means nothing.
        ShellCommand::GetVersion => Ok(LoopControl::Continue),
    }
}

/// Apply one developer-emulator edit to the service-owned fleet (debug-only).
///
/// Mirrors the shell's standalone emulator path: fleet transition, remembered
/// sync + save, analytics/prefs side effects, then publish of the merged
/// (hardware + emulator) live list through the normal session path so toasts,
/// Start policy, tray, and analytics edges all fire. The snapshot echo at the
/// loop end converges the shell (whose tab fleet is a pure derivation of the
/// snapshot, plus its local drag preview).
#[cfg(debug_assertions)]
#[allow(clippy::too_many_arguments)]
fn apply_emu_command(
    command: crate::controller::emulate::EmulatorCommand,
    session: &mut DeviceSession,
    hid_worker: &HidWorkerHandle,
    pipe: &PipeServer,
    tray: &mut Option<tray_icon::TrayIcon>,
    pending_open_start: &mut bool,
    start_visible: bool,
) {
    use crate::controller::emulate::{self, EmulatorCommand};
    use crate::controller::model::Connection;

    let fleet = session.emu_fleet.clone();
    let find = |serial: &str| fleet.iter().find(|p| p.serial() == serial).cloned();

    // `publish` runs the normal connect/disconnect path; `analytics` covers
    // the charge/drain walk. `PreviewPercent` arrives debounced-committed
    // (shell drag preview stays local), so it publishes like any level set.
    let (next_fleet, publish, analytics) = match &command {
        EmulatorCommand::AddPad => {
            let pad = emulate::new_pad(&fleet, emulate::DEFAULT_PERCENT, Connection::Bluetooth);
            let mut next = fleet.clone();
            next.push(pad);
            // Remembered but disconnected: no toast, no Start window.
            (next, false, false)
        }
        EmulatorCommand::RemovePad(serial) => {
            let next = fleet
                .iter()
                .filter(|p| p.serial() != serial)
                .cloned()
                .collect::<Vec<_>>();
            // The pruned pad leaves the remembered store via the sync below
            // (previous vs current), same as the shell path.
            (next, true, false)
        }
        EmulatorCommand::PreviewPercent(serial, percent) => {
            (emulate::set_percent(&fleet, serial, *percent), true, false)
        }
        // Plugging in or unplugging is a charge edge, so it credits
        // analytics the same way stepping the cycle does.
        EmulatorCommand::SetConnection(serial, connection) => {
            (emulate::set_link(&fleet, serial, *connection), true, true)
        }
        // Plugging in or unplugging is a real connect/disconnect edge.
        // Unplugging a forgotten pad removes it — same as forgetting an
        // unplugged pad. Either way the two together mean "gone".
        EmulatorCommand::SetConnected(serial, connected) => {
            let next = emulate::update(&fleet, serial, |p| p.connected = *connected);
            (emulate::prune(next), true, false)
        }
        // The popup's remember pin, for an emulated pad. Only takes effect
        // on unplug; `prune` then removes it if it is already off.
        EmulatorCommand::SetRemembered(serial, remembered) => {
            let next = emulate::update(&fleet, serial, |p| p.remembered = *remembered);
            (emulate::prune(next), true, false)
        }
        EmulatorCommand::SeedAnalytics(serial) => {
            session.analytics.dev_seed_estimates(serial);
            (fleet, false, true)
        }
        EmulatorCommand::StepCharge(serial) => {
            let Some(pad) = find(serial) else {
                return;
            };
            session
                .analytics
                .dev_credit_active(pad.serial(), emulate::CHARGE_STEP_TIME);
            let (percent, connection, state) = emulate::step_charge(&pad.status);
            let next = emulate::update(&fleet, serial, |p| {
                p.status.percent = percent;
                p.status.connection = connection;
                p.status.state = state;
            });
            (next, true, true)
        }
        EmulatorCommand::StepDrain(serial) => {
            let Some(pad) = find(serial) else {
                return;
            };
            session
                .analytics
                .dev_credit_active(pad.serial(), emulate::DRAIN_STEP_TIME);
            let (percent, connection, state) = emulate::step_drain(&pad.status);
            let next = emulate::update(&fleet, serial, |p| {
                p.status.percent = percent;
                p.status.connection = connection;
                p.status.state = state;
            });
            (next, true, true)
        }
    };

    // `previous` is captured before the swap so pruned pads still leave the
    // remembered store (forgetting must delete the record, not merely stop
    // writing it).
    let previous = std::mem::replace(&mut session.emu_fleet, next_fleet);
    if emulate::sync_remembered_emulated(&previous, &session.emu_fleet, &mut session.known) {
        session.known.save();
    }

    // Analytics state only means anything once it is recording.
    if analytics && !session.prefs.analytics_enabled {
        session.prefs.analytics_enabled = true;
        session.prefs.save();
        app_log::info("analytics enabled for developer emulator");
    }
    if analytics {
        session.analytics.save();
    }
    // Always echo: some edits (added-but-disconnected pads, seeded
    // estimates, disconnected percent commits) change no live row, so the
    // change-push would otherwise never carry them to the shell.
    session.echo_intent();

    if !publish {
        return;
    }
    // Merged live: hardware truth minus stale emu rows plus the fleet's
    // connected pads. Never touches HID sampling state.
    let mut merged: Vec<ControllerStatus> = session
        .controllers
        .iter()
        .filter(|c| !emulate::is_emulated(&c.serial))
        .cloned()
        .collect();
    merged.extend(emulate::live(&session.emu_fleet));
    let effects = session.apply_controllers(
        merged,
        ApplyContext {
            start_visible,
            fullscreen: start_input::foreground_is_exclusive_fullscreen(),
            now: Instant::now(),
            lightbar_enabled: lightbar::is_enabled(),
        },
    );
    dispatch_effects(session, hid_worker, pipe, tray, effects, pending_open_start);
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
    // Route through the rev-bumping setters so the retained snapshot (and the
    // shell rendering from it) converges on external prefs/known edits.
    session.apply_prefs(prefs);
    session.apply_known(KnownControllers::load());
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

/// Broadcast `Shutdown` to anything attached before our shell exists.
///
/// Stragglers can only be leftovers from a previous install: our shell has
/// not been spawned yet, and transient CLI calls are near-instant (a CLI
/// caught mid-call errors cleanly on the unexpected message). The 100ms
/// reconnect cadence of pre-handshake shells means a 250ms window catches
/// several attach cycles; the matching shell is spawned afterwards and can
/// never observe this broadcast.
fn shutdown_stale_shells(pipe: &PipeServer) {
    thread::sleep(Duration::from_millis(250));
    if pipe.client_count() == 0 {
        return;
    }
    app_log::info(format!(
        "service: shutting down {} stale pre-spawn client(s)",
        pipe.client_count()
    ));
    let _ = pipe.send(ServiceMessage::Shutdown);
    thread::sleep(Duration::from_millis(250));
}

/// Broadcast the retained snapshot when its rev advanced (change-push).
///
/// Cheap per-loop check (`u64` compare); the clone + send happen only on
/// actual session changes (same cadence as the existing `Controllers`
/// pushes). Keeps the shell's rendered state convergent without polling.
fn push_snapshot_if_changed(session: &DeviceSession, pipe: &PipeServer, last: &mut u64) {
    let rev = session.snapshot_rev();
    if rev != *last {
        *last = rev;
        let _ = pipe.send(ServiceMessage::Snapshot(Box::new(session.snapshot())));
    }
}

/// Merge connected emulated pads into a HID live snapshot (debug-only).
///
/// The session's live list is hardware + emulator; every ingestion merges so
/// either source alone drives toasts, Start policy, and analytics. Release
/// builds have no fleet: callers pass HID snapshots through unchanged.
#[cfg(debug_assertions)]
fn with_emu_live(hid: &[ControllerStatus], session: &DeviceSession) -> Vec<ControllerStatus> {
    let mut merged = hid.to_vec();
    merged.extend(crate::controller::emulate::live(&session.emu_fleet));
    merged
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

    /// Every ingestion merges hardware + emulator: with a pad connected the
    /// merged list carries both, and with none connected HID passes through.
    #[test]
    #[cfg(debug_assertions)]
    fn emu_merge_keeps_hardware_and_emulator() {
        use crate::controller::dualsense::battery::dualsense_status;
        use crate::controller::emulate;
        use crate::controller::model::{Connection, PowerState};
        use crate::persist::analytics::AnalyticsStore;
        use crate::persist::prefs::Prefs;

        let hid = vec![dualsense_status(
            1,
            "DualSense",
            Connection::Usb,
            "REAL-1".to_string(),
            80,
            PowerState::Charging,
        )];
        let mut session = DeviceSession::new(
            Prefs::default(),
            KnownControllers::default(),
            AnalyticsStore::default(),
        );
        let passthrough = with_emu_live(&hid, &session);
        assert_eq!(passthrough.len(), 1);
        assert_eq!(passthrough[0].serial, "REAL-1");

        let pad = emulate::new_pad(&[], emulate::DEFAULT_PERCENT, Connection::Bluetooth);
        session.emu_fleet = emulate::update(std::slice::from_ref(&pad), "emu-1", |p| {
            p.connected = true;
        });
        let merged = with_emu_live(&hid, &session);
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().any(|c| c.serial == "REAL-1"));
        assert!(merged.iter().any(|c| c.serial == "emu-1"));
    }
}
