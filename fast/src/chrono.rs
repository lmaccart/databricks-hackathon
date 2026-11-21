//! Owner-local time radix. There is at most one new session per person in a
//! bin no wider than GAP. Clicks are unordered inside it: classify them
//! against that person's actual start, not the bin boundary.
use super::{DAY, GAP, Map, ORGANIC, PCl, People, ShardOut, Small, Stream, Temporal, UserRow, WEEK, ckey, rs};

const WIDTH: u32 = GAP as u32;
const NONE: u32 = u32::MAX;
// Limit transient allocations per live worker independently of input skew.
const MAX_ROWS: usize = 4 << 20;
const MAX_PEOPLE: usize = 128 << 10;
const MAX_CELLS: usize = 128 << 10;

struct Radix<T> {
    buckets: Vec<Vec<T>>,
}

impl<T> Radix<T> {
    fn new(n: usize) -> Self {
        Self {
            buckets: (0..n).map(|_| Vec::new()).collect(),
        }
    }

    #[inline]
    fn push(&mut self, ts: u32, row: T) {
        self.buckets[(ts / WIDTH) as usize].push(row);
    }
}

#[derive(Clone, Copy)]
struct Purchase {
    id: u32,
    ts: u32,
    cat: u32,
    price: u32,
}

#[derive(Clone, Copy)]
struct Click {
    cid: u64,
    id: u32,
    ts: u32,
    camp: u32,
}

#[derive(Default, Clone, Copy)]
struct Summary {
    first: u32,
    last: u32,
    count: u32,
}

impl Summary {
    #[inline]
    fn add(&mut self, ts: u32) -> bool {
        let first = self.count == 0;
        if first {
            self.first = ts;
            self.last = ts;
        } else {
            self.first = self.first.min(ts);
            self.last = self.last.max(ts);
        }
        self.count += 1;
        first
    }
}

#[derive(Clone, Copy)]
struct Latest {
    cid: u64,
    ts: u32,
    camp: u32,
}

struct State {
    end: u32,
    day: i32,
    sessions: u32,
    events: u32,
    revenue: i64,
    credit_cell: u32,
    purchased: bool,
}

struct Window {
    latest: Latest,
    active: u32,
    active_len: u32,
    indexed: bool,
    multi: bool,
}

impl State {
    fn new() -> Self {
        Self {
            end: 0,
            day: i32::MIN,
            sessions: 0,
            events: 0,
            revenue: 0,
            credit_cell: 0,
            purchased: false,
        }
    }

    #[inline]
    fn opens(&self, first: u32) -> bool {
        self.sessions == 0 || first - self.end > GAP as u32
    }

    fn flush(&mut self, name: u64, users: &mut Vec<UserRow>, ranks: &[u32], cats: &mut Vec<(u32, i64)>) {
        if self.sessions == 0 {
            return;
        }
        let top = cats
            .iter()
            .max_by_key(|&&(cat, cents)| (cents, std::cmp::Reverse(ranks[cat as usize])))
            .map(|&(c, _)| c);
        users.push(UserRow {
            person: name,
            day: self.day,
            sessions: i64::from(self.sessions),
            events: i64::from(self.events),
            revenue: self.revenue,
            top,
        });
        self.sessions = 0;
        self.events = 0;
        self.revenue = 0;
        cats.clear();
    }
}

impl Window {
    fn new() -> Self {
        Self {
            latest: Latest {
                cid: 0,
                ts: 0,
                camp: ORGANIC,
            },
            active: NONE,
            active_len: 0,
            indexed: false,
            multi: false,
        }
    }
}

#[derive(Clone, Copy)]
struct Pair {
    last: u32,
    camp: u32,
    next: u32,
}

// For assists, latest time per campaign is sufficient: the campaign exists
// in the inclusive window iff its latest applied click is not too old.
// The small lists avoid hashing for ordinary people. Promote only the
// high-cardinality people, never allocate a person×campaign matrix.
struct Active {
    pairs: Vec<Pair>,
    index: Map<u64, u32>,
    free: u32,
}

impl Active {
    fn new() -> Self {
        Self {
            pairs: Vec::new(),
            index: Map::with_hasher(rs()),
            free: NONE,
        }
    }

    fn apply(&mut self, c: Click, s: &mut Window, people: &People) {
        let prev = s.latest;
        if s.latest.camp == ORGANIC
            || c.ts > s.latest.ts
            || (c.ts == s.latest.ts && people.codec.less(c.cid, s.latest.cid))
        {
            s.latest = Latest {
                cid: c.cid,
                ts: c.ts,
                camp: c.camp,
            };
        }
        if !s.multi {
            if prev.camp == ORGANIC || prev.camp == c.camp {
                return;
            }
            // A person with just one campaign needs no sparse slot: its
            // latest credited click is also that campaign's latest click.
            s.multi = true;
            self.push_pair(s, prev.camp, prev.ts);
        }

        let key = |camp| (u64::from(c.id) << 32) | u64::from(camp);
        if s.indexed {
            if let Some(&i) = self.index.get(&key(c.camp)) {
                let p = &mut self.pairs[i as usize];
                p.last = p.last.max(c.ts);
                return;
            }
        } else {
            let mut i = s.active;
            while i != NONE {
                let p = &mut self.pairs[i as usize];
                if p.camp == c.camp {
                    p.last = p.last.max(c.ts);
                    return;
                }
                i = p.next;
            }
            if s.active_len >= 8 {
                s.indexed = true;
                let mut i = s.active;
                while i != NONE {
                    let p = self.pairs[i as usize];
                    self.index.insert(key(p.camp), i);
                    i = p.next;
                }
            }
        }

        let i = self.push_pair(s, c.camp, c.ts);
        if s.indexed {
            self.index.insert(key(c.camp), i);
        }
    }

    fn push_pair(&mut self, s: &mut Window, camp: u32, last: u32) -> u32 {
        let p = Pair {
            last,
            camp,
            next: s.active,
        };
        let i = if self.free == NONE {
            let i = self.pairs.len() as u32;
            self.pairs.push(p);
            i
        } else {
            let i = self.free;
            self.free = self.pairs[i as usize].next;
            self.pairs[i as usize] = p;
            i
        };
        s.active = i;
        s.active_len += 1;
        i
    }

    fn assist(&mut self, id: u32, s: &mut Window, lo: u32, day: &mut [[i64; 4]]) {
        if !s.multi {
            if s.latest.camp != ORGANIC && s.latest.ts >= lo {
                day[s.latest.camp as usize][3] += 1;
            }
            return;
        }
        let mut i = s.active;
        let mut prev = NONE;
        while i != NONE {
            let p = self.pairs[i as usize];
            if p.last >= lo {
                day[p.camp as usize][3] += 1;
                prev = i;
            } else {
                // Expire lazily at the next query, including promoted keys.
                if prev == NONE {
                    s.active = p.next;
                } else {
                    self.pairs[prev as usize].next = p.next;
                }
                s.active_len -= 1;
                if s.indexed {
                    self.index.remove(&((u64::from(id) << 32) | u64::from(p.camp)));
                }
                self.pairs[i as usize].next = self.free;
                self.free = i;
            }
            i = p.next;
        }
    }
}

#[inline]
fn summarize(summaries: &mut [Summary], touched: &mut Vec<u32>, id: u32, ts: u32) {
    if summaries[id as usize].add(ts) {
        touched.push(id);
    }
}

/// Returns false before emitting any results when the bounded time-major
/// representation is unsuitable. The caller still owns all the source rows.
pub(super) fn process<T: Temporal>(
    pieces: &[Stream<T>],
    clicks: &[Vec<PCl>],
    sm: &Small,
    people: &People,
    layout: T::Layout,
    (ws, we): (i64, i64),
    out: &mut ShardOut,
) -> bool {
    let ne = pieces.iter().map(|s| s.times.len() + s.purchases.len()).sum::<usize>();
    if ne == 0 {
        return true;
    }
    let nc = clicks.iter().map(Vec::len).sum::<usize>();
    let origin = ws.saturating_sub(WEEK);
    let Some(span) = we.checked_sub(origin).and_then(|v| u32::try_from(v).ok()) else {
        return false;
    };
    if span == 0 || we <= ws || ne + nc > MAX_ROWS || sm.cat_names.len() > 256 {
        return false;
    }
    let day0 = ws.div_euclid(DAY);
    let day_count = ((we - 1).div_euclid(DAY) - day0 + 1) as usize;
    let columns = sm.camp_names.len() + 1;
    let Some(cells) = day_count.checked_mul(columns).filter(|&n| n <= MAX_CELLS) else {
        return false;
    };
    // A Date32 fallback has the same output semantics as the existing path.
    if day0 <= i32::MIN as i64 || day0 + day_count as i64 - 1 > i32::MAX as i64 {
        return false;
    }
    let bins = span.div_ceil(WIDTH) as usize;
    let offset = |ts: i64| (ts - origin) as u32;
    let direct = T::dense_names(layout, people);
    if direct.is_some_and(|n| n.len() > MAX_PEOPLE) {
        return false;
    }
    // If a shard has a dominant linked person, summarize its views/carts
    // during scatter in a small bin array, without allocating per event.
    // Sampling affects placement only, never which rows count.
    let mut hot_ids = Vec::new();
    if direct.is_some() && ne >= 128 << 10 {
        let mut sample = Map::with_hasher(rs());
        for &key in pieces.iter().flat_map(|s| &s.times).take(4096) {
            *sample.entry(key.person(layout) as u32).or_insert(0u32) += 1;
        }
        hot_ids.extend(sample.into_iter().filter(|&(_, n)| n >= 512).map(|(id, _)| id));
    }
    let mut hot_index = vec![u8::MAX; if hot_ids.is_empty() { 0 } else { direct.unwrap().len() }];
    for (i, &id) in hot_ids.iter().enumerate() {
        hot_index[id as usize] = i as u8;
    }
    let mut hot_sums = vec![Summary::default(); bins * hot_ids.len()];
    let mut last_hot = vec![0u32; hot_ids.len()];
    let mut last_event = vec![0u32; direct.map_or(0, <[u64]>::len)];
    let mut ids = Map::<u64, u32>::with_hasher(rs());
    let mut names = Vec::new();
    let mut id_and_ts = |key: T| {
        let rank = key.person(layout);
        let ts = offset(key.ts(layout));
        let id = if direct.is_some() {
            rank as u32
        } else {
            *ids.entry(rank).or_insert_with(|| {
                let id = names.len() as u32;
                names.push(T::name(rank, layout, people));
                id
            })
        };
        (id, ts)
    };
    let record_last = |last_event: &mut [u32], id: u32, ts: u32| {
        if direct.is_some() {
            let last = &mut last_event[id as usize];
            *last = (*last).max(ts + 1);
        }
    };
    let mut times = Radix::new(bins);
    let mut purchases = Radix::new(bins);
    for part in pieces {
        if hot_ids.is_empty() {
            for &key in &part.times {
                let (id, ts) = id_and_ts(key);
                if id as usize >= MAX_PEOPLE {
                    return false;
                }
                record_last(&mut last_event, id, ts);
                times.push(ts, (id, ts));
            }
        } else {
            for &key in &part.times {
                let id = key.person(layout) as u32;
                let ts = offset(key.ts(layout));
                let i = hot_index[id as usize];
                if i == u8::MAX {
                    record_last(&mut last_event, id, ts);
                    times.push(ts, (id, ts));
                } else {
                    let bin = (ts / WIDTH) as usize;
                    hot_sums[(i as usize) * bins + bin].add(ts);
                    last_hot[i as usize] = last_hot[i as usize].max(ts + 1);
                }
            }
        }
        for p in &part.purchases {
            let (id, ts) = id_and_ts(p.key);
            if id as usize >= MAX_PEOPLE {
                return false;
            }
            record_last(&mut last_event, id, ts);
            purchases.push(
                ts,
                Purchase {
                    id,
                    ts,
                    cat: p.cat,
                    price: p.price,
                },
            );
        }
    }
    for (&id, last) in hot_ids.iter().zip(last_hot) {
        last_event[id as usize] = last_event[id as usize].max(last);
    }
    let mut click_bins = Radix::new(bins);
    for part in clicks {
        for c in part {
            let ts = offset(c.ts);
            let id = if direct.is_some() {
                (ts < last_event[c.person as usize]).then_some(c.person as u32)
            } else {
                ids.get(&c.person).copied()
            };
            if let Some(id) = id {
                click_bins.push(
                    ts,
                    Click {
                        cid: c.cid,
                        id,
                        ts,
                        camp: c.camp,
                    },
                );
            }
        }
    }
    drop(ids);
    drop(last_event);
    let names = direct.unwrap_or(&names);
    let mut sums = vec![Summary::default(); names.len()];
    let mut states: Vec<_> = (0..names.len()).map(|_| State::new()).collect();
    let mut windows: Vec<_> = (0..names.len()).map(|_| Window::new()).collect();
    let mut categories: Vec<Vec<_>> = (0..names.len()).map(|_| Vec::new()).collect();
    let mut touched = Vec::new();
    let mut late = Vec::new();
    let mut active = Active::new();
    // Dense only on the small campaign×day output grid, never by person.
    let mut counts = vec![[0i64; 4]; cells];
    for (bin, ((events, buys), clicks)) in times
        .buckets
        .into_iter()
        .zip(purchases.buckets)
        .zip(click_bins.buckets)
        .enumerate()
    {
        for (i, &id) in hot_ids.iter().enumerate() {
            let sum = hot_sums[i * bins + bin];
            if sum.count != 0 {
                sums[id as usize] = sum;
                touched.push(id);
            }
        }
        for (id, ts) in events {
            summarize(&mut sums, &mut touched, id, ts);
        }
        for p in &buys {
            summarize(&mut sums, &mut touched, p.id, p.ts);
        }
        // At most one start per person in the bin: early clicks commute
        // within this pass (max timestamp, min ID on ties), as do late ones.
        for c in clicks {
            let e = sums[c.id as usize];
            if e.count != 0 && c.ts > e.first && states[c.id as usize].opens(e.first) {
                late.push(c);
            } else {
                active.apply(c, &mut windows[c.id as usize], people);
            }
        }
        for id in touched.drain(..) {
            let e = &mut sums[id as usize];
            let s = &mut states[id as usize];
            if s.opens(e.first) {
                let day = (origin + i64::from(e.first)).div_euclid(DAY);
                if s.day != day as i32 {
                    s.flush(
                        names[id as usize],
                        &mut out.users,
                        &sm.cat_rank,
                        &mut categories[id as usize],
                    );
                    s.day = day as i32;
                }
                let lo = e.first.saturating_sub(WEEK as u32);
                let w = &mut windows[id as usize];
                let credit = if w.latest.camp != ORGANIC && w.latest.ts >= lo {
                    w.latest.camp as usize
                } else {
                    columns - 1
                };
                let row = (day - day0) as usize * columns;
                s.credit_cell = (row + credit) as u32;
                s.purchased = false;
                s.sessions += 1;
                counts[s.credit_cell as usize][0] += 1;
                active.assist(id, w, lo, &mut counts[row..row + columns]);
            }
            s.end = e.last;
            s.events += e.count;
            e.count = 0;
        }
        // Each purchase belongs to the session opened or extended by this
        // person's summary, even when that session started the previous day.
        for p in buys {
            let s = &mut states[p.id as usize];
            let price = i64::from(p.price);
            let c = &mut counts[s.credit_cell as usize];
            if !s.purchased {
                c[1] += 1;
                s.purchased = true;
            }
            c[2] += price;
            s.revenue += price;
            let cats = &mut categories[p.id as usize];
            if let Some((_, total)) = cats.iter_mut().find(|(cat, _)| *cat == p.cat) {
                *total += price;
            } else {
                cats.push((p.cat, price));
            }
        }
        for c in late.drain(..) {
            active.apply(c, &mut windows[c.id as usize], people);
        }
    }
    for ((&name, s), cats) in names.iter().zip(&mut states).zip(&mut categories) {
        s.flush(name, &mut out.users, &sm.cat_rank, cats);
    }
    for (day, row) in counts.chunks_exact(columns).enumerate() {
        for (c, v) in row.iter().copied().enumerate() {
            if v[0] != 0 || v[3] != 0 {
                let camp = if c == columns - 1 { ORGANIC } else { c as u32 };
                let r = out
                    .camps
                    .entry(ckey(camp, (day0 + day as i64) as i32))
                    .or_insert([0; 4]);
                for i in 0..4 {
                    r[i] += v[i];
                }
            }
        }
    }
    true
}
