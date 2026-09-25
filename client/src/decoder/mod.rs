//! Video decoding behind a trait, plus the decode thread.
//!
//! [`VideoDecoder`] turns H.264 access units into [`DecodedImage`]s (raw planar frames);
//! [`scaler::Scaler`] converts and scales them to BGRA for presentation. The only v1
//! implementation is [`ffmpeg::FfmpegDecoder`] (D3D11VA / VA-API with software fallback).
//!
//! Recovery policy: any decode error, or an access unit that produced no picture before the
//! first one was ever decoded (typically an IDR whose SPS/PPS packet was lost, or a stream
//! joined mid-GOP), makes the thread ask the network thread for a keyframe. Requests are rate
//! limited to one per [`KEYFRAME_REQUEST_INTERVAL`]; the network thread applies the same limit
//! to the PLIs it actually sends.

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
    /// Pictures produced so far.
    fn pictures_decoded(&self) -> u64;
}

/// Consecutive failures of a hardware decoder that never produced a picture before falling
/// back to software. Once it has decoded pictures, corrupt input is treated as packet loss.
const HW_FAILURE_LIMIT: u32 = 8;

/// Minimum spacing between keyframe requests issued by the decode thread.
pub const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_secs(1);

/// H.264 NAL unit types relevant to the recovery policy.
pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;

/// Summary of the NAL unit types in an Annex B access unit.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AccessUnitInfo {
    pub has_sps: bool,
    pub has_pps: bool,
    pub has_idr: bool,
    pub has_slice: bool,
}

impl AccessUnitInfo {
    /// Scans start codes (3 or 4 bytes) and classifies the NAL units.
    pub fn scan(annexb: &[u8]) -> Self {
        let mut info = Self::default();
        let mut i = 0;
        while i + 3 <= annexb.len() {
            if annexb[i] == 0 && annexb[i + 1] == 0 && annexb[i + 2] == 1 {
                if let Some(&hdr) = annexb.get(i + 3) {
                    match hdr & 0x1F {
                        NAL_SPS => info.has_sps = true,
                        NAL_PPS => info.has_pps = true,
                        NAL_IDR => {
                            info.has_idr = true;
                            info.has_slice = true;
                        }
                        1 => info.has_slice = true,
                        _ => {}
                    }
                }
                i += 3;
            } else {
                i += 1;
            }
        }
        info
    }

    /// An IDR that lost its parameter sets, or a keyframe-less start: nothing the decoder can
    /// use until the server sends a fresh IDR with SPS/PPS.
    pub fn needs_keyframe_before_first_picture(&self) -> bool {
        !(self.has_idr && self.has_sps && self.has_pps)
    }
}

/// Decides when the decode thread asks for a keyframe (rate limited).
pub struct RecoveryPolicy {
    last_request: Option<Instant>,
    interval: Duration,
    pub requests: u64,
}

impl RecoveryPolicy {
    pub fn new(interval: Duration) -> Self {
        Self {
            last_request: None,
            interval,
            requests: 0,
        }
    }

    /// Returns true if a request should go out now.
    pub fn request(&mut self, now: Instant) -> bool {
        if self
            .last_request
            .is_some_and(|t| now.duration_since(t) < self.interval)
        {
            return false;
        }
        self.last_request = Some(now);
        self.requests += 1;
        true
    }

    /// Evaluates one access unit's outcome. `pictures_before` is the decoder's picture count
    /// before this unit; `produced` how many it yielded; `errored` whether decoding failed.
    pub fn after_access_unit(
        &mut self,
        now: Instant,
        info: AccessUnitInfo,
        pictures_before: u64,
        produced: usize,
        errored: bool,
    ) -> bool {
        let stuck_at_start =
            pictures_before == 0 && produced == 0 && info.needs_keyframe_before_first_picture();
        if errored || stuck_at_start {
            self.request(now)
        } else {
            false
        }
    }
}

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
    let mut policy = RecoveryPolicy::new(KEYFRAME_REQUEST_INTERVAL);

    while let Ok(au) = rx.recv() {
        images.clear();
        let info = AccessUnitInfo::scan(&au);
        let pictures_before = decoder.pictures_decoded();
        let result = decoder.decode(&au, &mut images);
        let produced = images.len();
        if policy.after_access_unit(
            Instant::now(),
            info,
            pictures_before,
            produced,
            result.is_err(),
        ) {
            tracing::info!(
                "requesting a keyframe (pictures so far {}, this unit: sps {} pps {} idr {}, error {})",
                pictures_before,
                info.has_sps,
                info.has_pps,
                info.has_idr,
                result.is_err()
            );
            let _ = requests.send(DecoderRequest::Keyframe);
        }
        match result {
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
                if decoder.is_hardware() && decoder.pictures_decoded() == 0 {
                    hw_failures += 1;
                    if hw_failures >= HW_FAILURE_LIMIT {
                        decoder = fallback_to_software(decoder, &view);
                        hw_failures = 0;
                    }
                }
            }
            Err(DecodeError::Fatal(msg)) => {
                tracing::warn!("decode: {msg}");
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

    fn pictures_decoded(&self) -> u64 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDR_WITH_PARAMS: &[u8] = &[
        0, 0, 0, 1, 0x67, 0x42, // SPS
        0, 0, 0, 1, 0x68, 0xCE, // PPS
        0, 0, 1, 0x65, 0x88, // IDR slice (3-byte start code)
    ];
    const IDR_ONLY: &[u8] = &[0, 0, 0, 1, 0x65, 0x88];
    const P_SLICE: &[u8] = &[0, 0, 0, 1, 0x41, 0x9A];

    #[test]
    fn access_unit_scan_classifies_nal_types() {
        let full = AccessUnitInfo::scan(IDR_WITH_PARAMS);
        assert_eq!(
            full,
            AccessUnitInfo {
                has_sps: true,
                has_pps: true,
                has_idr: true,
                has_slice: true
            }
        );
        assert!(!full.needs_keyframe_before_first_picture());
        let idr_only = AccessUnitInfo::scan(IDR_ONLY);
        assert!(idr_only.has_idr && !idr_only.has_sps && !idr_only.has_pps);
        assert!(idr_only.needs_keyframe_before_first_picture());
        let p = AccessUnitInfo::scan(P_SLICE);
        assert!(p.has_slice && !p.has_idr);
        assert!(p.needs_keyframe_before_first_picture());
        assert_eq!(AccessUnitInfo::scan(&[]), AccessUnitInfo::default());
    }

    #[test]
    fn policy_requests_on_error_and_when_stuck_before_first_picture_rate_limited() {
        let mut p = RecoveryPolicy::new(Duration::from_secs(1));
        let t0 = Instant::now();
        let p_slice = AccessUnitInfo::scan(P_SLICE);
        let idr = AccessUnitInfo::scan(IDR_WITH_PARAMS);
        // Mid-GOP start: P slices without a picture -> request once, then rate limited.
        assert!(p.after_access_unit(t0, p_slice, 0, 0, true));
        assert!(!p.after_access_unit(t0 + Duration::from_millis(33), p_slice, 0, 0, true));
        assert!(!p.after_access_unit(t0 + Duration::from_millis(900), p_slice, 0, 0, false));
        assert!(p.after_access_unit(t0 + Duration::from_millis(1000), p_slice, 0, 0, false));
        assert_eq!(p.requests, 2);
        // An IDR with SPS/PPS that decodes: no request.
        assert!(!p.after_access_unit(t0 + Duration::from_secs(3), idr, 0, 1, false));
        // Once pictures flow, a unit without output (decoder latency) is fine...
        assert!(!p.after_access_unit(t0 + Duration::from_secs(4), p_slice, 1, 0, false));
        // ...but an error still asks for a keyframe.
        assert!(p.after_access_unit(t0 + Duration::from_secs(5), p_slice, 5, 0, true));
    }
}
