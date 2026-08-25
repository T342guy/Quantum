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

const fn build_states() -> StateTable {
    let mut next = [[0u8; 2]; 256];
    let mut weight = [0u8; 256];
    let mut s = 0usize;
    while s < 256 {
        let n0 = (s >> 4) as u32;
        let n1 = (s & 15) as u32;
        weight[s] = (n0 + n1) as u8;
        let mut bit = 0usize;
        while bit < 2 {
            let (mut a, mut b) = if bit == 1 { (n1, n0) } else { (n0, n1) };
            a += 1;
            if a > 15 {
                a = 15;
            }
            if b > 2 {
                b = 2 + (b - 2) / 2;
            }
            let (n0, n1) = if bit == 1 { (b, a) } else { (a, b) };
            next[s][bit] = ((n0 << 4) | n1) as u8;
            bit += 1;
        }
        s += 1;
    }
    StateTable { next, weight }
}

impl StateTable {
    #[inline(always)]
    pub fn next(&self, state: u8, bit: u32) -> u8 {
        self.next[state as usize][(bit & 1) as usize]
    }

    #[inline(always)]
    pub fn weight(&self, state: u8) -> u32 {
        self.weight[state as usize] as u32
    }
}

/// Evaluated at compile time: this is consulted several times per coded bit.
pub static STATES: StateTable = build_states();

// ---------------------------------------------------------------------------
// StateMap
// ---------------------------------------------------------------------------

/// Adaptation rates: entry `n` is `65536 / (n + 1.5)`, so a counter that has
/// seen `n` observations moves by `1 / (n + 1.5)` of the remaining error.
/// Early observations move fast, later ones settle into a running average.
const fn build_rates() -> [i64; 1024] {
    let mut t = [0i64; 1024];
    let mut n = 0usize;
    while n < 1024 {
        t[n] = 131072 / (2 * n as i64 + 3);
        n += 1;
    }
    t
}

static RATE: [i64; 1024] = build_rates();

/// Maps a small context (typically a bit history state) to a probability,
/// learned online.
///
/// Each slot packs a 22-bit probability and a 10-bit observation count.
pub struct StateMap {
    t: Vec<u32>,
    /// `t.len() - 1`; sizes are powers of two so a mask replaces a bounds
    /// check in a function called about a dozen times per coded bit.
    mask: usize,
    cxt: usize,
    limit: u32,
}

/// Probabilities are held to 22 bits inside a slot, with the low 10 bits
/// carrying the observation count.
const P_MAX: i64 = (1 << 22) - 1;

impl StateMap {
    /// `n` contexts (a power of two), `limit` caps the observation count and
    /// so the minimum adaptation rate. Lower limits track non-stationary data
    /// better; higher ones give steadier estimates.
    pub fn new(n: usize, limit: u32) -> Self {
        assert!(n.is_power_of_two(), "state map size {n} must be a power of two");
        assert!(limit < 1024);
        // Probability 1/2 (bits 10..32), observation count 0 (bits 0..10).
        StateMap { t: vec![1u32 << 31; n], mask: n - 1, cxt: 0, limit }
    }

    /// Predict for context `cx`; returns a 12-bit probability.
    #[inline(always)]
    pub fn p(&mut self, cx: usize) -> i32 {
        debug_assert!(cx <= self.mask, "context {cx} is outside this state map");
        self.cxt = cx & self.mask;
        (self.t[self.cxt] >> 20) as i32
    }

    /// Fold the observed bit into the estimate for the context last passed to
    /// [`StateMap::p`].
    #[inline(always)]
    pub fn update(&mut self, bit: u32) {
        let slot = &mut self.t[self.cxt & self.mask];
        let n = *slot & 1023;
        let p = (*slot >> 10) as i64;
        let target = if bit == 1 { P_MAX } else { 0 };
        // Moving a fraction 1/(n + 1.5) of the way keeps `p` inside
        // `0..=P_MAX` for every reachable value, so no clamp is needed.
        let p = p + (((target - p) * RATE[(n & 1023) as usize]) >> 16);
        debug_assert!((0..=P_MAX).contains(&p), "state map probability escaped: {p}");
        let n = if n < self.limit { n + 1 } else { n };
        *slot = ((p as u32) << 10) | n;
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
///
/// Upper bound on mixer inputs, so they live in a fixed array rather than a
/// heap vector that is pushed to and cleared for every single bit.
pub const MAX_INPUTS: usize = 16;

pub struct Mixer {
    n: usize,
    weights: Vec<i32>,
    inputs: [i32; MAX_INPUTS],
    count: usize,
    base: usize,
    pr: i32,
    lr: i32,
}

impl Mixer {
    pub fn new(n: usize, contexts: usize, lr: i32) -> Self {
        assert!(n <= MAX_INPUTS, "mixer has {n} inputs, limit is {MAX_INPUTS}");
        Mixer {
            n,
            // Start as a plain average of the inputs.
            weights: vec![(1 << 16) / n as i32; n * contexts],
            inputs: [0; MAX_INPUTS],
            count: 0,
            base: 0,
            pr: 2048,
            lr,
        }
    }

    /// Add one stretched prediction.
    ///
    /// Callers must already be in `-2047..=2047`, which everything derived
    /// from [`stretch`](super::tables::stretch) is by construction. Re-clamping
    /// here cost several percent of total runtime for nothing.
    #[inline(always)]
    pub fn add(&mut self, x: i32) {
        debug_assert!(self.count < self.n);
        debug_assert!((-2047..=2047).contains(&x), "mixer input {x} is out of range");
        self.inputs[self.count & (MAX_INPUTS - 1)] = x;
        self.count += 1;
    }

    /// Select the weight vector to use for this prediction.
    #[inline(always)]
    pub fn set_context(&mut self, cx: usize) {
        self.base = cx * self.n;
        debug_assert!(self.base + self.n <= self.weights.len());
    }

    #[inline(always)]
    pub fn mix(&mut self) -> i32 {
        debug_assert_eq!(self.count, self.n);
        let w = &self.weights[self.base..self.base + self.n];
        let x = &self.inputs[..self.n];
        let dot: i64 = w.iter().zip(x).map(|(&w, &x)| w as i64 * x as i64).sum();
        self.pr = squash((dot >> 16) as i32);
        self.pr
    }

    /// Gradient step on coding loss. For this parameterisation the gradient is
    /// simply `(bit - p) * x`, which is why the update is one multiply-add per
    /// input and no activation derivative appears.
    #[inline(always)]
    pub fn update(&mut self, bit: u32) {
        let err = (((bit as i32) << 12) - self.pr) * self.lr;
        let w = &mut self.weights[self.base..self.base + self.n];
        let x = &self.inputs[..self.n];
        for (w, &x) in w.iter_mut().zip(x) {
            // Saturating rather than clamped: gradient descent keeps these
            // bounded on its own, and this only exists so that pathological
            // input cannot overflow.
            *w = w.saturating_add((x * err + 0x8000) >> 16);
        }
        self.count = 0;
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
        // Every context starts with the same identity curve. Building it once
        // and copying matters: a 64K-context map holds two million entries,
        // and it is rebuilt for every block.
        let mut seed = [0u16; 33];
        for (i, slot) in seed.iter_mut().enumerate() {
            *slot = (squash((i as i32 - 16) * 128) * 16) as u16;
        }
        let mut t = Vec::with_capacity(contexts * 33);
        for _ in 0..contexts {
            t.extend_from_slice(&seed);
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
        let st = &STATES;
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
