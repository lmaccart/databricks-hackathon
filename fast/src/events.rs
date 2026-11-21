//! Send payloads directly to the session partition. A separate, smaller
//! stream of claims (ID, ingest time, payload location) decides which payloads
//! must be removed. In particular, an invalid copy has a claim but no payload.
use super::*;

#[derive(Clone, Copy)]
struct Claim {
    id: u64,
    ingest: i64,
    target: u32,
}

const _: () = assert!(std::mem::size_of::<Claim>() == 24);

// Reserve the lowest 2^32 i64 values for byte offsets. Even year 0000
// with a ±23:59 offset is far above this range, and F3 is nonnegative.
const OFFSET_MAX: i64 = i64::MIN + u32::MAX as i64;

#[inline]
fn exact_ingest(ingest: i64, data: &[u8]) -> i64 {
    if ingest <= OFFSET_MAX {
        let offset = (ingest - i64::MIN) as usize;
        let stamp = data
            .get(offset..)
            .and_then(|s| s.first_chunk::<20>())
            .expect("deferred ingest must be in its source chunk");
        parse_ingest(stamp)
    } else {
        ingest
    }
}

const DATE_SLOTS: usize = 32;

#[derive(Clone, Copy)]
struct Date {
    day: i32, // i32::MIN is outside the range of decoded F1/F2/F3 timestamps' UTC days
    key: [u8; 10],
}

struct Dates([Date; DATE_SLOTS]);

impl Dates {
    fn new() -> Self {
        Self(
            [Date {
                day: i32::MIN,
                key: [0; 10],
            }; DATE_SLOTS],
        )
    }

    #[inline(always)]
    fn slot(day: i64) -> usize {
        day as usize & (DATE_SLOTS - 1)
    }

    #[inline]
    fn ingest(&mut self, raw: &[u8], ts: i64, data: &[u8]) -> (i64, bool) {
        if let Ok(stamp) = <&[u8; 20]>::try_from(raw) {
            let day = ts.div_euclid(DAY);
            // Index by event UTC day, but require both the full day and exact
            // date bytes: aliases can evict entries, never turn into false hits.
            let date = &self.0[Self::slot(day)];
            if i64::from(date.day) == day && date.key == stamp[..10] && ts::valid_f1_time(stamp) {
                // On the same UTC date lateness is strictly less than 24 h.
                // CSV field values are slices of this chunk, even when quoted.
                // Check their full range before keeping just the integer offset.
                let offset = stamp.as_ptr().addr().wrapping_sub(data.as_ptr().addr());
                if offset <= u32::MAX as usize && data.len() >= 20 && offset <= data.len() - 20 {
                    return (i64::MIN + offset as i64, false);
                }
            }
        }
        self.ingest_fallback(raw, ts)
    }

    #[inline(never)]
    fn ingest_fallback(&mut self, raw: &[u8], ts: i64) -> (i64, bool) {
        let ingest = parse_ingest(raw);
        // Only a successful F1 parse can teach us that these bytes name a UTC date.
        if ingest != i64::MAX && raw.len() == 20 && raw[4] == b'-' {
            let day = ingest.div_euclid(DAY);
            self.0[Self::slot(day)] = Date {
                day: day as i32, // F1 dates (0000..=9999) fit in i32
                key: raw[..10].try_into().unwrap(),
            };
        }
        (ingest, ingest.saturating_sub(ts) > DAY)
    }
}

// One chunk starts at most CHUNK+1 rows: extending its final line never adds a
// second newline. A row index within a bucket + its bucket number fit in u32.
const _: () = assert!(NS.is_power_of_two() && (CHUNK + 1) * NS < u32::MAX as usize);
const NONE: u32 = u32::MAX;
const KILLED: u32 = u32::MAX; // never a valid purchase price

#[derive(Clone, Copy)]
struct Location {
    batch: usize,
    target: u32,
}

#[derive(Clone, Copy, Default)]
struct Pending {
    id: u64,
    uid: u64, // zero is never the key of a nonempty ID
    hash: u64,
    ts: i64,
    ingest: i64,
    price: u32,
    cat: u32,
}

fn parse<'a>(
    c: &Chunk<'a>,
    sm: &Small,
    people: &People<'a>,
    ws: i64,
    we: i64,
    hs: &RandomState,
) -> (Vec<Vec<PEv>>, Vec<Vec<Claim>>) {
    let reserve = reserve_per_shard(c.data);
    let mut payload: Vec<Vec<PEv>> = (0..NS).map(|_| Vec::with_capacity(reserve)).collect();
    let mut claims: Vec<Vec<Claim>> = (0..NS).map(|_| Vec::with_capacity(reserve + reserve / 8)).collect();
    let mut emit = |e: Pending| {
        let mut target = NONE;
        if e.uid != 0 {
            let person = people.lookup(e.uid, e.hash);
            let p = shard(person);
            target = (payload[p].len() * NS + p) as u32;
            payload[p].push(PEv {
                person,
                ts: e.ts,
                price: e.price,
                cat: e.cat,
            });
        }
        claims[shard(hs.hash_one(e.id))].push(Claim {
            id: e.id,
            ingest: e.ingest,
            target,
        });
    };
    let mut ring = [Pending::default(); 16];
    let mut n = 0usize;
    let mut dates = Dates::new();
    for_rows(c, |r| {
        if r[0].is_empty() {
            return;
        }
        let id = people.codec.key(r[0]);
        let ts = parse_ts(r[2]);
        let (ingest, late) = dates.ingest(r[3], ts, c.data);
        let mut next = Pending {
            id,
            ts,
            ingest,
            ..Default::default()
        };
        if !r[1].is_empty() && !r[5].is_empty() && (ws..we).contains(&ts) && !late {
            let purchase = match r[4] {
                b"purchase" => parse_price(r[6]),
                b"view" | b"cart" => Some(0),
                _ => None,
            };
            if let Some(price) = purchase {
                let cat = if price == 0 {
                    0
                } else {
                    *sm.product_cat.get(r[5]).unwrap_or(&sm.unknown_cat)
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
    (payload, claims)
}

#[inline]
fn probe(id: u64, mask: usize) -> (usize, u64) {
    let x = (id ^ (id >> 27)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let bits = 1u64.rotate_left((x >> 52) as u32) | 1u64.rotate_left((x >> 58) as u32);
    (((x >> 17) as usize) & mask, bits)
}

fn losers(parts: Vec<(usize, Vec<Claim>)>, chunks: &[Chunk<'_>]) -> Vec<Location> {
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
        for c in claims {
            let (i, b) = probe(c.id, mask);
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
    let mut best = Map::with_capacity_and_hasher(suspects, rs());
    for (batch, claims) in parts {
        let data = chunks[batch].data;
        for c in claims {
            let (i, b) = probe(c.id, mask);
            if bloom[i].1 & b != b {
                continue;
            }
            let loc = Location {
                batch,
                target: c.target,
            };
            let ingest = exact_ingest(c.ingest, data);
            let lost = match best.entry(c.id) {
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
    ws: i64,
    we: i64,
    hs: &RandomState,
    log: &impl Fn(&str),
) -> Vec<Vec<Vec<PEv>>> {
    let parsed: Vec<_> = chunks.par_iter().map(|c| parse(c, sm, people, ws, we, hs)).collect();
    log("events parsed");
    let mut payloads = Vec::with_capacity(parsed.len());
    let mut claims: Vec<Vec<_>> = (0..NS).map(|_| Vec::new()).collect();
    for (batch, (payload, cs)) in parsed.into_iter().enumerate() {
        payloads.push(payload);
        for (bucket, c) in claims.iter_mut().zip(cs) {
            if !c.is_empty() {
                bucket.push((batch, c));
            }
        }
    }
    let removals: Vec<_> = claims.into_par_iter().map(|parts| losers(parts, chunks)).collect();
    let mut by_batch: Vec<Vec<u32>> = (0..payloads.len()).map(|_| Vec::new()).collect();
    for locations in removals {
        for loc in locations {
            by_batch[loc.batch].push(loc.target);
        }
    }
    payloads.par_iter_mut().zip(by_batch).for_each(|(payload, locations)| {
        for target in locations {
            let x = target as usize;
            payload[x % NS][x / NS].price = KILLED;
        }
        for dest in payload {
            dest.retain(|e| e.price != KILLED);
        }
    });
    payloads
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_date_cache_handles_collisions_and_negative_days() {
        let first = b"2028-01-28T12:00:00Z";
        let leap = b"2028-02-29T12:00:00Z";
        let pre_epoch = b"1969-12-31T23:59:59Z";
        let mut dates = Dates::new();
        let (a, b) = (parse_ingest(first), parse_ingest(leap));
        assert_eq!(Dates::slot(a.div_euclid(DAY)), Dates::slot(b.div_euclid(DAY)));
        assert_eq!(dates.ingest(first, a, first), (a, false));
        assert_eq!(dates.ingest(first, a, first), (i64::MIN, false));
        assert_eq!(dates.ingest(leap, a, leap), (b, true));
        assert_eq!(dates.ingest(first, a, first), (a, false)); // displaced by leap
        assert_eq!(dates.ingest(first, b, first), (a, false)); // different date, though both alias
        let later = b"2028-01-30T12:00:00Z";
        assert_eq!(dates.ingest(later, a, later), (parse_ingest(later), true)); // same 8-byte prefix
        let before = parse_ingest(pre_epoch);
        assert_eq!(dates.ingest(pre_epoch, before, pre_epoch), (before, false));
        let (offset, late) = dates.ingest(pre_epoch, -1, pre_epoch);
        assert_eq!(offset, i64::MIN);
        assert_eq!(exact_ingest(offset, pre_epoch), before);
        assert!(!late);
        // Even an identical value outside this chunk cannot be stored as an offset.
        let unrelated = *pre_epoch;
        assert_eq!(dates.ingest(&unrelated, before, pre_epoch), (before, false));
    }

    #[test]
    fn cached_suffix_matches_f1_parser_for_single_byte_mutations() {
        let base = b"2028-02-29T23:59:59Z";
        let epoch = parse_ingest(base);
        let mut dates = Dates::new();
        assert_eq!(dates.ingest(base, epoch, base), (epoch, false));
        for i in 10..20 {
            for byte in 0..=255u8 {
                let mut stamp = *base;
                stamp[i] = byte;
                let want = parse_ingest(&stamp);
                let (ingest, late) = dates.ingest(&stamp, epoch, &stamp);
                assert_eq!(ingest, if want == i64::MAX { want } else { i64::MIN });
                assert_eq!(exact_ingest(ingest, &stamp), want);
                assert_eq!(late, want.saturating_sub(epoch) > DAY);
            }
        }
    }

    #[test]
    fn bloom_alias_keeps_distinct_id_and_invalid_winner_kills_valid_loser() {
        // With one word, different IDs can have exactly the same two bit
        // positions. Force such a collision rather than relying on a random seed.
        let id = 13;
        let alias = (id + 1..).find(|&k| probe(k, 0) == probe(id, 0)).unwrap();
        let c = csv::chunks_of(b"h\nx\n", &[], Path::new("test.csv"))[0];
        let lost = losers(
            vec![
                (
                    0,
                    vec![
                        Claim {
                            id,
                            ingest: 3,
                            target: 0,
                        },
                        Claim {
                            id: alias,
                            ingest: 4,
                            target: 1,
                        },
                    ],
                ),
                (
                    1,
                    vec![Claim {
                        id,
                        ingest: 1,
                        target: NONE,
                    }],
                ),
            ],
            &[c, c],
        );
        assert_eq!(lost.len(), 1);
        assert_eq!((lost[0].batch, lost[0].target), (0, 0));
    }
}
