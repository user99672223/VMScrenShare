//! FFmpeg H.264 decoder with hardware acceleration and software fallback.
//!
//! Uses the `libavcodec` C API through `ffmpeg-sys-next` (the FFI crate behind `ffmpeg-next`)
//! because the safe wrapper does not expose `hw_device_ctx` / `get_format`.
//!
//! * Windows: D3D11VA, then DXVA2.  Linux: VA-API (Intel via `intel-media-va-driver`).
//! * Low-latency flags: `AV_CODEC_FLAG_LOW_DELAY`, `AV_CODEC_FLAG2_FAST`; the software path
//!   uses slice threading only (frame threading would add a frame of delay).
//! * Hardware frames are transferred to system memory with `av_hwframe_transfer_data`
//!   (NV12) and handed to the scaler like software frames.

use std::ffi::{c_char, c_int, c_void, CStr};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{anyhow, bail, Result};
use ffmpeg_sys_next as ff;

use super::{DecodeError, DecodedImage, HwPreference, VideoDecoder};

/// Passed to the `get_format` callback through `AVCodecContext::opaque`.
struct CallbackState {
    wanted: ff::AVPixelFormat,
    fell_back: AtomicBool,
}

pub struct FfmpegDecoder {
    ctx: *mut ff::AVCodecContext,
    frame: *mut ff::AVFrame,
    sw_frame: *mut ff::AVFrame,
    packet: *mut ff::AVPacket,
    hw_device: *mut ff::AVBufferRef,
    hw_pix_fmt: Option<ff::AVPixelFormat>,
    // Boxed so the pointer stored in `ctx.opaque` stays valid while `ctx` lives.
    callback: Option<Box<CallbackState>>,
    hw_name: Option<&'static str>,
    pictures: u64,
}

// All pointers are owned by this struct and only touched from the decode thread.
unsafe impl Send for FfmpegDecoder {}

fn hw_candidates() -> Vec<(ff::AVHWDeviceType, &'static str)> {
    if cfg!(windows) {
        vec![
            (ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA, "d3d11va"),
            (ff::AVHWDeviceType::AV_HWDEVICE_TYPE_DXVA2, "dxva2"),
        ]
    } else if cfg!(target_os = "linux") {
        vec![(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, "vaapi")]
    } else {
        Vec::new()
    }
}

fn err_str(code: c_int) -> String {
    let mut buf = [0 as c_char; 128];
    unsafe {
        ff::av_strerror(code, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

fn is_hw_format(fmt: ff::AVPixelFormat) -> bool {
    unsafe {
        let desc = ff::av_pix_fmt_desc_get(fmt);
        !desc.is_null() && ((*desc).flags & (ff::AV_PIX_FMT_FLAG_HWACCEL as u64)) != 0
    }
}

/// Picks the hardware pixel format we asked for, or the first software format if the
/// decoder cannot offer it (recorded in `CallbackState::fell_back`).
unsafe extern "C" fn get_format(
    ctx: *mut ff::AVCodecContext,
    formats: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    let state = (*ctx).opaque as *const CallbackState;
    let mut p = formats;
    while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
        if !state.is_null() && *p == (*state).wanted {
            return *p;
        }
        p = p.add(1);
    }
    if !state.is_null() {
        (*state).fell_back.store(true, Ordering::Relaxed);
    }
    let mut p = formats;
    while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
        if !is_hw_format(*p) {
            return *p;
        }
        p = p.add(1);
    }
    ff::AVPixelFormat::AV_PIX_FMT_NONE
}

impl FfmpegDecoder {
    pub fn new(preference: HwPreference) -> Result<Self> {
        unsafe {
            ff::av_log_set_level(ff::AV_LOG_WARNING as c_int);
        }
        if preference == HwPreference::Auto {
            for (dev, name) in hw_candidates() {
                match Self::open(Some((dev, name))) {
                    Ok(d) => return Ok(d),
                    Err(e) => tracing::warn!("hardware decoder {name} unavailable: {e:#}"),
                }
            }
            tracing::warn!("no hardware decoder, using software h264");
        }
        Self::open(None)
    }

    fn open(hw: Option<(ff::AVHWDeviceType, &'static str)>) -> Result<Self> {
        unsafe {
            let codec = ff::avcodec_find_decoder(ff::AVCodecID::AV_CODEC_ID_H264);
            if codec.is_null() {
                bail!("this FFmpeg build has no h264 decoder");
            }

            let mut hw_pix_fmt = None;
            let mut hw_device: *mut ff::AVBufferRef = ptr::null_mut();
            if let Some((dev_type, name)) = hw {
                let mut i = 0;
                loop {
                    let cfg = ff::avcodec_get_hw_config(codec, i);
                    if cfg.is_null() {
                        break;
                    }
                    let methods = (*cfg).methods as u32;
                    if (*cfg).device_type == dev_type
                        && methods & (ff::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as u32) != 0
                    {
                        hw_pix_fmt = Some((*cfg).pix_fmt);
                        break;
                    }
                    i += 1;
                }
                let Some(_) = hw_pix_fmt else {
                    bail!("{name}: this FFmpeg build has no {name} hwaccel for h264");
                };
                let rc = ff::av_hwdevice_ctx_create(
                    &mut hw_device,
                    dev_type,
                    ptr::null(),
                    ptr::null_mut(),
                    0,
                );
                if rc < 0 {
                    bail!("{name}: av_hwdevice_ctx_create failed: {}", err_str(rc));
                }
            }

            let ctx = ff::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                ff::av_buffer_unref(&mut hw_device);
                bail!("avcodec_alloc_context3 failed");
            }
            let mut this = Self {
                ctx,
                frame: ff::av_frame_alloc(),
                sw_frame: ff::av_frame_alloc(),
                packet: ff::av_packet_alloc(),
                hw_device,
                hw_pix_fmt,
                callback: None,
                hw_name: hw.map(|(_, n)| n),
                pictures: 0,
            };
            if this.frame.is_null() || this.sw_frame.is_null() || this.packet.is_null() {
                bail!("out of memory allocating frames");
            }

            (*ctx).flags |= ff::AV_CODEC_FLAG_LOW_DELAY as c_int;
            (*ctx).flags2 |= ff::AV_CODEC_FLAG2_FAST as c_int;
            if let Some(pix_fmt) = hw_pix_fmt {
                let state = Box::new(CallbackState {
                    wanted: pix_fmt,
                    fell_back: AtomicBool::new(false),
                });
                (*ctx).opaque = &*state as *const CallbackState as *mut c_void;
                (*ctx).get_format = Some(get_format);
                (*ctx).hw_device_ctx = ff::av_buffer_ref(hw_device);
                (*ctx).thread_count = 1;
                this.callback = Some(state);
            } else {
                let cores = std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(2);
                (*ctx).thread_count = cores.clamp(1, 4) as c_int;
                (*ctx).thread_type = ff::FF_THREAD_SLICE as c_int;
            }

            let rc = ff::avcodec_open2(ctx, codec, ptr::null_mut());
            if rc < 0 {
                bail!("avcodec_open2 failed: {}", err_str(rc));
            }
            Ok(this)
        }
    }

    /// Copies the frame's planes into a tightly packed buffer.
    unsafe fn copy_image(&self, frame: *mut ff::AVFrame) -> Result<DecodedImage, DecodeError> {
        let width = (*frame).width;
        let height = (*frame).height;
        let format = (*frame).format;
        if width <= 0 || height <= 0 || format < 0 {
            return Err(DecodeError::Corrupt(format!(
                "decoder produced an invalid frame {width}x{height} fmt {format}"
            )));
        }
        // SAFETY: `format` is a value FFmpeg itself set on the frame, hence a valid enum member.
        let pix_fmt: ff::AVPixelFormat = std::mem::transmute(format);
        let size = ff::av_image_get_buffer_size(pix_fmt, width, height, 1);
        if size <= 0 {
            return Err(DecodeError::Fatal(format!(
                "av_image_get_buffer_size failed for fmt {format}: {}",
                err_str(size)
            )));
        }
        let mut data = vec![0u8; size as usize];
        let rc = ff::av_image_copy_to_buffer(
            data.as_mut_ptr(),
            size,
            (*frame).data.as_ptr() as *const *const u8,
            (*frame).linesize.as_ptr(),
            pix_fmt,
            width,
            height,
            1,
        );
        if rc < 0 {
            return Err(DecodeError::Fatal(format!(
                "av_image_copy_to_buffer failed: {}",
                err_str(rc)
            )));
        }
        Ok(DecodedImage {
            width: width as u32,
            height: height as u32,
            pixel_format: format,
            data,
        })
    }

    fn classify(&self, rc: c_int, what: &str) -> DecodeError {
        let msg = format!("{what}: {} ({rc})", err_str(rc));
        if rc == ff::AVERROR_INVALIDDATA || rc == ff::AVERROR(libc::EINVAL) {
            DecodeError::Corrupt(msg)
        } else {
            DecodeError::Fatal(msg)
        }
    }

    fn fell_back(&self) -> bool {
        self.callback
            .as_ref()
            .map(|s| s.fell_back.load(Ordering::Relaxed))
            .unwrap_or(false)
    }
}

impl VideoDecoder for FfmpegDecoder {
    fn decode(
        &mut self,
        access_unit: &[u8],
        out: &mut Vec<DecodedImage>,
    ) -> Result<(), DecodeError> {
        if access_unit.is_empty() {
            return Ok(());
        }
        let len = c_int::try_from(access_unit.len())
            .map_err(|_| DecodeError::Corrupt("access unit too large".into()))?;
        unsafe {
            let rc = ff::av_new_packet(self.packet, len);
            if rc < 0 {
                return Err(DecodeError::Fatal(format!(
                    "av_new_packet: {}",
                    err_str(rc)
                )));
            }
            ptr::copy_nonoverlapping(access_unit.as_ptr(), (*self.packet).data, access_unit.len());
            let rc = ff::avcodec_send_packet(self.ctx, self.packet);
            ff::av_packet_unref(self.packet);
            if rc < 0 && rc != ff::AVERROR(libc::EAGAIN) {
                return Err(self.classify(rc, "avcodec_send_packet"));
            }
            loop {
                let rc = ff::avcodec_receive_frame(self.ctx, self.frame);
                if rc == ff::AVERROR(libc::EAGAIN) || rc == ff::AVERROR_EOF {
                    break;
                }
                if rc < 0 {
                    return Err(self.classify(rc, "avcodec_receive_frame"));
                }
                let is_hw = self
                    .hw_pix_fmt
                    .map(|f| (*self.frame).format == f as c_int)
                    .unwrap_or(false);
                let src = if is_hw {
                    let rc = ff::av_hwframe_transfer_data(self.sw_frame, self.frame, 0);
                    if rc < 0 {
                        ff::av_frame_unref(self.frame);
                        return Err(DecodeError::Fatal(format!(
                            "av_hwframe_transfer_data: {}",
                            err_str(rc)
                        )));
                    }
                    self.sw_frame
                } else {
                    self.frame
                };
                let image = self.copy_image(src);
                ff::av_frame_unref(self.frame);
                ff::av_frame_unref(self.sw_frame);
                out.push(image?);
                self.pictures += 1;
            }
        }
        Ok(())
    }

    fn name(&self) -> String {
        match self.hw_name {
            Some(hw) if self.fell_back() => format!("h264 (software, {hw} rejected the stream)"),
            Some(hw) => format!("h264 ({hw})"),
            None => "h264 (software)".to_string(),
        }
    }

    fn is_hardware(&self) -> bool {
        self.hw_name.is_some() && !self.fell_back()
    }
}

impl Drop for FfmpegDecoder {
    fn drop(&mut self) {
        unsafe {
            if !self.ctx.is_null() {
                // Detach the callback state before freeing so FFmpeg never sees a dangling pointer.
                (*self.ctx).opaque = ptr::null_mut();
                ff::avcodec_free_context(&mut self.ctx);
            }
            ff::av_frame_free(&mut self.frame);
            ff::av_frame_free(&mut self.sw_frame);
            ff::av_packet_free(&mut self.packet);
            ff::av_buffer_unref(&mut self.hw_device);
        }
    }
}

#[allow(dead_code)]
fn _assert_error_helpers() -> Result<()> {
    Err(anyhow!("{}", err_str(ff::AVERROR_EOF)))
}
