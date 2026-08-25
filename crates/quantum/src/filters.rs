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
        self.apply_at(buf, 0);
    }

    pub fn unapply(self, buf: &mut [u8]) {
        match self {
            Filter::None => {}
            Filter::X86 => x86(buf, false, 0),
            Filter::Delta(s) => delta_inverse(buf, s as usize),
        }
    }

    /// Apply to a slice that starts at `base` within some larger buffer.
    ///
    /// Only the probe uses a non-zero base, and it needs one: the x86
    /// transform maps a displacement to an absolute address using the byte's
    /// position, so a sample taken out of context would not reproduce the
    /// address agreement that makes the transform worth applying.
    pub fn apply_at(self, buf: &mut [u8], base: u32) {
        match self {
            Filter::None => {}
            Filter::X86 => x86(buf, true, base),
            Filter::Delta(s) => delta_forward(buf, s as usize),
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
fn x86(buf: &mut [u8], encode: bool, base: u32) {
    if buf.len() < 5 {
        return;
    }
    let n = buf.len();
    let mut i = 0usize;
    while i + 4 < n {
        if buf[i] == 0xE8 || buf[i] == 0xE9 {
            let d = u32::from_le_bytes([buf[i + 1], buf[i + 2], buf[i + 3], buf[i + 4]]);
            let next = base.wrapping_add(i as u32).wrapping_add(5);
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

/// The probe reads this many slices, spread evenly across the block...
const PROBE_SLICES: usize = 8;
/// ...each this long. One contiguous window is not enough: a solid block is
/// heterogeneous by construction, and a single sample from the middle of a
/// 40 MB block of executables can easily land in a data table and recommend a
/// transform that wrecks the other 39 MB.
const PROBE_SLICE_LEN: usize = 24 * 1024;
/// Blocks smaller than this are not worth probing.
const MIN_PROBE_INPUT: usize = 8 * 1024;
/// A transform has to beat leaving the data alone by this much before it is
/// worth the risk, since the probe only ever sees a sample.
const PROBE_MARGIN_PERCENT: usize = 2;

/// Gather a representative sample: several slices from across the block,
/// each paired with the offset it came from.
fn probe_slices(block: &[u8]) -> Vec<(usize, &[u8])> {
    let slice_len = PROBE_SLICE_LEN.min(block.len());
    let slices = PROBE_SLICES.min(block.len() / slice_len.max(1)).max(1);
    let stride = block.len() / slices;
    (0..slices)
        .map(|i| {
            let start = (i * stride).min(block.len() - slice_len);
            (start, &block[start..start + slice_len])
        })
        .collect()
}

/// What a cheap look at a block says about how to compress it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Analysis {
    pub filter: Filter,
    /// The block is already compressed (video, audio, images, other
    /// archives), and modelling it would burn a lot of time for nothing.
    pub incompressible: bool,
}

/// Percentage of its original size a sample has to stay above before the
/// block is written verbatim instead of modelled.
///
/// Level 1 on a sample is a good predictor of what level 9 will manage on the
/// block: the gap between levels is a few percent, not tens. So if a cheap
/// pass cannot get below this, an expensive one will not either -- and on
/// media files that is the difference between a second and several minutes
/// for the same result.
const INCOMPRESSIBLE_PERCENT: usize = 98;

/// Decide how to handle a block, from a sample of it.
///
/// The two filters need different decision procedures, because their payoffs
/// have different shapes:
///
/// * **x86** pays off over long distances -- it makes every call to a given
///   function look identical, which only shows up once the model has seen the
///   same target many times. A sample small enough to probe cheaply cannot
///   observe that, and measurably under-rates the filter. Detection is used
///   instead, and it is safe to be eager: on data that is not machine code
///   the transform barely fires, so a false positive costs nothing while a
///   false negative costs several percent.
/// * **delta** pays off immediately and locally, so a sample measures it
///   faithfully and it is decided by probing -- which matters, because delta
///   applied to the wrong data is destructive.
pub fn analyze(block: &[u8]) -> Analysis {
    let plain = Analysis { filter: Filter::None, incompressible: false };
    if block.len() < MIN_PROBE_INPUT {
        return plain;
    }
    let slices = probe_slices(block);
    let sample: Vec<u8> = slices.iter().flat_map(|(_, s)| s.iter().copied()).collect();

    // Machine code is never mistaken for already-compressed data, and the
    // check is free, so it settles the question first.
    if looks_like_code(&sample) {
        return Analysis { filter: Filter::X86, incompressible: false };
    }

    // A cheap level is enough both to judge compressibility and to rank
    // filters; the ordering barely moves with level, and probing at level 9
    // would cost real time.
    let probe_cfg = Config::new(1);
    let baseline = compress_block(&sample, &probe_cfg).len();
    if baseline * 100 >= sample.len() * INCOMPRESSIBLE_PERCENT {
        return Analysis { filter: Filter::None, incompressible: true };
    }

    let Some(stride) = likely_stride(&sample) else {
        return plain;
    };
    let mut transformed = sample.clone();
    Filter::Delta(stride).apply(&mut transformed);
    let with_delta = compress_block(&transformed, &probe_cfg).len();

    // Only switch if the win clears the margin, since the probe is an
    // estimate taken from a fraction of the block.
    if with_delta + baseline * PROBE_MARGIN_PERCENT / 100 < baseline {
        Analysis { filter: Filter::Delta(stride), incompressible: false }
    } else {
        plain
    }
}

/// Pick the preprocessing filter for a block, ignoring the rest of the
/// analysis.
pub fn choose(block: &[u8]) -> Filter {
    analyze(block).filter
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
    fn code_detection_is_specific() {
        let mut rng = Rng(4242);
        // Random bytes hit E8/E9 followed by 00/FF about once every 16 KB,
        // far below the threshold.
        let noise: Vec<u8> = (0..200_000).map(|_| rng.byte()).collect();
        assert!(!looks_like_code(&noise), "random data must not look like code");

        let text = b"the quick brown fox jumps over the lazy dog. ".repeat(5000);
        assert!(!looks_like_code(&text), "text must not look like code");

        // Something with a realistic density of near calls.
        let mut code: Vec<u8> = (0..200_000).map(|_| rng.byte()).collect();
        for i in (0..code.len() - 8).step_by(40) {
            code[i] = 0xE8;
            code[i + 4] = 0x00;
        }
        assert!(looks_like_code(&code), "dense near-calls must be detected");
    }

    #[test]
    fn already_compressed_data_is_recognised() {
        let mut rng = Rng(0xABCDEF);
        // Random bytes stand in for the payload of a video, JPEG or archive:
        // high entropy with no structure left to find.
        let noise: Vec<u8> = (0..2_000_000).map(|_| rng.byte()).collect();
        let verdict = analyze(&noise);
        assert!(verdict.incompressible, "high-entropy data should be recognised");
        assert_eq!(verdict.filter, Filter::None);

        // Real content must not be mistaken for it, or we would silently
        // stop compressing.
        let text = b"quantum compresses ordinary prose extremely well. ".repeat(40_000);
        assert!(!analyze(&text).incompressible, "text must not be written off");
        let mut code: Vec<u8> = (0..500_000).map(|_| rng.byte()).collect();
        for i in (0..code.len() - 8).step_by(40) {
            code[i] = 0xE8;
            code[i + 4] = 0x00;
        }
        assert!(!analyze(&code).incompressible, "executables must not be written off");

        // Mixed content: half incompressible, half prose. The compressible
        // half is worth having, so this must not be written off either.
        let mut mixed: Vec<u8> = (0..1_000_000).map(|_| rng.byte()).collect();
        mixed.extend_from_slice(&b"the compressible half of the block. ".repeat(28_000));
        assert!(!analyze(&mixed).incompressible, "mixed blocks must still be modelled");
    }

    #[test]
    fn choose_leaves_ordinary_data_alone() {
        let text = b"quantum picks filters by measuring, not guessing. ".repeat(4000);
        assert_eq!(choose(&text), Filter::None);
        let mut rng = Rng(1);
        let noise: Vec<u8> = (0..300_000).map(|_| rng.byte()).collect();
        assert_eq!(choose(&noise), Filter::None);
        assert_eq!(choose(b"short"), Filter::None);
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
