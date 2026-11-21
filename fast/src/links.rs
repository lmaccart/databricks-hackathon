//! Build the canonical-name lookup in parallel, including transitive paths
//! that span input files. Canonical node indexes also serve as exact sort ranks.
use super::{Chunk, Codec, NS, SHARD_SHIFT, for_rows};
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

#[derive(Default)]
struct Slot {
    key: AtomicU64,
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
            let k = self.slots[i].key.load(Relaxed);
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
    pub(super) rank_limit: usize,
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
        let mut i = (h >> LANES.trailing_zeros()) as usize & lane.mask;
        loop {
            let slot = &lane.slots[i];
            let occupant = slot.key.load(Relaxed);
            let value = slot.value;
            if occupant == key {
                return Identity::Linked(value & !LINKED);
            }
            if value & LINKED == 0 {
                // Originally empty: no linked key could be past this hole on
                // its probe sequence. Let one exact singleton own its unused
                // rank forever. A collision keeps the full-width stream.
                if occupant == 0 && value != NO_RANK {
                    let got = slot.key.compare_exchange(0, key, Relaxed, Relaxed);
                    if got.is_ok() || got == Err(key) {
                        return Identity::Linked(value);
                    }
                }
                return Identity::Standalone(h);
            }
            i = (i + 1) & lane.mask;
        }
    }

    #[inline]
    pub(super) fn original(&self, rank: u32) -> u64 {
        let r = rank as usize;
        if r < self.count {
            return self.roots[r];
        }
        let pos = r - self.count;
        mix(self.lanes[pos % LANES].slots[pos / LANES].key.load(Relaxed))
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

const NO_RANK: u64 = !LINKED;

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
                slots: std::iter::repeat_with(Slot::default).take(cap).collect(),
                mask: cap - 1,
            };
            for (j, &k) in keys.iter().enumerate() {
                let i = lane.find(k, mix(k));
                lane.slots[i] = Slot {
                    key: AtomicU64::new(k),
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
    let span = lanes
        .iter()
        .map(|l| l.slots.len())
        .max()
        .unwrap_or(0)
        .checked_mul(LANES)
        .and_then(|n| n.checked_add(count))
        .filter(|&n| n <= u32::MAX as usize);
    lanes.par_iter_mut().enumerate().for_each(|(s, lane)| {
        for (i, slot) in lane.slots.iter_mut().enumerate() {
            if *slot.key.get_mut() != 0 {
                let rank = root(&parent, (offsets[s] + slot.value as usize) as u32);
                let routing = roots[rank as usize] & ((NS as u64 - 1) << SHARD_SHIFT);
                slot.value = LINKED | routing | u64::from(rank);
            } else if span.is_some() {
                let rank = (count + i * LANES + s) as u64;
                slot.value = (mix(rank) & ((NS as u64 - 1) << SHARD_SHIFT)) | rank;
            } else {
                slot.value = NO_RANK;
            }
        }
    });
    People {
        codec,
        lanes,
        roots,
        count,
        rank_limit: span.unwrap_or(count),
    }
}

#[cfg(test)]
mod slot_tests {
    use super::*;
    use crate::csv::chunks_of;
    use std::path::Path;

    #[test]
    fn concurrent_singletons_share_holes_without_masking_aliases() {
        let input = b"user_id_a,user_id_b\nZalias,Aroot\nAroot,Balias\n";
        let chunks = chunks_of(input, &["user_id_a", "user_id_b"], Path::new("links.csv"));
        let names = person_names(&chunks);
        let mut groups = std::collections::HashMap::<(usize, usize), Vec<(u64, u64)>>::new();
        let mut clash = None;
        for n in 0..1024 {
            let k = crate::person::inline(format!("u{n}").as_bytes()).unwrap();
            let h = mix(k);
            let s = h as usize & (LANES - 1);
            let v = groups.entry((s, names.lanes[s].find(k, h))).or_default();
            v.push((k, h));
            if v.len() == 2 {
                clash = Some([v[0], v[1]]);
                break;
            }
        }
        let pair = clash.unwrap();
        let observed: Vec<_> = (0..1024usize)
            .into_par_iter()
            .map(|i| {
                let (k, h) = pair[i % 2];
                match names.lookup(k, h) {
                    Identity::Linked(v) => {
                        assert_eq!(names.original(v as u32), h);
                        (true, v)
                    }
                    Identity::Standalone(v) => {
                        assert_eq!(v, h);
                        (false, v)
                    }
                }
            })
            .collect();
        assert_ne!(observed[0].0, observed[1].0);
        for (i, v) in observed.iter().enumerate() {
            assert_eq!(*v, observed[i % 2]);
        }
        let root = mix(crate::person::inline(b"Aroot").unwrap());
        for bytes in [b"Aroot".as_slice(), b"Zalias", b"Balias"] {
            let k = crate::person::inline(bytes).unwrap();
            let Identity::Linked(v) = names.lookup(k, mix(k)) else {
                panic!("alias displaced");
            };
            assert_eq!(names.original(v as u32), root);
        }
    }
}
