//! Send payloads directly to the session partition. A separate, smaller
//! stream of claims (ID, ingest time, payload location) decides which payloads
//! must be removed. In particular, an invalid copy has a claim but no payload.
use super::*;
use std::hash::{BuildHasherDefault, Hasher};

mod word;

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

const CLAIMS_PER_CACHE_LINE: usize = 64 / std::mem::size_of::<Claim>();
const CLAIM_NT_MIN_BYTES: usize = 1 << 20;
const _: () = assert!(std::mem::size_of::<Claim>() == 16 && CLAIMS_PER_CACHE_LINE == 4);

/// Per-shard software combining for the fixed-size event-ID stream. Full
/// cache lines bypass write allocation only for sufficiently large chunks;
/// short chunks and alignment heads/tails stay on Vec's initialized path.
struct ClaimBuckets {
    buckets: Vec<Vec<Claim>>,
    staged: Vec<Claim>,
    pending: [u8; NS],
    use_nt: bool,
    has_streamed: bool,
    #[cfg(all(test, target_arch = "x86_64"))]
    streamed_lines: usize,
}

impl ClaimBuckets {
    fn with_capacity(capacity: usize, use_nt: bool) -> Self {
        let buckets = (0..NS).map(|_| Vec::with_capacity(capacity)).collect();
        let zero = Claim {
            key_and_target: 0,
            order_and_target: 0,
        };
        ClaimBuckets {
            buckets,
            staged: vec![zero; NS * CLAIMS_PER_CACHE_LINE],
            pending: [0; NS],
            use_nt,
            has_streamed: false,
            #[cfg(all(test, target_arch = "x86_64"))]
            streamed_lines: 0,
        }
    }

    #[inline]
    fn push(&mut self, bucket: usize, claim: Claim) {
        if !self.use_nt {
            self.buckets[bucket].push(claim);
            return;
        }
        let count = self.pending[bucket] as usize;
        self.staged[bucket * CLAIMS_PER_CACHE_LINE + count] = claim;
        self.pending[bucket] = (count + 1) as u8;
        if count + 1 == CLAIMS_PER_CACHE_LINE {
            self.flush(bucket, false);
        }
    }

    fn ensure_room(&mut self, bucket: usize, additional: usize) {
        let rows = &self.buckets[bucket];
        if rows.capacity() - rows.len() < additional {
            // Vec growth copies initialized rows and may read lines that were
            // written with streaming stores; make those stores visible first.
            if self.has_streamed {
                sfence();
                self.has_streamed = false;
            }
            self.buckets[bucket].reserve(additional);
        }
    }

    fn drain_temporal(&mut self, bucket: usize, count: usize) {
        let n = self.pending[bucket] as usize;
        debug_assert!(count > 0 && count <= n);
        self.ensure_room(bucket, count);
        let start = bucket * CLAIMS_PER_CACHE_LINE;
        self.buckets[bucket].extend_from_slice(&self.staged[start..start + count]);
        self.staged.copy_within(start + count..start + n, start);
        self.pending[bucket] = (n - count) as u8;
    }

    fn flush(&mut self, bucket: usize, finalizing: bool) {
        while self.pending[bucket] != 0 {
            let count = self.pending[bucket] as usize;
            if count < CLAIMS_PER_CACHE_LINE && !finalizing {
                return;
            }

            #[cfg(target_arch = "x86_64")]
            if self.use_nt && count == CLAIMS_PER_CACHE_LINE {
                self.ensure_room(bucket, CLAIMS_PER_CACHE_LINE);
                let len = self.buckets[bucket].len();
                let dst = unsafe { self.buckets[bucket].as_mut_ptr().add(len) };
                let address = dst as usize;

                // Claim's alignment is 8 bytes, so do not assume a Vec's
                // allocation is 16/64-byte aligned. A non-16-aligned base
                // cannot reach a cache-line boundary by appending claims.
                if address % 16 == 0 {
                    let head = ((64 - address % 64) % 64) / std::mem::size_of::<Claim>();
                    if head != 0 {
                        self.drain_temporal(bucket, head);
                        continue;
                    }

                    let start = bucket * CLAIMS_PER_CACHE_LINE;
                    // SAFETY: dst is 64-byte aligned, has four Claims of
                    // spare capacity, and staged holds four initialized
                    // Claims. Each SSE2 store covers one complete 16-byte
                    // record; Claim contains only valid u64 bit patterns.
                    unsafe {
                        stream_claim_line(dst, self.staged.as_ptr().add(start));
                        self.buckets[bucket].set_len(len + CLAIMS_PER_CACHE_LINE);
                    }
                    self.pending[bucket] = 0;
                    self.has_streamed = true;
                    #[cfg(all(test, target_arch = "x86_64"))]
                    {
                        self.streamed_lines += 1;
                    }
                    continue;
                }
            }

            self.drain_temporal(bucket, count);
        }
    }

    fn finish(mut self) -> Vec<Vec<Claim>> {
        for bucket in 0..NS {
            self.flush(bucket, true);
        }
        if self.has_streamed {
            // Publish every streamed cache line before claims can move to a
            // different Rayon worker for duplicate resolution.
            sfence();
        }
        self.buckets
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn stream_claim_line(dst: *mut Claim, src: *const Claim) {
    use std::arch::x86_64::{__m128i, _mm_loadu_si128, _mm_stream_si128};
    for i in 0..CLAIMS_PER_CACHE_LINE {
        // SAFETY: caller provides a full aligned destination line and four
        // readable staged claims; unaligned loads are allowed for the source.
        unsafe {
            let value = _mm_loadu_si128(src.add(i).cast::<__m128i>());
            _mm_stream_si128(dst.add(i).cast::<__m128i>(), value);
        }
    }
}

#[inline]
fn sfence() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::x86_64::_mm_sfence();
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
) -> (Vec<Payload>, Vec<Vec<Claim>>, Vec<i64>) {
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
    let use_nt = cfg!(target_arch = "x86_64") && c.data.len() >= CLAIM_NT_MIN_BYTES;
    let mut claims = ClaimBuckets::with_capacity(reserve + reserve / 8, use_nt);
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
    (payload, claims.finish(), spill)
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

fn losers(parts: Vec<(usize, Vec<Claim>)>, spills: &[Vec<i64>]) -> Vec<Location> {
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
        let lost = losers(vec![(0, vec![later, distinct]), (1, vec![invalid])], &[vec![], spill]);
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

    #[test]
    fn claim_write_combining_handles_empty_short_odd_aligned_and_growing_buckets() {
        // Prefix lengths vary the cache-line offset. A tiny initial capacity
        // forces growth after at least one complete staged line; the odd tail
        // must still be published exactly once.
        for prefix in 0..4u64 {
            let mut buckets = ClaimBuckets::with_capacity(4, true);
            #[cfg(target_arch = "x86_64")]
            let initially_streamable = buckets.buckets[0].as_ptr() as usize % 16 == 0;
            for i in 0..prefix {
                buckets.push(0, Claim::new(100 + i, i as u32, i + 1));
            }
            for i in 0..19u64 {
                buckets.push(0, Claim::new(1_000 + i, (i * 7) as u32, i + 50));
            }
            #[cfg(target_arch = "x86_64")]
            if initially_streamable {
                assert!(buckets.streamed_lines > 0);
            }
            let mut result = buckets.finish();
            let claims = std::mem::take(&mut result[0]);
            let mut expected = (0..prefix)
                .map(|i| Claim::new(100 + i, i as u32, i + 1))
                .collect::<Vec<_>>();
            expected.extend((0..19u64).map(|i| Claim::new(1_000 + i, (i * 7) as u32, i + 50)));
            assert_eq!(claims.len(), expected.len());
            for (actual, expected) in claims.iter().zip(expected) {
                assert_eq!(actual.key_and_target, expected.key_and_target);
                assert_eq!(actual.order_and_target, expected.order_and_target);
            }
        }

        let empty = ClaimBuckets::with_capacity(0, true).finish();
        assert!(empty.iter().all(Vec::is_empty));

        let mut one = ClaimBuckets::with_capacity(0, true);
        one.push(3, Claim::new(17, 29, 41));
        let one = one.finish();
        assert_eq!(one[3].len(), 1);
        assert_eq!(one[3][0].key_and_target, Claim::new(17, 29, 41).key_and_target);
        assert_eq!(one[3][0].order_and_target, Claim::new(17, 29, 41).order_and_target);
    }
}
