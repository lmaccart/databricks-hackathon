//! Build the canonical-name lookup in parallel, including transitive paths
//! that span input files. Canonical node indexes also serve as exact sort ranks.
use super::{Chunk, Codec, NS, SHARD_SHIFT, for_rows, shard};
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
    lanes: Vec<Lane>,
    roots: Vec<u64>,
    pub(super) count: usize,
    chooser: Option<Vec<u8>>,
}

// This bit is disjoint from the packed routing shard and the canonical node
// index. It is internal to the link table, not reserved in arbitrary person keys.
const LINKED: u64 = 1 << 63;
const _: () = assert!(SHARD_SHIFT >= 32 && ((NS as u64 - 1) << SHARD_SHIFT) & LINKED == 0);

#[derive(Clone, Copy)]
pub(super) enum Identity {
    Linked(u64), // shard bits at their normal position, root node index in low bits
    Standalone(u64),
}

impl<'a> People<'a> {
    #[inline]
    pub(super) fn lookup(&self, key: u64, h: u64) -> Identity {
        let lane = &self.lanes[self.route(h)];
        let s = lane.slots[lane.find(key, h)];
        if s.value & LINKED != 0 {
            Identity::Linked(s.value ^ LINKED)
        } else {
            Identity::Standalone(h)
        }
    }

    #[inline]
    pub(super) fn route(&self, hash: u64) -> usize {
        match &self.chooser {
            Some(table) => table[hash as usize & (table.len() - 1)] as usize,
            None => shard(hash),
        }
    }

    pub(super) fn coarse(&self) -> bool {
        self.chooser.is_some()
    }

    #[inline]
    pub(super) fn original(&self, rank: u32) -> u64 {
        self.roots[rank as usize]
    }
}

fn local_root(parent: &mut [u32], mut x: u32) -> u32 {
    while parent[x as usize] != x {
        let next = parent[x as usize];
        parent[x as usize] = parent[next as usize];
        x = next;
    }
    x
}

// Each canonical owner is a node. Shared slots union owners, while each
// owner's aliases already point at that same node. Never infer identity from
// this graph: it is only a conservative routing constraint.
fn chooser(lanes: &[Lane], roots: &[u64], count: usize) -> Option<Vec<u8>> {
    let size = count.saturating_mul(32).clamp(1 << 16, 1 << 26).next_power_of_two();
    let mask = size - 1;
    let mut occupied = Vec::with_capacity(count);
    for lane in lanes {
        for s in &lane.slots {
            if s.key != 0 {
                occupied.push(((mix(s.key) as usize & mask) as u32, s.value as u32));
            }
        }
    }
    occupied.sort_unstable_by_key(|x| x.0);
    let mut parent: Vec<u32> = (0..count as u32).collect();
    let mut weights = vec![1u32; count];
    for group in occupied.chunk_by(|a, b| a.0 == b.0) {
        let first = group[0].1;
        for &(_, rank) in &group[1..] {
            let mut a = local_root(&mut parent, first);
            let mut b = local_root(&mut parent, rank);
            if a != b {
                if weights[a as usize] < weights[b as usize] {
                    std::mem::swap(&mut a, &mut b);
                }
                parent[b as usize] = a;
                weights[a as usize] += weights[b as usize];
            }
        }
    }
    weights.fill(0);
    for &(_, rank) in &occupied {
        let r = local_root(&mut parent, rank);
        weights[r as usize] += 1;
    }
    // Alias counts are an observable lower-cost skew proxy; refuse a
    // percolated routing graph rather than serializing a large shard.
    let limit = (count / 16).max(4096);
    let largest = weights.iter().copied().max().unwrap_or(0) as usize;
    if largest > limit {
        eprintln!("coarse routing fallback: cluster {largest} aliases, limit {limit}");
        return None;
    }
    let mut table: Vec<u8> = (0..size).map(|i| shard(mix(i as u64)) as u8).collect();
    let mut loads = [0usize; NS];
    for group in occupied.chunk_by(|a, b| a.0 == b.0) {
        let rank = local_root(&mut parent, group[0].1);
        let dest = shard(mix(roots[rank as usize]));
        table[group[0].0 as usize] = dest as u8;
        loads[dest] += group.len();
    }
    let max_load = loads.into_iter().max().unwrap_or(0);
    if max_load > limit {
        eprintln!("coarse routing fallback: shard {max_load} aliases, limit {limit}");
        return None;
    }
    eprintln!("coarse routing: {size} slots, {count} aliases, largest cluster {largest}, largest shard {max_load}");
    Some(table)
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
    lanes.par_iter_mut().enumerate().for_each(|(s, lane)| {
        for slot in &mut lane.slots {
            if slot.key != 0 {
                let rank = root(&parent, (offsets[s] + slot.value as usize) as u32);
                let routing = roots[rank as usize] & ((NS as u64 - 1) << SHARD_SHIFT);
                slot.value = LINKED | routing | u64::from(rank);
            }
        }
    });
    let chooser = chooser(&lanes, &roots, count);
    if let Some(table) = &chooser {
        let mut sizes = [0usize; NS];
        for lane in &lanes {
            for slot in &lane.slots {
                if slot.key != 0 {
                    sizes[table[mix(slot.key) as usize & (table.len() - 1)] as usize] += 1;
                }
            }
        }
        let mut local: Vec<_> = sizes
            .into_iter()
            .map(|n| {
                let cap = (n * 2 + 1).next_power_of_two();
                Lane {
                    slots: vec![Slot::default(); cap],
                    mask: cap - 1,
                }
            })
            .collect();
        for lane in lanes {
            for slot in lane.slots {
                if slot.key != 0 {
                    let hash = mix(slot.key);
                    let dest = &mut local[table[hash as usize & (table.len() - 1)] as usize];
                    let i = dest.find(slot.key, hash);
                    dest.slots[i] = slot;
                }
            }
        }
        lanes = local;
    }
    People {
        codec,
        lanes,
        roots,
        count,
        chooser,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lane(entries: &[(u64, u32)]) -> Lane {
        let cap = (entries.len() * 2 + 1).next_power_of_two();
        let mut lane = Lane {
            slots: vec![Slot::default(); cap],
            mask: cap - 1,
        };
        for &(key, rank) in entries {
            let i = lane.find(key, mix(key));
            lane.slots[i] = Slot {
                key,
                value: LINKED | u64::from(rank),
            };
        }
        lane
    }

    #[test]
    fn occupied_slot_collision_glues_routes_not_names() {
        let mut seen = vec![0u64; 1 << 16];
        let mut pair = None;
        for key in 1..1_000_000 {
            let i = mix(key) as usize & 65535;
            if seen[i] != 0 {
                pair = Some((seen[i], key));
                break;
            }
            seen[i] = key;
        }
        let (a, b) = pair.unwrap();
        let alias = 1_000_001;
        let lanes = vec![lane(&[(a, 0), (alias, 0), (b, 1)])];
        let table = chooser(&lanes, &[mix(a), mix(b), mix(alias)], 3).unwrap();
        assert_eq!(table[mix(a) as usize & 65535], table[mix(alias) as usize & 65535]);
        assert_eq!(table[mix(a) as usize & 65535], table[mix(b) as usize & 65535]);
        assert_ne!(
            lanes[0].slots[lanes[0].find(a, mix(a))].value as u32,
            lanes[0].slots[lanes[0].find(b, mix(b))].value as u32
        );
    }

    #[test]
    fn pathological_component_falls_back() {
        let entries: Vec<_> = (1..=4097).map(|key| (key, 0)).collect();
        let lanes = vec![lane(&entries)];
        assert!(chooser(&lanes, &vec![0; entries.len()], entries.len()).is_none());
    }
}
