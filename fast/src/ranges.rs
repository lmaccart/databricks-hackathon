//! The low rank byte is already known from routing. Histogram the remaining
//! address, then scatter into person-local ranges instead of sorting rank keys.
use super::*;
use std::mem::MaybeUninit;

const T: usize = 0;
const P: usize = 1;
const C: usize = 2;
type Cursor = [usize; 3];

// A tagged temporal cursor is a fixed bin base, not a moving event offset.
// The high bit cannot be set by an offset into an allocatable Vec<u32>.
const HOT: usize = 1 << (usize::BITS - 1);
const BIN_THRESHOLD: usize = 4096;
const MAX_SCATTER_BINS: usize = 32_768; // 512 KiB per active owner on 64-bit

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

fn histogram<const TRACK_SPAN: bool>(
    dir: &mut impl Directory,
    occupied: &mut Vec<u32>,
    events: &[Stream<u64>],
) -> (usize, usize) {
    // An owner-wide observed span avoids growing every directory entry with
    // per-rank extrema. All hot ranges share these time coordinates.
    let (mut lo, mut hi) = (u32::MAX, 0);
    for part in events {
        for &key in &part.times {
            count(dir, occupied, key, false);
            if TRACK_SPAN {
                lo = lo.min(key as u32);
                hi = hi.max(key as u32);
            }
        }
        for p in &part.purchases {
            count(dir, occupied, p.key, true);
            if TRACK_SPAN {
                lo = lo.min(p.key as u32);
                hi = hi.max(p.key as u32);
            }
        }
    }
    if TRACK_SPAN {
        let origin = (lo >> BIN_SHIFT) as usize;
        (origin, (hi >> BIN_SHIFT) as usize - origin + 1)
    } else {
        (0, 0)
    }
}

fn buffer<V>(len: usize) -> Vec<MaybeUninit<V>> {
    let mut v = Vec::new();
    v.resize_with(len, MaybeUninit::uninit);
    v
}

fn plan_timeline(count: usize, span: usize, nt: &mut usize, nb: &mut usize) -> usize {
    // Reserve bins only when they use at most half the bytes of flat times.
    let per_bin = 2 * size_of::<Bin>() / size_of::<u32>();
    if count >= BIN_THRESHOLD.max(per_bin * span) && span != 0 && *nb + span <= MAX_SCATTER_BINS {
        let start = *nb;
        *nb += span;
        HOT | start
    } else {
        let start = *nt;
        *nt += count;
        start
    }
}

#[inline]
fn scatter_time<const BINNED: bool>(
    cursor: &mut usize,
    ts: u32,
    times: &mut [MaybeUninit<u32>],
    bins: &mut [Bin],
    origin: usize,
) {
    if BINNED && *cursor & HOT != 0 {
        bins[(*cursor ^ HOT) + ((ts >> BIN_SHIFT) as usize - origin)].add(ts);
    } else {
        times[*cursor].write(ts);
        *cursor += 1;
    }
}

fn scatter_events<const BINNED: bool>(
    dir: &mut impl Directory,
    events: Vec<Stream<u64>>,
    times: &mut [MaybeUninit<u32>],
    purchases: &mut [MaybeUninit<Purchase<u32>>],
    bins: &mut [Bin],
    origin: usize,
) {
    for part in events {
        for key in part.times {
            let cursor = dir.event((key >> 32) as u32);
            scatter_time::<BINNED>(&mut cursor[T], key as u32, times, bins, origin);
        }
        for p in part.purchases {
            let cursor = dir.event((p.key >> 32) as u32);
            let key = p.key as u32;
            scatter_time::<BINNED>(&mut cursor[T], key, times, bins, origin);
            purchases[cursor[P]].write(Purchase {
                key,
                price: p.price,
                cat: p.cat,
            });
            cursor[P] += 1;
        }
    }
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
            (events, clicks),
            ne,
            people,
            pack,
            reduce,
        );
    } else {
        scatter(
            Map::with_hasher(rs()),
            owner,
            (events, clicks),
            ne,
            people,
            pack,
            reduce,
        );
    }
}

fn scatter(
    mut dir: impl Directory,
    owner: usize,
    (events, clicks): (Vec<Stream<u64>>, Vec<Vec<RankedClick>>),
    ne: usize,
    people: &People,
    pack: Packing,
    reduce: &mut Reduce,
) {
    let mut occupied = Vec::new();
    let (origin, span) = if ne >= BIN_THRESHOLD {
        histogram::<true>(&mut dir, &mut occupied, &events)
    } else {
        histogram::<false>(&mut dir, &mut occupied, &events)
    };
    for part in &clicks {
        for c in part {
            if let Some(cursor) = dir.existing(c.rank).filter(|c| c[T] != 0) {
                cursor[C] += 1;
            }
        }
    }
    let mut totals = [0; 3];
    let mut nb = 0;
    for &rank in &occupied {
        let cursor = dir.event(rank);
        let counts = std::mem::replace(cursor, totals);
        cursor[T] = plan_timeline(counts[T], span, &mut totals[T], &mut nb);
        totals[P] += counts[P];
        totals[C] += counts[C];
    }
    let mut times = buffer(totals[T]);
    let mut purchases = buffer(totals[P]);
    let mut selected_clicks = buffer(totals[C]);
    let mut hot_bins = vec![Bin::default(); nb];
    if hot_bins.is_empty() {
        scatter_events::<false>(&mut dir, events, &mut times, &mut purchases, &mut [], 0);
    } else {
        scatter_events::<true>(&mut dir, events, &mut times, &mut purchases, &mut hot_bins, origin);
    }
    for part in clicks {
        for c in part {
            // All occupied ranks now have a nonzero event end or tagged
            // bin base. Unoccupied ranks retain their zero count.
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
    // each reserved flat interval once. Its cursor is now the interval end.
    let (mut times, mut purchases, mut clicks) =
        unsafe { (initialized(times), initialized(purchases), initialized(selected_clicks)) };
    let mut fallback_bins = Vec::new();
    let mut start = [0; 3];
    for rank in occupied {
        let end = *dir.event(rank);
        let ps = &mut purchases[start[P]..end[P]];
        let cs = &mut clicks[start[C]..end[C]];
        ps.sort_unstable_by_key(|p| p.key);
        cs.sort_unstable_by_key(|c| c.ts);
        let person = people.original((rank << NS.trailing_zeros()) | owner as u32);
        if end[T] & HOT != 0 {
            let base = end[T] ^ HOT;
            reduce.person(person, joined_bins(&hot_bins[base..base + span]), ps, cs, pack);
        } else {
            let ts = &mut times[start[T]..end[T]];
            if ts.len() >= BIN_THRESHOLD {
                reduce.person(person, bin_sessions(ts, &mut fallback_bins), ps, cs, pack);
            } else {
                ts.sort_unstable();
                reduce.person(person, ordered_sessions(ts), ps, cs, pack);
            }
            start[T] = end[T];
        }
        start[P] = end[P];
        start[C] = end[C];
    }
}

#[derive(Clone, Copy)]
struct Bin {
    lo: u32,
    hi: u32,
    count: usize,
}

impl Default for Bin {
    fn default() -> Self {
        Self {
            lo: u32::MAX,
            hi: 0,
            count: 0,
        }
    }
}

impl Bin {
    #[inline]
    fn add(&mut self, ts: u32) {
        self.lo = self.lo.min(ts);
        self.hi = self.hi.max(ts);
        self.count += 1;
    }
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
        bins[((ts >> BIN_SHIFT) - origin) as usize].add(ts);
    }
    joined_bins(bins)
}

fn joined_bins(bins: &[Bin]) -> impl Iterator<Item = (u32, u32, usize)> + '_ {
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
    fn timeline_plan_keeps_cold_offsets_contiguous_and_caps_hot_bins() {
        let span = (u32::MAX >> BIN_SHIFT) as usize + 1;
        assert_eq!(span, 4096);
        let (mut nt, mut nb) = (0, 0);
        let hot_count = 2 * size_of::<Bin>() / size_of::<u32>() * span;
        assert_eq!(plan_timeline(hot_count - 1, span, &mut nt, &mut nb), 0);
        for base in (0..MAX_SCATTER_BINS).step_by(span) {
            assert_eq!(plan_timeline(hot_count, span, &mut nt, &mut nb), HOT | base);
        }
        assert_eq!(plan_timeline(hot_count, span, &mut nt, &mut nb), hot_count - 1);
        assert_eq!(plan_timeline(3, span, &mut nt, &mut nb), 2 * hot_count - 1);
        assert_eq!((nt, nb), (2 * hot_count + 2, MAX_SCATTER_BINS));
    }

    #[test]
    fn scatter_mixes_hot_and_cold_ranks_across_parts_at_full_width() {
        let span = (u32::MAX >> BIN_SHIFT) as usize + 1;
        let g = GAP as u32;
        let key = |rank: u32, ts: u32| (u64::from(rank) << 32) | u64::from(ts);
        // Fixed hot bases and moving cold offsets can be interleaved in
        // the same owner, including a purchase-only hot timeline.
        let mut dir = vec![[0, 0, 0], [HOT, 0, 0], [1, 0, 0], [HOT | span, 1, 0]];
        let events = vec![
            Stream {
                times: vec![key(1, g), key(0, 9), key(1, 2 * g + 1)],
                purchases: vec![Purchase {
                    key: key(3, u32::MAX),
                    price: 7,
                    cat: 1,
                }],
            },
            Stream {
                times: vec![key(2, 1), key(1, 0), key(2, 2)],
                purchases: vec![
                    Purchase {
                        key: key(1, g),
                        price: 13,
                        cat: 2,
                    },
                    Purchase {
                        key: key(3, u32::MAX - g),
                        price: 17,
                        cat: 3,
                    },
                ],
            },
        ];
        let mut times = vec![MaybeUninit::new(u32::MAX); 3];
        let mut purchases = vec![
            MaybeUninit::new(Purchase {
                key: 0,
                price: 0,
                cat: 0
            });
            3
        ];
        let mut hot_bins = vec![Bin::default(); 2 * span];
        scatter_events::<true>(&mut dir, events, &mut times, &mut purchases, &mut hot_bins, 0);
        assert_eq!(dir, [[1, 0, 0], [HOT, 1, 0], [3, 0, 0], [HOT | span, 3, 0]]);
        assert_eq!(unsafe { initialized(times) }, [9, 1, 2]);
        assert_eq!(
            unsafe { initialized(purchases) }
                .iter()
                .map(|p| (p.key, p.price, p.cat))
                .collect::<Vec<_>>(),
            [(g, 13, 2), (u32::MAX, 7, 1), (u32::MAX - g, 17, 3)]
        );
        assert_eq!(
            joined_bins(&hot_bins[..span]).collect::<Vec<_>>(),
            [(0, g, 3), (2 * g + 1, 2 * g + 1, 1)]
        );
        assert_eq!(
            joined_bins(&hot_bins[span..]).collect::<Vec<_>>(),
            [(u32::MAX - g, u32::MAX, 2)]
        );
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
