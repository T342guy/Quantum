//! Cache-friendly hash table of bit-history states.
//!
//! Storage is organised as 16-byte buckets: one checksum byte plus the 15
//! nodes of a nibble's binary tree. Four buckets share a 64-byte group, so a
//! lookup touches exactly one cache line and can be 4-way associative for
//! free. On a miss the bucket holding the least evidence is recycled.

use super::adapt::STATES;

pub const BUCKET: usize = 16;
const WAYS: usize = 4;
const GROUP: usize = BUCKET * WAYS;

pub struct BucketTable {
    t: Vec<u8>,
    group_mask: usize,
}

impl BucketTable {
    /// `bytes` is rounded down to a power of two, at least one group.
    pub fn new(bytes: usize) -> Self {
        let bytes = bytes.max(GROUP).next_power_of_two();
        let groups = bytes / GROUP;
        BucketTable { t: vec![0u8; groups * GROUP], group_mask: groups - 1 }
    }

    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.t.len()
    }

    /// Locate the bucket for `hash`, allocating (by eviction) if absent.
    /// Returns the bucket's base index; state for tree node `n` lives at
    /// `base + n`, where `n` runs over `1..=15`.
    #[inline]
    pub fn find(&mut self, hash: u64) -> usize {
        let chk = {
            let c = (hash >> 40) as u8;
            // Zero marks an unused way, so it cannot double as a checksum.
            if c == 0 { 1 } else { c }
        };
        let base = ((hash as usize) & self.group_mask) * GROUP;
        let mut empty = usize::MAX;
        for way in 0..WAYS {
            let b = base + way * BUCKET;
            let tag = self.t[b];
            if tag == chk {
                return b;
            }
            if tag == 0 && empty == usize::MAX {
                empty = b;
            }
        }
        let victim = if empty != usize::MAX {
            // Never evict a live entry while a way has gone unused.
            empty
        } else {
            // Recycle whichever way carries the least accumulated evidence,
            // judged by the first node of its nibble tree. The scan starts at
            // a hash-derived offset so that ties -- which are common once
            // counts saturate -- do not always fall on the same way.
            let start = ((hash >> 12) as usize) & (WAYS - 1);
            let mut best = base;
            let mut lowest = u32::MAX;
            for i in 0..WAYS {
                let b = base + ((start + i) & (WAYS - 1)) * BUCKET;
                let w = STATES.weight(self.t[b + 1]);
                if w < lowest {
                    lowest = w;
                    best = b;
                }
            }
            best
        };
        self.t[victim..victim + BUCKET].fill(0);
        self.t[victim] = chk;
        victim
    }

    #[inline(always)]
    pub fn state(&self, slot: usize) -> u8 {
        debug_assert!(slot < self.t.len());
        unsafe { *self.t.get_unchecked(slot) }
    }

    #[inline(always)]
    pub fn set_state(&mut self, slot: usize, value: u8) {
        debug_assert!(slot < self.t.len());
        unsafe { *self.t.get_unchecked_mut(slot) = value }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookups_are_stable_and_in_bounds() {
        let mut t = BucketTable::new(1 << 16);
        let a = t.find(0x0123_4567_89AB_CDEF);
        assert_eq!(t.find(0x0123_4567_89AB_CDEF), a, "same hash must return same bucket");
        for node in 1..=15 {
            assert!(a + node < t.bytes());
        }
        // A different context in the same group must land in a different way
        // while an unused one is available.
        let b = t.find(0x0123_4567_89AB_CDEF ^ (1 << 40));
        assert_ne!(a, b);
        assert_eq!(t.find(0x0123_4567_89AB_CDEF), a, "first entry must survive");
    }

    #[test]
    fn unused_ways_are_filled_before_anything_is_evicted() {
        let mut t = BucketTable::new(GROUP); // exactly one group
        let slots: Vec<usize> = (1..=4u64).map(|i| t.find(i << 40)).collect();
        let mut sorted = slots.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), WAYS, "each context should get its own way");
        for (i, &slot) in slots.iter().enumerate() {
            assert_eq!(t.find((i as u64 + 1) << 40), slot);
        }
    }

    #[test]
    fn eviction_prefers_the_least_used_way() {
        let mut t = BucketTable::new(GROUP);
        let slots: Vec<usize> = (1..=4u64).map(|i| t.find(i << 40)).collect();
        // Give every way some history, with way 2 by far the best established.
        for (i, &slot) in slots.iter().enumerate() {
            t.set_state(slot + 1, if i == 2 { 0xFF } else { 0x10 });
        }
        let fresh = t.find(9 << 40);
        assert_ne!(fresh, slots[2], "the busiest way must not be evicted");
        assert_eq!(t.find(3 << 40), slots[2], "and it must still be found");
    }

    #[test]
    fn size_is_rounded_to_a_power_of_two() {
        assert_eq!(BucketTable::new(100).bytes(), 128);
        assert_eq!(BucketTable::new(0).bytes(), 64);
        assert_eq!(BucketTable::new(1 << 20).bytes(), 1 << 20);
    }
}
