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
) -> (Vec<Vec<PEv>>, Vec<Claim>, Vec<Vec<u64>>) {
    let reserve = reserve_per_shard(c.data);
    let mut payload: Vec<Vec<PEv>> = (0..NS).map(|_| Vec::with_capacity(reserve)).collect();
    let mut claims = Vec::with_capacity(reserve * NS);
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
        claims.push(Claim {
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
    let mut descriptors: Vec<Vec<u64>> = (0..NS).map(|_| Vec::with_capacity(reserve)).collect();
    for (i, claim) in claims.iter().enumerate() {
        let hash = hs.hash_one(claim.id);
        descriptors[shard(hash)].push(((hash as u32 as u64) << 32) | i as u64);
    }
    (payload, claims, descriptors)
}

#[derive(Clone, Copy)]
struct Slot {
    tag: u32,
    index: u32,
    batch: usize,
}

fn losers(parts: Vec<(usize, Vec<u64>)>, claims: &[Vec<Claim>]) -> Vec<Location> {
    let n: usize = parts.iter().map(|(_, v)| v.len()).sum();
    let mut remove = Vec::new();
    if n == 0 {
        return remove;
    }
    let size = n.checked_mul(2).unwrap().next_power_of_two();
    let mut table = vec![
        Slot {
            tag: 0,
            index: NONE,
            batch: 0
        };
        size
    ];
    let mask = size - 1;
    for (batch, descriptors) in parts {
        for descriptor in descriptors {
            let tag = (descriptor >> 32) as u32;
            let index = descriptor as u32;
            let mut pos = tag as usize & mask;
            loop {
                let slot = &mut table[pos];
                if slot.index == NONE {
                    *slot = Slot { tag, index, batch };
                    break;
                }
                if slot.tag == tag {
                    let current = claims[batch][index as usize];
                    let best = claims[slot.batch][slot.index as usize];
                    // Tags are only a filter; the codec key is an exact ID.
                    if current.id == best.id {
                        let lost = if current.ingest < best.ingest {
                            let loc = Location {
                                batch: slot.batch,
                                target: best.target,
                            };
                            *slot = Slot { tag, index, batch };
                            loc
                        } else {
                            Location {
                                batch,
                                target: current.target,
                            }
                        };
                        if lost.target != NONE {
                            remove.push(lost);
                        }
                        break;
                    }
                }
                pos = (pos + 1) & mask;
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
    let mut claims = Vec::with_capacity(parsed.len());
    let mut descriptors: Vec<Vec<_>> = (0..NS).map(|_| Vec::new()).collect();
    for (batch, (payload, cs, ds)) in parsed.into_iter().enumerate() {
        payloads.push(payload);
        claims.push(cs);
        for (bucket, d) in descriptors.iter_mut().zip(ds) {
            if !d.is_empty() {
                bucket.push((batch, d));
            }
        }
    }
    let removals: Vec<_> = descriptors
        .into_par_iter()
        .map(|parts| losers(parts, &claims))
        .collect();
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
    fn tag_collision_keeps_distinct_id_and_invalid_winner_kills_valid_loser() {
        // Force both a tag collision and an open-addressing collision.
        let claims = vec![
            vec![
                Claim {
                    id: 13,
                    ingest: 3,
                    target: 0,
                },
                Claim {
                    id: 14,
                    ingest: 4,
                    target: 1,
                },
            ],
            vec![Claim {
                id: 13,
                ingest: 1,
                target: NONE,
            }],
        ];
        let tag = 7u64 << 32;
        let lost = losers(vec![(0, vec![tag, tag | 1]), (1, vec![tag])], &claims);
        assert_eq!(lost.len(), 1);
        assert_eq!((lost[0].batch, lost[0].target), (0, 0));
    }

    #[test]
    fn colliding_tags_match_exact_minimum_for_all_processing_orders() {
        let mut claims = vec![Vec::new(); 4];
        for i in 0..400u32 {
            let batch = (i as usize * 7 + 1) % claims.len();
            claims[batch].push(Claim {
                id: (i % 83 + 1) as u64,
                ingest: i64::from((i * 173) % 401),
                target: if i % 11 == 0 { NONE } else { i },
            });
        }
        for reverse in [false, true] {
            let mut parts: Vec<_> = claims
                .iter()
                .enumerate()
                .map(|(batch, cs)| {
                    let mut descriptors: Vec<_> = (0..cs.len()).map(|i| (23u64 << 32) | i as u64).collect();
                    if reverse {
                        descriptors.reverse();
                    }
                    (batch, descriptors)
                })
                .collect();
            if reverse {
                parts.reverse();
            }
            let mut expected = Vec::new();
            let mut best = std::collections::HashMap::new();
            for (batch, cs) in claims.iter().enumerate() {
                for c in cs {
                    let loc = Location {
                        batch,
                        target: c.target,
                    };
                    match best.entry(c.id) {
                        std::collections::hash_map::Entry::Vacant(e) => {
                            e.insert((c.ingest, loc));
                        }
                        std::collections::hash_map::Entry::Occupied(mut e) => {
                            let lost = if c.ingest < e.get().0 {
                                e.insert((c.ingest, loc)).1
                            } else {
                                loc
                            };
                            if lost.target != NONE {
                                expected.push((lost.batch, lost.target));
                            }
                        }
                    }
                }
            }
            let mut actual: Vec<_> = losers(parts, &claims)
                .into_iter()
                .map(|loc| (loc.batch, loc.target))
                .collect();
            expected.sort_unstable();
            actual.sort_unstable();
            assert_eq!(actual, expected);
        }
    }
}
