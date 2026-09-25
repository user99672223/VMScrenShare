//! Synthetic desktop-like test images, shared by `doctor`, the unit tests and the `e2e` crate.
//!
//! The pattern has smooth gradients, hard edges (a text-like grid) and a moving block, so an
//! encoder produces realistic keyframe and P-frame sizes rather than the near-empty output a
//! flat colour would give.

use crate::convert::{xrgb_to_i420, I420Frame};

/// Renders frame `index` of the moving pattern as `XRGB8888` (bytes B, G, R, X) with
/// `pitch` bytes per row.
pub fn xrgb(width: usize, height: usize, pitch: usize, index: usize) -> Vec<u8> {
    assert!(pitch >= width * 4);
    let mut out = vec![0u8; pitch * height];
    let block = 64.min(width / 4).max(1);
    let bx = (index * 9) % width.saturating_sub(block).max(1);
    let by = (index * 5) % height.saturating_sub(block).max(1);
    for y in 0..height {
        let row = &mut out[y * pitch..][..width * 4];
        for x in 0..width {
            // Background: two gradients plus an 8x16 "character cell" grid with bright lines.
            let mut r = (x * 255 / width.max(1)) as u8;
            let mut g = (y * 255 / height.max(1)) as u8;
            let mut b = ((x / 8 + y / 16 + index / 30) % 2 * 40 + 40) as u8;
            if x % 8 == 0 || y % 16 == 0 {
                r = r.saturating_add(90);
                g = g.saturating_add(90);
                b = b.saturating_add(90);
            }
            // Moving white block with a dark border.
            if x >= bx && x < bx + block && y >= by && y < by + block {
                let edge = x == bx || y == by || x + 1 == bx + block || y + 1 == by + block;
                (r, g, b) = if edge { (10, 10, 10) } else { (245, 245, 245) };
            }
            let px = &mut row[x * 4..x * 4 + 4];
            px[0] = b;
            px[1] = g;
            px[2] = r;
            px[3] = 0xFF;
        }
    }
    out
}

/// Frame `index` of the pattern already converted to I420.
pub fn i420(width: usize, height: usize, index: usize) -> I420Frame {
    let src = xrgb(width, height, width * 4, index);
    let mut frame = I420Frame::new(0, 0);
    xrgb_to_i420(&src, width, height, width * 4, &mut frame);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_is_deterministic_and_moves() {
        let a = xrgb(64, 32, 64 * 4 + 16, 0);
        let b = xrgb(64, 32, 64 * 4 + 16, 0);
        let c = xrgb(64, 32, 64 * 4 + 16, 1);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), (64 * 4 + 16) * 32);
        let f = i420(64, 32, 3);
        assert_eq!((f.width, f.height), (64, 32));
        // Not a flat image.
        let (min, max) =
            f.y.iter()
                .fold((255u8, 0u8), |(lo, hi), &p| (lo.min(p), hi.max(p)));
        assert!(max - min > 100, "luma range {min}..{max}");
    }
}
