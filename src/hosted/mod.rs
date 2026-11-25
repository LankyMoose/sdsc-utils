//! Hosted app contract v0.
//!
//! Intentionally tiny: manifest + native surface + semantic actions + text
//! session + OS-owned lifecycle. There is no widget/status/toolbar toolkit in
//! v0 — apps provide a specification, not pixels or their own chrome.

use serde::{Deserialize, Serialize};

/// Contract version. Apps declare required capabilities against this.
pub const CONTRACT_VERSION: u32 = 0;

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
    /// App paints a native content surface (CEF, video, game).
    pub native_surface: bool,
    /// App publishes semantic actions and receives `ActionInvoked`.
    pub semantic_actions: bool,
    /// App can own a page text session fed by the system OSK.
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

    #[test]
    fn action_lookup_respects_enabled_state() {
        let enabled = spec(true);
        assert_eq!(enabled.contract, 0);
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
}
