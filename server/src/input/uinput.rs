//! uinput devices: a full keyboard and an absolute pointer.
//!
//! The pointer looks like QEMU's USB tablet (ABS_X/ABS_Y 0..65535, mouse buttons, wheel, no
//! BTN_TOUCH): udev tags it `ID_INPUT_MOUSE` and libinput maps the absolute axes onto the
//! whole screen, so client coordinates normalised over the video area land on the same spot of
//! the 1920x1080 desktop regardless of the client window size.

use std::collections::HashSet;

use anyhow::{Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode,
    RelativeAxisCode, UinputAbsSetup,
};
use proto::{ControlMessage, MouseButton, MouseMove};

use super::{InputEvent as Ev, InputSink};
use crate::input::keymap::{button_to_evdev, to_evdev};

const VENDOR: u16 = 0x1209; // pid.codes open-source vendor id
const PRODUCT_KEYBOARD: u16 = 0xdb01;
const PRODUCT_POINTER: u16 = 0xdb02;

pub struct UinputSink {
    keyboard: VirtualDevice,
    pointer: VirtualDevice,
    pressed_keys: HashSet<KeyCode>,
    pressed_buttons: HashSet<KeyCode>,
}

impl UinputSink {
    /// Creates both devices. Fails if `/dev/uinput` is missing or not writable.
    pub fn new() -> Result<Self> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for &k in proto::KeyCode::ALL {
            if let Some(code) = to_evdev(k) {
                keys.insert(code);
            }
        }
        let keyboard = VirtualDevice::builder()
            .context(
                "opening /dev/uinput (is the udev rule in place and are you in group `input`?)",
            )?
            .name("vmdesk virtual keyboard")
            .input_id(InputId::new(
                BusType::BUS_VIRTUAL,
                VENDOR,
                PRODUCT_KEYBOARD,
                1,
            ))
            .with_keys(&keys)
            .context("keyboard keys")?
            .build()
            .context("creating virtual keyboard")?;

        let mut buttons = AttributeSet::<KeyCode>::new();
        for b in MouseButton::ALL {
            buttons.insert(button_to_evdev(b));
        }
        let mut rel = AttributeSet::<RelativeAxisCode>::new();
        rel.insert(RelativeAxisCode::REL_WHEEL);
        rel.insert(RelativeAxisCode::REL_HWHEEL);
        let abs = |axis| {
            UinputAbsSetup::new(
                axis,
                AbsInfo::new(0, 0, i32::from(proto::MOUSE_RANGE_MAX), 0, 0, 0),
            )
        };
        let pointer = VirtualDevice::builder()
            .context("opening /dev/uinput")?
            .name("vmdesk virtual pointer")
            .input_id(InputId::new(
                BusType::BUS_VIRTUAL,
                VENDOR,
                PRODUCT_POINTER,
                1,
            ))
            .with_keys(&buttons)
            .context("pointer buttons")?
            .with_relative_axes(&rel)
            .context("pointer wheel axes")?
            .with_absolute_axis(&abs(AbsoluteAxisCode::ABS_X))
            .context("pointer ABS_X")?
            .with_absolute_axis(&abs(AbsoluteAxisCode::ABS_Y))
            .context("pointer ABS_Y")?
            .build()
            .context("creating virtual pointer")?;

        let mut this = Self {
            keyboard,
            pointer,
            pressed_keys: HashSet::new(),
            pressed_buttons: HashSet::new(),
        };
        this.log_nodes();
        Ok(this)
    }

    fn log_nodes(&mut self) {
        for (name, dev) in [
            ("keyboard", &mut self.keyboard),
            ("pointer", &mut self.pointer),
        ] {
            match dev.enumerate_dev_nodes_blocking() {
                Ok(nodes) => {
                    let nodes: Vec<String> = nodes
                        .filter_map(|n| n.ok())
                        .map(|p| p.display().to_string())
                        .collect();
                    tracing::info!("uinput {name}: {}", nodes.join(", "));
                }
                Err(e) => tracing::debug!("uinput {name}: cannot list device nodes: {e}"),
            }
        }
    }

    fn key(&mut self, code: KeyCode, pressed: bool) -> Result<()> {
        if pressed {
            self.pressed_keys.insert(code);
        } else {
            self.pressed_keys.remove(&code);
        }
        self.keyboard
            .emit(&[InputEvent::new(
                EventType::KEY.0,
                code.0,
                i32::from(pressed),
            )])
            .context("emitting key event")
    }

    fn button(&mut self, code: KeyCode, pressed: bool) -> Result<()> {
        if pressed {
            self.pressed_buttons.insert(code);
        } else {
            self.pressed_buttons.remove(&code);
        }
        self.pointer
            .emit(&[InputEvent::new(
                EventType::KEY.0,
                code.0,
                i32::from(pressed),
            )])
            .context("emitting button event")
    }

    fn wheel(&mut self, horizontal: i16, vertical: i16) -> Result<()> {
        let mut events = Vec::with_capacity(2);
        if vertical != 0 {
            events.push(InputEvent::new(
                EventType::RELATIVE.0,
                RelativeAxisCode::REL_WHEEL.0,
                i32::from(vertical),
            ));
        }
        if horizontal != 0 {
            events.push(InputEvent::new(
                EventType::RELATIVE.0,
                RelativeAxisCode::REL_HWHEEL.0,
                i32::from(horizontal),
            ));
        }
        if events.is_empty() {
            return Ok(());
        }
        self.pointer.emit(&events).context("emitting wheel event")
    }

    fn move_abs(&mut self, m: MouseMove) -> Result<()> {
        self.pointer
            .emit(&[
                InputEvent::new(
                    EventType::ABSOLUTE.0,
                    AbsoluteAxisCode::ABS_X.0,
                    i32::from(m.x),
                ),
                InputEvent::new(
                    EventType::ABSOLUTE.0,
                    AbsoluteAxisCode::ABS_Y.0,
                    i32::from(m.y),
                ),
            ])
            .context("emitting pointer move")
    }
}

impl InputSink for UinputSink {
    fn handle(&mut self, event: Ev) -> Result<()> {
        match event {
            Ev::Mouse(m) => self.move_abs(m),
            Ev::Control(ControlMessage::Key { code, pressed }) => match to_evdev(code) {
                Some(ev) => self.key(ev, pressed),
                None => {
                    tracing::debug!("no evdev code for {code}");
                    Ok(())
                }
            },
            Ev::Control(ControlMessage::Button { button, pressed }) => {
                self.button(button_to_evdev(button), pressed)
            }
            Ev::Control(ControlMessage::Wheel {
                horizontal,
                vertical,
            }) => self.wheel(horizontal, vertical),
            Ev::Control(ControlMessage::ReleaseAll) => self.release_all(),
            Ev::Control(ControlMessage::Hello { .. } | ControlMessage::SetBitrate { .. }) => Ok(()),
        }
    }

    fn release_all(&mut self) -> Result<()> {
        let keys: Vec<KeyCode> = self.pressed_keys.drain().collect();
        let buttons: Vec<KeyCode> = self.pressed_buttons.drain().collect();
        if keys.is_empty() && buttons.is_empty() {
            return Ok(());
        }
        tracing::debug!("releasing {} keys, {} buttons", keys.len(), buttons.len());
        if !keys.is_empty() {
            let events: Vec<InputEvent> = keys
                .iter()
                .map(|k| InputEvent::new(EventType::KEY.0, k.0, 0))
                .collect();
            self.keyboard.emit(&events).context("releasing keys")?;
        }
        if !buttons.is_empty() {
            let events: Vec<InputEvent> = buttons
                .iter()
                .map(|b| InputEvent::new(EventType::KEY.0, b.0, 0))
                .collect();
            self.pointer.emit(&events).context("releasing buttons")?;
        }
        Ok(())
    }
}
