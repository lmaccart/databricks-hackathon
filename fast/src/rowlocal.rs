//! Compress just one sampled hot person per shard, after global dedup.
use super::GAP;
use super::stream::{Purchase, Stream, Temporal};

const SAMPLE: usize = 256;
const MIN_HITS: usize = SAMPLE / 8;
const MIN_ROWS: usize = 8192;
const MAX_BINS: u64 = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Span<T> {
    pub start: T,
    pub end: T,
    pub count: usize,
}

impl<T: Temporal> Span<T> {
    #[inline]
    fn join(&mut self, next: Self) -> bool {
        if next.start > self.end && next.start.gap(self.end) > GAP as u64 {
            return false;
        }
        self.end = self.end.max(next.end);
        self.count += next.count;
        true
    }
}

// Sampling only affects the choice of algorithm. Every subsequent membership
// check compares the full person token, and unsampled people stay on raw keys.
fn hotspot<T: Temporal>(pieces: &[Stream<T>], ne: usize, layout: T::Layout) -> Option<T> {
    if ne < MIN_ROWS {
        return None;
    }
    let tile = pieces.iter().max_by_key(|s| s.times.len())?;
    if tile.times.len() < SAMPLE {
        return None;
    }
    let stride = tile.times.len() / SAMPLE;
    let mut sample: [_; SAMPLE] = std::array::from_fn(|i| tile.times[i * stride]);
    sample.sort_unstable();
    let mut best = (MIN_HITS - 1, None);
    let mut i = 0;
    while i < SAMPLE {
        let token = sample[i].person(layout);
        let mut end = i + 1;
        while end < SAMPLE && sample[end].person(layout) == token {
            end += 1;
        }
        if end - i > best.0 {
            best = (end - i, Some(sample[i]));
        }
        i = end;
    }
    best.1
}

struct Tile<T> {
    bins: Vec<Span<T>>,
    first: usize,
    stop: usize,
    origin: i64,
}

impl<T: Temporal> Tile<T> {
    #[inline]
    fn add(&mut self, key: T, layout: T::Layout) {
        // A bin spans strictly less than GAP from its smallest possible key
        // to its largest. It is connected even before the other tiles arrive.
        let i = (key.ts(layout).abs_diff(self.origin) / GAP as u64) as usize;
        let bin = &mut self.bins[i];
        if bin.count == 0 {
            *bin = Span {
                start: key,
                end: key,
                count: 1,
            };
            self.first = self.first.min(i);
            self.stop = self.stop.max(i + 1);
        } else {
            bin.start = bin.start.min(key);
            bin.end = bin.end.max(key);
            bin.count += 1;
        }
    }

    fn emit(&mut self, spans: &mut Vec<Span<T>>) {
        let previous_tiles = spans.len();
        if self.stop != 0 {
            for bin in &mut self.bins[self.first..self.stop] {
                if bin.count == 0 {
                    continue;
                }
                if spans.len() == previous_tiles || !spans.last_mut().unwrap().join(*bin) {
                    spans.push(*bin);
                }
                bin.count = 0;
            }
            self.first = self.bins.len();
            self.stop = 0;
        }
    }
}

fn coalesce<T: Temporal>(spans: &mut Vec<Span<T>>) {
    spans.sort_unstable_by_key(|s| s.start);
    let mut n = 0;
    for i in 0..spans.len() {
        let next = spans[i];
        if n == 0 || !spans[n - 1].join(next) {
            spans[n] = next;
            n += 1;
        }
    }
    spans.truncate(n);
}

pub(super) struct Gathered<T> {
    pub keys: Vec<T>,
    pub purchases: Vec<Purchase<T>>,
    pub hot: Option<u64>,
    pub sessions: Vec<Span<T>>,
}

pub(super) fn gather<T: Temporal>(
    pieces: Vec<Stream<T>>,
    (ne, np): (usize, usize),
    layout: T::Layout,
    (ws, we): (i64, i64),
) -> Gathered<T> {
    let nbins = we.abs_diff(ws).div_ceil(GAP as u64);
    let seed = (ws < we && nbins <= MAX_BINS)
        .then(|| hotspot(&pieces, ne, layout))
        .flatten();
    let mut keys = Vec::with_capacity(ne);
    let mut purchases = Vec::with_capacity(np);
    let mut sessions = Vec::new();
    let hot = seed.map(|s| s.person(layout));
    if let Some(seed) = seed {
        let hot = seed.person(layout);
        let mut tile = Tile {
            bins: vec![
                Span {
                    start: seed,
                    end: seed,
                    count: 0
                };
                nbins as usize
            ],
            first: nbins as usize,
            stop: 0,
            origin: ws,
        };
        for s in pieces {
            for &key in &s.times {
                if key.person(layout) == hot {
                    tile.add(key, layout);
                } else {
                    keys.push(key);
                }
            }
            for p in &s.purchases {
                if p.key.person(layout) == hot {
                    tile.add(p.key, layout);
                } else {
                    keys.push(p.key);
                }
            }
            purchases.extend_from_slice(&s.purchases);
            tile.emit(&mut sessions);
        }
        coalesce(&mut sessions);
        keys.extend(sessions.iter().map(|s| s.start));
    } else {
        for s in pieces {
            keys.extend_from_slice(&s.times);
            keys.extend(s.purchases.iter().map(|p| p.key));
            purchases.extend_from_slice(&s.purchases);
        }
    }
    Gathered {
        keys,
        purchases,
        hot,
        sessions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{Packing, Wide};

    #[test]
    fn adaptive_exact_token_and_window_fallbacks() {
        let window = (0, 4 * GAP);
        let pack = Packing::new(window.0, window.1, MIN_ROWS + 1).unwrap();
        let make = |dense| {
            vec![Stream {
                times: (0..MIN_ROWS)
                    .map(|i| pack.encode(if dense { 0 } else { i as u32 }, (i % 2) as i64 * GAP))
                    .collect(),
                purchases: Vec::new(),
            }]
        };
        let active = gather(make(true), (MIN_ROWS, 0), pack, window);
        assert_eq!(active.hot, Some(0));
        assert_eq!(
            active.sessions,
            vec![Span {
                start: 0,
                end: GAP as u64,
                count: MIN_ROWS
            }]
        );
        assert_eq!(active.keys, vec![0]);
        for fallback in [
            gather(make(false), (MIN_ROWS, 0), pack, window),
            gather(make(true), (MIN_ROWS, 0), pack, (0, GAP * (MAX_BINS as i64 + 1))),
        ] {
            assert_eq!(fallback.hot, None);
            assert!(fallback.sessions.is_empty());
            assert_eq!(fallback.keys.len(), MIN_ROWS);
        }
    }

    #[test]
    fn disordered_tiles_bridge_using_max_end() {
        let key = |ts| Wide { person: 42, ts };
        let start = -2 * GAP;
        let end = 2 * GAP;
        let distant = end + GAP + 1;
        let pieces = vec![
            Stream {
                times: vec![key(end), key(start), key(-GAP), key(start)].repeat(MIN_ROWS / 4),
                purchases: vec![],
            },
            Stream {
                times: vec![key(start + 1), key(GAP)],
                purchases: vec![Purchase {
                    key: key(0),
                    price: 20,
                    cat: 1,
                }],
            },
            Stream {
                times: vec![key(distant), Wide { person: 43, ts: 0 }],
                purchases: vec![],
            },
        ];
        let got = gather(pieces, (MIN_ROWS + 5, 1), (), (start, distant + 1));
        assert_eq!(got.hot, Some(42));
        assert_eq!(
            got.sessions
                .iter()
                .map(|s| (s.start.ts, s.end.ts, s.count))
                .collect::<Vec<_>>(),
            [(start, end, MIN_ROWS + 3), (distant, distant, 1)]
        );
        assert_eq!(got.keys.len(), 3);
        assert_eq!(got.purchases.len(), 1);
        assert_eq!(got.purchases[0].key.ts, 0);
    }
}
