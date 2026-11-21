//! Sort and compress physical timelines before resolving identities. Owner
//! partitions receive descriptors; their row arrays stay in the raw shard.
use super::*;
use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::ops::Range;

struct Interval {
    start: i64,
    end: i64,
    events: usize,
    // Purchases are sorted by (raw user, time), so the previous interval's
    // end is the beginning of this one's list, even across raw-user boundaries.
    purchase_end: usize,
}

struct Run {
    events: Range<usize>,
    clicks: Range<usize>,
}

#[derive(Clone, Copy)]
struct RunRef {
    owner: u64,
    address: usize,
}

impl RunRef {
    fn new(owner: u64, source: usize, index: usize) -> Self {
        // Pack the shard into the low bits of a vector index. With 256
        // shards, an addressable index stores up to 2^56 runs on the target.
        Self {
            owner,
            address: index.checked_mul(NS).unwrap() + source,
        }
    }

    #[inline]
    fn get(self, raw: &[RawShard]) -> (&Run, &RawShard) {
        let shard = &raw[self.address % NS];
        (&shard.runs[self.address / NS], shard)
    }
}

struct RawShard {
    events: Vec<Interval>,
    purchases: Vec<Purchase>,
    clicks: Vec<PCl>,
    runs: Vec<Run>,
    standalone: Vec<RunRef>,
}

fn compress(mut timeline: impl Iterator<Item = (i64, bool)>, mut purchase_end: usize, out: &mut Vec<Interval>) {
    let Some((start, purchased)) = timeline.next() else {
        return;
    };
    let mut s = Interval {
        start,
        end: start,
        events: 1,
        purchase_end: 0,
    };
    purchase_end += usize::from(purchased);
    for (ts, purchased) in timeline {
        if ts.abs_diff(s.end) > GAP as u64 {
            s.purchase_end = purchase_end;
            out.push(s);
            s = Interval {
                start: ts,
                end: ts,
                events: 1,
                purchase_end: 0,
            };
        } else {
            s.end = ts;
            s.events += 1;
        }
        purchase_end += usize::from(purchased);
    }
    s.purchase_end = purchase_end;
    out.push(s);
}

#[inline]
fn take_run<T>(rows: &[T], start: &mut usize, person: u64, key: impl Fn(&T) -> u64) -> Range<usize> {
    let begin = *start;
    while *start < rows.len() && key(&rows[*start]) == person {
        *start += 1;
    }
    begin..*start
}

fn prepare_shard(
    source: usize,
    parts: Vec<Payload>,
    clicks: Vec<Vec<PCl>>,
    people: &People,
) -> (RawShard, Vec<Vec<RunRef>>) {
    let (times, purchases): (Vec<_>, Vec<_>) = parts.into_iter().map(|p| (p.times, p.purchases)).unzip();
    let mut times = gather(times);
    let mut purchases = gather(purchases);
    let mut clicks = gather(clicks);
    rayon::join(
        || times.par_sort_unstable(),
        || {
            rayon::join(
                || purchases.par_sort_unstable_by_key(|e| e.key),
                || clicks.par_sort_unstable_by_key(|c| (c.person, c.ts)),
            )
        },
    );
    let mut intervals = Vec::new();
    let mut linked: Vec<Vec<RunRef>> = (0..NS).map(|_| Vec::new()).collect();
    let mut standalone = Vec::new();
    let mut runs = Vec::new();
    let (mut e, mut p, mut c) = (0, 0, 0);
    let mut ev = times.get(e).map(|x| x.person);
    let mut pu = purchases.get(p).map(|x| x.key.person);
    let mut cl = clicks.get(c).map(|x| x.person);
    while ev.is_some() || pu.is_some() || cl.is_some() {
        let id = ev
            .unwrap_or(u64::MAX)
            .min(pu.unwrap_or(u64::MAX))
            .min(cl.unwrap_or(u64::MAX));
        let (mut er, mut pr, mut cr) = (e..e, p..p, c..c);
        if ev == Some(id) {
            er = take_run(&times, &mut e, id, |x| x.person);
            ev = times.get(e).map(|x| x.person);
        }
        if pu == Some(id) {
            pr = take_run(&purchases, &mut p, id, |x| x.key.person);
            pu = purchases.get(p).map(|x| x.key.person);
        }
        if cl == Some(id) {
            cr = take_run(&clicks, &mut c, id, |x| x.person);
            cl = clicks.get(c).map(|x| x.person);
        }
        // The links lookup happens exactly once for a physical ID that has
        // retained events and/or clicks, including linked click-only aliases.
        let owner = people.owner(id);
        let start = intervals.len();
        if pr.is_empty() {
            compress(times[er].iter().map(|x| (x.ts, false)), pr.start, &mut intervals);
        } else {
            let begin = pr.start;
            let mut normal = times[er].iter().peekable();
            let mut paid = purchases[pr].iter().peekable();
            compress(
                std::iter::from_fn(|| match (normal.peek(), paid.peek()) {
                    (Some(x), Some(y)) if x.ts <= y.key.ts => normal.next().map(|x| (x.ts, false)),
                    (_, Some(_)) => paid.next().map(|x| (x.key.ts, true)),
                    (_, None) => normal.next().map(|x| (x.ts, false)),
                }),
                begin,
                &mut intervals,
            );
        }
        if owner.is_none() && start == intervals.len() {
            continue; // unrelated click-only user
        }
        let run = RunRef::new(owner.unwrap_or(id), source, runs.len());
        runs.push(Run {
            events: start..intervals.len(),
            clicks: cr,
        });
        if let Some(owner) = owner {
            linked[shard(owner)].push(run);
        } else {
            standalone.push(run);
        }
    }
    (
        RawShard {
            events: intervals,
            purchases,
            clicks,
            runs,
            standalone,
        },
        linked,
    )
}

trait Time {
    fn time(&self) -> i64;
}

impl Time for Interval {
    #[inline]
    fn time(&self) -> i64 {
        self.start
    }
}

impl Time for PCl {
    #[inline]
    fn time(&self) -> i64 {
        self.ts
    }
}

fn click_slice<'a>(r: &Run, s: &'a RawShard) -> (Range<usize>, &'a [PCl]) {
    (r.clicks.clone(), &s.clicks)
}

struct Cursor<'a, T> {
    source: usize,
    index: usize,
    rows: &'a [T],
}

impl<'a, T> Cursor<'a, T> {
    #[inline]
    fn pop(&mut self) -> Option<(usize, usize, &'a T)> {
        let (row, rest) = self.rows.split_first()?;
        self.rows = rest;
        let index = self.index;
        self.index += 1;
        Some((self.source, index, row))
    }
}

// The heap only holds nonempty cursors. Equal timestamps require no stable
// order: the session union and the exact click tie-break are order independent.
impl<T: Time> Ord for Cursor<'_, T> {
    fn cmp(&self, other: &Self) -> Ordering {
        other.rows[0].time().cmp(&self.rows[0].time())
    }
}

impl<T: Time> PartialOrd for Cursor<'_, T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T: Time> PartialEq for Cursor<'_, T> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl<T: Time> Eq for Cursor<'_, T> {}

enum Merge<'a, T: Time> {
    Empty,
    One(Cursor<'a, T>),
    Two(Cursor<'a, T>, Cursor<'a, T>),
    Heap(BinaryHeap<Cursor<'a, T>>),
}

impl<'a, T: Time> Merge<'a, T> {
    fn new(
        runs: &[RunRef],
        raw: &'a [RawShard],
        select: impl Fn(&Run, &'a RawShard) -> (Range<usize>, &'a [T]),
    ) -> Self {
        let mut cursors = runs.iter().filter_map(|r| {
            let (run, shard) = r.get(raw);
            let (range, all) = select(run, shard);
            (!range.is_empty()).then(|| Cursor {
                source: r.address % NS,
                index: range.start,
                rows: &all[range],
            })
        });
        let Some(a) = cursors.next() else { return Self::Empty };
        let Some(b) = cursors.next() else { return Self::One(a) };
        let Some(c) = cursors.next() else {
            return Self::Two(a, b);
        };
        Self::Heap([a, b, c].into_iter().chain(cursors).collect::<Vec<_>>().into())
    }
}

impl<'a, T: Time> Iterator for Merge<'a, T> {
    type Item = (usize, usize, &'a T);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Empty => None,
            Self::One(a) => a.pop(),
            Self::Two(a, b) => match (a.rows.first(), b.rows.first()) {
                (Some(x), Some(y)) if x.time() <= y.time() => a.pop(),
                (_, Some(_)) => b.pop(),
                (_, None) => a.pop(),
            },
            Self::Heap(heap) => {
                let mut head = heap.peek_mut()?;
                let next = head.pop();
                if head.rows.is_empty() {
                    std::collections::binary_heap::PeekMut::pop(head);
                }
                next
            }
        }
    }
}

struct Aggregator {
    out: ShardOut,
    cnt: Vec<usize>,
    pos: Vec<u32>,
    active: Vec<u32>,
    catsum: Vec<i64>,
}

impl Aggregator {
    fn new(sm: &Small) -> Self {
        let ncamp = sm.camp_names.len();
        Self {
            out: ShardOut {
                users: Vec::new(),
                camps: Map::with_hasher(rs()),
            },
            cnt: vec![0; ncamp],
            pos: vec![0; ncamp],
            active: Vec::new(),
            catsum: vec![0; sm.cat_names.len()],
        }
    }

    #[inline]
    fn flush_day(&mut self, sm: &Small, person: u64, day: i32, [sessions, events, revenue]: [i64; 3]) {
        self.out.users.push(UserRow {
            person,
            day,
            sessions,
            events,
            revenue,
            top: if revenue == 0 {
                None
            } else {
                top_category(&self.catsum, &sm.cat_rank)
            },
        });
        if revenue != 0 {
            self.catsum.fill(0);
        }
    }

    #[inline]
    fn purchases(&mut self, shard: &RawShard, index: usize, s: &Interval) -> i64 {
        // Adjacent physical intervals consume adjacent ranges; alias merging
        // changes the order of ranges, never which purchases are in each.
        let begin = index.checked_sub(1).map_or(0, |prev| shard.events[prev].purchase_end);
        let mut rev = 0i64;
        for p in &shard.purchases[begin..s.purchase_end] {
            rev += i64::from(p.price);
            self.catsum[p.cat as usize] += i64::from(p.price);
        }
        rev
    }

    fn person(&mut self, runs: &[RunRef], raw: &[RawShard], sm: &Small, codec: &Codec) {
        if let [only] = runs {
            let (r, shard) = only.get(raw);
            let source = only.address % NS;
            let events = r
                .events
                .clone()
                .zip(&shard.events[r.events.clone()])
                .map(|(i, s)| (source, i, s));
            let clicks = shard.clicks[r.clicks.clone()].iter();
            self.aggregate(only.owner, events, [clicks.clone(), clicks], raw, sm, codec);
        } else {
            let events = Merge::new(runs, raw, |r, s| (r.events.clone(), &s.events));
            let clicks = || Merge::new(runs, raw, click_slice).map(|(_, _, c)| c);
            self.aggregate(runs[0].owner, events, [clicks(), clicks()], raw, sm, codec);
        }
    }

    fn aggregate<'a>(
        &mut self,
        person: u64,
        events: impl Iterator<Item = (usize, usize, &'a Interval)>,
        clicks: [impl Iterator<Item = &'a PCl>; 2],
        raw: &[RawShard],
        sm: &Small,
        codec: &Codec,
    ) {
        let mut events = events.peekable();
        if events.peek().is_none() {
            return;
        }
        let [mut enters, mut expires] = clicks.map(Iterator::peekable);
        let mut admitted = 0usize;
        let mut latest: Option<&PCl> = None;
        let mut cur_day = i32::MIN;
        let mut daily = [0; 3];
        while let Some((source, index, first)) = events.next() {
            let start = first.start;
            let day = start.div_euclid(DAY) as i32;
            if day != cur_day {
                if cur_day != i32::MIN {
                    self.flush_day(sm, person, cur_day, daily);
                }
                cur_day = day;
                daily = [0; 3];
            }
            let mut end = first.end;
            let mut count = first.events;
            let mut rev = self.purchases(&raw[source], index, first);
            // A physical interval has no internal gap > GAP. Two intervals
            // belong together iff the next start is at most GAP after the
            // current maximum end, including overlap and exact ties.
            while events
                .peek()
                .is_some_and(|(_, _, s)| s.start <= end.saturating_add(GAP))
            {
                let (source, index, s) = events.next().unwrap();
                end = end.max(s.end);
                count += s.events;
                rev += self.purchases(&raw[source], index, s);
            }
            daily[0] += 1;
            daily[1] += count as i64;
            daily[2] += rev;

            // The two cursors merge the same runs in the same order. When
            // they are aligned, skip expired unseen clicks in both.
            let lower = start.saturating_sub(WEEK);
            while expires.peek().is_some_and(|c| c.ts < lower) {
                let c = expires.next().unwrap();
                if admitted == 0 {
                    enters.next();
                    continue;
                }
                admitted -= 1;
                let camp = c.camp as usize;
                self.cnt[camp] -= 1;
                if self.cnt[camp] == 0 {
                    let p = self.pos[camp] as usize;
                    let last = self.active.pop().unwrap();
                    if last as usize != camp {
                        self.active[p] = last;
                        self.pos[last as usize] = p as u32;
                    }
                }
            }
            while enters.peek().is_some_and(|c| c.ts <= start) {
                let c = enters.next().unwrap();
                admitted += 1;
                if latest.is_none_or(|prev| c.ts != prev.ts || codec.less(c.cid, prev.cid)) {
                    latest = Some(c);
                }
                let camp = c.camp as usize;
                if self.cnt[camp] == 0 {
                    self.pos[camp] = self.active.len() as u32;
                    self.active.push(c.camp);
                }
                self.cnt[camp] += 1;
            }
            let credit = latest.filter(|c| c.ts >= lower).map_or(ORGANIC, |c| c.camp);
            let credited = self.out.camps.entry(ckey(credit, day)).or_insert([0; 4]);
            credited[0] += 1;
            credited[1] += i64::from(rev != 0);
            credited[2] += rev;
            for &a in &self.active {
                self.out.camps.entry(ckey(a, day)).or_insert([0; 4])[3] += 1;
            }
        }
        self.flush_day(sm, person, cur_day, daily);
        for a in self.active.drain(..) {
            self.cnt[a as usize] = 0;
        }
    }
}

pub(super) fn process(
    events: Vec<Vec<Payload>>,
    clicks: Vec<Vec<Vec<PCl>>>,
    sm: &Small,
    people: &People,
    log: &impl Fn(&str),
) -> Vec<ShardOut> {
    let raw_events = by_destination(events, |p| !p.times.is_empty() || !p.purchases.is_empty());
    let raw_clicks = by_destination(clicks, |p| !p.is_empty());
    let (raw, linked): (Vec<_>, Vec<_>) = raw_events
        .into_par_iter()
        .zip(raw_clicks)
        .enumerate()
        .map(|(source, (e, c))| prepare_shard(source, e, c, people))
        .unzip();
    log("raw timelines sorted; identities resolved");

    let mut jobs: Vec<_> = by_destination(linked, |p| !p.is_empty())
        .into_par_iter()
        .enumerate()
        .map(|(dest, parts)| {
            let weight = raw[dest]
                .standalone
                .iter()
                .chain(parts.iter().flatten())
                .map(|r| {
                    let (r, _) = r.get(&raw);
                    r.events.len() + r.clicks.len()
                })
                .sum::<usize>();
            (Reverse(weight), dest, parts)
        })
        .collect();
    jobs.sort_unstable_by_key(|job| job.0);
    jobs.into_par_iter()
        .map(|(_, dest, parts)| {
            let mut linked = gather(parts);
            linked.sort_unstable_by_key(|r| r.owner);
            let mut agg = Aggregator::new(sm);
            for r in &raw[dest].standalone {
                agg.person(std::slice::from_ref(r), &raw, sm, &people.codec);
            }
            for runs in linked.chunk_by(|a, b| a.owner == b.owner) {
                agg.person(runs, &raw, sm, &people.codec);
            }
            agg.out
        })
        .collect()
}
