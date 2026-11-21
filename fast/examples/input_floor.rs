//! Diagnostic only. Time the real strict CSV and timestamp readers before
//! any catalog lookup, identity resolution, output allocation or session work.
use memmap2::Mmap;
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::File;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Instant;

const CHUNK: usize = 8 << 20;
const BAD: i64 = i64::MIN;
type Map<K, V> = HashMap<K, V, ahash::RandomState>;
fn rs() -> ahash::RandomState {
    ahash::RandomState::with_seeds(0x9E37_79B9, 0x85EB_CA6B, 0xC2B2_AE35, 0x27D4_EB2F)
}
#[allow(dead_code)]
#[path = "../src/csv.rs"]
mod csv;
#[allow(dead_code)]
#[path = "../src/links.rs"]
mod links;
#[allow(dead_code)]
#[path = "../src/person.rs"]
mod person;
#[allow(dead_code)]
#[path = "../src/ts.rs"]
mod ts;
#[allow(dead_code)]
#[path = "../src/word.rs"]
mod word;
use csv::{Chunk, for_rows};
use person::Codec;
const NS: usize = 256;
const SHARD_SHIFT: u32 = 40;

fn maps(path: &Path) -> Vec<(PathBuf, Mmap)> {
    let mut paths: Vec<_> = std::fs::read_dir(path)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "csv") && p.metadata().unwrap().len() > 0)
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|p| {
            let f = File::open(&p).unwrap();
            // Same read-only input lifetime as the scored entry.
            (p, unsafe { Mmap::map(&f).unwrap() })
        })
        .collect()
}

fn scan<const TYPED: bool>(chunks: &[Chunk], clock: &ts::ManifestClock) -> (usize, u64) {
    let codec = Codec::new();
    chunks
        .par_iter()
        .map(|c| {
            let mut n = 0usize;
            let mut checksum = 0u64;
            for_rows(c, |r| {
                // Make all field spans observable even in structural-only mode.
                let r = black_box(r);
                n += 1;
                let v = if TYPED && !r[0].is_empty() {
                    let id = codec.key(r[0]);
                    let uid = if r[1].is_empty() { 0 } else { codec.key(r[1]) };
                    let (ingest, _) = clock.ingest_for_claim(r[3]);
                    (clock.parse_ts(r[2]) as u64) ^ (ingest as u64) ^ id ^ uid
                } else {
                    r[0].len() as u64
                };
                checksum = checksum.wrapping_add(v);
            });
            (n, checksum)
        })
        .reduce(|| (0, 0), |(an, av), (bn, bv)| (an + bn, av.wrapping_add(bv)))
}

fn main() {
    let dir = PathBuf::from(std::env::args_os().nth(1).expect("pass a generator data directory"));
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    let clock = ts::ManifestClock::new(
        ts::parse_ts(manifest["window_start"].as_str().unwrap().as_bytes()),
        ts::parse_ts(manifest["window_end"].as_str().unwrap().as_bytes()),
    );
    let files = maps(&dir.join("events"));
    let columns = [
        "event_id",
        "user_id",
        "ts",
        "ingest_ts",
        "event_type",
        "product_id",
        "price_cents",
    ];
    let chunks: Vec<_> = files.iter().flat_map(|(p, m)| csv::chunks_of(m, &columns, p)).collect();
    let bytes: usize = chunks.iter().map(|c| c.data.len()).sum();
    let labels = ["count-LF", "strict-fields", "fields+IDs+clocks"];
    let mut samples: [Vec<f64>; 3] = Default::default();
    let mut counts = [0usize; 3];
    // One untimed round, then alternate each kernel's position relative to the others.
    for i in 0..=5 {
        for k in 0..3 {
            let j = if i % 2 == 0 { k } else { 2 - k };
            let start = Instant::now();
            let pair = match j {
                0 => (
                    chunks
                        .par_iter()
                        .map(|c| memchr::memchr_iter(b'\n', c.data).count())
                        .sum(),
                    0,
                ),
                1 => scan::<false>(&chunks, &clock),
                _ => scan::<true>(&chunks, &clock),
            };
            counts[j] = black_box(pair).0;
            if i != 0 {
                samples[j].push(start.elapsed().as_secs_f64());
            }
        }
    }
    assert_eq!(counts[1], counts[2]);
    for (i, sample) in samples.iter_mut().enumerate() {
        sample.sort_by(f64::total_cmp);
        let t = sample[2];
        println!(
            "budget: {}: {t:.4} s; {:.2} GB/s; count {}; {bytes} body bytes; {} workers",
            labels[i],
            bytes as f64 / 1e9 / t,
            counts[i],
            rayon::current_num_threads()
        );
    }
}
