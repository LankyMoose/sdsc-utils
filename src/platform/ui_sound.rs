//! Short embedded UI cues for the start screen.
//!
//! Playback runs on a dedicated worker thread so WASAPI/COM init from `cpal`
//! never touches the iced / winit UI thread (which causes RPC_E_CHANGED_MODE
//! issues and unclean teardown on Windows).

use crate::platform::app_log;
use rodio::{Decoder, OutputStream, Sink};
use std::io::Cursor;
use std::sync::OnceLock;
use std::sync::mpsc::{self, Sender};
use std::thread;

const NAV_MP3: &[u8] = include_bytes!("../../assets/sounds/ui-nav.mp3");
const ACTION_MP3: &[u8] = include_bytes!("../../assets/sounds/ui-action.mp3");
const HOLD_MP3: &[u8] = include_bytes!("../../assets/sounds/ui-hold.mp3");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiSoundKind {
    Nav,
    Action,
    Hold,
}

impl UiSoundKind {
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Nav => NAV_MP3,
            Self::Action => ACTION_MP3,
            Self::Hold => HOLD_MP3,
        }
    }
}

enum Cmd {
    Play { kind: UiSoundKind, volume: f32 },
}

fn sender() -> Option<&'static Sender<Cmd>> {
    static TX: OnceLock<Option<Sender<Cmd>>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let spawn = thread::Builder::new()
            .name("ui-sound".into())
            .spawn(move || {
                let Ok((_stream, handle)) = OutputStream::try_default() else {
                    app_log::warn("ui sound output unavailable");
                    // Drain so senders do not block forever if we ever switch to bounded.
                    while rx.recv().is_ok() {}
                    return;
                };
                while let Ok(Cmd::Play { kind, volume }) = rx.recv() {
                    let Ok(sink) = Sink::try_new(&handle) else {
                        continue;
                    };
                    let cursor = Cursor::new(kind.bytes());
                    let Ok(source) = Decoder::new(cursor) else {
                        continue;
                    };
                    sink.set_volume(volume.clamp(0.0, 1.0));
                    sink.append(source);
                    sink.detach();
                }
            });
        match spawn {
            Ok(_) => Some(tx),
            Err(err) => {
                app_log::warn(format!("ui sound worker failed to start: {err}"));
                None
            }
        }
    })
    .as_ref()
}

/// Play an embedded cue at `volume` in 0.0..=1.0. No-ops when volume is ~0.
pub fn play(kind: UiSoundKind, volume: f32) {
    let volume = volume.clamp(0.0, 1.0);
    if volume < 0.001 {
        return;
    }
    let Some(tx) = sender() else {
        return;
    };
    let _ = tx.send(Cmd::Play { kind, volume });
}
