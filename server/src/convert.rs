//! Pixel format conversion: DRM `XRGB8888` scanout → planar I420 for the encoder.
//!
//! BT.601 limited-range coefficients (what decoders assume for an H.264 stream without VUI
//! colour information). Chroma is the average of each 2x2 block.
//!
//! Two implementations produce bit-identical output:
//! * a scalar one (every platform, and the reference for the tests), and
//! * an aarch64 NEON one (`vld4q_u8` de-interleaves 16 pixels per load, the Y/U/V arithmetic
//!   runs on 16-bit lanes). NEON is part of the aarch64 baseline, so no runtime detection.
//!
//! Row pairs are distributed over rayon's thread pool; at 1080p the whole conversion takes
//! about a millisecond on the 16-core Ampere VM.
//!
//! [`copy_xrgb`] copies the scanout buffer (a PRIME mmap of the vkms GEM object) into an
//! ordinary heap buffer first: one linear read of the mapping per frame, and the conversion
//! then works on memory it owns.

use rayon::prelude::*;

/// A planar I420 frame with tightly packed planes (Y stride = width, U/V stride = width/2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct I420Frame {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl I420Frame {
    /// Allocates a black frame. `width` and `height` are rounded down to even values.
    pub fn new(width: usize, height: usize) -> Self {
        let width = width & !1;
        let height = height & !1;
        Self {
            width,
            height,
            y: vec![16; width * height],
            u: vec![128; (width / 2) * (height / 2)],
            v: vec![128; (width / 2) * (height / 2)],
        }
    }

    pub fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }
}

/// A tightly packed copy of an `XRGB8888` image (pitch = `width * 4`).
#[derive(Debug, Clone, Default)]
pub struct XrgbImage {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

impl XrgbImage {
    pub fn pitch(&self) -> usize {
        self.width * 4
    }
}

/// Copies the visible part of an `XRGB8888` scanout buffer (`pitch` bytes per row, possibly
/// larger than `width * 4`) into `dst`, tightly packed. Rows are copied in parallel.
///
/// # Panics
/// If `src` is shorter than `(height - 1) * pitch + width * 4`.
pub fn copy_xrgb(src: &[u8], width: usize, height: usize, pitch: usize, dst: &mut XrgbImage) {
    let row_bytes = width * 4;
    assert!(
        pitch >= row_bytes,
        "pitch {pitch} smaller than row of {width} px"
    );
    if width == 0 || height == 0 {
        *dst = XrgbImage::default();
        return;
    }
    assert!(
        src.len() >= (height - 1) * pitch + row_bytes,
        "source buffer too small: {} bytes for {}x{} pitch {}",
        src.len(),
        width,
        height,
        pitch
    );
    if dst.width != width || dst.height != height {
        dst.width = width;
        dst.height = height;
        dst.data = vec![0; row_bytes * height];
    }
    if pitch == row_bytes {
        // Contiguous: a few large memcpys instead of one per row.
        let chunk_rows = height.div_ceil(rayon::current_num_threads().clamp(1, 8));
        dst.data
            .par_chunks_mut(chunk_rows * row_bytes)
            .enumerate()
            .for_each(|(i, chunk)| {
                let start = i * chunk_rows * row_bytes;
                chunk.copy_from_slice(&src[start..start + chunk.len()]);
            });
    } else {
        dst.data
            .par_chunks_mut(row_bytes)
            .enumerate()
            .for_each(|(row, out)| out.copy_from_slice(&src[row * pitch..][..row_bytes]));
    }
}

/// Converts a `XRGB8888` (little-endian: bytes B, G, R, X) image of `width`x`height` pixels
/// whose rows are `pitch` bytes apart into `dst`, resizing `dst` if needed.
///
/// Odd widths/heights are cropped by one pixel (the encoder needs even dimensions).
///
/// # Panics
/// If `src` is shorter than `(height - 1) * pitch + width * 4`.
pub fn xrgb_to_i420(src: &[u8], width: usize, height: usize, pitch: usize, dst: &mut I420Frame) {
    convert(src, width, height, pitch, dst, convert_row_pair);
}

/// Same as [`xrgb_to_i420`] but always uses the scalar code (reference implementation).
pub fn xrgb_to_i420_scalar(
    src: &[u8],
    width: usize,
    height: usize,
    pitch: usize,
    dst: &mut I420Frame,
) {
    convert(src, width, height, pitch, dst, convert_row_pair_scalar);
}

/// Name of the conversion kernel in use, for logs and `doctor`.
pub fn kernel_name() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "NEON"
    } else {
        "scalar"
    }
}

type RowPairFn = fn(&[u8], &[u8], &mut [u8], &mut [u8], &mut [u8], &mut [u8]);

fn convert(
    src: &[u8],
    width: usize,
    height: usize,
    pitch: usize,
    dst: &mut I420Frame,
    row_pair: RowPairFn,
) {
    let width = width & !1;
    let height = height & !1;
    assert!(
        pitch >= width * 4,
        "pitch {pitch} smaller than row of {width} px"
    );
    if width == 0 || height == 0 {
        *dst = I420Frame::new(0, 0);
        return;
    }
    assert!(
        src.len() >= (height - 1) * pitch + width * 4,
        "source buffer too small: {} bytes for {}x{} pitch {}",
        src.len(),
        width,
        height,
        pitch
    );
    if dst.width != width || dst.height != height {
        *dst = I420Frame::new(width, height);
    }
    let cw = width / 2;
    dst.y
        .par_chunks_mut(width * 2)
        .zip(dst.u.par_chunks_mut(cw))
        .zip(dst.v.par_chunks_mut(cw))
        .enumerate()
        .for_each(|(pair, ((y2, u_row), v_row))| {
            let row0 = &src[pair * 2 * pitch..][..width * 4];
            let row1 = &src[(pair * 2 + 1) * pitch..][..width * 4];
            let (y0, y1) = y2.split_at_mut(width);
            row_pair(row0, row1, y0, y1, u_row, v_row);
        });
}

#[inline]
fn luma(r: u32, g: u32, b: u32) -> u8 {
    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8
}

/// Converts two rows (`width` pixels each, `width` even) of BGRX bytes into two Y rows and one
/// U/V row. Platform-specific entry point; falls back to the scalar code for the tail.
#[inline]
fn convert_row_pair(
    row0: &[u8],
    row1: &[u8],
    y0: &mut [u8],
    y1: &mut [u8],
    u: &mut [u8],
    v: &mut [u8],
) {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is mandatory on aarch64; the slices are checked inside.
        let done = unsafe { neon::convert_row_pair(row0, row1, y0, y1, u, v) };
        convert_row_pair_scalar(
            &row0[done * 4..],
            &row1[done * 4..],
            &mut y0[done..],
            &mut y1[done..],
            &mut u[done / 2..],
            &mut v[done / 2..],
        );
    }
    #[cfg(not(target_arch = "aarch64"))]
    convert_row_pair_scalar(row0, row1, y0, y1, u, v);
}

#[inline]
fn convert_row_pair_scalar(
    row0: &[u8],
    row1: &[u8],
    y0: &mut [u8],
    y1: &mut [u8],
    u: &mut [u8],
    v: &mut [u8],
) {
    let px0 = row0.chunks_exact(8);
    let px1 = row1.chunks_exact(8);
    for (i, (p0, p1)) in px0.zip(px1).enumerate() {
        // Two horizontally adjacent pixels per row: bytes [B G R X B G R X].
        let (b00, g00, r00) = (u32::from(p0[0]), u32::from(p0[1]), u32::from(p0[2]));
        let (b01, g01, r01) = (u32::from(p0[4]), u32::from(p0[5]), u32::from(p0[6]));
        let (b10, g10, r10) = (u32::from(p1[0]), u32::from(p1[1]), u32::from(p1[2]));
        let (b11, g11, r11) = (u32::from(p1[4]), u32::from(p1[5]), u32::from(p1[6]));

        y0[2 * i] = luma(r00, g00, b00);
        y0[2 * i + 1] = luma(r01, g01, b01);
        y1[2 * i] = luma(r10, g10, b10);
        y1[2 * i + 1] = luma(r11, g11, b11);

        let r = (r00 + r01 + r10 + r11 + 2) >> 2;
        let g = (g00 + g01 + g10 + g11 + 2) >> 2;
        let b = (b00 + b01 + b10 + b11 + 2) >> 2;
        // Signed arithmetic in i32; the results are within 16..=240 for in-range input.
        let (r, g, b) = (r as i32, g as i32, b as i32);
        u[i] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
        v[i] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
    }
}

#[cfg(target_arch = "aarch64")]
mod neon {
    use std::arch::aarch64::*;

    /// Converts as many pixels as fit in whole 16-pixel blocks and returns that pixel count.
    ///
    /// # Safety
    /// Requires NEON (always present on aarch64). The slices must hold at least `y0.len()`
    /// pixels (`row*`: 4 bytes per pixel, `u`/`v`: half the pixels).
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn convert_row_pair(
        row0: &[u8],
        row1: &[u8],
        y0: &mut [u8],
        y1: &mut [u8],
        u: &mut [u8],
        v: &mut [u8],
    ) -> usize {
        let width = y0
            .len()
            .min(y1.len())
            .min(row0.len() / 4)
            .min(row1.len() / 4);
        let blocks = width / 16;
        if blocks == 0 {
            return 0;
        }
        debug_assert!(u.len() >= blocks * 8 && v.len() >= blocks * 8);

        // Luma coefficients as u8 lanes (the u8 x u8 -> u16 products never overflow:
        // 66*255 + 129*255 + 25*255 + 128 = 56228 < 65536).
        let ky_r = vdup_n_u8(66);
        let ky_g = vdup_n_u8(129);
        let ky_b = vdup_n_u8(25);
        let round_y = vdupq_n_u16(128);
        let off_y = vdupq_n_u16(16);
        // Chroma coefficients on i16 lanes; inputs are 2x2 averages (0..=255), so every
        // partial sum stays well inside i16.
        let ku_r = vdupq_n_s16(-38);
        let ku_g = vdupq_n_s16(-74);
        let ku_b = vdupq_n_s16(112);
        let kv_r = vdupq_n_s16(112);
        let kv_g = vdupq_n_s16(-94);
        let kv_b = vdupq_n_s16(-18);
        let round_c = vdupq_n_s16(128);
        let off_c = vdupq_n_s16(128);
        let two = vdupq_n_u16(2);

        for blk in 0..blocks {
            let px = blk * 16;
            // De-interleave 16 BGRX pixels: .0 = B, .1 = G, .2 = R, .3 = X.
            let p0 = vld4q_u8(row0.as_ptr().add(px * 4));
            let p1 = vld4q_u8(row1.as_ptr().add(px * 4));

            // ---- Y for both rows ----
            for (p, out) in [(p0, y0.as_mut_ptr().add(px)), (p1, y1.as_mut_ptr().add(px))] {
                let (b, g, r) = (p.0, p.1, p.2);
                let lo = vmlal_u8(
                    vmlal_u8(
                        vmlal_u8(round_y, vget_low_u8(r), ky_r),
                        vget_low_u8(g),
                        ky_g,
                    ),
                    vget_low_u8(b),
                    ky_b,
                );
                let hi = vmlal_u8(
                    vmlal_u8(
                        vmlal_u8(round_y, vget_high_u8(r), ky_r),
                        vget_high_u8(g),
                        ky_g,
                    ),
                    vget_high_u8(b),
                    ky_b,
                );
                let lo = vaddq_u16(vshrq_n_u16::<8>(lo), off_y);
                let hi = vaddq_u16(vshrq_n_u16::<8>(hi), off_y);
                vst1q_u8(out, vcombine_u8(vmovn_u16(lo), vmovn_u16(hi)));
            }

            // ---- 2x2 averages (8 chroma samples per block) ----
            // vpaddlq_u8 adds horizontally adjacent bytes into u16 lanes: 16 px -> 8 sums.
            let avg = |c0: uint8x16_t, c1: uint8x16_t| -> int16x8_t {
                let s = vaddq_u16(vaddq_u16(vpaddlq_u8(c0), vpaddlq_u8(c1)), two);
                vreinterpretq_s16_u16(vshrq_n_u16::<2>(s))
            };
            let r = avg(p0.2, p1.2);
            let g = avg(p0.1, p1.1);
            let b = avg(p0.0, p1.0);

            let u_acc = vmlaq_s16(vmlaq_s16(vmlaq_s16(round_c, r, ku_r), g, ku_g), b, ku_b);
            let v_acc = vmlaq_s16(vmlaq_s16(vmlaq_s16(round_c, r, kv_r), g, kv_g), b, kv_b);
            let u_out = vaddq_s16(vshrq_n_s16::<8>(u_acc), off_c);
            let v_out = vaddq_s16(vshrq_n_s16::<8>(v_acc), off_c);
            // Values are within 16..=240: the plain narrowing equals the scalar `as u8`.
            vst1_u8(
                u.as_mut_ptr().add(blk * 8),
                vmovn_u16(vreinterpretq_u16_s16(u_out)),
            );
            vst1_u8(
                v.as_mut_ptr().add(blk * 8),
                vmovn_u16(vreinterpretq_u16_s16(v_out)),
            );
        }
        blocks * 16
    }
}

/// Converts `XRGB8888` to packed `RGB8` (for PNG output).
pub fn xrgb_to_rgb8(src: &[u8], width: usize, height: usize, pitch: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * height * 3);
    for row in 0..height {
        let line = &src[row * pitch..][..width * 4];
        for px in line.chunks_exact(4) {
            out.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: usize, height: usize, pitch: usize, rgb: (u8, u8, u8)) -> Vec<u8> {
        let mut v = vec![0xAAu8; pitch * height];
        for row in 0..height {
            for x in 0..width {
                let o = row * pitch + x * 4;
                v[o] = rgb.2;
                v[o + 1] = rgb.1;
                v[o + 2] = rgb.0;
                v[o + 3] = 0xFF;
            }
        }
        v
    }

    /// Deterministic pseudo-random image (xorshift) covering the whole 0..=255 range; `pitch`
    /// bytes per row (which is why the width does not appear).
    fn noise(height: usize, pitch: usize, seed: u64) -> Vec<u8> {
        let mut s = seed | 1;
        let mut v = vec![0u8; pitch * height];
        for byte in v.iter_mut() {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            *byte = (s >> 24) as u8;
        }
        v
    }

    fn assert_planes(frame: &I420Frame, y: u8, u: u8, v: u8) {
        assert!(
            frame.y.iter().all(|&p| p == y),
            "Y plane: {:?}",
            &frame.y[..8]
        );
        assert!(
            frame.u.iter().all(|&p| p == u),
            "U plane: {:?}",
            &frame.u[..4]
        );
        assert!(
            frame.v.iter().all(|&p| p == v),
            "V plane: {:?}",
            &frame.v[..4]
        );
    }

    #[test]
    fn primaries_match_bt601_limited_range() {
        let mut frame = I420Frame::new(0, 0);
        // (R, G, B) -> expected (Y, U, V) for BT.601 limited range.
        let cases = [
            ((0, 0, 0), (16, 128, 128)),
            ((255, 255, 255), (235, 128, 128)),
            ((255, 0, 0), (82, 90, 240)),
            ((0, 255, 0), (145, 54, 34)),
            ((0, 0, 255), (41, 240, 110)),
            ((128, 128, 128), (126, 128, 128)),
        ];
        for (rgb, (ey, eu, ev)) in cases {
            // 32 pixels wide so the NEON path handles two full blocks.
            let src = solid(32, 4, 32 * 4 + 16, rgb);
            xrgb_to_i420(&src, 32, 4, 32 * 4 + 16, &mut frame);
            assert_eq!((frame.width, frame.height), (32, 4));
            assert_eq!(frame.y.len(), 128);
            assert_eq!(frame.u.len(), 32);
            assert_eq!(frame.v.len(), 32);
            let (y, u, v) = (frame.y[0], frame.u[0], frame.v[0]);
            assert!((i32::from(y) - ey).abs() <= 1, "{rgb:?}: Y {y} != {ey}");
            assert!((i32::from(u) - eu).abs() <= 1, "{rgb:?}: U {u} != {eu}");
            assert!((i32::from(v) - ev).abs() <= 1, "{rgb:?}: V {v} != {ev}");
            assert_planes(&frame, y, u, v);
        }
    }

    #[test]
    fn chroma_is_averaged_over_2x2_blocks() {
        // Left block red, right block blue; averaging inside a block must not bleed across.
        let width = 4;
        let pitch = width * 4;
        let mut src = vec![0u8; pitch * 2];
        for row in 0..2 {
            for x in 0..width {
                let o = row * pitch + x * 4;
                if x < 2 {
                    src[o + 2] = 255; // R
                } else {
                    src[o] = 255; // B
                }
            }
        }
        let mut frame = I420Frame::new(4, 2);
        xrgb_to_i420(&src, width, 2, pitch, &mut frame);
        assert_eq!(frame.u.len(), 2);
        assert!(
            (i32::from(frame.u[0]) - 90).abs() <= 1 && (i32::from(frame.v[0]) - 240).abs() <= 1
        );
        assert!(
            (i32::from(frame.u[1]) - 240).abs() <= 1 && (i32::from(frame.v[1]) - 110).abs() <= 1
        );
        // A block mixing black and white rows averages to mid grey chroma with distinct luma rows.
        let mut src = vec![0u8; pitch * 2];
        for x in 0..width {
            let o = pitch + x * 4;
            src[o] = 255;
            src[o + 1] = 255;
            src[o + 2] = 255;
        }
        xrgb_to_i420(&src, width, 2, pitch, &mut frame);
        assert_eq!(frame.y[0], 16);
        assert_eq!(frame.y[width], 235);
        assert_eq!(frame.u[0], 128);
        assert_eq!(frame.v[0], 128);
    }

    #[test]
    fn odd_dimensions_are_cropped_and_large_frames_work() {
        let (w, h, pitch) = (1921, 1081, 1921 * 4 + 64);
        let src = solid(w, h, pitch, (10, 200, 30));
        let mut frame = I420Frame::new(0, 0);
        xrgb_to_i420(&src, w, h, pitch, &mut frame);
        assert_eq!((frame.width, frame.height), (1920, 1080));
        assert_eq!(frame.y.len(), 1920 * 1080);
        assert_eq!(frame.u.len(), 960 * 540);
        assert_planes(&frame, frame.y[0], frame.u[0], frame.v[0]);
        assert_eq!(frame.strides(), (1920, 960, 960));
    }

    /// The platform kernel (NEON on aarch64) must match the scalar reference bit for bit,
    /// including the non-multiple-of-16 tail and a padded pitch.
    #[test]
    fn platform_kernel_matches_scalar_reference() {
        for &(w, h) in &[
            (2usize, 2usize),
            (16, 2),
            (18, 4),
            (46, 6),
            (1920, 8),
            (1930, 4),
        ] {
            let pitch = w * 4 + 32;
            let src = noise(h, pitch, 0x1234_5678 + w as u64);
            let mut fast = I420Frame::new(0, 0);
            let mut reference = I420Frame::new(0, 0);
            xrgb_to_i420(&src, w, h, pitch, &mut fast);
            xrgb_to_i420_scalar(&src, w, h, pitch, &mut reference);
            assert_eq!(fast, reference, "{w}x{h} mismatch between kernels");
        }
        assert!(!kernel_name().is_empty());
    }

    #[test]
    fn copy_xrgb_packs_rows_and_matches_direct_conversion() {
        let (w, h, pitch) = (100usize, 6usize, 100 * 4 + 48);
        let src = noise(h, pitch, 99);
        let mut copy = XrgbImage::default();
        copy_xrgb(&src, w, h, pitch, &mut copy);
        assert_eq!((copy.width, copy.height, copy.pitch()), (w, h, w * 4));
        assert_eq!(copy.data.len(), w * 4 * h);
        for row in 0..h {
            assert_eq!(
                &copy.data[row * w * 4..][..w * 4],
                &src[row * pitch..][..w * 4]
            );
        }
        let mut from_copy = I420Frame::new(0, 0);
        let mut direct = I420Frame::new(0, 0);
        xrgb_to_i420(&copy.data, w, h, copy.pitch(), &mut from_copy);
        xrgb_to_i420(&src, w, h, pitch, &mut direct);
        assert_eq!(from_copy, direct);
        // Contiguous source (pitch == width * 4) takes the chunked path.
        let tight = noise(h, w * 4, 7);
        copy_xrgb(&tight, w, h, w * 4, &mut copy);
        assert_eq!(copy.data, tight);
        // Reuses the buffer when the size is unchanged, reallocates otherwise.
        let ptr = copy.data.as_ptr();
        copy_xrgb(&tight, w, h, w * 4, &mut copy);
        assert_eq!(copy.data.as_ptr(), ptr);
        copy_xrgb(&tight[..w * 4 * 2], w, 2, w * 4, &mut copy);
        assert_eq!((copy.width, copy.height), (w, 2));
    }

    #[test]
    fn rgb8_conversion_drops_padding_and_reorders_channels() {
        let src = solid(2, 1, 16, (1, 2, 3));
        assert_eq!(xrgb_to_rgb8(&src, 2, 1, 16), vec![1, 2, 3, 1, 2, 3]);
    }
}
