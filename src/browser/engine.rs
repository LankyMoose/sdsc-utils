//! Engine seam: commands down to the page backend, frames/events up.
//!
//! The `servo-engine` cargo feature swaps the reader backend for a Servo
//! offscreen worker thread (see `servo_backend`); without it the App never
//! constructs a handle and this module is pure types + tests. Either way the
//! UI only depends on [`EngineCommand`] / [`EngineEvent`], so the backend is
//! replaceable without touching Start.

use std::sync::{Arc, Mutex, mpsc};

/// One rendered page frame: RGBA pixels for an iced image handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Commands the UI sends to the backend. Fire-and-forget: a disconnected
/// backend drops them, and scroll floods coalesce naturally.
#[derive(Debug, Clone)]
pub enum EngineCommand {
    Navigate {
        url: String,
        generation: u64,
    },
    ScrollBy {
        dx: f32,
        dy: f32,
    },
    Key(keyboard_types::KeyboardEvent),
    SetDarkMode {
        dark: bool,
    },
    /// Click at a page point (device px == page px at 1x). Sent as
    /// down+up by the backend; the pad cursor owns this, never the OS mouse.
    ClickAt {
        x: f32,
        y: f32,
    },
    GoBack,
    GoForward,
    Resize {
        width: u32,
        height: u32,
    },
    SetVisible {
        visible: bool,
    },
    Suspend,
    Resume,
    Shutdown,
    /// Poke the loop awake (Servo's [`EventLoopWaker`] hook). No-op by the
    /// time it drains: the loop spins every few ms anyway.
    Wake,
}

/// Events the backend publishes to the UI subscription.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    Frame(PageFrame),
    TitleChanged {
        title: Option<String>,
    },
    UrlChanged {
        url: String,
    },
    /// Fresh focusable set from the accessibility tree. The UI re-snaps the
    /// cursor and drops stale attachments.
    A11yTree(Vec<crate::browser::cursor::Focusable>),
    LoadFinished {
        generation: u64,
        result: Result<(), String>,
    },
}

/// Channel pair between the UI shell and the backend worker thread.
///
/// Mirrors [`crate::domain::pad::InputHub`]: the worker owns the engine and
/// publishes here; the iced subscription binds an event sink while listening.
/// Everything is `try_send` — a saturated UI drops frames (the next one lands
/// in ~16ms), exactly like pad edges.
#[derive(Debug)]
pub struct EngineHandle {
    commands: mpsc::Sender<EngineCommand>,
    events: Arc<Mutex<Option<iced::futures::channel::mpsc::Sender<EngineEvent>>>>,
}

impl Clone for EngineHandle {
    fn clone(&self) -> Self {
        Self {
            commands: self.commands.clone(),
            events: Arc::clone(&self.events),
        }
    }
}

/// Identity hash: clones share the event sink, so the subscription recipe
/// stays stable across reconciles (same pattern as the pad `InputHub` id).
impl std::hash::Hash for EngineHandle {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.events).hash(state);
    }
}

impl PartialEq for EngineHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.events, &other.events)
    }
}

impl Eq for EngineHandle {}

impl EngineHandle {
    /// Create a handle plus the command receiver the worker thread drains.
    pub fn channel() -> (Self, mpsc::Receiver<EngineCommand>) {
        let (tx, rx) = mpsc::channel();
        (
            Self {
                commands: tx,
                events: Arc::new(Mutex::new(None)),
            },
            rx,
        )
    }

    /// A send-only twin for wakers and other threads.
    pub fn sender(&self) -> mpsc::Sender<EngineCommand> {
        self.commands.clone()
    }

    /// Fire-and-forget command (drops when the worker is gone).
    pub fn send(&self, command: EngineCommand) {
        let _ = self.commands.send(command);
    }

    /// Install the iced-side event sink (subscription start).
    pub fn bind(&self, tx: iced::futures::channel::mpsc::Sender<EngineEvent>) {
        if let Ok(mut guard) = self.events.lock() {
            *guard = Some(tx);
        }
    }

    /// Clear the iced-side sink (subscription end).
    pub fn clear(&self) {
        if let Ok(mut guard) = self.events.lock() {
            *guard = None;
        }
    }

    /// Publish one event. Called on the worker thread; never blocks.
    /// Frames that arrive while the UI is saturated are dropped (conflation
    /// in the subscription keeps the latest anyway).
    pub fn emit(&self, event: EngineEvent) {
        let Ok(mut guard) = self.events.lock() else {
            return;
        };
        let Some(tx) = guard.as_mut() else {
            return;
        };
        let _ = tx.try_send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unbound_emit_drops_silently() {
        let (handle, rx) = EngineHandle::channel();
        handle.emit(EngineEvent::UrlChanged {
            url: "https://example.com".to_string(),
        });
        handle.send(EngineCommand::Suspend);
        assert!(matches!(rx.recv(), Ok(EngineCommand::Suspend)));
    }

    #[test]
    fn scroll_commands_pass_through_in_order() {
        let (handle, rx) = EngineHandle::channel();
        handle.send(EngineCommand::ScrollBy { dx: 0.0, dy: 240.0 });
        handle.send(EngineCommand::ScrollBy {
            dx: 0.0,
            dy: -240.0,
        });
        assert!(matches!(
            rx.recv(),
            Ok(EngineCommand::ScrollBy { dy, .. }) if dy == 240.0
        ));
        assert!(matches!(
            rx.recv(),
            Ok(EngineCommand::ScrollBy { dy, .. }) if dy == -240.0
        ));
    }
}
