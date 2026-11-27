//! Shared UI feedback: sounds, haptics, and the settings that control them.
//!
//! Launcher, Browser, Configure, and Home all route cues through these
//! methods so the shared `ui_*` preferences apply globally.

use super::{App, Message};
use crate::controller::dualsense::rumble::MotorPulse;
use crate::domain::navigation::{ActionOutcome, MoveOutcome, NavCue};
use crate::domain::pad::PadId;
use crate::persist::prefs::{clamp_ui_haptics_strength, clamp_ui_sound_volume};
use crate::platform::ui_sound::UiSoundKind;
use iced::Task;

/// Debug-only tally of cues actually emitted, read by replay QA to prove a
/// single action cannot double-sound.
#[cfg(debug_assertions)]
static UI_CUE_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Debug-only tally of haptic commands actually sent to the HID worker.
#[cfg(debug_assertions)]
static UI_HAPTIC_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl App {
    pub(crate) fn play_ui_sound(&self, kind: UiSoundKind) {
        if !self.session.prefs.ui_sounds_enabled {
            return;
        }
        let volume = f32::from(self.session.prefs.ui_sound_volume) / 100.0;
        self.sound.play(kind, volume);
    }

    /// Sound + optional pad haptic for a UI cue.
    pub(crate) fn play_ui_cue(&self, kind: UiSoundKind) {
        #[cfg(debug_assertions)]
        let total = UI_CUE_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        #[cfg(debug_assertions)]
        if matches!(kind, UiSoundKind::Action) {
            // Action commits are rare; two lines from one press prove a
            // double cue (Home -> Browser regression).
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: ui cue kind=Action total={total}"
            ));
        }
        self.play_ui_sound(kind);
        self.play_ui_haptic(kind);
    }

    /// Monotonic count of emitted cues (debug QA only).
    #[cfg(debug_assertions)]
    pub(crate) fn ui_cue_count() -> u64 {
        UI_CUE_COUNT.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(debug_assertions)]
    pub(crate) fn ui_haptic_count() -> u64 {
        UI_HAPTIC_COUNT.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn nav_cue_kind(cue: NavCue) -> UiSoundKind {
        match cue {
            NavCue::Nav => UiSoundKind::Nav,
            NavCue::Action => UiSoundKind::Action,
        }
    }

    /// The one feedback choke point for navigation outcomes: rejected inputs
    /// stay silent, an accepted move/action plays exactly its own cue.
    pub(crate) fn apply_move_outcome(&self, outcome: MoveOutcome) {
        if let Some(cue) = outcome.cue() {
            self.play_ui_cue(Self::nav_cue_kind(cue));
        }
    }

    pub(crate) fn apply_action_outcome(&self, outcome: ActionOutcome) {
        if let Some(cue) = outcome.cue() {
            self.play_ui_cue(Self::nav_cue_kind(cue));
        }
    }

    /// For surfaces whose success is not a `MoveOutcome` (scroll position,
    /// domain navigation), still route the cue through the same mapping.
    pub(crate) fn apply_nav_cue(&self, cue: NavCue) {
        self.play_ui_cue(Self::nav_cue_kind(cue));
    }

    pub(crate) fn play_ui_haptic(&self, kind: UiSoundKind) {
        if !self.session.prefs.ui_haptics_enabled || crate::platform::debug_quiet::enabled() {
            return;
        }
        let Some(serial) = self.haptic_pad_serial.as_ref() else {
            return;
        };
        let pulse = match kind {
            UiSoundKind::Nav => MotorPulse::NAV,
            UiSoundKind::Action => MotorPulse::ACTION,
            UiSoundKind::Hold => MotorPulse::HOLD,
            UiSoundKind::Slide => MotorPulse::SLIDE,
        };
        let (right, left, duration) = pulse.scaled(self.session.prefs.ui_haptics_strength);
        if right == 0 && left == 0 {
            return;
        }
        #[cfg(debug_assertions)]
        UI_HAPTIC_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.hid_worker
            .rumble(serial.clone(), right, left, duration.as_millis() as u64);
    }

    /// Preview haptic on every connected DualSense (settings toggle / strength slider).
    pub(crate) fn preview_haptic_all(&self, pulse: MotorPulse) {
        if !self.session.prefs.ui_haptics_enabled || crate::platform::debug_quiet::enabled() {
            return;
        }
        let (right, left, duration) = pulse.scaled(self.session.prefs.ui_haptics_strength);
        if right == 0 && left == 0 {
            return;
        }
        let ms = duration.as_millis() as u64;
        for controller in &self.session.controllers {
            self.hid_worker
                .rumble(controller.serial.clone(), right, left, ms);
        }
    }

    pub(crate) fn set_haptic_pad_from_id(&mut self, pad: Option<&PadId>) {
        self.haptic_pad_serial = pad.and_then(|id| {
            id.as_str()
                .strip_prefix("hid:")
                .map(crate::controller::model::Serial::normalize)
        });
    }

    pub(crate) fn apply_ui_sounds(&mut self, enabled: bool) -> Task<Message> {
        self.session.prefs.ui_sounds_enabled = enabled;
        self.save_prefs();
        if enabled {
            self.play_ui_sound(UiSoundKind::Nav);
        }
        Task::none()
    }

    pub(crate) fn apply_ui_sound_volume(&mut self, volume: u8) -> Task<Message> {
        let volume = clamp_ui_sound_volume(volume);
        if self.session.prefs.ui_sound_volume != volume {
            self.session.prefs.ui_sound_volume = volume;
            self.save_prefs();
            if self.session.prefs.ui_sounds_enabled {
                self.play_ui_sound(UiSoundKind::Nav);
            }
        }
        Task::none()
    }

    pub(crate) fn apply_ui_haptics(&mut self, enabled: bool) -> Task<Message> {
        self.session.prefs.ui_haptics_enabled = enabled;
        self.save_prefs();
        if enabled {
            self.preview_haptic_all(MotorPulse::NAV);
        } else {
            self.hid_worker.rumble_stop_all();
        }
        Task::none()
    }

    pub(crate) fn apply_ui_haptics_strength(&mut self, strength: u8) -> Task<Message> {
        let strength = clamp_ui_haptics_strength(strength);
        if self.session.prefs.ui_haptics_strength != strength {
            self.session.prefs.ui_haptics_strength = strength;
            self.save_prefs();
            if self.session.prefs.ui_haptics_enabled {
                self.preview_haptic_all(MotorPulse::NAV);
            }
        }
        Task::none()
    }
}
