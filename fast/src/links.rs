//! Build the canonical-name lookup in parallel, including transitive paths
//! that span input files. Canonical node indexes also serve as exact sort ranks.
use super::{Chunk, Codec, NS, SHARD_SHIFT, for_rows};
use rayon::prelude::*;
use std::sync::atomic::{
    AtomicU32, AtomicU64,
    Ordering::{AcqRel, Acquire, Relaxed},
};

const SINGLETON_SLOTS: usize = 4;

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

pub(super) struct People<'a> {
    pub(super) codec: Codec<'a>,
    slots: Vec<Slot>,
    mask: usize,
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
        let mut i = h as usize & self.mask;
        let mut holes = SINGLETON_SLOTS;
        loop {
            let slot = &self.slots[i];
            let occupant = slot.key.load(Relaxed);
            let value = slot.value;
            if occupant == key {
                return Identity::Linked(value & !LINKED);
            }
            if value & LINKED == 0 {
                // Every singleton with this home probes the same bounded set
                // of originally empty slots. Keys only go from empty to owned,
                // so a key can never migrate between ranked and wide streams.
                if occupant == 0 {
                    let got = slot.key.compare_exchange(0, key, Relaxed, Relaxed);
                    if got.is_ok() || got == Err(key) {
                        return Identity::Linked(value);
                    }
                }
                holes -= 1;
                if holes == 0 {
                    return Identity::Standalone(h);
                }
            }
            i = (i + 1) & self.mask;
        }
    }

    #[inline]
    pub(super) fn original(&self, rank: u32) -> u64 {
        mix(self.slots[rank as usize].key.load(Relaxed))
    }

    #[inline]
    pub(super) fn prefetch(&self, h: u64) {
        let addr = &self.slots[h as usize & self.mask] as *const Slot;
        // Only an in-bounds address in a fixed-size, live table; prefetch is
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
        let p = node.load(Acquire);
        if p == x {
            return x;
        }
        let gp = parent[p as usize].load(Acquire);
        if p == gp {
            return p;
        }
        let _ = node.compare_exchange_weak(p, gp, AcqRel, Relaxed);
        x = gp;
    }
}

pub(super) fn person_names<'a>(chunks: &[Chunk<'a>]) -> People<'a> {
    let codec = Codec::new();
    let edges: Vec<_> = chunks
        .par_iter()
        .map(|c| {
            let mut edges = Vec::new();
            for_rows(c, |r| {
                if r[0].is_empty() || r[1].is_empty() {
                    return;
                }
                let a = codec.key(r[0]);
                let b = codec.key(r[1]);
                edges.push((a, b));
            });
            edges
        })
        .collect();
    let n: usize = edges.iter().map(Vec::len).sum();
    let cap = (n * 2 + n / 2 + 1).max(256).next_power_of_two();
    assert!(u32::try_from(cap).is_ok(), "alias table exceeds addressable memory");
    let mask = cap - 1;
    let mut slots: Vec<_> = (0..cap).into_par_iter().map(|_| Slot::default()).collect();
    let parent: Vec<_> = (0..cap).into_par_iter().map(|i| AtomicU32::new(i as u32)).collect();
    // The slot itself is an exact rank and union node. Key publication happens
    // before a cross-worker edge can make that node someone else's parent.
    let index = |key| {
        let mut i = mix(key) as usize & mask;
        loop {
            let k = &slots[i].key;
            let got = k.load(Acquire);
            if got == key {
                return i as u32;
            }
            if got == 0 {
                let r = k.compare_exchange(0, key, AcqRel, Acquire);
                if r.is_ok() || r == Err(key) {
                    return i as u32;
                }
            }
            i = (i + 1) & mask;
        }
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
                    let ka = slots[a as usize].key.load(Relaxed);
                    let kb = slots[b as usize].key.load(Relaxed);
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
                    if parent[b as usize].compare_exchange(b, a, AcqRel, Relaxed).is_ok() {
                        break;
                    }
                }
            }
        });
    drop(edges);
    let count = slots
        .par_iter_mut()
        .enumerate()
        .map(|(i, slot)| {
            let linked = *slot.key.get_mut() != 0;
            let rank = u64::from(if linked { root(&parent, i as u32) } else { i as u32 });
            slot.value = (if linked { LINKED } else { 0 }) | ((rank & (NS as u64 - 1)) << SHARD_SHIFT) | rank;
            usize::from(linked)
        })
        .sum();
    People {
        codec,
        slots,
        mask,
        count,
        rank_limit: cap,
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
        let mut groups = std::collections::HashMap::<usize, Vec<(u64, u64)>>::new();
        let mut clash = None;
        for n in 0..16384 {
            let k = crate::person::inline(format!("u{n}").as_bytes()).unwrap();
            let h = mix(k);
            let v = groups.entry(h as usize & names.mask).or_default();
            v.push((k, h));
            if v.len() == SINGLETON_SLOTS + 2 {
                clash = Some(v.clone());
                break;
            }
        }
        let pair = clash.unwrap();
        let observed: Vec<_> = (0..1024usize)
            .into_par_iter()
            .map(|i| {
                let (k, h) = pair[i % pair.len()];
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
        assert_eq!(observed[..pair.len()].iter().filter(|v| v.0).count(), SINGLETON_SLOTS);
        for (i, v) in observed.iter().enumerate() {
            assert_eq!(*v, observed[i % pair.len()]);
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
