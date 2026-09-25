//! Video encoder abstraction.
//!
//! The rest of the server only sees [`VideoEncoder`]; [`openh264::OpenH264Encoder`] is the v1
//! implementation. Swapping in another encoder (e.g. a hardware encoder on a GPU VM) means
//! implementing this trait and changing [`create`].

pub mod openh264;

use anyhow::Result;

use crate::convert::I420Frame;

/// Static encoder settings chosen at construction time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// IDR interval in frames, `0` = only on request.
    pub keyframe_interval: u32,
    /// Encoder threads, `0` = automatic.
    pub threads: u16,
}

/// One encoded access unit in Annex B byte-stream format (start codes included).
///
/// Keyframes carry SPS + PPS in front of the IDR slice so a client joining late can start
/// decoding from any keyframe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Presentation time in milliseconds (monotonic, as given to `encode`).
    pub pts_ms: u64,
}

pub trait VideoEncoder: Send {
    /// Encodes one frame. Returns `None` when the encoder skipped the frame.
    fn encode(
        &mut self,
        frame: &I420Frame,
        pts_ms: u64,
        force_keyframe: bool,
    ) -> Result<Option<EncodedFrame>>;

    /// Changes the target bitrate at runtime.
    fn set_bitrate(&mut self, kbps: u32) -> Result<()>;

    /// Human readable encoder name for logs.
    fn name(&self) -> &'static str;
}

/// Creates the default encoder.
pub fn create(settings: EncoderSettings) -> Result<Box<dyn VideoEncoder>> {
    Ok(Box::new(openh264::OpenH264Encoder::new(settings)?))
}

/// Iterates over the NAL units of an Annex B byte stream (3- or 4-byte start codes).
pub fn nal_units(annexb: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= annexb.len() {
        if annexb[i] == 0 && annexb[i + 1] == 0 && annexb[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let n = starts.len();
    (0..n).map(move |k| {
        let begin = starts[k];
        let mut end = if k + 1 < n {
            starts[k + 1] - 3
        } else {
            annexb.len()
        };
        // Strip the leading zero of a 4-byte start code belonging to the next NAL.
        while end > begin && k + 1 < n && annexb[end - 1] == 0 {
            end -= 1;
        }
        &annexb[begin..end]
    })
}

/// H.264 NAL unit type of a NAL payload (first byte after the start code).
pub fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| b & 0x1F)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nal_splitting_handles_both_start_code_lengths() {
        let stream = [
            0, 0, 0, 1, 0x67, 1, 2, // SPS (4-byte start code)
            0, 0, 1, 0x68, 3, // PPS (3-byte start code)
            0, 0, 0, 1, 0x65, 4, 5, 6, 0, // IDR, payload ending in a zero byte
        ];
        let nals: Vec<&[u8]> = nal_units(&stream).collect();
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], &[0x67, 1, 2]);
        assert_eq!(nals[1], &[0x68, 3]);
        assert_eq!(nals[2], &[0x65, 4, 5, 6, 0]);
        assert_eq!(nal_type(nals[0]), Some(7));
        assert_eq!(nal_type(nals[1]), Some(8));
        assert_eq!(nal_type(nals[2]), Some(5));
        assert_eq!(nal_units(&[]).count(), 0);
        assert_eq!(nal_units(&[0, 0]).count(), 0);
    }
}
