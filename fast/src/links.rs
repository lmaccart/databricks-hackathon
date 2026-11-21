//! Build the canonical-name lookup in parallel, including transitive paths
//! that span input files. Canonical node indexes also serve as exact sort ranks.
use super::{Chunk, Codec, Map, NS, SHARD_SHIFT, for_rows, rs};
use rayon::prelude::*;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

const LANES: usize = 64;

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

struct Lane {
    slots: Vec<Slot>,
    mask: usize,
}

impl Lane {
    #[inline]
    fn find(&self, key: u64, hash: u64) -> usize {
        let mut i = (hash >> LANES.trailing_zeros()) as usize & self.mask;
        loop {
            let k = self.slots[i].key;
            if k == key || k == 0 {
                return i;
            }
            i = (i + 1) & self.mask;
        }
    }
}

// The lane and natural slot supply all low `bits` of the mixed key. The
// remaining bits are stored verbatim, never as an approximate fingerprint.
struct CompactLane {
    slots: Vec<u64>,
    mask: usize,
    quotient_mask: u64,
    rank_mask: u64,
    overflow_flag: u64,
    overflow: Map<u64, u32>,
}

fn compact_capacity(count: usize) -> Option<(usize, u32)> {
    let total = count.checked_add(1)?.checked_mul(2)?.max(LANES);
    let total = total.checked_next_power_of_two()?;
    let bits = total.trailing_zeros();
    if bits >= 64 || (count as u64) >= (1u64 << (bits - 1)) {
        return None;
    }
    Some((total / LANES, bits))
}

impl CompactLane {
    fn new(cap: usize, bits: u32) -> Self {
        let flag = 1u64 << (bits - 1);
        Self {
            slots: vec![0; cap],
            mask: cap - 1,
            quotient_mask: !((1u64 << bits) - 1),
            rank_mask: flag - 1,
            overflow_flag: flag,
            overflow: Map::with_hasher(rs()),
        }
    }

    #[inline]
    fn index(&self, h: u64) -> usize {
        (h >> LANES.trailing_zeros()) as usize & self.mask
    }

    fn insert(&mut self, h: u64, rank: u32) {
        let rank = u64::from(rank) + 1;
        assert!(rank <= self.rank_mask);
        let i = self.index(h);
        if self.slots[i] & self.rank_mask == 0 {
            self.slots[i] = (h & self.quotient_mask) | rank;
        } else {
            // Different mixed keys with this index must have different full
            // quotients; only colliders go in the exact secondary table.
            assert_ne!(self.slots[i] & self.quotient_mask, h & self.quotient_mask);
            self.slots[i] |= self.overflow_flag;
            assert!(self.overflow.insert(h, (rank - 1) as u32).is_none());
        }
    }

    #[inline]
    fn get(&self, h: u64) -> Option<u32> {
        let slot = self.slots[self.index(h)];
        let rank = slot & self.rank_mask;
        if rank == 0 {
            None
        } else if (slot ^ h) & self.quotient_mask == 0 {
            Some((rank - 1) as u32)
        } else if slot & self.overflow_flag != 0 {
            self.overflow.get(&h).copied()
        } else {
            None
        }
    }
}

enum Lookup {
    Compact(Vec<CompactLane>),
    Full(Vec<Lane>),
}

pub(super) struct People<'a> {
    pub(super) codec: Codec<'a>,
    lookup: Lookup,
    roots: Vec<u64>,
    pub(super) count: usize,
}

// This bit is disjoint from the packed routing shard and the canonical node
// index. It is internal to the link table, not reserved in arbitrary person keys.
const LINKED: u64 = 1 << 63;
const _: () = assert!(SHARD_SHIFT >= 32 && ((NS as u64 - 1) << SHARD_SHIFT) & LINKED == 0);

#[inline]
fn linked_route(rank: u32) -> u64 {
    ((u64::from(rank) & (NS as u64 - 1)) << SHARD_SHIFT) | u64::from(rank)
}

#[derive(Clone, Copy)]
pub(super) enum Identity {
    Linked(u64), // shard bits at their normal position, root node index in low bits
    Standalone(u64),
}

impl<'a> People<'a> {
    #[inline]
    pub(super) fn lookup(&self, key: u64, h: u64) -> Identity {
        let lane = h as usize & (LANES - 1);
        match &self.lookup {
            Lookup::Compact(lanes) => match lanes[lane].get(h) {
                Some(rank) => Identity::Linked(linked_route(rank)),
                None => Identity::Standalone(h),
            },
            Lookup::Full(lanes) => {
                let lane = &lanes[lane];
                let s = lane.slots[lane.find(key, h)];
                if s.value & LINKED != 0 {
                    Identity::Linked(s.value ^ LINKED)
                } else {
                    Identity::Standalone(h)
                }
            }
        }
    }

    #[inline]
    pub(super) fn original(&self, rank: u32) -> u64 {
        self.roots[rank as usize]
    }

    #[inline]
    pub(super) fn prefetch(&self, h: u64) {
        let lane = h as usize & (LANES - 1);
        let addr = match &self.lookup {
            Lookup::Compact(lanes) => {
                let lane = &lanes[lane];
                (&lane.slots[lane.index(h)] as *const u64).cast::<u8>()
            }
            Lookup::Full(lanes) => {
                let lane = &lanes[lane];
                let i = (h >> LANES.trailing_zeros()) as usize & lane.mask;
                (&lane.slots[i] as *const Slot).cast::<u8>()
            }
        };
        // Only an in-bounds address in an immutable, live table; prefetch is
        // a hint, never an actual read used for a decision.
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use std::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
            _mm_prefetch(addr.cast(), _MM_HINT_T0);
        }
        #[cfg(target_arch = "aarch64")]
        unsafe {
            std::arch::asm!("prfm pldl1keep, [{p}]", p = in(reg) addr,
                            options(readonly, nostack, preserves_flags));
        }
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        let _ = addr;
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
                    keys[mix(k) as usize & (LANES - 1)].push(k);
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
        let s = hash as usize & (LANES - 1);
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
    let mut roots = keys;
    roots.par_iter_mut().for_each(|k| *k = mix(*k));
    let lookup = if let Some((cap, bits)) = compact_capacity(count) {
        let compact = lanes
            .into_par_iter()
            .enumerate()
            .map(|(s, old)| {
                drop(old);
                let mut lane = CompactLane::new(cap, bits);
                let end = if s + 1 == LANES { count } else { offsets[s + 1] };
                for i in offsets[s]..end {
                    lane.insert(roots[i], root(&parent, i as u32));
                }
                lane
            })
            .collect();
        Lookup::Compact(compact)
    } else {
        lanes.par_iter_mut().enumerate().for_each(|(s, lane)| {
            for slot in &mut lane.slots {
                if slot.key != 0 {
                    let rank = root(&parent, (offsets[s] + slot.value as usize) as u32);
                    slot.value = LINKED | linked_route(rank);
                }
            }
        });
        Lookup::Full(lanes)
    };
    People {
        codec,
        lookup,
        roots,
        count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_exact_collision_and_zero_quotient() {
        let mut lane = CompactLane::new(4, 8);
        // Low six lane bits and the next two bucket bits are implicit.
        let entries = [(0, 0), (256, 2), (512, 7), (1 << 63, 4), (64, 3), (320, 1)];
        for (hash, rank) in entries {
            lane.insert(hash, rank);
        }
        for (hash, rank) in entries {
            assert_eq!(lane.get(hash), Some(rank));
        }
        for hash in [768, 1024, (1 << 63) + 256, 128, 384] {
            assert_eq!(lane.get(hash), None);
        }
        assert_eq!(lane.get(0), Some(0)); // primary remains intact
    }

    #[test]
    fn compact_people_routes_aliases_and_unknowns_exactly() {
        let mut lanes: Vec<_> = (0..LANES).map(|_| CompactLane::new(4, 8)).collect();
        for (h, rank) in [(0, 3), (256, 3), (512, 7), (64, 1)] {
            lanes[0].insert(h, rank);
        }
        let people = People {
            codec: Codec::new(),
            lookup: Lookup::Compact(lanes),
            roots: (0..8).map(|rank| mix(rank + 123)).collect(),
            count: 8,
        };
        for (h, rank) in [(0, 3), (256, 3), (512, 7), (64, 1)] {
            match people.lookup(unmix(h), h) {
                Identity::Linked(route) => assert_eq!(route, linked_route(rank)),
                Identity::Standalone(_) => panic!("linked hash {h} not found"),
            }
        }
        for h in [768, 320, 128, 1 << 63] {
            match people.lookup(unmix(h), h) {
                Identity::Standalone(actual) => assert_eq!(actual, h),
                Identity::Linked(_) => panic!("unknown hash {h} was linked"),
            }
        }
        assert_eq!(people.original(3), mix(126));
    }

    #[test]
    fn compact_capacity_reserves_overflow_flag_for_every_rank() {
        for count in [0, 1, 30, 31, 32, 63, 64, 127, 128, 1000, 60_000_000] {
            let (cap, bits) = compact_capacity(count).unwrap();
            assert!(cap.is_power_of_two());
            assert!(cap * LANES >= 2 * (count + 1));
            assert!((count as u64) < (1u64 << (bits - 1)));
        }
        assert!(compact_capacity(usize::MAX).is_none());
    }

    #[test]
    fn compact_extreme_lane_skew_and_exact_misses() {
        let count = 10_000;
        let (cap, bits) = compact_capacity(count).unwrap();
        let mut lane = CompactLane::new(cap, bits);
        // All hashes land on just four natural slots, including zero and a
        // quotient with its high bit set. Every lookup must remain exact.
        for i in 0..count {
            let h = ((i as u64) << bits) | (((i % 4) as u64) << 6);
            lane.insert(h, i as u32);
        }
        lane.insert(1 << 63, 0);
        for i in 0..count {
            let h = ((i as u64) << bits) | (((i % 4) as u64) << 6);
            assert_eq!(lane.get(h), Some(i as u32));
            assert_eq!(lane.get(h | (1 << 62)), None);
        }
        assert_eq!(lane.get(1 << 63), Some(0));
        assert_eq!(lane.get((1 << 63) | (4 << 6)), None);
    }
}
