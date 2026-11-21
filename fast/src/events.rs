//! Send payloads directly to the session partition. A separate, smaller
//! duplicate census followed by an exact claim pass decides which payloads
//! must be removed. In particular, an invalid copy has a claim but no payload.
use super::*;

#[derive(Clone, Copy)]
struct Claim {
    id: u64,
    ingest: i64,
    target: u32,
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
    signature: u64,
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
    repeated: &[(u64, u64)],
    hs: &RandomState,
) -> (Vec<Vec<PEv>>, Vec<Vec<Claim>>) {
    let reserve = reserve_per_shard(c.data);
    let mut payload: Vec<Vec<PEv>> = (0..NS).map(|_| Vec::with_capacity(reserve)).collect();
    let mut claims: Vec<Vec<Claim>> = (0..NS).map(|_| Vec::new()).collect();
    let mask = repeated.len() - 1;
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
        let (i, bits) = probe(e.signature, mask);
        if repeated[i].1 & bits == bits {
            claims[shard(hs.hash_one(e.id))].push(Claim {
                id: e.id,
                ingest: e.ingest,
                target,
            });
        }
    };
    let mut ring = [Pending::default(); 16];
    let mut n = 0usize;
    for_rows(c, |r| {
        if r[0].is_empty() {
            return;
        }
        let id = people.codec.key(r[0]);
        let ingest = parse_ingest(r[3]);
        let ts = parse_ts(r[2]);
        let mut next = Pending {
            id,
            signature: hs.hash_one(r[0]),
            ts,
            ingest,
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

// The intersection identifies bits seen by two different workers. A duplicate
// in one worker has already marked both bits in that worker's repeated set.
fn merge_census(into: &mut [(u64, u64)], other: &[(u64, u64)]) {
    into.par_iter_mut().zip(other.par_iter()).for_each(|(a, b)| {
        a.1 |= b.1 | (a.0 & b.0);
        a.0 |= b.0;
    });
}

fn census_chunk(chunk: &Chunk<'_>, hs: &RandomState, census: &mut [(u64, u64)]) {
    let mask = census.len() - 1;
    csv::for_first_id(chunk, |id| {
        let (i, bits) = probe(hs.hash_one(id), mask);
        let (seen, repeated) = &mut census[i];
        if *seen & bits == bits {
            *repeated |= bits;
        }
        *seen |= bits;
    });
}

fn losers(parts: Vec<(usize, Vec<Claim>)>) -> Vec<Location> {
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
        for c in claims {
            let (i, b) = probe(c.id, mask);
            if bloom[i].1 & b != b {
                continue;
            }
            let loc = Location {
                batch,
                target: c.target,
            };
            let lost = match best.entry(c.id) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert((c.ingest, loc));
                    continue;
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    if c.ingest < e.get().0 {
                        e.insert((c.ingest, loc)).1
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
    if chunks.is_empty() {
        return Vec::new();
    }
    // This is only a sizing hint: collisions and underestimated row counts
    // increase exact work, never change which IDs are retained.
    let estimated_rows = chunks
        .iter()
        .map(|c| reserve_per_shard(c.data).saturating_mul(NS))
        .fold(0usize, usize::saturating_add);
    let words = estimated_rows.div_ceil(8).clamp(1, 1 << 24).next_power_of_two();
    let workers = rayon::current_num_threads().min(8).min(chunks.len());
    // Chunks from different files can be header-sized or full-sized. Balance
    // bytes instead of chunk count, without sharing a filter between workers.
    let mut ordered: Vec<_> = chunks.iter().collect();
    ordered.sort_unstable_by_key(|c| std::cmp::Reverse(c.data.len()));
    let mut groups: Vec<Vec<_>> = (0..workers).map(|_| Vec::new()).collect();
    let mut sizes = vec![0usize; workers];
    for chunk in ordered {
        let worker = (0..workers).min_by_key(|&i| sizes[i]).unwrap();
        sizes[worker] = sizes[worker].saturating_add(chunk.data.len());
        groups[worker].push(chunk);
    }
    let groups: Vec<_> = groups
        .into_par_iter()
        .map(|group| {
            let mut census = vec![(0u64, 0u64); words];
            for chunk in group {
                census_chunk(chunk, hs, &mut census);
            }
            census
        })
        .collect();
    let mut groups = groups.into_iter();
    let mut census = groups.next().unwrap();
    for other in groups {
        merge_census(&mut census, &other);
    }
    log("events census");
    let parsed: Vec<_> = chunks
        .par_iter()
        .map(|c| parse(c, sm, people, ws, we, &census, hs))
        .collect();
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
    let removals: Vec<_> = claims.into_par_iter().map(losers).collect();
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
    fn census_finds_local_and_cross_worker_duplicates_despite_aliases() {
        let mask = 31;
        let ids = [7u64, 91, 2000, 9_000_000_000, u64::MAX];
        let mut a = vec![(0u64, 0u64); mask + 1];
        let mut b = a.clone();
        let add = |bloom: &mut [(u64, u64)], id| {
            let (i, bits) = probe(id, mask);
            let word = &mut bloom[i];
            if word.0 & bits == bits {
                word.1 |= bits;
            }
            word.0 |= bits;
        };
        for id in [ids[0], ids[1], ids[1], ids[2]] {
            add(&mut a, id);
        }
        for id in [ids[0], ids[3], ids[4], ids[4]] {
            add(&mut b, id);
        }
        merge_census(&mut a, &b);
        for id in [ids[0], ids[1], ids[4]] {
            let (i, bits) = probe(id, mask);
            assert_eq!(a[i].1 & bits, bits);
        }
    }

    #[test]
    fn bloom_alias_keeps_distinct_id_and_invalid_winner_kills_valid_loser() {
        // With one word, different IDs can have exactly the same two bit
        // positions. Force such a collision rather than relying on a random seed.
        let id = 13;
        let alias = (id + 1..).find(|&k| probe(k, 0) == probe(id, 0)).unwrap();
        let lost = losers(vec![
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
        ]);
        assert_eq!(lost.len(), 1);
        assert_eq!((lost[0].batch, lost[0].target), (0, 0));
    }
}
