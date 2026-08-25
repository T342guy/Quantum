//! Match model: the long-range half of the compressor.
//!
//! Context models only see a handful of preceding bytes. This model indexes
//! the whole block by a rolling hash of the last few bytes, and when the
//! current position continues a previously seen sequence it predicts the
//! byte that followed last time. That is what lets the codec ride out long
//! repeats -- the job an LZ77 matcher does in a dictionary compressor,
//! except here it feeds a probability instead of emitting a token.

use super::adapt::StateMap;
use super::tables::stretch;
use crate::hash::finalize;

/// Bytes of context hashed to find a candidate match.
const MIN_MATCH: usize = 6;
/// Cap on the reported match length (longer matches are all "certain").
const MAX_LEN: u32 = 65534;

pub struct MatchModel {
    index: Vec<u32>,
    mask: usize,
    /// Position in the history of the next predicted byte, valid iff `len > 0`.
    ptr: usize,
    len: u32,
    /// Set while this byte's prediction is still consistent with the bits
    /// already coded.
    used: bool,
    sm_len: StateMap,
    sm_bits: StateMap,
}

impl MatchModel {
    pub fn new(entry_bits: u32) -> Self {
        let n = 1usize << entry_bits;
        MatchModel {
            index: vec![0u32; n],
            mask: n - 1,
            ptr: 0,
            len: 0,
            used: false,
            // Non-stationary limits: a match that starts failing must be
            // distrusted quickly.
            sm_len: StateMap::new(1024, 511),
            sm_bits: StateMap::new(512, 511),
        }
    }

    /// How confident the model currently is, bucketed for use as a mixer
    /// context: 0 = no match, 1 = short, 2 = long.
    #[inline(always)]
    pub fn confidence(&self) -> usize {
        match self.len {
            0 => 0,
            1..=15 => 1,
            _ => 2,
        }
    }

    /// Called once per byte, after `hist` has been extended with the byte
    /// that was just coded.
    pub fn update_byte(&mut self, hist: &[u8], ctx_hash: u64) {
        let pos = hist.len();
        if self.len > 0 {
            // Did the byte we predicted actually arrive?
            if self.ptr < pos && hist[self.ptr] == hist[pos - 1] {
                self.ptr += 1;
                self.len = (self.len + 1).min(MAX_LEN);
            } else {
                self.len = 0;
            }
        }
        if pos >= MIN_MATCH {
            let slot = (finalize(ctx_hash) as usize) & self.mask;
            if self.len == 0 {
                let cand = self.index[slot] as usize;
                if cand > 0 && cand < pos {
                    // Verify the hash hit by walking backwards: the further
                    // the two positions agree, the more the prediction is
                    // worth. This also filters out hash collisions.
                    let max = cand.min(64);
                    let mut n = 0usize;
                    while n < max && hist[cand - 1 - n] == hist[pos - 1 - n] {
                        n += 1;
                    }
                    if n >= MIN_MATCH {
                        self.ptr = cand;
                        self.len = n as u32;
                    }
                }
            }
            self.index[slot] = pos as u32;
        }
        self.used = false;
    }

    /// Mixer inputs for the current bit. Returns `(0, 0)` when there is no
    /// usable match.
    #[inline]
    pub fn predict(&mut self, hist: &[u8], c0: u32, bpos: u32) -> (i32, i32) {
        self.used = false;
        if self.len == 0 || self.ptr >= hist.len() {
            return (0, 0);
        }
        let expected = hist[self.ptr];
        // The bits coded so far in this byte must still agree with the
        // prediction; otherwise the match is broken and worthless.
        let so_far = (1u32 << bpos) | (expected as u32 >> (8 - bpos));
        if bpos > 0 && so_far != c0 {
            self.len = 0;
            return (0, 0);
        }
        let bit = ((expected as u32) >> (7 - bpos)) & 1;
        let len_bucket = self.len.min(15) as usize;
        let cx_len = ((len_bucket << 1) | bit as usize) << 3 | bpos as usize;
        let cx_bits = ((bit as usize) << 8) | c0 as usize;
        self.used = true;
        (stretch(self.sm_len.p(cx_len)), stretch(self.sm_bits.p(cx_bits)))
    }

    #[inline]
    pub fn update(&mut self, bit: u32) {
        if self.used {
            self.sm_len.update(bit);
            self.sm_bits.update(bit);
        }
    }
}
