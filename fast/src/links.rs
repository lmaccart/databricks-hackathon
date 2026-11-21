//! Build the canonical-name lookup in parallel, including transitive paths
//! that span input files. Canonical node indexes also serve as exact sort ranks.
use super::{Chunk, Codec, NS, for_rows, shard};
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

const GROUP: usize = 16;
const EMPTY: u8 = 0x80;

struct Lane {
    ctrl: Vec<u8>,
    keys: Vec<u64>,
    roots: Vec<u32>,
    mask: usize,
}

impl Lane {
    fn new(cap: usize) -> Self {
        debug_assert!(cap.is_power_of_two() && cap >= GROUP);
        Self {
            ctrl: vec![EMPTY; cap],
            keys: vec![0; cap],
            roots: vec![0; cap],
            mask: cap - 1,
        }
    }

    #[inline]
    fn fingerprint(hash: u64) -> u8 {
        (hash >> 57) as u8
    }

    // Probe 16 control bytes at once conceptually; keeping fingerprints and
    // payloads separate avoids loading root data for non-matching keys.
    #[inline]
    fn find(&self, hash: u64) -> usize {
        let groups = (self.mask + 1) / GROUP;
        let mut group = (hash as usize >> 4) & (groups - 1);
        let mut step = 0usize;
        let fingerprint = Self::fingerprint(hash);
        loop {
            let base = group * GROUP;
            let mut empty = None;
            for offset in 0..GROUP {
                let i = base + offset;
                if self.ctrl[i] == fingerprint && self.keys[i] == hash {
                    return i;
                }
                if self.ctrl[i] == EMPTY && empty.is_none() {
                    empty = Some(i);
                }
            }
            if let Some(i) = empty {
                return i;
            }
            step += 1;
            group = (group + step) & (groups - 1);
        }
    }

    #[inline]
    fn insert(&mut self, hash: u64, root: u32) {
        let i = self.find(hash);
        debug_assert_eq!(self.ctrl[i], EMPTY);
        self.keys[i] = hash;
        self.roots[i] = root;
        self.ctrl[i] = Self::fingerprint(hash);
    }

    #[inline]
    fn get(&self, hash: u64) -> u32 {
        let i = self.find(hash);
        if self.ctrl[i] == Self::fingerprint(hash) && self.keys[i] == hash {
            self.roots[i]
        } else {
            0
        }
    }

    #[inline]
    fn prefetch(&self, hash: u64) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use std::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
            let base = ((hash as usize >> 4) & ((self.mask + 1) / GROUP - 1)) * GROUP;
            _mm_prefetch(self.ctrl.as_ptr().add(base) as *const i8, _MM_HINT_T0);
            _mm_prefetch(self.keys.as_ptr().add(base) as *const i8, _MM_HINT_T0);
        }
        #[cfg(not(target_arch = "x86_64"))]
        let _ = hash;
    }
}

pub(super) struct People<'a> {
    pub(super) codec: Codec<'a>,
    lanes: Vec<Lane>,
    roots: Vec<u64>,
    routes: Vec<u8>,
    pub(super) count: usize,
}

#[derive(Clone, Copy)]
pub(super) enum Identity {
    Linked(u32),
    Standalone(u64),
}

impl<'a> People<'a> {
    #[inline]
    pub(super) fn lookup_tag(&self, hash: u64) -> u32 {
        self.lanes[shard(hash)].get(hash)
    }

    #[inline]
    pub(super) fn prefetch(&self, hash: u64) {
        self.lanes[shard(hash)].prefetch(hash);
    }

    #[inline]
    pub(super) fn original(&self, rank: u32) -> u64 {
        self.roots[rank as usize]
    }

    #[inline]
    pub(super) fn route(&self, rank: u32) -> u8 {
        self.routes[rank as usize]
    }
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
            let mut lane = Lane::new(cap.max(GROUP));
            for (j, &k) in keys.iter().enumerate() {
                lane.insert(mix(k), j as u32);
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
        (offsets[s] + lanes[s].get(hash) as usize) as u32
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
    let mut roots = keys;
    roots.par_iter_mut().for_each(|k| *k = mix(*k));
    lanes.par_iter_mut().enumerate().for_each(|(s, lane)| {
        for i in 0..lane.ctrl.len() {
            if lane.ctrl[i] != EMPTY {
                let rank = root(&parent, (offsets[s] + lane.roots[i] as usize) as u32);
                lane.roots[i] = rank + 1;
            }
        }
    });
    let routes = roots.iter().map(|&h| shard(h) as u8).collect();
    People {
        codec,
        lanes,
        roots,
        routes,
        count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prehashed_lane_matches_exact_tokens_across_probe_groups() {
        let mut lane = Lane::new(128);
        let keys: Vec<_> = (0..40).map(|i| ((i as u64) << 40) | 0x123).collect();
        for (root, &key) in keys.iter().enumerate() {
            lane.insert(key, root as u32 + 1);
        }
        for (root, &key) in keys.iter().enumerate() {
            assert_eq!(lane.get(key), root as u32 + 1);
        }
        assert_eq!(lane.get((90u64 << 40) | 0x123), 0);
        assert_eq!(lane.get(((1u64 << 40) | 0x123) ^ 1), 0);
    }
}
