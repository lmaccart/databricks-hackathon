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
    end: i64,
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
            end: we,
            bits,
            mask: (1 << bits) - 1,
        })
    }

    #[inline]
    pub fn encode(self, rank: u32, ts: i64) -> u64 {
        (u64::from(rank) << self.bits) | (ts as u64).wrapping_sub(self.origin as u64)
    }
}

/// Seconds use only the rank bits not already supplied by the shard and tile.
#[derive(Clone, Copy)]
pub(super) struct Seconds {
    origin: i64,
    bits: u32,
    mask: u32,
    low: u32,
    tile: u32,
}

impl Seconds {
    pub fn new(pack: Packing, rank: u32) -> Option<Self> {
        let origin = pack.origin.div_euclid(1000).checked_mul(1000)?;
        let last = (i128::from(pack.end) - 1 - i128::from(origin)) / 1000;
        let span = u32::try_from(last).ok()?;
        let bits = u32::BITS - span.leading_zeros();
        let rank_bits = 32 - bits;
        let shift = super::NS.trailing_zeros();
        let tile = (u64::from(rank) >> (shift + rank_bits)) as u32;
        Some(Self {
            origin,
            bits,
            mask: u32::MAX.checked_shr(32 - bits).unwrap_or(0),
            low: rank & (super::NS as u32 - 1),
            tile,
        })
    }

    pub fn tile(self, rank: u32) -> u32 {
        (u64::from(rank) >> (super::NS.trailing_zeros() + 32 - self.bits)) as u32
    }

    pub fn with_tile(self, tile: u32) -> Self {
        Self { tile, ..self }
    }

    pub fn encode(self, rank: u32, ts: i64) -> (u32, u16) {
        let offset = (i128::from(ts) - i128::from(self.origin)) as u64;
        let rank_bits = 32 - self.bits;
        let rank_mask = u32::MAX.checked_shr(32 - rank_bits).unwrap_or(0);
        let person = (rank >> super::NS.trailing_zeros()) & rank_mask;
        let key = ((u64::from(person) << self.bits) | (offset / 1000)) as u32;
        (key, (offset % 1000) as u16)
    }
}

pub(super) trait Temporal: Copy + Ord {
    type Layout: Copy;
    fn person(self, layout: Self::Layout) -> u64;
    fn ts(self, layout: Self::Layout) -> i64;
    fn name(person: u64, people: &People) -> u64;
    fn bounds(self, layout: Self::Layout, count: usize, detail: &[u64], cursor: &mut usize) -> (i64, i64) {
        let _ = (count, detail, cursor);
        let t = self.ts(layout);
        (t, t)
    }
}

impl Temporal for u32 {
    type Layout = Seconds;

    fn person(self, p: Seconds) -> u64 {
        let shift = super::NS.trailing_zeros();
        let local = u64::from(self) >> p.bits;
        (((u64::from(p.tile) << (32 - p.bits)) | local) << shift) | u64::from(p.low)
    }

    fn ts(self, p: Seconds) -> i64 {
        (i128::from(p.origin) + i128::from(self & p.mask) * 1000) as i64
    }

    fn name(rank: u64, people: &People) -> u64 {
        people.original(rank as u32)
    }

    fn bounds(self, p: Seconds, count: usize, detail: &[u64], cursor: &mut usize) -> (i64, i64) {
        let t = self.ts(p);
        let lo = *cursor;
        while *cursor < detail.len() && detail[*cursor] >> 10 == u64::from(self) {
            *cursor += 1;
        }
        let hi = *cursor;
        if lo == hi {
            return (t, t);
        }
        let min = if hi - lo < count { 0 } else { detail[lo] & 1023 };
        (t + min as i64, t + (detail[hi - 1] & 1023) as i64)
    }
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
    fn seconds_tiles_and_sparse_bounds_are_exact() {
        let pack = Packing::new(-1234, 20_000_000_000, 1 << 28).unwrap();
        let rank = 0x0234_56ab;
        let layout = Seconds::new(pack, rank).unwrap();
        let (key, rem) = layout.encode(rank, -1233);
        assert_eq!(rem, 767);
        assert_eq!(key.person(layout), u64::from(rank));
        let detail = vec![(u64::from(key) << 10) | 2, (u64::from(key) << 10) | 767];
        assert_eq!(key.bounds(layout, 2, &detail, &mut 0), (-1998, -1233));
        assert_eq!(key.bounds(layout, 3, &detail, &mut 0), (-2000, -1233));
        let other = rank.wrapping_add(1 << 18);
        let (k, _) = layout.encode(other, -1233);
        assert_eq!(k.person(layout.with_tile(layout.tile(other))), u64::from(other));
        assert_ne!(layout.tile(rank), layout.tile(other));
    }
}
