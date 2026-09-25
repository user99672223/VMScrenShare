//! Input injection.
//!
//! The session layer decodes data-channel messages into [`InputEvent`]s and hands them to an
//! [`InputSink`]. [`uinput::UinputSink`] creates the virtual keyboard and absolute pointer;
//! [`LogInput`] only logs and is used when `/dev/uinput` is not available.

pub mod keymap;
pub mod uinput;

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

/// Creates the uinput sink, falling back to logging (with a loud error) if uinput is unusable.
pub fn create_sink() -> Box<dyn InputSink> {
    match uinput::UinputSink::new() {
        Ok(sink) => Box::new(sink),
        Err(e) => {
            tracing::error!(
                "input disabled: {e:#}. Fix: run `sudo ./server setup` (udev rule + group input), \
                 or `sudo chgrp input /dev/uinput && sudo chmod 660 /dev/uinput` and re-login. \
                 Run `server doctor` for details."
            );
            Box::new(LogInput)
        }
    }
}
