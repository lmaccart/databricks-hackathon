//! One parse-order event store per CSV chunk. Dedup's radix pages contain
//! references, not copies of the claims or payloads. Only exact-check suspects
//! read the backing claims; only surviving rows leave the temporal columns.
use super::*;
use std::hash::{BuildHasherDefault, Hasher};
use stream::{Purchase, Wide};

// A well-formed row with an ID uses at least six commas, and at most one
// row can extend past CHUNK. A pathological line may be huge, not numerous.
const ROW_BITS: u32 = 21;
const ROW_MASK: u32 = (1 << ROW_BITS) - 1;
const _: () = assert!(CHUNK / 6 < ROW_MASK as usize && NS == 256);
const TILE: usize = 2048;
const PAGE: usize = 16;
const INVALID: u32 = 1 << 31;
const WIDE: u32 = 1 << 30;
const PURCHASE: u32 = 1 << 29;
const SPILL: u32 = 1 << 31;

// A worker writes only to cache-resident buffers until it has a contiguous
// run ready for a destination stream. Vec growth cannot invalidate row IDs.
struct Pages<T> {
    buffers: Box<[[T; PAGE]]>,
    used: [usize; NS],
    destinations: Vec<Vec<T>>,
}

impl<T: Copy> Pages<T> {
    fn new(seed: T, reserve: usize) -> Self {
        Self::with_capacities(seed, std::iter::repeat_n(reserve, NS))
    }

    fn with_capacities(seed: T, capacities: impl IntoIterator<Item = usize>) -> Self {
        Self {
            buffers: vec![[seed; PAGE]; NS].into_boxed_slice(),
            used: [0; NS],
            destinations: capacities.into_iter().map(Vec::with_capacity).collect(),
        }
    }

    #[inline]
    fn push(&mut self, dest: usize, value: T) {
        let n = self.used[dest];
        self.buffers[dest][n] = value;
        self.used[dest] = n + 1;
        if n == PAGE - 1 {
            self.flush(dest, PAGE);
        }
    }

    #[cold]
    fn flush(&mut self, dest: usize, n: usize) {
        let out = &mut self.destinations[dest];
        out.extend_from_slice(&self.buffers[dest][..n]);
        self.used[dest] = 0;
    }

    fn finish(mut self) -> Vec<Vec<T>> {
        for dest in 0..NS {
            let n = self.used[dest];
            if n != 0 {
                self.flush(dest, n);
            }
        }
        self.destinations
    }
}

#[derive(Clone, Copy)]
struct ClaimRef(u64);

impl ClaimRef {
    #[inline]
    fn new(token: u64, row: usize) -> Self {
        debug_assert!(row <= ROW_MASK as usize);
        Self((token << ROW_BITS) | row as u64)
    }

    #[inline]
    fn row(self) -> usize {
        (self.0 as u32 & ROW_MASK) as usize
    }

    #[inline]
    fn probe(self, mask: usize) -> (usize, u64) {
        let k = self.0;
        let bits = 1u64.rotate_left((k >> 51) as u32) | 1u64.rotate_left((k >> 57) as u32);
        (((k >> ROW_BITS) as usize) & mask, bits)
    }
}

#[derive(Clone, Copy)]
struct Ingest(i64); // origin in seconds

impl Ingest {
    fn new(ws: i64) -> Self {
        Self(ws.div_euclid(1000) - (1 << 30))
    }

    #[inline]
    fn encode(self, ms: i64, sec: i64, spills: &mut Vec<i64>) -> u32 {
        if sec != BAD {
            let rel = sec.wrapping_sub(self.0) as u64;
            if rel < u64::from(SPILL) {
                return rel as u32;
            }
        }
        let i = spills.len() as u32;
        spills.push(ms);
        SPILL | i
    }

    #[inline]
    fn decode(self, order: u32, spills: &[i64]) -> i64 {
        if order & SPILL == 0 {
            (self.0 + i64::from(order)) * 1000
        } else {
            spills[(order ^ SPILL) as usize]
        }
    }
}

// The dense columns share the same row ordinal. Wide time and price/category
// are side columns; ordinary packed views/carts need neither.
struct Batch {
    tokens: Vec<u64>,
    rows: Vec<Row>,
    ingests: Vec<i64>,
    wide: Vec<Wide>,
    prices: Vec<(u32, u32)>,
    counts: Box<[[usize; 4]; NS]>,
}

#[derive(Clone, Copy)]
struct Row {
    key: u64,   // exact packed person/time, or an ordinal in `wide`
    route: u32, // invalid/wide/purchase flags, shard, sparse price ordinal
    order: u32,
}

#[derive(Clone, Copy, Default)]
struct Pending {
    token: u64,
    uid: u64, // zero is never the key of a nonempty ID
    hash: u64,
    ts: i64,
    order: u32,
    price: u32,
    cat: u32,
}

fn parse<'a>(
    c: &Chunk<'a>,
    sm: &Small,
    people: &People<'a>,
    window: (i64, i64, &ManifestClock),
    packing: Option<Packing>,
    ingest: Ingest,
) -> (Batch, Vec<Vec<ClaimRef>>) {
    let (ws, we, clock) = window;
    let reserve = reserve_per_shard(c.data) + 1;
    let cap = (reserve * NS).min(c.data.len().div_ceil(7));
    let mut tokens = Vec::<u64>::with_capacity(cap);
    let mut rows = Vec::with_capacity(cap);
    let mut ingests = Vec::new();
    let mut wide = Vec::new();
    let mut prices = Vec::new();
    let mut counts = Box::new([[0usize; 4]; NS]);
    let mut pages = Pages::new(ClaimRef(0), reserve);
    let mut next_tile = 0;
    let mut partition = |tokens: &[u64], start: usize| {
        for (i, &token) in tokens[start..].iter().enumerate() {
            pages.push((token >> 56) as usize, ClaimRef::new(token, start + i));
        }
    };
    let mut emit = |e: Pending| {
        let mut route = INVALID;
        let mut key = 0;
        if e.uid != 0 {
            let person = people.lookup(e.uid, e.hash);
            let (h, temporal) = match (person, packing) {
                (Identity::Linked(h), Some(p)) => (h, Some(p.encode(h as u32, e.ts))),
                (Identity::Linked(h), None) => (people.original(h as u32), None),
                (Identity::Standalone(h), _) => (h, None),
            };
            let dest = shard(h);
            route = (dest as u32) << ROW_BITS;
            let lane;
            if let Some(k) = temporal {
                key = k;
                lane = 0;
            } else {
                route |= WIDE;
                key = wide.len() as u64;
                wide.push(Wide { person: h, ts: e.ts });
                lane = 2;
            }
            counts[dest][lane] += 1;
            if e.price != 0 {
                route |= PURCHASE | prices.len() as u32;
                prices.push((e.price, e.cat));
                counts[dest][lane + 1] += 1;
            }
        }
        rows.push(Row {
            key,
            route,
            order: e.order,
        });
        tokens.push(e.token);
        if tokens.len() - next_tile == TILE {
            partition(&tokens, next_tile);
            next_tile += TILE;
        }
    };
    let mut ring = [Pending::default(); 16];
    let mut n = 0usize;
    for_rows(c, |r| {
        if r[0].is_empty() {
            return;
        }
        let token = links::mix(people.codec.key(r[0]));
        let (ms, seconds) = clock.ingest_for_claim(r[3]);
        let ts = clock.parse_ts(r[2]);
        let mut next = Pending {
            token,
            ts,
            order: ingest.encode(ms, seconds, &mut ingests),
            ..Default::default()
        };
        if !r[1].is_empty() && !r[5].is_empty() && (ws..we).contains(&ts) && ms.saturating_sub(ts) <= DAY {
            let purchase = match r[4] {
                b"purchase" => parse_price(r[6]),
                b"view" | b"cart" => Some(0),
                _ => None,
            };
            if let Some(price) = purchase {
                next.uid = people.codec.key(r[1]);
                next.hash = links::mix(next.uid);
                people.prefetch(next.hash);
                next.price = price;
                if price != 0 {
                    next.cat = sm.product_cat.get(r[5]).unwrap_or(sm.unknown_cat);
                }
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
    partition(&tokens, next_tile);
    (
        Batch {
            tokens,
            rows,
            ingests,
            wide,
            prices,
            counts,
        },
        pages.finish(),
    )
}

#[derive(Default)]
struct ClaimHasher(u64);

impl Hasher for ClaimHasher {
    fn write(&mut self, _: &[u8]) {
        unreachable!("the claim table has only u64 keys");
    }

    #[inline]
    fn write_u64(&mut self, key: u64) {
        // The high byte of the exact token is constant within this shard.
        let tag_shift = usize::BITS - 8;
        self.0 = (key & ((1 << tag_shift) - 1)) | (((key >> 48) & 255) << tag_shift);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

#[inline]
fn prefetch(bloom: &[(u64, u64)], r: ClaimRef, mask: usize) {
    let ptr: *const (u64, u64) = &bloom[((r.0 >> ROW_BITS) as usize) & mask];
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

fn losers(parts: Vec<(usize, Vec<ClaimRef>)>, batches: &[Batch], ingest: Ingest) -> Vec<(usize, u32)> {
    let n: usize = parts.iter().map(|(_, v)| v.len()).sum();
    let words = n.div_ceil(8).next_power_of_two();
    let mut bloom = vec![(0u64, 0u64); words];
    let mask = words - 1;
    let mut suspects = 0usize;
    // The second copy necessarily marks both bits repeated; the second pass
    // includes the first copy too. Fingerprint collisions can only add work.
    for (_, claims) in &parts {
        for (j, &r) in claims.iter().enumerate() {
            if let Some(&next) = claims.get(j + 6) {
                prefetch(&bloom, next, mask);
            }
            let (i, b) = r.probe(mask);
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
        let store = &batches[batch];
        for (j, &r) in claims.iter().enumerate() {
            if let Some(&next) = claims.get(j + 6) {
                prefetch(&bloom, next, mask);
            }
            let (i, b) = r.probe(mask);
            if bloom[i].1 & b != b {
                continue;
            }
            let row = r.row();
            let token = store.tokens[row];
            let order = ingest.decode(store.rows[row].order, &store.ingests);
            let loc = (batch, row as u32);
            let lost = match best.entry(token) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert((order, loc));
                    continue;
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    if order < e.get().0 {
                        e.insert((order, loc)).1
                    } else {
                        loc
                    }
                }
            };
            remove.push(lost);
        }
    }
    remove
}

fn route(batch: Batch, mut lost: Vec<u32>) -> Vec<Payload> {
    let Batch {
        rows,
        wide,
        prices,
        counts,
        ..
    } = batch;
    let mut packed_keys = Pages::with_capacities(0, counts.iter().map(|c| c[0]));
    let mut wide_keys = Pages::with_capacities(Wide { person: 0, ts: 0 }, counts.iter().map(|c| c[2]));
    let mut out: Vec<Payload> = counts
        .iter()
        .map(|c| {
            let mut p = Payload::default();
            p.packed.purchases.reserve(c[1]);
            p.wide.purchases.reserve(c[3]);
            p
        })
        .collect();
    // A private validity bitmap never needs atomic RMWs. An empty loser list
    // has no allocation; still skip parse-time invalid winners below.
    let mut killed = Vec::new();
    if !lost.is_empty() {
        killed.resize(rows.len().div_ceil(64), 0u64);
        for r in lost.drain(..).map(|r| r as usize) {
            killed[r >> 6] |= 1 << (r & 63);
        }
    }
    for (page, chunk) in rows.chunks(64).enumerate() {
        let dead = killed.get(page).copied().unwrap_or(0);
        for (i, &Row { key, route, .. }) in chunk.iter().enumerate() {
            if (dead >> i) & 1 != 0 || route & INVALID != 0 {
                continue;
            }
            let dest = ((route >> ROW_BITS) & 255) as usize;
            if route & (WIDE | PURCHASE) == 0 {
                packed_keys.push(dest, key);
            } else if route & PURCHASE == 0 {
                wide_keys.push(dest, wide[key as usize]);
            } else {
                let (price, cat) = prices[(route & ROW_MASK) as usize];
                let p = &mut out[dest];
                if route & WIDE == 0 {
                    p.packed.purchases.push(Purchase { key, price, cat });
                } else {
                    p.wide.purchases.push(Purchase {
                        key: wide[key as usize],
                        price,
                        cat,
                    });
                }
            }
        }
    }
    for ((p, packed), wide) in out.iter_mut().zip(packed_keys.finish()).zip(wide_keys.finish()) {
        p.packed.times = packed;
        p.wide.times = wide;
    }
    out
}

pub(super) fn by_person<'a>(
    chunks: &[Chunk<'a>],
    sm: &Small,
    people: &People<'a>,
    window: (i64, i64, &ManifestClock),
    packing: Option<Packing>,
    log: &impl Fn(&str),
) -> Vec<Vec<Payload>> {
    let ingest = Ingest::new(window.0);
    let parsed: Vec<_> = chunks
        .par_iter()
        .map(|c| parse(c, sm, people, window, packing, ingest))
        .collect();
    log("events parsed");
    let mut batches = Vec::with_capacity(parsed.len());
    let mut claims: Vec<Vec<_>> = (0..NS).map(|_| Vec::new()).collect();
    for (batch, (store, cs)) in parsed.into_iter().enumerate() {
        batches.push(store);
        for (bucket, c) in claims.iter_mut().zip(cs) {
            if !c.is_empty() {
                bucket.push((batch, c));
            }
        }
    }
    let removals: Vec<_> = claims
        .into_par_iter()
        .map(|parts| losers(parts, &batches, ingest))
        .collect();
    let mut by_batch: Vec<Vec<u32>> = (0..batches.len()).map(|_| Vec::new()).collect();
    for locations in removals {
        for (batch, row) in locations {
            by_batch[batch].push(row);
        }
    }
    log("event winners resolved");
    batches
        .into_par_iter()
        .zip(by_batch)
        .map(|(b, lost)| route(b, lost))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radix_pages_preserve_order_and_grow_on_skew() {
        let mut pages = Pages::new(ClaimRef(0), 1);
        let n = TILE * 3 + 13;
        for row in 0..n {
            pages.push(if row % 113 == 0 { 255 } else { 0 }, ClaimRef::new(u64::MAX, row));
        }
        let out = pages.finish();
        assert!(out[1..255].iter().all(Vec::is_empty));
        assert!(out[0].iter().map(|r| r.row()).eq((0..n).filter(|i| i % 113 != 0)));
        assert!(out[255].iter().map(|r| r.row()).eq((0..n).filter(|i| i % 113 == 0)));
        let last = CHUNK / 6 + 1;
        let r = ClaimRef::new(u64::MAX, last);
        assert_eq!(r.row(), last);
        assert_eq!(r.probe(1023), ClaimRef::new(u64::MAX, 0).probe(1023));
    }

    #[test]
    fn quantized_ingest_guards_both_ends_and_spills_exact_milliseconds() {
        let q = Ingest::new(-500);
        let mut spill = Vec::new();
        for sec in [q.0 - 1, q.0, q.0 + i64::from(SPILL) - 1, q.0 + i64::from(SPILL)] {
            let encoded = q.encode(sec * 1000, sec, &mut spill);
            assert_eq!(encoded < SPILL, (0..i64::from(SPILL)).contains(&(sec - q.0)));
            assert_eq!(q.decode(encoded, &spill), sec * 1000);
        }
        for ms in [i64::MAX, 1234, -499] {
            let encoded = q.encode(ms, BAD, &mut spill);
            assert_eq!(q.decode(encoded, &spill), ms);
        }
    }

    #[test]
    fn same_fingerprint_is_not_same_id_and_invalid_earliest_still_wins() {
        let id = 13 | (217 << 56);
        let alias = id ^ (1 << 45);
        let later = ClaimRef::new(id, 0);
        let distinct = ClaimRef::new(alias, 1);
        assert_eq!(later.probe(1023), distinct.probe(1023));
        let q = Ingest::new(0);
        let batch = |tokens, orders: &[i64], route| {
            let mut ingests = Vec::new();
            let rows = orders
                .iter()
                .map(|&ms| Row {
                    key: 0,
                    route,
                    order: q.encode(ms, BAD, &mut ingests),
                })
                .collect();
            Batch {
                tokens,
                rows,
                ingests,
                wide: vec![],
                prices: vec![],
                counts: Box::new([[0; 4]; NS]),
            }
        };
        let a = batch(vec![id, alias], &[3000, 4000], 0);
        let b = batch(vec![id], &[1234], INVALID);
        let lost = losers(
            vec![(0, vec![later, distinct]), (1, vec![ClaimRef::new(id, 0)])],
            &[a, b],
            q,
        );
        assert_eq!(lost, [(0, 0)]);
    }
}
