//! Send payloads directly to the session partition. A separate, smaller
//! stream of claims (ID, ingest time, payload location) decides which payloads
//! must be removed. In particular, an invalid copy has a claim but no payload.
use super::*;
use crate::stream::{Purchase, Wide};
use std::hash::{BuildHasherDefault, Hasher};

mod tiles;
mod word;
use tiles::Tiles;

#[derive(Clone, Copy)]
struct Claim {
    key_and_target: u64,
    order_and_target: u64,
}

// An emitted event has at least six commas. At most one row extends past
// CHUNK. Packed slots include all first extents and at most one partial
// overflow tile per shard. The slot, stream kind and shard still fit.
const _: () = assert!(NS == 256 && (CHUNK / 6 + 1 + NS * (tiles::FIRST + tiles::TILE)) * 4 * NS < u32::MAX as usize);
const NONE: u32 = u32::MAX;
const KEY_BITS: u32 = u64::BITS - NS.trailing_zeros();
const KEY_MASK: u64 = (1 << KEY_BITS) - 1;
const TARGET_HI_BITS: u32 = 32 - NS.trailing_zeros();
const TARGET_HI_MASK: u64 = (1 << TARGET_HI_BITS) - 1;
// Signed 39-bit seconds cover every four-digit F1 year. In the 40-bit field,
// the two high bits are 00/11 for seconds and 01 for a side-table index.
const SPILL_TAG: u64 = 1 << 38;
const _: () = assert!(std::mem::size_of::<Claim>() == 16 && ((CHUNK / 6 + 1) as u64) < SPILL_TAG);

impl Claim {
    #[inline]
    fn order(ingest: i64, seconds: i64, spill: &mut Vec<i64>) -> u64 {
        if seconds != BAD {
            debug_assert!((-(SPILL_TAG as i64)..SPILL_TAG as i64).contains(&seconds));
            seconds as u64
        } else {
            let i = spill.len() as u64;
            spill.push(ingest);
            SPILL_TAG | i
        }
    }

    #[inline]
    fn new(mixed: u64, target: u32, order: u64) -> Self {
        Self {
            key_and_target: (mixed & KEY_MASK) | (u64::from(target & (NS as u32 - 1)) << KEY_BITS),
            order_and_target: (order << TARGET_HI_BITS) | u64::from(target >> NS.trailing_zeros()),
        }
    }

    #[inline]
    fn key(self) -> u64 {
        self.key_and_target & KEY_MASK
    }

    #[inline]
    fn target(self) -> u32 {
        (((self.order_and_target & TARGET_HI_MASK) << NS.trailing_zeros()) | (self.key_and_target >> KEY_BITS)) as u32
    }

    #[inline]
    fn ingest(self, spill: &[i64]) -> i64 {
        let order = (self.order_and_target as i64) >> TARGET_HI_BITS;
        if order >= SPILL_TAG as i64 {
            spill[(order as u64 ^ SPILL_TAG) as usize]
        } else {
            order * 1000
        }
    }
}

#[derive(Default)]
struct ClaimHasher(u64);

impl Hasher for ClaimHasher {
    fn write(&mut self, _: &[u8]) {
        unreachable!("the claim table has only u64 keys");
    }

    #[inline]
    fn write_u64(&mut self, key: u64) {
        // Keep low bits for indexing and move independent token bits into
        // the top byte for the table's fingerprint (also on 32-bit hosts).
        let tag_shift = usize::BITS - 8;
        self.0 = (key & ((1 << tag_shift) - 1)) | (((key >> 48) & 255) << tag_shift);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy)]
struct Location {
    batch: usize,
    target: u32,
}

#[derive(Clone, Copy, Default)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
struct Pending {
    claim_key: u64,
    uid: u64, // zero is never the key of a nonempty ID
    hash: u64,
    ts: i64,
    claim_order: u64,
    price: u32,
    cat: u32,
}

#[inline]
fn decode_fields<'a>(
    r: &[&'a [u8]; 8],
    sm: &Small,
    codec: &Codec<'a>,
    (ws, we, clock): (i64, i64, &ManifestClock),
    spill: &mut Vec<i64>,
) -> Option<Pending> {
    if r[0].is_empty() {
        return None;
    }
    let claim_key = links::mix(codec.key(r[0]));
    let (ingest, seconds) = clock.ingest_for_claim(r[3]);
    let ts = clock.parse_ts(r[2]);
    let mut next = Pending {
        claim_key,
        ts,
        claim_order: Claim::order(ingest, seconds, spill),
        ..Default::default()
    };
    if !r[1].is_empty() && !r[5].is_empty() && (ws..we).contains(&ts) && ingest.saturating_sub(ts) <= DAY {
        let purchase = match r[4] {
            b"purchase" => parse_price(r[6]),
            b"view" | b"cart" => Some(0),
            _ => None,
        };
        if let Some(price) = purchase {
            next.uid = codec.key(r[1]);
            next.hash = links::mix(next.uid);
            next.price = price;
            next.cat = if price == 0 {
                0
            } else {
                sm.product_cat.get(r[5]).unwrap_or(sm.unknown_cat)
            };
        }
    }
    Some(next)
}

pub(super) struct PackedTimes(Tiles<u64>);

impl PackedTimes {
    pub fn len(&self, shard: usize) -> usize {
        self.0.len(shard)
    }

    pub fn append_to(&self, shard: usize, times: &mut Vec<u64>) {
        if self.0.has_dead(shard) {
            for tile in self.0.slices(shard) {
                times.extend(tile.iter().copied().filter(|&k| k != u64::MAX));
            }
        } else {
            for tile in self.0.slices(shard) {
                times.extend_from_slice(tile);
            }
        }
    }
}

struct Batch {
    packed: PackedTimes,
    payload: Vec<Payload>,
}

impl Batch {
    #[inline]
    fn route(&mut self, identity: Identity, e: Pending, packing: Option<Packing>, people: &People) -> u32 {
        let (p, person) = match identity {
            Identity::Linked(routed) => {
                let p = shard(routed);
                let rank = routed as u32;
                if let Some(pack) = packing {
                    let key = pack.encode(rank, e.ts);
                    let slot = if e.price == 0 {
                        self.packed.0.push(p, key) * 4
                    } else {
                        let purchases = &mut self.payload[p].packed_purchases;
                        let i = purchases.len();
                        purchases.push(Purchase {
                            key,
                            price: e.price,
                            cat: e.cat,
                        });
                        i * 4 + 1
                    };
                    return (slot * NS + p) as u32;
                }
                (p, people.original(rank))
            }
            Identity::Standalone(person) => (shard(person), person),
        };
        let key = Wide { person, ts: e.ts };
        let slot = self.payload[p].wide.push(key, e.price, e.cat) * 4 + 2 + usize::from(e.price != 0);
        (slot * NS + p) as u32
    }

    fn prune(&mut self, locations: Vec<u32>) {
        let mut prune_payload = false;
        for target in locations {
            let x = target as usize;
            let p = x % NS;
            let slot = x / NS;
            let index = slot / 4;
            match slot % 4 {
                0 => self.packed.0.mark_dead(p, index, u64::MAX),
                kind => {
                    prune_payload = true;
                    let bucket = &mut self.payload[p];
                    match kind {
                        1 => bucket.packed_purchases[index].price = 0,
                        2 => bucket.wide.times[index].ts = BAD,
                        _ => bucket.wide.purchases[index].price = 0,
                    }
                }
            }
        }
        if prune_payload {
            for dest in &mut self.payload {
                dest.packed_purchases.retain(|p| p.price != 0);
                dest.wide.times.retain(|key| key.ts != BAD);
                dest.wide.purchases.retain(|p| p.price != 0);
            }
        }
    }
}

fn parse<'a>(
    c: &Chunk<'a>,
    sm: &Small,
    people: &People<'a>,
    window: (i64, i64, &ManifestClock),
    packing: Option<Packing>,
) -> (Batch, Tiles<Claim>, Vec<i64>) {
    let mut batch = Batch {
        packed: PackedTimes(Tiles::new()),
        payload: (0..NS).map(|_| Payload::default()).collect(),
    };
    let mut claims = Tiles::new();
    let mut spill = Vec::new();
    let mut emit = |e: Pending| {
        let mut target = NONE;
        if e.uid != 0 {
            let person = people.lookup(e.uid, e.hash);
            target = batch.route(person, e, packing, people);
        }
        // This shard supplies the omitted high bits; within it equality of
        // the stored low bits is equality of the full byte-exact ID token.
        claims.push(
            (e.claim_key >> KEY_BITS) as usize,
            Claim::new(e.claim_key, target, e.claim_order),
        );
    };
    let mut ring = [Pending::default(); 16];
    let mut n = 0usize;
    word::for_events(c, sm, &people.codec, window, &mut spill, |next| {
        if next.uid != 0 {
            people.prefetch(next.hash);
        }
        let i = n % ring.len();
        if n >= ring.len() {
            emit(ring[i]);
        }
        ring[i] = next;
        n += 1;
    });
    for i in n.saturating_sub(ring.len())..n {
        emit(ring[i % ring.len()]);
    }
    (batch, claims, spill)
}

#[inline]
fn probe_index(c: Claim, mask: usize) -> usize {
    // The mask excludes the colocated payload-shard byte.
    c.key_and_target as usize & mask
}

#[inline]
fn probe(c: Claim, mask: usize) -> (usize, u64) {
    let k = c.key_and_target;
    let bits = 1u64.rotate_left((k >> 36) as u32) | 1u64.rotate_left((k >> 42) as u32);
    (probe_index(c, mask), bits)
}

#[inline]
fn prefetch(bloom: &[(u64, u64)], c: Claim, mask: usize) {
    let ptr: *const (u64, u64) = &bloom[probe_index(c, mask)];
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use std::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
        _mm_prefetch(ptr.cast(), _MM_HINT_T0);
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        std::arch::asm!("prfm pldl1keep, [{p}]", p = in(reg) ptr,
                        options(readonly, nostack, preserves_flags));
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    let _ = ptr;
}

fn losers<'a, I: Iterator<Item = (usize, &'a [Claim])>>(
    parts: impl Fn() -> I,
    n: usize,
    spills: &[Vec<i64>],
) -> Vec<Location> {
    if n < 2 {
        return Vec::new();
    }
    // Local blocked Bloom: the second occurrence of any ID necessarily finds
    // both bits already set and marks it suspect. False positives go to an
    // EXACT key table. The second pass therefore includes the *first* claim of
    // every duplicate, too; no assumption about ingest order or validity.
    let words = n.div_ceil(8).next_power_of_two();
    let mut bloom = vec![(0u64, 0u64); words];
    let mask = (words - 1) & KEY_MASK as usize;
    let mut suspects = 0usize;
    for (_, claims) in parts() {
        for (j, &c) in claims.iter().enumerate() {
            if let Some(&next) = claims.get(j + 6) {
                prefetch(&bloom, next, mask);
            }
            let (i, b) = probe(c, mask);
            let (seen, repeated) = &mut bloom[i];
            if *seen & b == b {
                *repeated |= b;
                suspects += 1;
            }
            *seen |= b;
        }
    }
    let mut remove = Vec::new();
    if suspects == 0 {
        return remove;
    }
    let mut best = HashMap::with_capacity_and_hasher(suspects, BuildHasherDefault::<ClaimHasher>::default());
    for (batch, claims) in parts() {
        for (j, &c) in claims.iter().enumerate() {
            if let Some(&next) = claims.get(j + 6) {
                prefetch(&bloom, next, mask);
            }
            let (i, b) = probe(c, mask);
            if bloom[i].1 & b != b {
                continue;
            }
            let loc = Location {
                batch,
                target: c.target(),
            };
            let ingest = c.ingest(&spills[batch]);
            let lost = match best.entry(c.key()) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert((ingest, loc));
                    continue;
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    if ingest < e.get().0 {
                        e.insert((ingest, loc)).1
                    } else {
                        loc
                    }
                }
            };
            if lost.target != NONE {
                remove.push(lost);
            }
        }
    }
    remove
}

pub(super) fn by_person<'a>(
    chunks: &[Chunk<'a>],
    sm: &Small,
    people: &People<'a>,
    window: (i64, i64, &ManifestClock),
    packing: Option<Packing>,
    log: &impl Fn(&str),
) -> (Vec<PackedTimes>, Vec<Vec<Payload>>) {
    let parsed: Vec<_> = chunks
        .par_iter()
        .map(|c| parse(c, sm, people, window, packing))
        .collect();
    log("events parsed");
    let mut payloads = Vec::with_capacity(parsed.len());
    let mut spills = Vec::with_capacity(parsed.len());
    let mut claims = Vec::with_capacity(parsed.len());
    for (payload, cs, spill) in parsed {
        payloads.push(payload);
        spills.push(spill);
        claims.push(cs);
    }
    let removals: Vec<_> = (0..NS)
        .into_par_iter()
        .map(|shard| {
            let n = claims.iter().map(|c| c.len(shard)).sum();
            let parts = || {
                claims
                    .iter()
                    .enumerate()
                    .flat_map(|(batch, c)| c.slices(shard).map(move |s| (batch, s)))
            };
            losers(parts, n, &spills)
        })
        .collect();
    drop(claims);
    let mut by_batch: Vec<Vec<u32>> = (0..payloads.len()).map(|_| Vec::new()).collect();
    for locations in removals {
        for loc in locations {
            by_batch[loc.batch].push(loc.target);
        }
    }
    payloads
        .par_iter_mut()
        .zip(by_batch)
        .for_each(|(batch, locations)| batch.prune(locations));
    payloads.into_iter().map(|b| (b.packed, b.payload)).unzip()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bloom_alias_keeps_distinct_id_and_invalid_winner_kills_valid_loser() {
        // Same shard, probe, and exact-table fingerprint, but distinct keys.
        let id = 13 | (217 << KEY_BITS);
        let alias = id ^ (1 << 24);
        let later = Claim::new(id, 0, 3);
        let distinct = Claim::new(alias, 1, 4);
        assert_eq!(probe(later, 1023), probe(distinct, 1023));
        let mut spill = Vec::new();
        let invalid = Claim::new(id, NONE, Claim::order(1234, BAD, &mut spill));
        let first = [later, distinct];
        let second = [invalid];
        let parts = || [(0, first.as_slice()), (1, second.as_slice())].into_iter();
        let lost = losers(parts, 3, &[vec![], spill]);
        assert_eq!(lost.len(), 1);
        assert_eq!((lost[0].batch, lost[0].target), (0, 0));
    }

    #[test]
    fn claims_round_trip_exact_tokens_locators_and_millisecond_spills() {
        let clock = ManifestClock::new(parse_ts(b"2000-02-28T00:00:00Z"), parse_ts(b"2000-03-03T00:00:00Z"));
        let mut spill = Vec::new();
        for id in [b"e1".as_slice(), b"e1\0", b"e123456789", "é".as_bytes()] {
            let codec = Codec::new();
            let mixed = links::mix(codec.key(id));
            for target in [0, 255, 256, 0x5555_aaaa, NONE] {
                for s in [
                    b"0000-01-01T00:00:00Z".as_slice(),
                    b"9999-12-31T23:59:59Z",
                    b"2000-02-29T00:00:00Z",
                    b"00000001551",
                    b"2000-02-29T00:00:00+00:00",
                    b"2000-02-29T00:00:60Z",
                    b"9999-12-31T23:59:60Z",
                    b"not a timestamp",
                ] {
                    let (ms, secs) = clock.ingest_for_claim(s);
                    let claim = Claim::new(mixed, target, Claim::order(ms, secs, &mut spill));
                    assert_eq!(claim.ingest(&spill), ts::parse_ingest(s));
                    assert_eq!(claim.target(), target);
                    let reconstructed = claim.key() | ((mixed >> KEY_BITS) << KEY_BITS);
                    assert_eq!(id, codec.bytes(links::unmix(reconstructed), &mut [0; 9]));
                }
            }
        }
    }
}
