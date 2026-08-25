//! LEB128-style variable-length integers, used throughout the metadata.

use crate::error::{Error, Result};

pub fn write_u64(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub fn write_usize(out: &mut Vec<u8>, v: usize) {
    write_u64(out, v as u64);
}

/// Zigzag encoding, so small negative numbers stay small.
pub fn write_i64(out: &mut Vec<u8>, v: i64) {
    write_u64(out, ((v << 1) ^ (v >> 63)) as u64);
}

pub fn write_bytes(out: &mut Vec<u8>, data: &[u8]) {
    write_usize(out, data.len());
    out.extend_from_slice(data);
}

/// A cursor over a metadata buffer.
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    pub fn u64(&mut self) -> Result<u64> {
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            if self.pos >= self.data.len() {
                return Err(Error::Corrupt("metadata ended mid-integer"));
            }
            let byte = self.data[self.pos];
            self.pos += 1;
            if shift >= 64 || (shift == 63 && byte > 1) {
                return Err(Error::Corrupt("metadata integer overflows 64 bits"));
            }
            result |= ((byte & 0x7F) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
        }
    }

    pub fn usize(&mut self) -> Result<usize> {
        let v = self.u64()?;
        usize::try_from(v).map_err(|_| Error::Corrupt("metadata length exceeds address space"))
    }

    pub fn i64(&mut self) -> Result<i64> {
        let v = self.u64()?;
        Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
    }

    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.usize()?;
        if self.data.len() - self.pos < len {
            return Err(Error::Corrupt("metadata string runs past the end"));
        }
        let out = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok(out)
    }

    /// Borrow the next `len` bytes without a length prefix.
    pub fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        if self.data.len() - self.pos < len {
            return Err(Error::Corrupt("metadata ended mid-field"));
        }
        let out = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok(out)
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        if self.data.len() - self.pos < N {
            return Err(Error::Corrupt("metadata ended mid-field"));
        }
        let out: [u8; N] = self.data[self.pos..self.pos + N].try_into().unwrap();
        self.pos += N;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_across_the_range() {
        let values = [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX / 3, u64::MAX];
        let mut buf = Vec::new();
        for &v in &values {
            write_u64(&mut buf, v);
        }
        let mut r = Reader::new(&buf);
        for &v in &values {
            assert_eq!(r.u64().unwrap(), v);
        }
        assert!(r.is_empty());
    }

    #[test]
    fn signed_round_trips() {
        let mut buf = Vec::new();
        let values = [0i64, -1, 1, -64, 64, i64::MIN, i64::MAX];
        for &v in &values {
            write_i64(&mut buf, v);
        }
        let mut r = Reader::new(&buf);
        for &v in &values {
            assert_eq!(r.i64().unwrap(), v);
        }
    }

    #[test]
    fn truncated_input_is_rejected() {
        let mut buf = Vec::new();
        write_u64(&mut buf, 300);
        assert!(Reader::new(&buf[..1]).u64().is_err());

        let mut buf = Vec::new();
        write_bytes(&mut buf, b"hello");
        assert!(Reader::new(&buf[..3]).bytes().is_err());
    }

    #[test]
    fn overlong_integers_are_rejected() {
        let bogus = [0x80u8; 12];
        assert!(Reader::new(&bogus).u64().is_err());
    }
}
