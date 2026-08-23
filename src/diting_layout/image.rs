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
//! placeholder.
//!
//! Batch 6c adds [`ImageCache`]: decoded images keyed by their `src`, so a
//! layout re-run (or N imgs sharing one data: URL) decodes once, plus an
//! injected byte table for `http(s)` sources — the caller (the screenshot
//! prefetch path, over diting_net) fetches the bodies; the cache only ever
//! sees bytes. PNG-only for now; JPEG/WebP are later batches.

use std::cell::RefCell;
use std::collections::HashMap;
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

/// Decoded-image store shared across layouts (batch 6c). `resolve(src)`
/// handles the two source forms the pipeline knows:
///
/// - `data:image/png;base64,…` — decoded on first sight, then cached.
/// - `http(s)://…` — looked up in the injected byte table (populated by
///   the caller's fetch pass) and PNG-decoded + cached. A miss stays a
///   miss (the img keeps its 5a placeholder); nothing here touches the
///   network.
pub struct ImageCache<'a> {
    network_bytes: Option<&'a HashMap<String, Vec<u8>>>,
    cache: RefCell<HashMap<String, Arc<DecodedImage>>>,
}

impl Default for ImageCache<'_> {
    fn default() -> Self {
        Self { network_bytes: None, cache: RefCell::new(HashMap::new()) }
    }
}

impl<'a> ImageCache<'a> {
    /// A cache that also resolves http(s) sources against `bytes`
    /// (absolute URL → response body).
    pub fn with_network(bytes: &'a HashMap<String, Vec<u8>>) -> Self {
        Self { network_bytes: Some(bytes), cache: RefCell::new(HashMap::new()) }
    }

    /// Decode (or recall from cache) the image behind an `<img src>`.
    pub fn resolve(&self, src: &str) -> Option<Arc<DecodedImage>> {
        if let Some(hit) = self.cache.borrow().get(src) {
            return Some(Arc::clone(hit));
        }
        let decoded = self.decode_uncached(src)?;
        self.cache.borrow_mut().insert(src.to_string(), Arc::clone(&decoded));
        Some(decoded)
    }

    fn decode_uncached(&self, src: &str) -> Option<Arc<DecodedImage>> {
        if let Some(rest) = src.strip_prefix("data:") {
            return decode_data_url_png(&format!("data:{rest}")).map(Arc::new);
        }
        if (src.starts_with("http://") || src.starts_with("https://"))
            && self.network_bytes.is_some()
        {
            // Content-type is untrusted/absent in practice; sniff the PNG
            // signature instead of trusting the URL extension.
            let bytes = self.network_bytes.unwrap().get(src)?;
            return decode_png(bytes).map(Arc::new);
        }
        None
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

    fn two_by_one_png() -> (Vec<u8>, DecodedImage) {
        let rgba = vec![200u8, 40, 40, 255, 40, 40, 200, 255];
        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, 2, 1);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(&rgba).unwrap();
        }
        (png_bytes, DecodedImage::new(2, 1, rgba))
    }

    /// The cache decodes each distinct src once: repeated resolves hand
    /// back the SAME Arc, and two imgs sharing one data: URL share it too.
    #[test]
    fn cache_dedupes_decodes() {
        let (png_bytes, img) = two_by_one_png();
        let url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&png_bytes)
        );
        let cache = ImageCache::default();
        let a = cache.resolve(&url).expect("data URL decodes");
        let b = cache.resolve(&url).expect("cached");
        assert!(Arc::ptr_eq(&a, &b), "second resolve is the cached Arc");
        assert_eq!((a.width, a.height), (img.width, img.height));
    }

    /// http(s) sources decode from the injected byte table — the fetch is
    /// the caller's job. Unknown URLs stay None (placeholder path); a
    /// non-PNG body also declines.
    #[test]
    fn network_sources_resolve_from_injected_bytes() {
        let (png_bytes, _img) = two_by_one_png();
        let mut bytes = HashMap::new();
        bytes.insert("https://example.com/a.png".to_string(), png_bytes.clone());
        bytes.insert("https://example.com/bad.png".to_string(), b"not a png".to_vec());

        let cache = ImageCache::with_network(&bytes);
        let hit = cache.resolve("https://example.com/a.png").expect("fetched PNG decodes");
        assert_eq!((hit.width, hit.height), (2, 1));
        assert!(
            cache.resolve("https://example.com/bad.png").is_none(),
            "non-PNG body declines"
        );
        assert!(
            cache.resolve("https://example.com/missing.png").is_none(),
            "URL not in the table stays a placeholder"
        );
        // No network table at all: http(s) never resolves.
        assert!(ImageCache::default().resolve("https://example.com/a.png").is_none());
    }
}
