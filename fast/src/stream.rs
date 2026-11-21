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
    fn sort_keys(values: &mut [Self]);
    fn sort_purchases(purchases: &mut [Purchase<Self>]);
    fn person(self, layout: Self::Layout) -> u64;
    fn ts(self, layout: Self::Layout) -> i64;
    // Only compared within one person's ordered keys.
    fn gap(self, prev: Self) -> u64;
    fn name(person: u64, people: &People) -> u64;
}

impl Temporal for Wide {
    type Layout = ();

    fn sort_keys(values: &mut [Self]) {
        #[cfg(feature = "highway-sort")]
        {
            const SIGN: u64 = 1 << 63;
            // This is an exact 128-bit (person, ordered timestamp) key; no
            // truncated identity or global rank can merge distinct people.
            let mut keys: Vec<u128> = values
                .iter()
                .map(|wide| (u128::from(wide.person) << 64) | u128::from((wide.ts as u64) ^ SIGN))
                .collect();
            vqsort_rs::sort(&mut keys);
            for (wide, key) in values.iter_mut().zip(keys) {
                *wide = Self {
                    person: (key >> 64) as u64,
                    ts: (key as u64 ^ SIGN) as i64,
                };
            }
        }
        #[cfg(not(feature = "highway-sort"))]
        values.sort_unstable();
    }

    fn sort_purchases(purchases: &mut [Purchase<Self>]) {
        purchases.sort_unstable_by_key(|purchase| purchase.key);
    }

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
}

impl Temporal for u64 {
    type Layout = Packing;

    fn sort_keys(values: &mut [Self]) {
        #[cfg(feature = "highway-sort")]
        vqsort_rs::sort(values);
        #[cfg(not(feature = "highway-sort"))]
        values.sort_unstable();
    }

    fn sort_purchases(purchases: &mut [Purchase<Self>]) {
        #[cfg(feature = "highway-sort")]
        {
            use vqsort_rs::{Key64Value64, sort};

            // Highway compares only `key` for K64V64. Equal-time payload
            // order is immaterial, but the full payload must travel with it.
            let mut pairs: Vec<_> = purchases
                .iter()
                .map(|purchase| Key64Value64 {
                    key: purchase.key,
                    value: (u64::from(purchase.price) << 32) | u64::from(purchase.cat),
                })
                .collect();
            sort(&mut pairs);
            for (purchase, pair) in purchases.iter_mut().zip(pairs) {
                purchase.key = pair.key;
                purchase.price = (pair.value >> 32) as u32;
                purchase.cat = pair.value as u32;
            }
        }
        #[cfg(not(feature = "highway-sort"))]
        purchases.sort_unstable_by_key(|purchase| purchase.key);
    }

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
    fn exact_temporal_sort_preserves_full_unsigned_people_and_signed_times() {
        let mut input = vec![
            Wide {
                person: u64::MAX,
                ts: i64::MIN,
            },
            Wide {
                person: 0,
                ts: i64::MAX,
            },
            Wide {
                person: 1 << 63,
                ts: -1,
            },
            Wide {
                person: 1 << 63,
                ts: i64::MIN,
            },
            Wide {
                person: 1 << 63,
                ts: i64::MAX,
            },
            Wide {
                person: 1 << 63,
                ts: -1,
            },
        ];
        let mut expected = input.clone();
        expected.sort_unstable();

        <Wide as Temporal>::sort_keys(&mut input);
        assert!(input.iter().eq(expected.iter()));
    }

    #[test]
    fn packed_temporal_sort_covers_unsigned_edges_and_duplicates() {
        let mut input = vec![u64::MAX, 0, 1 << 63, 7, 1 << 63, u64::MAX, 1];
        let mut expected = input.clone();
        expected.sort_unstable();

        <u64 as Temporal>::sort_keys(&mut input);
        assert_eq!(input, expected);
    }

    #[test]
    fn packed_purchase_sort_keeps_payloads_attached_to_equal_time_keys() {
        let mut input = vec![
            Purchase {
                key: u64::MAX,
                price: 7,
                cat: 3,
            },
            Purchase {
                key: 4,
                price: 9,
                cat: 1,
            },
            Purchase {
                key: 4,
                price: 2,
                cat: 8,
            },
            Purchase {
                key: 0,
                price: 1,
                cat: 0,
            },
        ];
        let expected_payloads: Vec<_> = input.iter().map(|p| (p.price, p.cat)).collect();

        <u64 as Temporal>::sort_purchases(&mut input);

        assert!(input.windows(2).all(|pair| pair[0].key <= pair[1].key));
        let mut actual_payloads: Vec<_> = input.iter().map(|p| (p.price, p.cat)).collect();
        let mut expected_payloads = expected_payloads;
        actual_payloads.sort_unstable();
        expected_payloads.sort_unstable();
        assert_eq!(actual_payloads, expected_payloads);
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
