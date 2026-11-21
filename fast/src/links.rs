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

enum Membership {
    Empty,
    Bypass,
    Blocked(Box<[u64]>),
}

impl Membership {
    fn build(hashes: &[u64]) -> Self {
        // Aim for four IDs per word. Bound memory even if a huge or skewed
        // graph saturates blocks: saturation can only cause extra probes.
        let size = hashes.len().div_ceil(4).min(1 << 20).next_power_of_two();
        let mut words = vec![0; size];
        for &h in hashes {
            words[Self::block(h, size - 1)] |= Self::bits(h);
        }
        Self::Blocked(words.into_boxed_slice())
    }

    #[inline]
    fn block(h: u64, mask: usize) -> usize {
        (h >> 16) as usize & mask
    }

    #[inline]
    fn bits(h: u64) -> u64 {
        // Both probes share a word, and the bit selectors are disjoint from
        // the block selector (bits 16..35, even at the allocation cap).
        1u64.rotate_left((h >> 10) as u32) | (1u64 << (h >> 58))
    }

    #[inline]
    fn maybe_linked(&self, h: u64) -> bool {
        match self {
            Self::Empty => false,
            Self::Bypass => true,
            Self::Blocked(words) => {
                let bits = Self::bits(h);
                words[Self::block(h, words.len() - 1)] & bits == bits
            }
        }
    }
}

pub(super) struct People<'a> {
    pub(super) codec: Codec<'a>,
    lanes: Vec<Lane>,
    roots: Vec<u64>,
    membership: Membership,
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
    pub(super) fn configure_filter(&mut self, events: &[Chunk<'a>], clicks: &[Chunk<'a>]) {
        if self.count == 0 {
            return;
        }
        // When almost every row is linked, keep the original exact lookup
        // path. Sampling can only choose the strategy, not classify any row.
        let sample = |chunks: &[Chunk<'a>]| {
            let mut rows = Vec::new();
            let n = chunks.len().min(16);
            for i in 0..n {
                let mut c = chunks[i * chunks.len() / n];
                let len = c.data.len();
                if len > 4096 {
                    let start = (len - 4096) / 2;
                    let region = &c.data[start..start + 4096];
                    let (Some(first), Some(last)) = (memchr::memchr(b'\n', region), memchr::memrchr(b'\n', region))
                    else {
                        continue;
                    };
                    if first == last {
                        continue;
                    }
                    c.data = &region[first + 1..last + 1];
                }
                for_rows(&c, |r| {
                    if !r[0].is_empty() && !r[1].is_empty() {
                        let key = self.codec.key(r[1]);
                        let h = mix(key);
                        rows.push((h, matches!(self.lookup(key, h), Identity::Linked(_))));
                    }
                });
            }
            rows
        };
        let samples = [sample(events), sample(clicks)];
        let absent = |rows: &[(u64, bool)]| rows.iter().filter(|&&(_, linked)| !linked).count();
        let total: usize = samples.iter().map(Vec::len).sum();
        let misses: usize = samples.iter().map(|rows| absent(rows)).sum();
        if total == 0 || misses * 16 > total {
            // roots actually holds the mixed original name of *every* node,
            // not just component representatives (aliases need bits too).
            self.membership = Membership::build(&self.roots);
        }
        if std::env::var_os("SS_ID_FILTER_SAMPLE_LOG").is_some() {
            let bytes = match &self.membership {
                Membership::Blocked(words) => words.len() * 8,
                _ => 0,
            };
            for (source, rows) in ["events", "clicks"].into_iter().zip(&samples) {
                let probes = rows.iter().filter(|&&(h, _)| self.membership.maybe_linked(h)).count();
                eprintln!(
                    "id filter: {source}: {} sampled IDs, {} standalone, {probes} table probes, {bytes} filter bytes",
                    rows.len(),
                    absent(rows)
                );
            }
        }
    }

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
    pub(super) fn prefetch_if_linked(&self, h: u64) -> bool {
        if !self.membership.maybe_linked(h) {
            return false;
        }
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
        true
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
    lanes.par_iter_mut().enumerate().for_each(|(s, lane)| {
        for slot in &mut lane.slots {
            if slot.key != 0 {
                let rank = root(&parent, (offsets[s] + slot.value as usize) as u32);
                let routing = roots[rank as usize] & ((NS as u64 - 1) << SHARD_SHIFT);
                slot.value = LINKED | routing | u64::from(rank);
            }
        }
    });
    People {
        codec,
        lanes,
        roots,
        membership: if count == 0 {
            Membership::Empty
        } else {
            Membership::Bypass
        },
        count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunks_of;
    use std::path::Path;

    #[test]
    fn filter_preserves_exact_graph_membership_and_names() {
        let chunks = |data, cols: &[&str]| chunks_of(data, cols, Path::new("ids.csv"));
        let links = b"user_id_a,user_id_b\nu01,z\nz,\" a\"\nu1,u1\nu\x001,u1\nu1\x00,\xc3\xa9\n\xc3\xa9,unused canonical\n,ghost\n";
        let mut names = person_names(&chunks(links, &["user_id_a", "user_id_b"]));
        names.configure_filter(&[], &[]);
        for (raw, canonical) in [
            (&b"u01"[..], &b" a"[..]),
            (b"z", b" a"),
            (b" a", b" a"),
            (b"u1", b"u\x001"),
            (b"u1\x00", b"u1\x00"),
            (b"\xc3\xa9", b"u1\x00"),
            (b"unused canonical", b"u1\x00"),
        ] {
            let key = names.codec.key(raw);
            let hash = mix(key);
            assert!(names.prefetch_if_linked(hash));
            let Identity::Linked(rank) = names.lookup(key, hash) else {
                panic!("linked key was not found");
            };
            assert_eq!(
                names.codec.bytes(unmix(names.original(rank as u32)), &mut [0; 9]),
                canonical
            );
        }
        let mut false_positives = 0;
        let mut negatives = 0;
        let outsiders: Vec<_> = (0..2048).map(|i| format!("external {i:04}\0")).collect();
        for raw in std::iter::once(&b"ghost"[..]).chain(outsiders.iter().map(|s| s.as_bytes())) {
            let key = names.codec.key(raw);
            let hash = mix(key);
            if names.prefetch_if_linked(hash) {
                false_positives += 1;
                assert!(matches!(names.lookup(key, hash), Identity::Standalone(h) if h == hash));
            } else {
                negatives += 1;
            }
        }
        assert!(false_positives > 0 && negatives > 0);
    }

    #[test]
    fn empty_and_all_linked_samples_take_exact_fallbacks() {
        let mut empty = person_names(&[]);
        empty.configure_filter(&[], &[]);
        assert!(!empty.prefetch_if_linked(mix(123)));

        let path = Path::new("ids.csv");
        let links = chunks_of(b"user_id_a,user_id_b\na,b\n", &["user_id_a", "user_id_b"], path);
        let inputs = chunks_of(b"event_id,user_id\n1,a\n2,b\n", &["event_id", "user_id"], path);
        let mut names = person_names(&links);
        names.configure_filter(&inputs, &[]);
        assert!(matches!(names.membership, Membership::Bypass));
        let outsider = names.codec.key(b"standalone");
        assert!(names.prefetch_if_linked(mix(outsider)));
        assert!(matches!(names.lookup(outsider, mix(outsider)), Identity::Standalone(_)));
    }
}
