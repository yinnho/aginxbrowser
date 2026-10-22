//! Bytes of one font face: heap-owned, or a read-only memory map.
//!
//! A system color-emoji font is huge (Apple Color Emoji is 183 MB) and a page
//! touches a few dozen KB of it. Reading it into a `Vec` made every process
//! that ever saw one emoji hold the whole file; a map keeps only the touched
//! pages resident, as clean file pages the OS can drop under pressure.

use std::hash::{Hash, Hasher};
use std::ops::Deref;

pub enum FaceBytes {
    Owned(Vec<u8>),
    /// `ident` stands in for the content in the book fingerprint (hashing a
    /// map would fault the whole file in): path, length and mtime.
    Mapped { map: memmap2::Mmap, ident: u64 },
}

impl FaceBytes {
    /// Map a system font file. `None` when it is absent or unreadable.
    pub fn map_file(path: &std::path::Path) -> Option<Self> {
        let file = std::fs::File::open(path).ok()?;
        let meta = file.metadata().ok()?;
        // SAFETY: a read-only map of an OS-owned font file; nothing in this
        // process writes it, and system font files are not modified in place.
        let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut h);
        meta.len().hash(&mut h);
        meta.modified().ok().hash(&mut h);
        Some(FaceBytes::Mapped { map, ident: h.finish() })
    }
}

impl Deref for FaceBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            FaceBytes::Owned(v) => v,
            FaceBytes::Mapped { map, .. } => map,
        }
    }
}

impl From<Vec<u8>> for FaceBytes {
    fn from(v: Vec<u8>) -> Self {
        FaceBytes::Owned(v)
    }
}

impl Hash for FaceBytes {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            FaceBytes::Owned(v) => v.hash(state),
            FaceBytes::Mapped { ident, map } => {
                ident.hash(state);
                map.len().hash(state);
            }
        }
    }
}
