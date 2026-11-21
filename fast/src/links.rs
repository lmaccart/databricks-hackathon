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

struct Lane {
    keys: Vec<u64>,
    ranks: Vec<u32>,
    retrieval: Option<Retrieval>,
}

struct Retrieval {
    seed: u64,
    part: usize,
    values: Vec<u32>,
}

// Three disjoint vertex ranges ensure that an edge cannot repeat a vertex.
#[inline]
fn vertices(hash: u64, seed: u64, part: usize) -> [usize; 3] {
    let a = mix(hash ^ seed);
    let b = mix(a ^ 0x9e37_79b9_7f4a_7c15);
    let c = mix(b ^ 0xd1b5_4a32_d192_ed03);
    let reduce = |x: u64| ((u128::from(x) * part as u128) >> 64) as usize;
    [reduce(a), part + reduce(b), 2 * part + reduce(c)]
}

impl Retrieval {
    fn attempt(keys: &[u64], seed: u64, part: usize) -> Option<Self> {
        let size = part.checked_mul(3)?;
        if size > u32::MAX as usize || keys.len() > u32::MAX as usize {
            return None;
        }
        let mut degrees = vec![0u32; size];
        let mut edges = vec![0u32; size];
        for (i, &key) in keys.iter().enumerate() {
            for v in vertices(mix(key), seed, part) {
                degrees[v] += 1;
                edges[v] ^= i as u32;
            }
        }
        let mut queue: Vec<u32> = degrees
            .iter()
            .enumerate()
            .filter_map(|(i, &d)| (d == 1).then_some(i as u32))
            .collect();
        let mut stack = Vec::with_capacity(keys.len());
        while let Some(v) = queue.pop() {
            let v = v as usize;
            if degrees[v] != 1 {
                continue;
            }
            let edge = edges[v];
            stack.push((edge, v as u32));
            for u in vertices(mix(keys[edge as usize]), seed, part) {
                degrees[u] -= 1;
                edges[u] ^= edge;
                if degrees[u] == 1 {
                    queue.push(u as u32);
                }
            }
        }
        if stack.len() != keys.len() {
            return None;
        }
        let mut values = vec![0u32; size];
        for &(edge, v) in stack.iter().rev() {
            let [a, b, c] = vertices(mix(keys[edge as usize]), seed, part);
            values[v as usize] = edge ^ values[a] ^ values[b] ^ values[c];
        }
        Some(Self { seed, part, values })
    }

    #[inline]
    fn index(&self, hash: u64) -> usize {
        let [a, b, c] = vertices(hash, self.seed, self.part);
        (self.values[a] ^ self.values[b] ^ self.values[c]) as usize
    }
}

impl Lane {
    fn build(keys: Vec<u64>, attempts: u64) -> Self {
        // Fixed bounded retries: even adversarial cycles fall back to exact
        // binary search without growing the table or retrying indefinitely.
        let part = keys
            .len()
            .checked_mul(135)
            .and_then(|n| n.checked_add(299))
            .map(|n| (n / 300).max(1));
        Self::build_with_part(keys, attempts, part)
    }

    fn build_with_part(keys: Vec<u64>, attempts: u64, part: Option<usize>) -> Self {
        let retrieval = part.and_then(|part| (0..attempts).find_map(|seed| Retrieval::attempt(&keys, seed, part)));
        Self {
            ranks: vec![0; keys.len()],
            keys,
            retrieval,
        }
    }

    #[inline]
    fn find(&self, key: u64, hash: u64) -> Option<usize> {
        if let Some(retrieval) = &self.retrieval {
            let i = retrieval.index(hash);
            (self.keys.get(i) == Some(&key)).then_some(i)
        } else {
            self.keys.binary_search(&key).ok()
        }
    }
}

pub(super) struct People<'a> {
    pub(super) codec: Codec<'a>,
    lanes: Vec<Lane>,
    roots: Vec<u64>,
    pub(super) count: usize,
}

const _: () = assert!(SHARD_SHIFT >= 32);

#[derive(Clone, Copy)]
pub(super) enum Identity {
    Linked(u64), // shard bits at their normal position, root node index in low bits
    Standalone(u64),
}

impl<'a> People<'a> {
    #[inline]
    pub(super) fn lookup(&self, key: u64, h: u64) -> Identity {
        let lane = &self.lanes[h as usize & (LANES - 1)];
        if let Some(i) = lane.find(key, h) {
            let rank = lane.ranks[i];
            let routing = self.roots[rank as usize] & ((NS as u64 - 1) << SHARD_SHIFT);
            Identity::Linked(routing | u64::from(rank))
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
        let Some(retrieval) = &lane.retrieval else { return };
        let i = vertices(h, retrieval.seed, retrieval.part)[0];
        let addr = &retrieval.values[i] as *const u32;
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
            Lane::build(keys, 4)
        })
        .collect();
    drop(key_parts);
    let mut lanes = built;
    let mut offsets = [0; LANES];
    let mut count = 0usize;
    for (s, g) in lanes.iter().enumerate() {
        offsets[s] = count;
        count += g.keys.len();
    }
    assert!(
        u32::try_from(count).is_ok(),
        "too many linked IDs for addressable memory"
    );
    let keys: Vec<u64> = lanes.iter().flat_map(|lane| lane.keys.iter().copied()).collect();
    let parent: Vec<_> = (0..count as u32).map(AtomicU32::new).collect();
    let index = |key| {
        let hash = mix(key);
        let s = hash as usize & (LANES - 1);
        (offsets[s] + lanes[s].find(key, hash).expect("link endpoint indexed")) as u32
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
        for (i, rank) in lane.ranks.iter_mut().enumerate() {
            *rank = root(&parent, (offsets[s] + i) as u32);
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
    fn retrieval_exact_membership_and_fallback() {
        for n in [0, 1, 2, 31, 10_000] {
            let keys: Vec<_> = (1..=n).map(|i| i * 2).collect();
            for attempts in [0, 4] {
                let lane = Lane::build(keys.clone(), attempts);
                if attempts == 0 {
                    assert!(lane.retrieval.is_none());
                }
                if n == 10_000 && attempts > 0 {
                    assert!(lane.retrieval.is_some());
                }
                for i in 0..=(n * 2 + 100) {
                    assert_eq!(lane.find(i, mix(i)), keys.binary_search(&i).ok());
                }
            }
        }
    }

    #[test]
    fn forced_cycle_cannot_peel_and_uses_sorted_fallback() {
        let keys: Vec<_> = (1..=10_000).collect();
        // One vertex per partition forces every edge into the same cycle.
        assert!(Retrieval::attempt(&keys, 0, 1).is_none());
        let lane = Lane::build_with_part(keys.clone(), 4, Some(1));
        assert!(lane.retrieval.is_none());
        for &key in &keys {
            assert_eq!(lane.find(key, mix(key)), keys.binary_search(&key).ok());
        }
        assert_eq!(lane.find(10_001, mix(10_001)), None);
    }
}
