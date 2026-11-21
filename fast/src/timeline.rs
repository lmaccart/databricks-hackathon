//! Connected components of each person's surviving, unsorted timestamps.
use crate::stream::{Packing, Purchase, Session, Stream, Wide};
use crate::{Map, rs};

const GAP: u32 = crate::GAP as u32;
const INLINE: usize = 12;
const DENSE: usize = 128;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Interval {
    first: u32,
    last: u32,
    count: u32,
}

impl Interval {
    fn point(t: u32) -> Self {
        Self {
            first: t,
            last: t,
            count: 1,
        }
    }
}

struct Components {
    spans: Vec<Interval>,
    bins: Vec<Interval>,
    occupied: [u64; (u32::MAX / GAP / 64 + 1) as usize],
    words: u64,
    max_offset: u32,
}

impl Components {
    fn new(max_offset: u32) -> Self {
        Self {
            spans: Vec::with_capacity(INLINE),
            bins: Vec::new(),
            occupied: [0; (u32::MAX / GAP / 64 + 1) as usize],
            words: 0,
            max_offset,
        }
    }

    #[inline]
    fn insert(&mut self, t: u32) -> bool {
        // An interval represents a chain, so any point inside it belongs to
        // it. Outside the interval only the nearest actual endpoint matters.
        let low = t.saturating_sub(GAP);
        let high = t.saturating_add(GAP);
        let mut i = 0;
        while i < self.spans.len() && self.spans[i].last < low {
            i += 1;
        }
        if i == self.spans.len() || high < self.spans[i].first {
            if self.spans.len() == INLINE {
                return false;
            }
            self.spans.insert(i, Interval::point(t));
        } else {
            let s = &mut self.spans[i];
            s.count += 1;
            if t <= s.last {
                s.first = s.first.min(t);
                return true;
            }
            s.last = t;
            // Distinct components were more than GAP apart before insertion.
            // The new point can bridge at most its immediate two neighbors.
            if i + 1 < self.spans.len() && self.spans[i + 1].first <= high {
                let next = self.spans.remove(i + 1);
                self.spans[i].last = next.last;
                self.spans[i].count += next.count;
            }
        }
        true
    }

    fn append(&mut self, s: Interval) {
        if let Some(prev) = self.spans.last_mut().filter(|prev| s.first - prev.last <= GAP) {
            prev.last = s.last;
            prev.count += s.count;
        } else {
            self.spans.push(s);
        }
    }

    #[cold]
    fn dense(&mut self, times: &[u32]) {
        if self.bins.is_empty() {
            self.bins
                .resize((self.max_offset / GAP + 1) as usize, Interval::default());
        }
        for &t in times {
            let bin = (t / GAP) as usize;
            let s = &mut self.bins[bin];
            if s.count == 0 {
                *s = Interval::point(t);
                self.occupied[bin / 64] |= 1 << (bin % 64);
                self.words |= 1 << (bin / 64);
            } else {
                s.first = s.first.min(t);
                s.last = s.last.max(t);
                s.count += 1;
            }
        }
        // Only occupied words and occupied bins are visited or reset. Each
        // bin is connected (strictly less than GAP wide); neighboring bins
        // join iff the exact preceding max and following min are close enough.
        while self.words != 0 {
            let w = self.words.trailing_zeros() as usize;
            self.words &= self.words - 1;
            let mut bits = std::mem::take(&mut self.occupied[w]);
            while bits != 0 {
                let b = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let s = std::mem::take(&mut self.bins[w * 64 + b]);
                self.append(s);
            }
        }
    }

    #[cold]
    fn sorted(&mut self, times: &mut [u32]) {
        self.spans.clear();
        times.sort_unstable();
        for &t in times.iter() {
            self.append(Interval::point(t));
        }
    }

    fn fill(&mut self, times: &mut [u32]) {
        self.spans.clear();
        if times.len() >= DENSE {
            self.dense(times);
        } else if let Some((&first, rest)) = times.split_first() {
            // Most people have few sessions. Collapse an entire connected
            // prefix at once: if its extent is within GAP, every edge exists.
            let mut s = Interval::point(first);
            let mut remaining = rest;
            while let Some((&t, tail)) = remaining.split_first() {
                let lo = s.first.min(t);
                let hi = s.last.max(t);
                if hi - lo > GAP {
                    break;
                }
                s.first = lo;
                s.last = hi;
                remaining = tail;
            }
            s.count = (times.len() - remaining.len()) as u32;
            self.spans.push(s);
            for &t in remaining {
                if !self.insert(t) {
                    self.sorted(times);
                    break;
                }
            }
        }
    }
}

struct Grouped<I> {
    rank: usize,
    slices: I,
    times: Vec<u32>,
    components: Components,
    next: usize,
}

impl<I> Grouped<I> {
    fn new(slices: I, times: Vec<u32>, max_offset: u32) -> Self {
        Self {
            rank: 0,
            slices,
            times,
            components: Components::new(max_offset),
            next: 0,
        }
    }
}

impl<I: Iterator<Item = (usize, usize, usize)>> Iterator for Grouped<I> {
    type Item = (usize, Interval);

    fn next(&mut self) -> Option<Self::Item> {
        while self.next == self.components.spans.len() {
            let (rank, start, end) = self.slices.next()?;
            self.components.fill(&mut self.times[start..end]);
            self.rank = rank;
            self.next = 0;
        }
        let s = self.components.spans[self.next];
        self.next += 1;
        Some((self.rank, s))
    }
}

fn prefix_sum(counts: &mut [usize]) {
    let mut sum = 0;
    for count in counts {
        let n = *count;
        *count = sum;
        sum += n;
    }
}

pub(super) fn packed_sessions(
    pieces: Vec<Stream<u64>>,
    ranks: std::ops::Range<usize>,
    pack: Packing,
    n: usize,
    np: usize,
    max_offset: u32,
) -> (impl Iterator<Item = Session<u64>>, Vec<Purchase<u64>>) {
    let base = ranks.start;
    let rank_index = |key| pack.rank(key) as usize - base;
    let mut ends = vec![0usize; ranks.len()];
    for piece in &pieces {
        for &key in &piece.times {
            ends[rank_index(key)] += 1;
        }
        for p in &piece.purchases {
            ends[rank_index(p.key)] += 1;
        }
    }
    prefix_sum(&mut ends);
    let mut times = vec![0; n];
    let mut purchases = Vec::with_capacity(np);
    let mut emit = |key| {
        let next = &mut ends[rank_index(key)];
        times[*next] = pack.offset(key) as u32;
        *next += 1;
    };
    for piece in pieces {
        for key in piece.times {
            emit(key);
        }
        for p in piece.purchases {
            emit(p.key);
            purchases.push(p);
        }
    }
    let mut start = 0;
    let slices = ranks.zip(ends).map(move |(rank, end)| {
        let item = (rank, start, end);
        start = end;
        item
    });
    let sessions = Grouped::new(slices, times, max_offset).map(move |(rank, s)| Session {
        first: pack.with_offset(rank as u32, u64::from(s.first)),
        last: pack.with_offset(rank as u32, u64::from(s.last)),
        count: s.count as usize,
    });
    (sessions, purchases)
}

pub(super) fn wide_sessions(
    mut pieces: Vec<Stream<Wide>>,
    n: usize,
    np: usize,
    origin: i64,
    max_offset: u32,
) -> (impl Iterator<Item = Session<Wide>>, Vec<Purchase<Wide>>) {
    let mut by_person = Map::with_capacity_and_hasher(n / 16, rs());
    let mut names = Vec::new();
    let mut ends = Vec::new();
    let mut purchases = Vec::with_capacity(np);

    // Save each rank in the source payload for the scatter. Only the sparse
    // purchase sidecar and the distinct names retain full person tokens.
    let mut count = |key: &mut Wide| {
        let rank = *by_person.entry(key.person).or_insert_with(|| {
            let rank = names.len();
            names.push(key.person);
            ends.push(0);
            rank
        });
        key.person = rank as u64;
        ends[rank] += 1;
    };
    for s in &mut pieces {
        for t in &mut s.times {
            count(t);
        }
        for p in &mut s.purchases {
            purchases.push(*p);
            count(&mut p.key);
        }
    }
    drop(by_person);
    prefix_sum(&mut ends);
    let mut times = vec![0; n];
    let mut emit = |key: Wide| {
        let next = &mut ends[key.person as usize];
        times[*next] = key.ts.wrapping_sub(origin) as u32;
        *next += 1;
    };
    for s in pieces {
        for t in s.times {
            emit(t);
        }
        for p in s.purchases {
            emit(p.key);
        }
    }
    let mut order: Vec<_> = (0..ends.len()).collect();
    order.sort_unstable_by_key(|&rank| names[rank]);
    let slices = order
        .into_iter()
        .map(move |rank| (rank, if rank == 0 { 0 } else { ends[rank - 1] }, ends[rank]));
    let sessions = Grouped::new(slices, times, max_offset).map(move |(rank, s)| {
        let person = names[rank];
        Session {
            first: Wide {
                person,
                ts: origin.wrapping_add(i64::from(s.first)),
            },
            last: Wide {
                person,
                ts: origin.wrapping_add(i64::from(s.last)),
            },
            count: s.count as usize,
        }
    });
    (sessions, purchases)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(mut times: Vec<u32>, window: u32) {
        let mut actual = Components::new(window);
        actual.fill(&mut times.clone());
        times.sort_unstable();
        let mut expected: Vec<Interval> = Vec::new();
        for t in times {
            if let Some(s) = expected.last_mut().filter(|s| t - s.last <= GAP) {
                s.last = t;
                s.count += 1;
            } else {
                expected.push(Interval::point(t));
            }
        }
        assert_eq!(actual.spans, expected);
    }

    #[test]
    fn bridges_both_neighbors_at_an_inclusive_gap() {
        let midnight = crate::DAY as u32;
        for points in [
            vec![midnight + GAP, midnight - GAP, midnight, midnight],
            vec![2 * GAP + 1, 0, GAP, GAP, 0],
            vec![u32::MAX, u32::MAX - 2 * GAP, u32::MAX - GAP, 0, 0],
            vec![],
        ] {
            check(points.iter().copied().rev().collect(), u32::MAX);
            check(points, u32::MAX);
        }
    }

    #[test]
    fn online_bins_and_sort_fallback_match_ordered_scan() {
        let mut state = 123u32;
        for width in [7 * GAP, 672 * GAP, u32::MAX] {
            for len in [4, 17, 40, 120, 300, 1500] {
                for _ in 0..32 {
                    let points: Vec<_> = (0..len)
                        .map(|_| {
                            state ^= state << 13;
                            state ^= state >> 17;
                            state ^= state << 5;
                            (state % width) & !1
                        })
                        .collect();
                    check(points, width);
                }
            }
        }
        check((0..80).rev().map(|i| i * (GAP + 1)).collect(), u32::MAX);
        check(
            (0..600).rev().map(|i| i * (GAP + 1)).chain([0, GAP]).collect(),
            672 * GAP,
        );
    }
}
