//! Adaptive components shared by every model: counter state machines,
//! state maps, the logistic mixer and adaptive probability maps.

use super::tables::{squash, stretch};

// ---------------------------------------------------------------------------
// Bit history states
// ---------------------------------------------------------------------------

/// A bit history is packed into one byte as `n0 << 4 | n1`, where `n0` and
/// `n1` are saturating 4-bit counts of the zeros and ones seen in this
/// context.
///
/// The counts are *non-stationary*: observing a bit discounts the opposite
/// count, so a context that changes behaviour adapts within a few bits
/// instead of being anchored by ancient history. That is the single most
/// important difference between this and a naive frequency counter, and it is
/// worth several percent on real data.
pub struct StateTable {
    next: [[u8; 2]; 256],
    /// `n0 + n1` for each state, used as a cache-eviction priority.
    weight: [u8; 256],
}

impl StateTable {
    fn build() -> Self {
        let mut next = [[0u8; 2]; 256];
        let mut weight = [0u8; 256];
        for s in 0..256usize {
            let n0 = (s >> 4) as u32;
            let n1 = (s & 15) as u32;
            weight[s] = (n0 + n1) as u8;
            for bit in 0..2usize {
                let (mut a, mut b) = if bit == 1 { (n1, n0) } else { (n0, n1) };
                a = (a + 1).min(15);
                if b > 2 {
                    b = 2 + (b - 2) / 2;
                }
                let (n0, n1) = if bit == 1 { (b, a) } else { (a, b) };
                next[s][bit] = ((n0 << 4) | n1) as u8;
            }
        }
        StateTable { next, weight }
    }

    #[inline(always)]
    pub fn next(&self, state: u8, bit: u32) -> u8 {
        self.next[state as usize][bit as usize]
    }

    #[inline(always)]
    pub fn weight(&self, state: u8) -> u32 {
        self.weight[state as usize] as u32
    }
}

pub static STATES: std::sync::LazyLock<StateTable> = std::sync::LazyLock::new(StateTable::build);

// ---------------------------------------------------------------------------
// StateMap
// ---------------------------------------------------------------------------

/// Adaptation rates: entry `n` is `65536 / (n + 1.5)`, so a counter that has
/// seen `n` observations moves by `1 / (n + 1.5)` of the remaining error.
/// Early observations move fast, later ones settle into a running average.
static RATE: std::sync::LazyLock<[i64; 1024]> = std::sync::LazyLock::new(|| {
    let mut t = [0i64; 1024];
    for (n, slot) in t.iter_mut().enumerate() {
        *slot = (131072.0 / (2.0 * n as f64 + 3.0)) as i64;
    }
    t
});

/// Maps a small context (typically a bit history state) to a probability,
/// learned online.
///
/// Each slot packs a 22-bit probability and a 10-bit observation count.
pub struct StateMap {
    t: Vec<u32>,
    cxt: usize,
    limit: usize,
}

const P_MAX: i64 = (1 << 22) - 1;

impl StateMap {
    /// `n` contexts, `limit` caps the observation count (and so the minimum
    /// adaptation rate). Lower limits track non-stationary data better.
    pub fn new(n: usize, limit: usize) -> Self {
        assert!(limit < 1024);
        // Probability 1/2 (bits 10..32), observation count 0 (bits 0..10).
        StateMap { t: vec![1u32 << 31; n], cxt: 0, limit }
    }

    /// Predict for context `cx`; returns a 12-bit probability.
    #[inline(always)]
    pub fn p(&mut self, cx: usize) -> i32 {
        debug_assert!(cx < self.t.len());
        self.cxt = cx;
        (self.t[cx] >> 20) as i32
    }

    #[inline(always)]
    pub fn update(&mut self, bit: u32) {
        let slot = &mut self.t[self.cxt];
        let n = (*slot & 1023) as usize;
        let p = (*slot >> 10) as i64;
        let target = if bit == 1 { P_MAX } else { 0 };
        let p = p + (((target - p) * RATE[n]) >> 16);
        let n = if n < self.limit { n + 1 } else { n };
        *slot = ((p.clamp(0, P_MAX) as u32) << 10) | n as u32;
    }
}

// ---------------------------------------------------------------------------
// Mixer
// ---------------------------------------------------------------------------

/// Logistic mixer: a single-layer network that combines stretched
/// predictions, with one weight vector per mixing context.
///
/// `p = squash(sum_i w_i * x_i)`, trained online by gradient descent on
/// coding loss, which for this parameterisation is simply
/// `w_i += lr * (bit - p) * x_i`.
pub struct Mixer {
    n: usize,
    weights: Vec<i32>,
    inputs: Vec<i32>,
    base: usize,
    pr: i32,
    lr: i32,
}

impl Mixer {
    pub fn new(n: usize, contexts: usize, lr: i32) -> Self {
        Mixer {
            n,
            // Start as a plain average of the inputs.
            weights: vec![(1 << 16) / n as i32; n * contexts],
            inputs: Vec::with_capacity(n),
            base: 0,
            pr: 2048,
            lr,
        }
    }

    #[inline(always)]
    pub fn add(&mut self, x: i32) {
        debug_assert!(self.inputs.len() < self.n);
        self.inputs.push(x.clamp(-2047, 2047));
    }

    /// Select the weight vector to use for this prediction.
    #[inline(always)]
    pub fn set_context(&mut self, cx: usize) {
        self.base = cx * self.n;
        debug_assert!(self.base + self.n <= self.weights.len());
    }

    #[inline(always)]
    pub fn mix(&mut self) -> i32 {
        debug_assert_eq!(self.inputs.len(), self.n);
        let w = &self.weights[self.base..self.base + self.n];
        let mut dot: i64 = 0;
        for i in 0..self.n {
            dot += (w[i] as i64) * (self.inputs[i] as i64);
        }
        self.pr = squash((dot >> 16) as i32);
        self.pr
    }

    #[inline(always)]
    pub fn update(&mut self, bit: u32) {
        let err = (((bit as i32) << 12) - self.pr) * self.lr;
        let w = &mut self.weights[self.base..self.base + self.n];
        for i in 0..self.n {
            w[i] = (w[i] + ((self.inputs[i] * err + 0x8000) >> 16)).clamp(-(1 << 22), 1 << 22);
        }
        self.inputs.clear();
    }
}

// ---------------------------------------------------------------------------
// APM / SSE
// ---------------------------------------------------------------------------

/// Adaptive probability map (a.k.a. secondary symbol estimation).
///
/// Refines an existing prediction by looking it up, interpolated on a
/// 33-point logistic grid, in a table selected by some extra context. This
/// catches systematic biases the mixer cannot express.
pub struct Apm {
    t: Vec<u16>,
    cxt: usize,
    rate: u32,
}

impl Apm {
    pub fn new(contexts: usize, rate: u32) -> Self {
        let mut t = vec![0u16; contexts * 33];
        for (i, slot) in t.iter_mut().enumerate() {
            *slot = (squash(((i % 33) as i32 - 16) * 128) * 16) as u16;
        }
        Apm { t, cxt: 0, rate }
    }

    /// Refine 12-bit probability `pr` under context `cx`. Returns a 16-bit
    /// probability -- the extra precision matters on highly predictable data,
    /// where a 12-bit floor would cost real bytes.
    #[inline(always)]
    pub fn refine(&mut self, pr: i32, cx: usize) -> u32 {
        debug_assert!(cx * 33 + 32 < self.t.len());
        let s = (stretch(pr) + 2048) * 32;
        let w = (s & 4095) as u32;
        let i = (s >> 12) as usize + cx * 33;
        self.cxt = i + (w >> 11) as usize;
        (self.t[i] as u32 * (4096 - w) + self.t[i + 1] as u32 * w) >> 12
    }

    #[inline(always)]
    pub fn update(&mut self, bit: u32) {
        let g = ((bit << 16) + (bit << self.rate) - bit - bit) as i32;
        let slot = &mut self.t[self.cxt];
        *slot = (*slot as i32 + ((g - *slot as i32) >> self.rate)) as u16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_transitions_are_bounded_and_discount() {
        let st = &*STATES;
        // Feeding only ones saturates n1 and leaves n0 at zero.
        let mut s = 0u8;
        for _ in 0..64 {
            s = st.next(s, 1);
        }
        assert_eq!(s, 0x0F);
        // A single zero now must not wipe out the accumulated evidence...
        let s0 = st.next(s, 0);
        assert_eq!(s0 >> 4, 1);
        assert_eq!(s0 & 15, 2 + (15 - 2) / 2);
        // ...but a run of zeros must take over quickly.
        let mut s = s0;
        for _ in 0..8 {
            s = st.next(s, 0);
        }
        assert!(s >> 4 > s & 15, "state {s:#x} should now favour zeros");
        for s in 0..256u32 {
            for bit in 0..2 {
                let n = st.next(s as u8, bit);
                assert!(n >> 4 <= 15 && n & 15 <= 15);
            }
        }
    }

    #[test]
    fn statemap_converges() {
        let mut sm = StateMap::new(4, 255);
        for _ in 0..1000 {
            sm.p(0);
            sm.update(1);
        }
        assert!(sm.p(0) > 4000, "got {}", sm.p(0));
        for _ in 0..1000 {
            sm.p(0);
            sm.update(0);
        }
        assert!(sm.p(0) < 96, "got {}", sm.p(0));
        // Untouched contexts stay neutral.
        assert_eq!(sm.p(1), 2048);
    }

    #[test]
    fn mixer_learns_to_follow_the_informative_input() {
        let mut m = Mixer::new(2, 1, 6);
        let mut rng = 12345u64;
        for _ in 0..20_000 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let bit = (rng & 1) as u32;
            m.set_context(0);
            // Input 0 knows the answer, input 1 is noise.
            m.add(if bit == 1 { 800 } else { -800 });
            m.add(if rng & 2 == 0 { 800 } else { -800 });
            m.mix();
            m.update(bit);
        }
        m.set_context(0);
        m.add(800);
        m.add(-800);
        assert!(m.mix() > 2048, "mixer failed to prefer the useful input");
    }

    #[test]
    fn apm_corrects_a_biased_prediction() {
        let mut apm = Apm::new(1, 7);
        // The incoming prediction says 50/50 but the bit is always 1.
        for _ in 0..4000 {
            apm.refine(2048, 0);
            apm.update(1);
        }
        assert!(apm.refine(2048, 0) > 60000);
    }
}
