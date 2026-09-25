//! Protocol key codes → Linux evdev `KEY_*` codes.
//!
//! The protocol carries physical key positions (winit's `KeyCode` names), so the mapping is
//! layout independent; the X server on the VM applies its own XKB layout. Keys that have no
//! evdev equivalent (F25+, `NumpadMemory*`, `Fn`, ...) map to `None` and are dropped.

use evdev::KeyCode as Ev;
use proto::KeyCode as K;

pub fn to_evdev(key: K) -> Option<Ev> {
    Some(match key {
        K::Backquote => Ev::KEY_GRAVE,
        K::Backslash => Ev::KEY_BACKSLASH,
        K::BracketLeft => Ev::KEY_LEFTBRACE,
        K::BracketRight => Ev::KEY_RIGHTBRACE,
        K::Comma => Ev::KEY_COMMA,
        K::Digit0 => Ev::KEY_0,
        K::Digit1 => Ev::KEY_1,
        K::Digit2 => Ev::KEY_2,
        K::Digit3 => Ev::KEY_3,
        K::Digit4 => Ev::KEY_4,
        K::Digit5 => Ev::KEY_5,
        K::Digit6 => Ev::KEY_6,
        K::Digit7 => Ev::KEY_7,
        K::Digit8 => Ev::KEY_8,
        K::Digit9 => Ev::KEY_9,
        K::Equal => Ev::KEY_EQUAL,
        K::IntlBackslash => Ev::KEY_102ND,
        K::IntlRo => Ev::KEY_RO,
        K::IntlYen => Ev::KEY_YEN,
        K::KeyA => Ev::KEY_A,
        K::KeyB => Ev::KEY_B,
        K::KeyC => Ev::KEY_C,
        K::KeyD => Ev::KEY_D,
        K::KeyE => Ev::KEY_E,
        K::KeyF => Ev::KEY_F,
        K::KeyG => Ev::KEY_G,
        K::KeyH => Ev::KEY_H,
        K::KeyI => Ev::KEY_I,
        K::KeyJ => Ev::KEY_J,
        K::KeyK => Ev::KEY_K,
        K::KeyL => Ev::KEY_L,
        K::KeyM => Ev::KEY_M,
        K::KeyN => Ev::KEY_N,
        K::KeyO => Ev::KEY_O,
        K::KeyP => Ev::KEY_P,
        K::KeyQ => Ev::KEY_Q,
        K::KeyR => Ev::KEY_R,
        K::KeyS => Ev::KEY_S,
        K::KeyT => Ev::KEY_T,
        K::KeyU => Ev::KEY_U,
        K::KeyV => Ev::KEY_V,
        K::KeyW => Ev::KEY_W,
        K::KeyX => Ev::KEY_X,
        K::KeyY => Ev::KEY_Y,
        K::KeyZ => Ev::KEY_Z,
        K::Minus => Ev::KEY_MINUS,
        K::Period => Ev::KEY_DOT,
        K::Quote => Ev::KEY_APOSTROPHE,
        K::Semicolon => Ev::KEY_SEMICOLON,
        K::Slash => Ev::KEY_SLASH,
        K::AltLeft => Ev::KEY_LEFTALT,
        K::AltRight => Ev::KEY_RIGHTALT,
        K::Backspace => Ev::KEY_BACKSPACE,
        K::CapsLock => Ev::KEY_CAPSLOCK,
        K::ContextMenu => Ev::KEY_COMPOSE,
        K::ControlLeft => Ev::KEY_LEFTCTRL,
        K::ControlRight => Ev::KEY_RIGHTCTRL,
        K::Enter => Ev::KEY_ENTER,
        K::SuperLeft => Ev::KEY_LEFTMETA,
        K::SuperRight => Ev::KEY_RIGHTMETA,
        K::ShiftLeft => Ev::KEY_LEFTSHIFT,
        K::ShiftRight => Ev::KEY_RIGHTSHIFT,
        K::Space => Ev::KEY_SPACE,
        K::Tab => Ev::KEY_TAB,
        K::Convert => Ev::KEY_HENKAN,
        K::KanaMode => Ev::KEY_KATAKANAHIRAGANA,
        K::Lang1 => Ev::KEY_HANGEUL,
        K::Lang2 => Ev::KEY_HANJA,
        K::Lang3 => Ev::KEY_KATAKANA,
        K::Lang4 => Ev::KEY_HIRAGANA,
        K::Lang5 => Ev::KEY_ZENKAKUHANKAKU,
        K::NonConvert => Ev::KEY_MUHENKAN,
        K::Delete => Ev::KEY_DELETE,
        K::End => Ev::KEY_END,
        K::Help => Ev::KEY_HELP,
        K::Home => Ev::KEY_HOME,
        K::Insert => Ev::KEY_INSERT,
        K::PageDown => Ev::KEY_PAGEDOWN,
        K::PageUp => Ev::KEY_PAGEUP,
        K::ArrowDown => Ev::KEY_DOWN,
        K::ArrowLeft => Ev::KEY_LEFT,
        K::ArrowRight => Ev::KEY_RIGHT,
        K::ArrowUp => Ev::KEY_UP,
        K::NumLock => Ev::KEY_NUMLOCK,
        K::Numpad0 => Ev::KEY_KP0,
        K::Numpad1 => Ev::KEY_KP1,
        K::Numpad2 => Ev::KEY_KP2,
        K::Numpad3 => Ev::KEY_KP3,
        K::Numpad4 => Ev::KEY_KP4,
        K::Numpad5 => Ev::KEY_KP5,
        K::Numpad6 => Ev::KEY_KP6,
        K::Numpad7 => Ev::KEY_KP7,
        K::Numpad8 => Ev::KEY_KP8,
        K::Numpad9 => Ev::KEY_KP9,
        K::NumpadAdd => Ev::KEY_KPPLUS,
        K::NumpadComma => Ev::KEY_KPCOMMA,
        K::NumpadDecimal => Ev::KEY_KPDOT,
        K::NumpadDivide => Ev::KEY_KPSLASH,
        K::NumpadEnter => Ev::KEY_KPENTER,
        K::NumpadEqual => Ev::KEY_KPEQUAL,
        K::NumpadMultiply => Ev::KEY_KPASTERISK,
        K::NumpadParenLeft => Ev::KEY_KPLEFTPAREN,
        K::NumpadParenRight => Ev::KEY_KPRIGHTPAREN,
        K::NumpadSubtract => Ev::KEY_KPMINUS,
        K::Escape => Ev::KEY_ESC,
        K::Fn => Ev::KEY_FN,
        K::PrintScreen => Ev::KEY_SYSRQ,
        K::ScrollLock => Ev::KEY_SCROLLLOCK,
        K::Pause => Ev::KEY_PAUSE,
        K::BrowserBack => Ev::KEY_BACK,
        K::BrowserFavorites => Ev::KEY_BOOKMARKS,
        K::BrowserForward => Ev::KEY_FORWARD,
        K::BrowserHome => Ev::KEY_HOMEPAGE,
        K::BrowserRefresh => Ev::KEY_REFRESH,
        K::BrowserSearch => Ev::KEY_SEARCH,
        K::BrowserStop => Ev::KEY_STOP,
        K::Eject => Ev::KEY_EJECTCD,
        K::LaunchApp1 => Ev::KEY_COMPUTER,
        K::LaunchApp2 => Ev::KEY_CALC,
        K::LaunchMail => Ev::KEY_MAIL,
        K::MediaPlayPause => Ev::KEY_PLAYPAUSE,
        K::MediaSelect => Ev::KEY_MEDIA,
        K::MediaStop => Ev::KEY_STOPCD,
        K::MediaTrackNext => Ev::KEY_NEXTSONG,
        K::MediaTrackPrevious => Ev::KEY_PREVIOUSSONG,
        K::Power => Ev::KEY_POWER,
        K::Sleep => Ev::KEY_SLEEP,
        K::AudioVolumeDown => Ev::KEY_VOLUMEDOWN,
        K::AudioVolumeMute => Ev::KEY_MUTE,
        K::AudioVolumeUp => Ev::KEY_VOLUMEUP,
        K::WakeUp => Ev::KEY_WAKEUP,
        K::Again => Ev::KEY_AGAIN,
        K::Copy => Ev::KEY_COPY,
        K::Cut => Ev::KEY_CUT,
        K::Find => Ev::KEY_FIND,
        K::Open => Ev::KEY_OPEN,
        K::Paste => Ev::KEY_PASTE,
        K::Props => Ev::KEY_PROPS,
        K::Select => Ev::KEY_SELECT,
        K::Undo => Ev::KEY_UNDO,
        K::Hiragana => Ev::KEY_HIRAGANA,
        K::Katakana => Ev::KEY_KATAKANA,
        K::F1 => Ev::KEY_F1,
        K::F2 => Ev::KEY_F2,
        K::F3 => Ev::KEY_F3,
        K::F4 => Ev::KEY_F4,
        K::F5 => Ev::KEY_F5,
        K::F6 => Ev::KEY_F6,
        K::F7 => Ev::KEY_F7,
        K::F8 => Ev::KEY_F8,
        K::F9 => Ev::KEY_F9,
        K::F10 => Ev::KEY_F10,
        K::F11 => Ev::KEY_F11,
        K::F12 => Ev::KEY_F12,
        K::F13 => Ev::KEY_F13,
        K::F14 => Ev::KEY_F14,
        K::F15 => Ev::KEY_F15,
        K::F16 => Ev::KEY_F16,
        K::F17 => Ev::KEY_F17,
        K::F18 => Ev::KEY_F18,
        K::F19 => Ev::KEY_F19,
        K::F20 => Ev::KEY_F20,
        K::F21 => Ev::KEY_F21,
        K::F22 => Ev::KEY_F22,
        K::F23 => Ev::KEY_F23,
        K::F24 => Ev::KEY_F24,
        // No evdev equivalent (or only a duplicate of another key).
        K::NumpadBackspace
        | K::NumpadClear
        | K::NumpadClearEntry
        | K::NumpadHash
        | K::NumpadMemoryAdd
        | K::NumpadMemoryClear
        | K::NumpadMemoryRecall
        | K::NumpadMemoryStore
        | K::NumpadMemorySubtract
        | K::NumpadStar
        | K::FnLock
        | K::Meta
        | K::Hyper
        | K::Turbo
        | K::Abort
        | K::Resume
        | K::Suspend
        | K::F25
        | K::F26
        | K::F27
        | K::F28
        | K::F29
        | K::F30
        | K::F31
        | K::F32
        | K::F33
        | K::F34
        | K::F35 => return None,
    })
}

pub fn button_to_evdev(button: proto::MouseButton) -> Ev {
    match button {
        proto::MouseButton::Left => Ev::BTN_LEFT,
        proto::MouseButton::Right => Ev::BTN_RIGHT,
        proto::MouseButton::Middle => Ev::BTN_MIDDLE,
        proto::MouseButton::Back => Ev::BTN_SIDE,
        proto::MouseButton::Forward => Ev::BTN_EXTRA,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn spot_checks() {
        assert_eq!(to_evdev(K::KeyA), Some(Ev::KEY_A));
        assert_eq!(to_evdev(K::Digit0), Some(Ev::KEY_0));
        assert_eq!(to_evdev(K::F11), Some(Ev::KEY_F11));
        assert_eq!(to_evdev(K::ArrowUp), Some(Ev::KEY_UP));
        assert_eq!(to_evdev(K::SuperLeft), Some(Ev::KEY_LEFTMETA));
        assert_eq!(to_evdev(K::Numpad0), Some(Ev::KEY_KP0));
        assert_eq!(to_evdev(K::ContextMenu), Some(Ev::KEY_COMPOSE));
        assert_eq!(to_evdev(K::F35), None);
        assert_eq!(button_to_evdev(proto::MouseButton::Left), Ev::BTN_LEFT);
        assert_eq!(button_to_evdev(proto::MouseButton::Forward), Ev::BTN_EXTRA);
    }

    #[test]
    fn mapping_is_injective_except_documented_duplicates() {
        // Lang3/Lang4 (W3C) and Katakana/Hiragana (legacy) name the same physical keys.
        let allowed: [(K, K); 2] = [(K::Lang3, K::Katakana), (K::Lang4, K::Hiragana)];
        let mut seen: HashMap<Ev, K> = HashMap::new();
        let mut mapped = 0;
        for &k in K::ALL {
            let Some(ev) = to_evdev(k) else { continue };
            mapped += 1;
            if let Some(prev) = seen.insert(ev, k) {
                assert!(
                    allowed.contains(&(prev, k)) || allowed.contains(&(k, prev)),
                    "{prev:?} and {k:?} both map to {ev:?}"
                );
            }
        }
        assert!(mapped >= 160, "only {mapped} keys mapped");
    }

    #[test]
    fn every_key_is_classified() {
        // The match is exhaustive by construction; this guards the `None` list against
        // accidentally swallowing common keys.
        for &k in K::ALL {
            let name = k.name();
            let fkey = name
                .strip_prefix('F')
                .and_then(|n| n.parse::<u32>().ok())
                .is_some_and(|n| (1..=24).contains(&n));
            let common = name.starts_with("Key")
                || name.starts_with("Digit")
                || name.starts_with("Arrow")
                || fkey;
            if common {
                assert!(to_evdev(k).is_some(), "{name} must be mapped");
            }
        }
    }
}
