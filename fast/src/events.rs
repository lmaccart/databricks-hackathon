//! Route complete compact claims by event ID, then emit only exact winners.
use super::*;
use std::hash::{BuildHasherDefault, Hasher};

#[cfg(test)]
#[derive(Clone, Copy)]
struct Claim {
    key_and_target: u64,
    order_and_target: u64,
}

// The event-ID owner supplies exactly the high eight token bits.
const _: () = assert!(NS == 256);
#[cfg(test)]
const NONE: u32 = u32::MAX;
const KEY_BITS: u32 = u64::BITS - NS.trailing_zeros();
const KEY_MASK: u64 = (1 << KEY_BITS) - 1;
#[cfg(test)]
const TARGET_HI_BITS: u32 = 32 - NS.trailing_zeros();
#[cfg(test)]
const TARGET_HI_MASK: u64 = (1 << TARGET_HI_BITS) - 1;
// Signed 39-bit seconds cover every four-digit F1 year. In the 40-bit field,
// the two high bits are 00/11 for seconds and 01 for a side-table index.
const SPILL_TAG: u64 = 1 << 38;
const _: () = assert!(((CHUNK / 6 + 1) as u64) < SPILL_TAG);

#[inline]
fn ingest_order(ingest: i64, seconds: i64, spill: &mut Vec<i64>) -> u64 {
    if seconds != BAD {
        debug_assert!((-(SPILL_TAG as i64)..SPILL_TAG as i64).contains(&seconds));
        seconds as u64
    } else {
        let i = spill.len() as u64;
        spill.push(ingest);
        SPILL_TAG | i
    }
}

#[cfg(test)]
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

#[cfg(test)]
#[derive(Clone, Copy)]
struct Location {
    batch: usize,
    target: u32,
}

// The owner supplies the missing eight key bits. Forty bits store signed F1
// seconds or a per-batch exceptional-ingest index; the user token is exact.
#[derive(Clone, Copy, Default)]
struct Row {
    key_order: u64,
    uid: u64,
    time_order: u64,
}

const SIDE: u32 = 1 << 31;
const _: () = assert!(std::mem::size_of::<Row>() == 24 && CHUNK / 6 + 1 < SIDE as usize);

#[derive(Clone, Copy)]
struct Side {
    ts: i64,
    price: u32,
    cat: u32,
}

#[inline]
fn encode_time(ts: i64, ws: i64, price: u32, cat: u32, side: &mut Vec<Side>) -> u32 {
    let offset = (ts as i128) - (ws as i128);
    if price == 0 && (0..SIDE as i128).contains(&offset) {
        offset as u32
    } else {
        let index = u32::try_from(side.len()).expect("side index");
        assert!(index < SIDE);
        side.push(Side { ts, price, cat });
        SIDE | index
    }
}

impl Row {
    #[inline]
    fn new(key: u64, uid: u64, order: u64, time: u32) -> Self {
        Self {
            key_order: (key & KEY_MASK) | ((order & 255) << KEY_BITS),
            uid,
            time_order: ((order & ((1 << 40) - 1)) >> 8) << 32 | u64::from(time),
        }
    }

    #[inline]
    fn key(self) -> u64 {
        self.key_order & KEY_MASK
    }

    #[inline]
    fn ingest(self, spill: &[i64]) -> i64 {
        let order = ((self.time_order >> 32) << 8) | (self.key_order >> KEY_BITS);
        let signed = ((order << 24) as i64) >> 24;
        if signed >= SPILL_TAG as i64 {
            spill[(signed as u64 ^ SPILL_TAG) as usize]
        } else {
            signed * 1000
        }
    }

    #[inline]
    fn payload(self, ws: i64, side: &[Side]) -> (i64, u32, u32) {
        let time = self.time_order as u32;
        if time & SIDE == 0 {
            (ws + i64::from(time), 0, 0)
        } else {
            let s = side[(time ^ SIDE) as usize];
            (s.ts, s.price, s.cat)
        }
    }
}

#[derive(Default)]
struct BatchSide {
    spill: Vec<i64>,
    side: Vec<Side>,
}

fn parse<'a>(
    c: &Chunk<'a>,
    sm: &Small,
    people: &People<'a>,
    window: (i64, i64, &ManifestClock),
) -> (Vec<Vec<Row>>, BatchSide) {
    let (ws, we, clock) = window;
    let reserve = reserve_per_shard(c.data);
    let mut rows: Vec<Vec<Row>> = (0..NS).map(|_| Vec::with_capacity(reserve + reserve / 8)).collect();
    let mut spill = Vec::new();
    let mut side = Vec::new();
    for_rows(c, |r| {
        if r[0].is_empty() {
            return;
        }
        let claim_key = links::mix(people.codec.key(r[0]));
        let (ingest, seconds) = clock.ingest_for_claim(r[3]);
        let order = ingest_order(ingest, seconds, &mut spill);
        let mut uid = 0;
        let mut time = 0;
        if !r[1].is_empty() && !r[5].is_empty() {
            let purchase = match r[4] {
                b"purchase" => parse_price(r[6]),
                b"view" | b"cart" => Some(0),
                _ => None,
            };
            if let Some(price) = purchase {
                let ts = clock.parse_ts(r[2]);
                if (ws..we).contains(&ts) && ingest.saturating_sub(ts) <= DAY {
                    let cat = if price == 0 {
                        0
                    } else {
                        sm.product_cat.get(r[5]).unwrap_or(sm.unknown_cat)
                    };
                    uid = people.codec.key(r[1]);
                    time = encode_time(ts, ws, price, cat, &mut side);
                }
            }
        }
        rows[(claim_key >> KEY_BITS) as usize].push(Row::new(claim_key, uid, order, time));
    });
    (rows, BatchSide { spill, side })
}

#[inline]
fn probe_index(key: u64, mask: usize) -> usize {
    // The mask excludes the colocated ingest-order byte.
    key as usize & mask
}

#[inline]
fn probe(key: u64, mask: usize) -> (usize, u64) {
    let bits = 1u64.rotate_left((key >> 36) as u32) | 1u64.rotate_left((key >> 42) as u32);
    (probe_index(key, mask), bits)
}

#[inline]
fn prefetch(bloom: &[(u64, u64)], key: u64, mask: usize) {
    let ptr: *const (u64, u64) = &bloom[probe_index(key, mask)];
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

#[cfg(test)]
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
                prefetch(&bloom, next.key_and_target, mask);
            }
            let (i, b) = probe(c.key_and_target, mask);
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
                prefetch(&bloom, next.key_and_target, mask);
            }
            let (i, b) = probe(c.key_and_target, mask);
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

fn winners(
    parts: Vec<(usize, Vec<Row>)>,
    sides: &[BatchSide],
    people: &People,
    ws: i64,
    packing: Option<Packing>,
) -> Vec<Payload> {
    let n: usize = parts.iter().map(|(_, v)| v.len()).sum();
    // The second occurrence marks both bits as repeated, so the exact pass
    // includes the first occurrence as well. False positives never drop rows.
    let words = n.div_ceil(8).next_power_of_two();
    let mut bloom = vec![(0u64, 0u64); words];
    let mask = (words - 1) & KEY_MASK as usize;
    let mut suspects = 0;
    for (_, rows) in &parts {
        for (i, &row) in rows.iter().enumerate() {
            if let Some(&next) = rows.get(i + 6) {
                prefetch(&bloom, next.key_order, mask);
            }
            let (i, bits) = probe(row.key_order, mask);
            let (seen, repeated) = &mut bloom[i];
            if *seen & bits == bits {
                *repeated |= bits;
                suspects += 1;
            }
            *seen |= bits;
        }
    }
    let mut best: HashMap<u64, (i64, usize, Row), BuildHasherDefault<ClaimHasher>> =
        HashMap::with_capacity_and_hasher(suspects, BuildHasherDefault::default());
    let mut payloads: Vec<Payload> = (0..NS).map(|_| Payload::default()).collect();
    // Interleave lookup prefetch with independent winner materialization.
    let mut ring = [(Row::default(), 0usize, 0u64); 16];
    let mut count = 0usize;
    let mut emit = |row: Row, batch: usize| {
        if row.uid == 0 {
            return;
        }
        let hash = links::mix(row.uid);
        people.prefetch(hash);
        let i = count % ring.len();
        if count >= ring.len() {
            let (prior, batch, hash) = ring[i];
            let person = people.lookup(prior.uid, hash);
            let dest = shard(match person {
                Identity::Linked(p) | Identity::Standalone(p) => p,
            });
            let (ts, price, cat) = prior.payload(ws, &sides[batch].side);
            payloads[dest].push(person, ts, price, cat, packing, people);
        }
        ring[i] = (row, batch, hash);
        count += 1;
    };
    for (batch, rows) in parts {
        for (i, &row) in rows.iter().enumerate() {
            if let Some(&next) = rows.get(i + 6) {
                prefetch(&bloom, next.key_order, mask);
            }
            let (i, bits) = probe(row.key_order, mask);
            if bloom[i].1 & bits != bits {
                emit(row, batch);
                continue;
            }
            let ingest = row.ingest(&sides[batch].spill);
            match best.entry(row.key()) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert((ingest, batch, row));
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    if ingest < e.get().0 {
                        e.insert((ingest, batch, row));
                    }
                }
            }
        }
    }
    for (_, (_, batch, row)) in best {
        emit(row, batch);
    }
    drop(emit);
    for i in count.saturating_sub(ring.len())..count {
        let (row, batch, hash) = ring[i % ring.len()];
        let person = people.lookup(row.uid, hash);
        let dest = shard(match person {
            Identity::Linked(p) | Identity::Standalone(p) => p,
        });
        let (ts, price, cat) = row.payload(ws, &sides[batch].side);
        payloads[dest].push(person, ts, price, cat, packing, people);
    }
    payloads
}

pub(super) fn by_person<'a>(
    chunks: &[Chunk<'a>],
    sm: &Small,
    people: &People<'a>,
    window: (i64, i64, &ManifestClock),
    packing: Option<Packing>,
    log: &impl Fn(&str),
) -> Vec<Vec<Payload>> {
    let parsed: Vec<_> = chunks.par_iter().map(|c| parse(c, sm, people, window)).collect();
    log("events parsed");
    let mut sides = Vec::with_capacity(parsed.len());
    let mut claims: Vec<Vec<_>> = (0..NS).map(|_| Vec::new()).collect();
    for (batch, (cs, side)) in parsed.into_iter().enumerate() {
        sides.push(side);
        for (bucket, c) in claims.iter_mut().zip(cs) {
            if !c.is_empty() {
                bucket.push((batch, c));
            }
        }
    }
    claims
        .into_par_iter()
        .map(|parts| winners(parts, &sides, people, window.0, packing))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compound_rows_round_trip_compact_and_sparse_payloads() {
        assert_eq!(std::mem::size_of::<Row>(), 24);
        let mut side = Vec::new();
        let mut spill = Vec::new();
        for (ts, price, cat) in [(1000, 0, 0), (1001, 250, 17), (i64::MAX, 0, 0)] {
            let time = encode_time(ts, 1000, price, cat, &mut side);
            for ingest in [-62_167_219_200_000i64, 1_791_071_890_000, 253_402_300_799_000] {
                let order = ingest_order(ingest, ingest / 1000, &mut spill);
                let row = Row::new(0xfe12_3456_789a_bcde, 77, order, time);
                assert_eq!(row.key(), 0x12_3456_789a_bcde);
                assert_eq!(row.ingest(&spill), ingest);
                assert_eq!(row.payload(1000, &side), (ts, price, cat));
            }
        }
        let order = ingest_order(1234, BAD, &mut spill);
        let row = Row::new(3, 0, order, 0);
        assert_eq!(row.ingest(&spill), 1234);
    }

    #[test]
    fn compound_winners_keep_aliases_and_earliest_invalid_copy() {
        let people = person_names(&[]);
        let user = people.codec.key(b"u1");
        let mut side = Vec::new();
        let purchase = encode_time(123, 0, 250, 9, &mut side);
        let distant = encode_time(i64::from(SIDE), 0, 0, 0, &mut side);
        let a = 13;
        let alias = a ^ (1 << 24);
        let b = 25;
        assert_eq!(probe(a, 0), probe(alias, 0));
        let batches = vec![
            (0, vec![Row::new(a, user, 2, 123), Row::new(b, user, 3, 123)]),
            (
                1,
                vec![
                    Row::new(a, 0, 1, 0),
                    Row::new(alias, user, 4, distant),
                    Row::new(b, user, 2, purchase),
                ],
            ),
        ];
        let sides = vec![BatchSide::default(), BatchSide { side, spill: vec![] }];
        let result = winners(batches, &sides, &people, 0, None);
        let times: Vec<_> = result.iter().flat_map(|p| &p.wide.times).collect();
        let purchases: Vec<_> = result.iter().flat_map(|p| &p.wide.purchases).collect();
        assert_eq!(times.len(), 1);
        assert_eq!(times[0].ts, i64::from(SIDE));
        assert_eq!(purchases.len(), 1);
        assert_eq!(
            (purchases[0].key.ts, purchases[0].price, purchases[0].cat),
            (123, 250, 9)
        );
    }

    #[test]
    fn bloom_alias_keeps_distinct_id_and_invalid_winner_kills_valid_loser() {
        // Same shard, probe, and exact-table fingerprint, but distinct keys.
        let id = 13 | (217 << KEY_BITS);
        let alias = id ^ (1 << 24);
        let later = Claim::new(id, 0, 3);
        let distinct = Claim::new(alias, 1, 4);
        assert_eq!(probe(later.key_and_target, 1023), probe(distinct.key_and_target, 1023));
        let mut spill = Vec::new();
        let invalid = Claim::new(id, NONE, ingest_order(1234, BAD, &mut spill));
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
                    let claim = Claim::new(mixed, target, ingest_order(ms, secs, &mut spill));
                    assert_eq!(claim.ingest(&spill), ts::parse_ingest(s));
                    assert_eq!(claim.target(), target);
                    let reconstructed = claim.key() | ((mixed >> KEY_BITS) << KEY_BITS);
                    assert_eq!(id, codec.bytes(links::unmix(reconstructed), &mut [0; 9]));
                }
            }
        }
    }
}
