//! Warmed, nested component timings, excluding graph construction and dedup.
//! This module is absent from the production executable.
use super::*;
use std::hint::black_box;
use std::time::Duration;

fn decode<'a, const JOIN: bool>(
    chunks: &[Chunk<'a>],
    sm: &Small,
    people: &People<'a>,
    window: (i64, i64, &ManifestClock),
) -> (usize, usize) {
    chunks
        .par_iter()
        .map(|c| {
            let mut rows = 0usize;
            let mut valid = 0usize;
            let mut spill = Vec::new();
            let mut ring = [Pending::default(); 16];
            let join = |e: Pending| {
                if e.uid != 0 {
                    black_box(people.lookup(e.uid, e.hash));
                }
            };
            word::for_events(c, sm, &people.codec, window, &mut spill, |e| {
                // Make every decoded field observable without storing the input.
                let e = black_box(e);
                valid += usize::from(e.uid != 0);
                if JOIN {
                    if e.uid != 0 {
                        people.prefetch(e.hash);
                    }
                    let i = rows % ring.len();
                    if rows >= ring.len() {
                        join(ring[i]);
                    }
                    ring[i] = e;
                }
                rows += 1;
            });
            if JOIN {
                for i in rows.saturating_sub(ring.len())..rows {
                    join(ring[i % ring.len()]);
                }
            }
            (rows, valid)
        })
        .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
}

#[test]
#[ignore = "set SS_BENCH_DATA to an actual data directory"]
fn typed_routing_budget() {
    let data = PathBuf::from(std::env::var_os("SS_BENCH_DATA").expect("set SS_BENCH_DATA"));
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(data.join("manifest.json")).unwrap()).unwrap();
    let (ws, we) = (
        parse_ts(manifest["window_start"].as_str().unwrap().as_bytes()),
        parse_ts(manifest["window_end"].as_str().unwrap().as_bytes()),
    );
    let clock = ManifestClock::new(ws, we);
    let window = (ws, we, &clock);
    let pp = data.join("products.csv");
    let cp = data.join("campaigns.csv");
    let prod = std::fs::read(&pp).unwrap();
    let camp = std::fs::read(&cp).unwrap();
    let sm = read_small(&prod, &camp, &pp, &cp);
    let links = mmap_files(&data.join("links"));
    let events = mmap_files(&data.join("events"));
    let chunks = chunks_for(
        &events,
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
    let names = person_names(&chunks_for(&links, &["user_id_a", "user_id_b"]));
    let packing = Packing::new(ws, we, names.rank_limit);
    let bytes: usize = chunks.iter().map(|c| c.data.len()).sum();
    let labels = [
        "typed grammar",
        "typed grammar + exact alias lookup",
        "typed grammar + both real scatter streams",
    ];
    let mut samples: [Vec<Duration>; 3] = Default::default();
    for round in 0..=5 {
        let mut counts = [(0, 0); 3];
        for k in 0..3 {
            let i = (round + k) % 3;
            let t = Instant::now();
            let elapsed = match i {
                0 => {
                    counts[i] = decode::<false>(&chunks, &sm, &names, window);
                    t.elapsed()
                }
                1 => {
                    counts[i] = decode::<true>(&chunks, &sm, &names, window);
                    t.elapsed()
                }
                _ => {
                    let parts: Vec<_> = chunks
                        .par_iter()
                        .map(|c| parse(c, &sm, &names, window, packing))
                        .collect();
                    let elapsed = t.elapsed();
                    for (dest, claims, _) in black_box(&parts) {
                        counts[i].0 += claims.iter().map(Vec::len).sum::<usize>();
                        counts[i].1 += dest
                            .iter()
                            .map(|p| {
                                p.packed.times.len()
                                    + p.packed.purchases.len()
                                    + p.wide.times.len()
                                    + p.wide.purchases.len()
                            })
                            .sum::<usize>();
                    }
                    elapsed
                }
            };
            if round != 0 {
                samples[i].push(elapsed);
            }
        }
        assert_eq!(counts[0], counts[2]);
        assert_eq!(counts[1], counts[2]);
    }
    for (name, times) in labels.into_iter().zip(&mut samples) {
        times.sort_unstable();
        let sec = times[2].as_secs_f64();
        println!(
            "typed budget: {name}: {sec:.4} s; {:.2} GB/s; {bytes} event body bytes, {} workers",
            bytes as f64 / sec / 1e9,
            rayon::current_num_threads()
        );
    }
}
