//! Most events carry only a temporal key. Purchases carry their own full
//! payload until dedup, then contribute a key to the temporal stream as well.
use super::{GAP, Identity, People};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Wide {
    pub person: u64,
    pub ts: i64,
}

#[derive(Clone, Copy)]
pub(super) struct Packing {
    origin: i64,
    bits: u32,
    mask: u64,
}

impl Packing {
    pub fn new(ws: i64, we: i64, ranks: usize) -> Option<Self> {
        let span = (we as i128 - ws as i128 - 1).try_into().ok()?;
        let bits = u64::BITS - u64::leading_zeros(span);
        let rank_bits = usize::BITS - ranks.checked_sub(1)?.leading_zeros();
        // The top bit stays clear, leaving u64::MAX as a tombstone.
        if bits + rank_bits >= u64::BITS {
            return None;
        }
        Some(Self {
            origin: ws,
            bits,
            mask: (1 << bits) - 1,
        })
    }

    #[inline]
    pub fn encode(self, rank: u32, ts: i64) -> u64 {
        (u64::from(rank) << self.bits) | (ts as u64).wrapping_sub(self.origin as u64)
    }
}

pub(super) trait Temporal: Copy + Ord {
    type Layout: Copy;
    fn person(self, layout: Self::Layout) -> u64;
    fn ts(self, layout: Self::Layout) -> i64;
    fn at(self, ts: i64, layout: Self::Layout) -> Self;
    // Only compared within one person's ordered keys.
    fn gap(self, prev: Self) -> u64;
    fn name(person: u64, people: &People) -> u64;
}

impl Temporal for Wide {
    type Layout = ();

    #[inline]
    fn person(self, (): ()) -> u64 {
        self.person
    }

    #[inline]
    fn ts(self, (): ()) -> i64 {
        self.ts
    }

    #[inline]
    fn at(self, ts: i64, (): ()) -> Self {
        Self { ts, ..self }
    }

    #[inline]
    fn gap(self, prev: Self) -> u64 {
        self.ts.abs_diff(prev.ts)
    }

    #[inline]
    fn name(person: u64, _: &People) -> u64 {
        person
    }
}

impl Temporal for u64 {
    type Layout = Packing;

    #[inline]
    fn person(self, pack: Packing) -> u64 {
        self >> pack.bits
    }

    #[inline]
    fn ts(self, pack: Packing) -> i64 {
        (self & pack.mask).wrapping_add(pack.origin as u64) as i64
    }

    #[inline]
    fn at(self, ts: i64, pack: Packing) -> Self {
        (self & !pack.mask) | (ts as u64).wrapping_sub(pack.origin as u64)
    }

    #[inline]
    fn gap(self, prev: Self) -> u64 {
        self - prev
    }

    #[inline]
    fn name(rank: u64, people: &People) -> u64 {
        people.original(rank as u32)
    }
}

#[derive(Clone, Copy)]
pub(super) struct Purchase<T> {
    pub key: T,
    pub price: u32,
    pub cat: u32,
}

pub(super) struct Stream<T> {
    pub times: Vec<T>,
    pub purchases: Vec<Purchase<T>>,
}

impl<T> Default for Stream<T> {
    fn default() -> Self {
        Self {
            times: Vec::new(),
            purchases: Vec::new(),
        }
    }
}

impl<T: Copy> Stream<T> {
    #[inline]
    fn push(&mut self, key: T, price: u32, cat: u32) -> usize {
        if price == 0 {
            let i = self.times.len();
            self.times.push(key);
            i
        } else {
            let i = self.purchases.len();
            self.purchases.push(Purchase { key, price, cat });
            i
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Span<T> {
    pub start: T, // exact person and start time
    pub end: i64,
    pub count: i64,
}

pub(super) struct Local<T> {
    pub spans: Vec<Span<T>>,
    pub purchases: Vec<Purchase<T>>,
}

pub(super) fn sort_events<T: Temporal>(pieces: Vec<Stream<T>>) -> (Vec<T>, Vec<Purchase<T>>) {
    let np = pieces.iter().map(|s| s.purchases.len()).sum::<usize>();
    let ne = pieces.iter().map(|s| s.times.len()).sum::<usize>() + np;
    let mut evs = Vec::with_capacity(ne);
    let mut purchases = Vec::with_capacity(np);
    for s in pieces {
        evs.extend_from_slice(&s.times);
        evs.extend(s.purchases.iter().map(|p| p.key));
        purchases.extend_from_slice(&s.purchases);
    }
    evs.sort_unstable();
    (evs, purchases)
}

pub(super) fn local_spans<T: Temporal>(mut evs: &[T], layout: T::Layout) -> impl Iterator<Item = Span<T>> {
    std::iter::from_fn(move || {
        let &start = evs.first()?;
        let person = start.person(layout);
        let mut m = 1;
        while m < evs.len() && evs[m].person(layout) == person && evs[m].gap(evs[m - 1]) <= GAP as u64 {
            m += 1;
        }
        let end = evs[m - 1].ts(layout);
        evs = &evs[m..];
        Some(Span {
            start,
            end,
            count: m as i64,
        })
    })
}

pub(super) fn compress<T: Temporal>(pieces: Vec<Stream<T>>, layout: T::Layout) -> Local<T> {
    let (evs, purchases) = sort_events(pieces);
    Local {
        spans: local_spans(&evs, layout).collect(),
        purchases,
    }
}

pub(super) fn stitch<T: Temporal>(mut spans: &[Span<T>], layout: T::Layout) -> impl Iterator<Item = Span<T>> {
    std::iter::from_fn(move || {
        let (&first, rest) = spans.split_first()?;
        spans = rest;
        let mut session = first;
        let person = session.start.person(layout);
        // Local spans cannot hide a gap. Joining consecutive endpoints
        // preserves that invariant, including chains through more than two slabs.
        while let Some((&next, rest)) = spans.split_first() {
            if next.start.person(layout) != person || next.start.ts(layout) > session.end.saturating_add(GAP) {
                break;
            }
            session.end = session.end.max(next.end);
            session.count += next.count;
            spans = rest;
        }
        Some(session)
    })
}

#[derive(Default)]
pub(super) struct Payload {
    pub packed: Stream<u64>,
    pub wide: Stream<Wide>,
}

impl Payload {
    #[inline]
    pub fn push(
        &mut self,
        identity: Identity,
        ts: i64,
        price: u32,
        cat: u32,
        packing: Option<Packing>,
        people: &People,
    ) -> usize {
        let purchase = usize::from(price != 0);
        let person = match identity {
            Identity::Linked(routed) => {
                let rank = routed as u32;
                if let Some(p) = packing {
                    return self.packed.push(p.encode(rank, ts), price, cat) * 4 + purchase;
                }
                people.original(rank)
            }
            Identity::Standalone(person) => person,
        };
        self.wide.push(Wide { person, ts }, price, cat) * 4 + 2 + purchase
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packing_checks_total_width_and_preserves_signed_millisecond_order() {
        let p = Packing::new(-5000, 5000, 4).unwrap();
        assert!(p.encode(1, -5000) > p.encode(0, 4999));
        assert!(p.encode(1, -1) < p.encode(1, 0));
        assert_eq!(p.encode(3, -5000).ts(p), -5000);
        assert_eq!(p.encode(3, 4999).ts(p), 4999);
        assert_eq!(p.encode(3, 4999).person(p), 3);
        assert!(Packing::new(i64::MIN + 1, i64::MAX, 4).is_none());
        assert!(Packing::new(0, 1i64 << 48, 1 << 17).is_none());
        let p = Packing::new(0, 1i64 << 31, u32::MAX as usize).unwrap();
        assert_eq!(
            p.encode(u32::MAX - 1, (1i64 << 31) - 1).person(p),
            (u32::MAX - 1) as u64
        );
    }

    #[test]
    fn stitch_keeps_max_end_and_stops_at_exact_person_or_gap_plus_one() {
        let span = |person, ts, end, count| Span {
            start: Wide { person, ts },
            end,
            count,
        };
        let local = [
            span(1, 0, GAP, 2),
            span(1, 1, 1, 1),
            span(1, GAP * 2, GAP * 2, 1),
            span(1, GAP * 3 + 1, GAP * 3 + 1, 1),
            span(2, GAP * 3 + 1, GAP * 3 + 1, 1),
            span(3, i64::MAX - GAP, i64::MAX - 1, 2),
            span(3, i64::MAX, i64::MAX, 1),
        ];
        let got: Vec<_> = stitch(&local, ())
            .map(|s| (s.start.person, s.start.ts, s.end, s.count))
            .collect();
        assert_eq!(
            got,
            [
                (1, 0, GAP * 2, 4),
                (1, GAP * 3 + 1, GAP * 3 + 1, 1),
                (2, GAP * 3 + 1, GAP * 3 + 1, 1),
                (3, i64::MAX - GAP, i64::MAX, 3),
            ]
        );
    }
}
