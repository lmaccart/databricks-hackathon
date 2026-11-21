//! Resolve raw identities and scatter straight to owner bands. A band
//! contains both linked and singleton ranks; its tag is only six bits.
use super::{NS, PCl, People, Sessionizer, ShardOut, Small, by_destination, links};
use links::Lane;
use rayon::prelude::*;

pub(super) const BAND_BITS: u32 = 6;
pub(super) const BAND: usize = 1 << BAND_BITS;
const TAG_MASK: usize = BAND - 1;

#[derive(Clone, Copy)]
pub(super) struct Raw {
    pub person: u64,
    pub ts: i64,
}

#[derive(Clone, Copy)]
pub(super) struct Purchase<K> {
    pub key: K,
    pub price: u32,
    pub cat: u32,
}

pub(super) struct Stream<K> {
    pub times: Vec<K>,
    pub purchases: Vec<Purchase<K>>,
}

impl<K> Default for Stream<K> {
    fn default() -> Self {
        Self {
            times: Vec::new(),
            purchases: Vec::new(),
        }
    }
}

pub(super) trait Ticks: Copy + Default + Ord + Into<u64> + Send {
    fn from_offset(offset: u64) -> Self;
}

impl Ticks for u32 {
    #[inline]
    fn from_offset(offset: u64) -> Self {
        debug_assert!(u32::try_from(offset).is_ok());
        offset as u32
    }
}

impl Ticks for u64 {
    #[inline]
    fn from_offset(offset: u64) -> Self {
        offset
    }
}

pub(super) trait BandKey: Copy + Ord + Send {
    type Time: Ticks;
    fn encode(tag: usize, offset: u64) -> Self;
    fn tag(self) -> usize;
    fn time(self) -> Self::Time;
}

const SMALL_BITS: u32 = u32::BITS - BAND_BITS;
const WORD_BITS: u32 = u64::BITS - BAND_BITS;

impl BandKey for u32 {
    type Time = u32;

    #[inline]
    fn encode(tag: usize, offset: u64) -> Self {
        debug_assert!(tag < BAND && offset < 1 << SMALL_BITS);
        ((tag as u32) << SMALL_BITS) | offset as u32
    }

    #[inline]
    fn tag(self) -> usize {
        (self >> SMALL_BITS) as usize
    }

    #[inline]
    fn time(self) -> u32 {
        self & ((1 << SMALL_BITS) - 1)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Word<T>(u64, std::marker::PhantomData<T>);

impl<T: Ticks> BandKey for Word<T> {
    type Time = T;

    #[inline]
    fn encode(tag: usize, offset: u64) -> Self {
        debug_assert!(tag < BAND && offset < 1 << WORD_BITS);
        Self(((tag as u64) << WORD_BITS) | offset, std::marker::PhantomData)
    }

    #[inline]
    fn tag(self) -> usize {
        (self.0 >> WORD_BITS) as usize
    }

    #[inline]
    fn time(self) -> T {
        T::from_offset(self.0 & ((1 << WORD_BITS) - 1))
    }
}

// Even when a tag and timestamp cannot share a word, the histogram still
// separates people before the time sort. Unsigned offsets preserve signed
// chronological order across zero and cannot overflow a sorted gap.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Full {
    tag: usize,
    offset: u64,
}

impl BandKey for Full {
    type Time = u64;

    #[inline]
    fn encode(tag: usize, offset: u64) -> Self {
        Self { tag, offset }
    }

    #[inline]
    fn tag(self) -> usize {
        self.tag
    }

    #[inline]
    fn time(self) -> u64 {
        self.offset
    }
}

struct Band<K> {
    events: Stream<K>,
    clicks: Vec<PCl>, // the person field now holds the within-band tag
}

impl<K> Default for Band<K> {
    fn default() -> Self {
        Self {
            events: Stream::default(),
            clicks: Vec::new(),
        }
    }
}

// A direct-mapped hit is sufficient only after exact token comparison. The
// backing lane preserves the route across hot-cache evictions and across
// ordinary events, purchases, and clicks.
struct JoinCache {
    lane: Lane,
    linked_bands: usize,
    remote_bands: usize,
    names: Vec<u64>,
    hot: Vec<(u64, usize)>,
}

impl JoinCache {
    fn new(lane: Lane, names: Vec<u64>) -> Self {
        let linked_bands = names.len().div_ceil(BAND);
        Self {
            remote_bands: lane.routes.len() - linked_bands,
            lane,
            linked_bands,
            names,
            hot: vec![(0, 0); 4096],
        }
    }

    #[inline]
    fn resolve(&mut self, hash: u64) -> usize {
        let i = hash as usize & (self.hot.len() - 1);
        let (key, route) = self.hot[i];
        if key == hash {
            return route;
        }
        let route = self.lane.resolve(hash, || {
            let rank = self.names.len();
            self.names.push(links::unmix(hash));
            let band = rank >> BAND_BITS;
            // The first singleton can share the final linked band. Extra
            // bands follow this producer's precomputed remote route slots.
            let slot = if band < self.linked_bands {
                band
            } else {
                band + self.remote_bands
            };
            (slot << BAND_BITS) | (rank & TAG_MASK)
        });
        self.hot[i] = (hash, route);
        route
    }
}

#[inline]
fn band_mut<K>(bands: &mut Vec<Band<K>>, route: usize) -> &mut Band<K> {
    let i = route >> BAND_BITS;
    if i >= bands.len() {
        bands.resize_with(i + 1, Band::default);
    }
    &mut bands[i]
}

struct Owner<K> {
    names: Vec<u64>,
    bands: Vec<Vec<Band<K>>>,
    rows: usize,
}

fn join<K: BandKey>(
    raw_events: Vec<Vec<Stream<Raw>>>,
    raw_clicks: Vec<Vec<Vec<PCl>>>,
    people: &mut People,
    origin: i64,
) -> Vec<Owner<K>> {
    let joined: Vec<_> = raw_events
        .into_par_iter()
        .zip(raw_clicks)
        .zip(std::mem::take(&mut people.lanes))
        .zip(std::mem::take(&mut people.roots))
        .map(|(((evs, cls), lane), names)| {
            let mut cache = JoinCache::new(lane, names);
            let mut bands = Vec::new();
            for piece in evs {
                for row in piece.times {
                    let route = cache.resolve(row.person);
                    let key = K::encode(route & TAG_MASK, row.ts.wrapping_sub(origin) as u64);
                    band_mut(&mut bands, route).events.times.push(key);
                }
                for row in piece.purchases {
                    let route = cache.resolve(row.key.person);
                    let key = K::encode(route & TAG_MASK, row.key.ts.wrapping_sub(origin) as u64);
                    band_mut(&mut bands, route).events.purchases.push(Purchase {
                        key,
                        price: row.price,
                        cat: row.cat,
                    });
                }
            }
            for piece in cls {
                for mut row in piece {
                    let route = cache.resolve(row.person);
                    row.person = (route & TAG_MASK) as u64;
                    band_mut(&mut bands, route).clicks.push(row);
                }
            }
            (cache.names, cache.lane.routes, cache.remote_bands, bands)
        })
        .collect();

    let mut owners: Vec<_> = (0..NS)
        .map(|_| Owner {
            names: Vec::new(),
            bands: Vec::new(),
            rows: 0,
        })
        .collect();
    for (raw, (names, routes, remote_count, bands)) in joined.into_iter().enumerate() {
        owners[raw].names = names;
        for (slot, piece) in bands.into_iter().enumerate() {
            let rows = piece.events.times.len() + piece.events.purchases.len() + piece.clicks.len();
            if rows == 0 {
                continue;
            }
            let (dest, band) = routes.get(slot).copied().unwrap_or_else(|| (raw, slot - remote_count));
            let owner = &mut owners[dest];
            if band >= owner.bands.len() {
                owner.bands.resize_with(band + 1, Vec::new);
            }
            owner.bands[band].push(piece);
            owner.rows += rows;
        }
    }
    owners
}

fn process_owner<K: BandKey>(owner: Owner<K>, sm: &Small, people: &People, origin: i64) -> ShardOut {
    let mut sessionizer = Sessionizer::new(sm, &people.codec, origin);
    let mut times = Vec::<K::Time>::new();
    let mut purchases = Vec::new();
    let mut clicks = Vec::new();
    for (band, pieces) in owner.bands.into_iter().enumerate() {
        if pieces.is_empty() {
            continue;
        }
        let mut count = [0; BAND];
        for piece in &pieces {
            for &key in &piece.events.times {
                count[key.tag()] += 1;
            }
            for row in &piece.events.purchases {
                count[row.key.tag()] += 1;
            }
        }
        let mut offsets = [0; BAND + 1];
        for i in 0..BAND {
            offsets[i + 1] = offsets[i] + count[i];
        }
        if offsets[BAND] == 0 {
            continue;
        }
        if offsets[BAND] > times.len() {
            times.resize(offsets[BAND], K::Time::default());
        }
        purchases.clear();
        clicks.clear();
        let mut write = offsets;
        for piece in pieces {
            for key in piece.events.times {
                let at = &mut write[key.tag()];
                times[*at] = key.time();
                *at += 1;
            }
            for row in piece.events.purchases {
                let at = &mut write[row.key.tag()];
                times[*at] = row.key.time();
                *at += 1;
                purchases.push(row);
            }
            clicks.extend(piece.clicks);
        }
        debug_assert_eq!(&write[..BAND], &offsets[1..]);
        purchases.sort_unstable_by_key(|row| row.key);
        clicks.sort_unstable_by_key(|row| (row.person, row.ts));
        let (mut pi, mut ci) = (0, 0);
        for tag in 0..BAND {
            let evs = &mut times[offsets[tag]..offsets[tag + 1]];
            if evs.is_empty() {
                continue;
            }
            let p0 = pi;
            while pi < purchases.len() && purchases[pi].key.tag() == tag {
                pi += 1;
            }
            while ci < clicks.len() && clicks[ci].person < tag as u64 {
                ci += 1;
            }
            let c0 = ci;
            while ci < clicks.len() && clicks[ci].person == tag as u64 {
                ci += 1;
            }
            evs.sort_unstable();
            sessionizer.person::<K>(owner.names[band * BAND + tag], evs, &purchases[p0..pi], &clicks[c0..ci]);
        }
        debug_assert_eq!(pi, purchases.len());
    }
    sessionizer.out
}

fn run_key<K: BandKey>(
    events: Vec<Vec<Stream<Raw>>>,
    clicks: Vec<Vec<Vec<PCl>>>,
    sm: &Small,
    people: &mut People,
    origin: i64,
    log: &impl Fn(&str),
) -> Vec<ShardOut> {
    let raw_events = by_destination(events, |p| !p.times.is_empty() || !p.purchases.is_empty());
    let raw_clicks = by_destination(clicks, |p| !p.is_empty());
    let mut jobs = join::<K>(raw_events, raw_clicks, people, origin);
    log("identity join/bands");
    jobs.sort_unstable_by_key(|o| std::cmp::Reverse(o.rows));
    jobs.into_par_iter()
        .map(|o| process_owner(o, sm, people, origin))
        .collect()
}

pub(super) fn run(
    events: Vec<Vec<Stream<Raw>>>,
    clicks: Vec<Vec<Vec<PCl>>>,
    sm: &Small,
    people: &mut People,
    origin: i64,
    end: i64,
    log: &impl Fn(&str),
) -> Vec<ShardOut> {
    let span = (i128::from(end) - i128::from(origin) - 1).max(0) as u64;
    if span < 1 << SMALL_BITS {
        run_key::<u32>(events, clicks, sm, people, origin, log)
    } else if span <= u64::from(u32::MAX) {
        run_key::<Word<u32>>(events, clicks, sm, people, origin, log)
    } else if span < 1 << WORD_BITS {
        run_key::<Word<u64>>(events, clicks, sm, people, origin, log)
    } else {
        run_key::<Full>(events, clicks, sm, people, origin, log)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_keys_keep_tag_and_full_signed_time_range() {
        fn check<K: BandKey>(origin: i64, ts: i64) {
            let offset = ts.wrapping_sub(origin) as u64;
            let key = K::encode(BAND - 1, offset);
            assert_eq!(key.tag(), BAND - 1);
            assert_eq!(origin.wrapping_add(key.time().into() as i64), ts);
            assert!(key > K::encode(BAND - 2, offset));
            assert!(key > K::encode(BAND - 1, offset - 1));
        }
        check::<u32>(-1, (1 << SMALL_BITS) - 2);
        check::<Word<u32>>(-1, i64::from(u32::MAX) - 1);
        check::<Word<u64>>(-1, (1 << WORD_BITS) - 2);
        check::<Full>(i64::MIN + 1, i64::MAX - 1);
    }
}
