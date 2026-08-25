//! Logistic <-> linear probability conversion tables.
//!
//! The model works in the *logistic* domain (`stretch(p) = ln(p / (1 - p))`),
//! because that is the domain in which mixing predictions linearly is the
//! right thing to do. The arithmetic coder needs *linear* probabilities, so
//! `squash` converts back.

/// `squash(x) = 4096 / (1 + e^(-x/256))`, i.e. the inverse of [`stretch`].
///
/// Input is clamped to `-2047..=2047`, output is a 12-bit probability in
/// `1..=4095`.
pub fn squash(d: i32) -> i32 {
    /// 33 samples of `4096 / (1 + e^(-x/256))`, one every 128 stretch units,
    /// clamped away from 0 and 4096 so no probability is ever certain.
    /// `logistic_table_matches_the_formula` re-derives these.
    const T: [i32; 33] = [
        1, 2, 4, 6, 10, 17, 27, 45, 74, 120, 194, 311, 488, 747, 1102, 1546, 2048, 2550, 2994,
        3349, 3608, 3785, 3902, 3976, 4022, 4051, 4069, 4079, 4086, 4090, 4092, 4094, 4095,
    ];
    let d = d.clamp(-2047, 2047);
    let w = d & 127;
    let idx = ((d >> 7) + 16) as usize;
    (T[idx] * (128 - w) + T[idx + 1] * w + 64) >> 7
}

/// Lookup table for [`stretch`], built once by inverting [`squash`].
struct StretchTable([i16; 4096]);

impl StretchTable {
    const fn zeroed() -> Self {
        StretchTable([0; 4096])
    }

    fn build() -> Self {
        let mut t = StretchTable::zeroed();
        let mut pi = 0usize;
        for x in -2047..=2047i32 {
            let v = squash(x) as usize;
            // Every 12-bit probability in `pi..=v` stretches back to `x`.
            for p in pi..=v {
                t.0[p] = x as i16;
            }
            pi = v + 1;
        }
        for p in pi..4096 {
            t.0[p] = 2047;
        }
        t
    }
}

static STRETCH: std::sync::LazyLock<StretchTable> = std::sync::LazyLock::new(StretchTable::build);

/// `stretch(p) = ln(p / (1 - p))` scaled so the result lands in `-2047..=2047`.
///
/// `p` is a 12-bit probability (`0..4096`).
#[inline(always)]
pub fn stretch(p: i32) -> i32 {
    debug_assert!((0..4096).contains(&p));
    STRETCH.0[p as usize] as i32
}

/// Force the stretch table to be materialised (useful before timing loops).
pub fn warm_up() {
    let _ = stretch(2048);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squash_is_monotonic_and_bounded() {
        let mut prev = 0;
        for x in -2047..=2047 {
            let v = squash(x);
            assert!((1..=4095).contains(&v), "squash({x}) = {v}");
            assert!(v >= prev, "not monotonic at {x}");
            prev = v;
        }
        assert_eq!(squash(0), 2048);
        assert_eq!(squash(-100000), squash(-2047));
        assert_eq!(squash(100000), squash(2047));
    }

    #[test]
    fn logistic_table_matches_the_formula() {
        for x in (-2048..=2048).step_by(128) {
            let want = 4096.0 / (1.0 + (-(x as f64) / 256.0).exp());
            let got = squash(x.clamp(-2047, 2047)) as f64;
            assert!((got - want.clamp(1.0, 4095.0)).abs() <= 1.5, "squash({x}) = {got}, want {want}");
        }
    }

    #[test]
    fn squash_is_symmetric() {
        for x in 1..2047 {
            // Interpolation rounding allows a single unit of slack.
            assert!((squash(x) + squash(-x) - 4096).abs() <= 1, "asymmetric at {x}");
        }
    }

    #[test]
    fn stretch_inverts_squash() {
        // stretch(squash(x)) should return x to within the quantisation step.
        for x in -2047..=2047 {
            let round_trip = stretch(squash(x));
            assert!(
                (round_trip - x).abs() <= 128,
                "stretch(squash({x})) = {round_trip}"
            );
        }
    }

    #[test]
    fn stretch_is_monotonic() {
        for p in 1..4096 {
            assert!(stretch(p) >= stretch(p - 1));
        }
        assert_eq!(stretch(2048), 0);
    }
}
