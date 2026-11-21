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
// The 40-bit field holds the sortable day-rank/BCD word or a tagged
// index into this chunk's exact, uncached ingest times.
const SPILL_TAG: u64 = 1 << 39;
const _: () = assert!(std::mem::size_of::<Claim>() == 16 && ((CHUNK / 6 + 1) as u64) < SPILL_TAG);

impl Claim {
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
    fn order(self) -> u64 {
        self.order_and_target >> TARGET_HI_BITS
    }
}

#[cold]
#[inline(never)]
fn spill_ingest(s: &[u8], spill: &mut Vec<i128>, ts: i64, in_window: bool) -> (u64, bool) {
    let ingest = ts::parse_ingest(s);
    let index = spill.len();
    spill.push(ingest);
    (
        SPILL_TAG | index as u64,
        in_window && ingest <= i128::from(ts) + i128::from(DAY),
    )
}

#[cold]
#[inline(never)]
fn order_ms(order: u64, spill: &[i128], clock: &ManifestClock) -> i128 {
    if order >= SPILL_TAG {
        spill[(order ^ SPILL_TAG) as usize]
    } else {
        i128::from(clock.order_ms(order as u32))
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
) -> (Vec<Payload>, Vec<Vec<Claim>>, Vec<i128>) {
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
        let ts = clock.parse_ts(r[2]);
        let in_window = (ws..we).contains(&ts);
        let (claim_order, on_time) = if let Some(ingest) = clock.f1_order(r[3]) {
            (u64::from(ingest.word), in_window && ingest.on_time(ts))
        } else {
            spill_ingest(r[3], &mut spill, ts, in_window)
        };
        let mut next = Pending {
            claim_key,
            ts,
            claim_order,
            ..Default::default()
        };
        if on_time && !r[1].is_empty() && !r[5].is_empty() {
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

fn losers(parts: Vec<(usize, Vec<Claim>)>, spills: &[Vec<i128>], clock: &ManifestClock) -> Vec<Location> {
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
            let order = c.order();
            let lost = match best.entry(c.key()) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert((order, loc));
                    continue;
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    let &(prev_order, ref prev_loc): &(u64, Location) = e.get();
                    let earlier = if (order | prev_order) < SPILL_TAG {
                        order < prev_order
                    } else {
                        order_ms(order, &spills[batch], clock) < order_ms(prev_order, &spills[prev_loc.batch], clock)
                    };
                    if earlier { e.insert((order, loc)).1 } else { loc }
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
    let removals: Vec<_> = claims
        .into_par_iter()
        .map(|parts| losers(parts, &spills, window.2))
        .collect();
    let mut by_batch: Vec<Vec<u32>> = (0..payloads.len()).map(|_| Vec::new()).collect();
    for locations in removals {
        for loc in locations {
            by_batch[loc.batch].push(loc.target);
        }
    }
    payloads.par_iter_mut().zip(by_batch).for_each(|(payload, locations)| {
        if locations.is_empty() {
            return;
        }
        for target in locations {
            let x = target as usize;
            let bucket = &mut payload[x % NS];
            let slot = x / NS;
            let index = slot / 4;
            match slot % 4 {
                0 => bucket.packed.times[index] = u64::MAX,
                1 => bucket.packed.purchases[index].price = 0,
                2 => bucket.wide.times[index].ts = BAD,
                _ => bucket.wide.purchases[index].price = 0,
            }
        }
        for dest in payload {
            dest.packed.times.retain(|&key| key != u64::MAX);
            dest.packed.purchases.retain(|p| p.price != 0);
            dest.wide.times.retain(|key| key.ts != BAD);
            dest.wide.purchases.retain(|p| p.price != 0);
        }
    });
    payloads
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bloom_alias_keeps_distinct_id_and_invalid_winner_kills_valid_loser() {
        let clock = ManifestClock::new(0, DAY);
        // Same shard, probe, and exact-table fingerprint, but distinct keys.
        let id = 13 | (217 << KEY_BITS);
        let alias = id ^ (1 << 24);
        let late = clock.f1_order(b"1970-01-01T00:00:03Z").unwrap();
        let later = Claim::new(id, 0, u64::from(late.word));
        let distinct = Claim::new(alias, 1, u64::from(late.word));
        assert_eq!(probe(later, 1023), probe(distinct, 1023));
        let mut spill = Vec::new();
        let invalid = Claim::new(id, NONE, spill_ingest(b"1234", &mut spill, 0, false).0);
        let lost = losers(
            vec![(0, vec![later, distinct]), (1, vec![invalid])],
            &[vec![], spill],
            &clock,
        );
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
                    b"9223372036854775807",
                    b"9223372036854775808",
                    b"000100000000000000000000",
                    b"not a timestamp",
                ] {
                    let order = clock
                        .f1_order(s)
                        .map_or_else(|| spill_ingest(s, &mut spill, 0, false).0, |hit| u64::from(hit.word));
                    let claim = Claim::new(mixed, target, order);
                    assert_eq!(order_ms(claim.order(), &spill, &clock), ts::parse_ingest(s));
                    assert_eq!(claim.target(), target);
                    let reconstructed = claim.key() | ((mixed >> KEY_BITS) << KEY_BITS);
                    assert_eq!(id, codec.bytes(links::unmix(reconstructed), &mut [0; 9]));
                }
            }
        }
    }
}
