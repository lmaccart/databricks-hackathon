//! Build the canonical-name lookup in parallel, including transitive paths
//! that span input files. Canonical node indexes also serve as exact sort ranks.
use super::{Chunk, Codec, NS, for_rows, shard};
use rayon::prelude::*;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

// Projection lanes are indexed by the raw bits, independently of final owner.
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

struct Lane {
    slots: Vec<Slot>,
    mask: usize,
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
}

pub(super) struct People<'a> {
    pub(super) codec: Codec<'a>,
    projection: Projection,
    owners: Vec<Exact<u32>>,
    roots: Vec<u64>,
    pub(super) count: usize,
}

pub(super) const STANDALONE: u32 = u32::MAX;
const ESCAPE: u8 = u8::MAX;
const _: () = assert!(NS == 256);

// These keys are already exact, mixed, nonzero tokens, not hash fingerprints.
struct Exact<V> {
    keys: Vec<u64>,
    values: Vec<V>,
    mask: usize,
}

impl<V: Copy + Default> Exact<V> {
    fn new(n: usize) -> Self {
        let cap = (n * 2 + 1).next_power_of_two();
        Self {
            keys: vec![0; cap],
            values: vec![V::default(); cap],
            mask: cap - 1,
        }
    }

    #[inline]
    fn find(&self, key: u64) -> usize {
        // Overflow aliases already agree on the projection's low bits.
        let mut i = (key ^ (key >> 32)) as usize & self.mask;
        while self.keys[i] != 0 && self.keys[i] != key {
            i = (i + 1) & self.mask;
        }
        i
    }

    fn insert(&mut self, key: u64, value: V) {
        let i = self.find(key);
        self.keys[i] = key;
        self.values[i] = value;
    }

    #[inline]
    fn get(&self, key: u64) -> Option<V> {
        let i = self.find(key);
        (self.keys[i] == key).then(|| self.values[i])
    }
}

enum Overflow {
    None,
    Sparse(Exact<u8>),
    // Bound space and probes when many aliases of one raw lane share a slot.
    Dense(Vec<(u64, u8)>),
}

impl Overflow {
    #[cold]
    fn owner(&self, hash: u64, default_owner: u8) -> u8 {
        match self {
            Self::None => default_owner,
            Self::Sparse(m) => m.get(hash).unwrap_or(default_owner),
            Self::Dense(entries) => match entries.binary_search_by_key(&hash, |e| e.0) {
                Ok(i) => entries[i].1,
                Err(_) => default_owner,
            },
        }
    }
}

struct Projection {
    routes: Vec<u8>,
    conflicts: Vec<Vec<u64>>,
    shift: u32,
    mask: usize,
    overflow: Vec<Overflow>,
}

struct AliasPart {
    bounds: [usize; NS + 1],
    aliases: Vec<(u64, u32)>,
}

impl AliasPart {
    fn for_owner(&self, owner: usize) -> &[(u64, u32)] {
        &self.aliases[self.bounds[owner]..self.bounds[owner + 1]]
    }

    fn routed(&self) -> impl Iterator<Item = (u64, u8)> + '_ {
        (0..NS).flat_map(|owner| self.for_owner(owner).iter().map(move |&(hash, _)| (hash, owner as u8)))
    }
}

impl Projection {
    fn build(parts: &[AliasPart], count: usize) -> Self {
        // Use disjoint raw-lane ranges in a single shared read-only table.
        // Each gets the same power-of-two capacity, averaging 2–4 slots per alias.
        let cap = (count.div_ceil(LANES) * 2).next_power_of_two().max(16);
        let mut routes = vec![0; cap * LANES];
        let (conflicts, overflow) = routes
            .par_chunks_mut(cap)
            .zip(parts)
            .enumerate()
            .map(|(raw, (routes, part))| {
                let mask = routes.len() - 1;
                let mut occupied = vec![0u64; routes.len().div_ceil(64)];
                let mut conflicts = vec![0u64; occupied.len()];
                routes.fill(raw as u8);
                for (hash, owner) in part.routed() {
                    let i = hash as usize & mask;
                    let bit = 1u64 << (i % 64);
                    if occupied[i / 64] & bit == 0 {
                        occupied[i / 64] |= bit;
                        routes[i] = owner;
                    } else if routes[i] != owner {
                        conflicts[i / 64] |= bit;
                        routes[i] = ESCAPE;
                    }
                }
                // A literal owner 255 shares the byte marker. When the
                // conflict bit is clear, neither build nor lookup needs its key.
                let exceptions = || {
                    part.routed().filter(|&(hash, _)| {
                        let i = hash as usize & mask;
                        conflicts[i / 64] & (1u64 << (i % 64)) != 0
                    })
                };
                let overflow = if conflicts.iter().all(|&w| w == 0) {
                    Overflow::None
                } else {
                    let e = exceptions().count();
                    if e > part.aliases.len() / 2 {
                        let mut entries: Vec<_> = exceptions().collect();
                        entries.sort_unstable_by_key(|e| e.0);
                        Overflow::Dense(entries)
                    } else {
                        let mut exact = Exact::new(e);
                        for (hash, owner) in exceptions() {
                            exact.insert(hash, owner);
                        }
                        Overflow::Sparse(exact)
                    }
                };
                (conflicts, overflow)
            })
            .unzip();
        Self {
            routes,
            conflicts,
            shift: cap.trailing_zeros(),
            mask: cap - 1,
            overflow,
        }
    }

    #[inline]
    fn owner(&self, hash: u64) -> usize {
        let raw = shard(hash);
        let slot = hash as usize & self.mask;
        let owner = self.routes[(raw << self.shift) | slot];
        if owner == ESCAPE && (self.conflicts[raw][slot / 64] >> (slot % 64)) & 1 != 0 {
            self.overflow[raw].owner(hash, raw as u8) as usize
        } else {
            owner as usize
        }
    }
}

impl<'a> People<'a> {
    #[inline]
    pub(super) fn owner(&self, hash: u64) -> usize {
        if self.count == 0 {
            shard(hash)
        } else {
            self.projection.owner(hash)
        }
    }

    #[inline]
    pub(super) fn rank(&self, owner: usize, hash: u64) -> u32 {
        self.owners[owner].get(hash).unwrap_or(STANDALONE)
    }

    #[inline]
    pub(super) fn original(&self, rank: u32) -> u64 {
        self.roots[rank as usize]
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
    let mut roots = keys;
    roots.par_iter_mut().for_each(|k| *k = mix(*k));
    let parts: Vec<_> = lanes
        .par_iter_mut()
        .enumerate()
        .map(|(s, lane)| {
            // Counting scatter within each source lane. An owner then reads one
            // slice from each lane, without a square grid of allocated buffers.
            let mut ends = [0usize; NS + 1];
            for slot in lane.slots.iter_mut().filter(|s| s.key != 0) {
                let rank = root(&parent, (offsets[s] + slot.value as usize) as u32);
                slot.value = u64::from(rank);
                ends[shard(roots[rank as usize]) + 1] += 1;
            }
            for i in 1..=NS {
                ends[i] += ends[i - 1];
            }
            let mut next = ends;
            let mut aliases = vec![(0, 0); ends[NS]];
            for slot in lane.slots.iter().filter(|s| s.key != 0) {
                let owner = shard(roots[slot.value as usize]);
                aliases[next[owner]] = (mix(slot.key), slot.value as u32);
                next[owner] += 1;
            }
            AliasPart { bounds: ends, aliases }
        })
        .collect();
    drop(lanes);
    drop(parent);
    drop(edges);
    let (projection, owners) = rayon::join(
        || Projection::build(&parts, count),
        || {
            (0..NS)
                .into_par_iter()
                .map(|owner| {
                    let slices = || parts.iter().map(|part| part.for_owner(owner));
                    let mut exact = Exact::new(slices().map(<[_]>::len).sum());
                    for &(hash, rank) in slices().flatten() {
                        exact.insert(hash, rank);
                    }
                    exact
                })
                .collect()
        },
    );
    People {
        codec,
        projection,
        owners,
        roots,
        count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SHARD_SHIFT;

    #[test]
    fn projection_distinguishes_sparse_escape_from_literal_owner_255() {
        let raw = 3;
        let token = |suffix: u64| ((raw as u64) << SHARD_SHIFT) | suffix;
        let mut parts: Vec<_> = (0..NS)
            .map(|_| AliasPart {
                bounds: [0; NS + 1],
                aliases: Vec::new(),
            })
            .collect();
        let part = &mut parts[raw];
        for (owner, low) in [(1, 1), (2, 2), (2, 3), (2, 4), (255, 17), (255, 5)] {
            part.bounds[owner + 1] += 1;
            part.aliases.push((token(low), 0));
        }
        for i in 1..=NS {
            part.bounds[i] += part.bounds[i - 1];
        }
        let projection = Projection::build(&parts, 6);
        assert!(matches!(projection.overflow[raw], Overflow::Sparse(_)));
        assert_eq!(projection.owner(token(1)), 1);
        assert_eq!(projection.owner(token(17)), 255);
        assert_eq!(projection.owner(token(33)), raw);
        assert_eq!(projection.owner(token(5)), 255);
        assert_eq!(projection.owner(token(21)), 255);
        assert_eq!(projection.owner(token(6)), raw);
    }
}
