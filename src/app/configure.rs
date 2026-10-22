//! Configure (Settings) window: lifecycle, reducer, spectrum + emulator.
//!
//! Behavior-preserving extraction from `app::App`; no logic change.

use super::{App, Message, SPECTRUM_DEBOUNCE, delay, is_emulated_serial};
#[cfg(debug_assertions)]
use super::{EMULATOR_COMMIT_DEBOUNCE, PendingEmulatorCommit};
#[cfg(debug_assertions)]
use crate::controller::emulate;
#[cfg(debug_assertions)]
use crate::controller::model::Connection;
#[cfg(debug_assertions)]
use crate::controller::model::ControllerStatus;
use crate::games::launch;
use crate::persist::paths;
use crate::persist::prefs::{clamp_low_battery_percent, renderer_restart_needed};
use crate::platform::app_log;
#[cfg(windows)]
use crate::platform::host::Host;
#[cfg(debug_assertions)]
use crate::platform::ui_sound::UiSoundKind;
use crate::ui::color::{self, BatterySpectrum};
#[cfg(debug_assertions)]
use crate::ui::configure::EmulatorCommand;
use crate::ui::configure::{
    self as configure_view, AnalyticsPadRow, AnalyticsPanel, ConfigureMessage, ConfigureSettings,
    NotificationSetting, Section,
};
use crate::ui::layout::{focus_window, open_visible, window_platform_specific};
#[cfg(debug_assertions)]
use crate::ui::start::gesture;
use crate::ui::start::input::{self as start_input, NavAction};
use iced::widget::operation;
use iced::{Size, Task, window};
#[cfg(debug_assertions)]
use std::time::Instant;

impl App {
    pub(crate) fn configure_settings(&self) -> ConfigureSettings {
        ConfigureSettings {
            notify_low: self.session.prefs.notify_low,
            notify_charged: self.session.prefs.notify_charged,
            notify_connect: self.session.prefs.notify_connect,
            notify_disconnect: self.session.prefs.notify_disconnect,
            low_battery_percent: self.session.prefs.low_battery_percent,
            toast_position: self.session.prefs.toast_position,
            analytics_enabled: self.session.prefs.analytics_enabled,
            lightbar_enabled: self.session.prefs.lightbar_enabled,
            start_screen_enabled: self.session.prefs.start_screen_enabled,
            start_screen_auto_open: self.session.prefs.start_screen_auto_open,
            start_screen_always_immersive: self.session.prefs.start_screen_always_immersive,
            start_screen_immersive_layout: self.session.prefs.start_screen_immersive_layout,
            start_screen_clock_enabled: self.session.prefs.start_screen_clock_enabled,
            start_screen_navbar: self.session.prefs.start_screen_navbar,
            reduced_motion: self.session.prefs.reduced_motion,
            perf_throttle_immersive: self.session.prefs.perf_throttle_immersive,
            perf_antialiasing: self.session.prefs.perf_antialiasing,
            perf_vsync: self.session.prefs.perf_vsync,
            perf_restart_needed: renderer_restart_needed(
                self.session.prefs.perf_antialiasing,
                self.session.prefs.perf_vsync,
                self.renderer_boot_aa,
                self.renderer_boot_vsync,
            ),
            perf_reload_queued: self.renderer_reload_queued,
            start_screen_inactive_secs: self.session.prefs.start_screen_inactive_secs,
            start_screen_sleep_secs: self.session.prefs.start_screen_sleep_secs,
            start_screen_inactive_dim_percent: self.session.prefs.start_screen_inactive_dim_percent,
            start_screen_sounds_enabled: self.session.prefs.start_screen_sounds_enabled,
            start_screen_sound_volume: self.session.prefs.start_screen_sound_volume,
            start_screen_haptics_enabled: self.session.prefs.start_screen_haptics_enabled,
            start_screen_haptics_strength: self.session.prefs.start_screen_haptics_strength,
            extra_steam_paths: self.session.prefs.extra_steam_paths.clone(),
            steam_path_error: self.configure_state.steam_path_error.clone(),
            #[cfg(windows)]
            autostart: Host::is_enabled(),
        }
    }

    pub(crate) fn analytics_panel(&self) -> AnalyticsPanel {
        if !self.session.prefs.analytics_enabled {
            return AnalyticsPanel::default();
        }
        let rows = self
            .session
            .analytics
            .panel_rows()
            .iter()
            .map(|row| {
                let label = self
                    .session
                    .known
                    .nickname(&row.serial)
                    .map(str::to_string)
                    .filter(|name| !name.is_empty())
                    .or_else(|| {
                        self.session
                            .controllers
                            .iter()
                            .find(|c| c.serial.as_ref() == row.serial.as_str())
                            .map(|c| c.product.to_string())
                    })
                    .unwrap_or_else(|| {
                        let serial = &row.serial;
                        if serial.len() > 8 {
                            format!("Pad …{}", &serial[serial.len() - 6..])
                        } else {
                            serial.clone()
                        }
                    });
                AnalyticsPadRow::from_analytics(row, label)
            })
            .collect();
        AnalyticsPanel { rows }
    }

    pub(crate) fn refresh_analytics_panel(&mut self) {
        self.analytics_panel = self.analytics_panel();
    }

    pub(crate) fn open_configure(&mut self) -> Task<Message> {
        if let Some(id) = self.configure_window {
            return focus_window(id);
        }

        self.configure_state
            .set_spectrum(self.session.prefs.spectrum.clone());

        let (id, open) = window::open(window::Settings {
            size: Size::new(configure_view::WIDTH, configure_view::HEIGHT),
            position: window::Position::Centered,
            visible: open_visible(),
            transparent: crate::ui::chrome::transparent_windows(),
            resizable: false,
            decorations: false,
            // AlwaysOnTop like Start: a Normal Settings window loses presents to the
            // toast on Windows even when the toast is demoted (iced#3320).
            level: window::Level::AlwaysOnTop,
            exit_on_close_request: true,
            platform_specific: window_platform_specific(),
            ..window::Settings::default()
        });

        self.configure_window = Some(id);
        open.map(Message::ConfigureOpened)
            .chain(self.sync_toast_zorder())
    }

    /// Debug-only standalone ambient preview (Diagnostics "View immersive"):
    /// immersive-rate background in its own window, no Start required.
    #[cfg(debug_assertions)]
    pub(crate) fn open_ambient_preview(&mut self) -> Task<Message> {
        if let Some(id) = self.preview_window {
            return focus_window(id);
        }

        let (id, open) = window::open(window::Settings {
            size: Size::new(
                crate::ui::ambient_preview::WIDTH,
                crate::ui::ambient_preview::HEIGHT,
            ),
            position: window::Position::Centered,
            visible: open_visible(),
            transparent: crate::ui::chrome::transparent_windows(),
            resizable: false,
            decorations: false,
            // AlwaysOnTop like Settings so it stays beside Settings while tuning.
            level: window::Level::AlwaysOnTop,
            exit_on_close_request: true,
            platform_specific: window_platform_specific(),
            ..window::Settings::default()
        });

        self.preview_window = Some(id);
        open.map(Message::AmbientPreviewOpened)
            .chain(self.sync_toast_zorder())
    }

    pub(crate) fn on_configure_message(&mut self, message: ConfigureMessage) -> Task<Message> {
        match message {
            ConfigureMessage::SelectSection(section) => {
                self.configure_state.select_section(section);
                let scroll_top = operation::scroll_to(
                    configure_view::content_scroll_id(),
                    operation::AbsoluteOffset {
                        x: None,
                        y: Some(0.0),
                    },
                );
                if section == Section::PadInput {
                    match self.input_hub.read() {
                        start_input::NavReadingsOutcome::Readings { readings, .. } => {
                            self.apply_pad_input_panel_from_readings(&readings);
                        }
                        start_input::NavReadingsOutcome::Missing { .. } => {
                            self.apply_pad_input_panel_from_readings(&[]);
                        }
                    }
                    return scroll_top;
                }
                scroll_top
            }
            ConfigureMessage::ContentScrolled(y) => {
                self.configure_state.content_scroll_y = y;
                Task::none()
            }
            ConfigureMessage::SetNotification(setting, enabled) => {
                match setting {
                    NotificationSetting::Connect => self.session.prefs.notify_connect = enabled,
                    NotificationSetting::Disconnect => {
                        self.session.prefs.notify_disconnect = enabled
                    }
                    NotificationSetting::Low => self.session.prefs.notify_low = enabled,
                    NotificationSetting::Charged => self.session.prefs.notify_charged = enabled,
                }
                self.save_prefs();
                Task::none()
            }
            ConfigureMessage::SetLowBatteryPercent(percent) => {
                let percent = clamp_low_battery_percent(percent);
                if self.session.prefs.low_battery_percent != percent {
                    self.session.prefs.low_battery_percent = percent;
                    self.save_prefs();
                    self.sync_low_battery();
                    return self.sync_popup_rows_and_fit();
                }
                Task::none()
            }
            ConfigureMessage::SetToastPosition(position) => {
                self.session.prefs.toast_position = position;
                self.save_prefs();
                self.show_position_preview()
            }
            ConfigureMessage::SetAnalyticsEnabled(enabled) => {
                self.session.prefs.analytics_enabled = enabled;
                self.save_prefs();
                self.refresh_analytics_panel();
                self.sync_popup_rows_and_fit()
            }
            ConfigureMessage::SetLightbarEnabled(enabled) => {
                self.session.prefs.lightbar_enabled = enabled;
                self.save_prefs();
                self.claims.set_enabled(enabled);
                self.sync_low_battery();
                if enabled {
                    // Re-apply spectrum colors now that automatic writes are back on.
                    self.apply_spectrum(self.session.prefs.spectrum.clone())
                } else {
                    Task::none()
                }
            }
            ConfigureMessage::SetStartScreenEnabled(enabled) => {
                self.session.prefs.start_screen_enabled = enabled;
                self.save_prefs();
                if !enabled && self.start_visible {
                    return self.close_start_screen();
                }
                Task::none()
            }
            ConfigureMessage::SetStartScreenAutoOpen(enabled) => {
                self.apply_start_screen_auto_open(enabled)
            }
            ConfigureMessage::SetStartScreenAlwaysImmersive(enabled) => {
                self.apply_start_screen_always_immersive(enabled)
            }
            ConfigureMessage::SetStartScreenClock(enabled) => {
                self.apply_start_screen_clock(enabled)
            }
            ConfigureMessage::SetImmersiveLayout(layout) => {
                self.apply_start_screen_immersive_layout(layout)
            }
            ConfigureMessage::SetNavbarVisibility(navbar) => self.apply_start_screen_navbar(navbar),
            ConfigureMessage::SetReducedMotion(mode) => {
                self.apply_start_screen_reduced_motion(mode)
            }
            ConfigureMessage::SetPerfThrottleImmersive(enabled) => {
                self.apply_perf_throttle_immersive(enabled)
            }
            ConfigureMessage::SetPerfAntialiasing(enabled) => self.apply_perf_antialiasing(enabled),
            ConfigureMessage::SetPerfVsync(enabled) => self.apply_perf_vsync(enabled),
            ConfigureMessage::ReloadUi => self.reload_ui_for_renderer(),
            ConfigureMessage::SetStartScreenInactiveSecs(secs) => {
                self.apply_start_screen_inactive_secs(secs)
            }
            ConfigureMessage::SetStartScreenSleepSecs(secs) => {
                self.apply_start_screen_sleep_secs(secs)
            }
            ConfigureMessage::SetStartScreenInactiveDimPercent(percent) => {
                self.apply_start_screen_inactive_dim_percent(percent)
            }
            ConfigureMessage::SetStartScreenSounds(enabled) => {
                self.apply_start_screen_sounds(enabled)
            }
            ConfigureMessage::SetStartScreenSoundVolume(volume) => {
                self.apply_start_screen_sound_volume(volume)
            }
            ConfigureMessage::SetStartScreenHaptics(enabled) => {
                self.apply_start_screen_haptics(enabled)
            }
            ConfigureMessage::SetStartScreenHapticsStrength(strength) => {
                self.apply_start_screen_haptics_strength(strength)
            }
            ConfigureMessage::OpenDataFolder => {
                if let Err(err) = paths::open_data_folder() {
                    app_log::warn(format!("open data folder failed: {err}"));
                }
                Task::none()
            }
            ConfigureMessage::OpenChangelog => {
                self.configure_state.show_changelog = true;
                Task::none()
            }
            ConfigureMessage::CloseChangelog => {
                self.configure_state.show_changelog = false;
                Task::none()
            }
            ConfigureMessage::AddSteamPath => Task::perform(
                async move {
                    rfd::AsyncFileDialog::new()
                        .pick_folder()
                        .await
                        .map(|handle| handle.path().to_path_buf())
                },
                Message::SteamPathPicked,
            ),
            ConfigureMessage::RemoveSteamPath(index) => {
                if index < self.session.prefs.extra_steam_paths.len() {
                    self.session.prefs.extra_steam_paths.remove(index);
                    self.save_prefs();
                    self.configure_state.steam_path_error = None;
                    return self.refresh_steam_library();
                }
                Task::none()
            }
            #[cfg(debug_assertions)]
            ConfigureMessage::RunWindowStress => {
                if self.stress_running {
                    return Task::none();
                }
                self.stress_running = true;
                app_log::info("stress: window stress test started from Diagnostics");
                Task::done(Message::StressWindows(0))
            }
            #[cfg(debug_assertions)]
            ConfigureMessage::SetDebugLayoutBounds(enabled) => {
                // Local overlay only: deliberately no save_prefs / disk I/O.
                self.debug_layout_bounds = enabled;
                Task::none()
            }
            #[cfg(debug_assertions)]
            ConfigureMessage::SetDebugAmbientFreq(v) => {
                self.debug_ambient_freq = v.clamp(0.0, 5.0);
                crate::ui::shader::ambient::set_pulse_freq(self.debug_ambient_freq);
                self.start_state.ambient_tuning_keepalive = self.debug_tuning_active();
                Task::none()
            }
            #[cfg(debug_assertions)]
            ConfigureMessage::SetDebugAmbientVeilFreq(v) => {
                self.debug_ambient_veil_freq = v.clamp(0.0, 8.0);
                crate::ui::shader::ambient::set_veil_freq(self.debug_ambient_veil_freq);
                self.start_state.ambient_tuning_keepalive = self.debug_tuning_active();
                Task::none()
            }
            #[cfg(debug_assertions)]
            ConfigureMessage::SetDebugAmbientPulseMin(v) => {
                self.debug_ambient_pulse_min = v.clamp(0.0, 0.6);
                crate::ui::shader::ambient::set_pulse_min(self.debug_ambient_pulse_min);
                self.start_state.ambient_tuning_keepalive = self.debug_tuning_active();
                Task::none()
            }
            #[cfg(debug_assertions)]
            ConfigureMessage::SetDebugAmbientPulseMax(v) => {
                self.debug_ambient_pulse_max = v.clamp(0.0, 0.6);
                crate::ui::shader::ambient::set_pulse_max(self.debug_ambient_pulse_max);
                self.start_state.ambient_tuning_keepalive = self.debug_tuning_active();
                Task::none()
            }
            #[cfg(debug_assertions)]
            ConfigureMessage::SetDebugAmbientOrbSpeed(v) => {
                self.debug_ambient_orb_speed = v.clamp(0.0, 3.0);
                crate::ui::shader::ambient::set_orb_speed(self.debug_ambient_orb_speed);
                self.start_state.ambient_tuning_keepalive = self.debug_tuning_active();
                Task::none()
            }
            #[cfg(debug_assertions)]
            ConfigureMessage::SetDebugAmbientGlowHue(v) => {
                self.debug_ambient_glow_hue = v.clamp(0.0, 360.0);
                crate::ui::shader::ambient::set_glow_hue(self.debug_ambient_glow_hue);
                self.start_state.ambient_tuning_keepalive = self.debug_tuning_active();
                Task::none()
            }
            #[cfg(debug_assertions)]
            ConfigureMessage::SetDebugAmbientOrbMinX(v) => {
                self.debug_ambient_orb_min_x = v.clamp(-1.0, 1.0);
                crate::ui::shader::ambient::set_orb_min_x(self.debug_ambient_orb_min_x);
                self.start_state.ambient_tuning_keepalive = self.debug_tuning_active();
                Task::none()
            }
            #[cfg(debug_assertions)]
            ConfigureMessage::SetDebugAmbientOrbMaxX(v) => {
                self.debug_ambient_orb_max_x = v.clamp(-1.0, 1.0);
                crate::ui::shader::ambient::set_orb_max_x(self.debug_ambient_orb_max_x);
                self.start_state.ambient_tuning_keepalive = self.debug_tuning_active();
                Task::none()
            }
            #[cfg(debug_assertions)]
            ConfigureMessage::SetDebugAmbientOrbMinY(v) => {
                self.debug_ambient_orb_min_y = v.clamp(-1.0, 1.0);
                crate::ui::shader::ambient::set_orb_min_y(self.debug_ambient_orb_min_y);
                self.start_state.ambient_tuning_keepalive = self.debug_tuning_active();
                Task::none()
            }
            #[cfg(debug_assertions)]
            ConfigureMessage::SetDebugAmbientOrbMaxY(v) => {
                self.debug_ambient_orb_max_y = v.clamp(-1.0, 1.0);
                crate::ui::shader::ambient::set_orb_max_y(self.debug_ambient_orb_max_y);
                self.start_state.ambient_tuning_keepalive = self.debug_tuning_active();
                Task::none()
            }
            #[cfg(debug_assertions)]
            ConfigureMessage::SetDebugAmbientGrain(v) => {
                self.debug_ambient_grain = v.clamp(0.0, 5.0);
                crate::ui::shader::ambient::set_grain_gain(self.debug_ambient_grain);
                self.start_state.ambient_tuning_keepalive = self.debug_tuning_active();
                Task::none()
            }
            #[cfg(debug_assertions)]
            ConfigureMessage::SetDebugAmbientTimeScale(v) => {
                self.debug_ambient_time_scale = v.clamp(0.0, 4.0);
                crate::ui::shader::ambient::set_time_scale(self.debug_ambient_time_scale);
                self.start_state.ambient_tuning_keepalive = self.debug_tuning_active();
                Task::none()
            }
            #[cfg(debug_assertions)]
            ConfigureMessage::SetDebugKenburnsAmp(v) => {
                self.debug_kenburns_amp = v.clamp(0.0, 5.0);
                crate::ui::start::mode::set_kenburns_amp_scale(self.debug_kenburns_amp);
                self.start_state.ambient_tuning_keepalive = self.debug_tuning_active();
                Task::none()
            }
            #[cfg(debug_assertions)]
            ConfigureMessage::SetDebugKenburnsPan(v) => {
                self.debug_kenburns_pan = v.clamp(0.0, 5.0);
                crate::ui::start::mode::set_kenburns_pan_scale(self.debug_kenburns_pan);
                self.start_state.ambient_tuning_keepalive = self.debug_tuning_active();
                Task::none()
            }
            #[cfg(debug_assertions)]
            ConfigureMessage::SetDebugKenburnsSpeed(v) => {
                self.debug_kenburns_speed = v.clamp(0.0, 8.0);
                crate::ui::start::mode::set_kenburns_speed(self.debug_kenburns_speed);
                self.start_state.ambient_tuning_keepalive = self.debug_tuning_active();
                Task::none()
            }
            #[cfg(debug_assertions)]
            ConfigureMessage::ResetDebugAmbientTuning => {
                self.reset_debug_tuning();
                Task::none()
            }
            #[cfg(debug_assertions)]
            ConfigureMessage::ExportDebugAmbientTune => {
                let json = self.debug_tune_snapshot().export_json();
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: ambient tune export {json}"
                ));
                iced::clipboard::write(json)
            }
            #[cfg(debug_assertions)]
            ConfigureMessage::OpenAmbientPreview => self.open_ambient_preview(),
            ConfigureMessage::OpenExternalLink(url) => {
                if let Err(err) = launch::launch_target(url, "") {
                    app_log::warn(format!("open external link failed: {err}"));
                }
                Task::none()
            }
            #[cfg(windows)]
            ConfigureMessage::SetAutostart(enabled) => {
                if let Err(err) = Host::set_enabled(enabled) {
                    app_log::error(format!("autostart toggle failed: {err}"));
                }
                Task::none()
            }
            ConfigureMessage::SelectStop(index) => {
                self.configure_state.select(index);
                Task::none()
            }
            ConfigureMessage::MoveStop(percent) => {
                let next = self.configure_state.move_stop(percent);
                self.apply_spectrum_maybe(next)
            }
            ConfigureMessage::AddStopAt(percent) => {
                let next = self.configure_state.add_stop_at(percent);
                self.apply_spectrum_maybe(next)
            }
            ConfigureMessage::RemoveStop => {
                let next = self.configure_state.remove_selected();
                self.apply_spectrum_maybe(next)
            }
            ConfigureMessage::RemoveStopAt(index) => {
                let next = self.configure_state.remove_at(index);
                self.apply_spectrum_maybe(next)
            }
            ConfigureMessage::HueChanged(hue) => {
                let next = self.configure_state.set_hue(hue);
                self.apply_spectrum_maybe(next)
            }
            ConfigureMessage::SaturationValueChanged(saturation, value) => {
                let next = self.configure_state.set_saturation_value(saturation, value);
                self.apply_spectrum_maybe(next)
            }
            ConfigureMessage::ResetSpectrum => {
                let next = self.configure_state.reset();
                self.apply_spectrum(next)
            }
            #[cfg(debug_assertions)]
            ConfigureMessage::Emulator(command) => self.apply_emulator_command(command),
            #[cfg(debug_assertions)]
            ConfigureMessage::ToggleEmulator(serial) => {
                if !self.configure_state.expanded_emulators.remove(&serial) {
                    self.configure_state.expanded_emulators.insert(serial);
                }
                Task::none()
            }
            // Hide first so DWM cannot flash the default (white) brush while the
            // wgpu surface is torn down; keep id until WindowClosed.
            ConfigureMessage::Close => match self.configure_window {
                Some(id) => window::set_mode(id, window::Mode::Hidden).chain(window::close(id)),
                None => Task::none(),
            },
            ConfigureMessage::DragWindow => match self.configure_window {
                Some(id) => window::drag(id),
                None => Task::none(),
            },
        }
    }

    /// True while any debug ambient/ken-burns tuning differs from shipped
    /// behavior (drives the Start keep-alive so tuning repaints under idle
    /// throttle). Local only.
    #[cfg(debug_assertions)]
    pub(crate) fn debug_tuning_active(&self) -> bool {
        let defaults = crate::ui::configure::DebugAmbientTune::default();
        self.debug_ambient_freq != defaults.pulse_freq
            || self.debug_ambient_veil_freq != defaults.veil_freq
            || self.debug_ambient_pulse_min != defaults.pulse_min
            || self.debug_ambient_pulse_max != defaults.pulse_max
            || self.debug_ambient_orb_speed != defaults.orb_speed
            || self.debug_ambient_glow_hue != defaults.glow_hue
            || self.debug_ambient_orb_min_x != defaults.orb_min_x
            || self.debug_ambient_orb_max_x != defaults.orb_max_x
            || self.debug_ambient_orb_min_y != defaults.orb_min_y
            || self.debug_ambient_orb_max_y != defaults.orb_max_y
            || self.debug_ambient_grain != defaults.grain_gain
            || self.debug_ambient_time_scale != defaults.time_scale
            || self.debug_kenburns_amp != defaults.kenburns_amp
            || self.debug_kenburns_pan != defaults.kenburns_pan
            || self.debug_kenburns_speed != defaults.kenburns_speed
    }

    /// Current Diagnostics tuning snapshot (single construction site for the
    /// Settings view and the JSON export).
    #[cfg(debug_assertions)]
    pub(crate) fn debug_tune_snapshot(&self) -> crate::ui::configure::DebugAmbientTune {
        crate::ui::configure::DebugAmbientTune {
            pulse_freq: self.debug_ambient_freq,
            veil_freq: self.debug_ambient_veil_freq,
            pulse_min: self.debug_ambient_pulse_min,
            pulse_max: self.debug_ambient_pulse_max,
            orb_speed: self.debug_ambient_orb_speed,
            glow_hue: self.debug_ambient_glow_hue,
            orb_min_x: self.debug_ambient_orb_min_x,
            orb_max_x: self.debug_ambient_orb_max_x,
            orb_min_y: self.debug_ambient_orb_min_y,
            orb_max_y: self.debug_ambient_orb_max_y,
            grain_gain: self.debug_ambient_grain,
            time_scale: self.debug_ambient_time_scale,
            kenburns_amp: self.debug_kenburns_amp,
            kenburns_pan: self.debug_kenburns_pan,
            kenburns_speed: self.debug_kenburns_speed,
        }
    }

    /// Restore shipped ambient + ken-burns tuning (Diagnostics Reset and the
    /// in-immersive tuning panel share this; never persisted).
    #[cfg(debug_assertions)]
    pub(crate) fn reset_debug_tuning(&mut self) {
        let defaults = crate::ui::configure::DebugAmbientTune::default();
        self.debug_ambient_freq = defaults.pulse_freq;
        self.debug_ambient_veil_freq = defaults.veil_freq;
        self.debug_ambient_pulse_min = defaults.pulse_min;
        self.debug_ambient_pulse_max = defaults.pulse_max;
        self.debug_ambient_orb_speed = defaults.orb_speed;
        self.debug_ambient_glow_hue = defaults.glow_hue;
        self.debug_ambient_orb_min_x = defaults.orb_min_x;
        self.debug_ambient_orb_max_x = defaults.orb_max_x;
        self.debug_ambient_orb_min_y = defaults.orb_min_y;
        self.debug_ambient_orb_max_y = defaults.orb_max_y;
        self.debug_ambient_grain = defaults.grain_gain;
        self.debug_ambient_time_scale = defaults.time_scale;
        self.debug_kenburns_amp = defaults.kenburns_amp;
        self.debug_kenburns_pan = defaults.kenburns_pan;
        self.debug_kenburns_speed = defaults.kenburns_speed;
        crate::ui::shader::ambient::reset_ambient_tuning();
        crate::ui::start::mode::reset_kenburns_tuning();
        self.start_state.ambient_tuning_keepalive = false;
    }

    /// Apply one absolute tuning commit from the in-immersive tuning panel
    /// (pad nudge or mouse drag): mirror into the shell fields (drives the
    /// keep-alive + Settings sliders) and the process-wide tuning atomics
    /// (drives the shader). Ranges match the Diagnostics sliders; the
    /// setters clamp defensively.
    #[cfg(debug_assertions)]
    pub(crate) fn apply_ambient_tune(
        &mut self,
        param: crate::ui::start::ambient_tune::AmbientTuneParam,
        value: f32,
    ) -> Task<Message> {
        use crate::ui::start::ambient_tune::AmbientTuneParam as P;
        match param {
            P::PulseFreq => {
                self.debug_ambient_freq = value.clamp(0.0, 5.0);
                crate::ui::shader::ambient::set_pulse_freq(self.debug_ambient_freq);
            }
            P::PulseMin => {
                self.debug_ambient_pulse_min = value.clamp(0.0, 0.6);
                crate::ui::shader::ambient::set_pulse_min(self.debug_ambient_pulse_min);
            }
            P::PulseMax => {
                self.debug_ambient_pulse_max = value.clamp(0.0, 0.6);
                crate::ui::shader::ambient::set_pulse_max(self.debug_ambient_pulse_max);
            }
            P::GlowHue => {
                self.debug_ambient_glow_hue = value.clamp(0.0, 360.0);
                crate::ui::shader::ambient::set_glow_hue(self.debug_ambient_glow_hue);
            }
            P::OrbSpeed => {
                self.debug_ambient_orb_speed = value.clamp(0.0, 3.0);
                crate::ui::shader::ambient::set_orb_speed(self.debug_ambient_orb_speed);
            }
            P::OrbMinX => {
                self.debug_ambient_orb_min_x = value.clamp(-1.0, 1.0);
                crate::ui::shader::ambient::set_orb_min_x(self.debug_ambient_orb_min_x);
            }
            P::OrbMaxX => {
                self.debug_ambient_orb_max_x = value.clamp(-1.0, 1.0);
                crate::ui::shader::ambient::set_orb_max_x(self.debug_ambient_orb_max_x);
            }
            P::OrbMinY => {
                self.debug_ambient_orb_min_y = value.clamp(-1.0, 1.0);
                crate::ui::shader::ambient::set_orb_min_y(self.debug_ambient_orb_min_y);
            }
            P::OrbMaxY => {
                self.debug_ambient_orb_max_y = value.clamp(-1.0, 1.0);
                crate::ui::shader::ambient::set_orb_max_y(self.debug_ambient_orb_max_y);
            }
            P::VeilFreq => {
                self.debug_ambient_veil_freq = value.clamp(0.0, 8.0);
                crate::ui::shader::ambient::set_veil_freq(self.debug_ambient_veil_freq);
            }
            P::Grain => {
                self.debug_ambient_grain = value.clamp(0.0, 5.0);
                crate::ui::shader::ambient::set_grain_gain(self.debug_ambient_grain);
            }
            P::TimeScale => {
                self.debug_ambient_time_scale = value.clamp(0.0, 4.0);
                crate::ui::shader::ambient::set_time_scale(self.debug_ambient_time_scale);
            }
        }
        self.start_state.ambient_tuning_keepalive = self.debug_tuning_active();
        Task::none()
    }

    /// Open the in-immersive tuning panel: drawer closes, panel resets.
    #[cfg(debug_assertions)]
    pub(crate) fn open_ambient_tuning(&mut self) -> Task<Message> {
        self.ambient_r3_latched = false;
        self.start_state.request_ambient_tuning(true);
        self.start_state.request_settings(false, Instant::now());
        crate::controller::hid::diag::diag_info("ui-diag: start ambient tuning open");
        Task::none()
    }

    /// Close the tuning panel; `reopen_drawer` returns to the drawer.
    #[cfg(debug_assertions)]
    pub(crate) fn close_ambient_tuning(&mut self, reopen_drawer: bool) -> Task<Message> {
        self.ambient_r3_latched = false;
        self.start_state.request_ambient_tuning(false);
        if reopen_drawer {
            self.start_state.request_settings(true, Instant::now());
        }
        crate::controller::hid::diag::diag_info("ui-diag: start ambient tuning close");
        Task::none()
    }

    /// R3 rising edge while tuning: toggle all on-screen elements (panel
    /// included) for a clean preview. Release-latched so holding R3 cannot
    /// strobe. Pad-only (R3 has no NavAction); tuning-scoped.
    #[cfg(debug_assertions)]
    pub(crate) fn toggle_ambient_chrome_from_r3(
        &mut self,
        readings: &[start_input::NavReading],
    ) -> Option<Task<Message>> {
        if !self.start_visible
            || !self.start_state.immersive
            || !self.start_state.ambient_tuning_active()
        {
            return None;
        }
        let held = readings
            .iter()
            .any(|r| r.sample.held.contains(&gesture::GestureControl::R3));
        if held && !self.ambient_r3_latched {
            self.ambient_r3_latched = true;
            self.start_state.ambient_tune_hide_chrome = !self.start_state.ambient_tune_hide_chrome;
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: start ambient chrome hidden={}",
                self.start_state.ambient_tune_hide_chrome as u8
            ));
            self.play_start_cue(UiSoundKind::Nav);
            return Some(Task::none());
        }
        if !held {
            self.ambient_r3_latched = false;
        }
        None
    }

    /// Scroll the tuning list to keep the focused row in view.
    #[cfg(debug_assertions)]
    pub(crate) fn scroll_ambient_tune_into_view(&self) -> Task<Message> {
        use crate::ui::start::ambient_tune::{TuneRow, tune_reveal_y};
        let focus = self
            .start_state
            .ambient_tune_focus
            .min(TuneRow::count().saturating_sub(1));
        let Some(y) = tune_reveal_y(
            focus,
            self.start_state.ambient_tune_scroll_y,
            self.start_state.ambient_tune_viewport_h,
        ) else {
            return Task::none();
        };
        operation::scroll_to(
            crate::ui::start::ambient_tune::tune_scroll_id(),
            operation::AbsoluteOffset {
                x: None,
                y: Some(y),
            },
        )
    }

    /// Pad/keyboard nav while the tuning panel is open. Mirrors the settings
    /// drawer dispatch: Up/Down move focus, Left/Right nudge sliders, Cross
    /// confirms buttons, Circle closes back to the drawer. Release stub:
    /// the panel cannot open there.
    pub(crate) fn on_ambient_tuning_nav(&mut self, action: Option<NavAction>) -> Task<Message> {
        #[cfg(debug_assertions)]
        {
            use crate::ui::start::ambient_tune::TuneRow;
            let Some(action) = action else {
                return Task::none();
            };
            match action {
                NavAction::Up => {
                    if self.start_state.move_ambient_tune_focus(-1) {
                        self.play_start_cue(UiSoundKind::Nav);
                        return self.scroll_ambient_tune_into_view();
                    }
                    Task::none()
                }
                NavAction::Down => {
                    if self.start_state.move_ambient_tune_focus(1) {
                        self.play_start_cue(UiSoundKind::Nav);
                        return self.scroll_ambient_tune_into_view();
                    }
                    Task::none()
                }
                NavAction::PrevSlide | NavAction::NextSlide => {
                    let delta = if matches!(action, NavAction::PrevSlide) {
                        -1
                    } else {
                        1
                    };
                    let focus = self
                        .start_state
                        .ambient_tune_focus
                        .min(TuneRow::count().saturating_sub(1));
                    if let TuneRow::Param(param) = TuneRow::ALL[focus]
                        && let Some(next) = param.nudge(delta)
                    {
                        self.play_start_cue(UiSoundKind::Nav);
                        return self.apply_ambient_tune(param, next);
                    }
                    Task::none()
                }
                NavAction::Confirm => {
                    let focus = self
                        .start_state
                        .ambient_tune_focus
                        .min(TuneRow::count().saturating_sub(1));
                    match TuneRow::ALL[focus] {
                        TuneRow::Reset => {
                            self.play_start_cue(UiSoundKind::Action);
                            self.reset_debug_tuning();
                            Task::none()
                        }
                        TuneRow::Done => {
                            self.play_start_cue(UiSoundKind::Action);
                            self.close_ambient_tuning(true)
                        }
                        TuneRow::Param(_) => Task::none(),
                    }
                }
                NavAction::Cancel => self.close_ambient_tuning(true),
                NavAction::ToggleSettings => self.close_ambient_tuning(false),
                _ => Task::none(),
            }
        }
        #[cfg(not(debug_assertions))]
        {
            let _ = action;
            Task::none()
        }
    }

    pub(crate) fn apply_spectrum_maybe(
        &mut self,
        spectrum: Option<BatterySpectrum>,
    ) -> Task<Message> {
        if let Some(spectrum) = spectrum {
            self.apply_spectrum(spectrum)
        } else {
            Task::none()
        }
    }

    /// Update in-memory spectrum immediately; debounce prefs save + HID write.
    pub(crate) fn apply_spectrum(&mut self, spectrum: BatterySpectrum) -> Task<Message> {
        self.session.prefs.spectrum = spectrum.clone();
        self.spectrum.set(spectrum);
        self.spectrum_generation = self.spectrum_generation.wrapping_add(1);
        let generation = self.spectrum_generation;
        Task::perform(delay(SPECTRUM_DEBOUNCE), move |_| {
            Message::SpectrumCommit(generation)
        })
    }

    pub(crate) fn on_spectrum_commit(&mut self, generation: u64) -> Task<Message> {
        if self.spectrum_generation != generation {
            return Task::none();
        }

        self.save_prefs();

        if !self.claims.is_enabled() {
            return Task::none();
        }

        let targets: Vec<(String, color::Rgb)> = self
            .session
            .controllers
            .iter()
            .filter(|c| !is_emulated_serial(c.serial.as_ref()))
            .map(|c| (c.serial.to_string(), self.spectrum.color_at(c.percent)))
            .collect();

        if targets.is_empty() {
            return Task::none();
        }

        for (serial, color) in targets {
            self.hid_worker.set_rgb(serial, color);
        }
        Task::none()
    }

    /// Publish previewed battery levels whose debounce has elapsed.
    ///
    /// Advances the spinner every tick and commits once a pad has been quiet for
    /// [`EMULATOR_COMMIT_DEBOUNCE`]. Only subscribed while something is pending.
    /// Committed levels publish through the local session.
    #[cfg(debug_assertions)]
    pub(crate) fn on_emulator_commit_tick(&mut self) -> Task<Message> {
        if self.pending_emulator_commits.is_empty() {
            return Task::none();
        }
        let now = Instant::now();
        // Advance spinner frames even before anything is due.
        for pending in self.pending_emulator_commits.values_mut() {
            pending.tick = pending.tick.wrapping_add(1);
        }
        let due: Vec<(String, u8)> = self
            .pending_emulator_commits
            .iter()
            .filter(|(_, p)| now >= p.deadline)
            .map(|(serial, p)| (serial.clone(), p.percent))
            .collect();
        if due.is_empty() {
            return Task::none();
        }
        for (serial, _) in &due {
            self.pending_emulator_commits.remove(serial);
        }
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: emulator debounce committed {}",
            due.len()
        ));
        self.last_battery_poll = Instant::now();
        self.apply_controllers(self.live_controllers())
    }

    /// The emulated pad whose battery level is waiting out its debounce.
    ///
    /// Only the pad the Controllers tab has selected can show its spinner, so this
    /// resolves the selection to a concrete change. Percent comes from the
    /// preview overlay, committed on debounce (see `PreviewPercent` below).
    #[cfg(debug_assertions)]
    pub(crate) fn pending_emulator_changes(&self) -> Vec<configure_view::PendingEmulatorChange> {
        self.emulated_pads
            .iter()
            .filter_map(|pad| {
                let pending = self.pending_emulator_commits.get(pad.serial())?;
                Some(configure_view::PendingEmulatorChange {
                    serial: pad.serial().to_string(),
                    percent: pending.percent,
                    tick: pending.tick,
                })
            })
            .collect()
    }

    /// True while any emulated pad is plugged in — HID polling stands down so
    /// the emulator is the only source of live controllers.
    #[cfg(debug_assertions)]
    pub(crate) fn emulating(&self) -> bool {
        self.emulated_pads.iter().any(|p| p.connected)
    }

    /// Live snapshot = real hardware from the session + connected emulated pads.
    #[cfg(debug_assertions)]
    pub(crate) fn live_controllers(&self) -> Vec<ControllerStatus> {
        let mut live: Vec<ControllerStatus> = self
            .session
            .controllers
            .iter()
            .filter(|c| !emulate::is_emulated(c.serial.as_ref()))
            .cloned()
            .collect();
        live.extend(emulate::live(&self.emulated_pads));
        live
    }

    #[cfg(debug_assertions)]
    pub(crate) fn apply_emulator_command(&mut self, command: EmulatorCommand) -> Task<Message> {
        let fleet = self.emulated_pads.clone();
        let find = |serial: &str| fleet.iter().find(|p| p.serial() == serial).cloned();

        // `publish` runs the normal connect/disconnect path (toasts, Start-open
        // policy, analytics edges); `analytics` covers the charge/drain walk.
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
                self.session.known.forget(serial);
                self.pending_emulator_commits.remove(serial);
                (next, true, false)
            }
            // Preview repaints the tab only. Publishing is debounced so a drag
            // cannot spam toasts or Start opens — but only for a *connected*
            // pad, since a disconnected one is not published at all.
            EmulatorCommand::PreviewPercent(serial, percent) => {
                let next = emulate::set_percent(&fleet, serial, *percent);
                let connected = find(serial).is_some_and(|p| p.connected);
                if connected {
                    self.pending_emulator_commits.insert(
                        serial.clone(),
                        PendingEmulatorCommit {
                            deadline: Instant::now() + EMULATOR_COMMIT_DEBOUNCE,
                            tick: 0,
                            percent: *percent,
                        },
                    );
                }
                (next, false, false)
            }
            // Plugging in or unplugging is a charge edge, so it credits
            // analytics the same way stepping the cycle does.
            EmulatorCommand::SetConnection(serial, connection) => {
                (emulate::set_link(&fleet, serial, *connection), true, true)
            }
            // Plugging in or unplugging is a real connect/disconnect edge.
            EmulatorCommand::SetConnected(serial, connected) => {
                let next = emulate::update(&fleet, serial, |p| p.connected = *connected);
                if !connected {
                    // Nothing left to publish to; drop the armed timer so the
                    // commit cannot fire against a pad that is no longer live.
                    self.pending_emulator_commits.remove(serial);
                }
                // Unplugging a forgotten pad removes it — same as forgetting an
                // unplugged pad. Either way the two together mean "gone".
                (emulate::prune(next), true, false)
            }
            // The popup's remember pin, for an emulated pad. Only takes effect
            // on unplug; `prune` then removes it if it is already off.
            //
            // Publishes: forgetting removes the pad from the remembered list
            // the popup draws from, so those rows must be rebuilt or the pad
            // lingers there until something else triggers a sync.
            EmulatorCommand::SetRemembered(serial, remembered) => {
                let next = emulate::update(&fleet, serial, |p| p.remembered = *remembered);
                (emulate::prune(next), true, false)
            }
            EmulatorCommand::SeedAnalytics(serial) => {
                self.session.analytics.dev_seed_estimates(serial);
                (fleet, false, true)
            }
            EmulatorCommand::StepCharge(serial) => {
                let Some(pad) = find(serial) else {
                    return Task::none();
                };
                self.session
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
                    return Task::none();
                };
                self.session
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

        // `previous` is captured before the swap so `remember_emulated` can
        // see pads that `prune` dropped.
        let previous = std::mem::take(&mut self.emulated_pads);
        self.emulated_pads = next_fleet;
        if emulate::sync_remembered_emulated(
            &previous,
            &self.emulated_pads,
            &mut self.session.known,
        ) {
            self.session.known.save();
        }

        // Analytics state only means anything once it is recording.
        if analytics && !self.session.prefs.analytics_enabled {
            self.session.prefs.analytics_enabled = true;
            self.save_prefs();
            app_log::info("analytics enabled for developer emulator");
        }
        if analytics {
            self.session.analytics.save();
            self.refresh_analytics_panel();
        }

        if !publish {
            return Task::none();
        }
        self.last_battery_poll = Instant::now();
        self.apply_controllers(self.live_controllers())
    }
}
