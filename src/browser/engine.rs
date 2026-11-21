//! Engine seam: commands down to the page backend, frames/events up.
//!
//! The `cef-engine` cargo feature (on by default) backs this with the CEF
//! OSR worker (see `cef_backend`); `--no-default-features` is a UI-only build
//! and the App never constructs a handle. The UI only depends on
//! [`EngineCommand`] / [`EngineEvent`], so the backend stays replaceable.

use crate::browser::frame_slot::FrameSlot;
use std::sync::{Arc, Mutex, mpsc};

/// Coarse engine phase for the browser status strip. The backend owns the
/// transitions (navigate → load → DOM boot); the UI just renders the
/// latest value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EngineActivity {
    /// Nothing in flight.
    #[default]
    Idle,
    /// Explicit navigate accepted, load not started yet.
    Navigating,
    /// Document bytes arriving (`Started` / `HeadParsed`).
    Loading,
    /// Document complete; DOM observers installing and the first snapshot
    /// round-tripping ("initializing JS").
    Initializing,
}

impl EngineActivity {
    /// Status-strip label; empty when idle (Chrome shows nothing then).
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "",
            Self::Navigating => "Navigating…",
            Self::Loading => "Loading…",
            Self::Initializing => "Initializing…",
        }
    }
}

/// Stable tab identity for the multi-view engine.
///
/// The boot webview the worker creates at spawn is always [`TabId::BOOT`],
/// so the UI default-matches it with no handshake. UI-owned ids count up
/// from there (explicit new tabs); engine-owned ids count down from
/// [`TabId::ENGINE_TOP`] (popup `request_create_new` tabs). Disjoint by
/// construction over the fire-and-forget command channel — no coordination
/// needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TabId(pub u64);

impl TabId {
    /// First tab: the boot webview (home URL).
    pub const BOOT: TabId = TabId(1);
    /// Engine-owned ids count down from here.
    pub const ENGINE_TOP: u64 = u64::MAX;
}

/// Commands the UI sends to the backend. Fire-and-forget: a disconnected
/// backend drops them, and scroll floods coalesce naturally.
///
/// Every per-tab command carries its [`TabId`]; commands for unknown
/// (closed-race) tabs are ignored. Only the active tab receives input.
#[derive(Debug, Clone)]
pub enum EngineCommand {
    Navigate {
        tab: TabId,
        url: String,
        generation: u64,
    },
    ScrollBy {
        tab: TabId,
        dx: f32,
        dy: f32,
        /// Page point to scroll under (device px == page px at 1x): the pad
        /// cursor. Chromium scrolls the innermost scroller under the point, so
        /// this is both intent ("scroll what I point at") and the carousel
        /// trap escape.
        x: f32,
        y: f32,
    },
    Key(TabKey),
    SetDarkMode {
        dark: bool,
    },
    /// Mouse position sync: the pad cursor is the only mouse Chromium ever
    /// sees. The UI streams this on quantized movement so hover, hit-test
    /// and `:hover` state track the real cursor; clicks then need no
    /// pre-move.
    MouseMove {
        tab: TabId,
        x: f32,
        y: f32,
    },
    /// The UI cursor jumped: force a fresh DOM target cull around the
    /// current mouse position (the live set is culled around its pull
    /// center, so a jump can land where no snap target was collected yet).
    ReseedDom {
        tab: TabId,
    },
    /// Click at a page point (device px == page px at 1x), optionally on a
    /// known snap target (UI numeric id). Button-kind targets activate via
    /// element `.click()` (exact element, no hit-test staleness); everything
    /// else goes as trusted down+up at the point, which the pad cursor owns
    /// (never the OS mouse).
    ClickAt {
        tab: TabId,
        x: f32,
        y: f32,
        target: Option<u64>,
    },
    /// R3 diagnostic: arm a one-shot event-loop probe (timers, rAF,
    /// IntersectionObserver) and read it back ~700ms later. Tells whether
    /// page JS task sources are actually running.
    ProbeEventLoop {
        tab: TabId,
    },
    GoBack {
        tab: TabId,
        /// Entries to traverse at once (history selector jumps).
        steps: usize,
    },
    GoForward {
        tab: TabId,
        steps: usize,
    },
    /// Create a tab loading `url` from birth (builder URL, like the boot
    /// tab). A separate post-build `Navigate` loses to the blank's initial
    /// load (observed: blank history clears after the navigate and the real
    /// load never starts), so the URL rides the build. UI-assigned id.
    OpenTab {
        tab: TabId,
        url: String,
        generation: u64,
        /// Shared frame slot the worker writes into; the UI's stable
        /// texture primitive reads it. Owned by the UI tab page.
        frame: Arc<FrameSlot>,
    },
    /// Drop a tab (its `WebView` handle closes the view). The UI activates a
    /// neighbor first when closing the active tab.
    CloseTab {
        tab: TabId,
    },
    /// Foreground a tab: show + focus it, hide + unfocus the previous one,
    /// and gate all frame/DOM/input work to it.
    SetActiveTab {
        tab: TabId,
    },
    Resize {
        width: u32,
        height: u32,
    },
    SetVisible {
        visible: bool,
    },
    Suspend,
    Resume,
    /// Close all CEF browsers and stop the worker, then signal `done`.
    /// The sender waits on it (bounded) before process exit.
    Shutdown {
        done: std::sync::mpsc::Sender<()>,
    },
    /// Poke the loop awake. No-op by the time it drains: the loop wakes
    /// every few ms for DOM polling anyway.
    Wake,
}

/// Per-tab key event: the tab rides alongside so the worker routes the page
/// key to the right view.
#[derive(Debug, Clone)]
pub struct TabKey {
    pub tab: TabId,
    pub event: keyboard_types::KeyboardEvent,
}

/// Events the backend publishes to the UI subscription.
///
/// Every event carries its [`TabId`]; the UI routes active-tab events to the
/// live page state and background-tab events to the matching stored tab.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    Frame(TabFrame),
    TitleChanged {
        tab: TabId,
        title: Option<String>,
    },
    UrlChanged {
        tab: TabId,
        url: String,
    },
    /// Page favicon arrived (not emitted by CEF yet; reserved).
    /// Keyed by document URL; the UI caches handles per URL for the
    /// bookmark bar and history rows.
    FaviconChanged {
        tab: TabId,
        url: String,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    /// Engine phase changed (navigate → load → DOM boot). Drives the
    /// browser status strip ("real" status, not just clicks).
    ActivityChanged {
        tab: TabId,
        phase: EngineActivity,
    },
    /// Fresh focusable set from the accessibility tree. The UI re-snaps the
    /// Fresh focusable set from the accessibility tree. The UI re-snaps the
    /// cursor and drops stale attachments.
    A11yTree {
        tab: TabId,
        items: Vec<crate::browser::cursor::Focusable>,
    },
    /// Viewport scroll offset the backend translated the current tree with
    /// (document-space bounds minus this). Emitted when it changes so the
    /// debug overlay can correlate cursor against targets.
    ScrollOffset {
        tab: TabId,
        x: f32,
        y: f32,
    },
    /// Document extent in page pixels (grow-only per document). Drives the
    /// page scrollbar; reset to `(0,0)` on a new document.
    ScrollExtent {
        tab: TabId,
        extent: (f32, f32),
    },
    /// Page focus moved to a live focusable (autofocus attribute, JS
    /// `.focus()`, tabbing into the page). The UI docks the pad cursor
    /// onto it when possible. Fires only on change to a known target
    /// (never on blur-to-nothing).
    FocusChanged {
        tab: TabId,
        id: u64,
    },
    /// Canonical session history (engine-provided entries +
    /// current index). Overwrites the UI's locally-maintained session
    /// stack so the hold-bumper selector shows exact back/forward targets.
    HistoryChanged {
        tab: TabId,
        entries: Vec<String>,
        current: usize,
    },
    LoadFinished {
        tab: TabId,
        generation: u64,
        result: Result<(), String>,
    },
    /// Engine-created tab (popup `request_create_new`: `window.open()`,
    /// `target=_blank`). Already foregrounded backend-side; the UI adopts
    /// it (stash live, reset, set active).
    /// Engine-side slot handoff for the boot tab (UI-opened tabs already
    /// passed their slot in `OpenTab`). Replaces the UI page's placeholder.
    FrameSlot {
        tab: TabId,
        slot: Arc<FrameSlot>,
    },
    /// Engine-opened tab (`window.open`): carries the slot the worker
    /// writes into, so the UI adopts both in one event.
    TabOpened {
        tab: TabId,
        slot: Arc<FrameSlot>,
    },
    /// A tab's page called `window.close()`: the backend already dropped the
    /// view. The UI activates a neighbor (or a fresh home tab if none).
    TabClosed {
        tab: TabId,
    },
}

impl EngineEvent {
    /// Owning tab for page-state routing. `None` for tab-lifecycle events
    /// (`TabOpened` / `TabClosed`), which the UI handles before routing.
    pub fn tab(&self) -> Option<TabId> {
        match self {
            Self::Frame(f) => Some(f.tab),
            Self::FrameSlot { tab, .. } => Some(*tab),
            Self::TitleChanged { tab, .. }
            | Self::UrlChanged { tab, .. }
            | Self::FaviconChanged { tab, .. }
            | Self::ActivityChanged { tab, .. }
            | Self::ScrollOffset { tab, .. }
            | Self::ScrollExtent { tab, .. }
            | Self::FocusChanged { tab, .. }
            | Self::HistoryChanged { tab, .. }
            | Self::LoadFinished { tab, .. } => Some(*tab),
            Self::A11yTree { tab, .. } => Some(*tab),
            Self::TabOpened { .. } | Self::TabClosed { .. } => None,
        }
    }
}

/// One rendered page frame: owning tab plus RGBA pixels for an iced image
/// handle. Frames only ever flow for the active tab (background tabs never
/// paint), so the subscription's latest-frame conflation is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabFrame {
    pub tab: TabId,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Channel pair between the UI shell and the backend worker thread.
///
/// Mirrors [`crate::domain::pad::InputHub`]: the worker owns the engine and
/// publishes here; the iced subscription binds event sinks while listening.
///
/// Transport is split by data semantics, not by urgency. Frames are
/// idempotent state snapshots — latest wins, so a saturated UI drops stale
/// ones (the next one lands in ~16ms), exactly like pad edges. Everything
/// else is an edge-triggered fact (URL, history, focus, tree, tab
/// lifecycle) that must never drop: those ride an unbounded channel and
/// are forwarded in order. Cross-channel frame/control interleaving is
/// benign — frames are continuous paint, controls discrete facts, and the
/// UI already handles them arriving in any mix.
#[derive(Debug)]
pub struct EngineHandle {
    commands: mpsc::Sender<EngineCommand>,
    frames: Arc<Mutex<Option<iced::futures::channel::mpsc::Sender<TabFrame>>>>,
    controls: Arc<Mutex<Option<iced::futures::channel::mpsc::UnboundedSender<EngineEvent>>>>,
}

impl Clone for EngineHandle {
    fn clone(&self) -> Self {
        Self {
            commands: self.commands.clone(),
            frames: Arc::clone(&self.frames),
            controls: Arc::clone(&self.controls),
        }
    }
}

/// Identity hash: clones share the event sinks, so the subscription recipe
/// stays stable across reconciles (same pattern as the pad `InputHub` id).
impl std::hash::Hash for EngineHandle {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.frames).hash(state);
        Arc::as_ptr(&self.controls).hash(state);
    }
}

impl PartialEq for EngineHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.frames, &other.frames) && Arc::ptr_eq(&self.controls, &other.controls)
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
                frames: Arc::new(Mutex::new(None)),
                controls: Arc::new(Mutex::new(None)),
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

    /// Install the iced-side event sinks (subscription start): bounded for
    /// frames, unbounded for control events.
    pub fn bind(
        &self,
        frames: iced::futures::channel::mpsc::Sender<TabFrame>,
        controls: iced::futures::channel::mpsc::UnboundedSender<EngineEvent>,
    ) {
        if let Ok(mut guard) = self.frames.lock() {
            *guard = Some(frames);
        }
        if let Ok(mut guard) = self.controls.lock() {
            *guard = Some(controls);
        }
    }

    /// Clear the iced-side sinks (subscription end).
    pub fn clear(&self) {
        if let Ok(mut guard) = self.frames.lock() {
            *guard = None;
        }
        if let Ok(mut guard) = self.controls.lock() {
            *guard = None;
        }
    }

    /// Publish one event. Called on the worker thread; never blocks.
    /// Frames that arrive while the UI is saturated are dropped (the
    /// subscription keeps the latest anyway); control events are queued
    /// without bound and never drop — the subscription drains them in
    /// order.
    pub fn emit(&self, event: EngineEvent) {
        match event {
            EngineEvent::Frame(frame) => {
                let Ok(mut guard) = self.frames.lock() else {
                    return;
                };
                let Some(tx) = guard.as_mut() else {
                    return;
                };
                let _ = tx.try_send(frame);
            }
            control => {
                let Ok(mut guard) = self.controls.lock() else {
                    return;
                };
                let Some(tx) = guard.as_mut() else {
                    return;
                };
                let _ = tx.unbounded_send(control);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_report_their_tab() {
        let boot = TabId::BOOT;
        assert_eq!(
            EngineEvent::TitleChanged {
                tab: boot,
                title: None
            }
            .tab(),
            Some(boot)
        );
        assert_eq!(
            EngineEvent::Frame(TabFrame {
                tab: boot,
                width: 1,
                height: 1,
                rgba: vec![0, 0, 0, 255],
            })
            .tab(),
            Some(boot)
        );
        // Lifecycle events route before page state: no tab.
        assert_eq!(
            EngineEvent::TabOpened {
                tab: boot,
                slot: Arc::new(FrameSlot::default())
            }
            .tab(),
            None
        );
        assert_eq!(EngineEvent::TabClosed { tab: boot }.tab(), None);
    }

    #[test]
    fn unbound_emit_drops_silently() {
        let (handle, rx) = EngineHandle::channel();
        handle.emit(EngineEvent::UrlChanged {
            tab: TabId::BOOT,
            url: "https://example.com".to_string(),
        });
        handle.send(EngineCommand::Suspend);
        assert!(matches!(rx.recv(), Ok(EngineCommand::Suspend)));
    }

    #[test]
    fn control_events_arrive_lossless_and_in_order() {
        use iced::futures::channel::mpsc;
        let (handle, _) = EngineHandle::channel();
        let (frame_tx, _frame_rx) = mpsc::channel::<TabFrame>(64);
        let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded::<EngineEvent>();
        handle.bind(frame_tx, ctrl_tx);
        for i in 0..200u64 {
            handle.emit(EngineEvent::FocusChanged {
                tab: TabId::BOOT,
                id: i,
            });
        }
        // All 200 arrive, in order — no saturation drop, however bursty.
        for i in 0..200u64 {
            match ctrl_rx.try_recv() {
                Ok(EngineEvent::FocusChanged { id, .. }) => assert_eq!(id, i),
                other => panic!("lost control event at {i}: {other:?}"),
            }
        }
    }

    #[test]
    fn scroll_commands_pass_through_in_order() {
        let (handle, rx) = EngineHandle::channel();
        handle.send(EngineCommand::ScrollBy {
            tab: TabId::BOOT,
            dx: 0.0,
            dy: 240.0,
            x: 400.0,
            y: 300.0,
        });
        handle.send(EngineCommand::ScrollBy {
            tab: TabId::BOOT,
            dx: 0.0,
            dy: -240.0,
            x: 400.0,
            y: 300.0,
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
