//! Semantic action mapping for hosted apps.
//!
//! Apps declare actions; the OS owns the controller binding table and sends
//! `ActionInvoked { id }`. Browser v0 uses L1/R1/Options for back/forward/
//! reload, tap only.

use super::{App, Message};
use crate::domain::pad::{PadButton, PadButtonAction};
use crate::hosted::{
    ActionInvoked, ActionSpec, AppId, AppSpec, CONTRACT_VERSION, Capabilities, LifecycleSpec,
    Manifest, SurfaceKind, SurfaceSpec, TextSessionState,
};
use iced::Task;

pub const BROWSER_APP: &str = "browser";
pub const ACTION_BACK: &str = "back";
pub const ACTION_FORWARD: &str = "forward";
pub const ACTION_RELOAD: &str = "reload";

/// One OS-owned controller binding. Apps never see the button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionBinding {
    pub button: PadButton,
    pub action: &'static str,
}

/// Browser v0 defaults: L1 back, R1 forward, Options reload. Tap only.
pub const BROWSER_BINDINGS: [ActionBinding; 3] = [
    ActionBinding {
        button: PadButton::L1,
        action: ACTION_BACK,
    },
    ActionBinding {
        button: PadButton::R1,
        action: ACTION_FORWARD,
    },
    ActionBinding {
        button: PadButton::Options,
        action: ACTION_RELOAD,
    },
];

/// Semantic action for a source-side tap, if this button is bound.
pub fn browser_action(button: PadButton) -> Option<&'static str> {
    BROWSER_BINDINGS
        .iter()
        .find(|binding| binding.button == button)
        .map(|binding| binding.action)
}

/// The Browser's published contract v0 specification.
pub fn browser_spec(panel: &crate::ui::browser::BrowserPanel) -> AppSpec {
    AppSpec {
        contract: CONTRACT_VERSION,
        manifest: Manifest {
            id: AppId::new(BROWSER_APP),
            name: "Browser".to_string(),
            icon: None,
            version: crate::platform::app_meta::PKG_VERSION.to_string(),
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
        actions: vec![
            ActionSpec {
                id: ACTION_BACK.to_string(),
                label: "Back".to_string(),
                enabled: panel.can_go_back(),
            },
            ActionSpec {
                id: ACTION_FORWARD.to_string(),
                label: "Forward".to_string(),
                enabled: panel.can_go_forward(),
            },
            ActionSpec {
                id: ACTION_RELOAD.to_string(),
                label: "Reload".to_string(),
                enabled: !panel.session.url().is_empty(),
            },
        ],
        text_session: if panel.page.text_session_active {
            TextSessionState::Active
        } else {
            TextSessionState::Inactive
        },
        lifecycle: LifecycleSpec { os_owned: true },
    }
}

impl App {
    /// OS action-invoked path. Validates the published enabled state before the
    /// app receives the action; the app never knows the button.
    pub(crate) fn invoke_hosted_action(&mut self, invoked: ActionInvoked) -> Task<Message> {
        if invoked.app.as_str() != BROWSER_APP {
            crate::platform::trace::event(crate::platform::trace::Lane::Platform, "app.action")
                .field("app", invoked.app.as_str())
                .field("action", &invoked.action)
                .field("known_app", false)
                .emit();
            return Task::none();
        }
        let spec = browser_spec(&self.browser.panel);
        let enabled = spec.action_enabled(&invoked.action);
        crate::platform::trace::event(crate::platform::trace::Lane::Platform, "app.action")
            .field("app", invoked.app.as_str())
            .field("action", &invoked.action)
            .field("known_app", true)
            .field("enabled", enabled)
            .emit();
        if !enabled {
            return Task::none();
        }
        match invoked.action.as_str() {
            ACTION_BACK => self.on_browser_go_back(),
            ACTION_FORWARD => self.on_browser_go_forward(),
            ACTION_RELOAD => self.on_browser_reload(),
            _ => Task::none(),
        }
    }

    /// Dispatch a source-side tap through the binding table. Returns `None`
    /// when the button is not a semantic action.
    pub(crate) fn hosted_action_for_tap(
        &self,
        actions: &[crate::domain::pad::PadAction],
    ) -> Option<ActionInvoked> {
        actions.iter().find_map(|action| {
            if action.action != PadButtonAction::Tap {
                return None;
            }
            browser_action(action.button).map(|id| ActionInvoked::new(BROWSER_APP, id))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_default_bindings_are_l1_r1_options() {
        assert_eq!(browser_action(PadButton::L1), Some(ACTION_BACK));
        assert_eq!(browser_action(PadButton::R1), Some(ACTION_FORWARD));
        assert_eq!(browser_action(PadButton::Options), Some(ACTION_RELOAD));
        assert_eq!(browser_action(PadButton::Cross), None);
        assert_eq!(browser_action(PadButton::Square), None);
    }

    #[test]
    fn browser_spec_publishes_actions_and_text_session() {
        let mut panel = crate::ui::browser::BrowserPanel::default();
        panel
            .session
            .begin_load("https://example.test/".to_string());
        let spec = browser_spec(&panel);
        assert_eq!(spec.contract, CONTRACT_VERSION);
        assert_eq!(spec.manifest.id.as_str(), BROWSER_APP);
        assert!(spec.manifest.capabilities.native_surface);
        assert!(spec.manifest.capabilities.semantic_actions);
        assert!(spec.manifest.capabilities.text_session);
        assert_eq!(spec.surface.kind, SurfaceKind::Native);
        assert!(spec.action(ACTION_BACK).is_some());
        assert!(spec.action(ACTION_FORWARD).is_some());
        assert!(spec.action_enabled(ACTION_RELOAD));
        assert_eq!(spec.text_session, TextSessionState::Inactive);

        panel.page.text_session_active = true;
        panel.page.last_engine_can = Some((true, false));
        let spec = browser_spec(&panel);
        assert_eq!(spec.text_session, TextSessionState::Active);
        assert!(spec.action_enabled(ACTION_BACK));
        assert!(!spec.action_enabled(ACTION_FORWARD));
    }
}
