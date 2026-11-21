//! Shard-local two-dimensional radix partition for exact session boundaries.
use super::stream::{Purchase, Temporal, Wide};
use super::{Clicks, GAP, Map, NS, PCl, Packing, Payload, People, SHARD_SHIFT, ShardOut, Small, process_sessions};

const STANDALONE: u64 = 1 << SHARD_SHIFT;
const SHARD_MASK: u64 = (NS as u64 - 1) << SHARD_SHIFT;
const _: () = assert!(NS.is_power_of_two() && NS > 1 && SHARD_SHIFT >= u32::BITS);
// A full fine cell spans at most GAP, so its timestamps need no ordering.
const FINE_SHIFT: u32 = (GAP as u64).ilog2();

fn standalone(person: u64) -> u64 {
    // The shard bits are already known to the caller. Borrow one to tag
    // standalone tokens; the other 56 bits still distinguish all names here.
    (person & !SHARD_MASK) | STANDALONE
}

pub(super) struct Grid {
    origin: i64,
    rows: usize,
    pbits: u32,
    rank_shift: u32,
    tbits: u32,
    time_shift: u32,
}

impl Grid {
    pub(super) fn new((ws, we): (i64, i64), ranks: usize, rows: usize) -> Option<Self> {
        let span = u32::try_from(we as i128 - ws as i128 - 1).ok()?;
        if !(128..=u32::MAX as usize).contains(&rows) {
            return None;
        }
        // At most 16 KiB of counters and never more than one per eight rows.
        let budget = (rows / 8).ilog2().min(12);
        let time_bits = u32::BITS - span.leading_zeros();
        let tbits = (budget / 3).min(time_bits.saturating_sub(FINE_SHIFT));
        // Linked ranks and standalone tokens occupy disjoint prefix ranges.
        let pbits = budget - tbits - 1;
        let rank_bits = usize::BITS - ranks.saturating_sub(1).leading_zeros();
        Some(Self {
            origin: ws,
            rows,
            pbits,
            rank_shift: rank_bits.saturating_sub(pbits),
            tbits,
            time_shift: time_bits - tbits,
        })
    }

    fn cells(&self) -> usize {
        1 << (1 + self.pbits + self.tbits)
    }

    #[inline]
    fn index(&self, person: u64, offset: u32) -> usize {
        let prefix = if person < STANDALONE {
            person >> self.rank_shift
        } else {
            (1 << self.pbits) | (person >> (64 - self.pbits))
        };
        ((prefix as usize) << self.tbits) | (u64::from(offset) >> self.time_shift) as usize
    }

    #[inline]
    fn offset(&self, ts: i64) -> u32 {
        (ts as u64).wrapping_sub(self.origin as u64) as u32
    }

    fn key(&self, person: u64, offset: u32) -> Wide {
        Wide {
            person,
            ts: self.origin + i64::from(offset),
        }
    }
}

#[derive(Clone, Copy)]
struct Event {
    person: u64,
    offset: u32,
}

#[derive(Clone, Copy, Default)]
struct Run {
    lo: u32,
    hi: u32,
    count: u32,
}

impl Run {
    fn single(t: u32) -> Self {
        Self { lo: t, hi: t, count: 1 }
    }

    fn include(&mut self, t: u32) {
        self.lo = self.lo.min(t);
        self.hi = self.hi.max(t);
        self.count += 1;
    }
}

// At most one lookup per exact group in a tile, none for the same owner
// as last time (the common case for a dense prefix). Reused for each prefix.
#[derive(Default)]
struct Repair {
    by_person: Map<u64, usize>,
    owners: Vec<Owner>,
    nodes: Vec<Node>,
    last_owner: usize,
}

struct Owner {
    person: u64,
    head: u32,
    tail: u32,
}

struct Node {
    run: Run,
    next: u32,
}

#[derive(Clone, Copy)]
struct Session {
    person: u64,
    run: Run,
}

impl Repair {
    fn owner(&mut self, person: u64) -> usize {
        if self.owners.get(self.last_owner).is_some_and(|o| o.person == person) {
            return self.last_owner;
        }
        let n = self.owners.len();
        let i = if n < 5 {
            self.owners.iter().position(|o| o.person == person).unwrap_or(n)
        } else {
            if self.by_person.is_empty() {
                self.by_person
                    .extend(self.owners.iter().enumerate().map(|(i, o)| (o.person, i)));
            }
            *self.by_person.entry(person).or_insert(n)
        };
        if i == n {
            self.owners.push(Owner {
                person,
                head: u32::MAX,
                tail: u32::MAX,
            });
        }
        self.last_owner = i;
        i
    }

    fn append(&mut self, i: usize, run: Run) {
        let o = &mut self.owners[i];
        let next = self.nodes.len() as u32;
        if let Some(node) = self.nodes.get_mut(o.tail as usize) {
            debug_assert!(node.run.hi <= run.lo);
            if run.lo - node.run.hi <= GAP as u32 {
                node.run.hi = run.hi;
                node.run.count += run.count;
                return;
            }
            node.next = next;
        } else {
            o.head = next;
        }
        o.tail = next;
        self.nodes.push(Node { run, next: u32::MAX });
    }

    fn drain(&mut self, out: &mut Vec<Session>) {
        self.owners.sort_unstable_by_key(|o| o.person);
        for o in self.owners.drain(..) {
            let mut pos = o.head;
            while let Some(n) = self.nodes.get(pos as usize) {
                out.push(Session {
                    person: o.person,
                    run: n.run,
                });
                pos = n.next;
            }
        }
        self.nodes.clear();
        self.by_person.clear();
        self.last_owner = 0;
    }
}

#[derive(Default)]
struct Fine {
    slots: Vec<Run>,
    live: Vec<u64>,
}

impl Fine {
    fn group(&mut self, rows: &[Event], run: Run, repair: &mut Repair) {
        let owner = repair.owner(rows[0].person);
        if run.hi - run.lo <= GAP as u32 {
            repair.append(owner, run);
            return;
        }
        if rows.len() <= 8 {
            // Tiny sparse groups: coalesce equal fine buckets before ordering.
            let mut slots = [Run::default(); 8];
            let mut used = 0;
            for e in rows {
                let key = e.offset >> FINE_SHIFT;
                if let Some(s) = slots[..used].iter_mut().find(|s| s.lo >> FINE_SHIFT == key) {
                    s.include(e.offset);
                } else {
                    slots[used] = Run::single(e.offset);
                    used += 1;
                }
            }
            slots[..used].sort_unstable_by_key(|s| s.lo);
            for &s in &slots[..used] {
                repair.append(owner, s);
            }
            return;
        }
        let base = run.lo >> FINE_SHIFT;
        let len = ((run.hi >> FINE_SHIFT) - base) as usize + 1;
        let words = len.div_ceil(64);
        if self.slots.len() < len {
            self.slots.resize(len, Run::default());
            self.live.resize(words, 0);
        }
        for e in rows {
            let i = ((e.offset >> FINE_SHIFT) - base) as usize;
            let bit = 1 << (i % 64);
            let w = &mut self.live[i / 64];
            let slot = &mut self.slots[i];
            if *w & bit == 0 {
                *w |= bit;
                *slot = Run::single(e.offset);
            } else {
                slot.include(e.offset);
            }
        }
        for (w, live) in self.live[..words].iter_mut().enumerate() {
            let mut bits = std::mem::take(live);
            while bits != 0 {
                let i = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                repair.append(owner, self.slots[w * 64 + i]);
            }
        }
    }
}

fn visit(g: &Grid, piece: &Payload, pack: Option<Packing>, mut row: impl FnMut(Event, u32, u32)) {
    if let Some(p) = pack {
        for &key in &piece.packed.times {
            row(
                Event {
                    person: key.person(p),
                    offset: p.offset(key) as u32,
                },
                0,
                0,
            );
        }
        for e in &piece.packed.purchases {
            row(
                Event {
                    person: e.key.person(p),
                    offset: p.offset(e.key) as u32,
                },
                e.price,
                e.cat,
            );
        }
    }
    for e in &piece.wide.times {
        row(
            Event {
                person: standalone(e.person),
                offset: g.offset(e.ts),
            },
            0,
            0,
        );
    }
    for e in &piece.wide.purchases {
        row(
            Event {
                person: standalone(e.key.person),
                offset: g.offset(e.key.ts),
            },
            e.price,
            e.cat,
        );
    }
}

fn partition(g: &Grid, pieces: Vec<Payload>, pack: Option<Packing>) -> (Vec<Event>, Vec<u32>, Vec<Purchase<Wide>>) {
    let mut hist = vec![0u32; g.cells()];
    let mut np = 0;
    for piece in &pieces {
        np += piece.packed.purchases.len() + piece.wide.purchases.len();
        visit(g, piece, pack, |e, _, _| hist[g.index(e.person, e.offset)] += 1);
    }
    let mut sum = 0;
    for end in &mut hist {
        sum += *end;
        *end = sum;
    }
    assert_eq!(sum as usize, g.rows);
    let mut events = Vec::with_capacity(g.rows);
    let dst = events.spare_capacity_mut();
    let mut purchases = Vec::with_capacity(np);
    for piece in pieces {
        visit(g, &piece, pack, |e, price, cat| {
            let cursor = &mut hist[g.index(e.person, e.offset)];
            *cursor -= 1;
            dst[*cursor as usize].write(e);
            if price != 0 {
                purchases.push(Purchase {
                    key: g.key(e.person, e.offset),
                    price,
                    cat,
                });
            }
        });
    }
    // Each cell's exclusive end was decremented once for each event counted
    // by the identical visit/index, so every disjoint interval is initialized.
    unsafe { events.set_len(g.rows) };
    (events, hist, purchases)
}

fn resolve_cell(rows: &mut [Event], fine: &mut Fine, repair: &mut Repair) {
    let Some(first) = rows.first().copied() else { return };
    let mut run = Run::single(first.offset);
    let mut collision = 0;
    for e in &rows[1..] {
        collision |= e.person ^ first.person;
        run.include(e.offset);
    }
    if collision == 0 {
        fine.group(rows, run, repair);
    } else if rows.len() > 128 {
        // Split a skewed cell on its highest differing exact bit, keeping
        // the comparison sort in-cache regardless of its population.
        let bit = 1 << collision.ilog2();
        let (mut lo, mut hi) = (0, rows.len());
        while lo < hi {
            while lo < hi && rows[lo].person & bit == 0 {
                lo += 1;
            }
            while lo < hi && rows[hi - 1].person & bit != 0 {
                hi -= 1;
            }
            if lo < hi {
                hi -= 1;
                rows.swap(lo, hi);
                lo += 1;
            }
        }
        let (low, high) = rows.split_at_mut(lo);
        resolve_cell(low, fine, repair);
        resolve_cell(high, fine, repair);
    } else {
        // Offset order inside an exact group is irrelevant to its fine cells.
        rows.sort_unstable_by_key(|e| e.person);
        for group in rows.chunk_by(|a, b| a.person == b.person) {
            let mut run = Run::single(group[0].offset);
            for e in &group[1..] {
                run.include(e.offset);
            }
            fine.group(group, run, repair);
        }
    }
}

fn sessionize<'a>(g: &'a Grid, events: &'a mut [Event], starts: &'a [u32]) -> impl Iterator<Item = Session> + 'a {
    let mut sessions = Vec::new();
    let mut repair = Repair::default();
    let mut fine = Fine::default();
    let mut prefixes = (0..starts.len()).step_by(1 << g.tbits);
    let mut next = 0;
    std::iter::from_fn(move || {
        loop {
            if let Some(s) = sessions.get(next).copied() {
                next += 1;
                return Some(s);
            }
            let prefix = prefixes.next()?;
            if starts[prefix] as usize
                == starts
                    .get(prefix + (1 << g.tbits))
                    .map_or(events.len(), |&x| x as usize)
            {
                continue;
            }
            sessions.clear();
            next = 0;
            for cell in prefix..prefix + (1 << g.tbits) {
                let lo = starts[cell] as usize;
                let hi = starts.get(cell + 1).map_or(events.len(), |&x| x as usize);
                resolve_cell(&mut events[lo..hi], &mut fine, &mut repair);
            }
            repair.drain(&mut sessions);
        }
    })
}

pub(super) fn process(
    g: Grid,
    shard: usize,
    (evs, cls): (Vec<Payload>, Vec<Clicks>),
    pack: Option<Packing>,
    sm: &Small,
    people: &People,
    out: &mut ShardOut,
) {
    let (mut events, starts, purchases) = partition(&g, evs, pack);
    let mut clicks = Vec::with_capacity(cls.iter().map(|c| c.packed.len() + c.wide.len()).sum());
    for c in cls {
        clicks.extend(c.packed);
        clicks.extend(c.wide.into_iter().map(|c| PCl {
            person: standalone(c.person),
            ..c
        }));
    }
    process_sessions(
        sessionize(&g, &mut events, &starts)
            .map(|Session { person, run }| (g.key(person, run.lo), g.key(person, run.hi), run.count as usize)),
        purchases,
        clicks,
        sm,
        people,
        ((), |person| {
            if person < STANDALONE {
                people.original(person as u32)
            } else {
                (person & !SHARD_MASK) | ((shard as u64) << SHARD_SHIFT)
            }
        }),
        out,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(grid: &Grid, pieces: Vec<Payload>, pack: Option<Packing>) -> Vec<(u64, u32, u32, u32)> {
        let mut exact = Vec::new();
        for piece in &pieces {
            visit(grid, piece, pack, |e, _, _| exact.push((e.person, e.offset)));
        }
        exact.sort_unstable();
        let mut want: Vec<(u64, u32, u32, u32)> = Vec::new();
        for (person, ts) in exact {
            match want.last_mut() {
                Some((p, _, end, count)) if *p == person && ts - *end <= GAP as u32 => {
                    *end = ts;
                    *count += 1;
                }
                _ => want.push((person, ts, ts, 1)),
            }
        }
        let (mut rows, bounds, _) = partition(grid, pieces, pack);
        let got: Vec<_> = sessionize(grid, &mut rows, &bounds)
            .map(|s| (s.person, s.run.lo, s.run.hi, s.run.count))
            .collect();
        assert_eq!(got, want);
        got
    }

    #[test]
    fn true_collision_with_interleaved_people_never_repairs_the_wrong_tail() {
        let n = 384;
        let ws = -12_345;
        let we = ws + (1 << 27);
        let g = Grid::new((ws, we), 16_384, n).unwrap();
        let pack = Packing::new(ws, we, 16_384).unwrap();
        let edge = 1 << g.time_shift;
        let a = standalone(0xabcd_d024_0000_0001);
        let b = standalone(0xabcd_d024_0000_0002);
        assert_eq!(g.index(a, edge), g.index(b, edge));
        assert_ne!(a, b);
        assert_eq!(g.index(4, edge), g.index(7, edge));
        assert_ne!(g.index(4, edge), g.index(a, edge));
        let mut first = Payload::default();
        let mut second = Payload::default();
        for i in (0..96).rev() {
            // Both types share coarse prefixes and overlap in time. Only a
            // and rank 4 bridge the first coarse boundary at exactly GAP.
            let lag = match i % 6 {
                0 => edge - 1,
                1 => edge - (1 << FINE_SHIFT) - 1,
                2 => edge - (1 << (FINE_SHIFT + 1)) - 1,
                3 => edge - (1 << (FINE_SHIFT + 2)) - 1,
                _ => 1,
            };
            let next = edge - 1 + GAP as u32;
            first.wide.times.push(Wide {
                person: a,
                ts: ws + i64::from(lag),
            });
            first.packed.times.push(pack.encode(4, ws + i64::from(lag)));
            second.wide.times.push(Wide {
                person: b,
                ts: ws + i64::from(if i % 2 == 0 { edge - 1 } else { next + 1 }),
            });
            second
                .packed
                .times
                .push(pack.encode(7, ws + i64::from(if i % 2 == 0 { edge - 1 } else { next + 1 })));
        }
        first.wide.times[0].ts = ws + i64::from(edge - 1 + GAP as u32);
        first.packed.times[0] = pack.encode(4, first.wide.times[0].ts);
        let got = check(&g, vec![second, first], Some(pack));
        assert!(got.iter().any(|&(p, lo, hi, _)| p == a && lo < edge && hi >= edge));
        assert!(!got.iter().any(|&(p, lo, hi, _)| p == b && lo < edge && hi >= edge));
    }

    #[test]
    fn offset_limits_and_histogram_budget() {
        assert!(Grid::new((0, (1i64 << 32) + 1), 1, 128).is_none());
        assert!(Grid::new((i64::MIN + 1, i64::MAX), 4, 1024).is_none());
        assert!(Grid::new((-20, 11), 4, 127).is_none());
        let g = Grid::new((-20, (1i64 << 32) - 20), 0, 256).unwrap();
        assert!(g.cells() <= g.rows / 8);
        let mut piece = Payload::default();
        let p = 0xffff_ffc1_4000_0009;
        for i in 0..256 {
            piece.wide.times.push(Wide {
                person: p,
                ts: if i % 2 == 0 { (1i64 << 32) - 21 } else { -20 },
            });
        }
        let got = check(&g, vec![piece], None);
        assert_eq!(
            got,
            [(standalone(p), 0, 0, 128), (standalone(p), u32::MAX, u32::MAX, 128)]
        );
    }
}
