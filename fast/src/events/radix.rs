//! Partition immutable typed rows, not locators into the input or another
//! full-size allocation. Each event-ID bucket then owns all copies.
use super::*;
use std::mem::MaybeUninit;

const RADIX: usize = 128;
const INVALID: u32 = 0xff_ffff;
const LINKED: u32 = 1 << 31;

#[derive(Clone, Copy)]
struct Row {
    // Reversible ID token, exact within and across radix buckets.
    id: u64,
    // Linked: canonical node rank + destination bits. Otherwise exact token.
    person: u64,
    // Relative milliseconds, followed by a tag bit and a 31-bit category.
    time_cat: u64,
    // Existing 40-bit ingest encoding plus 24-bit price/invalid sentinel.
    order_price: u64,
}

const _: () = assert!(std::mem::size_of::<Row>() == 32 && MAX_PRICE < INVALID as i64);

impl Row {
    #[inline]
    fn bucket(self) -> usize {
        (self.id >> (64 - RADIX.trailing_zeros())) as usize
    }

    #[inline]
    fn price(self) -> u32 {
        self.order_price as u32 & INVALID
    }

    #[inline]
    fn lane(self) -> usize {
        let linked = self.time_cat as u32 & LINKED != 0;
        shard(self.person) * 4 + (usize::from(!linked) * 2) + usize::from(self.price() != 0)
    }

    #[inline]
    fn ingest(self, spills: &[i64]) -> i64 {
        let order = (self.order_price as i64) >> 24;
        if order >= SPILL_TAG as i64 {
            spills[(order as u64 ^ SPILL_TAG) as usize]
        } else {
            order * 1000
        }
    }
}

struct Source {
    rows: Vec<Row>,
    counts: [usize; RADIX],
    spill: Vec<i64>,
}

fn parse_chunk<'a>(chunk: &Chunk<'a>, sm: &Small, people: &People<'a>, window: (i64, i64, &ManifestClock)) -> Source {
    let mut source = Source {
        rows: Vec::with_capacity(reserve_per_shard(chunk.data) * NS * 9 / 8),
        counts: [0; RADIX],
        spill: Vec::new(),
    };
    let mut emit = |e: Pending| {
        let (owner, flag, price) = if e.uid == 0 {
            (0, 0, INVALID)
        } else {
            match people.lookup(e.uid, e.hash) {
                Identity::Linked(p) => (p, LINKED, e.price),
                Identity::Standalone(p) => (p, 0, e.price),
            }
        };
        let row = Row {
            id: e.claim_key,
            person: owner,
            time_cat: (u64::from(e.ts.wrapping_sub(window.0) as u32) << 32) | u64::from(e.cat | flag),
            order_price: (e.claim_order << 24) | u64::from(price),
        };
        source.counts[row.bucket()] += 1;
        source.rows.push(row);
    };
    let mut ring = [Pending::default(); 16];
    let mut n = 0;
    word::for_events(chunk, sm, &people.codec, window, &mut source.spill, |next| {
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
    source
}

// Producer ranges are disjoint mutable slices; Rayon can fill them without
// locks or concurrent access to the same output slot.
struct Writer<'a, T> {
    rest: &'a mut [MaybeUninit<T>],
}

impl<T> Writer<'_, T> {
    #[inline]
    fn push(&mut self, item: T) {
        let (first, tail) = std::mem::take(&mut self.rest)
            .split_first_mut()
            .expect("range undercount");
        first.write(item);
        self.rest = tail;
    }
}

impl<T> Drop for Writer<'_, T> {
    fn drop(&mut self) {
        assert!(self.rest.is_empty() || std::thread::panicking(), "unfilled radix range");
    }
}

fn allocate<T>(size: usize) -> Vec<MaybeUninit<T>> {
    allocate_with_capacity(size, size)
}

fn allocate_with_capacity<T>(size: usize, capacity: usize) -> Vec<MaybeUninit<T>> {
    let mut v = Vec::with_capacity(capacity);
    v.resize_with(size, MaybeUninit::uninit);
    v
}

fn split<T>(
    storage: &mut [MaybeUninit<T>],
    counts: impl Iterator<Item = usize>,
) -> impl Iterator<Item = Writer<'_, T>> {
    let mut rest = storage;
    counts.map(move |n| {
        let (part, tail) = std::mem::take(&mut rest).split_at_mut(n);
        rest = tail;
        Writer { rest: part }
    })
}

// SAFETY: all counts were assigned slices of this allocation, and every
// writer returned with an empty slice. Restrict to Copy so unwinding while
// scattering cannot leak the destructors of partially initialized records.
unsafe fn finish<T: Copy>(v: Vec<MaybeUninit<T>>) -> Vec<T> {
    let (ptr, len, cap) = v.into_raw_parts();
    unsafe { Vec::from_raw_parts(ptr.cast(), len, cap) }
}

fn event_buckets(sources: Vec<Source>) -> (Vec<Vec<Row>>, Vec<i64>) {
    let mut buckets: Vec<_> = (0..RADIX)
        .map(|lane| allocate(sources.iter().map(|s| s.counts[lane]).sum()))
        .collect();
    let mut writers: Vec<_> = sources.iter().map(|_| Vec::with_capacity(RADIX)).collect();
    for (lane, data) in buckets.iter_mut().enumerate() {
        for (table, w) in writers
            .iter_mut()
            .zip(split(data, sources.iter().map(|s| s.counts[lane])))
        {
            table.push(w);
        }
    }
    let mut spill = Vec::new();
    let spills: Vec<_> = sources
        .iter()
        .map(|s| {
            let start = spill.len();
            spill.extend_from_slice(&s.spill);
            start
        })
        .collect();
    assert!((spill.len() as u64) < SPILL_TAG);
    sources
        .into_par_iter()
        .zip(writers)
        .zip(spills)
        .for_each(|((mut src, mut out), spill_start)| {
            if spill_start != 0 && !src.spill.is_empty() {
                for row in &mut src.rows {
                    if (row.order_price as i64 >> 24) >= SPILL_TAG as i64 {
                        row.order_price += (spill_start as u64) << 24;
                    }
                }
            }
            for row in src.rows {
                out[row.bucket()].push(row);
            }
        });
    (buckets.into_iter().map(|v| unsafe { finish(v) }).collect(), spill)
}

const LANES: usize = NS * 4;

struct Winners {
    rows: Vec<Row>,
    counts: [usize; LANES],
}

fn coalesce(mut rows: Vec<Row>, spills: &[i64]) -> Winners {
    let mut bloom = vec![(0u64, 0u64); rows.len().div_ceil(8).next_power_of_two()];
    let mask = bloom.len() - 1;
    let mut suspects = 0;
    let mut counts = [0; LANES];
    for (j, row) in rows.iter().enumerate() {
        if let Some(next) = rows.get(j + 6) {
            prefetch(&bloom, next.id, mask);
        }
        let (i, b) = probe(row.id, mask);
        let (seen, repeated) = &mut bloom[i];
        if *seen & b == b {
            *repeated |= b;
            suspects += 1;
        }
        *seen |= b;
        if row.price() != INVALID {
            counts[row.lane()] += 1;
        }
    }
    if suspects != 0 {
        // The repeat filter only saves exact-table work. An invalid row
        // participates in minimum ingest normally and can kill valid copies.
        let mut best = HashMap::with_capacity_and_hasher(suspects, BuildHasherDefault::<ClaimHasher>::default());
        for j in 0..rows.len() {
            let r = rows[j];
            let (i, b) = probe(r.id, mask);
            if bloom[i].1 & b != b {
                continue;
            }
            let ingest = r.ingest(spills);
            let lost = match best.entry(r.id) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert((ingest, j));
                    continue;
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    if ingest < e.get().0 {
                        e.insert((ingest, j)).1
                    } else {
                        j
                    }
                }
            };
            let row = &mut rows[lost];
            if row.price() != INVALID {
                counts[row.lane()] -= 1;
                row.order_price |= u64::from(INVALID);
            }
        }
    }
    Winners { rows, counts }
}

#[derive(Default)]
struct OwnerBuffers {
    packed: Vec<MaybeUninit<u64>>,
    packed_purchase: Vec<MaybeUninit<stream::Purchase<u64>>>,
    wide: Vec<MaybeUninit<stream::Wide>>,
    wide_purchase: Vec<MaybeUninit<stream::Purchase<stream::Wide>>>,
}

struct OwnerWriter<'a> {
    packed: Writer<'a, u64>,
    packed_purchase: Writer<'a, stream::Purchase<u64>>,
    wide: Writer<'a, stream::Wide>,
    wide_purchase: Writer<'a, stream::Purchase<stream::Wide>>,
}

fn owner_streams(winners: Vec<Winners>, ws: i64, pack: Packing) -> Vec<Payload> {
    let mut buffers: Vec<_> = (0..NS)
        .map(|owner| {
            let n = |lane| winners.iter().map(|w| w.counts[owner * 4 + lane]).sum();
            OwnerBuffers {
                packed: allocate_with_capacity(n(0), n(0) + n(1)),
                packed_purchase: allocate(n(1)),
                wide: allocate_with_capacity(n(2), n(2) + n(3)),
                wide_purchase: allocate(n(3)),
            }
        })
        .collect();
    let mut writers: Vec<_> = winners.iter().map(|_| Vec::with_capacity(NS)).collect();
    for (owner, b) in buffers.iter_mut().enumerate() {
        let n = |lane| winners.iter().map(move |w| w.counts[owner * 4 + lane]);
        let parts = split(&mut b.packed, n(0))
            .zip(split(&mut b.packed_purchase, n(1)))
            .zip(split(&mut b.wide, n(2)))
            .zip(split(&mut b.wide_purchase, n(3)));
        for (table, (((packed, packed_purchase), wide), wide_purchase)) in writers.iter_mut().zip(parts) {
            table.push(OwnerWriter {
                packed,
                packed_purchase,
                wide,
                wide_purchase,
            });
        }
    }
    winners.into_par_iter().zip(writers).for_each(|(w, mut table)| {
        for row in w.rows {
            let price = row.price();
            if price == INVALID {
                continue;
            }
            let out = &mut table[shard(row.person)];
            let ts = ws + (row.time_cat >> 32) as i64;
            let cat = (row.time_cat as u32) & !LINKED;
            if row.time_cat as u32 & LINKED != 0 {
                let key = pack.encode(row.person as u32, ts);
                if price == 0 {
                    out.packed.push(key);
                } else {
                    out.packed_purchase.push(stream::Purchase { key, price, cat });
                }
            } else {
                let key = stream::Wide { person: row.person, ts };
                if price == 0 {
                    out.wide.push(key);
                } else {
                    out.wide_purchase.push(stream::Purchase { key, price, cat });
                }
            }
        }
    });
    buffers
        .into_iter()
        .map(|b| unsafe {
            Payload {
                packed: Stream {
                    times: finish(b.packed),
                    purchases: finish(b.packed_purchase),
                },
                wide: Stream {
                    times: finish(b.wide),
                    purchases: finish(b.wide_purchase),
                },
            }
        })
        .collect()
}

pub(super) fn by_person<'a>(
    chunks: &[Chunk<'a>],
    sm: &Small,
    people: &People<'a>,
    window: (i64, i64, &ManifestClock),
    pack: Packing,
    log: &impl Fn(&str),
) -> Vec<Payload> {
    let sources = chunks.par_iter().map(|c| parse_chunk(c, sm, people, window)).collect();
    log("events parsed");
    let (buckets, spills) = event_buckets(sources);
    log("events radix grouped");
    let winners = buckets.into_par_iter().map(|rows| coalesce(rows, &spills)).collect();
    log("events coalesced");
    owner_streams(winners, window.0, pack)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radix_rebases_spills_and_coalesces_exact_keys_not_bloom_fingerprints() {
        let id = 13 | (5 << 57);
        let setup = |id, price, owner: u64, linked| Row {
            id,
            person: owner << SHARD_SHIFT,
            time_cat: u64::from(if linked { LINKED } else { 0 }),
            order_price: (SPILL_TAG << 24) | u64::from(price),
        };
        let source = |rows: Vec<Row>, time| {
            let mut counts = [0; RADIX];
            for row in &rows {
                counts[row.bucket()] += 1;
            }
            Source {
                rows,
                counts,
                spill: vec![time],
            }
        };
        let (buckets, spills) = event_buckets(vec![
            source(vec![setup(id, 7, 18, true)], 52),
            source(
                vec![setup(id, INVALID, 0, false), setup(id ^ (1 << 24), 0, 30, false)],
                51,
            ),
        ]);
        let winners: Vec<_> = buckets.into_iter().map(|rows| coalesce(rows, &spills)).collect();
        let pack = Packing::new(-1, 1, 1).unwrap();
        let payloads = owner_streams(winners, -1, pack);
        assert!(payloads[18].packed.purchases.is_empty());
        assert_eq!(payloads[30].wide.times.len(), 1);
        assert_eq!(payloads[30].wide.times[0].ts, -1);
    }
}
