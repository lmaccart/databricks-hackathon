//! Shared, exact person descriptors for the two streams in a shard.
use super::{
    ClickOrder, Clicks, EventCell, GAP, Map, NS, Packing, Payload, People, SHARD_SHIFT, ShardOut, Small, Temporal,
    aggregate, rs, stream::Purchase,
};

const BIN_BITS: u32 = 20;
const _: () = assert!((1 << BIN_BITS) <= GAP);
const SHARD_MASK: u64 = (NS as u64 - 1) << SHARD_SHIFT;
// Routing bits are implicit here. Reuse one to distinguish exact link-root
// indexes from standalone tokens, which never appear in the link lookup.
// When parse-time packing is unavailable, both streams use full mixed keys.
const LINKED: u64 = 1 << SHARD_SHIFT;
const DESCRIPTOR_BITS: u64 = SHARD_MASK ^ LINKED;
const _: () = assert!(SHARD_SHIFT >= 32 && NS.is_power_of_two() && NS > 2);

struct Ranks {
    // Select the same low-bit prefix from every canonical token in this
    // shard. A prefix is just an address, never evidence of identity.
    slots: Vec<u64>,
    prefix_bits: u32,
    low_mask: u64,
    collisions: Map<u64, u32>,
    persons: Vec<u64>,
}

impl Ranks {
    fn new(rows: usize) -> Self {
        let size = (rows / 2).clamp(64, 1 << 16).next_power_of_two();
        Self {
            slots: vec![0; size],
            prefix_bits: size.trailing_zeros(),
            low_mask: size as u64 - 1,
            collisions: Map::with_hasher(rs()),
            persons: Vec::new(),
        }
    }

    fn max_count(&self) -> u64 {
        (1 << (self.prefix_bits + DESCRIPTOR_BITS.count_ones())) - 1
    }

    #[inline]
    fn rank(&mut self, person: u64) -> u64 {
        let i = (person & self.low_mask) as usize;
        let slot = &mut self.slots[i];
        let desc = *slot;
        let token = person & !(self.low_mask | DESCRIPTOR_BITS);
        if desc == 0 {
            let rank = self.persons.len() as u64;
            self.persons.push(person);
            // The caller rejects any rank that cannot fit before looking up
            // another key. Low bits are implicit in i, and shard bits in the job.
            let code = rank + 1;
            *slot = token | (code & self.low_mask) | ((code >> self.prefix_bits) << (SHARD_SHIFT + 1));
            return rank;
        }
        if (desc ^ token) & !(self.low_mask | DESCRIPTOR_BITS) == 0 {
            let high = (desc & DESCRIPTOR_BITS) >> (SHARD_SHIFT + 1);
            return ((high << self.prefix_bits) | (desc & self.low_mask)) - 1;
        }
        u64::from(*self.collisions.entry(person).or_insert_with(|| {
            let rank = self.persons.len() as u32;
            self.persons.push(person);
            rank
        }))
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Key(u64);

impl Key {
    #[inline]
    fn new(rank: u64, rel: u32) -> Self {
        Self(rank << 32 | u64::from(rel))
    }

    #[inline]
    fn cell(self) -> u32 {
        (self.0 >> BIN_BITS) as u32
    }
}

impl Temporal for Key {
    type Layout = i64;

    #[inline]
    fn person(self, _: Self::Layout) -> u64 {
        self.0 >> 32
    }
    #[inline]
    fn ts(self, origin: Self::Layout) -> i64 {
        origin + i64::from(self.0 as u32)
    }
    #[inline]
    fn gap(self, prev: Self) -> u64 {
        self.0 - prev.0
    }
}

#[derive(Clone, Copy, Default)]
struct Cell {
    first: Key,
    last_time: u32,
    count: u32,
}

impl Cell {
    #[inline]
    fn add(&mut self, k: Key) {
        if self.count == 0 {
            self.first = k;
            self.last_time = k.0 as u32;
        } else {
            self.first = self.first.min(k);
            self.last_time = self.last_time.max(k.0 as u32);
        }
        self.count += 1;
    }
}

fn extract_dense_cells(times: &mut Vec<Key>, persons: usize, bins: usize) -> Vec<Cell> {
    // Only allocate a dense timeline when the observed sample suggests that
    // the avoided event sort outweighs the cost of walking all the bins.
    const STRIDE: usize = 32;
    if times.len() < bins * 4 {
        return Vec::new();
    }
    let cutoff = (bins * 4).div_ceil(STRIDE).max(2);
    let mut offsets = vec![0usize; persons];
    for k in times.iter().step_by(STRIDE) {
        offsets[(k.0 >> 32) as usize] += 1;
    }
    let mut n = 0;
    for x in &mut offsets {
        if *x >= cutoff {
            *x = n + 1;
            n += bins;
        } else {
            *x = 0;
        }
    }
    if n == 0 {
        return Vec::new();
    }
    let mut dense = vec![Cell::default(); n];
    times.retain(|k| {
        let base = offsets[(k.0 >> 32) as usize];
        if base == 0 {
            true
        } else {
            dense[base - 1 + ((k.0 as u32) >> BIN_BITS) as usize].add(*k);
            false
        }
    });
    dense
}

impl EventCell<Key> for Cell {
    #[inline]
    fn first(&self) -> Key {
        self.first
    }
    #[inline]
    fn last(&self) -> Key {
        Key::new(self.first.0 >> 32, self.last_time)
    }
    #[inline]
    fn count(&self) -> usize {
        self.count as usize
    }
}

struct OrderedClicks<'a> {
    keys: Vec<u64>,
    camps: Vec<u32>,
    ids: Vec<&'a [u8]>,
    origin: i64,
    time_bits: u32,
    index_bits: u32,
}

fn width(value: u64) -> u32 {
    u64::BITS - value.leading_zeros()
}

impl<'a> OrderedClicks<'a> {
    fn new(n: usize, origin: i64, span: u32) -> Option<Self> {
        let time_bits = width(u64::from(span));
        let index_bits = width(n.saturating_sub(1) as u64);
        (time_bits + index_bits < 64).then(|| Self {
            keys: Vec::with_capacity(n),
            camps: Vec::with_capacity(n),
            ids: Vec::with_capacity(n),
            origin,
            time_bits,
            index_bits,
        })
    }

    #[inline]
    fn index(&self, i: usize) -> usize {
        (self.keys[i] & ((1 << self.index_bits) - 1)) as usize
    }

    #[inline]
    fn push(&mut self, rank: u64, ts: i64, camp: u32, id: &'a [u8]) {
        let rel = ts.wrapping_sub(self.origin) as u64;
        self.keys
            .push(((rank << self.time_bits | rel) << self.index_bits) | self.keys.len() as u64);
        self.camps.push(camp);
        self.ids.push(id);
    }
}

impl ClickOrder for OrderedClicks<'_> {
    #[inline]
    fn len(&self) -> usize {
        self.keys.len()
    }
    #[inline]
    fn person(&self, i: usize) -> u64 {
        self.keys[i] >> (self.time_bits + self.index_bits)
    }
    #[inline]
    fn ts(&self, i: usize) -> i64 {
        self.origin + ((self.keys[i] >> self.index_bits) & ((1 << self.time_bits) - 1)) as i64
    }
    #[inline]
    fn camp(&self, i: usize) -> u32 {
        self.camps[self.index(i)]
    }
    #[inline]
    fn id(&self, i: usize) -> &[u8] {
        self.ids[self.index(i)]
    }
}

fn observed_bounds(evs: &[Payload], cls: &[Clicks<'_>], packing: Option<Packing>) -> (i64, i64) {
    let (mut lo, mut hi) = (i64::MAX, i64::MIN);
    let mut observe = |ts| {
        lo = lo.min(ts);
        hi = hi.max(ts);
    };
    for e in evs {
        if let Some(p) = packing {
            for &key in &e.packed.times {
                observe(key.ts(p));
            }
            for x in &e.packed.purchases {
                observe(x.key.ts(p));
            }
        }
        for key in &e.wide.times {
            observe(key.ts(()));
        }
        for x in &e.wide.purchases {
            observe(x.key.ts(()));
        }
    }
    for clicks in cls {
        for c in clicks.packed.iter().chain(&clicks.wide) {
            observe(c.ts);
        }
    }
    (lo, hi)
}

pub(super) fn try_compact(
    evs: &mut Vec<Payload>,
    cls: &mut Vec<Clicks<'_>>,
    sm: &Small,
    people: &People,
    Layout {
        packing,
        click_window: (mut lo, end),
        shard,
    }: Layout,
    out: &mut ShardOut,
) -> bool {
    let np: usize = evs
        .iter()
        .map(|e| e.packed.purchases.len() + e.wide.purchases.len())
        .sum();
    let ne = np
        + evs
            .iter()
            .map(|e| e.packed.times.len() + e.wide.times.len())
            .sum::<usize>();
    if ne == 0 {
        return true;
    }
    let nc: usize = cls.iter().map(|c| c.packed.len() + c.wide.len()).sum();
    if u32::try_from(ne).is_err() {
        return false;
    }
    // Don't scan the common bounded input twice. A wide manifest is not a
    // reason to reject a sparse shard if its *actual* timestamps still fit.
    let span = if let Ok(span) = u32::try_from(i128::from(end) - 1 - i128::from(lo)) {
        span
    } else {
        let (actual_lo, actual_hi) = observed_bounds(evs, cls, packing);
        lo = actual_lo;
        let Ok(span) = u32::try_from(actual_hi.abs_diff(lo)) else {
            return false;
        };
        span
    };
    let Some(mut clicks) = OrderedClicks::new(nc, lo, span) else {
        return false;
    };
    let mut ranks = Ranks::new(ne.saturating_add(nc));
    let click_rank_bits = 64 - clicks.time_bits - clicks.index_bits;
    // A u32 coarse key has BIN_BITS free after dropping its millisecond tail.
    let max_rank = (1 << click_rank_bits.min(BIN_BITS)).min(ranks.max_count());
    let mut times = Vec::with_capacity(ne);
    let mut purchases = Vec::with_capacity(np);
    let mut emit = |person, ts: i64, price, cat| {
        let rank = ranks.rank(person);
        let key = Key::new(rank, ts.wrapping_sub(lo) as u32);
        times.push(key);
        if price != 0 {
            purchases.push(Purchase { key, price, cat });
        }
        rank < max_rank
    };
    for e in evs.iter() {
        if let Some(p) = packing {
            for &key in &e.packed.times {
                if !emit(key.person(p) | LINKED, key.ts(p), 0, 0) {
                    return false;
                }
            }
            for x in &e.packed.purchases {
                let k = x.key;
                if !emit(k.person(p) | LINKED, k.ts(p), x.price, x.cat) {
                    return false;
                }
            }
        }
        for k in &e.wide.times {
            if !emit(k.person & !SHARD_MASK, k.ts, 0, 0) {
                return false;
            }
        }
        for x in &e.wide.purchases {
            if !emit(x.key.person & !SHARD_MASK, x.key.ts, x.price, x.cat) {
                return false;
            }
        }
    }
    for c in cls.iter() {
        let source = c
            .wide
            .iter()
            .map(|c| (c.person & !SHARD_MASK, c))
            .chain(c.packed.iter().map(|c| (c.person | LINKED, c)));
        for (person, c) in source {
            let rank = ranks.rank(person);
            if rank >= max_rank {
                return false;
            }
            clicks.push(rank, c.ts, c.camp, c.cid);
        }
    }
    evs.clear();
    cls.clear();
    let persons = std::mem::take(&mut ranks.persons);
    drop(ranks);
    clicks.keys.sort_unstable();
    purchases.sort_unstable_by_key(|x| x.key);
    let dense = extract_dense_cells(&mut times, persons.len(), ((span >> BIN_BITS) + 1) as usize);
    times.sort_unstable_by_key(|k| k.cell());
    let mut cells = Vec::new();
    let mut dense = dense.into_iter().filter(|c| c.count != 0).peekable();
    // Two events in one bin cannot be separated by a session break: the
    // bin is narrower than GAP. Across bins only endpoints are relevant.
    for group in times.chunk_by(|a, b| a.cell() == b.cell()) {
        let (mut first, mut last) = (group[0], group[0]);
        for &k in &group[1..] {
            first = first.min(k);
            last = last.max(k);
        }
        while dense.peek().is_some_and(|d| d.first < first) {
            cells.push(dense.next().unwrap());
        }
        cells.push(Cell {
            first,
            last_time: last.0 as u32,
            count: group.len() as u32,
        });
    }
    cells.extend(dense);
    drop(times);
    let name = |r: u64| {
        let token = persons[r as usize];
        if token & LINKED != 0 {
            people.original(token as u32)
        } else {
            token | ((shard as u64) << SHARD_SHIFT)
        }
    };
    aggregate(&cells, &purchases, &clicks, sm, name, lo, out);
    true
}

#[derive(Clone, Copy)]
pub(super) struct Layout {
    pub packing: Option<Packing>,
    pub click_window: (i64, i64),
    pub shard: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_prefix_and_zero_still_get_distinct_exact_ranks() {
        let mut d = Ranks::new(12);
        let step = d.slots.len() as u64;
        let inputs = [0, step, 2 * step, 1, u64::MAX & !SHARD_MASK, LINKED];
        for (rank, person) in inputs.into_iter().enumerate() {
            assert_eq!(d.rank(person), rank as u64);
        }
        for (rank, person) in inputs.into_iter().enumerate().rev() {
            assert_eq!(d.rank(person), rank as u64);
        }
    }

    #[test]
    fn click_words_keep_milliseconds_and_dispatch_to_original_sidecars() {
        let mut x = OrderedClicks::new(4, -1, u32::MAX).unwrap();
        x.push(1, -1, 5, b"external click ID");
        x.push(0, 0, 7, b"later");
        x.push(0, -1, 11, b"z");
        x.push(0, -1, 13, b"a");
        x.keys.sort_unstable();
        assert_eq!((x.person(0), x.person(2), x.person(3)), (0, 0, 1));
        assert_eq!((x.ts(0), x.ts(1), x.ts(2), x.ts(3)), (-1, -1, 0, -1));
        assert_eq!((x.camp(0), x.camp(1), x.camp(2), x.camp(3)), (11, 13, 7, 5));
        assert_eq!(x.id(1), b"a");
        assert_eq!(x.id(3), b"external click ID");
    }

    #[test]
    fn dense_bins_only_remove_hot_rows_and_keep_true_extrema() {
        let mut times: Vec<_> = (0..1024)
            .rev()
            .map(|ms| Key::new(1, (ms % 2) * (1 << BIN_BITS) + ms))
            .collect();
        times.push(Key::new(0, 0));
        let out = extract_dense_cells(&mut times, 2, 2);
        assert_eq!(times.len(), 1);
        assert_eq!(times[0].0, 0);
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].first.0 as u32, out[0].last_time, out[0].count), (0, 1022, 512));
        assert_eq!(
            (out[1].first.0 as u32, out[1].last_time, out[1].count),
            ((1 << BIN_BITS) + 1, (1 << BIN_BITS) + 1023, 512),
        );
    }
}
