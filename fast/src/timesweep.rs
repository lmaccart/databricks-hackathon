//! Count and scatter only time bins, then combine exact per-person extrema.
use super::{GAP, PEv, rs};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

const MAX_BINS: usize = 4096; // bound histogram footprint, independent of the dataset
const SAMPLE: usize = 256;

#[derive(Clone, Copy)]
pub(super) struct Bins {
    base: i64,
    len: usize,
}

impl Bins {
    pub(super) fn new(ws: i64, we: i64) -> Option<Self> {
        let span = we.checked_sub(ws)?.checked_sub(1)?;
        if span < 0 || span / GAP >= MAX_BINS as i64 {
            return None;
        }
        Some(Self {
            base: ws,
            len: (span / GAP) as usize + 1,
        })
    }

    #[inline]
    fn index(self, ts: i64) -> usize {
        ((ts - self.base) / GAP) as usize
    }
}

#[derive(Clone, Copy)]
pub(super) struct Session {
    pub(super) person: u64,
    pub(super) start: i64,
    pub(super) count: u32,
    pub(super) head: u32,
}

pub(super) struct Purchase {
    pub(super) cat: u32,
    pub(super) price: u32,
    pub(super) next: u32,
}

#[derive(Default)]
struct State {
    slot: u32,
    last_max: i64,
    min: i64,
    max: i64,
    count: u32,
    head: u32,
    tail: u32,
}

pub(super) struct Sweep {
    pub(super) sessions: Vec<Session>,
    pub(super) purchases: Vec<Purchase>,
}

fn scatter(pieces: Vec<Vec<PEv>>, bins: Bins, n: usize) -> (Vec<PEv>, Vec<usize>) {
    let mut ends = vec![0usize; bins.len];
    for e in pieces.iter().flatten() {
        ends[bins.index(e.ts)] += 1;
    }
    let mut offset = 0;
    for end in &mut ends {
        let count = *end;
        *end = offset;
        offset += count;
    }
    let mut evs = Vec::with_capacity(n);
    let dst = evs.spare_capacity_mut();
    for piece in pieces {
        for e in piece {
            let next = &mut ends[bins.index(e.ts)];
            dst[*next].write(e);
            *next += 1;
        }
    }
    // Each bin writes exactly its counted number of rows into disjoint
    // contiguous ranges, and together the ranges cover all n surviving rows.
    unsafe { evs.set_len(n) };
    (evs, ends)
}

pub(super) fn prepare(pieces: Vec<Vec<PEv>>, bins: Option<Bins>) -> Result<Sweep, Vec<PEv>> {
    let n: usize = pieces.iter().map(Vec::len).sum();
    let Some(bins) = bins.filter(|b| n >= SAMPLE && n >= b.len && n < u32::MAX as usize) else {
        return Err(super::gather(pieces));
    };
    // Avoid scattering shards with too little observed repetition. Estimate
    // from a prefix only; a later hard guard handles a misleading sample.
    let mut sample = HashSet::with_capacity_and_hasher(SAMPLE, rs());
    sample.extend(pieces.iter().flatten().take(SAMPLE).map(|e| e.person));
    if sample.len() > SAMPLE * 7 / 8 {
        return Err(super::gather(pieces));
    }
    let (evs, ends) = scatter(pieces, bins, n);
    sweep(evs, &ends)
}

fn sweep(evs: Vec<PEv>, ends: &[usize]) -> Result<Sweep, Vec<PEv>> {
    let mut states: Vec<State> = Vec::new();
    let mut index = HashMap::with_hasher(rs());
    let mut touched = Vec::new();
    let mut purchases = Vec::new();
    let mut sessions: Vec<Session> = Vec::new();
    let limit = evs.len() / 2;
    let mut start = 0;
    for &end in ends {
        for e in &evs[start..end] {
            let i = match index.entry(e.person) {
                Entry::Occupied(e) => *e.get(),
                Entry::Vacant(e) => {
                    // Each distinct person requires a session. Bound growth
                    // even if the prefix missed a single skewed bin.
                    let i = states.len();
                    if i == limit {
                        return Err(evs);
                    }
                    states.push(State::default());
                    e.insert(i);
                    i
                }
            };
            let st = &mut states[i];
            if st.count == 0 {
                touched.push((e.person, i));
                st.min = e.ts;
                st.max = e.ts;
            } else {
                st.min = st.min.min(e.ts);
                st.max = st.max.max(e.ts);
            }
            st.count += 1;
            if e.price != 0 {
                purchases.push(Purchase {
                    cat: e.cat,
                    price: e.price,
                    next: st.head,
                });
                st.head = purchases.len() as u32;
                if st.tail == 0 {
                    st.tail = st.head;
                }
            }
        }
        start = end;
        for (person, i) in touched.drain(..) {
            let st = &mut states[i];
            // Inside a bin all differences are less than GAP. Only the
            // previous maximum and present minimum can bridge two bins.
            if st.slot == 0 || st.min - st.last_max > GAP {
                if sessions.len() == limit {
                    return Err(evs);
                }
                sessions.push(Session {
                    person,
                    start: st.min,
                    count: 0,
                    head: 0,
                });
                st.slot = sessions.len() as u32;
            }
            let session = &mut sessions[st.slot as usize - 1];
            session.count += st.count;
            if st.head != 0 {
                purchases[st.tail as usize - 1].next = session.head;
                session.head = st.head;
                st.head = 0;
                st.tail = 0;
            }
            st.last_max = st.max;
            st.count = 0;
        }
    }
    drop(evs);
    sessions.sort_unstable_by_key(|s| (s.person, s.start));
    Ok(Sweep { sessions, purchases })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(person: u64, ts: i64, price: u32, cat: u32) -> PEv {
        PEv { person, ts, price, cat }
    }

    #[test]
    fn bounded_ranges_use_exact_exclusive_end() {
        for ws in [i64::MIN + 1, -GAP - 7, 11, i64::MAX - (GAP * MAX_BINS as i64 + 1)] {
            let we = ws + GAP * MAX_BINS as i64;
            let bins = Bins::new(ws, we).unwrap();
            assert_eq!(bins.len, MAX_BINS);
            assert_eq!(bins.index(ws), 0);
            assert_eq!(bins.index(ws + GAP - 1), 0);
            assert_eq!(bins.index(ws + GAP), 1);
            assert_eq!(bins.index(we - 1), MAX_BINS - 1);
            assert!(Bins::new(ws, we + 1).is_none());
            assert_eq!(Bins::new(ws, ws + 1).unwrap().len, 1);
            assert!(Bins::new(ws, ws).is_none());
        }
        assert!(Bins::new(i64::MIN + 1, i64::MAX).is_none());
    }

    #[test]
    fn scatter_extrema_touch_only_present_people_and_splice_purchases() {
        let ws = -11;
        let a = ws + GAP - 1;
        let b = a + GAP;
        let c = b + GAP + 1;
        let d = c + GAP - 1;
        let mut first = vec![
            event(1, c + 1, 3, 9),
            event(1, b, 4, 7),
            event(1, a, 5, 8),
            event(2, a, 6, 8),
            event(1, c, 7, 7),
        ];
        first.extend((0..SAMPLE).map(|_| event(1, b, 0, 0)));
        let second = vec![event(1, d, 8, 8), event(2, c, 9, 7)];
        let result = prepare(vec![first, second], Bins::new(ws, d + 1)).ok().unwrap();
        let got: Vec<_> = result.sessions.iter().map(|s| (s.person, s.start, s.count)).collect();
        assert_eq!(got, [(1, a, SAMPLE as u32 + 2), (1, c, 3), (2, a, 1), (2, c, 1)]);
        let amounts: Vec<_> = result
            .sessions
            .iter()
            .map(|s| {
                let mut head = s.head;
                let mut ps = Vec::new();
                while head != 0 {
                    let p = &result.purchases[head as usize - 1];
                    ps.push((p.cat, p.price));
                    head = p.next;
                }
                ps.sort_unstable();
                ps
            })
            .collect();
        assert_eq!(
            amounts,
            [
                vec![(7, 4), (8, 5)],
                vec![(7, 7), (8, 8), (9, 3)],
                vec![(8, 6)],
                vec![(7, 9)]
            ]
        );
    }

    #[test]
    fn fallback_keeps_every_event_for_sparse_and_misleading_prefixes() {
        let bins = Bins::new(0, GAP * 96);
        for case in 0..3 {
            let rows: Vec<_> = (0..SAMPLE * 3)
                .map(|i| {
                    let (person, ts) = match case {
                        2 => (i % 16, 2 * GAP * (i / 16) as i64), // repeat people but never sessions
                        _ => (if i < case * SAMPLE { 0 } else { i }, GAP * (i % 3) as i64),
                    };
                    event(person as u64, ts, i as u32, 2)
                })
                .collect();
            let mut returned = prepare(vec![rows[..SAMPLE].to_vec(), rows[SAMPLE..].to_vec()], bins)
                .err()
                .unwrap();
            returned.sort_unstable_by_key(|e| e.price);
            assert_eq!(returned.len(), rows.len());
            for (a, b) in returned.iter().zip(rows) {
                assert_eq!((a.person, a.ts, a.price, a.cat), (b.person, b.ts, b.price, b.cat));
            }
        }
    }
}
