//! An exact person key, cheap enough to carry through both sorts. Nine ASCII
//! bytes fit in 63 bits. Every other spelling uses a collision-checked interner;
//! no hash (or truncated hash) is ever accepted as proof of identity.
use crate::{Map, rs};
use std::sync::RwLock;

const HIGH_BITS: u64 = 0x8080_8080_8080_8080;
const EXTERNAL: u64 = 1 << 63;
const LANES: usize = 64;

// A byte in the suffix needs only seven bits. Park the seven prefix bits in
// the otherwise unused high bits. Bit 63 stays free for the spill namespace.
const PREFIX_MASK: u64 = HIGH_BITS ^ EXTERNAL;
const PREFIX: [u64; 128] = {
    let mut v = [0; 128];
    let mut n = 0;
    while n < 128 {
        let mut bit = 0;
        while bit < 7 {
            v[n] |= ((n as u64 >> bit) & 1) << (7 + 8 * bit);
            bit += 1;
        }
        n += 1;
    }
    v
};

#[inline]
pub fn inline(bytes: &[u8]) -> Option<u64> {
    // A final NUL would be ambiguous with right padding. Keep such IDs in
    // the interner. Internal NULs are representable and significant.
    let (&last, _) = bytes.split_last()?;
    let n = bytes.len();
    if n > 9 || last == 0 || bytes[0] >= 128 {
        return None;
    }
    let suffix = if n >= 8 {
        u64::from_be_bytes(bytes[n - 8..].try_into().unwrap()) << ((9 - n) * 8)
    } else if n >= 5 {
        let head = u32::from_be_bytes(bytes[1..5].try_into().unwrap());
        let tail = u32::from_be_bytes(bytes[n - 4..].try_into().unwrap());
        (u64::from(head) << 32) | (u64::from(tail) << ((9 - n) * 8))
    } else {
        let mut tmp = [0; 8];
        tmp[..n - 1].copy_from_slice(&bytes[1..]);
        u64::from_be_bytes(tmp)
    };
    if suffix & HIGH_BITS != 0 {
        return None;
    }
    Some(suffix | PREFIX[bytes[0] as usize])
}

#[derive(Default)]
struct Spill<'a> {
    by_name: Map<&'a [u8], usize>,
    by_index: Vec<&'a [u8]>,
}

pub struct Codec<'a> {
    long: [RwLock<Spill<'a>>; LANES],
    hash: ahash::RandomState,
}

impl<'a> Codec<'a> {
    pub fn new() -> Self {
        Self {
            long: std::array::from_fn(|_| RwLock::default()),
            hash: rs(),
        }
    }

    #[inline]
    pub fn key(&self, bytes: &'a [u8]) -> u64 {
        inline(bytes).unwrap_or_else(|| self.intern(bytes))
    }

    #[inline]
    pub fn less(&self, a: u64, b: u64) -> bool {
        if (a | b) & EXTERNAL == 0 {
            let (pa, pb) = (a & PREFIX_MASK, b & PREFIX_MASK);
            pa < pb || (pa == pb && a < b)
        } else {
            self.bytes(a, &mut [0; 9]) < self.bytes(b, &mut [0; 9])
        }
    }

    #[cold]
    fn intern(&self, bytes: &'a [u8]) -> u64 {
        let i = (self.hash.hash_one(bytes) as usize) & (LANES - 1);
        let shard = &self.long[i];
        let known = shard.read().unwrap().by_name.get(bytes).copied();
        let n = known.unwrap_or_else(|| {
            let mut data = shard.write().unwrap();
            let Spill { by_name, by_index } = &mut *data;
            *by_name.entry(bytes).or_insert_with(|| {
                let index = by_index.len();
                by_index.push(bytes);
                index
            })
        });
        EXTERNAL | ((n as u64) << LANES.trailing_zeros()) | i as u64
    }

    #[inline]
    pub fn bytes<'b>(&self, key: u64, buf: &'b mut [u8; 9]) -> &'b [u8]
    where
        'a: 'b,
    {
        if key & EXTERNAL != 0 {
            let names = self.long[(key as usize) & (LANES - 1)].read().unwrap();
            return names.by_index[((key ^ EXTERNAL) >> LANES.trailing_zeros()) as usize];
        }
        buf[0] = (((key & PREFIX_MASK) >> 7).wrapping_mul(0x0102_0408_1020_4080) >> 56) as u8;
        let suffix = key & !HIGH_BITS;
        buf[1..].copy_from_slice(&suffix.to_be_bytes());
        let len = 9 - suffix.trailing_zeros() as usize / 8;
        &buf[..len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::links::{mix, unmix};

    #[test]
    fn inline_order_matches_raw_bytes_across_prefix_and_suffix_lanes() {
        let mut names: Vec<Vec<u8>> = vec![
            vec![0],
            vec![0, 0],
            "é".as_bytes().to_vec(),
            b"a much longer key".to_vec(),
        ];
        for first in 0..=127 {
            for n in 1..=9 {
                for fill in [0, 1, 42, 127] {
                    let mut name = vec![fill; n];
                    name[0] = first;
                    names.push(name);
                }
            }
        }
        names.sort();
        names.dedup();
        let codec = Codec::new();
        let keys: Vec<_> = names.iter().map(|s| codec.key(s)).collect();
        for i in 1..keys.len() {
            assert!(codec.less(keys[i - 1], keys[i]));
            assert!(!codec.less(keys[i], keys[i - 1]));
        }
    }

    #[test]
    fn every_ascii_lane_survives_routing_and_decoding() {
        let codec = Codec::new();
        // Include embedded/trailing zeros, ASCII controls, and all seven bits
        // at every lane rather than testing just generator-shaped names.
        let inputs: Vec<_> = (1..=9)
            .flat_map(|n| {
                (0..n).flat_map(move |i| {
                    (0..=127).map(move |v| {
                        let mut b = vec![b'x'; n];
                        b[i] = v;
                        b
                    })
                })
            })
            .collect();
        for input in &inputs {
            let key = codec.key(input);
            assert_eq!(codec.bytes(unmix(mix(key)), &mut [0; 9]), input.as_slice());
        }
    }
}
