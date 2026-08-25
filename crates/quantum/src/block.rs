//! Block framing: filter selection, the stored-block fallback, and integrity.
//!
//! A block is the unit of independent compression. Keeping blocks independent
//! is what makes compression and extraction parallel, and what lets a reader
//! decode one file without touching the rest of the archive.

use crate::codec::{Config, compress_block, decompress_block};
use crate::error::{Error, Result};
use crate::filters::Filter;
use crate::hash::fast_hash;

/// How a block's payload is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// Verbatim. Used when modelling made the data bigger, which is what
    /// happens to encrypted or already-compressed content.
    Stored,
    /// Context-mixing coded.
    Cm,
}

/// A compressed block plus everything needed to restore it.
#[derive(Clone, Debug)]
pub struct Packed {
    pub method: Method,
    pub filter: Filter,
    pub raw_len: usize,
    pub checksum: u64,
    pub data: Vec<u8>,
}

impl Packed {
    pub fn method_byte(&self) -> u8 {
        match self.method {
            Method::Stored => 0,
            Method::Cm => 1,
        }
    }

    pub fn method_from_byte(b: u8) -> Result<Method> {
        match b {
            0 => Ok(Method::Stored),
            1 => Ok(Method::Cm),
            _ => Err(Error::Corrupt("block names an unknown compression method")),
        }
    }
}

/// Compress one block. `filter` forces a transform; `None` picks one by
/// probing.
pub fn pack(raw: &[u8], cfg: &Config, filter: Option<Filter>) -> Packed {
    let checksum = fast_hash(raw, 0);
    let filter = filter.unwrap_or_else(|| crate::filters::choose(raw));

    let mut staged;
    let filtered: &[u8] = if filter == Filter::None {
        raw
    } else {
        staged = raw.to_vec();
        filter.apply(&mut staged);
        &staged
    };

    let data = compress_block(filtered, cfg);
    // Falling back to a verbatim copy bounds the worst case at a few bytes of
    // header rather than the ~0.3% the model loses on random data.
    if data.len() >= raw.len() {
        return Packed {
            method: Method::Stored,
            filter: Filter::None,
            raw_len: raw.len(),
            checksum,
            data: raw.to_vec(),
        };
    }
    Packed { method: Method::Cm, filter, raw_len: raw.len(), checksum, data }
}

/// Restore a block and verify it against its checksum.
pub fn unpack(packed: &Packed, cfg: &Config) -> Result<Vec<u8>> {
    let mut out = match packed.method {
        Method::Stored => {
            if packed.data.len() != packed.raw_len {
                return Err(Error::Corrupt("stored block has the wrong length"));
            }
            packed.data.clone()
        }
        Method::Cm => decompress_block(&packed.data, packed.raw_len, cfg)?,
    };
    packed.filter.unapply(&mut out);
    if fast_hash(&out, 0) != packed.checksum {
        return Err(Error::IntegrityFailure("a data block".into()));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Self-describing single-stream format (`quantum raw`)
// ---------------------------------------------------------------------------

const RAW_MAGIC: [u8; 4] = *b"QNTR";

/// Serialise a block with a small self-describing header, for the raw
/// stream commands that do not build an archive.
pub fn write_raw(packed: &Packed, level: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(packed.data.len() + 32);
    out.extend_from_slice(&RAW_MAGIC);
    out.push(level);
    out.push(packed.method_byte());
    out.push(packed.filter.to_byte());
    crate::varint::write_usize(&mut out, packed.raw_len);
    out.extend_from_slice(&packed.checksum.to_le_bytes());
    crate::varint::write_usize(&mut out, packed.data.len());
    out.extend_from_slice(&packed.data);
    out
}

/// Parse a stream written by [`write_raw`], returning the block and its level.
pub fn read_raw(input: &[u8]) -> Result<(Packed, u8)> {
    if input.len() < 8 || input[..4] != RAW_MAGIC {
        return Err(Error::BadMagic);
    }
    let level = input[4];
    let method = Packed::method_from_byte(input[5])?;
    let filter = Filter::from_byte(input[6])?;
    let mut r = crate::varint::Reader::new(&input[7..]);
    let raw_len = r.usize()?;
    let checksum = u64::from_le_bytes(r.array::<8>()?);
    let data_len = r.usize()?;
    let data = r.take(data_len)?.to_vec();
    Ok((Packed { method, filter, raw_len, checksum, data }, level))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_fallback_bounds_expansion() {
        // A pseudo-random block cannot be modelled, so it must be stored.
        let mut x = 0x243F_6A88_85A3_08D3u64;
        let random: Vec<u8> = (0..200_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 33) as u8
            })
            .collect();
        let cfg = Config::new(3);
        let packed = pack(&random, &cfg, None);
        assert_eq!(packed.method, Method::Stored);
        assert_eq!(packed.data.len(), random.len());
        assert_eq!(unpack(&packed, &cfg).unwrap(), random);
    }

    #[test]
    fn compressible_data_uses_the_model() {
        let text = b"the quick brown fox jumps over the lazy dog. ".repeat(500);
        let cfg = Config::new(3);
        let packed = pack(&text, &cfg, None);
        assert_eq!(packed.method, Method::Cm);
        assert!(packed.data.len() < text.len() / 20);
        assert_eq!(unpack(&packed, &cfg).unwrap(), text);
    }

    #[test]
    fn corruption_is_caught() {
        let text = b"quantum compresses this text".repeat(400);
        let cfg = Config::new(1);
        let mut packed = pack(&text, &cfg, None);
        packed.checksum ^= 1;
        assert!(matches!(unpack(&packed, &cfg), Err(Error::IntegrityFailure(_))));
    }

    #[test]
    fn raw_stream_round_trips() {
        let data = b"raw stream framing test ".repeat(300);
        let cfg = Config::new(2);
        let packed = pack(&data, &cfg, None);
        let bytes = write_raw(&packed, 2);
        let (back, level) = read_raw(&bytes).unwrap();
        assert_eq!(level, 2);
        assert_eq!(unpack(&back, &Config::new(level)).unwrap(), data);

        assert!(matches!(read_raw(b"nope").unwrap_err(), Error::BadMagic));
        assert!(read_raw(&bytes[..bytes.len() - 4]).is_err(), "truncation must be rejected");
    }

    #[test]
    fn empty_block_round_trips() {
        let cfg = Config::new(1);
        let packed = pack(&[], &cfg, None);
        assert_eq!(unpack(&packed, &cfg).unwrap(), Vec::<u8>::new());
    }
}
