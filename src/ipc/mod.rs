//! Named-pipe IPC between the HID/tray service and the iced shell.
//!
//! Length-prefixed JSON frames (`u32` LE length + UTF-8 JSON body).

use crate::controller::model::ControllerStatus;
use crate::domain::color::Rgb;
use crate::domain::pad::InputEdge;
use crate::persist::notify::NotifyEvent;
use crate::session::SessionEffect;
use crate::ui::layout::TrayAnchor;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::sync::Mutex;

/// Well-known pipe name for the user-session service.
#[cfg(windows)]
pub const PIPE_NAME: &str = r"\\.\pipe\sdsc-utils-ipc";

#[cfg(not(windows))]
pub const PIPE_NAME: &str = "/tmp/sdsc-utils-ipc.sock";

/// Env var the service sets so the shell knows to attach instead of owning HID.
/// Value is the loopback TCP port (Windows) or the socket path (Unix).
pub const SHELL_PIPE_ENV: &str = "SDSC_UTILS_PIPE";

/// Shell → service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ShellCommand {
    Quit,
    OpenPopup {
        anchor: TrayAnchor,
    },
    OpenSettings,
    Identify {
        serial: String,
        percent: u8,
    },
    PowerOff {
        serial: String,
    },
    SetRgb {
        serial: String,
        color: Rgb,
    },
    SetLowBatteryTargets {
        targets: Vec<(String, u8)>,
    },
    SetInputHot {
        hot: bool,
    },
    /// Tell the service whether the start screen is visible (auto-open / close policy).
    ReportStartVisible {
        visible: bool,
    },
    Rumble {
        serial: String,
        right: u8,
        left: u8,
        duration_ms: u64,
    },
    RumbleStopAll,
    ListControllers,
    SetLightbarAll {
        color: Rgb,
    },
    /// Reload prefs / known from disk after the shell saves them.
    ReloadPersist,
    Ping,
}

/// Service → shell (and CLI replies).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServiceMessage {
    /// Session effects to apply in the shell.
    Effects(Vec<SessionEffect>),
    /// Full live controller snapshot after a poll.
    Controllers(Vec<ControllerStatus>),
    /// Pad input edge (report-rate).
    PadInput(InputEdge),
    /// Tray asked to open the popup.
    TrayOpenPopup { anchor: TrayAnchor },
    /// Tray asked to open Settings.
    TrayOpenSettings,
    /// Reply to [`ShellCommand::ListControllers`].
    ControllerList(Result<Vec<ControllerStatus>, String>),
    /// Reply to [`ShellCommand::SetLightbarAll`].
    LightbarSet { count: usize },
    /// Reply to Ping / generic ack.
    Ack,
    /// Service is shutting down.
    Shutdown,
    /// Forwarded notification events (when shell owns toast presentation).
    Notifications {
        events: Vec<NotifyEvent>,
        open_start_after_toast: bool,
    },
}

fn encode_frame(value: &impl Serialize) -> Result<Vec<u8>, String> {
    let body = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    let len = u32::try_from(body.len()).map_err(|_| "ipc frame too large".to_string())?;
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(reader: &mut R) -> Result<T, String> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).map_err(|e| e.to_string())?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 16 * 1024 * 1024 {
        return Err("ipc frame too large".into());
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).map_err(|e| e.to_string())?;
    serde_json::from_slice(&body).map_err(|e| e.to_string())
}

fn write_frame<W: Write>(writer: &mut W, value: &impl Serialize) -> Result<(), String> {
    let frame = encode_frame(value)?;
    writer.write_all(&frame).map_err(|e| e.to_string())?;
    writer.flush().map_err(|e| e.to_string())
}

/// Write a message to any Write sink.
pub fn send_message<W: Write>(writer: &mut W, value: &impl Serialize) -> Result<(), String> {
    write_frame(writer, value)
}

/// Read a message from any Read sink.
pub fn recv_message<R: Read, T: for<'de> Deserialize<'de>>(reader: &mut R) -> Result<T, String> {
    read_frame(reader)
}

/// Path / name used for the pipe (overridable via env for tests).
pub fn pipe_endpoint() -> String {
    std::env::var(SHELL_PIPE_ENV).unwrap_or_else(|_| PIPE_NAME.to_string())
}

/// Long-lived command client used by the iced shell (and fire-and-forget HID proxies).
static CMD_CLIENT: Mutex<Option<PipeClient>> = Mutex::new(None);

/// Send a command to the running service, reconnecting if needed.
pub fn send_command(cmd: &ShellCommand) -> Result<(), String> {
    let mut guard = CMD_CLIENT
        .lock()
        .map_err(|_| "ipc command lock poisoned".to_string())?;
    if guard.is_none() {
        *guard = Some(PipeClient::connect()?);
    }
    match guard.as_mut().unwrap().send_command(cmd) {
        Ok(()) => Ok(()),
        Err(err) => {
            // One reconnect attempt.
            *guard = None;
            let mut client = PipeClient::connect()?;
            let result = client.send_command(cmd);
            if result.is_ok() {
                *guard = Some(client);
            }
            result.map_err(|_| err)
        }
    }
}

/// Drop the cached command client (e.g. on shell shutdown).
pub fn clear_command_client() {
    if let Ok(mut guard) = CMD_CLIENT.lock() {
        *guard = None;
    }
}

/// Drain until `pred` matches (skips broadcast PadInput / Controllers / Effects).
pub fn recv_matching(
    client: &mut PipeClient,
    mut pred: impl FnMut(&ServiceMessage) -> bool,
) -> Result<ServiceMessage, String> {
    loop {
        let msg = client.recv()?;
        if pred(&msg) {
            return Ok(msg);
        }
    }
}

#[cfg(windows)]
mod win_pipe;

#[cfg(windows)]
pub use win_pipe::{PipeClient, PipeServer, PipeServerHandle, bound_port};

#[cfg(not(windows))]
mod unix_pipe;

#[cfg(not(windows))]
pub use unix_pipe::{PipeClient, PipeServer, PipeServerHandle};

#[cfg(not(windows))]
pub fn bound_port() -> u16 {
    0
}
