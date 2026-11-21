//! Send payloads directly to the session partition. A separate, smaller
//! stream of claims (ID, ingest time, payload location) decides which payloads
//! must be removed. In particular, an invalid copy has a claim but no payload.
use super::*;
use std::hash::{BuildHasherDefault, Hasher};

mod word;

#[derive(Clone, Copy)]
struct Claim {
    key: u64,
    target: u32,
    order: i64,
}

const NONE: u32 = u32::MAX;
const KEY_BITS: u32 = u64::BITS - NS.trailing_zeros();
const KEY_MASK: u64 = (1 << KEY_BITS) - 1;
// Four-digit F1 seconds fit below this reserved tag, which indexes the
// per-chunk millisecond side table for exceptional ingests.
const SPILL_TAG: u64 = 1 << 38;
const _: () =
    assert!(NS == 256 && (CHUNK / 6 + 1) * 4 * NS < u32::MAX as usize && ((CHUNK / 6 + 1) as u64) < SPILL_TAG);

impl Claim {
    #[inline]
    fn order(ingest: i64, seconds: i64, spill: &mut Vec<i64>) -> i64 {
        if seconds != BAD {
            debug_assert!((-(SPILL_TAG as i64)..SPILL_TAG as i64).contains(&seconds));
            seconds
        } else {
            let i = spill.len() as u64;
            spill.push(ingest);
            (SPILL_TAG | i) as i64
        }
    }

    #[inline]
    fn new(mixed: u64, target: u32, order: i64) -> Self {
        Self {
            // The event-ID bucket supplies the omitted high shard byte.
            key: mixed & KEY_MASK,
            target,
            order,
        }
    }

    #[inline]
    fn from_words(key: u64, metadata: ClaimMeta) -> Self {
        Self {
            key,
            target: metadata.target,
            order: metadata.order,
        }
    }

    #[inline]
    fn ingest(self, spill: &[i64]) -> i64 {
        if self.order >= SPILL_TAG as i64 {
            spill[(self.order as u64 ^ SPILL_TAG) as usize]
        } else {
            self.order * 1000
        }
    }
}

#[derive(Clone, Copy, Default)]
struct ClaimMeta {
    order: i64,
    target: u32,
}

// One append-only event-ID bucket is striped into a 56-bit reversible key
// column (the bucket supplies the high shard byte) and paired metadata lines.
// Unique IDs never touch ingest/locator data during either Bloom traversal.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct ClaimBlock {
    keys: [std::mem::MaybeUninit<u64>; 8],
    metadata: [std::mem::MaybeUninit<ClaimMeta>; 8],
}

impl ClaimBlock {
    fn new() -> Self {
        Self {
            keys: [std::mem::MaybeUninit::uninit(); 8],
            metadata: [std::mem::MaybeUninit::uninit(); 8],
        }
    }

    #[inline]
    fn key(&self, lane: usize) -> u64 {
        // Only lanes below ClaimBucket::len are visited.
        unsafe { self.keys[lane].assume_init() }
    }

    #[inline]
    fn meta(&self, lane: usize) -> ClaimMeta {
        // Only candidate lanes below ClaimBucket::len are visited.
        unsafe { self.metadata[lane].assume_init() }
    }
}

const _: () = assert!(
    std::mem::size_of::<ClaimBlock>() == 192
        && std::mem::align_of::<ClaimBlock>() == 64
        && std::mem::size_of::<ClaimMeta>() == 16
);

#[derive(Default)]
struct ClaimBucket {
    blocks: Vec<ClaimBlock>,
    len: usize,
}

impl ClaimBucket {
    #[inline]
    fn with_capacity(claims: usize) -> Self {
        Self {
            blocks: Vec::with_capacity(claims.div_ceil(8)),
            len: 0,
        }
    }

    #[inline]
    fn push(&mut self, claim: Claim) {
        let block = self.len / 8;
        let lane = self.len % 8;
        if lane == 0 {
            self.blocks.push(ClaimBlock::new());
        }
        self.blocks[block].keys[lane].write(claim.key);
        self.blocks[block].metadata[lane].write(ClaimMeta {
            order: claim.order,
            target: claim.target,
        });
        self.len += 1;
    }

    #[inline]
    fn key_at(&self, index: usize) -> u64 {
        self.blocks[index / 8].key(index % 8)
    }

    #[inline]
    fn len(&self) -> usize {
        self.len
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
    claim_order: i64,
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
) -> (Vec<Payload>, Vec<ClaimBucket>, Vec<i64>) {
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
    let mut claims: Vec<ClaimBucket> = (0..NS)
        .map(|_| ClaimBucket::with_capacity(reserve + reserve / 8))
        .collect();
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
    (payload, claims, spill)
}

#[inline]
#[cfg(test)]
fn probe_index(c: Claim, mask: usize) -> usize {
    c.key as usize & mask
}

#[inline]
#[cfg(test)]
fn probe(c: Claim, mask: usize) -> (usize, u64) {
    let k = c.key;
    let bits = 1u64.rotate_left((k >> 36) as u32) | 1u64.rotate_left((k >> 42) as u32);
    (probe_index(c, mask), bits)
}

#[inline]
fn probe_key(key: u64, mask: usize) -> (usize, u64) {
    let bits = 1u64.rotate_left((key >> 36) as u32) | 1u64.rotate_left((key >> 42) as u32);
    (key as usize & mask, bits)
}

#[inline]
fn prefetch(bloom: &[(u64, u64)], key: u64, mask: usize) {
    let ptr: *const (u64, u64) = &bloom[key as usize & mask];
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

struct LoserPass {
    locations: Vec<Location>,
    claims: usize,
    candidates: usize,
}

fn losers(parts: Vec<(usize, ClaimBucket)>, spills: &[Vec<i64>]) -> LoserPass {
    let n: usize = parts.iter().map(|(_, v)| v.len()).sum();
    // Local blocked Bloom: the second occurrence of any ID necessarily finds
    // both bits already set and marks it suspect. False positives go to an
    // EXACT key table. The second pass therefore includes the *first* claim of
    // every duplicate, too; no assumption about ingest order or validity.
    let words = n.div_ceil(8).next_power_of_two();
    let mut bloom = vec![(0u64, 0u64); words];
    let mask = words - 1;
    let mut suspects = 0usize;
    for (_, claims) in &parts {
        for j in 0..claims.len() {
            let key = claims.key_at(j);
            if j + 6 < claims.len() {
                prefetch(&bloom, claims.key_at(j + 6), mask);
            }
            let (i, b) = probe_key(key, mask);
            let (seen, repeated) = &mut bloom[i];
            if *seen & b == b {
                *repeated |= b;
                suspects += 1;
            }
            *seen |= b;
        }
    }
    let mut result = LoserPass {
        locations: Vec::new(),
        claims: n,
        candidates: 0,
    };
    if suspects == 0 {
        return result;
    }
    let mut best = HashMap::with_capacity_and_hasher(suspects, BuildHasherDefault::<ClaimHasher>::default());
    for (batch, claims) in parts {
        for (block_index, block) in claims.blocks.iter().enumerate() {
            let valid = (claims.len() - block_index * 8).min(8);
            let mut candidate_lanes = 0u8;
            for lane in 0..valid {
                let j = block_index * 8 + lane;
                if j + 6 < claims.len() {
                    prefetch(&bloom, claims.key_at(j + 6), mask);
                }
                let key = block.key(lane);
                let (i, b) = probe_key(key, mask);
                if bloom[i].1 & b == b {
                    candidate_lanes |= 1 << lane;
                }
            }
            // The key summary decides candidate lanes before any paired
            // ordering/locator bytes are loaded.
            while candidate_lanes != 0 {
                let lane = candidate_lanes.trailing_zeros() as usize;
                candidate_lanes &= candidate_lanes - 1;
                result.candidates += 1;
                let c = Claim::from_words(block.key(lane), block.meta(lane));
                let loc = Location {
                    batch,
                    target: c.target,
                };
                let ingest = c.ingest(&spills[batch]);
                let lost = match best.entry(c.key) {
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
                    result.locations.push(lost);
                }
            }
        }
    }
    result
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
    let mut claims: Vec<Vec<(usize, ClaimBucket)>> = (0..NS).map(|_| Vec::new()).collect();
    for (batch, (payload, cs, spill)) in parsed.into_iter().enumerate() {
        payloads.push(payload);
        spills.push(spill);
        for (bucket, c) in claims.iter_mut().zip(cs) {
            if c.len() != 0 {
                bucket.push((batch, c));
            }
        }
    }
    let removals: Vec<_> = claims.into_par_iter().map(|parts| losers(parts, &spills)).collect();
    let mut by_batch: Vec<Vec<u32>> = (0..payloads.len()).map(|_| Vec::new()).collect();
    let mut total_claims = 0usize;
    let mut total_candidates = 0usize;
    for pass in removals {
        total_claims += pass.claims;
        total_candidates += pass.candidates;
        for loc in pass.locations {
            by_batch[loc.batch].push(loc.target);
        }
    }
    let key_scan_bytes = total_claims.saturating_mul(16);
    let candidate_metadata_bytes = total_candidates.saturating_mul(std::mem::size_of::<ClaimMeta>());
    let previous_scan_bytes = total_claims.saturating_mul(32);
    let scan_savings = previous_scan_bytes.saturating_sub(key_scan_bytes + candidate_metadata_bytes);
    let scan_savings_pct = if previous_scan_bytes == 0 {
        0.0
    } else {
        100.0 * scan_savings as f64 / previous_scan_bytes as f64
    };
    log(&format!(
        "event dedup lanes: {total_candidates}/{total_claims} metadata; logical scan bytes {}/{previous_scan_bytes} ({scan_savings_pct:.1}% less)",
        key_scan_bytes + candidate_metadata_bytes,
    ));
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
        // Same shard, probe, and exact-table fingerprint, but distinct keys.
        let id = 13 | (217 << KEY_BITS);
        let alias = id ^ (1 << 24);
        let later = Claim::new(id, 0, 3);
        let distinct = Claim::new(alias, 1, 4);
        assert_eq!(probe(later, 1023), probe(distinct, 1023));
        let mut spill = Vec::new();
        let invalid = Claim::new(id, NONE, Claim::order(1234, BAD, &mut spill));
        let mut first = ClaimBucket::with_capacity(2);
        first.push(later);
        first.push(distinct);
        let mut second = ClaimBucket::with_capacity(1);
        second.push(invalid);
        let lost = losers(vec![(0, first), (1, second)], &[vec![], spill]);
        assert_eq!(lost.locations.len(), 1);
        assert_eq!((lost.locations[0].batch, lost.locations[0].target), (0, 0));
        assert_eq!(lost.candidates, 3);
    }

    #[test]
    fn striped_claim_blocks_keep_keys_separate_from_candidate_metadata() {
        assert_eq!(std::mem::align_of::<ClaimBlock>(), 64);
        assert_eq!(std::mem::size_of::<ClaimBlock>(), 192);
        assert_eq!(std::mem::offset_of!(ClaimBlock, metadata), 64);

        let mut claims = ClaimBucket::with_capacity(8);
        let mut spill = Vec::new();
        for (i, target) in [0, 1, 0x5555_aaaa, NONE, 256, 255, 7, 9].into_iter().enumerate() {
            let id = (i as u64 + 1) | (81 << KEY_BITS);
            claims.push(Claim::new(
                id,
                target,
                Claim::order(1_234 + i as i64 * 1000, BAD, &mut spill),
            ));
        }

        // A first-pass key traversal can stay within the first aligned line;
        // locators and ordering live only in the paired metadata lines.
        assert_eq!(claims.len(), 8);
        assert_eq!(claims.blocks[0].key(0), 1);
        assert_eq!(claims.blocks[0].key(7), 8);
        for (lane, expected) in [0, 1, 0x5555_aaaa, NONE, 256, 255, 7, 9].into_iter().enumerate() {
            let meta = claims.blocks[0].meta(lane);
            assert_eq!(Claim::from_words(claims.blocks[0].key(lane), meta).target, expected);
            assert_eq!(
                Claim::from_words(claims.blocks[0].key(lane), meta).ingest(&spill),
                1_234 + lane as i64 * 1000
            );
        }
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
                    assert_eq!(claim.target, target);
                    let reconstructed = claim.key | ((mixed >> KEY_BITS) << KEY_BITS);
                    assert_eq!(id, codec.bytes(links::unmix(reconstructed), &mut [0; 9]));
                }
            }
        }
    }
}
