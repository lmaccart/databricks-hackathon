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

// An emitted event has at least six commas. At most one row extends past
// CHUNK, so the stream kind, per-stream ordinal and shard fit in the claim.
const _: () = assert!(NS.is_power_of_two() && (CHUNK / 6 + 1) * 4 * NS < u32::MAX as usize);
const NONE: u32 = u32::MAX;

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
    packing: Option<Packing>,
    hs: &RandomState,
) -> (Vec<Payload>, Vec<Vec<Claim>>) {
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
    let mut emit = |e: Pending| {
        let mut target = NONE;
        if e.uid != 0 {
            let person = people.lookup(e.uid, e.hash);
            let p = person.shard();
            let slot = payload[p].push(person, e.ts, e.price, e.cat, packing, people);
            target = (slot * NS + p) as u32;
        }
        claims[shard(hs.hash_one(e.id))].push(Claim {
            id: e.id,
            ingest: e.ingest,
            target,
        });
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
    (payload, claims)
}

#[inline]
fn probe(id: u64, mask: usize) -> (usize, u64) {
    let x = (id ^ (id >> 27)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let bits = 1u64.rotate_left((x >> 52) as u32) | 1u64.rotate_left((x >> 58) as u32);
    (((x >> 17) as usize) & mask, bits)
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
    packing: Option<Packing>,
    hs: &RandomState,
    log: &impl Fn(&str),
) -> Vec<Vec<Payload>> {
    let parsed: Vec<_> = chunks
        .par_iter()
        .map(|c| parse(c, sm, people, ws, we, packing, hs))
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
