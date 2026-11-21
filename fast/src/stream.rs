//! Most events carry only a temporal key. Purchases carry their own full
//! payload until dedup, then contribute a key to the temporal stream as well.
use super::{Identity, People};
use crate::ts::Moment;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Wide {
    pub person: u64,
    pub ts: Moment,
}

#[derive(Clone, Copy)]
pub(super) struct Packing {
    origin: Moment,
    bits: u32,
    mask: u64,
}

impl Packing {
    pub fn new(ws: Moment, we: Moment, ranks: usize) -> Option<Self> {
        let span = (we.code() - ws.code() - 1).try_into().ok()?;
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
    pub fn encode(self, rank: u32, ts: Moment) -> u64 {
        (u64::from(rank) << self.bits) | (ts.code() - self.origin.code()) as u64
    }
}

pub(super) trait Temporal: Copy + Ord {
    type Layout: Copy;
    fn person(self, layout: Self::Layout) -> u64;
    fn ts(self, layout: Self::Layout) -> Moment;
    // Only compared within one person's ordered keys.
    fn joins_after(self, prev: Self, layout: Self::Layout) -> bool;
    fn name(person: u64, people: &People) -> u64;
}

impl Temporal for Wide {
    type Layout = ();

    #[inline]
    fn person(self, (): ()) -> u64 {
        self.person
    }

    #[inline]
    fn ts(self, (): ()) -> Moment {
        self.ts
    }

    #[inline]
    fn joins_after(self, prev: Self, (): ()) -> bool {
        self.ts.joins_after(prev.ts)
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
    fn ts(self, pack: Packing) -> Moment {
        Moment::from_code(pack.origin.code() + i128::from(self & pack.mask))
    }

    #[inline]
    fn joins_after(self, prev: Self, pack: Packing) -> bool {
        let a = pack.origin.code() + i128::from(self & pack.mask);
        let b = pack.origin.code() + i128::from(prev & pack.mask);
        let minutes = a.div_euclid(65_536) - b.div_euclid(65_536);
        minutes < 30 || (minutes == 30 && a.rem_euclid(65_536) <= b.rem_euclid(65_536))
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
        ts: Moment,
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
        let p = Packing::new(Moment::from_epoch_ms(-5000), Moment::from_epoch_ms(5000), 4).unwrap();
        assert!(p.encode(1, Moment::from_epoch_ms(-5000)) > p.encode(0, Moment::from_epoch_ms(4999)));
        assert!(p.encode(1, Moment::from_epoch_ms(-1)) < p.encode(1, Moment::from_epoch_ms(0)));
        assert_eq!(p.encode(3, Moment::from_epoch_ms(-5000)).ts(p).epoch_ms(), -5000);
        assert_eq!(p.encode(3, Moment::from_epoch_ms(4999)).ts(p).epoch_ms(), 4999);
        assert_eq!(p.encode(3, Moment::from_epoch_ms(4999)).person(p), 3);
        assert!(Packing::new(Moment::from_epoch_ms(i64::MIN + 1), Moment::from_epoch_ms(i64::MAX), 4,).is_none());
        assert!(Packing::new(Moment::from_epoch_ms(0), Moment::from_epoch_ms(1i64 << 48), 1 << 17,).is_none());
        let p = Packing::new(
            Moment::from_epoch_ms(0),
            Moment::from_epoch_ms(1i64 << 30),
            u32::MAX as usize,
        )
        .unwrap();
        assert_eq!(
            p.encode(u32::MAX - 1, Moment::from_epoch_ms((1i64 << 30) - 1))
                .person(p),
            (u32::MAX - 1) as u64
        );
    }

    #[test]
    fn packed_components_resolve_only_the_exact_minute_boundary() {
        let p = Packing::new(Moment::from_epoch_ms(-3_600_000), Moment::from_epoch_ms(3_600_000), 2).unwrap();
        let start = p.encode(1, Moment::from_epoch_ms(-1_800_000));
        let exact = p.encode(1, Moment::from_epoch_ms(0));
        let over = p.encode(1, Moment::from_epoch_ms(1));
        let same_minute = p.encode(1, Moment::from_epoch_ms(59_999));
        assert!(exact.joins_after(start, p));
        assert!(!over.joins_after(start, p));
        assert!(same_minute.joins_after(exact, p));
    }
}
