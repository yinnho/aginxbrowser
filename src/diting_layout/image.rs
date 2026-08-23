//! data: URL PNG decoding (render claim batch 5b).
//!
//! The first real pixels behind an `<img>`: a self-contained, no-network
//! data source whose bytes both engines can share — our side decodes the
//! `src` attribute here, the blitz cross-check injects the SAME decoded
//! RGBA into its img node (upstream loads images through its net layer,
//! which the test harness does not wire).
//!
//! `decode_data_url_png` accepts `data:image/png[;base64],<payload>` only:
//! base64 is the one encoding real pages use for inline images, and the
//! unencoded (percent-encoded) form adds a decoder for zero real coverage.
//! Non-PNG media and network URLs return `None` — the img keeps its batch-5a
//! placeholder. Decoding runs per layout call; a decoded-image cache keyed
//! by URL is deferred until a product path (diting_net fetch) needs it.

use std::sync::Arc;

use base64::Engine as _;

/// One decoded raster image: RGBA8, row-major, `width × height`.
#[derive(Debug, Clone)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// Straight-alpha RGBA8, `width * height * 4` bytes.
    pub rgba: Arc<Vec<u8>>,
}

impl DecodedImage {
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Self {
        Self { width, height, rgba: Arc::new(rgba) }
    }
}

/// Decode a `data:image/png;base64,…` URL into a [`DecodedImage`].
/// Returns `None` for anything else (other media types, plain data URLs,
/// malformed base64/PNG) — callers fall back to the placeholder path.
pub fn decode_data_url_png(src: &str) -> Option<DecodedImage> {
    let rest = src.strip_prefix("data:")?;
    let comma = rest.find(',')?;
    let meta = &rest[..comma];
    let payload = &rest[comma + 1..];
    if !meta.starts_with("image/png") || !meta.contains(";base64") {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(payload).ok()?;
    decode_png(&bytes)
}

/// Decode PNG bytes to RGBA8 (palette and sub-byte formats expanded, gray
/// promoted, missing alpha filled opaque) — the same normalization upstream
/// applies before handing `RasterImageData` to the painter.
pub fn decode_png(bytes: &[u8]) -> Option<DecodedImage> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity(buf.len() / 3 * 4);
            for px in buf.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity(buf.len() * 4);
            for px in buf.chunks_exact(1) {
                out.extend_from_slice(&[px[0], px[0], px[0], 255]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity(buf.len() / 2 * 4);
            for px in buf.chunks_exact(2) {
                out.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
            out
        }
        png::ColorType::Indexed => return None, // EXPAND promised to remove this
    };
    Some(DecodedImage::new(w, h, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip: encode a two-quadrant RGBA PNG, wrap it in a data URL,
    /// decode it back — bytes and dimensions identical.
    #[test]
    fn data_url_png_round_trip() {
        let (w, h) = (4u32, 2u32);
        let mut rgba = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let left = x < w / 2;
                rgba.extend_from_slice(if left ^ (y < h / 2) {
                    &[200, 40, 40, 255]
                } else {
                    &[40, 40, 200, 255]
                });
            }
        }

        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(&rgba).unwrap();
        }

        let url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&png_bytes)
        );
        let img = decode_data_url_png(&url).expect("decodes");
        assert_eq!((img.width, img.height), (w, h));
        assert_eq!(img.rgba.as_slice(), rgba.as_slice());
    }

    /// Non-PNG media, missing base64, and garbage payloads all decline
    /// (None → the caller keeps the placeholder path).
    #[test]
    fn data_url_rejects_non_png_and_garbage() {
        assert!(decode_data_url_png("https://example.com/x.png").is_none());
        assert!(decode_data_url_png("data:image/jpeg;base64,AAAA").is_none());
        assert!(decode_data_url_png("data:image/png,raw-not-supported").is_none());
        assert!(decode_data_url_png("data:image/png;base64,!!!not-b64!!!").is_none());
    }
}
