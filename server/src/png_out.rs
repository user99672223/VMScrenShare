//! PNG output for `capture --png`.

use std::path::Path;

use anyhow::{Context, Result};

/// Writes an `XRGB8888` image as an 8-bit RGB PNG.
pub fn write_xrgb_png(
    path: &Path,
    src: &[u8],
    width: usize,
    height: usize,
    pitch: usize,
) -> Result<()> {
    let rgb = crate::convert::xrgb_to_rgb8(src, width, height, pitch);
    let file =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut encoder = png::Encoder::new(
        std::io::BufWriter::new(file),
        u32::try_from(width)?,
        u32::try_from(height)?,
    );
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().context("writing PNG header")?;
    writer.write_image_data(&rgb).context("writing PNG data")?;
    writer.finish().context("finishing PNG")?;
    Ok(())
}
