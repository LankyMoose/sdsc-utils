//! iced → Servo keyboard mapping for the in-Start browser.
//!
//! Pure functions (unit-tested): the subscription captures raw iced key
//! events, the shell decides page-vs-URL routing, and only mapped events
//! reach the engine as [`keyboard_types::KeyboardEvent`].
//!
//! Both enums model the same USB HID / UI Events vocabulary, so physical
//! codes round-trip by variant name with an `Unidentified` fallback; logical
//! keys map explicitly where the names diverge (notably `Space`, whose UI
//! Events value is `" "`).

use iced::keyboard;

/// Raw iced key event, captured by the subscription and routed in `update`
/// (the subscription cannot see shell state).
#[derive(Debug, Clone)]
pub struct RawKey {
    pub key: keyboard::Key,
    pub physical: keyboard::key::Physical,
    pub location: keyboard::Location,
    pub modifiers: keyboard::Modifiers,
    pub pressed: bool,
    pub repeat: bool,
}

/// Map a raw key into a Servo keyboard event. `pressed` selects
/// down/up; `repeat` passes through for held keys.
pub fn map_event(raw: &RawKey) -> keyboard_types::KeyboardEvent {
    keyboard_types::KeyboardEvent {
        state: if raw.pressed {
            keyboard_types::KeyState::Down
        } else {
            keyboard_types::KeyState::Up
        },
        key: map_key(&raw.key),
        code: map_code(&raw.physical),
        location: map_location(&raw.location),
        modifiers: map_modifiers(&raw.modifiers),
        repeat: raw.repeat,
        is_composing: false,
    }
}

/// Synthetic press+release pair for pad-driven keys (Cross = Enter,
/// Square = Tab). The pad has no physical code or modifiers.
pub fn pad_key(key: keyboard_types::NamedKey) -> [keyboard_types::KeyboardEvent; 2] {
    let base = keyboard_types::KeyboardEvent {
        state: keyboard_types::KeyState::Down,
        key: keyboard_types::Key::Named(key),
        code: keyboard_types::Code::Unidentified,
        location: keyboard_types::Location::Standard,
        modifiers: keyboard_types::Modifiers::empty(),
        repeat: false,
        is_composing: false,
    };
    let mut released = base.clone();
    released.state = keyboard_types::KeyState::Up;
    [base, released]
}

fn map_key(key: &keyboard::Key) -> keyboard_types::Key {
    use iced::keyboard::key::Named as In;
    use keyboard_types::{Key as Out, NamedKey};
    match key {
        keyboard::Key::Character(text) => Out::Character(text.to_string()),
        keyboard::Key::Unidentified => Out::Named(NamedKey::Unidentified),
        keyboard::Key::Named(named) => Out::Named(match named {
            In::Enter => NamedKey::Enter,
            In::Tab => NamedKey::Tab,
            In::Escape => NamedKey::Escape,
            In::Backspace => NamedKey::Backspace,
            In::Delete => NamedKey::Delete,
            In::ArrowUp => NamedKey::ArrowUp,
            In::ArrowDown => NamedKey::ArrowDown,
            In::ArrowLeft => NamedKey::ArrowLeft,
            In::ArrowRight => NamedKey::ArrowRight,
            In::Home => NamedKey::Home,
            In::End => NamedKey::End,
            In::PageUp => NamedKey::PageUp,
            In::PageDown => NamedKey::PageDown,
            // UI Events names the space key `" "`.
            In::Space => return Out::Character(" ".to_string()),
            // Same vocabulary beyond the control set (F-keys, etc.).
            other => format!("{other:?}")
                .parse()
                .unwrap_or(NamedKey::Unidentified),
        }),
    }
}

fn map_code(physical: &keyboard::key::Physical) -> keyboard_types::Code {
    match physical {
        keyboard::key::Physical::Code(code) => format!("{code:?}")
            .parse()
            .unwrap_or(keyboard_types::Code::Unidentified),
        keyboard::key::Physical::Unidentified(_) => keyboard_types::Code::Unidentified,
    }
}

fn map_location(location: &keyboard::Location) -> keyboard_types::Location {
    match location {
        keyboard::Location::Standard => keyboard_types::Location::Standard,
        keyboard::Location::Left => keyboard_types::Location::Left,
        keyboard::Location::Right => keyboard_types::Location::Right,
        keyboard::Location::Numpad => keyboard_types::Location::Numpad,
    }
}

fn map_modifiers(modifiers: &keyboard::Modifiers) -> keyboard_types::Modifiers {
    let mut out = keyboard_types::Modifiers::empty();
    if modifiers.shift() {
        out |= keyboard_types::Modifiers::SHIFT;
    }
    if modifiers.control() {
        out |= keyboard_types::Modifiers::CONTROL;
    }
    if modifiers.alt() {
        out |= keyboard_types::Modifiers::ALT;
    }
    if modifiers.logo() {
        out |= keyboard_types::Modifiers::META;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_key(key: keyboard::Key) -> RawKey {
        RawKey {
            key,
            physical: keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Windows(0)),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            pressed: true,
            repeat: false,
        }
    }

    #[test]
    fn control_keys_map_by_name() {
        use iced::keyboard::key::Named;
        for (named, expected) in [
            (Named::Enter, "Enter"),
            (Named::Tab, "Tab"),
            (Named::Escape, "Escape"),
            (Named::Backspace, "Backspace"),
            (Named::Delete, "Delete"),
            (Named::ArrowUp, "ArrowUp"),
            (Named::Home, "Home"),
            (Named::PageDown, "PageDown"),
        ] {
            let event = map_event(&raw_key(keyboard::Key::Named(named)));
            assert_eq!(event.key.to_string(), expected, "{named:?}");
            assert!(matches!(event.state, keyboard_types::KeyState::Down));
        }
    }

    #[test]
    fn space_maps_to_character_and_characters_pass_through() {
        use iced::keyboard::key::Named;
        let space = map_event(&raw_key(keyboard::Key::Named(Named::Space)));
        assert_eq!(space.key.to_string(), " ");
        let typed = map_event(&raw_key(keyboard::Key::Character("ä".into())));
        assert_eq!(typed.key.to_string(), "ä");
    }

    #[test]
    fn release_maps_to_key_up_with_modifiers() {
        let mut raw = raw_key(keyboard::Key::Character("a".into()));
        raw.pressed = false;
        raw.modifiers = keyboard::Modifiers::SHIFT;
        let event = map_event(&raw);
        assert!(matches!(event.state, keyboard_types::KeyState::Up));
        assert!(event.modifiers.contains(keyboard_types::Modifiers::SHIFT));
    }

    #[test]
    fn pad_key_produces_down_then_up() {
        let [down, up] = pad_key(keyboard_types::NamedKey::Enter);
        assert!(matches!(down.state, keyboard_types::KeyState::Down));
        assert!(matches!(up.state, keyboard_types::KeyState::Up));
        assert_eq!(down.key.to_string(), up.key.to_string());
    }
}
