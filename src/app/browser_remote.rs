//! Host-side supervisor for the standalone `sdsc-browser` add-on process.

use crate::domain::pad::{PadAction, PadButton, PadButtonAction};
use crate::hosted::{
    BrowserEvent, Hello, HostMessage, InputAction, InputActionKind, InputButton, InputEdge,
    LifecycleCommand, PROTOCOL_VERSION, PadReading, PadSnapshot, TextAction, read_frame,
    write_frame,
};
use iced::Task;
use std::io::BufWriter;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Receiver;

use super::{App, Message};

pub fn remote_enabled() -> bool {
    std::env::var_os("SDSC_BROWSER_REMOTE").is_some()
}

pub struct BrowserRemote {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    events: Receiver<BrowserEvent>,
    pub last_error: Option<String>,
    seq: u64,
}

impl BrowserRemote {
    pub fn spawn() -> Result<Self, String> {
        let current = std::env::current_exe().map_err(|err| err.to_string())?;
        let name = if cfg!(windows) {
            "sdsc-browser.exe"
        } else {
            "sdsc-browser"
        };
        let exe = current.with_file_name(name);
        if !exe.is_file() {
            return Err(format!("browser add-on not found: {}", exe.display()));
        }
        let mut child = Command::new(&exe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| format!("spawn {}: {err}", exe.display()))?;
        let stdin = child.stdin.take().ok_or("browser stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("browser stdout unavailable")?;
        let (tx, events) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut stdout = stdout;
            while let Ok(event) = read_frame::<_, BrowserEvent>(&mut stdout) {
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
        let hello = Hello {
            protocol: PROTOCOL_VERSION,
            app: crate::hosted::AppId::new("browser"),
            host_pid: std::process::id(),
            data_dir: crate::persist::paths::data_dir().display().to_string(),
        };
        let mut stdin = BufWriter::new(stdin);
        write_frame(&mut stdin, &HostMessage::Hello(hello)).map_err(|err| err.to_string())?;
        Ok(Self {
            child,
            stdin,
            events,
            last_error: None,
            seq: 0,
        })
    }

    pub fn send(&mut self, message: HostMessage) {
        if write_frame(&mut self.stdin, &message).is_err() {
            self.last_error = Some("browser protocol write failed".to_string());
        }
    }

    pub fn send_input(
        &mut self,
        readings: &[crate::ui::launcher::input::NavReading],
        tap_actions: &[PadAction],
    ) {
        self.seq = self.seq.wrapping_add(1);
        let edge = InputEdge {
            seq: self.seq,
            reason: "poll".to_string(),
            readings: readings.iter().map(to_reading).collect(),
            actions: tap_actions.iter().filter_map(to_action).collect(),
        };
        self.send(HostMessage::Input(edge));
    }

    pub fn poll(&mut self) -> Vec<BrowserEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            events.push(event);
        }
        if let Ok(Some(status)) = self.child.try_wait() {
            events.push(BrowserEvent::Closed {
                reason: format!("process exited {status}"),
            });
        }
        events
    }

    pub fn shutdown(&mut self) {
        self.send(HostMessage::Lifecycle(LifecycleCommand::Shutdown));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn to_reading(reading: &crate::ui::launcher::input::NavReading) -> PadReading {
    let sample = &reading.sample;
    PadReading {
        id: reading.id.0.clone(),
        sample: PadSnapshot {
            stick_x: sample.stick_x,
            stick_y: sample.stick_y,
            lstick_x: sample.lstick_x,
            lstick_y: sample.lstick_y,
            rstick_x: sample.rstick_x,
            rstick_y: sample.rstick_y,
            dpad_up: sample.dpad_up,
            dpad_down: sample.dpad_down,
            dpad_left: sample.dpad_left,
            dpad_right: sample.dpad_right,
            cross: sample.cross,
            circle: sample.circle,
            square: sample.square,
            triangle: sample.triangle,
            options: sample.options,
            create: sample.create,
            l1: sample.l1,
            r1: sample.r1,
            l2: sample.l2,
            r2: sample.r2,
            l3: sample.l3,
            ps: sample
                .held
                .contains(&crate::domain::gesture::GestureControl::Ps),
            touchpad: sample
                .held
                .contains(&crate::domain::gesture::GestureControl::Touchpad),
            held: sample
                .held
                .iter()
                .map(|control| format!("{control:?}"))
                .collect(),
        },
    }
}

fn to_action(action: &PadAction) -> Option<InputAction> {
    let button = match action.button {
        PadButton::Cross => InputButton::Cross,
        PadButton::Circle => InputButton::Circle,
        PadButton::Square => InputButton::Square,
        PadButton::Triangle => InputButton::Triangle,
        PadButton::Options => InputButton::Options,
        PadButton::Create => InputButton::Create,
        PadButton::L1 => InputButton::L1,
        PadButton::R1 => InputButton::R1,
        PadButton::L3 => InputButton::L3,
        PadButton::R3 => InputButton::R3,
    };
    let kind = match action.action {
        PadButtonAction::Tap => InputActionKind::Tap,
        PadButtonAction::Hold => InputActionKind::Hold,
    };
    Some(InputAction {
        pad: action.pad.0.clone(),
        button,
        action: kind,
    })
}

impl App {
    pub(crate) fn open_remote_browser(&mut self) -> Task<Message> {
        if self.browser.remote.is_some() {
            return Task::none();
        }
        if self.launcher_visible {
            let _ = self.close_launcher();
        }
        match BrowserRemote::spawn() {
            Ok(remote) => {
                self.browser.remote = Some(remote);
                self.browser_visible = true;
                self.browser_pending_show = false;
                let _ = self.home_apps.open(crate::app::home::AppId::Browser);
                crate::controller::hid::diag::diag_info("ui-diag: browser add-on process open");
                Task::none()
            }
            Err(err) => {
                crate::platform::app_log::warn(format!("browser add-on launch failed: {err}"));
                self.browser.remote_error = Some(err);
                Task::none()
            }
        }
    }

    pub(crate) fn close_remote_browser(&mut self) -> Task<Message> {
        if let Some(mut remote) = self.browser.remote.take() {
            remote.shutdown();
            self.browser.remote_spec = None;
        }
        self.browser_visible = false;
        self.browser_pending_show = false;
        let _ = self.home_apps.close(crate::app::home::AppId::Browser);
        crate::controller::hid::diag::diag_info("ui-diag: browser add-on process close");
        self.close_osk("browser_close")
    }

    pub(crate) fn poll_remote_browser(&mut self) -> Task<Message> {
        let Some(remote) = &mut self.browser.remote else {
            return Task::none();
        };
        let events = remote.poll();
        let mut task = Task::none();
        for event in events {
            match event {
                BrowserEvent::Ready(spec) => {
                    self.browser.remote_spec = Some(spec);
                }
                BrowserEvent::State {
                    actions,
                    text_session,
                } => {
                    if let Some(spec) = self.browser.remote_spec.as_mut() {
                        spec.actions = actions;
                        spec.text_session = text_session;
                    }
                }
                BrowserEvent::RequestTextInput => {
                    if self.browser_visible {
                        task = task
                            .chain(self.request_osk_open(crate::ui::osk::OskTarget::BrowserPage));
                    }
                }
                BrowserEvent::RealInput(_) => {
                    task = task.chain(self.close_osk("real_input"));
                }
                BrowserEvent::Lifecycle(state) => {
                    crate::platform::app_log::info(format!("browser add-on lifecycle {state:?}"));
                }
                BrowserEvent::Closed { reason } => {
                    crate::platform::app_log::info(format!("browser add-on closed: {reason}"));
                    task = task.chain(self.close_remote_browser());
                }
                BrowserEvent::Error { message } => {
                    self.browser.remote_error = Some(message.clone());
                    crate::platform::app_log::warn(format!("browser add-on error: {message}"));
                }
            }
        }
        task
    }

    pub(crate) fn remote_browser_pad(
        &mut self,
        readings: &[crate::ui::launcher::input::NavReading],
        tap_actions: &[PadAction],
    ) -> Task<Message> {
        let semantic = self.hosted_action_for_tap(tap_actions);
        if let Some(invoked) = semantic {
            if let Some(remote) = &mut self.browser.remote {
                remote.send(HostMessage::Action(invoked));
            }
            return Task::none();
        }
        if let Some(remote) = &mut self.browser.remote {
            remote.send_input(readings, tap_actions);
        }
        Task::none()
    }

    pub(crate) fn remote_browser_text(&mut self, action: TextAction) {
        if let Some(remote) = &mut self.browser.remote {
            remote.send(HostMessage::Text(action));
        }
    }

    pub(crate) fn remote_browser_focus(&mut self) {
        if let Some(remote) = &mut self.browser.remote {
            remote.send(HostMessage::Lifecycle(LifecycleCommand::Focus));
        }
    }

    pub(crate) fn remote_browser_osk(&mut self, open: bool) {
        if let Some(remote) = &mut self.browser.remote {
            remote.send(HostMessage::OskState { open });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::pad::{PadId, PadSample};

    #[test]
    fn wire_conversion_keeps_sticks_and_taps() {
        let reading = crate::ui::launcher::input::NavReading {
            id: PadId("hid:a".into()),
            sample: PadSample {
                lstick_x: 0.5,
                lstick_y: -0.25,
                r2: true,
                held: [crate::domain::gesture::GestureControl::Ps]
                    .into_iter()
                    .collect(),
                ..PadSample::default()
            },
            source: "hid",
        };
        let wire = to_reading(&reading);
        assert_eq!(wire.id, "hid:a");
        assert_eq!(wire.sample.lstick_x, 0.5);
        assert_eq!(wire.sample.lstick_y, -0.25);
        assert!(wire.sample.r2);
        assert!(wire.sample.ps);
        let action = to_action(&PadAction {
            pad: PadId("hid:a".into()),
            button: PadButton::Cross,
            action: PadButtonAction::Tap,
        })
        .unwrap();
        assert_eq!(action.button, InputButton::Cross);
        assert_eq!(action.action, InputActionKind::Tap);
    }
}
