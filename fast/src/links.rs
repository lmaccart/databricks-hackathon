//! Build the canonical-name lookup in parallel, including transitive paths
//! that span input files. Node indexes are temporary; only exact keys escape.
use super::{Chunk, Codec, for_rows};
use rayon::prelude::*;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

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
    pub(super) count: usize,
}

impl<'a> People<'a> {
    #[inline]
    pub(super) fn resolve(&self, uid: &'a [u8]) -> u64 {
        let key = self.codec.key(uid);
        let h = mix(key);
        self.lookup(key, h)
    }

    #[inline]
    pub(super) fn lookup(&self, key: u64, h: u64) -> u64 {
        let lane = &self.lanes[h as usize & (LANES - 1)];
        let s = lane.slots[lane.find(key, h)];
        if s.key != 0 { s.value } else { h }
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

pub(super) fn person_names<'a>(chunks: &[Chunk<'a>]) -> People<'a> {
    let codec = Codec::new();
    let parsed: Vec<_> = chunks
        .par_iter()
        .map(|c| {
            let mut endpoints = 0usize;
            let mut keys: Vec<Vec<(u64, usize)>> = (0..LANES).map(|_| Vec::new()).collect();
            for_rows(c, |r| {
                if r[0].is_empty() || r[1].is_empty() {
                    return;
                }
                let a = codec.key(r[0]);
                let b = codec.key(r[1]);
                for k in [a, b] {
                    keys[mix(k) as usize & (LANES - 1)].push((k, endpoints));
                    endpoints += 1;
                }
            });
            let edges: Vec<_> = (0..endpoints).map(|_| AtomicU64::new(0)).collect();
            (edges, keys)
        })
        .collect();
    let (edges, key_parts): (Vec<_>, Vec<_>) = parsed.into_iter().unzip();
    let built: Vec<_> = (0..LANES)
        .into_par_iter()
        .map(|s| {
            let n = key_parts.iter().map(|p| p[s].len()).sum::<usize>();
            // Avoid reserving from incidence count alone: repeated edges and
            // whales can have arbitrarily many occurrences of the same ID.
            let cap = (n.saturating_mul(2).saturating_add(1)).min(4096).next_power_of_two();
            let mut lane = Lane {
                slots: vec![Slot::default(); cap],
                mask: cap - 1,
            };
            let mut keys = Vec::new();
            for (part, occurrences) in key_parts.iter().enumerate() {
                for &(k, pos) in &occurrences[s] {
                    let hash = mix(k);
                    let mut i = lane.find(k, hash);
                    if lane.slots[i].key == 0 {
                        // Keep exactly the same final load factor as the
                        // immutable production table, even after duplicates.
                        if (keys.len() + 1) * 2 >= lane.slots.len() {
                            let mut larger = Lane {
                                slots: vec![Slot::default(); lane.slots.len() * 2],
                                mask: lane.slots.len() * 2 - 1,
                            };
                            for slot in &lane.slots {
                                if slot.key != 0 {
                                    let j = larger.find(slot.key, mix(slot.key));
                                    larger.slots[j] = *slot;
                                }
                            }
                            lane = larger;
                            i = lane.find(k, hash);
                        }
                        let id = u32::try_from(keys.len()).expect("too many linked IDs for addressable memory");
                        keys.push(k);
                        lane.slots[i] = Slot {
                            key: k,
                            value: id as u64,
                        };
                    }
                    let id = lane.slots[i].value;
                    edges[part][pos].store(((s as u64) << 32) | id, Relaxed);
                }
            }
            let final_cap = (keys.len() * 2 + 1).next_power_of_two();
            if lane.slots.len() != final_cap {
                let mut exact = Lane {
                    slots: vec![Slot::default(); final_cap],
                    mask: final_cap - 1,
                };
                for slot in &lane.slots {
                    if slot.key != 0 {
                        let i = exact.find(slot.key, mix(slot.key));
                        exact.slots[i] = *slot;
                    }
                }
                lane = exact;
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
    let index = |endpoint: &AtomicU64| {
        let id = endpoint.load(Relaxed);
        (offsets[(id >> 32) as usize] + id as u32 as usize) as u32
    };
    edges
        .par_iter()
        .flat_map(|part| part.par_chunks(16384))
        .for_each(|part| {
            for pair in part.chunks_exact(2) {
                let (mut a, mut b) = (index(&pair[0]), index(&pair[1]));
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
    lanes.par_iter_mut().enumerate().for_each(|(s, lane)| {
        for slot in &mut lane.slots {
            if slot.key != 0 {
                slot.value = mix(keys[root(&parent, (offsets[s] + slot.value as usize) as u32) as usize]);
            }
        }
    });
    People { codec, lanes, count }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{csv::chunks_of, person::inline};
    use std::path::Path;

    #[test]
    fn crowded_lane_grows_and_repeated_bridges_keep_exact_minimum() {
        let mut names = Vec::new();
        for i in 0.. {
            let name = format!("u{i:07}");
            if mix(inline(name.as_bytes()).unwrap()) as usize & (LANES - 1) == 0 {
                names.push(name);
                if names.len() == 3_000 {
                    break;
                }
            }
        }
        let mut data = String::from("user_id_a,user_id_b\n");
        for pair in names.windows(2) {
            data.push_str(&format!("{},{}\n", pair[0], pair[1]));
        }
        for _ in 0..5_000 {
            data.push_str(&format!("{},{}\n", names[0], names[1]));
        }
        data.push_str(&format!("  unused-é,{}\n", names[2999]));
        let chunks = chunks_of(data.as_bytes(), &["user_id_a", "user_id_b"], Path::new("links"));
        let people = person_names(&chunks);
        let minimum = people.resolve("  unused-é".as_bytes());
        for name in &names {
            assert_eq!(people.resolve(name.as_bytes()), minimum);
        }
        assert_eq!(people.count, names.len() + 1);
        let lane_count = people.lanes[0].slots.iter().filter(|s| s.key != 0).count();
        assert!(lane_count >= names.len());
        assert_eq!(people.lanes[0].slots.len(), (lane_count * 2 + 1).next_power_of_two());
    }

    #[test]
    fn whale_duplicates_do_not_inflate_immutable_table() {
        let mut data = String::from("user_id_a,user_id_b\n");
        for _ in 0..10_000 {
            data.push_str("whale,whale\n");
        }
        let chunks = chunks_of(data.as_bytes(), &["user_id_a", "user_id_b"], Path::new("links"));
        let people = person_names(&chunks);
        let lane = mix(inline(b"whale").unwrap()) as usize & (LANES - 1);
        assert_eq!(people.count, 1);
        assert_eq!(people.lanes[lane].slots.len(), 4);
        assert_eq!(people.resolve(b"whale"), mix(inline(b"whale").unwrap()));
    }
}
