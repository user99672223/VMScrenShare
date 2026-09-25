//! swscale: decoded frame (any planar YUV) → BGRA at the presentation size.

use std::ffi::c_int;
use std::ptr;

use anyhow::{bail, Result};
use ffmpeg_sys_next as ff;

use super::DecodedImage;
use crate::app::RgbFrame;

/// `SWS_BILINEAR`: a `#define` up to FFmpeg 7, an `enum SwsFlags` member since FFmpeg 8; the value
/// is part of the stable API either way.
const SWS_BILINEAR_FLAG: c_int = 2;

pub struct Scaler {
    ctx: *mut ff::SwsContext,
}

unsafe impl Send for Scaler {}

impl Default for Scaler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scaler {
    pub fn new() -> Self {
        Self {
            ctx: ptr::null_mut(),
        }
    }

    /// Converts `img` to `0x00RRGGBB` pixels scaled to `dst_w`x`dst_h`.
    pub fn scale(&mut self, img: &DecodedImage, dst_w: u32, dst_h: u32) -> Result<RgbFrame> {
        let (w, h) = (img.width as c_int, img.height as c_int);
        let (dw, dh) = (dst_w.max(1) as c_int, dst_h.max(1) as c_int);
        unsafe {
            // SAFETY: `pixel_format` came from an AVFrame filled by FFmpeg.
            let src_fmt: ff::AVPixelFormat = std::mem::transmute(img.pixel_format);
            let mut src_data: [*mut u8; 4] = [ptr::null_mut(); 4];
            let mut src_linesize: [c_int; 4] = [0; 4];
            let rc = ff::av_image_fill_arrays(
                src_data.as_mut_ptr(),
                src_linesize.as_mut_ptr(),
                img.data.as_ptr(),
                src_fmt,
                w,
                h,
                1,
            );
            if rc < 0 {
                bail!("av_image_fill_arrays failed ({rc})");
            }
            self.ctx = ff::sws_getCachedContext(
                self.ctx,
                w,
                h,
                src_fmt,
                dw,
                dh,
                ff::AVPixelFormat::AV_PIX_FMT_BGRA,
                SWS_BILINEAR_FLAG,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            );
            if self.ctx.is_null() {
                bail!(
                    "sws_getCachedContext failed ({}x{} fmt {} -> {}x{})",
                    w,
                    h,
                    img.pixel_format,
                    dw,
                    dh
                );
            }
            let mut pixels = vec![0u32; dw as usize * dh as usize];
            let dst_data: [*mut u8; 4] = [
                pixels.as_mut_ptr() as *mut u8,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ];
            let dst_linesize: [c_int; 4] = [dw * 4, 0, 0, 0];
            let rows = ff::sws_scale(
                self.ctx,
                src_data.as_ptr() as *const *const u8,
                src_linesize.as_ptr(),
                0,
                h,
                dst_data.as_ptr(),
                dst_linesize.as_ptr(),
            );
            if rows <= 0 {
                bail!("sws_scale produced no rows ({rows})");
            }
            Ok(RgbFrame {
                width: dw as u32,
                height: dh as u32,
                pixels,
            })
        }
    }
}

impl Drop for Scaler {
    fn drop(&mut self) {
        unsafe {
            if !self.ctx.is_null() {
                ff::sws_freeContext(self.ctx);
            }
        }
    }
}
