//! Reversible byte transforms applied before compression.
//!
//! A filter never removes information -- it rearranges bytes so the model
//! sees more structure. The two that pay for themselves broadly are the x86
//! branch filter (machine code) and a delta filter (sampled data and fixed
//! width records).
//!
//! Which one to use is decided by *probing*: a slice of the block is
//! compressed under each candidate and the winner is kept. Guessing from
//! magic numbers or byte statistics is cheaper but gets it wrong on exactly
//! the mixed content a solid archive produces, and a wrong guess costs more
//! than the probe does.

use crate::codec::{Config, compress_block};
use crate::error::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    None,
    /// Rewrite x86 `CALL`/`JMP rel32` displacements as absolute addresses, so
    /// that repeated calls to the same target become identical byte strings.
    X86,
    /// Subtract the byte `stride` positions back. Turns smooth or
    /// record-structured data into near-zero residuals.
    Delta(u8),
}

impl Filter {
    pub fn to_byte(self) -> u8 {
        match self {
            Filter::None => 0,
            Filter::X86 => 1,
            Filter::Delta(s) => 0x40 | s,
        }
    }

    pub fn from_byte(b: u8) -> Result<Filter> {
        match b {
            0 => Ok(Filter::None),
            1 => Ok(Filter::X86),
            _ if b & 0xC0 == 0x40 && (b & 0x3F) >= 1 => Ok(Filter::Delta(b & 0x3F)),
            _ => Err(Error::Corrupt("block names an unknown filter")),
        }
    }

    pub fn name(self) -> String {
        match self {
            Filter::None => "none".into(),
            Filter::X86 => "x86".into(),
            Filter::Delta(s) => format!("delta{s}"),
        }
    }

    pub fn apply(self, buf: &mut [u8]) {
        match self {
            Filter::None => {}
            Filter::X86 => x86(buf, true),
            Filter::Delta(s) => delta_forward(buf, s as usize),
        }
    }

    pub fn unapply(self, buf: &mut [u8]) {
        match self {
            Filter::None => {}
            Filter::X86 => x86(buf, false),
            Filter::Delta(s) => delta_inverse(buf, s as usize),
        }
    }
}

/// Convert x86 `E8`/`E9` displacements between relative and absolute form.
///
/// Reversibility rests on one detail: after transforming at `i` both
/// directions skip to `i + 5`. The opcode byte itself is never modified, and
/// the four bytes that are modified are skipped by both sides, so encoder and
/// decoder always agree on where the instructions are -- even when a rewritten
/// displacement happens to contain `E8`.
fn x86(buf: &mut [u8], encode: bool) {
    if buf.len() < 5 {
        return;
    }
    let n = buf.len();
    let mut i = 0usize;
    while i + 4 < n {
        if buf[i] == 0xE8 || buf[i] == 0xE9 {
            let d = u32::from_le_bytes([buf[i + 1], buf[i + 2], buf[i + 3], buf[i + 4]]);
            let next = (i as u32).wrapping_add(5);
            let v = if encode { d.wrapping_add(next) } else { d.wrapping_sub(next) };
            buf[i + 1..i + 5].copy_from_slice(&v.to_le_bytes());
            i += 5;
        } else {
            i += 1;
        }
    }
}

fn delta_forward(buf: &mut [u8], stride: usize) {
    if stride == 0 || buf.len() <= stride {
        return;
    }
    // Backwards, so each subtraction still sees the original predecessor.
    for i in (stride..buf.len()).rev() {
        buf[i] = buf[i].wrapping_sub(buf[i - stride]);
    }
}

fn delta_inverse(buf: &mut [u8], stride: usize) {
    if stride == 0 || buf.len() <= stride {
        return;
    }
    for i in stride..buf.len() {
        buf[i] = buf[i].wrapping_add(buf[i - stride]);
    }
}

/// Bytes of a block fed to each probe.
const PROBE_LEN: usize = 96 * 1024;
/// Blocks smaller than this are not worth probing.
const MIN_PROBE_INPUT: usize = 8 * 1024;

/// Pick the filter that compresses this block best.
pub fn choose(block: &[u8]) -> Filter {
    if block.len() < MIN_PROBE_INPUT {
        return Filter::None;
    }
    // Probe from the middle: the head of a solid block is often a run of
    // small files that says nothing about the bulk.
    let start = (block.len() / 2).saturating_sub(PROBE_LEN / 2);
    let sample = &block[start..(start + PROBE_LEN).min(block.len())];

    let mut candidates = vec![Filter::None];
    if looks_like_code(sample) {
        candidates.push(Filter::X86);
    }
    if let Some(stride) = likely_stride(sample) {
        candidates.push(Filter::Delta(stride));
    }
    if candidates.len() == 1 {
        return Filter::None;
    }

    // A cheap level is enough to rank the candidates; the ordering barely
    // moves with level and probing at level 9 would cost real time.
    let probe_cfg = Config::new(1);
    let mut best = Filter::None;
    let mut best_size = usize::MAX;
    let mut scratch = Vec::with_capacity(sample.len());
    for &f in &candidates {
        scratch.clear();
        scratch.extend_from_slice(sample);
        f.apply(&mut scratch);
        let size = compress_block(&scratch, &probe_cfg).len();
        if size < best_size {
            best_size = size;
            best = f;
        }
    }
    best
}

/// Is there enough branch-shaped data here for the x86 filter to be worth a
/// probe? A `rel32` that stays inside a normal binary has a displacement
/// whose top byte is `0x00` or `0xFF`.
fn looks_like_code(sample: &[u8]) -> bool {
    let mut plausible = 0usize;
    let mut i = 0usize;
    while i + 4 < sample.len() {
        if sample[i] == 0xE8 || sample[i] == 0xE9 {
            if sample[i + 4] == 0x00 || sample[i + 4] == 0xFF {
                plausible += 1;
            }
            i += 5;
        } else {
            i += 1;
        }
    }
    plausible * 1024 >= sample.len()
}

/// Look for a repeating stride, the signature of samples or fixed-width
/// records. Scores each stride by how often `x[i] == x[i - stride]`, and
/// returns the best only if it clearly beats plain byte repetition.
fn likely_stride(sample: &[u8]) -> Option<u8> {
    const MAX_STRIDE: usize = 32;
    if sample.len() < MAX_STRIDE * 16 {
        return None;
    }
    let mut best = 0usize;
    let mut best_score = 0usize;
    for stride in 1..=MAX_STRIDE {
        let mut score = 0usize;
        // Sampling every 7th position is plenty to rank strides and keeps
        // this loop off the profile.
        let mut i = stride;
        while i < sample.len() {
            if sample[i] == sample[i - stride] {
                score += 1;
            }
            i += 7;
        }
        if score > best_score {
            best_score = score;
            best = stride;
        }
    }
    let total = (sample.len() - MAX_STRIDE) / 7;
    if best >= 2 && best_score * 8 > total {
        Some(best as u8)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn byte(&mut self) -> u8 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 24) as u8
        }
    }

    fn round_trip(f: Filter, data: &[u8]) {
        let mut buf = data.to_vec();
        f.apply(&mut buf);
        f.unapply(&mut buf);
        assert_eq!(buf, data, "{:?} did not round trip", f);
    }

    #[test]
    fn filters_round_trip_on_adversarial_input() {
        let mut rng = Rng(0xDEADBEEF);
        let random: Vec<u8> = (0..10_000).map(|_| rng.byte()).collect();
        // Dense E8/E9 bytes are the worst case for the x86 filter, because
        // rewritten displacements keep producing new opcode-looking bytes.
        let dense: Vec<u8> = (0..10_000).map(|i| if i % 3 == 0 { 0xE8 } else { 0xE9 }).collect();
        let mut mixed = random.clone();
        for i in (0..mixed.len()).step_by(5) {
            mixed[i] = 0xE8;
        }
        for data in [&random, &dense, &mixed] {
            for f in [Filter::None, Filter::X86, Filter::Delta(1), Filter::Delta(4), Filter::Delta(63)] {
                round_trip(f, data);
            }
        }
        // Short and empty inputs must be handled without panicking.
        for len in 0..8 {
            for f in [Filter::None, Filter::X86, Filter::Delta(1), Filter::Delta(4)] {
                round_trip(f, &random[..len]);
            }
        }
    }

    #[test]
    fn filter_bytes_round_trip() {
        for f in [Filter::None, Filter::X86, Filter::Delta(1), Filter::Delta(16), Filter::Delta(63)] {
            assert_eq!(Filter::from_byte(f.to_byte()).unwrap(), f);
        }
        assert!(Filter::from_byte(0x40).is_err(), "delta with stride 0 is meaningless");
        assert!(Filter::from_byte(0x80).is_err());
        assert!(Filter::from_byte(0x02).is_err());
    }

    #[test]
    fn x86_makes_repeated_calls_identical() {
        // Two calls to the same absolute target from different addresses.
        let mut buf = vec![0x90u8; 64];
        buf[0] = 0xE8;
        buf[1..5].copy_from_slice(&(0x100u32.wrapping_sub(5)).to_le_bytes());
        buf[32] = 0xE8;
        buf[33..37].copy_from_slice(&(0x100u32.wrapping_sub(37)).to_le_bytes());
        Filter::X86.apply(&mut buf);
        assert_eq!(buf[1..5], buf[33..37], "same target should become the same bytes");
    }

    #[test]
    fn delta_flattens_a_ramp() {
        let ramp: Vec<u8> = (0..1000u32).map(|i| (i * 3) as u8).collect();
        let mut buf = ramp.clone();
        Filter::Delta(1).apply(&mut buf);
        assert!(buf[1..].iter().all(|&b| b == 3));
    }

    #[test]
    fn stride_detection_finds_record_width() {
        let mut rng = Rng(7);
        // 12-byte records where most columns are constant.
        let mut data = Vec::new();
        for _ in 0..4000 {
            data.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
            data.push(rng.byte());
        }
        assert_eq!(likely_stride(&data), Some(12));

        let noise: Vec<u8> = (0..20_000).map(|_| rng.byte()).collect();
        assert_eq!(likely_stride(&noise), None, "noise has no stride");
    }
}
