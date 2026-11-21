//! Build the canonical-name lookup in parallel, including transitive paths
//! that span input files. Node indexes are temporary; only exact keys escape.
use super::{Chunk, Codec, for_rows};
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

#[derive(Clone, Copy)]
struct Alias {
    key: u64,
    value: u64,
}

#[repr(C, align(64))]
#[derive(Clone, Copy, Default)]
struct Bucket {
    keys: [u64; 4],
    values: [u64; 4],
}

const _: () = assert!(std::mem::size_of::<Bucket>() == 64);

#[inline]
fn prefix(h: u64) -> usize {
    (h >> (64 - LANES.trailing_zeros())) as usize
}

struct Aliases {
    buckets: Vec<Bucket>,
    shift: u32,
    // Only read for overfull buckets. Their fourth slot describes a slice in
    // the builder's range, not another randomly placed probe bucket.
    overflow: Vec<Vec<Alias>>,
    overflow_fold: u32,
}

impl Aliases {
    fn build(mut ranges: Vec<Vec<Alias>>) -> Self {
        let n = ranges.iter().map(Vec::len).sum::<usize>();
        let cap = n.div_ceil(2).next_power_of_two().max(2);
        let shift = 64 - cap.trailing_zeros();
        // Match high-bit prefixes to disjoint slices in the one table. Size
        // from the total alias count, never the most crowded prefix.
        let partitions = cap.min(LANES);
        let range_width = LANES / partitions;
        let buckets_per_partition = cap / partitions;
        let mut buckets: Vec<_> = (0..cap)
            .into_par_iter()
            .with_min_len(1024)
            .map(|_| Bucket::default())
            .collect();
        let overflow = buckets
            .par_chunks_mut(buckets_per_partition)
            .zip(ranges.par_chunks_mut(range_width))
            .map(|(buckets, ranges)| {
                let local_index = |key| (mix(key) >> shift) as usize & (buckets_per_partition - 1);
                let mut spill = Vec::new();
                for entry in ranges.iter_mut().flat_map(|r| std::mem::take(r)) {
                    let b = &mut buckets[local_index(entry.key)];
                    if b.keys[3] == 0 && b.values[3] != 0 {
                        spill.push(entry);
                    } else if let Some(i) = b.keys.iter().position(|&k| k == 0) {
                        b.keys[i] = entry.key;
                        b.values[i] = entry.value;
                    } else {
                        // Keep all four slots in ordinary full buckets. Only
                        // on the fifth key give up one slot for spill metadata.
                        spill.push(Alias {
                            key: b.keys[3],
                            value: b.values[3],
                        });
                        spill.push(entry);
                        b.keys[3] = 0;
                        b.values[3] = 1;
                    }
                }
                spill.sort_unstable_by_key(|e| (local_index(e.key), e.key));
                let mut start = 0;
                while start < spill.len() {
                    let i = local_index(spill[start].key);
                    let mut end = start + 1;
                    while end < spill.len() && local_index(spill[end].key) == i {
                        end += 1;
                    }
                    // The linked ID count already fits in u32.
                    buckets[i].values[3] = ((start as u64) << 32) | (end - start) as u64;
                    start = end;
                }
                spill
            })
            .collect();
        Self {
            buckets,
            shift,
            overflow,
            overflow_fold: range_width.trailing_zeros(),
        }
    }

    #[inline]
    fn bucket(&self, h: u64) -> &Bucket {
        // SAFETY: build allocates exactly 2^(64 - shift) buckets.
        unsafe { self.buckets.get_unchecked((h >> self.shift) as usize) }
    }

    #[inline]
    fn lookup(&self, key: u64, h: u64) -> u64 {
        let b = self.bucket(h);
        if key == b.keys[0] {
            b.values[0]
        } else if key == b.keys[1] {
            b.values[1]
        } else if key == b.keys[2] {
            b.values[2]
        } else if key == b.keys[3] && key != 0 {
            b.values[3]
        } else if b.keys[3] == 0 && b.values[3] != 0 {
            self.lookup_spill(key, h, b.values[3])
        } else {
            h
        }
    }

    #[cold]
    fn lookup_spill(&self, key: u64, h: u64, range: u64) -> u64 {
        let start = (range >> 32) as usize;
        let end = start + range as u32 as usize;
        let entries = &self.overflow[prefix(h) >> self.overflow_fold][start..end];
        match entries.binary_search_by_key(&key, |e| e.key) {
            Ok(i) => entries[i].value,
            Err(_) => h,
        }
    }
}

pub(super) struct People<'a> {
    pub(super) codec: Codec<'a>,
    aliases: Aliases,
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
        self.aliases.lookup(key, h)
    }

    #[inline]
    pub(super) fn prefetch(&self, h: u64) {
        let addr = self.aliases.bucket(h) as *const Bucket;
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
    let (lanes, groups): (Vec<_>, Vec<_>) = built.into_iter().unzip();
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
    drop(edges);
    drop(lanes);
    let parts: Vec<_> = keys
        .par_chunks(8192)
        .enumerate()
        .map(|(part, keys_part)| {
            let mut ranges: Vec<Vec<Alias>> = (0..LANES).map(|_| Vec::new()).collect();
            for (i, &key) in keys_part.iter().enumerate() {
                let node = (part * 8192 + i) as u32;
                let canonical = root(&parent, node);
                if canonical != node {
                    ranges[prefix(mix(key))].push(Alias {
                        key,
                        value: mix(keys[canonical as usize]),
                    });
                }
            }
            ranges
        })
        .collect();
    let ranges = (0..LANES)
        .into_par_iter()
        .map(|s| {
            let mut range = Vec::with_capacity(parts.iter().map(|p| p[s].len()).sum());
            for part in &parts {
                range.extend_from_slice(&part[s]);
            }
            range
        })
        .collect();
    drop(parts);
    let aliases = Aliases::build(ranges);
    People { codec, aliases, count }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csv::chunks_of;
    use std::path::Path;

    fn links(file: &[u8]) -> Vec<Chunk<'_>> {
        chunks_of(file, &["user_id_a", "user_id_b"], Path::new("links.csv"))
    }

    fn table(entries: &[Alias]) -> Aliases {
        let mut ranges: Vec<_> = (0..LANES).map(|_| Vec::new()).collect();
        for &entry in entries {
            ranges[prefix(mix(entry.key))].push(entry);
        }
        Aliases::build(ranges)
    }

    #[test]
    fn all_aliases_in_one_bucket_remain_exact() {
        let entries: Vec<_> = (1..=4096)
            .map(|h| Alias {
                key: unmix(h),
                value: mix(h + 4096),
            })
            .collect();
        let full = table(&entries[..4]);
        assert!(full.overflow.iter().all(Vec::is_empty));
        let miss = entries[5].key;
        assert_eq!(full.lookup(miss, mix(miss)), mix(miss));

        let skewed = table(&entries);
        assert_eq!(skewed.buckets.len(), 2048);
        assert_eq!(skewed.overflow.iter().map(Vec::len).sum::<usize>(), entries.len() - 3);
        for e in &entries {
            assert_eq!(skewed.lookup(e.key, mix(e.key)), e.value);
        }
        for h in [4097, 9001, u64::MAX] {
            assert_eq!(skewed.lookup(unmix(h), h), h);
        }
    }

    #[test]
    fn omitted_canonicals_and_cross_file_bridges_use_original_byte_order() {
        let mut chunks = links(b"user_id_a,user_id_b\nz,unused-bridge\nz,z\nsolo,solo\n");
        chunks.extend(links(
            "user_id_a,user_id_b\nunused-bridge,\" A-longer-than-nine\"\nÉ,unused-bridge\nB,c\n".as_bytes(),
        ));
        let people = person_names(&chunks);
        assert_eq!(people.count, 7);
        assert_eq!(
            people
                .aliases
                .buckets
                .iter()
                .flat_map(|b| b.keys)
                .filter(|&k| k != 0)
                .count()
                + people.aliases.overflow.iter().map(Vec::len).sum::<usize>(),
            4
        );
        let canonical = mix(people.codec.key(b" A-longer-than-nine"));
        for uid in [
            b"z".as_slice(),
            b"unused-bridge",
            "É".as_bytes(),
            b" A-longer-than-nine",
        ] {
            assert_eq!(people.resolve(uid), canonical);
        }
        assert_eq!(people.resolve(b"c"), people.resolve(b"B"));
        for uid in [b"solo".as_slice(), b"unknown-long-id", b"a\0"] {
            assert_eq!(people.resolve(uid), mix(people.codec.key(uid)));
        }
        assert_eq!(person_names(&[]).resolve(b"a"), mix(Codec::new().key(b"a")));
    }

    #[test]
    fn parallel_build_preserves_a_large_two_file_chain() {
        use std::fmt::Write;

        let mut odd = String::from("user_id_a,user_id_b\n");
        let mut even = odd.clone();
        const N: usize = 20_000;
        for i in (1..=N).rev() {
            let part = if i % 2 == 0 { &mut even } else { &mut odd };
            writeln!(part, "u{i:05},u{:05}", i - 1).unwrap();
        }
        let mut chunks = links(odd.as_bytes());
        chunks.extend(links(even.as_bytes()));
        let people = person_names(&chunks);
        assert_eq!(people.count, N + 1);
        let canonical = mix(people.codec.key(b"u00000"));
        for i in 0..=N {
            let name = format!("u{i:05}");
            let key = crate::person::inline(name.as_bytes()).unwrap();
            assert_eq!(people.lookup(key, mix(key)), canonical);
        }
    }
}
