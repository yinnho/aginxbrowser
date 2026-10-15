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

// --- Baseline JPEG encode (300015 P4): RGBA → 4:2:0 baseline JPEG ---
//
// The XHS create page fingerprints a DRAWN canvas through
// toDataURL('image/jpeg') and checks the bytes are actually JPEG — the
// engine used to return PNG bytes under a `data:image/png` prefix, a
// one-line in-page detect. Hand-rolled for the same reason as the PNG
// encoder above: the `image` crate is a `screenshot`-only dependency and
// the canvas face is core, so every build flavor must serve it.
// Annex K quant/Huffman tables, BT.601 YCbCr with 4:2:0 subsampling,
// libjpeg's quality scaling (default 0.92 = the canvas spec's default).
// Round-trips through the `image` crate in screenshot-gated tests.

/// MSB-first bit stream with 0xFF byte stuffing (Annex B).
struct JBits {
    out: Vec<u8>,
    acc: u64,
    nbits: u32,
}

impl JBits {
    fn new() -> Self {
        Self { out: Vec::new(), acc: 0, nbits: 0 }
    }
    /// Append the low `len` bits of `code`, most-significant first.
    fn put(&mut self, code: u32, len: u32) {
        for i in (0..len).rev() {
            self.acc = (self.acc << 1) | ((code >> i) & 1) as u64;
            self.nbits += 1;
            if self.nbits == 8 {
                let b = (self.acc & 0xFF) as u8;
                self.out.push(b);
                if b == 0xFF {
                    self.out.push(0x00); // stuffing
                }
                self.acc = 0;
                self.nbits = 0;
            }
        }
    }
    /// Pad the final byte with 1-bits (F.1.2.3), then hand over the stream.
    fn flush(mut self) -> Vec<u8> {
        while self.nbits > 0 {
            self.put(1, 1);
        }
        self.out
    }
}

/// Natural index of each position in the zigzag scan order.
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27,
    20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58,
    59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

const QLUMA: [u8; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69,
    56, 14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81,
    104, 113, 92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];
const QCHROMA: [u8; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99,
    99, 47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];

const DC_LUMA_BITS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const DC_LUMA_VALS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const DC_CHROMA_BITS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const DC_CHROMA_VALS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const AC_LUMA_BITS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 125];
const AC_LUMA_VALS: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61,
    0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52,
    0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25,
    0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45,
    0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64,
    0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83,
    0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99,
    0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6,
    0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3,
    0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8,
    0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
];
const AC_CHROMA_BITS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 119];
const AC_CHROMA_VALS: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61,
    0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33,
    0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18,
    0x19, 0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44,
    0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63,
    0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a,
    0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97,
    0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4,
    0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca,
    0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7,
    0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
];

/// Canonical Huffman codes for one table (C.2): code assignments derived
/// from the BITS/HUFFVAL pair, symbols up to 255 (0xF0 ZRL and 0x00 EOB
/// live in the AC tables' own symbol space).
struct HuffTable {
    codes: [u32; 256],
    lens: [u8; 256],
}

fn build_huff(bits: &[u8; 16], vals: &[u8]) -> HuffTable {
    let mut t = HuffTable { codes: [0; 256], lens: [0; 256] };
    let mut code = 0u32;
    let mut k = 0usize;
    for (li, &count) in bits.iter().enumerate() {
        for _ in 0..count {
            let v = vals[k] as usize;
            t.codes[v] = code;
            t.lens[v] = (li + 1) as u8;
            code += 1;
            k += 1;
        }
        code <<= 1;
    }
    t
}

/// libjpeg quality scaling: 1..=100 → the Annex K tables scaled
/// (Chrome passes the spec's default 0.92 → 92).
fn scaled_tables(quality: u32) -> ([u8; 64], [u8; 64]) {
    let q = quality.clamp(1, 100);
    let scale = if q < 50 { 5000 / q } else { 200 - q * 2 };
    let mut l = [0u8; 64];
    let mut c = [0u8; 64];
    for i in 0..64 {
        l[i] = ((QLUMA[i] as u32 * scale + 50) / 100).clamp(1, 255) as u8;
        c[i] = ((QCHROMA[i] as u32 * scale + 50) / 100).clamp(1, 255) as u8;
    }
    (l, c)
}

/// Split RGBA into Y (full res) + Cb/Cr (2×2 averaged, edge-replicated on
/// odd dims). JPEG carries no alpha: transparent pixels composite over
/// black, which is what Chrome's jpeg canvases show.
fn rgba_to_ycbcr(w: usize, h: usize, rgba: &[u8]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    let mut yp = vec![0u8; w * h];
    let mut cb = vec![0u8; cw * ch];
    let mut cr = vec![0u8; cw * ch];
    for py in 0..h {
        for px in 0..w {
            let i = (py * w + px) * 4;
            let a = rgba[i + 3] as u32;
            let r = (rgba[i] as u32 * a + 127) / 255;
            let g = (rgba[i + 1] as u32 * a + 127) / 255;
            let b = (rgba[i + 2] as u32 * a + 127) / 255;
            let yv = 0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64;
            yp[py * w + px] = yv.round().clamp(0.0, 255.0) as u8;
        }
    }
    for cy in 0..ch {
        for cx in 0..cw {
            let (x0, y0) = (cx * 2, cy * 2);
            let (mut rs, mut gs, mut bs) = (0f64, 0f64, 0f64);
            for (dx, dy) in [(0usize, 0usize), (1, 0), (0, 1), (1, 1)] {
                let x = (x0 + dx).min(w - 1);
                let y = (y0 + dy).min(h - 1);
                let i = (y * w + x) * 4;
                let a = rgba[i + 3] as f64 / 255.0;
                rs += rgba[i] as f64 * a;
                gs += rgba[i + 1] as f64 * a;
                bs += rgba[i + 2] as f64 * a;
            }
            let (r, g, b) = (rs / 4.0, gs / 4.0, bs / 4.0);
            let di = cy * cw + cx;
            cb[di] = (128.0 - 0.168736 * r - 0.331264 * g + 0.5 * b).round().clamp(0.0, 255.0)
                as u8;
            cr[di] = (128.0 + 0.5 * r - 0.418688 * g - 0.081312 * b).round().clamp(0.0, 255.0)
                as u8;
        }
    }
    (yp, cb, cr)
}

fn dct_cos() -> &'static [f64; 64] {
    static T: std::sync::OnceLock<[f64; 64]> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0f64; 64];
        for u in 0..8 {
            for x in 0..8 {
                t[u * 8 + x] = ((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI / 16.0).cos();
            }
        }
        t
    })
}

/// Separable forward 8×8 DCT, f64, natural (row-major) order. `src` is
/// already level-shifted (−128). The per-axis scale is C(k)/2 — the spec's
/// ¼·C(u)·C(v) overall factor — so C(0) axis scale is 1/(2√2), NOT 1/√2
/// (that mistake scales DC by 4× and every AC by 2×).
fn fdct8(src: &[f64; 64], out: &mut [f64; 64]) {
    const ALPHA_DC: f64 = std::f64::consts::FRAC_1_SQRT_2 / 2.0;
    let c = dct_cos();
    let mut tmp = [0f64; 64];
    for y in 0..8 {
        for u in 0..8 {
            let mut s = 0.0;
            for x in 0..8 {
                s += src[y * 8 + x] * c[u * 8 + x];
            }
            tmp[y * 8 + u] = s;
        }
    }
    for v in 0..8 {
        for u in 0..8 {
            let mut s = 0.0;
            for y in 0..8 {
                s += tmp[y * 8 + u] * c[v * 8 + y];
            }
            let au = if u == 0 { ALPHA_DC } else { 0.5 };
            let av = if v == 0 { ALPHA_DC } else { 0.5 };
            out[v * 8 + u] = s * au * av;
        }
    }
}

/// Sample one 8×8 block from a plane at (x0, y0), edge-replicating past
/// the plane bounds (odd-dimension padding), level-shifted by −128.
fn sample_block(plane: &[u8], w: usize, h: usize, x0: usize, y0: usize, out: &mut [f64; 64]) {
    for v in 0..8 {
        let y = (y0 + v).min(h - 1);
        for u in 0..8 {
            let x = (x0 + u).min(w - 1);
            out[v * 8 + u] = plane[y * w + x] as f64 - 128.0;
        }
    }
}

/// Bit-length category of a DC diff / AC coefficient value (F.1.2.1).
fn mag_bits(v: i32) -> u32 {
    if v == 0 {
        0
    } else {
        32 - v.unsigned_abs().leading_zeros()
    }
}

#[allow(clippy::too_many_arguments)]
fn encode_block(
    bits: &mut JBits,
    freq: &[f64; 64],
    qtab: &[u8; 64],
    prev_dc: &mut i32,
    dc: &HuffTable,
    ac: &HuffTable,
) {
    // Quantize straight into zigzag order.
    let mut coef = [0i32; 64];
    for k in 0..64 {
        let n = ZIGZAG[k];
        coef[k] = (freq[n] / qtab[n] as f64).round() as i32;
    }
    // DC — category Huffman code, then the diff's magnitude bits
    // (negative values encoded as value − 1, F.1.2.1).
    let diff = coef[0] - *prev_dc;
    *prev_dc = coef[0];
    let s = mag_bits(diff);
    bits.put(dc.codes[s as usize], dc.lens[s as usize] as u32);
    if s > 0 {
        let v = if diff < 0 { diff + (1 << s) - 1 } else { diff };
        bits.put(v as u32, s);
    }
    // AC — run-length of zeros folded into the symbol; ZRL for runs > 15,
    // EOB when the tail is all zero.
    let mut run = 0usize;
    // (needless_range_loop: the zigzag POSITION is the semantics — the
    // run length counts positions, so an index loop it is.)
    #[allow(clippy::needless_range_loop)]
    for k in 1..64 {
        let c = coef[k];
        if c == 0 {
            run += 1;
            continue;
        }
        while run > 15 {
            bits.put(ac.codes[0xF0], ac.lens[0xF0] as u32);
            run -= 16;
        }
        let s = mag_bits(c);
        let sym = ((run as u8) << 4) | s as u8;
        bits.put(ac.codes[sym as usize], ac.lens[sym as usize] as u32);
        let v = if c < 0 { c + (1 << s) - 1 } else { c };
        bits.put(v as u32, s);
        run = 0;
    }
    if run > 0 {
        bits.put(ac.codes[0x00], ac.lens[0x00] as u32);
    }
}

/// Encode a `width × height` RGBA8 buffer as a baseline JPEG (4:2:0,
/// Annex K tables, quality 0.0–1.0, anything else → the 0.92 default).
pub(crate) fn canvas_jpeg_encode(
    width: u32,
    height: u32,
    rgba: &[u8],
    quality: f64,
) -> Result<Vec<u8>, String> {
    let expect = width as usize * height as usize * 4;
    if width == 0 || height == 0 || rgba.len() != expect {
        return Err(format!(
            "canvas buffer is {len} bytes, expected {expect} for {width}x{height}",
            len = rgba.len()
        ));
    }
    // Out-of-range/non-number quality is IGNORED per spec → default 0.92.
    let q100 = if quality.is_finite() && (0.0..=1.0).contains(&quality) {
        (quality * 100.0).round() as u32
    } else {
        92
    };
    let (ql, qc) = scaled_tables(q100);

    let w = width as usize;
    let h = height as usize;
    let (yp, cbp, crp) = rgba_to_ycbcr(w, h, rgba);
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);

    let dc_l = build_huff(&DC_LUMA_BITS, &DC_LUMA_VALS);
    let dc_c = build_huff(&DC_CHROMA_BITS, &DC_CHROMA_VALS);
    let ac_l = build_huff(&AC_LUMA_BITS, &AC_LUMA_VALS);
    let ac_c = build_huff(&AC_CHROMA_BITS, &AC_CHROMA_VALS);

    let mut out = vec![0xFF, 0xD8]; // SOI
    // APP0 / JFIF: version 1.1, no density, no thumbnail.
    out.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]);
    out.extend_from_slice(b"JFIF");
    out.extend_from_slice(&[0x00, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00]);
    // DQT ×2 — 8-bit precision, values in zigzag order.
    for (tab, id) in [(&ql, 0u8), (&qc, 1u8)] {
        out.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, id]);
        for k in 0..64 {
            out.push(tab[ZIGZAG[k]]);
        }
    }
    // SOF0 — 3 components, Y 2×2-sampled, both chroma 1×1.
    out.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
    out.extend_from_slice(&(height as u16).to_be_bytes());
    out.extend_from_slice(&(width as u16).to_be_bytes());
    out.extend_from_slice(&[0x03, 0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01]);
    // DHT ×4 — DC/AC × luma/chroma.
    for (cls, bits, vals) in [
        (0x00u8, &DC_LUMA_BITS[..], &DC_LUMA_VALS[..]),
        (0x10u8, &AC_LUMA_BITS[..], &AC_LUMA_VALS[..]),
        (0x01u8, &DC_CHROMA_BITS[..], &DC_CHROMA_VALS[..]),
        (0x11u8, &AC_CHROMA_BITS[..], &AC_CHROMA_VALS[..]),
    ] {
        let len = 2 + 1 + 16 + vals.len();
        out.extend_from_slice(&[0xFF, 0xC4]);
        out.extend_from_slice(&(len as u16).to_be_bytes());
        out.push(cls);
        out.extend_from_slice(bits);
        out.extend_from_slice(vals);
    }
    // SOS — interleaved scan, Ss=0 Se=63, successive-approximation 0.
    out.extend_from_slice(&[
        0xFF, 0xDA, 0x00, 0x0C, 0x03, 0x01, 0x00, 0x02, 0x11, 0x03, 0x11, 0x00, 0x3F, 0x00,
    ]);

    // Entropy-coded data: 4:2:0 MCUs (4 Y blocks + 1 Cb + 1 Cr), each
    // component with its own DC predictor.
    let mut bits = JBits::new();
    let (mut ydc, mut cbdc, mut crdc) = (0i32, 0i32, 0i32);
    let mut blk = [0f64; 64];
    let mut freq = [0f64; 64];
    let mcols = w.div_ceil(16);
    let mrows = h.div_ceil(16);
    for mr in 0..mrows {
        for mc in 0..mcols {
            for (by, bx) in [(0usize, 0usize), (0, 1), (1, 0), (1, 1)] {
                sample_block(&yp, w, h, mc * 16 + bx * 8, mr * 16 + by * 8, &mut blk);
                fdct8(&blk, &mut freq);
                encode_block(&mut bits, &freq, &ql, &mut ydc, &dc_l, &ac_l);
            }
            sample_block(&cbp, cw, ch, mc * 8, mr * 8, &mut blk);
            fdct8(&blk, &mut freq);
            encode_block(&mut bits, &freq, &qc, &mut cbdc, &dc_c, &ac_c);
            sample_block(&crp, cw, ch, mc * 8, mr * 8, &mut blk);
            fdct8(&blk, &mut freq);
            encode_block(&mut bits, &freq, &qc, &mut crdc, &dc_c, &ac_c);
        }
    }
    out.extend_from_slice(&bits.flush());
    out.extend_from_slice(&[0xFF, 0xD9]); // EOI
    Ok(out)
}

#[op2]
#[buffer]
pub(crate) fn op_canvas_jpeg(
    width: u32,
    height: u32,
    #[buffer] rgba: &[u8],
    quality: f64,
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    canvas_jpeg_encode(width, height, rgba, quality).map_err(deno_error::JsErrorBox::generic)
}

/// Snapshot one canvas's 2D buffer into the page's canvas bitmap table
/// (#210) so the render path can paint it. Called by the realm's flush
/// helper before a render; returns true when the pixels changed. A change
/// is a paint-only invalidation: geometry never moves, but the collected
/// items and the damage signature must move (`drop_paint_only` covers
/// both), so a canvas that just drew reaches the next frame instead of a
/// cached empty box.
#[cfg(feature = "screenshot")]
#[op2(fast)]
pub(crate) fn op_canvas_store(
    state: &OpState,
    nid: u32,
    width: u32,
    height: u32,
    #[buffer] rgba: &[u8],
) -> bool {
    use crate::diting_layout::DecodedImage;
    if width == 0 || height == 0 || rgba.len() != width as usize * height as usize * 4 {
        return false;
    }
    let gs = state.borrow::<SharedState>().clone();
    let g = gs.borrow_mut();
    let id = NodeId::new(nid);
    if let Some(prev) = g.canvas_images.borrow().get(&id) {
        if prev.width == width && prev.height == height && prev.rgba.as_slice() == rgba {
            return false;
        }
    }
    g.canvas_images
        .borrow_mut()
        .insert(id, DecodedImage::new(width, height, rgba.to_vec()));
    g.drop_paint_only();
    true
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
    let raster = book.rasterize(text, px, bold, rgba, px * 1.2, mono, None);
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

    #[test]
    fn canvas_jpeg_emits_valid_baseline_structure() {
        // Odd dims on both axes exercise MCU padding + chroma edge
        // replication; a gradient exercises real AC coefficients.
        let (w, h) = (137u32, 91u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                rgba[i] = (x * 255 / (w - 1)) as u8;
                rgba[i + 1] = (y * 255 / (h - 1)) as u8;
                rgba[i + 2] = 128;
                rgba[i + 3] = 255;
            }
        }
        let jpg = canvas_jpeg_encode(w, h, &rgba, 0.92).unwrap();
        assert_eq!(&jpg[0..2], &[0xFF, 0xD8], "SOI");
        assert_eq!(&jpg[jpg.len() - 2..], &[0xFF, 0xD9], "EOI");
        // Marker walk: everything between SOI and the entropy-coded scan.
        let mut i = 2usize;
        let mut seen = Vec::new();
        loop {
            assert_eq!(jpg[i], 0xFF, "marker prefix at {i}");
            let m = jpg[i + 1];
            if m == 0xDA {
                seen.push(m);
                break;
            }
            let len = u16::from_be_bytes([jpg[i + 2], jpg[i + 3]]) as usize;
            if m == 0xC0 {
                // SOF0: 8-bit, h, w, 3 comps, Y 2×2-sampled, chroma 1×1.
                assert_eq!(jpg[i + 4], 8);
                assert_eq!(u16::from_be_bytes([jpg[i + 5], jpg[i + 6]]), h as u16);
                assert_eq!(u16::from_be_bytes([jpg[i + 7], jpg[i + 8]]), w as u16);
                assert_eq!(jpg[i + 9], 3);
                assert_eq!(jpg[i + 11], 0x22);
            }
            seen.push(m);
            i += 2 + len;
        }
        assert_eq!(seen, vec![0xE0, 0xDB, 0xDB, 0xC0, 0xC4, 0xC4, 0xC4, 0xC4, 0xDA]);
        assert!(canvas_jpeg_encode(4, 2, &[0u8; 7], 0.92).is_err());
    }

    #[test]
    fn canvas_jpeg_quality_scales_output_size() {
        // Noisy pixels so quantization actually bites.
        let (w, h) = (64u32, 64u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        let mut seed = 0x2545F491u32;
        for px in rgba.chunks_exact_mut(4) {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            px[0] = (seed >> 16) as u8;
            px[1] = (seed >> 8) as u8;
            px[2] = seed as u8;
            px[3] = 255;
        }
        let lo = canvas_jpeg_encode(w, h, &rgba, 0.05).unwrap();
        let hi = canvas_jpeg_encode(w, h, &rgba, 0.98).unwrap();
        assert!(
            lo.len() * 2 < hi.len(),
            "q=0.05 {} bytes vs q=0.98 {} bytes",
            lo.len(),
            hi.len()
        );
        assert_eq!(
            canvas_jpeg_encode(w, h, &rgba, f64::NAN).unwrap().len(),
            canvas_jpeg_encode(w, h, &rgba, 0.92).unwrap().len(),
            "non-number quality is ignored → 0.92 default"
        );
    }


    #[cfg(feature = "screenshot")]
    #[test]
    fn canvas_jpeg_round_trips_through_image_crate() {
        let (w, h) = (137u32, 91u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                rgba[i] = (x * 255 / (w - 1)) as u8;
                rgba[i + 1] = (y * 255 / (h - 1)) as u8;
                rgba[i + 2] = 128;
                rgba[i + 3] = 255;
            }
        }
        let jpg = canvas_jpeg_encode(w, h, &rgba, 0.95).unwrap();
        let img = image::load_from_memory(&jpg).expect("decodes as JPEG");
        assert_eq!((img.width(), img.height()), (w, h));
        let rgb = img.to_rgb8();
        let bytes = rgb.as_raw();
        for y in 0..h as usize {
            for x in 0..w as usize {
                let s = (y * w as usize + x) * 4;
                let di = (y * w as usize + x) * 3;
                for c in 0..3 {
                    let diff = (bytes[di + c] as i32 - rgba[s + c] as i32).abs();
                    assert!(diff <= 32, "px ({x},{y}) ch{c}: {} vs {}", bytes[di + c], rgba[s + c]);
                }
            }
        }
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
