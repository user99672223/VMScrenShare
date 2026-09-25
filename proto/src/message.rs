//! Binary encoding of the data-channel messages.
//!
//! Layout: one tag byte followed by little-endian fixed-width fields. Messages are tiny
//! (max 7 bytes), so a hand-rolled codec is simpler and more predictable than a
//! serialisation framework and gives us exhaustive tests.

use crate::keycode::KeyCode;

/// Errors produced by [`ControlMessage::decode`] and [`MouseMove::decode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("message is empty")]
    Empty,
    #[error("unknown message tag {0}")]
    UnknownTag(u8),
    #[error("message truncated: expected {expected} bytes, got {actual}")]
    Truncated { expected: usize, actual: usize },
    #[error("trailing bytes after message: {0}")]
    Trailing(usize),
    #[error("unknown key code {0}")]
    UnknownKey(u16),
    #[error("unknown mouse button {0}")]
    UnknownButton(u8),
}

/// Mouse buttons carried on the control channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MouseButton {
    Left = 0,
    Right = 1,
    Middle = 2,
    /// "Back" thumb button (evdev `BTN_SIDE`).
    Back = 3,
    /// "Forward" thumb button (evdev `BTN_EXTRA`).
    Forward = 4,
}

impl MouseButton {
    pub const ALL: [MouseButton; 5] = [
        MouseButton::Left,
        MouseButton::Right,
        MouseButton::Middle,
        MouseButton::Back,
        MouseButton::Forward,
    ];

    pub fn from_u8(v: u8) -> Option<Self> {
        Self::ALL.iter().copied().find(|b| *b as u8 == v)
    }
}

/// Messages on the reliable, ordered `"control"` channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlMessage {
    /// First message sent by the client after the channel opens.
    Hello { version: u16 },
    /// Physical key press/release.
    Key { code: KeyCode, pressed: bool },
    /// Mouse button press/release.
    Button { button: MouseButton, pressed: bool },
    /// Wheel movement in notches. Positive `vertical` scrolls up (away from the user),
    /// positive `horizontal` scrolls right; matches evdev `REL_WHEEL`/`REL_HWHEEL` signs.
    Wheel { horizontal: i16, vertical: i16 },
    /// Ask the encoder to change its target bitrate (kilobits per second).
    SetBitrate { kbps: u32 },
    /// Release every key and button the server currently holds down
    /// (client lost focus, is closing, ...).
    ReleaseAll,
}

const TAG_HELLO: u8 = 0x01;
const TAG_KEY: u8 = 0x02;
const TAG_BUTTON: u8 = 0x03;
const TAG_WHEEL: u8 = 0x04;
const TAG_SET_BITRATE: u8 = 0x05;
const TAG_RELEASE_ALL: u8 = 0x06;

impl ControlMessage {
    /// Largest encoded size of any control message.
    pub const MAX_ENCODED_LEN: usize = 5;

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::MAX_ENCODED_LEN);
        self.encode_into(&mut out);
        out
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        match *self {
            ControlMessage::Hello { version } => {
                out.push(TAG_HELLO);
                out.extend_from_slice(&version.to_le_bytes());
            }
            ControlMessage::Key { code, pressed } => {
                out.push(TAG_KEY);
                out.extend_from_slice(&(code as u16).to_le_bytes());
                out.push(pressed as u8);
            }
            ControlMessage::Button { button, pressed } => {
                out.push(TAG_BUTTON);
                out.push(button as u8);
                out.push(pressed as u8);
            }
            ControlMessage::Wheel {
                horizontal,
                vertical,
            } => {
                out.push(TAG_WHEEL);
                out.extend_from_slice(&horizontal.to_le_bytes());
                out.extend_from_slice(&vertical.to_le_bytes());
            }
            ControlMessage::SetBitrate { kbps } => {
                out.push(TAG_SET_BITRATE);
                out.extend_from_slice(&kbps.to_le_bytes());
            }
            ControlMessage::ReleaseAll => out.push(TAG_RELEASE_ALL),
        }
    }

    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        let (&tag, body) = buf.split_first().ok_or(DecodeError::Empty)?;
        let expect = |n: usize| -> Result<(), DecodeError> {
            if body.len() < n {
                Err(DecodeError::Truncated {
                    expected: n + 1,
                    actual: buf.len(),
                })
            } else if body.len() > n {
                Err(DecodeError::Trailing(body.len() - n))
            } else {
                Ok(())
            }
        };
        match tag {
            TAG_HELLO => {
                expect(2)?;
                Ok(ControlMessage::Hello {
                    version: u16::from_le_bytes([body[0], body[1]]),
                })
            }
            TAG_KEY => {
                expect(3)?;
                let raw = u16::from_le_bytes([body[0], body[1]]);
                let code = KeyCode::from_u16(raw).ok_or(DecodeError::UnknownKey(raw))?;
                Ok(ControlMessage::Key {
                    code,
                    pressed: body[2] != 0,
                })
            }
            TAG_BUTTON => {
                expect(2)?;
                let button =
                    MouseButton::from_u8(body[0]).ok_or(DecodeError::UnknownButton(body[0]))?;
                Ok(ControlMessage::Button {
                    button,
                    pressed: body[1] != 0,
                })
            }
            TAG_WHEEL => {
                expect(4)?;
                Ok(ControlMessage::Wheel {
                    horizontal: i16::from_le_bytes([body[0], body[1]]),
                    vertical: i16::from_le_bytes([body[2], body[3]]),
                })
            }
            TAG_SET_BITRATE => {
                expect(4)?;
                Ok(ControlMessage::SetBitrate {
                    kbps: u32::from_le_bytes([body[0], body[1], body[2], body[3]]),
                })
            }
            TAG_RELEASE_ALL => {
                expect(0)?;
                Ok(ControlMessage::ReleaseAll)
            }
            other => Err(DecodeError::UnknownTag(other)),
        }
    }
}

/// Absolute pointer position on the unordered `"mouse"` channel.
///
/// Coordinates are normalised to `0..=65535` over the *video area* of the client window,
/// so they map 1:1 onto the server's uinput `ABS_X`/`ABS_Y` range regardless of window size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseMove {
    pub x: u16,
    pub y: u16,
}

const TAG_MOUSE_MOVE: u8 = 0x10;

impl MouseMove {
    pub const ENCODED_LEN: usize = 5;

    /// Normalise a position inside a video area of `width`x`height` pixels, clamping
    /// positions outside the area onto its edge.
    pub fn from_video_position(x: f64, y: f64, width: f64, height: f64) -> Self {
        let norm = |v: f64, size: f64| -> u16 {
            if size <= 1.0 || !v.is_finite() {
                return 0;
            }
            let f = (v / (size - 1.0)).clamp(0.0, 1.0);
            (f * f64::from(u16::MAX)).round() as u16
        };
        MouseMove {
            x: norm(x, width),
            y: norm(y, height),
        }
    }

    pub fn encode(&self) -> [u8; Self::ENCODED_LEN] {
        let x = self.x.to_le_bytes();
        let y = self.y.to_le_bytes();
        [TAG_MOUSE_MOVE, x[0], x[1], y[0], y[1]]
    }

    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        match buf.first() {
            None => Err(DecodeError::Empty),
            Some(&TAG_MOUSE_MOVE) => {
                if buf.len() < Self::ENCODED_LEN {
                    return Err(DecodeError::Truncated {
                        expected: Self::ENCODED_LEN,
                        actual: buf.len(),
                    });
                }
                if buf.len() > Self::ENCODED_LEN {
                    return Err(DecodeError::Trailing(buf.len() - Self::ENCODED_LEN));
                }
                Ok(MouseMove {
                    x: u16::from_le_bytes([buf[1], buf[2]]),
                    y: u16::from_le_bytes([buf[3], buf[4]]),
                })
            }
            Some(&other) => Err(DecodeError::UnknownTag(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_control_messages() -> Vec<ControlMessage> {
        let mut v = vec![
            ControlMessage::Hello { version: 1 },
            ControlMessage::Hello { version: u16::MAX },
            ControlMessage::Wheel {
                horizontal: -3,
                vertical: 7,
            },
            ControlMessage::Wheel {
                horizontal: i16::MIN,
                vertical: i16::MAX,
            },
            ControlMessage::SetBitrate { kbps: 12_000 },
            ControlMessage::SetBitrate { kbps: u32::MAX },
            ControlMessage::ReleaseAll,
        ];
        for &code in KeyCode::ALL {
            v.push(ControlMessage::Key {
                code,
                pressed: true,
            });
            v.push(ControlMessage::Key {
                code,
                pressed: false,
            });
        }
        for button in MouseButton::ALL {
            v.push(ControlMessage::Button {
                button,
                pressed: true,
            });
            v.push(ControlMessage::Button {
                button,
                pressed: false,
            });
        }
        v
    }

    #[test]
    fn control_round_trip() {
        for msg in all_control_messages() {
            let bytes = msg.encode();
            assert!(bytes.len() <= ControlMessage::MAX_ENCODED_LEN, "{msg:?}");
            assert_eq!(ControlMessage::decode(&bytes), Ok(msg), "{msg:?}");
        }
    }

    #[test]
    fn control_rejects_bad_input() {
        assert_eq!(ControlMessage::decode(&[]), Err(DecodeError::Empty));
        assert_eq!(
            ControlMessage::decode(&[0xEE]),
            Err(DecodeError::UnknownTag(0xEE))
        );
        assert_eq!(
            ControlMessage::decode(&[TAG_KEY, 0x00]),
            Err(DecodeError::Truncated {
                expected: 4,
                actual: 2
            })
        );
        assert_eq!(
            ControlMessage::decode(&[TAG_RELEASE_ALL, 0x00]),
            Err(DecodeError::Trailing(1))
        );
        assert_eq!(
            ControlMessage::decode(&[TAG_KEY, 0xFF, 0xFF, 0x01]),
            Err(DecodeError::UnknownKey(0xFFFF))
        );
        assert_eq!(
            ControlMessage::decode(&[TAG_BUTTON, 0x09, 0x01]),
            Err(DecodeError::UnknownButton(9))
        );
    }

    #[test]
    fn mouse_round_trip() {
        for m in [
            MouseMove { x: 0, y: 0 },
            MouseMove {
                x: u16::MAX,
                y: u16::MAX,
            },
            MouseMove {
                x: 0x1234,
                y: 0xABCD,
            },
        ] {
            assert_eq!(MouseMove::decode(&m.encode()), Ok(m));
        }
        assert_eq!(MouseMove::decode(&[]), Err(DecodeError::Empty));
        assert_eq!(
            MouseMove::decode(&[TAG_MOUSE_MOVE, 1, 2]),
            Err(DecodeError::Truncated {
                expected: 5,
                actual: 3
            })
        );
        assert_eq!(
            MouseMove::decode(&[TAG_MOUSE_MOVE, 1, 2, 3, 4, 5]),
            Err(DecodeError::Trailing(1))
        );
        // Control tags are never valid on the mouse channel and vice versa.
        assert_eq!(
            MouseMove::decode(&[TAG_RELEASE_ALL]),
            Err(DecodeError::UnknownTag(TAG_RELEASE_ALL))
        );
        assert_eq!(
            ControlMessage::decode(&MouseMove { x: 1, y: 1 }.encode()),
            Err(DecodeError::UnknownTag(TAG_MOUSE_MOVE))
        );
    }

    #[test]
    fn mouse_normalisation() {
        let m = MouseMove::from_video_position(0.0, 0.0, 1920.0, 1080.0);
        assert_eq!((m.x, m.y), (0, 0));
        let m = MouseMove::from_video_position(1919.0, 1079.0, 1920.0, 1080.0);
        assert_eq!((m.x, m.y), (u16::MAX, u16::MAX));
        let m = MouseMove::from_video_position(959.5, 539.5, 1920.0, 1080.0);
        assert!((i32::from(m.x) - 32768).abs() <= 1, "{}", m.x);
        assert!((i32::from(m.y) - 32768).abs() <= 1, "{}", m.y);
        // Outside the area clamps to the edge.
        let m = MouseMove::from_video_position(-50.0, 5000.0, 1920.0, 1080.0);
        assert_eq!((m.x, m.y), (0, u16::MAX));
        // Degenerate area does not divide by zero.
        let m = MouseMove::from_video_position(3.0, 3.0, 0.0, 1.0);
        assert_eq!((m.x, m.y), (0, 0));
    }
}
