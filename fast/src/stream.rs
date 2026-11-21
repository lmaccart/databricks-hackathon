//! Most events carry only a temporal key. Purchases carry their own full
//! payload until dedup, then contribute a key to the temporal stream as well.
use super::{Identity, NS, People};

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
    // Only compared within one person's ordered keys.
    fn gap(self, prev: Self) -> u64;
    fn name(person: u64, people: &People) -> u64;
    fn with_person(self, person: u64, layout: Self::Layout) -> Self;
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
    fn gap(self, prev: Self) -> u64 {
        self.ts.abs_diff(prev.ts)
    }

    #[inline]
    fn name(person: u64, _: &People) -> u64 {
        person
    }

    #[inline]
    fn with_person(self, _: u64, (): ()) -> Self {
        self
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
    fn gap(self, prev: Self) -> u64 {
        self - prev
    }

    #[inline]
    fn name(rank: u64, people: &People) -> u64 {
        people.original(rank as u32)
    }

    #[inline]
    fn with_person(self, rank: u64, pack: Packing) -> Self {
        self | (rank << pack.bits)
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
            Identity::Linked(rank) => {
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

// In this physical shard no two global root ranks can have the same quotient.
// The counts become start cursors after the prefix pass, and end offsets after
// scatter. Only the timestamps are stored in the resulting person ranges.
pub(super) fn scatter_linked(
    pieces: Vec<Stream<u64>>,
    pack: Packing,
    slots: usize,
    ne: usize,
    np: usize,
) -> (Vec<u64>, Vec<Purchase<u64>>, Vec<u32>) {
    let local = |key: u64| (key >> pack.bits) as usize / NS;
    let mut ends = vec![0u32; slots];
    for s in &pieces {
        for &key in &s.times {
            ends[local(key)] += 1;
        }
        for p in &s.purchases {
            ends[local(p.key)] += 1;
        }
    }
    let mut sum = 0;
    for n in &mut ends {
        sum += *n;
        *n = sum - *n;
    }
    debug_assert_eq!(sum as usize, ne);
    let mut times = vec![0; ne];
    let mut purchases = Vec::with_capacity(np);
    let mut put = |key: u64| {
        let cursor = &mut ends[local(key)];
        times[*cursor as usize] = key & pack.mask;
        *cursor += 1;
    };
    for s in pieces {
        for key in s.times {
            put(key);
        }
        for p in s.purchases {
            put(p.key);
            purchases.push(p);
        }
    }
    let mut start = 0;
    for &end in &ends {
        let end = end as usize;
        if end - start > 1 {
            times[start..end].sort_unstable();
        }
        start = end;
    }
    (times, purchases, ends)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::links::linked_shard;

    #[test]
    fn direct_scatter_keeps_rank_ranges_and_sorts_only_offsets() {
        let rank = 5;
        let other = rank + NS as u32 * 2;
        assert_eq!(linked_shard(rank), linked_shard(other));
        assert_ne!(linked_shard(rank), linked_shard(rank + 1));
        let pack = Packing::new(-2, 10, other as usize + 1).unwrap();
        let pieces = vec![
            Stream {
                times: vec![pack.encode(other, 1), pack.encode(rank, 4)],
                purchases: vec![Purchase {
                    key: pack.encode(rank, -2),
                    price: 1,
                    cat: 2,
                }],
            },
            Stream {
                times: vec![pack.encode(other, 0), pack.encode(rank, 4)],
                purchases: vec![Purchase {
                    key: pack.encode(other, 0),
                    price: 3,
                    cat: 4,
                }],
            },
        ];
        let (times, purchases, ends) = scatter_linked(pieces, pack, 3, 6, 2);
        assert_eq!(ends, [3, 3, 6]);
        assert_eq!(times, [0, 6, 6, 2, 2, 3]);
        assert_eq!(
            purchases.iter().map(|p| p.key).collect::<Vec<_>>(),
            [pack.encode(rank, -2), pack.encode(other, 0)]
        );
    }

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
}
