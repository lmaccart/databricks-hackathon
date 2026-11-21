//! Group by exact person once, then order only that person's timeline.
use super::{Map, PCl, PEv, rs};
use std::collections::hash_map::Entry;

pub(super) struct Group {
    pub person: u64,
    pub e_start: usize,
    pub e_end: usize,
    pub c_start: usize,
    pub c_end: usize,
}

struct Counts {
    person: u64,
    events: usize,
    clicks: usize,
}

pub(super) fn group(event_parts: Vec<Vec<PEv>>, click_parts: Vec<Vec<PCl>>) -> (Vec<PEv>, Vec<PCl>, Vec<Group>) {
    let mut index: Map<u64, usize> = Map::with_hasher(rs());
    let mut counts: Vec<Counts> = Vec::new();
    for e in event_parts.iter().flatten() {
        match index.entry(e.person) {
            Entry::Occupied(x) => counts[*x.get()].events += 1,
            Entry::Vacant(x) => {
                x.insert(counts.len());
                counts.push(Counts {
                    person: e.person,
                    events: 1,
                    clicks: 0,
                });
            }
        }
    }
    for c in click_parts.iter().flatten() {
        if let Some(&i) = index.get(&c.person) {
            counts[i].clicks += 1;
        }
    }
    let (mut ne, mut nc) = (0, 0);
    let mut groups = Vec::with_capacity(counts.len());
    for c in counts {
        let g = Group {
            person: c.person,
            e_start: ne,
            e_end: ne + c.events,
            c_start: nc,
            c_end: nc + c.clicks,
        };
        ne = g.e_end;
        nc = g.c_end;
        groups.push(g);
    }
    let mut evs = vec![
        PEv {
            person: 0,
            ts: 0,
            price: 0,
            cat: 0
        };
        ne
    ];
    let mut cls = vec![
        PCl {
            person: 0,
            ts: 0,
            cid: 0,
            camp: 0
        };
        nc
    ];
    let mut positions: Vec<_> = groups.iter().map(|g| (g.e_start, g.c_start)).collect();
    for e in event_parts.into_iter().flatten() {
        let i = index[&e.person];
        evs[positions[i].0] = e;
        positions[i].0 += 1;
    }
    for c in click_parts.into_iter().flatten() {
        if let Some(&i) = index.get(&c.person) {
            cls[positions[i].1] = c;
            positions[i].1 += 1;
        }
    }
    for g in &groups {
        sort_time(&mut evs[g.e_start..g.e_end], |e| e.ts);
        sort_time(&mut cls[g.c_start..g.c_end], |c| c.ts);
    }
    (evs, cls, groups)
}

// Large whales get a full stable LSD radix, not a prefix partition followed
// by comparisons. Skipping constant bytes keeps the ordinary narrow window
// cheap; signed ordering and arbitrarily wide spans remain exact.
fn sort_time<T: Copy>(rows: &mut [T], time: impl Fn(&T) -> i64) {
    const RADIX_MIN: usize = 2048;
    if rows.len() < RADIX_MIN {
        rows.sort_unstable_by_key(time);
        return;
    }
    let key = |r: &T| (time(r) as u64) ^ (1u64 << 63);
    let first = key(&rows[0]);
    let variation = rows.iter().fold(0u64, |v, r| v | (key(r) ^ first));
    if variation == 0 {
        return;
    }
    let mut scratch = rows.to_vec();
    let mut in_scratch = false;
    for shift in (0..64).step_by(8) {
        if variation & (0xffu64 << shift) == 0 {
            continue;
        }
        let (src, dst): (&[T], &mut [T]) = if in_scratch {
            (&scratch, rows)
        } else {
            (rows, &mut scratch)
        };
        let mut counts = [0usize; 256];
        for r in src {
            counts[((key(r) >> shift) & 255) as usize] += 1;
        }
        let mut offset = 0;
        for count in &mut counts {
            let n = *count;
            *count = offset;
            offset += n;
        }
        for r in src {
            let p = &mut counts[((key(r) >> shift) & 255) as usize];
            dst[*p] = *r;
            *p += 1;
        }
        in_scratch = !in_scratch;
    }
    if in_scratch {
        rows.copy_from_slice(&scratch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_groups_and_wide_signed_timestamps() {
        let ev = |person, ts, price| PEv {
            person,
            ts,
            price,
            cat: 0,
        };
        let click = |person, ts, cid| PCl {
            person,
            ts,
            cid,
            camp: 0,
        };
        let mut whale: Vec<_> = (0..5000)
            .map(|i| ev(17, ((i * 7919) % 5000) as i64 - 2500, i as u32))
            .collect();
        whale.push(ev(17, i64::MAX, 0));
        whale.push(ev(17, i64::MIN, 0));
        whale.push(ev(17, -1, 0));
        let (events, clicks, groups) = group(
            vec![vec![ev(9, 0, 0), ev(9, -3, 0)], whale, vec![ev(u64::MAX, 5, 0)]],
            vec![vec![click(9, 0, 2), click(9, -3, 1), click(17, 2, 5), click(123, 0, 0)]],
        );
        assert_eq!(groups.len(), 3);
        assert_eq!(clicks.len(), 3); // a click-only person cannot affect sessions
        for g in groups {
            let es = &events[g.e_start..g.e_end];
            let cs = &clicks[g.c_start..g.c_end];
            assert!(es.iter().all(|e| e.person == g.person));
            assert!(cs.iter().all(|c| c.person == g.person));
            assert!(es.windows(2).all(|w| w[0].ts <= w[1].ts));
            assert!(cs.windows(2).all(|w| w[0].ts <= w[1].ts));
            if g.person == 17 {
                assert_eq!(es.len(), 5003);
                assert_eq!(es[0].ts, i64::MIN);
                assert_eq!(es.last().unwrap().ts, i64::MAX);
            }
        }
    }

    #[test]
    fn radix_retains_all_payloads_on_ties_and_sparse_digits() {
        let mut rows: Vec<_> = (0..6000)
            .map(|i| PEv {
                person: 1,
                ts: (((i * 1237) % 13) as i64) << 40,
                price: i as u32,
                cat: 0,
            })
            .collect();
        sort_time(&mut rows, |e| e.ts);
        assert!(rows.windows(2).all(|w| w[0].ts <= w[1].ts));
        let mut ids: Vec<_> = rows.iter().map(|e| e.price).collect();
        ids.sort_unstable();
        assert_eq!(ids, (0..6000).collect::<Vec<_>>());
    }
}
