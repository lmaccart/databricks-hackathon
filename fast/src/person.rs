//! Exact role-local keys. A sampled common prefix permits a raw-word suffix;
//! otherwise nine ASCII bytes fit in 63 bits. All other spellings are interned.
use crate::{Map, rs};
use std::sync::RwLock;

const HIGH_BITS: u64 = 0x8080_8080_8080_8080;
const EXTERNAL: u64 = 1 << 63;
const LANES: usize = 64;

pub struct PrefixSample {
    counts: [usize; 256],
    total: usize,
}

impl Default for PrefixSample {
    fn default() -> Self {
        Self {
            counts: [0; 256],
            total: 0,
        }
    }
}

impl PrefixSample {
    pub fn add(&mut self, bytes: &[u8]) {
        if bytes.is_empty() || self.total >= 256 {
            return;
        }
        self.total += 1;
        if (2..=9).contains(&bytes.len()) && bytes[1] < 128 && *bytes.last().unwrap() != 0 {
            self.counts[bytes[0] as usize] += 1;
        }
    }

    pub fn prefix(&self) -> Option<u8> {
        let (prefix, &count) = self.counts.iter().enumerate().max_by_key(|&(_, n)| n)?;
        (self.total >= 16 && count * 4 >= self.total * 3).then_some(prefix as u8)
    }
}

#[inline]
fn inline_prefix(bytes: &[u8], prefix: u8) -> Option<u64> {
    let n = bytes.len();
    if !(2..=9).contains(&n) || bytes[0] != prefix || bytes[n - 1] == 0 {
        return None;
    }
    let word = if n == 9 {
        u64::from_be_bytes(bytes[1..9].try_into().unwrap())
    } else if n == 8 {
        u64::from_be_bytes(bytes[..8].try_into().unwrap()) << 8
    } else {
        let mut buf = [0; 8];
        buf[..n - 1].copy_from_slice(&bytes[1..]);
        u64::from_be_bytes(buf)
    };
    (word & EXTERNAL == 0).then_some(word)
}

#[inline]
pub fn inline(bytes: &[u8]) -> Option<u64> {
    // A final NUL would be ambiguous with right padding. Keep such IDs in
    // the interner. Internal NULs are representable and significant.
    let (&last, _) = bytes.split_last()?;
    if last == 0 || bytes.len() > 9 {
        return None;
    }
    let (word, tail) = if bytes.len() >= 8 {
        (
            u64::from_be_bytes(bytes[..8].try_into().unwrap()),
            bytes.get(8).copied().unwrap_or(0),
        )
    } else {
        let mut tmp = [0; 8];
        tmp[..bytes.len()].copy_from_slice(bytes);
        (u64::from_be_bytes(tmp), 0)
    };
    if word & HIGH_BITS != 0 || tail > 127 {
        return None;
    }
    // Compress unsigned byte pairs, then quads, then the entire word.
    let pairs = (word & 0x007f_007f_007f_007f) | ((word >> 1) & 0x3f80_3f80_3f80_3f80);
    let quads = (pairs & 0x0000_3fff_0000_3fff) | ((pairs >> 2) & 0x0fff_c000_0fff_c000);
    let eight = (quads & 0x0fff_ffff) | ((quads >> 4) & 0x00ff_ffff_f000_0000);
    Some((eight << 7) | u64::from(tail))
}

#[derive(Default)]
struct Spill<'a> {
    by_name: Map<&'a [u8], usize>,
    by_index: Vec<&'a [u8]>,
}

pub struct Codec<'a> {
    long: [RwLock<Spill<'a>>; LANES],
    hash: ahash::RandomState,
    prefix: Option<u8>,
}

impl<'a> Codec<'a> {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::with_prefix(None)
    }

    pub fn with_prefix(prefix: Option<u8>) -> Self {
        Self {
            long: std::array::from_fn(|_| RwLock::default()),
            hash: rs(),
            prefix,
        }
    }

    #[inline]
    pub fn key(&self, bytes: &'a [u8]) -> u64 {
        match self.prefix {
            Some(prefix) => inline_prefix(bytes, prefix),
            None => inline(bytes),
        }
        .unwrap_or_else(|| self.intern(bytes))
    }

    #[inline]
    pub fn less(&self, a: u64, b: u64) -> bool {
        if (a | b) & EXTERNAL == 0 {
            a < b
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
        if let Some(prefix) = self.prefix {
            buf[0] = prefix;
            let suffix = key.to_be_bytes();
            let len = 8 - (key.trailing_zeros() as usize / 8);
            buf[1..1 + len].copy_from_slice(&suffix[..len]);
            return &buf[..1 + len];
        }
        for (i, v) in buf.iter_mut().enumerate() {
            *v = ((key >> (7 * (8 - i))) & 127) as u8;
        }
        let len = 9 - (key.trailing_zeros() as usize / 7).min(9);
        &buf[..len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::links::{mix, unmix};

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

    #[test]
    fn raw_prefix_roundtrip_and_order_for_all_byte_boundaries() {
        for prefix in [0, b'u', b'e', 0xc3, 255] {
            let codec = Codec::with_prefix(Some(prefix));
            let mut inputs = vec![vec![prefix], vec![prefix, 0], vec![prefix, 0, 0], vec![prefix; 12]];
            for n in 2..=10 {
                for i in 0..n {
                    for v in 0..=255 {
                        let mut b = vec![b'x'; n];
                        b[0] = prefix;
                        b[i] = v;
                        inputs.push(b);
                    }
                }
            }
            inputs.sort();
            inputs.dedup();
            let keys: Vec<_> = inputs.iter().map(|s| codec.key(s)).collect();
            for (s, &key) in inputs.iter().zip(&keys) {
                assert_ne!(key, 0);
                assert_eq!(codec.bytes(unmix(mix(key)), &mut [0; 9]), s);
            }
            for pair in keys.windows(2) {
                assert_ne!(pair[0], pair[1]);
                assert!(codec.less(pair[0], pair[1]));
            }
        }
    }

    #[test]
    fn diverse_sample_disables_prefix_and_roles_are_independent() {
        let mut a = PrefixSample::default();
        let mut b = PrefixSample::default();
        for _ in 0..32 {
            a.add(b"alpha");
            b.add(b"zebra");
        }
        assert_eq!(a.prefix(), Some(b'a'));
        assert_eq!(b.prefix(), Some(b'z'));
        for _ in 0..32 {
            a.add(b"beta");
        }
        assert_eq!(a.prefix(), None);
    }
}
