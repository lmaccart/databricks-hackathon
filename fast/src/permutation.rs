//! Sort compact descriptors first, then repair any order that the descriptors
//! could not prove. The payloads never move during the speculative sort.
use super::{NS, SHARD_SHIFT, gather};

const ROUTE_BITS: u32 = NS.trailing_zeros();
const ABOVE_ROUTE: u32 = SHARD_SHIFT + ROUTE_BITS;
const _: () = assert!(NS.is_power_of_two() && ABOVE_ROUTE < 64);

struct Layout {
    lo: i64,
    index_bits: u32,
    index_mask: u64,
    suffix_bits: u32,
    person_shift: u32,
    time_shift: u32,
}

impl Layout {
    fn new(len: usize, lo: i64, hi: i64) -> Option<Self> {
        // Small slices are cheap to sort in place. Leave enough bits for both
        // useful discrimination and time buckets on large slices.
        if len < 128 || hi < lo {
            return None;
        }
        let index_bits = (len - 1).ilog2() + 1;
        let min_person = (index_bits + 4).clamp(20, 24);
        let time_needed = u64::BITS - offset(hi, lo).leading_zeros();
        let time_available = 64u32.checked_sub(index_bits + min_person)?;
        if time_available < time_needed.min(12) {
            return None;
        }
        let time_bits = time_needed.min(time_available);
        let suffix_bits = index_bits + time_bits;
        Some(Self {
            lo,
            index_bits,
            index_mask: (1u64 << index_bits) - 1,
            suffix_bits,
            person_shift: suffix_bits.saturating_sub(ROUTE_BITS),
            time_shift: time_needed - time_bits,
        })
    }

    #[inline]
    fn prefix(&self, person: u64) -> u64 {
        // Remove only the shard selector: it is identical for all rows here.
        // Keeping the high bits of the rest is monotone in the full u64 person.
        let dense = ((person >> ABOVE_ROUTE) << SHARD_SHIFT) | (person & ((1 << SHARD_SHIFT) - 1));
        dense >> self.person_shift
    }

    #[inline]
    fn bucket(&self, ts: i64) -> u64 {
        offset(ts, self.lo) >> self.time_shift
    }
}

#[inline]
fn offset(ts: i64, lo: i64) -> u64 {
    // Subtraction in unsigned space is exact even for MIN..=MAX.
    (ts as u64).wrapping_sub(lo as u64)
}

fn compare_sort<T: Copy>(pieces: Vec<Vec<T>>, key: impl Fn(&T) -> (u64, i64)) -> Vec<T> {
    let mut rows = gather(pieces);
    rows.sort_unstable_by_key(key);
    rows
}

/// All pieces must belong to the same person shard and their accepted
/// timestamps must lie in the inclusive [lo, hi] manifest bounds.
pub(super) fn gather_sorted<T: Copy>(pieces: Vec<Vec<T>>, lo: i64, hi: i64, key: impl Fn(&T) -> (u64, i64)) -> Vec<T> {
    let len = pieces.iter().map(Vec::len).sum();
    let Some(layout) = Layout::new(len, lo, hi) else {
        return compare_sort(pieces, key);
    };
    let mut words = Vec::with_capacity(len);
    let mut rows = Vec::with_capacity(len);
    #[cfg(debug_assertions)]
    let route = super::shard(key(pieces.iter().find_map(|p| p.first()).unwrap()).0);
    for piece in pieces {
        for row in piece {
            let (person, ts) = key(&row);
            debug_assert!(ts >= lo && ts <= hi);
            #[cfg(debug_assertions)]
            debug_assert_eq!(super::shard(person), route);
            words.push(
                (layout.prefix(person) << layout.suffix_bits)
                    | (layout.bucket(ts) << layout.index_bits)
                    | rows.len() as u64,
            );
            rows.push(row);
        }
    }
    words.sort_unstable();

    // Gather and verify together. Distinct prefix runs are already ordered.
    // A full-width timestamp bucket needs no tie repair. With coarse buckets,
    // an exact timestamp inversion can only occur inside a single bucket.
    let mut sorted = Vec::with_capacity(len);
    for run in words.chunk_by(|a, b| (a >> layout.suffix_bits) == (b >> layout.suffix_bits)) {
        let start = sorted.len();
        let person = key(&rows[(run[0] & layout.index_mask) as usize]).0;
        let mut collision = false;
        let mut inverted = false;
        let mut prev_ts = i64::MIN;
        for &word in run {
            let row = rows[(word & layout.index_mask) as usize];
            let (p, ts) = key(&row);
            collision |= p != person;
            inverted |= layout.time_shift != 0 && ts < prev_ts;
            prev_ts = ts;
            sorted.push(row);
        }
        let out = &mut sorted[start..];
        if collision {
            out.sort_unstable_by_key(&key);
        } else if inverted {
            let mut i = 0;
            for bucket in run.chunk_by(|a, b| (a >> layout.index_bits) == (b >> layout.index_bits)) {
                let end = i + bucket.len();
                if !out[i..end].is_sorted_by_key(|r| key(r).1) {
                    out[i..end].sort_unstable_by_key(|r| key(r).1);
                }
                i = end;
            }
        }
    }
    sorted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compare(pieces: Vec<Vec<(u64, i64, usize)>>, lo: i64, hi: i64) {
        let mut want: Vec<_> = pieces.iter().flatten().copied().collect();
        want.sort_unstable();
        let got = gather_sorted(pieces, lo, hi, |r| (r.0, r.1));
        assert_eq!(
            got.iter().map(|r| (r.0, r.1)).collect::<Vec<_>>(),
            want.iter().map(|r| (r.0, r.1)).collect::<Vec<_>>()
        );
        let mut got = got;
        got.sort_unstable();
        assert_eq!(got, want); // preserve each payload alongside its exact key
    }

    #[test]
    fn repairs_shared_prefix_and_reversed_bucket() {
        let layout = Layout::new(256, i64::MIN, i64::MAX).unwrap();
        let p = 0xabc0_7a00_0000_0000;
        let q = p + 1;
        let r = 0xabd0_7a00_0000_0000;
        assert_eq!(layout.prefix(p), layout.prefix(q));
        assert_ne!(layout.prefix(p), layout.prefix(r));
        assert!(layout.time_shift > 0);
        assert_eq!(layout.bucket(-2), layout.bucket(-3));

        // p and q interleave by time; r has no collision but a reversed tie.
        // Duplicates have distinct payloads and must not be lost in repair.
        let rows: Vec<_> = (0..256)
            .map(|i| {
                let (person, ts) = [(q, i64::MIN), (p, i64::MAX), (r, -2), (r, -3), (p, i64::MIN)][i % 5];
                (person, ts, i)
            })
            .collect();
        compare(rows.chunks(19).map(|c| c.to_vec()).collect(), i64::MIN, i64::MAX);
    }

    #[test]
    fn exact_time_and_constant_time_still_repair_person_collisions() {
        let rows: Vec<_> = (0..256)
            .map(|i| (0xfeed_5300_0000_0000 + (255 - i) as u64, -42 - (i % 3) as i64, i))
            .collect();
        let layout = Layout::new(rows.len(), -44, -42).unwrap();
        assert_eq!(layout.time_shift, 0);
        // Force the first two IDs into one descriptor prefix even with exact time.
        assert_eq!(layout.prefix(rows[0].0), layout.prefix(rows[1].0));
        compare(vec![rows.clone()], -44, -42);
        compare(vec![rows.into_iter().map(|(p, _, i)| (p, -42, i)).collect()], -42, -42);
    }

    #[test]
    fn declines_short_shards_and_impossible_layouts() {
        assert!(Layout::new(0, 0, 0).is_none());
        assert!(Layout::new(2, 0, 0).is_none());
        assert!(Layout::new(256, 1, 0).is_none());
        #[cfg(target_pointer_width = "64")]
        assert!(Layout::new(usize::MAX, i64::MIN, i64::MAX).is_none());
        compare(vec![vec![(5, -2, 0)], vec![(4, 3, 1), (4, -3, 2)]], -3, 3);
    }

    #[test]
    fn sorts_mixed_inline_and_spill_tokens_numerically() {
        let ids: Vec<_> = (0..2048)
            .map(|i| {
                if i % 2 == 0 {
                    format!("id{i}")
                } else {
                    format!("long-α-id-{i}")
                }
            })
            .collect();
        let codec = crate::Codec::new();
        let mut shards = vec![Vec::new(); NS];
        for (i, id) in ids.iter().enumerate() {
            let person = crate::links::mix(codec.key(id.as_bytes()));
            shards[crate::shard(person)].push((person, i % 2));
        }
        let people = shards
            .iter()
            .find(|p| p.iter().any(|&(_, k)| k == 0) && p.iter().any(|&(_, k)| k == 1))
            .unwrap();
        for len in [127, 128, 129, 255, 256, 257, 8192] {
            let rows: Vec<_> = (0..len)
                .map(|i| {
                    let person = people[i % people.len()].0;
                    (person, (crate::links::mix(i as u64) as i64) >> (i % 13), i)
                })
                .collect();
            compare(rows.chunks(89).map(|c| c.to_vec()).collect(), i64::MIN, i64::MAX);
        }
    }
}
