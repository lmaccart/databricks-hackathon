//! Sessionize dense streams without sorting every temporal key.
use super::stream::{Purchase, Stream, Temporal};
use super::{GAP, NS};

const MAX_BINS: usize = 4096; // bound histogram footprint
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Session<T> {
    pub start: T,
    pub end: T,
    pub count: i64,
}

pub(super) enum Ordered<T> {
    Events(Vec<T>),
    Sessions(Vec<Session<T>>),
}

pub(super) fn exact_sessions<T: Temporal>(mut events: &[T], layout: T::Layout) -> impl Iterator<Item = Session<T>> {
    std::iter::from_fn(move || {
        let start = *events.first()?;
        let person = start.person(layout);
        let mut tail = start;
        let mut n = 1;
        for &key in &events[1..] {
            if key.person(layout) != person || key.gap(tail) > GAP as u64 {
                break;
            }
            tail = key;
            n += 1;
        }
        events = &events[n..];
        Some(Session {
            start,
            end: tail,
            count: n as i64,
        })
    })
}

fn keys<T: Copy>(piece: &Stream<T>) -> impl Iterator<Item = T> + '_ {
    piece.times.iter().copied().chain(piece.purchases.iter().map(|p| p.key))
}

pub(super) fn prepare<T: Temporal>(
    pieces: Vec<Stream<T>>,
    layout: T::Layout,
    bins: Option<Bins>,
    direct: Option<(usize, usize)>,
    ne: usize,
    np: usize,
) -> (Ordered<T>, Vec<Purchase<T>>) {
    let direct = direct.filter(|&(shard, count)| shard < NS && count.saturating_sub(shard).div_ceil(NS) <= ne / 4);
    let bins = bins.filter(|b| direct.is_some() && ne >= SAMPLE.max(b.len) && ne < u32::MAX as usize);
    let mut purchases = Vec::with_capacity(np);
    let mut events = Vec::with_capacity(ne);
    if let Some(bins) = bins {
        let mut ends = vec![0usize; bins.len];
        for key in pieces.iter().flat_map(keys) {
            ends[bins.index(key.ts(layout))] += 1;
        }
        let mut offset = 0;
        for end in &mut ends {
            let count = *end;
            *end = offset;
            offset += count;
        }
        let dst = events.spare_capacity_mut();
        for piece in pieces {
            for key in keys(&piece) {
                let end = &mut ends[bins.index(key.ts(layout))];
                dst[*end].write(key);
                *end += 1;
            }
            purchases.extend(piece.purchases);
        }
        // Counting and scattering visit the same keys into disjoint
        // contiguous ranges. The purchases themselves never enter this buffer.
        unsafe { events.set_len(offset) };
        let (shard, count) = direct.unwrap();
        if let Some(sessions) = sweep(&events, &ends, layout, shard, count) {
            return (Ordered::Sessions(sessions), purchases);
        }
    } else {
        for piece in pieces {
            events.extend(piece.times);
            events.extend(piece.purchases.iter().map(|p| p.key));
            purchases.extend(piece.purchases);
        }
    }
    events.sort_unstable();
    (Ordered::Events(events), purchases)
}

#[derive(Clone, Copy)]
struct State<T> {
    min: T,
    max: T,
    slot: u32,
    head: u32,
    count: u32,
}

fn sweep<T: Temporal>(
    events: &[T],
    ends: &[usize],
    layout: T::Layout,
    shard: usize,
    rank_count: usize,
) -> Option<Vec<Session<T>>> {
    let slots = rank_count.saturating_sub(shard).div_ceil(NS);
    let mut states = vec![
        State {
            min: events[0],
            max: events[0],
            slot: u32::MAX,
            head: u32::MAX,
            count: 0,
        };
        slots
    ];
    let mut touched = Vec::new();
    let mut sessions: Vec<Session<T>> = Vec::new();
    let mut next: Vec<u32> = Vec::new();
    // Do not grow a large interval buffer unless it compresses the input.
    let limit = events.len() / 4;
    let mut start = 0;
    for &end in ends {
        for &key in &events[start..end] {
            let rank = key.person(layout) as usize;
            assert!(rank < rank_count && rank % NS == shard);
            let i = rank / NS;
            let st = &mut states[i];
            if st.count == 0 {
                touched.push(i);
                st.min = key;
                st.max = key;
            } else {
                st.min = st.min.min(key);
                st.max = st.max.max(key);
            }
            st.count += 1;
        }
        start = end;
        for i in touched.drain(..) {
            let st = &mut states[i];
            // All events of a person within this bin are connected. Only
            // the previous occupied bin's max and this bin's min can bridge.
            if let Some(last) = sessions.get_mut(st.slot as usize)
                && st.min.gap(last.end) <= GAP as u64
            {
                last.end = st.max;
                last.count += i64::from(st.count);
            } else {
                if sessions.len() == limit {
                    return None;
                }
                let slot = sessions.len() as u32;
                if st.head == u32::MAX {
                    st.head = slot;
                } else {
                    next[st.slot as usize] = slot;
                }
                st.slot = slot;
                sessions.push(Session {
                    start: st.min,
                    end: st.max,
                    count: i64::from(st.count),
                });
                next.push(u32::MAX);
            }
            st.count = 0;
        }
    }
    // Slots are ascending ranks within the shard, and each chain was appended
    // in chronological bin order. Attribution therefore needs no session sort.
    let mut ordered = Vec::with_capacity(sessions.len());
    for st in states {
        let mut slot = st.head;
        while slot != u32::MAX {
            ordered.push(sessions[slot as usize]);
            slot = next[slot as usize];
        }
    }
    Some(ordered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{Packing, Wide};

    fn prepare_rows<T: Temporal>(
        pieces: Vec<Stream<T>>,
        layout: T::Layout,
        bins: Option<Bins>,
    ) -> (Ordered<T>, Vec<Purchase<T>>) {
        let np = pieces.iter().map(|s| s.purchases.len()).sum::<usize>();
        let ne = pieces.iter().map(|s| s.times.len()).sum::<usize>() + np;
        let count = pieces
            .iter()
            .flat_map(keys)
            .map(|k| k.person(layout) as usize + 1)
            .max()
            .unwrap_or(0);
        prepare(pieces, layout, bins, Some((1, count)), ne, np)
    }

    #[test]
    fn bounded_ranges_use_exact_exclusive_end() {
        let ws = i64::MAX - (GAP * MAX_BINS as i64 + 1);
        let we = ws + GAP * MAX_BINS as i64;
        let bins = Bins::new(ws, we).unwrap();
        assert_eq!(bins.len, MAX_BINS);
        assert_eq!(bins.index(ws), 0);
        assert_eq!(bins.index(ws + GAP - 1), 0);
        assert_eq!(bins.index(ws + GAP), 1);
        assert_eq!(bins.index(we - 1), MAX_BINS - 1);
        assert!(Bins::new(ws, we + 1).is_none());
        assert!(Bins::new(ws, ws).is_none());
        assert!(Bins::new(i64::MIN + 1, i64::MAX).is_none());
    }

    fn check_intervals<T: Temporal + std::fmt::Debug>(layout: T::Layout, key: impl Fn(u32, i64) -> T) {
        let ws = -GAP - 11;
        let a = ws + GAP - 1;
        let b = a + GAP;
        let c = b + GAP + 1;
        let d = c + GAP - 1;
        let pieces = vec![
            Stream {
                times: (0..SAMPLE).map(|_| key(1, b)).collect(),
                purchases: vec![
                    Purchase {
                        key: key(1, d),
                        price: 42,
                        cat: 3,
                    },
                    Purchase {
                        key: key(1, a),
                        price: 9,
                        cat: 2,
                    },
                    Purchase {
                        key: key(2, c),
                        price: 20,
                        cat: 1,
                    },
                ],
            },
            Stream {
                times: vec![key(1, c + 1), key(2, a), key(1, c)],
                purchases: vec![
                    Purchase {
                        key: key(1, d),
                        price: 10,
                        cat: 1,
                    },
                    Purchase {
                        key: key(1, b),
                        price: 31,
                        cat: 1,
                    },
                ],
            },
        ];
        let mut all: Vec<_> = pieces.iter().flat_map(keys).collect();
        all.sort_unstable();
        let (Ordered::Sessions(got), sidecar) = prepare_rows(pieces, layout, Bins::new(ws, d + 1)) else {
            panic!("dense stream did not use bin sweep");
        };
        assert_eq!(got, exact_sessions(&all, layout).collect::<Vec<_>>());
        assert_eq!(
            got,
            [
                Session {
                    start: key(1, a),
                    end: key(1, b),
                    count: SAMPLE as i64 + 2
                },
                Session {
                    start: key(1, c),
                    end: key(1, d),
                    count: 4
                },
                Session {
                    start: key(2, a),
                    end: key(2, a),
                    count: 1
                },
                Session {
                    start: key(2, c),
                    end: key(2, c),
                    count: 1
                },
            ]
        );
        let mut at_end = sidecar.iter().filter(|p| p.key == key(1, d));
        assert_eq!(at_end.next().map(|p| (p.price, p.cat)), Some((42, 3)));
        assert_eq!(at_end.next().map(|p| (p.price, p.cat)), Some((10, 1)));
        assert!(at_end.next().is_none());
    }

    #[test]
    fn both_layouts_sweep_exact_intervals_and_keep_sidecar_ties() {
        check_intervals((), |rank, ts| Wide {
            person: 1 + u64::from(rank - 1) * NS as u64,
            ts,
        });
        let pack = Packing::new(-GAP - 11, 6 * GAP, 258).unwrap();
        check_intervals(pack, |rank, ts| pack.encode(1 + (rank - 1) * NS as u32, ts));
    }

    #[test]
    fn sparse_or_misleading_streams_keep_all_keys_on_fallback() {
        let bins = Bins::new(0, GAP * 128);
        for case in 0..4 {
            let mut a = Stream::default();
            for i in 0..SAMPLE * 3 {
                let (person, ts) = match case {
                    2 => (i % 16, 2 * GAP * (i / 16) as i64), // repeated people, not sessions
                    3 => (0, 0),                              // dense but over-wide window
                    _ => (if i < case * SAMPLE { 0 } else { i }, GAP * (i % 3) as i64),
                };
                let key = Wide {
                    person: 1 + person as u64 * NS as u64,
                    ts,
                };
                if i % 11 == 0 && i >= SAMPLE {
                    a.purchases.push(Purchase {
                        key,
                        price: i as u32,
                        cat: 7,
                    });
                } else {
                    a.times.push(key);
                }
            }
            let mut expected: Vec<_> = keys(&a).collect();
            expected.sort_unstable();
            let np = a.purchases.len();
            let bins = if case == 3 {
                Bins::new(0, GAP * (MAX_BINS as i64 + 1))
            } else {
                bins
            };
            let (Ordered::Events(got), sidecar) = prepare_rows(vec![a], (), bins) else {
                panic!("case {case} did not fall back");
            };
            assert!(got == expected);
            assert_eq!(sidecar.len(), np);
        }
    }

    #[test]
    fn direct_rank_sweep_matches_sort_for_all_shards_and_empty_input() {
        let bins = Bins::new(0, 32 * GAP);
        let pack = Packing::new(0, 32 * GAP, NS * 16).unwrap();
        for shard in [0, 1, NS - 1] {
            let mut seed = 7u64;
            let mut stream = Stream::default();
            for i in 0..8192 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                let rank = shard + ((seed >> 32) as usize % 16) * NS;
                let ts = ((seed >> 8) % (32 * GAP) as u64) as i64;
                let key = pack.encode(rank as u32, ts);
                if i % 19 == 0 {
                    stream.purchases.push(Purchase { key, price: 1, cat: 0 });
                } else {
                    stream.times.push(key);
                }
            }
            let mut all: Vec<_> = keys(&stream).collect();
            all.sort_unstable();
            let np = stream.purchases.len();
            let (Ordered::Sessions(got), purchases) =
                prepare(vec![stream], pack, bins, Some((shard, NS * 16)), all.len(), np)
            else {
                panic!("dense shard unexpectedly fell back");
            };
            assert_eq!(got, exact_sessions(&all, pack).collect::<Vec<_>>());
            assert_eq!(purchases.len(), np);
        }
        let (Ordered::Events(events), purchases) = prepare::<u64>(vec![], pack, bins, Some((0, 0)), 0, 0) else {
            panic!("empty input must not sweep");
        };
        assert!(events.is_empty() && purchases.is_empty());
    }

    #[test]
    fn huge_sparse_link_graph_falls_back_without_dense_allocation() {
        let rank_count = 1_000_000_000;
        let pack = Packing::new(0, GAP * 2, rank_count).unwrap();
        let key = pack.encode((rank_count - 1) as u32, 1);
        let stream = Stream {
            times: vec![key; 1024],
            purchases: vec![],
        };
        let shard = (rank_count - 1) % NS;
        let (Ordered::Events(events), _) = prepare(
            vec![stream],
            pack,
            Bins::new(0, GAP * 2),
            Some((shard, rank_count)),
            1024,
            0,
        ) else {
            panic!("sparse graph must use the sort fallback");
        };
        assert_eq!(events, vec![key; 1024]);
    }
}
