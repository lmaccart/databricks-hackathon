//! Shopping Sessions: parallel, exact, hash-partitioned Rust pipeline.
//!
//! Reads $DATA_DIR (default /data), writes $OUTPUT_DIR (default /output).
//! Every rule is implemented to match rules.md and reference/pipeline.py.
//! Phases (see ARCHITECTURE.md):
//!   0. small tables (manifest, products, campaigns)
//!   1. links  -> union-find -> id -> person name
//!   2. events -> payloads by person, claims by event ID
//!   3. reject losing payloads after exact minimum-ingest selection on suspect IDs
//!   4. clicks -> parse, validate and bucket directly by person (identical copies are idempotent)
//!   5. per person shard: sort, sessionize, credit, assists, aggregate
//!   6. write Parquet in parallel

use ahash::RandomState;
use memmap2::Mmap;
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Instant;
mod dims;
mod events;
mod links;
mod output;
mod person;
mod radix;
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

// Sampling only chooses a fast path; each actual ID is independently checked.
fn sample_radix(links: &[Chunk], events: &[Chunk], ws: i64, we: i64) -> Option<radix::Radix> {
    let mut counts = [0usize; 256];
    for (chunks, field) in [(links, 0), (events, 1)] {
        for c in chunks.iter().take(8) {
            let mut sample = *c;
            let n = sample.data.len().min(16 * 1024);
            sample.data = &sample.data[..n];
            for_rows(&sample, |r| {
                let b = r[field];
                if (7..=9).contains(&b.len())
                    && let Some(layout) = radix::Radix::new(b[0], ws, we)
                    && layout.bytes(b) != 0
                {
                    counts[b[0] as usize] += 1;
                }
            });
        }
    }
    let (prefix, n) = counts.iter().enumerate().max_by_key(|&(_, n)| n)?;
    if *n == 0 {
        None
    } else {
        radix::Radix::new(prefix as u8, ws, we)
    }
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
    compact: Vec<PCl>,
}

fn parse_clicks<'a>(
    c: &Chunk<'a>,
    sm: &Small,
    window: (i64, i64, &ManifestClock),
    names: &People<'a>,
    packed: bool,
) -> Vec<Clicks> {
    let (lo, we, clock) = window;
    let hint = reserve_per_shard(c.data);
    let mut b: Vec<_> = (0..NS)
        .map(|_| Clicks {
            wide: Vec::with_capacity(if packed { hint / 2 } else { hint }),
            packed: Vec::with_capacity(if packed { hint / 2 } else { 0 }),
            compact: Vec::with_capacity(if names.radix.is_some() { hint } else { 0 }),
        })
        .collect();
    #[derive(Clone, Copy, Default)]
    struct PendingClick {
        uid: u64,
        hash: u64,
        compact: u64,
        cid: u64,
        ts: i64,
        camp: u32,
    }
    let mut emit = |row: PendingClick| {
        let (dest, person) = match names.lookup(row.uid, row.hash, row.compact) {
            Identity::Compact(token) => (&mut b[token as u8 as usize].compact, token >> 8),
            Identity::Linked(p) if packed => (&mut b[shard(p)].packed, u64::from(p as u32)),
            Identity::Linked(p) => (&mut b[shard(p)].wide, names.original(p as u32)),
            Identity::Standalone(p) => (&mut b[shard(p)].wide, p),
        };
        dest.push(PCl {
            person,
            ts: row.ts,
            cid: row.cid,
            camp: row.camp,
        });
    };
    let mut ring = [PendingClick::default(); 16];
    let mut n = 0usize;
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
        names.prefetch(hash);
        let i = n % ring.len();
        if n >= ring.len() {
            emit(ring[i]);
        }
        ring[i] = PendingClick {
            uid: key,
            hash,
            compact: names.radix.map_or(0, |r| r.bytes(uid)),
            ts,
            cid: names.codec.key(cid),
            camp: ci,
        };
        n += 1;
    });
    for i in n.saturating_sub(ring.len())..n {
        emit(ring[i % ring.len()]);
    }
    b
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
    ShardOut { users, camps }: &mut ShardOut,
) {
    let np = pieces.iter().map(|s| s.purchases.len()).sum::<usize>();
    let ne = pieces.iter().map(|s| s.times.len()).sum::<usize>() + np;
    if ne == 0 {
        return;
    }
    let mut evs = Vec::with_capacity(ne);
    let mut purchases = Vec::with_capacity(np);
    for s in pieces {
        evs.extend_from_slice(&s.times);
        evs.extend(s.purchases.iter().map(|p| p.key));
        purchases.extend_from_slice(&s.purchases);
    }
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
        let person = T::name(rank, layout, people);
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
    shard: usize,
    evs: Vec<Payload>,
    cls: Vec<Clicks>,
    sm: &Small,
    people: &People,
    packing: Option<Packing>,
) -> ShardOut {
    let (packed, rest): (Vec<_>, Vec<_>) = evs.into_iter().map(|e| (e.packed, (e.wide, e.compact))).unzip();
    let (wide, compact): (Vec<_>, Vec<_>) = rest.into_iter().unzip();
    let (pc, rest): (Vec<_>, Vec<_>) = cls.into_iter().map(|c| (c.packed, (c.wide, c.compact))).unzip();
    let (wc, cc): (Vec<_>, Vec<_>) = rest.into_iter().unzip();
    let mut out = ShardOut {
        users: Vec::new(),
        camps: Map::with_hasher(rs()),
    };
    if let Some(pack) = packing {
        process_stream(packed, gather(pc), sm, people, pack, &mut out);
    }
    if let Some(r) = people.radix {
        process_stream(
            compact,
            gather(cc),
            sm,
            people,
            radix::Radix {
                shard: shard as u8,
                ..r
            },
            &mut out,
        );
    }
    process_stream(wide, gather(wc), sm, people, (), &mut out);
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
    let radix = sample_radix(&link_chunks, &ev_chunks, ws, we);
    let names = person_names(&link_chunks, radix);
    let packing = Packing::new(ws, we, names.count);
    log(&format!("links: {} linked ids", names.count));

    let ev_by_person = events::by_person(&ev_chunks, &sm, &names, (ws, we, &clock), packing, &log);
    log("events deduped");

    let cl_by_person = cl_chunks
        .par_iter()
        .map(|c| parse_clicks(c, &sm, (ws.saturating_sub(WEEK), we, &clock), &names, packing.is_some()))
        .collect();
    log("clicks routed");

    let len = |e: &Payload| {
        e.packed.times.len()
            + e.packed.purchases.len()
            + e.wide.times.len()
            + e.wide.purchases.len()
            + e.compact.times.len()
            + e.compact.purchases.len()
    };
    let mut jobs: Vec<_> = by_destination(ev_by_person, |e| len(e) > 0)
        .into_iter()
        .zip(by_destination(cl_by_person, |c| {
            !c.packed.is_empty() || !c.wide.is_empty() || !c.compact.is_empty()
        }))
        .enumerate()
        .collect();
    jobs.sort_unstable_by_key(|(_, (e, c))| {
        std::cmp::Reverse(
            e.iter().map(len).sum::<usize>()
                + c.iter()
                    .map(|x| x.packed.len() + x.wide.len() + x.compact.len())
                    .sum::<usize>(),
        )
    });
    let outs: Vec<_> = jobs
        .into_par_iter()
        .map(|(s, (e, c))| process_shard(s, e, c, &sm, &names, packing))
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
