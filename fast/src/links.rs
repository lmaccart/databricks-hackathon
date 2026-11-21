//! Build the canonical-name lookup in parallel, including transitive paths
//! that span input files. Node indexes are temporary; only exact keys escape.
use super::{Chunk, Codec, for_rows};
use rayon::prelude::*;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

const LANES: usize = 64;
const LANE_SHIFT: u32 = 64 - LANES.trailing_zeros();
const RAGGED: u32 = 64;

#[inline]
fn lane_of(hash: u64) -> usize {
    (hash >> LANE_SHIFT) as usize
}

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

#[repr(C)]
struct Slot {
    key: u64,
    value: u64,
}

#[derive(Clone, Copy)]
struct Range {
    start: usize,
    mask: usize,
}

struct AliasTable {
    slots: Box<[Slot]>,
    // Uniform layout uses the high bits for both the lane and its home slot.
    // Ragged layouts use no hot-path lane metadata for the uniform case.
    shift: u32,
    ragged: Option<Box<[Range; LANES]>>,
}

impl AliasTable {
    fn build(groups: &[Vec<u64>; LANES]) -> Self {
        let caps = groups.each_ref().map(|keys| match keys.len() {
            0 => 0,
            n => (2 * n + 1).next_power_of_two(),
        });
        let ragged_len = caps.iter().sum::<usize>();
        let uniform_cap = caps.iter().copied().max().unwrap();
        // Bound uniform padding to 50% over exact, per-lane sizing. Empty
        // ragged lanes take no slots at all.
        let uniform = ragged_len != 0 && uniform_cap <= ragged_len.saturating_add(ragged_len / 2) / LANES;
        let mut start = 0;
        let ranges = caps.map(|cap| {
            let cap = if uniform { uniform_cap } else { cap };
            let range = Range {
                start,
                mask: cap.saturating_sub(1),
            };
            start += cap;
            range
        });
        let shift = if uniform {
            LANE_SHIFT - uniform_cap.trailing_zeros()
        } else {
            RAGGED
        };
        // SAFETY: both Slot fields are u64s; zero initializes the empty sentinel.
        let mut slots = unsafe { Box::<[Slot]>::new_zeroed_slice(start).assume_init() };
        let mut rest = slots.as_mut();
        let mut offset = 0;
        let jobs: Vec<_> = groups
            .iter()
            .zip(ranges)
            .map(|(keys, range)| {
                let len = if keys.is_empty() && !uniform { 0 } else { range.mask + 1 };
                let (lane, tail) = std::mem::take(&mut rest).split_at_mut(len);
                rest = tail;
                let base = offset;
                offset += keys.len();
                (keys, range, lane, base)
            })
            .collect();
        let home_shift = if uniform { shift } else { 0 };
        jobs.into_par_iter().for_each(|(keys, range, lane, base)| {
            for (j, &key) in keys.iter().enumerate() {
                let mut i = (mix(key) >> home_shift) as usize & range.mask;
                while lane[i].key != 0 {
                    i = (i + 1) & range.mask;
                }
                lane[i] = Slot {
                    key,
                    value: (base + j) as u64,
                };
            }
        });
        Self {
            slots,
            shift,
            ragged: (!uniform && ragged_len != 0).then(|| Box::new(ranges)),
        }
    }

    #[inline]
    fn lookup(&self, key: u64, h: u64) -> u64 {
        let shift = self.shift;
        if shift == RAGGED {
            return self.lookup_ragged(key, h);
        }
        let i = (h >> shift) as usize;
        // SAFETY: a uniform table contains exactly 2^(64 - shift) slots.
        let slot = unsafe { self.slots.get_unchecked(i) };
        if slot.key == 0 {
            h
        } else if slot.key == key {
            slot.value
        } else {
            let mask = (1usize << (LANE_SHIFT - shift)) - 1;
            self.probe(key, h, i, i & !mask, mask)
        }
    }

    #[cold]
    fn lookup_ragged(&self, key: u64, h: u64) -> u64 {
        let Some(range) = self.ragged_range(h) else {
            return h;
        };
        let i = range.start + (h as usize & range.mask);
        let slot = &self.slots[i];
        if slot.key == 0 {
            h
        } else if slot.key == key {
            slot.value
        } else {
            self.probe(key, h, i, range.start, range.mask)
        }
    }

    #[cold]
    fn probe(&self, key: u64, h: u64, home: usize, start: usize, mask: usize) -> u64 {
        let lane = &self.slots[start..start + mask + 1];
        let mut i = (home - start + 1) & mask;
        loop {
            let s = &lane[i];
            if s.key == 0 {
                return h;
            }
            if s.key == key {
                return s.value;
            }
            i = (i + 1) & mask;
        }
    }

    fn ragged_range(&self, h: u64) -> Option<Range> {
        let range = self.ragged.as_ref()?[lane_of(h)];
        // A nonempty lane always has capacity >= 4.
        (range.mask != 0).then_some(range)
    }

    #[inline]
    fn prefetch(&self, h: u64) {
        let shift = self.shift;
        if shift == RAGGED {
            self.prefetch_ragged(h);
        } else {
            prefetch_slot(self.slots.as_ptr().wrapping_add((h >> shift) as usize));
        }
    }

    #[cold]
    fn prefetch_ragged(&self, h: u64) {
        if let Some(range) = self.ragged_range(h) {
            prefetch_slot(
                self.slots
                    .as_ptr()
                    .wrapping_add(range.start + (h as usize & range.mask)),
            );
        }
    }
}

#[inline]
fn prefetch_slot(addr: *const Slot) {
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

pub(super) struct People<'a> {
    pub(super) codec: Codec<'a>,
    table: AliasTable,
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
        self.table.lookup(key, h)
    }

    #[inline]
    pub(super) fn prefetch(&self, h: u64) {
        self.table.prefetch(h);
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
                    keys[lane_of(mix(k))].push(k);
                }
            });
            (edges, keys)
        })
        .collect();
    let (edges, key_parts): (Vec<_>, Vec<_>) = parsed.into_iter().unzip();
    let groups: Vec<_> = (0..LANES)
        .into_par_iter()
        .map(|s| {
            let n = key_parts.iter().map(|p| p[s].len()).sum::<usize>();
            let mut keys = Vec::with_capacity(n);
            for p in &key_parts {
                keys.extend_from_slice(&p[s]);
            }
            keys.sort_unstable();
            keys.dedup();
            keys
        })
        .collect();
    drop(key_parts);
    let groups: [Vec<u64>; LANES] = groups.try_into().unwrap();
    let count: usize = groups.iter().map(Vec::len).sum();
    assert!(
        u32::try_from(count).is_ok(),
        "too many linked IDs for addressable memory"
    );
    // During union, the final alias slots hold dense indexes into the
    // temporary node arrays. Replace those indexes with person tokens in place.
    let mut table = AliasTable::build(&groups);
    let keys: Vec<u64> = groups.into_iter().flatten().collect();
    let parent: Vec<_> = (0..count as u32).map(AtomicU32::new).collect();
    let index = |key| table.lookup(key, mix(key)) as u32;
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
                    if codec.less(kb, ka) {
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
    table.slots.par_iter_mut().for_each(|slot| {
        if slot.key != 0 {
            slot.value = mix(keys[root(&parent, slot.value as u32) as usize]);
        }
    });
    People { codec, table, count }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn uniform_collisions_wrap_inside_each_lane() {
        // Eight distinct keys per lane, all landing in its last slot.
        let groups: [_; LANES] = std::array::from_fn(|s| {
            let home = (((s + 1) * 32 - 1) as u64) << 53;
            (1..=8).map(|n| unmix(home | n)).collect::<Vec<_>>()
        });
        let table = AliasTable::build(&groups);
        assert_eq!(table.shift, 53);
        assert_eq!(table.slots.len(), LANES * 32);
        for (s, keys) in groups.iter().enumerate() {
            assert_eq!(table.slots[s * 32 + 31].key, keys[0]);
            assert_eq!(table.slots[s * 32].key, keys[1]);
            for (j, &key) in keys.iter().enumerate() {
                let h = mix(key);
                table.prefetch(h);
                assert_eq!(table.lookup(key, h), (s * 8 + j) as u64);
            }
            let miss = mix(keys[0]) ^ 1024;
            table.prefetch(miss);
            assert_eq!(table.lookup(unmix(miss), miss), miss);
        }
    }

    #[test]
    fn ragged_skew_with_collisions_and_empty_lanes_stays_bounded() {
        let mut groups: [Vec<u64>; LANES] = std::array::from_fn(|_| Vec::new());
        // Make the busiest lane cross its physical end on insertion.
        let home = (37u64 << 58) | 2047;
        groups[37] = (1..=600).map(|n| unmix(home | (n << 11))).collect();
        groups[38] = vec![unmix(38u64 << 58)];
        let table = AliasTable::build(&groups);
        assert_eq!(table.shift, RAGGED);
        assert_eq!(table.slots.len(), 2048 + 4);
        assert_eq!(table.slots[0].key, groups[37][1]);
        for (j, &key) in groups.iter().flatten().enumerate() {
            let h = mix(key);
            table.prefetch(h);
            assert_eq!(table.lookup(key, h), j as u64);
        }
        for miss in [home | (1024 << 11), 1, u64::MAX] {
            table.prefetch(miss);
            assert_eq!(table.lookup(unmix(miss), miss), miss);
        }
    }

    #[test]
    fn linked_names_keep_exact_spills_and_unlinked_identity() {
        let empty = person_names(&[]);
        assert_eq!(empty.count, 0);
        assert!(empty.table.slots.is_empty());
        empty.prefetch(0);
        empty.prefetch(u64::MAX);
        assert_eq!(empty.resolve(b"unlinked-name"), mix(empty.codec.key(b"unlinked-name")));

        let cols = ["user_id_a", "user_id_b"];
        let path = Path::new("links.csv");
        let mut chunks = crate::chunks_of(
            "user_id_a,user_id_b\néclair,middle\n\"middle\",a longer name\nz,z\nz,\n".as_bytes(),
            &cols,
            path,
        );
        chunks.extend(crate::chunks_of(
            b"user_id_a,user_id_b\nmiddle,\0\n\0,middle\n\"bad,ignored\n",
            &cols,
            path,
        ));
        let people = person_names(&chunks);
        assert_eq!(people.count, 5);
        let canonical = people.resolve(b"\0");
        assert_eq!(people.codec.bytes(unmix(canonical), &mut [0; 9]), b"\0");
        for name in ["éclair".as_bytes(), b"middle", b"a longer name"] {
            assert_eq!(people.resolve(name), canonical);
        }
        for name in [b"z".as_slice(), b"Middle", b"middle\0", "Éclair".as_bytes()] {
            assert_eq!(people.resolve(name), mix(people.codec.key(name)));
        }
    }
}
