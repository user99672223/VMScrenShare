//! Input injection.
//!
//! The session layer decodes data-channel messages into [`InputEvent`]s and hands them to an
//! [`InputSink`]. [`LogInput`] only logs (used until uinput devices are set up);
//! [`uinput::UinputSink`] creates the virtual keyboard and absolute pointer.

use anyhow::Result;
use proto::{ControlMessage, MouseMove};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    Control(ControlMessage),
    Mouse(MouseMove),
}

pub trait InputSink: Send {
    fn handle(&mut self, event: InputEvent) -> Result<()>;
    /// Release every key and button currently held down.
    fn release_all(&mut self) -> Result<()>;
}

/// Sink that only logs events.
pub struct LogInput;

impl InputSink for LogInput {
    fn handle(&mut self, event: InputEvent) -> Result<()> {
        tracing::debug!("input (no uinput): {event:?}");
        Ok(())
    }

    fn release_all(&mut self) -> Result<()> {
        tracing::debug!("input (no uinput): release all");
        Ok(())
    }
}
