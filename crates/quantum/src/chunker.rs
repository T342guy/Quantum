//! Content-defined chunking (a Gear/FastCDC variant).
//!
//! Splitting on content rather than at fixed offsets is what makes
//! deduplication survive insertions: adding a byte to the middle of a file
//! shifts every later fixed-size window, but only disturbs the one
//! content-defined chunk that contains it.
//!
//! This is where a large part of Quantum's advantage on bulk data comes from.
//! ZIP compresses each file in isolation and cannot notice that two of them
//! are the same; even a solid archive only finds redundancy inside one
//! compression window. Chunk deduplication removes repeats across the entire
//! input before the model ever sees them.

/// Per-byte mixing table, generated from a fixed SplitMix64 sequence so the
/// chunk boundaries an archive was built with can always be reproduced.
static GEAR: std::sync::LazyLock<[u64; 256]> = std::sync::LazyLock::new(|| {
    let mut t = [0u64; 256];
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    for slot in t.iter_mut() {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        *slot = z ^ (z >> 31);
    }
    t
});

/// Place `bits` set bits across positions 16..48, giving the rolling hash an
/// effective window of roughly 48 bytes.
const fn spread_mask(bits: u32) -> u64 {
    let mut m = 0u64;
    let mut i = 0u32;
    while i < bits {
        m |= 1u64 << (16 + (i * 32) / bits);
        i += 1;
    }
    m
}

/// Chunk size limits. The average is a compromise: smaller chunks find more
/// duplicates, larger ones keep the chunk table small.
#[derive(Clone, Copy, Debug)]
pub struct ChunkSizes {
    pub min: usize,
    pub avg: usize,
    pub max: usize,
}

pub const DEFAULT_SIZES: ChunkSizes =
    ChunkSizes { min: 8 * 1024, avg: 32 * 1024, max: 128 * 1024 };

/// Length of the next chunk of `data`, always at least `min` unless the input
/// is shorter.
///
/// Two masks are used, as in FastCDC: a strict one before the average size is
/// reached (which suppresses short chunks) and a lax one after it (which
/// suppresses long ones). The result is a much tighter size distribution than
/// a single mask gives, and tighter sizes mean better dedup hit rates.
pub fn next_chunk(data: &[u8], sizes: ChunkSizes) -> usize {
    const STRICT: u64 = spread_mask(18);
    const LAX: u64 = spread_mask(14);

    let n = data.len();
    if n <= sizes.min {
        return n;
    }
    let limit = n.min(sizes.max);
    let normal = sizes.avg.min(limit);
    let gear = &*GEAR;

    let mut h = 0u64;
    let mut i = sizes.min;
    while i < normal {
        h = (h << 1).wrapping_add(gear[data[i] as usize]);
        if h & STRICT == 0 {
            return i + 1;
        }
        i += 1;
    }
    while i < limit {
        h = (h << 1).wrapping_add(gear[data[i] as usize]);
        if h & LAX == 0 {
            return i + 1;
        }
        i += 1;
    }
    limit
}

/// Split `data` into content-defined chunks.
pub fn split(data: &[u8], sizes: ChunkSizes) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let n = next_chunk(rest, sizes);
        let (chunk, tail) = rest.split_at(n);
        out.push(chunk);
        rest = tail;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 33) as u8
            })
            .collect()
    }

    #[test]
    fn masks_have_the_intended_weight() {
        assert_eq!(spread_mask(18).count_ones(), 18);
        assert_eq!(spread_mask(14).count_ones(), 14);
        assert_eq!(spread_mask(16).count_ones(), 16);
    }

    #[test]
    fn chunks_respect_the_size_limits() {
        let data = pseudo_random(4 * 1024 * 1024, 42);
        let chunks = split(&data, DEFAULT_SIZES);
        assert!(chunks.len() > 20, "expected many chunks, got {}", chunks.len());
        let total: usize = chunks.iter().map(|c| c.len()).sum();
        assert_eq!(total, data.len());
        for (i, c) in chunks.iter().enumerate() {
            assert!(c.len() <= DEFAULT_SIZES.max, "chunk {i} is too long");
            let is_last = i + 1 == chunks.len();
            assert!(c.len() >= DEFAULT_SIZES.min || is_last, "chunk {i} is too short");
        }
        let mean = total / chunks.len();
        // The average should land within a factor of two of the target.
        assert!(
            mean > DEFAULT_SIZES.avg / 2 && mean < DEFAULT_SIZES.avg * 2,
            "mean chunk size {mean} is off target"
        );
    }

    #[test]
    fn boundaries_resync_after_an_insertion() {
        // This is the property the whole design exists for: inserting bytes
        // near the start must not renumber every later chunk.
        let data = pseudo_random(2 * 1024 * 1024, 7);
        let mut edited = Vec::with_capacity(data.len() + 5);
        edited.extend_from_slice(&data[..100]);
        edited.extend_from_slice(b"HELLO");
        edited.extend_from_slice(&data[100..]);

        let a = split(&data, DEFAULT_SIZES);
        let b = split(&edited, DEFAULT_SIZES);
        let shared = a.iter().filter(|c| b.contains(c)).count();
        assert!(
            shared * 10 >= a.len() * 9,
            "only {shared}/{} chunks survived the insertion",
            a.len()
        );
    }

    #[test]
    fn identical_input_chunks_identically() {
        let data = pseudo_random(1024 * 1024, 99);
        let a: Vec<usize> = split(&data, DEFAULT_SIZES).iter().map(|c| c.len()).collect();
        let b: Vec<usize> = split(&data, DEFAULT_SIZES).iter().map(|c| c.len()).collect();
        assert_eq!(a, b);
    }

    #[test]
    fn short_and_empty_inputs() {
        assert!(split(&[], DEFAULT_SIZES).is_empty());
        let tiny = vec![1u8; 10];
        assert_eq!(split(&tiny, DEFAULT_SIZES).len(), 1);
    }
}
