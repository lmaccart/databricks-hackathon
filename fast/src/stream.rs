//! Most events carry only a temporal key. Purchases carry their own full
//! payload until dedup, then contribute a key to the temporal stream as well.
use super::{Identity, NS, People};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Wide {
    pub person: u64,
    pub ts: i64,
}

#[derive(Clone, Copy)]
pub(super) struct Packing(pub i64);

impl Packing {
    pub fn new(ws: i64, we: i64) -> Option<Self> {
        u32::try_from(i128::from(we) - i128::from(ws) - 1).ok()?;
        Some(Self(ws))
    }

    #[inline]
    pub fn encode(self, rank: u32, ts: i64) -> u64 {
        // Destination supplies the low byte of rank. The top byte remains
        // zero, leaving u64::MAX available as a dedup tombstone.
        (u64::from(rank >> NS.trailing_zeros()) << 32) | (ts - self.0) as u64
    }
}

pub(super) trait Temporal: Copy + Ord {
    type Layout: Copy;
    fn ts(self, layout: Self::Layout) -> i64;
    // Only compared within one person's ordered keys.
    fn gap(self, prev: Self) -> u64;
}

impl Temporal for Wide {
    type Layout = ();

    #[inline]
    fn ts(self, (): ()) -> i64 {
        self.ts
    }

    #[inline]
    fn gap(self, prev: Self) -> u64 {
        self.ts.abs_diff(prev.ts)
    }
}

impl Temporal for u32 {
    type Layout = Packing;

    #[inline]
    fn ts(self, pack: Packing) -> i64 {
        pack.0 + i64::from(self)
    }

    #[inline]
    fn gap(self, prev: Self) -> u64 {
        u64::from(self - prev)
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
            Identity::Ranked(rank) => {
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
    fn packing_checks_offset_width_and_keeps_dedup_sentinel_outside_domain() {
        let p = Packing::new(-5000, (1i64 << 32) - 5000).unwrap();
        assert_eq!(p.encode(999, -5000) as u32, 0);
        assert_eq!((p.encode(999, -5001 + (1i64 << 32)) as u32).ts(p), -5001 + (1i64 << 32));
        assert_eq!(p.encode(u32::MAX, -5000) >> 32, 0x00ff_ffff);
        assert_ne!(p.encode(u32::MAX, -5001 + (1i64 << 32)), u64::MAX);
        assert!(Packing::new(-5000, (1i64 << 32) - 4999).is_none());
        assert!(Packing::new(i64::MIN + 1, i64::MAX).is_none());
    }
}
