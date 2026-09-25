//! (#117) Canvas fingerprint realism. The JS side's 2D simulation draws
//! into a plain RGBA buffer; the ops here give it the outputs Chrome
//! derives from the same buffer — a real, decodable PNG (`toDataURL`) and
//! (behind `screenshot`, where the swash font stack lives) real glyph
//! rasters for `fillText`. Before this, `toDataURL` returned a fixed-length
//! fake base64 blob that fails any server-side PNG decode, and `fillText`
//! drew random noise boxes.
//!
//! The PNG encoder is hand-rolled — fixed-Huffman DEFLATE + zlib wrapper +
//! chunks — because the `png` crate is a `screenshot`-feature dependency,
//! and the canvas face is core: every build flavor must serve a decodable
//! PNG. The dev-dependency `png` crate is the round-trip oracle in tests.

use super::*;

// --- PNG encode: DEFLATE (fixed Huffman, single block, greedy LZ77) ---

struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    nbits: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self { out: Vec::new(), acc: 0, nbits: 0 }
    }
    /// LSB-first integer bits (extra bits, block headers).
    fn bits(&mut self, value: u32, n: u32) {
        self.acc |= (value as u64) << self.nbits;
        self.nbits += n;
        while self.nbits >= 8 {
            self.out.push((self.acc & 0xFF) as u8);
            self.acc >>= 8;
            self.nbits -= 8;
        }
    }
    /// A Huffman code: packed MSB-first, so the reversed code goes through
    /// the LSB-first stream.
    fn huff(&mut self, code: u32, len: u32) {
        self.bits(code.reverse_bits() >> (32 - len), len);
    }
    fn finish(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            self.out.push((self.acc & 0xFF) as u8);
        }
        self.out
    }
}

fn fixed_literal(bw: &mut BitWriter, b: u8) {
    match b {
        0..=143 => bw.huff(0x30 + b as u32, 8),
        144..=255 => bw.huff(0x190 + (b as u32 - 144), 9),
        // (256..=287 are length/EOB codes, never literals — and not u8s.)
    }
}

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
    131, 163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
    13, 13,
];

fn emit_match(bw: &mut BitWriter, len: usize, dist: usize) {
    let li = LEN_BASE.iter().rposition(|&b| (len as u16) >= b).unwrap();
    let code = 257 + li as u32;
    if code <= 279 {
        bw.huff(code - 256, 7);
    } else {
        bw.huff(0xC0 + (code - 280), 8);
    }
    let extra = LEN_EXTRA[li];
    if extra > 0 {
        bw.bits((len - LEN_BASE[li] as usize) as u32, extra as u32);
    }
    let di = DIST_BASE.iter().rposition(|&b| (dist as u16) >= b).unwrap();
    bw.huff(di as u32, 5);
    let dextra = DIST_EXTRA[di];
    if dextra > 0 {
        bw.bits((dist - DIST_BASE[di] as usize) as u32, dextra as u32);
    }
}

/// Fixed-Huffman DEFLATE of `raw` in one final block. Greedy LZ77 with a
/// single-slot 3-byte hash table — canvas payloads (long transparent runs +
/// small glyph regions) compress well without match chains.
fn deflate_fixed(raw: &[u8]) -> Vec<u8> {
    let mut bw = BitWriter::new();
    bw.bits(1, 1); // BFINAL
    bw.bits(1, 2); // BTYPE=01 fixed Huffman
    const HASH_BITS: u32 = 15;
    const HASH_MASK: usize = (1 << HASH_BITS) as usize - 1;
    let mut head = vec![usize::MAX; 1 << HASH_BITS];
    let hash3 = |r: &[u8], i: usize| -> usize {
        let v = ((r[i] as usize) << 16) ^ ((r[i + 1] as usize) << 8) ^ (r[i + 2] as usize);
        (v.wrapping_mul(0x9E3779B1)) >> (32 - HASH_BITS) & HASH_MASK
    };
    let mut i = 0usize;
    while i < raw.len() {
        let mut best = (0usize, 0usize); // (len, dist)
        if i + 3 <= raw.len() {
            let h = hash3(raw, i);
            let j = head[h];
            if j != usize::MAX && i - j <= 32768 && i >= 3 {
                let mut len = 0usize;
                let max = (raw.len() - i).min(258);
                while len < max && raw[j + len] == raw[i + len] {
                    len += 1;
                }
                if len >= 3 {
                    best = (len, i - j);
                }
            }
            head[h] = i;
        }
        if best.0 >= 3 {
            emit_match(&mut bw, best.0, best.1);
            // Interior positions of the match are findable via their own
            // hashes — insert them so overlapping repeats still match.
            for k in i + 1..i + best.0 {
                if k + 3 <= raw.len() {
                    head[hash3(raw, k)] = k;
                }
            }
            i += best.0;
        } else {
            fixed_literal(&mut bw, raw[i]);
            i += 1;
        }
    }
    bw.huff(0, 7); // EOB (code 256)
    bw.finish()
}

fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn crc32_ieee(parts: &[&[u8]]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (n, slot) in t.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB88320 ^ (c >> 1) } else { c >> 1 };
            }
            *slot = c;
        }
        t
    });
    let mut c = 0xFFFF_FFFFu32;
    for part in parts {
        for &byte in *part {
            c = table[((c ^ byte as u32) & 0xFF) as usize] ^ (c >> 8);
        }
    }
    c ^ 0xFFFF_FFFF
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32_ieee(&[kind, data]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// Encode a `width × height` RGBA8 buffer as a real PNG (filter-0 scanlines,
/// RGBA, 8-bit, non-interlaced). Decodable by any decoder — verified
/// round-trip in tests via the dev-dependency `png` crate.
pub(crate) fn canvas_png_encode(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let expect = width as usize * height as usize * 4;
    if width == 0 || height == 0 || rgba.len() != expect {
        return Err(format!(
            "canvas buffer is {len} bytes, expected {expect} for {width}x{height}",
            len = rgba.len()
        ));
    }
    // Filter-0 scanlines.
    let stride = width as usize * 4;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for y in 0..height as usize {
        raw.push(0u8);
        raw.extend_from_slice(&rgba[y * stride..(y + 1) * stride]);
    }
    let mut zlib = vec![0x78, 0x9C];
    zlib.extend_from_slice(&deflate_fixed(&raw));
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // depth, RGBA, deflate, adaptive, no interlace
    png_chunk(&mut out, b"IHDR", &ihdr);
    png_chunk(&mut out, b"IDAT", &zlib);
    png_chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

#[op2]
#[buffer]
pub(crate) fn op_canvas_png(
    width: u32,
    height: u32,
    #[buffer] rgba: &[u8],
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    canvas_png_encode(width, height, rgba).map_err(deno_error::JsErrorBox::generic)
}

/// A single-line swash raster for `fillText`: the same font book, shaper
/// and raster cache the page painter uses, sized to the canvas font's px.
/// `baseline` is the distance from the tile's top edge to the text
/// baseline; the canvas anchor (alphabetic/top/middle/…) derives its
/// placement from it. Gated on `screenshot` because that is where the
/// font stack lives; without it the JS side keeps its glyph fallback.
#[cfg(feature = "screenshot")]
#[derive(serde::Serialize)]
pub(crate) struct CanvasTextTile {
    width: usize,
    height: usize,
    baseline: f32,
    /// RGBA8, row-major, straight alpha — `width * height * 4` bytes.
    data: Vec<u8>,
}

#[cfg(feature = "screenshot")]
pub(crate) fn canvas_text_tile(
    text: &str,
    size: f64,
    bold: bool,
    mono: bool,
    rgba: [u8; 4],
) -> CanvasTextTile {
    if text.is_empty() {
        return CanvasTextTile { width: 0, height: 0, baseline: 0.0, data: Vec::new() };
    }
    let px = size.clamp(1.0, 512.0) as f32;
    let book = crate::diting_fonts::font_book();
    let raster = book.rasterize(text, px, bold, rgba, px * 1.2, mono);
    CanvasTextTile {
        width: raster.width,
        height: raster.height,
        baseline: raster.baseline,
        data: raster.data.clone(),
    }
}

#[cfg(feature = "screenshot")]
#[op2]
#[serde]
pub(crate) fn op_canvas_text(
    #[string] text: &str,
    size: f64,
    bold: bool,
    mono: bool,
    #[buffer] color: &[u8],
) -> Result<CanvasTextTile, deno_error::JsErrorBox> {
    let rgba = [
        color.first().copied().unwrap_or(0),
        color.get(1).copied().unwrap_or(0),
        color.get(2).copied().unwrap_or(0),
        color.get(3).copied().unwrap_or(255),
    ];
    Ok(canvas_text_tile(text, size, bold, mono, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode through the dev-dependency `png` crate as the oracle: the
    /// whole point of the encoder is that OTHER decoders accept it.
    fn decode(png: &[u8]) -> (u32, u32, Vec<u8>) {
        let mut dec = png::Decoder::new(std::io::Cursor::new(png));
        dec.set_transformations(png::Transformations::EXPAND);
        let mut reader = dec.read_info().expect("header parses");
        let mut buf = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
        let info = reader.next_frame(&mut buf).expect("frame decodes");
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(info.bit_depth, png::BitDepth::Eight);
        (info.width, info.height, buf)
    }

    #[test]
    fn canvas_png_round_trips_various_payloads() {
        let cases: Vec<(u32, u32, Vec<u8>)> = vec![
            // Uniform transparent page (the pristine-canvas case).
            (5, 3, vec![0u8; 5 * 3 * 4]),
            // One opaque pixel in a zero page.
            {
                let mut v = vec![0u8; 7 * 5 * 4];
                v[0] = 210;
                v[3] = 255;
                (7, 5, v)
            },
            // Text-like: two runs with a bright glyph band between them.
            {
                let w = 220u32;
                let h = 30u32;
                let mut v = vec![0u8; (w * h * 4) as usize];
                for y in 2..16 {
                    for x in 0..60 {
                        let i = ((y * w + x) * 4) as usize;
                        v[i] = 0x06;
                        v[i + 1] = 0x69;
                        v[i + 3] = 255;
                    }
                }
                (w, h, v)
            },
        ];
        for (w, h, rgba) in cases {
            let png = canvas_png_encode(w, h, &rgba).unwrap();
            let (dw, dh, dec) = decode(&png);
            assert_eq!((dw, dh), (w, h));
            assert_eq!(dec, rgba, "{w}x{h} round-trips");
        }
    }

    #[test]
    fn canvas_png_rejects_mismatched_buffer_lengths() {
        assert!(canvas_png_encode(4, 2, &[0u8; 7]).is_err());
        assert!(canvas_png_encode(0, 2, &[]).is_err());
    }

    #[test]
    fn canvas_png_compresses_runs() {
        // 26KB of transparent zeros must come out far smaller than raw —
        // fixed-Huffman with LZ77 turns runs into back-references.
        let png = canvas_png_encode(220, 30, &vec![0u8; 220 * 30 * 4]).unwrap();
        assert!(png.len() < 2048, "expected run compression, got {} bytes", png.len());
    }

    #[cfg(feature = "screenshot")]
    #[test]
    fn canvas_text_rasterizes_nonempty_ink() {
        let tile = canvas_text_tile("mm", 14.0, false, false, [10, 20, 30, 255]);
        assert!(tile.width > 0 && tile.height > 0);
        assert_eq!(tile.data.len(), tile.width * tile.height * 4);
        let ink = tile.data.chunks_exact(4).filter(|px| px[3] > 0).count();
        assert!(ink > 0, "glyph raster carries coverage");
        let empty = canvas_text_tile("", 14.0, false, false, [0, 0, 0, 255]);
        assert_eq!((empty.width, empty.height), (0, 0));
    }
}
