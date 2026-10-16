//! Named-pipe IPC between the HID/tray service and the iced shell.
//!
//! Length-prefixed JSON frames (`u32` LE length + UTF-8 JSON body).
//!
//! Strict protocol, no cross-version compat:
//! - [`IPC_PROTOCOL_VERSION`] gates everything. Bump on any breaking
//!   `ShellCommand` / `ServiceMessage` change.
//! - The first frame on every connection MUST be `GetVersion` (see
//!   `PipeClient::handshake`). Anything else — including a connection that
//!   stays silent — is dropped without registering. Unversioned shells
//!   (1.6.1 and earlier) therefore can never attach, and are additionally
//!   swept by the service at startup (it broadcasts `Shutdown` before
//!   spawning its own shell; pre-handshake shells honor it).
//! - The shell handshakes on boot and every reconnect and quits (exit code
//!   3) on mismatch or when no service answers.

use crate::controller::model::ControllerStatus;
use crate::domain::color::Rgb;
use crate::domain::pad::InputEdge;
use crate::persist::notify::NotifyEvent;
use crate::persist::prefs::Prefs;
use crate::session::{SessionEffect, SessionSnapshot};
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

/// IPC protocol version. Bump on any breaking `ShellCommand` /
/// `ServiceMessage` change. The shell handshakes on every connect and quits
/// on mismatch (or when no service answers) — mismatched peers never limp
/// along on half-understood messages.
///
/// v2: `SetStartSelection` carries `game_index` (by-position restore) and the
/// snapshot prefs carry `start_selection_persist` / `start_selection_mode`.
pub const IPC_PROTOCOL_VERSION: u32 = 2;

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
    /// Absolute prefs write (shell → service). The service validates
    /// (`Prefs::normalized`), persists, and broadcasts the snapshot echo.
    /// Single-writer: the shell in client mode never writes `prefs.json`.
    SetPrefs {
        prefs: Prefs,
    },
    /// Absolute remember/forget pin (shell → service). Remembering resolves
    /// against the service's live list; unknown serials are a no-op echo.
    SetRemembered {
        serial: String,
        remembered: bool,
    },
    /// Absolute nickname write (shell → service; `None` clears).
    SetNickname {
        serial: String,
        nickname: Option<String>,
    },
    /// Start-screen selection changed (stable keys + game row index; service
    /// stores in memory and folds it into the retained snapshot for shell
    /// restarts). `game_index` serves by-position restore only; there is no
    /// controller index (controller persistence is out of scope).
    SetStartSelection {
        game_key: Option<String>,
        controller_key: Option<String>,
        #[serde(default)]
        game_index: Option<usize>,
    },
    /// Shell (re)start: ask the service for the retained session snapshot —
    /// the single convergent pull (live + remembered + prefs + analytics +
    /// selection under one rev).
    RequestSnapshot,
    /// Handshake probe: ask the service for its IPC protocol version.
    GetVersion,
    Ping,
    /// Developer emulator edit (debug-only). The service owns the fleet;
    /// the shell sends edits and renders the snapshot echo. Release builds
    /// have no sender and no fleet: the service ignores it.
    #[cfg(debug_assertions)]
    Emu {
        command: crate::controller::emulate::EmulatorCommand,
    },
}

/// Service → shell (and CLI replies).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServiceMessage {
    /// Session effects to apply in the shell.
    Effects(Vec<SessionEffect>),
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
    /// Retained session snapshot (reply to `RequestSnapshot`, push on change,
    /// proactive push when the shell attaches). The shell renders popup,
    /// Start dock, prefs-dependent UI, and analytics views from this — never
    /// from its own boot-time disk copies. Boxed: the snapshot is KBs next
    /// to unit/send-small sibling variants.
    Snapshot(Box<SessionSnapshot>),
    /// Reply to [`ShellCommand::GetVersion`]: the service's IPC protocol and
    /// build version. The shell quits unless `protocol` matches
    /// [`IPC_PROTOCOL_VERSION`].
    Version { protocol: u32, version: String },
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

/// Read one length-prefixed frame as an untyped JSON value.
///
/// Framing primitive used by the version handshake (which must skip
/// interleaved broadcasts on its way to the `Version` reply). All other
/// reads are strictly typed: undecodable frames are connection errors, not
/// something to skip over.
pub fn recv_value<R: Read>(reader: &mut R) -> Result<serde_json::Value, String> {
    read_frame(reader)
}

/// Path / name used for the pipe (overridable via env for tests).
pub fn pipe_endpoint() -> String {
    std::env::var(SHELL_PIPE_ENV).unwrap_or_else(|_| PIPE_NAME.to_string())
}

/// Long-lived command client used by the iced shell (and fire-and-forget HID proxies).
static CMD_CLIENT: Mutex<Option<PipeClient>> = Mutex::new(None);

/// Send a command to the running service, reconnecting if needed.
///
/// The first frame on a fresh connection is always the version handshake —
/// a service that fails it is one this process must not talk to.
pub fn send_command(cmd: &ShellCommand) -> Result<(), String> {
    let mut guard = CMD_CLIENT
        .lock()
        .map_err(|_| "ipc command lock poisoned".to_string())?;
    if guard.is_none() {
        let mut client = PipeClient::connect()?;
        let (protocol, version) = client.handshake(std::time::Duration::from_secs(2))?;
        ensure_protocol(protocol, &version)?;
        *guard = Some(client);
    }
    match guard.as_mut().unwrap().send_command(cmd) {
        Ok(()) => Ok(()),
        Err(_) => {
            // One reconnect attempt (with handshake).
            *guard = None;
            let mut client = PipeClient::connect()?;
            let (protocol, version) = client.handshake(std::time::Duration::from_secs(2))?;
            ensure_protocol(protocol, &version)?;
            let result = client.send_command(cmd);
            if result.is_ok() {
                *guard = Some(client);
            }
            result
        }
    }
}

/// Reject a handshake answer that is not our protocol. Mismatched peers
/// never exchange another frame.
pub fn ensure_protocol(protocol: u32, version: &str) -> Result<(), String> {
    if protocol != IPC_PROTOCOL_VERSION {
        return Err(format!(
            "ipc protocol mismatch (local {IPC_PROTOCOL_VERSION} vs service {protocol} v{version})"
        ));
    }
    Ok(())
}

/// Seed the cached command client with an already-handshook connection.
///
/// Shell boot verifies the service before iced starts, then hands that
/// connection over here instead of dropping it — no throwaway probe that
/// could steal a latched newcomer message on its way out.
pub fn seed_command_client(client: PipeClient) {
    if let Ok(mut guard) = CMD_CLIENT.lock() {
        *guard = Some(client);
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
pub use win_pipe::{PipeClient, PipeServer, PipeServerHandle, bound_port, service_endpoint_ready};

#[cfg(not(windows))]
mod unix_pipe;

#[cfg(not(windows))]
pub use unix_pipe::{PipeClient, PipeServer, PipeServerHandle, service_endpoint_ready};

#[cfg(not(windows))]
pub fn bound_port() -> u16 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::StartSelection;

    fn frame_round_trip<T>(value: &T) -> T
    where
        T: serde::Serialize + for<'de> serde::Deserialize<'de>,
    {
        let mut buf = Vec::new();
        send_message(&mut buf, value).expect("encode");
        let mut cursor = std::io::Cursor::new(buf);
        recv_message(&mut cursor).expect("decode")
    }

    #[test]
    fn set_start_selection_round_trip() {
        let cmd = ShellCommand::SetStartSelection {
            game_key: Some("steam:440".to_string()),
            controller_key: Some("pad-a".to_string()),
            game_index: Some(2),
        };
        match frame_round_trip(&cmd) {
            ShellCommand::SetStartSelection {
                game_key,
                controller_key,
                game_index,
            } => {
                assert_eq!(game_key.as_deref(), Some("steam:440"));
                assert_eq!(controller_key.as_deref(), Some("pad-a"));
                assert_eq!(game_index, Some(2));
            }
            other => panic!("unexpected command: {other:?}"),
        }
        // Pre-v2 payloads (no game_index) still decode.
        match frame_round_trip(
            &serde_json::from_str::<ShellCommand>(
                r#"{"SetStartSelection":{"game_key":"steam:440","controller_key":null}}"#,
            )
            .expect("legacy literal parses"),
        ) {
            ShellCommand::SetStartSelection { game_index, .. } => {
                assert_eq!(game_index, None);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn set_prefs_remember_nickname_round_trip() {
        let prefs = Prefs::default();
        match frame_round_trip(&ShellCommand::SetPrefs {
            prefs: prefs.clone(),
        }) {
            ShellCommand::SetPrefs { prefs: decoded } => {
                assert_eq!(
                    serde_json::to_value(&decoded).unwrap(),
                    serde_json::to_value(&prefs).unwrap()
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }
        match frame_round_trip(&ShellCommand::SetRemembered {
            serial: "abc".into(),
            remembered: true,
        }) {
            ShellCommand::SetRemembered { serial, remembered } => {
                assert_eq!(serial, "abc");
                assert!(remembered);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        match frame_round_trip(&ShellCommand::SetNickname {
            serial: "abc".into(),
            nickname: Some("Couch".into()),
        }) {
            ShellCommand::SetNickname { serial, nickname } => {
                assert_eq!(serial, "abc");
                assert_eq!(nickname.as_deref(), Some("Couch"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn selection_lives_in_snapshot_not_its_own_message() {
        // Cutover: stored selection travels folded into the retained
        // snapshot (one rev, keys meet rows). There is intentionally no
        // `ServiceMessage::StartSelection` anymore.
        use crate::session::SessionSnapshot;
        let snap = SessionSnapshot {
            rev: 3,
            live: Vec::new(),
            remembered: Vec::new(),
            nicknames: Default::default(),
            prefs: crate::persist::prefs::Prefs::default(),
            selection: StartSelection {
                game_key: Some("steam:440".to_string()),
                controller_key: None,
                game_index: None,
            },
            analytics: Default::default(),
        };
        match frame_round_trip(&ServiceMessage::Snapshot(Box::new(snap))) {
            ServiceMessage::Snapshot(decoded) => {
                assert_eq!(decoded.rev, 3);
                assert_eq!(decoded.selection.game_key.as_deref(), Some("steam:440"));
                assert_eq!(decoded.selection.controller_key, None);
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn snapshot_message_round_trips() {
        use crate::session::SessionSnapshot;
        let snap = SessionSnapshot {
            rev: 7,
            live: Vec::new(),
            remembered: Vec::new(),
            nicknames: Default::default(),
            prefs: crate::persist::prefs::Prefs::default(),
            selection: StartSelection::default(),
            analytics: Default::default(),
        };
        let msg = ServiceMessage::Snapshot(Box::new(snap));
        match frame_round_trip(&msg) {
            ServiceMessage::Snapshot(decoded) => assert_eq!(decoded.rev, 7),
            other => panic!("unexpected message: {other:?}"),
        }
        match frame_round_trip(&ShellCommand::RequestSnapshot) {
            ShellCommand::RequestSnapshot => {}
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn version_handshake_round_trips() {
        match frame_round_trip(&ShellCommand::GetVersion) {
            ShellCommand::GetVersion => {}
            other => panic!("unexpected command: {other:?}"),
        }
        let msg = ServiceMessage::Version {
            protocol: IPC_PROTOCOL_VERSION,
            version: "1.7.0".to_string(),
        };
        match frame_round_trip(&msg) {
            ServiceMessage::Version { protocol, version } => {
                assert_eq!(protocol, IPC_PROTOCOL_VERSION);
                assert_eq!(version, "1.7.0");
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn protocol_version_is_nonzero() {
        // Zero is reserved for pre-handshake (unversioned) peers, which never
        // answer GetVersion and therefore fail the handshake by timeout.
        const { assert!(IPC_PROTOCOL_VERSION > 0) }
    }

    #[test]
    fn ensure_protocol_rejects_gaps() {
        assert!(ensure_protocol(IPC_PROTOCOL_VERSION, "x").is_ok());
        let err = ensure_protocol(IPC_PROTOCOL_VERSION + 1, "future")
            .expect_err("must reject newer service");
        assert!(err.contains("mismatch"), "unexpected error: {err}");
        let err = ensure_protocol(0, "1.6.1").expect_err("must reject unversioned");
        assert!(err.contains("mismatch"), "unexpected error: {err}");
    }

    /// Developer emulator edits cross IPC as intents (debug-only).
    #[test]
    #[cfg(debug_assertions)]
    fn emu_command_round_trips() {
        let cmd = ShellCommand::Emu {
            command: crate::controller::emulate::EmulatorCommand::SetConnected(
                "emu-1".to_string(),
                true,
            ),
        };
        match frame_round_trip(&cmd) {
            ShellCommand::Emu { command } => {
                assert_eq!(
                    command,
                    crate::controller::emulate::EmulatorCommand::SetConnected(
                        "emu-1".to_string(),
                        true
                    )
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
