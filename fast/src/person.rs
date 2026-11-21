//! An exact person key, cheap enough to carry through both sorts. Nine ASCII
//! bytes fit in 63 bits. Every other spelling uses a collision-checked interner;
//! no hash (or truncated hash) is ever accepted as proof of identity.
use crate::{Map, rs};
use std::sync::RwLock;

const HIGH_BITS: u64 = 0x8080_8080_8080_8080;
const EXTERNAL: u64 = 1 << 63;
const LANES: usize = 64;

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
    pack(word, tail)
}

/// `word` holds the first eight bytes in big-endian order, with zero padding.
#[inline(always)]
pub(crate) fn pack(word: u64, tail: u8) -> Option<u64> {
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
}
