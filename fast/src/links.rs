//! Build the canonical-name lookup in parallel, including transitive paths
//! that span input files. Canonical node indexes also serve as exact sort ranks.
use super::{Chunk, Codec, NS, SHARD_SHIFT, for_rows};
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

pub(super) struct People<'a> {
    pub(super) codec: Codec<'a>,
    lanes: Vec<Lane>,
    roots: Vec<u64>,
    pub(super) count: usize,
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
        let lane = &self.lanes[h as usize & (LANES - 1)];
        let s = lane.slots[lane.find(key, h)];
        if s.value & LINKED != 0 {
            Identity::Linked(s.value ^ LINKED)
        } else {
            Identity::Standalone(h)
        }
    }

    #[inline]
    pub(super) fn original(&self, rank: u32) -> u64 {
        self.roots[rank as usize]
    }

    #[inline]
    pub(super) fn prefetch(&self, h: u64) {
        let lane = &self.lanes[h as usize & (LANES - 1)];
        let i = (h >> LANES.trailing_zeros()) as usize & lane.mask;
        let addr = &lane.slots[i] as *const Slot;
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

fn local_root(parent: &mut [u32], mut x: u32) -> u32 {
    while parent[x as usize] != x {
        parent[x as usize] = parent[parent[x as usize] as usize];
        x = parent[x as usize];
    }
    x
}

fn local_id(key: u64, table: &mut Lane, keys: &mut Vec<u64>, parent: &mut Vec<u32>) -> u32 {
    let mut slot = table.find(key, mix(key));
    if table.slots[slot].key != 0 {
        return table.slots[slot].value as u32;
    }
    if keys.len() * 2 >= table.slots.len() {
        let cap = table.slots.len() * 2;
        let mut larger = Lane {
            slots: vec![Slot::default(); cap],
            mask: cap - 1,
        };
        for &old in &table.slots {
            if old.key != 0 {
                let i = larger.find(old.key, mix(old.key));
                larger.slots[i] = old;
            }
        }
        *table = larger;
        slot = table.find(key, mix(key));
    }
    let id = u32::try_from(keys.len()).expect("too many linked IDs");
    table.slots[slot] = Slot { key, value: id as u64 };
    keys.push(key);
    parent.push(id);
    id
}

pub(super) fn person_names<'a>(chunks: &[Chunk<'a>]) -> People<'a> {
    let codec = Codec::new();
    let parsed: Vec<_> = chunks
        .par_iter()
        .map(|c| {
            let cap = (c.data.len() / 8 + 1).next_power_of_two().max(2);
            let mut table = Lane {
                slots: vec![Slot::default(); cap],
                mask: cap - 1,
            };
            let mut keys = Vec::new();
            let mut parent = Vec::new();
            let mut sizes = Vec::<u32>::new();
            for_rows(c, |r| {
                if r[0].is_empty() || r[1].is_empty() {
                    return;
                }
                let a = local_id(codec.key(r[0]), &mut table, &mut keys, &mut parent);
                let b = local_id(codec.key(r[1]), &mut table, &mut keys, &mut parent);
                sizes.resize(keys.len(), 1);
                let mut a = local_root(&mut parent, a);
                let mut b = local_root(&mut parent, b);
                if a != b {
                    if sizes[a as usize] < sizes[b as usize] {
                        std::mem::swap(&mut a, &mut b);
                    }
                    parent[b as usize] = a;
                    sizes[a as usize] += sizes[b as usize];
                }
            });
            let mut parts: Vec<Vec<u32>> = (0..LANES).map(|_| Vec::new()).collect();
            for (i, &key) in keys.iter().enumerate() {
                parent[i] = local_root(&mut parent, i as u32);
                parts[mix(key) as usize & (LANES - 1)].push(i as u32);
            }
            (keys, parent, parts)
        })
        .collect();
    let mut offsets = Vec::with_capacity(parsed.len());
    let mut count = 0usize;
    for (keys, _, _) in &parsed {
        offsets.push(count);
        count += keys.len();
    }
    assert!(
        u32::try_from(count).is_ok(),
        "too many linked IDs for addressable memory"
    );
    let built: Vec<_> = (0..LANES)
        .into_par_iter()
        .map(|s| {
            let n = parsed.iter().map(|p| p.2[s].len()).sum::<usize>();
            let cap = (n * 2 + 1).next_power_of_two();
            let mut lane = Lane {
                slots: vec![Slot::default(); cap],
                mask: cap - 1,
            };
            let mut bridges = Vec::new();
            let mut unique = 0usize;
            for (part, (keys, _, parts)) in parsed.iter().enumerate() {
                for &id in &parts[s] {
                    let k = keys[id as usize];
                    let node = (offsets[part] + id as usize) as u32;
                    let slot = lane.find(k, mix(k));
                    if lane.slots[slot].key == 0 {
                        unique += 1;
                        lane.slots[slot] = Slot {
                            key: k,
                            value: u64::from(node),
                        };
                    } else {
                        bridges.push((lane.slots[slot].value as u32, node));
                    }
                }
            }
            // Repeated IDs across many chunks must not make the immutable
            // lookup table needlessly larger than the distinct graph.
            let compact = (unique * 2 + 1).next_power_of_two();
            if compact < cap {
                let mut smaller = Lane {
                    slots: vec![Slot::default(); compact],
                    mask: compact - 1,
                };
                for &entry in &lane.slots {
                    if entry.key != 0 {
                        let i = smaller.find(entry.key, mix(entry.key));
                        smaller.slots[i] = entry;
                    }
                }
                lane = smaller;
            }
            (lane, bridges)
        })
        .collect();
    let (mut lanes, bridges): (Vec<_>, Vec<_>) = built.into_iter().unzip();
    let parent: Vec<_> = parsed
        .iter()
        .enumerate()
        .flat_map(|(s, (_, p, _))| {
            let offset = offsets[s];
            p.iter().map(move |&id| AtomicU32::new((offset + id as usize) as u32))
        })
        .collect();
    let keys: Vec<_> = parsed.into_iter().flat_map(|(keys, _, _)| keys).collect();
    bridges
        .par_iter()
        .flat_map(|part| part.par_chunks(8192))
        .for_each(|part| {
            for &(mut a, mut b) in part {
                loop {
                    a = root(&parent, a);
                    b = root(&parent, b);
                    if a == b {
                        break;
                    }
                    if a > b {
                        std::mem::swap(&mut a, &mut b);
                    }
                    // Fixed root priority prevents cycles even if a concurrent
                    // union changes the target before this CAS succeeds.
                    if parent[b as usize].compare_exchange(b, a, Relaxed, Relaxed).is_ok() {
                        break;
                    }
                }
            }
        });
    // The structural root need not be the smallest spelling. Reduce over all
    // nodes, including aliases that occur in no event or click.
    let minima: Vec<_> = (0..count as u32).map(AtomicU32::new).collect();
    keys.par_iter().enumerate().for_each(|(i, &key)| {
        let r = root(&parent, i as u32);
        let min = &minima[r as usize];
        let mut old = min.load(Relaxed);
        while codec.less(key, keys[old as usize]) {
            match min.compare_exchange_weak(old, i as u32, Relaxed, Relaxed) {
                Ok(_) => break,
                Err(actual) => old = actual,
            }
        }
    });
    let mut roots = keys;
    roots.par_iter_mut().for_each(|k| *k = mix(*k));
    lanes.par_iter_mut().for_each(|lane| {
        for slot in &mut lane.slots {
            if slot.key != 0 {
                let component = root(&parent, slot.value as u32);
                let rank = minima[component as usize].load(Relaxed);
                let routing = roots[rank as usize] & ((NS as u64 - 1) << SHARD_SHIFT);
                slot.value = LINKED | routing | u64::from(rank);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_table_keeps_colliding_keys_through_growth() {
        let mut table = Lane {
            slots: vec![Slot::default(); 2],
            mask: 1,
        };
        let mut keys = Vec::new();
        let mut parent = Vec::new();
        let colliding: Vec<_> = (1u64..)
            .map(|k| k | (1u64 << 63))
            .filter(|&k| (mix(k) >> LANES.trailing_zeros()) & 255 == 0)
            .take(64)
            .chain(
                (1u64..)
                    .filter(|&k| (mix(k) >> LANES.trailing_zeros()) & 255 == 0)
                    .take(64),
            )
            .collect();
        for (i, &key) in colliding.iter().enumerate() {
            assert_eq!(local_id(key, &mut table, &mut keys, &mut parent), i as u32);
        }
        for (i, &key) in colliding.iter().rev().enumerate() {
            assert_eq!(local_id(key, &mut table, &mut keys, &mut parent), (127 - i) as u32);
        }
        assert_eq!(keys, colliding);
    }
}
