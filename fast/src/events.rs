//! Send payloads directly to the session partition. A separate, smaller
//! stream of claims (ID, ingest time, payload location) decides which payloads
//! must be removed. In particular, an invalid copy has a claim but no payload.
use super::*;
use std::hash::{BuildHasherDefault, Hasher};

mod word;

#[derive(Clone, Copy)]
#[repr(C)]
struct Claim {
    lo: u32,
    mid: u32,
    hi: u32,
}
#[derive(Clone, Copy)]
struct Exact {
    target: u32,
    ingest: i64,
}

// An emitted event has at least six commas. At most one row extends past
// CHUNK, so the stream kind, per-stream ordinal and shard fit in the claim.
const _: () = assert!(NS == 256 && (CHUNK / 6 + 1) * 4 * NS < u32::MAX as usize);
const NONE: u32 = u32::MAX;
const KEY_BITS: u32 = u64::BITS - NS.trailing_zeros();
const KEY_MASK: u64 = (1 << KEY_BITS) - 1;
const TARGET_BITS: u32 = 19;
const SMALL_NONE: u32 = (1 << TARGET_BITS) - 1;
// Leave enough escape indices for every possible well-formed row in a chunk;
// even a completely unrepresentative window or a single dominant person works.
const ESCAPE: u64 = (1 << 40) - (1 << 21);
// Signed 39-bit seconds cover every four-digit F1 year. In the 40-bit field,
// the two high bits are 00/11 for seconds and 01 for a side-table index.
const SPILL_TAG: u64 = 1 << 38;
const _: () = assert!(std::mem::size_of::<Claim>() == 12 && CHUNK / 6 + 1 < 1 << 21);

fn time_origin(ws: i64) -> i64 {
    ws.div_euclid(1000) - 2 * 86_400
}

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
    fn new(mixed: u64, target: u32, order: u64, origin: i64, exact: &mut Vec<Exact>) -> Self {
        let raw = order as i64;
        let delta = raw.wrapping_sub(origin) as u64;
        let state =
            if raw < SPILL_TAG as i64 && delta < (ESCAPE >> TARGET_BITS) && (target < SMALL_NONE || target == NONE) {
                (delta << TARGET_BITS) | u64::from(target & SMALL_NONE)
            } else {
                let index = exact.len();
                exact.push(Exact { target, ingest: raw });
                ESCAPE + index as u64
            };
        Self {
            lo: mixed as u32,
            mid: ((mixed >> 32) as u32 & 0xff_ffff) | ((state as u32) << 24),
            hi: (state >> 8) as u32,
        }
    }

    #[inline]
    fn key(self) -> u64 {
        u64::from(self.lo) | (u64::from(self.mid & 0xff_ffff) << 32)
    }

    #[inline]
    fn unpack(self, origin: i64, exact: &[Exact]) -> Exact {
        let state = (u64::from(self.hi) << 8) | u64::from(self.mid >> 24);
        if state >= ESCAPE {
            return exact[(state - ESCAPE) as usize];
        }
        let t = state as u32 & SMALL_NONE;
        Exact {
            target: if t == SMALL_NONE { NONE } else { t },
            ingest: (origin + (state >> TARGET_BITS) as i64) * 1000,
        }
    }
}

fn finish_exact(exact: &mut [Exact], spill: &[i64]) {
    for e in exact {
        let raw = e.ingest;
        e.ingest = if raw >= SPILL_TAG as i64 {
            spill[(raw as u64 ^ SPILL_TAG) as usize]
        } else {
            raw * 1000
        };
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

fn parse<'a>(
    c: &Chunk<'a>,
    sm: &Small,
    people: &People<'a>,
    window: (i64, i64, &ManifestClock),
    packing: Option<Packing>,
) -> (Vec<Payload>, Vec<Vec<Claim>>, Vec<Exact>) {
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
    let mut exact = Vec::new();
    let origin = time_origin(window.0);
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
        // Exceptional input timestamps already carry their input side-table
        // index; finalize those after parsing ends, without rereading CSV.
        claims[(e.claim_key >> KEY_BITS) as usize].push(Claim::new(
            e.claim_key,
            target,
            e.claim_order,
            origin,
            &mut exact,
        ));
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
    finish_exact(&mut exact, &spill);
    (payload, claims, exact)
}

#[inline]
fn probe_index(c: Claim, mask: usize) -> usize {
    // The mask excludes the colocated payload-shard byte.
    c.lo as usize & mask
}

#[inline]
fn probe(c: Claim, mask: usize) -> (usize, u64) {
    let k = c.key();
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

fn losers(parts: Vec<(usize, Vec<Claim>)>, spills: &[Vec<Exact>], origin: i64) -> Vec<Location> {
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
            let Exact { target, ingest } = c.unpack(origin, &spills[batch]);
            let loc = Location { batch, target };
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
    let origin = time_origin(window.0);
    let removals: Vec<_> = claims
        .into_par_iter()
        .map(|parts| losers(parts, &spills, origin))
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
    fn coarse_claims_compare_far_future_with_modern_copy() {
        let clock = ManifestClock::new(parse_ts(b"2028-02-01T00:00:00Z"), parse_ts(b"2028-03-01T00:00:00Z"));
        let mut spill = vec![];
        let order = |s: &[u8], spill: &mut Vec<i64>| {
            let (ms, sec) = clock.ingest_for_claim(s);
            Claim::order(ms, sec, spill)
        };
        let mut exact = vec![];
        let origin = time_origin(parse_ts(b"2028-02-27T00:00:00Z"));
        let future = Claim::new(3, 23, order(b"9999-12-31T23:59:59Z", &mut spill), origin, &mut exact);
        let now = Claim::new(3, NONE, order(b"2028-02-29T00:00:00Z", &mut spill), origin, &mut exact);
        assert_eq!(exact.len(), 1);
        finish_exact(&mut exact, &spill);
        let losers = losers(vec![(0, vec![future, now])], &[exact], origin);
        assert_eq!(losers.len(), 1);
        assert_eq!(losers[0].target, 23);
    }

    #[test]
    fn bloom_alias_keeps_distinct_id_and_invalid_winner_kills_valid_loser() {
        // Same shard, probe, and exact-table fingerprint, but distinct keys.
        let id = 13 | (217 << KEY_BITS);
        let alias = id ^ (1 << 24);
        let mut exact = Vec::new();
        let later = Claim::new(id, 0, 3, 0, &mut exact);
        let distinct = Claim::new(alias, 1, 4, 0, &mut exact);
        assert_eq!(probe(later, 1023), probe(distinct, 1023));
        let mut spill = Vec::new();
        let invalid = Claim::new(id, NONE, Claim::order(1234, BAD, &mut spill), 0, &mut exact);
        finish_exact(&mut exact, &spill);
        let lost = losers(
            vec![(0, vec![later, distinct]), (1, vec![invalid])],
            &[vec![], exact],
            0,
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
                    b"not a timestamp",
                ] {
                    let (ms, secs) = clock.ingest_for_claim(s);
                    let origin = time_origin(parse_ts(b"2000-02-28T00:00:00Z"));
                    let mut exact = vec![];
                    let claim = Claim::new(mixed, target, Claim::order(ms, secs, &mut spill), origin, &mut exact);
                    finish_exact(&mut exact, &spill);
                    let unpacked = claim.unpack(origin, &exact);
                    assert_eq!(unpacked.ingest, ts::parse_ingest(s));
                    assert_eq!(unpacked.target, target);
                    let reconstructed = claim.key() | ((mixed >> KEY_BITS) << KEY_BITS);
                    assert_eq!(id, codec.bytes(links::unmix(reconstructed), &mut [0; 9]));
                }
            }
        }
    }
}
