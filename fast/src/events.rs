//! Route tentative payloads directly to the person partition. An independent
//! stream of exact event tokens includes invalid rows, but not malformed rows.
use super::*;
use memchr::memchr3;

const ROUTE_BITS: u32 = 11;
const ROUTE_MASK: u32 = (1 << ROUTE_BITS) - 1;
const INVALID: usize = NS * 4;
const ANCHOR_ROWS: usize = 128;
const WIDE_OFFSET: u32 = 1 << (32 - ROUTE_BITS - 1);
const _: () = {
    assert!(INVALID <= ROUTE_MASK as usize && CHUNK < u32::MAX as usize);
    assert!((CHUNK / WIDE_OFFSET as usize + 2) * ANCHOR_ROWS < WIDE_OFFSET as usize);
};

/// Ordinal-aligned side buffer: a route (not a payload slot) and a byte offset
/// into the original unquoted ingest field. Ingest is required once during
/// validation of a tentative payload, but no parsed ingest is retained.
#[derive(Default)]
struct Origins {
    codes: Vec<u32>,
    anchors: Vec<usize>,
    wide: Vec<usize>,
}

impl Origins {
    #[inline]
    fn push(&mut self, offset: usize, route: usize) {
        if self.codes.len() % ANCHOR_ROWS == 0 {
            self.anchors.push(offset);
        }
        let delta = offset - self.anchors[self.anchors.len() - 1];
        let encoded = if delta < WIDE_OFFSET as usize {
            delta as u32
        } else {
            let i = self.wide.len() as u32;
            // Offsets are increasing and anchors reset every ANCHOR_ROWS.
            // This can happen in at most one group per WIDE_OFFSET source
            // bytes before CHUNK, plus the final (possibly enormous) row.
            debug_assert!(i < WIDE_OFFSET);
            self.wide.push(offset);
            WIDE_OFFSET | i
        };
        self.codes.push((encoded << ROUTE_BITS) | route as u32);
    }

    #[inline]
    fn ingest(&self, ordinal: usize, data: &[u8], clock: &ManifestClock) -> i64 {
        let code = self.codes[ordinal] >> ROUTE_BITS;
        let offset = if code < WIDE_OFFSET {
            self.anchors[ordinal / ANCHOR_ROWS] + code as usize
        } else {
            self.wide[(code ^ WIDE_OFFSET) as usize]
        };
        let tail = &data[offset..];
        // The CSV parser already validated the whole row and removed the
        // opening quote; comma, quote, LF and EOF are the only possible ends.
        let field = &tail[..memchr3(b',', b'"', b'\n', tail).unwrap_or(tail.len())];
        clock.ingest_for_claim(field).0
    }
}

struct Batch {
    payload: Vec<Payload>,
    ids: Vec<u64>,
    origins: Origins,
}

#[derive(Clone, Copy, Default)]
struct Pending {
    id: u64,
    uid: u64, // zero is never the key of a nonempty ID
    hash: u64,
    ts: i64,
    ingest_offset: usize,
    price: u32,
    cat: u32,
}

fn parse<'a>(
    c: &Chunk<'a>,
    sm: &Small,
    people: &People<'a>,
    window: (i64, i64, &ManifestClock),
    packing: Option<Packing>,
) -> Batch {
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
    let claim_reserve = (reserve + (reserve / 8)) * NS;
    let mut ids = Vec::with_capacity(claim_reserve);
    let mut origins = Origins {
        codes: Vec::with_capacity(claim_reserve),
        anchors: Vec::with_capacity(claim_reserve.div_ceil(ANCHOR_ROWS)),
        wide: Vec::new(),
    };
    let mut emit = |e: Pending| {
        let mut route = INVALID;
        if e.uid != 0 {
            let person = people.lookup(e.uid, e.hash);
            let p = shard(match person {
                Identity::Linked(routed) | Identity::Standalone(routed) => routed,
            });
            let slot = payload[p].push(person, e.ts, e.price, e.cat, packing, people);
            route = p * 4 + (slot % 4);
        }
        ids.push(e.id);
        origins.push(e.ingest_offset, route);
    };
    let mut ring = [Pending::default(); 16];
    let mut n = 0usize;
    for_rows(c, |r| {
        if r[0].is_empty() {
            return;
        }
        // Codec keys (including interned names) and mix are both exact.
        let id = links::mix(people.codec.key(r[0]));
        let ts = clock.parse_ts(r[2]);
        let mut next = Pending {
            id,
            ts,
            ingest_offset: r[3].as_ptr() as usize - c.data.as_ptr() as usize,
            ..Default::default()
        };
        if !r[1].is_empty() && !r[5].is_empty() && (ws..we).contains(&ts) {
            let price = match r[4] {
                b"purchase" => parse_price(r[6]),
                b"view" | b"cart" => Some(0),
                _ => None,
            };
            if let Some(price) = price.filter(|_| clock.ingest_for_claim(r[3]).0.saturating_sub(ts) <= DAY) {
                next.uid = people.codec.key(r[1]);
                next.hash = links::mix(next.uid);
                people.prefetch(next.hash);
                next.price = price;
                next.cat = if price == 0 {
                    0
                } else {
                    sm.product_cat.get(r[5]).unwrap_or(sm.unknown_cat)
                };
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
    Batch { payload, ids, origins }
}

#[inline]
fn probe(id: u64, mask: usize) -> (usize, u64) {
    let bits = (1 << ((id >> 32) & 31)) | (1 << (32 + ((id >> 37) & 31)));
    (id as usize & mask, bits)
}

#[derive(Clone, Copy, Default)]
struct Witness {
    seen: u64,
    repeated: u64,
}

impl Witness {
    #[inline]
    fn add(&mut self, bits: u64) {
        self.repeated |= self.seen & bits;
        self.seen |= bits;
    }

    #[inline]
    fn merge(&mut self, other: Self) {
        self.repeated |= other.repeated | (self.seen & other.seen);
        self.seen |= other.seen;
    }
}

#[inline]
fn prefetch<T>(filter: &[T], id: u64) {
    let ptr = &filter[id as usize & (filter.len() - 1)] as *const T;
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

fn summarize(batches: &[Batch], words: usize) -> Vec<u64> {
    let lanes = rayon::current_num_threads().min(batches.len()).min(8);
    (0..lanes)
        .into_par_iter()
        .map(|lane| {
            let mut local = vec![Witness::default(); words];
            for batch in batches.iter().skip(lane).step_by(lanes) {
                for (i, &id) in batch.ids.iter().enumerate() {
                    if let Some(&next) = batch.ids.get(i + 12) {
                        prefetch(&local, next);
                    }
                    let (word, bits) = probe(id, words - 1);
                    local[word].add(bits);
                }
            }
            local
        })
        .reduce_with(|mut a, b| {
            for (x, y) in a.iter_mut().zip(b) {
                x.merge(y);
            }
            a
        })
        .unwrap_or_default()
        .into_iter()
        .map(|w| w.repeated)
        .collect()
}

#[derive(Clone, Copy)]
struct Candidate {
    id: u64,
    batch: u32,
    ordinal: u32,
}

fn candidates(batch: &Batch, index: usize, filter: &[u64]) -> Vec<Vec<Candidate>> {
    let mut buckets: Vec<Vec<_>> = (0..NS).map(|_| Vec::new()).collect();
    let index = u32::try_from(index).expect("too many event chunks");
    for (ordinal, &id) in batch.ids.iter().enumerate() {
        if let Some(&next) = batch.ids.get(ordinal + 12) {
            prefetch(filter, next);
        }
        let (word, bits) = probe(id, filter.len() - 1);
        if filter[word] & bits == bits {
            buckets[(id >> (64 - NS.trailing_zeros())) as usize].push(Candidate {
                id,
                batch: index,
                ordinal: ordinal as u32,
            });
        }
    }
    buckets
}

fn losers<'a>(
    mut rows: Vec<Candidate>,
    batches: &[Batch],
    chunks: &[Chunk<'a>],
    clock: &ManifestClock,
) -> Vec<Candidate> {
    // Saturation or adversarial aliases only increase the number of exact
    // tokens sorted. Even then, no singleton requires a source revisit.
    rows.sort_unstable_by_key(|x| x.id);
    let mut lost = Vec::new();
    for group in rows.chunk_by(|a, b| a.id == b.id).filter(|g| g.len() > 1) {
        let order = |x: Candidate| {
            let b = x.batch as usize;
            let ingest = batches[b].origins.ingest(x.ordinal as usize, chunks[b].data, clock);
            (ingest, x.batch, x.ordinal)
        };
        let mut best = group[0];
        let mut best_order = order(best);
        for &c in &group[1..] {
            let next = order(c);
            let loser = if next < best_order {
                best_order = next;
                std::mem::replace(&mut best, c)
            } else {
                c
            };
            let route = batches[loser.batch as usize].origins.codes[loser.ordinal as usize] & ROUTE_MASK;
            if route != INVALID as u32 {
                lost.push(loser);
            }
        }
    }
    lost
}

fn repair(batch: &mut Batch, mut locations: Vec<u32>) {
    if locations.is_empty() {
        return;
    }
    locations.sort_unstable();
    let mut counts = [0usize; INVALID + 1];
    let mut start = 0;
    for ordinal in locations.into_iter().map(|x| x as usize) {
        for &code in &batch.origins.codes[start..ordinal] {
            counts[(code & ROUTE_MASK) as usize] += 1;
        }
        let route = (batch.origins.codes[ordinal] & ROUTE_MASK) as usize;
        let index = counts[route];
        counts[route] += 1;
        start = ordinal + 1;
        let bucket = &mut batch.payload[route / 4];
        match route % 4 {
            0 => bucket.packed.times[index] = u64::MAX,
            1 => bucket.packed.purchases[index].price = 0,
            2 => bucket.wide.times[index].ts = BAD,
            _ => bucket.wide.purchases[index].price = 0,
        }
    }
    for dest in &mut batch.payload {
        dest.packed.times.retain(|&key| key != u64::MAX);
        dest.packed.purchases.retain(|p| p.price != 0);
        dest.wide.times.retain(|key| key.ts != BAD);
        dest.wide.purchases.retain(|p| p.price != 0);
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
    let batches: Vec<_> = chunks
        .par_iter()
        .map(|c| parse(c, sm, people, window, packing))
        .collect();
    log("events parsed");
    let n: usize = batches.iter().map(|x| x.ids.len()).sum();
    if n < 2 {
        return batches.into_iter().map(|b| b.payload).collect();
    }
    // Bound auxiliary memory independently of input distribution.
    let words = n.div_ceil(24).next_power_of_two().min(1 << 22);
    let filter = summarize(&batches, words);
    log("event witnesses");
    let candidates: Vec<_> = batches
        .par_iter()
        .enumerate()
        .map(|(b, batch)| candidates(batch, b, &filter))
        .collect();
    drop(filter);
    let mut buckets: Vec<Vec<Vec<Candidate>>> = (0..NS).map(|_| Vec::new()).collect();
    for parts in candidates {
        for (dest, v) in buckets.iter_mut().zip(parts) {
            if !v.is_empty() {
                dest.push(v);
            }
        }
    }
    let lost: Vec<_> = buckets
        .into_par_iter()
        .map(|parts| {
            let mut rows = Vec::with_capacity(parts.iter().map(Vec::len).sum());
            for p in parts {
                rows.extend(p);
            }
            losers(rows, &batches, chunks, window.2)
        })
        .collect();
    let mut by_batch = vec![Vec::new(); batches.len()];
    for c in lost.into_iter().flatten() {
        by_batch[c.batch as usize].push(c.ordinal);
    }
    batches
        .into_par_iter()
        .zip(by_batch)
        .map(|(mut b, locations)| {
            repair(&mut b, locations);
            b.payload
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliased_probes_and_cross_lane_repeats_keep_exact_ids() {
        let id = 13 | (217 << 56);
        let alias = id ^ (1 << 24);
        let (i, bits) = probe(id, 1023);
        assert_eq!(probe(alias, 1023), (i, bits));

        let mut a = Witness::default();
        let mut b = Witness::default();
        a.add(bits);
        b.add(bits);
        assert_eq!(a.repeated | b.repeated, 0);
        a.merge(b);
        assert_eq!(a.repeated, bits);
        let mut saturated = a;
        saturated.add(!bits);
        saturated.add(!bits);
        assert_eq!(saturated.repeated, u64::MAX);

        let clock = ManifestClock::new(parse_ts(b"2026-10-01T00:00:00Z"), parse_ts(b"2026-10-15T00:00:00Z"));
        let mut batch = Batch {
            payload: Vec::new(),
            ids: vec![id, alias, id],
            origins: Origins::default(),
        };
        let data = b"2026-10-02T10:00:00Z\nnot read\n2026-10-02T09:00:00Z";
        batch.origins.push(0, 0);
        batch.origins.push(21, 0);
        batch.origins.push(b"2026-10-02T10:00:00Z\nnot read\n".len(), INVALID);
        let chunks = csv::chunks_of(
            b"ingest\n2026-10-02T10:00:00Z\nnot read\n2026-10-02T09:00:00Z",
            &["ingest"],
            std::path::Path::new("test.csv"),
        );
        assert_eq!(chunks[0].data, data);
        let mut filter = vec![0; 1024];
        filter[i] = saturated.repeated;
        let suspects = candidates(&batch, 0, &filter).remove(217);
        assert_eq!(suspects.len(), 3);
        let lost = losers(suspects, &[batch], &chunks, &clock);
        assert_eq!(lost.iter().map(|x| x.ordinal).collect::<Vec<_>>(), [0]);
    }

    #[test]
    fn field_offset_wide_spill_and_mid_block_anchor() {
        let clock = ManifestClock::new(0, DAY);
        let mut origin = Origins::default();
        let mut source = vec![b'x'; WIDE_OFFSET as usize + 19];
        source[WIDE_OFFSET as usize..(WIDE_OFFSET as usize + 6)].copy_from_slice(b"\"123\",");
        source.extend_from_slice(b"456");
        origin.push(0, INVALID);
        origin.push(WIDE_OFFSET as usize + 1, 17);
        for _ in 2..ANCHOR_ROWS {
            origin.push(WIDE_OFFSET as usize + 1, 17);
        }
        origin.push(WIDE_OFFSET as usize + 19, 17);
        assert_eq!(origin.ingest(1, &source, &clock), 123);
        assert_eq!(origin.ingest(ANCHOR_ROWS, &source, &clock), 456);
    }
}
