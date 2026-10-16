//! Image header dimension sniffing (god-file ratchet split from mod.rs, pure move).

/// Natural dimensions from an image header alone — PNG IHDR, GIF logical
/// screen descriptor, JPEG SOFn, WebP canvas (VP8X / VP8 / VP8L). Sites
/// gate on `naturalWidth != 0` to decide whether an artifact decoded (the
/// xhs zeus AB SDK retries its PNG artifact 25× and dead-ends when it
/// reads 0), so the Image shim needs real numbers — but a full software
/// decoder lives behind the `screenshot` feature, and bootstrap.js must
/// work in every feature combination. Header parsing is dependency-free.
/// Truncated/garbage input is `None`, never a panic.
pub(super) fn image_header_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    // PNG: 8-byte signature, then chunk length + "IHDR", then BE u32 w/h.
    if b.len() >= 24
        && b[..8] == [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n']
    {
        let w = u32::from_be_bytes([b[16], b[17], b[18], b[19]]);
        let h = u32::from_be_bytes([b[20], b[21], b[22], b[23]]);
        return Some((w, h));
    }
    // GIF: LE u16 logical screen size right after the 6-byte version.
    if b.len() >= 10 && (b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")) {
        return Some((
            u16::from_le_bytes([b[6], b[7]]) as u32,
            u16::from_le_bytes([b[8], b[9]]) as u32,
        ));
    }
    // JPEG: walk the marker segments (EXIF/AppN sit before SOF). Standalone
    // markers carry no length; fill bytes (extra 0xFFs) are skipped.
    if b.len() >= 2 && b[0] == 0xFF && b[1] == 0xD8 {
        let mut i = 2usize;
        while i + 1 < b.len() {
            if b[i] != 0xFF {
                return None; // desynced — not a marker boundary
            }
            let marker = b[i + 1];
            if marker == 0xFF {
                i += 1; // fill byte before a real marker
                continue;
            }
            if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
                i += 2; // standalone, no length payload
                continue;
            }
            if i + 4 > b.len() {
                return None;
            }
            let seg_len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
            let is_sof = (0xC0..=0xCF).contains(&marker)
                && marker != 0xC4 // DHT
                && marker != 0xC8 // JPG (reserved)
                && marker != 0xCC; // DAC
            if is_sof {
                if i + 9 > b.len() {
                    return None;
                }
                // Segment: len(2) precision(1) height(2) width(2).
                let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
                let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
                return Some((w, h));
            }
            i += 2 + seg_len;
        }
        return None;
    }
    // WebP: RIFF container, first chunk fourcc at 12. VP8X carries the
    // canvas size as two 24-bit LE minus-one values; VP8 (lossy) hides it
    // behind the 3-byte frame tag + start code; VP8L packs two 14-bit
    // minus-one values into the bits after the 0x2F signature.
    if b.len() >= 16 && b[..4] == *b"RIFF" && b[8..12] == *b"WEBP" {
        let fourcc = &b[12..16];
        if fourcc == b"VP8X" && b.len() >= 30 {
            let w = 1 + (b[24] as u32 | (b[25] as u32) << 8 | (b[26] as u32) << 16);
            let h = 1 + (b[27] as u32 | (b[28] as u32) << 8 | (b[29] as u32) << 16);
            return Some((w, h));
        }
        if fourcc == b"VP8 " && b.len() >= 30 && b[23..26] == [0x9D, 0x01, 0x2A] {
            let w = u16::from_le_bytes([b[26], b[27]]) as u32 & 0x3FFF;
            let h = u16::from_le_bytes([b[28], b[29]]) as u32 & 0x3FFF;
            return Some((w, h));
        }
        if fourcc == b"VP8L" && b.len() >= 25 && b[20] == 0x2F {
            let w = 1 + (b[21] as u32 | ((b[22] as u32) & 0x3F) << 8);
            let h = 1 + ((b[22] as u32) >> 6 | (b[23] as u32) << 2 | ((b[24] as u32) & 0x0F) << 10);
            return Some((w, h));
        }
        return None;
    }
    None
}
