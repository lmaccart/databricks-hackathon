//! The low rank byte is already known from routing. Histogram the remaining
//! address, then scatter into person-local ranges instead of sorting rank keys.
use super::*;
use std::mem::MaybeUninit;

const T: usize = 0;
const P: usize = 1;
const C: usize = 2;
trait Word: Copy + PartialEq + std::ops::AddAssign {
    const ZERO: Self;
    const ONE: Self;
    fn from_usize(value: usize) -> Self;
    fn as_usize(self) -> usize;
}

impl Word for u32 {
    const ZERO: Self = 0;
    const ONE: Self = 1;
    #[inline]
    fn from_usize(value: usize) -> Self {
        u32::try_from(value).expect("compact directory endpoint exceeds u32")
    }
    #[inline]
    fn as_usize(self) -> usize {
        self as usize
    }
}

impl Word for usize {
    const ZERO: Self = 0;
    const ONE: Self = 1;
    #[inline]
    fn from_usize(value: usize) -> Self {
        value
    }
    #[inline]
    fn as_usize(self) -> usize {
        self
    }
}

type Cursor<W> = [W; 3];

trait Directory<W: Word> {
    fn event(&mut self, rank: u32) -> &mut Cursor<W>;
    fn existing(&mut self, rank: u32) -> Option<&mut Cursor<W>>;
}

impl<W: Word> Directory<W> for Vec<Cursor<W>> {
    #[inline]
    fn event(&mut self, rank: u32) -> &mut Cursor<W> {
        &mut self[rank as usize]
    }

    #[inline]
    fn existing(&mut self, rank: u32) -> Option<&mut Cursor<W>> {
        self.get_mut(rank as usize)
    }
}

impl<W: Word> Directory<W> for Map<u32, Cursor<W>> {
    #[inline]
    fn event(&mut self, rank: u32) -> &mut Cursor<W> {
        self.entry(rank).or_insert([W::ZERO; 3])
    }

    #[inline]
    fn existing(&mut self, rank: u32) -> Option<&mut Cursor<W>> {
        self.get_mut(&rank)
    }
}

#[inline]
fn count<W: Word>(dir: &mut impl Directory<W>, occupied: &mut Vec<u32>, key: u64, purchase: bool) {
    let rank = (key >> 32) as u32;
    let cursor = dir.event(rank);
    if cursor[T] == W::ZERO {
        occupied.push(rank);
    }
    cursor[T] += W::ONE;
    if purchase {
        cursor[P] += W::ONE;
    }
}

fn compact_fits(events: usize, mut clicks: impl Iterator<Item = usize>) -> bool {
    u32::try_from(events).is_ok()
        && clicks
            .try_fold(0u32, |total, n| total.checked_add(u32::try_from(n).ok()?))
            .is_some()
}

fn prefix<W: Word>(dir: &mut impl Directory<W>, occupied: &[u32]) -> [usize; 3] {
    let mut totals = [0usize; 3];
    for &rank in occupied {
        let cursor = dir.event(rank);
        for i in 0..3 {
            let count = cursor[i].as_usize();
            cursor[i] = W::from_usize(totals[i]);
            totals[i] = totals[i].checked_add(count).expect("directory total overflow");
        }
    }
    totals
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
    // The total events bound both event and purchase endpoints. Counting all
    // clicks, including click-only ranks, bounds the selected-click endpoints.
    if compact_fits(ne, clicks.iter().map(Vec::len)) {
        dispatch::<u32>(owner, events, clicks, people, pack, reduce, ne);
    } else {
        dispatch::<usize>(owner, events, clicks, people, pack, reduce, ne);
    }
}

fn dispatch<W: Word>(
    owner: usize,
    events: Vec<Stream<u64>>,
    clicks: Vec<Vec<RankedClick>>,
    people: &People,
    pack: Packing,
    reduce: &mut Reduce,
    ne: usize,
) {
    // A tiny input can touch only a few addresses of a large link table.
    // In that case do not allocate or walk the entire address space.
    if people.local_slots <= ne.saturating_mul(2) {
        scatter(
            vec![[W::ZERO; 3]; people.local_slots],
            owner,
            events,
            clicks,
            people,
            pack,
            reduce,
        );
    } else {
        scatter::<W>(Map::with_hasher(rs()), owner, events, clicks, people, pack, reduce);
    }
}

fn scatter<W: Word>(
    mut dir: impl Directory<W>,
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
            if let Some(cursor) = dir.existing(c.rank).filter(|c| c[T] != W::ZERO) {
                cursor[C] += W::ONE;
            }
        }
    }
    let totals = prefix(&mut dir, &occupied);
    let mut times = buffer(totals[T]);
    let mut purchases = buffer(totals[P]);
    let mut selected_clicks = buffer(totals[C]);
    for part in events {
        for key in part.times {
            let cursor = dir.event((key >> 32) as u32);
            times[cursor[T].as_usize()].write(key as u32);
            cursor[T] += W::ONE;
        }
        for p in part.purchases {
            let cursor = dir.event((p.key >> 32) as u32);
            let key = p.key as u32;
            times[cursor[T].as_usize()].write(key);
            cursor[T] += W::ONE;
            purchases[cursor[P].as_usize()].write(Purchase {
                key,
                price: p.price,
                cat: p.cat,
            });
            cursor[P] += W::ONE;
        }
    }
    for part in clicks {
        for c in part {
            // All occupied ranks now have a nonzero event *end*. A rank
            // with no event was never prefixed and retains its zero count.
            if let Some(cursor) = dir.existing(c.rank).filter(|c| c[T] != W::ZERO) {
                selected_clicks[cursor[C].as_usize()].write(Click {
                    ts: c.ts,
                    cid: c.cid,
                    camp: c.camp,
                });
                cursor[C] += W::ONE;
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
        let end = dir.event(rank).map(Word::as_usize);
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
    fn compact_width_includes_maximum_endpoints() {
        assert_eq!(std::mem::size_of::<Cursor<u32>>(), 12);
        let max = u32::MAX as usize;
        assert!(compact_fits(max, [max, 0].into_iter()));
        assert!(compact_fits(1, [max - 1, 1].into_iter()));
        assert!(!compact_fits(1, [max, 1].into_iter()));
        assert!(!compact_fits(1, [usize::MAX, 1].into_iter()));
        #[cfg(target_pointer_width = "64")]
        assert!(!compact_fits(max + 1, [0].into_iter()));

        let mut counted = vec![[u32::MAX - 1; 3]];
        let mut occupied = Vec::new();
        count(&mut counted, &mut occupied, 0, true);
        counted[0][C] += 1;
        assert_eq!(counted[0], [u32::MAX; 3]);
        assert!(occupied.is_empty());

        let mut dir = vec![[0u32; 3]; 2];
        dir[0] = [u32::MAX - 1, u32::MAX - 1, u32::MAX - 1];
        dir[1] = [1, 1, 1];
        assert_eq!(prefix(&mut dir, &[0, 1]), [max; 3]);
        assert_eq!(dir[1], [u32::MAX - 1; 3]);
        for cursor in &mut dir[1] {
            *cursor += 1;
        }
        assert_eq!(dir[1], [u32::MAX; 3]);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn wide_prefix_keeps_counts_above_u32_max() {
        let max = u32::MAX as usize;
        let mut dir = vec![[max, max, max], [1, 1, 1]];
        assert_eq!(prefix(&mut dir, &[0, 1]), [max + 1; 3]);
        assert_eq!(dir[1], [max; 3]);
        let mut sparse: Map<u32, Cursor<usize>> = Map::with_hasher(rs());
        sparse.insert(3, [max; 3]);
        sparse.insert(u32::MAX, [1; 3]);
        assert_eq!(prefix(&mut sparse, &[3, u32::MAX]), [max + 1; 3]);
        assert_eq!(sparse[&u32::MAX], [max; 3]);
    }

    #[test]
    fn sparse_directory_keeps_large_rank_and_ignores_missing_clicks() {
        let mut dir: Map<u32, Cursor<u32>> = Map::with_hasher(rs());
        let mut occupied = Vec::new();
        let rank = u32::MAX;
        count(&mut dir, &mut occupied, (rank as u64) << 32, true);
        count(&mut dir, &mut occupied, (rank as u64) << 32, false);
        assert!(dir.existing(rank - 1).is_none());
        dir.existing(rank).unwrap()[C] += 1;
        assert_eq!(prefix(&mut dir, &occupied), [2, 1, 1]);
        assert_eq!(occupied, [rank]);
        assert_eq!(*dir.existing(rank).unwrap(), [0, 0, 0]);
    }

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
