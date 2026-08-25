//! Binary range coder (the LZMA variant, which is carry-safe).
//!
//! Probabilities are 16-bit: `p` is `P(bit == 1) * 65536` and must lie in
//! `1..=65535`. The encoder keeps `range >= 2^24` at all times, so
//! `bound = (range >> 16) * p` is always in `1..range`, which means no
//! interval can ever collapse.

use crate::error::{Error, Result};

const TOP: u32 = 1 << 24;

/// Arithmetic encoder writing into an in-memory buffer.
pub struct Encoder {
    low: u64,
    range: u32,
    cache: u8,
    cache_size: u64,
    out: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Self {
        Encoder { low: 0, range: u32::MAX, cache: 0, cache_size: 1, out: Vec::new() }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Encoder { low: 0, range: u32::MAX, cache: 0, cache_size: 1, out: Vec::with_capacity(cap) }
    }

    /// Encode `bit` given `p1`, the probability that it is a 1 (16-bit scale).
    #[inline(always)]
    pub fn encode(&mut self, bit: u32, p1: u16) {
        debug_assert!(bit <= 1);
        let bound = (self.range >> 16) * p1 as u32;
        if bit == 1 {
            self.range = bound;
        } else {
            self.low += bound as u64;
            self.range -= bound;
        }
        while self.range < TOP {
            self.shift_low();
            self.range <<= 8;
        }
    }

    #[inline]
    fn shift_low(&mut self) {
        if (self.low as u32) < 0xFF00_0000 || (self.low >> 32) != 0 {
            let carry = (self.low >> 32) as u8;
            let mut byte = self.cache;
            loop {
                self.out.push(byte.wrapping_add(carry));
                byte = 0xFF;
                self.cache_size -= 1;
                if self.cache_size == 0 {
                    break;
                }
            }
            self.cache = ((self.low >> 24) & 0xFF) as u8;
        }
        self.cache_size += 1;
        self.low = (self.low << 8) & 0xFFFF_FFFF;
    }

    /// Flush the coder and return the encoded bytes.
    ///
    /// The first emitted byte is always zero (an artefact of the carry
    /// machinery); it is dropped here and re-inserted by the decoder.
    pub fn finish(mut self) -> Vec<u8> {
        for _ in 0..5 {
            self.shift_low();
        }
        debug_assert_eq!(self.out.first().copied(), Some(0));
        self.out.remove(0);
        self.out
    }
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Arithmetic decoder reading from a byte slice.
pub struct Decoder<'a> {
    code: u32,
    range: u32,
    input: &'a [u8],
    pos: usize,
    /// Number of bytes read past the end of `input`; a healthy stream reads at
    /// most a handful, a corrupt one runs away.
    overrun: u32,
}

impl<'a> Decoder<'a> {
    pub fn new(input: &'a [u8]) -> Self {
        let mut d = Decoder { code: 0, range: u32::MAX, input, pos: 0, overrun: 0 };
        // Skip the leading zero byte the encoder dropped, then load 4 bytes.
        for _ in 0..4 {
            d.code = (d.code << 8) | d.next_byte() as u32;
        }
        d
    }

    #[inline(always)]
    fn next_byte(&mut self) -> u8 {
        if self.pos < self.input.len() {
            let b = self.input[self.pos];
            self.pos += 1;
            b
        } else {
            self.overrun = self.overrun.saturating_add(1);
            0
        }
    }

    /// Decode one bit given `p1`, the probability that it is a 1 (16-bit).
    #[inline(always)]
    pub fn decode(&mut self, p1: u16) -> u32 {
        let bound = (self.range >> 16) * p1 as u32;
        let bit = if self.code < bound {
            self.range = bound;
            1
        } else {
            self.code -= bound;
            self.range -= bound;
            0
        };
        while self.range < TOP {
            self.code = (self.code << 8) | self.next_byte() as u32;
            self.range <<= 8;
        }
        bit
    }

    /// Fail if the decoder consumed meaningfully more input than exists,
    /// which means the stream was truncated or corrupt.
    pub fn check(&self) -> Result<()> {
        if self.overrun > 8 {
            return Err(Error::Corrupt("compressed stream is truncated"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic xorshift, so tests do not need a rand dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    fn round_trip(bits: &[(u32, u16)]) {
        let mut enc = Encoder::new();
        for &(bit, p) in bits {
            enc.encode(bit, p);
        }
        let data = enc.finish();
        let mut dec = Decoder::new(&data);
        for (i, &(bit, p)) in bits.iter().enumerate() {
            assert_eq!(dec.decode(p), bit, "bit {i} of {}", bits.len());
        }
        dec.check().unwrap();
    }

    #[test]
    fn empty_stream() {
        round_trip(&[]);
    }

    #[test]
    fn extreme_probabilities_round_trip() {
        // The interesting failure mode is a probability at the very edge of
        // the representable range paired with the "wrong" bit.
        for &p in &[1u16, 2, 32, 32768, 65503, 65534, 65535] {
            for bit in 0..2u32 {
                round_trip(&vec![(bit, p); 4096]);
            }
        }
    }

    #[test]
    fn random_bits_round_trip() {
        let mut rng = Rng(0x1234_5678_9ABC_DEF0);
        let bits: Vec<(u32, u16)> = (0..200_000)
            .map(|_| {
                let r = rng.next();
                let p = ((r >> 32) as u16).max(1);
                ((r & 1) as u32, p)
            })
            .collect();
        round_trip(&bits);
    }

    #[test]
    fn carry_propagation() {
        // Long runs of a highly-skewed symbol are what stress the carry chain.
        let mut bits = vec![(1u32, 65535u16); 100_000];
        bits.push((0, 65535));
        bits.extend(std::iter::repeat_n((1u32, 65535u16), 100_000));
        round_trip(&bits);
    }

    #[test]
    fn skewed_stream_is_tiny() {
        let mut enc = Encoder::new();
        for _ in 0..80_000 {
            enc.encode(1, 65535);
        }
        // 80k near-certain bits should cost only a handful of bytes.
        assert!(enc.finish().len() < 64);
    }

    #[test]
    fn truncated_stream_is_detected() {
        let mut enc = Encoder::new();
        let mut rng = Rng(99);
        for _ in 0..10_000 {
            enc.encode((rng.next() & 1) as u32, 32768);
        }
        let data = enc.finish();
        let mut dec = Decoder::new(&data[..data.len() / 2]);
        for _ in 0..10_000 {
            dec.decode(32768);
        }
        assert!(dec.check().is_err());
    }
}
