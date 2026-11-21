//! The low rank byte is already known from routing. Histogram the remaining
//! address, then scatter into person-local ranges instead of sorting rank keys.
use super::*;
use std::mem::MaybeUninit;

const T: usize = 0;
const P: usize = 1;
const C: usize = 2;
type Cursor = [usize; 3];

trait Directory {
    fn event(&mut self, rank: u32) -> &mut Cursor;
    fn existing(&mut self, rank: u32) -> Option<&mut Cursor>;
}

impl Directory for Vec<Cursor> {
    #[inline]
    fn event(&mut self, rank: u32) -> &mut Cursor {
        &mut self[rank as usize]
    }

    #[inline]
    fn existing(&mut self, rank: u32) -> Option<&mut Cursor> {
        self.get_mut(rank as usize)
    }
}

impl Directory for Map<u32, Cursor> {
    #[inline]
    fn event(&mut self, rank: u32) -> &mut Cursor {
        self.entry(rank).or_insert([0; 3])
    }

    #[inline]
    fn existing(&mut self, rank: u32) -> Option<&mut Cursor> {
        self.get_mut(&rank)
    }
}

#[inline]
fn count(dir: &mut impl Directory, occupied: &mut Vec<u32>, key: u64, purchase: bool) {
    let rank = (key >> 32) as u32;
    let cursor = dir.event(rank);
    if cursor[T] == 0 {
        occupied.push(rank);
    }
    cursor[T] += 1;
    cursor[P] += usize::from(purchase);
}

fn buffer<V>(len: usize) -> Vec<MaybeUninit<V>> {
    let mut v = Vec::new();
    v.resize_with(len, MaybeUninit::uninit);
    v
}

/// Caller must have written every position.
unsafe fn initialized<V>(mut v: Vec<MaybeUninit<V>>) -> Vec<V> {
    let (ptr, len, cap) = (v.as_mut_ptr(), v.len(), v.capacity());
    std::mem::forget(v);
    // MaybeUninit<V> has exactly V's size and alignment.
    unsafe { Vec::from_raw_parts(ptr.cast(), len, cap) }
}

pub(super) fn process(
    owner: usize,
    events: Vec<Stream<u64>>,
    clicks: Vec<Vec<RankedClick>>,
    people: &People,
    pack: Packing,
    reduce: &mut Reduce,
) {
    let ne: usize = events.iter().map(|p| p.times.len() + p.purchases.len()).sum();
    if ne == 0 {
        return;
    }
    // A tiny input can touch only a few addresses of a large link table.
    // In that case do not allocate or walk the entire address space.
    if people.local_slots <= ne.saturating_mul(2) {
        scatter(
            vec![[0; 3]; people.local_slots],
            owner,
            events,
            clicks,
            people,
            pack,
            reduce,
        );
    } else {
        scatter(Map::with_hasher(rs()), owner, events, clicks, people, pack, reduce);
    }
}

fn scatter(
    mut dir: impl Directory,
    owner: usize,
    events: Vec<Stream<u64>>,
    clicks: Vec<Vec<RankedClick>>,
    people: &People,
    pack: Packing,
    reduce: &mut Reduce,
) {
    let mut occupied = Vec::new();
    for part in &events {
        for &key in &part.times {
            count(&mut dir, &mut occupied, key, false);
        }
        for p in &part.purchases {
            count(&mut dir, &mut occupied, p.key, true);
        }
    }
    for part in &clicks {
        for c in part {
            if let Some(cursor) = dir.existing(c.rank).filter(|c| c[T] != 0) {
                cursor[C] += 1;
            }
        }
    }
    let mut totals = [0; 3];
    for &rank in &occupied {
        let cursor = dir.event(rank);
        let counts = std::mem::replace(cursor, totals);
        for i in 0..3 {
            totals[i] += counts[i];
        }
    }
    let mut times = buffer(totals[T]);
    let mut purchases = buffer(totals[P]);
    let mut selected_clicks = buffer(totals[C]);
    for part in events {
        for key in part.times {
            let cursor = dir.event((key >> 32) as u32);
            times[cursor[T]].write(key as u32);
            cursor[T] += 1;
        }
        for p in part.purchases {
            let cursor = dir.event((p.key >> 32) as u32);
            let key = p.key as u32;
            times[cursor[T]].write(key);
            cursor[T] += 1;
            purchases[cursor[P]].write(Purchase {
                key,
                price: p.price,
                cat: p.cat,
            });
            cursor[P] += 1;
        }
    }
    for part in clicks {
        for c in part {
            // All occupied ranks now have a nonzero event *end*. A rank
            // with no event was never prefixed and retains its zero count.
            if let Some(cursor) = dir.existing(c.rank).filter(|c| c[T] != 0) {
                selected_clicks[cursor[C]].write(Click {
                    ts: c.ts,
                    cid: c.cid,
                    camp: c.camp,
                });
                cursor[C] += 1;
            }
        }
    }
    // The scatter replays the exact histogram inputs and predicate, writing
    // each reserved interval once. Each cursor is now the interval end.
    let (mut times, mut purchases, mut clicks) =
        unsafe { (initialized(times), initialized(purchases), initialized(selected_clicks)) };
    let mut bins = Vec::new();
    let mut start = [0; 3];
    for rank in occupied {
        let end = *dir.event(rank);
        let ts = &mut times[start[T]..end[T]];
        let ps = &mut purchases[start[P]..end[P]];
        let cs = &mut clicks[start[C]..end[C]];
        ps.sort_unstable_by_key(|p| p.key);
        cs.sort_unstable_by_key(|c| c.ts);
        let person = people.original((rank << NS.trailing_zeros()) | owner as u32);
        if ts.len() >= 4096 {
            reduce.person(person, bin_sessions(ts, &mut bins), ps, cs, pack);
        } else {
            ts.sort_unstable();
            reduce.person(person, ordered_sessions(ts), ps, cs, pack);
        }
        start = end;
    }
}

#[derive(Clone, Copy, Default)]
struct Bin {
    lo: u32,
    hi: u32,
    count: usize,
}

// Every pair inside a bin is closer than GAP. The only possible session
// boundary lies between exact extrema of consecutive nonempty bins.
const BIN_SHIFT: u32 = 20;
const _: () = assert!((1 << BIN_SHIFT) <= GAP);

fn bin_sessions<'a>(times: &[u32], bins: &'a mut Vec<Bin>) -> impl Iterator<Item = (u32, u32, usize)> + 'a {
    let (mut lo, mut hi) = (u32::MAX, 0);
    for &ts in times {
        lo = lo.min(ts);
        hi = hi.max(ts);
    }
    let origin = lo >> BIN_SHIFT;
    bins.clear();
    bins.resize(((hi >> BIN_SHIFT) - origin + 1) as usize, Bin::default());
    for &ts in times {
        let bin = &mut bins[((ts >> BIN_SHIFT) - origin) as usize];
        if bin.count == 0 {
            bin.lo = ts;
            bin.hi = ts;
        } else {
            bin.lo = bin.lo.min(ts);
            bin.hi = bin.hi.max(ts);
        }
        bin.count += 1;
    }
    let mut iter = bins.iter().copied().filter(|bin| bin.count != 0).peekable();
    std::iter::from_fn(move || {
        let mut bin = iter.next()?;
        while iter.peek().is_some_and(|next| next.lo - bin.hi <= GAP as u32) {
            let next = iter.next().unwrap();
            bin.hi = next.hi;
            bin.count += next.count;
        }
        Some((bin.lo, bin.hi, bin.count))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bin_extrema_equal_sorted_sessions_even_across_empty_bins() {
        let mut bins = Vec::new();
        for origin in [0, (1 << BIN_SHIFT) - 1, u32::MAX - 12_000_000] {
            for extra in [0, 1] {
                let g = GAP as u32;
                let mut points = [
                    origin + 2 * g + extra,
                    origin + g,
                    origin,
                    origin + g,
                    origin + 5 * g,
                    origin + 6 * g + extra,
                ];
                let compressed = bin_sessions(&points, &mut bins).collect::<Vec<_>>();
                points.sort_unstable();
                assert_eq!(compressed, ordered_sessions(&points).collect::<Vec<_>>());
            }
        }
    }

    #[test]
    fn bin_extrema_match_sort_on_shuffled_clustered_offsets() {
        let mut bins = Vec::new();
        let mut rng = 7u32;
        for n in [1, 83, 4096, 16_384] {
            let mut points: Vec<_> = (0..n)
                .map(|_| {
                    rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (rng >> 26) * 40_000_000 + (rng & 0x3f_ffff)
                })
                .collect();
            let compressed = bin_sessions(&points, &mut bins).collect::<Vec<_>>();
            points.sort_unstable();
            assert_eq!(compressed, ordered_sessions(&points).collect::<Vec<_>>());
        }
    }
}
