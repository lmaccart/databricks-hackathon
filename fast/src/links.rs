//! Build the canonical-name lookup in parallel, including transitive paths
//! that span input files. The final table routes directly to owner bands.
use super::{
    Chunk, Codec, Map, NS,
    bands::{BAND, BAND_BITS},
    for_rows, rs, shard,
};
use rayon::prelude::*;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

// The raw-user partition and the eventual singleton owner use the same bits.
const LANES: usize = NS;

#[inline]
pub(super) fn mix(key: u64) -> u64 {
    let x = (key ^ (key >> 27)).wrapping_mul(0xa24b_aed4_963e_e407);
    x ^ (x >> 33)
}

/// `mix` is a permutation, so the routing hash can *be* the person token.
/// Unpack it only when spelling a key for the output.
#[inline]
pub(super) fn unmix(x: u64) -> u64 {
    const INVERSE: u64 = {
        let k = 0xa24b_aed4_963e_e407u64;
        let mut inv = 1u64;
        let mut i = 0;
        while i < 6 {
            inv = inv.wrapping_mul(2u64.wrapping_sub(k.wrapping_mul(inv)));
            i += 1;
        }
        inv
    };
    let y = (x ^ (x >> 33)).wrapping_mul(INVERSE);
    y ^ (y >> 27) ^ (y >> 54)
}

#[derive(Clone, Copy, Default)]
struct Slot {
    key: u64,
    value: u64,
}

pub(super) struct Lane {
    slots: Vec<Slot>,
    mask: usize,
    used: usize,
    pub(super) routes: Vec<(usize, usize)>, // local band slot -> (owner shard, owner band)
}

impl Lane {
    #[inline]
    fn find(&self, key: u64, hash: u64) -> usize {
        let mut i = hash as usize & self.mask;
        loop {
            let k = self.slots[i].key;
            if k == key || k == 0 {
                return i;
            }
            i = (i + 1) & self.mask;
        }
    }

    // After the graph is complete this lane belongs to one producer. Reuse
    // its vacant slots for exact singleton routes instead of looking them up
    // in a second table on every hot-cache miss.
    #[inline]
    pub(super) fn resolve(&mut self, hash: u64, insert: impl FnOnce() -> usize) -> usize {
        let mut i = self.find(hash, hash);
        if self.slots[i].key != 0 {
            return self.slots[i].value as usize;
        }
        if self.used >= self.slots.len() / 2 + self.slots.len() / 8 {
            self.grow();
            i = self.find(hash, hash);
        }
        let value = insert();
        self.slots[i] = Slot {
            key: hash,
            value: value as u64,
        };
        self.used += 1;
        value
    }

    #[cold]
    fn grow(&mut self) {
        let cap = (2 * self.slots.len()).max(16);
        self.mask = cap - 1;
        for slot in std::mem::replace(&mut self.slots, vec![Slot::default(); cap]) {
            if slot.key != 0 {
                let i = self.find(slot.key, slot.key);
                self.slots[i] = slot;
            }
        }
    }
}

pub(super) struct People<'a> {
    pub(super) codec: Codec<'a>,
    pub(super) lanes: Vec<Lane>,
    pub(super) roots: Vec<Vec<u64>>, // owner shard -> dense linked prefix of exact name keys
    pub(super) count: usize,
}

#[inline]
fn root(parent: &[AtomicU32], mut x: u32) -> u32 {
    loop {
        let node = &parent[x as usize];
        let p = node.load(Relaxed);
        if p == x {
            return x;
        }
        let gp = parent[p as usize].load(Relaxed);
        if p == gp {
            return p;
        }
        let _ = node.compare_exchange_weak(p, gp, Relaxed, Relaxed);
        x = gp;
    }
}

pub(super) fn person_names<'a>(chunks: &[Chunk<'a>]) -> People<'a> {
    let codec = Codec::new();
    let parsed: Vec<_> = chunks
        .par_iter()
        .map(|c| {
            let mut edges = Vec::new();
            let mut keys: Vec<Vec<u64>> = (0..LANES).map(|_| Vec::new()).collect();
            for_rows(c, |r| {
                if r[0].is_empty() || r[1].is_empty() {
                    return;
                }
                let a = codec.key(r[0]);
                let b = codec.key(r[1]);
                edges.push((a, b));
                for k in [a, b] {
                    keys[shard(mix(k))].push(k);
                }
            });
            (edges, keys)
        })
        .collect();
    let (edges, key_parts): (Vec<_>, Vec<_>) = parsed.into_iter().unzip();
    let built: Vec<_> = (0..LANES)
        .into_par_iter()
        .map(|s| {
            let n = key_parts.iter().map(|p| p[s].len()).sum::<usize>();
            let mut keys = Vec::with_capacity(n);
            for p in &key_parts {
                keys.extend_from_slice(&p[s]);
            }
            keys.sort_unstable();
            keys.dedup();
            let cap = (keys.len() * 2 + 1).next_power_of_two();
            let mut lane = Lane {
                slots: vec![Slot::default(); cap],
                mask: cap - 1,
                used: keys.len(),
                routes: Vec::new(),
            };
            for (j, &k) in keys.iter().enumerate() {
                let i = lane.find(k, mix(k));
                lane.slots[i] = Slot {
                    key: k,
                    value: j as u64,
                };
            }
            (lane, keys)
        })
        .collect();
    drop(key_parts);
    let (mut lanes, groups): (Vec<_>, Vec<_>) = built.into_iter().unzip();
    let mut offsets = [0; LANES];
    let mut count = 0usize;
    for (s, g) in groups.iter().enumerate() {
        offsets[s] = count;
        count += g.len();
    }
    assert!(
        u32::try_from(count).is_ok(),
        "too many linked IDs for addressable memory"
    );
    let keys: Vec<u64> = groups.into_iter().flatten().collect();
    let parent: Vec<_> = (0..count as u32).map(AtomicU32::new).collect();
    let index = |key| {
        let hash = mix(key);
        let s = shard(hash);
        (offsets[s] + lanes[s].slots[lanes[s].find(key, hash)].value as usize) as u32
    };
    edges
        .par_iter()
        .flat_map(|part| part.par_chunks(8192))
        .for_each(|part| {
            for &(a, b) in part {
                let (mut a, mut b) = (index(a), index(b));
                loop {
                    a = root(&parent, a);
                    b = root(&parent, b);
                    if a == b {
                        break;
                    }
                    let ka = keys[a as usize];
                    let kb = keys[b as usize];
                    // The ASCII encoding is order preserving for its domain.
                    // Spill numbers are not; compare their original bytes.
                    let swap = if (ka | kb) >> 63 == 0 {
                        ka > kb
                    } else {
                        codec.bytes(ka, &mut [0; 9]) > codec.bytes(kb, &mut [0; 9])
                    };
                    if swap {
                        std::mem::swap(&mut a, &mut b);
                    }
                    // Every successful edge points to a strictly smaller name.
                    // A concurrent union can change either root, never form a cycle.
                    if parent[b as usize].compare_exchange(b, a, Relaxed, Relaxed).is_ok() {
                        break;
                    }
                }
            }
        });
    // A canonical node already lives in its actual destination shard. Number
    // only roots, not aliases or nodes in other shards, in that shard's prefix.
    let mut dense = vec![0u32; count];
    let mut tail = dense.as_mut_slice();
    let mut parts = Vec::with_capacity(LANES);
    for (s, &start) in offsets.iter().enumerate() {
        let end = offsets.get(s + 1).copied().unwrap_or(count);
        let (head, rest) = tail.split_at_mut(end - start);
        tail = rest;
        parts.push((start, head));
    }
    let roots: Vec<Vec<u64>> = parts
        .into_par_iter()
        .map(|(offset, ranks)| {
            let mut names = Vec::new();
            for (i, rank) in ranks.iter_mut().enumerate() {
                let idx = offset + i;
                if parent[idx].load(Relaxed) as usize == idx {
                    *rank = names.len() as u32;
                    names.push(keys[idx]);
                }
            }
            names
        })
        .collect();
    lanes.par_iter_mut().enumerate().for_each(|(s, lane)| {
        lane.routes
            .extend((0..roots[s].len().div_ceil(BAND)).map(|band| (s, band)));
        let mut remote = Map::with_hasher(rs());
        for slot in &mut lane.slots {
            if slot.key != 0 {
                let node = root(&parent, (offsets[s] + slot.value as usize) as u32) as usize;
                let dest = shard(mix(keys[node]));
                let rank = dense[node] as usize;
                let band = rank >> BAND_BITS;
                let route = if dest == s {
                    band
                } else {
                    *remote.entry((dest, band)).or_insert_with(|| {
                        let i = lane.routes.len();
                        lane.routes.push((dest, band));
                        i
                    })
                };
                slot.value = ((route << BAND_BITS) | (rank & (BAND - 1))) as u64;
                // The permutation is exact and preserves the original probe
                // index. The join can compare its existing token verbatim.
                slot.key = mix(slot.key);
            }
        }
    });
    People {
        codec,
        lanes,
        roots,
        count,
    }
}
