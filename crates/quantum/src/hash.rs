//! Hashing primitives: a fast non-cryptographic hash for integrity checks and
//! context hashing, plus SHA-256 for content-addressed deduplication.

// ---------------------------------------------------------------------------
// Fast hash (xxHash64-style)
// ---------------------------------------------------------------------------

const P1: u64 = 0x9E37_79B1_85EB_CA87;
const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const P3: u64 = 0x1656_67B1_9E37_79F9;
const P4: u64 = 0x85EB_CA77_C2B2_AE63;
const P5: u64 = 0x27D4_EB2F_1656_67C5;

#[inline(always)]
fn round(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(P2)).rotate_left(31).wrapping_mul(P1)
}

#[inline(always)]
fn merge(acc: u64, val: u64) -> u64 {
    let val = round(0, val);
    ((acc ^ val).wrapping_mul(P1)).wrapping_add(P4)
}

#[inline(always)]
fn avalanche(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(P2);
    h ^= h >> 29;
    h = h.wrapping_mul(P3);
    h ^= h >> 32;
    h
}

/// 64-bit hash of `data`. Used for cheap integrity checks, not for security.
pub fn fast_hash(data: &[u8], seed: u64) -> u64 {
    let len = data.len() as u64;
    let mut rest = data;
    let mut h = if data.len() >= 32 {
        let (mut v1, mut v2, mut v3, mut v4) = (
            seed.wrapping_add(P1).wrapping_add(P2),
            seed.wrapping_add(P2),
            seed,
            seed.wrapping_sub(P1),
        );
        while rest.len() >= 32 {
            v1 = round(v1, le64(&rest[0..8]));
            v2 = round(v2, le64(&rest[8..16]));
            v3 = round(v3, le64(&rest[16..24]));
            v4 = round(v4, le64(&rest[24..32]));
            rest = &rest[32..];
        }
        let h = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        let h = merge(h, v1);
        let h = merge(h, v2);
        let h = merge(h, v3);
        merge(h, v4)
    } else {
        seed.wrapping_add(P5)
    };
    h = h.wrapping_add(len);
    while rest.len() >= 8 {
        h = (h ^ round(0, le64(&rest[0..8]))).rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
        rest = &rest[8..];
    }
    if rest.len() >= 4 {
        h = (h ^ (le32(&rest[0..4]) as u64).wrapping_mul(P1))
            .rotate_left(23)
            .wrapping_mul(P2)
            .wrapping_add(P3);
        rest = &rest[4..];
    }
    for &b in rest {
        h = (h ^ (b as u64).wrapping_mul(P5)).rotate_left(11).wrapping_mul(P1);
    }
    avalanche(h)
}

#[inline(always)]
fn le64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}

#[inline(always)]
fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}

/// Mix an accumulator with one more value. The compressor uses this to build
/// context hashes incrementally, so it must be cheap and well-mixed in the
/// low bits (which index hash tables).
#[inline(always)]
pub fn mix(acc: u64, value: u64) -> u64 {
    let h = (acc.wrapping_add(value).wrapping_add(P3)).wrapping_mul(P1);
    h ^ (h >> 29)
}

/// Final avalanche for a context hash.
#[inline(always)]
pub fn finalize(h: u64) -> u64 {
    avalanche(h)
}

// ---------------------------------------------------------------------------
// SHA-256
// ---------------------------------------------------------------------------

/// The round constants are the first 32 bits of the fractional parts of the
/// cube roots of the first 64 primes; the initial state uses square roots of
/// the first 8. Deriving them here rather than pasting a table means the
/// values cannot be silently wrong -- and the test vectors below prove it.
fn derive_constants() -> ([u32; 8], [u32; 64]) {
    let mut primes = Vec::with_capacity(64);
    let mut n = 2u32;
    while primes.len() < 64 {
        if (2..n).take_while(|d| d * d <= n).all(|d| n % d != 0) {
            primes.push(n);
        }
        n += 1;
    }
    let frac = |x: f64| (x.fract() * 4294967296.0) as u32;
    let mut iv = [0u32; 8];
    for (i, slot) in iv.iter_mut().enumerate() {
        *slot = frac((primes[i] as f64).sqrt());
    }
    let mut k = [0u32; 64];
    for (i, slot) in k.iter_mut().enumerate() {
        *slot = frac((primes[i] as f64).cbrt());
    }
    (iv, k)
}

static SHA_CONST: std::sync::LazyLock<([u32; 8], [u32; 64])> =
    std::sync::LazyLock::new(derive_constants);

/// Streaming SHA-256.
pub struct Sha256 {
    state: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total: u64,
}

impl Sha256 {
    pub fn new() -> Self {
        Sha256 { state: SHA_CONST.0, buf: [0; 64], buf_len: 0, total: 0 }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len < 64 {
                // `take` consumed all of `data`, so there is nothing left to
                // do; falling through here would clobber `buf_len` below.
                return;
            }
            let block = self.buf;
            self.compress(&block);
            self.buf_len = 0;
        }
        while data.len() >= 64 {
            let (block, rest) = data.split_at(64);
            self.compress(block.try_into().unwrap());
            data = rest;
        }
        self.buf[..data.len()].copy_from_slice(data);
        self.buf_len = data.len();
    }

    pub fn finish(mut self) -> [u8; 32] {
        let bits = self.total.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buf_len != 56 {
            self.update(&[0]);
        }
        // The padding above advanced `total`, but the length field must
        // describe the original message, so it is written directly.
        let mut block = self.buf;
        block[56..64].copy_from_slice(&bits.to_be_bytes());
        self.compress(&block);
        let mut out = [0u8; 32];
        for (i, w) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let k = &SHA_CONST.1;
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let a = w[i - 15];
            let b = w[i - 2];
            let s0 = a.rotate_right(7) ^ a.rotate_right(18) ^ (a >> 3);
            let s1 = b.rotate_right(17) ^ b.rotate_right(19) ^ (b >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut s = self.state;
        for i in 0..64 {
            let s1 = s[4].rotate_right(6) ^ s[4].rotate_right(11) ^ s[4].rotate_right(25);
            let ch = (s[4] & s[5]) ^ (!s[4] & s[6]);
            let t1 = s[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(k[i])
                .wrapping_add(w[i]);
            let s0 = s[0].rotate_right(2) ^ s[0].rotate_right(13) ^ s[0].rotate_right(22);
            let maj = (s[0] & s[1]) ^ (s[0] & s[2]) ^ (s[1] & s[2]);
            let t2 = s0.wrapping_add(maj);
            s[7] = s[6];
            s[6] = s[5];
            s[5] = s[4];
            s[4] = s[3].wrapping_add(t1);
            s[3] = s[2];
            s[2] = s[1];
            s[1] = s[0];
            s[0] = t1.wrapping_add(t2);
        }
        for i in 0..8 {
            self.state[i] = self.state[i].wrapping_add(s[i]);
        }
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

/// 128-bit content fingerprint: SHA-256 truncated to its first 16 bytes.
///
/// Truncated SHA-256 is what makes deduplication safe to do by hash alone:
/// with 2^32 distinct chunks (~256 TB at the default chunk size) the chance
/// of any collision is around 2^-64.
pub type ChunkId = [u8; 16];

pub fn chunk_id(data: &[u8]) -> ChunkId {
    let mut h = Sha256::new();
    h.update(data);
    let full = h.finish();
    full[..16].try_into().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn sha256_constants_match_the_standard() {
        let (iv, k) = &*SHA_CONST;
        assert_eq!(iv[0], 0x6a09e667);
        assert_eq!(iv[7], 0x5be0cd19);
        assert_eq!(k[0], 0x428a2f98);
        assert_eq!(k[63], 0xc67178f2);
    }

    #[test]
    fn sha256_known_answers() {
        let mut h = Sha256::new();
        h.update(b"");
        assert_eq!(
            hex(&h.finish()),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        let mut h = Sha256::new();
        h.update(b"abc");
        assert_eq!(
            hex(&h.finish()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        let mut h = Sha256::new();
        h.update(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq");
        assert_eq!(
            hex(&h.finish()),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );

        // One million 'a', which exercises multi-block updates and padding.
        let mut h = Sha256::new();
        for _ in 0..1000 {
            h.update(&[b'a'; 1000]);
        }
        assert_eq!(
            hex(&h.finish()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn sha256_is_insensitive_to_chunking() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut a = Sha256::new();
        a.update(&data);
        let want = a.finish();
        for split in [1usize, 7, 63, 64, 65, 1000, 4999] {
            let mut b = Sha256::new();
            for part in data.chunks(split) {
                b.update(part);
            }
            assert_eq!(b.finish(), want, "split {split}");
        }
    }

    #[test]
    fn fast_hash_is_stable_and_sensitive() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 256) as u8).collect();
        let h = fast_hash(&data, 0);
        assert_eq!(h, fast_hash(&data, 0));
        assert_ne!(h, fast_hash(&data, 1));
        for flip in [0usize, 1, 500, 999] {
            let mut other = data.clone();
            other[flip] ^= 1;
            assert_ne!(h, fast_hash(&other, 0), "flip at {flip}");
        }
        // Different lengths of the same prefix must differ.
        assert_ne!(fast_hash(&data[..100], 0), fast_hash(&data[..101], 0));
    }
}
