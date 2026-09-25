//! Shared wire protocol between the vmdesk server (VM) and the client (laptop).
//!
//! Everything here is pure logic with no I/O so it can be unit-tested on any host:
//!
//! * [`ControlMessage`] / [`MouseMove`]: the binary messages sent over the two WebRTC
//!   data channels (`"control"`: reliable+ordered, `"mouse"`: unordered, no retransmits).
//! * [`KeyCode`]: a stable copy of winit's physical key code enumeration. The client
//!   translates winit key codes into this enum, the server maps it onto evdev `KEY_*`.
//! * [`signalling`]: the JSON body exchanged over `POST /offer`.
//! * [`sdp`]: small SDP inspection/rewriting helpers used by the signalling server.
//! * [`logging`]: tracing setup with a rate-limited bridge for the WebRTC/FFmpeg `log` output.

pub mod keycode;
pub mod logging;
pub mod message;
pub mod sdp;
pub mod signalling;

pub use keycode::KeyCode;
pub use message::{ControlMessage, DecodeError, MouseButton, MouseMove};

/// Bumped whenever the wire format changes incompatibly. Sent in [`ControlMessage::Hello`].
pub const PROTOCOL_VERSION: u16 = 1;

/// Label of the reliable, ordered data channel (keys, buttons, wheel, bitrate).
pub const CONTROL_CHANNEL: &str = "control";
/// Label of the unordered, unreliable data channel (absolute mouse moves).
pub const MOUSE_CHANNEL: &str = "mouse";

/// Absolute mouse coordinates are normalised to this range on both axes
/// (matches the uinput `ABS_X`/`ABS_Y` range created by the server).
pub const MOUSE_RANGE_MAX: u16 = u16::MAX;
