//! winit → protocol key/button mapping.
//!
//! `proto::KeyCode` mirrors `winit::keyboard::KeyCode` name for name, so the mapping is
//! generated from the shared list and cannot drift.

use winit::event::MouseButton as WinitButton;
use winit::keyboard::KeyCode as WinitKey;

macro_rules! gen_keymap {
    ($($name:ident),* $(,)?) => {
        /// Physical winit key code → protocol key code (`None` for codes added to winit later).
        pub fn to_proto(code: WinitKey) -> Option<proto::KeyCode> {
            match code {
                $(WinitKey::$name => Some(proto::KeyCode::$name),)*
                _ => None,
            }
        }
    };
}

proto::with_keycodes!(gen_keymap);

/// winit mouse button → protocol button (`None` for extra buttons we do not forward).
pub fn button_to_proto(button: WinitButton) -> Option<proto::MouseButton> {
    match button {
        WinitButton::Left => Some(proto::MouseButton::Left),
        WinitButton::Right => Some(proto::MouseButton::Right),
        WinitButton::Middle => Some(proto::MouseButton::Middle),
        WinitButton::Back => Some(proto::MouseButton::Back),
        WinitButton::Forward => Some(proto::MouseButton::Forward),
        WinitButton::Other(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_line_up() {
        assert_eq!(to_proto(WinitKey::KeyA), Some(proto::KeyCode::KeyA));
        assert_eq!(to_proto(WinitKey::F11), Some(proto::KeyCode::F11));
        assert_eq!(
            to_proto(WinitKey::SuperLeft),
            Some(proto::KeyCode::SuperLeft)
        );
        assert_eq!(
            to_proto(WinitKey::NumpadEnter),
            Some(proto::KeyCode::NumpadEnter)
        );
        assert_eq!(to_proto(WinitKey::F35), Some(proto::KeyCode::F35));
        assert_eq!(
            button_to_proto(WinitButton::Back),
            Some(proto::MouseButton::Back)
        );
        assert_eq!(button_to_proto(WinitButton::Other(7)), None);
    }
}
