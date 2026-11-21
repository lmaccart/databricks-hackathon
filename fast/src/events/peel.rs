//! Direct owner cells peel singletons without consulting ingest or payload.
//! The first collision forwards the *original* owner and the new claim. Each
//! later collision forwards only its new claim. This is not an open-addressed
//! lookup: different keys never displace each other into arbitrary cells.
use super::{Claim, KEY_MASK, Location, NONE};

const ORD_BITS: u32 = 19;
const ORD_MASK: u32 = (1 << ORD_BITS) - 1;
const REPEATED: u32 = 1 << 31; // zero ordinal, distinct from an empty cell
const TILES: usize = 32;

#[inline]
fn source(parts: &[(usize, Vec<Claim>)], starts: &[usize], ordinal: u32) -> (usize, Claim) {
    let ordinal = ordinal as usize;
    let part = starts.partition_point(|&start| start <= ordinal) - 1;
    (parts[part].0, parts[part].1[ordinal - starts[part]])
}

#[inline]
fn witness(c: Claim, ordinal: u32) -> u32 {
    // A 13-bit second index (starting above the largest possible primary
    // index) and an ordinal fit in one word. Never use either as an ID.
    (((c.key_and_target >> 20) as u32) << ORD_BITS) | (ordinal + 1)
}

fn resolve(
    rows: &mut [[u32; 3]],
    parts: &[(usize, Vec<Claim>)],
    starts: &[usize],
    spills: &[Vec<i64>],
    remove: &mut Vec<Location>,
) {
    // Unlike a fixed-fingerprint collision table, sort has bounded worst-case
    // work even for distinct keys sharing *all* routing and fingerprint bits.
    // The compact fallback stores the exact key and just a claim ordinal.
    // Ingest and target need not be loaded for distinct keys.
    rows.sort_unstable();
    let mut prev: Option<[u32; 3]> = None;
    let mut best = None;
    for &[hi, lo, ordinal] in &*rows {
        if let Some([old_hi, old_lo, old_ordinal]) = prev
            && hi == old_hi
            && lo == old_lo
        {
            let (at, mut loc) = evidence(parts, starts, spills, ordinal);
            let winner = best.get_or_insert_with(|| evidence(parts, starts, spills, old_ordinal));
            if at < winner.0 {
                loc = std::mem::replace(winner, (at, loc)).1;
            }
            if loc.target != NONE {
                remove.push(loc);
            }
        } else {
            best = None;
        }
        prev = Some([hi, lo, ordinal]);
    }
}

#[inline]
fn evidence(parts: &[(usize, Vec<Claim>)], starts: &[usize], spills: &[Vec<i64>], ordinal: u32) -> (i64, Location) {
    let (batch, c) = source(parts, starts, ordinal);
    (
        c.ingest(&spills[batch]),
        Location {
            batch,
            target: c.target(),
        },
    )
}

pub(super) fn losers(parts: Vec<(usize, Vec<Claim>)>, spills: &[Vec<i64>]) -> Vec<Location> {
    let n: usize = parts.iter().map(|(_, v)| v.len()).sum();
    if n == 0 {
        return Vec::new();
    }
    if n > ORD_MASK as usize {
        return super::losers(parts, spills);
    }
    let mut starts = Vec::with_capacity(parts.len());
    let mut owners = vec![0u32; (n * 2).next_power_of_two()];
    let mask = (owners.len() - 1) & KEY_MASK as usize;
    let mut tiles: [Vec<u32>; TILES] = std::array::from_fn(|_| Vec::new());
    let mut ordinal = 0u32;
    for (_, claims) in &parts {
        starts.push(ordinal as usize);
        for &c in claims {
            let index = c.key_and_target as usize & mask;
            let owner = &mut owners[index];
            let current = witness(c, ordinal);
            if *owner == 0 {
                *owner = current;
            } else {
                let tile = &mut tiles[index & (TILES - 1)];
                if *owner != REPEATED {
                    tile.push(*owner);
                    *owner = REPEATED;
                }
                tile.push(current);
            }
            ordinal += 1;
        }
    }
    // Release the wide table before working on one cache-sized tile at a time.
    drop(owners);
    let mut cells = Vec::new();
    let mut rows = Vec::new();
    let mut remove = Vec::new();
    for tile in tiles.into_iter().filter(|t| !t.is_empty()) {
        cells.clear();
        // Bound the cache footprint even when the partition is heavily skewed.
        cells.resize(tile.len().saturating_mul(4).min(8192).next_power_of_two(), 0u32);
        rows.clear();
        let mut forward = |r: u32| {
            let ordinal = (r & ORD_MASK) - 1;
            let key = source(&parts, &starts, ordinal).1.key();
            rows.push([(key >> 32) as u32, key as u32, ordinal]);
        };
        let mask = cells.len() - 1;
        for r in tile {
            let cell = &mut cells[(r >> ORD_BITS) as usize & mask];
            if *cell == 0 {
                *cell = r;
            } else {
                if *cell != REPEATED {
                    forward(*cell);
                    *cell = REPEATED;
                }
                forward(r);
            }
        }
        resolve(&mut rows, &parts, &starts, spills, &mut remove);
    }
    remove
}
