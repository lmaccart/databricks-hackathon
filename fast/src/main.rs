//! Shopping Sessions: parallel, exact, hash-partitioned Rust pipeline.
//!
//! Reads $DATA_DIR (default /data), writes $OUTPUT_DIR (default /output).
//! Every rule is implemented to match rules.md and reference/pipeline.py.
//! Phases (see ARCHITECTURE.md):
//!   0. small tables (manifest, products, campaigns)
//!   1. links  -> union-find -> id -> person name
//!   2. events -> raw-user payload partitions, claims by event ID
//!   3. reject losing payloads after exact minimum-ingest selection on suspect IDs
//!   4. clicks -> raw-user partitions (identical copies are idempotent)
//!   5. cache-local identity join, then per-owner sort, sessionize and aggregate
//!   6. write Parquet in parallel

use ahash::RandomState;
use memmap2::Mmap;
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering::Relaxed};
use std::time::Instant;
mod dims;
mod events;
mod links;
mod output;
mod person;
mod stream;
use links::{Identity, People, person_names};
use person::Codec;
use stream::{Packing, Payload, Stream, Temporal};

const DAY: i64 = 86_400_000;
const GAP: i64 = 1_800_000;
const WEEK: i64 = 7 * DAY;
const BAD: i64 = i64::MIN;
const MAX_PRICE: i64 = 10_000_000;
const NS: usize = 256; // shards for every hash partition
const SHARD_SHIFT: u32 = 40;
const CHUNK: usize = 8 << 20; // bytes per parse task
const ORGANIC: u32 = u32::MAX;

type Map<K, V> = HashMap<K, V, RandomState>;

fn rs() -> RandomState {
    RandomState::with_seeds(0x9E37_79B9, 0x85EB_CA6B, 0xC2B2_AE35, 0x27D4_EB2F)
}

// ------------------------------------------------------------------ CSV dialect, timestamps

mod csv;
mod ts;

use csv::{Chunk, chunks_of, for_rows};
use ts::{ManifestClock, parse_ts};

// ------------------------------------------------------------------ prices

#[inline]
fn digits(b: &[u8]) -> bool {
    !b.is_empty() && b.iter().all(|c| c.is_ascii_digit())
}

#[inline]
fn num(b: &[u8]) -> i64 {
    b.iter().fold(0i64, |a, &c| a * 10 + (c - b'0') as i64)
}

/// price_cents for a purchase: digits only, value 1..=10,000,000 (leading zeros allowed).
#[inline]
fn parse_price(p: &[u8]) -> Option<u32> {
    if !digits(p) {
        return None;
    }
    let t = &p[p.iter().position(|&c| c != b'0')?..];
    if t.len() > 8 {
        return None;
    }
    let v = num(t);
    if (1..=MAX_PRICE).contains(&v) {
        Some(v as u32)
    } else {
        None
    }
}

// ------------------------------------------------------------------ inputs

fn mmap_files(dir: &Path) -> Vec<(PathBuf, Mmap)> {
    let mut out = Vec::new();
    let rd = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    for e in rd {
        let p = e.expect("read directory entry").path();
        if p.extension().is_some_and(|x| x == "csv") {
            let meta = p.metadata().unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            if !meta.is_file() || meta.len() == 0 {
                continue;
            }
            let f = File::open(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            let m = unsafe { Mmap::map(&f).expect("mmap") };
            out.push((p, m));
        }
    }
    out
}

fn chunks_for<'a>(files: &'a [(PathBuf, Mmap)], cols: &[&str]) -> Vec<Chunk<'a>> {
    let mut v: Vec<Chunk> = files.iter().flat_map(|(p, m)| chunks_of(&m[..], cols, p)).collect();
    v.sort_by_key(|c| std::cmp::Reverse(c.data.len())); // big tasks first
    v
}

struct Small<'a> {
    product_cat: dims::Dimension<'a>,
    cat_names: Vec<Vec<u8>>, // index -> name; last one is "unknown"
    cat_rank: Vec<u32>,      // index -> byte-wise rank among all names
    unknown_cat: u32,
    camp_idx: dims::Dimension<'a>,
    camp_names: Vec<&'a [u8]>,
    camp_channel: Vec<&'a [u8]>,
}

fn read_small<'a>(prod: &'a [u8], camp: &'a [u8], pp: &Path, cp: &Path) -> Small<'a> {
    let mut cat_id: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut cat_names: Vec<Vec<u8>> = Vec::new();
    let mut product_cat = Map::with_hasher(rs());
    let mut first_product = None;
    for c in chunks_of(prod, &["product_id", "category"], pp) {
        for_rows(&c, |r| {
            first_product.get_or_insert(r[0]);
            let n = cat_names.len() as u32;
            let id = *cat_id.entry(r[1].to_vec()).or_insert_with(|| {
                cat_names.push(r[1].to_vec());
                n
            });
            product_cat.insert(r[0], id);
        });
    }
    let unknown_cat = cat_names.len() as u32;
    cat_names.push(b"unknown".to_vec());
    let mut order: Vec<u32> = (0..cat_names.len() as u32).collect();
    order.sort_by(|&a, &b| cat_names[a as usize].cmp(&cat_names[b as usize]));
    let mut cat_rank = vec![0u32; cat_names.len()];
    for (rank, &i) in order.iter().enumerate() {
        cat_rank[i as usize] = rank as u32;
    }
    let mut camp_idx = Map::with_hasher(rs());
    let mut first_campaign = None;
    let mut camp_names = Vec::new();
    let mut camp_channel = Vec::new();
    for c in chunks_of(camp, &["campaign_id", "channel"], cp) {
        for_rows(&c, |r| {
            first_campaign.get_or_insert(r[0]);
            camp_idx.insert(r[0], camp_names.len() as u32);
            camp_names.push(r[0]);
            camp_channel.push(r[1]);
        });
    }
    Small {
        product_cat: dims::Dimension::new(product_cat, first_product),
        cat_names,
        cat_rank,
        unknown_cat,
        camp_idx: dims::Dimension::new(camp_idx, first_campaign),
        camp_names,
        camp_channel,
    }
}

// ------------------------------------------------------------------ events & clicks

#[derive(Clone, Copy)]
struct PCl {
    person: u64,
    ts: i64,
    cid: u64,
    camp: u32,
}

#[inline]
fn shard(h: u64) -> usize {
    ((h >> SHARD_SHIFT) as usize) % NS
}

// A small live sample is only an allocation hint. Vectors still grow normally
// for skewed shards, short records, long records, or a non-representative prefix.
fn reserve_per_shard(data: &[u8]) -> usize {
    let n = data.len().min(16 * 1024);
    if n == 0 {
        return 0;
    }
    let lines = memchr::memchr_iter(b'\n', &data[..n]).count();
    lines.saturating_mul(data.len()) / n / NS
}

fn by_destination<T>(batches: Vec<Vec<T>>, nonempty: impl Fn(&T) -> bool) -> Vec<Vec<T>> {
    let mut jobs: Vec<Vec<_>> = (0..NS).map(|_| Vec::with_capacity(batches.len())).collect();
    for batch in batches {
        for (dest, rows) in jobs.iter_mut().zip(batch) {
            if nonempty(&rows) {
                dest.push(rows);
            }
        }
    }
    jobs
}

fn gather<T: Copy>(mut pieces: Vec<Vec<T>>) -> Vec<T> {
    if pieces.len() <= 1 {
        return pieces.pop().unwrap_or_default();
    }
    let size: usize = pieces.iter().map(Vec::len).sum();
    let mut rows = Vec::with_capacity(size);
    for piece in pieces {
        rows.extend_from_slice(&piece);
    }
    rows
}

#[derive(Default)]
struct Clicks {
    packed: Vec<PCl>,
    wide: Vec<PCl>,
}

fn parse_clicks<'a>(c: &Chunk<'a>, sm: &Small, window: (i64, i64, &ManifestClock), names: &People<'a>) -> Vec<Clicks> {
    let (lo, we, clock) = window;
    let hint = reserve_per_shard(c.data);
    let mut b: Vec<_> = (0..NS)
        .map(|_| Clicks {
            wide: Vec::with_capacity(hint),
            packed: Vec::new(),
        })
        .collect();
    for_rows(c, |r| {
        // click_id,user_id,campaign_id,ts. Copies are identical, so validating before dedup is equivalent.
        let (cid, uid, camp) = (r[0], r[1], r[2]);
        if cid.is_empty() || uid.is_empty() {
            return;
        }
        let Some(ci) = sm.camp_idx.get(camp) else { return };
        let ts = clock.parse_ts(r[3]);
        if ts == BAD || ts < lo || ts >= we {
            return;
        }
        let key = names.codec.key(uid);
        let hash = links::mix(key);
        b[shard(hash)].wide.push(PCl {
            person: hash,
            ts,
            cid: names.codec.key(cid),
            camp: ci,
        });
    });
    b
}

struct JoinJob {
    events: Vec<Payload>,
    clicks: Vec<Clicks>,
    packing: Option<Packing>,
    local_names: Option<Vec<u64>>,
}

struct LocalIndex {
    by_raw: Map<u64, u32>,
    names: Vec<u64>,
    linked: Vec<Option<u64>>,
}

// Unused link IDs still determine a component's name, but only active raw
// partitions need to participate in the owner census (rules.md, Linked IDs).
// 0 = unseen, 1..=NS = one active raw shard, NS+1 = shared.
fn observe(owner: &AtomicU16, raw: usize) {
    let value = raw as u16 + 1;
    let old = owner.load(Relaxed);
    if old != value && old != NS as u16 + 1 {
        let _ = owner.compare_exchange(0, value, Relaxed, Relaxed);
        if owner.load(Relaxed) != value {
            owner.store(NS as u16 + 1, Relaxed);
        }
    }
}

fn join_raw(
    events: Vec<Vec<Payload>>,
    clicks: Vec<Vec<Clicks>>,
    people: &People,
    packing: Option<Packing>,
    window: (i64, i64),
) -> Vec<JoinJob> {
    let raw_events = by_destination(events, |p| !p.wide.times.is_empty() || !p.wide.purchases.is_empty());
    let raw_clicks = by_destination(clicks, |p| !p.wide.is_empty());
    let owners: Vec<_> = (0..people.count).map(|_| AtomicU16::new(0)).collect();
    let indexed: Vec<_> = raw_events
        .into_par_iter()
        .zip(raw_clicks.into_par_iter())
        .enumerate()
        .map(|(raw, (evs, cls))| {
            let mut by_raw = Map::with_hasher(rs());
            let mut by_name = Map::with_hasher(rs());
            let mut names = Vec::new();
            let mut linked = Vec::new();
            let mut recent = vec![0u64; 4096];
            let mut add = |hash: u64| {
                let slot = hash as usize & 4095;
                if recent[slot] == hash {
                    return;
                }
                recent[slot] = hash;
                by_raw.entry(hash).or_insert_with(|| {
                    let id = people.lookup(links::unmix(hash), hash);
                    let (name, route) = match id {
                        Identity::Standalone(name) => (name, None),
                        Identity::Linked(route) => (people.original(route as u32), Some(route)),
                    };
                    *by_name.entry(name).or_insert_with(|| {
                        let rank = u32::try_from(names.len()).expect("too many local people");
                        names.push(name);
                        linked.push(route);
                        if let Some(route) = route {
                            observe(&owners[route as u32 as usize], raw);
                        }
                        rank
                    })
                });
            };
            for p in &evs {
                for e in &p.wide.times {
                    add(e.person);
                }
                for e in &p.wide.purchases {
                    add(e.key.person);
                }
            }
            for p in &cls {
                for c in &p.wide {
                    add(c.person);
                }
            }
            (raw, evs, cls, LocalIndex { by_raw, names, linked })
        })
        .collect();
    let joined: Vec<_> = indexed
        .into_par_iter()
        .map(|(raw, evs, cls, index)| {
            let local_pack = Packing::new(window.0, window.1, index.names.len());
            let shared: Vec<_> = index
                .linked
                .iter()
                .map(|route| route.filter(|&r| owners[r as u32 as usize].load(Relaxed) == NS as u16 + 1))
                .collect();
            let mut local_e = Payload::default();
            let mut local_c = Clicks::default();
            let mut moved_e: Vec<Payload> = (0..NS).map(|_| Payload::default()).collect();
            let mut moved_c: Vec<Clicks> = (0..NS).map(|_| Clicks::default()).collect();
            let mut cache_keys = vec![0u64; 4096];
            let mut cache_ranks = vec![0u32; 4096];
            let mut resolve = |hash: u64| {
                let slot = hash as usize & 4095;
                if cache_keys[slot] != hash {
                    cache_keys[slot] = hash;
                    cache_ranks[slot] = index.by_raw[&hash];
                }
                cache_ranks[slot]
            };
            let mut push_event = |person: u64, ts: i64, price: u32, cat: u32| {
                let rank = resolve(person);
                if let Some(route) = shared[rank as usize] {
                    moved_e[shard(route)].push(Identity::Linked(route), ts, price, cat, packing, people);
                } else {
                    local_e.push_local(rank, index.names[rank as usize], ts, price, cat, local_pack);
                }
            };
            for part in evs {
                for row in part.wide.times {
                    push_event(row.person, row.ts, 0, 0);
                }
                for row in part.wide.purchases {
                    push_event(row.key.person, row.key.ts, row.price, row.cat);
                }
            }
            for part in cls {
                for mut row in part.wide {
                    let rank = resolve(row.person);
                    if let Some(route) = shared[rank as usize] {
                        let dest = &mut moved_c[shard(route)];
                        if packing.is_some() {
                            row.person = u64::from(route as u32);
                            dest.packed.push(row);
                        } else {
                            row.person = people.original(route as u32);
                            dest.wide.push(row);
                        }
                    } else if local_pack.is_some() {
                        row.person = u64::from(rank);
                        local_c.packed.push(row);
                    } else {
                        row.person = index.names[rank as usize];
                        local_c.wide.push(row);
                    }
                }
            }
            (raw, local_e, local_c, local_pack, index.names, moved_e, moved_c)
        })
        .collect();
    let mut jobs: Vec<JoinJob> = Vec::with_capacity(NS + joined.len());
    jobs.extend((0..NS).map(|_| JoinJob {
        events: Vec::new(),
        clicks: Vec::new(),
        packing,
        local_names: None,
    }));
    for (_raw, local_e, local_c, local_pack, names, moved_e, moved_c) in joined {
        if !local_e.packed.times.is_empty()
            || !local_e.packed.purchases.is_empty()
            || !local_e.wide.times.is_empty()
            || !local_e.wide.purchases.is_empty()
            || !local_c.packed.is_empty()
            || !local_c.wide.is_empty()
        {
            jobs.push(JoinJob {
                events: vec![local_e],
                clicks: vec![local_c],
                packing: local_pack,
                local_names: Some(names),
            });
        }
        for (dest, (e, c)) in moved_e.into_iter().zip(moved_c).enumerate() {
            if !e.packed.times.is_empty()
                || !e.packed.purchases.is_empty()
                || !e.wide.times.is_empty()
                || !e.wide.purchases.is_empty()
            {
                jobs[dest].events.push(e);
            }
            if !c.packed.is_empty() || !c.wide.is_empty() {
                jobs[dest].clicks.push(c);
            }
        }
    }
    jobs
}

// ------------------------------------------------------------------ sessions

struct UserRow {
    person: u64,
    day: i32,
    sessions: i64,
    events: i64,
    revenue: i64,
    top: Option<u32>,
}

struct ShardOut {
    users: Vec<UserRow>,
    camps: Map<u64, [i64; 4]>, // (camp << 32 | day) -> sessions, purchasing, revenue, assisted
}

#[inline]
fn ckey(camp: u32, day: i32) -> u64 {
    ((camp as u64) << 32) | (day as u32 as u64)
}

fn top_category(catsum: &[i64], rank: &[u32]) -> Option<u32> {
    let mut best: Option<usize> = None;
    for c in 0..catsum.len() {
        if catsum[c] == 0 {
            continue;
        }
        best = match best {
            None => Some(c),
            Some(b) if catsum[c] > catsum[b] || (catsum[c] == catsum[b] && rank[c] < rank[b]) => Some(c),
            keep => keep,
        };
    }
    best.map(|b| b as u32)
}

fn process_stream<T: Temporal>(
    pieces: Vec<Stream<T>>,
    mut cls: Vec<PCl>,
    sm: &Small,
    people: &People,
    layout: T::Layout,
    local_names: Option<&[u64]>,
    ShardOut { users, camps }: &mut ShardOut,
) {
    let np = pieces.iter().map(|s| s.purchases.len()).sum::<usize>();
    let ne = pieces.iter().map(|s| s.times.len()).sum::<usize>() + np;
    if ne == 0 {
        return;
    }
    let (mut evs, mut purchases) = if pieces.len() == 1 {
        let s = pieces.into_iter().next().unwrap();
        (s.times, s.purchases)
    } else {
        let mut evs = Vec::with_capacity(ne);
        let mut purchases = Vec::with_capacity(np);
        for s in pieces {
            evs.extend_from_slice(&s.times);
            purchases.extend_from_slice(&s.purchases);
        }
        (evs, purchases)
    };
    evs.extend(purchases.iter().map(|p| p.key));
    evs.sort_unstable();
    purchases.sort_unstable_by_key(|p| p.key);
    cls.sort_unstable_by_key(|c| (c.person, c.ts));
    let ncamp = sm.camp_names.len();
    let mut cnt = vec![0usize; ncamp]; // copies enter and leave the window together
    let mut active: Vec<u32> = Vec::new(); // campaigns with cnt > 0
    let mut pos = vec![0u32; ncamp]; // index of a campaign inside `active`
    let mut catsum = vec![0i64; sm.cat_names.len()];

    let mut ci = 0usize;
    let mut i = 0usize;
    let mut pi = 0usize;
    while i < evs.len() {
        let rank = evs[i].person(layout);
        let person = T::name(rank, people, local_names);
        let mut j = i;
        while j < evs.len() && evs[j].person(layout) == rank {
            j += 1;
        }
        while ci < cls.len() && cls[ci].person < rank {
            ci += 1;
        }
        let c0 = ci;
        while ci < cls.len() && cls[ci].person == rank {
            ci += 1;
        }
        let clicks = &cls[c0..ci];
        let (mut lo, mut hi) = (0usize, 0usize);
        let mut latest = 0;

        let mut cur_day: i32 = i32::MIN;
        let (mut d_sess, mut d_ev, mut d_rev) = (0i64, 0i64, 0i64);

        let mut k = i;
        while k < j {
            // one session is evs[k..m]
            let start = evs[k].ts(layout);
            let mut m = k + 1;
            while m < j && evs[m].gap(evs[m - 1]) <= GAP as u64 {
                m += 1;
            }
            let day = start.div_euclid(DAY) as i32;
            if day != cur_day {
                if cur_day != i32::MIN {
                    let top = top_category(&catsum, &sm.cat_rank);
                    users.push(UserRow {
                        person,
                        day: cur_day,
                        sessions: d_sess,
                        events: d_ev,
                        revenue: d_rev,
                        top,
                    });
                    catsum.iter_mut().for_each(|x| *x = 0);
                }
                cur_day = day;
                (d_sess, d_ev, d_rev) = (0, 0, 0);
            }
            let mut rev = 0i64;
            // Each retained purchase contributed one immutable temporal key
            // before the sort. Sessions end on inclusive, disjoint key ranges:
            // consume ALL purchases at the last instant, including exact ties.
            let p0 = pi;
            let end = evs[m - 1];
            debug_assert!(purchases.get(pi).is_none_or(|p| p.key >= evs[k]));
            while pi < np && purchases[pi].key <= end {
                let e = &purchases[pi];
                rev += e.price as i64;
                catsum[e.cat as usize] += e.price as i64;
                pi += 1;
            }
            d_sess += 1;
            d_ev += (m - k) as i64;
            d_rev += rev;

            // slide the click window to [start - 7d, start]
            while hi < clicks.len() && clicks[hi].ts <= start {
                if hi == 0
                    || clicks[hi].ts != clicks[latest].ts
                    || people.codec.less(clicks[hi].cid, clicks[latest].cid)
                {
                    latest = hi;
                }
                let c = clicks[hi].camp as usize;
                if cnt[c] == 0 {
                    pos[c] = active.len() as u32;
                    active.push(c as u32);
                }
                cnt[c] += 1;
                hi += 1;
            }
            while lo < hi && clicks[lo].ts < start.saturating_sub(WEEK) {
                let c = clicks[lo].camp as usize;
                cnt[c] -= 1;
                if cnt[c] == 0 {
                    let p = pos[c] as usize;
                    let last = active.pop().unwrap();
                    if last as usize != c {
                        active[p] = last;
                        pos[last as usize] = p as u32;
                    }
                }
                lo += 1;
            }
            // The entire latest timestamp run expires together. Identical
            // copies change neither the tie winner nor campaign membership.
            let credit = if lo < hi { clicks[latest].camp } else { ORGANIC };
            let e = camps.entry(ckey(credit, day)).or_insert([0; 4]);
            e[0] += 1;
            e[1] += (pi != p0) as i64;
            e[2] += rev;
            for &a in &active {
                camps.entry(ckey(a, day)).or_insert([0; 4])[3] += 1;
            }
            k = m;
        }
        let top = top_category(&catsum, &sm.cat_rank);
        users.push(UserRow {
            person,
            day: cur_day,
            sessions: d_sess,
            events: d_ev,
            revenue: d_rev,
            top,
        });
        catsum.iter_mut().for_each(|x| *x = 0);
        for &a in &active {
            cnt[a as usize] = 0;
        }
        active.clear();
        i = j;
    }
    debug_assert_eq!(pi, np);
}

fn process_shard(
    evs: Vec<Payload>,
    cls: Vec<Clicks>,
    sm: &Small,
    people: &People,
    packing: Option<Packing>,
    local_names: Option<&[u64]>,
) -> ShardOut {
    let (packed, wide): (Vec<_>, Vec<_>) = evs.into_iter().map(|e| (e.packed, e.wide)).unzip();
    let (pc, wc): (Vec<_>, Vec<_>) = cls.into_iter().map(|c| (c.packed, c.wide)).unzip();
    let mut out = ShardOut {
        users: Vec::new(),
        camps: Map::with_hasher(rs()),
    };
    if let Some(pack) = packing {
        process_stream(packed, gather(pc), sm, people, pack, local_names, &mut out);
    }
    process_stream(wide, gather(wc), sm, people, (), None, &mut out);
    out
}

// ------------------------------------------------------------------ output

fn main() {
    let t0 = Instant::now();
    // Some runtimes expose host CPU count despite the competition's quota.
    // Respect an explicit Rayon setting for reproducible local comparisons.
    if std::env::var_os("RAYON_NUM_THREADS").is_none()
        && let Some(n) = std::env::var("NCPU")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&n| n > 0)
    {
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()
            .expect("Rayon pool");
    }
    let data = PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into()));
    let outd = PathBuf::from(std::env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into()));
    let log = |m: &str| eprintln!("[{:>7.3}s] {m}", t0.elapsed().as_secs_f64());

    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(data.join("manifest.json")).expect("manifest")).expect("json");
    let ws = parse_ts(manifest["window_start"].as_str().unwrap().as_bytes());
    let we = parse_ts(manifest["window_end"].as_str().unwrap().as_bytes());
    assert!(ws != BAD && we != BAD);
    let clock = ManifestClock::new(ws, we);

    let pf = File::open(data.join("products.csv")).expect("products.csv");
    let cf = File::open(data.join("campaigns.csv")).expect("campaigns.csv");
    let pm = unsafe { Mmap::map(&pf).unwrap() };
    let cm = unsafe { Mmap::map(&cf).unwrap() };
    let sm = read_small(&pm, &cm, &data.join("products.csv"), &data.join("campaigns.csv"));

    let link_files = mmap_files(&data.join("links"));
    let event_files = mmap_files(&data.join("events"));
    let click_files = mmap_files(&data.join("clicks"));
    let link_chunks = chunks_for(&link_files, &["user_id_a", "user_id_b"]);
    let ev_chunks = chunks_for(
        &event_files,
        &[
            "event_id",
            "user_id",
            "ts",
            "ingest_ts",
            "event_type",
            "product_id",
            "price_cents",
        ],
    );
    let cl_chunks = chunks_for(&click_files, &["click_id", "user_id", "campaign_id", "ts"]);
    let names = person_names(&link_chunks);
    let packing = Packing::new(ws, we, names.count);
    log(&format!("links: {} linked ids", names.count));

    let ev_by_person = events::by_person(&ev_chunks, &sm, &names, (ws, we, &clock), &log);
    log("events deduped");

    let cl_by_person = cl_chunks
        .par_iter()
        .map(|c| parse_clicks(c, &sm, (ws.saturating_sub(WEEK), we, &clock), &names))
        .collect();
    log("clicks parsed");

    let len =
        |e: &Payload| e.packed.times.len() + e.packed.purchases.len() + e.wide.times.len() + e.wide.purchases.len();
    let mut jobs = join_raw(ev_by_person, cl_by_person, &names, packing, (ws, we));
    log("identity join");
    jobs.sort_unstable_by_key(|job| {
        std::cmp::Reverse(
            job.events.iter().map(len).sum::<usize>()
                + job.clicks.iter().map(|x| x.packed.len() + x.wide.len()).sum::<usize>(),
        )
    });
    let outs: Vec<_> = jobs
        .into_par_iter()
        .map(|job| {
            process_shard(
                job.events,
                job.clicks,
                &sm,
                &names,
                job.packing,
                job.local_names.as_deref(),
            )
        })
        .collect();
    log("sessions done");

    let mut camps: Map<u64, [i64; 4]> = Map::with_hasher(rs());
    for o in &outs {
        for (k, v) in &o.camps {
            let e = camps.entry(*k).or_insert([0; 4]);
            for q in 0..4 {
                e[q] += v[q];
            }
        }
    }
    let ud = outd.join("user_daily");
    let cd = outd.join("campaign_daily");
    std::fs::create_dir_all(&ud).expect("mkdir user_daily");
    std::fs::create_dir_all(&cd).expect("mkdir campaign_daily");

    let camp_rows: Vec<output::CampRow> = camps
        .iter()
        .map(|(k, v)| {
            let c = (*k >> 32) as u32;
            let (campaign, channel) = if c == ORGANIC {
                (&b"organic"[..], &b"organic"[..])
            } else {
                (sm.camp_names[c as usize], sm.camp_channel[c as usize])
            };
            output::CampRow {
                campaign,
                channel,
                day: *k as u32 as i32,
                counts: *v,
            }
        })
        .collect();
    let shards: Vec<&[UserRow]> = outs.iter().map(|o| &o.users[..]).collect();
    let parts = rayon::current_num_threads().clamp(1, 16);
    rayon::join(
        || output::campaign_daily(&cd.join("part-0.parquet"), &camp_rows),
        || {
            output::user_daily(
                &ud,
                &shards,
                parts,
                &|p, out| out.extend_from_slice(names.codec.bytes(links::unmix(p), &mut [0; 9])),
                &sm.cat_names,
            )
        },
    );
    log("written");
    // Everything is on disk; skip tearing down the shard tables and mmaps.
    std::process::exit(0);
}

#[cfg(test)]
mod join_tests {
    use super::*;

    #[test]
    fn active_shards_not_unused_graph_aliases_determine_ownership() {
        let ids: Vec<String> = (0..2000).map(|n| format!("u{n}")).collect();
        let raw_shard = |s: &str| shard(links::mix(person::inline(s.as_bytes()).unwrap()));
        let a = &ids[0];
        let b = ids.iter().find(|s| *s != a && raw_shard(s) == raw_shard(a)).unwrap();
        let c = ids.iter().find(|s| raw_shard(s) != raw_shard(a)).unwrap();
        let d = ids
            .iter()
            .find(|s| *s != a && *s != b && raw_shard(s) != raw_shard(c))
            .unwrap();
        // The unused spill name may hash to any shard but still names the local component.
        let data = format!("user_id_a,user_id_b\n!unused-local-name,{a}\n{a},{b}\n!unused-shared-name,{c}\n{c},{d}\n");
        let chunks = chunks_of(data.as_bytes(), &["user_id_a", "user_id_b"], Path::new("links.csv"));
        let people = person_names(&chunks);
        let ws = 1_000_000;
        let we = ws + DAY;
        let mut batch: Vec<Payload> = (0..NS).map(|_| Payload::default()).collect();
        for id in [a, b, c, d] {
            let h = links::mix(people.codec.key(id.as_bytes()));
            batch[shard(h)].push(Identity::Standalone(h), ws, 0, 0, None, &people);
        }
        let jobs = join_raw(
            vec![batch],
            vec![],
            &people,
            Packing::new(ws, we, people.count),
            (ws, we),
        );
        let local: Vec<_> = jobs.iter().filter(|j| j.local_names.is_some()).collect();
        let named = links::mix(people.codec.key(b"!unused-local-name"));
        let local_job = local
            .iter()
            .find(|j| j.local_names.as_ref().unwrap().contains(&named))
            .unwrap();
        assert_eq!(local_job.events[0].packed.times.len(), 2);
        let shared: usize = jobs
            .iter()
            .filter(|j| j.local_names.is_none())
            .flat_map(|j| &j.events)
            .map(|p| p.packed.times.len())
            .sum();
        assert_eq!(shared, 2);
    }
}
