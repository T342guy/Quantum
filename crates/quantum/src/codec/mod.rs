//! The Quantum codec: a context-mixing model driving a binary range coder.
//!
//! # How it works
//!
//! Data is coded one *bit* at a time. Before each bit, a set of models each
//! predict its value from a different view of the recent past -- the previous
//! byte, the previous six bytes, the current word, the byte that followed the
//! last time this sequence appeared. Each prediction is converted to the
//! logistic domain, a small neural network mixes them into a single
//! probability, two adaptive maps correct for residual bias, and the range
//! coder writes the bit at almost exactly its information content.
//!
//! Everything adapts online, and the decoder runs the identical model against
//! the bits it has already decoded, so no model parameters are ever stored.
//! That is where the compression comes from: the "dictionary" is rebuilt on
//! the fly on both sides instead of being transmitted.

mod adapt;
mod match_model;
mod rc;
mod table;
pub mod tables;

use adapt::{Apm, Mixer, StateMap, STATES};
use match_model::MatchModel;
use rc::{Decoder, Encoder};
use table::BucketTable;
use tables::stretch;

use crate::error::{Error, Result};
use crate::hash::{finalize, mix};

/// One model's view of the past.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ctx {
    /// The previous `n` bytes.
    Order(u8),
    /// The word being typed (letters and digits since the last separator).
    Word,
    /// The previous word plus the current one.
    WordPrev,
    /// Bytes at -1 and -3, which catches alternating structure.
    Sparse13,
    /// Bytes at -2, -3 and -4: order-3 with the noisiest byte removed.
    Skip1,
}

/// Compression settings. The decoder reconstructs these from the level stored
/// in the stream, so the two sides always agree.
///
/// Table sizes depend on the level *and* on the length of the block being
/// coded -- there is no point giving a 4 KB block a 16 MB hash table. Both
/// sides know the block length, so both derive the same sizes.
#[derive(Clone, Debug)]
pub struct Config {
    pub level: u8,
    models: Vec<Ctx>,
    /// Total hash-table budget shared by the context models.
    budget: usize,
}

/// Default compression level: a good ratio at a tolerable speed.
pub const DEFAULT_LEVEL: u8 = 5;
pub const MIN_LEVEL: u8 = 1;
pub const MAX_LEVEL: u8 = 9;

/// Smallest useful table: below this, collisions swamp the statistics.
const MIN_TABLE: usize = 1 << 16;

impl Config {
    pub fn new(level: u8) -> Self {
        let level = level.clamp(MIN_LEVEL, MAX_LEVEL);
        use Ctx::*;
        // Higher levels buy ratio with both more models and more memory for
        // each of them.
        let models: Vec<Ctx> = match level {
            1 => vec![Order(2), Order(3)],
            2 => vec![Order(2), Order(3), Order(4)],
            3 => vec![Order(2), Order(3), Order(4), Order(6)],
            4 | 5 => vec![Order(2), Order(3), Order(4), Order(6), Word],
            6 => vec![Order(2), Order(3), Order(4), Order(6), Word, Skip1],
            7 => vec![Order(2), Order(3), Order(4), Order(5), Order(6), Word, Skip1],
            _ => vec![
                Order(2),
                Order(3),
                Order(4),
                Order(5),
                Order(6),
                Order(8),
                Word,
                WordPrev,
                Sparse13,
                Skip1,
            ],
        };
        Config { level, models, budget: 1usize << (22 + level as u32) }
    }

    /// Bytes of hash table given to each context model for a block of
    /// `block_len` bytes.
    fn table_bytes(&self, block_len: usize) -> usize {
        let share = (self.budget / self.models.len()).next_power_of_two() >> 1;
        // Roughly four table bytes per input byte is the point past which
        // more memory stops paying for itself.
        let want = block_len.saturating_mul(4).max(MIN_TABLE).next_power_of_two();
        want.min(share).max(MIN_TABLE)
    }

    /// Log2 of the match model's index size for a block of `block_len` bytes.
    fn match_bits(&self, block_len: usize) -> u32 {
        let want = block_len.max(4096).next_power_of_two().trailing_zeros();
        want.clamp(12, 14 + self.level as u32)
    }

    /// Working-set size of one codec instance for a block of this size.
    /// Multiply by the thread count to size a job.
    pub fn memory_for_block(&self, block_len: usize) -> usize {
        self.models.len() * self.table_bytes(block_len)
            + (1usize << self.match_bits(block_len)) * 4
            + 65536 * 33 * 2
            + 65536
            + block_len
    }
}

impl Default for Config {
    fn default() -> Self {
        Config::new(DEFAULT_LEVEL)
    }
}

/// Number of previous bytes hashed by the match model.
const MATCH_CTX_BYTES: u32 = 6;

/// The predictor: all models plus the machinery that combines them.
struct Model {
    cfg_models: Vec<Ctx>,
    tables: Vec<BucketTable>,
    maps: Vec<StateMap>,
    /// Context hash of the current byte, one per model.
    ctx_hash: Vec<u64>,
    /// Base index of the bucket currently in use, one per model.
    bucket: Vec<usize>,

    /// Order-1 states, indexed directly by `(c1 << 8) | c0` -- no hashing, so
    /// no collisions, for the model that is consulted most often.
    order1: Vec<u8>,
    map0: StateMap,
    map1: StateMap,

    matcher: MatchModel,
    mixer: Mixer,
    apm_c0: Apm,
    apm_order1: Apm,

    /// Node within the current nibble's binary tree, `1..=15`.
    node: usize,
    /// Partial byte, with a leading 1 bit as a length marker.
    c0: u32,
    bpos: u32,
    /// The last eight bytes, most recent in the low byte.
    c8: u64,
    word: u64,
    prev_word: u64,

    hist: Vec<u8>,
    /// Scratch: states read during `predict`, reused by `learn`.
    seen: Vec<u8>,
    mix_pr: i32,
}

const NIBBLE_SALT: u64 = 0x9E37_79B9_7F4A_7C15;

impl Model {
    fn new(cfg: &Config, expected_len: usize) -> Self {
        let n_models = cfg.models.len();
        // order-0, order-1, each context model, two match inputs, bias.
        let inputs = 2 + n_models + 2 + 1;
        let mut m = Model {
            cfg_models: cfg.models.clone(),
            tables: (0..n_models).map(|_| BucketTable::new(cfg.table_bytes(expected_len))).collect(),
            maps: (0..n_models).map(|_| StateMap::new(256, 1023)).collect(),
            ctx_hash: vec![0; n_models],
            bucket: vec![0; n_models],
            order1: vec![0u8; 1 << 16],
            map0: StateMap::new(256, 1023),
            map1: StateMap::new(256, 1023),
            matcher: MatchModel::new(cfg.match_bits(expected_len)),
            // Three mixer weight sets per partial byte, selected by how much
            // the match model currently knows.
            mixer: Mixer::new(inputs, 256 * 3, 7),
            apm_c0: Apm::new(256, 7),
            apm_order1: Apm::new(1 << 16, 7),
            node: 1,
            c0: 1,
            bpos: 0,
            c8: 0,
            word: 0,
            prev_word: 0,
            hist: Vec::with_capacity(expected_len),
            seen: vec![0u8; n_models],
            mix_pr: 2048,
        };
        m.begin_byte();
        m
    }

    /// Context hash for model `i` over the current history.
    fn hash_for(&self, ctx: Ctx) -> u64 {
        let h = match ctx {
            Ctx::Order(n) => {
                let bits = 8 * n as u32;
                let masked = if bits >= 64 { self.c8 } else { self.c8 & ((1u64 << bits) - 1) };
                mix(masked, 0x100 + n as u64)
            }
            Ctx::Word => mix(self.word, 0x201),
            Ctx::WordPrev => mix(mix(self.prev_word, self.word), 0x202),
            Ctx::Sparse13 => mix((self.c8 & 0xFF) | ((self.c8 >> 8) & 0xFF00), 0x301),
            Ctx::Skip1 => mix((self.c8 >> 8) & 0xFF_FFFF, 0x302),
        };
        finalize(h)
    }

    /// Start a new byte: recompute every context hash and look up its bucket.
    fn begin_byte(&mut self) {
        for i in 0..self.cfg_models.len() {
            self.ctx_hash[i] = self.hash_for(self.cfg_models[i]);
            self.bucket[i] = self.tables[i].find(self.ctx_hash[i]);
        }
        self.node = 1;
    }

    /// Start the low nibble: the high nibble is now known, so it joins the
    /// context and every model re-enters the table.
    fn begin_low_nibble(&mut self) {
        for i in 0..self.cfg_models.len() {
            let h = finalize(self.ctx_hash[i] ^ (self.c0 as u64).wrapping_mul(NIBBLE_SALT));
            self.bucket[i] = self.tables[i].find(h);
        }
        self.node = 1;
    }

    #[inline(always)]
    fn order1_slot(&self) -> usize {
        (((self.c8 & 0xFF) as usize) << 8) | self.c0 as usize
    }

    /// Probability that the next bit is a 1, on a 16-bit scale.
    #[inline]
    fn predict(&mut self) -> u16 {
        self.mixer.set_context(self.c0 as usize * 3 + self.matcher.confidence());
        self.mixer.add(stretch(self.map0.p(self.c0 as usize)));

        let s1 = self.order1[self.order1_slot()];
        self.mixer.add(stretch(self.map1.p(s1 as usize)));

        for i in 0..self.cfg_models.len() {
            let state = self.tables[i].state(self.bucket[i] + self.node);
            self.seen[i] = state;
            let p = self.maps[i].p(state as usize);
            self.mixer.add(stretch(p));
        }

        let (m1, m2) = self.matcher.predict(&self.hist, self.c0, self.bpos);
        self.mixer.add(m1);
        self.mixer.add(m2);
        // A constant input lets the mixer learn a per-context bias.
        self.mixer.add(256);

        let pr = self.mixer.mix();
        self.mix_pr = pr;

        // Two rounds of secondary estimation. Each is averaged 3:1 with its
        // input so a cold map cannot make things worse. From here on the
        // probability is carried at 16 bits: the extra precision is what lets
        // near-certain predictions cost near-zero bits.
        let pr16 = (pr as u32) << 4;
        let refined = self.apm_c0.refine(pr, self.c0 as usize);
        let p16 = (pr16 + refined * 3) >> 2;

        let cx = (((self.c8 & 0xFF) as usize) << 8) | self.c0 as usize;
        let refined2 = self.apm_order1.refine(((p16 >> 4) as i32).clamp(0, 4095), cx);
        let p16 = (p16 + refined2 * 3) >> 2;

        p16.clamp(1, 65535) as u16
    }

    /// Feed the coded bit back into every adaptive component.
    #[inline]
    fn learn(&mut self, bit: u32) {
        self.map0.update(bit);
        self.map1.update(bit);
        let slot = self.order1_slot();
        self.order1[slot] = STATES.next(self.order1[slot], bit);

        for i in 0..self.cfg_models.len() {
            self.maps[i].update(bit);
            let next = STATES.next(self.seen[i], bit);
            let slot = self.bucket[i] + self.node;
            self.tables[i].set_state(slot, next);
        }
        self.matcher.update(bit);
        self.mixer.update(bit);
        self.apm_c0.update(bit);
        self.apm_order1.update(bit);

        self.c0 = (self.c0 << 1) | bit;
        self.bpos += 1;
        match self.bpos {
            8 => self.end_byte(),
            4 => self.begin_low_nibble(),
            _ => self.node = self.node * 2 + bit as usize,
        }
    }

    fn end_byte(&mut self) {
        let byte = (self.c0 & 0xFF) as u8;
        self.hist.push(byte);
        self.c8 = (self.c8 << 8) | byte as u64;

        // Word boundary tracking for the text models.
        if byte.is_ascii_alphanumeric() || byte >= 0x80 {
            self.word = mix(self.word, byte.to_ascii_lowercase() as u64);
        } else if self.word != 0 {
            self.prev_word = self.word;
            self.word = 0;
        }

        let match_ctx = mix(self.c8 & ((1u64 << (8 * MATCH_CTX_BYTES)) - 1), 0x401);
        self.matcher.update_byte(&self.hist, match_ctx);

        self.c0 = 1;
        self.bpos = 0;
        self.begin_byte();
    }
}

/// Compress one independent block.
pub fn compress_block(input: &[u8], cfg: &Config) -> Vec<u8> {
    let mut model = Model::new(cfg, input.len());
    let mut enc = Encoder::with_capacity(input.len() / 3 + 64);
    for &byte in input {
        for shift in (0..8).rev() {
            let bit = ((byte >> shift) & 1) as u32;
            let p = model.predict();
            enc.encode(bit, p);
            model.learn(bit);
        }
    }
    enc.finish()
}

/// Decompress one block. `out_len` is the original length, which the
/// container stores alongside the block.
pub fn decompress_block(data: &[u8], out_len: usize, cfg: &Config) -> Result<Vec<u8>> {
    let mut model = Model::new(cfg, out_len);
    let mut dec = Decoder::new(data);
    for _ in 0..out_len {
        for _ in 0..8 {
            let p = model.predict();
            let bit = dec.decode(p);
            model.learn(bit);
        }
    }
    dec.check()?;
    if model.hist.len() != out_len {
        return Err(Error::Corrupt("block decoded to the wrong length"));
    }
    Ok(model.hist)
}
