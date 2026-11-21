//! Shopping Sessions fast pipeline (v1): parallel, hash-partitioned, Rust.
//!
//! Reads $DATA_DIR (default /data), writes $OUTPUT_DIR (default /output).
//! Every rule is implemented to match rules.md and reference/pipeline.py.
//! Phases (see ARCHITECTURE.md):
//!   0. small tables (manifest, products, campaigns)
//!   1. links  -> union-find -> id -> person name
//!   2. events -> parse in parallel chunks, bucket by hash(event_id)
//!   3. per event shard: dedup (min ingest_ts) -> keep if valid -> bucket by hash(person)
//!   4. clicks -> parse, validate, bucket by hash(click_id) -> dedup -> bucket by hash(person)
//!   5. per person shard: sort, sessionize, credit, assists, aggregate
//!   6. write Parquet in parallel

use ahash::RandomState;
use arrow_array::{ArrayRef, Date32Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use memchr::memchr;
use memmap2::Mmap;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
mod person;
use person::Codec;

const DAY: i64 = 86_400_000;
const GAP: i64 = 1_800_000;
const WEEK: i64 = 7 * DAY;
const BAD: i64 = i64::MIN;
const MAX_PRICE: i64 = 10_000_000;
const NS: usize = 256; // shards for every hash partition
const CHUNK: usize = 8 << 20; // bytes per parse task
const ORGANIC: u32 = u32::MAX;

const T_VIEW: u8 = 1;
const T_CART: u8 = 2;
const T_PURCHASE: u8 = 3;

type Map<K, V> = HashMap<K, V, RandomState>;

fn rs() -> RandomState {
    RandomState::with_seeds(0x9E37_79B9, 0x85EB_CA6B, 0xC2B2_AE35, 0x27D4_EB2F)
}

// ------------------------------------------------------------------ CSV dialect

#[inline]
fn unquote(f: &[u8]) -> Option<&[u8]> {
    if memchr(b'"', f).is_none() {
        return Some(f);
    }
    let n = f.len();
    if n >= 2 && f[0] == b'"' && f[n - 1] == b'"' && memchr(b'"', &f[1..n - 1]).is_none() {
        Some(&f[1..n - 1])
    } else {
        None
    }
}

/// Split a line into at most 8 unquoted fields. None = malformed (empty line,
/// bad quoting, or more than 8 fields). The caller checks the field count.
#[inline]
fn split_fields<'a>(line: &'a [u8], out: &mut [&'a [u8]; 8]) -> Option<usize> {
    if line.is_empty() {
        return None;
    }
    let mut n = 0;
    let mut start = 0;
    loop {
        let end = match memchr(b',', &line[start..]) {
            Some(p) => start + p,
            None => line.len(),
        };
        if n == 8 {
            return None;
        }
        out[n] = unquote(&line[start..end])?;
        n += 1;
        if end == line.len() {
            return Some(n);
        }
        start = end + 1;
    }
}

#[inline]
fn for_lines<'a>(data: &'a [u8], mut f: impl FnMut(&'a [u8])) {
    let mut start = 0;
    while start < data.len() {
        match memchr(b'\n', &data[start..]) {
            Some(p) => {
                f(&data[start..start + p]);
                start += p + 1;
            }
            None => {
                f(&data[start..]);
                break;
            }
        }
    }
}

/// A slice of one CSV file's body (whole lines only) plus that file's column map.
#[derive(Clone, Copy)]
struct Chunk<'a> {
    data: &'a [u8],
    nfields: usize,
    idx: [usize; 8],
}

fn chunks_of<'a>(file: &'a [u8], cols: &[&str], path: &Path) -> Vec<Chunk<'a>> {
    let mut out = Vec::new();
    if file.is_empty() {
        return out;
    }
    let hdr_end = memchr(b'\n', file).unwrap_or(file.len());
    let header: Vec<&[u8]> = file[..hdr_end]
        .split(|&c| c == b',')
        .map(|h| unquote(h).unwrap_or(h))
        .collect();
    let mut idx = [0usize; 8];
    for (k, c) in cols.iter().enumerate() {
        idx[k] = header
            .iter()
            .position(|h| *h == c.as_bytes())
            .unwrap_or_else(|| panic!("{}: header lacks column {c}", path.display()));
    }
    let body = if hdr_end < file.len() { &file[hdr_end + 1..] } else { &file[file.len()..] };
    let mut start = 0;
    while start < body.len() {
        let mut end = (start + CHUNK).min(body.len());
        if end < body.len() {
            end = match memchr(b'\n', &body[end..]) {
                Some(p) => end + p + 1,
                None => body.len(),
            };
        }
        out.push(Chunk { data: &body[start..end], nfields: header.len(), idx });
        start = end;
    }
    out
}

/// Call f with the selected columns of every well-formed line of a chunk.
#[inline]
fn for_rows<'a>(c: &Chunk<'a>, mut f: impl FnMut(&[&'a [u8]; 8])) {
    let mut raw: [&[u8]; 8] = [&[]; 8];
    let mut sel: [&[u8]; 8] = [&[]; 8];
    for_lines(c.data, |line| {
        if let Some(n) = split_fields(line, &mut raw) {
            if n == c.nfields {
                for k in 0..8 {
                    sel[k] = raw[c.idx[k].min(7)];
                }
                f(&sel);
            }
        }
    });
}

// ------------------------------------------------------------------ timestamps

#[inline]
fn digits(b: &[u8]) -> bool {
    !b.is_empty() && b.iter().all(|c| c.is_ascii_digit())
}

#[inline]
fn num(b: &[u8]) -> i64 {
    b.iter().fold(0i64, |a, &c| a * 10 + (c - b'0') as i64)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// Epoch ms, or BAD. Mirrors reference/pipeline.py::parse_ts.
fn parse_ts(s: &[u8]) -> i64 {
    if s.is_empty() {
        return BAD;
    }
    if digits(s) {
        // F3: any length; a value too big for i64 is simply outside every window
        let mut v: i64 = 0;
        for &c in s {
            v = match v.checked_mul(10).and_then(|x| x.checked_add((c - b'0') as i64)) {
                Some(x) => x,
                None => return BAD,
            };
        }
        return v;
    }
    if s.len() != 20 && s.len() != 25 {
        return BAD;
    }
    if s[4] != b'-' || s[7] != b'-' || s[10] != b'T' || s[13] != b':' || s[16] != b':' {
        return BAD;
    }
    let parts = [&s[0..4], &s[5..7], &s[8..10], &s[11..13], &s[14..16], &s[17..19]];
    if !parts.iter().all(|p| digits(p)) {
        return BAD;
    }
    let (y, mo, d, h, mi, se) =
        (num(parts[0]), num(parts[1]), num(parts[2]), num(parts[3]), num(parts[4]), num(parts[5]));
    if !(1..=12).contains(&mo) || h > 23 || mi > 59 || se > 59 {
        return BAD;
    }
    const MD: [i64; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mdays = if mo == 2 && is_leap(y) { 29 } else { MD[mo as usize] };
    if d < 1 || d > mdays {
        return BAD;
    }
    let tail = &s[19..];
    let off_min = if tail == b"Z" {
        0
    } else if tail.len() == 6 && (tail[0] == b'+' || tail[0] == b'-') && tail[3] == b':' {
        let (oh, om) = (&tail[1..3], &tail[4..6]);
        if !digits(oh) || !digits(om) {
            return BAD;
        }
        let (oh, om) = (num(oh), num(om));
        if oh > 23 || om > 59 {
            return BAD;
        }
        let o = oh * 60 + om;
        if tail[0] == b'-' { -o } else { o }
    } else {
        return BAD;
    };
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se;
    (secs - off_min * 60) * 1000
}

/// price_cents for a purchase: digits only, value 1..=10,000,000 (leading zeros allowed).
#[inline]
fn parse_price(p: &[u8]) -> Option<u32> {
    if !digits(p) {
        return None;
    }
    let t = match p.iter().position(|&c| c != b'0') {
        Some(i) => &p[i..],
        None => return None, // all zeros
    };
    if t.len() > 8 {
        return None;
    }
    let v = num(t);
    if (1..=MAX_PRICE).contains(&v) { Some(v as u32) } else { None }
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
    product_cat: Map<&'a [u8], u32>,
    cat_names: Vec<Vec<u8>>, // index -> name; last one is "unknown"
    cat_rank: Vec<u32>,      // index -> byte-wise rank among all names
    unknown_cat: u32,
    camp_idx: Map<&'a [u8], u32>,
    camp_names: Vec<&'a [u8]>,
    camp_channel: Vec<&'a [u8]>,
}

fn read_small<'a>(prod: &'a [u8], camp: &'a [u8], pp: &Path, cp: &Path) -> Small<'a> {
    let mut cat_id: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut cat_names: Vec<Vec<u8>> = Vec::new();
    let mut product_cat = Map::with_hasher(rs());
    for c in chunks_of(prod, &["product_id", "category"], pp) {
        for_rows(&c, |r| {
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
    let mut camp_names = Vec::new();
    let mut camp_channel = Vec::new();
    for c in chunks_of(camp, &["campaign_id", "channel"], cp) {
        for_rows(&c, |r| {
            camp_idx.insert(r[0], camp_names.len() as u32);
            camp_names.push(r[0]);
            camp_channel.push(r[1]);
        });
    }
    Small { product_cat, cat_names, cat_rank, unknown_cat, camp_idx, camp_names, camp_channel }
}

// ------------------------------------------------------------------ links

fn find(parent: &mut [u32], mut x: u32) -> u32 {
    while parent[x as usize] != x {
        let p = parent[x as usize];
        parent[x as usize] = parent[p as usize]; // path halving
        x = parent[x as usize];
    }
    x
}

/// Map every linked ID to its person's name (the byte-wise smallest ID of the
/// person). IDs missing from the map are their own person.
struct People<'a> {
    codec: Codec<'a>,
    aliases: Map<u64, u64>,
}

impl<'a> People<'a> {
    #[inline]
    fn resolve(&self, uid: &'a [u8]) -> u64 {
        let key = self.codec.key(uid);
        self.aliases.get(&key).copied().unwrap_or(key)
    }
}

fn person_names<'a>(chunks: &[Chunk<'a>]) -> People<'a> {
    let pairs: Vec<Vec<(&'a [u8], &'a [u8])>> = chunks
        .par_iter()
        .map(|c| {
            let mut v = Vec::new();
            for_rows(c, |r| {
                if !r[0].is_empty() && !r[1].is_empty() {
                    v.push((r[0], r[1]));
                }
            });
            v
        })
        .collect();
    let codec = Codec::new();
    let mut ids: Map<u64, u32> = Map::with_hasher(rs());
    let mut names: Vec<&'a [u8]> = Vec::new();
    let mut parent: Vec<u32> = Vec::new();
    for v in pairs {
        for (a, b) in v {
            let mut ix = [0u32; 2];
            for (k, s) in [a, b].into_iter().enumerate() {
                ix[k] = *ids.entry(codec.key(s)).or_insert_with(|| {
                    names.push(s);
                    parent.push(parent.len() as u32);
                    (names.len() - 1) as u32
                });
            }
            let (ra, rb) = (find(&mut parent, ix[0]), find(&mut parent, ix[1]));
            if ra != rb {
                // keep the byte-wise smallest name as root, so a root names its person
                if names[rb as usize] < names[ra as usize] {
                    parent[ra as usize] = rb;
                } else {
                    parent[rb as usize] = ra;
                }
            }
        }
    }
    let mut aliases = Map::with_capacity_and_hasher(names.len(), rs());
    for (key, i) in ids {
        let r = find(&mut parent, i);
        if r != i {
            aliases.insert(key, codec.key(names[r as usize]));
        }
    }
    People { codec, aliases }
}

// ------------------------------------------------------------------ events & clicks

#[derive(Clone, Copy)]
struct EvRow<'a> {
    eid: &'a [u8],
    uid: &'a [u8],
    ts: i64,
    ingest: i64,
    price: u32,
    cat: u32,
    etype: u8,
    ok: bool,
}

#[derive(Clone, Copy)]
struct PEv {
    person: u64,
    ts: i64,
    price: u32,
    cat: u32,
}

#[derive(Clone, Copy)]
struct PCl<'a> {
    person: u64,
    ts: i64,
    cid: &'a [u8],
    camp: u32,
}

type ClRow<'a> = (&'a [u8], &'a [u8], i64, u32); // click_id, user_id, ts, campaign index

#[inline]
fn shard(h: u64) -> usize {
    ((h >> 40) as usize) % NS
}

fn parse_events<'a>(c: &Chunk<'a>, sm: &Small, ws: i64, we: i64, hs: &RandomState) -> Vec<Vec<EvRow<'a>>> {
    let mut b: Vec<Vec<EvRow>> = (0..NS).map(|_| Vec::new()).collect();
    for_rows(c, |r| {
        // event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents
        let eid = r[0];
        if eid.is_empty() {
            return;
        }
        let ingest = match parse_ts(r[3]) {
            BAD => i64::MAX, // never happens per rules.md; such a copy can only lose or be late
            t => t,
        };
        let uid = r[1];
        let ts = parse_ts(r[2]);
        let etype = match r[4] {
            b"view" => T_VIEW,
            b"cart" => T_CART,
            b"purchase" => T_PURCHASE,
            _ => 0,
        };
        let pid = r[5];
        // validity of this copy; it only matters if this copy wins the dedup
        let mut ok = !uid.is_empty() && ts != BAD && ts >= ws && ts < we && etype != 0 && !pid.is_empty();
        let mut price = 0u32;
        if ok && etype == T_PURCHASE {
            match parse_price(r[6]) {
                Some(p) => price = p,
                None => ok = false,
            }
        }
        if ok && ingest.saturating_sub(ts) > DAY {
            ok = false; // late
        }
        let cat = if ok { *sm.product_cat.get(pid).unwrap_or(&sm.unknown_cat) } else { 0 };
        b[shard(hs.hash_one(eid))].push(EvRow { eid, uid, ts, ingest, price, cat, etype, ok });
    });
    b
}

fn dedup_events<'a>(
    parsed: &[Vec<Vec<EvRow<'a>>>],
    s: usize,
    names: &People<'a>,
    hs: &RandomState,
) -> Vec<Vec<PEv>> {
    let n: usize = parsed.iter().map(|p| p[s].len()).sum();
    let mut best: Map<&'a [u8], EvRow<'a>> = Map::with_capacity_and_hasher(n, rs());
    for p in parsed {
        for r in &p[s] {
            best.entry(r.eid)
                .and_modify(|cur| {
                    if r.ingest < cur.ingest {
                        *cur = *r;
                    }
                })
                .or_insert(*r);
        }
    }
    let mut out: Vec<Vec<PEv>> = (0..NS).map(|_| Vec::new()).collect();
    for r in best.values() {
        if !r.ok {
            continue;
        }
        let person = names.resolve(r.uid);
        out[shard(hs.hash_one(person))].push(PEv { person, ts: r.ts, price: r.price, cat: r.cat });
    }
    out
}

fn parse_clicks<'a>(c: &Chunk<'a>, sm: &Small, lo: i64, we: i64, hs: &RandomState) -> Vec<Vec<ClRow<'a>>> {
    let mut b: Vec<Vec<ClRow>> = (0..NS).map(|_| Vec::new()).collect();
    for_rows(c, |r| {
        // click_id,user_id,campaign_id,ts. Copies are identical, so validating before dedup is equivalent.
        let (cid, uid, camp) = (r[0], r[1], r[2]);
        if cid.is_empty() || uid.is_empty() {
            return;
        }
        let Some(&ci) = sm.camp_idx.get(camp) else { return };
        let ts = parse_ts(r[3]);
        if ts == BAD || ts < lo || ts >= we {
            return;
        }
        b[shard(hs.hash_one(cid))].push((cid, uid, ts, ci));
    });
    b
}

fn dedup_clicks<'a>(
    parsed: &[Vec<Vec<ClRow<'a>>>],
    s: usize,
    names: &People<'a>,
    hs: &RandomState,
) -> Vec<Vec<PCl<'a>>> {
    let n: usize = parsed.iter().map(|p| p[s].len()).sum();
    let mut seen: Map<&'a [u8], ()> = Map::with_capacity_and_hasher(n, rs());
    let mut out: Vec<Vec<PCl>> = (0..NS).map(|_| Vec::new()).collect();
    for p in parsed {
        for &(cid, uid, ts, camp) in &p[s] {
            if seen.insert(cid, ()).is_some() {
                continue;
            }
            let person = names.resolve(uid);
            out[shard(hs.hash_one(person))].push(PCl { person, ts, cid, camp });
        }
    }
    out
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

fn process_shard(mut evs: Vec<PEv>, mut cls: Vec<PCl>, sm: &Small) -> ShardOut {
    evs.sort_unstable_by_key(|e| (e.person, e.ts));
    cls.sort_unstable_by(|a, b| {
        a.person.cmp(&b.person).then(a.ts.cmp(&b.ts)).then_with(|| a.cid.cmp(b.cid))
    });
    let ncamp = sm.camp_names.len();
    let mut users = Vec::new();
    let mut camps: Map<u64, [i64; 4]> = Map::with_hasher(rs());
    let mut cnt = vec![0u32; ncamp]; // clicks per campaign inside the current 7-day window
    let mut active: Vec<u32> = Vec::new(); // campaigns with cnt > 0
    let mut pos = vec![0u32; ncamp]; // index of a campaign inside `active`
    let mut catsum = vec![0i64; sm.cat_names.len()];

    let mut ci = 0usize;
    let mut i = 0usize;
    while i < evs.len() {
        let person = evs[i].person;
        let mut j = i;
        while j < evs.len() && evs[j].person == person {
            j += 1;
        }
        while ci < cls.len() && cls[ci].person < person {
            ci += 1;
        }
        let c0 = ci;
        while ci < cls.len() && cls[ci].person == person {
            ci += 1;
        }
        let clicks = &cls[c0..ci];
        let (mut lo, mut hi) = (0usize, 0usize);

        let mut cur_day: i32 = i32::MIN;
        let (mut d_sess, mut d_ev, mut d_rev) = (0i64, 0i64, 0i64);

        let mut k = i;
        while k < j {
            // one session is evs[k..m]
            let start = evs[k].ts;
            let mut m = k + 1;
            while m < j && evs[m].ts - evs[m - 1].ts <= GAP {
                m += 1;
            }
            let day = start.div_euclid(DAY) as i32;
            if day != cur_day {
                if cur_day != i32::MIN {
                    let top = top_category(&catsum, &sm.cat_rank);
                    users.push(UserRow { person, day: cur_day, sessions: d_sess, events: d_ev, revenue: d_rev, top });
                    catsum.iter_mut().for_each(|x| *x = 0);
                }
                cur_day = day;
                (d_sess, d_ev, d_rev) = (0, 0, 0);
            }
            let mut rev = 0i64;
            let mut purch = false;
            for e in &evs[k..m] {
                if e.price != 0 {
                    purch = true;
                    rev += e.price as i64;
                    catsum[e.cat as usize] += e.price as i64;
                }
            }
            d_sess += 1;
            d_ev += (m - k) as i64;
            d_rev += rev;

            // slide the click window to [start - 7d, start]
            while hi < clicks.len() && clicks[hi].ts <= start {
                let c = clicks[hi].camp as usize;
                if cnt[c] == 0 {
                    pos[c] = active.len() as u32;
                    active.push(c as u32);
                }
                cnt[c] += 1;
                hi += 1;
            }
            while lo < hi && clicks[lo].ts < start - WEEK {
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
            let credit = if lo < hi {
                // latest ts; among equal ts the smallest click_id (clicks are sorted by ts, cid)
                let best = clicks[hi - 1].ts;
                let mut q = hi - 1;
                while q > lo && clicks[q - 1].ts == best {
                    q -= 1;
                }
                clicks[q].camp
            } else {
                ORGANIC
            };
            let e = camps.entry(ckey(credit, day)).or_insert([0; 4]);
            e[0] += 1;
            e[1] += purch as i64;
            e[2] += rev;
            for &a in &active {
                camps.entry(ckey(a, day)).or_insert([0; 4])[3] += 1;
            }
            k = m;
        }
        let top = top_category(&catsum, &sm.cat_rank);
        users.push(UserRow { person, day: cur_day, sessions: d_sess, events: d_ev, revenue: d_rev, top });
        catsum.iter_mut().for_each(|x| *x = 0);
        for &a in &active {
            cnt[a as usize] = 0;
        }
        active.clear();
        i = j;
    }
    ShardOut { users, camps }
}

// ------------------------------------------------------------------ output

fn write_batch(path: PathBuf, batch: RecordBatch) {
    let props = WriterProperties::builder()
        .set_compression(Compression::UNCOMPRESSED)
        .set_dictionary_enabled(false)
        .build();
    let f = File::create(&path).expect("create parquet");
    let mut w = ArrowWriter::try_new(f, batch.schema(), Some(props)).expect("writer");
    w.write(&batch).expect("write");
    w.close().expect("close");
}

fn s(b: &[u8]) -> &str {
    std::str::from_utf8(b).expect("IDs must be UTF-8")
}

fn main() {
    let t0 = Instant::now();
    let data = PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into()));
    let outd = PathBuf::from(std::env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into()));
    let log = |m: &str| eprintln!("[{:>7.3}s] {m}", t0.elapsed().as_secs_f64());

    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(data.join("manifest.json")).expect("manifest")).expect("json");
    let ws = parse_ts(manifest["window_start"].as_str().unwrap().as_bytes());
    let we = parse_ts(manifest["window_end"].as_str().unwrap().as_bytes());
    assert!(ws != BAD && we != BAD);

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
        &["event_id", "user_id", "ts", "ingest_ts", "event_type", "product_id", "price_cents"],
    );
    let cl_chunks = chunks_for(&click_files, &["click_id", "user_id", "campaign_id", "ts"]);
    let hs = rs();

    let names = person_names(&link_chunks);
    log(&format!("links: {} linked ids", names.aliases.len()));

    let ev_parsed: Vec<_> = ev_chunks.par_iter().map(|c| parse_events(c, &sm, ws, we, &hs)).collect();
    log("events parsed");
    let ev_by_person: Vec<Vec<Vec<PEv>>> =
        (0..NS).into_par_iter().map(|s| dedup_events(&ev_parsed, s, &names, &hs)).collect();
    drop(ev_parsed);
    log("events deduped");

    let cl_parsed: Vec<_> = cl_chunks.par_iter().map(|c| parse_clicks(c, &sm, ws - WEEK, we, &hs)).collect();
    let cl_by_person: Vec<Vec<Vec<PCl>>> =
        (0..NS).into_par_iter().map(|s| dedup_clicks(&cl_parsed, s, &names, &hs)).collect();
    drop(cl_parsed);
    log("clicks deduped");

    let outs: Vec<ShardOut> = (0..NS)
        .into_par_iter()
        .map(|p| {
            let evs: Vec<PEv> = ev_by_person.iter().flat_map(|v| v[p].iter().copied()).collect();
            let cls: Vec<PCl> = cl_by_person.iter().flat_map(|v| v[p].iter().copied()).collect();
            process_shard(evs, cls, &sm)
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

    let cbatch = {
        let rows: Vec<(u64, [i64; 4])> = camps.iter().map(|(k, v)| (*k, *v)).collect();
        let camp_of = |k: u64| (k >> 32) as u32;
        let cid = |k: u64| if camp_of(k) == ORGANIC { "organic" } else { s(sm.camp_names[camp_of(k) as usize]) };
        let ch = |k: u64| if camp_of(k) == ORGANIC { "organic" } else { s(sm.camp_channel[camp_of(k) as usize]) };
        let schema = Arc::new(Schema::new(vec![
            Field::new("campaign_id", DataType::Utf8, false),
            Field::new("channel", DataType::Utf8, false),
            Field::new("day", DataType::Date32, false),
            Field::new("sessions", DataType::Int64, false),
            Field::new("purchasing_sessions", DataType::Int64, false),
            Field::new("revenue_cents", DataType::Int64, false),
            Field::new("assisted_sessions", DataType::Int64, false),
        ]));
        let cols: Vec<ArrayRef> = vec![
            Arc::new(StringArray::from_iter_values(rows.iter().map(|r| cid(r.0)))),
            Arc::new(StringArray::from_iter_values(rows.iter().map(|r| ch(r.0)))),
            Arc::new(Date32Array::from_iter_values(rows.iter().map(|r| r.0 as u32 as i32))),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.1[0]))),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.1[1]))),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.1[2]))),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.1[3]))),
        ];
        RecordBatch::try_new(schema, cols).expect("campaign batch")
    };

    let uschema = Arc::new(Schema::new(vec![
        Field::new("user_id", DataType::Utf8, false),
        Field::new("day", DataType::Date32, false),
        Field::new("sessions", DataType::Int64, false),
        Field::new("events", DataType::Int64, false),
        Field::new("revenue_cents", DataType::Int64, false),
        Field::new("top_category", DataType::Utf8, true),
    ]));
    let parts = rayon::current_num_threads().clamp(1, 16);
    let per = NS.div_ceil(parts);
    rayon::join(
        || write_batch(cd.join("part-0.parquet"), cbatch),
        || {
            outs.par_chunks(per).enumerate().for_each(|(pi, group)| {
                let rows: Vec<&UserRow> = group.iter().flat_map(|o| o.users.iter()).collect();
                let mut keys = arrow_array::builder::StringBuilder::with_capacity(rows.len(), 9 * rows.len());
                for r in &rows {
                    keys.append_value(s(names.codec.bytes(r.person, &mut [0; 9])));
                }
                let cols: Vec<ArrayRef> = vec![
                    Arc::new(keys.finish()),
                    Arc::new(Date32Array::from_iter_values(rows.iter().map(|r| r.day))),
                    Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.sessions))),
                    Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.events))),
                    Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.revenue))),
                    Arc::new(StringArray::from_iter(
                        rows.iter().map(|r| r.top.map(|c| s(&sm.cat_names[c as usize]))),
                    )),
                ];
                let b = RecordBatch::try_new(uschema.clone(), cols).expect("user batch");
                write_batch(ud.join(format!("part-{pi}.parquet")), b);
            });
        },
    );
    log("written");
}
