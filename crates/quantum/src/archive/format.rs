//! On-disk layout of a `.quantum` archive.
//!
//! ```text
//!   +--------------------------------------------------+
//!   | header      16 bytes: magic, version, level, ...  |
//!   | data blocks  compressed, independent, in order    |
//!   | metadata     one compressed block: index + tables |
//!   | footer      40 bytes: where the metadata lives    |
//!   +--------------------------------------------------+
//! ```
//!
//! The index lives at the *end* so the writer can stream blocks out as they
//! are produced without knowing the final layout up front, and the reader
//! still finds everything with one seek.

use crate::error::{Error, Result};
use crate::hash::ChunkId;
use crate::varint::{self, Reader};

pub const MAGIC: [u8; 4] = *b"QNTM";
pub const FOOTER_MAGIC: [u8; 4] = *b"MTNQ";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 16;
pub const FOOTER_LEN: usize = 40;

/// Archive-wide flags.
pub const FLAG_DEDUP: u16 = 1 << 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub version: u8,
    pub level: u8,
    pub flags: u16,
    pub block_size: u32,
}

impl Header {
    pub fn write(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..4].copy_from_slice(&MAGIC);
        out[4] = self.version;
        out[5] = self.level;
        out[6..8].copy_from_slice(&self.flags.to_le_bytes());
        out[8..12].copy_from_slice(&self.block_size.to_le_bytes());
        out
    }

    pub fn parse(bytes: &[u8]) -> Result<Header> {
        if bytes.len() < HEADER_LEN || bytes[0..4] != MAGIC {
            return Err(Error::BadMagic);
        }
        let version = bytes[4];
        if version != VERSION {
            return Err(Error::UnsupportedVersion(version));
        }
        Ok(Header {
            version,
            level: bytes[5],
            flags: u16::from_le_bytes([bytes[6], bytes[7]]),
            block_size: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
        })
    }

    pub fn dedup(&self) -> bool {
        self.flags & FLAG_DEDUP != 0
    }
}

/// Where the index is and how to decode it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Footer {
    pub meta_offset: u64,
    pub meta_comp_len: u64,
    pub meta_raw_len: u64,
    pub meta_checksum: u64,
    pub meta_method: u8,
    pub meta_filter: u8,
}

impl Footer {
    pub fn write(&self) -> [u8; FOOTER_LEN] {
        let mut out = [0u8; FOOTER_LEN];
        out[0..8].copy_from_slice(&self.meta_offset.to_le_bytes());
        out[8..16].copy_from_slice(&self.meta_comp_len.to_le_bytes());
        out[16..24].copy_from_slice(&self.meta_raw_len.to_le_bytes());
        out[24..32].copy_from_slice(&self.meta_checksum.to_le_bytes());
        out[32] = self.meta_method;
        out[33] = self.meta_filter;
        out[36..40].copy_from_slice(&FOOTER_MAGIC);
        out
    }

    pub fn parse(bytes: &[u8]) -> Result<Footer> {
        if bytes.len() < FOOTER_LEN || bytes[36..40] != FOOTER_MAGIC {
            return Err(Error::Corrupt("archive footer is missing or damaged"));
        }
        Ok(Footer {
            meta_offset: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
            meta_comp_len: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            meta_raw_len: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
            meta_checksum: u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
            meta_method: bytes[32],
            meta_filter: bytes[33],
        })
    }
}

/// One compressed block and the range of chunks it holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockRec {
    pub offset: u64,
    pub comp_len: u64,
    pub raw_len: u64,
    pub first_chunk: u64,
    pub n_chunks: u64,
    pub method: u8,
    pub filter: u8,
    pub checksum: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

impl Kind {
    fn to_byte(self) -> u8 {
        match self {
            Kind::File => 0,
            Kind::Dir => 1,
            Kind::Symlink => 2,
        }
    }

    fn from_byte(b: u8) -> Result<Kind> {
        match b {
            0 => Ok(Kind::File),
            1 => Ok(Kind::Dir),
            2 => Ok(Kind::Symlink),
            _ => Err(Error::Corrupt("entry has an unknown type")),
        }
    }
}

/// One archived path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Always relative, `/`-separated, and free of `.`/`..` components.
    pub path: String,
    pub kind: Kind,
    /// Unix permission bits; 0 when the source filesystem had none.
    pub mode: u32,
    /// Modification time in seconds since the Unix epoch.
    pub mtime: i64,
    pub size: u64,
    /// SHA-256/128 of the file's contents, checked on extraction.
    pub content_id: ChunkId,
    /// Indices into the archive's chunk table.
    pub chunks: Vec<u32>,
    /// Target of a symlink; empty otherwise.
    pub link_target: String,
}

impl Entry {
    pub fn is_file(&self) -> bool {
        self.kind == Kind::File
    }
}

/// Everything needed to interpret the data blocks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Metadata {
    pub chunk_lens: Vec<u32>,
    pub blocks: Vec<BlockRec>,
    pub entries: Vec<Entry>,
}

impl Metadata {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.entries.len() * 64 + self.chunk_lens.len() * 3);

        varint::write_usize(&mut out, self.chunk_lens.len());
        for &len in &self.chunk_lens {
            varint::write_u64(&mut out, len as u64);
        }

        varint::write_usize(&mut out, self.blocks.len());
        let mut prev_end = 0u64;
        let mut prev_chunk = 0u64;
        for b in &self.blocks {
            // Blocks are written back to back, so the gap is almost always 0.
            varint::write_u64(&mut out, b.offset.saturating_sub(prev_end));
            varint::write_u64(&mut out, b.comp_len);
            varint::write_u64(&mut out, b.raw_len);
            varint::write_u64(&mut out, b.first_chunk.saturating_sub(prev_chunk));
            varint::write_u64(&mut out, b.n_chunks);
            out.push(b.method);
            out.push(b.filter);
            out.extend_from_slice(&b.checksum.to_le_bytes());
            prev_end = b.offset + b.comp_len;
            prev_chunk = b.first_chunk;
        }

        varint::write_usize(&mut out, self.entries.len());
        for e in &self.entries {
            varint::write_bytes(&mut out, e.path.as_bytes());
            out.push(e.kind.to_byte());
            varint::write_u64(&mut out, e.mode as u64);
            varint::write_i64(&mut out, e.mtime);
            match e.kind {
                Kind::File => {
                    varint::write_u64(&mut out, e.size);
                    out.extend_from_slice(&e.content_id);
                    varint::write_usize(&mut out, e.chunks.len());
                    // Runs of new chunks are consecutive, so deltas are almost
                    // always 1 and cost a single byte.
                    let mut prev = 0i64;
                    for &c in &e.chunks {
                        varint::write_i64(&mut out, c as i64 - prev);
                        prev = c as i64;
                    }
                }
                Kind::Symlink => varint::write_bytes(&mut out, e.link_target.as_bytes()),
                Kind::Dir => {}
            }
        }
        out
    }

    pub fn decode(data: &[u8]) -> Result<Metadata> {
        let mut r = Reader::new(data);
        let n_chunks = r.usize()?;
        // A truncated buffer cannot describe more chunks than it has bytes.
        if n_chunks > data.len() {
            return Err(Error::Corrupt("chunk table is implausibly large"));
        }
        let mut chunk_lens = Vec::with_capacity(n_chunks);
        for _ in 0..n_chunks {
            chunk_lens.push(u32::try_from(r.u64()?).map_err(|_| Error::Corrupt("chunk too long"))?);
        }

        let n_blocks = r.usize()?;
        if n_blocks > data.len() {
            return Err(Error::Corrupt("block table is implausibly large"));
        }
        let mut blocks = Vec::with_capacity(n_blocks);
        let mut prev_end = 0u64;
        let mut prev_chunk = 0u64;
        for _ in 0..n_blocks {
            let offset = prev_end + r.u64()?;
            let comp_len = r.u64()?;
            let raw_len = r.u64()?;
            let first_chunk = prev_chunk + r.u64()?;
            let n_chunks_in = r.u64()?;
            let method = r.array::<1>()?[0];
            let filter = r.array::<1>()?[0];
            let checksum = u64::from_le_bytes(r.array::<8>()?);
            prev_end = offset.checked_add(comp_len).ok_or(Error::Corrupt("block overflows"))?;
            prev_chunk = first_chunk;
            blocks.push(BlockRec {
                offset,
                comp_len,
                raw_len,
                first_chunk,
                n_chunks: n_chunks_in,
                method,
                filter,
                checksum,
            });
        }

        let n_entries = r.usize()?;
        if n_entries > data.len() {
            return Err(Error::Corrupt("entry table is implausibly large"));
        }
        let mut entries = Vec::with_capacity(n_entries);
        for _ in 0..n_entries {
            let path = String::from_utf8(r.bytes()?.to_vec())
                .map_err(|_| Error::Corrupt("entry path is not valid UTF-8"))?;
            let kind = Kind::from_byte(r.array::<1>()?[0])?;
            let mode = r.u64()? as u32;
            let mtime = r.i64()?;
            let mut entry = Entry {
                path,
                kind,
                mode,
                mtime,
                size: 0,
                content_id: [0; 16],
                chunks: Vec::new(),
                link_target: String::new(),
            };
            match kind {
                Kind::File => {
                    entry.size = r.u64()?;
                    entry.content_id = r.array::<16>()?;
                    let n = r.usize()?;
                    if n > data.len() {
                        return Err(Error::Corrupt("entry claims more chunks than possible"));
                    }
                    entry.chunks.reserve(n);
                    let mut prev = 0i64;
                    for _ in 0..n {
                        let id = prev + r.i64()?;
                        let id = u32::try_from(id)
                            .map_err(|_| Error::Corrupt("entry references a negative chunk"))?;
                        if id as usize >= chunk_lens.len() {
                            return Err(Error::Corrupt("entry references a missing chunk"));
                        }
                        entry.chunks.push(id);
                        prev = id as i64;
                    }
                }
                Kind::Symlink => {
                    entry.link_target = String::from_utf8(r.bytes()?.to_vec())
                        .map_err(|_| Error::Corrupt("link target is not valid UTF-8"))?;
                }
                Kind::Dir => {}
            }
            entries.push(entry);
        }
        Ok(Metadata { chunk_lens, blocks, entries })
    }
}

/// Reject paths that would let an archive write outside the destination.
///
/// Extraction is the one place where an untrusted archive can do real damage,
/// so this is deliberately strict: relative paths only, no `..`, no roots, no
/// Windows drive letters, no NUL.
pub fn validate_path(path: &str) -> Result<()> {
    let bad = || Error::UnsafePath(path.to_string());
    if path.is_empty() || path.len() > 4096 {
        return Err(bad());
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err(bad());
    }
    if path.contains('\0') || path.contains('\\') {
        return Err(bad());
    }
    // "C:" style prefixes, which some platforms resolve as absolute.
    if path.as_bytes().get(1) == Some(&b':') {
        return Err(bad());
    }
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(bad());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Metadata {
        Metadata {
            chunk_lens: vec![1024, 2048, 40, 999999],
            blocks: vec![
                BlockRec {
                    offset: 16,
                    comp_len: 500,
                    raw_len: 3112,
                    first_chunk: 0,
                    n_chunks: 3,
                    method: 1,
                    filter: 0,
                    checksum: 0xDEAD_BEEF_CAFE_1234,
                },
                BlockRec {
                    offset: 516,
                    comp_len: 700,
                    raw_len: 999999,
                    first_chunk: 3,
                    n_chunks: 1,
                    method: 0,
                    filter: 1,
                    checksum: 7,
                },
            ],
            entries: vec![
                Entry {
                    path: "dir".into(),
                    kind: Kind::Dir,
                    mode: 0o755,
                    mtime: 1_700_000_000,
                    size: 0,
                    content_id: [0; 16],
                    chunks: vec![],
                    link_target: String::new(),
                },
                Entry {
                    path: "dir/a.txt".into(),
                    kind: Kind::File,
                    mode: 0o644,
                    mtime: -5,
                    size: 3112,
                    content_id: [9; 16],
                    chunks: vec![0, 1, 2],
                    link_target: String::new(),
                },
                Entry {
                    path: "dir/dup.txt".into(),
                    kind: Kind::File,
                    mode: 0o600,
                    mtime: 0,
                    size: 1024,
                    content_id: [3; 16],
                    // Deliberately out of order: a deduplicated file points
                    // back at chunks written earlier.
                    chunks: vec![2, 0],
                    link_target: String::new(),
                },
                Entry {
                    path: "link".into(),
                    kind: Kind::Symlink,
                    mode: 0o777,
                    mtime: 12345,
                    size: 0,
                    content_id: [0; 16],
                    chunks: vec![],
                    link_target: "dir/a.txt".into(),
                },
            ],
        }
    }

    #[test]
    fn metadata_round_trips() {
        let meta = sample();
        let bytes = meta.encode();
        assert_eq!(Metadata::decode(&bytes).unwrap(), meta);
    }

    #[test]
    fn empty_metadata_round_trips() {
        let meta = Metadata::default();
        assert_eq!(Metadata::decode(&meta.encode()).unwrap(), meta);
    }

    #[test]
    fn header_and_footer_round_trip() {
        let h = Header { version: VERSION, level: 7, flags: FLAG_DEDUP, block_size: 1 << 24 };
        assert_eq!(Header::parse(&h.write()).unwrap(), h);
        assert!(h.dedup());

        let f = Footer {
            meta_offset: 1 << 40,
            meta_comp_len: 4242,
            meta_raw_len: 90000,
            meta_checksum: u64::MAX,
            meta_method: 1,
            meta_filter: 0,
        };
        assert_eq!(Footer::parse(&f.write()).unwrap(), f);
    }

    #[test]
    fn bad_headers_are_rejected() {
        assert!(matches!(Header::parse(b"NOPE............").unwrap_err(), Error::BadMagic));
        let mut h = Header { version: VERSION, level: 1, flags: 0, block_size: 0 }.write();
        h[4] = 99;
        assert!(matches!(Header::parse(&h).unwrap_err(), Error::UnsupportedVersion(99)));
        assert!(Header::parse(b"QNTM").is_err(), "short header must be rejected");
        assert!(Footer::parse(&[0u8; FOOTER_LEN]).is_err());
    }

    #[test]
    fn truncated_metadata_is_rejected_not_panicked() {
        let bytes = sample().encode();
        for cut in 0..bytes.len() {
            // Every prefix must produce an error rather than a panic.
            let _ = Metadata::decode(&bytes[..cut]);
        }
    }

    #[test]
    fn corrupt_metadata_is_rejected_not_panicked() {
        let bytes = sample().encode();
        for i in 0..bytes.len() {
            for bit in [0x01u8, 0x80] {
                let mut damaged = bytes.clone();
                damaged[i] ^= bit;
                let _ = Metadata::decode(&damaged);
            }
        }
    }

    #[test]
    fn unsafe_paths_are_rejected() {
        for good in ["a", "a/b", "a/b/c.txt", "weird name .tar.gz", "ünïcødé/文件"] {
            validate_path(good).unwrap_or_else(|e| panic!("{good:?} should be safe: {e}"));
        }
        for bad in [
            "",
            "/etc/passwd",
            "../escape",
            "a/../../escape",
            "a/./b",
            "a//b",
            "C:/windows",
            "back\\slash",
            "nul\0byte",
        ] {
            assert!(validate_path(bad).is_err(), "{bad:?} should be rejected");
        }
    }
}
