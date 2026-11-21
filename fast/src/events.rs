//! Send payloads directly to the session partition. A separate, smaller
//! stream of claims (ID, ingest time, payload location) decides which payloads
//! must be removed. In particular, an invalid copy has a claim but no payload.
use super::*;
use std::hash::{BuildHasherDefault, Hasher};

#[derive(Clone, Copy)]
struct Claim {
    key_and_target: u64,
    order_and_target: u64,
}

// An emitted event has at least six commas. At most one row extends past
// CHUNK, so the stream kind, per-stream ordinal and shard fit in the claim.
const _: () = assert!(NS == 256 && (CHUNK / 6 + 1) * 4 * NS < u32::MAX as usize);
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
struct Pending {
    claim_key: u64,
    uid: u64, // zero is never the key of a nonempty ID
    hash: u64,
    ts: i64,
    claim_order: u64,
    price: u32,
    cat: u32,
}

fn parse<'a>(
    c: &Chunk<'a>,
    sm: &Small,
    people: &People<'a>,
    window: (i64, i64, &ManifestClock),
    packing: Option<Packing>,
) -> (Vec<Payload>, Vec<Vec<Claim>>, Vec<i64>) {
    let (ws, we, clock) = window;
    let reserve = reserve_per_shard(c.data);
    let mut payload: Vec<Payload> = (0..NS)
        .map(|_| {
            let mut p = Payload::default();
            p.wide
                .times
                .reserve(if packing.is_some() { reserve / 2 } else { reserve });
            if packing.is_some() {
                p.packed.times.reserve(reserve / 2);
            }
            p
        })
        .collect();
    let mut claims: Vec<Vec<Claim>> = (0..NS).map(|_| Vec::with_capacity(reserve + reserve / 8)).collect();
    let mut spill = Vec::new();
    let mut emit = |e: Pending| {
        let mut target = NONE;
        if e.uid != 0 {
            let person = people.lookup(e.uid, e.hash);
            let p = shard(match person {
                Identity::Linked(routed) | Identity::Standalone(routed) => routed,
            });
            let slot = payload[p].push(person, e.ts, e.price, e.cat, packing, people);
            target = (slot * NS + p) as u32;
        }
        // This shard supplies the omitted high bits; within it equality of
        // the stored low bits is equality of the full byte-exact ID token.
        claims[(e.claim_key >> KEY_BITS) as usize].push(Claim::new(e.claim_key, target, e.claim_order));
    };
    let mut ring = [Pending::default(); 16];
    let mut n = 0usize;
    for_rows(c, |r| {
        if r[0].is_empty() {
            return;
        }
        let claim_key = links::mix(people.codec.key(r[0]));
        let (ingest, seconds) = clock.ingest_for_claim(r[3]);
        let ts = clock.parse_ts(r[2]);
        let mut next = Pending {
            claim_key,
            ts,
            claim_order: Claim::order(ingest, seconds, &mut spill),
            ..Default::default()
        };
        if !r[1].is_empty() && !r[5].is_empty() && (ws..we).contains(&ts) && ingest.saturating_sub(ts) <= DAY {
            let purchase = match r[4] {
                b"purchase" => parse_price(r[6]),
                b"view" | b"cart" => Some(0),
                _ => None,
            };
            if let Some(price) = purchase {
                let cat = if price == 0 {
                    0
                } else {
                    sm.product_cat.get(r[5]).unwrap_or(sm.unknown_cat)
                };
                next.uid = people.codec.key(r[1]);
                next.hash = links::mix(next.uid);
                people.prefetch(next.hash);
                next.price = price;
                next.cat = cat;
            }
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
    (payload, claims, spill)
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

fn losers_bloom(parts: Vec<(usize, Vec<Claim>)>, spills: &[Vec<i64>]) -> Vec<Location> {
    let n: usize = parts.iter().map(|(_, v)| v.len()).sum();
    // Local blocked Bloom: the second occurrence of any ID necessarily finds
    // both bits already set and marks it suspect. False positives go to an
    // EXACT key table. The second pass therefore includes the *first* claim of
    // every duplicate, too; no assumption about ingest order or validity.
    let words = n.div_ceil(8).next_power_of_two();
    let mut bloom = vec![(0u64, 0u64); words];
    let mask = (words - 1) & KEY_MASK as usize;
    let mut suspects = 0usize;
    for (_, claims) in &parts {
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
    for (batch, claims) in parts {
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

fn losers(parts: Vec<(usize, Vec<Claim>)>, spills: &[Vec<i64>]) -> Vec<Location> {
    let n: usize = parts.iter().map(|(_, v)| v.len()).sum();
    if n == 0 {
        return Vec::new();
    }
    // Ordinal + 1 reserves zero for an empty table slot. Keep the existing
    // exact algorithm as a bounded fallback for exceptionally large shards.
    if n >= u32::MAX as usize || n.checked_mul(2).and_then(usize::checked_next_power_of_two).is_none() {
        return losers_bloom(parts, spills);
    }
    let capacity = (n * 2).next_power_of_two();
    let mask = capacity - 1;
    let shift = capacity.trailing_zeros();
    // The bucket consumes the low token bits, and the entry stores the next
    // 32 bits and a winner ordinal. A matching fingerprint is never proof:
    // resolve its ordinal back to the original full 56-bit token first.
    let mut table = vec![0u64; capacity];
    let mut ends = Vec::with_capacity(parts.len());
    let mut end = 0usize;
    for (_, claims) in &parts {
        end += claims.len();
        ends.push(end);
    }
    let mut remove = Vec::new();
    let mut ordinal = 0u32;
    for (batch, claims) in &parts {
        for &c in claims {
            let key = c.key();
            let tag = (key >> shift) as u32;
            let mut slot = key as usize & mask;
            loop {
                let entry = table[slot];
                if entry == 0 {
                    table[slot] = (u64::from(tag) << 32) | u64::from(ordinal + 1);
                    break;
                }
                if (entry >> 32) as u32 == tag {
                    let old_ord = (entry as u32 - 1) as usize;
                    let part = ends.partition_point(|&end| end <= old_ord);
                    let start = if part == 0 { 0 } else { ends[part - 1] };
                    let (old_batch, old_claims) = &parts[part];
                    let old = old_claims[old_ord - start];
                    if old.key() == key {
                        let loc = Location {
                            batch: *batch,
                            target: c.target(),
                        };
                        let lost = if c.ingest(&spills[*batch]) < old.ingest(&spills[*old_batch]) {
                            table[slot] = (u64::from(tag) << 32) | u64::from(ordinal + 1);
                            Location {
                                batch: *old_batch,
                                target: old.target(),
                            }
                        } else {
                            loc
                        };
                        if lost.target != NONE {
                            remove.push(lost);
                        }
                        break;
                    }
                }
                slot = (slot + 1) & mask;
            }
            ordinal += 1;
        }
    }
    remove
}

fn remove_payloads(payload: &mut [Payload], mut locations: Vec<u32>) {
    // Each payload vector is independent, and its indices appear descending
    // under this ordering even when other shards/stream kinds are interleaved.
    locations.sort_unstable_by(|a, b| b.cmp(a));
    for target in locations {
        let x = target as usize;
        let bucket = &mut payload[x % NS];
        let slot = x / NS;
        let index = slot / 4;
        match slot % 4 {
            0 => {
                bucket.packed.times.swap_remove(index);
            }
            1 => {
                bucket.packed.purchases.swap_remove(index);
            }
            2 => {
                bucket.wide.times.swap_remove(index);
            }
            _ => {
                bucket.wide.purchases.swap_remove(index);
            }
        }
    }
}

pub(super) fn by_person<'a>(
    chunks: &[Chunk<'a>],
    sm: &Small,
    people: &People<'a>,
    window: (i64, i64, &ManifestClock),
    packing: Option<Packing>,
    log: &impl Fn(&str),
) -> Vec<Vec<Payload>> {
    let parsed: Vec<_> = chunks
        .par_iter()
        .map(|c| parse(c, sm, people, window, packing))
        .collect();
    log("events parsed");
    let mut payloads = Vec::with_capacity(parsed.len());
    let mut spills = Vec::with_capacity(parsed.len());
    let mut claims: Vec<Vec<_>> = (0..NS).map(|_| Vec::new()).collect();
    for (batch, (payload, cs, spill)) in parsed.into_iter().enumerate() {
        payloads.push(payload);
        spills.push(spill);
        for (bucket, c) in claims.iter_mut().zip(cs) {
            if !c.is_empty() {
                bucket.push((batch, c));
            }
        }
    }
    let removals: Vec<_> = claims.into_par_iter().map(|parts| losers(parts, &spills)).collect();
    let mut by_batch: Vec<Vec<u32>> = (0..payloads.len()).map(|_| Vec::new()).collect();
    for locations in removals {
        for loc in locations {
            by_batch[loc.batch].push(loc.target);
        }
    }
    payloads.par_iter_mut().zip(by_batch).for_each(|(payload, locations)| {
        remove_payloads(payload, locations);
    });
    payloads
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{Purchase, Wide};

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
        let lost = losers(vec![(0, vec![later, distinct]), (1, vec![invalid])], &[vec![], spill]);
        assert_eq!(lost.len(), 1);
        assert_eq!((lost[0].batch, lost[0].target), (0, 0));
    }

    #[test]
    fn colliding_claims_and_cross_batch_winners_are_exact() {
        // Identical low bucket bits and quotient fingerprint force witness checks.
        let first = 17;
        let alias = first | (1u64 << 48);
        let mut a = Vec::new();
        let mut b = Vec::new();
        let old = Claim::new(first, 4, Claim::order(4001, BAD, &mut a));
        let other = Claim::new(alias, 12, Claim::order(3000, 3, &mut a));
        let invalid = Claim::new(first, NONE, Claim::order(1501, BAD, &mut b));
        let tied = Claim::new(alias, 20, Claim::order(3000, 3, &mut b));
        let later = Claim::new(first, 28, Claim::order(9000, 9, &mut b));
        let lost = losers(vec![(0, vec![old, other]), (1, vec![invalid, tied, later])], &[a, b]);
        let mut locations: Vec<_> = lost.iter().map(|l| (l.batch, l.target)).collect();
        locations.sort_unstable();
        assert_eq!(locations, vec![(0, 4), (1, 20), (1, 28)]);
    }

    #[test]
    fn adversarial_clusters_match_exact_baseline() {
        let mut parts = Vec::new();
        let mut spills = Vec::new();
        for batch in 0..7 {
            let mut claims = Vec::new();
            let mut spill = Vec::new();
            for i in 0..128u64 {
                // Every distinct key has the same low 44 bits, including
                // both the table index and the stored fingerprint.
                let key = 23 | (i << 44);
                let ts = ((batch * 97 + i as usize * 31) % 503) as i64 - 250;
                let target = if (i + batch as u64).is_multiple_of(9) {
                    NONE
                } else {
                    (i * 256) as u32
                };
                claims.push(Claim::new(key, target, Claim::order(ts, BAD, &mut spill)));
            }
            parts.push((batch, claims));
            spills.push(spill);
        }
        let normalize = |locations: Vec<Location>| {
            let mut x: Vec<_> = locations.iter().map(|loc| (loc.batch, loc.target)).collect();
            x.sort_unstable();
            x
        };
        assert_eq!(
            normalize(losers(parts.clone(), &spills)),
            normalize(losers_bloom(parts, &spills))
        );
    }

    #[test]
    fn sparse_removal_preserves_nonlosers_across_streams() {
        let mut payload: Vec<Payload> = (0..NS).map(|_| Payload::default()).collect();
        payload[7].packed.times = (0..8).collect();
        payload[7].wide.times = (0..6)
            .map(|n| Wide {
                person: n,
                ts: n as i64,
            })
            .collect();
        payload[7].packed.purchases = (0..5)
            .map(|n| Purchase {
                key: n,
                price: n as u32 + 1,
                cat: 0,
            })
            .collect();
        payload[7].wide.purchases = (0..5)
            .map(|n| Purchase {
                key: Wide {
                    person: n,
                    ts: n as i64,
                },
                price: n as u32 + 1,
                cat: 0,
            })
            .collect();
        let target = |index: usize, kind: usize| ((index * 4 + kind) * NS + 7) as u32;
        let targets = vec![
            target(1, 0),
            target(6, 0),
            target(2, 2),
            target(4, 2),
            target(1, 1),
            target(3, 1),
            target(0, 3),
            target(4, 3),
        ];
        remove_payloads(&mut payload, targets);
        payload[7].packed.times.sort_unstable();
        payload[7].wide.times.sort_unstable();
        assert_eq!(payload[7].packed.times, vec![0, 2, 3, 4, 5, 7]);
        assert_eq!(
            payload[7].wide.times.iter().map(|v| v.ts).collect::<Vec<_>>(),
            vec![0, 1, 3, 5]
        );
        let mut packed_prices: Vec<_> = payload[7].packed.purchases.iter().map(|p| p.price).collect();
        packed_prices.sort_unstable();
        assert_eq!(packed_prices, vec![1, 3, 5]);
        let mut wide_prices: Vec<_> = payload[7].wide.purchases.iter().map(|p| p.price).collect();
        wide_prices.sort_unstable();
        assert_eq!(wide_prices, vec![2, 3, 4]);
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
