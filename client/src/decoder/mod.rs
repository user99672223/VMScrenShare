//! Video decoding behind a trait, plus the decode thread.
//!
//! [`VideoDecoder`] turns H.264 access units into [`DecodedImage`]s (raw planar frames);
//! [`scaler::Scaler`] converts and scales them to BGRA for presentation. The only v1
//! implementation is [`ffmpeg::FfmpegDecoder`] (D3D11VA / VA-API with software fallback).

pub mod ffmpeg;
pub mod scaler;

use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::mpsc::UnboundedSender;
use winit::event_loop::EventLoopProxy;

use crate::app::{fit_aspect, SharedView, UserEvent};
use crate::net::DecoderRequest;

/// A decoded picture: planes packed back to back (`align = 1`), in the decoder's native
/// pixel format. `pixel_format` is an `AVPixelFormat` value, opaque to everything but the
/// scaler.
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub pixel_format: i32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwPreference {
    /// Try the platform hardware decoder first, fall back to software.
    Auto,
    Software,
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// The stream is damaged at this point (packet loss); ask for a keyframe and carry on.
    #[error("corrupt data: {0}")]
    Corrupt(String),
    /// The decoder cannot continue; recreate it.
    #[error("decoder failure: {0}")]
    Fatal(String),
}

pub trait VideoDecoder: Send {
    /// Decodes one access unit (Annex B), appending every picture it produces to `out`.
    fn decode(
        &mut self,
        access_unit: &[u8],
        out: &mut Vec<DecodedImage>,
    ) -> Result<(), DecodeError>;
    /// Short description for the title bar, e.g. `h264 (vaapi)`.
    fn name(&self) -> String;
    fn is_hardware(&self) -> bool;
}

/// Consecutive failures of a hardware decoder before falling back to software.
const HW_FAILURE_LIMIT: u32 = 4;

/// Decode thread main loop.
pub fn run(
    rx: Receiver<Bytes>,
    view: Arc<SharedView>,
    proxy: EventLoopProxy<UserEvent>,
    requests: UnboundedSender<DecoderRequest>,
    preference: HwPreference,
) {
    let mut decoder: Box<dyn VideoDecoder> = match ffmpeg::FfmpegDecoder::new(preference) {
        Ok(d) => Box::new(d),
        Err(e) => {
            let _ = proxy.send_event(UserEvent::Fatal(format!("cannot create decoder: {e:#}")));
            return;
        }
    };
    view.set_decoder_name(&decoder.name());
    tracing::info!("decoder: {}", decoder.name());

    let mut scaler = scaler::Scaler::new();
    let mut images = Vec::new();
    let mut hw_failures = 0u32;
    let mut last_request = Instant::now() - Duration::from_secs(1);
    let request_keyframe = |last: &mut Instant| {
        if last.elapsed() >= Duration::from_millis(200) {
            let _ = requests.send(DecoderRequest::Keyframe);
            *last = Instant::now();
        }
    };

    while let Ok(au) = rx.recv() {
        images.clear();
        match decoder.decode(&au, &mut images) {
            Ok(()) => {
                hw_failures = 0;
                for img in images.drain(..) {
                    view.set_source_size(img.width, img.height);
                    let (tw, th) = fit_aspect(view.target_size(), (img.width, img.height));
                    match scaler.scale(&img, tw, th) {
                        Ok(frame) => {
                            view.publish(frame);
                            let _ = proxy.send_event(UserEvent::Frame);
                        }
                        Err(e) => tracing::warn!("scaling failed: {e:#}"),
                    }
                }
                // A decoder that silently fell back (get_format) reports it in its name.
                let name = decoder.name();
                if name != view.decoder_name() {
                    view.set_decoder_name(&name);
                }
            }
            Err(DecodeError::Corrupt(msg)) => {
                tracing::debug!("decode: {msg}");
                request_keyframe(&mut last_request);
                if decoder.is_hardware() {
                    hw_failures += 1;
                    if hw_failures >= HW_FAILURE_LIMIT {
                        decoder = fallback_to_software(decoder, &view);
                        hw_failures = 0;
                    }
                }
            }
            Err(DecodeError::Fatal(msg)) => {
                tracing::warn!("decode: {msg}");
                request_keyframe(&mut last_request);
                if decoder.is_hardware() {
                    decoder = fallback_to_software(decoder, &view);
                } else {
                    match ffmpeg::FfmpegDecoder::new(HwPreference::Software) {
                        Ok(d) => decoder = Box::new(d),
                        Err(e) => {
                            let _ = proxy.send_event(UserEvent::Fatal(format!(
                                "cannot recreate decoder: {e:#}"
                            )));
                            return;
                        }
                    }
                }
            }
        }
    }
    tracing::info!("decoder thread exiting");
}

fn fallback_to_software(
    current: Box<dyn VideoDecoder>,
    view: &SharedView,
) -> Box<dyn VideoDecoder> {
    tracing::warn!(
        "hardware decoder {} keeps failing, switching to software decoding",
        current.name()
    );
    drop(current);
    match ffmpeg::FfmpegDecoder::new(HwPreference::Software) {
        Ok(d) => {
            view.set_decoder_name(&d.name());
            Box::new(d)
        }
        Err(e) => {
            tracing::error!("software decoder unavailable: {e:#}");
            Box::new(BrokenDecoder)
        }
    }
}

/// Placeholder used when no decoder could be created at all.
struct BrokenDecoder;

impl VideoDecoder for BrokenDecoder {
    fn decode(&mut self, _: &[u8], _: &mut Vec<DecodedImage>) -> Result<(), DecodeError> {
        Err(DecodeError::Fatal("no decoder available".into()))
    }

    fn name(&self) -> String {
        "none".into()
    }

    fn is_hardware(&self) -> bool {
        false
    }
}
