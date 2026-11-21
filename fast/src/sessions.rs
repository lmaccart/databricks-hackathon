use crate::{BAD, Codec, DAY, GAP, Map, ORGANIC, PCl, PEv, Small, WEEK, rs};

pub(crate) struct UserRow {
    pub person: u64,
    pub day: i32,
    pub sessions: i64,
    pub events: i64,
    pub revenue: i64,
    pub top: Option<u32>,
}

pub(crate) struct ShardOut {
    pub users: Vec<UserRow>,
    pub camps: Map<u64, [i64; 4]>, // sessions, purchasing, revenue, assisted
}

// Bound scratch by bytes, not by the manifest's day span or dataset shape.
// A very large dimension uses storage proportional to IDs actually touched.
enum Touched<T> {
    Dense(Vec<T>, Vec<u32>),
    Sparse(Map<u32, T>),
}

impl<T: Copy + Default + PartialEq> Touched<T> {
    fn new(dim: usize) -> Self {
        if dim.saturating_mul(size_of::<T>() + size_of::<u32>()) <= 64 * 1024 {
            Self::Dense(vec![T::default(); dim], Vec::new())
        } else {
            Self::Sparse(Map::with_hasher(rs()))
        }
    }

    #[inline]
    fn at(&mut self, id: u32) -> &mut T {
        // Callers leave every touched value non-default until drain.
        match self {
            Self::Dense(values, touched) => {
                let value = &mut values[id as usize];
                if *value == T::default() {
                    touched.push(id);
                }
                value
            }
            Self::Sparse(values) => values.entry(id).or_default(),
        }
    }

    #[inline]
    fn drain(&mut self, mut f: impl FnMut(u32, T)) {
        match self {
            Self::Dense(values, touched) => {
                for id in touched.drain(..) {
                    f(id, std::mem::take(&mut values[id as usize]));
                }
            }
            Self::Sparse(values) => {
                for (id, value) in values.drain() {
                    f(id, value);
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Session {
    ts: i64,
    revenue: i64,
    day: i32,
}

#[derive(Clone, Copy, PartialEq)]
struct Coverage {
    first: usize,
    last_click: i64,
}

impl Default for Coverage {
    fn default() -> Self {
        Self {
            first: 0,
            last_click: BAD,
        }
    }
}

#[inline]
fn counts(camps: &mut Map<u64, [i64; 4]>, camp: u32, day: i32) -> &mut [i64; 4] {
    let key = ((camp as u64) << 32) | (day as u32 as u64);
    camps.entry(key).or_default()
}

#[inline]
fn credit(camps: &mut Map<u64, [i64; 4]>, s: Session, camp: u32, assists: i64) {
    let e = counts(camps, camp, s.day);
    e[0] += 1;
    e[1] += (s.revenue > 0) as i64;
    e[2] += s.revenue;
    e[3] += assists;
}

fn emit_assists(camps: &mut Map<u64, [i64; 4]>, camp: u32, mut sessions: &[Session], last_click: i64) {
    // The first session is known covered; the last is often covered as well.
    if last_click < sessions.last().unwrap().ts - WEEK {
        sessions = &sessions[..sessions.partition_point(|s| s.ts - WEEK <= last_click)];
    }
    while let Some(first) = sessions.first() {
        let n = if first.day == sessions.last().unwrap().day {
            sessions.len()
        } else {
            sessions.partition_point(|s| s.day == first.day)
        };
        counts(camps, camp, first.day)[3] += n as i64;
        sessions = &sessions[n..];
    }
}

fn attribute_many(
    sessions: &[Session],
    mut clicks: &[PCl],
    codec: &Codec,
    pending: &mut Touched<Coverage>,
    camps: &mut Map<u64, [i64; 4]>,
) {
    // Each campaign's pending range begins at its first covered session.
    // As clicks are consumed in timestamp order, a new click can extend that
    // range if the previous click still covered the preceding session.
    let mut winner: Option<&PCl> = None;
    let mut prev_start = sessions[0].ts;
    for (i, &s) in sessions.iter().enumerate() {
        let lower = s.ts - WEEK;
        // A click between the previous start and this lower bound cannot
        // assist any session, even if its real-time interval overlaps others.
        while let Some((click, rest)) = clicks.split_first() {
            if click.ts > s.ts {
                break;
            }
            clicks = rest;
            if click.ts < lower {
                continue;
            }
            if winner.is_none_or(|w| w.ts != click.ts || codec.less(click.cid, w.cid)) {
                winner = Some(click);
            }
            let range = pending.at(click.camp);
            if range.last_click < prev_start - WEEK {
                if range.last_click != BAD {
                    emit_assists(camps, click.camp, &sessions[range.first..i], range.last_click);
                }
                range.first = i;
            }
            // Overlapping or adjacent *session indices* are one range, even
            // when the campaign went inactive between consecutive sessions.
            range.last_click = click.ts;
        }
        let campaign = winner.filter(|w| w.ts >= lower).map_or(ORGANIC, |w| w.camp);
        credit(camps, s, campaign, 0);
        prev_start = s.ts;
    }
    pending.drain(|camp, range| emit_assists(camps, camp, &sessions[range.first..], range.last_click));
}

fn attribute_one(
    s: Session,
    clicks: &[PCl],
    codec: &Codec,
    pending: &mut Touched<Coverage>,
    camps: &mut Map<u64, [i64; 4]>,
) {
    let last = clicks.partition_point(|click| click.ts <= s.ts);
    let clicks = &clicks[..last];
    let first = clicks.partition_point(|click| click.ts < s.ts - WEEK);
    let clicks = &clicks[first..];
    let Some(mut winner) = clicks.last() else {
        credit(camps, s, ORGANIC, 0);
        return;
    };
    let latest_ts = winner.ts;
    for click in clicks.iter().rev().skip(1).take_while(|c| c.ts == latest_ts) {
        if codec.less(click.cid, winner.cid) {
            winner = click;
        }
    }
    if clicks.len() <= 4 {
        // For tiny slices, test distinctness directly; no interval state.
        for (i, click) in clicks.iter().enumerate() {
            if click.camp != winner.camp && !clicks[..i].iter().any(|p| p.camp == click.camp) {
                counts(camps, click.camp, s.day)[3] += 1;
            }
        }
    } else {
        for click in clicks {
            if click.camp != winner.camp {
                pending.at(click.camp).last_click = click.ts;
            }
        }
        pending.drain(|camp, _| counts(camps, camp, s.day)[3] += 1);
    }
    credit(camps, s, winner.camp, 1);
}

fn finish_day(row: &mut UserRow, cats: &mut Touched<i64>, rank: &[u32]) {
    if row.revenue == 0 {
        return;
    }
    let mut best = 0;
    cats.drain(|cat, sum| {
        if sum > best || (sum == best && rank[cat as usize] < rank[row.top.unwrap() as usize]) {
            best = sum;
            row.top = Some(cat);
        }
    });
}

pub(crate) fn process_shard(mut evs: Vec<PEv>, mut cls: Vec<PCl>, sm: &Small, codec: &Codec) -> ShardOut {
    evs.sort_unstable_by_key(|e| (e.person, e.ts));
    cls.sort_unstable_by_key(|c| (c.person, c.ts));
    let mut users = Vec::new();
    let mut camps = Map::with_hasher(rs());
    let mut pending = Touched::new(sm.camp_names.len());
    let mut cats = Touched::new(sm.cat_names.len());
    let mut sessions = Vec::new();
    let mut clicks = cls.as_slice();
    let mut events = evs.as_slice();
    while let Some(first) = events.first() {
        let person = first.person;
        let n = 1 + events[1..].iter().take_while(|e| e.person == person).count();
        let person_events = &events[..n];
        events = &events[n..];
        while clicks.first().is_some_and(|c| c.person < person) {
            clicks = &clicks[1..];
        }
        let n = clicks.iter().take_while(|c| c.person == person).count();
        let person_clicks = &clicks[..n];
        clicks = &clicks[n..];

        let first_day = (first.ts.div_euclid(DAY)) as i32;
        let mut row = UserRow {
            person,
            day: first_day,
            sessions: 0,
            events: 0,
            revenue: 0,
            top: None,
        };
        let mut k = 0;
        while k < person_events.len() {
            let start = person_events[k].ts;
            let day = start.div_euclid(DAY) as i32;
            if day != row.day {
                finish_day(&mut row, &mut cats, &sm.cat_rank);
                users.push(row);
                row = UserRow {
                    person,
                    day,
                    sessions: 0,
                    events: 0,
                    revenue: 0,
                    top: None,
                };
            }
            let mut revenue = 0;
            let mut m = k;
            loop {
                let e = &person_events[m];
                if e.price != 0 {
                    revenue += e.price as i64;
                    *cats.at(e.cat) += e.price as i64;
                }
                m += 1;
                if m == person_events.len() || person_events[m].ts - e.ts > GAP {
                    break;
                }
            }
            row.sessions += 1;
            row.events += (m - k) as i64;
            row.revenue += revenue;

            let s = Session {
                ts: start,
                revenue,
                day,
            };
            match person_clicks {
                [] => credit(&mut camps, s, ORGANIC, 0),
                [click] if (start - WEEK..=start).contains(&click.ts) => credit(&mut camps, s, click.camp, 1),
                [_] => credit(&mut camps, s, ORGANIC, 0),
                _ if k == 0 && m == person_events.len() => {
                    attribute_one(s, person_clicks, codec, &mut pending, &mut camps);
                }
                _ => sessions.push(s),
            }
            k = m;
        }
        finish_day(&mut row, &mut cats, &sm.cat_rank);
        users.push(row);
        if !sessions.is_empty() {
            attribute_many(&sessions, person_clicks, codec, &mut pending, &mut camps);
            sessions.clear();
        }
    }
    ShardOut { users, camps }
}
