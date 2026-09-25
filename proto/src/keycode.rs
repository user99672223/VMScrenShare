//! Physical key codes.
//!
//! [`KeyCode`] mirrors winit 0.30's `KeyCode` enum variant-for-variant (same names, same
//! meaning: physical key positions, independent of keyboard layout). The client converts
//! `winit::keyboard::KeyCode` into it mechanically via [`with_keycodes!`]; the server maps it
//! onto evdev `KEY_*` codes.
//!
//! The wire value is the variant index. **The list is append-only**: never reorder or remove
//! entries, or old clients and servers will disagree about which key was pressed.

/// Invokes `$cb!` with the full list of key code identifiers.
///
/// This lets other crates generate exhaustive `match` arms against a foreign enum with the
/// same variant names (the client does this for `winit::keyboard::KeyCode`) without
/// duplicating the list.
#[macro_export]
macro_rules! with_keycodes {
    ($cb:ident) => {
        $cb! {
            Backquote, Backslash, BracketLeft, BracketRight, Comma,
            Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9,
            Equal, IntlBackslash, IntlRo, IntlYen,
            KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM,
            KeyN, KeyO, KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
            Minus, Period, Quote, Semicolon, Slash,
            AltLeft, AltRight, Backspace, CapsLock, ContextMenu, ControlLeft, ControlRight,
            Enter, SuperLeft, SuperRight, ShiftLeft, ShiftRight, Space, Tab,
            Convert, KanaMode, Lang1, Lang2, Lang3, Lang4, Lang5, NonConvert,
            Delete, End, Help, Home, Insert, PageDown, PageUp,
            ArrowDown, ArrowLeft, ArrowRight, ArrowUp,
            NumLock, Numpad0, Numpad1, Numpad2, Numpad3, Numpad4, Numpad5, Numpad6, Numpad7,
            Numpad8, Numpad9, NumpadAdd, NumpadBackspace, NumpadClear, NumpadClearEntry,
            NumpadComma, NumpadDecimal, NumpadDivide, NumpadEnter, NumpadEqual, NumpadHash,
            NumpadMemoryAdd, NumpadMemoryClear, NumpadMemoryRecall, NumpadMemoryStore,
            NumpadMemorySubtract, NumpadMultiply, NumpadParenLeft, NumpadParenRight,
            NumpadStar, NumpadSubtract,
            Escape, Fn, FnLock, PrintScreen, ScrollLock, Pause,
            BrowserBack, BrowserFavorites, BrowserForward, BrowserHome, BrowserRefresh,
            BrowserSearch, BrowserStop, Eject, LaunchApp1, LaunchApp2, LaunchMail,
            MediaPlayPause, MediaSelect, MediaStop, MediaTrackNext, MediaTrackPrevious,
            Power, Sleep, AudioVolumeDown, AudioVolumeMute, AudioVolumeUp, WakeUp,
            Meta, Hyper, Turbo, Abort, Resume, Suspend,
            Again, Copy, Cut, Find, Open, Paste, Props, Select, Undo,
            Hiragana, Katakana,
            F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12, F13, F14, F15, F16, F17, F18,
            F19, F20, F21, F22, F23, F24, F25, F26, F27, F28, F29, F30, F31, F32, F33, F34, F35
        }
    };
}

macro_rules! define_keycode_enum {
    ($($name:ident),* $(,)?) => {
        /// Physical key code, see the [module docs](self).
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[repr(u16)]
        #[allow(missing_docs)]
        pub enum KeyCode {
            $($name,)*
        }

        impl KeyCode {
            /// Every key code, in wire order.
            pub const ALL: &'static [KeyCode] = &[$(KeyCode::$name,)*];

            /// Decode a wire value.
            pub fn from_u16(v: u16) -> Option<Self> {
                match v {
                    $(x if x == KeyCode::$name as u16 => Some(KeyCode::$name),)*
                    _ => None,
                }
            }

            /// The variant name, e.g. `"KeyA"`.
            pub fn name(self) -> &'static str {
                match self {
                    $(KeyCode::$name => stringify!($name),)*
                }
            }
        }
    };
}

with_keycodes!(define_keycode_enum);

impl KeyCode {
    /// Wire value of this key code.
    pub fn as_u16(self) -> u16 {
        self as u16
    }
}

impl core::fmt::Display for KeyCode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_values_are_dense_and_round_trip() {
        assert_eq!(KeyCode::ALL.len(), 194, "winit 0.30 has 194 key codes");
        for (i, &k) in KeyCode::ALL.iter().enumerate() {
            assert_eq!(k.as_u16() as usize, i);
            assert_eq!(KeyCode::from_u16(k.as_u16()), Some(k));
        }
        assert_eq!(KeyCode::from_u16(KeyCode::ALL.len() as u16), None);
        assert_eq!(KeyCode::from_u16(u16::MAX), None);
    }

    #[test]
    fn stable_anchor_values() {
        // Guard against accidental reordering: these values are part of the wire format.
        assert_eq!(KeyCode::Backquote.as_u16(), 0);
        assert_eq!(KeyCode::KeyA.as_u16(), 19);
        assert_eq!(KeyCode::Escape.as_u16(), 114);
        assert_eq!(KeyCode::F1.as_u16(), 159);
        assert_eq!(KeyCode::F35.as_u16(), 193);
        assert_eq!(KeyCode::KeyA.name(), "KeyA");
        assert_eq!(KeyCode::F11.to_string(), "F11");
    }
}
