//! Window, presentation and input capture (runs on the main thread).
//!
//! * Decoded frames arrive already scaled to the video area (window size, aspect preserved)
//!   as BGRA `u32` pixels in [`SharedView::latest`]; the redraw blits them centred with black
//!   bars and presents through softbuffer.
//! * Mouse positions are normalised over the video area, keys are forwarded as physical codes,
//!   F11 toggles fullscreen, focus loss releases everything on the server.

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use proto::{ControlMessage, MouseMove};
use tokio::sync::mpsc::UnboundedSender;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowAttributes, WindowId};

use crate::keymap;
use crate::net::UiCommand;

/// Events from the network/decoder threads to the UI thread.
#[derive(Debug)]
pub enum UserEvent {
    /// A new frame is available in [`SharedView::latest`].
    Frame,
    /// Connection status text for the title bar.
    Status(String),
    /// Unrecoverable error: show it and quit.
    Fatal(String),
}

/// A frame scaled to its final on-screen size, `0x00RRGGBB` pixels (softbuffer layout).
#[derive(Clone)]
pub struct RgbFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
}

/// State shared between the decoder thread and the UI thread.
pub struct SharedView {
    pub latest: Mutex<Option<RgbFrame>>,
    /// Window inner size packed as `(w << 32) | h`; the decoder scales into it.
    target_size: AtomicU64,
    /// Source video size packed like `target_size`.
    source_size: AtomicU64,
    pub decoded_frames: AtomicU64,
    decoder_name: Mutex<String>,
}

fn pack(w: u32, h: u32) -> u64 {
    (u64::from(w) << 32) | u64::from(h)
}

fn unpack(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, v as u32)
}

impl SharedView {
    pub fn new() -> Self {
        Self {
            latest: Mutex::new(None),
            target_size: AtomicU64::new(pack(1280, 720)),
            source_size: AtomicU64::new(0),
            decoded_frames: AtomicU64::new(0),
            decoder_name: Mutex::new("starting".into()),
        }
    }

    pub fn set_target_size(&self, w: u32, h: u32) {
        self.target_size
            .store(pack(w.max(1), h.max(1)), Ordering::Relaxed);
    }

    pub fn target_size(&self) -> (u32, u32) {
        unpack(self.target_size.load(Ordering::Relaxed))
    }

    pub fn set_source_size(&self, w: u32, h: u32) {
        self.source_size.store(pack(w, h), Ordering::Relaxed);
    }

    pub fn source_size(&self) -> Option<(u32, u32)> {
        match unpack(self.source_size.load(Ordering::Relaxed)) {
            (0, _) | (_, 0) => None,
            s => Some(s),
        }
    }

    pub fn set_decoder_name(&self, name: &str) {
        *self.decoder_name.lock().unwrap_or_else(|e| e.into_inner()) = name.to_string();
    }

    pub fn decoder_name(&self) -> String {
        self.decoder_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn publish(&self, frame: RgbFrame) {
        *self.latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(frame);
        self.decoded_frames.fetch_add(1, Ordering::Relaxed);
    }
}

/// Computes the largest `src`-aspect rectangle that fits into `target`.
pub fn fit_aspect(target: (u32, u32), src: (u32, u32)) -> (u32, u32) {
    let (tw, th) = (target.0.max(1), target.1.max(1));
    let (sw, sh) = (f64::from(src.0.max(1)), f64::from(src.1.max(1)));
    let scale = (f64::from(tw) / sw).min(f64::from(th) / sh);
    let w = ((sw * scale).round() as u32).clamp(1, tw).max(2);
    let h = ((sh * scale).round() as u32).clamp(1, th).max(2);
    (w, h)
}

pub fn run(
    event_loop: EventLoop<UserEvent>,
    ui: UnboundedSender<UiCommand>,
    view: Arc<SharedView>,
) -> Result<()> {
    let mut app = App {
        ui,
        view,
        window: None,
        context: None,
        surface: None,
        status: "connecting".into(),
        next_title: Instant::now(),
        fps_frames: 0,
        fps_at: Instant::now(),
        fps: 0.0,
        fullscreen: false,
        pressed_keys: HashSet::new(),
        pressed_buttons: HashSet::new(),
        wheel_acc: (0.0, 0.0),
        fatal: None,
    };
    event_loop.set_control_flow(ControlFlow::WaitUntil(
        Instant::now() + Duration::from_secs(1),
    ));
    event_loop.run_app(&mut app).context("event loop")?;
    if let Some(msg) = app.fatal {
        anyhow::bail!("{msg}");
    }
    Ok(())
}

struct App {
    ui: UnboundedSender<UiCommand>,
    view: Arc<SharedView>,
    window: Option<Rc<Window>>,
    context: Option<softbuffer::Context<Rc<Window>>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    status: String,
    next_title: Instant,
    fps_frames: u64,
    fps_at: Instant,
    fps: f64,
    fullscreen: bool,
    pressed_keys: HashSet<proto::KeyCode>,
    pressed_buttons: HashSet<proto::MouseButton>,
    /// Fractional wheel notches carried over between events (horizontal, vertical).
    wheel_acc: (f32, f32),
    fatal: Option<String>,
}

/// Pixels of touchpad scrolling that count as one wheel notch.
const PIXELS_PER_NOTCH: f32 = 60.0;

impl App {
    fn send(&self, cmd: UiCommand) {
        if self.ui.send(cmd).is_err() {
            tracing::debug!("network thread gone");
        }
    }

    fn control(&self, msg: ControlMessage) {
        self.send(UiCommand::Control(msg));
    }

    fn release_all(&mut self) {
        if !self.pressed_keys.is_empty() || !self.pressed_buttons.is_empty() {
            tracing::debug!(
                "releasing {} keys / {} buttons",
                self.pressed_keys.len(),
                self.pressed_buttons.len()
            );
        }
        self.pressed_keys.clear();
        self.pressed_buttons.clear();
        self.control(ControlMessage::ReleaseAll);
    }

    /// Video rectangle (x, y, w, h) inside the window, from the latest frame's size.
    fn video_rect(&self) -> (f64, f64, f64, f64) {
        let (ww, wh) = self
            .window
            .as_ref()
            .map(|w| {
                let s = w.inner_size();
                (f64::from(s.width.max(1)), f64::from(s.height.max(1)))
            })
            .unwrap_or((1.0, 1.0));
        let frame_size = self
            .view
            .latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|f| (f64::from(f.width), f64::from(f.height)));
        match frame_size {
            Some((fw, fh)) if fw <= ww && fh <= wh => {
                (((ww - fw) / 2.0).floor(), ((wh - fh) / 2.0).floor(), fw, fh)
            }
            _ => (0.0, 0.0, ww, wh),
        }
    }

    fn update_title(&mut self) {
        let Some(window) = &self.window else { return };
        let frames = self.view.decoded_frames.load(Ordering::Relaxed);
        let elapsed = self.fps_at.elapsed().as_secs_f64();
        if elapsed >= 0.5 {
            self.fps = (frames - self.fps_frames) as f64 / elapsed;
            self.fps_frames = frames;
            self.fps_at = Instant::now();
        }
        let source = self
            .view
            .source_size()
            .map(|(w, h)| format!(" {w}x{h}"))
            .unwrap_or_default();
        window.set_title(&format!(
            "vmdesk - {} - {}{} - {:.0} fps - F11 fullscreen",
            self.status,
            self.view.decoder_name(),
            source,
            self.fps
        ));
        self.next_title = Instant::now() + Duration::from_secs(1);
    }

    fn redraw(&mut self) {
        let (Some(window), Some(surface)) = (self.window.as_ref(), self.surface.as_mut()) else {
            return;
        };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return;
        };
        if let Err(e) = surface.resize(w, h) {
            tracing::warn!("surface resize failed: {e}");
            return;
        }
        let mut buffer = match surface.buffer_mut() {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("buffer_mut failed: {e}");
                return;
            }
        };
        {
            let latest = self.view.latest.lock().unwrap_or_else(|e| e.into_inner());
            match latest.as_ref() {
                Some(frame) => blit(frame, &mut buffer, size.width, size.height),
                None => buffer.fill(0),
            }
        }
        if let Err(e) = buffer.present() {
            tracing::warn!("present failed: {e}");
        }
    }

    fn handle_key(&mut self, event: KeyEvent, event_loop: &ActiveEventLoop) {
        let PhysicalKey::Code(code) = event.physical_key else {
            return;
        };
        let pressed = event.state == ElementState::Pressed;
        if code == KeyCode::F11 {
            if pressed && !event.repeat {
                self.toggle_fullscreen();
            }
            return;
        }
        if event.repeat {
            // The X server on the VM generates auto-repeat itself.
            return;
        }
        let Some(key) = keymap::to_proto(code) else {
            tracing::debug!("no protocol key for {code:?}");
            return;
        };
        if pressed {
            self.pressed_keys.insert(key);
        } else {
            self.pressed_keys.remove(&key);
        }
        self.control(ControlMessage::Key { code: key, pressed });
        let _ = event_loop;
    }

    fn toggle_fullscreen(&mut self) {
        let Some(window) = &self.window else { return };
        self.fullscreen = !self.fullscreen;
        window.set_fullscreen(if self.fullscreen {
            Some(Fullscreen::Borderless(None))
        } else {
            None
        });
    }

    fn handle_wheel(&mut self, delta: MouseScrollDelta) {
        let (dx, dy) = match delta {
            MouseScrollDelta::LineDelta(x, y) => (x, y),
            MouseScrollDelta::PixelDelta(p) => {
                (p.x as f32 / PIXELS_PER_NOTCH, p.y as f32 / PIXELS_PER_NOTCH)
            }
        };
        self.wheel_acc.0 += dx;
        self.wheel_acc.1 += dy;
        let h = self.wheel_acc.0.trunc();
        let v = self.wheel_acc.1.trunc();
        if h != 0.0 || v != 0.0 {
            self.wheel_acc.0 -= h;
            self.wheel_acc.1 -= v;
            self.control(ControlMessage::Wheel {
                horizontal: h.clamp(-1000.0, 1000.0) as i16,
                vertical: v.clamp(-1000.0, 1000.0) as i16,
            });
        }
    }
}

/// Copies `frame` centred into `dst` (a `width`x`height` softbuffer buffer), black elsewhere.
fn blit(frame: &RgbFrame, dst: &mut [u32], width: u32, height: u32) {
    let (w, h) = (width as usize, height as usize);
    let (fw, fh) = (frame.width as usize, frame.height as usize);
    if fw == w && fh == h && frame.pixels.len() == dst.len() {
        dst.copy_from_slice(&frame.pixels);
        return;
    }
    dst.fill(0);
    let copy_w = fw.min(w);
    let copy_h = fh.min(h);
    let x0 = (w - copy_w) / 2;
    let y0 = (h - copy_h) / 2;
    let sx0 = (fw - copy_w) / 2;
    let sy0 = (fh - copy_h) / 2;
    for row in 0..copy_h {
        let src = &frame.pixels[(sy0 + row) * fw + sx0..][..copy_w];
        let dst_row = &mut dst[(y0 + row) * w + x0..][..copy_w];
        dst_row.copy_from_slice(src);
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = WindowAttributes::default()
            .with_title("vmdesk - connecting")
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                self.fatal = Some(format!("cannot create window: {e}"));
                event_loop.exit();
                return;
            }
        };
        let context = match softbuffer::Context::new(Rc::clone(&window)) {
            Ok(c) => c,
            Err(e) => {
                self.fatal = Some(format!("softbuffer context: {e}"));
                event_loop.exit();
                return;
            }
        };
        let surface = match softbuffer::Surface::new(&context, Rc::clone(&window)) {
            Ok(s) => s,
            Err(e) => {
                self.fatal = Some(format!("softbuffer surface: {e}"));
                event_loop.exit();
                return;
            }
        };
        let size = window.inner_size();
        self.view.set_target_size(size.width, size.height);
        self.window = Some(window);
        self.context = Some(context);
        self.surface = Some(surface);
        self.update_title();
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Frame => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            UserEvent::Status(s) => {
                self.status = s;
                self.update_title();
            }
            UserEvent::Fatal(msg) => {
                tracing::error!("{msg}");
                self.fatal = Some(msg);
                self.release_all();
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.release_all();
                self.send(UiCommand::Quit);
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                self.view.set_target_size(size.width, size.height);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Focused(focused) => {
                if !focused {
                    self.release_all();
                }
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } => {
                if !is_synthetic {
                    self.handle_key(event, event_loop);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let (x, y, w, h) = self.video_rect();
                let m = MouseMove::from_video_position(position.x - x, position.y - y, w, h);
                self.send(UiCommand::Mouse(m));
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(button) = keymap::button_to_proto(button) {
                    let pressed = state == ElementState::Pressed;
                    if pressed {
                        self.pressed_buttons.insert(button);
                    } else {
                        self.pressed_buttons.remove(&button);
                    }
                    self.control(ControlMessage::Button { button, pressed });
                }
            }
            WindowEvent::MouseWheel { delta, .. } => self.handle_wheel(delta),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if Instant::now() >= self.next_title {
            self.update_title();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_title));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aspect_fit() {
        assert_eq!(fit_aspect((1920, 1080), (1920, 1080)), (1920, 1080));
        assert_eq!(fit_aspect((1280, 720), (1920, 1080)), (1280, 720));
        assert_eq!(fit_aspect((1280, 1024), (1920, 1080)), (1280, 720));
        assert_eq!(fit_aspect((3000, 1080), (1920, 1080)), (1920, 1080));
        assert_eq!(fit_aspect((10, 10), (1920, 1080)), (10, 6));
        assert_eq!(fit_aspect((0, 0), (1920, 1080)), (2, 2));
    }

    #[test]
    fn blit_centres_and_clips() {
        let frame = RgbFrame {
            width: 2,
            height: 2,
            pixels: vec![1, 2, 3, 4],
        };
        let mut dst = vec![9u32; 16];
        blit(&frame, &mut dst, 4, 4);
        assert_eq!(dst, vec![0, 0, 0, 0, 0, 1, 2, 0, 0, 3, 4, 0, 0, 0, 0, 0]);
        // Same size: straight copy.
        let mut dst = vec![0u32; 4];
        blit(&frame, &mut dst, 2, 2);
        assert_eq!(dst, vec![1, 2, 3, 4]);
        // Frame larger than the window: clipped around the centre.
        let mut dst = vec![9u32; 1];
        blit(&frame, &mut dst, 1, 1);
        assert_eq!(dst, vec![1]);
    }
}
