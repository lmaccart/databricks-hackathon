//! Most events carry only a temporal key. Purchases carry their own full
//! payload until dedup, then contribute a key to the temporal stream as well.
use super::{Identity, People};

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
        let span = u64::try_from(we as i128 - ws as i128 - 1)
            .ok()?
            .checked_add(crate::GAP as u64)?;
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
    fn connected(self, prev: Self) -> bool;
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
    fn gap(self, prev: Self) -> u64 {
        self.ts.abs_diff(prev.ts)
    }

    #[inline]
    fn connected(self, prev: Self) -> bool {
        self.person == prev.person && self.gap(prev) <= crate::GAP as u64
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
    fn gap(self, prev: Self) -> u64 {
        self - prev
    }

    #[inline]
    fn connected(self, prev: Self) -> bool {
        // The unused interval at the end of each rank is at least GAP:
        // the first timestamp of any other person always breaks the run.
        self.gap(prev) <= crate::GAP as u64
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
    fn adjacent_people_never_share_a_packed_session_at_stride_edges() {
        for span in [1, 128, 1_800_000, 1_800_001, 1 << 24, 1 << 31] {
            let lo = -1_000_000;
            let p = Packing::new(lo, lo + span, 4).unwrap();
            let a = p.encode(1, lo + span - 1);
            let b = p.encode(2, lo);
            assert!(b - a > crate::GAP as u64);
        }
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
        let last = (1i64 << 31) - crate::GAP;
        let p = Packing::new(0, last, u32::MAX as usize).unwrap();
        assert_eq!(p.encode(u32::MAX - 1, last - 1).person(p), (u32::MAX - 1) as u64);
    }
}
