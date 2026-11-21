//! Most events carry only a temporal key. Purchases carry their own full
//! payload until dedup, then contribute a key to the temporal stream as well.
use super::{Identity, People, UserName};

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

// The offset ranges encode suffix length as well as value, preserving zero padding.
const RADIX36_TOKEN_BITS: u32 = 42;
const RADIX36_OFFSETS: [u64; 9] = [
    0,
    1,
    37,
    1333,
    47_989,
    1_727_605,
    62_193_781,
    2_238_976_117,
    80_603_140_213,
];
const RADIX36_END: u64 = 2_821_109_907_456 + RADIX36_OFFSETS[8];

#[derive(Clone, Copy)]
pub(super) struct RadixLayout {
    prefix: u8,
    origin: i64,
    time_bits: u32,
    time_mask: u64,
    local_bits: u32,
    local_mask: u64,
    owner_id: u64,
}

impl RadixLayout {
    pub fn new(origin: i64, end: i64, prefix: Option<u8>) -> Option<Self> {
        let prefix = prefix?;
        let span: u64 = (end as i128 - origin as i128).try_into().ok()?;
        let time_bits = u64::BITS - u64::leading_zeros(span.saturating_sub(1));
        if time_bits >= u64::BITS {
            return None;
        }
        // High token bits name the owner subshard; the low bits share the key with time.
        let local_bits = RADIX36_TOKEN_BITS.min(u64::BITS - time_bits);
        let local_mask = (1u64 << local_bits) - 1;
        Some(Self {
            prefix,
            origin,
            time_bits,
            time_mask: (1u64 << time_bits).wrapping_sub(1),
            local_bits,
            local_mask,
            owner_id: 0,
        })
    }

    pub fn for_owner(self, owner_id: u64) -> Self {
        Self { owner_id, ..self }
    }

    #[inline]
    pub fn encode(self, bytes: &[u8]) -> Option<u64> {
        if bytes.first().copied()? != self.prefix {
            return None;
        }
        let suffix = &bytes[1..];
        if suffix.len() > 8 {
            return None;
        }
        let mut value = 0u64;
        for &b in suffix {
            let digit = match b {
                b'0'..=b'9' => u64::from(b - b'0'),
                b'a'..=b'z' => u64::from(b - b'a' + 10),
                _ => return None,
            };
            value = value * 36 + digit;
        }
        Some(RADIX36_OFFSETS[suffix.len()] + value)
    }

    #[inline]
    pub fn owner(self, id: u64) -> u64 {
        id >> self.local_bits
    }

    pub fn local_id(self, id: u64) -> u64 {
        id & self.local_mask
    }

    #[inline]
    pub fn key(self, id: u64, ts: i64) -> CompactKey {
        let delta = ts.wrapping_sub(self.origin) as u64;
        debug_assert!(delta <= self.time_mask);
        CompactKey(((id & self.local_mask) << self.time_bits) | delta)
    }

    pub fn decode(self, id: u64, out: &mut Vec<u8>) {
        debug_assert!(id < RADIX36_END);
        out.push(self.prefix);
        let len = (0..8).find(|&n| id < RADIX36_OFFSETS[n + 1]).unwrap_or(8);
        let mut value = id - RADIX36_OFFSETS[len];
        let start = out.len();
        out.resize(start + len, b'0');
        for slot in out[start..].iter_mut().rev() {
            let digit = (value % 36) as u8;
            *slot = if digit < 10 { b'0' + digit } else { b'a' + digit - 10 };
            value /= 36;
        }
        debug_assert_eq!(value, 0);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct CompactKey(u64);

#[derive(Clone, Copy)]
pub(super) struct CompactEvent {
    pub id: u64,
    pub ts: i64,
}

#[derive(Clone, Copy)]
pub(super) struct CompactPurchase {
    pub id: u64,
    pub ts: i64,
    pub price: u32,
    pub cat: u32,
}

#[derive(Clone, Copy)]
pub(super) struct CompactClick {
    pub id: u64,
    pub ts: i64,
    pub cid: u64,
    pub camp: u32,
}

#[derive(Default)]
pub(super) struct CompactStream {
    pub times: Vec<CompactEvent>,
    pub purchases: Vec<CompactPurchase>,
}

impl CompactStream {
    #[inline]
    fn push(&mut self, id: u64, ts: i64, price: u32, cat: u32) -> usize {
        if price == 0 {
            let i = self.times.len();
            self.times.push(CompactEvent { id, ts });
            i
        } else {
            let i = self.purchases.len();
            self.purchases.push(CompactPurchase { id, ts, price, cat });
            i
        }
    }
}

pub(super) trait Temporal: Copy + Ord {
    type Layout: Copy;
    fn person(self, layout: Self::Layout) -> u64;
    fn ts(self, layout: Self::Layout) -> i64;
    // Only compared within one person's ordered keys.
    fn gap(self, prev: Self) -> u64;
    fn name(person: u64, layout: Self::Layout, people: &People) -> UserName;
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
    fn name(person: u64, (): (), _: &People) -> UserName {
        UserName::Exact(person)
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
    fn name(rank: u64, _: Packing, people: &People) -> UserName {
        UserName::Exact(people.original(rank as u32))
    }
}

impl Temporal for CompactKey {
    type Layout = RadixLayout;

    #[inline]
    fn person(self, layout: RadixLayout) -> u64 {
        self.0 >> layout.time_bits
    }

    #[inline]
    fn ts(self, layout: RadixLayout) -> i64 {
        ((self.0 & layout.time_mask).wrapping_add(layout.origin as u64)) as i64
    }

    #[inline]
    fn gap(self, prev: Self) -> u64 {
        self.0 - prev.0
    }

    #[inline]
    fn name(local: u64, layout: RadixLayout, _: &People) -> UserName {
        let id = (layout.owner_id << layout.local_bits) | local;
        UserName::Radix36(id)
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
    pub radix36: CompactStream,
}

#[derive(Clone, Copy)]
pub(super) struct RouteKey {
    pub packing: Option<Packing>,
    pub radix_id: Option<u64>,
}

impl Payload {
    #[inline]
    pub fn push(
        &mut self,
        identity: Identity,
        ts: i64,
        price: u32,
        cat: u32,
        route: RouteKey,
        people: &People,
    ) -> usize {
        let purchase = usize::from(price != 0);
        let person = match identity {
            Identity::Linked(routed) => {
                let rank = routed as u32;
                if let Some(p) = route.packing {
                    return self.packed.push(p.encode(rank, ts), price, cat) * 6 + purchase;
                }
                people.original(rank)
            }
            Identity::Standalone(_) if let Some(radix_id) = route.radix_id => {
                return self.radix36.push(radix_id, ts, price, cat) * 6 + 4 + purchase;
            }
            Identity::Standalone(person) => person,
        };
        self.wide.push(Wide { person, ts }, price, cat) * 6 + 2 + purchase
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radix36_codes_are_lossless_and_guarded() {
        let layout = RadixLayout::new(0, 21 * 86_400_000, Some(b'u')).unwrap();
        for suffix in [
            b"".as_slice(),
            b"0",
            b"00",
            b"00000000",
            b"00000001",
            b"zzzzzzzz",
            b"01234567",
        ] {
            let mut id = b"u".to_vec();
            id.extend_from_slice(suffix);
            let token = layout.encode(&id).unwrap();
            let mut decoded = Vec::new();
            layout.decode(token, &mut decoded);
            assert_eq!(decoded, id);
        }
        for id in [b"U1".as_slice(), b"uA", b"u_1", b"u\0", b"u123456789", b"v1"] {
            assert_eq!(layout.encode(id), None);
        }
        assert!(RadixLayout::new(i64::MIN, i64::MAX, Some(b'u')).is_none());
    }

    #[test]
    fn radix36_subshards_round_trip_identity_and_millisecond_time() {
        let layout = RadixLayout::new(0, 21 * 86_400_000, Some(b'u')).unwrap();
        let first = layout.encode(b"u00000000").unwrap();
        let second = layout.encode(b"u00000001").unwrap();
        assert_eq!(layout.owner(first), layout.owner(second));
        let owner_layout = layout.for_owner(layout.owner(first));
        let a = owner_layout.key(first, 1234);
        let a_next = owner_layout.key(first, 1235);
        let b = owner_layout.key(second, 1235);
        assert_eq!(a.person(owner_layout), layout.local_id(first));
        assert_eq!(b.person(owner_layout), layout.local_id(second));
        assert_eq!(a.ts(owner_layout), 1234);
        assert_eq!(b.ts(owner_layout), 1235);
        assert_eq!(a_next.gap(a), 1);

        let other = layout.encode(b"uzzzzzzzz").unwrap();
        assert_ne!(layout.owner(first), layout.owner(other));
        assert_eq!(
            (layout.owner(other) << layout.local_bits) | layout.local_id(other),
            other
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
