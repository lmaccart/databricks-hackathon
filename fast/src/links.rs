//! Build the canonical-name lookup in parallel, including transitive paths
//! that span input files. Node indexes are temporary; only exact keys escape.
use super::{Chunk, Codec, for_rows};
use crate::identity::{self, BuildParts, Family, Match, Shape};
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
    family: Option<Family>,
    pub(super) count: usize,
}

#[derive(Clone, Copy, Default)]
pub(super) enum Probe {
    #[default]
    None,
    Exact(u64, u64),
    Structural(u64, usize),
    Cached(u64),
}

const HOT_LEN: usize = 256;
const RAW_TAG: u64 = 1 << 63;

/// An exact, bounded cache local to one parse task. Cache resolved misses too:
/// a frequently seen UID need not occur in any link.
pub(super) struct Hot([(u64, u64); HOT_LEN]);

impl Hot {
    pub(super) fn new() -> Self {
        Self([(0, 0); HOT_LEN])
    }

    #[inline]
    fn slot(probe: Probe) -> Option<(usize, u64)> {
        let (hash, tag) = match probe {
            // Spill IDs use the high bit, so leave them on the exact path.
            Probe::Exact(key, h) if key & RAW_TAG == 0 => (h as usize, key),
            Probe::Structural(raw, index) => (index, raw | RAW_TAG),
            _ => return None,
        };
        Some((hash & (HOT_LEN - 1), tag))
    }

    #[inline]
    pub(super) fn prepare<'a>(&self, people: &People<'a>, uid: &'a [u8]) -> Probe {
        let probe = people.prepare(uid);
        if let Some((i, tag)) = Self::slot(probe) {
            let (saved, person) = self.0[i];
            if saved == tag {
                return Probe::Cached(person);
            }
        }
        probe
    }

    #[inline]
    pub(super) fn finish(&mut self, people: &People<'_>, probe: Probe) -> u64 {
        let person = people.lookup(probe);
        if let Some((i, tag)) = Self::slot(probe) {
            self.0[i] = (tag, person);
        }
        person
    }
}

impl<'a> People<'a> {
    #[inline]
    pub(super) fn prepare(&self, uid: &'a [u8]) -> Probe {
        if let Some(family) = &self.family {
            if let Some(raw) = family.shape.raw(uid) {
                return Probe::Structural(raw, family.index(raw));
            }
        }
        let key = self.codec.key(uid);
        Probe::Exact(key, mix(key))
    }

    #[inline]
    pub(super) fn lookup(&self, probe: Probe) -> u64 {
        match probe {
            Probe::None => 0,
            Probe::Exact(key, h) => self.lookup_exact(key, h),
            Probe::Cached(person) => person,
            Probe::Structural(raw, index) => {
                let family = self.family.as_ref().unwrap();
                match family.lookup(index, raw) {
                    Match::Canonical(name) => name,
                    Match::Unlinked => mix(family.exact_key(raw)),
                    Match::Overflow => {
                        let key = family.exact_key(raw);
                        self.lookup_exact(key, mix(key))
                    }
                }
            }
        }
    }

    #[inline]
    fn lookup_exact(&self, key: u64, h: u64) -> u64 {
        let lane = &self.lanes[h as usize & (LANES - 1)];
        let s = lane.slots[lane.find(key, h)];
        if s.key != 0 { s.value } else { h }
    }

    #[inline]
    pub(super) fn prefetch(&self, probe: Probe) {
        let addr = match probe {
            Probe::Exact(_, h) => {
                let lane = &self.lanes[h as usize & (LANES - 1)];
                let i = (h >> LANES.trailing_zeros()) as usize & lane.mask;
                (&lane.slots[i] as *const Slot).cast::<u8>()
            }
            Probe::Structural(_, index) => self.family.as_ref().unwrap().address(index),
            Probe::None | Probe::Cached(_) => return,
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
    let shape = if std::env::var_os("SS_IDENTITY_FLAT").is_some_and(|value| value == "1") {
        None
    } else {
        Shape::learn(&groups)
    };
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
    let parts: Vec<BuildParts> = lanes
        .par_iter_mut()
        .enumerate()
        .map(|(s, lane)| {
            let mut parts = BuildParts::default();
            for slot in &mut lane.slots {
                if slot.key != 0 {
                    let name = keys[root(&parent, (offsets[s] + slot.value as usize) as u32) as usize];
                    slot.value = mix(name);
                    if let Some(shape) = shape {
                        // A missing accelerator tag means "itself". Roots
                        // need no slots, including explicit self-links.
                        if name != slot.key {
                            if let Some(raw) = shape.packed_raw(slot.key) {
                                identity::add(&mut parts, raw, slot.value);
                            }
                        }
                    }
                }
            }
            parts
        })
        .collect();
    let family = shape.and_then(|shape| Family::build(shape, parts));
    People {
        codec,
        lanes,
        family,
        count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hot_cache_confirms_full_tag_after_same_slot_and_handles_self() {
        let keys: Vec<_> = (0..=HOT_LEN).map(|n| format!("some{n}")).collect();
        let people = person_names(&[]);
        let mut hot = Hot::new();
        let mut old = None;
        let mut pair = None;
        let mut seen = [None; HOT_LEN];
        for (n, name) in keys.iter().enumerate() {
            let (i, _) = Hot::slot(people.prepare(name.as_bytes())).unwrap();
            if let Some(previous) = seen[i].replace(n) {
                old = Some(keys[previous].as_bytes());
                pair = Some(name.as_bytes());
                break;
            }
        }
        let (old, pair) = (old.unwrap(), pair.unwrap());
        let old_probe = hot.prepare(&people, old);
        let first = hot.finish(&people, old_probe);
        assert!(matches!(hot.prepare(&people, old), Probe::Cached(value) if value == first));
        let pair_probe = hot.prepare(&people, pair);
        assert!(matches!(pair_probe, Probe::Exact(..)));
        let second = hot.finish(&people, pair_probe);
        assert_ne!(first, second);
        assert!(matches!(hot.prepare(&people, old), Probe::Exact(..)));
        assert!(matches!(hot.prepare(&people, pair), Probe::Cached(value) if value == second));
        assert!(matches!(hot.prepare(&people, b"abc\0"), Probe::Exact(..)));
    }
}
