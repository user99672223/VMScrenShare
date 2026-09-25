//! Pixel format conversion: DRM `XRGB8888` scanout → planar I420 for the encoder.
//!
//! BT.601 limited-range coefficients (what decoders assume for an H.264 stream without VUI
//! colour information). Chroma is the average of each 2x2 block. Row pairs are converted in
//! parallel with rayon; the VM has plenty of cores and this keeps the capture thread well
//! under one frame time at 1080p.

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

/// Converts a `XRGB8888` (little-endian: bytes B, G, R, X) image of `width`x`height` pixels
/// whose rows are `pitch` bytes apart into `dst`, resizing `dst` if needed.
///
/// Odd widths/heights are cropped by one pixel (the encoder needs even dimensions).
///
/// # Panics
/// If `src` is shorter than `(height - 1) * pitch + width * 4`.
pub fn xrgb_to_i420(src: &[u8], width: usize, height: usize, pitch: usize, dst: &mut I420Frame) {
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
            convert_row_pair(row0, row1, y0, y1, u_row, v_row);
        });
}

#[inline]
fn luma(r: u32, g: u32, b: u32) -> u8 {
    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8
}

#[inline]
fn convert_row_pair(
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
            let src = solid(8, 4, 8 * 4 + 16, rgb);
            xrgb_to_i420(&src, 8, 4, 8 * 4 + 16, &mut frame);
            assert_eq!((frame.width, frame.height), (8, 4));
            assert_eq!(frame.y.len(), 32);
            assert_eq!(frame.u.len(), 8);
            assert_eq!(frame.v.len(), 8);
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

    #[test]
    fn rgb8_conversion_drops_padding_and_reorders_channels() {
        let src = solid(2, 1, 16, (1, 2, 3));
        assert_eq!(xrgb_to_rgb8(&src, 2, 1, 16), vec![1, 2, 3, 1, 2, 3]);
    }
}
