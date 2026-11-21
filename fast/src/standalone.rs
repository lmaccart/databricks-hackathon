//! Temporary exact ranks for the standalone stream, not identity hashes.
use super::stream::{Packing, Purchase, Stream, Wide};
use super::{Map, PCl, rs};

const PROBES: usize = 4;
const MAX_SLOTS: usize = 1 << 15; // 256 KiB per worker
const MAX_EVENTS: usize = 1 << 19; // bound speculative allocations before fallback

pub(super) struct Ranks {
    // A slot's index is also its rank, so the same array decodes it for output.
    slots: Vec<u64>,
    overflow: Map<u64, u32>,
    inverse_overflow: Vec<u64>,
    overflow_limit: usize,
}

impl Ranks {
    fn new(slots: usize, overflow_limit: usize) -> Self {
        Self {
            slots: vec![0; slots],
            overflow: Map::with_hasher(rs()),
            inverse_overflow: Vec::new(),
            overflow_limit,
        }
    }

    #[inline]
    fn probe(&self, person: u64) -> Option<(usize, bool)> {
        // The codec never emits zero for a nonempty ID, and the routing
        // permutation maps zero only to zero. All other bits are significant.
        debug_assert_ne!(person, 0);
        let mask = self.slots.len() - 1;
        for offset in 0..PROBES {
            let slot = ((person as usize).wrapping_add(offset)) & mask;
            let key = self.slots[slot];
            if key == person {
                return Some((slot, true));
            }
            if key == 0 {
                return Some((slot, false));
            }
        }
        None
    }

    #[inline]
    fn insert(&mut self, person: u64) -> Option<u32> {
        if let Some((slot, present)) = self.probe(person) {
            if !present {
                self.slots[slot] = person;
            }
            return Some(slot as u32);
        }
        self.insert_overflow(person)
    }

    #[cold]
    fn insert_overflow(&mut self, person: u64) -> Option<u32> {
        if let Some(&rank) = self.overflow.get(&person) {
            return Some(rank);
        }
        if self.inverse_overflow.len() == self.overflow_limit {
            return None;
        }
        let rank = (self.slots.len() + self.inverse_overflow.len()) as u32;
        self.inverse_overflow.push(person);
        self.overflow.insert(person, rank);
        Some(rank)
    }

    #[inline]
    fn get(&self, person: u64) -> Option<u32> {
        if let Some((slot, present)) = self.probe(person) {
            return present.then_some(slot as u32);
        }
        self.overflow.get(&person).copied()
    }

    #[inline]
    pub(super) fn original(&self, rank: u64) -> u64 {
        let slot = rank as usize;
        if slot < self.slots.len() {
            self.slots[slot]
        } else {
            self.inverse_overflow[slot - self.slots.len()]
        }
    }

    pub(super) fn clicks(&self, pieces: Vec<Vec<PCl>>) -> Vec<PCl> {
        let mut clicks = Vec::with_capacity(pieces.iter().map(Vec::len).sum());
        for piece in pieces {
            clicks.extend(piece.into_iter().filter_map(|mut c| {
                c.person = u64::from(self.get(c.person)?);
                Some(c)
            }));
        }
        clicks
    }
}

pub(super) struct Compact {
    pub ranks: Ranks,
    pub packing: Packing,
    pub times: Vec<u64>,
    pub purchases: Vec<Purchase<u64>>,
}

pub(super) fn compact(pieces: &[Stream<Wide>], (ws, we): (i64, i64)) -> Option<Compact> {
    let np = pieces.iter().map(|s| s.purchases.len()).sum::<usize>();
    let ne = pieces.iter().map(|s| s.times.len()).sum::<usize>() + np;
    if ne == 0 || ne > MAX_EVENTS {
        return None;
    }
    // Size from event counts rather than first walking the full input to
    // count people. Crowded shards fall back without modifying their input.
    let slots = (ne / 4).next_power_of_two().clamp(16, MAX_SLOTS);
    let overflow_limit = (slots / 32).clamp(4, 512);
    let packing = Packing::new(ws, we, slots + overflow_limit)?;
    let mut ranks = Ranks::new(slots, overflow_limit);
    let mut times = Vec::with_capacity(ne);
    let mut purchases = Vec::with_capacity(np);
    for piece in pieces {
        for &Wide { person, ts } in &piece.times {
            times.push(packing.encode(ranks.insert(person)?, ts));
        }
        for &Purchase {
            key: Wide { person, ts },
            price,
            cat,
        } in &piece.purchases
        {
            let key = packing.encode(ranks.insert(person)?, ts);
            times.push(key);
            purchases.push(Purchase { key, price, cat });
        }
    }
    Some(Compact {
        ranks,
        packing,
        times,
        purchases,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::Temporal;

    #[test]
    fn colliding_slots_verify_full_token_and_use_exact_overflow() {
        let mut ranks = Ranks::new(16, 2);
        // Same initial slot, wraparound, and even the same low 32 bits.
        for i in 1..=6u64 {
            let key = (i << 48) | 15;
            let rank = ranks.insert(key).unwrap();
            assert_eq!(rank, if i <= 4 { (14 + i) as u32 & 15 } else { (11 + i) as u32 });
            assert_eq!(ranks.original(u64::from(rank)), key);
        }
        for i in 1..=6 {
            let key = (i << 48) | 15;
            assert_eq!(ranks.get(key), ranks.insert(key));
        }
        let click_only = (7 << 48) | 15;
        assert_eq!(ranks.get(click_only), None);
        let click = |person| PCl {
            person,
            ts: 0,
            cid: 1,
            camp: 0,
        };
        let clicks = ranks.clicks(vec![vec![click(click_only), click((6 << 48) | 15)]]);
        assert_eq!(clicks.len(), 1);
        assert_eq!(clicks[0].person, 17);
        assert_eq!(ranks.insert(click_only), None);
    }

    #[test]
    fn compact_fuses_gather_at_timestamp_bounds_and_can_fall_back() {
        let ws = -100;
        let we = ws + (1 << 31);
        let pieces = [Stream {
            times: vec![Wide { person: 1, ts: ws }],
            purchases: vec![Purchase {
                key: Wide { person: 1, ts: we - 1 },
                price: 7,
                cat: 9,
            }],
        }];
        let c = compact(&pieces, (ws, we)).unwrap();
        assert_eq!(c.times[0].ts(c.packing), ws);
        assert_eq!(c.times[1].ts(c.packing), we - 1);
        assert_eq!(c.times[1], c.purchases[0].key);
        assert_eq!(c.ranks.original(c.times[1].person(c.packing)), 1);
        assert!(compact(&pieces, (i64::MIN + 1, i64::MAX)).is_none());
        let crowded = [Stream {
            times: (1..=9)
                .map(|i| Wide {
                    person: i << 48,
                    ts: ws,
                })
                .collect(),
            purchases: vec![],
        }];
        assert!(compact(&crowded, (ws, we)).is_none());
        assert_eq!(crowded[0].times.len(), 9);
    }
}
