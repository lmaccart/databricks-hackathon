//! Optional raw-byte lookup for a frequent fixed-width family learned from the
//! links. It is an accelerator, not another spelling or numbering scheme.
use crate::person::{HIGH_BITS, pack_ascii};
use rayon::prelude::*;

const OVERFLOW: u64 = 1 << 63;
const BUILD_SHARDS: usize = 16;

#[derive(Clone, Copy)]
pub(super) struct Shape {
    prefix: u8,
    width: usize,
}

impl Shape {
    pub(super) fn learn(keys: &[Vec<u64>]) -> Option<Self> {
        // Bounded, deterministic samples across the (sorted) union-find lanes.
        // A poor sample can only select an unhelpful optimization.
        let mut seen = [[0usize; 128]; 10];
        let mut n = 0;
        for lane in keys {
            for &key in lane.iter().step_by(lane.len().div_ceil(128).max(1)) {
                n += 1;
                if key >> 63 == 0 {
                    let width = 9 - key.trailing_zeros() as usize / 7;
                    seen[width][(key >> 56) as usize] += 1;
                }
            }
        }
        let (width, prefix, count) = (2..=9)
            .flat_map(|w| seen[w].into_iter().enumerate().map(move |(p, n)| (w, p, n)))
            .max_by_key(|&(_, _, n)| n)?;
        (count >= 64 && count * 2 >= n).then_some(Self {
            prefix: prefix as u8,
            width,
        })
    }

    #[inline]
    pub(super) fn raw(self, uid: &[u8]) -> Option<u64> {
        if uid.len() != self.width || uid[0] != self.prefix {
            return None;
        }
        let raw = if uid.len() == 9 {
            u64::from_le_bytes(uid[1..].try_into().unwrap())
        } else {
            let mut bytes = [0; 8];
            bytes[..uid.len() - 1].copy_from_slice(&uid[1..]);
            u64::from_le_bytes(bytes)
        };
        // Keep spill spellings on the existing byte-exact path. Reject trailing
        // NUL before using zero as the empty tag; the internal NULs are fine.
        (raw & HIGH_BITS == 0 && uid[uid.len() - 1] != 0).then_some(raw)
    }

    #[inline]
    pub(super) fn packed_raw(self, key: u64) -> Option<u64> {
        if key >> 56 != u64::from(self.prefix) || 9 - key.trailing_zeros() as usize / 7 != self.width {
            return None;
        }
        // Expand the eight packed suffix lanes; only used during construction.
        let mut raw = 0u64;
        for i in 0..8 {
            raw |= ((key >> (7 * (7 - i))) & 127) << (8 * i);
        }
        Some(raw)
    }

    #[inline]
    fn key(self, raw: u64) -> u64 {
        let word = (u64::from(self.prefix) << 56) | (raw.swap_bytes() >> 8);
        pack_ascii(word, (raw >> 56) as u8)
    }
}

#[inline]
fn hash(raw: u64) -> u64 {
    (raw ^ (raw >> 33)).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

#[derive(Clone, Copy, Default)]
#[repr(align(64))]
struct Bucket {
    tags: [u64; 4],
    names: [u64; 4],
}

impl Bucket {
    fn insert(&mut self, raw: u64, name: u64) {
        if let Some(i) = self.tags.iter().position(|&tag| tag == 0) {
            self.tags[i] = raw;
            self.names[i] = name;
        } else {
            self.tags[0] |= OVERFLOW;
        }
    }

    #[inline]
    fn lookup(&self, raw: u64) -> Match {
        let first = self.tags[0];
        let i = if first & !OVERFLOW == raw {
            0
        } else if self.tags[1] == raw {
            1
        } else if self.tags[2] == raw {
            2
        } else if self.tags[3] == raw {
            3
        } else if first & OVERFLOW != 0 {
            return Match::Overflow;
        } else {
            return Match::Unlinked;
        };
        Match::Canonical(self.names[i])
    }
}

pub(super) enum Match {
    Canonical(u64),
    Unlinked,
    Overflow,
}

pub(super) struct Family {
    pub(super) shape: Shape,
    buckets: Vec<Bucket>,
    shift: u32,
}

pub(super) type BuildParts = [Vec<(u64, u64, u64)>; BUILD_SHARDS];

pub(super) fn add(parts: &mut BuildParts, raw: u64, name: u64) {
    let h = hash(raw);
    parts[(h >> (64 - BUILD_SHARDS.ilog2())) as usize].push((h, raw, name));
}

impl Family {
    pub(super) fn build(shape: Shape, parts: Vec<BuildParts>) -> Option<Self> {
        let n: usize = parts.iter().flatten().map(Vec::len).sum();
        if n == 0 {
            return None;
        }
        let cap = n.div_ceil(2).next_power_of_two().max(BUILD_SHARDS);
        let shift = 64 - cap.ilog2();
        let per_shard = cap / BUILD_SHARDS;
        let mut buckets = vec![Bucket::default(); cap];
        buckets
            .par_chunks_exact_mut(per_shard)
            .enumerate()
            .for_each(|(s, slice)| {
                for p in &parts {
                    for &(h, raw, name) in &p[s] {
                        slice[(h >> shift) as usize & (per_shard - 1)].insert(raw, name);
                    }
                }
            });
        Some(Self { shape, buckets, shift })
    }

    #[inline]
    pub(super) fn index(&self, raw: u64) -> usize {
        (hash(raw) >> self.shift) as usize
    }

    #[inline]
    pub(super) fn lookup(&self, index: usize, raw: u64) -> Match {
        self.buckets[index].lookup(raw)
    }

    #[inline]
    pub(super) fn exact_key(&self, raw: u64) -> u64 {
        self.shape.key(raw)
    }

    #[inline]
    pub(super) fn address(&self, index: usize) -> *const u8 {
        (&self.buckets[index] as *const Bucket).cast()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::person::inline;

    #[test]
    fn every_supported_width_round_trips_without_normalization() {
        for width in 2..=9 {
            let shape = Shape { prefix: b'0', width };
            let mut name = vec![b'0'; width];
            for pos in 1..width {
                for ch in 0..=255 {
                    name[pos] = ch;
                    let expected = inline(&name);
                    assert_eq!(shape.raw(&name).map(|r| shape.key(r)), expected);
                    if let Some(k) = expected {
                        assert_eq!(shape.raw(&name), shape.packed_raw(k));
                    }
                }
                name[pos] = b'0';
            }
            assert!(shape.raw(&name[..width - 1]).is_none());
            name[0] = b'+';
            assert!(shape.raw(&name).is_none());
        }
    }

    #[test]
    fn full_cache_line_discriminates_hits_and_requests_exact_overflow() {
        let mut b = Bucket::default();
        assert!(matches!(b.lookup(1), Match::Unlinked));
        for i in 1..=4 {
            b.insert(i, i + 100);
        }
        assert!(matches!(b.lookup(5), Match::Unlinked));
        b.insert(5, 105);
        assert!(matches!(b.lookup(5), Match::Overflow));
        assert!(matches!(b.lookup(6), Match::Overflow));
        for i in 1..=4 {
            assert!(matches!(b.lookup(i), Match::Canonical(n) if n == i + 100));
        }
    }

    #[test]
    fn learn_only_from_a_frequent_observed_shape() {
        assert!(Shape::learn(&[]).is_none());
        let keys: Vec<_> = (0..64)
            .map(|i| inline(format!("Z{i:05x}").as_bytes()).unwrap())
            .collect();
        assert!(Shape::learn(&[keys[..63].to_vec()]).is_none());
        let shape = Shape::learn(&[keys]).unwrap();
        assert_eq!((shape.prefix, shape.width), (b'Z', 6));
    }
}
