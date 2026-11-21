//! Reading image files: PNG and JPEG into straight-alpha RGBA, made smaller
//! on the way where they are larger than what they are needed for.

use std::path::Path;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

/// A decoded image.
pub struct Image {
    pub w: usize,
    pub h: usize,
    pub rgba: Vec<u8>,
    /// How many times the image was halved to fit.
    pub halved: u32,
}

fn png(data: &[u8]) -> Result<(usize, usize, Vec<u8>), String> {
    let mut decoder = png::Decoder::new(data);
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let (w, h) = (info.width as usize, info.height as usize);
    let px = &buf[..info.buffer_size()];
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => px.to_vec(),
        png::ColorType::Rgb => px.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => px.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => px.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return Err("palette was not expanded".into()),
    };
    Ok((w, h, rgba))
}

fn jpeg(data: &[u8]) -> Result<(usize, usize, Vec<u8>), String> {
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(data), options);
    let rgba = decoder.decode().map_err(|e| e.to_string())?;
    let info = decoder.info().ok_or("no image header")?;
    Ok((info.width as usize, info.height as usize, rgba))
}

/// Average 2x2 blocks, weighting colour by alpha so that transparent
/// pixels do not darken their neighbours.
fn halve(w: usize, h: usize, rgba: &[u8]) -> (usize, usize, Vec<u8>) {
    let (nw, nh) = (w / 2, h / 2);
    let mut small = vec![0u8; nw * nh * 4];
    for (i, out) in small.chunks_exact_mut(4).enumerate() {
        let (x, y) = (i % nw * 2, i / nw * 2);
        let mut sum = [0u32; 4];
        for at in [(y * w + x) * 4, (y * w + x + 1) * 4, ((y + 1) * w + x) * 4, ((y + 1) * w + x + 1) * 4] {
            let a = rgba[at + 3] as u32;
            for c in 0..3 {
                sum[c] += rgba[at + c] as u32 * a;
            }
            sum[3] += a;
        }
        for c in 0..3 {
            out[c] = sum[c].checked_div(sum[3]).unwrap_or(0) as u8;
        }
        out[3] = (sum[3] / 4) as u8;
    }
    (nw, nh, small)
}

/// Read the image in `path`, whichever of the two formats it is in
/// (whatever its name says), halving it until it is no larger than
/// `max_w` x `max_h`.
pub fn load(path: &Path, max_w: usize, max_h: usize) -> Result<Image, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let (mut w, mut h, mut rgba) = if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        png(&data)?
    } else if data.starts_with(&[0xff, 0xd8]) {
        jpeg(&data)?
    } else {
        return Err("neither a PNG nor a JPEG image".into());
    };
    if w == 0 || h == 0 || rgba.len() != w * h * 4 {
        return Err("unexpected amount of pixel data".into());
    }
    let mut halved = 0;
    while (w > max_w || h > max_h) && w >= 2 && h >= 2 {
        (w, h, rgba) = halve(w, h, &rgba);
        halved += 1;
    }
    Ok(Image { w, h, rgba, halved })
}
