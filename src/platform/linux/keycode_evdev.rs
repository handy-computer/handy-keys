//! Linux key conversion utilities for evdev key codes.

use crate::types::{Key, Modifiers};
use evdev_rs::enums::EV_KEY;

/// Convert an evdev key code to our Key type.
pub fn evdev_key_to_key(key: &EV_KEY) -> Option<Key> {
    match key {
        EV_KEY::KEY_A => Some(Key::A),
        EV_KEY::KEY_B => Some(Key::B),
        EV_KEY::KEY_C => Some(Key::C),
        EV_KEY::KEY_D => Some(Key::D),
        EV_KEY::KEY_E => Some(Key::E),
        EV_KEY::KEY_F => Some(Key::F),
        EV_KEY::KEY_G => Some(Key::G),
        EV_KEY::KEY_H => Some(Key::H),
        EV_KEY::KEY_I => Some(Key::I),
        EV_KEY::KEY_J => Some(Key::J),
        EV_KEY::KEY_K => Some(Key::K),
        EV_KEY::KEY_L => Some(Key::L),
        EV_KEY::KEY_M => Some(Key::M),
        EV_KEY::KEY_N => Some(Key::N),
        EV_KEY::KEY_O => Some(Key::O),
        EV_KEY::KEY_P => Some(Key::P),
        EV_KEY::KEY_Q => Some(Key::Q),
        EV_KEY::KEY_R => Some(Key::R),
        EV_KEY::KEY_S => Some(Key::S),
        EV_KEY::KEY_T => Some(Key::T),
        EV_KEY::KEY_U => Some(Key::U),
        EV_KEY::KEY_V => Some(Key::V),
        EV_KEY::KEY_W => Some(Key::W),
        EV_KEY::KEY_X => Some(Key::X),
        EV_KEY::KEY_Y => Some(Key::Y),
        EV_KEY::KEY_Z => Some(Key::Z),

        EV_KEY::KEY_0 => Some(Key::Num0),
        EV_KEY::KEY_1 => Some(Key::Num1),
        EV_KEY::KEY_2 => Some(Key::Num2),
        EV_KEY::KEY_3 => Some(Key::Num3),
        EV_KEY::KEY_4 => Some(Key::Num4),
        EV_KEY::KEY_5 => Some(Key::Num5),
        EV_KEY::KEY_6 => Some(Key::Num6),
        EV_KEY::KEY_7 => Some(Key::Num7),
        EV_KEY::KEY_8 => Some(Key::Num8),
        EV_KEY::KEY_9 => Some(Key::Num9),

        EV_KEY::KEY_F1 => Some(Key::F1),
        EV_KEY::KEY_F2 => Some(Key::F2),
        EV_KEY::KEY_F3 => Some(Key::F3),
        EV_KEY::KEY_F4 => Some(Key::F4),
        EV_KEY::KEY_F5 => Some(Key::F5),
        EV_KEY::KEY_F6 => Some(Key::F6),
        EV_KEY::KEY_F7 => Some(Key::F7),
        EV_KEY::KEY_F8 => Some(Key::F8),
        EV_KEY::KEY_F9 => Some(Key::F9),
        EV_KEY::KEY_F10 => Some(Key::F10),
        EV_KEY::KEY_F11 => Some(Key::F11),
        EV_KEY::KEY_F12 => Some(Key::F12),
        EV_KEY::KEY_F13 => Some(Key::F13),
        EV_KEY::KEY_F14 => Some(Key::F14),
        EV_KEY::KEY_F15 => Some(Key::F15),
        EV_KEY::KEY_F16 => Some(Key::F16),
        EV_KEY::KEY_F17 => Some(Key::F17),
        EV_KEY::KEY_F18 => Some(Key::F18),
        EV_KEY::KEY_F19 => Some(Key::F19),
        EV_KEY::KEY_F20 => Some(Key::F20),

        EV_KEY::KEY_SPACE => Some(Key::Space),
        EV_KEY::KEY_ENTER => Some(Key::Return),
        EV_KEY::KEY_TAB => Some(Key::Tab),
        EV_KEY::KEY_ESC => Some(Key::Escape),
        EV_KEY::KEY_BACKSPACE => Some(Key::Delete),
        EV_KEY::KEY_DELETE => Some(Key::ForwardDelete),
        EV_KEY::KEY_INSERT => Some(Key::Insert),
        EV_KEY::KEY_HOME => Some(Key::Home),
        EV_KEY::KEY_END => Some(Key::End),
        EV_KEY::KEY_PAGEUP => Some(Key::PageUp),
        EV_KEY::KEY_PAGEDOWN => Some(Key::PageDown),
        EV_KEY::KEY_LEFT => Some(Key::LeftArrow),
        EV_KEY::KEY_RIGHT => Some(Key::RightArrow),
        EV_KEY::KEY_UP => Some(Key::UpArrow),
        EV_KEY::KEY_DOWN => Some(Key::DownArrow),

        EV_KEY::KEY_MINUS => Some(Key::Minus),
        EV_KEY::KEY_EQUAL => Some(Key::Equal),
        EV_KEY::KEY_LEFTBRACE => Some(Key::LeftBracket),
        EV_KEY::KEY_RIGHTBRACE => Some(Key::RightBracket),
        EV_KEY::KEY_BACKSLASH => Some(Key::Backslash),
        EV_KEY::KEY_SEMICOLON => Some(Key::Semicolon),
        EV_KEY::KEY_APOSTROPHE => Some(Key::Quote),
        EV_KEY::KEY_COMMA => Some(Key::Comma),
        EV_KEY::KEY_DOT => Some(Key::Period),
        EV_KEY::KEY_SLASH => Some(Key::Slash),
        EV_KEY::KEY_GRAVE => Some(Key::Grave),
        EV_KEY::KEY_102ND => Some(Key::Section),

        EV_KEY::KEY_KP0 => Some(Key::Keypad0),
        EV_KEY::KEY_KP1 => Some(Key::Keypad1),
        EV_KEY::KEY_KP2 => Some(Key::Keypad2),
        EV_KEY::KEY_KP3 => Some(Key::Keypad3),
        EV_KEY::KEY_KP4 => Some(Key::Keypad4),
        EV_KEY::KEY_KP5 => Some(Key::Keypad5),
        EV_KEY::KEY_KP6 => Some(Key::Keypad6),
        EV_KEY::KEY_KP7 => Some(Key::Keypad7),
        EV_KEY::KEY_KP8 => Some(Key::Keypad8),
        EV_KEY::KEY_KP9 => Some(Key::Keypad9),
        EV_KEY::KEY_KPDOT => Some(Key::KeypadDecimal),
        EV_KEY::KEY_KPASTERISK => Some(Key::KeypadMultiply),
        EV_KEY::KEY_KPPLUS => Some(Key::KeypadPlus),
        EV_KEY::KEY_KPSLASH => Some(Key::KeypadDivide),
        EV_KEY::KEY_KPENTER => Some(Key::KeypadEnter),
        EV_KEY::KEY_KPMINUS => Some(Key::KeypadMinus),
        EV_KEY::KEY_KPEQUAL => Some(Key::KeypadEquals),
        EV_KEY::KEY_KPCOMMA | EV_KEY::KEY_KPJPCOMMA => Some(Key::KeypadComma),

        EV_KEY::KEY_CAPSLOCK => Some(Key::CapsLock),
        EV_KEY::KEY_SCROLLLOCK => Some(Key::ScrollLock),
        EV_KEY::KEY_NUMLOCK => Some(Key::NumLock),

        EV_KEY::BTN_LEFT => Some(Key::MouseLeft),
        EV_KEY::BTN_RIGHT => Some(Key::MouseRight),
        EV_KEY::BTN_MIDDLE => Some(Key::MouseMiddle),
        EV_KEY::BTN_SIDE => Some(Key::MouseX1),
        EV_KEY::BTN_EXTRA => Some(Key::MouseX2),

        _ => None,
    }
}

/// Convert an evdev modifier key to our side-specific Modifiers type.
pub fn evdev_key_to_modifier(key: &EV_KEY) -> Option<Modifiers> {
    match key {
        EV_KEY::KEY_LEFTSHIFT => Some(Modifiers::SHIFT_LEFT),
        EV_KEY::KEY_RIGHTSHIFT => Some(Modifiers::SHIFT_RIGHT),
        EV_KEY::KEY_LEFTCTRL => Some(Modifiers::CTRL_LEFT),
        EV_KEY::KEY_RIGHTCTRL => Some(Modifiers::CTRL_RIGHT),
        EV_KEY::KEY_LEFTALT => Some(Modifiers::OPT_LEFT),
        EV_KEY::KEY_RIGHTALT => Some(Modifiers::OPT_RIGHT),
        EV_KEY::KEY_LEFTMETA => Some(Modifiers::CMD_LEFT),
        EV_KEY::KEY_RIGHTMETA => Some(Modifiers::CMD_RIGHT),
        _ => None,
    }
}

/// Update modifier state based on an evdev key event.
pub fn update_modifiers(current: Modifiers, key: &EV_KEY, pressed: bool) -> Modifiers {
    let Some(modifier) = evdev_key_to_modifier(key) else {
        return current;
    };

    if pressed {
        current | modifier
    } else {
        current & !modifier
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_common_keyboard_keys() {
        assert_eq!(evdev_key_to_key(&EV_KEY::KEY_A), Some(Key::A));
        assert_eq!(evdev_key_to_key(&EV_KEY::KEY_SPACE), Some(Key::Space));
        assert_eq!(evdev_key_to_key(&EV_KEY::KEY_INSERT), Some(Key::Insert));
    }

    #[test]
    fn maps_mouse_buttons() {
        assert_eq!(evdev_key_to_key(&EV_KEY::BTN_LEFT), Some(Key::MouseLeft));
        assert_eq!(evdev_key_to_key(&EV_KEY::BTN_EXTRA), Some(Key::MouseX2));
    }

    #[test]
    fn tracks_side_specific_modifiers() {
        let modifiers = update_modifiers(Modifiers::empty(), &EV_KEY::KEY_RIGHTALT, true);
        assert!(modifiers.contains(Modifiers::OPT_RIGHT));
        assert!(Modifiers::OPT.matches(modifiers));

        let modifiers = update_modifiers(modifiers, &EV_KEY::KEY_RIGHTALT, false);
        assert!(modifiers.is_empty());
    }
}
