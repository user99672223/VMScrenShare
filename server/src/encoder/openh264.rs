//! OpenH264 encoder (Cisco's OpenH264 compiled from the bundled source by `openh264-sys2`).
//!
//! We talk to the `ISVCEncoder` vtable directly rather than through the high-level
//! `openh264::encoder::Encoder`, because the wrapper does not expose the parameters that
//! matter for a low-latency desktop stream: CABAC/High profile, a fixed slice count matching
//! the thread count (needed for real multi-threaded encoding), the max bitrate and runtime
//! bitrate changes.
//!
//! Settings chosen for remote-desktop use:
//! * `SCREEN_CONTENT_REAL_TIME` usage, bitrate rate control, frame skipping off.
//! * High profile with CABAC, level 4.1, one reference frame, no B-frames (OpenH264 never
//!   produces them), so the decoder can output every frame immediately.
//! * `uiIntraPeriod` = configured keyframe interval; IDR frames additionally on request.

use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::os::raw::{c_int, c_uchar};
use std::ptr;

use anyhow::{anyhow, bail, ensure, Context, Result};
use openh264::OpenH264API;
use openh264_sys2::{
    videoFormatI420, videoFrameTypeIDR, videoFrameTypeInvalid, videoFrameTypeSkip, ISVCEncoder,
    SBitrateInfo, SEncParamExt, SFrameBSInfo, SSourcePicture, API, CONSTANT_ID,
    ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE, ENCODER_OPTION_TRACE_LEVEL, LEVEL_4_1,
    MEDIUM_COMPLEXITY, PRO_HIGH, RC_BITRATE_MODE, SCREEN_CONTENT_REAL_TIME, SM_FIXEDSLCNUM_SLICE,
    SM_SINGLE_SLICE, SPATIAL_LAYER_ALL, WELS_LOG_WARNING,
};

use super::{EncodedFrame, EncoderSettings, VideoEncoder};
use crate::convert::I420Frame;

pub struct OpenH264Encoder {
    api: OpenH264API,
    encoder: *mut ISVCEncoder,
    settings: EncoderSettings,
    bitrate_kbps: u32,
    frames: u64,
}

// The encoder handle is only ever used from the thread that owns the struct.
unsafe impl Send for OpenH264Encoder {}

/// Max bitrate is allowed to exceed the target by this factor (transient bursts after big
/// screen changes); OpenH264 requires `iMaxBitrate >= iTargetBitrate`.
const MAX_BITRATE_FACTOR: u32 = 3;

impl OpenH264Encoder {
    pub fn new(settings: EncoderSettings) -> Result<Self> {
        ensure!(
            settings.width >= 16 && settings.height >= 16,
            "frame too small: {}x{}",
            settings.width,
            settings.height
        );
        ensure!(
            settings.width.is_multiple_of(2) && settings.height.is_multiple_of(2),
            "frame dimensions must be even"
        );
        let api = OpenH264API::from_source();
        let mut encoder: *mut ISVCEncoder = ptr::null_mut();
        let rc = unsafe { api.WelsCreateSVCEncoder(&mut encoder) };
        if rc != 0 || encoder.is_null() {
            bail!("WelsCreateSVCEncoder failed: {rc}");
        }
        let mut this = Self {
            api,
            encoder,
            settings,
            bitrate_kbps: settings.bitrate_kbps,
            frames: 0,
        };
        this.initialize().context("initialising OpenH264 encoder")?;
        Ok(this)
    }

    fn vtbl(&self) -> &openh264_sys2::ISVCEncoderVtbl {
        // SAFETY: `encoder` points at a live `ISVCEncoder` (a vtable pointer) until `Drop`.
        unsafe { &**self.encoder }
    }

    fn threads(&self) -> u16 {
        if self.settings.threads != 0 {
            return self.settings.threads.min(16);
        }
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        cores.clamp(1, 4) as u16
    }

    fn initialize(&mut self) -> Result<()> {
        let vt = self.vtbl();
        let get_default = vt
            .GetDefaultParams
            .ok_or_else(|| anyhow!("vtable: GetDefaultParams"))?;
        let initialize_ext = vt
            .InitializeExt
            .ok_or_else(|| anyhow!("vtable: InitializeExt"))?;
        let set_option = vt.SetOption.ok_or_else(|| anyhow!("vtable: SetOption"))?;

        let mut params = MaybeUninit::<SEncParamExt>::zeroed();
        let rc = unsafe { get_default(self.encoder, params.as_mut_ptr()) };
        ensure!(rc == 0, "GetDefaultParams failed: {rc}");
        // SAFETY: GetDefaultParams fully initialises the struct.
        let mut p = unsafe { params.assume_init() };

        let s = self.settings;
        let threads = self.threads();
        let target_bps = i32::try_from(s.bitrate_kbps.saturating_mul(1000)).unwrap_or(i32::MAX);
        let max_bps = i32::try_from(
            s.bitrate_kbps
                .saturating_mul(1000)
                .saturating_mul(MAX_BITRATE_FACTOR),
        )
        .unwrap_or(i32::MAX);

        p.iUsageType = SCREEN_CONTENT_REAL_TIME;
        p.iPicWidth = s.width as c_int;
        p.iPicHeight = s.height as c_int;
        p.iTargetBitrate = target_bps;
        p.iMaxBitrate = max_bps;
        p.iRCMode = RC_BITRATE_MODE;
        p.fMaxFrameRate = s.fps as f32;
        p.iTemporalLayerNum = 1;
        p.iSpatialLayerNum = 1;
        p.iComplexityMode = MEDIUM_COMPLEXITY;
        p.uiIntraPeriod = s.keyframe_interval;
        p.iNumRefFrame = 1;
        p.eSpsPpsIdStrategy = CONSTANT_ID;
        p.bPrefixNalAddingCtrl = false;
        p.bEnableSSEI = false;
        p.bSimulcastAVC = false;
        p.iPaddingFlag = 0;
        p.iEntropyCodingModeFlag = 1; // CABAC
        p.bEnableFrameSkip = false;
        p.bEnableLongTermReference = false;
        p.iLtrMarkPeriod = 30;
        p.iMultipleThreadIdc = threads;
        p.bUseLoadBalancing = true;
        p.iLoopFilterDisableIdc = 0;
        p.bEnableDenoise = false;
        p.bEnableBackgroundDetection = true;
        p.bEnableAdaptiveQuant = true;
        p.bEnableFrameCroppingFlag = true;
        p.bEnableSceneChangeDetect = true;

        let layer = &mut p.sSpatialLayers[0];
        layer.iVideoWidth = s.width as c_int;
        layer.iVideoHeight = s.height as c_int;
        layer.fFrameRate = s.fps as f32;
        layer.iSpatialBitrate = target_bps;
        layer.iMaxSpatialBitrate = max_bps;
        layer.uiProfileIdc = PRO_HIGH;
        layer.uiLevelIdc = LEVEL_4_1;
        if threads > 1 {
            layer.sSliceArgument.uiSliceMode = SM_FIXEDSLCNUM_SLICE;
            layer.sSliceArgument.uiSliceNum = u32::from(threads);
        } else {
            layer.sSliceArgument.uiSliceMode = SM_SINGLE_SLICE;
            layer.sSliceArgument.uiSliceNum = 1;
        }

        let rc = unsafe { initialize_ext(self.encoder, &p) };
        ensure!(rc == 0, "InitializeExt failed: {rc} (settings {s:?})");

        let mut level: c_int = WELS_LOG_WARNING as c_int;
        // Trace level is best effort: not fatal if the option is rejected.
        unsafe {
            set_option(
                self.encoder,
                ENCODER_OPTION_TRACE_LEVEL,
                ptr::addr_of_mut!(level).cast::<c_void>(),
            );
        }
        tracing::info!(
            "OpenH264 encoder: {}x{} @ {} fps, {} kbit/s (max {} kbit/s), {} thread(s), IDR every {} frames, High profile CABAC",
            s.width,
            s.height,
            s.fps,
            s.bitrate_kbps,
            s.bitrate_kbps * MAX_BITRATE_FACTOR,
            threads,
            s.keyframe_interval
        );
        Ok(())
    }

    fn set_option_bitrate(&self, option: c_int, bps: i32) -> Result<()> {
        let set_option = self
            .vtbl()
            .SetOption
            .ok_or_else(|| anyhow!("vtable: SetOption"))?;
        let mut info = SBitrateInfo {
            iLayer: SPATIAL_LAYER_ALL,
            iBitrate: bps,
        };
        let rc = unsafe {
            set_option(
                self.encoder,
                option,
                ptr::addr_of_mut!(info).cast::<c_void>(),
            )
        };
        ensure!(rc == 0, "SetOption({option}) failed: {rc}");
        Ok(())
    }
}

impl VideoEncoder for OpenH264Encoder {
    fn encode(
        &mut self,
        frame: &I420Frame,
        pts_ms: u64,
        force_keyframe: bool,
    ) -> Result<Option<EncodedFrame>> {
        ensure!(
            frame.width == self.settings.width as usize
                && frame.height == self.settings.height as usize,
            "frame size {}x{} does not match encoder {}x{}",
            frame.width,
            frame.height,
            self.settings.width,
            self.settings.height
        );
        let (sy, su, sv) = frame.strides();
        ensure!(
            frame.y.len() == sy * frame.height
                && frame.u.len() == su * (frame.height / 2)
                && frame.v.len() == sv * (frame.height / 2),
            "malformed I420 planes"
        );
        let vt = self.vtbl();
        let encode_frame = vt
            .EncodeFrame
            .ok_or_else(|| anyhow!("vtable: EncodeFrame"))?;
        if force_keyframe {
            let force = vt
                .ForceIntraFrame
                .ok_or_else(|| anyhow!("vtable: ForceIntraFrame"))?;
            let rc = unsafe { force(self.encoder, true) };
            if rc != 0 {
                tracing::warn!("ForceIntraFrame failed: {rc}");
            }
        }

        // SAFETY: an all-zero SSourcePicture is a valid (empty) value; we fill the used fields.
        let mut pic: SSourcePicture = unsafe { MaybeUninit::zeroed().assume_init() };
        pic.iColorFormat = videoFormatI420 as c_int;
        pic.iStride = [sy as c_int, su as c_int, sv as c_int, 0];
        pic.pData = [
            frame.y.as_ptr() as *mut c_uchar,
            frame.u.as_ptr() as *mut c_uchar,
            frame.v.as_ptr() as *mut c_uchar,
            ptr::null_mut(),
        ];
        pic.iPicWidth = frame.width as c_int;
        pic.iPicHeight = frame.height as c_int;
        pic.uiTimeStamp = pts_ms as i64;
        let mut info = SFrameBSInfo::default();
        let rc = unsafe { encode_frame(self.encoder, &pic, &mut info) };
        ensure!(rc == 0, "EncodeFrame failed: {rc}");
        self.frames += 1;

        match info.eFrameType {
            t if t == videoFrameTypeSkip || t == videoFrameTypeInvalid => return Ok(None),
            _ => {}
        }
        let mut data = Vec::with_capacity(info.iFrameSizeInBytes.max(0) as usize);
        for layer in info.sLayerInfo.iter().take(info.iLayerNum.max(0) as usize) {
            if layer.pBsBuf.is_null() || layer.iNalCount <= 0 {
                continue;
            }
            let mut len = 0usize;
            for i in 0..layer.iNalCount as usize {
                // SAFETY: pNalLengthInByte has iNalCount entries per the OpenH264 API.
                len += unsafe { *layer.pNalLengthInByte.add(i) }.max(0) as usize;
            }
            // SAFETY: pBsBuf holds `len` contiguous bytes of Annex B data for this layer.
            let bytes = unsafe { std::slice::from_raw_parts(layer.pBsBuf, len) };
            data.extend_from_slice(bytes);
        }
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(EncodedFrame {
            data,
            keyframe: info.eFrameType == videoFrameTypeIDR,
            pts_ms,
        }))
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        let kbps = kbps.max(100);
        if kbps == self.bitrate_kbps {
            return Ok(());
        }
        let target = i32::try_from(kbps.saturating_mul(1000)).unwrap_or(i32::MAX);
        let max = i32::try_from(kbps.saturating_mul(1000).saturating_mul(MAX_BITRATE_FACTOR))
            .unwrap_or(i32::MAX);
        // Raise the ceiling first so the target never exceeds it.
        if max
            > self
                .bitrate_kbps
                .saturating_mul(1000)
                .saturating_mul(MAX_BITRATE_FACTOR) as i32
        {
            self.set_option_bitrate(ENCODER_OPTION_MAX_BITRATE, max)?;
            self.set_option_bitrate(ENCODER_OPTION_BITRATE, target)?;
        } else {
            self.set_option_bitrate(ENCODER_OPTION_BITRATE, target)?;
            self.set_option_bitrate(ENCODER_OPTION_MAX_BITRATE, max)?;
        }
        tracing::info!("encoder bitrate -> {kbps} kbit/s");
        self.bitrate_kbps = kbps;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "openh264"
    }
}

impl Drop for OpenH264Encoder {
    fn drop(&mut self) {
        unsafe {
            if let Some(uninit) = self.vtbl().Uninitialize {
                uninit(self.encoder);
            }
            self.api.WelsDestroySVCEncoder(self.encoder);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::{nal_type, nal_units};
    use openh264::decoder::Decoder;
    use openh264::formats::YUVSource;

    const W: usize = 320;
    const H: usize = 240;

    /// Gradient background with a moving bright square.
    fn synthetic_frame(index: usize) -> I420Frame {
        let mut f = I420Frame::new(W, H);
        for y in 0..H {
            for x in 0..W {
                f.y[y * W + x] = (16 + ((x + y + index * 3) % 200)) as u8;
            }
        }
        for y in 0..H / 2 {
            for x in 0..W / 2 {
                f.u[y * (W / 2) + x] = (96 + (x % 64)) as u8;
                f.v[y * (W / 2) + x] = (96 + (y % 64)) as u8;
            }
        }
        let bx = (index * 7) % (W - 40);
        let by = (index * 5) % (H - 40);
        for y in by..by + 40 {
            for x in bx..bx + 40 {
                f.y[y * W + x] = 235;
            }
        }
        f
    }

    fn psnr_y(a: &[u8], b: &[u8]) -> f64 {
        assert_eq!(a.len(), b.len());
        let mse: f64 = a
            .iter()
            .zip(b)
            .map(|(&p, &q)| {
                let d = f64::from(p) - f64::from(q);
                d * d
            })
            .sum::<f64>()
            / a.len() as f64;
        if mse == 0.0 {
            return 99.0;
        }
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }

    fn settings() -> EncoderSettings {
        EncoderSettings {
            width: W as u32,
            height: H as u32,
            fps: 30,
            bitrate_kbps: 1500,
            keyframe_interval: 0,
            threads: 2,
        }
    }

    #[test]
    fn round_trip_through_openh264_decoder() {
        let mut enc = OpenH264Encoder::new(settings()).unwrap();
        let mut dec = Decoder::new().unwrap();
        let mut decoded = 0;
        let mut last_psnr = 0.0;
        for i in 0..24usize {
            let frame = synthetic_frame(i);
            let force = i == 12;
            let out = enc
                .encode(&frame, (i as u64) * 33, force)
                .unwrap()
                .expect("frame skipping is disabled");
            assert_eq!(out.pts_ms, (i as u64) * 33);
            let types: Vec<u8> = nal_units(&out.data).filter_map(nal_type).collect();
            if i == 0 || force {
                assert!(out.keyframe, "frame {i} should be an IDR");
                assert!(types.contains(&7), "SPS missing in keyframe: {types:?}");
                assert!(types.contains(&8), "PPS missing in keyframe: {types:?}");
                assert!(types.contains(&5), "IDR slice missing: {types:?}");
            } else {
                assert!(!out.keyframe, "frame {i} should be a P frame");
                assert!(types.contains(&1), "non-IDR slice missing: {types:?}");
            }
            assert!(out.data.starts_with(&[0, 0, 0, 1]) || out.data.starts_with(&[0, 0, 1]));

            if let Some(yuv) = dec.decode(&out.data).unwrap() {
                decoded += 1;
                assert_eq!(yuv.dimensions(), (W, H));
                let (sy, _, _) = yuv.strides();
                let mut y = Vec::with_capacity(W * H);
                for row in 0..H {
                    y.extend_from_slice(&yuv.y()[row * sy..row * sy + W]);
                }
                last_psnr = psnr_y(&frame.y, &y);
            }
        }
        // OpenH264 has no B-frames and we run without reordering delay, so every access unit
        // decodes to a picture immediately.
        assert_eq!(decoded, 24, "every frame must decode without delay");
        assert!(last_psnr > 30.0, "PSNR too low: {last_psnr:.1} dB");
    }

    #[test]
    fn static_content_produces_small_p_frames() {
        let mut enc = OpenH264Encoder::new(settings()).unwrap();
        let frame = synthetic_frame(0);
        let first = enc.encode(&frame, 0, false).unwrap().unwrap();
        assert!(first.keyframe);
        let mut total = 0usize;
        for i in 1..10u64 {
            let out = enc.encode(&frame, i * 33, false).unwrap().unwrap();
            assert!(!out.keyframe);
            total += out.data.len();
        }
        assert!(
            total < first.data.len(),
            "9 static P frames ({total} B) should be far smaller than the IDR ({} B)",
            first.data.len()
        );
    }

    #[test]
    fn bitrate_changes_are_accepted() {
        let mut enc = OpenH264Encoder::new(settings()).unwrap();
        enc.encode(&synthetic_frame(0), 0, false).unwrap();
        enc.set_bitrate(4000).unwrap();
        enc.set_bitrate(500).unwrap();
        enc.set_bitrate(50).unwrap(); // clamped to the minimum
        let out = enc.encode(&synthetic_frame(1), 33, false).unwrap().unwrap();
        assert!(!out.data.is_empty());
        assert_eq!(enc.name(), "openh264");
    }

    #[test]
    fn rejects_mismatched_frame_size() {
        let mut enc = OpenH264Encoder::new(settings()).unwrap();
        let wrong = I420Frame::new(W / 2, H / 2);
        assert!(enc.encode(&wrong, 0, false).is_err());
    }
}
