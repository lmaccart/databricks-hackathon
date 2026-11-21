//! Exact compact IDs for a sampled prefix and up to eight lowercase base-36
//! bytes. All other names remain in the ordinary exact codec.
use crate::person;

const ONES: u64 = 0x0101_0101_0101_0101;
const HIGH: u64 = 0x8080_8080_8080_8080;
const OFFSETS: [u64; 9] = [
    0,
    1,
    37,
    1333,
    47_989,
    1_727_605,
    62_193_781,
    2_238_976_117,
    80_603_140_213,
];
const END: u64 = OFFSETS[8] + 2_821_109_907_456;
const _: () = assert!(END < 1 << 62);

#[derive(Clone, Copy)]
pub struct Radix {
    pub prefix: u8,
    pub origin: i64,
    pub span: u64,
    pub reciprocal: u64,
    pub shard: u8,
}

impl Radix {
    pub fn new(prefix: u8, ws: i64, we: i64) -> Option<Self> {
        if prefix == 0 || prefix > 127 {
            return None;
        }
        let span: u64 = (we as i128 - ws as i128).try_into().ok()?;
        if span == 0 || ((END / 256) as u128 + 1) * (span as u128) >= u64::MAX as u128 {
            return None;
        }
        let reciprocal = ((1u128 << 64) / u128::from(span)) as u64;
        Some(Self {
            prefix,
            origin: ws,
            span,
            reciprocal,
            shard: 0,
        })
    }

    /// `w` contains the first eight bytes in little endian, `tail` the ninth.
    /// The delimiters have already been checked by the CSV parser.
    #[inline(always)]
    pub fn word(self, w: u64, tail: u8, len: usize) -> u64 {
        if !(1..=9).contains(&len) || w as u8 != self.prefix {
            return 0;
        }
        let n = len - 1;
        let active = if n == 8 { u64::MAX } else { (1u64 << (8 * n)) - 1 };
        let x = ((w >> 8) | (u64::from(tail) << 56)) & active;
        // Each lane has its own guard bit, preventing cross-byte borrows.
        let ge0 = (x | HIGH).wrapping_sub(ONES * u64::from(b'0')) & HIGH;
        let gt9 = (x | HIGH).wrapping_sub(ONES * u64::from(b'9' + 1)) & HIGH;
        let gea = (x | HIGH).wrapping_sub(ONES * u64::from(b'a')) & HIGH;
        let gtz = (x | HIGH).wrapping_sub(ONES * u64::from(b'z' + 1)) & HIGH;
        let valid = (ge0 & !gt9) | (gea & !gtz);
        if x & HIGH != 0 || valid & active != HIGH & active {
            return 0;
        }
        let digits = x
            .wrapping_sub((ONES * u64::from(b'0')) & active)
            .wrapping_sub((gea >> 7).wrapping_mul(39));
        let digits = if n == 0 { 0 } else { digits << (8 * (8 - n)) };
        let d = |i: u32| (digits >> (8 * i)) & 255u64;
        let a = d(0) * 36 + d(1);
        let b = d(2) * 36 + d(3);
        let c = d(4) * 36 + d(5);
        let e = d(6) * 36 + d(7);
        1 + OFFSETS[n] + ((a * 1296 + b) * 1296 + c) * 1296 + e
    }

    pub fn bytes(self, bytes: &[u8]) -> u64 {
        if bytes.is_empty() || bytes.len() > 9 {
            return 0;
        }
        let mut b = [0; 8];
        let n = bytes.len().min(8);
        b[..n].copy_from_slice(&bytes[..n]);
        self.word(u64::from_le_bytes(b), bytes.get(8).copied().unwrap_or(0), bytes.len())
    }

    #[inline]
    pub fn key(self, token: u64, ts: i64) -> u64 {
        (token >> 8) * self.span + (ts as u64).wrapping_sub(self.origin as u64)
    }

    /// floor(2^64 / span) underestimates the quotient by at most one.
    #[inline]
    pub fn div(self, key: u64) -> u64 {
        if self.span == 1 {
            return key;
        }
        let q = ((u128::from(key) * u128::from(self.reciprocal)) >> 64) as u64;
        q + u64::from(key - q * self.span >= self.span)
    }

    pub fn name(self, residual: u64) -> u64 {
        let token = (residual << 8) | u64::from(self.shard);
        let code = token - 1;
        let n = (0..8).find(|&i| code < OFFSETS[i + 1]).unwrap_or(8);
        let mut val = code - OFFSETS[n];
        let mut bytes = [0u8; 9];
        bytes[0] = self.prefix;
        for i in (1..=n).rev() {
            let d = (val % 36) as u8;
            bytes[i] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
            val /= 36;
        }
        crate::links::mix(person::inline(&bytes[..n + 1]).unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_lengths_digits_and_boundaries() {
        let r = Radix::new(b'x', -500, 1_209_600_000).unwrap();
        for s in [
            b"x".as_slice(),
            b"x0",
            b"x00",
            b"x00000000",
            b"xzzzzzzzz",
            b"x1234567",
            b"x12345678",
        ] {
            let t = r.bytes(s);
            assert_ne!(t, 0, "{s:?}");
            let rr = Radix { shard: t as u8, ..r };
            let key = rr.key(t, -499);
            assert_eq!(key / r.span, t >> 8);
            let codec = crate::Codec::new();
            assert_eq!(codec.bytes(crate::links::unmix(rr.name(t >> 8)), &mut [0; 9]), s);
        }
        for s in [b"X1".as_slice(), b"xA", b"x_", b"x:", b"x{", b"x\x80", b"x123456789"] {
            assert_eq!(r.bytes(s), 0, "{s:?}");
        }
        assert!(Radix::new(b'x', i64::MIN, i64::MAX).is_none());
        assert!(Radix::new(0, 0, 1).is_none());
        assert!(Radix::new(128, 0, 1).is_none());
    }

    #[test]
    fn swar_matches_scalar_for_every_byte_and_lane() {
        let r = Radix::new(b'x', -500, 1_209_600_000).unwrap();
        for (n, &offset) in OFFSETS.iter().enumerate() {
            let mut s = vec![b'x'; n + 1];
            for i in 1..=n {
                for byte in 0..=255u8 {
                    s[i] = byte;
                    let mut v = 0u64;
                    let mut valid = true;
                    for &b in &s[1..] {
                        let d = match b {
                            b'0'..=b'9' => b - b'0',
                            b'a'..=b'z' => b - b'a' + 10,
                            _ => {
                                valid = false;
                                0
                            }
                        };
                        v = v * 36 + u64::from(d);
                    }
                    let want = if valid { 1 + offset + v } else { 0 };
                    assert_eq!(r.bytes(&s), want, "{s:?}");
                }
                s[i] = b'x';
            }
        }
    }

    #[test]
    fn reciprocal_division_matches_exact_division() {
        for span in [1, 2, 3, 7, 86_400_000, 1_209_600_000, 1_600_000_000] {
            let r = Radix::new(b'x', 0, span).unwrap();
            let mut x = 0xfedc_ba98_7654_3210u64;
            for key in [0, 1, span as u64 - 1, span as u64, u64::MAX] {
                assert_eq!(r.div(key), key / span as u64);
            }
            for _ in 0..10_000 {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                assert_eq!(r.div(x), x / span as u64);
            }
        }
    }
}
