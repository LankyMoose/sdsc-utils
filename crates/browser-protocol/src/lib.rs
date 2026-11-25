//! Versioned host ↔ Browser app protocol for SDSC Utils.
//!
//! The same contract is used by the transitional in-process host and by the
//! optional `sdsc-browser` add-on. Browser v0 is deliberately tiny: native
//! surface, semantic actions, page text session, and OS-owned lifecycle. There
//! is no widget/status/toolbar toolkit.

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

/// Wire protocol version. Bump on incompatible host/Browser message changes.
pub const PROTOCOL_VERSION: u16 = 1;

/// Hosted app contract version (app specification surface).
pub const CONTRACT_VERSION: u32 = 0;

/// A control frame is capped so a corrupt/hostile length prefix cannot
/// allocate unbounded memory.
pub const MAX_FRAME_BYTES: usize = 1 << 20;

/// Stable app identity (e.g. `browser`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AppId(pub String);

impl AppId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// App capabilities the OS must provide/validate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub native_surface: bool,
    pub semantic_actions: bool,
    pub text_session: bool,
}

/// Manifest: identity, display metadata, and required capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub id: AppId,
    pub name: String,
    pub icon: Option<String>,
    pub version: String,
    pub capabilities: Capabilities,
}

/// v0 only supports native content surfaces. Semantic widgets are future work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceKind {
    Native,
}

/// The app-provided content surface. Geometry/hit-testing stay app-owned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceSpec {
    pub id: String,
    pub kind: SurfaceKind,
    pub label: String,
}

/// One semantic action and its published enabled state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionSpec {
    pub id: String,
    pub label: String,
    pub enabled: bool,
}

/// Page text focus state. The OS opens the system OSK; the app only reports
/// whether a text session is active (framing/autofocus never opens it).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextSessionState {
    #[default]
    Inactive,
    Active,
}

/// v0 lifecycle is OS-owned: launch/pause/resume/close are shell decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleSpec {
    pub os_owned: bool,
}

/// Complete serializable app specification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSpec {
    pub contract: u32,
    pub manifest: Manifest,
    pub surface: SurfaceSpec,
    pub actions: Vec<ActionSpec>,
    pub text_session: TextSessionState,
    pub lifecycle: LifecycleSpec,
}

impl AppSpec {
    pub fn action(&self, id: &str) -> Option<&ActionSpec> {
        self.actions.iter().find(|action| action.id == id)
    }

    pub fn action_enabled(&self, id: &str) -> bool {
        self.action(id).is_some_and(|action| action.enabled)
    }
}

/// Event sent from the OS to the app when a bound controller action fires.
/// The app never sees which button was pressed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionInvoked {
    pub app: AppId,
    pub action: String,
}

impl ActionInvoked {
    pub fn new(app: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            app: AppId::new(app),
            action: action.into(),
        }
    }
}

/// First message from the host. The Browser validates protocol + app identity
/// before creating its engine/window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u16,
    pub app: AppId,
    pub host_pid: u32,
    pub data_dir: String,
}

impl Hello {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.protocol != PROTOCOL_VERSION {
            return Err(ProtocolError::VersionMismatch {
                expected: PROTOCOL_VERSION,
                got: self.protocol,
            });
        }
        if self.app.as_str() != "browser" {
            return Err(ProtocolError::WrongApp(self.app.0.clone()));
        }
        Ok(())
    }
}

/// Controller button identity on the wire. Mirrors the source-side pad gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputButton {
    Cross,
    Circle,
    Square,
    Triangle,
    Options,
    Create,
    L1,
    R1,
    L3,
    R3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputActionKind {
    Tap,
    Hold,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputAction {
    pub pad: String,
    pub button: InputButton,
    pub action: InputActionKind,
}

/// Wire snapshot of one pad sample. All values are post-deadzone and already
/// normalized by the host HID worker; `held` carries control names used for
/// hold/power/gesture routing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PadSnapshot {
    pub stick_x: f32,
    pub stick_y: f32,
    pub lstick_x: f32,
    pub lstick_y: f32,
    pub rstick_x: f32,
    pub rstick_y: f32,
    pub dpad_up: bool,
    pub dpad_down: bool,
    pub dpad_left: bool,
    pub dpad_right: bool,
    pub cross: bool,
    pub circle: bool,
    pub square: bool,
    pub triangle: bool,
    pub options: bool,
    pub create: bool,
    pub l1: bool,
    pub r1: bool,
    pub l2: bool,
    pub r2: bool,
    pub l3: bool,
    pub ps: bool,
    pub touchpad: bool,
    pub held: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PadReading {
    pub id: String,
    pub sample: PadSnapshot,
}

/// One HID edge batch. `seq` is monotonic per host session; Browser input
/// handling is lossy-safe only in the same way the current in-process worker
/// is (latest state plus ordered action taps).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputEdge {
    pub seq: u64,
    pub reason: String,
    pub readings: Vec<PadReading>,
    pub actions: Vec<InputAction>,
}

/// Text actions from the system OSK to the Browser text session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextAction {
    Insert(String),
    Backspace,
    Enter,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleCommand {
    Focus,
    Pause,
    Resume,
    Close,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Starting,
    Running,
    Paused,
    Closing,
    Closed,
}

/// Real (non-controller) input the host needs for OSK dismissal/hand-off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RealInput {
    /// A physical keyboard event. `escape` is the only key the OSK consumes.
    Key {
        escape: bool,
    },
    Ime,
    Mouse,
    Touch,
}

/// Host → Browser control messages. Pad samples and semantic actions only;
/// the Browser renders its own window, so no frame/pixel IPC exists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostMessage {
    Hello(Hello),
    Resize {
        width: u32,
        height: u32,
    },
    Input(InputEdge),
    Action(ActionInvoked),
    Text(TextAction),
    Lifecycle(LifecycleCommand),
    /// System OSK open/close state for the Browser text session.
    OskState {
        open: bool,
    },
}

/// Browser → Host state messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowserEvent {
    /// App specification after engine/window creation, including enabled
    /// actions and current text-session state.
    Ready(AppSpec),
    /// Action enabled/text-session state changed without a full manifest.
    State {
        actions: Vec<ActionSpec>,
        text_session: TextSessionState,
    },
    /// A Cross landed on a page text field; the host may open the system OSK.
    /// The request is tied to the input/action that produced it.
    RequestTextInput,
    /// Real mouse/keyboard/IME/touch input. Closes the system OSK.
    RealInput(RealInput),
    Lifecycle(LifecycleState),
    Closed {
        reason: String,
    },
    Error {
        message: String,
    },
}

#[derive(Debug)]
pub enum ProtocolError {
    Io(io::Error),
    Json(serde_json::Error),
    FrameTooLarge(usize),
    VersionMismatch { expected: u16, got: u16 },
    WrongApp(String),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "protocol io: {err}"),
            Self::Json(err) => write!(f, "protocol json: {err}"),
            Self::FrameTooLarge(bytes) => write!(f, "protocol frame too large: {bytes} bytes"),
            Self::VersionMismatch { expected, got } => {
                write!(
                    f,
                    "protocol version mismatch: host {expected}, browser {got}"
                )
            }
            Self::WrongApp(app) => write!(f, "unexpected app id: {app}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

impl From<io::Error> for ProtocolError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<serde_json::Error> for ProtocolError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

/// Encode one length-prefixed JSON frame (`u32` LE length + payload).
pub fn encode_frame<T: Serialize>(message: &T) -> Result<Vec<u8>, ProtocolError> {
    let payload = serde_json::to_vec(message)?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(payload.len()));
    }
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Decode one length-prefixed JSON frame.
pub fn decode_frame<T: for<'de> Deserialize<'de>>(frame: &[u8]) -> Result<T, ProtocolError> {
    if frame.len() < 4 {
        return Err(ProtocolError::FrameTooLarge(frame.len()));
    }
    let len = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(len));
    }
    let payload = frame.get(4..4 + len).ok_or_else(|| {
        ProtocolError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated protocol frame",
        ))
    })?;
    Ok(serde_json::from_slice(payload)?)
}

pub fn write_frame<W: Write, T: Serialize>(
    writer: &mut W,
    message: &T,
) -> Result<(), ProtocolError> {
    writer.write_all(&encode_frame(message)?)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(
    reader: &mut R,
) -> Result<T, ProtocolError> {
    let mut len = [0u8; 4];
    reader.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(len));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    Ok(serde_json::from_slice(&payload)?)
}

/// In-process transport used by the transitional host and protocol tests.
/// A future named-pipe transport implements the same directions.
pub struct ChannelHost {
    to_browser: std::sync::mpsc::SyncSender<HostMessage>,
    from_browser: std::sync::mpsc::Receiver<BrowserEvent>,
}

pub struct ChannelBrowser {
    to_host: std::sync::mpsc::SyncSender<BrowserEvent>,
    from_host: std::sync::mpsc::Receiver<HostMessage>,
}

impl ChannelHost {
    pub fn send(&self, message: HostMessage) -> Result<(), ProtocolError> {
        self.to_browser.send(message).map_err(|_| {
            ProtocolError::Io(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "browser transport closed",
            ))
        })
    }

    pub fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<BrowserEvent, ProtocolError> {
        self.from_browser.recv_timeout(timeout).map_err(|err| {
            let kind = match err {
                std::sync::mpsc::RecvTimeoutError::Timeout => io::ErrorKind::TimedOut,
                std::sync::mpsc::RecvTimeoutError::Disconnected => io::ErrorKind::BrokenPipe,
            };
            ProtocolError::Io(io::Error::new(kind, "browser event receive failed"))
        })
    }
}

impl ChannelBrowser {
    pub fn send(&self, event: BrowserEvent) -> Result<(), ProtocolError> {
        self.to_host.send(event).map_err(|_| {
            ProtocolError::Io(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "host transport closed",
            ))
        })
    }

    pub fn recv_timeout(&self, timeout: std::time::Duration) -> Result<HostMessage, ProtocolError> {
        self.from_host.recv_timeout(timeout).map_err(|err| {
            let kind = match err {
                std::sync::mpsc::RecvTimeoutError::Timeout => io::ErrorKind::TimedOut,
                std::sync::mpsc::RecvTimeoutError::Disconnected => io::ErrorKind::BrokenPipe,
            };
            ProtocolError::Io(io::Error::new(kind, "host message receive failed"))
        })
    }
}

/// Create a bounded in-process host ↔ browser channel pair.
pub fn channel_pair(capacity: usize) -> (ChannelHost, ChannelBrowser) {
    let (host_tx, host_rx) = std::sync::mpsc::sync_channel(capacity.max(1));
    let (browser_tx, browser_rx) = std::sync::mpsc::sync_channel(capacity.max(1));
    (
        ChannelHost {
            to_browser: host_tx,
            from_browser: browser_rx,
        },
        ChannelBrowser {
            to_host: browser_tx,
            from_host: host_rx,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(enabled: bool) -> AppSpec {
        AppSpec {
            contract: CONTRACT_VERSION,
            manifest: Manifest {
                id: AppId::new("browser"),
                name: "Browser".to_string(),
                icon: None,
                version: "0".to_string(),
                capabilities: Capabilities {
                    native_surface: true,
                    semantic_actions: true,
                    text_session: true,
                },
            },
            surface: SurfaceSpec {
                id: "page".to_string(),
                kind: SurfaceKind::Native,
                label: "Page".to_string(),
            },
            actions: vec![ActionSpec {
                id: "back".to_string(),
                label: "Back".to_string(),
                enabled,
            }],
            text_session: TextSessionState::Inactive,
            lifecycle: LifecycleSpec { os_owned: true },
        }
    }

    fn hello() -> Hello {
        Hello {
            protocol: PROTOCOL_VERSION,
            app: AppId::new("browser"),
            host_pid: 42,
            data_dir: "C:\\data".to_string(),
        }
    }

    #[test]
    fn action_lookup_respects_enabled_state() {
        let enabled = spec(true);
        assert_eq!(enabled.contract, CONTRACT_VERSION);
        assert!(enabled.action_enabled("back"));
        assert!(!enabled.action_enabled("forward"));
        assert!(!spec(false).action_enabled("back"));
    }

    #[test]
    fn action_invoked_serializes_without_button_identity() {
        let invoked = ActionInvoked::new("browser", "reload");
        let json = serde_json::to_value(&invoked).unwrap();
        assert_eq!(json["app"], "browser");
        assert_eq!(json["action"], "reload");
        assert_eq!(json.get("button"), None);
    }

    #[test]
    fn hello_validates_protocol_and_app() {
        assert!(hello().validate().is_ok());
        let mut wrong_version = hello();
        wrong_version.protocol = PROTOCOL_VERSION + 1;
        assert!(matches!(
            wrong_version.validate(),
            Err(ProtocolError::VersionMismatch { .. })
        ));
        let mut wrong_app = hello();
        wrong_app.app = AppId::new("launcher");
        assert!(matches!(
            wrong_app.validate(),
            Err(ProtocolError::WrongApp(_))
        ));
    }

    #[test]
    fn host_and_browser_messages_round_trip() {
        let input = HostMessage::Input(InputEdge {
            seq: 7,
            reason: "timer".to_string(),
            readings: vec![PadReading {
                id: "hid:a".to_string(),
                sample: PadSnapshot {
                    lstick_x: 0.5,
                    lstick_y: -0.25,
                    cross: true,
                    held: vec!["Cross".to_string()],
                    ..PadSnapshot::default()
                },
            }],
            actions: vec![InputAction {
                pad: "hid:a".to_string(),
                button: InputButton::L1,
                action: InputActionKind::Tap,
            }],
        });
        let frame = encode_frame(&input).unwrap();
        let decoded: HostMessage = decode_frame(&frame).unwrap();
        assert_eq!(decoded, input);

        let event = BrowserEvent::State {
            actions: spec(true).actions,
            text_session: TextSessionState::Active,
        };
        let frame = encode_frame(&event).unwrap();
        let decoded: BrowserEvent = decode_frame(&frame).unwrap();
        assert_eq!(decoded, event);
    }

    #[test]
    fn stream_frames_round_trip_and_reject_oversize() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, &hello()).unwrap();
        write_frame(
            &mut buffer,
            &HostMessage::Lifecycle(LifecycleCommand::Pause),
        )
        .unwrap();
        let mut reader = buffer.as_slice();
        let first: Hello = read_frame(&mut reader).unwrap();
        let second: HostMessage = read_frame(&mut reader).unwrap();
        assert_eq!(first, hello());
        assert_eq!(second, HostMessage::Lifecycle(LifecycleCommand::Pause));

        let oversize = ((MAX_FRAME_BYTES as u32) + 1).to_le_bytes();
        let err = decode_frame::<HostMessage>(&oversize).unwrap_err();
        assert!(matches!(err, ProtocolError::FrameTooLarge(_)));
    }

    #[test]
    fn in_process_channel_carries_both_directions() {
        let (host, browser) = channel_pair(4);
        host.send(HostMessage::Hello(hello())).unwrap();
        let received = browser
            .recv_timeout(std::time::Duration::from_millis(50))
            .unwrap();
        assert_eq!(received, HostMessage::Hello(hello()));

        browser
            .send(BrowserEvent::Lifecycle(LifecycleState::Running))
            .unwrap();
        let received = host
            .recv_timeout(std::time::Duration::from_millis(50))
            .unwrap();
        assert_eq!(received, BrowserEvent::Lifecycle(LifecycleState::Running));
    }
}
