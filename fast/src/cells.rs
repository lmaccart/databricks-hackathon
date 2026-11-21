//! Sessionize by coalescing instead of sorting every event (rules.md, Events
//! steps 5 and 6).
//!
//! Two of a person's events in the same epoch cell `[k·GAP, (k+1)·GAP)` are at
//! most GAP − 1 ms apart, so every gap between them is at most 30 minutes and
//! they share a session. No event of that person lies between two of their
//! nonempty cells that are consecutive in time, so those cells share a session
//! iff `later.first − earlier.last <= GAP`; cells further apart than adjacent
//! never do. A cell therefore only keeps its first and last `ts`, its event
//! count and its purchases, and sorting cells by (person, cell) reproduces each
//! session's start (hence its day and click window), event count and purchases
//! exactly, for any `ts` sign and across midnight. Equal timestamps always share
//! a cell. Typical sessions span one or two cells, so far fewer rows are sorted.
use super::{GAP, PEv};
use std::ops::Range;

const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, Default)]
struct Cell {
    person: u64,
    cell: i64, // ts.div_euclid(GAP)
    lo: u32,   // first ts − cell·GAP
    hi: u32,   // last ts − cell·GAP
    // Zero marks an unused slot. A shard's PEv rows outnumber its cells and
    // purchases, and 2³² rows of PEv would exceed any memory this runs in.
    events: u32,
    head: u32, // the cell's most recently added purchase, or NONE
}

impl Cell {
    #[inline]
    fn first(&self) -> i64 {
        self.cell * GAP + self.lo as i64
    }

    #[inline]
    fn last(&self) -> i64 {
        self.cell * GAP + self.hi as i64
    }
}

#[derive(Clone, Copy)]
struct Purchase {
    cat: u32,
    price: u32,
    next: u32,
}

pub(super) struct Session {
    pub person: u64,
    pub start: i64,
    pub events: i64,
    range: Range<usize>, // source cells, or source events in the wide fallback
}

/// Target events per partition: keep ordinary tables small and sort each
/// partition alone. Skewed partitions still grow without a correctness limit.
const PART: usize = 1024;
const BLANK: PEv = PEv { person: 0, ts: 0, price: 0, cat: 0 };

/// Reusable per worker. Between partitions every `table` slot is empty.
#[derive(Default)]
pub(super) struct Cells {
    rows: Vec<PEv>,      // the shard's events, grouped by partition
    starts: Vec<usize>,  // partition p is rows[starts[p]..starts[p + 1]]
    fill: Vec<usize>,
    table: Vec<Cell>,
    cells: Vec<Cell>,    // nonempty cells sorted by (person, cell)
    purchases: Vec<Purchase>,
    wide: bool,
}

#[inline]
fn hash(person: u64, cell: i64) -> u64 {
    (person ^ (cell as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)).wrapping_mul(0xD6E8_FEB8_6659_FD93)
}

/// Linear probing in a power-of-two table: the slot holding (person, cell),
/// or the empty slot where it belongs.
#[inline]
fn probe(table: &[Cell], person: u64, cell: i64) -> usize {
    let mask = table.len() - 1;
    let mut i = (hash(person, cell) >> (64 - table.len().trailing_zeros())) as usize;
    loop {
        let c = &table[i];
        if c.events == 0 || (c.person == person && c.cell == cell) {
            return i;
        }
        i = (i + 1) & mask;
    }
}

/// Move every occupied slot to `out`, leaving the table empty.
fn drain(table: &mut [Cell], out: &mut Vec<Cell>) {
    for s in table {
        if s.events != 0 {
            out.push(*s);
            *s = Cell::default();
        }
    }
}

impl Cells {
    /// Build sorted cells, or retain sorted wide events for a sparse shard.
    pub(super) fn build(&mut self, pieces: &[Vec<PEv>]) {
        let Cells { rows, starts, fill, table, cells, purchases, wide } = self;
        *wide = false;
        let n: usize = pieces.iter().map(Vec::len).sum();
        // Partition by the top bits of the person key, so partition order is
        // person order and a person never spans two partitions.
        let bits = (n / PART).max(1).next_power_of_two().trailing_zeros().min(16);
        let part = |person: u64| person.checked_shr(64 - bits).unwrap_or(0) as usize;
        starts.clear();
        starts.resize((1 << bits) + 1, 0);
        for e in pieces.iter().flatten() {
            starts[part(e.person) + 1] += 1;
        }
        for p in 1..starts.len() {
            starts[p] += starts[p - 1];
        }
        fill.clear();
        fill.extend_from_slice(&starts[..1 << bits]);
        rows.resize(n, BLANK);
        for e in pieces.iter().flatten() {
            let f = &mut fill[part(e.person)];
            rows[*f] = *e;
            *f += 1;
        }
        cells.clear();
        purchases.clear();
        for w in starts.windows(2) {
            if w[0] == w[1] {
                continue;
            }
            let first = cells.len();
            // Small partitions fit every event in its own cell; large ones grow.
            let mut size = (w[1] - w[0]).next_power_of_two().clamp(16, 4096);
            if table.len() < size {
                table.resize(size, Cell::default());
            }
            let mut len = 0usize;
            for e in &rows[w[0]..w[1]] {
                let cell = e.ts.div_euclid(GAP);
                let off = (e.ts - cell * GAP) as u32;
                let i = probe(&table[..size], e.person, cell);
                let c = &mut table[i];
                if c.events == 0 {
                    *c = Cell { person: e.person, cell, lo: off, hi: off, events: 0, head: NONE };
                    len += 1;
                }
                c.lo = c.lo.min(off);
                c.hi = c.hi.max(off);
                c.events += 1;
                if e.price != 0 {
                    purchases.push(Purchase { cat: e.cat, price: e.price, next: c.head });
                    c.head = (purchases.len() - 1) as u32;
                }
                // Heuristic: require at least two events per cell to justify
                // sorting wider records. The count is exact, not a sample or
                // a property of the input window; both paths give equal output.
                if first + len > n / 2 {
                    table[..size].fill(Cell::default());
                    cells.clear();
                    purchases.clear();
                    rows.sort_unstable_by_key(|e| (e.person, e.ts));
                    *wide = true;
                    return;
                }
                if 2 * len > size {
                    drain(&mut table[..size], cells);
                    size *= 2;
                    if table.len() < size {
                        table.resize(size, Cell::default());
                    }
                    for c in cells.drain(first..) {
                        let i = probe(&table[..size], c.person, c.cell);
                        table[i] = c;
                    }
                }
            }
            drain(&mut table[..size], cells);
            cells[first..].sort_unstable_by_key(|c| (c.person, c.cell));
        }
    }

    /// Sessions in (person, start) order; valid after `build`.
    pub(super) fn sessions(&self) -> impl Iterator<Item = Session> + '_ {
        let cs = &self.cells[..];
        let mut k = 0;
        std::iter::from_fn(move || {
            if self.wide {
                let head = self.rows.get(k)?;
                let mut m = k + 1;
                while m < self.rows.len()
                    && self.rows[m].person == head.person
                    && self.rows[m].ts - self.rows[m - 1].ts <= GAP
                {
                    m += 1;
                }
                let s = Session {
                    person: head.person, start: head.ts, events: (m - k) as i64, range: k..m,
                };
                k = m;
                return Some(s);
            }
            let head = cs.get(k)?;
            let mut events = head.events as i64;
            let mut m = k + 1;
            while m < cs.len() && cs[m].person == head.person && cs[m].first() - cs[m - 1].last() <= GAP {
                events += cs[m].events as i64;
                m += 1;
            }
            let s = Session { person: head.person, start: head.first(), events, range: k..m };
            k = m;
            Some(s)
        })
    }

    /// (category, price) of every purchase in a session, in no particular order.
    pub(super) fn purchases(&self, s: &Session) -> impl Iterator<Item = (u32, u32)> + '_ {
        let mut range = s.range.clone();
        let mut p = NONE;
        std::iter::from_fn(move || {
            if self.wide {
                for i in range.by_ref() {
                    let e = self.rows[i];
                    if e.price != 0 {
                        return Some((e.cat, e.price));
                    }
                }
                return None;
            }
            while p == NONE {
                p = self.cells[range.next()?].head;
            }
            let x = self.purchases[p as usize];
            p = x.next;
            Some((x.cat, x.price))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Got = (u64, i64, i64, Vec<(u32, u32)>);

    fn naive(pieces: &[Vec<PEv>]) -> Vec<Got> {
        let mut evs: Vec<PEv> = pieces.iter().flatten().copied().collect();
        evs.sort_by_key(|e| (e.person, e.ts));
        let mut out: Vec<Got> = Vec::new();
        for (i, e) in evs.iter().enumerate() {
            if i == 0 || evs[i - 1].person != e.person || e.ts - evs[i - 1].ts > GAP {
                out.push((e.person, e.ts, 0, Vec::new()));
            }
            let s = out.last_mut().unwrap();
            s.2 += 1;
            if e.price != 0 {
                s.3.push((e.cat, e.price));
            }
        }
        out.iter_mut().for_each(|s| s.3.sort());
        out
    }

    fn coalesced(cells: &mut Cells, pieces: &[Vec<PEv>]) -> Vec<Got> {
        cells.build(pieces);
        cells
            .sessions()
            .map(|s| {
                let mut p: Vec<_> = cells.purchases(&s).collect();
                p.sort();
                (s.person, s.start, s.events, p)
            })
            .collect()
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            // splitmix64
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[test]
    fn matches_sorting_every_event() {
        let mut rng = Rng(7);
        let mut cells = Cells::default();
        let edges = [0, 1, 2, u64::MAX, 0x8000_0000_0000_0000, 0x1234_5678_9abc_def0];
        for round in 0..3000 {
            // Some rounds span many partitions, with heavy edge-key people.
            let big = round % 100 == 0;
            let people: Vec<u64> = (0..if big { 3000 } else { 0 }).map(|_| rng.next()).collect();
            // Cells straddle the epoch and midnight; offsets favour cell edges
            // and exact 30:00.000 / 30:00.001 gaps.
            let base = (rng.below(9) as i64 - 4) * crate::DAY;
            let spread = 1 + rng.below(12) as i64;
            let n = rng.below(if big { 40_000 } else if round % 10 == 0 { 2000 } else { 120 }) as usize;
            let mut pieces: Vec<Vec<PEv>> = vec![Vec::new(); 1 + rng.below(4) as usize];
            for _ in 0..n {
                let cell = rng.below(2 * spread as u64) as i64 - spread;
                let off = match rng.below(6) {
                    0 => 0,
                    1 => 1,
                    2 => GAP - 1,
                    3 => GAP / 2,
                    _ => rng.below(GAP as u64) as i64,
                };
                let purchase = rng.below(4) == 0;
                let e = PEv {
                    person: if people.is_empty() || rng.below(4) == 0 {
                        edges[rng.below(6) as usize]
                    } else {
                        people[rng.below(people.len() as u64) as usize]
                    },
                    ts: base + cell * GAP + off,
                    price: if purchase { 1 + rng.below(10_000_000) as u32 } else { 0 },
                    cat: if purchase { rng.below(5) as u32 } else { 0 },
                };
                let p = rng.below(pieces.len() as u64) as usize;
                pieces[p].push(e);
                if rng.below(8) == 0 {
                    pieces[p].push(e); // equal timestamps share a session
                }
            }
            assert_eq!(coalesced(&mut cells, &pieces), naive(&pieces), "round {round}");
        }
    }

    #[test]
    fn far_cells_and_extreme_timestamps() {
        let ev = |person, ts| PEv { person, ts, price: 0, cat: 0 };
        let pieces = vec![vec![
            ev(1, i64::MIN / 2),
            ev(1, i64::MIN / 2 + GAP),
            ev(1, i64::MAX / 2),
            ev(2, -1),
            ev(2, GAP - 1),
            ev(2, -GAP - 1),
        ]];
        let mut cells = Cells::default();
        assert_eq!(coalesced(&mut cells, &pieces), naive(&pieces));
    }

    #[test]
    fn sparse_dense_and_empty_shards_reuse_storage_without_losing_purchases() {
        let sparse = vec![(0..5000)
            .rev()
            .map(|i| PEv {
                person: u64::MAX, ts: (i - 2500) * 2 * GAP,
                price: (i % 100 + 1) as u32, cat: (i % 7) as u32,
            })
            .collect::<Vec<_>>()];
        let dense = vec![sparse[0].clone(); 4];
        let empty = vec![Vec::new()];
        let mut cells = Cells::default();
        for pieces in [&dense, &sparse, &empty, &dense, &sparse, &dense] {
            assert_eq!(coalesced(&mut cells, pieces), naive(pieces));
        }
    }
}
